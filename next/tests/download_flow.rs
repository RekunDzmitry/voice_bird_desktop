//! End-to-end language download flow tests. No test touches the network.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::watch;
use tokio::time::timeout;

use voice_bird_next::bus::{AppEvent, DownloadStatus, EventBus, EventSender};
use voice_bird_next::consumer::ui_view::BlockState;
use voice_bird_next::consumer::{Consumer, UiView};
use voice_bird_next::db::downloads::CancelCheck;
use voice_bird_next::db::{downloads, Database};
use voice_bird_next::input::Intent;
use voice_bird_next::language::{LanguageProfile, LANGUAGES};
use voice_bird_next::producer::download::{DownloadError, Downloader};
use voice_bird_next::producer::input::resolve_intent;
use voice_bird_next::producer::model_watch::ModelWatcher;
use voice_bird_next::producer::sources::{FunnelStep, NoSources};
use voice_bird_next::testing::{FixtureDownloader, FixtureStore, Outcome};
use voice_bird_next::transcription_models::{CacheDirStore, ModelStore};

fn english() -> &'static LanguageProfile {
    &LANGUAGES[0]
}
struct WritingDownloader {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Downloader for WritingDownloader {
    async fn fetch(
        &self,
        _url: &str,
        staged: &Path,
        _expected_sha: &str,
        cancel: &mut (dyn CancelCheck + Send),
        progress: &mut (dyn FnMut(u64, Option<u64>) + Send),
    ) -> Result<(), DownloadError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if cancel.is_cancelled() {
            return Err(DownloadError::Cancelled);
        }
        tokio::fs::write(staged, b"fixture model").await
            .map_err(|error| DownloadError::Io(error.to_string()))?;
        progress(13, Some(13));
        Ok(())
    }
}

struct FailDuringAvailabilityCheck {
    inner: FixtureStore,
    tx: EventSender,
    model: &'static str,
    emitted: AtomicBool,
}

impl ModelStore for FailDuringAvailabilityCheck {
    fn is_available(&self, entry: &voice_bird_next::picker::ModelEntry) -> bool {
        if entry.id == self.model && !self.emitted.swap(true, Ordering::SeqCst) {
            self.tx.publish(AppEvent::DownloadFailed {
                attempt: 1,
                model: entry.id,
                error: "HTTP 404".to_string(),
            });
        }
        self.inner.is_available(entry)
    }

    fn staging_path(
        &self,
        entry: &voice_bird_next::picker::ModelEntry,
        attempt: u32,
    ) -> Result<std::path::PathBuf, DownloadError> {
        self.inner.staging_path(entry, attempt)
    }

    fn install(
        &self,
        entry: &voice_bird_next::picker::ModelEntry,
        staged: &Path,
        cancel: &mut dyn CancelCheck,
    ) -> Result<(), DownloadError> {
        self.inner.install(entry, staged, cancel)
    }

    fn clear_staging(&self, entry: &voice_bird_next::picker::ModelEntry) {
        self.inner.clear_staging(entry);
    }

    fn discard_inflight(&self, entry: &voice_bird_next::picker::ModelEntry) {
        self.inner.discard_inflight(entry);
    }
}

struct DownloadsHandle {
    db: Database,
    _tmp: tempfile::TempDir,
}

fn downloads_with(bus: &EventBus) -> DownloadsHandle {
    let tmp = tempfile::tempdir().unwrap();
    let db = Database::open(&tmp.path().join("downloads.sqlite"), bus.sender()).unwrap();
    DownloadsHandle { db, _tmp: tmp }
}

fn consume_batch(
    events: Vec<AppEvent>,
    consumer: &mut Consumer,
    db: &mut Database,
    tx: &EventSender,
) -> Vec<AppEvent> {
    let accepted: Vec<_> = events
        .into_iter()
        .filter(|event| voice_bird_next::db::apply(db, event).unwrap())
        .collect();
    consumer.consume(&accepted, db, tx);
    accepted
}

// Consume follow-up events too: database transitions and producer commands use
// the same gate and projection as the original batch.
fn drain_consume(
    bus: &mut EventBus,
    consumer: &mut Consumer,
    db: &mut Database,
) -> Vec<AppEvent> {
    let tx = bus.sender();
    let mut accepted = Vec::new();
    loop {
        let events: Vec<_> = bus.drain().collect();
        if events.is_empty() {
            return accepted;
        }
        accepted.extend(consume_batch(events, consumer, db, &tx));
    }
}

fn confirm_and_consume(
    bus: &mut EventBus,
    consumer: &mut Consumer,
    db: &mut Database,
) -> Vec<AppEvent> {
    resolve_intent(Intent::Confirm, &consumer.view, db, &bus.sender());
    drain_consume(bus, consumer, db)
}

fn retry_and_consume(bus: &mut EventBus, consumer: &mut Consumer, db: &mut Database) {
    resolve_intent(Intent::Retry, &consumer.view, db, &bus.sender());
    drain_consume(bus, consumer, db);
}

async fn settle_until(
    bus: &mut EventBus,
    consumer: &mut Consumer,
    db: &mut Database,
    predicate: impl Fn(&UiView, &Database) -> bool,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        drain_consume(bus, consumer, db);
        if predicate(&consumer.view, db) {
            return;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let event = timeout(remaining, bus.recv())
            .await
            .unwrap_or_else(|_| panic!("view did not settle: {:?}", consumer.view.blocks))
            .expect("event bus closed before the view settled");
        let mut events = vec![event];
        events.extend(bus.drain());
        consume_batch(events, consumer, db, &bus.sender());
    }
}

