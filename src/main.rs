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
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, MouseEventKind,
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
    // `--mcp-server` and `--register` were the MCP-server and
    // agent-runtime registration entry points. Both removed in §8
    // along with `src/agent/mcp_server.rs` and
    // `src/agent/register.rs`; the `omp` integration no longer
    // exists. Users who need an LLM-backed Agent run the
    // desktop, pick a Agent at voicebird.app, and press `g`
    // (§11).
    let args: Vec<String> = std::env::args().collect();
    // Handle `--recover <dir>` before any TTY/terminal setup so it works
    // from non-TTY shells (e.g., piped or macOS `open` invocations).
    if let Some(pos) = args.iter().position(|a| a == "--recover") {
        let dir = args
            .get(pos + 1)
            .ok_or_else(|| anyhow::anyhow!("--recover requires a path"))?;
        voice_bird_cli::session::recover::recover(std::path::Path::new(dir))?;
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
                        match &app.mode {
                            AppMode::Normal => handle_normal_mode(app, key.code),
                            AppMode::ModelPicker => handle_picker_mode(app, key.code),
                            AppMode::Help => handle_help_mode(app, key.code),
                            AppMode::Status => handle_status_mode(app, key.code),
                            AppMode::ApiKeyModal => handle_api_key_modal(app, &key),
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
        // `?` is help-only (D7 resolved in #33): status lives on
        // its own key so a status event can never race the help
        // modal.
        KeyCode::Char('?') => app.toggle_help(),
        KeyCode::Char('t') => app.toggle_status(),
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
                Agents => Apps,
            };
            app.picker_focus = next;
            log::info!("keys: Left → picker_focus = {:?}", next);
        }
        KeyCode::Right => {
            use crate::app::PickerFocus::*;
            let next = match app.picker_focus {
                Devices => Apps,
                Apps => Agents,
                Agents => Agents,
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
        // `+` / `-` grow or shrink the slot workspace. `+` is
        // disabled when at `MAX_SECTIONS` slots (workspace cap);
        // `-` is disabled on a single-slot workspace or when the
        // focused slot is recording (the user must `s`top it
        // first — keeps the runtime cleanly disposed).
        KeyCode::Char('+') | KeyCode::Char('=') => match app.add_slot() {
            Some(id) => {
                log::info!("keys: + → added slot {id}");
            }
            None => {
                app.banner = Some(format!(
                    "Already at the max of {} slots — remove one with [-] first",
                    crate::app::MAX_SECTIONS
                ));
                log::info!(
                    "keys: + → refused (at MAX_SECTIONS = {})",
                    crate::app::MAX_SECTIONS
                );
            }
        },
        KeyCode::Char('-') | KeyCode::Char('_') => {
            if app.remove_focused_slot() {
                log::info!(
                    "keys: - → removed focused slot; now {} slot(s)",
                    app.slots.len()
                );
            } else {
                // Either only one slot left, or the focused one is
                // recording. Surface a banner so the user knows
                // which (and what to do).
                let recording = app
                    .slot_index(app.focused_slot)
                    .map(|i| matches!(app.slots[i].kind, crate::app::SlotKind::Recording { .. }))
                    .unwrap_or(false);
                let msg = if recording {
                    "Stop the recording with [s] before removing the slot".into()
                } else {
                    "At least one slot must remain — no slot to remove".into()
                };
                app.banner = Some(msg);
                log::info!(
                    "keys: - → refused ({})",
                    app.banner.as_deref().unwrap_or("?")
                );
            }
        }
        // `a`/`e`/`d` agent CRUD keys removed in §8 — see plan §8.1.
        // Their handler functions and `AppMode` variants are gone in §8.3.
        KeyCode::Enter => {
            // When the Agents pane is focused, Enter first applies
            // the picked target to the focused slot's
            // pending_target_overrides (so start_section consumes
            // it). The picked target may be disabled (e.g. Agent when
            // the binary is missing) — in that case we surface a
            // banner and abort; the user can press Down to land on
            // a pickable row.
            if app.picker_focus == crate::app::PickerFocus::Agents {
                if let Some(kind) = app.focused_target_kind() {
                    let target = app.pick_target(kind);
                    log::info!(
                        "keys: Enter in Agents pane → picked {target:?} for slot {}",
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
            // Hand the rest off to App::try_start_new_section. The
            // Agents pick_target above is the only Enter-specific
            // UI concern; everything else (slot pick, source
            // resolution, config persist, start_section) is shared
            // and lives in the App method so it can be unit-tested.
            match app.try_start_new_section() {
                Ok(()) => {
                    log::info!(
                        "keys: Enter → start_section OK; sections active = {}",
                        app.active_section_count()
                    );
                }
                Err(msg) => {
                    log::warn!("keys: Enter → start_section FAILED: {msg}");
                    app.banner = Some(msg.clone());
                    app.status = RecordingStatus::Error(msg);
                }
            }
        }
        // 'r' refreshes the inventory; refresh_inventory preserves
        // both cursors by name when the previously-cursored
        // entries still exist after refresh.
        KeyCode::Char('r') => {
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

        // 'R' (Shift+r) resumes capture in the focused slot. The
        // lowercase `r` is taken by refresh, so resume lives on the
        // shifted variant of the same letter — discoverable as
        // "uppercase R for resume". A no-op (with a banner) when
        // there is nothing to resume, so the key is never silently
        // dead.
        KeyCode::Char('R') => {
            let slot = app.focused_slot;
            match app.resume_section(slot) {
                Ok(()) => {
                    log::info!("keys: R → resume_section[{slot}]");
                }
                Err(msg) => {
                    log::info!("keys: R → resume_section[{slot}] no-op: {msg}");
                    app.banner = Some(msg);
                }
            }
        }


        // 'K' (Shift+k) opens the API-key modal from any normal-mode
        // state (idle or recording). The lowercase 'k' is unbound
        // (we don't steal it for modal-only access because it could
        // collide with future text-input features). The user reported
        // needing to manage their key while a recording was already
        // running; in that state 'c' toggles cloud_on on the focused
        // section, so a dedicated K shortcut gives them a reliable
        // discoverable path to the modal. Pairs with the existing
        // capital-letter shortcuts: 'R' resume, 'S' stop all, 'T'
        // status.
        KeyCode::Char('K') => {
            log::info!("keys: K -> open_api_key_modal");
            // `false`: K is a peek/edit shortcut. The user opens the
            // modal to read the saved key (the modal's "Current key:"
            // row) or to type a new one. Esc must close the modal
            // silently without touching cloud — the cloud-enable
            // flow is its own funnel (the `c` toggle's
            // "Cloud ON, no key" branch sets `reverts_cloud = true`).
            app.open_api_key_modal(false);
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
            // `false`: there's no cloud toggle to revert on Windows;
            // Cloud is always on. Esc just closes the modal.
            app.open_api_key_modal(false);
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
                // Idle: no section is focused. The Mode panel
                // advertises the GLOBAL flag
                // (`cloud_broadcast_enabled`); a press of `c`
                // must visibly flip the advertised state. Seed
                // the flip from the displayed value (not from
                // any stale per-source override) and write the
                // result to BOTH the per-source override AND
                // the global flag so they stay in lockstep —
                // the panel, the next-start state, and the
                // banner all agree on the new value.
                //
                // Pre-this-commit the seed was
                // `!ov.cloud_on` (per-source override's
                // current value). When the override was stale
                // — a prior session left it at OFF while the
                // global is now ON — the first press of `c`
                // was a visible no-op: panel said ON, user
                // pressed `c` expecting OFF, both values
                // landed on ON. Reproduces the bug screenshot
                // from #48 review.
                let source = app.resolve_picker_source();
                let key = source
                    .as_ref()
                    .map(|s| app.source_key_for(s));
                // Displayed value → flip target. The per-source
                // override is rewritten to match (so the
                // next start sees the new state), and the
                // global is rewritten to match (so the panel
                // and the next-start default see the new
                // state).
                let on = !app.config.cloud_broadcast_enabled;
                let ov = voice_bird_cli::config::SourceSettingsOverride {
                    cloud_on: on,
                    // Preserve the other dimensions of the
                    // per-source override when one exists, so
                    // this toggle doesn't clobber a user's
                    // per-source language/model preferences
                    // when they flip Cloud.
                    language: key
                        .as_ref()
                        .and_then(|k| app.config.source_overrides.get(k))
                        .map(|existing| existing.language.clone())
                        .unwrap_or_else(|| app.config.language.clone()),
                    model: key
                        .as_ref()
                        .and_then(|k| app.config.source_overrides.get(k))
                        .map(|existing| existing.model.clone())
                        .unwrap_or_else(|| app.config.default_model.clone()),
                };
                if let Some(k) = key {
                    app.config.upsert_source_override(k, ov);
                }
                app.config.cloud_broadcast_enabled = on;
                if let Err(e) = app.config.save() {
                    log::error!("config save (cloud toggle): {e}");
                }
                // Re-fetch the cloud Agents list whenever cloud
                // toggles. `refresh_agents()` no-ops (and clears
                // `self.agents`) when the gate fails (cloud off,
                // empty key, empty URL) and fetches when the gate
                // passes — covers both the ON and OFF transitions
                // without callers having to branch.
                app.refresh_agents();
                if on && app.config.voicebird_api_key.is_empty() {
                    // Cloud-enable gate. Esc reverts the just-toggled
                    // `cloud_broadcast_enabled` back to OFF (it was
                    // OFF before this `c` press — the user just
                    // flipped it). Without this, the user could
                    // press `c` to enable Cloud, then change their
                    // mind and `Esc` out of the modal — and be left
                    // with Cloud ON, no key, and no way to start a
                    // recording until they re-toggle `c` OFF.
                    app.open_api_key_modal(true);
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
        // Agents picker pane — the user picks a target with the
        // same arrow / Enter pattern as Devices and Apps. The change
        // is queued in `pending_target_overrides` by the Enter
        // handler when the Agents pane is focused.
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

fn handle_api_key_modal(app: &mut App, key: &KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            // Cancel: only revert the cloud toggle if THIS modal
            // was opened as the cloud-enable gate. `K` peek,
            // first-run bootstrap, auth-failure recovery, and
            // `start_section`'s pre-flight all set
            // `api_key_modal_reverts_cloud = false` — for those
            // flows Esc must close the modal silently, because
            // cloud is either already on (and the user just wants
            // to fix the key) or not in play (first run / peek).
            //
            // The cloud-enable funnel is the one path that
            // flipped `cloud_broadcast_enabled` from false to
            // true just before opening the modal; cancelling the
            // modal there must unwind that flip or the user is
            // stuck with Cloud ON + no key + no way to start a
            // recording. (The pre-R-key world had no other exit.)
            if app.api_key_modal_reverts_cloud {
                #[cfg(not(windows))]
                {
                    app.config.cloud_broadcast_enabled = false;
                    if let Err(e) = app.config.save() {
                        log::error!("config save (modal cancel): {e}");
                    }
                    app.banner =
                        Some("Cloud: OFF (cancelled API key entry)".into());
                }
                #[cfg(windows)]
                {
                    // Windows can't fall back to local, so cloud
                    // stays on; the start-recording guard re-opens
                    // this modal when needed. (This branch is
                    // defensive — the c-toggle's `reverts_cloud =
                    // true` path is non-Windows only.)
                    app.banner = Some(
                        "Windows is cloud-only — press 'c' to set an API key before recording"
                            .into(),
                    );
                }
            }
            app.api_key_buf = None;
            app.api_key_modal_reverts_cloud = false;
            app.mode = AppMode::Normal;
        }
        KeyCode::Enter => {
            if let Some(buf) = app.api_key_buf.take() {
                // Distinguish "user saved a new key" from "user
                // wiped the buffer (Ctrl+U or Backspace × N) and
                // committed an empty key". The first case sets
                // `voicebird_api_key` to a non-empty string and
                // the existing 'API key saved' banner is fine.
                // The second case CLEARS the saved key on disk
                // — `voicebird_api_key` becomes `""` and the
                // old "API key saved" banner is actively
                // misleading. The cloud-enable gate (if this
                // modal was opened as one) will re-trigger on
                // the next recording because `key.is_empty()`
                let trimmed = buf.trim();
                app.config.voicebird_api_key = trimmed.to_string();
                if let Err(e) = app.config.save() {
                    log::error!("config save (modal save): {e}");
                    app.banner = Some(format!("Save failed: {e}"));
                } else if trimmed.is_empty() {
                    app.banner = Some("API key cleared".into());
                    // Empty key: clear any stale agents list so the
                    // picker doesn't keep showing entries from a
                    // prior fetch.
                    app.refresh_agents();
                } else {
                    app.banner =
                        Some("API key saved — start a recording to verify".into());
                    // New key just landed: re-fetch the Agents list
                    // so the picker reflects what this key is
                    // authorized to see. Without this the picker
                    // stays empty until the next `App::new()`
                    // (i.e. process restart).
                    app.refresh_agents();
                }
            }
            app.mode = AppMode::Normal;
        }
        KeyCode::Backspace => {
            if let Some(buf) = app.api_key_buf.as_mut() {
                buf.pop();
            }
        }
        // Ctrl+U clears the buffer so the user can retype from scratch
        // (standard text-editor kill-line). crossterm encodes this as
        // `KeyCode::Char('u')` with the CONTROL modifier set; the guard
        // is mandatory - a plain `u` keystroke (no modifier) must still
        // type into the buffer normally. Saving an empty buffer (Enter
        // with the empty buf) clears the saved key on disk, which gives
        // the user a way to remove a corrupted key without editing
        // config.toml by hand.
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(buf) = app.api_key_buf.as_mut() {
                buf.clear();
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

/// Status overlay (`t`): any of the usual dismiss keys close it.
fn handle_status_mode(app: &mut App, key: KeyCode) {
    match key {
        KeyCode::Char('t') | KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter => {
            app.toggle_status();
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

/// Regression tests for the cloud toggle (PR #48 review item 8):
/// toggling Cloud while the focused slot is empty must also write
/// to the per-source override so the next start agrees with what
/// the toggle just advertised. The previous `funnel_dispatch_tests`
/// module that drove the agent-funnel key path is removed with
/// `--mcp-server` / `--register` / the funnel in §8.

// Bug: toggling Cloud (`c`) while the focused slot is empty
// only updates the global `cloud_broadcast_enabled` default,
// not the per-source override. The next start uses the
// per-source override (when present, it wins over the
// global), so the banner/display say "Cloud: ON" but the
// session starts with `cloud_on = false`. Reproduces the
// screenshot from #48 review: user toggled Cloud ON, hit
// Enter, the Mode panel flipped to OFF during the
// recording.
//
// The fix must also write to the per-source override for
// the source the picker would resolve to on Enter, so the
// next start agrees with what the toggle just advertised.
#[cfg(test)]
mod cloud_toggle_dispatch_tests {
    use super::*;
    use crate::platform::{AppSession, AudioDevice};
    use voice_bird_cli::config::AudioSessionKind;
    use voice_bird_cli::session::layout::SessionSource;

    /// `c` toggled while no section is focused must update the
    /// per-source override for the source the picker resolves
    /// to on Enter. Today (pre-fix) the `else` branch in
    /// `handle_normal_mode` only flips the global
    /// `cloud_broadcast_enabled`, leaving a stale per-source
    /// override `cloud_on = false` in place. The next start
    /// reads the per-source override first, so the section
    /// starts with `cloud_on = false` even though the user
    /// just clicked Cloud ON.
    #[test]
    fn c_toggle_when_no_section_focused_updates_per_source_override() {
        use voice_bird_cli::config::SourceSettingsOverride;

        let mut app = App::new();
        app.config.voicebird_api_key = "sk-test".into();

        // Picker is parked on EPOS PC 8 USB (output/loopback) +
        // Google Chrome — the exact combo from the screenshot.
        app.devices = vec![AudioDevice {
            name: "EPOS PC 8 USB".into(),
            kind: AudioSessionKind::Output,
        }];
        app.selected_device_index = 0;
        app.apps = vec![AppSession {
            id: "com.google.Chrome".into(),
            name: "Google Chrome".into(),
            process_id: 12345,
        }];
        app.selected_app_index = Some(0);
        app.config.input_device = Some("EPOS PC 8 USB".into());
        app.config.input_device_kind = Some(AudioSessionKind::Output);

        // Stale per-source override: this combo last recorded
        // with Cloud OFF. The toggle target is the SAME source
        // so the override must be updated.
        let source = SessionSource::App {
            id: "com.google.Chrome".into(),
            name: "Google Chrome".into(),
            device_name: "EPOS PC 8 USB".into(),
        };
        let key = app.source_key_for(&source);
        app.config.source_overrides.insert(
            key.clone(),
            SourceSettingsOverride {
                cloud_on: false,
                language: "en".into(),
                model: "base.en".into(),
            },
        );
        // Global default is also OFF today. The toggle should
        // flip BOTH the per-source override AND the global
        // default so the Mode panel ("Cloud: ON") and the
        // next-start state agree.
        app.config.cloud_broadcast_enabled = false;

        // Sanity: no Recording/Saved section is focused, so the
        // toggle must take the `else` branch in main.rs.
        assert!(app.focused().is_none(), "setup must have no focused section");

        // The user presses `c`.
        #[cfg(not(windows))]
        {
            handle_normal_mode(&mut app, KeyCode::Char('c'));

            // Assert 1: the per-source override was flipped.
            // This is the contract that picks up the toggle and
            // matches what `start_section` reads via
            // `effective_settings_for(source)`.
            let ov = app
                .config
                .source_overrides
                .get(&key)
                .expect("per-source override must exist after toggle");
            assert!(
                ov.cloud_on,
                "toggling Cloud ON while no section is focused must \
                 update the per-source override for the picker-resolved \
                 source; otherwise the next start silently uses the stale \
                 cloud_on=false. Override key: {key}"
            );

            // Assert 2: the global default also flipped, so the
            // Mode panel agrees with the next-start state.
            assert!(
                app.config.cloud_broadcast_enabled,
                "toggling Cloud ON must also flip the global default \
                 so the Mode panel (which reads cloud_broadcast_enabled \
                 when no section is focused) advertises the new state"
            );
        }
    }
}

/// `handle_api_key_modal` must clear the in-progress paste buffer when
/// the user presses Ctrl+U. The same keypress saves with an empty
/// buffer, which deletes the previously-saved key (the user wants a
/// fresh start — e.g. after spotting `sk-test` test-pollution in
/// their existing saved key, they want to retype from scratch). The
/// Char('u') WITHOUT control must still type into the buffer like
/// any other printable char.
#[cfg(test)]
mod api_key_modal_ctrl_u_tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn ctrl_u_clears_the_in_progress_api_key_buffer() {
        let mut app = App::new();
        app.mode = AppMode::ApiKeyModal;
        app.api_key_buf = Some("vb_partial_paste".into());

        let evt = KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL);
        handle_api_key_modal(&mut app, &evt);

        assert_eq!(
            app.api_key_buf.as_deref(),
            Some(""),
            "Ctrl+U must clear the in-progress buffer to an empty string"
        );
        // Mode stays in the modal so the user can keep typing.
        assert_eq!(app.mode, AppMode::ApiKeyModal);
    }

    #[test]
    fn plain_u_typing_into_api_key_buffer_works_when_control_held_is_false() {
        // Regression sentinel: Ctrl+U must NOT eat the printable 'u'
        // keystroke when no modifier is held. The handler still has a
        // normal-agent typing path.
        let mut app = App::new();
        app.mode = AppMode::ApiKeyModal;
        app.api_key_buf = Some(String::new());

        let evt = KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE);
        handle_api_key_modal(&mut app, &evt);

        assert_eq!(
            app.api_key_buf.as_deref(),
            Some("u"),
            "a plain 'u' keystroke (no modifiers) must append to the buffer"
        );
    }

    /// Ctrl+U + Enter (with the empty buffer) must surface a
    /// "cleared" banner, not the misleading "API key saved —
    /// start a recording to verify". The user explicitly
    /// removed the key; the banner should reflect that.
    #[test]
    fn enter_with_empty_buf_after_ctrl_u_shows_cleared_banner() {
        let mut app = App::new();
        app.mode = AppMode::ApiKeyModal;
        app.api_key_buf = Some("vb_partial_paste".into());

        // Ctrl+U clears the buffer…
        let ctrl_u =
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL);
        handle_api_key_modal(&mut app, &ctrl_u);
        // …Enter commits the empty buf.
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        handle_api_key_modal(&mut app, &enter);

        assert!(
            app.banner.as_deref()
                == Some("API key cleared"),
            "Ctrl+U + Enter must surface an 'API key cleared' banner \
             so the user sees the key was removed, not saved. \
             Got: {:?}",
            app.banner,
        );
        // The saved key is now empty.
        assert!(
            app.config.voicebird_api_key.is_empty(),
            "saving an empty buffer must clear the saved key on disk"
        );
        // And the modal is dismissed.
        assert_eq!(app.mode, AppMode::Normal);
    }
}

/// `K` (uppercase) opens the API-key modal from Normal mode, regardless
/// of whether a section is focused. The lowercase `k` keystroke must
/// still be a regular agent typed into nothing (we don't bind it),
/// because crossterm encodes Ctrl+K the same as a plain 'k' - the
/// uppercase letter is the discoverable, conflict-free shortcut.
#[cfg(test)]
mod api_key_dispatcher_uppercase_k_tests {
    use super::*;

    #[test]
    #[allow(non_snake_case)]
    fn uppercase_K_opens_the_api_key_modal_from_normal_mode() {
        let mut app = App::new();
        app.mode = AppMode::Normal;
        let evt = KeyEvent::new(KeyCode::Char('K'), KeyModifiers::NONE);
        handle_normal_mode(&mut app, evt.code);
        assert_eq!(
            app.mode,
            AppMode::ApiKeyModal,
            "uppercase K must open the API-key modal; mode = {:?}",
            app.mode,
        );
        assert!(
            app.api_key_buf.is_some(),
            "opening the modal must seed api_key_buf so the user sees what's saved"
        );
    }

    #[test]
    #[allow(non_snake_case)]
    fn lowercase_k_is_NOT_an_api_key_shortcut_only_uppercase_K_is() {
        let mut app = App::new();
        app.mode = AppMode::Normal;
        let evt = KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE);
        handle_normal_mode(&mut app, evt.code);
        // We do not assert a specific behaviour for lowercase 'k'
        // (it's not bound to anything), only that it does NOT open
        // the API-key modal. If a future feature binds 'k' to
        // something else, this test continues to assert what we
        // explicitly want - no overlap with the K shortcut.
        assert_ne!(
            app.mode,
            AppMode::ApiKeyModal,
            "lowercase k must not open the API-key modal",
        );
    }
}

// PR #48 review — regression guards. These started life as RED
// tests pinning contracts the code violated; the fixes landed in
// 992ae64 (item 1, Esc-from-K), a608402 (item 2, config-path
// injection), and 25ae726 (item 8, c-toggle seed). They stay as
// permanent guards against re-regression.
#[cfg(test)]
#[cfg(not(windows))]
mod pr48_review_regression_tests {
    use super::*;
    use crate::platform::AudioDevice;
    use voice_bird_cli::config::{AudioSessionKind, SourceSettingsOverride};
    use voice_bird_cli::session::layout::SessionSource;

    /// Snapshot of the developer's real `config.toml`, restored on
    /// drop (including panic unwind). With config-path injection in
    /// place the handlers under test write to the tempdir, so this
    /// guard should never observe a change — it stays as the safety
    /// net that lets `key_handlers_in_tests_must_not_write_real_user_config`
    /// fail loudly (without lasting damage) if the injection ever
    /// regresses.
    struct RealConfigGuard {
        path: std::path::PathBuf,
        before: Option<Vec<u8>>,
        _serial: std::sync::MutexGuard<'static, ()>,
    }
    impl RealConfigGuard {
        fn snapshot() -> Self {
            // Serialize the tests in this module: they all read,
            // mutate, and restore the same real file, so running
            // them in parallel would interleave snapshots and
            // restores nondeterministically.
            static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
            let serial = SERIAL
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Use the *real* config path, not the env-var-overridden
            // `AppConfig::config_path()`. These tests verify that
            // the production code does NOT touch the developer's
            // real config — the guard's snapshot/restore has to
            // refer to that real path, otherwise it would point
            // at the test tempdir and miss any real-config
            // pollution.
            let path = voice_bird_cli::test_utils::real_config_path();
            let before = std::fs::read(&path).ok();
            Self {
                path,
                before,
                _serial: serial,
            }
        }
    }
    impl Drop for RealConfigGuard {
        fn drop(&mut self) {
            match &self.before {
                Some(bytes) => {
                    let _ = std::fs::write(&self.path, bytes);
                }
                None => {
                    let _ = std::fs::remove_file(&self.path);
                }
            }
        }
    }

    /// Regression guard (review item 1; was RED, fixed in 992ae64).
    ///
    /// `K` opens the API-key modal from anywhere, and the modal's
    /// "Current key:" line invites opening it just to *look* at the
    /// saved key. The Esc arm used to unconditionally revert
    /// `cloud_broadcast_enabled` (it was written for the
    /// cloud-toggle-needs-a-key funnel, the only flow that existed
    /// before `K`) — so peek-and-Esc silently disabled cloud. Now
    /// `App::api_key_modal_reverts_cloud` records why the modal was
    /// opened and Esc only reverts cloud for the cloud-enable gate.
    #[test]
    fn esc_after_k_peek_must_not_disable_cloud() {
        let _guard = RealConfigGuard::snapshot();
        let mut app = App::new();
        app.config.cloud_broadcast_enabled = true;

        // The user peeks at the saved key via K…
        handle_normal_mode(&mut app, KeyCode::Char('K'));
        assert_eq!(app.mode, AppMode::ApiKeyModal, "K must open the modal");

        // …and backs out without changing anything.
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        handle_api_key_modal(&mut app, &esc);

        assert_eq!(app.mode, AppMode::Normal, "Esc must close the modal");
        assert!(
            app.config.cloud_broadcast_enabled,
            "Esc from a K-opened (peek) modal must NOT disable cloud — \
             the cancel-reverts-cloud behaviour only makes sense when the \
             modal was opened by the cloud-enable flow that needs a key"
        );
    }

    /// Regression guard (review item 2; was RED, fixed in a608402).
    ///
    /// `AppConfig::save()` used to write straight to the machine's
    /// real config path, so tests driving real key handlers
    /// persisted flipped flags and synthetic `sk-test` keys over
    /// the developer's real `config.toml` on every `cargo test`
    /// run. `App::new()` now installs a process-local tempdir
    /// (via `test_utils::INSTALL_TEST_CONFIG` +
    /// `VOICE_BIRD_TEST_CONFIG_PATH`) under cfg(test), so the
    /// real file must stay byte-identical across any handler run.
    #[test]
    fn key_handlers_in_tests_must_not_write_real_user_config() {
        let guard = RealConfigGuard::snapshot();
        let before = guard.before.clone();

        // Drive the idle `c` toggle — its else-branch calls
        // `app.config.save()` unconditionally.
        let mut app = App::new();
        handle_normal_mode(&mut app, KeyCode::Char('c'));

        let after = std::fs::read(&guard.path).ok();
        assert_eq!(
            before, after,
            "driving a key handler in a unit test must not rewrite the \
             developer's real config.toml at {} — inject the config path \
             (env override or App::with_config) so tests run against a \
             tempdir",
            guard.path.display()
        );
    }

    /// Regression guard (review item 8; was RED, fixed in 25ae726).
    ///
    /// The idle Mode panel displays the GLOBAL flag
    /// (`cloud_broadcast_enabled`), and the idle `c` toggle used to
    /// seed its flip from the per-source OVERRIDE's current value —
    /// when the two disagreed (stale override), the first press was
    /// a visible no-op. The toggle now seeds from the displayed
    /// (global) value and writes it to both the global flag and the
    /// per-source override, so one press always flips the
    /// advertised state.
    #[test]
    fn idle_c_toggle_must_flip_the_displayed_cloud_state() {
        let _guard = RealConfigGuard::snapshot();
        let mut app = App::new();

        // Panel says ON (global flag)…
        app.config.cloud_broadcast_enabled = true;
        // …but a stale per-source override says OFF for the source
        // the picker currently resolves to.
        app.devices = vec![AudioDevice {
            name: "MacBook Pro Microphone".into(),
            kind: AudioSessionKind::Input,
        }];
        app.selected_device_index = 0;
        let key = app.source_key_for(&SessionSource::Microphone);
        app.config.source_overrides.insert(
            key.clone(),
            SourceSettingsOverride {
                cloud_on: false,
                language: "en".into(),
                model: "base.en".into(),
            },
        );

        // The user sees "Cloud: ON" and presses `c` to turn it OFF.
        handle_normal_mode(&mut app, KeyCode::Char('c'));

        assert!(
            !app.config.cloud_broadcast_enabled,
            "the toggle must flip the DISPLAYED state: panel showed ON, so \
             one press of `c` must land on OFF — seeding the flip from the \
             stale override (OFF→ON) makes the first press a visible no-op"
        );
        assert!(
            !app
                .config
                .source_overrides
                .get(&key)
                .expect("override must survive the toggle")
                .cloud_on,
            "the per-source override must agree with the newly displayed \
             state (OFF) after the toggle"
        );
    }
}
