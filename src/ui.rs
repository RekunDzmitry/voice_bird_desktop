use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Gauge},
    Frame, Terminal,
};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use anyhow::Result;
use std::io::{stdout, Stdout};
use crate::session::{AudioSessionInfo, RecordingSession, SessionStatus};

pub struct App {
    pub selected_session_index: usize,
    pub list_state: ListState,
    pub available_sessions: Vec<AudioSessionInfo>,
    pub selected_sessions: Vec<bool>, // which sessions are selected for recording
    pub mode: AppMode,
}

#[derive(PartialEq)]
pub enum AppMode {
    SessionBrowser, // Selecting which apps/devices to record
    Recording,      // Actively recording sessions
}

impl App {
    pub fn new(available_sessions: Vec<AudioSessionInfo>) -> Self {
        let mut list_state = ListState::default();
        if !available_sessions.is_empty() {
            list_state.select(Some(0));
        }

        let selected_sessions = vec![false; available_sessions.len()];

        Self {
            selected_session_index: 0,
            list_state,
            available_sessions,
            selected_sessions,
            mode: AppMode::SessionBrowser,
        }
    }

    pub fn next_session(&mut self) {
        if self.available_sessions.is_empty() {
            return;
        }
        let i = match self.list_state.selected() {
            Some(i) => {
                if i >= self.available_sessions.len() - 1 {
                    0
                } else {
                    i + 1
                }
            }
            None => 0,
        };
        self.list_state.select(Some(i));
        self.selected_session_index = i;
    }

    pub fn previous_session(&mut self) {
        if self.available_sessions.is_empty() {
            return;
        }
        let i = match self.list_state.selected() {
            Some(i) => {
                if i == 0 {
                    self.available_sessions.len() - 1
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        self.list_state.select(Some(i));
        self.selected_session_index = i;
    }

    pub fn toggle_selected(&mut self) {
        if let Some(i) = self.list_state.selected() {
            if i < self.selected_sessions.len() {
                self.selected_sessions[i] = !self.selected_sessions[i];
            }
        }
    }

    pub fn get_selected_sessions(&self) -> Vec<AudioSessionInfo> {
        self.available_sessions
            .iter()
            .enumerate()
            .filter(|(i, _)| *i < self.selected_sessions.len() && self.selected_sessions[*i])
            .map(|(_, session)| session.clone())
            .collect()
    }

    pub fn start_recording_mode(&mut self) {
        self.mode = AppMode::Recording;
    }
}

pub fn init_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout());
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

pub fn restore_terminal(mut terminal: Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    stdout().execute(LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

pub fn render_session_browser(frame: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(10),
            Constraint::Length(5),
        ])
        .split(frame.area());

    // Title
    let title = Paragraph::new("Voice Bird Desktop - Session Browser")
        .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
        .block(Block::default().borders(Borders::ALL));
    frame.render_widget(title, chunks[0]);

    // Session list
    let sessions: Vec<ListItem> = app
        .available_sessions
        .iter()
        .enumerate()
        .map(|(i, session)| {
            let checkbox = if app.selected_sessions.get(i).copied().unwrap_or(false) {
                "[X]"
            } else {
                "[ ]"
            };

            let device_type = if session.is_input { "Input" } else { "Output" };
            let content = format!(
                "{} {} | {} | {}",
                checkbox, session.app_name, session.device_name, device_type
            );

            ListItem::new(content).style(Style::default().fg(Color::White))
        })
        .collect();

    let sessions_list = List::new(sessions)
        .block(
            Block::default()
                .title("Available Audio Sessions (Space=Toggle, Enter=Start)")
                .borders(Borders::ALL),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(">> ");

    frame.render_stateful_widget(sessions_list, chunks[1], &mut app.list_state);

    // Instructions
    let instructions = Paragraph::new(vec![
        Line::from(vec![
            Span::styled("↑/↓", Style::default().fg(Color::Yellow)),
            Span::raw(": Navigate  "),
            Span::styled("Space", Style::default().fg(Color::Yellow)),
            Span::raw(": Toggle  "),
            Span::styled("Enter", Style::default().fg(Color::Yellow)),
            Span::raw(": Start Recording  "),
            Span::styled("Q", Style::default().fg(Color::Yellow)),
            Span::raw(": Quit"),
        ]),
        Line::from(Span::raw("")),
        Line::from(Span::styled(
            format!("{} session(s) selected", app.selected_sessions.iter().filter(|&&x| x).count()),
            Style::default().fg(Color::Green),
        )),
    ])
    .block(Block::default().borders(Borders::ALL).title("Controls"));

    frame.render_widget(instructions, chunks[2]);
}

pub fn render_recording_dashboard(frame: &mut Frame, sessions: &[&RecordingSession]) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(6),
            Constraint::Length(3),
        ])
        .split(frame.area());

    // Title
    let title = Paragraph::new("Voice Bird Desktop - Recording Dashboard")
        .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
        .block(Block::default().borders(Borders::ALL));
    frame.render_widget(title, chunks[0]);

    // Active sessions panel
    render_active_sessions(frame, chunks[1], sessions);

    // Transcripts panel
    render_transcripts(frame, chunks[2], sessions);

