mod app;
mod logger;
mod platform;
mod settings_view;
mod ui;

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind,
        MouseEventKind,
    },
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

    // `--debug-state-snapshot <path>` opts into a JSONL file where every
    // key press appends a serialized App-state snapshot. Used by the
    // e2e_human test harness to drive the TUI without relying on
    // screenshot OCR. No-op when the flag is absent.
    let debug_snapshot_path: Option<PathBuf> = args
        .iter()
        .position(|a| a == "--debug-state-snapshot")
        .and_then(|pos| args.get(pos + 1))
        .map(PathBuf::from);

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

    // Sentinel line: version + exe path + compile time so we can tell at
    // a glance which binary was launched. If you don't see this with a
    // fresh timestamp, you're running a stale build.
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "<unknown>".into());
    log::info!(
        "Voice Bird v{} starting — ui=inline-devices-panel exe={} built_at={}",
        env!("CARGO_PKG_VERSION"),
        exe,
        env!("VOICE_BIRD_BUILD_TS"),
    );

    // Route whisper.cpp + ggml logs (including Metal init) through the
    // `log` crate so they land in our file log and don't scribble over
    // the TUI on stderr.
    whisper_rs::install_whisper_log_trampoline();

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

    // Seed cursor to the saved device. Many devices (USB headsets like
    // EPOS) appear twice — once as Input (mic) and once as Output (for
    // loopback capture). Prefer matching on BOTH name and saved kind so
    // a user who picked the loopback variant gets the loopback variant
    // back. Falls back to name-only match for old configs that did not
    // store input_device_kind.
    if let Some(saved) = app.config.input_device.clone() {
        let saved_kind = app.config.input_device_kind;
        let by_name_and_kind = saved_kind.and_then(|k| {
            app.sessions
                .iter()
                .position(|s| s.device_name == saved && s.kind == k)
        });
        let i = by_name_and_kind.or_else(|| {
            app.sessions.iter().position(|s| s.device_name == saved)
        });
        if let Some(i) = i {
            app.selected_index = i;
        }
    }

    log::info!("Found {} audio sessions", app.sessions.len());
    for (i, s) in app.sessions.iter().enumerate() {
        log::info!("  device[{}] = {:?} kind={:?}", i, s.device_name, s.kind);
    }
    log::info!(
        "config.input_device = {:?}, cursor selected_index = {}",
        app.config.input_device,
        app.selected_index
    );

    if let Some(p) = &debug_snapshot_path {
        log::info!("debug state snapshots → {}", p.display());
        // Truncate any prior file so each run is fresh.
        let _ = std::fs::write(p, "");
        // Emit one initial snapshot so the e2e harness can read the
        // device list before sending its first keystroke.
        write_state_snapshot(&app, "<startup>", p);
    }

    let result = run_app(&mut terminal, &mut app, debug_snapshot_path.as_deref());

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

fn run_app<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    debug_snapshot_path: Option<&Path>,
) -> Result<()> {
    let mut consecutive_poll_errors: u32 = 0;
    // Periodic snapshot: while recording, the snapshot file should
    // also reflect transcript events that arrive between key presses.
    // Throttled to once per second to keep file growth bounded.
    let mut last_periodic_snapshot = std::time::Instant::now();

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

        // Drain any error the engine's consumer task posted — sets the
        // banner + flips status to Error, so the user sees the message
        // and can press `r` to retry.
        app.check_engine_error();

        // If the model picker has a completed download, commit config &
        // return to Normal mode.
        app.poll_picker_download();

        // Draw UI
        terminal.draw(|f| ui::render(f, app))?;

        // Handle events with timeout for smooth updates
        match event::poll(Duration::from_millis(50)) {
            Ok(true) => {
                consecutive_poll_errors = 0;
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        match app.mode {
                            AppMode::Normal => handle_normal_mode(app, key.code),
                            AppMode::ModelPicker => handle_picker_mode(app, key.code),
                            AppMode::Help => handle_help_mode(app, key.code),
                            AppMode::Settings => settings_view::handle_key(app, key.code),
                        }
                        if let Some(path) = debug_snapshot_path {
                            write_state_snapshot(app, &format!("{:?}", key.code), path);
                        }
                    }
                    Event::Mouse(mouse) if app.mode == AppMode::Normal => {
                        match mouse.kind {
                            MouseEventKind::ScrollUp => app.scroll_transcript_up(3),
                            MouseEventKind::ScrollDown => app.scroll_transcript_down(3),
                            _ => {}
                        }
                    }
                    _ => {}
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

        // Periodic state snapshot — once per second, regardless of
        // keys, so the e2e harness can observe transcripts arriving.
        if let Some(path) = debug_snapshot_path {
            if last_periodic_snapshot.elapsed() >= Duration::from_secs(1) {
                write_state_snapshot(app, "<periodic>", path);
                last_periodic_snapshot = std::time::Instant::now();
            }
        }
    }

    Ok(())
}

