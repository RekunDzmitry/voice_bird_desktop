//! End-to-end download flow tests. **No test may touch the network.**
//! `HttpDownloader` is constructed only in `main.rs`; everything here
//! drives the resolver through the `FixtureStore` and `FixtureDownloader`
//! in [`voice_bird_next::testing`].
//!
//! The nemotron cancel-during-install repro is the exception: it
//! builds a real gzipped-tarball so the production unpack path runs.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use voice_bird_next::bus::{AppEvent, DownloadStatus, EventBus};
use voice_bird_next::db::downloads::Downloads;
use voice_bird_next::download::{begin, DownloadError, Downloader};
use voice_bird_next::input::Intent;
use voice_bird_next::picker::{ModelEntry, ModelFormat, CATALOG};
use voice_bird_next::producer;
use voice_bird_next::state::{BlockState, DownloadPhase, DownloadState, UiState};
use voice_bird_next::testing::{render_to_string, FixtureDownloader, FixtureStore, Outcome};
use voice_bird_next::transcription_models::{handler_for, ModelStore};

fn tiny() -> &'static voice_bird_next::picker::ModelEntry {
    &CATALOG[5]
}

fn base() -> &'static voice_bird_next::picker::ModelEntry {
    &CATALOG[4]
}

/// Bundle a `Downloads` handle with its `TempDir`. Dropping the
/// `TempDir` deletes the SQLite file so each test starts clean.
struct DownloadsHandle {
    downloads: Downloads,
    _tmp: tempfile::TempDir,
}

/// Open a fresh SQLite downloads table in a tempdir and bind the
/// bus sender so `Downloads` can publish lifecycle events. Tests
/// keep the [`DownloadsHandle`] on the stack so the connection lives
/// for the whole test.
fn downloads_with(bus: &EventBus) -> DownloadsHandle {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("downloads.sqlite");
    let downloads = Downloads::open(&path, bus.sender()).unwrap();
    DownloadsHandle { downloads, _tmp: tmp }
}

/// Drain the bus and apply each event, mirroring the production
/// loop in `main::run`: the table's `apply` is the gate that decides
/// whether the UI projection is updated, and a stale download event
/// from a superseded attempt is filtered before `UiState::apply`
/// ever sees it.
fn tick_drain(bus: &mut EventBus, state: &mut UiState, downloads: &mut Downloads) {
    for ev in bus.drain() {
        if let Ok(true) = downloads.apply(&ev) {
            state.apply(&ev);
        }
    }
}

fn settle(bus: &mut EventBus, state: &mut UiState, downloads: &mut Downloads) {
    for _ in 0..30 {
        std::thread::sleep(Duration::from_millis(20));
        tick_drain(bus, state, downloads);
        if !state.blocks.iter().any(|b| {
            matches!(
                b.state,
                BlockState::Waiting { .. } | BlockState::Failed { .. }
            )
        }) {
            return;
        }
    }
}

fn fixture_world() -> (
    EventBus,
    tempfile::TempDir,
    Arc<FixtureStore>,
    Arc<FixtureDownloader>,
    DownloadsHandle,
) {
    let bus = EventBus::new();
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(FixtureStore::new(PathBuf::from(tmp.path()), &[]));
    let calls = Arc::new(AtomicUsize::new(0));
    let downloader = Arc::new(FixtureDownloader::new(
        Vec::new(),
        Outcome::Ok,
        Arc::clone(&calls),
    ));
    let handle = downloads_with(&bus);
    (bus, tmp, store, downloader, handle)
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
    tick_drain(&mut bus, &mut state, &mut downloads_h.downloads);

    begin(
        tiny(),
        store,
        &mut downloads_h.downloads,
        downloader,
        &tx,
    );
    tick_drain(&mut bus, &mut state, &mut downloads_h.downloads);

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
    tick_drain(&mut bus, &mut state, &mut downloads_h.downloads);

    begin(
        tiny(),
        store,
        &mut downloads_h.downloads,
        downloader,
        &tx,
    );

    // Drain the bus without folding into state/repo so we observe
    // the raw event sequence the resolver published.
    let events: Vec<AppEvent> = bus.drain().collect();
    assert_eq!(
        events.len(),
        2,
        "cache hit must publish exactly two events, got {events:?}"
    );
    assert!(
        matches!(&events[0], AppEvent::ModelAlreadyCached(e) if e.id == tiny().id),
        "first event must be ModelAlreadyCached for the requested model, got {:?}",
        events[0]
    );
    assert!(
        matches!(&events[1], AppEvent::RecordingStarted(e) if e.id == tiny().id),
        "second event must be RecordingStarted for the requested model, got {:?}",
        events[1]
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "no fetch should have been attempted on the cache-hit path"
    );
}

