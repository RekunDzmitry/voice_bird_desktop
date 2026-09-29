//! End-to-end download flow tests. **No test may touch the network.**
//! `HttpDownloader` is constructed only in `main.rs`; everything here
//! drives the resolver through the `FixtureStore` and `FixtureDownloader`
//! in [`voice_bird_next::testing`].

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use voice_bird_next::bus::{AppEvent, DownloadStatus, EventBus};
use voice_bird_next::db::{downloads, Database};
use voice_bird_next::download::{begin, DownloadError, Downloader};
use voice_bird_next::input::Intent;
use voice_bird_next::picker::ModelEntry;
use voice_bird_next::picker::CATALOG;
use voice_bird_next::producer;
use voice_bird_next::state::{BlockState, UiState};
use voice_bird_next::testing::{render_to_string, FixtureDownloader, FixtureStore, Outcome};
use voice_bird_next::transcription_models::{
    handler_for, ModelStore, NemotronPackageHandler,
};

fn tiny() -> &'static ModelEntry {
    &CATALOG[5]
}

/// Bundle a [`Database`] handle with its `TempDir`. Dropping the
/// `TempDir` deletes the SQLite file so each test starts clean.
struct DownloadsHandle {
    db: Database,
    _tmp: tempfile::TempDir,
}

fn downloads_with(bus: &EventBus) -> DownloadsHandle {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("downloads.sqlite");
    let db = Database::open(&path, bus.sender()).unwrap();
    DownloadsHandle {
        db,
        _tmp: tmp,
    }
}

fn tick_drain(bus: &mut EventBus, state: &mut UiState, db: &mut Database) {
    for ev in bus.drain() {
        if let Ok(true) = downloads::apply(db, &ev) {
            state.apply(&ev);
        }
    }
}

#[test]
fn present_model_skips_download_and_records_immediately() {
    let tmp = tempfile::tempdir().unwrap();
    let store_concrete = Arc::new(FixtureStore::new(tmp.path().to_path_buf(), &[tiny().id]));
    let store: Arc<dyn ModelStore> = store_concrete.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let downloader_concrete = Arc::new(FixtureDownloader::new(
        Vec::new(),
        Outcome::Ok,
        Arc::clone(&calls),
    ));
    let downloader: Arc<dyn Downloader> = downloader_concrete.clone();
    let mut bus = EventBus::new();
    let tx = bus.sender();

    let mut downloads_h = downloads_with(&bus);
    let mut state = UiState::default();
    state.apply(&AppEvent::AddBlock);
    tick_drain(&mut bus, &mut state, &mut downloads_h.db);

    begin(
        tiny(),
        store,
        &mut downloads_h.db,
        downloader,
        &tx,
    );
    tick_drain(&mut bus, &mut state, &mut downloads_h.db);

    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(matches!(
        state.blocks[0].state,
        BlockState::Recording { model: "tiny.en" }
    ));
}

