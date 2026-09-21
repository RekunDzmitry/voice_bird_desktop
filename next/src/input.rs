//! Pure key-to-intent map.
//!
//! The picker/global split collapses into one `map_key` because there
//! is no modal any more — but `Enter`, arrows and `r` are only
//! *meaningful* for certain block states, and that's the resolver's
//! judgement in `main.rs`, not the key layer's.
//!
//! [`Intent`] is the internal vocabulary the resolver consumes;
//! [`map_key`] is the only entry point.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// Internal intent emitted by [`map_key`]. The main loop turns
/// [`Intent::Confirm`] and [`Intent::Retry`] into real bus events
/// after consulting the focused block's stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    AddBlock,
    FocusPrev,
    FocusNext,
    PickerPrev,
    PickerNext,
    Confirm,
    Retry,
    BlockClosed,
    /// `Tab`: open the session menu (or close it if already open).
    /// The resolver decides which side of the toggle to land on
    /// based on `state.menu`.
    ToggleMenu,
    Quit,
}

/// Map one key event to an [`Intent`]. Only `Press` events count (the
/// filter prevents a held key from firing twice on Windows and on
/// terminals with the kitty keyboard protocol enabled). Returns
/// `None` for keys the app does not act on.
///
/// Matches `'+'` by code only — many layouts report `'+'` with SHIFT
/// set, and stripping the modifier at this layer would drop those.
pub fn map_key(key: KeyEvent) -> Option<Intent> {
    if key.kind != KeyEventKind::Press {
        return None;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    match key.code {
        KeyCode::Char('q') => Some(Intent::Quit),
        KeyCode::Char('c') if ctrl => Some(Intent::Quit),
        KeyCode::Char('+') => Some(Intent::AddBlock),
        KeyCode::Char('=') if shift => Some(Intent::AddBlock),
        KeyCode::Esc => Some(Intent::BlockClosed),
        KeyCode::Left => Some(Intent::FocusPrev),
        KeyCode::Right => Some(Intent::FocusNext),
        KeyCode::Up => Some(Intent::PickerPrev),
        KeyCode::Down => Some(Intent::PickerNext),
        KeyCode::Enter => Some(Intent::Confirm),
        KeyCode::Char('r') | KeyCode::Char('R') => Some(Intent::Retry),
        KeyCode::Tab => Some(Intent::ToggleMenu),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn q_esc_and_ctrl_c_map_to_quit_or_block_closed() {
        assert_eq!(map_key(press(KeyCode::Char('q'))), Some(Intent::Quit));
        assert_eq!(map_key(press(KeyCode::Esc)), Some(Intent::BlockClosed));
        assert_eq!(
            map_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(Intent::Quit)
        );
    }

    #[test]
    fn plus_maps_to_add_block_with_no_modifier() {
        assert_eq!(
            map_key(press(KeyCode::Char('+'))),
            Some(Intent::AddBlock)
        );
    }

    #[test]
    fn plus_maps_to_add_block_with_shift() {
        assert_eq!(
            map_key(KeyEvent::new(KeyCode::Char('+'), KeyModifiers::SHIFT)),
            Some(Intent::AddBlock)
        );
    }

    #[test]
    fn arrows_route_to_focus_or_picker() {
        assert_eq!(map_key(press(KeyCode::Left)), Some(Intent::FocusPrev));
        assert_eq!(map_key(press(KeyCode::Right)), Some(Intent::FocusNext));
        assert_eq!(map_key(press(KeyCode::Up)), Some(Intent::PickerPrev));
        assert_eq!(map_key(press(KeyCode::Down)), Some(Intent::PickerNext));
    }

    #[test]
    fn enter_and_r_route_to_confirm_and_retry() {
        assert_eq!(map_key(press(KeyCode::Enter)), Some(Intent::Confirm));
        assert_eq!(map_key(press(KeyCode::Char('r'))), Some(Intent::Retry));
        assert_eq!(map_key(press(KeyCode::Char('R'))), Some(Intent::Retry));
    }

    #[test]
    fn other_keys_are_ignored() {
        assert_eq!(map_key(press(KeyCode::Char('x'))), None);
        assert_eq!(map_key(press(KeyCode::Char('c'))), None); // plain `c`, no ctrl
    }

    #[test]
    fn tab_maps_to_toggle_menu() {
        assert_eq!(map_key(press(KeyCode::Tab)), Some(Intent::ToggleMenu));
    }

    #[test]
    fn arrows_unchanged_with_tab_added() {
        // Sanity: Tab only adds a binding; nothing else moves.
        assert_eq!(map_key(press(KeyCode::Left)), Some(Intent::FocusPrev));
        assert_eq!(map_key(press(KeyCode::Right)), Some(Intent::FocusNext));
        assert_eq!(map_key(press(KeyCode::Up)), Some(Intent::PickerPrev));
        assert_eq!(map_key(press(KeyCode::Down)), Some(Intent::PickerNext));
        assert_eq!(map_key(press(KeyCode::Enter)), Some(Intent::Confirm));
        assert_eq!(map_key(press(KeyCode::Esc)), Some(Intent::BlockClosed));
    }

    #[test]
    fn release_events_are_ignored() {
        let release = KeyEvent {
            kind: KeyEventKind::Release,
            ..press(KeyCode::Char('q'))
        };
        assert_eq!(map_key(release), None);
    }
}