#[tokio::test]
async fn language_with_both_models_cached_records_immediately() {
    let tmp = tempfile::tempdir().unwrap();
    let present: Vec<_> = english().models().map(|model| model.id).to_vec();
    let store: Arc<dyn ModelStore> =
        Arc::new(FixtureStore::new(tmp.path().to_path_buf(), &present));
    let calls = Arc::new(AtomicUsize::new(0));
    let downloader: Arc<dyn Downloader> = Arc::new(FixtureDownloader::new(
        Vec::new(),
        Outcome::Ok,
        calls.clone(),
    ));
    let mut consumer = Consumer::new(downloader, store, Arc::new(NoSources));
    let mut bus = EventBus::new();
    let mut handle = downloads_with(&bus);
    consumer.view.apply(&AppEvent::AddBlock);

    let events = confirm_and_consume(&mut bus, &mut consumer, &mut handle.db);

    assert!(events.iter().any(|event| matches!(
        event,
        AppEvent::LanguageSelected { block: 1, pending, .. } if pending.is_empty()
    )));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(matches!(
        consumer.view.blocks[0].state,
        BlockState::Recording { language } if language == english()
    ));
}

#[tokio::test]
async fn language_waits_for_both_models_then_records() {
    let mut state = UiView::default();
    state.apply(&AppEvent::AddBlock);
    state.apply(&AppEvent::LanguageSelected {
        block: 1,
        language: english(),
        pending: english().models().map(|model| model.id).to_vec(),
    });

    state.apply(&AppEvent::DownloadSucceeded {
        attempt: 1,
        model: english().live.id,
    });
    assert!(matches!(
        &state.blocks[0].state,
        BlockState::Waiting { pending, .. }
            if pending.as_slice() == [english().refine.id]
    ));

    state.apply(&AppEvent::DownloadSucceeded {
        attempt: 1,
        model: english().refine.id,
    });
    assert!(matches!(
        state.blocks[0].state,
        BlockState::Recording { language } if language == english()
    ));
}

#[tokio::test]
async fn language_with_one_cached_model_downloads_only_the_other() {
    let tmp = tempfile::tempdir().unwrap();
    let store_concrete = Arc::new(FixtureStore::new(
        tmp.path().to_path_buf(),
        &[english().live.id],
    ));
    let store: Arc<dyn ModelStore> = store_concrete.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let downloader: Arc<dyn Downloader> = Arc::new(FixtureDownloader::new(
        vec![1; 64],
        Outcome::Ok,
        calls.clone(),
    ));
    let mut consumer = Consumer::new(downloader, store, Arc::new(NoSources));
    let mut bus = EventBus::new();
    let mut handle = downloads_with(&bus);
    consumer.view.apply(&AppEvent::AddBlock);

    let events = confirm_and_consume(&mut bus, &mut consumer, &mut handle.db);
    let selection = events.iter().position(|event| matches!(
        event,
        AppEvent::LanguageSelected { block: 1, pending, .. }
            if pending.as_slice() == [english().refine.id]
    )).unwrap();
    let request = events.iter().position(|event| matches!(
        event, AppEvent::DownloadRequested(model) if model.id == english().refine.id
    )).unwrap();
    assert!(selection < request);
    assert_eq!(consumer.view.blocks[0].pending_models(), &[english().refine.id]);
    settle_until(&mut bus, &mut consumer, &mut handle.db, |view, _| {
        matches!(view.blocks[0].state, BlockState::Recording { .. })
    }).await;

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(store_concrete.is_available(english().live));
    assert!(store_concrete.is_available(english().refine));
}

#[tokio::test]
async fn refine_model_failure_fails_block_and_retry_downloads_only_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let store_concrete = Arc::new(FixtureStore::new(
        tmp.path().to_path_buf(),
        &[english().live.id],
    ));
    let store: Arc<dyn ModelStore> = store_concrete.clone();
    let failed_calls = Arc::new(AtomicUsize::new(0));
    let failing: Arc<dyn Downloader> = Arc::new(FixtureDownloader::new(
        vec![1; 64],
        Outcome::ShaMismatch,
        failed_calls.clone(),
    ));
    let mut consumer = Consumer::new(failing, store.clone(), Arc::new(NoSources));
    let mut bus = EventBus::new();
    let mut handle = downloads_with(&bus);
    consumer.view.apply(&AppEvent::AddBlock);

    confirm_and_consume(&mut bus, &mut consumer, &mut handle.db);
    settle_until(&mut bus, &mut consumer, &mut handle.db, |view, _| {
        matches!(view.blocks[0].state, BlockState::Failed { .. })
    }).await;
    assert_eq!(failed_calls.load(Ordering::SeqCst), 1);
    let failed = downloads::get(&handle.db, english().refine.id).unwrap().unwrap();
    assert_eq!(failed.attempt, 1);
    assert_eq!(failed.status, DownloadStatus::Failed);

    let retry_calls = Arc::new(AtomicUsize::new(0));
    let successful: Arc<dyn Downloader> = Arc::new(FixtureDownloader::new(
        vec![2; 64],
        Outcome::Ok,
        retry_calls.clone(),
    ));
    consumer.producers.downloads = successful;
    retry_and_consume(&mut bus, &mut consumer, &mut handle.db);
    settle_until(&mut bus, &mut consumer, &mut handle.db, |view, _| {
        matches!(view.blocks[0].state, BlockState::Recording { .. })
    }).await;

    assert_eq!(retry_calls.load(Ordering::SeqCst), 1);
    assert!(store_concrete.is_available(english().refine));
    let retried = downloads::get(&handle.db, english().refine.id).unwrap().unwrap();
    assert_eq!(retried.attempt, 2);
    assert_eq!(retried.status, DownloadStatus::Succeeded);
}

#[tokio::test]
async fn claim_failure_on_one_model_fails_block() {
    let mut bus = EventBus::new();
    let mut handle = downloads_with(&bus);
    let mut consumer = source_consumer(&handle);
    consumer.view.apply(&AppEvent::AddBlock);
    let tx = bus.sender();
    tx.publish(AppEvent::LanguageSelected {
        block: 1,
        language: english(),
        pending: vec![english().refine.id],
    });
    tx.publish(AppEvent::DownloadRequested(english().refine));
    tx.publish(AppEvent::DownloadClaimFailed {
        attempt: 1,
        model: english().refine.id,
        error: "database locked".to_string(),
    });
    drain_consume(&mut bus, &mut consumer, &mut handle.db);

    assert!(matches!(
        &consumer.view.blocks[0].state,
        BlockState::Failed {
            language, error, ..
        } if *language == english() && error == "database locked"
    ));
}

