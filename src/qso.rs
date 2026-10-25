//! One EchoLink QSO at a time: connection state, keepalives, voice and chat.
//! Runs on its own thread; the UI talks to it through `Cmd` and `Event`.

use crate::audio::Shared;
use crate::gsm::{FRAME_BYTES, FRAME_SAMPLES, Gsm};
use crate::net::{Net, ProxyManager};
use crate::proxy::Udp;
use crate::rtp::{self, AudioPort, FRAMES_PER_PACKET};
use anyhow::{Context, Result, anyhow};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::cell::Cell;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const AUDIO_PORT: u16 = 5198;
pub const CTRL_PORT: u16 = 5199;
const KEEPALIVE: Duration = Duration::from_secs(10);
const CONNECT_TRIES: u32 = 5;
const LINK_TIMEOUT: Duration = Duration::from_secs(50);
const RX_HOLD: Duration = Duration::from_millis(400);
pub const TX_LIMIT: Duration = Duration::from_secs(180);
const PACKET_SAMPLES: usize = FRAME_SAMPLES * FRAMES_PER_PACKET;

pub enum Cmd {
    Connect { ip: Ipv4Addr, label: String },
    Disconnect,
    Chat(String),
    Ptt(bool),
    Quit,
}

#[derive(Clone, Debug, PartialEq)]
pub enum State {
    Idle,
    Connecting { ip: Ipv4Addr, label: String },
    Connected { ip: Ipv4Addr, callsign: String, name: String },
}

pub enum Event {
    State(State),
    Info(String),
    Chat(String),
    Receiving(bool),
    Transmitting(bool),
    Log(String),
}

enum Input {
    Cmd(Cmd),
    Ctrl(Ipv4Addr, Vec<u8>),
    Audio(Ipv4Addr, Vec<u8>),
}

pub struct Station {
    pub callsign: String,
    pub name: String,
    pub info: String,
    pub accept_incoming: bool,
}

pub struct Handle {
    pub cmds: Sender<Cmd>,
    pub events: Receiver<Event>,
    pub transmitting: Arc<AtomicBool>,
}

fn bind(port: u16) -> Result<UdpSocket> {
    UdpSocket::bind(("0.0.0.0", port))
        .with_context(|| format!("can't open UDP port {port}; is another EchoLink program (Qtel, SvxLink) running?"))
}

fn reader(sock: UdpSocket, tx: Sender<Input>, audio: bool) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 2048];
        loop {
            let Ok((n, SocketAddr::V4(from))) = sock.recv_from(&mut buf) else {
                continue;
            };
            let data = buf[..n].to_vec();
            let msg = if audio { Input::Audio(*from.ip(), data) } else { Input::Ctrl(*from.ip(), data) };
            if tx.send(msg).is_err() {
                return;
            }
        }
    });
}

/// Where station traffic goes: our own UDP ports, or an EchoLink proxy.
enum Transport {
    Sockets { audio: UdpSocket, ctrl: UdpSocket },
    Proxy(Arc<ProxyManager>),
}