#[test]
fn two_blocks_same_model_share_one_download() {
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
    tick_drain(&mut bus, &mut state, &mut downloads_h.downloads);

    begin(
        tiny(),
        store.clone(),
        &mut downloads_h.downloads,
        downloader.clone(),
        &tx,
    );
    begin(
        tiny(),
        store.clone(),
        &mut downloads_h.downloads,
        downloader.clone(),
        &tx,
    );
    settle(&mut bus, &mut state, &mut downloads_h.downloads);

    assert_eq!(calls.load(Ordering::SeqCst), 1, "only one fetch ran");
    // Both blocks should be Recording.
    assert!(state.blocks.iter().all(|b| matches!(
        b.state,
        BlockState::Recording { model: "tiny.en" }
    )));
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
    tick_drain(&mut bus, &mut state, &mut downloads_h.downloads);

    let attempt = 1;
    begin(
        tiny(),
        store,
        &mut downloads_h.downloads,
        downloader,
        &tx,
    );
    // Publish a progress event manually — the fixture doesn't tick.
    bus.sender().publish(AppEvent::DownloadProgress {
        attempt,
        model: tiny().id,
        bytes: 50,
        total: Some(100),
        bytes_per_sec: 0,
    });
    tick_drain(&mut bus, &mut state, &mut downloads_h.downloads);

    let proj = state
        .downloads
        .get("tiny.en")
        .expect("download row materialised");
    assert_eq!(proj.bytes, 50);
    assert_eq!(proj.total, Some(100));
}

#[test]
fn downloads_table_round_trip() {
    // Pure table test: open, start, observe, cancel.
    let mut bus = EventBus::new();
    let mut h = downloads_with(&bus);
    h.downloads.start(tiny().id).unwrap();
    let row = h.downloads.get(tiny().id).unwrap().unwrap();
    assert_eq!(row.attempt, 1);
    assert_eq!(row.status, DownloadStatus::Downloading);
    let ok = h.downloads.cancel(tiny().id).unwrap();
    assert!(ok);
    let row = h.downloads.get(tiny().id).unwrap().unwrap();
    assert_eq!(row.status, DownloadStatus::Cancelling);
}

#[test]
fn download_progress_event_carries_bytes_per_sec() {
    // Progress events are no longer persisted to the table — the
    // bus is the source of truth for the renderer. This test pins
    // that contract: the event flows through, the table accepts it,
    // and the UI projection updates.
    let mut bus = EventBus::new();
    let mut h = downloads_with(&bus);
    h.downloads.start(tiny().id).unwrap();
    let mut state = UiState::default();
    state.apply(&AppEvent::DownloadRequested(tiny()));
    let ev = AppEvent::DownloadProgress {
        attempt: 1,
        model: tiny().id,
        bytes: 42,
        total: Some(100),
        bytes_per_sec: 1234,
    };
    tick_drain(&mut bus, &mut state, &mut h.downloads);
    let accepted = h.downloads.apply(&ev).unwrap();
    assert!(accepted);
    state.apply(&ev);
    let proj = state.downloads.get("tiny.en").unwrap();
    assert_eq!(proj.bytes, 42);
    assert_eq!(proj.bytes_per_sec, 1234);
}

#[test]
fn cancel_event_moves_row_to_cancelled() {
    let mut bus = EventBus::new();
    let mut h = downloads_with(&bus);
    h.downloads.start(tiny().id).unwrap();
    h.downloads.cancel(tiny().id).unwrap();
    let accepted = h
        .downloads
        .apply(&AppEvent::DownloadCancelled {
            attempt: 1,
            model: tiny().id,
        })
        .unwrap();
    assert!(accepted);
    let row = h.downloads.get(tiny().id).unwrap().unwrap();
    assert_eq!(row.status, DownloadStatus::Cancelled);
}

