use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Gauge, List, ListItem, Paragraph, Clear},
    Frame,
};

use crate::app::{App, AppMode, RecordingStatus};

/// Render the application UI
pub fn render(frame: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // Title
            Constraint::Min(5),     // Session list
            Constraint::Length(6),  // Status panel
            Constraint::Length(3),  // Controls
        ])
        .split(frame.area());

    render_title(frame, chunks[0], app);
    render_session_list(frame, chunks[1], app);
    render_status_panel(frame, chunks[2], app);
    render_controls(frame, chunks[3], app);

    // Render overlays
    match app.mode {
        AppMode::ConfigInput => render_config_dialog(frame, app),
        AppMode::Help => render_help_dialog(frame),
        AppMode::Normal => {}
    }
}

fn render_title(frame: &mut Frame, area: Rect, app: &App) {
    let api_status = if app.config.has_api_key() {
        Span::styled(" [API Key: Set]", Style::default().fg(Color::Green))
    } else {
        Span::styled(" [API Key: Not Set]", Style::default().fg(Color::Yellow))
    };

    let log_info = if let Some(ref path) = app.log_path {
        Span::styled(
            format!(" [log: {}]", path.display()),
            Style::default().fg(Color::DarkGray),
        )
    } else {
        Span::raw("")
    };

    let title = Line::from(vec![
        Span::styled(
            format!(" Voice Bird CLI v{} ", env!("CARGO_PKG_VERSION")),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        api_status,
        log_info,
        Span::raw("  "),
        Span::styled("[q]", Style::default().fg(Color::DarkGray)),
        Span::raw("uit  "),
        Span::styled("[?]", Style::default().fg(Color::DarkGray)),
        Span::raw("help"),
    ]);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let paragraph = Paragraph::new(title).block(block);
    frame.render_widget(paragraph, area);
}

fn render_session_list(frame: &mut Frame, area: Rect, app: &App) {
    let items: Vec<ListItem> = app
        .sessions
        .iter()
        .enumerate()
        .map(|(i, session)| {
            let selected_marker = if app.is_selected(i) { "[*]" } else { "[ ]" };
            let cursor = if i == app.selected_index { "> " } else { "  " };

            let device_type = if session.is_input { "mic" } else { "out" };

            let content = format!(
                "{}{} {} ({}) [{}]",
                cursor, selected_marker, session.app_name, session.device_name, device_type
            );

            let style = if i == app.selected_index {
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            } else if app.is_selected(i) {
                Style::default().fg(Color::Green)
            } else {
                Style::default()
            };

            ListItem::new(content).style(style)
        })
        .collect();

    let title = if app.sessions.is_empty() {
        " Audio Sources (none found - press 'r' to refresh) "
    } else {
        " Audio Sources "
    };

    let list = List::new(items)
        .block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        );

    frame.render_widget(list, area);
}

fn render_status_panel(frame: &mut Frame, area: Rect, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),  // Status line
            Constraint::Length(2),  // Audio level gauge
            Constraint::Length(1),  // Duration
        ])
        .margin(1)
        .split(area);

    // Status text
    let (status_text, status_color) = match &app.status {
        RecordingStatus::Idle => ("Idle", Color::DarkGray),
        RecordingStatus::Connecting => ("Connecting...", Color::Yellow),
        RecordingStatus::Streaming => ("Streaming to server", Color::Green),
        RecordingStatus::Error(msg) => (msg.as_str(), Color::Red),
    };

    let status_line = Paragraph::new(format!("Status: {}", status_text))
        .style(Style::default().fg(status_color));

    // Audio level gauge
    let level = app.get_audio_level();
    let level_percent = (level * 100.0).min(100.0) as u16;

    let gauge_color = if level > 0.8 {
        Color::Red
    } else if level > 0.5 {
        Color::Yellow
    } else {
        Color::Green
    };

    let gauge = Gauge::default()
        .gauge_style(Style::default().fg(gauge_color))
        .percent(level_percent)
        .label(format!("Level: {}%", level_percent));

    // Duration
    let duration_text = Paragraph::new(format!("Duration: {}", app.format_duration()))
        .style(Style::default().fg(Color::Cyan));

    let block = Block::default()
        .title(" Status ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    frame.render_widget(block, area);
    frame.render_widget(status_line, chunks[0]);
    frame.render_widget(gauge, chunks[1]);
    frame.render_widget(duration_text, chunks[2]);
}

