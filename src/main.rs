mod app;
mod logger;
mod platform;
mod ui;

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseEventKind,
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
        fn objc_msgSend(
            obj: *mut std::ffi::c_void,
            sel: *mut std::ffi::c_void,
            ...
        ) -> *mut std::ffi::c_void;
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
    // `--mcp-server` runs voice-bird as a stdio MCP server for omp.
    // Disables the TUI; blocks on stdin/stdout JSON-RPC. We
    // intentionally check this before any TTY setup so the parent
    // (omp) gets a clean pipe instead of a raw-mode terminal.
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--mcp-server") {
        let session = voice_bird_cli::omp::mcp_server::resolve_initial_session_id(&args);
        let state = voice_bird_cli::omp::mcp_server::ServerState::new(session);
        return voice_bird_cli::omp::mcp_server::run_on_stdio(state);
    }

    // Handle `--recover <dir>` before any TTY/terminal setup so it works
    // from non-TTY shells (e.g., piped or macOS `open` invocations).
    let args: Vec<String> = std::env::args().collect();
    if let Some(pos) = args.iter().position(|a| a == "--recover") {
        let dir = args
            .get(pos + 1)
            .ok_or_else(|| anyhow::anyhow!("--recover requires a path"))?;
        voice_bird_cli::session::recover::recover(std::path::Path::new(dir))?;
        println!("Recovered transcripts in {}", dir);
        return Ok(());
    }

    // `--register` writes this binary's path into ~/.omp/agent/mcp.json
    // so the user's `omp` picks up voice-bird as an MCP server on next
    // launch. Idempotent: re-running updates the entry in place. The
    // binary path defaults to current_exe(); pass a different one as
    // the first arg to register a different build (e.g., a release
    // copy under ~/bin). Intended for users who never run the TUI
    // (CI, sandboxed agent runs, or just to wire up MCP before
    // launching the TUI for the first time).
    let args: Vec<String> = std::env::args().collect();
    if let Some(pos) = args.iter().position(|a| a == "--register") {
        let binary = match args.get(pos + 1) {
            Some(p) => std::path::PathBuf::from(p),
            None => std::env::current_exe()?,
        };
        let home = voice_bird_cli::omp::register::register_home();
        voice_bird_cli::omp::register::register(&binary, &home)?;
        println!(
            "registered {} in {}/agent/mcp.json",
            binary.display(),
            home.display(),
        );
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
    // the TUI on stderr. No whisper.cpp on cloud-only Windows.
    #[cfg(not(windows))]
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
    app.refresh_inventory();

    // Seed Devices-pane cursor to the saved device. Many devices (USB
    // headsets like EPOS) appear twice — once as Input (mic) and once
    // as Output (for loopback capture). Prefer matching on BOTH name
    // and saved kind so a user who picked the loopback variant gets the
    // loopback variant back. Falls back to name-only match for old
    // configs that did not store input_device_kind.
    if let Some(saved) = app.config.input_device.clone() {
        let saved_kind = app.config.input_device_kind;
        let by_name_and_kind = saved_kind.and_then(|k| {
            app.devices
                .iter()
                .position(|d| d.name == saved && d.kind == k)
        });
        let i = by_name_and_kind.or_else(|| app.devices.iter().position(|d| d.name == saved));
        if let Some(i) = i {
            app.selected_device_index = i;
        }
    }
    // Seed Apps-pane cursor to the last-used app id, if it still exists.
    if let Some(last_id) = app.config.last_app_id.clone() {
        app.selected_app_index = app.apps.iter().position(|a| a.id == last_id);
    }

    log::info!(
        "Found {} devices, {} apps",
        app.devices.len(),
        app.apps.len()
    );
    for (i, d) in app.devices.iter().enumerate() {
        log::info!("  device[{}] = {:?} kind={:?}", i, d.name, d.kind);
    }
    log::info!(
        "config.input_device = {:?}, cursor selected_device_index = {}, selected_app_index = {:?}",
        app.config.input_device,
        app.selected_device_index,
        app.selected_app_index,
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
                            AppMode::ApiKeyModal => handle_api_key_modal(app, key.code),
                            AppMode::PathModal => handle_path_modal(app, key.code),
                        }
                        if let Some(path) = debug_snapshot_path {
                            write_state_snapshot(app, &format!("{:?}", key.code), path);
                        }
                    }
                    Event::Mouse(mouse) if app.mode == AppMode::Normal => match mouse.kind {
                        MouseEventKind::ScrollUp => app.scroll_transcript_up(3),
                        MouseEventKind::ScrollDown => app.scroll_transcript_down(3),
                        _ => {}
                    },
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
        // Tab cycles which section the c/l/m/s/PgUp keys target.
        KeyCode::Tab => {
            app.focus_next();
            log::info!(
                "keys: Tab → focused_slot = {} (sections active = {})",
                app.focused_slot,
                app.active_section_count()
            );
        }
        KeyCode::BackTab => {
            app.focus_prev();
            log::info!(
                "keys: BackTab → focused_slot = {} (sections active = {})",
                app.focused_slot,
                app.active_section_count()
            );
        }
        // ↑/↓/k/j navigate within the focused picker pane.
        KeyCode::Up | KeyCode::Char('k') => app.select_previous(),
        KeyCode::Down | KeyCode::Char('j') => app.select_next(),
        // ←/→ cycle picker pane focus. `h` is aliased to `←` for
        // vim-style users. `l` is deliberately not bound: it cycles
        // cloud language already, and we don't want the picker
        // focus to drift while the user is in cloud mode.
        KeyCode::Left | KeyCode::Char('h') => {
            use crate::app::PickerFocus::*;
            let next = match app.picker_focus {
                Devices => Devices,
                Apps => Devices,
                Targets => Apps,
            };
            app.picker_focus = next;
            log::info!("keys: Left → picker_focus = {:?}", next);
        }
        KeyCode::Right => {
            use crate::app::PickerFocus::*;
            let next = match app.picker_focus {
                Devices => Apps,
                Apps => Targets,
                Targets => Targets,
            };
            app.picker_focus = next;
            log::info!("keys: Right → picker_focus = {:?}", next);
        }
        // Space clears the Apps cursor → starts the section with the
        // device alone. Only meaningful with the Apps pane focused; in
        // the Devices pane it's a no-op (avoids surprising behaviour).
        KeyCode::Char(' ') => {
            if app.picker_focus == crate::app::PickerFocus::Apps {
                app.clear_app_selection();
                log::info!("keys: Space → cleared app selection");
            }
        }
        KeyCode::Enter => {
            // When the Targets pane is focused, Enter first applies
            // the picked target to the focused slot's
            // pending_target_overrides (so start_section consumes
            // it). The picked target may be disabled (e.g. Omp when
            // the binary is missing) — in that case we surface a
            // banner and abort; the user can press Down to land on
            // a pickable row.
            if app.picker_focus == crate::app::PickerFocus::Targets {
                if let Some(kind) = app.focused_target_kind() {
                    let target = app.pick_target(kind);
                    log::info!(
                        "keys: Enter in Targets pane → picked {target:?} for slot {}",
                        app.focused_slot.0
                    );
                } else {
                    let i = app.selected_target_index.unwrap_or(0);
                    app.banner = Some(format!(
                        "Target row {i} is disabled (binary missing?) — pick a different row"
                    ));
                    return;
                }
            }
            // Pick the next free slot (or refuse if all are full).
            let Some(slot) = app.next_free_slot() else {
                log::info!("keys: Enter → refused (all 3 sections full)");
                app.banner = Some("All 3 sections are recording — stop one first ([s])".into());
                return;
            };
            use voice_bird_cli::config::AudioSessionKind;
            use voice_bird_cli::session::layout::SessionSource;

            let Some(dev) = app.devices.get(app.selected_device_index).cloned() else {
                log::warn!(
                    "keys: Enter → refused (no device at idx {})",
                    app.selected_device_index
                );
                app.banner = Some("No audio device selected — press [r] to refresh".into());
                return;
            };
            let app_pick = app.focused_app().cloned();
            log::info!(
                "keys: Enter → slot={} focused_slot_before={} dev=({:?}, {:?}) app={:?}",
                slot,
                app.focused_slot,
                dev.name,
                dev.kind,
                app_pick.as_ref().map(|a| (a.name.clone(), a.id.clone())),
            );

            // Reject Input + App: per-app loopback can't pair with a
            // mic capture, and combining them silently would just record
            // the mic.
            if matches!(dev.kind, AudioSessionKind::Input) && app_pick.is_some() {
                app.banner = Some(
                    "Mic + per-app capture isn't supported — pick an output device or [Space] to clear the app".into(),
                );
                return;
            }

            // Persist the selected device + last app id.
            let name_changed = app.config.input_device.as_deref() != Some(dev.name.as_str());
            let kind_changed = app.config.input_device_kind != Some(dev.kind);
            let app_id_changed =
                app.config.last_app_id.as_deref() != app_pick.as_ref().map(|a| a.id.as_str());
            if name_changed || kind_changed || app_id_changed {
                app.config.input_device = Some(dev.name.clone());
                app.config.input_device_kind = Some(dev.kind);
                app.config.last_app_id = app_pick.as_ref().map(|a| a.id.clone());
                if let Err(e) = app.config.save() {
                    log::error!("config save: {e}");
                }
            }

            let source = match (dev.kind, app_pick) {
                (AudioSessionKind::Input, _) => SessionSource::Microphone,
                (AudioSessionKind::Output, None) => SessionSource::System,
                (AudioSessionKind::Output, Some(a)) => SessionSource::App {
                    id: a.id,
                    name: a.name,
                    device_name: dev.name.clone(),
                },
                (AudioSessionKind::App, _) => {
                    // Devices pane never emits AudioSessionKind::App
                    // entries (apps live in the Apps pane), but be safe.
                    app.banner = Some("Unexpected device kind — press [r] to refresh".into());
                    return;
                }
            };

            // Start in the chosen slot; route through the per-section API
            // so settings come from `effective_settings_for(source)`.
            let settings = app.effective_settings_for(&source);
            app.focused_slot = slot;
            log::info!(
                "keys: Enter → resolved source={:?}; calling start_section[{}]",
                source,
                slot
            );
            match app.start_section(slot, source, settings) {
                Ok(()) => {
                    log::info!(
                        "keys: Enter → start_section[{}] OK; sections active = {}",
                        slot,
                        app.active_section_count()
                    );
                }
                Err(msg) => {
                    log::warn!("keys: Enter → start_section[{}] FAILED: {}", slot, msg);
                    app.banner = Some(msg.clone());
                    app.status = RecordingStatus::Error(msg);
                }
            }
        }
        KeyCode::Char('r') => {
            // Refresh inventory; refresh_inventory preserves both cursors.
            let before_d = app.devices.len();
            let before_a = app.apps.len();
            app.refresh_inventory();
            log::info!(
                "keys: r refresh_inventory: devices {} → {}, apps {} → {}",
                before_d,
                app.devices.len(),
                before_a,
                app.apps.len()
            );
        }
        // 's' stops the focused section (no-op if its slot is empty).
        KeyCode::Char('s') => {
            let slot = app.focused_slot;
            let pos = app.slot_index(slot);
            let is_recording = pos
                .and_then(|i| app.slots.get(i))
                .map(|s| matches!(s.kind, crate::app::SlotKind::Recording { .. }))
                .unwrap_or(false);
            if is_recording {
                log::info!("keys: s → stop_section[{}]", slot);
                app.stop_section(slot);
            } else {
                log::info!("keys: s → no-op (slot {slot} empty)");
            }
        }
        // 'x' clears the transcript for the focused slot (both live and
        // saved/preserved).
        KeyCode::Char('x') => {
            let slot = app.focused_slot;
            let had_text = app.focused_committed().lock().len() > 0
                || app
                    .slot_index(slot)
                    .and_then(|i| app.slots.get(i))
                    .map(|s| matches!(s.kind, crate::app::SlotKind::Saved { .. }))
                    .unwrap_or(false);
            app.clear_slot_transcript(slot);
            if had_text {
                log::info!("keys: x → cleared transcript for slot {slot}");
            }
        }

        KeyCode::Char('m') => {
            // Manual model override. Seeds the picker at the current
            // displayed model (focused section's if running, else the
            // global default) so the user sees what's already chosen.
            let catalog = voice_bird_cli::transcription::models::Catalog::builtin();
            let current = app.display_model();
            let current_idx = catalog
                .all()
                .iter()
                .position(|m| m.id == current.as_str())
                .unwrap_or(0);
            app.picker = Some(crate::app::PickerState {
                index: current_idx,
                downloading: None,
            });
            app.mode = AppMode::ModelPicker;
        }
        #[cfg(windows)]
        KeyCode::Char('m') => {
            // No local models on cloud-only Windows; keep the key from
            // being silently dead.
            app.banner = Some("Windows is cloud-only — no local models".into());
        }
        // Windows is cloud-only: 'c' never toggles the mode, it only
        // opens the API-key modal (the one cloud setting that matters).
        #[cfg(windows)]
        KeyCode::Char('c') => {
            app.open_api_key_modal();
        }
        // Toggle cloud transcription. When idle, mutates the global
        // config so the next-start defaults flip (and the mode panel
        // updates). When a section is focused, mutates that section's
        // settings AND persists a per-source override. The running
        // engine itself is untouched until the user stops & restarts —
        // mid-flight engine rebuild is Stage 3+.
        #[cfg(not(windows))]
        KeyCode::Char('c') => {
            if let Some(section) = app.focused_mut() {
                section.settings.cloud_on = !section.settings.cloud_on;
                let on = section.settings.cloud_on;
                app.persist_focused_settings();
                app.banner = Some(if on {
                    "Cloud: ON for focused section (applies on next start)".into()
                } else {
                    "Cloud: OFF for focused section (applies on next start)".into()
                });
            } else {
                app.config.cloud_broadcast_enabled = !app.config.cloud_broadcast_enabled;
                let on = app.config.cloud_broadcast_enabled;
                if let Err(e) = app.config.save() {
                    log::error!("config save (cloud toggle): {e}");
                }
                if on && app.config.voicebird_api_key.is_empty() {
                    app.open_api_key_modal();
                } else {
                    app.banner = Some(if on {
                        "Cloud: ON (next recording streams to voicebird.app)".into()
                    } else {
                        "Cloud: OFF (next recording is local-only)".into()
                    });
                }
            }
        }
        // Cycle the cloud language. When idle, mutates the global config
        // (and is hidden when cloud is off). When focused-section
        // recording, cycles that section's language and persists the
        // override.
        KeyCode::Char('l') => {
            let langs = crate::app::CLOUD_LANGUAGES;
            if let Some(section) = app.focused_mut() {
                if !section.settings.cloud_on {
                    return;
                }
                let i = langs
                    .iter()
                    .position(|&l| l == section.settings.language)
                    .unwrap_or(0);
                let next = (i + 1) % langs.len();
                section.settings.language = langs[next].into();
                app.persist_focused_settings();
            } else if app.config.cloud_broadcast_enabled {
                let i = langs
                    .iter()
                    .position(|&l| l == app.config.language)
                    .unwrap_or(0);
                let next = (i + 1) % langs.len();
                app.config.language = langs[next].into();
                if let Err(e) = app.config.save() {
                    log::error!("config save (lang cycle): {e}");
                }
            }
        }
        // Export the most recent local transcript to the cloud.
        // Idempotent — second press is a no-op once .uploaded exists.
        // Local-only concept: gated off cloud-only Windows.
        #[cfg(not(windows))]
        KeyCode::Char('e') if app.active_section_count() == 0 => {
            app.export_transcript();
        }
        // Open the output-path modal. Only when idle (can't change
        // paths mid-recording — sessions have already landed).
        // Local-only concept: gated off cloud-only Windows.
        #[cfg(not(windows))]
        KeyCode::Char('p') if app.active_section_count() == 0 => {
            app.open_path_modal();
        }
        // The O / S / A target-cycle keys have been replaced by the
        // Targets picker pane — the user picks a target with the
        // same arrow / Enter pattern as Devices and Apps. The change
        // is queued in `pending_target_overrides` by the Enter
        // handler when the Targets pane is focused.
        _ => {}
    }
}

