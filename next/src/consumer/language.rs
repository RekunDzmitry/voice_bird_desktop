//! Checks language models and publishes requests for the next bus pass.

use std::sync::Arc;

use crate::bus::{AppEvent, EventSender};
use crate::db::downloads::Claim;
use crate::db::{downloads, Database};
use crate::language::LanguageProfile;
use crate::picker::ModelEntry;
use super::downloads::truncate_error;
use crate::transcription_models::ModelStore;

pub struct LanguageConsumer {
    pub model_store: Arc<dyn ModelStore>,
}

impl LanguageConsumer {
    pub fn new(model_store: Arc<dyn ModelStore>) -> Self {
        Self { model_store }
    }

    /// Publish the selection before any per-model reply. Download claims and
    /// workers start only when the requests return through the bus gate.
    pub fn begin(
        &self,
        block: u8,
        language: &'static LanguageProfile,
        db: &Database,
        tx: &EventSender,
    ) {
        let models = language
            .models()
            .map(|model| (model, self.model_store.is_available(model)));
        let pending = models
            .iter()
            .filter_map(|(model, available)| (!available).then_some(model.id))
            .collect();
        tx.publish(AppEvent::LanguageSelected {
            block,
            language,
            pending,
        });
        for (model, available) in models {
            tx.publish(if available {
                AppEvent::ModelAlreadyCached(model)
            } else {
                Self::request_event(model, db)
            });
        }
    }

    pub fn model_missing(&self, entry: &'static ModelEntry, db: &Database, tx: &EventSender) {
        tx.publish(Self::request_event(entry, db));
    }

    /// Snapshot the intended attempt without claiming a row. A download that
    /// finishes before this request returns through the bus must not turn a
    /// join into an implicit retry.
    fn request_event(entry: &'static ModelEntry, db: &Database) -> AppEvent {
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