/// Start the engine. `mic` delivers 8 kHz audio while `transmitting` is set.
/// With a proxy, its station traffic arrives on `proxy_udp`.
pub fn spawn(
    me: Station,
    shared: Arc<Shared>,
    mic: Receiver<Vec<i16>>,
    transmitting: Arc<AtomicBool>,
    net: &Net,
    proxy_udp: Receiver<Udp>,
) -> Result<Handle> {
    let (in_tx, in_rx) = channel();
    let (ev_tx, ev_rx) = channel();
    let transport = match net {
        Net::Direct => {
            let audio = bind(AUDIO_PORT)?;
            let ctrl = bind(CTRL_PORT)?;
            crate::log!("listening on UDP {AUDIO_PORT} (audio) and {CTRL_PORT} (control) as {}", me.callsign);
            reader(audio.try_clone()?, in_tx.clone(), true);
            reader(ctrl.try_clone()?, in_tx.clone(), false);
            Transport::Sockets { audio, ctrl }
        }
        Net::Proxy(m) => {
            let in_tx = in_tx.clone();
            std::thread::spawn(move || {
                for u in proxy_udp {
                    let msg = match u {
                        Udp::Audio(ip, d) => Input::Audio(ip, d),
                        Udp::Ctrl(ip, d) => Input::Ctrl(ip, d),
                    };
                    if in_tx.send(msg).is_err() {
                        return;
                    }
                }
            });
            Transport::Proxy(m.clone())
        }
    };

    let (cmd_tx, cmd_rx) = channel::<Cmd>();
    {
        let in_tx = in_tx.clone();
        std::thread::spawn(move || {
            for c in cmd_rx {
                if in_tx.send(Input::Cmd(c)).is_err() {
                    return;
                }
            }
        });
    }

    let mut engine = Engine {
        me,
        transport,
        events: ev_tx,
        shared,
        transmitting: transmitting.clone(),
        state: State::Idle,
        sdes: Vec::new(),
        next_keepalive: Instant::now(),
        tries: 0,
        last_heard: Instant::now(),
        last_voice: None,
        tx_started: None,
        tx_buf: Vec::new(),
        seq: 0,
        enc: Gsm::new(),
        dec: Gsm::new(),
        rx_packets: Cell::new(0),
        tx_packets: Cell::new(0),
    };
    engine.sdes = rtp::sdes(&engine.me.callsign, &engine.me.name);
    std::thread::spawn(move || engine.run(in_rx, mic));

    Ok(Handle { cmds: cmd_tx, events: ev_rx, transmitting })
}

struct Engine {
    me: Station,
    transport: Transport,
    events: Sender<Event>,
    shared: Arc<Shared>,
    transmitting: Arc<AtomicBool>,
    state: State,
    sdes: Vec<u8>,
    next_keepalive: Instant,
    tries: u32,
    last_heard: Instant,
    last_voice: Option<Instant>,
    tx_started: Option<Instant>,
    tx_buf: Vec<i16>,
    seq: u16,
    enc: Gsm,
    dec: Gsm,
    rx_packets: Cell<u32>,
    tx_packets: Cell<u32>,
}

fn kind(p: &[u8]) -> &'static str {
    if rtp::is_bye(p) {
        "BYE"
    } else if rtp::sdes_name(p).is_some() {
        "SDES"
    } else {
        "unknown"
    }
}

impl Engine {
    fn emit(&self, e: Event) {
        match &e {
            Event::State(s) => crate::log!("state: {s:?}"),
            Event::Log(t) => crate::log!("{t}"),
            Event::Info(t) => crate::log!("info text received: {:?}", t),
            Event::Chat(t) => crate::log!("chat: {t}"),
            Event::Receiving(true) => crate::log!("RX start"),
            Event::Receiving(false) => crate::log!("RX end: {} voice packets", self.rx_packets.replace(0)),
            Event::Transmitting(true) => crate::log!("TX start"),
            Event::Transmitting(false) => crate::log!("TX end: {} voice packets sent", self.tx_packets.replace(0)),
        }
        let _ = self.events.send(e);
    }

    fn peer(&self) -> Option<Ipv4Addr> {
        match &self.state {
            State::Idle => None,
            State::Connecting { ip, .. } | State::Connected { ip, .. } => Some(*ip),
        }
    }

    fn set_state(&mut self, s: State) {
        if s != self.state {
            self.state = s.clone();
            self.emit(Event::State(s));
        }
    }

    fn send_to(&self, ip: Ipv4Addr, p: &[u8], ctrl: bool) -> Result<()> {
        match &self.transport {
            Transport::Sockets { audio, ctrl: c } => {
                let (sock, port) = if ctrl { (c, CTRL_PORT) } else { (audio, AUDIO_PORT) };
                sock.send_to(p, (IpAddr::V4(ip), port)).map(drop).map_err(Into::into)
            }
            Transport::Proxy(m) => {
                let proxy = m.current().ok_or_else(|| anyhow!("proxy not connected"))?;
                if ctrl { proxy.udp_ctrl(ip, p) } else { proxy.udp_audio(ip, p) }
            }
        }
    }