#[tokio::test]
async fn failure_between_availability_check_and_selection_reaches_late_waiter() {
    let tmp = tempfile::tempdir().unwrap();
    let mut bus = EventBus::new();
    let mut handle = downloads_with(&bus);
    let mut consumer = source_consumer(&handle);

    downloads::start(&mut handle.db, english().live.id).unwrap();
    drain_consume(&mut bus, &mut consumer, &mut handle.db);

    consumer.view.apply(&AppEvent::AddBlock);
    consumer.view.apply(&AppEvent::LanguageSelected {
        block: 1,
        language: english(),
        pending: vec![english().live.id],
    });
    consumer.view.apply(&AppEvent::AddBlock);

    let store: Arc<dyn ModelStore> = Arc::new(FailDuringAvailabilityCheck {
        inner: FixtureStore::new(tmp.path().to_path_buf(), &[english().refine.id]),
        tx: bus.sender(),
        model: english().live.id,
        emitted: AtomicBool::new(false),
    });
    let calls = Arc::new(AtomicUsize::new(0));
    let downloader: Arc<dyn Downloader> = Arc::new(FixtureDownloader::new(
        Vec::new(),
        Outcome::Ok,
        calls.clone(),
    ));

    consumer.producers.model_store = store;
    consumer.producers.downloads = downloader;
    bus.sender().publish(AppEvent::BeginLanguage {
        block: 2,
        language: english(),
        source_rev: None,
    });

    let first = drain_consume(&mut bus, &mut consumer, &mut handle.db);
    assert!(first.iter().any(|event| matches!(
        event,
        AppEvent::DownloadFailed { model, .. } if *model == english().live.id
    )));
    let failure = first.iter().position(|event| matches!(event,
        AppEvent::DownloadFailed { model, .. } if *model == english().live.id
    )).unwrap();
    let selection = first.iter().position(|event| matches!(event,
        AppEvent::LanguageSelected { block: 2, pending, .. }
            if pending.as_slice() == [english().live.id]
    )).unwrap();
    let terminal = first.iter().position(|event| matches!(event,
        AppEvent::DownloadStatusChanged { model, to: DownloadStatus::Failed, .. }
            if model.as_ref() == english().live.id
    )).unwrap();
    assert!(failure < selection && selection < terminal);
    assert_eq!(
        downloads::get(&handle.db, english().live.id)
            .unwrap()
            .unwrap()
            .status,
        DownloadStatus::Failed
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "the stale Downloading row must make the second block join"
    );

    assert!(first.iter().any(|event| matches!(
        event,
        AppEvent::DownloadStatusChanged {
            model,
            to: DownloadStatus::Failed,
            ..
        } if model.as_ref() == english().live.id
    )));
    assert!(consumer.view.blocks.iter().all(|block| matches!(
        &block.state, BlockState::Failed { error, .. } if error == "HTTP 404"
    )));
}

#[tokio::test]
async fn two_blocks_same_language_share_downloads() {
    let tmp = tempfile::tempdir().unwrap();
    let store: Arc<dyn ModelStore> = Arc::new(FixtureStore::new(tmp.path().to_path_buf(), &[]));
    let calls = Arc::new(AtomicUsize::new(0));
    let (gate, receiver) = watch::channel(false);
    let downloader: Arc<dyn Downloader> = Arc::new(GatedDownloader {
        inner: FixtureDownloader::new(vec![1; 128 * 1024], Outcome::Ok, calls.clone()),
        gate: receiver,
    });
    let mut consumer = Consumer::new(downloader, store, Arc::new(NoSources));
    let mut bus = EventBus::new();
    let mut handle = downloads_with(&bus);

    consumer.view.apply(&AppEvent::AddBlock);
    confirm_and_consume(&mut bus, &mut consumer, &mut handle.db);
    drain_consume(&mut bus, &mut consumer, &mut handle.db);
    consumer.view.apply(&AppEvent::AddBlock);
    confirm_and_consume(&mut bus, &mut consumer, &mut handle.db);
    drain_consume(&mut bus, &mut consumer, &mut handle.db);
    for model in english().models() {
        let row = downloads::get(&handle.db, model.id).unwrap().unwrap();
        assert_eq!(row.attempt, 1);
        assert_eq!(row.status, DownloadStatus::Downloading);
    }
    assert!(consumer.view
        .blocks
        .iter()
        .all(|block| matches!(block.state, BlockState::Waiting { .. })));
    gate.send_replace(true);
    settle_until(&mut bus, &mut consumer, &mut handle.db, |view, _| {
        view.blocks.iter().all(|block| matches!(block.state, BlockState::Recording { .. }))
    }).await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    for model in english().models() {
        let row = downloads::get(&handle.db, model.id).unwrap().unwrap();
        assert_eq!(row.attempt, 1);
        assert_eq!(row.status, DownloadStatus::Succeeded);
    }
}

#[tokio::test]
async fn closing_last_waiter_cancels_both_pending_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let store: Arc<dyn ModelStore> = Arc::new(FixtureStore::new(tmp.path().to_path_buf(), &[]));
    let (gate, receiver) = watch::channel(false);
    let downloader: Arc<dyn Downloader> = Arc::new(GatedDownloader {
        inner: FixtureDownloader::new(
            vec![1; 128 * 1024], Outcome::Ok, Arc::new(AtomicUsize::new(0)),
        ),
        gate: receiver,
    });
    let mut consumer = Consumer::new(downloader, store, Arc::new(NoSources));
    let mut bus = EventBus::new();
    let mut handle = downloads_with(&bus);
    consumer.view.apply(&AppEvent::AddBlock);
    confirm_and_consume(&mut bus, &mut consumer, &mut handle.db);
    drain_consume(&mut bus, &mut consumer, &mut handle.db);

    resolve_intent(Intent::BlockClosed, &consumer.view, &mut handle.db, &bus.sender());
    drain_consume(&mut bus, &mut consumer, &mut handle.db);
    assert!(consumer.view.blocks.is_empty());

    for model in english().models() {
        let row = downloads::get(&handle.db, model.id).unwrap().unwrap();
        assert_eq!(row.status, DownloadStatus::Cancelling);
    }
    gate.send_replace(true);
    settle_until(&mut bus, &mut consumer, &mut handle.db, |_, db| {
        english().models().iter().all(|model| {
            downloads::get(db, model.id).unwrap().unwrap().status == DownloadStatus::Cancelled
        })
    }).await;
    for model in english().models() {
        assert!(!consumer.producers.model_store.is_available(model));
        assert!(!tmp.path().join(format!("{}.1.part", model.id)).exists());
    }
}

