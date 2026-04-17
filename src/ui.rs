use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

use crate::app::{App, RecordingStatus};

pub fn render(f: &mut Frame, app: &App) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header
            Constraint::Min(4),    // committed zone
            Constraint::Length(3), // tentative line
            Constraint::Length(1), // footer/keys
        ])
        .split(f.area());

    render_header(f, root[0], app);
    render_committed(f, root[1], app);
    render_tentative(f, root[2], app);
    render_footer(f, root[3], app);
}

fn render_header(f: &mut Frame, area: Rect, app: &App) {
    let timer = app.format_duration();
    let status = match &app.status {
        RecordingStatus::Idle => "idle",
        RecordingStatus::Recording => "● REC",
        RecordingStatus::Error(_) => "ERROR",
    };
    let line = Line::from(vec![
        Span::styled("Voice Bird", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("  │  "),
        Span::raw(app.config.default_model.as_str()),
        Span::raw("  │  "),
        Span::raw(format!("engine: {}", app.config.engine_prefer.as_str())),
        Span::raw("  │  "),
        Span::styled(status, status_style(&app.status)),
        Span::raw("  │  "),
        Span::raw(timer),
    ]);
    let p = Paragraph::new(line).block(Block::default().borders(Borders::ALL));
    f.render_widget(p, area);
}

fn status_style(status: &RecordingStatus) -> Style {
    match status {
        RecordingStatus::Recording => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        RecordingStatus::Error(_) => Style::default().fg(Color::Red),
        _ => Style::default().fg(Color::Gray),
    }
}

fn render_committed(f: &mut Frame, area: Rect, app: &App) {
    let committed = app.committed.lock();
    let lines: Vec<Line> = committed
        .iter()
        .map(|c| {
            let ts = format!(
                "{:02}:{:02}",
                c.t_start_ms / 60_000,
                (c.t_start_ms % 60_000) / 1000
            );
            Line::from(vec![
                Span::styled(ts, Style::default().fg(Color::DarkGray)),
                Span::raw("  "),
                Span::raw(c.text.clone()),
            ])
        })
        .collect();

    let p = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(Block::default().borders(Borders::ALL).title(" Transcript "));
    f.render_widget(p, area);
}

fn render_tentative(f: &mut Frame, area: Rect, app: &App) {
    let text = app.tentative.lock().clone();
    let p = Paragraph::new(Line::from(Span::styled(
        format!("… {}", text),
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC),
    )))
    .block(Block::default().borders(Borders::ALL));
    f.render_widget(p, area);
}

fn render_footer(f: &mut Frame, area: Rect, app: &App) {
    let keys = match &app.status {
        RecordingStatus::Idle => "[r] record  [m] model  [q] quit  [?] help",
        RecordingStatus::Recording => "[s] stop  [q] quit",
        RecordingStatus::Error(_) => "[r] retry  [q] quit",
    };
    let p = Paragraph::new(keys).style(Style::default().fg(Color::Gray));
    f.render_widget(p, area);
}
