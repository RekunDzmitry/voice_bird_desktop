//! Projects accepted events into the UI and routes commands to producers.
//!
//! The database gates events before they reach [`Consumer::consume`]. Each
//! accepted event updates the view before its command starts producer work;
//! producer replies return through the bus and the same database gate.

use std::sync::Arc;

use crate::bus::{AppEvent, EventSender};
use crate::db::Database;
use crate::picker::CATALOG;
use crate::producer::download::{begin_language, ensure_model, Downloader};
use crate::producer::sources::SourceCatalog;
use crate::producer::Producers;
use crate::transcription_models::ModelStore;

pub mod ui_view;
pub use ui_view::UiView;

/// The loop's accepted-event consumer and render-side projection.
pub struct Consumer {
    pub view: UiView,
    pub producers: Producers,
}

impl Consumer {
    pub fn new(
        downloader: Arc<dyn Downloader>,
        store: Arc<dyn ModelStore>,
        sources: Arc<dyn SourceCatalog>,
    ) -> Self {
        Self {
            view: UiView::default(),
            producers: Producers::new(downloader, store, sources),
        }
    }

    /// Fold accepted events in order, then route each event's command.
    /// Source requests return immediately; their answers arrive through the bus.
    pub fn consume(&mut self, events: &[AppEvent], db: &mut Database, tx: &EventSender) {
        for event in events {
            self.view.apply(event);
            match event {
                AppEvent::RequestBlock => self.producers.sources.request(tx),
                AppEvent::BeginLanguage {
                    block, language, ..
                } => begin_language(
                    *block,
                    language,
                    self.producers.store.clone(),
                    db,
                    self.producers.downloads.clone(),
                    tx,
                ),
                AppEvent::ModelMissing(entry) => ensure_model(
                    entry,
                    self.producers.store.clone(),
                    db,
                    self.producers.downloads.clone(),
                    tx,
                ),
                AppEvent::DiscardInflight { model } => {
                    // Unknown model ids have no inflight artifacts to discard.
                    if let Some(entry) = CATALOG.iter().find(|entry| entry.id == model.as_ref()) {
                        self.producers.store.discard_inflight(entry);
                    }
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use tokio::sync::{mpsc, oneshot, Mutex};
    use tokio::time::timeout;

    use super::*;
    use crate::bus::EventBus;
    use crate::producer::sources::AudioSourceSnapshot;
    use crate::testing::{sample_source_snapshot, FixtureDownloader, FixtureStore, Outcome};

    // Deadlock watchdog only; channels establish query and loop ordering.
    const WAIT: Duration = Duration::from_secs(5);

    #[tokio::test]
    async fn blocked_catalog_does_not_hold_quit_discard_or_drop_and_keeps_queued_requests() {
        struct BlockedCatalog {
            entered: mpsc::UnboundedSender<usize>,
            release: Mutex<mpsc::UnboundedReceiver<()>>,
            calls: AtomicUsize,
        }

        impl SourceCatalog for BlockedCatalog {
            fn snapshot(&self) -> Option<AudioSourceSnapshot> {
                let call = self.calls.fetch_add(1, Ordering::SeqCst);
                self.entered.send(call).unwrap();
                self.release.blocking_lock().blocking_recv().unwrap();
                (call == 0).then(sample_source_snapshot)
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let store = Arc::new(FixtureStore::new(root.clone(), &[]));
        let staged = store.staging_path(&CATALOG[0], 1).unwrap();
        std::fs::write(&staged, b"unfinished archive").unwrap();
        let (entered_tx, mut entered) = mpsc::unbounded_channel();
        let (release, release_rx) = mpsc::unbounded_channel();
        let catalog = Arc::new(BlockedCatalog {
            entered: entered_tx,
            release: Mutex::new(release_rx),
            calls: AtomicUsize::new(0),
        });
        let mut bus = EventBus::new();
        let tx = bus.sender();
        let sources = catalog.clone();
        let (proceed, proceed_rx) = oneshot::channel();

        // Keep a watchdog on synchronous consume/Drop without blocking this
        // test's runtime task, which controls the stalled native query.
        let event_loop = tokio::task::spawn_blocking(move || {
            let mut db = Database::open(&root.join("consumer.db"), tx.clone()).unwrap();
            let mut consumer = Consumer::new(
                Arc::new(FixtureDownloader::new(
                    Vec::new(),
                    Outcome::Ok,
                    Arc::new(AtomicUsize::new(0)),
                )),
                store,
                sources,
            );
            consumer.consume(&[AppEvent::RequestBlock], &mut db, &tx);
            proceed_rx.blocking_recv().unwrap();
            consumer.consume(
                &[
                    AppEvent::RequestBlock,
                    AppEvent::DiscardInflight {
                        model: CATALOG[0].id.into(),
                    },
                    AppEvent::Quit,
                ],
                &mut db,
                &tx,
            );
            let should_quit = consumer.view.should_quit;
            drop(consumer);
            should_quit
        });

        assert_eq!(timeout(WAIT, entered.recv()).await.unwrap().unwrap(), 0);
        proceed.send(()).unwrap();
        assert!(timeout(WAIT, event_loop).await.unwrap().unwrap());
        assert!(!staged.exists(), "discard must complete before query release");
        assert_eq!(catalog.calls.load(Ordering::SeqCst), 1);
        assert!(bus.drain().next().is_none());

        release.send(()).unwrap();
        assert_eq!(
            timeout(WAIT, bus.recv()).await.unwrap().unwrap(),
            AppEvent::AddSourceBlock {
                snapshot: sample_source_snapshot(),
            }
        );
        assert_eq!(timeout(WAIT, entered.recv()).await.unwrap().unwrap(), 1);
        release.send(()).unwrap();
        assert_eq!(
            timeout(WAIT, bus.recv()).await.unwrap().unwrap(),
            AppEvent::AddBlock
        );
        assert_eq!(catalog.calls.load(Ordering::SeqCst), 2);
    }
}