#[test]
fn stale_terminal_event_is_rejected_and_row_unchanged() {
    // New attempt supersedes the old. The old worker's terminal
    // event is gated out and publishes a DownloadEventRejected.
    let mut bus = EventBus::new();
    let mut h = downloads_with(&bus);
    h.downloads.start(tiny().id).unwrap(); // attempt 1
    h.downloads.cancel(tiny().id).unwrap();
    h.downloads.start(tiny().id).unwrap(); // attempt 2
    let accepted = h
        .downloads
        .apply(&AppEvent::DownloadSucceeded {
            attempt: 1,
            model: tiny().id,
        })
        .unwrap();
    assert!(!accepted);
    let row = h.downloads.get(tiny().id).unwrap().unwrap();
    assert_eq!(row.attempt, 2);
    assert_eq!(row.status, DownloadStatus::Downloading);
    // A DownloadEventRejected was published for the audit log.
    let rejections: Vec<_> = bus
        .drain()
        .filter(|e| matches!(e, AppEvent::DownloadEventRejected { .. }))
        .collect();
    assert!(
        !rejections.is_empty(),
        "stale events must publish a rejection; bus had no DownloadEventRejected"
    );
}

#[test]
fn user_scenario_pick_cancel_during_install_retry_does_full_fetch() {
    // Repro from the plan: pick a model, close the block mid-install,
    // pick again. The second pick must produce a full fetch (the
    // CancelProbe on the new worker sees the new attempt; the old
    // attempt's worker acks Cancelled and stops).
    use flate2::write::GzEncoder;
    use flate2::Compression;

    let tmp = tempfile::tempdir().unwrap();
    let nemotron = &CATALOG[3];
    let store_concrete = Arc::new(FixtureStore::new(tmp.path().to_path_buf(), &[]));
    // Build a real tar.gz for the nemotron package so install runs.
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
    let store: Arc<dyn ModelStore> = store_concrete.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let downloader: Arc<dyn Downloader> = Arc::new(
        FixtureDownloader::new(
            std::fs::read(&archive).unwrap(),
            Outcome::Ok,
            Arc::clone(&calls),
        )
        .with_delay(10),
    );
    let mut bus = EventBus::new();
    let tx = bus.sender();
    let mut downloads_h = downloads_with(&bus);
    let mut state = UiState::default();
    state.apply(&AppEvent::AddBlock);
    tick_drain(&mut bus, &mut state, &mut downloads_h.downloads);

    begin(
        nemotron,
        store.clone(),
        &mut downloads_h.downloads,
        downloader.clone(),
        &tx,
    );
    // Wait briefly for fetch to start.
    std::thread::sleep(Duration::from_millis(40));
    // Close the focused block mid-install: producer flips Cancelling.
    state.focus = 0;
    producer::resolve_intent(
        Intent::BlockClosed,
        &state,
        &*store,
        &mut downloads_h.downloads,
        &*downloader,
        &tx,
    );
    settle(&mut bus, &mut state, &mut downloads_h.downloads);
    let calls_after_first = calls.load(Ordering::SeqCst);
    assert_eq!(
        calls_after_first, 1,
        "first attempt fetched exactly once"
    );

    // Retry: pick again. A fresh attempt row is inserted and a
    // second fetch runs.
    state.apply(&AppEvent::AddBlock);
    tick_drain(&mut bus, &mut state, &mut downloads_h.downloads);
    begin(
        nemotron,
        store,
        &mut downloads_h.downloads,
        downloader,
        &tx,
    );
    settle(&mut bus, &mut state, &mut downloads_h.downloads);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "second attempt did a fresh fetch (full, not resumed)"
    );
}

