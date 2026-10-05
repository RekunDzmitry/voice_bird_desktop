//! Binary entry point: the only file that touches a real terminal.

use crossterm::{
    cursor,
    event::{self, Event, KeyEvent},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io::{self, Stdout};
use std::sync::Arc;
use std::time::Duration;

use voice_bird_next::bus::{AppEvent, EventBus, EventSender};
use voice_bird_next::db::{downloads, Database};
use voice_bird_next::dispatcher::Dispatcher;
use voice_bird_next::download::Downloader;
#[cfg(feature = "net")]
use voice_bird_next::download::HttpDownloader;
use voice_bird_next::model_watch::ModelWatcher;
use voice_bird_next::state::UiState;
use voice_bird_next::transcription_models::{CacheDirStore, ModelStore};
use voice_bird_next::{input, producer};
/// Runs `restore` on drop.
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
///
/// The resolver does NOT hold `Downloader` or `ModelStore` — it
/// only publishes `BeginLanguage` on Enter/Retry. The dispatcher
/// that owns the collaborators answers it.
fn handle_key(key: KeyEvent, state: &UiState, db: &mut Database, tx: &EventSender) {
    if let Some(intent) = input::map_key(key) {
        producer::resolve_intent(intent, state, db, tx);
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
    let mut db = match voice_bird_next::db::db_path() {
        Some(path) => match Database::open(&path, tx.clone()) {
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
    let dispatcher = Dispatcher::new(
        downloader.clone(),
        store.clone(),
        voice_bird_next::audio_source::system_sources(),
    );
    let watcher = ModelWatcher::new(store.clone());
    let mut dirty = true;
    loop {
        if dirty {
            terminal.draw(|f| voice_bird_next::ui::render(f, &state))?;
        }
        // Drain once, fold UI events, then dispatch the same
        // events to the dispatcher. The dispatcher answers
        // `BeginLanguage` by calling `download::begin_language` and
        // `DiscardInflight` by calling
        // `ModelStore::discard_inflight`. Command variants
        // published during dispatch (language selection, cache-hit
        // diagnostics, `DownloadRequested`) appear in `events` on the
        // next tick.
        if event::poll(TICK)? {
            if let Event::Key(k) = event::read()? {
                handle_key(k, &state, &mut db, &tx);
                dirty = true;
            }
        }
        watcher.check(&state, &tx);
        let events: Vec<AppEvent> = bus.drain().collect();
        if !events.is_empty() {
            let mut accepted_events = Vec::with_capacity(events.len());
            for ev in events {
                if let Some(l) = log.as_mut() {
                    l.append(&ev);
                }
                if let Ok(true) = voice_bird_next::db::apply(&mut db, &ev) {
                    state.apply(&ev);
                    accepted_events.push(ev);
                }
            }
            dispatcher.dispatch(&accepted_events, &mut db, &tx);
            dirty = true;
        }
        if state.should_quit {
            cleanup_inflight(&mut db, &mut bus, &dispatcher);
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
/// `DiscardInflight { model }` is published for each active
/// claim instead of reaching into the model store directly. The
/// dispatcher that owns the store answers it on the next drain.
fn cleanup_inflight(db: &mut Database, bus: &mut EventBus, dispatcher: &Dispatcher) {
    let active = match downloads::active(db) {
        Ok(a) => a,
        Err(_) => return,
    };
    for row in &active {
        let _ = downloads::cancel(db, row.model.as_ref());
    }
    let models: Vec<std::sync::Arc<str>> = active.iter().map(|r| r.model.clone()).collect();
    for model in models {
        bus.sender().publish(AppEvent::DiscardInflight { model });
    }
    // Drain the DiscardInflight events we just published and run
    // them through the dispatcher so `ModelStore::discard_inflight`
    // is actually called. Without this dispatch step, the staged
    // archive and unpack scratch directory for every in-flight
    // attempt would survive process exit, and the next session's
    // first pick on the same model would attempt to unpack from
    // a half-written `.tmp/` (the user-observed 2026-09-16
    // `install: unpack: failed to unpack ...tmp/...` error).
    let events: Vec<AppEvent> = bus.drain().collect();
    for ev in &events {
        if let Some(l) = voice_bird_next::event_log::EventLog::open().as_mut() {
            l.append(ev);
        }
    }
    dispatcher.dispatch(&events, db, &bus.sender());
}

#[cfg(feature = "net")]
fn cfg_build_downloader() -> Arc<dyn Downloader> {
    Arc::new(HttpDownloader)
}

#[cfg(not(feature = "net"))]
fn cfg_build_downloader() -> Arc<dyn Downloader> {
    // Without the `net` feature the binary can't download. Tests
    // exercise the full flow through FixtureDownloader; the
    // binary is the production switch and exits early here.
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
