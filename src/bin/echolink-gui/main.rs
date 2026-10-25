//! EchoLink desktop app: the same station core as echolink-tui, with a
//! window, a big push-to-talk button and a settings dialog.

mod settings;

use eframe::egui::{self, Color32, RichText};
use echolink::config::{self, Config};
use echolink::directory::Station;
use echolink::qso::{State, TX_LIMIT};
use echolink::session::{Filter, LogKind, Session, Update};
use egui_extras::{Column, TableBuilder};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const ECHOTEST_NODE: u32 = 9999;
const RESTART_ENV: &str = "ECHOLINK_GUI_RESTARTED";

fn main() -> eframe::Result {
    echolink::log::init();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("EchoLink")
            .with_app_id("echolink-gui")
            .with_inner_size([1150.0, 720.0])
            .with_min_inner_size([800.0, 520.0]),
        // Don't block on vsync: a window on a hidden workspace gets no frame
        // callbacks, which would otherwise stall the whole UI thread.
        vsync: false,
        ..Default::default()
    };
    eframe::run_native("EchoLink", options, Box::new(|cc| Ok(Box::new(Gui::new(cc)))))
}

struct Gui {
    session: Option<Session>,
    start_error: Option<String>,
    settings: Option<settings::Form>,
    filter: Filter,
    search: String,
    favorites_only: bool,
    visible: Vec<usize>,
    selected: Option<String>,
    node_input: String,
    chat_input: String,
    ptt_held_by_mouse: bool,
    ptt_on: bool,
    closing: bool,
    /// Whether the window has focus; notifications only pop up when it doesn't.
    focused: Arc<AtomicBool>,
}

impl Gui {
    fn new(cc: &eframe::CreationContext) -> Gui {
        cc.egui_ctx.set_zoom_factor(1.1);
        let mut gui = Gui {
            session: None,
            start_error: None,
            settings: None,
            filter: Filter::All,
            search: String::new(),
            favorites_only: false,
            visible: Vec::new(),
            selected: None,
            node_input: String::new(),
            chat_input: String::new(),
            ptt_held_by_mouse: false,
            ptt_on: false,
            closing: false,
            focused: Arc::new(AtomicBool::new(true)),
        };
        match config::load() {
            Ok(cfg) => {
                if std::env::var_os(RESTART_ENV).is_some() {
                    // Give the previous instance a moment to release its ports.
                    std::thread::sleep(Duration::from_millis(1500));
                }
                gui.start(cfg);
            }
            Err(_) => gui.settings = Some(settings::Form::new(None)),
        }
        gui
    }

    fn start(&mut self, cfg: Config) {
        let focused = self.focused.clone();
        let notifier: echolink::session::Notifier = Box::new(move |text| {
            if !focused.load(Ordering::Relaxed) {
                notify(text);
            }
        });
        match Session::start(cfg, "echolink-gui", Some(notifier)) {
            Ok(s) => {
                self.session = Some(s);
                self.start_error = None;
            }
            Err(e) => self.start_error = Some(format!("{e:#}")),
        }
    }

    fn favorites(&self) -> &[String] {
        self.session.as_ref().map(|s| s.cfg.favorites.as_slice()).unwrap_or(&[])
    }

    fn refilter(&mut self) {
        let Some(s) = &self.session else { return };
        let favs = &s.cfg.favorites;
        let mut v = s.matching(self.filter, &self.search);
        if self.favorites_only {
            v.retain(|&i| favs.iter().any(|f| f == &s.stations[i].callsign));
        }
        // Favorites first, then alphabetical (the list is already sorted).
        v.sort_by_key(|&i| !favs.iter().any(|f| f == &s.stations[i].callsign));
        self.visible = v;
    }

    fn toggle_favorite(&mut self, call: &str) {
        let Some(s) = &mut self.session else { return };
        let favs = &mut s.cfg.favorites;
        match favs.iter().position(|f| f == call) {
            Some(i) => drop(favs.remove(i)),
            None => favs.push(call.to_string()),
        }
        // Save just the favorites onto whatever is on disk.
        if let Ok(mut on_disk) = config::load() {
            on_disk.favorites = s.cfg.favorites.clone();
            if let Err(e) = config::save(&on_disk) {
                s.push(LogKind::System, format!("Couldn't save favorites: {e:#}"));
            }
        }
        self.refilter();
    }

