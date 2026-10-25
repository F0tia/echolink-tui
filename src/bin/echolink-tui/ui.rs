use crate::app::{App, Mode};
use echolink::qso::{State, TX_LIMIT};
use echolink::session::{Filter, LogKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Gauge, Paragraph, Row, Table, Wrap};
use std::sync::atomic::Ordering;

const ACCENT: Color = Color::Cyan;

pub fn draw(f: &mut Frame, app: &mut App) {
    let [top, main, bottom] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(8), Constraint::Length(1)]).areas(f.area());
    header(f, app, top);
    let [left, right] = Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)]).areas(main);
    stations(f, app, left);
    qso(f, app, right);
    footer(f, app, bottom);
}

fn header(f: &mut Frame, app: &App, area: Rect) {
    let dir_color = if app.s.dir_ok { Color::Green } else { Color::Yellow };
    let mut spans = vec![
        Span::styled(" EchoLink ", Style::new().fg(Color::Black).bg(ACCENT).bold()),
        Span::raw(format!("  {}  ", app.s.cfg.callsign)),
        Span::styled(format!("● {}", app.s.dir_status), Style::new().fg(dir_color)),
        Span::raw("   "),
    ];
    spans.push(match &app.s.state {
        State::Idle => Span::styled("Not connected", Style::new().dim()),
        State::Connecting { label, .. } => Span::styled(format!("Calling {label}…"), Style::new().fg(Color::Yellow)),
        State::Connected { callsign, .. } => Span::styled(format!("Connected: {callsign}"), Style::new().fg(Color::Green).bold()),
    });
    if app.s.receiving {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(" RX ", Style::new().fg(Color::Black).bg(Color::Green).bold()));
    }
    if let Some(t) = app.s.tx_since {
        let s = t.elapsed().as_secs();
        spans.push(Span::raw("  "));
        spans.push(Span::styled(format!(" TX {}:{:02} ", s / 60, s % 60), Style::new().fg(Color::White).bg(Color::Red).bold()));
    }
    f.render_widget(Line::from(spans), area);
}

