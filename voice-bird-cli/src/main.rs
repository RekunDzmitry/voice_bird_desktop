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

use app::{App, AppMode, RecordingStatus, RecordingError, ActiveSession};

/// When launched via macOS `open --args --tty <path>`, the process gets
/// stdin from `open` which doesn't properly forward terminal input.
/// We detect the `--tty` flag, reopen the real TTY, and dup2 it onto
/// stdin so crossterm gets proper raw-mode keyboard events.
#[cfg(target_os = "macos")]
fn reconnect_tty_from_args() {
    let args: Vec<String> = std::env::args().collect();
    let tty_idx = match args.iter().position(|a| a == "--tty") {
        Some(idx) => idx,
        None => return,
    };
    let tty_path = match args.get(tty_idx + 1) {
        Some(p) => p.clone(),
        None => return,
    };

    let c_path = match std::ffi::CString::new(tty_path) {
        Ok(p) => p,
        Err(_) => return,
    };

    unsafe {
        extern "C" {
            fn open(path: *const std::ffi::c_char, oflag: i32) -> i32;
            fn dup2(oldfd: i32, newfd: i32) -> i32;
            fn close(fd: i32) -> i32;
        }
        const O_RDWR: i32 = 2;

        let fd = open(c_path.as_ptr(), O_RDWR);
        if fd < 0 {
            return;
        }
        // Reconnect stdin to the real TTY for keyboard input
        dup2(fd, 0);
        if fd > 2 {
            close(fd);
        }
    }
}

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

/// On Windows, ensure VIRTUAL_TERMINAL_INPUT is disabled on the console input
/// handle. When VT Input is ON, special keys (Tab, Esc, Backspace, Ctrl+key)
/// are sent as VT escape sequences, causing crossterm to report them as
/// Release-only events. Since the event loop filters on `KeyEventKind::Press`,
/// those keys are silently dropped — making the config dialog appear frozen.
///
/// This must be called AFTER all terminal setup (`enable_raw_mode()`,
/// `EnterAlternateScreen`, `EnableMouseCapture`) and after any COM operations
/// (like `refresh_sessions()`), because any of those may alter console mode.
#[cfg(windows)]
fn ensure_console_mode() {
    use windows::Win32::System::Console::*;

    unsafe {
        let handle = GetStdHandle(STD_INPUT_HANDLE);
        if let Ok(handle) = handle {
            let mut mode = CONSOLE_MODE::default();
            if GetConsoleMode(handle, &mut mode).is_ok() {
                let before = mode.0;
                let mut new_mode = mode;
                // Disable VT Input — we want ReadConsoleInputW to return
                // proper key records, not VT escape sequences
                new_mode &= !ENABLE_VIRTUAL_TERMINAL_INPUT;
                // Disable Quick Edit to prevent mouse selection from blocking
                new_mode &= !ENABLE_QUICK_EDIT_MODE;
                new_mode |= ENABLE_EXTENDED_FLAGS;

                if new_mode != mode {
                    let _ = SetConsoleMode(handle, new_mode);
                    log::info!(
                        "Console mode adjusted: 0x{:04X} -> 0x{:04X} (VT_INPUT was {})",
                        before, new_mode.0,
                        if before & 0x0200 != 0 { "ON" } else { "OFF" }
                    );
                } else {
                    log::debug!("Console mode OK: 0x{:04X}", before);
                }
            }
        }
    }
}

