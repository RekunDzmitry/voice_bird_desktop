mod app;
mod audio;
mod config;
mod logger;
mod platform;
mod streaming;
mod ui;

use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::prelude::*;
use uuid::Uuid;

use app::{App, AppMode, RecordingStatus, ActiveSession};

/// On macOS, when launched via `open` as an .app bundle, macOS expects
/// the process to initialize NSApplication and process Apple Events.
/// Without this, Finder shows "application is not responding" dialogs.
/// We spin up a background thread to satisfy macOS while the TUI runs
/// on the main thread.
#[cfg(target_os = "macos")]
fn init_macos_app_event_handler() {
    use std::ffi::CString;

    #[link(name = "AppKit", kind = "framework")]
    extern "C" {
        fn NSApplicationLoad() -> bool;
    }

    #[link(name = "objc", kind = "dylib")]
    extern "C" {
        fn objc_getClass(name: *const std::ffi::c_char) -> *mut std::ffi::c_void;
        fn sel_registerName(name: *const std::ffi::c_char) -> *mut std::ffi::c_void;
        fn objc_msgSend(obj: *mut std::ffi::c_void, sel: *mut std::ffi::c_void, ...) -> *mut std::ffi::c_void;
    }

    std::thread::spawn(|| {
        unsafe {
            // Initialize the NSApplication shared instance
            NSApplicationLoad();

            let cls_name = CString::new("NSApplication").unwrap();
            let cls = objc_getClass(cls_name.as_ptr());
            if cls.is_null() {
                return;
            }

            let sel_shared = CString::new("sharedApplication").unwrap();
            let app = objc_msgSend(cls, sel_registerName(sel_shared.as_ptr()));
            if app.is_null() {
                return;
            }

            // Tell macOS we've finished launching — suppresses "not responding"
            let sel_finish = CString::new("finishLaunching").unwrap();
            objc_msgSend(app, sel_registerName(sel_finish.as_ptr()));
        }
    });

    // Give the background thread a moment to initialize
    std::thread::sleep(Duration::from_millis(50));
}

fn main() -> Result<()> {
    // On macOS, satisfy Apple Event expectations to prevent "not responding" dialogs
    #[cfg(target_os = "macos")]
    init_macos_app_event_handler();

    // Initialize file logger before TUI takes over the screen
    let log_path = match logger::init() {
        Ok(path) => Some(path),
        Err(e) => {
            eprintln!("Warning: failed to initialize logger: {}", e);
            None
        }
    };

    log::info!("Voice Bird CLI starting");

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Create app and run
    let mut app = App::new();
    app.log_path = log_path;
    app.refresh_sessions();

    log::info!("Found {} audio sessions", app.sessions.len());

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
        log::error!("Application error: {}", e);
        eprintln!("Error: {}", e);
    }

    log::info!("Voice Bird CLI exiting");

    Ok(())
}

fn run_app<B: Backend>(terminal: &mut Terminal<B>, app: &mut App) -> Result<()> {
    loop {
        // Update duration if recording
        app.update_duration();

        // Check for errors from recording threads
        app.check_error();

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
                    AppMode::ConfigInput => handle_config_mode(app, key.code, key.modifiers),
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
        KeyCode::Char('l') => {
            copy_error_to_clipboard(app);
        }
        KeyCode::Char('L') => {
            copy_log_path_to_clipboard(app);
        }
        _ => {}
    }
}

fn handle_config_mode(app: &mut App, key: KeyCode, modifiers: KeyModifiers) {
    // Ctrl+U clears entire input
    if modifiers.contains(KeyModifiers::CONTROL) {
        match key {
            KeyCode::Char('u') => {
                app.api_key_input.clear();
                return;
            }
            KeyCode::Char('v') => {
                app.paste_from_clipboard();
                return;
            }
            _ => {}
        }
    }

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
        KeyCode::Tab => {
            app.toggle_api_key_visibility();
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

fn copy_error_to_clipboard(app: &mut App) {
    if let RecordingStatus::Error(ref msg) = app.status {
        let text = msg.clone();
        match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(&text)) {
            Ok(()) => {
                app.status_message = Some("Error copied to clipboard".to_string());
            }
            Err(e) => {
                log::warn!("Failed to copy to clipboard: {}", e);
                app.status_message = Some(format!("Clipboard error: {}", e));
            }
        }
    }
}

fn copy_log_path_to_clipboard(app: &mut App) {
    if let Some(ref path) = app.log_path {
        let text = path.display().to_string();
        match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(&text)) {
            Ok(()) => {
                app.status_message = Some("Log path copied to clipboard".to_string());
            }
            Err(e) => {
                log::warn!("Failed to copy to clipboard: {}", e);
                app.status_message = Some(format!("Clipboard error: {}", e));
            }
        }
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
            let error_channel = app.error_channel.clone();

            let server_url_clone = server_url.clone();
            let api_key_clone = api_key.clone();
            let session_id_str = session_id.to_string();
            let session_clone = session.clone();
            let stop_signal_clone = stop_signal.clone();
            let audio_level_clone = audio_level.clone();

            log::info!(
                "Starting recording: session={}, device={}, input={}",
                session_id_str, session_clone.device_name, session_clone.is_input
            );

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
                    let msg = format!("Recording error: {}", e);
                    log::error!("{}", msg);
                    if let Ok(mut err) = error_channel.lock() {
                        *err = Some(msg);
                    }
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
    log::info!("Stopping all sessions");

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
