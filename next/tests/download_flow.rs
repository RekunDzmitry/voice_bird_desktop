//! End-to-end language download flow tests. No test touches the network.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use voice_bird_next::audio_source::{FunnelStep, NoSources};
use voice_bird_next::bus::{AppEvent, DownloadStatus, EventBus, EventSender};
use voice_bird_next::db::downloads::CancelCheck;
use voice_bird_next::db::{downloads, Database};
use voice_bird_next::dispatcher::Dispatcher;
use voice_bird_next::download::{DownloadError, Downloader};
use voice_bird_next::input::Intent;
use voice_bird_next::language::{LanguageProfile, LANGUAGES};
use voice_bird_next::model_watch::ModelWatcher;
use voice_bird_next::producer;
use voice_bird_next::state::{BlockState, UiState};
use voice_bird_next::testing::{FixtureDownloader, FixtureStore, Outcome};
use voice_bird_next::transcription_models::{CacheDirStore, ModelStore};

fn english() -> &'static LanguageProfile {
    &LANGUAGES[0]
}
struct WritingDownloader {
    calls: Arc<AtomicUsize>,
}

impl Downloader for WritingDownloader {
    fn fetch(
        &self,
        _url: &str,
        staged: &Path,
        _expected_sha: &str,
        cancel: &mut dyn CancelCheck,
        progress: &mut dyn FnMut(u64, Option<u64>),
    ) -> Result<(), DownloadError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if cancel.is_cancelled() {
            return Err(DownloadError::Cancelled);
        }
        std::fs::write(staged, b"fixture model")
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

fn drain_apply(bus: &mut EventBus, state: &mut UiState, db: &mut Database) -> Vec<AppEvent> {
    let events: Vec<_> = bus.drain().collect();
    events
        .into_iter()
        .filter(|event| {
            if voice_bird_next::db::apply(db, event).unwrap() {
                state.apply(event);
                true
            } else {
                false
            }
        })
        .collect()
}

fn confirm_and_dispatch(
    bus: &mut EventBus,
    state: &mut UiState,
    db: &mut Database,
    dispatcher: &Dispatcher,
) -> Vec<AppEvent> {
    let tx = bus.sender();
    producer::resolve_intent(Intent::Confirm, state, db, &tx);
    let commands = drain_apply(bus, state, db);
    dispatcher.dispatch(&commands, db, &tx);
    commands
}

fn retry_and_dispatch(
    bus: &mut EventBus,
    state: &mut UiState,
    db: &mut Database,
    dispatcher: &Dispatcher,
) {
    let tx = bus.sender();
    producer::resolve_intent(Intent::Retry, state, db, &tx);
    let commands = drain_apply(bus, state, db);
    dispatcher.dispatch(&commands, db, &tx);
}

fn settle_until(
    bus: &mut EventBus,
    state: &mut UiState,
    db: &mut Database,
    predicate: impl Fn(&UiState) -> bool,
) {
    for _ in 0..100 {
        drain_apply(bus, state, db);
        if predicate(state) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("state did not settle: {:?}", state.blocks);
}

#[test]
fn language_with_both_models_cached_records_immediately() {
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
    let dispatcher = Dispatcher::new(downloader, store, Arc::new(NoSources));
    let mut bus = EventBus::new();
    let mut handle = downloads_with(&bus);
    let mut state = UiState::default();
    state.apply(&AppEvent::AddBlock);

    confirm_and_dispatch(&mut bus, &mut state, &mut handle.db, &dispatcher);
    let events = drain_apply(&mut bus, &mut state, &mut handle.db);

    assert!(events.iter().any(|event| matches!(
        event,
        AppEvent::LanguageSelected { block: 1, pending, .. } if pending.is_empty()
    )));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(matches!(
        state.blocks[0].state,
        BlockState::Recording { language } if language == english()
    ));
}

#[test]
fn language_waits_for_both_models_then_records() {
    let mut bus = EventBus::new();
    let mut handle = downloads_with(&bus);
    let mut state = UiState::default();
    state.apply(&AppEvent::AddBlock);
    bus.sender().publish(AppEvent::LanguageSelected {
        block: 1,
        language: english(),
        pending: english().models().map(|model| model.id).to_vec(),
    });
    drain_apply(&mut bus, &mut state, &mut handle.db);

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

#[test]
fn language_with_one_cached_model_downloads_only_the_other() {
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
    let dispatcher = Dispatcher::new(downloader, store, Arc::new(NoSources));
    let mut bus = EventBus::new();
    let mut handle = downloads_with(&bus);
    let mut state = UiState::default();
    state.apply(&AppEvent::AddBlock);

    confirm_and_dispatch(&mut bus, &mut state, &mut handle.db, &dispatcher);
    settle_until(&mut bus, &mut state, &mut handle.db, |state| {
        matches!(state.blocks[0].state, BlockState::Recording { .. })
    });

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(store_concrete.is_available(english().live));
    assert!(store_concrete.is_available(english().refine));
}

#[test]
fn refine_model_failure_fails_block_and_retry_downloads_only_missing() {
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
    let failing_dispatcher = Dispatcher::new(failing, store.clone(), Arc::new(NoSources));
    let mut bus = EventBus::new();
    let mut handle = downloads_with(&bus);
    let mut state = UiState::default();
    state.apply(&AppEvent::AddBlock);

    confirm_and_dispatch(&mut bus, &mut state, &mut handle.db, &failing_dispatcher);
    settle_until(&mut bus, &mut state, &mut handle.db, |state| {
        matches!(state.blocks[0].state, BlockState::Failed { .. })
    });
    assert_eq!(failed_calls.load(Ordering::SeqCst), 1);

    let retry_calls = Arc::new(AtomicUsize::new(0));
    let successful: Arc<dyn Downloader> = Arc::new(FixtureDownloader::new(
        vec![2; 64],
        Outcome::Ok,
        retry_calls.clone(),
    ));
    let retry_dispatcher = Dispatcher::new(successful, store, Arc::new(NoSources));
    retry_and_dispatch(&mut bus, &mut state, &mut handle.db, &retry_dispatcher);
    settle_until(&mut bus, &mut state, &mut handle.db, |state| {
        matches!(state.blocks[0].state, BlockState::Recording { .. })
    });

    assert_eq!(retry_calls.load(Ordering::SeqCst), 1);
    assert!(store_concrete.is_available(english().refine));
}

#[test]
fn claim_failure_on_one_model_fails_block() {
    let mut bus = EventBus::new();
    let mut handle = downloads_with(&bus);
    let mut state = UiState::default();
    state.apply(&AppEvent::AddBlock);
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
    drain_apply(&mut bus, &mut state, &mut handle.db);

    assert!(matches!(
        &state.blocks[0].state,
        BlockState::Failed {
            language, error, ..
        } if *language == english() && error == "database locked"
    ));
}

#[test]
fn failure_between_availability_check_and_selection_reaches_late_waiter() {
    let tmp = tempfile::tempdir().unwrap();
    let mut bus = EventBus::new();
    let mut handle = downloads_with(&bus);
    let mut state = UiState::default();

    downloads::start(&mut handle.db, english().live.id).unwrap();
    drain_apply(&mut bus, &mut state, &mut handle.db);

    state.apply(&AppEvent::AddBlock);
    state.apply(&AppEvent::LanguageSelected {
        block: 1,
        language: english(),
        pending: vec![english().live.id],
    });
    state.apply(&AppEvent::AddBlock);

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

    voice_bird_next::download::begin_language(
        2,
        english(),
        store,
        &mut handle.db,
        downloader,
        &bus.sender(),
    );

    let first = drain_apply(&mut bus, &mut state, &mut handle.db);
    assert!(matches!(state.blocks[1].state, BlockState::Waiting { .. }));
    assert!(first.iter().any(|event| matches!(
        event,
        AppEvent::DownloadFailed { model, .. } if *model == english().live.id
    )));
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

    let second = drain_apply(&mut bus, &mut state, &mut handle.db);
    assert!(second.iter().any(|event| matches!(
        event,
        AppEvent::DownloadStatusChanged {
            model,
            to: DownloadStatus::Failed,
            ..
        } if model.as_ref() == english().live.id
    )));
    assert!(matches!(
        &state.blocks[1].state,
        BlockState::Failed { error, .. } if error == "HTTP 404"
    ));
}

#[test]
fn two_blocks_same_language_share_downloads() {
    let tmp = tempfile::tempdir().unwrap();
    let store: Arc<dyn ModelStore> = Arc::new(FixtureStore::new(tmp.path().to_path_buf(), &[]));
    let calls = Arc::new(AtomicUsize::new(0));
    let downloader: Arc<dyn Downloader> = Arc::new(
        FixtureDownloader::new(vec![1; 128 * 1024], Outcome::Ok, calls.clone()).with_delay(100),
    );
    let dispatcher = Dispatcher::new(downloader, store, Arc::new(NoSources));
    let mut bus = EventBus::new();
    let mut handle = downloads_with(&bus);
    let mut state = UiState::default();

    state.apply(&AppEvent::AddBlock);
    confirm_and_dispatch(&mut bus, &mut state, &mut handle.db, &dispatcher);
    drain_apply(&mut bus, &mut state, &mut handle.db);
    state.apply(&AppEvent::AddBlock);
    confirm_and_dispatch(&mut bus, &mut state, &mut handle.db, &dispatcher);
    drain_apply(&mut bus, &mut state, &mut handle.db);
    std::thread::sleep(Duration::from_millis(20));

    assert_eq!(calls.load(Ordering::SeqCst), 2);
    for model in english().models() {
        let row = downloads::get(&handle.db, model.id).unwrap().unwrap();
        assert_eq!(row.attempt, 1);
        assert_eq!(row.status, DownloadStatus::Downloading);
    }
    assert!(state
        .blocks
        .iter()
        .all(|block| matches!(block.state, BlockState::Waiting { .. })));
}

#[test]
fn closing_last_waiter_cancels_both_pending_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let store: Arc<dyn ModelStore> = Arc::new(FixtureStore::new(tmp.path().to_path_buf(), &[]));
    let downloader: Arc<dyn Downloader> = Arc::new(
        FixtureDownloader::new(
            vec![1; 128 * 1024],
            Outcome::Ok,
            Arc::new(AtomicUsize::new(0)),
        )
        .with_delay(100),
    );
    let dispatcher = Dispatcher::new(downloader, store, Arc::new(NoSources));
    let mut bus = EventBus::new();
    let mut handle = downloads_with(&bus);
    let mut state = UiState::default();
    state.apply(&AppEvent::AddBlock);
    confirm_and_dispatch(&mut bus, &mut state, &mut handle.db, &dispatcher);
    drain_apply(&mut bus, &mut state, &mut handle.db);

    producer::resolve_intent(Intent::BlockClosed, &state, &mut handle.db, &bus.sender());

    for model in english().models() {
        let row = downloads::get(&handle.db, model.id).unwrap().unwrap();
        assert_eq!(row.status, DownloadStatus::Cancelling);
    }
}

#[test]
fn language_selected_targets_block_id_not_focus() {
    let tmp = tempfile::tempdir().unwrap();
    let present: Vec<_> = english().models().map(|model| model.id).to_vec();
    let store: Arc<dyn ModelStore> =
        Arc::new(FixtureStore::new(tmp.path().to_path_buf(), &present));
    let downloader: Arc<dyn Downloader> = Arc::new(FixtureDownloader::new(
        Vec::new(),
        Outcome::Ok,
        Arc::new(AtomicUsize::new(0)),
    ));
    let dispatcher = Dispatcher::new(downloader, store, Arc::new(NoSources));
    let mut bus = EventBus::new();
    let mut handle = downloads_with(&bus);
    let mut state = UiState::default();
    state.apply(&AppEvent::AddBlock);
    state.apply(&AppEvent::AddBlock);
    assert_eq!(state.focus, 1);

    bus.sender().publish(AppEvent::BeginLanguage {
        block: 1,
        language: english(),
        source_rev: None,
    });
    let commands = drain_apply(&mut bus, &mut state, &mut handle.db);
    dispatcher.dispatch(&commands, &mut handle.db, &bus.sender());
    drain_apply(&mut bus, &mut state, &mut handle.db);

    assert!(matches!(
        state.blocks[0].state,
        BlockState::Recording { .. }
    ));
    assert!(matches!(state.blocks[1].state, BlockState::Picking(_)));
    assert_eq!(state.focus, 1);
}

#[test]
fn second_session_is_warm() {
    let tmp = tempfile::tempdir().unwrap();
    let models_root = tmp.path().join("models");
    let store: Arc<dyn ModelStore> =
        Arc::new(CacheDirStore::from_root(models_root.clone()).unwrap());
    let first_calls = Arc::new(AtomicUsize::new(0));
    let downloader: Arc<dyn Downloader> = Arc::new(WritingDownloader {
        calls: first_calls.clone(),
    });
    let dispatcher = Dispatcher::new(downloader, store, Arc::new(NoSources));
    let mut first_bus = EventBus::new();
    let mut first_db = downloads_with(&first_bus);
    let mut first_state = UiState::default();
    first_state.apply(&AppEvent::AddBlock);
    confirm_and_dispatch(
        &mut first_bus,
        &mut first_state,
        &mut first_db.db,
        &dispatcher,
    );
    settle_until(
        &mut first_bus,
        &mut first_state,
        &mut first_db.db,
        |state| matches!(state.blocks[0].state, BlockState::Recording { .. }),
    );
    assert_eq!(first_calls.load(Ordering::SeqCst), 2);

    let second_store: Arc<dyn ModelStore> =
        Arc::new(CacheDirStore::from_root(models_root).unwrap());
    let second_calls = Arc::new(AtomicUsize::new(0));
    let second_downloader: Arc<dyn Downloader> = Arc::new(FixtureDownloader::new(
        Vec::new(),
        Outcome::Ok,
        second_calls.clone(),
    ));
    let second_dispatcher = Dispatcher::new(second_downloader, second_store, Arc::new(NoSources));
    let mut second_bus = EventBus::new();
    let mut second_db = downloads_with(&second_bus);
    let mut second_state = UiState::default();
    second_state.apply(&AppEvent::AddBlock);
    confirm_and_dispatch(
        &mut second_bus,
        &mut second_state,
        &mut second_db.db,
        &second_dispatcher,
    );
    let events = drain_apply(&mut second_bus, &mut second_state, &mut second_db.db);

    assert!(events.iter().any(|event| matches!(
        event,
        AppEvent::LanguageSelected { pending, .. } if pending.is_empty()
    )));
    assert_eq!(second_calls.load(Ordering::SeqCst), 0);
    assert!(matches!(
        second_state.blocks[0].state,
        BlockState::Recording { .. }
    ));
}

#[test]
fn cleanup_preserves_installed_models() {
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
    let dispatcher = Dispatcher::new(downloader, store, Arc::new(NoSources));
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
    let events: Vec<_> = bus.drain().collect();
    dispatcher.dispatch(&events, &mut handle.db, &tx);

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

#[test]
fn model_dropped_while_recording_redownloads_and_resumes() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(FixtureStore::new(
        tmp.path().to_path_buf(),
        &english().models().map(|model| model.id),
    ));
    let calls = Arc::new(AtomicUsize::new(0));
    let dispatcher = Dispatcher::new(
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
    let mut state = UiState::default();
    state.apply(&AppEvent::AddBlock);
    confirm_and_dispatch(&mut bus, &mut state, &mut handle.db, &dispatcher);
    drain_apply(&mut bus, &mut state, &mut handle.db);
    assert!(matches!(state.blocks[0].state, BlockState::Recording { .. }));

    for expected_attempt in 1..=2 {
        store.present.lock().expect("fixture store poisoned")
            .retain(|id| *id != english().live.id);
        watcher.check(&state, &bus.sender());
        let events = drain_apply(&mut bus, &mut state, &mut handle.db);
        assert!(events.contains(&AppEvent::ModelMissing(english().live)));
        assert_eq!(state.blocks[0].pending_models(), &[english().live.id]);
        assert!(matches!(state.blocks[0].state, BlockState::Waiting { .. }));
        dispatcher.dispatch(&events, &mut handle.db, &bus.sender());
        let events = drain_apply(&mut bus, &mut state, &mut handle.db);
        assert!(events.contains(&AppEvent::DownloadRequested(english().live)));
        settle_until(&mut bus, &mut state, &mut handle.db, |state| {
            matches!(state.blocks[0].state, BlockState::Recording { .. })
        });
        assert_eq!(calls.load(Ordering::SeqCst), expected_attempt);
        let row = downloads::get(&handle.db, english().live.id).unwrap().unwrap();
        assert_eq!(row.attempt, expected_attempt as u32);
        assert_eq!(row.status, DownloadStatus::Succeeded);
        assert!(store.is_available(english().live));
    }
}

#[test]
fn model_dropped_with_two_recording_blocks_shares_one_download() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(FixtureStore::new(
        tmp.path().to_path_buf(),
        &english().models().map(|model| model.id),
    ));
    let calls = Arc::new(AtomicUsize::new(0));
    let dispatcher = Dispatcher::new(
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
    let mut state = UiState::default();
    for _ in 0..2 {
        state.apply(&AppEvent::AddBlock);
        confirm_and_dispatch(&mut bus, &mut state, &mut handle.db, &dispatcher);
        drain_apply(&mut bus, &mut state, &mut handle.db);
    }
    state.blocks[1].visible = false;
    assert!(state.blocks.iter().all(|block| matches!(block.state, BlockState::Recording { .. })));
    store.present.lock().expect("fixture store poisoned")
        .retain(|id| *id != english().refine.id);

    watcher.check(&state, &bus.sender());
    let events = drain_apply(&mut bus, &mut state, &mut handle.db);
    assert_eq!(events, vec![AppEvent::ModelMissing(english().refine)]);
    for block in &state.blocks {
        assert!(matches!(block.state, BlockState::Waiting { .. }));
        assert_eq!(block.pending_models(), &[english().refine.id]);
    }
    dispatcher.dispatch(&events, &mut handle.db, &bus.sender());
    settle_until(&mut bus, &mut state, &mut handle.db, |state| {
        state.blocks.iter().all(|block| matches!(block.state, BlockState::Recording { .. }))
    });
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(store.is_available(english().refine));
}

#[test]
fn redownload_failure_fails_block_and_retry_recovers() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(FixtureStore::new(
        tmp.path().to_path_buf(),
        &english().models().map(|model| model.id),
    ));
    let failed_calls = Arc::new(AtomicUsize::new(0));
    let failing = Dispatcher::new(
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
    let mut state = UiState::default();
    state.apply(&AppEvent::AddBlock);
    confirm_and_dispatch(&mut bus, &mut state, &mut handle.db, &failing);
    drain_apply(&mut bus, &mut state, &mut handle.db);
    assert!(matches!(state.blocks[0].state, BlockState::Recording { .. }));
    store.present.lock().expect("fixture store poisoned")
        .retain(|id| *id != english().live.id);
    watcher.check(&state, &bus.sender());
    let events = drain_apply(&mut bus, &mut state, &mut handle.db);
    failing.dispatch(&events, &mut handle.db, &bus.sender());
    settle_until(&mut bus, &mut state, &mut handle.db, |state| {
        matches!(state.blocks[0].state, BlockState::Failed { .. })
    });
    assert_eq!(failed_calls.load(Ordering::SeqCst), 1);
    assert!(
        matches!(&state.blocks[0].state, BlockState::Failed { error, .. }
        if error.contains("sha256 mismatch"))
    );
    watcher.check(&state, &bus.sender());
    let events = drain_apply(&mut bus, &mut state, &mut handle.db);
    assert!(!events
        .iter()
        .any(|event| matches!(event, AppEvent::ModelMissing(_))));

    let retry_calls = Arc::new(AtomicUsize::new(0));
    let successful = Dispatcher::new(
        Arc::new(FixtureDownloader::new(
            vec![2; 64],
            Outcome::Ok,
            retry_calls.clone(),
        )),
        store.clone(),
        Arc::new(NoSources),
    );
    retry_and_dispatch(&mut bus, &mut state, &mut handle.db, &successful);
    settle_until(&mut bus, &mut state, &mut handle.db, |state| {
        matches!(state.blocks[0].state, BlockState::Recording { .. })
    });
    assert_eq!(retry_calls.load(Ordering::SeqCst), 1);
    assert!(store.is_available(english().live));
    let row = downloads::get(&handle.db, english().live.id).unwrap().unwrap();
    assert_eq!(row.attempt, 2);
    assert_eq!(row.status, DownloadStatus::Succeeded);
}

struct GatedDownloader {
    inner: FixtureDownloader,
    gate: Arc<AtomicBool>,
}

impl Downloader for GatedDownloader {
    fn fetch(
        &self,
        url: &str,
        staged: &Path,
        expected_sha: &str,
        cancel: &mut dyn CancelCheck,
        progress: &mut dyn FnMut(u64, Option<u64>),
    ) -> Result<(), DownloadError> {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !self.gate.load(Ordering::SeqCst) {
            if std::time::Instant::now() >= deadline {
                return Err(DownloadError::Io("test download gate timed out".to_string()));
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        self.inner.fetch(url, staged, expected_sha, cancel, progress)
    }
}

#[test]
fn model_dropped_while_other_model_downloading() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(FixtureStore::new(tmp.path().to_path_buf(), &[english().live.id]));
    let calls = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(AtomicBool::new(false));
    let dispatcher = Dispatcher::new(
        Arc::new(GatedDownloader {
            inner: FixtureDownloader::new(vec![1; 64], Outcome::Ok, calls.clone()),
            gate: gate.clone(),
        }),
        store.clone(),
        Arc::new(NoSources),
    );
    let watcher = ModelWatcher::new(store.clone());
    let mut bus = EventBus::new();
    let mut handle = downloads_with(&bus);
    let mut state = UiState::default();
    state.apply(&AppEvent::AddBlock);
    confirm_and_dispatch(&mut bus, &mut state, &mut handle.db, &dispatcher);
    drain_apply(&mut bus, &mut state, &mut handle.db);
    assert_eq!(state.blocks[0].pending_models(), &[english().refine.id]);
    store.present.lock().expect("fixture store poisoned")
        .retain(|id| *id != english().live.id);
    watcher.check(&state, &bus.sender());
    let events = drain_apply(&mut bus, &mut state, &mut handle.db);
    assert_eq!(events, vec![AppEvent::ModelMissing(english().live)]);
    assert_eq!(state.blocks[0].pending_models(), &[english().refine.id, english().live.id]);
    dispatcher.dispatch(&events, &mut handle.db, &bus.sender());
    // Neither worker can install until both pending models have been observed.
    gate.store(true, Ordering::SeqCst);
    settle_until(&mut bus, &mut state, &mut handle.db, |state| {
        matches!(state.blocks[0].state, BlockState::Recording { .. })
    });
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(english()
        .models()
        .iter()
        .all(|model| store.is_available(model)));
}

fn source_state() -> UiState {
    let mut state = UiState::default();
    state.apply(&AppEvent::AddSourceBlock {
        snapshot: voice_bird_next::testing::sample_source_snapshot(),
    });
    state
}

fn source_intent(
    intent: Intent,
    state: &mut UiState,
    db: &mut Database,
    bus: &mut EventBus,
) -> AppEvent {
    producer::resolve_intent(intent, state, db, &bus.sender());
    let events: Vec<_> = bus.drain().collect();
    assert_eq!(events.len(), 1, "{events:?}");
    let event = events.into_iter().next().unwrap();
    assert!(voice_bird_next::db::apply(db, &event).unwrap());
    state.apply(&event);
    event
}

#[test]
fn output_funnel_restores_selected_rows_and_preserves_source_through_model_lifecycle() {
    let mut bus = EventBus::new();
    let mut handle = downloads_with(&bus);
    let mut state = source_state();
    let snapshot = voice_bird_next::testing::sample_source_snapshot();
    assert!(matches!(
        state.blocks[0].state,
        BlockState::PickingDevice(_)
    ));
    source_intent(Intent::PickerNext, &mut state, &mut handle.db, &mut bus);
    assert_eq!(
        source_intent(Intent::Confirm, &mut state, &mut handle.db, &mut bus),
        AppEvent::SourceStepChanged {
            block: 1,
            from: FunnelStep::Device,
            to: FunnelStep::App,
            rev: 0,
            device: Some(snapshot.devices[1].clone()),
            app: None,
        }
    );
    source_intent(Intent::PickerNext, &mut state, &mut handle.db, &mut bus);
    assert_eq!(
        source_intent(Intent::Confirm, &mut state, &mut handle.db, &mut bus),
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
        source_intent(Intent::StepBack, &mut state, &mut handle.db, &mut bus),
        AppEvent::SourceStepChanged {
            block: 1,
            from: FunnelStep::Language,
            to: FunnelStep::App,
            rev: 2,
            device: Some(snapshot.devices[1].clone()),
            app: Some(snapshot.apps[1].clone()),
        }
    );
    assert!(matches!(&state.blocks[0].state, BlockState::PickingApp(cursor) if cursor.index == 1));
    source_intent(Intent::StepBack, &mut state, &mut handle.db, &mut bus);
    assert!(
        matches!(&state.blocks[0].state, BlockState::PickingDevice(cursor) if cursor.index == 1)
    );
    source_intent(Intent::Confirm, &mut state, &mut handle.db, &mut bus);
    assert!(matches!(&state.blocks[0].state, BlockState::PickingApp(cursor) if cursor.index == 1));
    source_intent(Intent::Confirm, &mut state, &mut handle.db, &mut bus);
    let selected = state.blocks[0].source.clone();
    state.apply(&AppEvent::LanguageSelected {
        block: 1,
        language: english(),
        pending: Vec::new(),
    });
    assert_eq!(state.blocks[0].source, selected);
    state.apply(&AppEvent::ModelMissing(english().live));
    assert_eq!(state.blocks[0].source, selected);
    state.apply(&AppEvent::DownloadSucceeded {
        attempt: 1,
        model: english().live.id,
    });
    assert_eq!(state.blocks[0].source, selected);
    assert!(matches!(
        state.blocks[0].state,
        BlockState::Recording { .. }
    ));
}

#[test]
fn input_skips_app_and_back_returns_to_device() {
    let mut bus = EventBus::new();
    let mut handle = downloads_with(&bus);
    let mut state = source_state();
    let snapshot = voice_bird_next::testing::sample_source_snapshot();
    assert_eq!(
        source_intent(Intent::Confirm, &mut state, &mut handle.db, &mut bus),
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
        source_intent(Intent::StepBack, &mut state, &mut handle.db, &mut bus),
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
        matches!(&state.blocks[0].state, BlockState::PickingDevice(cursor) if cursor.index == 0)
    );
}

#[test]
fn duplicate_confirm_advances_source_once() {
    let mut bus = EventBus::new();
    let mut handle = downloads_with(&bus);
    let mut state = source_state();
    for _ in 0..2 {
        producer::resolve_intent(Intent::Confirm, &state, &mut handle.db, &bus.sender());
    }
    let accepted = drain_apply(&mut bus, &mut state, &mut handle.db);
    assert_eq!(accepted.len(), 1);
    assert!(matches!(state.blocks[0].state, BlockState::Picking(_)));
    assert_eq!(state.blocks[0].source.as_ref().unwrap().rev, 1);
    let rejected: Vec<_> = bus.drain().collect();
    assert_eq!(
        rejected,
        vec![AppEvent::SourceStepRejected {
            block: 1,
            from: FunnelStep::Device,
            to: FunnelStep::Language,
            rev: 0,
            actual: Some((FunnelStep::Language, 1)),
        }]
    );
}

#[test]
fn language_confirm_and_back_race_never_orphans_downloads() {
    for confirm_first in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let dispatcher = Dispatcher::new(
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
        let mut state = source_state();
        source_intent(Intent::PickerNext, &mut state, &mut handle.db, &mut bus);
        source_intent(Intent::Confirm, &mut state, &mut handle.db, &mut bus);
        source_intent(Intent::Confirm, &mut state, &mut handle.db, &mut bus);
        let intents = if confirm_first {
            [Intent::Confirm, Intent::StepBack]
        } else {
            [Intent::StepBack, Intent::Confirm]
        };
        for intent in intents {
            producer::resolve_intent(intent, &state, &mut handle.db, &bus.sender());
        }
        let accepted = drain_apply(&mut bus, &mut state, &mut handle.db);
        assert_eq!(accepted.len(), 1);
        dispatcher.dispatch(&accepted, &mut handle.db, &bus.sender());
        if confirm_first {
            settle_until(&mut bus, &mut state, &mut handle.db, |state| {
                matches!(state.blocks[0].state, BlockState::Recording { .. })
            });
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
            assert!(matches!(state.blocks[0].state, BlockState::PickingApp(_)));
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            for model in english().models() {
                assert!(downloads::get(&handle.db, model.id).unwrap().is_none());
            }
        }
    }
}

#[test]
fn back_is_ignored_outside_source_pickers_and_menu_intercepts_it() {
    let mut bus = EventBus::new();
    let mut handle = downloads_with(&bus);
    let mut state = source_state();
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
        state.blocks[0].state = phase;
        producer::resolve_intent(Intent::StepBack, &state, &mut handle.db, &bus.sender());
        assert_eq!(bus.drain().count(), 0);
    }
    state = UiState::default();
    state.apply(&AppEvent::AddBlock);
    producer::resolve_intent(Intent::StepBack, &state, &mut handle.db, &bus.sender());
    assert_eq!(bus.drain().count(), 0);
    state = source_state();
    source_intent(Intent::Confirm, &mut state, &mut handle.db, &mut bus);
    state.apply(&AppEvent::MenuOpened);
    producer::resolve_intent(Intent::StepBack, &state, &mut handle.db, &bus.sender());
    assert_eq!(bus.drain().count(), 0);
}

#[test]
fn empty_apps_cannot_confirm_and_close_resets_reused_id() {
    let mut bus = EventBus::new();
    let mut handle = downloads_with(&bus);
    let mut state = source_state();
    state.blocks[0]
        .source
        .as_mut()
        .unwrap()
        .snapshot
        .apps
        .clear();
    source_intent(Intent::PickerNext, &mut state, &mut handle.db, &mut bus);
    source_intent(Intent::Confirm, &mut state, &mut handle.db, &mut bus);
    producer::resolve_intent(Intent::Confirm, &state, &mut handle.db, &bus.sender());
    assert_eq!(bus.drain().count(), 0);
    source_intent(Intent::BlockClosed, &mut state, &mut handle.db, &mut bus);
    assert!(state.blocks.is_empty());
    state.next_block_id = 1;
    state.apply(&AppEvent::AddSourceBlock {
        snapshot: voice_bird_next::testing::sample_source_snapshot(),
    });
    source_intent(Intent::Confirm, &mut state, &mut handle.db, &mut bus);
    assert!(matches!(state.blocks[0].state, BlockState::Picking(_)));
    assert_eq!(state.blocks[0].source.as_ref().unwrap().rev, 1);
}

#[test]
fn queued_confirm_then_close_clears_steps_before_block_id_reuse() {
    let mut bus = EventBus::new();
    let mut handle = downloads_with(&bus);
    let mut state = source_state();
    // Both intents observe Device/0, before either event has been applied.
    producer::resolve_intent(Intent::Confirm, &state, &mut handle.db, &bus.sender());
    producer::resolve_intent(Intent::BlockClosed, &state, &mut handle.db, &bus.sender());
    let accepted = drain_apply(&mut bus, &mut state, &mut handle.db);
    assert!(matches!(
        accepted.as_slice(),
        [AppEvent::SourceStepChanged { .. }, AppEvent::BlockClosed { block: 1 }]
    ));
    assert!(state.blocks.is_empty());
    state.next_block_id = 1;
    state.apply(&AppEvent::AddSourceBlock {
        snapshot: voice_bird_next::testing::sample_source_snapshot(),
    });
    source_intent(Intent::Confirm, &mut state, &mut handle.db, &mut bus);
    assert!(matches!(state.blocks[0].state, BlockState::Picking(_)));
    assert_eq!(state.blocks[0].source.as_ref().unwrap().rev, 1);
}

#[test]
fn add_block_with_menu_open_uses_catalog_and_focuses_new_source_picker() {
    let tmp = tempfile::tempdir().unwrap();
    let dispatcher = Dispatcher::new(
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
    let mut state = UiState::default();
    state.apply(&AppEvent::MenuOpened);
    producer::resolve_intent(Intent::AddBlock, &state, &mut handle.db, &bus.sender());
    let commands = drain_apply(&mut bus, &mut state, &mut handle.db);
    assert_eq!(commands, vec![AppEvent::RequestBlock]);
    dispatcher.dispatch(&commands, &mut handle.db, &bus.sender());
    settle_until(&mut bus, &mut state, &mut handle.db, |state| {
        matches!(state.blocks.first().map(|block| &block.state), Some(BlockState::PickingDevice(_)))
    });
    assert!(matches!(
        state.focused().unwrap().state,
        BlockState::PickingDevice(_)
    ));
    assert_eq!(
        state.focused().unwrap().source.as_ref().unwrap().snapshot,
        voice_bird_next::testing::sample_source_snapshot()
    );
}