fn main() -> Result<()> {
    // When launched via macOS `open`, reconnect stdin to the real TTY
    // so crossterm gets proper keyboard input. Must happen before anything
    // reads from stdin.
    #[cfg(target_os = "macos")]
    reconnect_tty_from_args();

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

    // On Windows, fix console input mode AFTER all terminal setup.
    // enable_raw_mode(), EnterAlternateScreen, or EnableMouseCapture may
    // enable VIRTUAL_TERMINAL_INPUT which breaks key event reporting.
    #[cfg(windows)]
    ensure_console_mode();

    // Create app and run
    let mut app = App::new();
    app.log_path = log_path;
    app.refresh_sessions();

    // On Windows, refresh_sessions() uses COM (CoInitializeEx) for WASAPI
    // audio enumeration, which may alter console input mode. Re-apply fix.
    #[cfg(windows)]
    ensure_console_mode();

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
    let mut consecutive_poll_errors: u32 = 0;

    loop {
        // Detect revoked stdin (e.g., TTY closed while app bundle still running).
        // On Unix, isatty(0) returns false when stdin fd is revoked, which would
        // cause event::poll to return instantly and spin the CPU at 100%.
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let stdin_fd = std::io::stdin().as_raw_fd();
            if unsafe { libc::isatty(stdin_fd) } == 0 {
                log::warn!("stdin is no longer a TTY, exiting gracefully");
                stop_all_sessions(app);
                return Ok(());
            }
        }

        // On Windows, verify the console handle is still valid.
        // If the console was detached (e.g., parent closed), GetConsoleMode
        // fails and event::poll would spin.
        #[cfg(windows)]
        {
            use windows::Win32::System::Console::*;
            unsafe {
                if let Ok(handle) = GetStdHandle(STD_INPUT_HANDLE) {
                    let mut mode = CONSOLE_MODE::default();
                    if GetConsoleMode(handle, &mut mode).is_err() {
                        log::warn!("Console handle invalid, exiting gracefully");
                        stop_all_sessions(app);
                        return Ok(());
                    }
                }
            }
        }

        // Update duration if recording
        app.update_duration();

        // Check for errors from recording threads
        app.check_error();

        // Check for init results from streaming threads
        app.check_init_result();

        terminal.draw(|f| ui::render(f, app))?;

        // Handle events with timeout for smooth updates
        match event::poll(Duration::from_millis(50)) {
            Ok(true) => {
                consecutive_poll_errors = 0;
                if let Event::Key(key) = event::read()? {
                    // Log all key events in config mode for diagnostics
                    if app.mode == AppMode::ConfigInput {
                        log::debug!(
                            "config key: kind={:?} code={:?} mod={:?} state={:?}",
                            key.kind, key.code, key.modifiers, key.state
                        );
                    }

                    if key.kind != KeyEventKind::Press {
                        continue;
                    }

                    match app.mode {
                        AppMode::Normal => handle_normal_mode(app, key.code),
                        AppMode::ConfigInput => {
                            handle_config_mode(app, key.code, key.modifiers);
                        }
                        AppMode::Help => handle_help_mode(app, key.code),
                    }
                }
            }
            Ok(false) => {
                consecutive_poll_errors = 0;
            }
            Err(e) => {
                consecutive_poll_errors += 1;
                log::warn!("event::poll error ({}/5): {}", consecutive_poll_errors, e);
                if consecutive_poll_errors >= 5 {
                    log::error!("Too many consecutive poll errors, exiting");
                    stop_all_sessions(app);
                    return Ok(());
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
        KeyCode::Tab | KeyCode::BackTab => {
            app.toggle_api_key_visibility();
        }
        KeyCode::Char('\t') => {
            // Some terminals report Tab as Char('\t') instead of KeyCode::Tab
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
    if let RecordingStatus::Error(ref err) = app.status {
        let text = err.display_message();
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
        RecordingStatus::Streaming { .. } | RecordingStatus::Connecting => {
            stop_all_sessions(app);
        }
    }
}

fn start_recording(app: &mut App) {
    // Check API key
    if !app.config.has_api_key() {
        app.status = RecordingStatus::Error(RecordingError::NoApiKey);
        return;
    }

    // Check selections
    if app.selected_sessions.is_empty() {
        app.status = RecordingStatus::Error(RecordingError::NoSelection);
        return;
    }

    app.status = RecordingStatus::Connecting;
    app.start_time = Some(std::time::Instant::now());

    let server_url = app.config.server_url();
    let api_key = app.config.api_key.clone().unwrap_or_default();

    // Create init result channel — streaming threads report init success/failure here
    let (init_tx, init_rx) = std::sync::mpsc::channel();
    app.init_result_rx = Some(init_rx);

    // Start each selected session
    for &idx in &app.selected_sessions.clone() {
        if let Some(session) = app.sessions.get(idx).cloned() {
            let session_id = Uuid::new_v4();
            let stop_signal = Arc::new(Mutex::new(false));
            let audio_level = app.audio_level.clone();
            let error_channel = app.error_channel.clone();
            let init_tx_clone = init_tx.clone();

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
                        init_tx_clone,
                    )
                } else {
                    platform::start_output_recording(
                        &session_clone,
                        server_url_clone,
                        api_key_clone,
                        session_id_str,
                        audio_level_clone,
                        stop_signal_clone,
                        init_tx_clone,
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

    if app.active_sessions.is_empty() {
        app.status = RecordingStatus::Error(RecordingError::NoSessionsStarted);
    }
    // Status stays as Connecting until check_init_result() receives confirmation
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
    app.init_result_rx = None;

    if let Ok(mut level) = app.audio_level.lock() {
        *level = 0.0;
    }
}
