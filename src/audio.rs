//! Sound card I/O at EchoLink's 8 kHz mono, resampling when the device
//! can't run at 8 kHz itself.

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, SampleRate, Stream, StreamConfig, SupportedStreamConfigRange};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

pub const RATE: u32 = 8000;
/// Most audio queued for playback before old audio is dropped (0.6 s).
const MAX_PLAYBACK: usize = RATE as usize * 6 / 10;
/// Audio buffered before playback starts, to ride out network jitter (0.24 s).
const PREBUFFER: usize = RATE as usize * 24 / 100;

/// Low-pass FIR (windowed sinc) that keeps the 300-3400 Hz voice band.
struct LowPass {
    taps: Vec<f32>,
    hist: VecDeque<f32>,
}

impl LowPass {
    fn new(rate: u32, cutoff: f32) -> Self {
        let n = 63;
        let fc = cutoff / rate as f32;
        let m = (n - 1) as f32 / 2.0;
        let mut taps: Vec<f32> = (0..n)
            .map(|i| {
                let x = i as f32 - m;
                let sinc = if x == 0.0 { 2.0 * fc } else { (2.0 * std::f32::consts::PI * fc * x).sin() / (std::f32::consts::PI * x) };
                let w = 0.54 - 0.46 * (2.0 * std::f32::consts::PI * i as f32 / (n - 1) as f32).cos();
                sinc * w
            })
            .collect();
        let sum: f32 = taps.iter().sum();
        taps.iter_mut().for_each(|t| *t /= sum);
        LowPass { hist: VecDeque::from(vec![0.0; n]), taps }
    }

    fn push(&mut self, x: f32) -> f32 {
        self.hist.pop_front();
        self.hist.push_back(x);
        self.hist.iter().zip(&self.taps).map(|(a, b)| a * b).sum()
    }
}

/// Linear-interpolating rate converter with a low-pass on the high-rate side.
struct Resampler {
    step: f64, // input samples per output sample
    pos: f64,
    prev: f32,
    filter: Option<LowPass>,
    filter_input: bool,
}

impl Resampler {
    fn new(from: u32, to: u32) -> Self {
        let high = from.max(to);
        Resampler {
            step: from as f64 / to as f64,
            pos: 0.0,
            prev: 0.0,
            filter: (from != to).then(|| LowPass::new(high, 3600.0)),
            filter_input: from > to,
        }
    }

    fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.filter.is_none() {
            out.extend_from_slice(input);
            return;
        }
        for &raw in input {
            let x = match (&mut self.filter, self.filter_input) {
                (Some(f), true) => f.push(raw),
                _ => raw,
            };
            // Emit every output sample that falls between prev and x.
            while self.pos < 1.0 {
                let y = self.prev + (x - self.prev) * self.pos as f32;
                out.push(match (&mut self.filter, self.filter_input) {
                    (Some(f), false) => f.push(y),
                    _ => y,
                });
                self.pos += self.step;
            }
            self.pos -= 1.0;
            self.prev = x;
        }
    }
}

/// Shared state between the audio callbacks and the rest of the app.
#[derive(Default)]
pub struct Shared {
    playback: Mutex<VecDeque<i16>>,
    playing: AtomicBool,
    pub rx_level: AtomicU32,
    pub tx_level: AtomicU32,
}

impl Shared {
    pub fn play(&self, pcm: &[i16]) {
        let mut q = self.playback.lock().unwrap();
        q.extend(pcm);
        let excess = q.len().saturating_sub(MAX_PLAYBACK);
        q.drain(..excess);
    }

    #[cfg(test)]
    pub fn queued(&self) -> usize {
        self.playback.lock().unwrap().len()
    }

    pub fn flush(&self) {
        self.playback.lock().unwrap().clear();
        self.playing.store(false, Ordering::Relaxed);
    }
}

fn peak(samples: impl Iterator<Item = f32>) -> u32 {
    (samples.fold(0.0f32, |m, s| m.max(s.abs())).min(1.0) * 1000.0) as u32
}

fn pick(ranges: Vec<SupportedStreamConfigRange>) -> Result<(StreamConfig, SampleFormat)> {
    let usable = |r: &&SupportedStreamConfigRange| matches!(r.sample_format(), SampleFormat::F32 | SampleFormat::I16);
    let best = ranges
        .iter()
        .filter(usable)
        .min_by_key(|r| {
            let fits = r.min_sample_rate().0 <= RATE && RATE <= r.max_sample_rate().0;
            (!fits, r.channels() != 1, r.sample_format() != SampleFormat::F32)
        })
        .ok_or_else(|| anyhow!("no f32 or i16 audio format available"))?;
    let rate = if best.min_sample_rate().0 <= RATE && RATE <= best.max_sample_rate().0 {
        RATE
    } else if best.min_sample_rate().0 <= 48000 && 48000 <= best.max_sample_rate().0 {
        48000
    } else {
        best.max_sample_rate().0
    };
    let cfg = best.with_sample_rate(SampleRate(rate));
    Ok((cfg.config(), cfg.sample_format()))
}

pub struct Audio {
    _input: Stream,
    _output: Stream,
    pub shared: Arc<Shared>,
    pub description: String,
}

