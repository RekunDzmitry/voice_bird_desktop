//! End-to-end language download flow tests. No test touches the network.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use voice_bird_next::bus::{AppEvent, DownloadStatus, EventBus};
use voice_bird_next::db::downloads::CancelCheck;
use voice_bird_next::db::{downloads, Database};
use voice_bird_next::dispatcher::Dispatcher;
use voice_bird_next::download::{DownloadError, Downloader};
use voice_bird_next::input::Intent;
use voice_bird_next::language::{LanguageProfile, LANGUAGES};
use voice_bird_next::producer;
use voice_bird_next::state::{BlockState, UiState};
use voice_bird_next::testing::{FixtureDownloader, FixtureStore, Outcome};
use voice_bird_next::transcription_models::{handler_for, CacheDirStore, ModelStore};

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
    for event in &events {
        if downloads::apply(db, event).unwrap_or(false) {
            state.apply(event);
        }
    }
    events
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
    let dispatcher = Dispatcher::new(downloader, store);
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
    let dispatcher = Dispatcher::new(downloader, store);
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
    let failing_dispatcher = Dispatcher::new(failing, store.clone());
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
    let retry_dispatcher = Dispatcher::new(successful, store);
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
        BlockState::Failed { language, error }
            if *language == english() && error == "database locked"
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
    let dispatcher = Dispatcher::new(downloader, store);
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
    let dispatcher = Dispatcher::new(downloader, store);
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
    let dispatcher = Dispatcher::new(downloader, store);
    let mut bus = EventBus::new();
    let mut handle = downloads_with(&bus);
    let mut state = UiState::default();
    state.apply(&AppEvent::AddBlock);
    state.apply(&AppEvent::AddBlock);
    assert_eq!(state.focus, 1);

    bus.sender().publish(AppEvent::BeginLanguage {
        block: 1,
        language: english(),
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
    let dispatcher = Dispatcher::new(downloader, store);
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
    let second_dispatcher = Dispatcher::new(second_downloader, second_store);
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
    let dispatcher = Dispatcher::new(downloader, store);
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
fn every_catalog_format_has_a_handler() {
    for model in voice_bird_next::picker::CATALOG {
        let _ = handler_for(model.format);
    }
}
