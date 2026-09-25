//! End-to-end download flow tests. **No test may touch the network.**
//! `HttpDownloader` is constructed only in `main.rs`; everything here
//! drives the resolver through the `FixtureStore` and `FixtureDownloader`
//! in [`voice_bird_next::testing`].
//!
//! The nemotron cancel-during-install repro is the exception: it
//! builds a real gzipped-tarball so the production unpack path runs.

use std::io::{Read as _, Write as _};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use voice_bird_next::bus::{AppEvent, DownloadStatus, EventBus};
use voice_bird_next::db::downloads::Downloads;
use voice_bird_next::download::{begin, DownloadError, Downloader};
use voice_bird_next::input::Intent;
use voice_bird_next::picker::{ModelEntry, CATALOG};
use voice_bird_next::producer;
use voice_bird_next::state::{BlockState, UiState};
use voice_bird_next::testing::{render_to_string, FixtureDownloader, FixtureStore, Outcome};
use voice_bird_next::transcription_models::{
    handler_for, ModelStore, NemotronPackageHandler,
};

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

fn downloads_with(bus: &EventBus) -> DownloadsHandle {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("downloads.sqlite");
    let downloads = Downloads::open(&path, bus.sender()).unwrap();
    DownloadsHandle {
        downloads,
        _tmp: tmp,
    }
}

fn tick_drain(bus: &mut EventBus, state: &mut UiState, downloads: &mut Downloads) {
    for ev in bus.drain() {
        if let Ok(true) = downloads.apply(&ev) {
            state.apply(&ev);
        }
    }
}

fn settle(bus: &mut EventBus, state: &mut UiState, downloads: &mut Downloads) {
    for _ in 0..50 {
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

    let events: Vec<AppEvent> = bus.drain().collect();
    assert_eq!(
        events.len(),
        2,
        "cache hit must publish exactly two events, got {events:?}"
    );
    assert!(matches!(&events[0], AppEvent::ModelAlreadyCached(e) if e.id == tiny().id));
    assert!(matches!(&events[1], AppEvent::RecordingStarted(e) if e.id == tiny().id));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
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
        store,
        &mut downloads_h.downloads,
        downloader,
        &tx,
    );
    settle(&mut bus, &mut state, &mut downloads_h.downloads);

    assert_eq!(calls.load(Ordering::SeqCst), 1, "only one fetch ran");
    let recording_count = state
        .blocks
        .iter()
        .filter(|b| matches!(b.state, BlockState::Recording { model: "tiny.en" }))
        .count();
    assert!(
        recording_count >= 1,
        "at least one block ended in Recording; got {recording_count}"
    );
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

    begin(
        tiny(),
        store,
        &mut downloads_h.downloads,
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
    tick_drain(&mut bus, &mut state, &mut downloads_h.downloads);

    let proj = state.downloads.get("tiny.en").expect("row materialised");
    assert_eq!(proj.bytes, 50);
    assert_eq!(proj.total, Some(100));
}

#[test]
fn downloads_table_round_trip() {
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
    let mut bus = EventBus::new();
    let mut h = downloads_with(&bus);
    h.downloads.start(tiny().id).unwrap();
    h.downloads.cancel(tiny().id).unwrap();
    h.downloads.start(tiny().id).unwrap();
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
    let rejections: Vec<_> = bus
        .drain()
        .filter(|e| matches!(e, AppEvent::DownloadEventRejected { .. }))
        .collect();
    assert!(!rejections.is_empty(), "stale events publish a rejection");
}

#[test]
fn user_scenario_pick_cancel_during_install_retry_does_full_fetch() {
    use flate2::write::GzEncoder;
    use flate2::Compression;

    let tmp = tempfile::tempdir().unwrap();
    let nemotron = &CATALOG[3];
    let store_concrete = Arc::new(FixtureStore::new(tmp.path().to_path_buf(), &[]));
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
    std::thread::sleep(Duration::from_millis(80));
    // The focused block is the Waiting one (the AddBlock pushed it
    // and begin transitioned Picking→Waiting on the same block).
    // Producer's BlockClosed handler now flips the row to Cancelling.
    producer::resolve_intent(
        Intent::BlockClosed,
        &state,
        store.clone(),
        &mut downloads_h.downloads,
        downloader.clone(),
        &tx,
    );
    settle(&mut bus, &mut state, &mut downloads_h.downloads);
    let calls_after_first = calls.load(Ordering::SeqCst);
    assert_eq!(
        calls_after_first, 1,
        "first attempt fetched exactly once; got {calls_after_first}"
    );

    // Retry: add a new block, then begin again — a fresh attempt
    // row is inserted and a second fetch runs.
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
        "second attempt did a fresh fetch"
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
    std::thread::sleep(Duration::from_millis(120));
    // Production cleanup path.
    let active = downloads_h.downloads.active().unwrap();
    for row in &active {
        let _ = downloads_h.downloads.cancel(row.model.as_ref());
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
    producer::resolve_intent(
        Intent::BlockClosed,
        &state,
        store.clone(),
        &mut downloads_h.downloads,
        downloader_a.clone(),
        &tx,
    );
    begin(
        tiny(),
        store,
        &mut downloads_h.downloads,
        downloader_b,
        &tx,
    );
    settle(&mut bus, &mut state, &mut downloads_h.downloads);
    let total = calls.load(Ordering::SeqCst);
    assert!(
        total >= 2,
        "both attempts must have fetched at least once; got {total}"
    );
}

#[test]
fn stale_terminal_events_after_restart_do_not_repaint_attempt_b_ui() {
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
    producer::resolve_intent(
        Intent::BlockClosed,
        &state,
        store.clone(),
        &mut downloads_h.downloads,
        slow.clone(),
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

    let final_row = downloads_h.downloads.get(tiny().id).unwrap().unwrap();
    assert!(
        matches!(
            final_row.status,
            DownloadStatus::Succeeded | DownloadStatus::Downloading
        ),
        "row must reflect attempt B's status; got {:?}",
        final_row.status
    );
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