fn handle_path_modal(app: &mut App, key: KeyCode) {
    match key {
        KeyCode::Esc => {
            app.path_buf = None;
            app.mode = AppMode::Normal;
        }
        KeyCode::Enter => {
            if let Some(buf) = app.path_buf.take() {
                app.config.session_dir = buf.trim().to_string();
                if let Err(e) = app.config.save() {
                    log::error!("config save (path modal): {e}");
                    app.banner = Some(format!("Save failed: {e}"));
                } else {
                    app.banner = Some(format!(
                        "Output path → {}",
                        app.config.session_dir_expanded()
                    ));
                }
            }
            app.mode = AppMode::Normal;
        }
        KeyCode::Backspace => {
            if let Some(buf) = app.path_buf.as_mut() {
                buf.pop();
            }
        }
        KeyCode::Char(ch) => {
            if let Some(buf) = app.path_buf.as_mut() {
                buf.push(ch);
            }
        }
        _ => {}
    }
}

fn handle_api_key_modal(app: &mut App, key: KeyCode) {
    match key {
        KeyCode::Esc => {
            // Cancel: revert the cloud toggle so the user isn't left
            // with cloud=on and no key — that would block the next
            // recording. Leaves the saved key untouched, so a partial
            // edit is discarded.
            #[cfg(not(windows))]
            {
                app.config.cloud_broadcast_enabled = false;
                if let Err(e) = app.config.save() {
                    log::error!("config save (modal cancel): {e}");
                }
                app.banner = Some("Cloud: OFF (cancelled API key entry)".into());
            }
            // Windows can't fall back to local, so cloud stays on; the
            // start-recording guard re-opens this modal when needed.
            #[cfg(windows)]
            {
                app.banner = Some(
                    "Windows is cloud-only — press 'c' to set an API key before recording".into(),
                );
            }
            app.api_key_buf = None;
            app.mode = AppMode::Normal;
        }
        KeyCode::Enter => {
            if let Some(buf) = app.api_key_buf.take() {
                app.config.voicebird_api_key = buf.trim().to_string();
                if let Err(e) = app.config.save() {
                    log::error!("config save (modal save): {e}");
                    app.banner = Some(format!("Save failed: {e}"));
                } else {
                    app.banner = Some("API key saved — start a recording to verify".into());
                }
            }
            app.mode = AppMode::Normal;
        }
        KeyCode::Backspace => {
            if let Some(buf) = app.api_key_buf.as_mut() {
                buf.pop();
            }
        }
        KeyCode::Char(ch) => {
            if let Some(buf) = app.api_key_buf.as_mut() {
                buf.push(ch);
            }
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

    let catalog = voice_bird_cli::transcription::models::Catalog::builtin();
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
    app.stop_all_sections();
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

    let device_names: Vec<String> = app.devices.iter().map(|d| d.name.clone()).collect();
    let selected_device_name = app
        .devices
        .get(app.selected_device_index)
        .map(|d| d.name.clone())
        .unwrap_or_default();
    let app_names: Vec<String> = app.apps.iter().map(|a| a.name.clone()).collect();
    let selected_app_name = app
        .selected_app_index
        .and_then(|i| app.apps.get(i))
        .map(|a| a.name.clone())
        .unwrap_or_default();

    let committed_arc = app.focused_committed();
    let committed = committed_arc.lock();
    let committed_count = committed.len();
    let last_committed_text = committed.last().map(|c| c.text.clone()).unwrap_or_default();
    drop(committed);
    let tentative_text = app.focused_tentative().lock().clone();

    // Mask the API key so the snapshot file is safe to ship in test
    // artifacts. The e2e harness only needs to know whether a key is
    // present (length > 0), not what it is.
    let api_key_masked = if app.config.voicebird_api_key.is_empty() {
        String::new()
    } else {
        let n = app.config.voicebird_api_key.len();
        format!("•••{n}")
    };
    let api_key_buf_len = app.api_key_buf.as_ref().map(|s| s.len()).unwrap_or(0);

    let snap = serde_json::json!({
        "ts": chrono::Utc::now().to_rfc3339(),
        "last_key": last_key,
        "mode": format!("{:?}", app.mode),
        "status": status,
        "cloud_broadcast_active": app.focused_cloud_active(),
        "cloud_broadcast_enabled": app.config.cloud_broadcast_enabled,
        "language": app.config.language,
        "default_model": app.config.default_model,
        "engine_kind": app.focused_engine_kind(),
        "duration_secs": app.duration,
        "banner": app.banner,
        "status_message": app.status_message,
        "device_names": device_names,
        "selected_device_index": app.selected_device_index,
        "selected_device_name": selected_device_name,
        "app_names": app_names,
        "selected_app_index": app.selected_app_index,
        "selected_app_name": selected_app_name,
        "picker_focus": format!("{:?}", app.picker_focus),
        "voicebird_api_key_masked": api_key_masked,
        "api_key_buf_len": api_key_buf_len,
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
