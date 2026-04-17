use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};

use crate::app::{App, AppMode, RecordingStatus};

pub fn render(f: &mut Frame, app: &App) {
    if app.mode == AppMode::ModelPicker {
        render_model_picker(f, f.area(), app);
        return;
    }

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

pub fn render_model_picker(f: &mut Frame, area: Rect, app: &App) {
    let catalog = voice_bird::transcription::models::Catalog::builtin();
    let selected = app.picker.as_ref().map(|p| p.index);
    let items: Vec<Line> = catalog
        .all()
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let marker = if Some(i) == selected { "▶ " } else { "  " };
            Line::from(vec![
                Span::raw(marker),
                Span::styled(m.id, Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(format!("  {} MB  {}  ", m.size_mb, m.language)),
                Span::raw(if m.is_default { "(default)" } else { "" }),
            ])
        })
        .collect();

    let title = if app.config_was_loaded_from_disk {
        " Pick a model (Esc to cancel) "
    } else {
        " Pick a model (first run — required) "
    };

    let p = Paragraph::new(items)
        .block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(p, area);

    // Popup: download progress / error if a download is in flight.
    if let Some(progress_arc) = app
        .picker
        .as_ref()
        .and_then(|p| p.downloading.as_ref())
    {
        let g = progress_arc.lock();
        let msg = if let Some(err) = &g.error {
            format!("Download failed: {}: {}", g.model_id, err)
        } else {
            let pct = g
                .total
                .map(|t| (g.bytes * 100 / t.max(1)))
                .unwrap_or(0);
            format!("Downloading {}: {pct}%", g.model_id)
        };
        let popup = Paragraph::new(msg)
            .block(Block::default().borders(Borders::ALL).title(" Download "));
        let popup_area = centered(60, 3, area);
        f.render_widget(Clear, popup_area);
        f.render_widget(popup, popup_area);
    }
}

fn centered(pct_w: u16, h: u16, parent: Rect) -> Rect {
    let w = parent.width * pct_w / 100;
    Rect {
        x: parent.x + (parent.width - w) / 2,
        y: parent.y + parent.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    }
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