#[tokio::test]
async fn language_selected_targets_block_id_not_focus() {
    let tmp = tempfile::tempdir().unwrap();
    let present: Vec<_> = english().models().map(|model| model.id).to_vec();
    let store: Arc<dyn ModelStore> =
        Arc::new(FixtureStore::new(tmp.path().to_path_buf(), &present));
    let downloader: Arc<dyn Downloader> = Arc::new(FixtureDownloader::new(
        Vec::new(),
        Outcome::Ok,
        Arc::new(AtomicUsize::new(0)),
    ));
    let mut consumer = Consumer::new(downloader, store, Arc::new(NoSources));
    let mut bus = EventBus::new();
    let mut handle = downloads_with(&bus);
    consumer.view.apply(&AppEvent::AddBlock);
    consumer.view.apply(&AppEvent::AddBlock);
    assert_eq!(consumer.view.focus, 1);

    bus.sender().publish(AppEvent::BeginLanguage {
        block: 1,
        language: english(),
        source_rev: None,
    });
    drain_consume(&mut bus, &mut consumer, &mut handle.db);


    assert!(matches!(
        consumer.view.blocks[0].state,
        BlockState::Recording { .. }
    ));
    assert!(matches!(consumer.view.blocks[1].state, BlockState::Picking(_)));
    assert_eq!(consumer.view.focus, 1);
}

#[tokio::test]
async fn second_session_is_warm() {
    let tmp = tempfile::tempdir().unwrap();
    let models_root = tmp.path().join("models");
    let store: Arc<dyn ModelStore> =
        Arc::new(CacheDirStore::from_root(models_root.clone()).unwrap());
    let first_calls = Arc::new(AtomicUsize::new(0));
    let downloader: Arc<dyn Downloader> = Arc::new(WritingDownloader {
        calls: first_calls.clone(),
    });
    let mut consumer = Consumer::new(downloader, store, Arc::new(NoSources));
    let mut first_bus = EventBus::new();
    let mut first_db = downloads_with(&first_bus);
    consumer.view.apply(&AppEvent::AddBlock);
    confirm_and_consume(
        &mut first_bus,
        &mut consumer,
        &mut first_db.db,
    );
    settle_until(
        &mut first_bus,
        &mut consumer,
        &mut first_db.db,
        |view, _| matches!(view.blocks[0].state, BlockState::Recording { .. }),
    ).await;
    assert_eq!(first_calls.load(Ordering::SeqCst), 2);

    let second_store: Arc<dyn ModelStore> =
        Arc::new(CacheDirStore::from_root(models_root).unwrap());
    let second_calls = Arc::new(AtomicUsize::new(0));
    let second_downloader: Arc<dyn Downloader> = Arc::new(FixtureDownloader::new(
        Vec::new(),
        Outcome::Ok,
        second_calls.clone(),
    ));
    let mut second_consumer = Consumer::new(second_downloader, second_store, Arc::new(NoSources));
    let mut second_bus = EventBus::new();
    let mut second_db = downloads_with(&second_bus);
    second_consumer.view.apply(&AppEvent::AddBlock);
    let events = confirm_and_consume(
        &mut second_bus,
        &mut second_consumer,
        &mut second_db.db,
    );

    assert!(events.iter().any(|event| matches!(
        event,
        AppEvent::LanguageSelected { pending, .. } if pending.is_empty()
    )));
    assert_eq!(second_calls.load(Ordering::SeqCst), 0);
    assert!(matches!(
        second_consumer.view.blocks[0].state,
        BlockState::Recording { .. }
    ));
}

#[tokio::test]
async fn cleanup_preserves_installed_models() {
    let tmp = tempfile::tempdir().unwrap();
    let store_concrete = Arc::new(CacheDirStore::from_root(tmp.path().to_path_buf()).unwrap());
    for model in english().models() {
        std::fs::write(tmp.path().join(format!("{}.gguf", model.id)), b"installed").unwrap();
        std::fs::write(
            tmp.path().join(format!("{}.1.gguf.part", model.id)),
            b"partial",
        )
        .unwrap();
        std::fs::create_dir(tmp.path().join(format!("{}.1.tmp", model.id))).unwrap();
    }

    let store: Arc<dyn ModelStore> = store_concrete.clone();
    let downloader: Arc<dyn Downloader> = Arc::new(FixtureDownloader::new(
        Vec::new(),
        Outcome::Ok,
        Arc::new(AtomicUsize::new(0)),
    ));
    let mut consumer = Consumer::new(downloader, store, Arc::new(NoSources));
    let mut bus = EventBus::new();
    let tx = bus.sender();
    let mut handle = downloads_with(&bus);
    for model in english().models() {
        downloads::start(&mut handle.db, model.id).unwrap();
    }

    let active = downloads::active(&handle.db).unwrap();
    for row in &active {
        downloads::cancel(&mut handle.db, row.model.as_ref()).unwrap();
        tx.publish(AppEvent::DiscardInflight {
            model: row.model.clone(),
        });
    }
    drain_consume(&mut bus, &mut consumer, &mut handle.db);


    for model in english().models() {
        assert!(store_concrete.is_available(model));
        assert!(!tmp
            .path()
            .join(format!("{}.1.gguf.part", model.id))
            .exists());
        assert!(!tmp.path().join(format!("{}.1.tmp", model.id)).exists());
        assert_eq!(
            downloads::get(&handle.db, model.id).unwrap().unwrap().status,
            DownloadStatus::Cancelling
        );
    }
}

