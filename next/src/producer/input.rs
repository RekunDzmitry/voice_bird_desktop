//! Intent → bus event translation.
//!
//! [`resolve_intent`] is the single seam where a user key press becomes
//! one or more [`AppEvent`]s. The reducer does the rest. Splitting it
//! out of `main.rs` keeps the binary entry point focused on terminal
//! plumbing (raw mode, alt screen, panic hook, the render loop) and
//! puts everything that talks to the bus next to the bus itself.
use crate::producer::sources::{DeviceKind, FunnelStep};
use crate::bus::{AppEvent, EventSender, FocusMove};
use crate::picker::PickerMove;

use crate::db::{downloads, Database};
use crate::input::Intent;
use crate::language::LANGUAGES;
use crate::picker;
use crate::consumer::ui_view::{BlockState, UiView};
/// Stamp the `from_language`/`to_language` codes onto `PickerMoved`.
/// The input layer has no registry context, so the resolver reads the
/// focused block's picker without mutating it.
pub fn stamp_picker_move(tx: &EventSender, state: &UiView, direction: picker::PickerMove) {
    let (from_language, to_language) = match state.focused() {
        Some(block) => match &block.state {
            BlockState::PickingLanguage(picker) => {
                let from = LANGUAGES[picker.index].code;
                let to = LANGUAGES[picker::step(picker.index, LANGUAGES.len(), direction)].code;
                (Some(from), Some(to))
            }
            _ => (None, None),
        },
        None => (None, None),
    };
    tx.publish(AppEvent::PickerMoved {
        direction,
        from_language,
        to_language,
    });
}
/// Resolve one [`Intent`] into bus events. The reducer does the rest.
///
/// - `Confirm` and `Retry` publish [`AppEvent::BeginLanguage`] with the
///   target block id. The consumer routes it to the language consumer.
/// - All other intents are direct mappings.
///
/// [`AppEvent::BeginLanguage`]: crate::bus::AppEvent::BeginLanguage
pub fn resolve_intent(intent: Intent, state: &UiView, db: &mut Database, tx: &EventSender) {
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
            Intent::StepBack => return,
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
        Intent::AddBlock => tx.publish(AppEvent::RequestBlock),
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
                match &block.state {
                    BlockState::PickingDevice(cursor) => {
                        if let Some(source) = &block.source {
                            if let Some(device) = source.snapshot.devices.get(cursor.index) {
                                let to = match device.kind {
                                    DeviceKind::Input => FunnelStep::Language,
                                    DeviceKind::Output => FunnelStep::App,
                                };
                                tx.publish(AppEvent::SourceStepChanged {
                                    block: block.id,
                                    from: FunnelStep::Device,
                                    to,
                                    rev: source.rev,
                                    device: Some(device.clone()),
                                    app: if source.device.as_ref() == Some(device)
                                        && device.kind == DeviceKind::Output
                                    {
                                        source.app.clone()
                                    } else {
                                        None
                                    },
                                });
                            }
                        }
                    }
                    BlockState::PickingApp(cursor) => {
                        if let Some(source) = &block.source {
                            if let Some(app) = source.snapshot.apps.get(cursor.index) {
                                tx.publish(AppEvent::SourceStepChanged {
                                    block: block.id,
                                    from: FunnelStep::App,
                                    to: FunnelStep::Language,
                                    rev: source.rev,
                                    device: source.device.clone(),
                                    app: Some(app.clone()),
                                });
                            }
                        }
                    }
                    BlockState::PickingLanguage(picker) => tx.publish(AppEvent::BeginLanguage {
                        block: block.id,
                        language: &LANGUAGES[picker.index],
                        source_rev: block.source.as_ref().map(|source| source.rev),
                    }),
                    _ => {}
                }
            }
        }
        Intent::StepBack => {
            if let Some(block) = state.focused() {
                if let Some(source) = &block.source {
                    let edge = match block.state {
                        BlockState::PickingApp(_) => Some((FunnelStep::App, FunnelStep::Device)),
                        BlockState::PickingLanguage(_) => Some((
                            FunnelStep::Language,
                            if source.app.is_some() {
                                FunnelStep::App
                            } else {
                                FunnelStep::Device
                            },
                        )),
                        _ => None,
                    };
                    if let Some((from, to)) = edge {
                        tx.publish(AppEvent::SourceStepChanged {
                            block: block.id,
                            from,
                            to,
                            rev: source.rev,
                            device: source.device.clone(),
                            app: source.app.clone(),
                        });
                    }
                }
            }
        }
        Intent::Retry => {
            if let Some(block) = state.focused() {
                if let BlockState::Failed { language, .. } = &block.state {
                    tx.publish(AppEvent::BeginLanguage {
                        block: block.id,
                        language,
                        source_rev: None,
                    });
                }
            }
        }
        Intent::BlockClosed => {
            if let Some(block) = state.focused() {
                for &model in block.pending_models() {
                    let any_other = state.blocks.iter().any(|other| {
                        other.id != block.id && other.pending_models().contains(&model)
                    });
                    if !any_other {
                        if let Err(error) = downloads::cancel(db, model) {
                            tx.publish(AppEvent::DownloadFailed {
                                attempt: 0,
                                model,
                                error: format!("downloads table: {error}"),
                            });
                        }
                    }
                }
                tx.publish(AppEvent::BlockClosed { block: block.id });
            }
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
    use crate::language::{LanguageProfile, LANGUAGES};
    use crate::picker::SessionMenu;
    use crate::consumer::ui_view::Block;

    fn english() -> &'static LanguageProfile {
        &LANGUAGES[0]
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

    /// The state with five PickingLanguage blocks (one focused). Mirrors the
    /// original `state_with_five_blocks` helper but walks the bus so
    /// the reducer's AddBlock path — including `show_block` — runs.
    fn state_with_five_blocks() -> UiView {
        let mut s = UiView::default();
        for _ in 0..5 {
            s.apply(&AppEvent::AddBlock);
        }
        s
    }

    // -----------------------------------------------------------------
    // Pre-menu intent tests (from 595e0a0)
    // -----------------------------------------------------------------

    #[test]
    fn confirm_on_picking_block_invokes_begin_language() {
        let mut bus = EventBus::new();
        let tx = bus.sender();
        let mut h = db_with(&bus);
        let mut state = UiView::default();
        state.apply(&AppEvent::AddBlock);
        resolve_intent(Intent::Confirm, &state, &mut h.db, &tx);
        let events: Vec<_> = bus.drain().collect();
        assert!(matches!(
            events.as_slice(),
            [AppEvent::BeginLanguage { block: 1, language, source_rev: None }]
                if *language == english()
        ));
    }

    #[test]
    fn quit_publishes_quit_event() {
        let mut bus = EventBus::new();
        let tx = bus.sender();
        let mut h = db_with(&bus);
        let state = UiView::default();
        resolve_intent(Intent::Quit, &state, &mut h.db, &tx);
        let events: Vec<_> = bus.drain().collect();
        assert!(
            matches!(events.last(), Some(AppEvent::Quit)),
            "Quit must publish Quit; got {events:?}"
        );
    }

    #[test]
    fn block_closed_cancels_both_pending_models() {
        let mut bus = EventBus::new();
        let tx = bus.sender();
        let mut h = db_with(&bus);
        let mut state = UiView::default();
        for model in english().models() {
            crate::db::downloads::start(&mut h.db, model.id).unwrap();
        }
        state.blocks.push(Block::new(
            1,
            BlockState::Waiting {
                language: english(),
                pending: english().models().map(|model| model.id).to_vec(),
            },
        ));
        resolve_intent(Intent::BlockClosed, &state, &mut h.db, &tx);
        let events: Vec<_> = bus.drain().collect();
        assert!(!events
            .iter()
            .any(|event| matches!(event, AppEvent::DownloadFailed { .. })));
        for model in english().models() {
            let row = crate::db::downloads::get(&h.db, model.id).unwrap().unwrap();
            assert_eq!(row.status, crate::bus::DownloadStatus::Cancelling);
        }
    }

    #[test]
    fn block_closed_cancels_the_sibling_still_running_after_failure() {
        let bus = EventBus::new();
        let tx = bus.sender();
        let mut h = db_with(&bus);
        crate::db::downloads::start(&mut h.db, english().refine.id).unwrap();
        let mut state = UiView::default();
        state.blocks.push(Block::new(
            1,
            BlockState::Failed {
                language: english(),
                error: "live model failed".to_string(),
                pending: vec![english().refine.id],
            },
        ));

        resolve_intent(Intent::BlockClosed, &state, &mut h.db, &tx);

        let row = crate::db::downloads::get(&h.db, english().refine.id)
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
        let state = UiView::default();
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
        let state = UiView {
            menu: Some(SessionMenu::open_at(0)),
            ..UiView::default()
        };
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
        assert_eq!(
            events.len(),
            1,
            "expected exactly one event; got {events:?}"
        );
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
        assert_eq!(
            events.len(),
            1,
            "expected exactly one event; got {events:?}"
        );
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
                AppEvent::MenuMoved {
                    direction: PickerMove::Up
                }
            ),
            "first event must be MenuMoved Up; got {:?}",
            events[0]
        );
        assert!(
            matches!(
                &events[1],
                AppEvent::MenuMoved {
                    direction: PickerMove::Down
                }
            ),
            "second event must be MenuMoved Down; got {:?}",
            events[1]
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
        let state = UiView::default();
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
        let mut state = UiView::default();
        for _ in 0..3 {
            state.apply(&AppEvent::AddBlock);
        }
        state.menu = Some(SessionMenu::open_at(2));
        resolve_intent(Intent::Confirm, &state, &mut h.db, &tx);
        let events: Vec<AppEvent> = bus.drain().collect();
        assert_eq!(
            events.len(),
            1,
            "expected exactly one event; got {events:?}"
        );
        match &events[0] {
            AppEvent::SessionShown { id } => assert_eq!(*id, 3),
            other => panic!("expected SessionShown for id=3, got {other:?}"),
        }
    }
}