    // Controls
    let controls = Paragraph::new(vec![
        Line::from(vec![
            Span::styled("ESC", Style::default().fg(Color::Yellow)),
            Span::raw(": Stop All & Save  "),
            Span::styled("Q", Style::default().fg(Color::Yellow)),
            Span::raw(": Quit without saving"),
        ]),
    ])
    .block(Block::default().borders(Borders::ALL).title("Controls"));
    frame.render_widget(controls, chunks[3]);
}

fn render_active_sessions(frame: &mut Frame, area: Rect, sessions: &[&RecordingSession]) {
    let block = Block::default()
        .title("Active Recording Sessions")
        .borders(Borders::ALL);

    if sessions.is_empty() {
        let empty = Paragraph::new("No active sessions")
            .style(Style::default().fg(Color::DarkGray))
            .block(block);
        frame.render_widget(empty, area);
        return;
    }

    // Calculate layout for each session (2 lines per session)
    let session_height = 3;
    let inner_area = block.inner(area);

    frame.render_widget(block, area);

    for (i, session) in sessions.iter().enumerate() {
        let y_offset = i as u16 * session_height;
        if y_offset + session_height > inner_area.height {
            break; // Don't render if it doesn't fit
        }

        let session_area = Rect {
            x: inner_area.x,
            y: inner_area.y + y_offset,
            width: inner_area.width,
            height: session_height,
        };

        render_session_row(frame, session_area, i + 1, session);
    }
}

fn render_session_row(frame: &mut Frame, area: Rect, index: usize, session: &RecordingSession) {
    // Line 1: Session info
    let status_symbol = match session.get_status() {
        SessionStatus::Recording => "🔴",
        SessionStatus::Paused => "⏸",
        SessionStatus::Stopped => "⏹",
        SessionStatus::Idle => "⚪",
    };

    let status_color = match session.get_status() {
        SessionStatus::Recording => Color::Red,
        SessionStatus::Paused => Color::Yellow,
        SessionStatus::Stopped => Color::Gray,
        SessionStatus::Idle => Color::DarkGray,
    };

    let duration = session.get_duration();
    let info_line = Line::from(vec![
        Span::styled(format!("{}. ", index), Style::default().fg(Color::Cyan)),
        Span::raw(format!("{} | {} ", session.app_name, session.device_name)),
        Span::styled(status_symbol, Style::default().fg(status_color)),
        Span::raw(format!(" {:.1}s", duration)),
    ]);

    let info_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: 1,
    };

    frame.render_widget(Paragraph::new(info_line), info_area);

    // Line 2: Audio level gauge
    let level = session.get_audio_level();
    let gauge = Gauge::default()
        .gauge_style(
            Style::default()
                .fg(if level > 0.8 {
                    Color::Red
                } else if level > 0.5 {
                    Color::Yellow
                } else {
                    Color::Green
                })
                .bg(Color::Black),
        )
        .ratio(level.min(1.0) as f64)
        .label(format!("{:.0}%", level * 100.0));

    let gauge_area = Rect {
        x: area.x,
        y: area.y + 1,
        width: area.width,
        height: 1,
    };

    frame.render_widget(gauge, gauge_area);
}

fn render_transcripts(frame: &mut Frame, area: Rect, sessions: &[&RecordingSession]) {
    let block = Block::default()
        .title("Transcripts (last 3 segments)")
        .borders(Borders::ALL);

    let mut lines = Vec::new();

    for session in sessions {
        if let Ok(segments) = session.transcript_buffer.lock() {
            if !segments.is_empty() {
                // Get last 3 segments
                let display_count = segments.len().min(3);
                let start_idx = segments.len() - display_count;

                lines.push(Line::from(vec![
                    Span::styled(
                        format!("[{}]", session.app_name),
                        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                    ),
                ]));

                for segment in &segments[start_idx..] {
                    // Truncate long segments
                    let display_text = if segment.len() > 80 {
                        format!("  {}...", &segment[..77])
                    } else {
                        format!("  {}", segment)
                    };
                    lines.push(Line::from(Span::raw(display_text)));
                }
            }
        }
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "No transcripts yet...",
            Style::default().fg(Color::DarkGray),
        )));
    }

    let transcript = Paragraph::new(lines).block(block);
    frame.render_widget(transcript, area);
}

pub fn handle_session_browser_input(app: &mut App) -> Result<bool> {
    if event::poll(std::time::Duration::from_millis(100))? {
        if let Event::Key(key) = event::read()? {
            if key.kind == KeyEventKind::Press {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Char('Q') => return Ok(true),
                    KeyCode::Up => app.previous_session(),
                    KeyCode::Down => app.next_session(),
                    KeyCode::Char(' ') => app.toggle_selected(),
                    KeyCode::Enter => {
                        if app.selected_sessions.iter().any(|&x| x) {
                            app.start_recording_mode();
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(false)
}

pub fn handle_recording_input() -> Result<RecordingInputAction> {
    if event::poll(std::time::Duration::from_millis(100))? {
        if let Event::Key(key) = event::read()? {
            if key.kind == KeyEventKind::Press {
                match key.code {
                    KeyCode::Esc => return Ok(RecordingInputAction::StopAndSave),
                    KeyCode::Char('q') | KeyCode::Char('Q') => return Ok(RecordingInputAction::QuitWithoutSaving),
                    _ => {}
                }
            }
        }
    }
    Ok(RecordingInputAction::Continue)
}

pub enum RecordingInputAction {
    Continue,
    StopAndSave,
    QuitWithoutSaving,
}
