//! Enumerates accepted source requests without blocking the event loop.
//!
//! Native enumeration is serialized per session on Tokio's blocking pool.
//! A query panic disables enumeration for the session, while every current,
//! queued, and future request still publishes a language-first fallback.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::audio_sources::{AudioSourcesCatalog, SourceQueryActive};
use crate::bus::{AppEvent, EventSender};

/// Requests retain their own handles, so queued replies survive producer Drop.
pub struct AudioSourcesProducer {
    audio_sources: Arc<dyn AudioSourcesCatalog>,
    requests: Arc<Mutex<()>>,
    failed: Arc<AtomicBool>,
}

impl AudioSourcesProducer {
    pub fn new(audio_sources: Arc<dyn AudioSourcesCatalog>) -> Self {
        Self {
            audio_sources,
            requests: Arc::new(Mutex::new(())),
            failed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Publish one source-picker or language-picker answer through the bus.
    pub fn start(&self, tx: &EventSender) {
        let audio_sources = self.audio_sources.clone();
        let requests = self.requests.clone();
        let failed = self.failed.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            let _request = requests.lock().await;
            let snapshot = if failed.load(Ordering::Acquire) {
                None
            } else {
                match tokio::task::spawn_blocking(move || {
                    // The guard remains alive during the panic hook and restores
                    // the blocking worker's thread-local when the query unwinds.
                    let _query = SourceQueryActive::enter();
                    audio_sources.snapshot()
                })
                .await
                {
                    Ok(snapshot) => snapshot,
                    Err(error) => {
                        if error.is_panic() {
                            // Native state may be inconsistent after unwinding.
                            failed.store(true, Ordering::Release);
                        }
                        None
                    }
                }
            };
            tx.publish(match snapshot {
                Some(snapshot) if !snapshot.devices.is_empty() => {
                    AppEvent::AddSourceBlock { snapshot }
                }
                _ => AppEvent::AddBlock,
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    use tokio::sync::{mpsc, oneshot};
    use tokio::time::timeout;

    use super::*;
    use crate::audio_sources::{source_query_panicking, AudioSourceSnapshot};
    use crate::bus::EventBus;
    use crate::testing::{sample_source_snapshot, FixtureSources};

    // Deadlock watchdog only; channels establish request and release ordering.
    const WAIT: Duration = Duration::from_secs(5);

    async fn reply(bus: &mut EventBus) -> AppEvent {
        timeout(WAIT, bus.recv()).await.unwrap().unwrap()
    }

    async fn request_block(snapshot: Option<AudioSourceSnapshot>) -> AppEvent {
        let mut bus = EventBus::new();
        let producer = AudioSourcesProducer::new(Arc::new(FixtureSources(snapshot)));
        producer.start(&bus.sender());
        reply(&mut bus).await
    }

    #[tokio::test]
    async fn request_block_starts_source_funnel_when_devices_exist() {
        let snapshot = sample_source_snapshot();
        assert_eq!(
            request_block(Some(snapshot.clone())).await,
            AppEvent::AddSourceBlock { snapshot }
        );
    }

    #[tokio::test]
    async fn request_block_falls_back_when_sources_are_unavailable() {
        assert_eq!(request_block(None).await, AppEvent::AddBlock);
    }

    #[tokio::test]
    async fn request_block_falls_back_when_only_apps_exist() {
        let mut snapshot = sample_source_snapshot();
        snapshot.devices.clear();
        assert_eq!(request_block(Some(snapshot)).await, AppEvent::AddBlock);
    }

    #[tokio::test]
    async fn request_block_falls_back_when_snapshot_is_empty() {
        assert_eq!(
            request_block(Some(AudioSourceSnapshot {
                devices: Vec::new(),
                apps: Vec::new(),
            }))
            .await,
            AppEvent::AddBlock
        );
    }

    #[tokio::test]
    async fn request_block_keeps_source_funnel_when_no_apps_are_running() {
        let mut snapshot = sample_source_snapshot();
        snapshot.apps.clear();
        assert_eq!(
            request_block(Some(snapshot.clone())).await,
            AppEvent::AddSourceBlock { snapshot }
        );
    }

    #[tokio::test]
    async fn panicking_catalog_answers_current_queued_and_future_requests() {
        struct QueryUnwind(Arc<AtomicBool>);

        impl Drop for QueryUnwind {
            fn drop(&mut self) {
                self.0.store(source_query_panicking(), Ordering::SeqCst);
            }
        }

        struct PanickingCatalog {
            entered: mpsc::UnboundedSender<()>,
            release: Mutex<Option<oneshot::Receiver<()>>>,
            calls: AtomicUsize,
            recoverable_unwind: Arc<AtomicBool>,
        }

        impl AudioSourcesCatalog for PanickingCatalog {
            fn snapshot(&self) -> Option<AudioSourceSnapshot> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let _unwind = QueryUnwind(self.recoverable_unwind.clone());
                self.entered.send(()).unwrap();
                self.release
                    .blocking_lock()
                    .take()
                    .unwrap()
                    .blocking_recv()
                    .unwrap();
                panic!("native enumeration failed");
            }
        }

        let mut bus = EventBus::new();
        let tx = bus.sender();
        let (entered_tx, mut entered) = mpsc::unbounded_channel();
        let (release, release_rx) = oneshot::channel();
        let recoverable_unwind = Arc::new(AtomicBool::new(false));
        let audio_sources = Arc::new(PanickingCatalog {
            entered: entered_tx,
            release: Mutex::new(Some(release_rx)),
            calls: AtomicUsize::new(0),
            recoverable_unwind: recoverable_unwind.clone(),
        });
        let producer = AudioSourcesProducer::new(audio_sources.clone());
        producer.start(&tx);
        timeout(WAIT, entered.recv()).await.unwrap().unwrap();
        producer.start(&tx);
        release.send(()).unwrap();
        assert_eq!(reply(&mut bus).await, AppEvent::AddBlock);
        assert_eq!(reply(&mut bus).await, AppEvent::AddBlock);
        producer.start(&tx);
        assert_eq!(reply(&mut bus).await, AppEvent::AddBlock);
        assert_eq!(audio_sources.calls.load(Ordering::SeqCst), 1);
        assert!(recoverable_unwind.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn requests_are_serialized_and_each_enumerates_a_fresh_snapshot() {
        struct SerializedCatalog {
            entered: mpsc::UnboundedSender<(usize, usize)>,
            release: Mutex<mpsc::UnboundedReceiver<()>>,
            calls: AtomicUsize,
            active: AtomicUsize,
        }

        impl AudioSourcesCatalog for SerializedCatalog {
            fn snapshot(&self) -> Option<AudioSourceSnapshot> {
                let call = self.calls.fetch_add(1, Ordering::SeqCst);
                let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                self.entered.send((call, active)).unwrap();
                self.release.blocking_lock().blocking_recv().unwrap();
                self.active.fetch_sub(1, Ordering::SeqCst);
                (call == 0).then(sample_source_snapshot)
            }
        }

        let mut bus = EventBus::new();
        let tx = bus.sender();
        let (entered_tx, mut entered) = mpsc::unbounded_channel();
        let (release, release_rx) = mpsc::unbounded_channel();
        let audio_sources = Arc::new(SerializedCatalog {
            entered: entered_tx,
            release: Mutex::new(release_rx),
            calls: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
        });
        let producer = AudioSourcesProducer::new(audio_sources.clone());
        producer.start(&tx);
        assert_eq!(
            timeout(WAIT, entered.recv()).await.unwrap().unwrap(),
            (0, 1)
        );
        producer.start(&tx);
        tokio::task::yield_now().await;
        assert_eq!(audio_sources.calls.load(Ordering::SeqCst), 1);
        assert!(entered.try_recv().is_err());
        assert!(bus.drain().next().is_none());

        release.send(()).unwrap();
        assert_eq!(
            reply(&mut bus).await,
            AppEvent::AddSourceBlock {
                snapshot: sample_source_snapshot(),
            }
        );
        assert_eq!(
            timeout(WAIT, entered.recv()).await.unwrap().unwrap(),
            (1, 1)
        );
        release.send(()).unwrap();
        assert_eq!(reply(&mut bus).await, AppEvent::AddBlock);
        assert_eq!(audio_sources.calls.load(Ordering::SeqCst), 2);
        assert_eq!(audio_sources.active.load(Ordering::SeqCst), 0);
    }
}