    fn send_ctrl(&self, ip: Ipv4Addr, p: &[u8]) {
        let r = self.send_to(ip, p, true);
        crate::log!("-> {ip}:{CTRL_PORT} {} ({} bytes){}", kind(p), p.len(), r.err().map(|e| format!(" FAILED: {e}")).unwrap_or_default());
    }

    fn send_audio(&self, ip: Ipv4Addr, p: &[u8]) {
        let r = self.send_to(ip, p, false);
        if p.first() == Some(&0xC0) {
            self.tx_packets.set(self.tx_packets.get() + 1);
            if let Err(e) = r {
                crate::log!("-> {ip}:{AUDIO_PORT} voice FAILED: {e}");
            }
        } else {
            let text = String::from_utf8_lossy(&p[..p.len().min(60)]).replace(['\r', '\0'], " ");
            crate::log!("-> {ip}:{AUDIO_PORT} text {text:?}{}", r.err().map(|e| format!(" FAILED: {e}")).unwrap_or_default());
        }
    }

    fn run(&mut self, input: Receiver<Input>, mic: Receiver<Vec<i16>>) {
        loop {
            match input.recv_timeout(Duration::from_millis(10)) {
                Ok(Input::Cmd(Cmd::Quit)) => {
                    self.hang_up("quit");
                    return;
                }
                Ok(Input::Cmd(c)) => self.command(c),
                Ok(Input::Ctrl(ip, p)) => self.ctrl(ip, &p),
                Ok(Input::Audio(ip, p)) => self.audio(ip, &p),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(_) => return,
            }
            while let Ok(pcm) = mic.try_recv() {
                self.mic(&pcm);
            }
            self.timers();
        }
    }

    fn command(&mut self, c: Cmd) {
        match c {
            Cmd::Connect { ip, label } => {
                self.hang_up("switching stations");
                self.set_state(State::Connecting { ip, label: label.clone() });
                self.tries = 0;
                self.next_keepalive = Instant::now();
                self.emit(Event::Log(format!("Calling {label} ({ip})…")));
            }
            Cmd::Disconnect => self.hang_up("disconnected"),
            Cmd::Chat(msg) => {
                if let State::Connected { ip, .. } = self.state {
                    self.send_audio(ip, &rtp::chat(&self.me.callsign, &msg));
                    self.emit(Event::Chat(format!("{}>{msg}", self.me.callsign)));
                } else {
                    self.emit(Event::Log("Not connected; chat not sent.".into()));
                }
            }
            Cmd::Ptt(on) => self.ptt(on),
            Cmd::Quit => {}
        }
    }

    fn ptt(&mut self, on: bool) {
        let connected = matches!(self.state, State::Connected { .. });
        let on = on && connected;
        if on == self.transmitting.load(Ordering::Relaxed) {
            return;
        }
        if on {
            self.tx_buf.clear();
            self.tx_started = Some(Instant::now());
        } else {
            // Send the last partial packet, padded with silence.
            if !self.tx_buf.is_empty() {
                self.tx_buf.resize(PACKET_SAMPLES, 0);
                self.flush_tx();
            }
            self.tx_started = None;
        }
        self.transmitting.store(on, Ordering::Relaxed);
        self.emit(Event::Transmitting(on));
    }

    fn hang_up(&mut self, why: &str) {
        self.ptt(false);
        if let Some(ip) = self.peer() {
            self.send_ctrl(ip, &rtp::bye());
            self.emit(Event::Log(format!("Disconnected ({why}).")));
        }
        self.end_rx();
        self.set_state(State::Idle);
    }

    fn end_rx(&mut self) {
        if self.last_voice.take().is_some() {
            self.emit(Event::Receiving(false));
        }
        self.shared.flush();
    }

