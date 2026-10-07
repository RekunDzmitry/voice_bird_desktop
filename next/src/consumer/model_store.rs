//! Prepares staging metadata, installs fetched models, and discards inflight files.

use std::sync::Arc;

use crate::bus::{AppEvent, DownloadStatus, EventSender};
use crate::db::downloads::CancelCheck;
use crate::db::{downloads, model_staging, Database};
use crate::picker::{ModelEntry, CATALOG};
use crate::producer::download::DownloadError;
use crate::transcription_models::{handler_for, ModelStore};

use super::downloads::truncate_error;

pub struct ModelStoreConsumer {
    pub model_store: Arc<dyn ModelStore>,
}

impl ModelStoreConsumer {
    pub fn new(model_store: Arc<dyn ModelStore>) -> Self {
        Self { model_store }
    }

    pub fn prepare_staging(
        &self,
        entry: &ModelEntry,
        attempt: u32,
        db: &mut Database,
    ) -> rusqlite::Result<()> {
        let row = downloads::get(db, entry.id)?;
        match downloads::decide(row.as_ref()) {
            downloads::Claim::Start { attempt: intended }
            | downloads::Claim::Restart { attempt: intended }
                if intended == attempt =>
            {
                model_staging::observe(db, self.model_store.as_ref(), entry, attempt)
            }
            _ => Ok(()),
        }
    }

    /// An accepted fetch has already moved its row to Installing. Recheck it
    /// because a later event in the same gated batch may have cancelled it.
    pub fn install(
        &self,
        entry: &'static ModelEntry,
        attempt: u32,
        db: &Database,
        tx: &EventSender,
    ) {
        match downloads::get(db, entry.id) {
            Ok(Some(row))
                if row.attempt == attempt && row.status == DownloadStatus::Installing => {}
            Ok(Some(row))
                if row.attempt == attempt && row.status == DownloadStatus::Cancelling =>
            {
                tx.publish(AppEvent::DownloadCancelled { attempt, model: entry.id });
                return;
            }
            Ok(_) => return,
            Err(error) => {
                tx.publish(AppEvent::DownloadFailed {
                    attempt,
                    model: entry.id,
                    error: truncate_error(&format!("downloads table: {error}")),
                });
                return;
            }
        }
        let staged = match model_staging::get(db, entry.id, attempt) {
            Ok(Some(result)) => result,
            Ok(None) => Err(format!(
                "model staging path missing for {} attempt {attempt}",
                entry.id
            )),
            Err(error) => Err(format!("model staging table: {error}")),
        };
        let staged = match staged {
            Ok(path) => path,
            Err(error) => {
                tx.publish(AppEvent::DownloadFailed {
                    attempt,
                    model: entry.id,
                    error: truncate_error(&error),
                });
                return;
            }
        };
        let store = self.model_store.clone();
        let mut probe = downloads::probe(db, entry.id, attempt);
        let tx = tx.clone();
        tokio::spawn(async move {
            let model = entry.id;
            if handler_for(entry.format).install_is_slow() {
                tx.publish(AppEvent::DownloadInstalling { attempt, model });
            }
            let result = tokio::task::spawn_blocking(move || {
                // Use a fresh probe here: cancellation after fetch must be
                // observed even by a fast handler that only renames a file.
                if probe.is_cancelled() {
                    return Err(DownloadError::Cancelled);
                }
                store.install(entry, &staged, &mut probe)
            })
            .await
            .unwrap_or_else(|error| Err(DownloadError::Install(error.to_string())));
            match result {
                Ok(()) => tx.publish(AppEvent::DownloadSucceeded { attempt, model }),
                Err(DownloadError::Cancelled) => {
                    tx.publish(AppEvent::DownloadCancelled { attempt, model });
                }
                Err(error) => tx.publish(AppEvent::DownloadFailed {
                    attempt,
                    model,
                    error: truncate_error(&error.to_string()),
                }),
            }
        });
    }

