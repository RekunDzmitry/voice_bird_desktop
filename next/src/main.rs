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

/// Owns raw mode + the alternate screen. `Drop` restores the terminal on
/// every exit path (including `?` early returns); the panic hook covers
/// unwinds, which the old `main.rs` did not.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
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
    let _guard = TerminalGuard::enter()?;
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
