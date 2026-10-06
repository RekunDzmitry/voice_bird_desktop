//! Claims accepted download requests and owns their asynchronous workers.

use std::sync::Arc;

use tokio::task::JoinHandle;

use crate::bus::{AppEvent, DownloadStatus, EventSender};
use crate::db::downloads::{CancelProbe, Claim};
use crate::db::{downloads, Database};
use crate::picker::{ModelEntry, CATALOG};
use crate::producer::download::{DownloadError, Downloader, Throttle};
use crate::transcription_models::ModelStore;

pub(super) fn truncate_error(s: &str) -> String {
    const MAX: usize = 160;
    if s.chars().count() <= MAX {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(MAX).collect();
        format!("{truncated}…")
    }
}

pub struct DownloadsConsumer {
    pub model_store: Arc<dyn ModelStore>,
    pub downloader: Arc<dyn Downloader>,
}

impl DownloadsConsumer {
    pub fn new(downloader: Arc<dyn Downloader>, model_store: Arc<dyn ModelStore>) -> Self {
        Self {
            model_store,
            downloader,
        }
    }

    /// Start or join a model download after its request passes the bus gate.
    /// The database is the single writer, so duplicate requests observe the
    /// claimed row and join the existing worker instead of spawning another.
    pub fn request(
        &self,
        entry: &'static ModelEntry,
        requested_attempt: u32,
        db: &mut Database,
        tx: &EventSender,
    ) {
        let row = match downloads::get(db, entry.id) {
            Ok(row) => row,
            Err(error) => {
                // Lookup failed before a worker or a row could be claimed.
                // A worker failure would be rejected by the attempt gate when
                // the row is absent; the pre-claim variant still reaches UI.
                tx.publish(AppEvent::DownloadClaimFailed {
                    attempt: 0,
                    model: entry.id,
                    error: truncate_error(&format!("downloads table: {error}")),
                });
                return;
            }
        };
        match downloads::decide(row.as_ref()) {
            Claim::Start { attempt } | Claim::Restart { attempt }
                if attempt == requested_attempt =>
            {
                let attempt = match downloads::start(db, entry.id) {
                    Ok(attempt) => attempt,
                    Err(error) => {
                        // Never spawn without a persisted attempt: every worker
                        // event would otherwise be rejected by the table gate.
                        tx.publish(AppEvent::DownloadClaimFailed {
                            attempt,
                            model: entry.id,
                            error: truncate_error(&format!("downloads table: {error}")),
                        });
                        return;
                    }
                };
                self.spawn(
                    entry,
                    downloads::probe(db, entry.id, attempt),
                    attempt,
                    tx.clone(),
                );
            }
            Claim::Join
                if row
                    .as_ref()
                    .is_some_and(|row| row.attempt == requested_attempt) => {}
            _ => match row {
                Some(row)
                    if row.attempt == requested_attempt
                        && matches!(
                            row.status,
                            DownloadStatus::Succeeded
                                | DownloadStatus::Failed
                                | DownloadStatus::Cancelled
                                | DownloadStatus::Interrupted
                        ) =>
                {
                    // This is a replay of the persisted lifecycle, not a SQL
                    // transition. A terminal result that overtook the request
                    // still reaches late waiters, without implicitly retrying.
                    tx.publish(AppEvent::DownloadStatusChanged {
                        model: row.model,
                        attempt: row.attempt,
                        from: Some(row.status),
                        to: row.status,
                        error: row.error,
                    });
                }
                row => {
                    let row_attempt = row.as_ref().map(|row| row.attempt);
                    tx.publish(AppEvent::DownloadEventRejected {
                        model: Arc::from(entry.id),
                        rejected: "DownloadRequested",
                        event_attempt: requested_attempt,
                        row_attempt,
                    });
                    if row_attempt.is_none() {
                        tx.publish(AppEvent::DownloadClaimFailed {
                            attempt: requested_attempt,
                            model: entry.id,
                            error: "download attempt no longer exists".to_string(),
                        });
                    }
                }
            },
        }
    }