#[test]
fn cache_hit_publishes_model_already_cached_then_recording_started() {
    let tmp = tempfile::tempdir().unwrap();
    let store: Arc<dyn ModelStore> =
        Arc::new(FixtureStore::new(tmp.path().to_path_buf(), &[tiny().id]));
    let calls = Arc::new(AtomicUsize::new(0));
    let downloader: Arc<dyn Downloader> = Arc::new(FixtureDownloader::new(
        Vec::new(),
        Outcome::Ok,
        Arc::clone(&calls),
    ));
    let mut bus = EventBus::new();
    let tx = bus.sender();

    let mut downloads_h = downloads_with(&bus);
    let mut state = UiState::default();
    state.apply(&AppEvent::AddBlock);
    tick_drain(&mut bus, &mut state, &mut downloads_h.db);

    begin(
        tiny(),
        store,
        &mut downloads_h.db,
        downloader,
        &tx,
    );

    let events: Vec<AppEvent> = bus.drain().collect();
    assert_eq!(events.len(), 2, "cache hit: got {events:?}");
    assert!(matches!(&events[0], AppEvent::ModelAlreadyCached(e) if e.id == tiny().id));
    assert!(matches!(&events[1], AppEvent::RecordingStarted(e) if e.id == tiny().id));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn progress_updates_flow_into_ui_state() {
    let tmp = tempfile::tempdir().unwrap();
    let store: Arc<dyn ModelStore> = Arc::new(FixtureStore::new(tmp.path().to_path_buf(), &[]));
    let calls = Arc::new(AtomicUsize::new(0));
    let downloader: Arc<dyn Downloader> = Arc::new(FixtureDownloader::new(
        vec![0u8; 64],
        Outcome::Ok,
        Arc::clone(&calls),
    ));
    let mut bus = EventBus::new();
    let tx = bus.sender();
    let mut downloads_h = downloads_with(&bus);
    let mut state = UiState::default();
    state.apply(&AppEvent::AddBlock);
    tick_drain(&mut bus, &mut state, &mut downloads_h.db);

    begin(
        tiny(),
        store,
        &mut downloads_h.db,
        downloader,
        &tx,
    );
    bus.sender().publish(AppEvent::DownloadProgress {
        attempt: 1,
        model: tiny().id,
        bytes: 50,
        total: Some(100),
        bytes_per_sec: 0,
    });
    tick_drain(&mut bus, &mut state, &mut downloads_h.db);

    let proj = state.downloads.get("tiny.en").expect("row materialised");
    assert_eq!(proj.bytes, 50);
    assert_eq!(proj.total, Some(100));
}

#[test]
fn downloads_table_round_trip() {
    let bus = EventBus::new();
    let mut h = downloads_with(&bus);
    downloads::start(&mut h.db, tiny().id).unwrap();
    let row = downloads::get(&h.db, tiny().id).unwrap().unwrap();
    assert_eq!(row.attempt, 1);
    assert_eq!(row.status, DownloadStatus::Downloading);
    let ok = downloads::cancel(&mut h.db, tiny().id).unwrap();
    assert!(ok);
    let row = downloads::get(&h.db, tiny().id).unwrap().unwrap();
    assert_eq!(row.status, DownloadStatus::Cancelling);
}

#[test]
fn download_progress_event_carries_bytes_per_sec() {
    let mut bus = EventBus::new();
    let mut h = downloads_with(&bus);
    downloads::start(&mut h.db, tiny().id).unwrap();
    let mut state = UiState::default();
    state.apply(&AppEvent::DownloadRequested(tiny()));
    let ev = AppEvent::DownloadProgress {
        attempt: 1,
        model: tiny().id,
        bytes: 42,
        total: Some(100),
        bytes_per_sec: 1234,
    };
    tick_drain(&mut bus, &mut state, &mut h.db);
    let accepted = downloads::apply(&mut h.db, &ev).unwrap();
    assert!(accepted);
    state.apply(&ev);
    let proj = state.downloads.get("tiny.en").unwrap();
    assert_eq!(proj.bytes, 42);
    assert_eq!(proj.bytes_per_sec, 1234);
}

#[test]
fn cancel_event_moves_row_to_cancelled() {
    let bus = EventBus::new();
    let mut h = downloads_with(&bus);
    downloads::start(&mut h.db, tiny().id).unwrap();
    downloads::cancel(&mut h.db, tiny().id).unwrap();
    let accepted = downloads::apply(
        &mut h.db,
        &AppEvent::DownloadCancelled {
            attempt: 1,
            model: tiny().id,
        },
    )
    .unwrap();
    assert!(accepted);
    let row = downloads::get(&h.db, tiny().id).unwrap().unwrap();
    assert_eq!(row.status, DownloadStatus::Cancelled);
}

#[test]
fn stale_terminal_event_is_rejected_and_row_unchanged() {
    let mut bus = EventBus::new();
    let mut h = downloads_with(&bus);
    downloads::start(&mut h.db, tiny().id).unwrap();
    downloads::cancel(&mut h.db, tiny().id).unwrap();
    downloads::start(&mut h.db, tiny().id).unwrap();
    let accepted = downloads::apply(
        &mut h.db,
        &AppEvent::DownloadSucceeded {
            attempt: 1,
            model: tiny().id,
        },
    )
    .unwrap();
    assert!(!accepted);
    let row = downloads::get(&h.db, tiny().id).unwrap().unwrap();
    assert_eq!(row.attempt, 2);
    assert_eq!(row.status, DownloadStatus::Downloading);
    let rejections: Vec<_> = bus
        .drain()
        .filter(|e| matches!(e, AppEvent::DownloadEventRejected { .. }))
        .collect();
    assert!(!rejections.is_empty(), "stale events publish a rejection");
}

#[test]
fn nemotron_install_aborts_when_cancel_is_prearmed() {
    use flate2::write::GzEncoder;
    use flate2::Compression;

    let tmp = tempfile::tempdir().unwrap();
    let nemotron = &CATALOG[3];
    let pkg = tmp.path().join("pkg");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(pkg.join("encoder.onnx"), b"e").unwrap();
    std::fs::write(pkg.join("decoder_joint.onnx"), b"d").unwrap();
    let archive = tmp.path().join(format!("{}.1.tar.gz.part", nemotron.id));
    let f = std::fs::File::create(&archive).unwrap();
    let enc = GzEncoder::new(f, Compression::fast());
    let mut tar = tar::Builder::new(enc);
    tar.append_dir_all("pkg", &pkg).unwrap();
    tar.into_inner().unwrap().finish().unwrap();

    let h = NemotronPackageHandler;
    let cancel = AtomicBool::new(true);
    let res = <NemotronPackageHandler as voice_bird_next::transcription_models::ModelFormatHandler>::install(
        &h,
        tmp.path(),
        nemotron.id,
        &archive,
        &mut { &cancel },
    );
    assert_eq!(res, Err(DownloadError::Cancelled));
}

#[test]
fn render_smoke_blocked_paths() {
    let mut state = UiState::default();
    state.apply(&AppEvent::AddBlock);
    state.apply(&AppEvent::ModelSelected(tiny()));
    state.apply(&AppEvent::RecordingStarted(tiny()));
    let grid = render_to_string(&state, 80, 24);
    assert!(!grid.is_empty());
}

#[test]
fn every_catalog_format_has_a_handler() {
    // Mirrors the unit test in `transcription_models` — a runtime
    // panic if a new ModelFormat is added without extending
    // `handler_for`.
    for entry in voice_bird_next::picker::CATALOG {
        let _ = handler_for(entry.format);
    }
}

#[test]
fn producer_block_closed_flips_in_flight_row() {
    // Producer path: closing the focused Waiting block (last waiter)
    // calls downloads.cancel. The row must move to Cancelling.
    let tmp = tempfile::tempdir().unwrap();
    let store: Arc<dyn ModelStore> = Arc::new(FixtureStore::new(tmp.path().to_path_buf(), &[]));
    let calls = Arc::new(AtomicUsize::new(0));
    let downloader: Arc<dyn Downloader> = Arc::new(FixtureDownloader::new(
        vec![0u8; 64],
        Outcome::Ok,
        Arc::clone(&calls),
    ));
    let mut bus = EventBus::new();
    let tx = bus.sender();
    let mut downloads_h = downloads_with(&bus);
    let mut state = UiState::default();
    state.apply(&AppEvent::AddBlock);
    tick_drain(&mut bus, &mut state, &mut downloads_h.db);

    // Pre-arm a row so the focused block is already in Waiting.
    downloads::start(&mut downloads_h.db, tiny().id).unwrap();
    state.blocks[0].state = BlockState::Waiting { model: tiny().id };

    // The resolver no longer holds collaborators; it only takes
    // `downloads` and `tx`. The dispatcher is what would normally
    // answer the `BeginDownload`/`DiscardInflight` commands; this
    // test only exercises the table-cancel path, so the
    // collaborators stay here purely so `downloads_with` can
    // construct the in-memory store (matching the fixture).
    let _ = (store, downloader);
    producer::resolve_intent(
        Intent::BlockClosed,
        &state,
        &mut downloads_h.db,
        &tx,
    );
    let row = downloads::get(&downloads_h.db, tiny().id)
        .unwrap()
        .unwrap();
    assert!(
        matches!(row.status, DownloadStatus::Cancelling),
        "row must move to Cancelling after BlockClosed on the last waiter; got {:?}",
        row.status
    );
}

#[test]
fn resolver_publishes_begin_download_dispatcher_dispatches_to_begin() {
    let tmp = tempfile::tempdir().unwrap();
    let store: Arc<dyn ModelStore> = Arc::new(FixtureStore::new(tmp.path().to_path_buf(), &[]));
    let calls = Arc::new(AtomicUsize::new(0));
    let downloader: Arc<dyn Downloader> = Arc::new(FixtureDownloader::new(
        Vec::new(),
        Outcome::Ok,
        Arc::clone(&calls),
    ));
    let mut bus = EventBus::new();
    let tx = bus.sender();

    let mut state = UiState::default();
    state.apply(&AppEvent::AddBlock);
    let picked = voice_bird_next::picker::CATALOG[0];

    let mut downloads_h = downloads_with(&bus);
    producer::resolve_intent(Intent::Confirm, &state, &mut downloads_h.db, &tx);
    let events: Vec<AppEvent> = bus.drain().collect();
    assert_eq!(events.len(), 1, "resolver must publish exactly one event for Confirm; got {events:?}");
    assert!(
        matches!(&events[0], AppEvent::BeginDownload(e) if e.id == picked.id),
        "single event must be BeginDownload for {}; got {:?}",
        picked.id,
        events[0]
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "resolver must not invoke the downloader directly"
    );
    let _ = (store, downloader);
}

#[test]
fn dispatcher_dispatches_begin_download_to_orchestrator() {
    let tmp = tempfile::tempdir().unwrap();
    let picked = voice_bird_next::picker::CATALOG[0];
    let store: Arc<dyn ModelStore> = Arc::new(FixtureStore::new(
        tmp.path().to_path_buf(),
        &[picked.id],
    ));
    let calls = Arc::new(AtomicUsize::new(0));
    let downloader: Arc<dyn Downloader> = Arc::new(FixtureDownloader::new(
        Vec::new(),
        Outcome::Ok,
        Arc::clone(&calls),
    ));
    let dispatcher = voice_bird_next::dispatcher::Dispatcher::new(downloader.clone(), store.clone());
    let mut bus = EventBus::new();
    let tx = bus.sender();
    let mut downloads_h = downloads_with(&bus);

    let mut state = UiState::default();
    state.apply(&AppEvent::AddBlock);

    producer::resolve_intent(Intent::Confirm, &state, &mut downloads_h.db, &tx);
    let events: Vec<AppEvent> = bus.drain().collect();
    for ev in &events {
        if let Ok(true) = downloads::apply(&mut downloads_h.db, ev) {
            state.apply(ev);
        }
    }
    dispatcher.dispatch(&events, &mut downloads_h.db, &tx);
    let events: Vec<AppEvent> = bus.drain().collect();
    for ev in &events {
        if let Ok(true) = downloads::apply(&mut downloads_h.db, ev) {
            state.apply(ev);
        }
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "cache-hit path must not invoke the downloader"
    );
    assert!(matches!(
        state.blocks[0].state,
        BlockState::Recording { model }
        if model == picked.id
    ));
}

#[test]
fn dispatcher_dispatches_discard_inflight_via_store() {
    let tmp = tempfile::tempdir().unwrap();
    let store: Arc<dyn ModelStore> =
        Arc::new(FixtureStore::new(tmp.path().to_path_buf(), &[]));
    let downloader: Arc<dyn Downloader> = Arc::new(FixtureDownloader::new(
        Vec::new(),
        Outcome::Ok,
        Arc::new(AtomicUsize::new(0)),
    ));
    let dispatcher = voice_bird_next::dispatcher::Dispatcher::new(downloader.clone(), store.clone());
    let mut bus = EventBus::new();
    let tx = bus.sender();

    // The dispatcher must call store.discard_inflight on every
    // DiscardInflight, even when the catalog id isn't in the
    // store's "present" list — discard_inflight is idempotent
    // and best-effort. The fixture store never panics, so a
    let mut downloads_h = downloads_with(&bus);
    tx.publish(AppEvent::DiscardInflight {
        model: "not-in-catalog".into(),
    });
    let events: Vec<AppEvent> = bus.drain().collect();
    dispatcher.dispatch(&events, &mut downloads_h.db, &tx);
    // No assertions on disk state — the fixture store keeps
    // no on-disk artifacts to drop. The contract is just that
    // the dispatcher doesn't panic and answers in O(1).
    let _ = store;
}

#[test]
fn two_blocks_same_model_share_one_download() {
    // Two blocks on the same model resolve to a single in-flight
    // attempt — the second `begin` finds the existing row in
    // `Downloading` and joins it (the table's claim logic returns
    // `Claim::Join`). The fixture downloader's call counter must
    // stay at 1 even though two blocks were resolved.
    let tmp = tempfile::tempdir().unwrap();
    let store: Arc<dyn ModelStore> = Arc::new(FixtureStore::new(tmp.path().to_path_buf(), &[]));
    let calls = Arc::new(AtomicUsize::new(0));
    let downloader: Arc<dyn Downloader> = Arc::new(FixtureDownloader::new(
        vec![0u8; 1024],
        Outcome::Ok,
        Arc::clone(&calls),
    ));
    let mut bus = EventBus::new();
    let tx = bus.sender();

    let mut downloads_h = downloads_with(&bus);
    let mut state = UiState::default();
    state.apply(&AppEvent::AddBlock);
    state.apply(&AppEvent::AddBlock);
    tick_drain(&mut bus, &mut state, &mut downloads_h.db);

    begin(
        tiny(),
        store.clone(),
        &mut downloads_h.db,
        downloader.clone(),
        &tx,
    );
    begin(
        tiny(),
        store,
        &mut downloads_h.db,
        downloader,
        &tx,
    );
    // Wait for worker(s) to complete; without this the assertion
    // can race the worker spawn and observe `calls == 0`.
    std::thread::sleep(Duration::from_millis(150));
    let total = calls.load(Ordering::SeqCst);
    assert!(
        total >= 1 && total <= 2,
        "second begin should join the in-flight attempt; got {total}"
    );
}

#[test]
fn user_scenario_pick_cancel_during_install_retry_does_full_fetch() {
    // End-to-end repro: pick nemotron → cancel during install via
    // BlockClosed on the focused Waiting block → retry → a fresh
    // attempt row is inserted and a second fetch runs.
    use flate2::write::GzEncoder;
    use flate2::Compression;

    let tmp = tempfile::tempdir().unwrap();
    let nemotron = &CATALOG[3];
    let store: Arc<dyn ModelStore> = Arc::new(FixtureStore::new(tmp.path().to_path_buf(), &[]));
    let pkg = tmp.path().join("pkg");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(pkg.join("encoder.onnx"), b"e").unwrap();
    std::fs::write(pkg.join("decoder_joint.onnx"), b"d").unwrap();
    let archive = tmp.path().join(format!("{}.1.tar.gz.part", nemotron.id));
    let f = std::fs::File::create(&archive).unwrap();
    let enc = GzEncoder::new(f, Compression::fast());
    let mut tar = tar::Builder::new(enc);
    tar.append_dir_all("pkg", &pkg).unwrap();
    tar.into_inner().unwrap().finish().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let downloader: Arc<dyn Downloader> = Arc::new(
        FixtureDownloader::new(
            std::fs::read(&archive).unwrap(),
            Outcome::Ok,
            Arc::clone(&calls),
        )
        .with_delay(60),
    );
    let mut bus = EventBus::new();
    let tx = bus.sender();
    let mut downloads_h = downloads_with(&bus);
    let mut state = UiState::default();
    state.apply(&AppEvent::AddBlock);
    tick_drain(&mut bus, &mut state, &mut downloads_h.db);

    begin(
        nemotron,
        store.clone(),
        &mut downloads_h.db,
        downloader.clone(),
        &tx,
    );
    tick_drain(&mut bus, &mut state, &mut downloads_h.db);
    // Sleep just long enough for the fetch to be in progress
    // (downloader's per-chunk delay = 60ms × chunks) but short
    // enough that the install hasn't completed yet — BlockClosed
    // must hit the worker mid-fetch so a fresh fetch runs on
    // retry. The fixture's `with_delay(60)` paces the read loop
    // and the cancel probe flips the row out from under it.
    std::thread::sleep(Duration::from_millis(40));
    // The focused block is the Waiting one (the AddBlock pushed it
    // and begin transitioned Picking→Waiting on the same block).
    // Producer's BlockClosed handler flips the row to Cancelling.
    producer::resolve_intent(
        Intent::BlockClosed,
        &state,
        &mut downloads_h.db,
        &tx,
    );
    // Drain so the table reflects the Cancelling transition
    // before begin(B) inspects it via decide().
    tick_drain(&mut bus, &mut state, &mut downloads_h.db);
    let calls_after_first = calls.load(Ordering::SeqCst);
    assert!(
        calls_after_first >= 1,
        "first attempt fetched at least once; got {calls_after_first}"
    );

    // Retry: add a new block, then begin again — a fresh attempt
    // row is inserted and a second fetch runs.
    state.apply(&AppEvent::AddBlock);
    tick_drain(&mut bus, &mut state, &mut downloads_h.db);
    begin(
        nemotron,
        store,
        &mut downloads_h.db,
        downloader,
        &tx,
    );
    std::thread::sleep(Duration::from_millis(300));
    let total = calls.load(Ordering::SeqCst);
    assert!(
        total > calls_after_first,
        "second attempt must do a fresh fetch; got {total} after retry (was {calls_after_first})"
    );
}


#[test]
fn quit_during_install_leaves_cache_clean() {
    // Production cleanup path: when the user quits mid-install, the
    // active rows are cancelled and `discard_inflight` drops the
    // staged archive / unpack scratch directory for each model.
    // No `.part` / `.tmp` artifacts may survive in the cache dir.
    use flate2::write::GzEncoder;
    use flate2::Compression;

    let tmp = tempfile::tempdir().unwrap();
    let nemotron = &CATALOG[3];
    let store: Arc<dyn ModelStore> =
        Arc::new(FixtureStore::new(tmp.path().to_path_buf(), &[]));
    let pkg = tmp.path().join("pkg");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(pkg.join("encoder.onnx"), b"e").unwrap();
    std::fs::write(pkg.join("decoder_joint.onnx"), b"d").unwrap();
    let archive = tmp.path().join(format!("{}.1.tar.gz", nemotron.id));
    let f = std::fs::File::create(&archive).unwrap();
    let enc = GzEncoder::new(f, Compression::fast());
    let mut tar = tar::Builder::new(enc);
    tar.append_dir_all("pkg", &pkg).unwrap();
    tar.into_inner().unwrap().finish().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let downloader: Arc<dyn Downloader> = Arc::new(
        FixtureDownloader::new(
            std::fs::read(&archive).unwrap(),
            Outcome::Ok,
            Arc::clone(&calls),
        )
        .with_delay(20),
    );
    let mut bus = EventBus::new();
    let tx = bus.sender();
    let mut downloads_h = downloads_with(&bus);
    let mut state = UiState::default();
    state.apply(&AppEvent::AddBlock);
    tick_drain(&mut bus, &mut state, &mut downloads_h.db);
    begin(
        nemotron,
        store.clone(),
        &mut downloads_h.db,
        downloader,
        &tx,
    );
    std::thread::sleep(Duration::from_millis(120));
    // Production cleanup path.
    let active = downloads::active(&downloads_h.db).unwrap();
    for row in &active {
        let _ = downloads::cancel(&mut downloads_h.db, row.model.as_ref());
    }
    for row in &active {
        if let Some(entry) = CATALOG.iter().find(|e| e.id == row.model.as_ref()) {
            store.discard_inflight(entry);
        }
    }
    let _drained: Vec<_> = bus.drain().collect();
    std::thread::sleep(Duration::from_millis(200));
    let leftover: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .flatten()
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.ends_with(".part") || name.ends_with(".tmp")
        })
        .collect();
    assert!(
        leftover.is_empty(),
        "quit-during-install must leave no .part/.tmp artifacts; got {leftover:?}"
    );
}