    pub fn discard_inflight(&self, model: &str) {
        if let Some(entry) = CATALOG.iter().find(|entry| entry.id == model) {
            self.model_store.discard_inflight(entry);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::EventBus;
    use crate::consumer::ui_view::BlockState;
    use crate::consumer::UiView;
    use crate::language::LANGUAGES;
    use crate::transcription_models::CacheDirStore;

    fn stage(
        consumer: &ModelStoreConsumer,
        db: &mut Database,
        entry: &ModelEntry,
        attempt: u32,
    ) -> std::path::PathBuf {
        model_staging::observe(db, consumer.model_store.as_ref(), entry, attempt).unwrap();
        let path = model_staging::get(db, entry.id, attempt)
            .unwrap()
            .unwrap()
            .unwrap();
        std::fs::write(&path, b"verified model bytes").unwrap();
        path
    }

    #[tokio::test]
    async fn fetched_bytes_only_make_waiter_ready_after_install_finishes() {
        let tmp = tempfile::tempdir().unwrap();
        let mut bus = EventBus::new();
        let mut db = Database::open(&tmp.path().join("downloads.sqlite"), bus.sender()).unwrap();
        let store = Arc::new(CacheDirStore::from_root(tmp.path().join("models")).unwrap());
        let consumer = ModelStoreConsumer::new(store.clone());
        let language = &LANGUAGES[0];
        let entry = language.live;
        let attempt = downloads::start(&mut db, entry.id).unwrap();
        let staged = stage(&consumer, &mut db, entry, attempt);
        bus.drain().for_each(drop);
        let mut view = UiView::default();
        view.apply(&AppEvent::AddBlock);
        view.apply(&AppEvent::LanguageSelected {
            block: 1,
            language,
            pending: vec![entry.id],
        });
        view.apply(&AppEvent::DownloadRequested { model: entry, attempt });

        let fetched = AppEvent::DownloadFetched { model: entry.id, attempt };
        assert!(crate::db::apply(&mut db, &fetched).unwrap());
        view.apply(&fetched);
        for event in bus.drain() {
            if crate::db::apply(&mut db, &event).unwrap() {
                view.apply(&event);
            }
        }
        assert!(!store.is_available(entry));
        assert!(matches!(view.blocks[0].state, BlockState::Waiting { .. }));
        assert_eq!(downloads::get(&db, entry.id).unwrap().unwrap().status, DownloadStatus::Installing);
        assert_eq!(view.downloads[entry.id].phase, crate::consumer::ui_view::DownloadPhase::Fetching);

        consumer.install(entry, attempt, &db, &bus.sender());
        assert!(!store.is_available(entry));
        let succeeded = tokio::time::timeout(std::time::Duration::from_secs(2), bus.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(succeeded, AppEvent::DownloadSucceeded { model, attempt: a } if model == entry.id && a == attempt));
        assert!(store.is_available(entry));
        assert!(!staged.exists());
        assert_eq!(
            std::fs::read(handler_for(entry.format).installed_path(store.root(), entry.id)).unwrap(),
            b"verified model bytes"
        );
        assert!(crate::db::apply(&mut db, &succeeded).unwrap());
        view.apply(&succeeded);
        assert_eq!(view.blocks[0].state, BlockState::Recording { language });
        assert!(crate::db::models::is_available(&db, entry.id).unwrap());
        assert_eq!(downloads::get(&db, entry.id).unwrap().unwrap().status, DownloadStatus::Succeeded);
    }

    #[test]
    fn accepted_fetch_does_not_install_after_later_terminal_event_in_batch() {
        for cancelled in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let bus = EventBus::new();
            let mut db = Database::open(&tmp.path().join("downloads.sqlite"), bus.sender()).unwrap();
            let store = Arc::new(CacheDirStore::from_root(tmp.path().join("models")).unwrap());
            let consumer = ModelStoreConsumer::new(store.clone());
            let entry = LANGUAGES[0].live;
            let attempt = downloads::start(&mut db, entry.id).unwrap();
            let staged = stage(&consumer, &mut db, entry, attempt);
            assert!(crate::db::apply(&mut db, &AppEvent::DownloadFetched { model: entry.id, attempt }).unwrap());
            let terminal = if cancelled {
                AppEvent::DownloadCancelled { model: entry.id, attempt }
            } else {
                AppEvent::DownloadFailed { model: entry.id, attempt, error: "fetch was cancelled".into() }
            };
            assert!(crate::db::apply(&mut db, &terminal).unwrap());

            // No runtime is needed: an obsolete fetched stage must not spawn.
            consumer.install(entry, attempt, &db, &bus.sender());
            assert!(staged.is_file());
            assert!(!store.is_available(entry));
            assert_eq!(
                downloads::get(&db, entry.id).unwrap().unwrap().status,
                if cancelled { DownloadStatus::Cancelled } else { DownloadStatus::Failed }
            );
        }
    }

    #[test]
    fn superseded_fetch_cannot_install_into_the_new_attempt() {
        let tmp = tempfile::tempdir().unwrap();
        let mut bus = EventBus::new();
        let mut db = Database::open(&tmp.path().join("downloads.sqlite"), bus.sender()).unwrap();
        let store = Arc::new(CacheDirStore::from_root(tmp.path().join("models")).unwrap());
        let consumer = ModelStoreConsumer::new(store.clone());
        let entry = LANGUAGES[0].live;
        let old_attempt = downloads::start(&mut db, entry.id).unwrap();
        let old_staged = stage(&consumer, &mut db, entry, old_attempt);
        downloads::cancel(&mut db, entry.id).unwrap();
        let attempt = downloads::start(&mut db, entry.id).unwrap();
        let staged = stage(&consumer, &mut db, entry, attempt);
        bus.drain().for_each(drop);

        assert!(!crate::db::apply(&mut db, &AppEvent::DownloadFetched { model: entry.id, attempt: old_attempt }).unwrap());
        consumer.install(entry, old_attempt, &db, &bus.sender());
        assert!(old_staged.is_file());
        assert!(staged.is_file());
        assert!(!store.is_available(entry));
        assert_eq!(downloads::get(&db, entry.id).unwrap().unwrap().attempt, attempt);
        assert_eq!(downloads::get(&db, entry.id).unwrap().unwrap().status, DownloadStatus::Downloading);
        assert!(bus.drain().any(|event| matches!(event,
            AppEvent::DownloadEventRejected { rejected: "DownloadFetched", event_attempt, row_attempt: Some(row_attempt), .. }
                if event_attempt == old_attempt && row_attempt == attempt
        )));
    }

    #[tokio::test]
    async fn cancellation_after_fetch_before_blocking_install_preserves_staged_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let mut bus = EventBus::new();
        let mut db = Database::open(&tmp.path().join("downloads.sqlite"), bus.sender()).unwrap();
        let store = Arc::new(CacheDirStore::from_root(tmp.path().join("models")).unwrap());
        let consumer = ModelStoreConsumer::new(store.clone());
        let entry = LANGUAGES[0].live;
        let attempt = downloads::start(&mut db, entry.id).unwrap();
        let staged = stage(&consumer, &mut db, entry, attempt);
        assert!(crate::db::apply(&mut db, &AppEvent::DownloadFetched { model: entry.id, attempt }).unwrap());
        bus.drain().for_each(drop);

        consumer.install(entry, attempt, &db, &bus.sender());
        downloads::cancel(&mut db, entry.id).unwrap();
        bus.drain().for_each(drop);
        let cancelled = tokio::time::timeout(std::time::Duration::from_secs(2), bus.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(cancelled, AppEvent::DownloadCancelled { model, attempt: a } if model == entry.id && a == attempt));
        assert!(staged.is_file());
        assert!(!store.is_available(entry));
        assert!(crate::db::apply(&mut db, &cancelled).unwrap());
        assert_eq!(downloads::get(&db, entry.id).unwrap().unwrap().status, DownloadStatus::Cancelled);
    }
    #[test]
    fn cancellation_after_accepted_fetch_acknowledges_without_starting_install() {
        let tmp = tempfile::tempdir().unwrap();
        let mut bus = EventBus::new();
        let mut db = Database::open(&tmp.path().join("downloads.sqlite"), bus.sender()).unwrap();
        let store = Arc::new(CacheDirStore::from_root(tmp.path().join("models")).unwrap());
        let consumer = ModelStoreConsumer::new(store.clone());
        let entry = LANGUAGES[0].live;
        let attempt = downloads::start(&mut db, entry.id).unwrap();
        let staged = stage(&consumer, &mut db, entry, attempt);
        assert!(crate::db::apply(&mut db, &AppEvent::DownloadFetched { model: entry.id, attempt }).unwrap());
        downloads::cancel(&mut db, entry.id).unwrap();
        bus.drain().for_each(drop);

        consumer.install(entry, attempt, &db, &bus.sender());
        let events: Vec<_> = bus.drain().collect();
        assert_eq!(events, vec![AppEvent::DownloadCancelled { model: entry.id, attempt }]);
        assert!(crate::db::apply(&mut db, &events[0]).unwrap());
        assert_eq!(downloads::get(&db, entry.id).unwrap().unwrap().status, DownloadStatus::Cancelled);
        assert!(staged.is_file());
        assert!(!store.is_available(entry));
    }