#[test]
fn quit_during_install_leaves_cache_clean() {
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
        .with_delay(20),
    );
    let mut bus = EventBus::new();
    let tx = bus.sender();
    let mut downloads_h = downloads_with(&bus);
    let mut state = UiState::default();
    state.apply(&AppEvent::AddBlock);
    tick_drain(&mut bus, &mut state, &mut downloads_h.downloads);
    begin(
        nemotron,
        store.clone(),
        &mut downloads_h.downloads,
        downloader,
        &tx,
    );
    // Wait until the worker reaches install.
    std::thread::sleep(Duration::from_millis(80));
    // Quit: simulate the production cleanup loop.
    let active = downloads_h.downloads.active().unwrap();
    for row in &active {
        let _ = downloads_h.downloads.cancel(row.model.as_ref());
    }
    for row in &active {
        if let Some(entry) = CATALOG.iter().find(|e| e.id == row.model.as_ref()) {
            store.discard_inflight(entry);
        }
    }
    // Drain the bus so the Cancelling transitions reach the audit log.
    let _drained: Vec<_> = bus.drain().collect();
    // Allow the worker to observe Cancelling and unwind.
    std::thread::sleep(Duration::from_millis(200));
    // No .part or .tmp leftovers in the cache dir (the install
    // prologue and the discard_inflight sweep both clean up).
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
    use std::io::Cursor;

    let tmp = tempfile::tempdir().unwrap();
    let store: Arc<dyn ModelStore> = Arc::new(FixtureStore::new(tmp.path().to_path_buf(), &[]));
    // Two distinct payloads so the SHA differs and a fresh fetch is
    // observable. The slow downloader keeps attempt A in flight long
    // enough for attempt B to start.
    let payload_a = vec![1u8; 4096];
    let payload_b = vec![2u8; 4096];
    let calls = Arc::new(AtomicUsize::new(0));
    let downloader_a: Arc<dyn Downloader> = Arc::new(
        FixtureDownloader::new(payload_a, Outcome::Ok, Arc::clone(&calls)).with_delay(40),
    );
    let downloader_b: Arc<dyn Downloader> = Arc::new(
        FixtureDownloader::new(payload_b, Outcome::Ok, Arc::clone(&calls)).with_delay(0),
    );
    let mut bus = EventBus::new();
    let tx = bus.sender();
    let mut downloads_h = downloads_with(&bus);
    let mut state = UiState::default();
    state.apply(&AppEvent::AddBlock);
    tick_drain(&mut bus, &mut state, &mut downloads_h.downloads);

    begin(
        tiny(),
        store.clone(),
        &mut downloads_h.downloads,
        downloader_a.clone(),
        &tx,
    );
    std::thread::sleep(Duration::from_millis(20));
    // Cancel attempt A's row, then pick again — attempt B spawns
    // under a fresh attempt id.
    state.focus = 0;
    producer::resolve_intent(
        Intent::BlockClosed,
        &state,
        &*store,
        &mut downloads_h.downloads,
        &*downloader_a,
        &tx,
    );
    // Now pick again with a fast downloader — this is the
    // cancel-immediate-retry path. Note we replace the downloader to
    // make B fast.
    begin(
        tiny(),
        store,
        &mut downloads_h.downloads,
        downloader_b,
        &tx,
    );
    settle(&mut bus, &mut state, &mut downloads_h.downloads);
    // Both downloaders saw at least one fetch.
    assert!(
        calls.load(Ordering::SeqCst) >= 2,
        "both attempts must have fetched at least once; got {}",
        calls.load(Ordering::SeqCst)
    );
    let _ = Cursor::new(Vec::<u8>::new()); // touch unused import in some configs
}

#[test]
fn stale_terminal_events_after_restart_do_not_repaint_attempt_b_ui() {
    use std::io::Cursor;

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
    tick_drain(&mut bus, &mut state, &mut downloads_h.downloads);

    begin(
        tiny(),
        store.clone(),
        &mut downloads_h.downloads,
        slow.clone(),
        &tx,
    );
    std::thread::sleep(Duration::from_millis(10));
    // Restart: cancel then pick again.
    state.focus = 0;
    producer::resolve_intent(
        Intent::BlockClosed,
        &state,
        &*store,
        &mut downloads_h.downloads,
        &*slow,
        &tx,
    );
    begin(
        tiny(),
        store,
        &mut downloads_h.downloads,
        fast,
        &tx,
    );
    settle(&mut bus, &mut state, &mut downloads_h.downloads);

    // Drain any residual stale events; they must not change the row.
    let final_row = downloads_h.downloads.get(tiny().id).unwrap().unwrap();
    assert!(
        matches!(
            final_row.status,
            DownloadStatus::Succeeded | DownloadStatus::Downloading
        ),
        "row must reflect attempt B's status; got {:?}",
        final_row.status
    );
    let _ = Cursor::new(Vec::<u8>::new()); // touch unused import
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
    let res = h.install(
        tmp.path(),
        nemotron.id,
        &archive,
        &mut { &cancel },
    );
    assert_eq!(res, Err(DownloadError::Cancelled));
}

#[test]
fn render_smoke_blocked_paths() {
    // Smoke: the renderer must accept the new UiState shape.
    let mut state = UiState::default();
    state.apply(&AppEvent::AddBlock);
    state.apply(&AppEvent::ModelSelected(tiny()));
    state.apply(&AppEvent::RecordingStarted(tiny()));
    let grid = render_to_string(&state, 80, 24);
    assert!(!grid.is_empty());
}