    fn ctrl(&mut self, ip: Ipv4Addr, p: &[u8]) {
        let from_peer = self.peer() == Some(ip);
        crate::log!(
            "<- {ip}:{CTRL_PORT} {} ({} bytes){}{}",
            kind(p),
            p.len(),
            rtp::sdes_name(p).map(|n| format!(" name {n:?}")).unwrap_or_default(),
            if from_peer { "" } else { " [not our peer]" }
        );
        if rtp::is_bye(p) {
            if from_peer {
                self.ptt(false);
                self.end_rx();
                self.emit(Event::Log("The other station disconnected.".into()));
                self.set_state(State::Idle);
            }
            return;
        }
        let Some(name) = rtp::sdes_name(p) else { return };
        let (callsign, rest) = rtp::split_name(&name);
        match &self.state {
            _ if from_peer => {
                self.last_heard = Instant::now();
                if let State::Connecting { .. } = self.state {
                    self.connected(ip, callsign, rest);
                }
            }
            State::Idle if self.me.accept_incoming => {
                self.emit(Event::Log(format!("Incoming connection from {callsign} ({ip}).")));
                self.connected(ip, callsign, rest);
            }
            _ => {
                // Busy or not accepting: tell the caller we're not available.
                self.send_ctrl(ip, &rtp::bye());
                self.emit(Event::Log(format!("Refused a connection from {callsign}.")));
            }
        }
    }

    fn connected(&mut self, ip: Ipv4Addr, callsign: String, name: String) {
        self.last_heard = Instant::now();
        self.set_state(State::Connected { ip, callsign: callsign.clone(), name });
        self.send_ctrl(ip, &self.sdes);
        self.next_keepalive = Instant::now() + KEEPALIVE;
        self.send_audio(ip, &rtp::info(&self.me.info));
        self.emit(Event::Log(format!("Connected to {callsign}.")));
    }

    fn audio(&mut self, ip: Ipv4Addr, p: &[u8]) {
        if self.peer() != Some(ip) {
            crate::log!("<- {ip}:{AUDIO_PORT} {} bytes from a station we're not connected to; ignored", p.len());
            return;
        }
        self.last_heard = Instant::now();
        match rtp::parse_audio_port(p) {
            AudioPort::Voice(frames) => {
                if self.transmitting.load(Ordering::Relaxed) {
                    return; // half duplex, like the radio on the other end
                }
                let mut pcm = Vec::with_capacity(PACKET_SAMPLES);
                for f in frames {
                    if let Some(s) = self.dec.decode(f) {
                        pcm.extend_from_slice(&s);
                    }
                }
                self.rx_packets.set(self.rx_packets.get() + 1);
                self.shared.play(&pcm);
                if self.last_voice.replace(Instant::now()).is_none() {
                    self.emit(Event::Receiving(true));
                }
            }
            AudioPort::Info(t) => self.emit(Event::Info(t)),
            AudioPort::Chat(t) => self.emit(Event::Chat(t)),
            AudioPort::Other => {}
        }
    }

    fn mic(&mut self, pcm: &[i16]) {
        if !self.transmitting.load(Ordering::Relaxed) {
            return;
        }
        self.tx_buf.extend_from_slice(pcm);
        while self.tx_buf.len() >= PACKET_SAMPLES {
            self.flush_tx();
        }
    }

    fn flush_tx(&mut self) {
        let Some(ip) = self.peer() else { return };
        let mut frames = [[0u8; FRAME_BYTES]; FRAMES_PER_PACKET];
        for (i, f) in frames.iter_mut().enumerate() {
            let mut block = [0i16; FRAME_SAMPLES];
            block.copy_from_slice(&self.tx_buf[i * FRAME_SAMPLES..(i + 1) * FRAME_SAMPLES]);
            *f = self.enc.encode(&block);
        }
        self.tx_buf.drain(..PACKET_SAMPLES);
        self.send_audio(ip, &rtp::voice(self.seq, &frames));
        self.seq = self.seq.wrapping_add(1);
    }

