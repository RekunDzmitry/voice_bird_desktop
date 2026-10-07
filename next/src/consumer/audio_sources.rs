//! Hands accepted source requests to the producer-side enumeration service.

use std::sync::Arc;

use crate::audio_sources::AudioSourcesCatalog;
use crate::bus::EventSender;
use crate::producer::audio_sources::AudioSourcesProducer;

pub struct AudioSourcesConsumer {
    producer: AudioSourcesProducer,
}

impl AudioSourcesConsumer {
    pub fn new(audio_sources: Arc<dyn AudioSourcesCatalog>) -> Self {
        Self {
            producer: AudioSourcesProducer::new(audio_sources),
        }
    }

    /// Hand off one accepted source request without waiting for enumeration.
    pub fn request(&self, tx: &EventSender) {
        self.producer.start(tx);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use tokio::sync::{mpsc, oneshot, Mutex};
    use tokio::time::timeout;

    use super::*;
    use crate::audio_sources::AudioSourceSnapshot;
    use crate::bus::{AppEvent, EventBus};
    use crate::consumer::{Consumer, Consumers};
    use crate::db::Database;
    use crate::picker::CATALOG;
    use crate::testing::{sample_source_snapshot, FixtureDownloader, FixtureStore, Outcome};
    use crate::transcription_models::ModelStore;

    // Deadlock watchdog only; channels establish request and release ordering.
    const WAIT: Duration = Duration::from_secs(5);

    async fn reply(bus: &mut EventBus) -> AppEvent {
        timeout(WAIT, bus.recv()).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn blocked_catalog_does_not_hold_quit_discard_or_drop_and_keeps_queued_requests() {
        struct BlockedCatalog {
            entered: mpsc::UnboundedSender<usize>,
            release: Mutex<mpsc::UnboundedReceiver<()>>,
            calls: AtomicUsize,
        }

        impl AudioSourcesCatalog for BlockedCatalog {
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
        let audio_sources = Arc::new(BlockedCatalog {
            entered: entered_tx,
            release: Mutex::new(release_rx),
            calls: AtomicUsize::new(0),
        });
        let mut bus = EventBus::new();
        let tx = bus.sender();
        let consumer_audio_sources = audio_sources.clone();
        let (proceed, proceed_rx) = oneshot::channel();

        // Keep a watchdog on synchronous consume/Drop without blocking this
        // test's runtime task, which controls the stalled native query.
        let event_loop = tokio::task::spawn_blocking(move || {
            let mut db = Database::open(&root.join("consumer.db"), tx.clone()).unwrap();
            let mut consumer = Consumer::new(Consumers::new(
                Arc::new(FixtureDownloader::new(
                    Vec::new(),
                    Outcome::Ok,
                    Arc::new(AtomicUsize::new(0)),
                )),
                store,
                consumer_audio_sources,
            ));
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
            let should_quit = consumer.consumers.ui_view.should_quit;
            drop(consumer);
            should_quit
        });

        assert_eq!(timeout(WAIT, entered.recv()).await.unwrap().unwrap(), 0);
        proceed.send(()).unwrap();
        assert!(timeout(WAIT, event_loop).await.unwrap().unwrap());
        assert!(!staged.exists(), "discard must complete before query release");
        assert_eq!(audio_sources.calls.load(Ordering::SeqCst), 1);
        assert!(bus.drain().next().is_none());

        release.send(()).unwrap();
        assert_eq!(
            reply(&mut bus).await,
            AppEvent::AddSourceBlock {
                snapshot: sample_source_snapshot(),
            }
        );
        assert_eq!(timeout(WAIT, entered.recv()).await.unwrap().unwrap(), 1);
        release.send(()).unwrap();
        assert_eq!(reply(&mut bus).await, AppEvent::AddBlock);
        assert_eq!(audio_sources.calls.load(Ordering::SeqCst), 2);
    }
}
