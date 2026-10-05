//! Bus command dispatcher for the long-lived collaborators.
//!
//! The `Downloader`, `ModelStore`, and `SourceCatalog` collaborators are wired
//! once in `main.rs` to a single [`Dispatcher`] that lives on the
//! event-loop thread. The resolver and the Quit-time cleanup speak to the
//! dispatcher through bus command variants. Download commands call their
//! collaborators directly; source requests queue work on one serialized
//! background thread so native enumeration cannot stall input or Quit.
//!
//! ## Bus commands consumed
//!
//! - [`AppEvent::RequestBlock`](crate::bus::AppEvent::RequestBlock) —
//!   enqueue enumeration of the current sources and open a source picker,
//!   or fall back to the language picker when enumeration is unavailable
//!   or has no devices. Each queued request publishes one answer.
//! - [`AppEvent::BeginLanguage`](crate::bus::AppEvent::BeginLanguage) —
//!   the resolver saw `Confirm` or `Retry`. The dispatcher calls
//!   [`download::begin_language`](crate::download::begin_language).
//! - [`AppEvent::ModelMissing`](crate::bus::AppEvent::ModelMissing) —
//!   an active block lost an installed model. The dispatcher calls
//!   [`download::ensure_model`](crate::download::ensure_model) to re-download it.
//! - [`AppEvent::DiscardInflight`] — Quit-time cleanup asks the
//!   dispatcher to drop the staged archive and unpack scratch
//!   directory for one model. No reply (best-effort).
//!
//! ## Thread ownership
//!
//! `download::begin_language` checks the model store synchronously on the loop
//! thread. Source snapshots run only on the dispatcher's dedicated worker,
//! one at a time, and publish replies through the event bus. Dropping the
//! dispatcher closes the request queue without joining the worker: a stalled
//! native query must not block Quit. Once queued work finishes, the worker exits.
//!
//! [`AppEvent::DiscardInflight`]: crate::bus::AppEvent::DiscardInflight

use std::sync::{mpsc, Arc};

use crate::audio_source::SourceCatalog;
use crate::bus::{AppEvent, EventSender};
use crate::db::Database;
use crate::download::{begin_language, ensure_model};
use crate::picker::{ModelEntry, CATALOG};
use crate::transcription_models::ModelStore;

/// Owns the download, model-store, and source-catalog collaborators and answers
/// the bus commands that ask them to do work. Constructed once in
/// `main::run`; never cloned.
pub struct Dispatcher {
    downloader: Arc<dyn crate::download::Downloader>,
    model_store: Arc<dyn ModelStore>,
    source_requests: mpsc::Sender<EventSender>,
}

impl Dispatcher {
    /// Build a dispatcher that owns clones of the given collaborators.
    pub fn new(
        downloader: Arc<dyn crate::download::Downloader>,
        model_store: Arc<dyn ModelStore>,
        sources: Arc<dyn SourceCatalog>,
    ) -> Self {
        let (source_requests, requests) = mpsc::channel::<EventSender>();
        // Detach the worker: dropping the dispatcher must not wait for native
        // enumeration. Closing its sender lets the worker exit after its queue.
        let _ = std::thread::spawn(move || {
            for tx in requests {
                let event = match sources.snapshot() {
                    Some(snapshot) if !snapshot.devices.is_empty() => {
                        AppEvent::AddSourceBlock { snapshot }
                    }
                    _ => AppEvent::AddBlock,
                };
                tx.publish(event);
            }
        });
        Self {
            downloader,
            model_store,
            source_requests,
        }
    }

    /// Process every bus command in `events`. Called once per tick
    /// from the main loop, after the UI reducer has folded the
    /// events. UI events are silently ignored — only the command
    /// variants do anything here.
    ///
    /// `db` is borrowed mutably for each `download::begin_language` call,
    /// so the next tick gets a fresh `&mut`.
    /// Source requests enqueue immediately; their answers arrive on a later tick.
    pub fn dispatch(&self, events: &[AppEvent], db: &mut Database, tx: &EventSender) {
        for ev in events {
            match ev {
                AppEvent::RequestBlock => {
                    self.source_requests
                        .send(tx.clone())
                        .expect("source catalog worker stopped");
                }
                AppEvent::BeginLanguage {
                    block, language, ..
                } => {
                    begin_language(
                        *block,
                        language,
                        self.model_store.clone(),
                        db,
                        self.downloader.clone(),
                        tx,
                    );
                }
                AppEvent::ModelMissing(entry) => {
                    ensure_model(
                        entry,
                        self.model_store.clone(),
                        db,
                        self.downloader.clone(),
                        tx,
                    );
                }
                AppEvent::DiscardInflight { model } => {
                    // `model` is `Arc<str>`; `catalog_lookup` wants
                    // `&str` so `Arc::as_ref` gives us that.
                    if let Some(entry) = catalog_lookup(model.as_ref()) {
                        self.model_store.discard_inflight(entry);
                    }
                }
                _ => {
                    // Non-command variants are silently ignored.
                    let _ = tx;
                }
            }
        }
    }
}

