//! The settings window: station details, password and proxy.

use eframe::egui;
use echolink::config::{self, Config};

#[derive(PartialEq, Clone, Copy)]
enum ProxyMode {
    Direct,
    Auto,
    Custom,
}

pub struct Form {
    cfg: Config,
    /// Blank keeps the saved password.
    password: String,
    proxy_mode: ProxyMode,
    proxy_host: String,
    error: String,
    first_run: bool,
}

pub enum Outcome {
    Open,
    Cancelled,
    Saved(Config),
}

impl Form {
    pub fn new(cfg: Option<&Config>) -> Form {
        let first_run = cfg.is_none();
        let cfg = cfg.cloned().unwrap_or_else(Config::blank);
        let (proxy_mode, proxy_host) = match cfg.proxy.as_deref().map(str::trim) {
            None | Some("") => (ProxyMode::Direct, String::new()),
            Some(p) if p.eq_ignore_ascii_case("none") => (ProxyMode::Direct, String::new()),
            Some(p) if p.eq_ignore_ascii_case("auto") => (ProxyMode::Auto, String::new()),
            Some(p) => (ProxyMode::Custom, p.to_string()),
        };
        Form { cfg, password: String::new(), proxy_mode, proxy_host, error: String::new(), first_run }
    }

    pub fn show(&mut self, ctx: &egui::Context) -> Outcome {
        let mut outcome = Outcome::Open;
        let title = if self.first_run { "Welcome to EchoLink" } else { "Settings" };
        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_width(440.0);
                if self.first_run {
                    ui.label("Enter your station details. Your callsign must be validated for EchoLink at echolink.org.");
                    ui.add_space(6.0);
                }
                egui::Grid::new("settings").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                    ui.label("Callsign");
                    ui.add(egui::TextEdit::singleline(&mut self.cfg.callsign).hint_text("N0CALL").desired_width(200.0));
                    ui.end_row();

                    ui.label("Password");
                    let hint = if self.first_run { "EchoLink password" } else { "leave blank to keep" };
                    ui.add(egui::TextEdit::singleline(&mut self.password).password(true).hint_text(hint).desired_width(200.0));
                    ui.end_row();

                    ui.label("Name");
                    ui.add(egui::TextEdit::singleline(&mut self.cfg.name).desired_width(200.0));
                    ui.end_row();

                    ui.label("Location");
                    ui.add(egui::TextEdit::singleline(&mut self.cfg.location).hint_text("City, ST").char_limit(27).desired_width(200.0));
                    ui.end_row();

                    ui.label("Connection");
                    ui.vertical(|ui| {
                        ui.radio_value(&mut self.proxy_mode, ProxyMode::Direct, "Direct (UDP 5198–5199 forwarded to this computer)");
                        ui.radio_value(&mut self.proxy_mode, ProxyMode::Auto, "Public proxy, picked automatically");
                        ui.horizontal(|ui| {
                            ui.radio_value(&mut self.proxy_mode, ProxyMode::Custom, "This proxy:");
                            ui.add_enabled(
                                self.proxy_mode == ProxyMode::Custom,
                                egui::TextEdit::singleline(&mut self.proxy_host).hint_text("host[:port]").desired_width(150.0),
                            );
                        });
                        ui.label(
                            egui::RichText::new("Starlink, cellular and other shared-IP internet need a proxy.").small().weak(),
                        );
                    });
                    ui.end_row();

                    ui.label("");
                    ui.checkbox(&mut self.cfg.accept_incoming, "Accept calls from other stations");
                    ui.end_row();
                });

                if !self.error.is_empty() {
                    ui.colored_label(ui.visuals().error_fg_color, &self.error);
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button(if self.first_run { "Start" } else { "Save" }).clicked() {
                        match self.validated() {
                            Ok(cfg) => match config::save(&cfg) {
                                Ok(()) => outcome = Outcome::Saved(cfg),
                                Err(e) => self.error = format!("Couldn't save: {e:#}"),
                            },
                            Err(e) => self.error = e,
                        }
                    }
                    if !self.first_run && ui.button("Cancel").clicked() {
                        outcome = Outcome::Cancelled;
                    }
                });
            });
        outcome
    }

    fn validated(&self) -> Result<Config, String> {
        let mut cfg = self.cfg.clone();
        cfg.callsign = cfg.callsign.trim().to_uppercase();
        cfg.location = cfg.location.trim().chars().take(27).collect();
        if cfg.callsign.is_empty() {
            return Err("Enter your callsign.".into());
        }
        if !self.password.is_empty() {
            cfg.password = self.password.clone();
        }
        if cfg.password.is_empty() {
            return Err("Enter your EchoLink password.".into());
        }
        cfg.proxy = match self.proxy_mode {
            ProxyMode::Direct => None,
            ProxyMode::Auto => Some("auto".into()),
            ProxyMode::Custom if self.proxy_host.trim().is_empty() => return Err("Enter the proxy's host name.".into()),
            ProxyMode::Custom => Some(self.proxy_host.trim().into()),
        };
        Ok(cfg)
    }
}
