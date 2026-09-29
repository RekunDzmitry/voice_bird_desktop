//! Intent → bus event translation.
//!
//! [`resolve_intent`] is the single seam where a user key press becomes
//! one or more [`AppEvent`]s. The reducer does the rest. Splitting it
//! out of `main.rs` keeps the binary entry point focused on terminal
//! plumbing (raw mode, alt screen, panic hook, the render loop) and
//! puts everything that talks to the bus next to the bus itself.
use crate::bus::{AppEvent, EventSender, FocusMove};
use crate::picker::PickerMove;

use crate::db::{downloads, Database};
use crate::input::Intent;
use crate::picker::{self, CATALOG};
use crate::state::{BlockState, UiState};
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
/// - `Confirm` and `Retry` publish [`AppEvent::BeginDownload`] for
///   the picked entry. The dispatcher that owns the collaborators is
///   the only thing that calls [`download::begin`] — this
///   resolver never sees a `Downloader` or `ModelStore`.
/// - All other intents are direct mappings.
///
/// [`AppEvent::BeginDownload`]: crate::bus::AppEvent::BeginDownload
pub fn resolve_intent(
    intent: Intent,
    state: &UiState,
    db: &mut Database,
    tx: &EventSender,
) {
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
                    tx.publish(AppEvent::BeginDownload(entry));
                }
            }
        }
        Intent::Retry => {
            if let Some(block) = state.focused() {
                if let BlockState::Failed { model, .. } = &block.state {
                    if let Some(entry) = CATALOG.iter().find(|e| e.id == *model) {
                        tx.publish(AppEvent::BeginDownload(entry));
                    }
                }
            }
        }
        Intent::BlockClosed => {
            // Closing the focused block: if it was the last waiter on
            // its model, atomically flip the table row to Cancelling
            // (logged as DownloadStatusChanged). The reducer's
            // BlockClosed arm removes `downloads[model]` when no
            // other block is `Waiting` on that model — pure reducer
            // logic, no separate publish of `DownloadCancelled` from
            // the producer.
            if let Some(block) = state.focused() {
                if let Some(model) = block.model() {
                    let any_other = state.blocks.iter().any(|b| {
                        b.id != block.id
                            && matches!(&b.state, BlockState::Waiting { model: m } if *m == model)
                    });
                    if !any_other
                        && matches!(block.state, BlockState::Waiting { .. })
                    {
                        // Best-effort: surface DB errors as a Failed
                        // event so the user sees the cause instead
                        // of a stuck Cancelling row.
                        if let Err(e) = downloads::cancel(db, model) {
                            tx.publish(AppEvent::DownloadFailed {
                                attempt: 0,
                                model,
                                error: format!("downloads table: {e}"),
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
    use super::*;
    use crate::bus::EventBus;
    use crate::db::Database;
    use crate::picker::SessionMenu;
    use crate::state::Block;


    fn tiny() -> &'static picker::ModelEntry {
        &CATALOG[5]
    }

    /// `downloads_with` in `tests/download_flow.rs`.
    struct DbHandle {
        db: Database,
        _tmp: tempfile::TempDir,
    }

    fn db_with(bus: &EventBus) -> DbHandle {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("downloads.sqlite");
        let db = Database::open(&path, bus.sender()).unwrap();
        DbHandle { db, _tmp: tmp }
    }

    /// The state with five Picking blocks (one focused). Mirrors the
    /// original `state_with_five_blocks` helper but walks the bus so
    /// the reducer's AddBlock path — including `show_block` — runs.
    fn state_with_five_blocks() -> UiState {
        let mut s = UiState::default();
        for _ in 0..5 {
            s.apply(&AppEvent::AddBlock);
        }
        s
    }

    // -----------------------------------------------------------------
    // Pre-menu intent tests (from 595e0a0)
    // -----------------------------------------------------------------

    #[test]
    fn confirm_on_picking_block_invokes_begin() {
        // Pick the tiny model and confirm — the resolver must publish
        // BeginDownload for the focused Picking block. The dispatcher
        // (not the resolver) is the one that calls `begin`; this
        // test only checks the publish.
        let mut bus = EventBus::new();
        let tx = bus.sender();
        let mut h = db_with(&bus);
        let mut state = UiState::default();
        state.apply(&AppEvent::AddBlock);
        // Pick the tiny model — AddBlock starts at picker index 0,
        // and tiny.en is at CATALOG[5], so 5 PickerNext moves land
        // there. The resolver only publishes `PickerMoved` events;
        // the reducer applies them to update the picker's index, so
        // we drain and apply after each move.
        for _ in 0..5 {
            resolve_intent(Intent::PickerNext, &state, &mut h.db, &tx);
            for ev in bus.drain() {
                state.apply(&ev);
            }
        }
        resolve_intent(Intent::Confirm, &state, &mut h.db, &tx);
        let events: Vec<_> = bus.drain().collect();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AppEvent::BeginDownload(entry) if entry.id == tiny().id)),
            "Confirm must publish BeginDownload for {}; got {events:?}",
            tiny().id
        );
    }

    #[test]
    fn quit_publishes_quit_event() {
        let mut bus = EventBus::new();
        let tx = bus.sender();
        let mut h = db_with(&bus);
        let state = UiState::default();
        resolve_intent(Intent::Quit, &state, &mut h.db, &tx);
        let events: Vec<_> = bus.drain().collect();
        assert!(
            matches!(events.last(), Some(AppEvent::Quit)),
            "Quit must publish Quit; got {events:?}"
        );
    }

    #[test]
    fn block_closed_cancels_in_flight_row() {
        // Smoke test: BlockClosed on the focused Waiting block
        // (when no other block is waiting on the same model) flips
        // the row to Cancelling without publishing a Failed event.
        let mut bus = EventBus::new();
        let tx = bus.sender();
        let mut h = db_with(&bus);
        let mut state = UiState::default();
        // Manually start a row and put the focused block in Waiting.
        crate::db::downloads::start(&mut h.db, tiny().id).unwrap();
        state.blocks.push(Block::new(
            1,
            BlockState::Waiting { model: tiny().id },
        ));
        resolve_intent(
            Intent::BlockClosed,
            &state,
            &mut h.db,
            &tx,
        );
        let events: Vec<_> = bus.drain().collect();
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AppEvent::DownloadFailed { .. })),
            "BlockClosed must not publish Failed when cancel succeeded; got {events:?}"
        );
        let row = crate::db::downloads::get(&h.db, tiny().id)
            .unwrap()
            .unwrap();
        assert_eq!(row.status, crate::bus::DownloadStatus::Cancelling);
    }

    // -----------------------------------------------------------------
    // Session-menu tests (from a231a62)
    // -----------------------------------------------------------------

    #[test]
    fn menu_open_toggle_publishes_menu_opened() {
        let mut bus = EventBus::new();
        let tx = bus.sender();
        let mut h = db_with(&bus);
        let state = UiState::default();
        resolve_intent(Intent::ToggleMenu, &state, &mut h.db, &tx);
        let events: Vec<AppEvent> = bus.drain().collect();
        assert!(
            matches!(events.as_slice(), [AppEvent::MenuOpened]),
            "ToggleMenu on a closed menu must publish MenuOpened; got {events:?}"
        );
    }

    #[test]
    fn menu_open_toggle_publishes_menu_closed_when_already_open() {
        let mut bus = EventBus::new();
        let tx = bus.sender();
        let mut h = db_with(&bus);
        let mut state = UiState::default();
        state.menu = Some(SessionMenu::open_at(0));
        resolve_intent(Intent::ToggleMenu, &state, &mut h.db, &tx);
        let events: Vec<AppEvent> = bus.drain().collect();
        assert!(
            matches!(events.as_slice(), [AppEvent::MenuClosed]),
            "ToggleMenu on an open menu must publish MenuClosed; got {events:?}"
        );
    }

    #[test]
    fn confirm_with_menu_open_publishes_session_shown_not_download() {
        let mut bus = EventBus::new();
        let tx = bus.sender();
        let mut h = db_with(&bus);
        let mut state = state_with_five_blocks();
        // Highlight the first block in the menu list — that's the
        // hidden block 1.
        state.menu = Some(SessionMenu::open_at(0));
        resolve_intent(Intent::Confirm, &state, &mut h.db, &tx);
        let events: Vec<AppEvent> = bus.drain().collect();
        assert_eq!(events.len(), 1, "expected exactly one event; got {events:?}");
        match &events[0] {
            AppEvent::SessionShown { id } => assert_eq!(*id, 1),
            other => panic!("expected SessionShown, got {other:?}"),
        }
    }

    #[test]
    fn esc_with_menu_open_publishes_menu_closed_not_block_closed() {
        let mut bus = EventBus::new();
        let tx = bus.sender();
        let mut h = db_with(&bus);
        let mut state = state_with_five_blocks();
        state.menu = Some(SessionMenu::open_at(0));
        resolve_intent(Intent::BlockClosed, &state, &mut h.db, &tx);
        let events: Vec<AppEvent> = bus.drain().collect();
        assert_eq!(events.len(), 1, "expected exactly one event; got {events:?}");
        assert!(
            matches!(events.as_slice(), [AppEvent::MenuClosed]),
            "Esc with the menu open must publish MenuClosed, not BlockClosed; got {events:?}"
        );
    }

    #[test]
    fn picker_prev_next_with_menu_open_publish_menu_moved() {
        let mut bus = EventBus::new();
        let tx = bus.sender();
        let mut h = db_with(&bus);
        let mut state = state_with_five_blocks();
        state.menu = Some(SessionMenu::open_at(0));
        resolve_intent(Intent::PickerPrev, &state, &mut h.db, &tx);
        resolve_intent(Intent::PickerNext, &state, &mut h.db, &tx);
        let events: Vec<AppEvent> = bus.drain().collect();
        assert_eq!(events.len(), 2, "expected two events; got {events:?}");
        assert!(
            matches!(
                &events[0],
                AppEvent::MenuMoved { direction: PickerMove::Up }
            ),
            "first event must be MenuMoved Up; got {:?}",
            events[0]
        );
        assert!(
            matches!(
                &events[1],
                AppEvent::MenuMoved { direction: PickerMove::Down }
            ),
            "second event must be MenuMoved Down; got {:?}",
            events[1]
        );
    }

    #[test]
    fn add_block_while_menu_open_still_publishes_add_block() {
        // The menu only intercepts ToggleMenu, picker moves, Confirm,
        // and Esc. AddBlock falls through unchanged so the user can
        // still press `+` with the menu open.
        let mut bus = EventBus::new();
        let tx = bus.sender();
        let mut h = db_with(&bus);
        let mut state = state_with_five_blocks();
        state.menu = Some(SessionMenu::open_at(0));
        resolve_intent(Intent::AddBlock, &state, &mut h.db, &tx);
        let events: Vec<AppEvent> = bus.drain().collect();
        assert!(
            matches!(events.as_slice(), [AppEvent::AddBlock]),
            "AddBlock with the menu open must still publish AddBlock; got {events:?}"
        );
    }

    #[test]
    fn quit_with_menu_open_still_publishes_quit() {
        // Quit is not intercepted — the user must be able to leave
        // the app even with the menu open.
        let mut bus = EventBus::new();
        let tx = bus.sender();
        let mut h = db_with(&bus);
        let mut state = state_with_five_blocks();
        state.menu = Some(SessionMenu::open_at(0));
        resolve_intent(Intent::Quit, &state, &mut h.db, &tx);
        let events: Vec<AppEvent> = bus.drain().collect();
        assert!(
            matches!(events.as_slice(), [AppEvent::Quit]),
            "Quit with the menu open must still publish Quit; got {events:?}"
        );
    }

    #[test]
    fn menu_open_with_no_blocks_publishes_toggle_only() {
        // Even without blocks, opening the menu must publish exactly
        // one event so the loop drains cleanly.
        let mut bus = EventBus::new();
        let tx = bus.sender();
        let mut h = db_with(&bus);
        let state = UiState::default();
        resolve_intent(Intent::ToggleMenu, &state, &mut h.db, &tx);
        let events: Vec<AppEvent> = bus.drain().collect();
        assert!(
            matches!(events.as_slice(), [AppEvent::MenuOpened]),
            "ToggleMenu on a fresh state must publish MenuOpened; got {events:?}"
        );
    }

    #[test]
    fn session_shown_picks_up_menu_index_for_block_id() {
        // Sanity: SessionShown reads the id from the menu's selected
        // row, not from `state.focus`. Highlight the last row in a
        // 3-block state — id=3, even though focus=2.
        let mut bus = EventBus::new();
        let tx = bus.sender();
        let mut h = db_with(&bus);
        let mut state = UiState::default();
        for _ in 0..3 {
            state.apply(&AppEvent::AddBlock);
        }
        state.menu = Some(SessionMenu::open_at(2));
        resolve_intent(Intent::Confirm, &state, &mut h.db, &tx);
        let events: Vec<AppEvent> = bus.drain().collect();
        assert_eq!(events.len(), 1, "expected exactly one event; got {events:?}");
        match &events[0] {
            AppEvent::SessionShown { id } => assert_eq!(*id, 3),
            other => panic!("expected SessionShown for id=3, got {other:?}"),
        }
    }
}
