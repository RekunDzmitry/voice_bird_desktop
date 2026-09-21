//! Intent → bus event translation.
//!
//! [`resolve_intent`] is the single seam where a user key press becomes
//! one or more [`AppEvent`]s. The reducer does the rest. Splitting it
//! out of `main.rs` keeps the binary entry point focused on terminal
//! plumbing (raw mode, alt screen, panic hook, the render loop) and
//! puts everything that talks to the bus next to the bus itself.

use std::sync::Arc;

use crate::bus::{AppEvent, EventSender, FocusMove};
use crate::picker::PickerMove;
use crate::download::{begin, Downloader};
use crate::input::Intent;
use crate::picker::{self, CATALOG};
use crate::state::{BlockState, UiState};
use crate::store::DownloadRepository;
use crate::transcription_models::ModelStore;

/// Stamp the `from_model`/`to_model` fields onto `PickerMoved` using
/// the focused block's current `picker_index` plus a non-mutating peek
/// at the target index. The input layer has no catalog context, so the
/// resolver carries it. If the focused block isn't `Picking` (e.g. the
/// user pressed Up/Down while recording), the event logs both fields
/// as `None` and the reducer still runs the move on the picker if one
/// is open.
pub fn stamp_picker_move(tx: &EventSender, state: &UiState, direction: picker::PickerMove) {
    let (from_model, to_model) = match state.focused() {
        Some(block) => match &block.state {
            BlockState::Picking(picker) => {
                let from = CATALOG[picker.index].id;
                let to_idx = picker.peek_next(direction);
                let to = CATALOG[to_idx].id;
                (Some(from), Some(to))
            }
            _ => (None, None),
        },
        None => (None, None),
    };
    tx.publish(AppEvent::PickerMoved {
        direction,
        from_model,
        to_model,
    });
}

