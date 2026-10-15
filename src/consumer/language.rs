//! Queries persisted model availability and publishes requests for the next bus pass.

use crate::bus::{AppEvent, EventSender};
use crate::db::downloads::Claim;
use crate::db::{downloads, models, Database};
use crate::download::truncate_error;
use crate::language::LanguageProfile;
use crate::picker::ModelEntry;

pub struct LanguageConsumer;

impl LanguageConsumer {
    /// Publish the selection before any per-model reply. Download claims and
    /// workers start only when the requests return through the bus gate.
    pub fn begin(
        &self,
        block: u8,
        language: &'static LanguageProfile,
        db: &Database,
        tx: &EventSender,
    ) {
        let events: Vec<_> = language
            .models()
            .iter()
            .copied()
            .map(|model| (model.id, Self::request_event(model, db)))
            .collect();
        let pending = events
            .iter()
            .filter_map(|(model, event)| {
                (!matches!(event, AppEvent::ModelAlreadyCached(_))).then_some(*model)
            })
            .collect();
        tx.publish(AppEvent::LanguageSelected {
            block,
            language,
            pending,
        });
        for (_, event) in events {
            tx.publish(event);
        }
    }

    pub fn model_missing(&self, entry: &'static ModelEntry, db: &Database, tx: &EventSender) {
        tx.publish(Self::request_event(entry, db));
    }

    /// Snapshot the intended attempt without claiming a row. A download that
    /// finishes before this request returns through the bus must not turn a
    /// join into an implicit retry.
    fn request_event(entry: &'static ModelEntry, db: &Database) -> AppEvent {
        match models::is_available(db, entry.id) {
            Ok(true) => return AppEvent::ModelAlreadyCached(entry),
            Ok(false) => {}
            Err(error) => {
                return AppEvent::DownloadClaimFailed {
                    attempt: 0,
                    model: entry.id,
                    error: truncate_error(&format!("models table: {error}")),
                };
            }
        }
        match downloads::get(db, entry.id) {
            Ok(row) => {
                let attempt = match downloads::decide(row.as_ref()) {
                    Claim::Start { attempt } | Claim::Restart { attempt } => attempt,
                    Claim::Join => row.as_ref().expect("join requires a row").attempt,
                };
                AppEvent::DownloadRequested {
                    model: entry,
                    attempt,
                }
            }
            Err(error) => AppEvent::DownloadClaimFailed {
                attempt: 0,
                model: entry.id,
                error: truncate_error(&format!("downloads table: {error}")),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::EventBus;
    use crate::consumer::{ui_view::BlockState, UiView};
    use crate::language::LANGUAGES;

    #[test]
    fn persisted_availability_records_even_when_download_history_is_unreadable() {
        let tmp = tempfile::tempdir().unwrap();
        let mut bus = EventBus::new();
        let mut db = Database::open(&tmp.path().join("models.sqlite"), bus.sender()).unwrap();
        let language = &LANGUAGES[0];
        for model in language.models() {
            models::set_available(&mut db, model.id, true).unwrap();
        }
        db.conn_mut().execute_batch("DROP TABLE downloads").unwrap();
        let mut view = UiView::default();
        view.apply(&AppEvent::AddBlock);

        LanguageConsumer.begin(1, language, &db, &bus.sender());
        let events: Vec<_> = bus.drain().collect();
        assert_eq!(
            events,
            std::iter::once(AppEvent::LanguageSelected {
                block: 1,
                language,
                pending: vec![]
            })
            .chain(
                language
                    .models()
                    .iter()
                    .copied()
                    .map(AppEvent::ModelAlreadyCached)
            )
            .collect::<Vec<_>>()
        );
        for event in events {
            assert!(crate::db::apply(&mut db, &event).unwrap());
            view.apply(&event);
        }
        assert!(
            matches!(view.blocks[0].state, BlockState::Recording { language: selected, .. }
            if selected == language)
        );
    }

    #[test]
    fn availability_and_download_lookup_errors_fail_both_pending_models() {
        for table in ["models", "downloads"] {
            let tmp = tempfile::tempdir().unwrap();
            let mut bus = EventBus::new();
            let mut db = Database::open(&tmp.path().join("models.sqlite"), bus.sender()).unwrap();
            db.conn_mut()
                .execute_batch(&format!("DROP TABLE {table}"))
                .unwrap();
            let language = &LANGUAGES[0];
            let mut view = UiView::default();
            view.apply(&AppEvent::AddBlock);

            LanguageConsumer.begin(1, language, &db, &bus.sender());
            let events: Vec<_> = bus.drain().collect();
            assert_eq!(
                events[0],
                AppEvent::LanguageSelected {
                    block: 1,
                    language,
                    pending: language.models().iter().map(|model| model.id).collect(),
                }
            );
            for (event, model) in events[1..].iter().zip(language.models()) {
                assert!(matches!(event, AppEvent::DownloadClaimFailed {
                    attempt: 0, model: failed, error,
                } if *failed == model.id && error.starts_with(&format!("{table} table:"))));
            }
            assert_eq!(events.len(), 1 + language.models().len());
            for event in events {
                assert!(crate::db::apply(&mut db, &event).unwrap());
                view.apply(&event);
            }
            assert!(matches!(&view.blocks[0].state, BlockState::Failed {
                language: selected, pending, error,
            } if *selected == language
                && pending.is_empty()
                && error.starts_with(&format!("{table} table:"))));
        }
    }

    #[cfg(all(target_os = "macos", feature = "local-engine"))]
    #[test]
    fn cached_decoder_weights_request_only_missing_required_encoders() {
        let tmp = tempfile::tempdir().unwrap();
        let mut bus = EventBus::new();
        let mut db = Database::open(&tmp.path().join("models.sqlite"), bus.sender()).unwrap();
        let language = &LANGUAGES[0];
        for model in [language.live, language.refine] {
            models::set_available(&mut db, model.id, true).unwrap();
        }
        LanguageConsumer.begin(1, language, &db, &bus.sender());
        let events: Vec<_> = bus.drain().collect();
        assert_eq!(
            events[0],
            AppEvent::LanguageSelected {
                block: 1,
                language,
                pending: vec!["distil-small.en-encoder", "large-v3-turbo-encoder"],
            }
        );
        let requested: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                AppEvent::DownloadRequested { model, .. } => Some(model.id),
                _ => None,
            })
            .collect();
        assert_eq!(
            requested,
            ["distil-small.en-encoder", "large-v3-turbo-encoder"]
        );
        assert!(events.contains(&AppEvent::ModelAlreadyCached(language.live)));
        assert!(events.contains(&AppEvent::ModelAlreadyCached(language.refine)));
    }
}
