//! Intent → bus event translation.
//!
//! [`resolve_intent`] is the single seam where a user key press becomes
//! one or more [`AppEvent`]s. The reducer does the rest. Splitting it
//! out of `main.rs` keeps the binary entry point focused on terminal
//! plumbing (raw mode, alt screen, panic hook, the render loop) and
//! puts everything that talks to the bus next to the bus itself.
use crate::audio_sources::{DeviceKind, FunnelStep};
use crate::bus::{AppEvent, EventSender, FocusMove};

use crate::consumer::ui_view::{BlockState, UiView};
use crate::db::{downloads, Database};
use crate::input::Intent;
use crate::language::LANGUAGES;
use crate::picker;
/// Stamp the `from_language`/`to_language` codes onto `PickerMoved`.
/// The input layer has no registry context, so the resolver reads the
/// focused block's picker without mutating it.
pub fn stamp_picker_move(tx: &EventSender, ui_view: &UiView, direction: picker::PickerMove) {
    let (from_language, to_language) = match ui_view.focused() {
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
pub fn resolve_intent(intent: Intent, ui_view: &UiView, db: &mut Database, tx: &EventSender) {
    match intent {
        Intent::AddBlock => tx.publish(AppEvent::RequestBlock),
        Intent::FocusPrev => tx.publish(AppEvent::FocusMoved {
            direction: FocusMove::Prev,
        }),
        Intent::FocusNext => tx.publish(AppEvent::FocusMoved {
            direction: FocusMove::Next,
        }),
        Intent::PickerPrev => stamp_picker_move(tx, ui_view, picker::PickerMove::Up),
        Intent::PickerNext => stamp_picker_move(tx, ui_view, picker::PickerMove::Down),
        Intent::Confirm => {
            if let Some(block) = ui_view.focused() {
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
            if let Some(block) = ui_view.focused() {
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
            if let Some(block) = ui_view.focused() {
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
            if let Some(block) = ui_view.focused() {
                for &model in block.pending_models() {
                    let any_other = ui_view.blocks.iter().any(|other| {
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::EventBus;
    use crate::consumer::ui_view::Block;
    use crate::db::Database;
    use crate::language::{LanguageProfile, LANGUAGES};

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

    #[test]
    fn confirm_on_picking_block_invokes_begin_language() {
        let mut bus = EventBus::new();
        let tx = bus.sender();
        let mut h = db_with(&bus);
        let mut ui_view = UiView::default();
        ui_view.apply(&AppEvent::AddBlock);
        resolve_intent(Intent::Confirm, &ui_view, &mut h.db, &tx);
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
        let ui_view = UiView::default();
        resolve_intent(Intent::Quit, &ui_view, &mut h.db, &tx);
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
        let mut ui_view = UiView::default();
        for model in english().models() {
            crate::db::downloads::start(&mut h.db, model.id).unwrap();
        }
        ui_view.blocks.push(Block::new(
            1,
            BlockState::Waiting {
                language: english(),
                pending: english().models().iter().map(|model| model.id).collect(),
            },
        ));
        resolve_intent(Intent::BlockClosed, &ui_view, &mut h.db, &tx);
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
        let mut ui_view = UiView::default();
        ui_view.blocks.push(Block::new(
            1,
            BlockState::Failed {
                language: english(),
                error: "live model failed".to_string(),
                pending: vec![english().refine.id],
            },
        ));

        resolve_intent(Intent::BlockClosed, &ui_view, &mut h.db, &tx);

        let row = crate::db::downloads::get(&h.db, english().refine.id)
            .unwrap()
            .unwrap();
        assert_eq!(row.status, crate::bus::DownloadStatus::Cancelling);
    }
}
