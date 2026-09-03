// lora-server/src/ui.rs

use std::time::{Duration, Instant};
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, List, ListItem, Paragraph},
    Frame, Terminal,
};
use crate::app::{App, LogEntry};
use crate::backend::Radio;

pub fn render(frame: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(3),
        ])
        .split(frame.area());

    render_config(frame, app, chunks[0]);
    render_log(frame, app, chunks[1]);
    render_input(frame, app, chunks[2]);
}

fn render_config(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" Config ");

    let text = Line::from(vec![
        Span::styled(app.config_line(), Style::default().fg(Color::Yellow)),
    ]);

    let para = Paragraph::new(text).block(block);
    frame.render_widget(para, area);
}

fn render_log(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let inner_height = area.height.saturating_sub(2) as usize;
    let total = app.log.len();

    let max_scroll = total.saturating_sub(inner_height);
    let scroll = app.scroll_offset.min(max_scroll);

    let end = total.saturating_sub(scroll);
    let start = end.saturating_sub(inner_height);

    let title = if scroll > 0 {
        format!(" Traffic [+{}↓ to follow] ", scroll)
    } else {
        " Traffic ".to_string()
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(title);

    let items: Vec<ListItem> = app
        .log
        .range(start..end)
        .map(|entry| ListItem::new(format_entry(entry)))
        .collect();

    let list = List::new(items).block(block);
    frame.render_widget(list, area);
}

fn render_input(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::White))
        .title(" Send ");

    let text = format!("> {}_", app.input);
    let para = Paragraph::new(text).block(block);
    frame.render_widget(para, area);
}

fn format_entry(entry: &LogEntry) -> Line<'static> {
    match entry {
        LogEntry::Rx { timestamp, src_addr, payload, rssi } => {
            let mut spans = vec![
                Span::styled(
                    format!("[{}] ", timestamp),
                    Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM),
                ),
                Span::styled(
                    format!("RX from {:5}  ", src_addr),
                    Style::default().fg(Color::Green),
                ),
                Span::styled(
                    format!("{}", payload),
                    Style::default().fg(Color::Green),
                ),
            ];
            if let Some(dbm) = rssi {
                let rssi_color = rssi_color(*dbm);
                spans.push(Span::styled(
                    format!("   RSSI: {}dBm", dbm),
                    Style::default().fg(rssi_color),
                ));
            }
            Line::from(spans)
        }
        LogEntry::Tx { timestamp, dest_addr, payload } => Line::from(vec![
            Span::styled(
                format!("[{}] ", timestamp),
                Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM),
            ),
            Span::styled(
                format!("TX to   {:5}  {}", dest_addr, payload),
                Style::default().fg(Color::Cyan),
            ),
        ]),
        LogEntry::Heartbeat { timestamp, dest_addr } => Line::from(vec![
            Span::styled(
                format!("[{}] ", timestamp),
                Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM),
            ),
            Span::styled(
                format!("HB to   {:5}", dest_addr),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        LogEntry::Error { timestamp, message } => Line::from(vec![
            Span::styled(
                format!("[{}] ", timestamp),
                Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM),
            ),
            Span::styled(
                format!("ERR  {}", message),
                Style::default().fg(Color::Red),
            ),
        ]),
        LogEntry::SiPunch { timestamp, card_id, punches } => {
            let punch_str = punches.iter()
                .map(|(s, t)| format!("{}@{:02}:{:02}:{:02}", s, t / 3600, (t % 3600) / 60, t % 60))
                .collect::<Vec<_>>()
                .join(" ");
            Line::from(vec![
                Span::styled(
                    format!("[{}] ", timestamp),
                    Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM),
                ),
                Span::styled(
                    format!("SI  card {:>7}  {}", card_id, punch_str),
                    Style::default().fg(Color::Magenta),
                ),
            ])
        }
        LogEntry::CmdResult { timestamp, target, message, ok } => Line::from(vec![
            Span::styled(
                format!("[{}] ", timestamp),
                Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM),
            ),
            Span::styled(
                format!("CMD  {:5}  {}", target, message),
                Style::default().fg(if *ok { Color::Green } else { Color::Red }),
            ),
        ]),
    }
}

fn rssi_color(dbm: i16) -> Color {
    if dbm > -90 {
        Color::Green
    } else if dbm >= -110 {
        Color::Yellow
    } else {
        Color::Red
    }
}