#[tokio::test]
async fn model_dropped_while_recording_redownloads_and_resumes() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(FixtureStore::new(
        tmp.path().to_path_buf(),
        &english().models().map(|model| model.id),
    ));
    let calls = Arc::new(AtomicUsize::new(0));
    let mut consumer = Consumer::new(
        Arc::new(FixtureDownloader::new(
            vec![1; 64],
            Outcome::Ok,
            calls.clone(),
        )),
        store.clone(),
        Arc::new(NoSources),
    );
    let watcher = ModelWatcher::new(store.clone());
    let mut bus = EventBus::new();
    let mut handle = downloads_with(&bus);
    consumer.view.apply(&AppEvent::AddBlock);
    confirm_and_consume(&mut bus, &mut consumer, &mut handle.db);
    drain_consume(&mut bus, &mut consumer, &mut handle.db);
    assert!(matches!(consumer.view.blocks[0].state, BlockState::Recording { .. }));

    for expected_attempt in 1..=2 {
        store.present.lock().expect("fixture store poisoned")
            .retain(|id| *id != english().live.id);
        watcher.check(&consumer.view, &bus.sender());
        let events = drain_consume(&mut bus, &mut consumer, &mut handle.db);
        assert!(events.contains(&AppEvent::ModelMissing(english().live)));
        assert_eq!(consumer.view.blocks[0].pending_models(), &[english().live.id]);
        assert!(matches!(consumer.view.blocks[0].state, BlockState::Waiting { .. }));

        assert!(events.contains(&AppEvent::DownloadRequested(english().live)));
        settle_until(&mut bus, &mut consumer, &mut handle.db, |view, _| {
            matches!(view.blocks[0].state, BlockState::Recording { .. })
        }).await;
        assert_eq!(calls.load(Ordering::SeqCst), expected_attempt);
        let row = downloads::get(&handle.db, english().live.id).unwrap().unwrap();
        assert_eq!(row.attempt, expected_attempt as u32);
        assert_eq!(row.status, DownloadStatus::Succeeded);
        assert!(store.is_available(english().live));
    }
}

#[tokio::test]
async fn model_dropped_with_two_recording_blocks_shares_one_download() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(FixtureStore::new(
        tmp.path().to_path_buf(),
        &english().models().map(|model| model.id),
    ));
    let calls = Arc::new(AtomicUsize::new(0));
    let mut consumer = Consumer::new(
        Arc::new(FixtureDownloader::new(
            vec![1; 64],
            Outcome::Ok,
            calls.clone(),
        )),
        store.clone(),
        Arc::new(NoSources),
    );
    let watcher = ModelWatcher::new(store.clone());
    let mut bus = EventBus::new();
    let mut handle = downloads_with(&bus);
    for _ in 0..2 {
        consumer.view.apply(&AppEvent::AddBlock);
        confirm_and_consume(&mut bus, &mut consumer, &mut handle.db);
        drain_consume(&mut bus, &mut consumer, &mut handle.db);
    }
    consumer.view.blocks[1].visible = false;
    assert!(consumer.view.blocks.iter().all(|block| matches!(block.state, BlockState::Recording { .. })));
    store.present.lock().expect("fixture store poisoned")
        .retain(|id| *id != english().refine.id);

    watcher.check(&consumer.view, &bus.sender());
    let events = drain_consume(&mut bus, &mut consumer, &mut handle.db);
    assert_eq!(events.iter().filter(|event| matches!(event,
        AppEvent::ModelMissing(model) if model.id == english().refine.id
    )).count(), 1);
    assert!(events.contains(&AppEvent::DownloadRequested(english().refine)));
    for block in &consumer.view.blocks {
        assert!(matches!(block.state, BlockState::Waiting { .. }));
        assert_eq!(block.pending_models(), &[english().refine.id]);
    }

    settle_until(&mut bus, &mut consumer, &mut handle.db, |view, _| {
        view.blocks.iter().all(|block| matches!(block.state, BlockState::Recording { .. }))
    }).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(store.is_available(english().refine));
}

#[tokio::test]
async fn redownload_failure_fails_block_and_retry_recovers() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(FixtureStore::new(
        tmp.path().to_path_buf(),
        &english().models().map(|model| model.id),
    ));
    let failed_calls = Arc::new(AtomicUsize::new(0));
    let mut consumer = Consumer::new(
        Arc::new(FixtureDownloader::new(
            vec![1; 64],
            Outcome::ShaMismatch,
            failed_calls.clone(),
        )),
        store.clone(),
        Arc::new(NoSources),
    );
    let watcher = ModelWatcher::new(store.clone());
    let mut bus = EventBus::new();
    let mut handle = downloads_with(&bus);
    consumer.view.apply(&AppEvent::AddBlock);
    confirm_and_consume(&mut bus, &mut consumer, &mut handle.db);
    drain_consume(&mut bus, &mut consumer, &mut handle.db);
    assert!(matches!(consumer.view.blocks[0].state, BlockState::Recording { .. }));
    store.present.lock().expect("fixture store poisoned")
        .retain(|id| *id != english().live.id);
    watcher.check(&consumer.view, &bus.sender());
    let events = drain_consume(&mut bus, &mut consumer, &mut handle.db);
    assert!(events.contains(&AppEvent::ModelMissing(english().live)));
    assert!(events.contains(&AppEvent::DownloadRequested(english().live)));
    settle_until(&mut bus, &mut consumer, &mut handle.db, |view, _| {
        matches!(view.blocks[0].state, BlockState::Failed { .. })
    }).await;
    assert_eq!(failed_calls.load(Ordering::SeqCst), 1);
    assert!(
        matches!(&consumer.view.blocks[0].state, BlockState::Failed { error, .. }
        if error.contains("sha256 mismatch"))
    );
    watcher.check(&consumer.view, &bus.sender());
    let events = drain_consume(&mut bus, &mut consumer, &mut handle.db);
    assert!(!events
        .iter()
        .any(|event| matches!(event, AppEvent::ModelMissing(_))));

    let retry_calls = Arc::new(AtomicUsize::new(0));
    consumer.producers.downloads = Arc::new(FixtureDownloader::new(
            vec![2; 64],
            Outcome::Ok,
            retry_calls.clone(),
        ));
    retry_and_consume(&mut bus, &mut consumer, &mut handle.db);
    settle_until(&mut bus, &mut consumer, &mut handle.db, |view, _| {
        matches!(view.blocks[0].state, BlockState::Recording { .. })
    }).await;
    assert_eq!(retry_calls.load(Ordering::SeqCst), 1);
    assert!(store.is_available(english().live));
    let row = downloads::get(&handle.db, english().live.id).unwrap().unwrap();
    assert_eq!(row.attempt, 2);
    assert_eq!(row.status, DownloadStatus::Succeeded);
}