    /// Disconnect and log off, waiting (briefly) for it to finish so it
    /// doesn't depend on the window drawing another frame.
    fn shutdown(&mut self) {
        let Some(s) = &mut self.session else { return };
        echolink::log!("closing: disconnecting and logging off");
        s.quit();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if s.poll() == Update::Finished {
                echolink::log!("closing: logged off");
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        echolink::log!("closing: log-off timed out");
    }

    fn selected_station(&self) -> Option<&Station> {
        let s = self.session.as_ref()?;
        s.find(self.selected.as_deref()?)
    }
}

fn notify(text: &str) {
    let _ = std::process::Command::new("notify-send")
        .args(["--app-name=EchoLink", "--icon=audio-input-microphone", "EchoLink", text])
        .spawn();
}

impl eframe::App for Gui {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.request_repaint_after(Duration::from_millis(50));

        // Background events.
        if let Some(s) = &mut self.session
            && s.poll() == Update::StationsChanged
        {
            self.refilter();
        }
        self.focused.store(ctx.input(|i| i.focused), Ordering::Relaxed);

        // Closing the window disconnects and logs off first.
        if ctx.input(|i| i.viewport().close_requested()) && !self.closing {
            self.closing = true;
            self.shutdown();
        }

        // Settings dialog (also the first-run screen).
        if let Some(form) = &mut self.settings {
            match form.show(ctx) {
                settings::Outcome::Open => {}
                settings::Outcome::Cancelled => self.settings = None,
                settings::Outcome::Saved(cfg) => {
                    self.settings = None;
                    match &mut self.session {
                        Some(_) => {
                            // Network settings take effect on a fresh start.
                            self.shutdown();
                            if let Ok(exe) = std::env::current_exe() {
                                let _ = std::process::Command::new(exe).env(RESTART_ENV, "1").spawn();
                            }
                            self.closing = true;
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            return;
                        }
                        None => self.start(cfg),
                    }
                }
            }
        }

        self.top_bar(ctx);
        self.status_bar(ctx);

        if self.session.is_none() {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(80.0);
                    if let Some(e) = &self.start_error {
                        ui.heading("EchoLink couldn't start");
                        ui.add_space(8.0);
                        ui.colored_label(ui.visuals().error_fg_color, e);
                        ui.add_space(12.0);
                        if ui.button("Open settings").clicked() {
                            self.settings = Some(settings::Form::new(config::load().ok().as_ref()));
                        }
                    } else {
                        ui.heading("Set up your station to begin.");
                    }
                });
            });
            return;
        }

        // Leave the right-hand panel room for its controls, even in a narrow tiled window.
        let width = ctx.screen_rect().width();
        let list_max = (width - 400.0).max(360.0);
        egui::SidePanel::left("stations")
            .resizable(true)
            .default_width((width * 0.55).min(600.0))
            .min_width(360.0_f32.min(list_max))
            .max_width(list_max)
            .show(ctx, |ui| {
                self.station_list(ui);
            });
        egui::CentralPanel::default().show(ctx, |ui| self.qso_panel(ui));
        self.handle_ptt(ctx);
    }
}