fn handle_normal_mode(app: &mut App, key: KeyCode) {
    let recording = matches!(app.status, RecordingStatus::Recording);
    // Transcript scroll keys — always available, no mode conflict.
    match key {
        KeyCode::PageUp => {
            app.scroll_transcript_up(10);
            return;
        }
        KeyCode::PageDown => {
            app.scroll_transcript_down(10);
            return;
        }
        KeyCode::Home => {
            app.scroll_transcript_home();
            return;
        }
        KeyCode::End => {
            app.scroll_transcript_end();
            return;
        }
        _ => {}
    }
    match key {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Char('?') => app.toggle_help(),
        KeyCode::Up | KeyCode::Char('k') if !recording => {
            if app.selected_index > 0 {
                app.selected_index -= 1;
            }
        }
        KeyCode::Down | KeyCode::Char('j') if !recording => {
            if app.selected_index + 1 < app.sessions.len() {
                app.selected_index += 1;
            }
        }
        KeyCode::Up | KeyCode::Char('k') if recording => app.scroll_transcript_up(1),
        KeyCode::Down | KeyCode::Char('j') if recording => app.scroll_transcript_down(1),
        KeyCode::Enter if !recording => {
            // Persist the selected device (name + kind), then start recording.
            use voice_bird::config::AudioSessionKind;
            use voice_bird::session::layout::SessionSource;
            let source = if let Some(dev) = app.sessions.get(app.selected_index).cloned() {
                let name_changed =
                    app.config.input_device.as_deref() != Some(dev.device_name.as_str());
                let kind_changed = app.config.input_device_kind != Some(dev.kind);
                if name_changed || kind_changed {
                    app.config.input_device = Some(dev.device_name.clone());
                    app.config.input_device_kind = Some(dev.kind);
                    if let Err(e) = app.config.save() {
                        log::error!("config save: {e}");
                    }
                }
                match dev.kind {
                    AudioSessionKind::Input => SessionSource::Microphone,
                    AudioSessionKind::Output => SessionSource::System,
                }
            } else {
                SessionSource::Microphone
            };
            app.start_recording(source);
        }
        KeyCode::Char('r') if !recording => {
            // Refresh device list, preserving cursor on the same name if present.
            let prior = app
                .sessions
                .get(app.selected_index)
                .map(|s| s.device_name.clone());
            app.refresh_sessions();
            if let Some(name) = prior {
                if let Some(i) = app.sessions.iter().position(|s| s.device_name == name) {
                    app.selected_index = i;
                }
            }
        }
        KeyCode::Char('s') if !recording => {
            settings_view::open(app);
        }
        KeyCode::Char('s') if recording => {
            app.stop_recording();
        }
        KeyCode::Char('m') if !recording => app.mode = AppMode::ModelPicker,
        // Toggle live broadcast for the next recording. In-memory only —
        // does not persist to config.toml; the user changes the default
        // in Settings ('s'). Disabled mid-recording so a flip doesn't
        // strand the active engine.
        KeyCode::Char('b') if !recording => {
            app.config.cloud_broadcast_enabled = !app.config.cloud_broadcast_enabled;
            let on = app.config.cloud_broadcast_enabled;
            app.banner = Some(
                if on {
                    "Live broadcast: ON (next recording streams to voicebird.app)".into()
                } else {
                    "Live broadcast: OFF (next recording is local-only)".into()
                },
            );
        }
        _ => {}
    }
}

