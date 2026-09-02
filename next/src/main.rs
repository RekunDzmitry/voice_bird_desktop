//! Binary entry point: the only file that touches a real terminal.

use std::io::{self, Stdout};

use crossterm::{
    cursor,
    event::{self, Event},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};

use voice_bird_next::{input, state::UiState, ui};

/// Runs `restore` on drop. Constructed as soon as the first irreversible
/// terminal step (raw mode) has succeeded, so every later failure — the
/// alternate-screen write, `?` early returns, unwinds via the panic hook —
/// rolls the terminal back.
struct RestoreGuard<F: FnMut()> {
    restore: F,
}

impl<F: FnMut()> Drop for RestoreGuard<F> {
    fn drop(&mut self) {
        (self.restore)();
    }
}

/// Enable raw mode, then enter the alternate screen. The guard is created
/// between the two steps: if `enter_alt` fails, `?` drops it and `restore`
/// undoes raw mode. If `enable_raw` itself fails there is nothing to undo
/// and `restore` never runs. Takes the steps as closures so tests can drive
/// the partial-initialization paths without a real terminal.
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
    // Harmless if the alternate screen was never entered (terminals treat
    // a redundant leave as a no-op).
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

/// Draw, block until the next terminal event, apply it, repeat. Nothing in
/// the state changes on its own yet, so there is no tick: `event::read`
/// wakes on key presses and on resizes (which just trigger a redraw).
fn run(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> io::Result<()> {
    let mut state = UiState::default();
    loop {
        terminal.draw(|f| ui::render(f, &state))?;
        if let Event::Key(key) = event::read()? {
            input::handle_key(&mut state, key);
        }
        if state.should_quit {
            break;
        }
    }
    Ok(())
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