struct GatedDownloader {
    inner: FixtureDownloader,
    gate: watch::Receiver<bool>,
}

#[async_trait]
impl Downloader for GatedDownloader {
    async fn fetch(
        &self,
        url: &str,
        staged: &Path,
        expected_sha: &str,
        cancel: &mut (dyn CancelCheck + Send),
        progress: &mut (dyn FnMut(u64, Option<u64>) + Send),
    ) -> Result<(), DownloadError> {
        let mut gate = self.gate.clone();
        timeout(Duration::from_secs(5), gate.wait_for(|open| *open))
            .await
            .map_err(|_| DownloadError::Io("test download gate timed out".to_string()))?
            .map_err(|_| DownloadError::Io("test download gate closed".to_string()))?;
        self.inner.fetch(url, staged, expected_sha, cancel, progress).await
    }
}

#[tokio::test]
async fn model_dropped_while_other_model_downloading() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(FixtureStore::new(tmp.path().to_path_buf(), &[english().live.id]));
    let calls = Arc::new(AtomicUsize::new(0));
    let (gate, receiver) = watch::channel(false);
    let mut consumer = Consumer::new(
        Arc::new(GatedDownloader {
            inner: FixtureDownloader::new(vec![1; 64], Outcome::Ok, calls.clone()),
            gate: receiver,
        }),
        store.clone(),
        Arc::new(NoSources),
    );
    let watcher = ModelWatcher::new(store.clone());
    let mut bus = EventBus::new();
    let mut handle = downloads_with(&bus);
    consumer.view.apply(&AppEvent::AddBlock);
    confirm_and_consume(&mut bus, &mut consumer, &mut handle.db);
    drain_consume(&mut bus, &mut consumer, &mut handle.db);
    assert_eq!(consumer.view.blocks[0].pending_models(), &[english().refine.id]);
    store.present.lock().expect("fixture store poisoned")
        .retain(|id| *id != english().live.id);
    watcher.check(&consumer.view, &bus.sender());
    let events = drain_consume(&mut bus, &mut consumer, &mut handle.db);
    assert_eq!(events.iter().filter(|event| matches!(event,
        AppEvent::ModelMissing(model) if model.id == english().live.id
    )).count(), 1);
    assert!(events.contains(&AppEvent::DownloadRequested(english().live)));
    assert_eq!(consumer.view.blocks[0].pending_models(), &[english().refine.id, english().live.id]);

    // Neither worker can install until both pending models have been observed.
    gate.send_replace(true);
    settle_until(&mut bus, &mut consumer, &mut handle.db, |view, _| {
        matches!(view.blocks[0].state, BlockState::Recording { .. })
    }).await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(english()
        .models()
        .iter()
        .all(|model| store.is_available(model)));
}

fn source_view() -> UiView {
    let mut view = UiView::default();
    view.apply(&AppEvent::AddSourceBlock {
        snapshot: voice_bird_next::testing::sample_source_snapshot(),
    });
    view
}

fn source_consumer(handle: &DownloadsHandle) -> Consumer {
    Consumer::new(
        Arc::new(FixtureDownloader::new(
            vec![1; 64], Outcome::Ok, Arc::new(AtomicUsize::new(0)),
        )),
        Arc::new(FixtureStore::new(
            handle._tmp.path().to_path_buf(),
            &english().models().map(|model| model.id),
        )),
        Arc::new(NoSources),
    )
}

fn source_intent(
    intent: Intent,
    consumer: &mut Consumer,
    db: &mut Database,
    bus: &mut EventBus,
) -> AppEvent {
    resolve_intent(intent, &consumer.view, db, &bus.sender());
    let events: Vec<_> = bus.drain().collect();
    assert_eq!(events.len(), 1, "{events:?}");
    let event = events[0].clone();
    let accepted = consume_batch(events, consumer, db, &bus.sender());
    assert_eq!(accepted, vec![event.clone()]);
    drain_consume(bus, consumer, db);
    event
}

