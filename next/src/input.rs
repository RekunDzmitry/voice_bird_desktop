use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::state::UiState;

/// Apply one key event to the state. Only `Press` events count (same
/// filter as the old `run_app`, which otherwise double-fires on Windows).
pub fn handle_key(state: &mut UiState, key: KeyEvent) {
    if key.kind != KeyEventKind::Press {
        return;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => state.should_quit = true,
        KeyCode::Char('c') if ctrl => state.should_quit = true,
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plain key press: `KeyEvent::new` requires a modifier set, and
    /// `NONE` means no Ctrl/Shift/Alt held. Ctrl-C is built explicitly below.
    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn quits(key: KeyEvent) -> bool {
        let mut s = UiState::default();
        handle_key(&mut s, key);
        s.should_quit
    }

    #[test]
    fn q_esc_and_ctrl_c_quit() {
        assert!(quits(press(KeyCode::Char('q'))));
        assert!(quits(press(KeyCode::Esc)));
        assert!(quits(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));
    }

    #[test]
    fn other_keys_are_ignored() {
        assert!(!quits(press(KeyCode::Char('x'))));
        assert!(!quits(press(KeyCode::Char('c')))); // plain `c`, no ctrl
        assert!(!quits(press(KeyCode::Enter)));
    }

    /// crossterm reports both `Press` and `Release` on Windows and on
    /// terminals with the kitty keyboard protocol enabled. Without the
    /// `kind != Press` filter a single `q` would fire twice, and releasing a
    /// key held while the app started would quit it immediately.
    #[test]
    fn release_events_are_ignored() {
        let release = KeyEvent {
            kind: KeyEventKind::Release,
            ..press(KeyCode::Char('q'))
        };
        assert!(!quits(release));
    }
}
