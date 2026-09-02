use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::bus::AppEvent;

/// Map one key event to an [`AppEvent`]. Only `Press` events count (same
/// filter as the old `run_app`, which otherwise double-fires on Windows).
/// Returns `None` for keys the app does not act on.
///
/// Matches `'+'` by code only — many layouts report `'+'` with SHIFT set,
/// and stripping the modifier at this layer would drop those.
pub fn map_key(key: KeyEvent) -> Option<AppEvent> {
    if key.kind != KeyEventKind::Press {
        return None;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => Some(AppEvent::Quit),
        KeyCode::Char('c') if ctrl => Some(AppEvent::Quit),
        KeyCode::Char('+') => Some(AppEvent::AddBlock),
        _ => None,
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

    #[test]
    fn q_esc_and_ctrl_c_map_to_quit() {
        assert_eq!(map_key(press(KeyCode::Char('q'))), Some(AppEvent::Quit));
        assert_eq!(map_key(press(KeyCode::Esc)), Some(AppEvent::Quit));
        assert_eq!(
            map_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(AppEvent::Quit)
        );
    }

    #[test]
    fn plus_maps_to_add_block_with_no_modifier() {
        assert_eq!(
            map_key(press(KeyCode::Char('+'))),
            Some(AppEvent::AddBlock)
        );
    }

    #[test]
    fn plus_maps_to_add_block_with_shift() {
        // Many layouts report `+` with SHIFT set; we still want AddBlock.
        assert_eq!(
            map_key(KeyEvent::new(KeyCode::Char('+'), KeyModifiers::SHIFT)),
            Some(AppEvent::AddBlock)
        );
    }

    #[test]
    fn other_keys_are_ignored() {
        assert_eq!(map_key(press(KeyCode::Char('x'))), None);
        assert_eq!(map_key(press(KeyCode::Char('c'))), None); // plain `c`, no ctrl
        assert_eq!(map_key(press(KeyCode::Enter)), None);
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
        assert_eq!(map_key(release), None);
    }
}