    #[tokio::test]
    async fn discard_sweeps_staging_without_waiting_for_a_blocked_install() {
        struct BlockingStore {
            inner: CacheDirStore,
            entered: std::sync::mpsc::Sender<()>,
            release: tokio::sync::Notify,
        }

        impl ModelStore for BlockingStore {
            fn is_available(&self, entry: &ModelEntry) -> bool {
                self.inner.is_available(entry)
            }

            fn staging_path(&self, entry: &ModelEntry, attempt: u32) -> Result<std::path::PathBuf, DownloadError> {
                self.inner.staging_path(entry, attempt)
            }

            fn install(&self, entry: &ModelEntry, staged: &std::path::Path, cancel: &mut dyn CancelCheck) -> Result<(), DownloadError> {
                self.entered.send(()).unwrap();
                tokio::runtime::Handle::current().block_on(tokio::time::timeout(
                    std::time::Duration::from_secs(2), self.release.notified(),
                )).map_err(|error| DownloadError::Install(error.to_string()))?;
                std::thread::sleep(std::time::Duration::from_millis(downloads::CANCEL_PROBE_INTERVAL_MS + 5));
                if cancel.is_cancelled() {
                    return Err(DownloadError::Cancelled);
                }
                self.inner.install(entry, staged, cancel)
            }

            fn clear_staging(&self, entry: &ModelEntry) {
                self.inner.clear_staging(entry);
            }

            fn discard_inflight(&self, entry: &ModelEntry) {
                self.inner.discard_inflight(entry);
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let mut bus = EventBus::new();
        let mut db = Database::open(&tmp.path().join("downloads.sqlite"), bus.sender()).unwrap();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let store = Arc::new(BlockingStore {
            inner: CacheDirStore::from_root(tmp.path().join("models")).unwrap(),
            entered: entered_tx,
            release: tokio::sync::Notify::new(),
        });
        let consumer = ModelStoreConsumer::new(store.clone());
        let entry = LANGUAGES[0].live;
        let attempt = downloads::start(&mut db, entry.id).unwrap();
        let staged = stage(&consumer, &mut db, entry, attempt);
        assert!(crate::db::apply(&mut db, &AppEvent::DownloadFetched { model: entry.id, attempt }).unwrap());
        bus.drain().for_each(drop);
        consumer.install(entry, attempt, &db, &bus.sender());
        tokio::task::spawn_blocking(move || entered_rx.recv_timeout(std::time::Duration::from_secs(2)))
            .await.unwrap().unwrap();
        downloads::cancel(&mut db, entry.id).unwrap();
        bus.drain().for_each(drop);

        consumer.discard_inflight(entry.id);
        assert!(!staged.exists(), "sweep must finish before the installer is released");
        store.release.notify_one();
        let cancelled = tokio::time::timeout(std::time::Duration::from_secs(2), bus.recv())
            .await.unwrap().unwrap();
        assert!(matches!(cancelled, AppEvent::DownloadCancelled { model, attempt: a } if model == entry.id && a == attempt));
        assert!(!store.is_available(entry));
        assert!(crate::db::apply(&mut db, &cancelled).unwrap());
        assert_eq!(downloads::get(&db, entry.id).unwrap().unwrap().status, DownloadStatus::Cancelled);
    }
}
