//! Routes accepted events to event-specific consumers.
//!
//! Consumers may publish follow-up events. Those events return through the bus
//! and SQLite gate before another consumer handles them; no handler directly
//! invokes the next stage of a flow.

use std::sync::Arc;

use crate::bus::{AppEvent, EventSender};
use crate::db::Database;
use crate::picker::CATALOG;
use crate::producer::download::Downloader;
use crate::producer::sources::AudioSourcesCatalog;
use crate::transcription_models::ModelStore;

pub mod audio_sources;
pub mod downloads;
pub mod language;
pub mod model_store;
pub mod ui_view;
pub use ui_view::UiView;

use audio_sources::AudioSourcesConsumer;
use downloads::DownloadsConsumer;
use language::LanguageConsumer;
use model_store::ModelStoreConsumer;

/// Independent event consumers and their own collaborators.
pub struct Consumers {
    pub ui_view: UiView,
    pub audio_sources: AudioSourcesConsumer,
    pub language: LanguageConsumer,
    pub downloads: DownloadsConsumer,
    pub model_store: ModelStoreConsumer,
}

impl Consumers {
    pub fn new(
        downloader: Arc<dyn Downloader>,
        model_store: Arc<dyn ModelStore>,
        audio_sources: Arc<dyn AudioSourcesCatalog>,
    ) -> Self {
        Self {
            ui_view: UiView::default(),
            audio_sources: AudioSourcesConsumer::new(audio_sources),
            language: LanguageConsumer,
            downloads: DownloadsConsumer::new(downloader),
            model_store: ModelStoreConsumer::new(model_store),
        }
    }
}

/// Dispatches only events already accepted by the database gate.
pub struct Consumer {
    pub consumers: Consumers,
}

impl Consumer {
    pub fn new(consumers: Consumers) -> Self {
        Self { consumers }
    }