#[tokio::test]
async fn output_funnel_restores_selected_rows_and_preserves_source_through_model_lifecycle() {
    let mut bus = EventBus::new();
    let mut handle = downloads_with(&bus);
    let mut consumer = source_consumer(&handle);
    consumer.view = source_view();
    let store = Arc::new(FixtureStore::new(
        handle._tmp.path().to_path_buf(),
        &english().models().map(|model| model.id),
    ));
    consumer.producers.model_store = store.clone();
    let watcher = ModelWatcher::new(store.clone());
    let snapshot = voice_bird_next::testing::sample_source_snapshot();
    assert!(matches!(
        consumer.view.blocks[0].state,
        BlockState::PickingDevice(_)
    ));
    source_intent(Intent::PickerNext, &mut consumer, &mut handle.db, &mut bus);
    assert_eq!(
        source_intent(Intent::Confirm, &mut consumer, &mut handle.db, &mut bus),
        AppEvent::SourceStepChanged {
            block: 1,
            from: FunnelStep::Device,
            to: FunnelStep::App,
            rev: 0,
            device: Some(snapshot.devices[1].clone()),
            app: None,
        }
    );
    source_intent(Intent::PickerNext, &mut consumer, &mut handle.db, &mut bus);
    assert_eq!(
        source_intent(Intent::Confirm, &mut consumer, &mut handle.db, &mut bus),
        AppEvent::SourceStepChanged {
            block: 1,
            from: FunnelStep::App,
            to: FunnelStep::Language,
            rev: 1,
            device: Some(snapshot.devices[1].clone()),
            app: Some(snapshot.apps[1].clone()),
        }
    );
    assert_eq!(
        source_intent(Intent::StepBack, &mut consumer, &mut handle.db, &mut bus),
        AppEvent::SourceStepChanged {
            block: 1,
            from: FunnelStep::Language,
            to: FunnelStep::App,
            rev: 2,
            device: Some(snapshot.devices[1].clone()),
            app: Some(snapshot.apps[1].clone()),
        }
    );
    assert!(matches!(&consumer.view.blocks[0].state, BlockState::PickingApp(cursor) if cursor.index == 1));
    source_intent(Intent::StepBack, &mut consumer, &mut handle.db, &mut bus);
    assert!(
        matches!(&consumer.view.blocks[0].state, BlockState::PickingDevice(cursor) if cursor.index == 1)
    );
    source_intent(Intent::Confirm, &mut consumer, &mut handle.db, &mut bus);
    assert!(matches!(&consumer.view.blocks[0].state, BlockState::PickingApp(cursor) if cursor.index == 1));
    source_intent(Intent::Confirm, &mut consumer, &mut handle.db, &mut bus);
    let mut selected = consumer.view.blocks[0].source.clone();
    selected.as_mut().unwrap().rev += 1;
    source_intent(Intent::Confirm, &mut consumer, &mut handle.db, &mut bus);
    assert_eq!(consumer.view.blocks[0].source, selected);
    assert!(matches!(consumer.view.blocks[0].state, BlockState::Recording { .. }));

    store.present.lock().expect("fixture store poisoned").retain(|model| *model != english().live.id);
    watcher.check(&consumer.view, &bus.sender());
    let events = drain_consume(&mut bus, &mut consumer, &mut handle.db);
    assert!(events.contains(&AppEvent::ModelMissing(english().live)));
    assert!(matches!(consumer.view.blocks[0].state, BlockState::Waiting { .. }));
    assert_eq!(consumer.view.blocks[0].source, selected);
    settle_until(&mut bus, &mut consumer, &mut handle.db, |view, _| {
        matches!(view.blocks[0].state, BlockState::Recording { .. })
    }).await;
    assert_eq!(consumer.view.blocks[0].source, selected);
    assert!(store.is_available(english().live));
}

#[tokio::test]
async fn input_skips_app_and_back_returns_to_device() {
    let mut bus = EventBus::new();
    let mut handle = downloads_with(&bus);
    let mut consumer = source_consumer(&handle);
    consumer.view = source_view();
    let snapshot = voice_bird_next::testing::sample_source_snapshot();
    assert_eq!(
        source_intent(Intent::Confirm, &mut consumer, &mut handle.db, &mut bus),
        AppEvent::SourceStepChanged {
            block: 1,
            from: FunnelStep::Device,
            to: FunnelStep::Language,
            rev: 0,
            device: Some(snapshot.devices[0].clone()),
            app: None,
        }
    );
    assert_eq!(
        source_intent(Intent::StepBack, &mut consumer, &mut handle.db, &mut bus),
        AppEvent::SourceStepChanged {
            block: 1,
            from: FunnelStep::Language,
            to: FunnelStep::Device,
            rev: 1,
            device: Some(snapshot.devices[0].clone()),
            app: None,
        }
    );
    assert!(
        matches!(&consumer.view.blocks[0].state, BlockState::PickingDevice(cursor) if cursor.index == 0)
    );
}

#[tokio::test]
async fn duplicate_confirm_advances_source_once() {
    let mut bus = EventBus::new();
    let mut handle = downloads_with(&bus);
    let mut consumer = source_consumer(&handle);
    consumer.view = source_view();
    for _ in 0..2 {
        resolve_intent(Intent::Confirm, &consumer.view, &mut handle.db, &bus.sender());
    }
    let accepted = drain_consume(&mut bus, &mut consumer, &mut handle.db);
    assert_eq!(accepted.iter().filter(|event| matches!(event,
        AppEvent::SourceStepChanged { .. }
    )).count(), 1);
    assert!(matches!(consumer.view.blocks[0].state, BlockState::Picking(_)));
    assert_eq!(consumer.view.blocks[0].source.as_ref().unwrap().rev, 1);
    assert!(accepted.contains(&AppEvent::SourceStepRejected {
            block: 1,
            from: FunnelStep::Device,
            to: FunnelStep::Language,
            rev: 0,
            actual: Some((FunnelStep::Language, 1)),
        }));
}

#[tokio::test]
async fn language_confirm_and_back_race_never_orphans_downloads() {
    for confirm_first in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut consumer = Consumer::new(
            Arc::new(FixtureDownloader::new(
                vec![1; 64],
                Outcome::Ok,
                calls.clone(),
            )),
            Arc::new(FixtureStore::new(tmp.path().to_path_buf(), &[])),
            Arc::new(NoSources),
        );
        let mut bus = EventBus::new();
        let mut handle = downloads_with(&bus);
        consumer.view = source_view();
        source_intent(Intent::PickerNext, &mut consumer, &mut handle.db, &mut bus);
        source_intent(Intent::Confirm, &mut consumer, &mut handle.db, &mut bus);
        source_intent(Intent::Confirm, &mut consumer, &mut handle.db, &mut bus);
        let intents = if confirm_first {
            [Intent::Confirm, Intent::StepBack]
        } else {
            [Intent::StepBack, Intent::Confirm]
        };
        for intent in intents {
            resolve_intent(intent, &consumer.view, &mut handle.db, &bus.sender());
        }
        let accepted = drain_consume(&mut bus, &mut consumer, &mut handle.db);
        assert_eq!(accepted.iter().filter(|event| matches!(event,
            AppEvent::SourceStepChanged { .. } | AppEvent::BeginLanguage { .. }
        )).count(), 1);

        if confirm_first {
            settle_until(&mut bus, &mut consumer, &mut handle.db, |view, _| {
                matches!(view.blocks[0].state, BlockState::Recording { .. })
            }).await;
            assert_eq!(calls.load(Ordering::SeqCst), 2);
            assert!(!voice_bird_next::db::apply(&mut handle.db, &accepted[0]).unwrap());
            assert!(bus.drain().any(|event| matches!(
                event,
                AppEvent::SourceStepRejected {
                    actual: Some((FunnelStep::Committed, 3)),
                    ..
                }
            )));
        } else {
            assert!(matches!(consumer.view.blocks[0].state, BlockState::PickingApp(_)));
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            for model in english().models() {
                assert!(downloads::get(&handle.db, model.id).unwrap().is_none());
            }
        }
    }
}