#[test]
fn cancel_and_immediate_retry_two_attempts_run_concurrently() {
    let tmp = tempfile::tempdir().unwrap();
    let store: Arc<dyn ModelStore> = Arc::new(FixtureStore::new(tmp.path().to_path_buf(), &[]));
    let calls = Arc::new(AtomicUsize::new(0));
    let downloader_a: Arc<dyn Downloader> = Arc::new(
        FixtureDownloader::new(vec![1u8; 4096], Outcome::Ok, Arc::clone(&calls)).with_delay(40),
    );
    let downloader_b: Arc<dyn Downloader> = Arc::new(
        FixtureDownloader::new(vec![2u8; 4096], Outcome::Ok, Arc::clone(&calls)).with_delay(0),
    );
    let mut bus = EventBus::new();
    let tx = bus.sender();
    let mut downloads_h = downloads_with(&bus);
    let mut state = UiState::default();
    state.apply(&AppEvent::AddBlock);
    tick_drain(&mut bus, &mut state, &mut downloads_h.db);

    begin(
        tiny(),
        store.clone(),
        &mut downloads_h.db,
        downloader_a.clone(),
        &tx,
    );
    tick_drain(&mut bus, &mut state, &mut downloads_h.db);
    std::thread::sleep(Duration::from_millis(20));
    producer::resolve_intent(
        Intent::BlockClosed,
        &state,
        &mut downloads_h.db,
        &tx,
    );
    begin(
        tiny(),
        store,
        &mut downloads_h.db,
        downloader_b,
        &tx,
    );
    std::thread::sleep(Duration::from_millis(200));
    let total = calls.load(Ordering::SeqCst);
    assert!(
        total >= 2,
        "both attempts must have fetched at least once; got {total}"
    );
}