/// Resolve a static model id back to its `&'static ModelEntry` in
/// the catalog. Returns `None` if the id isn't in the catalog —
/// `discard_inflight` for an unknown model is a no-op rather than
/// a panic.
fn catalog_lookup(model: &str) -> Option<&'static ModelEntry> {
    CATALOG.iter().find(|e| e.id == model)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Barrier;
    use std::thread;
    use std::time::Duration;

    use super::*;
    use crate::audio_source::AudioSourceSnapshot;
    use crate::testing::{
        sample_source_snapshot, FixtureDownloader, FixtureSources, FixtureStore, Outcome,
    };

    // Only a deadlock watchdog; all ordering is established by channels.
    const WAIT: Duration = Duration::from_secs(5);

    fn request_block(snapshot: Option<AudioSourceSnapshot>) -> Vec<AppEvent> {
        let tmp = tempfile::tempdir().unwrap();
        let (events, replies) = mpsc::channel();
        let tx = EventSender::from_mpsc(events);
        let mut db = Database::open(&tmp.path().join("dispatcher.db"), tx.clone()).unwrap();
        let dispatcher = Dispatcher::new(
            Arc::new(FixtureDownloader::new(
                Vec::new(),
                Outcome::Ok,
                Arc::new(AtomicUsize::new(0)),
            )),
            Arc::new(FixtureStore::new(tmp.path().into(), &[])),
            Arc::new(FixtureSources(snapshot)),
        );
        dispatcher.dispatch(&[AppEvent::RequestBlock], &mut db, &tx);
        vec![replies.recv_timeout(WAIT).unwrap()]
    }

    #[test]
    fn request_block_starts_source_funnel_when_devices_exist() {
        let snapshot = sample_source_snapshot();
        assert_eq!(
            request_block(Some(snapshot.clone())),
            vec![AppEvent::AddSourceBlock { snapshot }]
        );
    }

    #[test]
    fn request_block_falls_back_when_sources_are_unavailable() {
        assert_eq!(request_block(None), vec![AppEvent::AddBlock]);
    }

    #[test]
    fn request_block_falls_back_when_only_apps_exist() {
        let mut snapshot = sample_source_snapshot();
        snapshot.devices.clear();
        assert_eq!(request_block(Some(snapshot)), vec![AppEvent::AddBlock]);
    }

    #[test]
    fn request_block_falls_back_when_snapshot_is_empty() {
        assert_eq!(
            request_block(Some(AudioSourceSnapshot {
                devices: Vec::new(),
                apps: Vec::new(),
            })),
            vec![AppEvent::AddBlock]
        );
    }

    #[test]
    fn request_block_keeps_source_funnel_when_no_apps_are_running() {
        let mut snapshot = sample_source_snapshot();
        snapshot.apps.clear();
        assert_eq!(
            request_block(Some(snapshot.clone())),
            vec![AppEvent::AddSourceBlock { snapshot }]
        );
    }

    #[test]
    fn blocked_catalog_does_not_hold_dispatch_or_quit_and_preserves_queued_requests() {
        struct BlockedCatalog {
            entered: mpsc::Sender<thread::ThreadId>,
            release: Arc<Barrier>,
            calls: AtomicUsize,
        }

        impl SourceCatalog for BlockedCatalog {
            fn snapshot(&self) -> Option<AudioSourceSnapshot> {
                let call = self.calls.fetch_add(1, Ordering::SeqCst);
                self.entered.send(thread::current().id()).ok()?;
                self.release.wait();
                // Each request gets a fresh result, in queue order.
                (call == 0).then(sample_source_snapshot)
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let (entered_tx, entered) = mpsc::channel();
        let release = Arc::new(Barrier::new(2));
        let (events, replies) = mpsc::channel();
        let (finished_tx, finished) = mpsc::channel();
        let catalog = Arc::new(BlockedCatalog {
            entered: entered_tx,
            release: release.clone(),
            calls: AtomicUsize::new(0),
        });
        let sources = catalog.clone();

        let event_loop = thread::spawn(move || {
            let tx = EventSender::from_mpsc(events);
            let mut db = Database::open(&root.join("dispatcher.db"), tx.clone()).unwrap();
            let store = Arc::new(FixtureStore::new(root, &[]));
            let dispatcher = Dispatcher::new(
                Arc::new(FixtureDownloader::new(
                    Vec::new(),
                    Outcome::Ok,
                    Arc::new(AtomicUsize::new(0)),
                )),
                store.clone(),
                sources,
            );
            dispatcher.dispatch(&[AppEvent::RequestBlock], &mut db, &tx);
            dispatcher.dispatch(
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
            drop(dispatcher);
            finished_tx
                .send(store.clear_staging_calls.lock().unwrap().clone())
                .unwrap();
        });

        let source_thread = entered.recv_timeout(WAIT).unwrap();
        assert_ne!(source_thread, event_loop.thread().id());
        // Neither dispatch nor Drop may wait for the first snapshot's release.
        assert_eq!(finished.recv_timeout(WAIT).unwrap(), vec![CATALOG[0].id]);
        assert_eq!(catalog.calls.load(Ordering::SeqCst), 1);
        assert_eq!(replies.try_recv(), Err(mpsc::TryRecvError::Empty));

        release.wait();
        assert_eq!(
            replies.recv_timeout(WAIT).unwrap(),
            AppEvent::AddSourceBlock {
                snapshot: sample_source_snapshot(),
            }
        );
        // The queued request survives dispatcher Drop and uses the same worker.
        assert_eq!(entered.recv_timeout(WAIT).unwrap(), source_thread);
        release.wait();
        assert_eq!(replies.recv_timeout(WAIT).unwrap(), AppEvent::AddBlock);
        event_loop.join().unwrap();
        assert_eq!(
            replies.recv_timeout(WAIT),
            Err(mpsc::RecvTimeoutError::Disconnected)
        );
        assert_eq!(catalog.calls.load(Ordering::SeqCst), 2);
    }
}