#[tokio::test]
async fn back_is_ignored_outside_source_pickers_and_menu_intercepts_it() {
    let mut bus = EventBus::new();
    let mut handle = downloads_with(&bus);
    let mut consumer = source_consumer(&handle);
    consumer.view = source_view();
    let states = [
        BlockState::PickingDevice(Default::default()),
        BlockState::Waiting {
            language: english(),
            pending: vec![english().live.id],
        },
        BlockState::Recording {
            language: english(),
        },
        BlockState::Failed {
            language: english(),
            pending: Vec::new(),
            error: "failure".into(),
        },
    ];
    for phase in states {
        consumer.view.blocks[0].state = phase;
        resolve_intent(Intent::StepBack, &consumer.view, &mut handle.db, &bus.sender());
        assert_eq!(bus.drain().count(), 0);
    }
    consumer.view = UiView::default();
    consumer.view.apply(&AppEvent::AddBlock);
    resolve_intent(Intent::StepBack, &consumer.view, &mut handle.db, &bus.sender());
    assert_eq!(bus.drain().count(), 0);
    consumer.view = source_view();
    source_intent(Intent::Confirm, &mut consumer, &mut handle.db, &mut bus);
    consumer.view.apply(&AppEvent::MenuOpened);
    resolve_intent(Intent::StepBack, &consumer.view, &mut handle.db, &bus.sender());
    assert_eq!(bus.drain().count(), 0);
}

#[tokio::test]
async fn empty_apps_cannot_confirm_and_close_resets_reused_id() {
    let mut bus = EventBus::new();
    let mut handle = downloads_with(&bus);
    let mut consumer = source_consumer(&handle);
    consumer.view = source_view();
    consumer.view.blocks[0]
        .source
        .as_mut()
        .unwrap()
        .snapshot
        .apps
        .clear();
    source_intent(Intent::PickerNext, &mut consumer, &mut handle.db, &mut bus);
    source_intent(Intent::Confirm, &mut consumer, &mut handle.db, &mut bus);
    resolve_intent(Intent::Confirm, &consumer.view, &mut handle.db, &bus.sender());
    assert_eq!(bus.drain().count(), 0);
    source_intent(Intent::BlockClosed, &mut consumer, &mut handle.db, &mut bus);
    assert!(consumer.view.blocks.is_empty());
    consumer.view.next_block_id = 1;
    consumer.view.apply(&AppEvent::AddSourceBlock {
        snapshot: voice_bird_next::testing::sample_source_snapshot(),
    });
    source_intent(Intent::Confirm, &mut consumer, &mut handle.db, &mut bus);
    assert!(matches!(consumer.view.blocks[0].state, BlockState::Picking(_)));
    assert_eq!(consumer.view.blocks[0].source.as_ref().unwrap().rev, 1);
}

#[tokio::test]
async fn queued_confirm_then_close_clears_steps_before_block_id_reuse() {
    let mut bus = EventBus::new();
    let mut handle = downloads_with(&bus);
    let mut consumer = source_consumer(&handle);
    consumer.view = source_view();
    // Both intents observe Device/0, before either event has been applied.
    resolve_intent(Intent::Confirm, &consumer.view, &mut handle.db, &bus.sender());
    resolve_intent(Intent::BlockClosed, &consumer.view, &mut handle.db, &bus.sender());
    let accepted = drain_consume(&mut bus, &mut consumer, &mut handle.db);
    assert!(matches!(
        accepted.as_slice(),
        [AppEvent::SourceStepChanged { .. }, AppEvent::BlockClosed { block: 1 }]
    ));
    assert!(consumer.view.blocks.is_empty());
    consumer.view.next_block_id = 1;
    consumer.view.apply(&AppEvent::AddSourceBlock {
        snapshot: voice_bird_next::testing::sample_source_snapshot(),
    });
    source_intent(Intent::Confirm, &mut consumer, &mut handle.db, &mut bus);
    assert!(matches!(consumer.view.blocks[0].state, BlockState::Picking(_)));
    assert_eq!(consumer.view.blocks[0].source.as_ref().unwrap().rev, 1);
}

#[tokio::test]
async fn add_block_with_menu_open_uses_catalog_and_focuses_new_source_picker() {
    let tmp = tempfile::tempdir().unwrap();
    let mut consumer = Consumer::new(
        Arc::new(FixtureDownloader::new(
            Vec::new(),
            Outcome::Ok,
            Arc::new(AtomicUsize::new(0)),
        )),
        Arc::new(FixtureStore::new(tmp.path().to_path_buf(), &[])),
        Arc::new(voice_bird_next::testing::FixtureSources(Some(
            voice_bird_next::testing::sample_source_snapshot(),
        ))),
    );
    let mut bus = EventBus::new();
    let mut handle = downloads_with(&bus);
    consumer.view.apply(&AppEvent::MenuOpened);
    resolve_intent(Intent::AddBlock, &consumer.view, &mut handle.db, &bus.sender());
    let commands = drain_consume(&mut bus, &mut consumer, &mut handle.db);
    assert_eq!(commands, vec![AppEvent::RequestBlock]);

    settle_until(&mut bus, &mut consumer, &mut handle.db, |view, _| {
        matches!(view.blocks.first().map(|block| &block.state), Some(BlockState::PickingDevice(_)))
    }).await;
    assert!(matches!(
        consumer.view.focused().unwrap().state,
        BlockState::PickingDevice(_)
    ));
    assert_eq!(
        consumer.view.focused().unwrap().source.as_ref().unwrap().snapshot,
        voice_bird_next::testing::sample_source_snapshot()
    );
}
