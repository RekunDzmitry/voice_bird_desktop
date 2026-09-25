//! End-to-end download flow tests. **No test may touch the network.**
//! `HttpDownloader` is constructed only in `main.rs`; everything here
//! drives the resolver through the `FixtureStore` and `FixtureDownloader`
//! in [`voice_bird_next::testing`].

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use voice_bird_next::bus::{AppEvent, DownloadStatus, EventBus};
use voice_bird_next::db::downloads::Downloads;
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
    tick_drain(&mut bus, &mut state, &mut downloads_h.downloads);

    // Pre-arm a row so the focused block is already in Waiting.
    downloads_h.downloads.start(tiny().id).unwrap();
    state.blocks[0].state = BlockState::Waiting { model: tiny().id };

    producer::resolve_intent(
        Intent::BlockClosed,
        &state,
        store,
        &mut downloads_h.downloads,
        downloader,
        &tx,
    );
    let row = downloads_h
        .downloads
        .get(tiny().id)
        .unwrap()
        .unwrap();
    assert_eq!(row.status, DownloadStatus::Cancelling);
}