/// Open the default microphone and speaker. Captured audio is delivered to
/// `mic` in 8 kHz chunks while `transmitting` is set.
pub fn open(mic: Sender<Vec<i16>>, transmitting: Arc<AtomicBool>) -> Result<Audio> {
    let host = cpal::default_host();
    let shared = Arc::new(Shared::default());

    let indev = host.default_input_device().context("no microphone found")?;
    let (incfg, infmt) = pick(indev.supported_input_configs()?.collect())?;
    let outdev = host.default_output_device().context("no speaker found")?;
    let (outcfg, outfmt) = pick(outdev.supported_output_configs()?.collect())?;

    let description = format!(
        "mic {} @ {} Hz, speaker {} @ {} Hz",
        indev.name().unwrap_or_default(),
        incfg.sample_rate.0,
        outdev.name().unwrap_or_default(),
        outcfg.sample_rate.0
    );

    // Capture: mix to mono, resample to 8 kHz, hand off while transmitting.
    let input = {
        let ch = incfg.channels as usize;
        let mut rs = Resampler::new(incfg.sample_rate.0, RATE);
        let shared = shared.clone();
        let mut mono = Vec::new();
        let mut out = Vec::new();
        let mut handle = move |data: &mut dyn Iterator<Item = f32>| {
            mono.clear();
            let mut acc = 0.0;
            for (i, s) in data.enumerate() {
                acc += s;
                if i % ch == ch - 1 {
                    mono.push(acc / ch as f32);
                    acc = 0.0;
                }
            }
            let tx = transmitting.load(Ordering::Relaxed);
            shared.tx_level.store(if tx { peak(mono.iter().copied()) } else { 0 }, Ordering::Relaxed);
            if !tx {
                return;
            }
            out.clear();
            rs.process(&mono, &mut out);
            let pcm = out.iter().map(|s| (s.clamp(-1.0, 1.0) * 32767.0) as i16).collect();
            let _ = mic.send(pcm);
        };
        let err = |e| eprintln!("microphone: {e}");
        match infmt {
            SampleFormat::F32 => indev.build_input_stream(&incfg, move |d: &[f32], _: &_| handle(&mut d.iter().copied()), err, None)?,
            _ => indev.build_input_stream(
                &incfg,
                move |d: &[i16], _: &_| handle(&mut d.iter().map(|&s| s as f32 / 32768.0)),
                err,
                None,
            )?,
        }
    };

    // Playback: pull 8 kHz audio from the queue, resample, fan out to channels.
    let output = {
        let ch = outcfg.channels as usize;
        let mut rs = Resampler::new(RATE, outcfg.sample_rate.0);
        let ratio = outcfg.sample_rate.0 as f64 / RATE as f64;
        let shared = shared.clone();
        let mut pending: VecDeque<f32> = VecDeque::new();
        let mut src = Vec::new();
        let mut res = Vec::new();
        let mut fill = move |frames: usize| -> Vec<f32> {
            while pending.len() < frames {
                let need = ((frames - pending.len()) as f64 / ratio).ceil() as usize + 1;
                src.clear();
                {
                    let mut q = shared.playback.lock().unwrap();
                    if !shared.playing.load(Ordering::Relaxed) && q.len() >= PREBUFFER {
                        shared.playing.store(true, Ordering::Relaxed);
                    }
                    if shared.playing.load(Ordering::Relaxed) {
                        let take = need.min(q.len());
                        src.extend(q.drain(..take).map(|s| s as f32 / 32768.0));
                        if q.is_empty() {
                            shared.playing.store(false, Ordering::Relaxed);
                        }
                    }
                }
                src.resize(need, 0.0); // silence on underrun
                res.clear();
                rs.process(&src, &mut res);
                pending.extend(res.iter());
            }
            let block: Vec<f32> = pending.drain(..frames).collect();
            shared.rx_level.store(peak(block.iter().copied()), Ordering::Relaxed);
            block
        };
        let err = |e| eprintln!("speaker: {e}");
        match outfmt {
            SampleFormat::F32 => outdev.build_output_stream(
                &outcfg,
                move |d: &mut [f32], _: &_| {
                    let block = fill(d.len() / ch);
                    for (frame, s) in d.chunks_mut(ch).zip(block) {
                        frame.fill(s);
                    }
                },
                err,
                None,
            )?,
            _ => outdev.build_output_stream(
                &outcfg,
                move |d: &mut [i16], _: &_| {
                    let block = fill(d.len() / ch);
                    for (frame, s) in d.chunks_mut(ch).zip(block) {
                        frame.fill((s.clamp(-1.0, 1.0) * 32767.0) as i16);
                    }
                },
                err,
                None,
            )?,
        }
    };

    input.play()?;
    output.play()?;
    Ok(Audio { _input: input, _output: output, shared, description })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampler_ratios() {
        let input = vec![0.5f32; 48000];
        let mut out = Vec::new();
        Resampler::new(48000, 8000).process(&input, &mut out);
        assert!((out.len() as i64 - 8000).abs() <= 2, "{}", out.len());

        let input = vec![0.5f32; 8000];
        let mut out = Vec::new();
        Resampler::new(8000, 44100).process(&input, &mut out);
        assert!((out.len() as i64 - 44100).abs() <= 6, "{}", out.len());
        // DC passes through the filter at unity gain once it settles.
        assert!((out[40000] - 0.5).abs() < 0.01);
    }
}