    pub fn consume(&mut self, events: &[AppEvent], db: &mut Database, tx: &EventSender) {
        for event in events {
            // A request can outlive its last waiter across bus passes. Do not
            // create a projection or claim for work nobody needs anymore.
            if let AppEvent::DownloadRequested { model: entry, .. } = event {
                if self.consumers.ui_view.should_quit
                    || !self.consumers.ui_view.blocks.iter().any(|block| {
                        block.pending_models().contains(&entry.id)
                    })
                {
                    continue;
                }
            }
            self.consumers.ui_view.apply(event);
            match event {
                AppEvent::RequestBlock if !self.consumers.ui_view.should_quit => {
                    self.consumers.audio_sources.request(tx);
                }
                AppEvent::BeginLanguage { block, language, .. }
                    if !self.consumers.ui_view.should_quit
                        && self.consumers.ui_view.blocks.iter().any(|candidate| candidate.id == *block) =>
                {
                    self.consumers.language.begin(*block, language, db, tx);
                }
                AppEvent::ModelMissing(entry) if !self.consumers.ui_view.should_quit => {
                    self.consumers.language.model_missing(entry, db, tx);
                }
                AppEvent::DownloadRequested { model, attempt } => {
                    match self.consumers.model_store.prepare_staging(model, *attempt, db) {
                        Ok(()) => self.consumers.downloads.request(model, *attempt, db, tx),
                        Err(error) => tx.publish(AppEvent::DownloadClaimFailed {
                            attempt: 0,
                            model: model.id,
                            error: downloads::truncate_error(&format!("model staging table: {error}")),
                        }),
                    }
                }
                AppEvent::DownloadFetched { model, attempt }
                    if !self.consumers.ui_view.should_quit =>
                {
                    if let Some(entry) = CATALOG.iter().find(|entry| entry.id == *model) {
                        self.consumers.model_store.install(entry, *attempt, db, tx);
                    }
                }
                AppEvent::DiscardInflight { model } => {
                    self.consumers.model_store.discard_inflight(model);
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::{DownloadStatus, EventBus};
    use crate::consumer::ui_view::BlockState;
    use crate::db::downloads;
    use crate::language::LANGUAGES;
    use crate::testing::{FixtureDownloader, FixtureSources, Outcome};
    use crate::transcription_models::CacheDirStore;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn dispatcher(root: &std::path::Path, calls: Arc<AtomicUsize>) -> Consumer {
        let mut consumer = Consumer::new(Consumers::new(
            Arc::new(FixtureDownloader::new(Vec::new(), Outcome::Ok, calls)),
            Arc::new(CacheDirStore::from_root(root.join("models")).unwrap()),
            Arc::new(FixtureSources(None)),
        ));
        consumer.consumers.ui_view.apply(&AppEvent::AddBlock);
        consumer.consumers.ui_view.apply(&AppEvent::LanguageSelected {
            block: 1,
            language: &LANGUAGES[0],
            pending: vec![LANGUAGES[0].live.id],
        });
        consumer
    }

    #[test]
    fn metadata_write_failure_reaches_waiter_before_download_claim() {
        let tmp = tempfile::tempdir().unwrap();
        let mut bus = EventBus::new();
        let mut db = Database::open(&tmp.path().join("downloads.sqlite"), bus.sender()).unwrap();
        db.conn_mut().execute_batch("DROP TABLE model_staging").unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut consumer = dispatcher(tmp.path(), calls.clone());
        let entry = LANGUAGES[0].live;

        consumer.consume(&[AppEvent::DownloadRequested { model: entry, attempt: 1 }], &mut db, &bus.sender());
        let failures: Vec<_> = bus.drain().collect();
        assert!(failures.iter().any(|event| matches!(event,
            AppEvent::DownloadClaimFailed { attempt: 0, model, error }
                if *model == entry.id && error.contains("model staging") && error.contains("no such table")
        )));
        let accepted: Vec<_> = failures.into_iter()
            .filter(|event| crate::db::apply(&mut db, event).unwrap()).collect();
        consumer.consume(&accepted, &mut db, &bus.sender());
        assert!(downloads::get(&db, entry.id).unwrap().is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(matches!(&consumer.consumers.ui_view.blocks[0].state,
            BlockState::Failed { error, .. } if error.contains("model staging")
        ));
        assert!(!consumer.consumers.ui_view.downloads.contains_key(entry.id));
    }

    #[test]
    fn join_terminal_replay_and_stale_requests_do_not_prepare_metadata() {
        for kind in ["join", "terminal", "superseded"] {
            let tmp = tempfile::tempdir().unwrap();
            let mut bus = EventBus::new();
            let mut db = Database::open(&tmp.path().join("downloads.sqlite"), bus.sender()).unwrap();
            let entry = LANGUAGES[0].live;
            let requested_attempt = downloads::start(&mut db, entry.id).unwrap();
            if kind == "terminal" {
                crate::db::apply(&mut db, &AppEvent::DownloadFailed {
                    model: entry.id,
                    attempt: requested_attempt,
                    error: "original fetch failure".into(),
                }).unwrap();
            } else if kind == "superseded" {
                downloads::cancel(&mut db, entry.id).unwrap();
                downloads::start(&mut db, entry.id).unwrap();
            }
            let prior = downloads::get(&db, entry.id).unwrap().unwrap();
            // A metadata write would fail, so a stale request must not even
            // try it and turn that unrelated error into a current UI failure.
            db.conn_mut().execute_batch("DROP TABLE model_staging").unwrap();
            bus.drain().for_each(drop);
            let calls = Arc::new(AtomicUsize::new(0));
            let mut consumer = dispatcher(tmp.path(), calls.clone());

            consumer.consume(&[AppEvent::DownloadRequested { model: entry, attempt: requested_attempt }], &mut db, &bus.sender());
            let events: Vec<_> = bus.drain().collect();
            assert!(!events.iter().any(|event| matches!(event, AppEvent::DownloadClaimFailed { .. })));
            let accepted: Vec<_> = events.into_iter()
                .filter(|event| crate::db::apply(&mut db, event).unwrap()).collect();
            consumer.consume(&accepted, &mut db, &bus.sender());
            let row = downloads::get(&db, entry.id).unwrap().unwrap();
            assert_eq!(row.attempt, prior.attempt);
            assert_eq!(row.status, prior.status);
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            if kind == "terminal" {
                assert_eq!(row.status, DownloadStatus::Failed);
                assert!(matches!(&consumer.consumers.ui_view.blocks[0].state,
                    BlockState::Failed { error, .. } if error == "original fetch failure"
                ));
            } else {
                assert!(matches!(&consumer.consumers.ui_view.blocks[0].state,
                    BlockState::Waiting { pending, .. } if pending.as_slice() == [entry.id]
                ));
            }
        }
    }
}
