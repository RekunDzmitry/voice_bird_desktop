mod app;
mod config;
mod logger;
mod platform;
mod ui;

use std::io;
use std::time::Duration;

use anyhow::Result;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::prelude::*;

use app::{App, AppMode, RecordingStatus};

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

fn main() -> Result<()> {
    // Handle `--recover <dir>` before any TTY/terminal setup so it works
    // from non-TTY shells (e.g., piped or macOS `open` invocations).
    let args: Vec<String> = std::env::args().collect();
    if let Some(pos) = args.iter().position(|a| a == "--recover") {
        let dir = args
            .get(pos + 1)
            .ok_or_else(|| anyhow::anyhow!("--recover requires a path"))?;
        voice_bird::session::recover::recover(std::path::Path::new(dir))?;
        println!("Recovered transcripts in {}", dir);
        return Ok(());
    }

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

        // Update duration if recording
        app.update_duration();

        // Check for errors from recording threads
        app.check_error();

        // Draw UI
        terminal.draw(|f| ui::render(f, app))?;

        // Handle events with timeout for smooth updates
        match event::poll(Duration::from_millis(50)) {
            Ok(true) => {
                consecutive_poll_errors = 0;
                if let Event::Key(key) = event::read()? {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }

                    match app.mode {
                        AppMode::Normal => handle_normal_mode(app, key.code),
                        AppMode::ModelPicker => {
                            // Wired in Stage 3 (Task 18). For now, Esc returns to Normal.
                            if matches!(key.code, KeyCode::Esc) {
                                app.mode = AppMode::Normal;
                            }
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
        KeyCode::Char('r') => {
            toggle_recording(app);
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
        KeyCode::Char('L') => {
            copy_log_path_to_clipboard(app);
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
            app.start_recording(voice_bird::session::layout::SessionSource::Microphone);
        }
        RecordingStatus::Recording => {
            stop_all_sessions(app);
        }
    }
}

fn stop_all_sessions(app: &mut App) {
    log::info!("Stopping all sessions");
    app.stop_recording();
}
