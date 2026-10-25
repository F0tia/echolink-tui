//! A running EchoLink station, independent of any user interface: the
//! directory worker, the station list, call state, and the message log.
//! Front ends call `poll()` regularly and the action methods on input.

use crate::audio::{self, Audio};
use crate::config::Config;
use crate::directory::{self, Kind, Station, Status};
use crate::net::Net;
use crate::qso::{self, Cmd, Event, State};
use anyhow::Result;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

const RELOGIN: Duration = Duration::from_secs(5 * 60);
/// How often to check that the proxy connection is still up.
/// How soon to try again after a login that never got an answer.
const RETRY: Duration = Duration::from_secs(15);
const TICK: Duration = Duration::from_secs(10);
const LOG_LINES: usize = 500;

enum DirCmd {
    Status(Status),
    Refresh,
    Quit,
}

enum DirEvent {
    LoggedIn(Status, String),
    Stations(Vec<Station>),
    Error(String),
    Done,
}

/// Keeps us registered with the directory and fetches the station list.
/// With a proxy, a dropped connection is replaced and we log in again
/// through the new one (other stations see us at the proxy's IP).
fn directory_worker(cfg: Config, net: Net) -> (Sender<DirCmd>, Receiver<DirEvent>) {
    let (cmd_tx, cmd_rx) = channel();
    let (ev_tx, ev_rx) = channel();
    std::thread::spawn(move || {
        let mut status = Status::Online;
        let mut fetch = true;
        loop {
            let mut wait = RELOGIN;
            let result = directory::login(&net, &cfg.servers, &cfg.callsign, &cfg.password, &cfg.location, status);
            crate::log!("directory login as {} ({status:?}): {}", cfg.callsign, result.as_ref().map(|_| "OK".to_string()).unwrap_or_else(|e| format!("{e:#}")));
            match result {
                Ok(()) => {
                    let _ = ev_tx.send(DirEvent::LoggedIn(status, net.describe()));
                    if fetch {
                        match directory::stations(&net, &cfg.servers) {
                            Ok(s) => {
                                crate::log!("station list: {} stations", s.len());
                                if let Some(w) = directory::stub_warning(&s) {
                                    crate::log!("WARNING: {w}");
                                    let _ = ev_tx.send(DirEvent::Error(w));
                                }
                                let _ = ev_tx.send(DirEvent::Stations(s));
                            }
                            Err(e) => {
                                crate::log!("station list failed: {e:#}");
                                let _ = ev_tx.send(DirEvent::Error(format!("{e:#}")));
                            }
                        }
                    }
                }
                Err(e) => {
                    let _ = ev_tx.send(DirEvent::Error(format!("{e:#}")));
                    if e.downcast_ref::<directory::Refused>().is_none() {
                        // No answer at all: try another proxy, and soon.
                        if let Net::Proxy(m) = &net {
                            m.abandon("couldn't reach the directory");
                        }
                        wait = RETRY;
                    }
                }
            }
            // Keep asking for the list until we have one.
            fetch &= wait == RETRY;
            let next_login = Instant::now() + wait;
            loop {
                match cmd_rx.recv_timeout(TICK) {
                    Ok(DirCmd::Status(s)) => {
                        status = s;
                        break;
                    }
                    Ok(DirCmd::Refresh) => {
                        fetch = true;
                        break;
                    }
                    Ok(DirCmd::Quit) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        let _ = directory::login(&net, &cfg.servers, &cfg.callsign, &cfg.password, &cfg.location, Status::Off);
                        let _ = ev_tx.send(DirEvent::Done);
                        return;
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                }
                if let Net::Proxy(m) = &net
                    && m.is_down()
                {
                    crate::log!("proxy connection is down; reconnecting");
                    let _ = ev_tx.send(DirEvent::Error("Proxy connection lost; reconnecting…".into()));
                    fetch = true;
                    break;
                }
                if Instant::now() >= next_login {
                    break;
                }
            }
        }
    });
    (cmd_tx, ev_rx)
}

#[derive(Clone, Copy, PartialEq)]
pub enum Filter {
    All,
    Repeaters,
    Links,
    Conferences,
    Users,
}

impl Filter {
    pub const ALL: [Filter; 5] = [Filter::All, Filter::Repeaters, Filter::Links, Filter::Conferences, Filter::Users];

