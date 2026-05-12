// lora-cli/src/ui.rs

use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, List, ListItem, Paragraph},
    Frame,
};

use crate::app::{App, LogEntry};

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
        Span::styled(&app.config_line, Style::default().fg(Color::Yellow)),
    ]);

    let para = Paragraph::new(text).block(block);
    frame.render_widget(para, area);
}

fn render_log(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" Traffic ");

    let items: Vec<ListItem> = app
        .log
        .iter()
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

/// Temporary stub — full implementation added in Task 9.
pub fn run_app(_port: &str, _dest_addr: u16, _config: sx126x::Config, _radio: Box<dyn crate::backend::Radio>) -> anyhow::Result<()> {
    Ok(())
}
