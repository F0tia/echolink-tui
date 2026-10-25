//! Terminal front end state and keyboard handling over a `Session`.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use echolink::directory::Station;
use echolink::session::{Filter, LogKind, Session, Update};
use ratatui::widgets::TableState;
use std::time::{Duration, Instant};

/// Terminals report a held key's auto-repeat as release+press pairs (every
/// 25 ms at 40/s), so only a release with no new press within this long ends TX.
const RELEASE_GRACE: Duration = Duration::from_millis(150);
/// In tap mode, Space events closer together than this are auto-repeat of
/// one keypress, not a second tap.
const REPEAT_GAP: Duration = Duration::from_millis(400);

#[derive(PartialEq)]
pub enum Mode {
    Normal,
    Search,
    Chat,
    Node,
}

pub struct App {
    pub s: Session,
    pub visible: Vec<usize>,
    pub table: TableState,
    pub filter: Filter,
    pub search: String,
    pub mode: Mode,
    pub input: String,
    pub hold_ptt: bool,
    release_at: Option<Instant>,
    last_space: Option<Instant>,
}

impl App {
    pub fn new(mut s: Session, hold_ptt: bool) -> Self {
        s.push(
            LogKind::System,
            if hold_ptt { "Hold Space to talk.".into() } else { "Press Space to start talking, Space again to stop.".into() },
        );
        App {
            s,
            visible: Vec::new(),
            table: TableState::default(),
            filter: Filter::All,
            search: String::new(),
            mode: Mode::Normal,
            input: String::new(),
            hold_ptt,
            release_at: None,
            last_space: None,
        }
    }

    pub fn selected(&self) -> Option<&Station> {
        self.table.selected().and_then(|i| self.visible.get(i)).map(|&i| &self.s.stations[i])
    }

    pub fn refilter(&mut self) {
        let keep = self.selected().map(|s| s.callsign.clone());
        self.visible = self.s.matching(self.filter, &self.search);
        let idx = keep
            .and_then(|c| self.visible.iter().position(|&i| self.s.stations[i].callsign == c))
            .or((!self.visible.is_empty()).then_some(0));
        self.table.select(idx);
    }

    /// Handle background events. Returns false once it's safe to exit.
    pub fn poll(&mut self) -> bool {
        if self.release_at.is_some_and(|t| Instant::now() >= t) {
            self.release_at = None;
            self.s.ptt(false);
        }
        match self.s.poll() {
            Update::StationsChanged => {
                self.refilter();
                true
            }
            Update::Nothing => true,
            Update::Finished => false,
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
                    if !self.s.transmitting() {
                        self.s.ptt(true);
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
            self.s.quit();
            return;
        }
        match self.mode {
            Mode::Normal => self.key_normal(k),
            _ => self.key_input(k),
        }
    }

    fn key_normal(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Char('q') => self.s.quit(),
            KeyCode::Char(' ') => {
                let now = Instant::now();
                let fresh = self.last_space.is_none_or(|t| now - t > REPEAT_GAP);
                self.last_space = Some(now);
                if fresh {
                    let on = !self.s.transmitting();
                    self.s.ptt(on);
                }
            }
            KeyCode::Esc => {
                if self.s.transmitting() {
                    self.s.ptt(false);
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
                if let Some(st) = self.selected().cloned() {
                    self.s.connect(&st);
                }
            }
            KeyCode::Char('d') => self.s.disconnect(),
            KeyCode::Char('r') => self.s.refresh(),
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
                    Mode::Chat => self.s.chat(&text),
                    Mode::Node => {
                        if let Ok(id) = text.trim().parse() {
                            self.s.connect_node(id);
                        }
                    }
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
}
