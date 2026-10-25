//! Application state, the directory worker, and keyboard handling.

use crate::audio::Audio;
use crate::config::Config;
use crate::directory::{self, Kind, Station, Status};
use crate::net::Net;
use crate::qso::{self, Cmd, Event, State};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::widgets::TableState;
use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

const RELOGIN: Duration = Duration::from_secs(5 * 60);
/// How often to check that the proxy connection is still up.
const TICK: Duration = Duration::from_secs(10);
const LOG_LINES: usize = 500;
/// Terminals report a held key's auto-repeat as release+press pairs (every
/// 25 ms at 40/s), so only a release with no new press within this long ends TX.
const RELEASE_GRACE: Duration = Duration::from_millis(150);
/// In tap mode, Space events closer together than this are auto-repeat of
/// one keypress, not a second tap.
const REPEAT_GAP: Duration = Duration::from_millis(400);

pub enum DirCmd {
    Status(Status),
    Refresh,
    Quit,
}

pub enum DirEvent {
    LoggedIn(Status, String),
    Stations(Vec<Station>),
    Error(String),
    Done,
}

/// Keeps us registered with the directory and fetches the station list.
/// With a proxy, a dropped connection is replaced and we log in again
/// through the new one (other stations see us at the proxy's IP).
pub fn directory_worker(cfg: Config, net: Net) -> (Sender<DirCmd>, Receiver<DirEvent>) {
    let (cmd_tx, cmd_rx) = channel();
    let (ev_tx, ev_rx) = channel();
    std::thread::spawn(move || {
        let mut status = Status::Online;
        let mut fetch = true;
        loop {
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
                Err(e) => drop(ev_tx.send(DirEvent::Error(format!("{e:#}")))),
            }
            fetch = false;
            let next_login = Instant::now() + RELOGIN;
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

    fn matches(self, k: Kind) -> bool {
        match self {
            Filter::All => true,
            Filter::Repeaters => k == Kind::Repeater,
            Filter::Links => k == Kind::Link,
            Filter::Conferences => k == Kind::Conference,
            Filter::Users => k == Kind::User,
        }
    }
}

#[derive(PartialEq)]
pub enum Mode {
    Normal,
    Search,
    Chat,
    Node,
}

#[derive(Clone, Copy)]
pub enum LogKind {
    System,
    Chat,
}

pub struct App {
    pub cfg: Config,
    pub audio: Audio,
    pub qso: qso::Handle,
    dir_cmds: Sender<DirCmd>,
    dir_events: Receiver<DirEvent>,

    pub stations: Vec<Station>,
    pub visible: Vec<usize>,
    pub table: TableState,
    pub filter: Filter,
    pub search: String,
    pub mode: Mode,
    pub input: String,

    pub dir_status: String,
    pub dir_ok: bool,
    pub state: State,
    pub remote_info: String,
    pub log: VecDeque<(LogKind, String)>,
    pub receiving: bool,
    pub tx_since: Option<Instant>,
    pub hold_ptt: bool,
    release_at: Option<Instant>,
    last_space: Option<Instant>,
    pub quitting: bool,
}

impl App {
    pub fn new(cfg: Config, net: Net, audio: Audio, qso: qso::Handle, hold_ptt: bool) -> Self {
        let (dir_cmds, dir_events) = directory_worker(cfg.clone(), net);
        let mut app = App {
            cfg,
            qso,
            dir_cmds,
            dir_events,
            stations: Vec::new(),
            visible: Vec::new(),
            table: TableState::default(),
            filter: Filter::All,
            search: String::new(),
            mode: Mode::Normal,
            input: String::new(),
            dir_status: "Logging in…".into(),
            dir_ok: false,
            state: State::Idle,
            remote_info: String::new(),
            log: VecDeque::new(),
            receiving: false,
            tx_since: None,
            hold_ptt,
            release_at: None,
            last_space: None,
            quitting: false,
            audio,
        };
        app.push(LogKind::System, format!("Audio: {}", app.audio.description));
        app.push(
            LogKind::System,
            if hold_ptt { "Hold Space to talk.".into() } else { "Press Space to start talking, Space again to stop.".into() },
        );
        app
    }

    pub fn push(&mut self, kind: LogKind, text: String) {
        if let LogKind::System = kind {
            crate::log!("ui: {text}");
        }
        for line in text.lines() {
            self.log.push_back((kind, line.to_string()));
        }
        while self.log.len() > LOG_LINES {
            self.log.pop_front();
        }
    }

    pub fn selected(&self) -> Option<&Station> {
        self.table.selected().and_then(|i| self.visible.get(i)).map(|&i| &self.stations[i])
    }

    pub fn refilter(&mut self) {
        let keep = self.selected().map(|s| s.callsign.clone());
        let q = self.search.to_lowercase();
        self.visible = (0..self.stations.len())
            .filter(|&i| {
                let s = &self.stations[i];
                self.filter.matches(s.kind())
                    && (q.is_empty()
                        || s.callsign.to_lowercase().contains(&q)
                        || s.description.to_lowercase().contains(&q)
                        || s.id.to_string() == q)
            })
            .collect();
        let idx = keep
            .and_then(|c| self.visible.iter().position(|&i| self.stations[i].callsign == c))
            .or((!self.visible.is_empty()).then_some(0));
        self.table.select(idx);
    }

    /// Handle background events. Returns false once it's safe to exit.
    pub fn poll(&mut self) -> bool {
        if self.release_at.is_some_and(|t| Instant::now() >= t) {
            self.release_at = None;
            self.ptt(false);
        }
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
                    self.refilter();
                    self.push(LogKind::System, format!("Directory: {n} stations."));
                }
                DirEvent::Error(e) => {
                    self.dir_ok = false;
                    self.dir_status = "Error".into();
                    self.push(LogKind::System, format!("Directory: {e}"));
                }
                DirEvent::Done => return false,
            }
        }
        while let Ok(e) = self.qso.events.try_recv() {
            match e {
                Event::State(s) => {
                    let status = if matches!(s, State::Connected { .. }) { Status::Busy } else { Status::Online };
                    if !matches!(s, State::Connected { .. }) {
                        self.remote_info.clear();
                    }
                    let _ = self.dir_cmds.send(DirCmd::Status(status));
                    self.state = s;
                }
                Event::Info(t) => self.remote_info = t,
                Event::Chat(t) => self.push(LogKind::Chat, t),
                Event::Receiving(r) => self.receiving = r,
                Event::Transmitting(t) => self.tx_since = t.then(Instant::now),
                Event::Log(t) => self.push(LogKind::System, t),
            }
        }
        true
    }

    fn connect(&mut self, s: Station) {
        if s.ip.is_unspecified() {
            self.push(LogKind::System, format!("{} has no address in the directory.", s.callsign));
            return;
        }
        let label = format!("{} ({})", s.callsign, s.id);
        let _ = self.qso.cmds.send(Cmd::Connect { ip: s.ip, label });
    }

    fn ptt(&mut self, on: bool) {
        if on && !matches!(self.state, State::Connected { .. }) {
            self.push(LogKind::System, "Connect to a station before talking.".into());
            return;
        }
        let _ = self.qso.cmds.send(Cmd::Ptt(on));
    }

    pub fn quit(&mut self) {
        if !self.quitting {
            self.quitting = true;
            let _ = self.qso.cmds.send(Cmd::Quit);
            let _ = self.dir_cmds.send(DirCmd::Quit);
            self.dir_status = "Logging off…".into();
        }
    }

    fn move_sel(&mut self, delta: isize) {
        if self.visible.is_empty() {
            return;
        }
        let cur = self.table.selected().unwrap_or(0) as isize;
        let max = self.visible.len() as isize - 1;
        self.table.select(Some((cur + delta).clamp(0, max) as usize));
    }

    pub fn key(&mut self, k: KeyEvent) {
        // Space release only matters in hold-to-talk mode.
        if k.code == KeyCode::Char(' ') && self.mode == Mode::Normal && self.hold_ptt {
            match k.kind {
                KeyEventKind::Press | KeyEventKind::Repeat => {
                    self.release_at = None;
                    if !self.qso.transmitting.load(Ordering::Relaxed) {
                        self.ptt(true);
                    }
                }
                KeyEventKind::Release => self.release_at = Some(Instant::now() + RELEASE_GRACE),
            }
            return;
        }
        if k.kind == KeyEventKind::Release {
            return;
        }
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            self.quit();
            return;
        }
        match self.mode {
            Mode::Normal => self.key_normal(k),
            _ => self.key_input(k),
        }
    }

    fn key_normal(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Char('q') => self.quit(),
            KeyCode::Char(' ') => {
                let now = Instant::now();
                let fresh = self.last_space.is_none_or(|t| now - t > REPEAT_GAP);
                self.last_space = Some(now);
                if fresh {
                    let on = !self.qso.transmitting.load(Ordering::Relaxed);
                    self.ptt(on);
                }
            }
            KeyCode::Esc => {
                if self.qso.transmitting.load(Ordering::Relaxed) {
                    self.ptt(false);
                } else if !self.search.is_empty() {
                    self.search.clear();
                    self.refilter();
                }
            }
            KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),
            KeyCode::PageDown => self.move_sel(20),
            KeyCode::PageUp => self.move_sel(-20),
            KeyCode::Home | KeyCode::Char('g') => self.move_sel(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.move_sel(isize::MAX / 2),
            KeyCode::Tab | KeyCode::BackTab => {
                let i = Filter::ALL.iter().position(|f| *f == self.filter).unwrap();
                let n = Filter::ALL.len();
                let i = if k.code == KeyCode::Tab { (i + 1) % n } else { (i + n - 1) % n };
                self.filter = Filter::ALL[i];
                self.refilter();
            }
            KeyCode::Enter => {
                if let Some(s) = self.selected().cloned() {
                    self.connect(s);
                }
            }
            KeyCode::Char('d') => {
                let _ = self.qso.cmds.send(Cmd::Disconnect);
            }
            KeyCode::Char('r') => {
                self.push(LogKind::System, "Refreshing the station list…".into());
                let _ = self.dir_cmds.send(DirCmd::Refresh);
            }
            KeyCode::Char('/') => {
                self.mode = Mode::Search;
                self.input = self.search.clone();
            }
            KeyCode::Char('c') => {
                self.mode = Mode::Chat;
                self.input.clear();
            }
            KeyCode::Char('n') => {
                self.mode = Mode::Node;
                self.input.clear();
            }
            _ => {}
        }
    }

    fn key_input(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Esc => {
                if self.mode == Mode::Search {
                    self.search.clear();
                    self.refilter();
                }
                self.mode = Mode::Normal;
            }
            KeyCode::Enter => {
                let text = std::mem::take(&mut self.input);
                match self.mode {
                    Mode::Chat if !text.trim().is_empty() => drop(self.qso.cmds.send(Cmd::Chat(text.trim().into()))),
                    Mode::Node => self.connect_node(text.trim()),
                    _ => {}
                }
                if self.mode == Mode::Search {
                    self.input = self.search.clone();
                }
                self.mode = Mode::Normal;
            }
            KeyCode::Backspace => {
                self.input.pop();
                if self.mode == Mode::Search {
                    self.search = self.input.clone();
                    self.refilter();
                }
            }
            KeyCode::Char(c) => {
                if self.mode == Mode::Node && !c.is_ascii_digit() {
                    return;
                }
                self.input.push(c);
                if self.mode == Mode::Search {
                    self.search = self.input.clone();
                    self.refilter();
                }
            }
            _ => {}
        }
    }

    fn connect_node(&mut self, text: &str) {
        let Ok(id) = text.parse::<u32>() else { return };
        match self.stations.iter().find(|s| s.id == id).cloned() {
            Some(s) => self.connect(s),
            None => self.push(LogKind::System, format!("Node {id} isn't online in the directory.")),
        }
    }
}
