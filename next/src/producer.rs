//! Intent → bus event translation.
//!
//! [`resolve_intent`] is the single seam where a user key press becomes
//! one or more [`AppEvent`]s. The reducer does the rest. Splitting it
//! out of `main.rs` keeps the binary entry point focused on terminal
//! plumbing (raw mode, alt screen, panic hook, the render loop) and
//! puts everything that talks to the bus next to the bus itself.

use crate::bus::{AppEvent, EventSender, FocusMove};
use crate::picker::PickerMove;
use crate::db::downloads::Downloads;
use crate::download::{begin, Downloader};
use crate::input::Intent;
use crate::picker::{self, CATALOG};
use crate::state::{BlockState, UiState};
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
///   focused block's stage and access to the store / table.
/// - All other intents are direct mappings.
pub fn resolve_intent(
    intent: Intent,
    state: &UiState,
    store: &dyn ModelStore,
    downloads: &mut Downloads,
    downloader: &dyn Downloader,
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
                    begin(entry, store, downloads, downloader, tx);
                }
            }
        }
        Intent::Retry => {
            if let Some(block) = state.focused() {
                if let BlockState::Failed { model, .. } = &block.state {
                    if let Some(entry) = CATALOG.iter().find(|e| e.id == *model) {
                        begin(entry, store, downloads, downloader, tx);
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
                        if let Err(e) = downloads.cancel(model) {
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