fn handle_picker_mode(app: &mut App, key: KeyCode) {
    // Ignore input while a download is in progress (no error yet).
    let download_in_flight = app
        .picker
        .as_ref()
        .and_then(|p| p.downloading.as_ref())
        .map(|arc| arc.lock().error.is_none())
        .unwrap_or(false);
    if download_in_flight {
        return;
    }

    let catalog = voice_bird::transcription::models::Catalog::builtin();
    let total = catalog.all().len();

    match key {
        KeyCode::Up | KeyCode::Char('k') => {
            if let Some(picker) = app.picker.as_mut() {
                picker.index = picker.index.saturating_sub(1);
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Some(picker) = app.picker.as_mut() {
                if picker.index + 1 < total {
                    picker.index += 1;
                }
            }
        }
        KeyCode::Enter => {
            let idx = app.picker.as_ref().map(|p| p.index).unwrap_or(0);
            if let Some(entry) = catalog.all().get(idx).cloned() {
                app.begin_model_download(&entry);
            }
        }
        KeyCode::Esc => {
            if app.config_was_loaded_from_disk {
                app.mode = AppMode::Normal;
                app.picker = None;
            }
            // First run: Esc is ignored — user must pick a model.
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

fn stop_all_sessions(app: &mut App) {
    log::info!("Stopping all sessions");
    app.stop_recording();
}

/// Append one JSON-line snapshot of relevant App state to `path`.
/// Used by the e2e_human harness to drive the TUI without relying on
/// screenshot OCR. Errors are intentionally swallowed — debug-only feature
/// must never crash the TUI.
fn write_state_snapshot(app: &App, last_key: &str, path: &Path) {
    use std::io::Write;

    let status = match &app.status {
        RecordingStatus::Idle => "Idle".to_string(),
        RecordingStatus::Recording => "Recording".to_string(),
        RecordingStatus::Error(s) => format!("Error: {s}"),
    };

    // When in Settings mode, surface the focused field's label, current
    // displayed value, and the cycle options (if any) so the agent can
    // pick the right key to send next without needing pixels.
    let (settings_field_label, settings_field_value, settings_field_options) =
        if app.mode == AppMode::Settings {
            let fields = settings_view::FIELDS;
            if let Some(fld) = fields.get(app.settings_cursor) {
                let value = settings_view::field_display(app, fld.key);
                let options = settings_view::cycle_options_for(fld.key, app);
                (fld.label.to_string(), value, options)
            } else {
                (String::new(), String::new(), Vec::new())
            }
        } else {
            (String::new(), String::new(), Vec::new())
        };

    let device_names: Vec<String> =
        app.sessions.iter().map(|s| s.device_name.clone()).collect();
    let selected_device_name = app
        .sessions
        .get(app.selected_index)
        .map(|s| s.device_name.clone())
        .unwrap_or_default();

    let committed = app.committed.lock();
    let committed_count = committed.len();
    let last_committed_text = committed
        .last()
        .map(|c| c.text.clone())
        .unwrap_or_default();
    drop(committed);
    let tentative_text = app.tentative.lock().clone();

    let snap = serde_json::json!({
        "ts": chrono::Utc::now().to_rfc3339(),
        "last_key": last_key,
        "mode": format!("{:?}", app.mode),
        "status": status,
        "cloud_broadcast_active": app.cloud_broadcast_active,
        "engine_kind": app.engine_kind,
        "duration_secs": app.duration,
        "banner": app.banner,
        "status_message": app.status_message,
        "device_names": device_names,
        "selected_device_index": app.selected_index,
        "selected_device_name": selected_device_name,
        "settings_cursor": app.settings_cursor,
        "settings_field_label": settings_field_label,
        "settings_field_value": settings_field_value,
        "settings_field_options": settings_field_options,
        "settings_edit_buf": app.settings_edit_buf,
        "settings_error": app.settings_error,
        "committed_count": committed_count,
        "last_committed_text": last_committed_text,
        "tentative_text": tentative_text,
    });

    let line = match serde_json::to_string(&snap) {
        Ok(s) => s,
        Err(_) => return,
    };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{line}");
    }
}