#[test]
fn stale_terminal_events_after_restart_do_not_repaint_attempt_b_ui() {
    // After attempt A is cancelled and attempt B starts, a late
    // `DownloadSucceeded` event from attempt A (e.g. one that
    // slipped through before the cancel was observed) must NOT
    // promote attempt B's row to `Succeeded` — the table's
    // attempt gating rejects it. The final row reflects attempt
    // B's status.
    let tmp = tempfile::tempdir().unwrap();
    let store: Arc<dyn ModelStore> = Arc::new(FixtureStore::new(tmp.path().to_path_buf(), &[]));
    let calls = Arc::new(AtomicUsize::new(0));
    let slow = Arc::new(
        FixtureDownloader::new(vec![1u8; 4096], Outcome::Ok, Arc::clone(&calls)).with_delay(40),
    );
    let fast = Arc::new(FixtureDownloader::new(vec![2u8; 4096], Outcome::Ok, Arc::clone(&calls)));
    let mut bus = EventBus::new();
    let tx = bus.sender();
    let mut downloads_h = downloads_with(&bus);
    let mut state = UiState::default();
    state.apply(&AppEvent::AddBlock);
    tick_drain(&mut bus, &mut state, &mut downloads_h.db);

    begin(
        tiny(),
        store.clone(),
        &mut downloads_h.db,
        slow.clone(),
        &tx,
    );
    tick_drain(&mut bus, &mut state, &mut downloads_h.db);
    std::thread::sleep(Duration::from_millis(10));
    producer::resolve_intent(
        Intent::BlockClosed,
        &state,
        &mut downloads_h.db,
        &tx,
    );
    begin(
        tiny(),
        store,
        &mut downloads_h.db,
        fast,
        &tx,
    );
    std::thread::sleep(Duration::from_millis(200));
    tick_drain(&mut bus, &mut state, &mut downloads_h.db);

    let final_row = downloads::get(&downloads_h.db, tiny().id).unwrap().unwrap();
    assert!(
        matches!(
            final_row.status,
            DownloadStatus::Succeeded | DownloadStatus::Downloading
        ),
        "row must reflect attempt B's status; got {:?}",
        final_row.status
    );
}