/// Resolve one [`Intent`] into bus events. The reducer does the rest.
///
/// - `Confirm` and `Retry` are the resolver's job: they need the
///   focused block's stage and access to the store / repo.
/// - All other intents are direct mappings.
pub fn resolve_intent(
    intent: Intent,
    state: &UiState,
    store: &Arc<dyn ModelStore>,
    repo: &Arc<dyn DownloadRepository>,
    downloader: &Arc<dyn Downloader>,
    tx: &EventSender,
) {
    // The session menu is a small modal: while it's open, certain
    // intents are hijacked to drive the menu instead of falling
    // through to their default reducer. Branch on `state.menu.is_some()`
    // FIRST so the menu's behaviour is local and obvious; everything
    // that doesn't intercept (`AddBlock`, `Quit`, `Retry`, focus
    // moves) keeps its old semantics even with the menu open.
    let menu_open = state.menu.is_some();
    if menu_open {
        match intent {
            Intent::ToggleMenu => {
                tx.publish(AppEvent::MenuClosed);
                return;
            }
            Intent::PickerPrev => {
                tx.publish(AppEvent::MenuMoved {
                    direction: PickerMove::Up,
                });
                return;
            }
            Intent::PickerNext => {
                tx.publish(AppEvent::MenuMoved {
                    direction: PickerMove::Down,
                });
                return;
            }
            Intent::Confirm => {
                // Publish a SessionShown for the menu's selected row.
                // The reducer closes the menu, runs `show_block` to
                // evict the oldest-focused visible peer if needed, and
                // moves focus to the chosen id. `state.menu` is the
                // session menu's struct, not the picker — `index` is
                // a position in `state.blocks`.
                if let Some(menu) = state.menu.as_ref() {
                    if let Some(block) = state.blocks.get(menu.index) {
                        tx.publish(AppEvent::SessionShown { id: block.id });
                    }
                }
                return;
            }
            Intent::BlockClosed => {
                // Esc closes the menu instead of the focused block.
                tx.publish(AppEvent::MenuClosed);
                return;
            }
            _ => {} // fall through to default handling
        }
    } else if matches!(intent, Intent::ToggleMenu) {
        // Tab toggles open (the close branch above already returned).
        tx.publish(AppEvent::MenuOpened);
        return;
    }

    match intent {
        Intent::AddBlock => tx.publish(AppEvent::AddBlock),
        Intent::FocusPrev => tx.publish(AppEvent::FocusMoved {
            direction: FocusMove::Prev,
        }),
        Intent::FocusNext => tx.publish(AppEvent::FocusMoved {
            direction: FocusMove::Next,
        }),
        Intent::PickerPrev => stamp_picker_move(tx, state, picker::PickerMove::Up),
        Intent::PickerNext => stamp_picker_move(tx, state, picker::PickerMove::Down),
        Intent::Confirm => {
            if let Some(block) = state.focused() {
                if let BlockState::Picking(picker) = &block.state {
                    let entry: &'static picker::ModelEntry = &CATALOG[picker.index];
                    begin(entry, store, repo, downloader, tx);
                }
            }
        }
        Intent::Retry => {
            if let Some(block) = state.focused() {
                if let BlockState::Failed { model, .. } = &block.state {
                    if let Some(entry) = CATALOG.iter().find(|e| e.id == *model) {
                        begin(entry, store, repo, downloader, tx);
                    }
                }
            }
        }
        Intent::BlockClosed => {
            // Closing the focused block: if it was the last waiter on
            // its model, atomically cancel the in-flight download
            // (set the token, drop both tables) and publish the
            // DownloadCancelled event so the reducer tears down
            // UiState.downloads and the store drops the row.
            if let Some(block) = state.focused() {
                if let Some(model) = block.model() {
                    let any_other = state.blocks.iter().any(|b| {
                        b.id != block.id
                            && matches!(&b.state, BlockState::Waiting { model: m } if *m == model)
                    });
                    if !any_other
                        && matches!(block.state, BlockState::Waiting { .. })
                        && repo.cancel(model)
                    {
                        // `repo.cancel` kept the row (Cancelling
                        // state). Read back the attempt id so the
                        // event we publish matches the row the
                        // store is tracking — the store's attempt
                        // gate will discard this event if a
                        // subsequent Restart supersedes the
                        // attempt.
                        if let Some(row) = repo.get(model) {
                            tx.publish(AppEvent::DownloadCancelled {
                                attempt: row.attempt,
                                model,
                            });
                        }
                    }
                }
            }
            tx.publish(AppEvent::BlockClosed);
        }
        Intent::Quit => tx.publish(AppEvent::Quit),
        Intent::ToggleMenu => unreachable!("handled above"),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::type_complexity)]
    #![allow(clippy::field_reassign_with_default)]

    use super::*;
    use crate::bus::EventBus;
    use crate::picker::SessionMenu;
    
    use crate::testing::{FixtureDownloader, FixtureStore, Outcome};
    use crate::transcription_models::ModelStore;
    use std::sync::Arc;

    fn fixture() -> (EventBus, Arc<dyn ModelStore>, Arc<dyn DownloadRepository>, Arc<dyn Downloader>) {
        let bus = EventBus::new();
        let store: Arc<dyn ModelStore> = Arc::new(FixtureStore::new(std::path::PathBuf::from("/tmp"), &[]));
        let repo: Arc<dyn DownloadRepository> = Arc::new(crate::store::InMemoryDownloadRepository::new());
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let downloader: Arc<dyn Downloader> = Arc::new(FixtureDownloader::new(
            Vec::new(),
            Outcome::Ok,
            calls,
        ));
        (bus, store, repo, downloader)
    }

    fn state_with_five_blocks() -> UiState {
        let mut s = UiState::default();
        for _ in 0..5 {
            s.apply(&AppEvent::AddBlock);
        }
        s
    }

    #[test]
    fn menu_open_toggle_publishes_menu_opened() {
        let (mut bus, store, repo, dl) = fixture();
        let tx = bus.sender();
        let state = UiState::default();
        resolve_intent(Intent::ToggleMenu, &state, &store, &repo, &dl, &tx);
        let events: Vec<AppEvent> = bus.drain().collect();
        assert!(matches!(events.as_slice(), [AppEvent::MenuOpened]));
    }

    #[test]
    fn menu_open_toggle_publishes_menu_closed_when_already_open() {
        let (mut bus, store, repo, dl) = fixture();
        let tx = bus.sender();
        let mut state = UiState::default();
        state.menu = Some(SessionMenu::open_at(0));
        resolve_intent(Intent::ToggleMenu, &state, &store, &repo, &dl, &tx);
        let events: Vec<AppEvent> = bus.drain().collect();
        assert!(matches!(events.as_slice(), [AppEvent::MenuClosed]));
    }

    #[test]
    fn confirm_with_menu_open_publishes_session_shown_not_download() {
        let (mut bus, store, repo, dl) = fixture();
        let tx = bus.sender();
        let mut state = state_with_five_blocks();
        // Highlight the first block in the menu list — that's the
        // hidden block 1.
        state.menu = Some(SessionMenu::open_at(0));
        resolve_intent(Intent::Confirm, &state, &store, &repo, &dl, &tx);
        let events: Vec<AppEvent> = bus.drain().collect();
        assert_eq!(events.len(), 1, "expected exactly one event; got {events:?}");
        match &events[0] {
            AppEvent::SessionShown { id } => assert_eq!(*id, 1),
            other => panic!("expected SessionShown, got {other:?}"),
        }
    }

    #[test]
    fn esc_with_menu_open_publishes_menu_closed_not_block_closed() {
        let (mut bus, store, repo, dl) = fixture();
        let tx = bus.sender();
        let mut state = state_with_five_blocks();
        state.menu = Some(SessionMenu::open_at(0));
        resolve_intent(Intent::BlockClosed, &state, &store, &repo, &dl, &tx);
        let events: Vec<AppEvent> = bus.drain().collect();
        assert_eq!(events.len(), 1, "expected exactly one event; got {events:?}");
        assert!(matches!(events.as_slice(), [AppEvent::MenuClosed]));
    }

    #[test]
    fn picker_prev_next_with_menu_open_publish_menu_moved() {
        let (mut bus, store, repo, dl) = fixture();
        let tx = bus.sender();
        let mut state = state_with_five_blocks();
        state.menu = Some(SessionMenu::open_at(0));
        resolve_intent(Intent::PickerPrev, &state, &store, &repo, &dl, &tx);
        resolve_intent(Intent::PickerNext, &state, &store, &repo, &dl, &tx);
        let events: Vec<AppEvent> = bus.drain().collect();
        assert_eq!(events.len(), 2);
        assert!(matches!(
            &events[0],
            AppEvent::MenuMoved { direction: PickerMove::Up }
        ));
        assert!(matches!(
            &events[1],
            AppEvent::MenuMoved { direction: PickerMove::Down }
        ));
    }

    #[test]
    fn add_block_while_menu_open_still_publishes_add_block() {
        // The menu only intercepts ToggleMenu, picker moves, Confirm,
        // and Esc. AddBlock falls through unchanged so the user can
        // still press `+` with the menu open.
        let (mut bus, store, repo, dl) = fixture();
        let tx = bus.sender();
        let mut state = state_with_five_blocks();
        state.menu = Some(SessionMenu::open_at(0));
        resolve_intent(Intent::AddBlock, &state, &store, &repo, &dl, &tx);
        let events: Vec<AppEvent> = bus.drain().collect();
        assert!(matches!(events.as_slice(), [AppEvent::AddBlock]));
    }

    #[test]
    fn quit_with_menu_open_still_publishes_quit() {
        // Quit is not intercepted — the user must be able to leave
        // the app even with the menu open.
        let (mut bus, store, repo, dl) = fixture();
        let tx = bus.sender();
        let mut state = state_with_five_blocks();
        state.menu = Some(SessionMenu::open_at(0));
        resolve_intent(Intent::Quit, &state, &store, &repo, &dl, &tx);
        let events: Vec<AppEvent> = bus.drain().collect();
        assert!(matches!(events.as_slice(), [AppEvent::Quit]));
    }

    #[test]
    fn menu_open_with_no_blocks_publishes_toggle_only() {
        let (mut bus, store, repo, dl) = fixture();
        let tx = bus.sender();
        let state = UiState::default();
        // Even without blocks, opening/closing the menu must publish
        // exactly one event so the loop drains cleanly.
        resolve_intent(Intent::ToggleMenu, &state, &store, &repo, &dl, &tx);
        let events: Vec<AppEvent> = bus.drain().collect();
        assert!(matches!(events.as_slice(), [AppEvent::MenuOpened]));
    }

    #[test]
    fn session_shown_picks_up_menu_index_for_block_id() {
        // Sanity: SessionShown reads the id from the menu's selected
        // row, not from `state.focus`. Highlight the last row in a
        // 3-block state — id=3, even though focus=2.
        let (mut bus, store, repo, dl) = fixture();
        let tx = bus.sender();
        let mut state = UiState::default();
        for _ in 0..3 {
            state.apply(&AppEvent::AddBlock);
        }
        state.menu = Some(SessionMenu::open_at(2));
        resolve_intent(Intent::Confirm, &state, &store, &repo, &dl, &tx);
        let events: Vec<AppEvent> = bus.drain().collect();
        assert_eq!(events.len(), 1);
        match &events[0] {
            AppEvent::SessionShown { id } => assert_eq!(*id, 3),
            other => panic!("expected SessionShown for id=3, got {other:?}"),
        }
    }

}