fn stations(f: &mut Frame, app: &mut App, area: Rect) {
    let mut tabs = vec![Span::raw(" ")];
    for flt in Filter::ALL {
        let st = if flt == app.filter { Style::new().fg(Color::Black).bg(ACCENT).bold() } else { Style::new().dim() };
        tabs.push(Span::styled(format!(" {} ", flt.label()), st));
        tabs.push(Span::raw(" "));
    }
    if !app.search.is_empty() {
        tabs.push(Span::styled(format!(" /{} ", app.search), Style::new().fg(Color::Yellow)));
    }
    let title = format!(" Stations {}/{} ", app.visible.len(), app.s.stations.len());
    let block = Block::new().borders(Borders::ALL).title(title).title_bottom(Line::from(tabs));

    let rows = app.visible.iter().map(|&i| {
        let s = &app.s.stations[i];
        let busy = if s.busy { Span::styled("busy", Style::new().fg(Color::Yellow)) } else { Span::raw("") };
        Row::new(vec![
            Cell::from(s.callsign.clone()).style(Style::new().bold()),
            Cell::from(s.id.to_string()),
            Cell::from(s.description.clone()),
            Cell::from(busy),
        ])
    });
    let table = Table::new(rows, [Constraint::Length(11), Constraint::Length(7), Constraint::Min(10), Constraint::Length(4)])
        .header(Row::new(["Call", "Node", "Location", ""]).style(Style::new().fg(ACCENT).bold()))
        .row_highlight_style(Style::new().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ")
        .block(block);
    if app.s.stations.is_empty() {
        let msg = Paragraph::new(if app.s.dir_ok { "Loading station list…" } else { "Waiting for the directory…" })
            .style(Style::new().dim())
            .block(Block::new().borders(Borders::ALL).title(" Stations "));
        f.render_widget(msg, area);
    } else {
        f.render_stateful_widget(table, area, &mut app.table);
    }
}

fn qso(f: &mut Frame, app: &App, area: Rect) {
    let [info, log, meters] =
        Layout::vertical([Constraint::Length(8), Constraint::Min(4), Constraint::Length(4)]).areas(area);

    let (title, body) = match &app.s.state {
        State::Connected { callsign, name, ip } => (
            format!(" {callsign} "),
            if app.s.remote_info.is_empty() { format!("{name}\n{ip}") } else { app.s.remote_info.clone() },
        ),
        State::Connecting { label, .. } => (" Calling ".into(), format!("Waiting for {label} to answer…")),
        State::Idle => (
            " Station ".into(),
            app.selected()
                .map(|s| format!("{}  (node {})\n{}\n{}{}", s.callsign, s.id, s.description, s.ip, if s.busy { "\nBusy" } else { "" }))
                .unwrap_or_default(),
        ),
    };
    f.render_widget(
        Paragraph::new(body).wrap(Wrap { trim: false }).block(Block::new().borders(Borders::ALL).title(title)),
        info,
    );

    let height = log.height.saturating_sub(2) as usize;
    let lines: Vec<Line> = app
        .s
        .log
        .iter()
        .rev()
        .take(height)
        .rev()
        .map(|(k, t)| match k {
            LogKind::Chat => Line::styled(t.clone(), Style::new().fg(Color::Yellow)),
            LogKind::System => Line::styled(t.clone(), Style::new().dim()),
        })
        .collect();
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(Block::new().borders(Borders::ALL).title(" Messages ")),
        log,
    );

    let rx = app.s.audio.shared.rx_level.load(Ordering::Relaxed);
    let tx = app.s.audio.shared.tx_level.load(Ordering::Relaxed);
    let block = Block::new().borders(Borders::ALL).title(" Audio ");
    let inner = block.inner(meters);
    f.render_widget(block, meters);
    let [r, t] = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(inner);
    let gauge = |label: &str, v: u32, c: Color| {
        Gauge::default()
            .label(label.to_string())
            .ratio((v as f64 / 1000.0).clamp(0.0, 1.0))
            .gauge_style(Style::new().fg(c))
            .use_unicode(true)
    };
    f.render_widget(gauge("RX", rx, Color::Green), r);
    let tx_color = if app.s.tx_since.is_some_and(|t| t.elapsed() > TX_LIMIT - std::time::Duration::from_secs(30)) {
        Color::Red
    } else {
        Color::LightRed
    };
    f.render_widget(gauge("TX", tx, tx_color), t);
}

fn footer(f: &mut Frame, app: &App, area: Rect) {
    let prompt = |label: &str| {
        Line::from(vec![
            Span::styled(format!(" {label} "), Style::new().fg(Color::Black).bg(ACCENT).bold()),
            Span::raw(format!(" {}", app.input)),
            Span::styled("█", Style::new().fg(ACCENT)),
            Span::styled("   Enter ok · Esc cancel", Style::new().dim()),
        ])
    };
    let line = match app.mode {
        Mode::Search => prompt("Search"),
        Mode::Chat => prompt("Chat"),
        Mode::Node => prompt("Node #"),
        Mode::Normal => {
            let ptt = if app.hold_ptt { "hold Space talk" } else { "Space talk" };
            let keys = [
                ("Enter", "connect"),
                ("n", "node #"),
                ("d", "disconnect"),
                ("", ptt),
                ("c", "chat"),
                ("/", "search"),
                ("Tab", "filter"),
                ("r", "refresh"),
                ("q", "quit"),
            ];
            let mut spans = vec![Span::raw(" ")];
            for (k, v) in keys {
                if !k.is_empty() {
                    spans.push(Span::styled(k, Style::new().fg(ACCENT).bold()));
                    spans.push(Span::raw(" "));
                }
                spans.push(Span::styled(v, Style::new().dim()));
                spans.push(Span::raw("  "));
            }
            Line::from(spans)
        }
    };
    f.render_widget(line, area);
}
