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

use voice_bird_next::{
    bus::{EventBus, EventSender},
    download::{CancelRegistry, Downloader},
    input,
    producer,
    state::UiState,
    store::{DownloadRepository, InMemoryDownloadRepository},
    transcription_models::{CacheDirStore, ModelStore},
};
#[cfg(feature = "net")]
use voice_bird_next::download::HttpDownloader;



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
    store: &Arc<dyn ModelStore>,
    repo: &Arc<dyn DownloadRepository>,
    downloader: &Arc<dyn Downloader>,
    cancels: &CancelRegistry,
    tx: &EventSender,
) {
    if let Some(intent) = input::map_key(key) {
        producer::resolve_intent(intent, state, store, repo, downloader, cancels, tx);
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
    let repo: Arc<dyn DownloadRepository> =
        Arc::new(InMemoryDownloadRepository::new());
    let downloader: Arc<dyn Downloader> = cfg_build_downloader();
    let cancels = CancelRegistry::new();

    let mut dirty = true;
    loop {
        if dirty {
            terminal.draw(|f| voice_bird_next::ui::render(f, &state))?;
            dirty = false;
        }
        if event::poll(TICK)? {
            if let Event::Key(k) = event::read()? {
                handle_key(k, &state, &store, &repo, &downloader, &cancels, &tx);
                dirty = true;
            }
        }
        for ev in bus.drain() {
            if let Some(l) = log.as_mut() {
                l.append(&ev);
            }
            voice_bird_next::store::apply(&*repo, &ev);
            state.apply(&ev);
            dirty = true;
        }
        if state.should_quit {
            break;
        }
    }
    Ok(())
}

#[cfg(feature = "net")]
fn cfg_build_downloader() -> Arc<dyn Downloader> {
    Arc::new(HttpDownloader)
}

#[cfg(not(feature = "net"))]
fn cfg_build_downloader() -> Arc<dyn Downloader> {
    // Without the `net` feature the binary can't download. Tests
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