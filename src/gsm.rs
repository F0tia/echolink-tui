//! Minimal binding to libgsm (GSM 06.10 full rate), the codec EchoLink uses.

use std::os::raw::{c_int, c_void};

pub const FRAME_SAMPLES: usize = 160;
pub const FRAME_BYTES: usize = 33;

#[link(name = "gsm")]
unsafe extern "C" {
    fn gsm_create() -> *mut c_void;
    fn gsm_destroy(g: *mut c_void);
    fn gsm_encode(g: *mut c_void, src: *const i16, dst: *mut u8);
    fn gsm_decode(g: *mut c_void, src: *const u8, dst: *mut i16) -> c_int;
}

pub struct Gsm(*mut c_void);

// A gsm state is plain heap memory owned by this handle.
unsafe impl Send for Gsm {}

impl Default for Gsm {
    fn default() -> Self {
        Self::new()
    }
}

impl Gsm {
    pub fn new() -> Self {
        let g = unsafe { gsm_create() };
        assert!(!g.is_null(), "gsm_create failed");
        Gsm(g)
    }

    pub fn encode(&mut self, pcm: &[i16; FRAME_SAMPLES]) -> [u8; FRAME_BYTES] {
        let mut out = [0u8; FRAME_BYTES];
        unsafe { gsm_encode(self.0, pcm.as_ptr(), out.as_mut_ptr()) };
        out
    }

    pub fn decode(&mut self, frame: &[u8]) -> Option<[i16; FRAME_SAMPLES]> {
        if frame.len() < FRAME_BYTES {
            return None;
        }
        let mut out = [0i16; FRAME_SAMPLES];
        let rc = unsafe { gsm_decode(self.0, frame.as_ptr(), out.as_mut_ptr()) };
        (rc == 0).then_some(out)
    }
}

impl Drop for Gsm {
    fn drop(&mut self) {
        unsafe { gsm_destroy(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_keeps_a_tone() {
        let (mut enc, mut dec) = (Gsm::new(), Gsm::new());
        let mut out = Vec::new();
        for n in 0..10 {
            let mut pcm = [0i16; FRAME_SAMPLES];
            for (i, s) in pcm.iter_mut().enumerate() {
                let t = (n * FRAME_SAMPLES + i) as f32 / 8000.0;
                *s = ((2.0 * std::f32::consts::PI * 1000.0 * t).sin() * 8000.0) as i16;
            }
            out.extend(dec.decode(&enc.encode(&pcm)).unwrap());
        }
        // Skip the codec's start-up frames, then expect most of the energy back.
        let rms = (out[800..].iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / 800.0).sqrt();
        assert!(rms > 3000.0 && rms < 8000.0, "rms {rms}");
        assert!(dec.decode(&[0u8; 10]).is_none());
    }
}