    pub fn discard_inflight(&self, model: &str) {
        if let Some(entry) = CATALOG.iter().find(|entry| entry.id == model) {
            self.model_store.discard_inflight(entry);
        }
    }

    fn spawn(
        &self,
        entry: &'static ModelEntry,
        probe: CancelProbe,
        attempt: u32,
        tx: EventSender,
    ) -> JoinHandle<()> {
        let store = self.model_store.clone();
        let downloader = self.downloader.clone();
        tokio::spawn(async move {
            let url = entry.download_url;
            let sha = entry.download_sha256;
            let model = entry.id;
            let staged = match store.staging_path(entry, attempt) {
                Ok(p) => p,
                Err(e) => {
                    tx.publish(AppEvent::DownloadFailed {
                        attempt,
                        model,
                        error: truncate_error(&e.to_string()),
                    });
                    return;
                }
            };
            let format = entry.format;
            let mut throttle = Throttle::new();
            let mut probe = probe;
            let result = {
                let mut progress = |bytes: u64, total: Option<u64>| {
                    throttle.call(attempt, bytes, total, &tx, model);
                };
                downloader
                    .fetch(url, &staged, sha, &mut probe, &mut progress)
                    .await
            };
            match result {
                Ok(()) => {
                    if let Some(total) = throttle.last_total() {
                        throttle.finalize(attempt, total, Some(total), &tx, model);
                    }
                    if crate::transcription_models::handler_for(format).install_is_slow() {
                        tx.publish(AppEvent::DownloadInstalling { attempt, model });
                    }
                    let install_result = tokio::task::spawn_blocking(move || {
                        store.install(entry, &staged, &mut probe)
                    })
                    .await
                    .unwrap_or_else(|e| Err(DownloadError::Install(e.to_string())));
                    match install_result {
                        Ok(()) => {
                            tx.publish(AppEvent::DownloadSucceeded { attempt, model });
                        }
                        Err(DownloadError::Cancelled) => {
                            tx.publish(AppEvent::DownloadCancelled { attempt, model });
                        }
                        Err(e) => tx.publish(AppEvent::DownloadFailed {
                            attempt,
                            model,
                            error: truncate_error(&e.to_string()),
                        }),
                    }
                }
                Err(DownloadError::Cancelled) => {
                    tx.publish(AppEvent::DownloadCancelled { attempt, model });
                }
                Err(e) => tx.publish(AppEvent::DownloadFailed {
                    attempt,
                    model,
                    error: truncate_error(&e.to_string()),
                }),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Outcome;
    use std::sync::atomic::Ordering;

    // Regression for the silent `unwrap_or(attempt)` bug: when
    // `downloads::start` fails (disk full, lock timeout, write
    // error), the consumer must publish `DownloadClaimFailed` with
    // the underlying error and return without spawning a worker.
    // Otherwise the worker would start without a row in the table,
    // the attempt gate would reject every event it publishes, and
    // the UI would be stuck on `DownloadRequested` forever.
    //
    // Drive the failure with a `Database` whose writer connection
    // is opened `SQLITE_OPEN_READ_ONLY`: every `execute()` write
    // returns `SQLITE_READONLY`. This mirrors the production
    // failure mode without an OS-level chmod dance and without
    // consuming the `Connection` via `close` (which would prevent
    // us from embedding it back into `Database::conn`).
    #[test]
    fn request_surfaces_db_write_failure_without_starting_worker() {
        use crate::bus::EventBus;
        use crate::db::Database;
        use crate::picker::{ModelEntry, CATALOG};

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("downloads.sqlite");
        // Bootstrap: open the file, run the migration, drop the
        let bootstrap = rusqlite::Connection::open(&path).unwrap();
        bootstrap
            .execute_batch(<crate::db::downloads::DownloadsTable as crate::db::Table>::DEFINITION)
            .unwrap();
        bootstrap.close().map_err(|(_, e)| e).unwrap();
        // Re-open with `SQLITE_OPEN_READ_ONLY`. Every subsequent
        // write from `downloads::start` returns `SQLITE_READONLY`.
        let readonly = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .expect("open read-only");
        let mut bus = EventBus::new();
        let tiny: &'static ModelEntry = &CATALOG[5];
        let store: Arc<dyn crate::transcription_models::ModelStore> = Arc::new(
            crate::testing::FixtureStore::new(tmp.path().to_path_buf(), &[]),
        );
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut db = Database::from_connection_for_test(readonly, path.clone(), bus.sender());
        let downloader: Arc<dyn Downloader> = Arc::new(crate::testing::FixtureDownloader::new(
            Vec::new(),
            Outcome::Ok,
            Arc::clone(&calls),
        ));

        DownloadsConsumer::new(downloader, store).request(tiny, 1, &mut db, &bus.sender());

        // The fetcher must not have been touched — the failure
        // happens at the row-write step, before `spawn` runs.
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "worker must not spawn when downloads::start fails"
        );

        // A pre-claim failure remains accepted even when no attempt row exists.
        let events: Vec<AppEvent> = bus.drain().collect();
        let failed = events.iter().find_map(|ev| match ev {
            AppEvent::DownloadClaimFailed { attempt, model, .. } => Some((*attempt, *model)),
            _ => None,
        });
        let (attempt, model) = failed.expect("DownloadClaimFailed must be published");
        assert_eq!(attempt, 1);
        assert_eq!(model, tiny.id);
        assert!(downloads::apply(&mut db, &events[0]).unwrap());
    }

    // End-to-end regression: a failed downloads-table claim must pass through
    // the bus/table/state pipeline and fail the waiting language block.
    #[test]
    fn db_write_failure_surfaces_to_ui_via_full_drain_apply_flow() {
        use crate::bus::EventBus;
        use crate::db::Database;
        use crate::language::LANGUAGES;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("downloads.sqlite");
        // A failed row at attempt 3 proposes retry attempt 4. Re-opening
        // read-only prevents the claim; its failure and derived audit event
        // must retain attempt 4 while the waiting language block fails.
        {
            let bootstrap = rusqlite::Connection::open(&path).unwrap();
            bootstrap
                .execute_batch(
                    <crate::db::downloads::DownloadsTable as crate::db::Table>::DEFINITION,
                )
                .unwrap();
            bootstrap
                .execute_batch(
                    <crate::db::models::ModelsTable as crate::db::Table>::DEFINITION,
                )
                .unwrap();
            for model in LANGUAGES[0].models() {
                bootstrap
                    .execute(
                        "INSERT INTO models (model, available) VALUES (?1, 0)",
                        [model.id],
                    )
                    .unwrap();
            }
            bootstrap
                .execute(
                    "INSERT INTO downloads (model, attempt, status, error, created_at, updated_at) \
                     VALUES (?1, ?2, ?3, NULL, ?4, ?4)",
                    rusqlite::params![
                        LANGUAGES[0].live.id,
                        3u32,
                        "Failed",
                        "2026-09-29T00:00:00.000Z",
                    ],
                )
                .unwrap();
            bootstrap.close().map_err(|(_, e)| e).unwrap();
        }
        let readonly = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .expect("open read-only");
        let mut bus = EventBus::new();
        let mut db = Database::from_connection_for_test(readonly, path.clone(), bus.sender());
        let mut state = crate::consumer::UiView::default();
        state.apply(&AppEvent::AddBlock);
        let _ = crate::db::downloads::apply(&mut db, &AppEvent::AddBlock);

        let language = &LANGUAGES[0];
        crate::producer::input::resolve_intent(
            crate::input::Intent::Confirm,
            &state,
            &mut db,
            &bus.sender(),
        );
        let events: Vec<AppEvent> = bus.drain().collect();

        // Language replies and accepted download requests run in separate passes.
        let store: Arc<dyn crate::transcription_models::ModelStore> = Arc::new(
            crate::testing::FixtureStore::new(tmp.path().to_path_buf(), &[]),
        );
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let downloader: Arc<dyn Downloader> = Arc::new(crate::testing::FixtureDownloader::new(
            Vec::new(),
            Outcome::Ok,
            Arc::clone(&calls),
        ));
        let language_consumer = crate::consumer::language::LanguageConsumer;
        let downloads_consumer = DownloadsConsumer::new(downloader, store);
        for event in &events {
            if downloads::apply(&mut db, event).unwrap() {
                state.apply(event);
                if let AppEvent::BeginLanguage { block, language, .. } = event {
                    language_consumer.begin(*block, language, &db, &bus.sender());
                }
            }
        }
        let mut drained = Vec::new();
        loop {
            let events: Vec<AppEvent> = bus.drain().collect();
            if events.is_empty() {
                break;
            }
            for event in &events {
                if downloads::apply(&mut db, event).unwrap() {
                    state.apply(event);
                    if let AppEvent::DownloadRequested { model, attempt } = event {
                        downloads_consumer.request(model, *attempt, &mut db, &bus.sender());
                    }
                }
            }
            drained.extend(events);
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "worker must not spawn when downloads::start fails"
        );

        let failure_error = drained
            .iter()
            .find_map(|event| match event {
                AppEvent::DownloadClaimFailed { model, error, .. }
                    if *model == language.live.id =>
                {
                    Some(error)
                }
                _ => None,
            })
            .expect("live model claim must fail");
        let block = state
            .blocks
            .first()
            .expect("block must still exist after the failure");
        match &block.state {
            crate::consumer::ui_view::BlockState::Failed {
                language: failed_language,
                error,
                ..
            } => {
                assert_eq!(*failed_language, language);
                assert_eq!(error, failure_error);
            }
            other => panic!(
                "block must be Failed after a DB-write failure surfaces through the bus; got {other:?}"
            ),
        }
        // Derived audit event must forward the orchestrator's
        // predicted attempt (4 in this scenario — `decide`
        // proposed `attempt: 4` for the Failed row at attempt 3
        // pre-populated above). Without the forward, the log
        // records `DownloadClaimFailed { attempt: 4 }` followed
        // by `DownloadStatusChanged { attempt: 0 }`, breaking
        // attempt correlation and contradicting the table-side
        // invariant that every status change carries the
        // orchestrator's claim attempt.
        let status_changes: Vec<_> = drained
            .iter()
            .filter_map(|ev| match ev {
                AppEvent::DownloadStatusChanged { attempt, to, .. } => Some((*attempt, *to)),
                _ => None,
            })
            .collect();
        assert!(
            status_changes
                .iter()
                .any(|(a, t)| *a == 4 && *t == crate::bus::DownloadStatus::Failed),
            "DownloadStatusChanged must forward attempt=4 (Failed); got {status_changes:?}"
        );
    }

    fn fixture_consumer(
        root: &std::path::Path,
    ) -> (DownloadsConsumer, Arc<std::sync::atomic::AtomicUsize>) {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let consumer = DownloadsConsumer::new(
            Arc::new(crate::testing::FixtureDownloader::new(
                Vec::new(),
                Outcome::Ok,
                calls.clone(),
            )),
            Arc::new(crate::testing::FixtureStore::new(root.to_path_buf(), &[])),
        );
        (consumer, calls)
    }

    fn waiting_view() -> crate::consumer::UiView {
        let mut view = crate::consumer::UiView::default();
        view.apply(&AppEvent::AddBlock);
        view.apply(&AppEvent::LanguageSelected {
            block: 1,
            language: &crate::language::LANGUAGES[0],
            pending: vec![crate::language::LANGUAGES[0].live.id],
        });
        view
    }

    #[test]
    fn request_lookup_failure_reaches_waiter_without_an_attempt_row() {
        let tmp = tempfile::tempdir().unwrap();
        let mut bus = crate::bus::EventBus::new();
        let mut db = Database::from_connection_for_test(
            rusqlite::Connection::open_in_memory().unwrap(),
            tmp.path().join("missing-schema.sqlite"),
            bus.sender(),
        );
        let (consumer, calls) = fixture_consumer(tmp.path());
        let model = crate::language::LANGUAGES[0].live;
        let mut view = waiting_view();
        view.apply(&AppEvent::DownloadRequested { model, attempt: 1 });

        consumer.request(model, 1, &mut db, &bus.sender());
        let events: Vec<_> = bus.drain().collect();
        for event in &events {
            if downloads::apply(&mut db, event).unwrap() {
                view.apply(event);
            }
        }
        assert!(matches!(
            &view.blocks[0].state,
            crate::consumer::ui_view::BlockState::Failed { .. }
        ));
        assert!(!view.downloads.contains_key(model.id));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(events.iter().any(|event| matches!(
            event,
            AppEvent::DownloadClaimFailed { attempt: 0, model: failed_model, .. }
                if *failed_model == model.id
        )));
    }

    #[test]
    fn missing_attempt_request_does_not_create_a_worker_or_strand_a_waiter() {
        let tmp = tempfile::tempdir().unwrap();
        let mut bus = crate::bus::EventBus::new();
        let mut db = Database::open(&tmp.path().join("downloads.sqlite"), bus.sender()).unwrap();
        let (consumer, calls) = fixture_consumer(tmp.path());
        let model = crate::language::LANGUAGES[0].live;
        let mut view = waiting_view();
        view.apply(&AppEvent::DownloadRequested { model, attempt: 2 });

        consumer.request(model, 2, &mut db, &bus.sender());
        for event in bus.drain() {
            if downloads::apply(&mut db, &event).unwrap() {
                view.apply(&event);
            }
        }
        assert!(matches!(
            &view.blocks[0].state,
            crate::consumer::ui_view::BlockState::Failed { .. }
        ));
        assert!(!view.downloads.contains_key(model.id));
        assert!(downloads::get(&db, model.id).unwrap().is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn superseded_request_does_not_replay_a_newer_failure_into_current_waiters() {
        let tmp = tempfile::tempdir().unwrap();
        let mut bus = crate::bus::EventBus::new();
        let mut db = Database::open(&tmp.path().join("downloads.sqlite"), bus.sender()).unwrap();
        let (consumer, calls) = fixture_consumer(tmp.path());
        let model = crate::language::LANGUAGES[0].live;
        downloads::start(&mut db, model.id).unwrap();
        let attempt = downloads::start(&mut db, model.id).unwrap();
        downloads::apply(
            &mut db,
            &AppEvent::DownloadFailed {
                attempt,
                model: model.id,
                error: "previous retry failed".to_string(),
            },
        )
        .unwrap();
        bus.drain().for_each(drop);
        let mut view = waiting_view();

        consumer.request(model, attempt - 1, &mut db, &bus.sender());
        let events: Vec<_> = bus.drain().collect();
        for event in &events {
            if downloads::apply(&mut db, event).unwrap() {
                view.apply(event);
            }
        }
        assert!(matches!(
            &view.blocks[0].state,
            crate::consumer::ui_view::BlockState::Waiting { pending, .. }
                if pending.as_slice() == [model.id]
        ));
        assert!(events.iter().any(|event| matches!(
            event,
            AppEvent::DownloadEventRejected {
                event_attempt,
                row_attempt: Some(row_attempt),
                ..
            } if *event_attempt == attempt - 1 && *row_attempt == attempt
        )));
        assert!(!events.iter().any(|event| matches!(
            event,
            AppEvent::DownloadStatusChanged { .. }
        )));
        let row = downloads::get(&db, model.id).unwrap().unwrap();
        assert_eq!(row.attempt, attempt);
        assert_eq!(row.status, DownloadStatus::Failed);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}