impl Gui {
    fn top_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("EchoLink").size(20.0).strong());
                if let Some(s) = &self.session {
                    ui.separator();
                    ui.label(RichText::new(&s.cfg.callsign).size(16.0).strong());
                    ui.separator();
                    // Painted, since the default font has no filled-circle glyph.
                    let color = if s.dir_ok { Color32::from_rgb(60, 180, 90) } else { Color32::from_rgb(220, 160, 40) };
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
                    ui.painter().circle_filled(rect.center(), 5.0, color);
                    ui.label(&s.dir_status);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("⚙ Settings").clicked() {
                        let current = config::load().ok();
                        self.settings = Some(settings::Form::new(current.as_ref()));
                    }
                    if let Some(s) = &mut self.session
                        && ui.button("⟳ Refresh list").clicked()
                    {
                        s.refresh();
                    }
                });
            });
            ui.add_space(4.0);
        });
    }

    fn status_bar(&self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            let line = self
                .session
                .as_ref()
                .and_then(|s| s.log.iter().rev().find(|(k, _)| *k == LogKind::System).map(|(_, t)| t.as_str()))
                .unwrap_or("");
            ui.label(RichText::new(line).weak());
        });
    }

    fn station_list(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        let mut changed = false;
        ui.horizontal(|ui| {
            changed |= ui
                .add(egui::TextEdit::singleline(&mut self.search).hint_text("🔍 Search callsign, place or node #").desired_width(260.0))
                .changed();
            if !self.search.is_empty() && ui.small_button("✖").clicked() {
                self.search.clear();
                changed = true;
            }
            changed |= ui.toggle_value(&mut self.favorites_only, "★ Favorites").changed();
        });
        ui.horizontal(|ui| {
            for f in Filter::ALL {
                if ui.selectable_label(self.filter == f, f.label()).clicked() {
                    self.filter = f;
                    changed = true;
                }
            }
        });
        if changed {
            self.refilter();
        }

        let Some(s) = &self.session else { return };
        if s.stations.is_empty() {
            ui.add_space(30.0);
            ui.vertical_centered(|ui| {
                ui.spinner();
                if s.dir_ok {
                    ui.label("Loading the station list…");
                } else if s.dir_status == "Error" {
                    ui.label("Couldn't reach the EchoLink directory. Trying again…");
                    if let Some((_, why)) = s.log.iter().rev().find(|(_, t)| t.starts_with("Directory: ")) {
                        ui.label(RichText::new(why.trim_start_matches("Directory: ")).small().weak());
                    }
                } else {
                    ui.label("Connecting to the EchoLink directory…");
                }
            });
            return;
        }
        ui.label(RichText::new(format!("{} of {} stations", self.visible.len(), s.stations.len())).small().weak());

        let favs: Vec<String> = self.favorites().to_vec();
        let mut clicked: Option<String> = None;
        let mut connect: Option<String> = None;
        let mut star: Option<String> = None;
        TableBuilder::new(ui)
            .striped(true)
            .sense(egui::Sense::click())
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::exact(24.0))
            .column(Column::initial(105.0))
            .column(Column::initial(70.0))
            .column(Column::remainder().clip(true))
            .column(Column::exact(44.0))
            .header(22.0, |mut h| {
                for t in ["", "Call", "Node", "Location", ""] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|body| {
                body.rows(24.0, self.visible.len(), |mut row| {
                    let st = &s.stations[self.visible[row.index()]];
                    let is_fav = favs.contains(&st.callsign);
                    row.set_selected(self.selected.as_deref() == Some(st.callsign.as_str()));
                    row.col(|ui| {
                        let icon = if is_fav { RichText::new("★").color(Color32::GOLD) } else { RichText::new("☆").weak() };
                        if ui.add(egui::Label::new(icon).sense(egui::Sense::click())).on_hover_text("Favorite").clicked() {
                            star = Some(st.callsign.clone());
                        }
                    });
                    row.col(|ui| {
                        ui.label(RichText::new(&st.callsign).strong());
                    });
                    row.col(|ui| {
                        ui.label(st.id.to_string());
                    });
                    row.col(|ui| {
                        ui.label(&st.description);
                    });
                    row.col(|ui| {
                        if st.busy {
                            ui.label(RichText::new("busy").color(Color32::from_rgb(220, 160, 40)).small());
                        }
                    });
                    let r = row.response();
                    if r.double_clicked() {
                        connect = Some(st.callsign.clone());
                    } else if r.clicked() {
                        clicked = Some(st.callsign.clone());
                    }
                });
            });
        if let Some(c) = star {
            self.toggle_favorite(&c);
        }
        if let Some(c) = clicked {
            self.selected = Some(c);
        }
        if let Some(c) = connect {
            self.selected = Some(c.clone());
            if let Some(s) = &mut self.session
                && let Some(st) = s.find(&c).cloned()
            {
                s.connect(&st);
            }
        }
    }

    fn qso_panel(&mut self, ui: &mut egui::Ui) {
        let selected = self.selected_station().cloned();
        let Some(s) = &mut self.session else { return };
        ui.add_space(6.0);

        // Who we're talking to, or the station picked in the list.
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            match s.state.clone() {
                State::Connected { callsign, name, .. } => {
                    ui.label(RichText::new("Connected").color(Color32::from_rgb(60, 180, 90)).strong());
                    ui.label(RichText::new(&callsign).size(28.0).strong());
                    let info = if s.remote_info.is_empty() { name } else { s.remote_info.clone() };
                    ui.label(info.trim());
                    ui.add_space(4.0);
                    if ui.button("Disconnect").clicked() {
                        s.disconnect();
                    }
                }
                State::Connecting { label, .. } => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(RichText::new(format!("Calling {label}…")).size(18.0));
                    });
                    if ui.button("Cancel").clicked() {
                        s.disconnect();
                    }
                }
                State::Idle => match &selected {
                    Some(st) => {
                        ui.label(RichText::new(&st.callsign).size(28.0).strong());
                        ui.label(format!("Node {}{}", st.id, if st.busy { " · busy" } else { "" }));
                        ui.label(&st.description);
                        ui.add_space(4.0);
                        if ui.add(egui::Button::new(RichText::new("📞 Connect").size(16.0))).clicked() {
                            s.connect(st);
                        }
                    }
                    None => {
                        ui.label(RichText::new("Not connected").size(18.0));
                        ui.label(RichText::new("Pick a station on the left and double-click it, or enter a node number.").weak());
                    }
                },
            }
        });

        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.node_input)
                    .hint_text("Node number")
                    .char_limit(7)
                    .desired_width(110.0),
            );
            self.node_input.retain(|c| c.is_ascii_digit());
            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (ui.button("Connect to node").clicked() || enter)
                && let Ok(id) = self.node_input.parse()
            {
                s.connect_node(id);
            }
            if ui.button("🔊 Audio test (ECHOTEST)").on_hover_text("Records what you say and plays it back").clicked() {
                s.connect_node(ECHOTEST_NODE);
            }
        });

        ui.add_space(10.0);
        self.ptt_button(ui);

        // Audio meters.
        let Some(s) = &self.session else { return };
        ui.add_space(8.0);
        let rx = s.audio.shared.rx_level.load(std::sync::atomic::Ordering::Relaxed) as f32 / 1000.0;
        let tx = s.audio.shared.tx_level.load(std::sync::atomic::Ordering::Relaxed) as f32 / 1000.0;
        ui.horizontal(|ui| {
            ui.label(RichText::new("RX").strong());
            let rx_color = if s.receiving { Color32::from_rgb(60, 180, 90) } else { Color32::from_gray(120) };
            ui.add(egui::ProgressBar::new(rx).fill(rx_color).desired_width(ui.available_width()));
        });
        ui.horizontal(|ui| {
            ui.label(RichText::new("TX").strong());
            ui.add(egui::ProgressBar::new(tx).fill(Color32::from_rgb(210, 60, 50)).desired_width(ui.available_width()));
        });

        // Messages and chat.
        ui.add_space(10.0);
        ui.label(RichText::new("Messages").strong());
        let log_height = (ui.available_height() - 40.0).max(60.0);
        egui::Frame::group(ui.style()).show(ui, |ui| {
            egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).max_height(log_height).show(ui, |ui| {
                for (kind, line) in &s.log {
                    match kind {
                        LogKind::Chat => ui.label(RichText::new(line).color(Color32::from_rgb(230, 180, 40))),
                        LogKind::System => ui.label(RichText::new(line).weak()),
                    };
                }
            });
        });
        let connected = s.connected();
        let Some(s) = &mut self.session else { return };
        ui.horizontal(|ui| {
            let r = ui.add_enabled(
                connected,
                egui::TextEdit::singleline(&mut self.chat_input).hint_text("Chat message").desired_width(ui.available_width() - 70.0),
            );
            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (ui.add_enabled(connected, egui::Button::new("Send")).clicked() || enter) && !self.chat_input.trim().is_empty() {
                s.chat(&self.chat_input);
                self.chat_input.clear();
                r.request_focus();
            }
        });
    }

    fn ptt_button(&mut self, ui: &mut egui::Ui) {
        let Some(s) = &self.session else { return };
        let connected = s.connected();
        let (text, fill) = match s.tx_since {
            Some(t) => {
                let secs = t.elapsed().as_secs();
                let warn = t.elapsed() > TX_LIMIT - Duration::from_secs(30);
                (
                    format!("● TRANSMITTING  {}:{:02}", secs / 60, secs % 60),
                    if warn { Color32::from_rgb(255, 90, 0) } else { Color32::from_rgb(200, 40, 40) },
                )
            }
            None if s.receiving => ("Receiving…".to_string(), Color32::from_rgb(40, 130, 70)),
            None if connected => ("Hold to talk  (or hold Space)".to_string(), Color32::from_rgb(55, 90, 150)),
            None => ("Connect to a station to talk".to_string(), Color32::from_gray(70)),
        };
        let size = egui::vec2(ui.available_width(), 84.0);
        let font = (size.x / 18.0).clamp(14.0, 22.0);
        let button = egui::Button::new(RichText::new(text).size(font).strong().color(Color32::WHITE)).fill(fill);
        let r = ui.add_enabled(connected, button.min_size(size));
        self.ptt_held_by_mouse = r.is_pointer_button_down_on();
    }

    fn handle_ptt(&mut self, ctx: &egui::Context) {
        let Some(s) = &mut self.session else { return };
        // Space works while no text box has the keyboard.
        let typing = ctx.memory(|m| m.focused().is_some()) && ctx.wants_keyboard_input();
        let space = !typing && self.settings.is_none() && ctx.input(|i| i.key_down(egui::Key::Space));
        let want = (self.ptt_held_by_mouse || space) && s.connected();
        if want != self.ptt_on {
            self.ptt_on = want;
            s.ptt(want);
        }
    }
}
