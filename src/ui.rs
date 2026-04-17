use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Gauge, List, ListItem, Paragraph},
    Frame,
};

use crate::app::{App, AppMode, RecordingStatus};

/// Render the application UI.
///
/// Modal views (config, help) render as full-screen layouts instead of
/// overlays.  This avoids a Windows-specific issue where ratatui's
/// buffer-diff algorithm fails to visually update overlay content,
/// requiring `terminal.clear()` which causes visible screen flicker.
pub fn render(frame: &mut Frame, app: &App) {
    match app.mode {
        AppMode::Normal => render_normal(frame, app),
        AppMode::ConfigInput => render_config_fullscreen(frame, app),
        AppMode::Help => render_help_fullscreen(frame),
    }
}

fn render_normal(frame: &mut Frame, app: &App) {
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
        RecordingStatus::Idle => ("Idle".to_string(), Color::DarkGray),
        RecordingStatus::Connecting => ("Connecting...".to_string(), Color::Yellow),
        RecordingStatus::Streaming { usage } => {
            let text = if let Some(u) = usage {
                let remaining_secs = u.seconds_remaining as u32;
                let mins = remaining_secs / 60;
                let secs = remaining_secs % 60;
                format!("Streaming to server ({:02}:{:02} remaining)", mins, secs)
            } else {
                "Streaming to server".to_string()
            };
            (text, Color::Green)
        }
        RecordingStatus::Error(err) => (err.display_message(), Color::Red),
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
    let recording = matches!(app.status, RecordingStatus::Streaming { .. } | RecordingStatus::Connecting);

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

/// Full-screen config view — no overlay, no Clear widget.
/// Ratatui's normal diff handles updates without flicker.
fn render_config_fullscreen(frame: &mut Frame, app: &App) {
    let area = frame.area();

    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // Title bar
            Constraint::Min(0),    // Config content
            Constraint::Length(1), // Bottom hint
        ])
        .split(area);

    // Title bar
    let title = Paragraph::new(Line::from(vec![
        Span::styled(
            format!(" Voice Bird CLI v{} ", env!("CARGO_PKG_VERSION")),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" — Configure API Key", Style::default().fg(Color::White)),
    ]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)),
    );
    frame.render_widget(title, outer[0]);

    // Config content — vertically centered
    let inner = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),    // Top padding
            Constraint::Length(8), // Config card
            Constraint::Min(0),    // Bottom padding
        ])
        .split(outer[1]);

    let card_area = centered_horiz(60, inner[1]);

    let block = Block::default()
        .title(" Configure API Key ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // Current key status
            Constraint::Length(1), // Instructions
            Constraint::Length(3), // Input (with borders)
            Constraint::Length(1), // Actions
        ])
        .margin(1)
        .split(card_area);

    // Current stored key (masked)
    let current_key_line = if let Some(masked) = app.masked_stored_key() {
        Line::from(vec![
            Span::styled("Current: ", Style::default().fg(Color::DarkGray)),
            Span::styled(masked, Style::default().fg(Color::Green)),
        ])
    } else {
        Line::from(Span::styled(
            "No API key configured",
            Style::default().fg(Color::Yellow),
        ))
    };

    let instructions = Paragraph::new("Enter new API key (paste replaces all):")
        .style(Style::default().fg(Color::White));

    let input_display = if app.api_key_input.is_empty() {
        "(empty — type or Ctrl+V to paste)".to_string()
    } else if app.api_key_visible {
        app.api_key_input.clone()
    } else {
        let len = app.api_key_input.len();
        if len <= 8 {
            "*".repeat(len)
        } else {
            let prefix = &app.api_key_input[..4];
            let suffix = &app.api_key_input[len.saturating_sub(4)..];
            format!("{}...{} ({}ch)", prefix, suffix, len)
        }
    };

    let input = Paragraph::new(input_display)
        .style(Style::default().fg(Color::Yellow))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        );

    let actions =
        Paragraph::new("[Enter] Save  [Esc] Cancel  [Tab] Show/Hide  [Ctrl+V] Paste")
            .style(Style::default().fg(Color::DarkGray));

    frame.render_widget(block, card_area);
    frame.render_widget(Paragraph::new(current_key_line), chunks[0]);
    frame.render_widget(instructions, chunks[1]);
    frame.render_widget(input, chunks[2]);
    frame.render_widget(actions, chunks[3]);

    // Bottom hint
    let hint = Paragraph::new(" Press Esc to return")
        .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(hint, outer[2]);
}

/// Full-screen help view.
fn render_help_fullscreen(frame: &mut Frame) {
    let area = frame.area();

    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // Title bar
            Constraint::Min(0),    // Help content
            Constraint::Length(1), // Bottom hint
        ])
        .split(area);

    // Title bar
    let title = Paragraph::new(Line::from(vec![
        Span::styled(
            format!(" Voice Bird CLI v{} ", env!("CARGO_PKG_VERSION")),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" — Help", Style::default().fg(Color::White)),
    ]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)),
    );
    frame.render_widget(title, outer[0]);

    // Help content
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
        "  Configuration (press 'c' to open):",
        "    c          Configure API key",
        "    Tab        Show/hide key value",
        "    Ctrl+V     Paste from clipboard",
        "    Ctrl+U     Clear input",
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
    frame.render_widget(paragraph, outer[1]);

    // Bottom hint
    let hint = Paragraph::new(" Press Esc or ? to return")
        .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(hint, outer[2]);
}

/// Helper to horizontally center a rect within its row.
fn centered_horiz(percent_x: u16, r: Rect) -> Rect {
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(r)[1]
}