fn render_controls(frame: &mut Frame, area: Rect, app: &App) {
    let recording = matches!(app.status, RecordingStatus::Streaming | RecordingStatus::Connecting);

    let controls = if recording {
        vec![
            Span::styled("[Enter]", Style::default().fg(Color::Red)),
            Span::raw(" Stop  "),
        ]
    } else {
        vec![
            Span::styled("[Enter]", Style::default().fg(Color::Green)),
            Span::raw(" Start  "),
            Span::styled("[Space]", Style::default().fg(Color::Cyan)),
            Span::raw(" Select  "),
        ]
    };

    let mut all_controls = controls;
    all_controls.extend(vec![
        Span::styled("[r]", Style::default().fg(Color::Blue)),
        Span::raw("efresh  "),
        Span::styled("[c]", Style::default().fg(Color::Magenta)),
        Span::raw("onfig  "),
    ]);

    // Show copy controls when there's an error or log path
    if matches!(app.status, RecordingStatus::Error(_)) {
        all_controls.extend(vec![
            Span::styled("[l]", Style::default().fg(Color::Yellow)),
            Span::raw(" copy error  "),
        ]);
    }
    if app.log_path.is_some() {
        all_controls.extend(vec![
            Span::styled("[L]", Style::default().fg(Color::Yellow)),
            Span::raw(" copy log path  "),
        ]);
    }

    let controls_line = Line::from(all_controls);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let paragraph = Paragraph::new(controls_line).block(block);
    frame.render_widget(paragraph, area);
}

fn render_config_dialog(frame: &mut Frame, app: &App) {
    let area = centered_rect(60, 30, frame.area());

    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(" Configure API Key ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),  // Instructions
            Constraint::Length(3),  // Input
            Constraint::Length(2),  // Actions
        ])
        .margin(1)
        .split(area);

    let instructions = Paragraph::new("Enter your Voice Bird API key:")
        .style(Style::default().fg(Color::White));

    let input_display = if app.api_key_input.is_empty() {
        "(empty)".to_string()
    } else {
        "*".repeat(app.api_key_input.len().min(40))
    };

    let input = Paragraph::new(input_display)
        .style(Style::default().fg(Color::Yellow))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        );

    let actions = Paragraph::new("[Enter] Save  [Esc] Cancel")
        .style(Style::default().fg(Color::DarkGray));

    frame.render_widget(block, area);
    frame.render_widget(instructions, chunks[0]);
    frame.render_widget(input, chunks[1]);
    frame.render_widget(actions, chunks[2]);
}

fn render_help_dialog(frame: &mut Frame) {
    let area = centered_rect(50, 60, frame.area());

    frame.render_widget(Clear, area);

    let help_text = vec![
        "",
        "  Navigation:",
        "    Up/Down    Navigate session list",
        "    Space      Select/deselect session",
        "",
        "  Recording:",
        "    Enter      Start/stop streaming",
        "    r          Refresh session list",
        "",
        "  Configuration:",
        "    c          Configure API key",
        "",
        "  Diagnostics:",
        "    l          Copy error text to clipboard",
        "    L          Copy log file path to clipboard",
        "",
        "  Other:",
        "    ?          Toggle this help",
        "    q          Quit application",
        "",
    ];

    let paragraph = Paragraph::new(help_text.join("\n"))
        .style(Style::default().fg(Color::White))
        .block(
            Block::default()
                .title(" Help ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        );

    frame.render_widget(paragraph, area);
}

/// Helper to create a centered rectangle
fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}
