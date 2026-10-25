mod app;
mod audio;
mod config;
mod directory;
mod gsm;
mod log;
mod net;
mod proxy;
mod qso;
mod rtp;
mod ui;

use anyhow::Result;
use crossterm::event::{self, Event, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags};
use crossterm::execute;
use crossterm::terminal::supports_keyboard_enhancement;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
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
    log!("echolink-tui {} starting as {}", env!("CARGO_PKG_VERSION"), cfg.callsign);

    let transmitting = Arc::new(AtomicBool::new(false));
    let (mic_tx, mic_rx) = channel();
    let audio = audio::open(mic_tx, transmitting.clone())?;
    log!("audio: {}", audio.description);
    let me = qso::Station {
        callsign: cfg.callsign.clone(),
        name: cfg.name.clone(),
        info: format!("{}\n{}\n{}\necholink-tui {} (Linux)", cfg.callsign, cfg.name, cfg.location, env!("CARGO_PKG_VERSION")),
        accept_incoming: cfg.accept_incoming,
    };
    let (udp_tx, udp_rx) = channel();
    let net = net::Net::from_config(&cfg, udp_tx);
    if let net::Net::Proxy(_) = net {
        log!("using an EchoLink proxy ({})", cfg.proxy.as_deref().unwrap_or(""));
    }
    let handle = qso::spawn(me, audio.shared.clone(), mic_rx, transmitting, &net, udp_rx)?;

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

    let mut app = app::App::new(cfg, net, audio, handle, hold_ptt);
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
