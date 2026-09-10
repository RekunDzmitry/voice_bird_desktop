use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::bus::AppEvent;
use crate::picker::PickerMove;

/// Internal marker emitted by [`picker_keys`] when the user presses Enter
/// on a highlighted row. The main loop translates this to
/// [`AppEvent::ModelSelected`] using the live `ModelPicker::index`, so the
/// bus only ever sees resolved entries.
///
/// Not a variant of [`AppEvent`] on purpose: nothing else in the system
/// should ever publish it; the marker exists only to bridge the key
/// layer to the resolver in `main.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKey {
    /// The user confirmed the highlighted row. Resolver turns this into
    /// `AppEvent::ModelSelected`.
    PickerEnter,
    /// The user moved the highlight. Resolver publishes
    /// `AppEvent::PickerMoved`.
    PickerMoved(PickerMove),
    /// The user dismissed the picker. Resolver publishes
    /// `AppEvent::PickerCancelled`.
    PickerCancelled,
}

/// Map one key event to an [`AppEvent`]. Only `Press` events count (same
/// filter as the old `run_app`, which otherwise double-fires on Windows).
/// Returns `None` for keys the app does not act on.
///
/// Matches `'+'` by code only — many layouts report `'+'` with SHIFT set,
/// and stripping the modifier at this layer would drop those.
///
/// `map_key` is the closed-picker translator. The main loop calls
/// [`picker_keys`] **first** when the picker is open; this function then
/// only sees `q`, plain `Esc`, and Ctrl-C as quit signals.
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

/// Map one key event while the picker is open. Returns `Some(PickerKey)`
/// only for keys that act on the picker; every other key yields `None`
/// so the main loop can fall back to [`map_key`] (e.g. for `q`/Ctrl-C).
pub fn picker_keys(key: KeyEvent) -> Option<PickerKey> {
    if key.kind != KeyEventKind::Press {
        return None;
    }
    match key.code {
        KeyCode::Up => Some(PickerKey::PickerMoved(PickerMove::Up)),
        KeyCode::Down => Some(PickerKey::PickerMoved(PickerMove::Down)),
        KeyCode::Enter => Some(PickerKey::PickerEnter),
        KeyCode::Esc => Some(PickerKey::PickerCancelled),
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

    // ---------- picker_keys ----------

    #[test]
    fn picker_keys_arrow_keys_emit_typed_moves() {
        assert_eq!(
            picker_keys(press(KeyCode::Up)),
            Some(PickerKey::PickerMoved(PickerMove::Up))
        );
        assert_eq!(
            picker_keys(press(KeyCode::Down)),
            Some(PickerKey::PickerMoved(PickerMove::Down))
        );
    }

    #[test]
    fn picker_keys_enter_emits_internal_marker() {
        // PickerEnter is private — the resolver in main.rs converts it to
        // AppEvent::ModelSelected. We assert the marker here, not a bus
        // variant, so the bus stays free of `KeyCode` look-alikes.
        assert_eq!(picker_keys(press(KeyCode::Enter)), Some(PickerKey::PickerEnter));
    }

    #[test]
    fn picker_keys_esc_emits_cancelled() {
        assert_eq!(
            picker_keys(press(KeyCode::Esc)),
            Some(PickerKey::PickerCancelled)
        );
    }

    #[test]
    fn picker_keys_q_is_not_consumed() {
        // The picker overlay shouldn't capture quit; the loop falls back
        // to `map_key` when `picker_keys` returns None.
        assert_eq!(picker_keys(press(KeyCode::Char('q'))), None);
    }

    #[test]
    fn picker_keys_unrelated_chars_are_ignored() {
        assert_eq!(picker_keys(press(KeyCode::Char('x'))), None);
        assert_eq!(picker_keys(press(KeyCode::Char('+'))), None);
    }

    #[test]
    fn picker_keys_release_events_are_ignored() {
        let release = KeyEvent {
            kind: KeyEventKind::Release,
            ..press(KeyCode::Up)
        };
        assert_eq!(picker_keys(release), None);
    }
}