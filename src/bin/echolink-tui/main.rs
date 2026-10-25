mod app;
mod ui;

use anyhow::Result;
use crossterm::event::{self, Event, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags};
use crossterm::execute;
use crossterm::terminal::supports_keyboard_enhancement;
use echolink::session::Session;
use echolink::{config, directory, log, net};
use std::sync::mpsc::channel;
use std::time::Duration;

const USAGE: &str = "\
echolink-tui: EchoLink for the Linux terminal

Usage:
  echolink-tui            start (run `setup` first)
  echolink-tui setup      set callsign, name, location and password
  echolink-tui list [Q]   log in and print the station list, optionally filtered
";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None => run(),
        Some("setup") => config::setup(),
        Some("list") => list(args.get(1).map(|s| s.to_lowercase())),
        Some("-V" | "--version") => {
            println!("echolink-tui {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        _ => {
            print!("{USAGE}");
            Ok(())
        }
    }
}

fn list(query: Option<String>) -> Result<()> {
    let cfg = config::load()?;
    let (udp_tx, _udp_rx) = channel();
    let net = net::Net::from_config(&cfg, udp_tx);
    directory::login(&net, &cfg.servers, &cfg.callsign, &cfg.password, &cfg.location, directory::Status::Online)?;
    let mut stations = directory::stations(&net, &cfg.servers)?;
    stations.sort_by(|a, b| a.callsign.cmp(&b.callsign));
    for s in stations.iter().filter(|s| {
        query.as_ref().is_none_or(|q| {
            s.callsign.to_lowercase().contains(q) || s.description.to_lowercase().contains(q) || s.id.to_string() == *q
        })
    }) {
        println!("{:<11} {:>7}  {:<15} {}{}", s.callsign, s.id, s.ip, s.description, if s.busy { "  [busy]" } else { "" });
    }
    eprintln!("{} stations online", stations.len());
    if let Some(w) = directory::stub_warning(&stations) {
        eprintln!("\nWarning: {w}");
    }
    eprintln!("({})", net.describe());
    directory::login(&net, &cfg.servers, &cfg.callsign, &cfg.password, &cfg.location, directory::Status::Off)?;
    Ok(())
}

fn run() -> Result<()> {
    let cfg = config::load()?;
    log::init();
    let session = Session::start(cfg, "echolink-tui", None)?;

    let mut terminal = ratatui::init();
    // Key release events (kitty keyboard protocol) allow hold-to-talk.
    let hold_ptt = supports_keyboard_enhancement().unwrap_or(false)
        && execute!(
            std::io::stdout(),
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
            )
        )
        .is_ok();

    let mut app = app::App::new(session, hold_ptt);
    let result = (|| -> Result<()> {
        loop {
            if !app.poll() {
                return Ok(());
            }
            terminal.draw(|f| ui::draw(f, &mut app))?;
            if event::poll(Duration::from_millis(50))?
                && let Event::Key(k) = event::read()?
            {
                app.key(k);
            }
        }
    })();

    if hold_ptt {
        let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    }
    ratatui::restore();
    result
}
