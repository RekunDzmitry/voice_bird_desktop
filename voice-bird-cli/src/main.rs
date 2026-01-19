mod app;
mod audio;
mod config;
mod platform;
mod streaming;
mod ui;

use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::prelude::*;
use uuid::Uuid;

use app::{App, AppMode, RecordingStatus, ActiveSession};

fn main() -> Result<()> {
    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Create app and run
    let mut app = App::new();
    app.refresh_sessions();

    let result = run_app(&mut terminal, &mut app);

    // Restore terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    if let Err(e) = result {
        eprintln!("Error: {}", e);
    }

    Ok(())
}

fn run_app<B: Backend>(terminal: &mut Terminal<B>, app: &mut App) -> Result<()> {
    loop {
        // Update duration if recording
        app.update_duration();

        // Draw UI
        terminal.draw(|f| ui::render(f, app))?;

        // Handle events with timeout for smooth updates
        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }

                match app.mode {
                    AppMode::Normal => handle_normal_mode(app, key.code),
                    AppMode::ConfigInput => handle_config_mode(app, key.code),
                    AppMode::Help => handle_help_mode(app, key.code),
                }
            }
        }

        if app.should_quit {
            // Stop all sessions before quitting
            stop_all_sessions(app);
            break;
        }
    }

    Ok(())
}

fn handle_normal_mode(app: &mut App, key: KeyCode) {
    match key {
        KeyCode::Char('q') => {
            app.should_quit = true;
        }
        KeyCode::Char('?') => {
            app.toggle_help();
        }
        KeyCode::Char('c') => {
            app.enter_config_mode();
        }
        KeyCode::Char('r') => {
            app.refresh_sessions();
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.select_previous();
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.select_next();
        }
        KeyCode::Char(' ') => {
            app.toggle_selection();
        }
        KeyCode::Enter => {
            toggle_recording(app);
        }
        _ => {}
    }
}

fn handle_config_mode(app: &mut App, key: KeyCode) {
    match key {
        KeyCode::Enter => {
            app.save_api_key();
        }
        KeyCode::Esc => {
            app.cancel_config();
        }
        KeyCode::Backspace => {
            app.api_key_input.pop();
        }
        KeyCode::Char(c) => {
            app.api_key_input.push(c);
        }
        _ => {}
    }
}

fn handle_help_mode(app: &mut App, key: KeyCode) {
    match key {
        KeyCode::Char('?') | KeyCode::Esc | KeyCode::Enter => {
            app.toggle_help();
        }
        _ => {}
    }
}

fn toggle_recording(app: &mut App) {
    match &app.status {
        RecordingStatus::Idle | RecordingStatus::Error(_) => {
            start_recording(app);
        }
        RecordingStatus::Streaming | RecordingStatus::Connecting => {
            stop_all_sessions(app);
        }
    }
}

fn start_recording(app: &mut App) {
    // Check API key
    if !app.config.has_api_key() {
        app.status = RecordingStatus::Error("API key not configured. Press 'c' to configure.".to_string());
        return;
    }

    // Check selections
    if app.selected_sessions.is_empty() {
        app.status = RecordingStatus::Error("No sessions selected. Press Space to select.".to_string());
        return;
    }

    app.status = RecordingStatus::Connecting;
    app.start_time = Some(std::time::Instant::now());

    let server_url = app.config.server_url();
    let api_key = app.config.api_key.clone().unwrap_or_default();

    // Start each selected session
    for &idx in &app.selected_sessions.clone() {
        if let Some(session) = app.sessions.get(idx).cloned() {
            let session_id = Uuid::new_v4();
            let stop_signal = Arc::new(Mutex::new(false));
            let audio_level = app.audio_level.clone();

            let server_url_clone = server_url.clone();
            let api_key_clone = api_key.clone();
            let session_id_str = session_id.to_string();
            let session_clone = session.clone();
            let stop_signal_clone = stop_signal.clone();
            let audio_level_clone = audio_level.clone();

            // Spawn recording thread
            std::thread::spawn(move || {
                let result = if session_clone.is_input {
                    audio::start_input_recording(
                        &session_clone,
                        server_url_clone,
                        api_key_clone,
                        session_id_str,
                        audio::RecordingContext {
                            stop_signal: stop_signal_clone,
                            audio_level: audio_level_clone,
                        },
                    )
                } else {
                    platform::start_output_recording(
                        &session_clone,
                        server_url_clone,
                        api_key_clone,
                        session_id_str,
                        audio_level_clone,
                        stop_signal_clone,
                    )
                };

                if let Err(e) = result {
                    eprintln!("Recording error: {}", e);
                }
            });

            app.active_sessions.push(ActiveSession {
                id: session_id,
                session,
                stop_signal,
            });
        }
    }

    if !app.active_sessions.is_empty() {
        app.status = RecordingStatus::Streaming;
    } else {
        app.status = RecordingStatus::Error("Failed to start any sessions".to_string());
    }
}

fn stop_all_sessions(app: &mut App) {
    for session in &app.active_sessions {
        if let Ok(mut stop) = session.stop_signal.lock() {
            *stop = true;
        }
    }

    app.active_sessions.clear();
    app.status = RecordingStatus::Idle;
    app.start_time = None;
    app.duration = 0.0;

    if let Ok(mut level) = app.audio_level.lock() {
        *level = 0.0;
    }
}