    pub fn label(self) -> &'static str {
        match self {
            Filter::All => "All",
            Filter::Repeaters => "Repeaters",
            Filter::Links => "Links",
            Filter::Conferences => "Conferences",
            Filter::Users => "Users",
        }
    }

    pub fn matches(self, k: Kind) -> bool {
        match self {
            Filter::All => true,
            Filter::Repeaters => k == Kind::Repeater,
            Filter::Links => k == Kind::Link,
            Filter::Conferences => k == Kind::Conference,
            Filter::Users => k == Kind::User,
        }
    }
}

/// What `Session::poll` found.
#[derive(PartialEq)]
pub enum Update {
    Nothing,
    StationsChanged,
    /// Logged off after `quit()`; safe to exit.
    Finished,
}

#[derive(Clone, Copy, PartialEq)]
pub enum LogKind {
    System,
    Chat,
}

/// Called from a background thread for incoming calls and chat from others,
/// so it works even while the front end isn't drawing (e.g. window hidden).
pub type Notifier = Box<dyn Fn(&str) + Send>;

pub struct Session {
    pub cfg: Config,
    pub audio: Audio,
    cmds: Sender<Cmd>,
    events: Receiver<Event>,
    transmitting: Arc<AtomicBool>,
    dir_cmds: Sender<DirCmd>,
    dir_events: Receiver<DirEvent>,

    /// Sorted by callsign.
    pub stations: Vec<Station>,
    pub dir_status: String,
    pub dir_ok: bool,
    pub state: State,
    pub remote_info: String,
    pub log: VecDeque<(LogKind, String)>,
    pub receiving: bool,
    pub tx_since: Option<Instant>,
    pub quitting: bool,
}

impl Session {
    /// Open the sound card, start the network engine, and log in.
    /// `client` names the program in the info text other stations see.
    pub fn start(cfg: Config, client: &str, notifier: Option<Notifier>) -> Result<Session> {
        crate::log!("{client} {} starting as {}", env!("CARGO_PKG_VERSION"), cfg.callsign);
        let transmitting = Arc::new(AtomicBool::new(false));
        let (mic_tx, mic_rx) = channel();
        let audio = audio::open(mic_tx, transmitting.clone())?;
        crate::log!("audio: {}", audio.description);
        let me = qso::Station {
            callsign: cfg.callsign.clone(),
            name: cfg.name.clone(),
            info: format!("{}\n{}\n{}\n{client} {} (Linux)", cfg.callsign, cfg.name, cfg.location, env!("CARGO_PKG_VERSION")),
            accept_incoming: cfg.accept_incoming,
        };
        let (udp_tx, udp_rx) = channel();
        let net = Net::from_config(&cfg, udp_tx);
        if let Net::Proxy(_) = net {
            crate::log!("using an EchoLink proxy ({})", cfg.proxy.as_deref().unwrap_or(""));
        }
        let qso = qso::spawn(me, audio.shared.clone(), mic_rx, transmitting, &net, udp_rx)?;
        let (dir_cmds, dir_events) = directory_worker(cfg.clone(), net);
        let (ev_tx, events) = channel();
        let mine = format!("{}>", cfg.callsign);
        let qso_events = qso.events;
        std::thread::spawn(move || {
            for e in qso_events {
                if let Some(n) = &notifier {
                    match &e {
                        Event::Incoming(call) => n(&format!("Incoming EchoLink call from {call}")),
                        Event::Chat(t) if !t.starts_with(&mine) => n(t),
                        _ => {}
                    }
                }
                if ev_tx.send(e).is_err() {
                    return;
                }
            }
        });
        let mut s = Session {
            cfg,
            audio,
            cmds: qso.cmds,
            events,
            transmitting: qso.transmitting,
            dir_cmds,
            dir_events,
            stations: Vec::new(),
            dir_status: "Logging in…".into(),
            dir_ok: false,
            state: State::Idle,
            remote_info: String::new(),
            log: VecDeque::new(),
            receiving: false,
            tx_since: None,
            quitting: false,
        };
        s.push(LogKind::System, format!("Audio: {}", s.audio.description));
        Ok(s)
    }

    pub fn push(&mut self, kind: LogKind, text: String) {
        if kind == LogKind::System {
            crate::log!("ui: {text}");
        }
        for line in text.lines() {
            self.log.push_back((kind, line.to_string()));
        }
        while self.log.len() > LOG_LINES {
            self.log.pop_front();
        }
    }

