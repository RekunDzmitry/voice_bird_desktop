//! Intent → bus event translation.
//!
//! [`resolve_intent`] is the single seam where a user key press becomes
//! one or more [`AppEvent`]s. The reducer does the rest. Splitting it
//! out of `main.rs` keeps the binary entry point focused on terminal
//! plumbing (raw mode, alt screen, panic hook, the render loop) and
//! puts everything that talks to the bus next to the bus itself.

use std::sync::Arc;

use crate::bus::{AppEvent, EventSender, FocusMove};
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
                        tx.publish(AppEvent::DownloadCancelled { model });
                    }
                }
            }
            tx.publish(AppEvent::BlockClosed);
        }
        Intent::Quit => tx.publish(AppEvent::Quit),
    }
}
