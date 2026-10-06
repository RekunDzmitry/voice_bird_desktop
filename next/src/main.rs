//! Binary entry point: the only file that touches a real terminal.

use crossterm::{
    cursor,
    event::{Event, EventStream, KeyEvent},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use futures::StreamExt;
use std::io::{self, Stdout};
use std::sync::Arc;
use std::time::Duration;

use voice_bird_next::bus::{AppEvent, EventBus, EventSender};
use voice_bird_next::db::{downloads, Database};
use voice_bird_next::consumer::{Consumer, Consumers, UiView};
use voice_bird_next::producer::download::Downloader;
#[cfg(feature = "net")]
use voice_bird_next::producer::download::HttpDownloader;
use voice_bird_next::producer::model_watch::ModelWatcher;
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
    let ui_thread = std::thread::current().id();
    std::panic::set_hook(Box::new(move |info| {
        if voice_bird_next::producer::sources::source_query_panicking() {
            return;
        }
        if std::thread::current().id() == ui_thread {
            restore_terminal();
        }
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
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = rt.block_on(run(&mut terminal));
    // A stalled native query or transfer must never delay restoring the TUI.
    rt.shutdown_background();
    result
}

/// Resolve keys into events without mutating the UI projection.
fn handle_key(key: KeyEvent, view: &UiView, db: &mut Database, tx: &EventSender) {
    if let Some(intent) = input::map_key(key) {
        producer::input::resolve_intent(intent, view, db, tx);
    }
}

/// Keep SQLite on this task while Tokio schedules input and producer work.
async fn run(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> io::Result<()> {
    const TICK: Duration = Duration::from_millis(100);

    let mut bus = EventBus::new();
    let tx = bus.sender();
    let mut log = voice_bird_next::event_log::EventLog::open();

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
    let mut consumer = Consumer::new(Consumers::new(
        downloader,
        store.clone(),
        voice_bird_next::producer::sources::system_sources(),
    ));
    let watcher = ModelWatcher::new(store);
    let mut input = EventStream::new();
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut dirty = true;
    loop {
        if dirty {
            terminal.draw(|f| voice_bird_next::ui::render(f, &consumer.consumers.ui_view))?;
            dirty = false;
        }
        tokio::select! {
            event = input.next() => {
                match event {
                    Some(Ok(Event::Key(key))) => handle_key(key, &consumer.consumers.ui_view, &mut db, &tx),
                    Some(Ok(Event::Resize(_, _))) => dirty = true,
                    Some(Ok(_)) => {}
                    Some(Err(error)) => return Err(error),
                    None => break,
                }
            }
            event = bus.recv() => {
                let Some(event) = event else { break };
                let events = std::iter::once(event).chain(bus.drain()).collect();
                consume_logged(events, &mut log, &mut consumer, &mut db, &tx);
                dirty = true;
            }
            _ = tick.tick() => watcher.check(&consumer.consumers.ui_view, &tx),
        }
        if consumer.consumers.ui_view.should_quit {
            cleanup_inflight(&mut db, &mut bus, &mut consumer, &mut log);
            break;
        }
    }
    Ok(())
}

/// Log before the database gate; only accepted events reach the consumer.
fn consume_logged(
    events: Vec<AppEvent>,
    log: &mut Option<voice_bird_next::event_log::EventLog>,
    consumer: &mut Consumer,
    db: &mut Database,
    tx: &EventSender,
) {
    let mut accepted = Vec::with_capacity(events.len());
    for event in events {
        if let Some(log) = log.as_mut() {
            log.append(&event);
        }
        match voice_bird_next::db::apply(db, &event) {
            Ok(true) => accepted.push(event),
            Ok(false) => {}
            Err(error) => {
                eprintln!("voice-bird-next: event gate failed for {event:?}: {error}");
            }
        }
    }
    consumer.consume(&accepted, db, tx);
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
/// downloads consumer that owns the model store answers it on the next drain.
fn cleanup_inflight(
    db: &mut Database,
    bus: &mut EventBus,
    consumer: &mut Consumer,
    log: &mut Option<voice_bird_next::event_log::EventLog>,
) {
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
    // Flush cleanup and lifecycle events through the same log/database gate.
    let tx = bus.sender();
    loop {
        let events: Vec<_> = bus.drain().collect();
        if events.is_empty() {
            break;
        }
        consume_logged(events, log, consumer, db, &tx);
    }
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
