//! Binary entry point: the only file that touches a real terminal.

use std::io::{self, Stdout};
use std::sync::Arc;
use std::time::Duration;
use crossterm::{
    cursor,
    event::{self, Event, KeyEvent},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};

#[cfg(feature = "net")]
use voice_bird_next::download::HttpDownloader;
use voice_bird_next::picker::CATALOG;
use voice_bird_next::{
    bus::{EventBus, EventSender},
    db::downloads::Downloads,
    download::Downloader,
    input, producer,
    state::UiState,
    transcription_models::{CacheDirStore, ModelStore},
};

/// Runs `restore` on drop. Constructed as soon as the first irreversible
/// terminal step (raw mode) has succeeded.
struct RestoreGuard<F: FnMut()> {
    restore: F,
}

impl<F: FnMut()> Drop for RestoreGuard<F> {
    fn drop(&mut self) {
        (self.restore)();
    }
}

fn enter_terminal<F: FnMut()>(
    enable_raw: impl FnOnce() -> io::Result<()>,
    enter_alt: impl FnOnce() -> io::Result<()>,
    restore: F,
) -> io::Result<RestoreGuard<F>> {
    enable_raw()?;
    let guard = RestoreGuard { restore };
    enter_alt()?;
    Ok(guard)
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, cursor::Show);
}

fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_hook(info);
    }));
}

fn main() -> io::Result<()> {
    install_panic_hook();
    let _guard = enter_terminal(
        enable_raw_mode,
        || execute!(io::stdout(), EnterAlternateScreen),
        restore_terminal,
    )?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    run(&mut terminal)
}

/// Map one key press to bus events via the [`producer`] module.
/// [`producer::resolve_intent`] is the single seam where user
/// intent becomes [`AppEvent`]s; the reducer does the rest.
fn handle_key(
    key: KeyEvent,
    state: &UiState,
    store: Arc<dyn ModelStore>,
    downloads: &mut Downloads,
    downloader: Arc<dyn Downloader>,
    tx: &EventSender,
) {
    if let Some(intent) = input::map_key(key) {
        producer::resolve_intent(intent, state, store, downloads, downloader, tx);
    }
}

/// Drive one tick: draw if dirty, drain events, fold into state. The
/// 100 ms poll bounds bar latency at 10 fps while the dirty flag keeps
/// an idle app from spamming the terminal.
fn run(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> io::Result<()> {
    const TICK: Duration = Duration::from_millis(100);

    let mut bus = EventBus::new();
    let tx = bus.sender();
    let mut log = voice_bird_next::event_log::EventLog::open();
    let mut state = UiState::default();

    // Wired only in `main` — the live HTTP downloader. Tests use
    // `FixtureDownloader` through the same trait.
    let store: Arc<dyn ModelStore> = match CacheDirStore::new() {
        Ok(s) => {
            let _ = s.sweep_staging();
            Arc::new(s)
        }
        Err(e) => {
            eprintln!("voice-bird-next: cannot resolve cache dir: {e}");
            std::process::exit(2);
        }
    };
    // Open the SQLite downloads table at the platform data dir.
    // Mirrors how `CacheDirStore` failures are handled: exit 2 with a
    // readable error so the failure mode is unambiguous.
    let mut downloads = match voice_bird_next::db::db_path() {
        Some(path) => match Downloads::open(&path, tx.clone()) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("voice-bird-next: cannot open downloads table at {path:?}: {e}");
                std::process::exit(2);
            }
        },
        None => {
            eprintln!("voice-bird-next: cannot resolve downloads table path");
            std::process::exit(2);
        }
    };
    let downloader: Arc<dyn Downloader> = cfg_build_downloader();

    let mut dirty = true;
    loop {
        if dirty {
            terminal.draw(|f| voice_bird_next::ui::render(f, &state))?;
            dirty = false;
        }
        if event::poll(TICK)? {
            if let Event::Key(k) = event::read()? {
                handle_key(k, &state, store.clone(), &mut downloads, downloader.clone(), &tx);
                dirty = true;
            }
        }
        for ev in bus.drain() {
            if let Some(l) = log.as_mut() {
                l.append(&ev);
            }
            // `downloads.apply` returns `true` for non-stale events
            // (non-download events pass through; download events
            // whose attempt matches the current row pass; stale
            // download events return `false`). Only accepted
            // events touch `UiState` so a cancelled-but-still-
            // running worker cannot repaint the new attempt's
            // gauge or move attempt B's blocks out of Waiting.
            if let Ok(accepted) = downloads.apply(&ev) {
                if accepted {
                    state.apply(&ev);
                }
            }
            dirty = true;
        }
        if state.should_quit {
            cleanup_inflight(&*store, &mut downloads, &mut bus);
            break;
        }
    }
    Ok(())
}

/// Drop staged archives and unpack scratch directories for every
/// model with an active claim. Called at Quit so the cache dir is
/// left clean for the next session — without this, a half-written
/// `<id>.tmp/` from a killed worker would survive and the next
/// session's first pick on the same model would attempt to unpack
/// from it (the `install: unpack: failed to unpack ...tmp/...`
/// error the user observed on 2026-09-16).
///
/// The active rows flip to `Cancelling` synchronously here; the
/// worker probes will see the change and stop. Process exit then
/// kills any workers still running.
///
/// The final `bus.drain()` loop only logs: it doesn't touch
/// state. This guarantees the Cancelling transitions reach the
/// JSONL event log even though the loop is about to exit.
fn cleanup_inflight(store: &dyn ModelStore, downloads: &mut Downloads, bus: &mut EventBus) {
    let active = match downloads.active() {
        Ok(a) => a,
        Err(_) => return,
    };
    for row in &active {
        let _ = downloads.cancel(row.model.as_ref());
    }
    for row in &active {
        if let Some(entry) = CATALOG.iter().find(|e| e.id == row.model.as_ref()) {
            store.discard_inflight(entry);
        }
    }
    for ev in bus.drain() {
        if let Some(l) = voice_bird_next::event_log::EventLog::open().as_mut() {
            l.append(&ev);
        }
    }
}

#[cfg(feature = "net")]
fn cfg_build_downloader() -> Arc<dyn Downloader> {
    Arc::new(HttpDownloader)
}

#[cfg(not(feature = "net"))]
fn cfg_build_downloader() -> Arc<dyn Downloader> {
    // exercise the full flow through FixtureDownloader; the binary
    // is the production switch and exits early here.
    eprintln!("voice-bird-next: built without the `net` feature, downloads are disabled");
    std::process::exit(2);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn fail() -> io::Result<()> {
        Err(io::Error::other("boom"))
    }

    #[test]
    fn alt_screen_failure_still_restores_raw_mode() {
        let restored = Cell::new(0);
        let result = enter_terminal(|| Ok(()), fail, || restored.set(restored.get() + 1));
        assert!(result.is_err());
        assert_eq!(restored.get(), 1, "raw mode was not rolled back");
    }

    #[test]
    fn raw_mode_failure_has_nothing_to_restore() {
        let restored = Cell::new(0);
        let result = enter_terminal(fail, || Ok(()), || restored.set(restored.get() + 1));
        assert!(result.is_err());
        assert_eq!(restored.get(), 0, "restore ran though nothing was enabled");
    }

    #[test]
    fn success_restores_exactly_once_on_drop() {
        let restored = Cell::new(0);
        {
            let _guard = enter_terminal(|| Ok(()), || Ok(()), || restored.set(restored.get() + 1))
                .expect("enter");
            assert_eq!(restored.get(), 0, "restore ran before drop");
        }
        assert_eq!(restored.get(), 1);
    }
}