pub fn run_app(
    port_info: String,
    addr: u16,
    dest_addr: u16,
    mut radio: Box<dyn Radio>,
    heartbeat_interval: u64,
    si_rx: std::sync::mpsc::Receiver<crate::sportident::CardReadout>,
) -> anyhow::Result<()> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = crate::app::App::new(port_info, addr, dest_addr);

    let tick_rate = Duration::from_millis(100);
    let mut last_tick = Instant::now();
    let heartbeat_period = (heartbeat_interval > 0).then(|| Duration::from_secs(heartbeat_interval));
    let mut last_heartbeat = Instant::now();

    // Run the event loop in a closure so a mid-loop I/O error still reaches the
    // terminal-restore step below, instead of leaving the shell in raw/alt-screen mode.
    let result: anyhow::Result<()> = (|| {
    loop {
        terminal.draw(|f| render(f, &app))?;

        let timeout = tick_rate.saturating_sub(last_tick.elapsed());
        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Esc | KeyCode::Char('q') => {
                            app.should_quit = true;
                        }
                        KeyCode::Enter => {
                            if !app.input.is_empty() {
                                let msg = std::mem::take(&mut app.input);
                                let ts = timestamp();
                                if let Some(val) = msg.strip_prefix("/dest ") {
                                    match val.trim().parse::<u16>() {
                                        Ok(n) => {
                                            let _ = radio.set_dest(n);
                                            app.dest = n;
                                        }
                                        Err(_) => app.push_log(LogEntry::Error {
                                            timestamp: ts,
                                            message: format!("invalid address: {}", val.trim()),
                                        }),
                                    }
                                } else if let Some(val) = msg.strip_prefix("/addr ") {
                                    match val.trim().parse::<u16>() {
                                        Ok(n) => app.addr = n,
                                        Err(_) => app.push_log(LogEntry::Error {
                                            timestamp: ts,
                                            message: format!("invalid address: {}", val.trim()),
                                        }),
                                    }
                                } else if let Some(val) = msg.strip_prefix("/cmd ") {
                                    let mut parts = val.trim().splitn(2, ' ');
                                    let parsed = parts.next().zip(parts.next())
                                        .and_then(|(t, s)| Some((t.parse::<u16>().ok()?, s.parse::<u32>().ok()?)));
                                    match parsed {
                                        Some((target, secs)) => match radio.send_command(target, secs) {
                                            Ok(()) => app.push_log(LogEntry::Tx {
                                                timestamp: ts,
                                                dest_addr: target,
                                                payload: format!("CMD hb_interval={}", secs),
                                            }),
                                            Err(e) => app.push_log(LogEntry::Error { timestamp: ts, message: e.to_string() }),
                                        },
                                        None => app.push_log(LogEntry::Error {
                                            timestamp: ts,
                                            message: "usage: /cmd <target-addr> <heartbeat-secs>".to_string(),
                                        }),
                                    }
                                } else {
                                    match radio.send(app.dest, msg.as_bytes()) {
                                        Ok(()) => app.push_log(LogEntry::Tx { timestamp: ts, dest_addr: app.dest, payload: msg }),
                                        Err(e) => app.push_log(LogEntry::Error { timestamp: ts, message: e.to_string() }),
                                    }
                                }
                            }
                        }
                        KeyCode::Up => {
                            app.scroll_up();
                        }
                        KeyCode::Down => {
                            app.scroll_down();
                        }
                        KeyCode::Backspace => {
                            app.input.pop();
                        }
                        KeyCode::Char(c) => {
                            app.input.push(c);
                        }
                        _ => {}
                    }
                }
            }
        }

        if let Some(period) = heartbeat_period {
            if last_heartbeat.elapsed() >= period {
                last_heartbeat = Instant::now();
                let ts = timestamp();
                let dest = app.dest;
                match radio.send(dest, b"HB") {
                    Ok(()) => app.push_log(LogEntry::Heartbeat { timestamp: ts, dest_addr: dest }),
                    Err(e) => app.push_log(LogEntry::Error { timestamp: ts, message: e.to_string() }),
                }
            }
        }

        while let Ok(readout) = si_rx.try_recv() {
            let ts = timestamp();
            let payload = readout.to_payload();
            if let Err(e) = radio.send(app.dest, payload.as_bytes()) {
                app.push_log(LogEntry::Error {
                    timestamp: ts.clone(),
                    message: format!("SI TX failed: {}", e),
                });
            }
            app.push_log(crate::app::LogEntry::SiPunch {
                timestamp: ts,
                card_id: readout.card_id,
                punches: readout.punches.iter().map(|p| (p.station, p.time_s)).collect(),
            });
        }

        if last_tick.elapsed() >= tick_rate {
            match radio.receive() {
                Ok(Some(pkt)) => {
                    let payload = String::from_utf8_lossy(&pkt.payload).into_owned();
                    app.push_log(LogEntry::Rx {
                        timestamp: timestamp(),
                        src_addr: pkt.src_addr,
                        payload,
                        rssi: pkt.rssi,
                    });
                }
                Ok(None) => {}
                Err(e) => app.push_log(LogEntry::Error {
                    timestamp: timestamp(),
                    message: e.to_string(),
                }),
            }

            for evt in radio.poll_status() {
                match evt {
                    crate::backend::StatusEvent::Heartbeat { dest } => {
                        app.push_log(LogEntry::Heartbeat { timestamp: timestamp(), dest_addr: dest });
                    }
                    crate::backend::StatusEvent::Tx { dest, payload } => {
                        app.push_log(LogEntry::Tx { timestamp: timestamp(), dest_addr: dest, payload });
                    }
                    crate::backend::StatusEvent::Err(message) => {
                        app.push_log(LogEntry::Error { timestamp: timestamp(), message });
                    }
                    crate::backend::StatusEvent::CmdOk { target, setting } => {
                        app.push_log(LogEntry::CmdResult {
                            timestamp: timestamp(),
                            target,
                            message: format!("acked: {}", setting.encode()),
                            ok: true,
                        });
                    }
                    crate::backend::StatusEvent::CmdErr { target, setting } => {
                        app.push_log(LogEntry::CmdResult {
                            timestamp: timestamp(),
                            target,
                            message: format!("no ack: {}", setting.encode()),
                            ok: false,
                        });
                    }
                }
            }
            last_tick = Instant::now();
        }

        if app.should_quit {
            break;
        }
    }
    Ok(())
    })();

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
    )?;
    terminal.show_cursor()?;

    result
}

fn timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let h = (secs % 86400) / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    format!("{:02}:{:02}:{:02}", h, m, s)
}