    /// Handle background events.
    pub fn poll(&mut self) -> Update {
        let mut list_changed = false;
        while let Ok(e) = self.dir_events.try_recv() {
            match e {
                DirEvent::LoggedIn(s, route) => {
                    self.dir_ok = true;
                    let s = match s {
                        Status::Online => "Online",
                        Status::Busy => "Busy",
                        Status::Off => "Off",
                    };
                    self.dir_status = if route == "direct" { s.into() } else { format!("{s} · {route}") };
                }
                DirEvent::Stations(mut list) => {
                    list.sort_by(|a, b| a.callsign.cmp(&b.callsign));
                    let n = list.len();
                    self.stations = list;
                    list_changed = true;
                    self.push(LogKind::System, format!("Directory: {n} stations."));
                }
                DirEvent::Error(e) => {
                    self.dir_ok = false;
                    self.dir_status = "Error".into();
                    self.push(LogKind::System, format!("Directory: {e}"));
                }
                DirEvent::Done => return Update::Finished,
            }
        }
        while let Ok(e) = self.events.try_recv() {
            match e {
                Event::State(s) => {
                    let connected = matches!(s, State::Connected { .. });
                    if !connected {
                        self.remote_info.clear();
                    }
                    let _ = self.dir_cmds.send(DirCmd::Status(if connected { Status::Busy } else { Status::Online }));
                    self.state = s;
                }
                Event::Info(t) => self.remote_info = t,
                Event::Chat(t) => self.push(LogKind::Chat, t),
                Event::Receiving(r) => self.receiving = r,
                Event::Transmitting(t) => self.tx_since = t.then(Instant::now),
                Event::Log(t) => self.push(LogKind::System, t),
                Event::Incoming(_) => {}
            }
        }
        if list_changed { Update::StationsChanged } else { Update::Nothing }
    }

    /// Indices into `stations` matching a filter and search text
    /// (callsign, location, or exact node number).
    pub fn matching(&self, filter: Filter, query: &str) -> Vec<usize> {
        let q = query.trim().to_lowercase();
        (0..self.stations.len())
            .filter(|&i| {
                let s = &self.stations[i];
                filter.matches(s.kind())
                    && (q.is_empty()
                        || s.callsign.to_lowercase().contains(&q)
                        || s.description.to_lowercase().contains(&q)
                        || s.id.to_string() == q)
            })
            .collect()
    }

    pub fn find(&self, callsign: &str) -> Option<&Station> {
        self.stations.iter().find(|s| s.callsign.eq_ignore_ascii_case(callsign))
    }

    pub fn connected(&self) -> bool {
        matches!(self.state, State::Connected { .. })
    }

    pub fn transmitting(&self) -> bool {
        self.transmitting.load(Ordering::Relaxed)
    }

    pub fn connect(&mut self, s: &Station) {
        if s.ip.is_unspecified() {
            self.push(LogKind::System, format!("{} has no address in the directory.", s.callsign));
            return;
        }
        let label = format!("{} ({})", s.callsign, s.id);
        let _ = self.cmds.send(Cmd::Connect { ip: s.ip, label });
    }

    pub fn connect_node(&mut self, id: u32) {
        match self.stations.iter().find(|s| s.id == id).cloned() {
            Some(s) => self.connect(&s),
            None => self.push(LogKind::System, format!("Node {id} isn't online in the directory.")),
        }
    }

    pub fn disconnect(&mut self) {
        let _ = self.cmds.send(Cmd::Disconnect);
    }

    pub fn ptt(&mut self, on: bool) {
        if on && !self.connected() {
            self.push(LogKind::System, "Connect to a station before talking.".into());
            return;
        }
        let _ = self.cmds.send(Cmd::Ptt(on));
    }

    pub fn chat(&mut self, text: &str) {
        let text = text.trim();
        if !text.is_empty() {
            let _ = self.cmds.send(Cmd::Chat(text.into()));
        }
    }

    pub fn refresh(&mut self) {
        self.push(LogKind::System, "Refreshing the station list…".into());
        let _ = self.dir_cmds.send(DirCmd::Refresh);
    }

    /// Disconnect and log off; `poll()` returns `Err` when that's done.
    pub fn quit(&mut self) {
        if !self.quitting {
            self.quitting = true;
            let _ = self.cmds.send(Cmd::Quit);
            let _ = self.dir_cmds.send(DirCmd::Quit);
            self.dir_status = "Logging off…".into();
        }
    }
}