    fn timers(&mut self) {
        let now = Instant::now();
        if self.last_voice.is_some_and(|t| now - t > RX_HOLD) {
            self.last_voice = None;
            self.emit(Event::Receiving(false));
        }
        if self.tx_started.is_some_and(|t| now - t > TX_LIMIT) {
            self.emit(Event::Log("Transmit time limit (3 min) reached; unkeyed.".into()));
            self.ptt(false);
        }
        match self.state.clone() {
            State::Idle => {}
            State::Connecting { ip, label } => {
                if now >= self.next_keepalive {
                    if self.tries == CONNECT_TRIES {
                        self.emit(Event::Log(format!(
                            "{label} didn't answer. It may be offline or busy, or it may not see this computer logged in yet; try again in a minute."
                        )));
                        self.send_ctrl(ip, &rtp::bye());
                        self.set_state(State::Idle);
                        return;
                    }
                    self.tries += 1;
                    self.send_ctrl(ip, &self.sdes);
                    self.next_keepalive = now + KEEPALIVE;
                }
            }
            State::Connected { ip, callsign, .. } => {
                if now - self.last_heard > LINK_TIMEOUT {
                    self.ptt(false);
                    self.end_rx();
                    self.emit(Event::Log(format!("Lost contact with {callsign}.")));
                    self.set_state(State::Idle);
                } else if now >= self.next_keepalive {
                    self.send_ctrl(ip, &self.sdes);
                    self.next_keepalive = now + KEEPALIVE;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// End-to-end against tests/peer.py, a simulated station at $PEER_IP.
    /// Needs its own network namespace; run through tests/run-peer-test.sh.
    #[test]
    #[ignore]
    fn qso_with_simulated_peer() {
        let ip: Ipv4Addr = std::env::var("PEER_IP").expect("PEER_IP").parse().unwrap();
        let shared = Arc::new(Shared::default());
        let (mic_tx, mic_rx) = channel();
        let me = Station { callsign: "TEST1".into(), name: "Tester".into(), info: "test info".into(), accept_incoming: false };
        let (_udp_tx, udp_rx) = channel();
        let h = spawn(me, shared.clone(), mic_rx, Arc::new(AtomicBool::new(false)), &Net::Direct, udp_rx).unwrap();

        let wait_for = |what: &str, f: &dyn Fn(&Event) -> bool| {
            let deadline = Instant::now() + Duration::from_secs(15);
            while Instant::now() < deadline {
                if let Ok(e) = h.events.recv_timeout(Duration::from_millis(100))
                    && f(&e)
                {
                    return;
                }
            }
            panic!("timed out waiting for {what}");
        };

        h.cmds.send(Cmd::Connect { ip, label: "PEER-L".into() }).unwrap();
        wait_for("connected", &|e| matches!(e, Event::State(State::Connected { callsign, .. }) if callsign == "PEER-L"));
        wait_for("info", &|e| matches!(e, Event::Info(t) if t == "peer info\nline 2"));
        wait_for("chat", &|e| matches!(e, Event::Chat(t) if t == "PEER-L>hi there"));
        wait_for("rx start", &|e| matches!(e, Event::Receiving(true)));
        std::thread::sleep(Duration::from_millis(300));
        assert!(shared.queued() >= 4 * PACKET_SAMPLES, "queued {}", shared.queued());
        wait_for("rx end", &|e| matches!(e, Event::Receiving(false)));

        h.cmds.send(Cmd::Chat("hello back".into())).unwrap();
        h.cmds.send(Cmd::Ptt(true)).unwrap();
        wait_for("tx on", &|e| matches!(e, Event::Transmitting(true)));
        // 8.5 packets of a tone; the half packet is padded on unkey.
        let tone: Vec<i16> = (0..PACKET_SAMPLES * 17 / 2).map(|i| ((i as f32 * 0.785).sin() * 8000.0) as i16).collect();
        for chunk in tone.chunks(160) {
            mic_tx.send(chunk.to_vec()).unwrap();
        }
        std::thread::sleep(Duration::from_millis(300));
        h.cmds.send(Cmd::Ptt(false)).unwrap();
        wait_for("tx off", &|e| matches!(e, Event::Transmitting(false)));
        std::thread::sleep(Duration::from_millis(300));
        h.cmds.send(Cmd::Disconnect).unwrap();
        wait_for("idle", &|e| matches!(e, Event::State(State::Idle)));
        std::thread::sleep(Duration::from_millis(500));
    }
}
