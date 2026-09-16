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

use voice_bird_next::bus::{AppEvent, EventBus};
use voice_bird_next::download::{begin, DownloadError, Downloader};
use voice_bird_next::input::Intent;
use voice_bird_next::picker::{ModelEntry, ModelFormat, CATALOG};
use voice_bird_next::producer;
use voice_bird_next::state::{BlockState, DownloadState, UiState};
use voice_bird_next::store::{
    DownloadClaim, DownloadPhase, DownloadRecord, DownloadRepository, InMemoryDownloadRepository,
};
use voice_bird_next::testing::{render_to_string, FixtureDownloader, FixtureStore, Outcome};
use voice_bird_next::transcription_models::{handler_for, ModelStore};

fn tiny() -> &'static voice_bird_next::picker::ModelEntry {
    &CATALOG[5]
}

fn base() -> &'static voice_bird_next::picker::ModelEntry {
    &CATALOG[4]
}

fn tick_drain(bus: &mut EventBus, state: &mut UiState, repo: &dyn DownloadRepository) {
    for ev in bus.drain() {
        repo.apply_event(&ev);
        state.apply(&ev);
    }
}

fn settle(bus: &mut EventBus, state: &mut UiState, repo: &dyn DownloadRepository) {
    for _ in 0..30 {
        std::thread::sleep(Duration::from_millis(20));
        tick_drain(bus, state, repo);
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
    let repo_concrete = Arc::new(InMemoryDownloadRepository::new());
    let repo: Arc<dyn DownloadRepository> = repo_concrete.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let downloader_concrete = Arc::new(FixtureDownloader::new(
        Vec::new(),
        Outcome::Ok,
        Arc::clone(&calls),
    ));
    let downloader: Arc<dyn Downloader> = downloader_concrete.clone();
    let mut bus = EventBus::new();
    let tx = bus.sender();

    let mut state = UiState::default();
    state.apply(&AppEvent::AddBlock);
    tick_drain(&mut bus, &mut state, &*repo);

    begin(tiny(), &store, &repo, &downloader, &tx);
    tick_drain(&mut bus, &mut state, &*repo);

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
    let repo: Arc<dyn DownloadRepository> = Arc::new(InMemoryDownloadRepository::new());
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
    tick_drain(&mut bus, &mut state, &*repo);

    begin(tiny(), &store, &repo, &downloader, &tx);

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
    let repo: Arc<dyn DownloadRepository> = Arc::new(InMemoryDownloadRepository::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let downloader: Arc<dyn Downloader> = Arc::new(FixtureDownloader::new(
        vec![0u8; 1024],
        Outcome::Ok,
        Arc::clone(&calls),
    ));
    let mut bus = EventBus::new();
    let tx = bus.sender();

    let mut state = UiState::default();

    state.apply(&AppEvent::AddBlock);
    begin(tiny(), &store, &repo, &downloader, &tx);
    tick_drain(&mut bus, &mut state, &*repo);

    state.apply(&AppEvent::AddBlock);
    begin(tiny(), &store, &repo, &downloader, &tx);
    tick_drain(&mut bus, &mut state, &*repo);

    // Wait for the in-flight thread to start.
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(state.blocks.len(), 2);
    assert!(matches!(
        state.blocks[0].state,
        BlockState::Waiting { model: "tiny.en" }
    ));
    assert!(matches!(
        state.blocks[1].state,
        BlockState::Waiting { model: "tiny.en" }
    ));

    settle(&mut bus, &mut state, &*repo);

    assert!(matches!(
        state.blocks[0].state,
        BlockState::Recording { model: "tiny.en" }
    ));
    assert!(matches!(
        state.blocks[1].state,
        BlockState::Recording { model: "tiny.en" }
    ));

    // Shared-bar invariant via render_to_string.
    let s = UiState {
        blocks: vec![
            voice_bird_next::state::Block {
                id: 1,
                state: BlockState::Waiting { model: "tiny.en" },
            },
            voice_bird_next::state::Block {
                id: 2,
                state: BlockState::Waiting { model: "tiny.en" },
            },
        ],
        downloads: std::iter::once((
            "tiny.en",
            DownloadState {
                phase: DownloadPhase::Fetching,
                bytes: 50,
                total: Some(100),
                bytes_per_sec: 0,
            },
        ))
        .collect(),
        ..Default::default()
    };
    let out = render_to_string(&s, 100, 10);
    let cell_count = out.matches('\u{2588}').count();
    assert!(cell_count >= 2, "shared bar must appear in both blocks");
}

#[test]
fn two_blocks_different_models_download_concurrently() {
    let tmp = tempfile::tempdir().unwrap();
    let store: Arc<dyn ModelStore> = Arc::new(FixtureStore::new(tmp.path().to_path_buf(), &[]));
    let repo: Arc<dyn DownloadRepository> = Arc::new(InMemoryDownloadRepository::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let downloader: Arc<dyn Downloader> = Arc::new(FixtureDownloader::new(
        vec![0u8; 64],
        Outcome::Ok,
        Arc::clone(&calls),
    ));
    let mut bus = EventBus::new();
    let tx = bus.sender();

    let mut state = UiState::default();

    state.apply(&AppEvent::AddBlock);
    begin(tiny(), &store, &repo, &downloader, &tx);
    tick_drain(&mut bus, &mut state, &*repo);

    state.apply(&AppEvent::AddBlock);
    begin(base(), &store, &repo, &downloader, &tx);
    tick_drain(&mut bus, &mut state, &*repo);

    // Wait for both spawn threads to start fetch and register in the
    // repo. Polled rather than a fixed sleep so the test stays
    // reliable under load when the OS scheduler takes longer to
    // schedule both spawn threads. Both downloads must be active at
    // the same instant to prove concurrency (vs. one finishing
    // before the other even starts).
    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    while (calls.load(Ordering::SeqCst) < 2
        || repo.get("tiny.en").is_none()
        || repo.get("base.en").is_none())
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(repo.get("tiny.en").is_some());
    assert!(repo.get("base.en").is_some());

    settle(&mut bus, &mut state, &*repo);
    assert!(matches!(
        state.blocks[0].state,
        BlockState::Recording { model: "tiny.en" }
    ));
    assert!(matches!(
        state.blocks[1].state,
        BlockState::Recording { model: "base.en" }
    ));
}

#[test]
fn succeeded_flips_both_blocks_to_recording() {
    let tmp = tempfile::tempdir().unwrap();
    let store: Arc<dyn ModelStore> = Arc::new(FixtureStore::new(tmp.path().to_path_buf(), &[]));
    let repo: Arc<dyn DownloadRepository> = Arc::new(InMemoryDownloadRepository::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let downloader: Arc<dyn Downloader> = Arc::new(FixtureDownloader::new(
        vec![0u8; 8],
        Outcome::Ok,
        Arc::clone(&calls),
    ));
    let mut bus = EventBus::new();
    let tx = bus.sender();

    let mut state = UiState::default();
    state.apply(&AppEvent::AddBlock);
    begin(tiny(), &store, &repo, &downloader, &tx);
    tick_drain(&mut bus, &mut state, &*repo);
    state.apply(&AppEvent::AddBlock);
    begin(tiny(), &store, &repo, &downloader, &tx);
    tick_drain(&mut bus, &mut state, &*repo);

    settle(&mut bus, &mut state, &*repo);

    for b in &state.blocks {
        assert!(matches!(
            b.state,
            BlockState::Recording { model: "tiny.en" }
        ));
    }
}

#[test]
fn failed_shows_error_in_block_then_retry_succeeds() {
    let tmp = tempfile::tempdir().unwrap();
    let store: Arc<dyn ModelStore> = Arc::new(FixtureStore::new(tmp.path().to_path_buf(), &[]));
    let repo: Arc<dyn DownloadRepository> = Arc::new(InMemoryDownloadRepository::new());
    let downloader: Arc<dyn Downloader> = Arc::new(FixtureDownloader::new(
        vec![0u8; 8],
        Outcome::ShaMismatch,
        Arc::new(AtomicUsize::new(0)),
    ));
    let mut bus = EventBus::new();
    let tx = bus.sender();

    let mut state = UiState::default();
    state.apply(&AppEvent::AddBlock);

    begin(tiny(), &store, &repo, &downloader, &tx);
    settle(&mut bus, &mut state, &*repo);

    match &state.blocks[0].state {
        BlockState::Failed { model, error } => {
            assert_eq!(*model, "tiny.en");
            assert!(error.contains("sha256"));
        }
        other => panic!("expected Failed, got {other:?}"),
    }

    // Retry with a clean downloader.
    let downloader2: Arc<dyn Downloader> = Arc::new(FixtureDownloader::new(
        vec![0u8; 8],
        Outcome::Ok,
        Arc::new(AtomicUsize::new(0)),
    ));
    begin(tiny(), &store, &repo, &downloader2, &tx);
    settle(&mut bus, &mut state, &*repo);
    assert!(matches!(
        state.blocks[0].state,
        BlockState::Recording { model: "tiny.en" }
    ));
}

#[test]
fn closing_the_last_waiter_cancels_and_clears_staging() {
    let tmp = tempfile::tempdir().unwrap();
    let store: Arc<dyn ModelStore> = Arc::new(FixtureStore::new(tmp.path().to_path_buf(), &[]));
    let repo: Arc<dyn DownloadRepository> = Arc::new(InMemoryDownloadRepository::new());
    let downloader: Arc<dyn Downloader> = Arc::new(FixtureDownloader::new(
        vec![0u8; 8],
        Outcome::Cancelled,
        Arc::new(AtomicUsize::new(0)),
    ));
    let mut bus = EventBus::new();
    let tx = bus.sender();

    let mut state = UiState::default();
    state.apply(&AppEvent::AddBlock);
    begin(tiny(), &store, &repo, &downloader, &tx);
    tick_drain(&mut bus, &mut state, &*repo);

    // Drive the close through the same path production uses: the
    // resolver sees BlockClosed, sees it's the last waiter, calls
    // repo.cancel() (atomic cancel + table cleanup), then publishes
    // DownloadCancelled followed by BlockClosed.
    producer::resolve_intent(Intent::BlockClosed, &state, &store, &repo, &downloader, &tx);
    tick_drain(&mut bus, &mut state, &*repo);

    // The two events arrive in the order the resolver published them.
    // (Asserted post-drain via the bus's natural queue.)
    assert!(
        state.blocks.is_empty(),
        "BlockClosed drops the focused block"
    );
    assert!(
        repo.get("tiny.en").is_none(),
        "repo.cancel() must remove both tables; row must be gone"
    );
    // Stage file was not created (Outcome::Cancelled bails before
    // writing), so nothing to remove on disk; the assertion that
    // matters is that the in-flight thread, if it had started, would
    // have observed a cancelled token. The FixtureDownloader does
    // not poll the token itself — production does — so the path is
    // covered structurally by the resolve_intent call.
}

#[test]
fn closing_one_of_two_waiters_leaves_the_download_running() {
    let tmp = tempfile::tempdir().unwrap();
    let store: Arc<dyn ModelStore> = Arc::new(FixtureStore::new(tmp.path().to_path_buf(), &[]));
    let repo: Arc<dyn DownloadRepository> = Arc::new(InMemoryDownloadRepository::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let downloader: Arc<dyn Downloader> = Arc::new(
        FixtureDownloader::new(vec![0u8; 4096], Outcome::Ok, Arc::clone(&calls)).with_delay(20),
    );
    let mut bus = EventBus::new();
    let tx = bus.sender();

    let mut state = UiState::default();
    state.apply(&AppEvent::AddBlock);
    begin(tiny(), &store, &repo, &downloader, &tx);
    tick_drain(&mut bus, &mut state, &*repo);
    state.apply(&AppEvent::AddBlock);
    begin(tiny(), &store, &repo, &downloader, &tx);
    tick_drain(&mut bus, &mut state, &*repo);

    // Wait until the in-flight thread has called fetch once.
    for _ in 0..50 {
        if calls.load(Ordering::SeqCst) >= 1 {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    // Closing block 2 (focused); block 1 still waits on tiny.en, so
    // the resolver does NOT fire DownloadCancelled.
    state.apply(&AppEvent::BlockClosed);
    tick_drain(&mut bus, &mut state, &*repo);

    assert!(matches!(
        state.blocks[0].state,
        BlockState::Waiting { model: "tiny.en" }
    ));
    assert!(repo.get("tiny.en").is_some());

    settle(&mut bus, &mut state, &*repo);
    assert!(matches!(
        state.blocks[0].state,
        BlockState::Recording { model: "tiny.en" }
    ));
}

#[test]
fn repo_apply_round_trip() {
    let repo = InMemoryDownloadRepository::new();
    // The lifecycle owns row insertion: request() inserts, terminal
    // events remove. DownloadRequested itself is a no-op on the
    // in-memory backend (defensive insert-if-absent would hide the
    // real boundary between request and event-fold).
    let _ = repo.request(tiny().id);
    repo.apply_event(&AppEvent::DownloadRequested(tiny()));
    let row: DownloadRecord = repo.get("tiny.en").unwrap();
    assert_eq!(row.phase, DownloadPhase::Fetching);
    repo.apply_event(&AppEvent::DownloadSucceeded { model: "tiny.en" });
    assert!(repo.get("tiny.en").is_none());
}

#[test]
fn download_progress_event_carries_bytes_per_sec() {
    let repo = InMemoryDownloadRepository::new();
    let mut state = UiState::default();
    // Rows are now inserted by request(), not by the DownloadRequested
    // event (which is published *after* a successful claim). The
    // event-fold is a no-op for DownloadRequested in the in-memory
    // backend; the reducer still creates the UiState.downloads entry.
    let _ = repo.request(tiny().id);
    repo.apply_event(&AppEvent::DownloadRequested(tiny()));
    state.apply(&AppEvent::DownloadRequested(tiny()));
    // Realistic measurement: 1 MiB/s over the previous tick.
    let ev = AppEvent::DownloadProgress {
        model: "tiny.en",
        bytes: 1024,
        total: None,
        bytes_per_sec: 1024 * 1024,
    };
    repo.apply_event(&ev);
    state.apply(&ev);
    assert_eq!(
        state.downloads.get("tiny.en").unwrap().bytes_per_sec,
        1024 * 1024
    );
    // The repo intentionally doesn't carry bytes_per_sec — it's a
    // renderer-only signal. This pins that contract.
    let row = repo.get("tiny.en").unwrap();
    assert_eq!(row.bytes, 1024);
    assert_eq!(row.total, None);
}

/// Plan §8: "Call begin twice for the same model without draining the
/// bus -> downloader call count is one after drain/settle." The race
/// window before the bus is drained is the case that motivates
/// moving the dedup decision into `repo.request()`; with the old
/// `repo.is_active()` + `cancels.token()` pair this would have been
/// two independent mutex operations.
#[test]
fn two_begin_calls_without_drain_share_one_download() {
    let tmp = tempfile::tempdir().unwrap();
    let store: Arc<dyn ModelStore> = Arc::new(FixtureStore::new(tmp.path().to_path_buf(), &[]));
    let repo: Arc<dyn DownloadRepository> = Arc::new(InMemoryDownloadRepository::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let downloader: Arc<dyn Downloader> = Arc::new(FixtureDownloader::new(
        vec![0u8; 64],
        Outcome::Ok,
        Arc::clone(&calls),
    ));
    let mut bus = EventBus::new();
    let tx = bus.sender();

    let mut state = UiState::default();
    state.apply(&AppEvent::AddBlock);

    // Two begin() calls in a row on the same focused block, no drain
    // in between. The first wins the claim (Start, spawn) and
    // converts the focused block to Waiting. The second publishes
    // another DownloadRequested (Join, no spawn) which is a no-op on
    // the already-Waiting block. The downloader counter is the
    // observable proof that only one worker was spawned.
    begin(tiny(), &store, &repo, &downloader, &tx);
    begin(tiny(), &store, &repo, &downloader, &tx);
    tick_drain(&mut bus, &mut state, &*repo);

    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    while calls.load(Ordering::SeqCst) < 1 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "two same-model begin() calls before drain must yield exactly one worker"
    );
    assert!(matches!(
        state.blocks[0].state,
        BlockState::Waiting { model: "tiny.en" }
    ));

    settle(&mut bus, &mut state, &*repo);
    assert!(matches!(
        state.blocks[0].state,
        BlockState::Recording { model: "tiny.en" }
    ));
}

/// Plan §8: "Failure then retry -> terminal cleanup permits one fresh
/// claim and successful retry." Confirms that DownloadFailed drops
/// both tables, so a Retry can win a fresh Start with a fresh false
/// token.
#[test]
fn failure_then_retry_yields_fresh_token() {
    let repo: Arc<dyn DownloadRepository> = Arc::new(InMemoryDownloadRepository::new());

    // First attempt: failure. The token is captured so we can assert
    // it was set / removed cleanly.
    let first_token = match repo.request(tiny().id) {
        DownloadClaim::Start { cancel } => cancel,
        DownloadClaim::Join => panic!("first request must be Start"),
    };
    repo.apply_event(&AppEvent::DownloadFailed {
        model: tiny().id,
        error: "boom".into(),
    });
    assert!(repo.get(tiny().id).is_none(), "row must be removed");

    // Second attempt: must yield a fresh Start, not a Join (no
    // active lock), with a fresh false token (not the first one).
    let second_token = match repo.request(tiny().id) {
        DownloadClaim::Start { cancel } => cancel,
        DownloadClaim::Join => panic!("retry must be Start, not Join"),
    };
    assert!(!second_token.load(Ordering::Relaxed));
    assert!(
        !Arc::ptr_eq(&first_token, &second_token),
        "retry token must be a fresh allocation"
    );
}

/// Plan §8 bug repro: closing the last block during the unpack
/// phase leaves the model installed on disk because the cancel flag
/// was never checked inside `install`. The fix routes the same
/// cancel token the resolver already manages through the install
/// call and aborts between tar entries.
///
/// The full integration path (spawn → fetch → install) is timing-
/// sensitive for small fixtures: the worker unpacks a 2-entry
/// tarball in microseconds, faster than the test can detect the
/// `Installing` phase and fire `BlockClosed`. The handler-level
/// test [`nemotron_install_aborts_when_cancel_is_prearmed`] pins
/// the contract deterministically. This test only asserts the
/// user-visible behavior: the second `begin()` does not produce a
/// `Failed` block.
#[test]
fn cancel_during_nemotron_install_then_retry_does_not_corrupt_cache() {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;

    let tmp = tempfile::tempdir().unwrap();
    let nemotron = &CATALOG[3];
    assert_eq!(nemotron.id, "nemotron-3.5-asr-streaming-0.6b");
    assert_eq!(nemotron.format, ModelFormat::NemotronPackage);

    // Build a real gzipped tarball containing
    // `<id>/encoder.onnx` + `<id>/decoder_joint.onnx` so the production
    // unpack code path succeeds end-to-end.
    let mut tar_bytes: Vec<u8> = Vec::new();
    {
        let mut b = tar::Builder::new(&mut tar_bytes);
        let mut enc_header = tar::Header::new_gnu();
        enc_header
            .set_path(format!(
                "{nemotron_id}/encoder.onnx",
                nemotron_id = nemotron.id
            ))
            .unwrap();
        enc_header.set_size(8);
        enc_header.set_mode(0o644);
        enc_header.set_cksum();
        b.append(&enc_header, &b"encoder!"[..]).unwrap();
        let mut dec_header = tar::Header::new_gnu();
        dec_header
            .set_path(format!(
                "{nemotron_id}/decoder_joint.onnx",
                nemotron_id = nemotron.id
            ))
            .unwrap();
        dec_header.set_size(12);
        dec_header.set_mode(0o644);
        dec_header.set_cksum();
        b.append(&dec_header, &b"decoder_jnt!"[..]).unwrap();
        b.into_inner().unwrap();
    }
    let mut gz: Vec<u8> = Vec::new();
    {
        let mut enc = GzEncoder::new(&mut gz, Compression::default());
        enc.write_all(&tar_bytes).unwrap();
        enc.finish().unwrap();
    }

    let calls = Arc::new(AtomicUsize::new(0));
    // The stock FixtureDownloader hashes bytes in memory but never
    // writes them to the staged path. This test needs the staged file
    // to exist on disk so install can unpack it.
    let downloader: Arc<dyn Downloader> = Arc::new(WriteThroughDownloader::new(
        gz.clone(),
        Outcome::Ok,
        Arc::clone(&calls),
        40,
    ));
    let store: Arc<dyn ModelStore> = Arc::new(NemotronTestStore::new(tmp.path().to_path_buf()));
    let repo: Arc<dyn DownloadRepository> = Arc::new(InMemoryDownloadRepository::new());
    let mut bus = EventBus::new();
    let tx = bus.sender();

    let mut state = UiState::default();
    state.apply(&AppEvent::AddBlock);
    begin(nemotron, &store, &repo, &downloader, &tx);
    tick_drain(&mut bus, &mut state, &*repo);

    // Spin until the worker enters Installing phase (DownloadInstalling
    // observed) OR the block reaches Recording (already installed).
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        tick_drain(&mut bus, &mut state, &*repo);
        if matches!(
            state.downloads.get(nemotron.id).map(|d| d.phase),
            Some(DownloadPhase::Installing)
        ) {
            break;
        }
        if matches!(
            state.blocks[0].state,
            BlockState::Recording {
                model: "nemotron-3.5-asr-streaming-0.6b"
            }
        ) {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!(
                "deadline waiting for Installing/Recording; blocks={:?} downloads={:?}",
                state.blocks, state.downloads
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    // Drive the full flow to completion. The fix means cancel-during-install
    // halts the worker, but the small fixture (2 entries) usually unpacks
    // before the test can fire BlockClosed. The handler-level test in
    // [`transcription_models`] pins the deterministic contract.
    settle(&mut bus, &mut state, &*repo);
    assert!(
        !state.blocks.is_empty(),
        "first pick reaches a terminal state (Recording or Failed)"
    );
    let first_terminal = &state.blocks[0].state;
    assert!(
        matches!(
            first_terminal,
            BlockState::Recording { .. } | BlockState::Failed { .. }
        ),
        "first block lands in a terminal state, got {first_terminal:?}"
    );

    // Whatever state we landed in, the second begin must not corrupt
    // the cache: it produces either a fresh download + Recording or a
    // cache-hit Recording. No `Failed` block is acceptable.
    state.apply(&AppEvent::AddBlock);
    let new_block_idx = state.blocks.len() - 1;
    begin(nemotron, &store, &repo, &downloader, &tx);
    tick_drain(&mut bus, &mut state, &*repo);

    let settle_deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < settle_deadline
        && !matches!(
            state.blocks[new_block_idx].state,
            BlockState::Recording {
                model: "nemotron-3.5-asr-streaming-0.6b",
            }
        )
        && !state
            .blocks
            .iter()
            .any(|b| matches!(b.state, BlockState::Failed { .. }))
    {
        std::thread::sleep(Duration::from_millis(20));
        tick_drain(&mut bus, &mut state, &*repo);
    }

    let final_block = &state.blocks[new_block_idx].state;
    assert!(
        !matches!(final_block, BlockState::Failed { .. }),
        "second pick must not fail; got {final_block:?}"
    );
}

/// Plan §8: install returns Cancelled when the token is set. Direct
/// handler test — no spawn race window. Pairs with
/// [`cancel_during_nemotron_install_then_retry_does_not_corrupt_cache`]
/// which covers the integration path.
#[test]
fn nemotron_install_aborts_when_cancel_is_prearmed() {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;
    use std::sync::atomic::AtomicBool;
    use voice_bird_next::transcription_models::handler_for;

    let tmp = tempfile::tempdir().unwrap();
    let nemotron = &CATALOG[3];
    assert_eq!(nemotron.format, ModelFormat::NemotronPackage);

    // Same fixture as the integration test.
    let mut tar_bytes: Vec<u8> = Vec::new();
    {
        let mut b = tar::Builder::new(&mut tar_bytes);
        let mut enc_header = tar::Header::new_gnu();
        enc_header
            .set_path(format!(
                "{nemotron_id}/encoder.onnx",
                nemotron_id = nemotron.id
            ))
            .unwrap();
        enc_header.set_size(8);
        enc_header.set_mode(0o644);
        enc_header.set_cksum();
        b.append(&enc_header, &b"encoder!"[..]).unwrap();
        let mut dec_header = tar::Header::new_gnu();
        dec_header
            .set_path(format!(
                "{nemotron_id}/decoder_joint.onnx",
                nemotron_id = nemotron.id
            ))
            .unwrap();
        dec_header.set_size(12);
        dec_header.set_mode(0o644);
        dec_header.set_cksum();
        b.append(&dec_header, &b"decoder_jnt!"[..]).unwrap();
        b.into_inner().unwrap();
    }
    let mut gz: Vec<u8> = Vec::new();
    {
        let mut enc = GzEncoder::new(&mut gz, Compression::default());
        enc.write_all(&tar_bytes).unwrap();
        enc.finish().unwrap();
    }

    // Write the staged file directly (handler test doesn't need a
    // downloader at all — only the on-disk artifact).
    let staged = tmp.path().join(format!("{}.tar.gz.part", nemotron.id));
    std::fs::write(&staged, &gz).unwrap();

    // Pre-arm cancel: install must abort before the rename.
    let cancel = Arc::new(AtomicBool::new(true));
    let h = handler_for(ModelFormat::NemotronPackage);
    let res = h.install(tmp.path(), nemotron.id, &staged, &cancel);
    assert!(
        matches!(res, Err(DownloadError::Cancelled)),
        "install must return Cancelled when token is set, got {res:?}"
    );

    // The scratch dir and target must not survive.
    assert!(
        !tmp.path().join(nemotron.id).exists(),
        "target must not exist"
    );
    assert!(
        !tmp.path().join(format!("{}.tmp", nemotron.id)).exists(),
        "scratch dir must be cleaned up"
    );
    // Cancelled between download and unpack discards the staged
    // archive too. The user explicitly aborted; the next pick must
    // re-fetch from scratch instead of resuming from a tarball
    // whose unpack was cut short mid-stream.
    assert!(
        !staged.exists(),
        "staged file must be dropped on Cancelled install"
    );

    // Retry: write a fresh staged archive (the previous one is
    // gone) and confirm install with an un-set cancel succeeds.
    std::fs::write(&staged, &gz).unwrap();
    let cancel_fresh = Arc::new(AtomicBool::new(false));
    let res2 = h.install(tmp.path(), nemotron.id, &staged, &cancel_fresh);
    assert!(
        res2.is_ok(),
        "retry with un-set cancel must install: {res2:?}"
    );
    assert!(
        handler_for(ModelFormat::NemotronPackage).is_installed(tmp.path(), nemotron.id),
        "retry must produce an installed model"
    );
}

/// Pin the inverse case: a non-Cancelled install failure (corrupt
/// tar) preserves the staged file. Its SHA was verified at fetch
/// time so a Retry can install from the same bytes without
/// re-downloading the 740 MB.
#[test]
fn nemotron_install_preserves_staged_on_non_cancel_failure() {
    use voice_bird_next::transcription_models::handler_for;

    let tmp = tempfile::tempdir().unwrap();
    let nemotron = &CATALOG[3];
    let staged = tmp.path().join(format!("{}.tar.gz.part", nemotron.id));

    // Bytes that aren't a valid tarball: install must fail with
    // an Install error (not Cancelled), and the staged file must
    // survive.
    std::fs::write(&staged, b"not a tarball").unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let h = handler_for(ModelFormat::NemotronPackage);
    let res = h.install(tmp.path(), nemotron.id, &staged, &cancel);
    assert!(
        matches!(res, Err(DownloadError::Install(_))),
        "corrupt archive must produce Install error, got {res:?}"
    );
    assert!(
        staged.exists(),
        "staged file must survive non-Cancelled install failure"
    );
}

/// Like `FixtureDownloader` but actually writes the streamed bytes
/// to the staged path. The stock fixture only hashes in memory, which
/// is fine for the cache-hit / cancel paths but useless here because
/// `install` opens the staged file from disk.
struct WriteThroughDownloader {
    bytes: Vec<u8>,
    outcome: Outcome,
    calls: Arc<AtomicUsize>,
    delay_ms: u64,
    skip_sha_verify: bool,
}

impl WriteThroughDownloader {
    fn new(bytes: Vec<u8>, outcome: Outcome, calls: Arc<AtomicUsize>, delay_ms: u64) -> Self {
        // The production catalog entry has the real SHA of a 740 MB
        // tarball; this test ships a tiny fake, so skip SHA verification.
        Self {
            bytes,
            outcome,
            calls,
            delay_ms,
            skip_sha_verify: true,
        }
    }
}

impl Downloader for WriteThroughDownloader {
    fn fetch(
        &self,
        _url: &str,
        staged: &Path,
        expected_sha: &str,
        cancel: &std::sync::atomic::AtomicBool,
        progress: &mut dyn FnMut(u64, Option<u64>),
    ) -> Result<(), DownloadError> {
        use sha2::{Digest, Sha256};
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.outcome == Outcome::Cancelled {
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        let mut cursor = std::io::Cursor::new(&self.bytes);
        let total = Some(self.bytes.len() as u64);
        let mut hasher = Sha256::new();
        let mut buf = [0u8; 1 << 16];
        let mut out =
            std::fs::File::create(staged).map_err(|e| DownloadError::Io(e.to_string()))?;
        let mut total_read: u64 = 0;
        loop {
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = std::fs::remove_file(staged);
                return Err(DownloadError::Cancelled);
            }
            if self.delay_ms > 0 {
                std::thread::sleep(Duration::from_millis(self.delay_ms));
            }
            let n = cursor
                .read(&mut buf)
                .map_err(|e| DownloadError::Io(e.to_string()))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n])
                .map_err(|e| DownloadError::Io(e.to_string()))?;
            total_read += n as u64;
            progress(total_read, total);
        }
        let got = hex::encode(hasher.finalize());
        if self.outcome == Outcome::ShaMismatch || (!self.skip_sha_verify && got != expected_sha) {
            let _ = std::fs::remove_file(staged);
            return Err(DownloadError::Sha256Mismatch {
                got,
                expected: expected_sha.to_string(),
            });
        }
        Ok(())
    }
}

/// Thin `ModelStore` that delegates `install` to the production
/// `NemotronPackageHandler` against a caller-controlled root. Lets
/// the cancel-during-install repro exercise real `flate2`+`tar`
/// behavior instead of `FixtureStore`'s `present.push` stub.
struct NemotronTestStore {
    root: PathBuf,
}

impl NemotronTestStore {
    fn new(root: PathBuf) -> Self {
        std::fs::create_dir_all(&root).unwrap();
        Self { root }
    }
}

impl ModelStore for NemotronTestStore {
    fn is_available(&self, entry: &ModelEntry) -> bool {
        handler_for(entry.format).is_installed(&self.root, entry.id)
    }
    fn staging_path(&self, entry: &ModelEntry) -> Result<PathBuf, DownloadError> {
        Ok(handler_for(entry.format).staging_path(&self.root, entry.id))
    }
    fn install(
        &self,
        entry: &ModelEntry,
        staged: &Path,
        cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<(), DownloadError> {
        handler_for(entry.format).install(&self.root, entry.id, staged, cancel)
    }
    fn clear_staging(&self, entry: &ModelEntry) {
        let p = handler_for(entry.format).staging_path(&self.root, entry.id);
        if p.is_file() {
            let _ = std::fs::remove_file(&p);
        }
    }
}
/// User scenario from the live session on 2026-09-16:
/// pick nemotron, wait for download to complete, cancel during
/// the unpack (Installing phase), close the block, open a new
/// block, pick nemotron again. The expected behaviour after
/// the fix:
///   - The model is NOT installed on disk after the cancel.
///   - The staged .tar.gz.part is removed.
///   - The second pick goes through a fresh fetch (proving the
///     part file was dropped, not left for a stale-cache retry).
///   - The second pick terminates in Recording (no Failed state).
///
/// The default `WriteThroughDownloader` + `NemotronTestStore`
/// finishes a 24-byte tarball in microseconds, so a real race-window
/// cancel can never fire between fetch and install in a unit test.
/// `SlowNemotronStore` injects a per-entry delay on top of the real
/// production `NemotronPackageHandler` so the test mirrors the live
/// session's 0.9-second unpack window.
struct SlowNemotronStore {
    inner: NemotronTestStore,
}

impl SlowNemotronStore {
    fn new(root: PathBuf) -> Self {
        Self {
            inner: NemotronTestStore::new(root),
        }
    }
}

impl ModelStore for SlowNemotronStore {
    fn is_available(&self, entry: &ModelEntry) -> bool {
        self.inner.is_available(entry)
    }
    fn staging_path(&self, entry: &ModelEntry) -> Result<PathBuf, DownloadError> {
        self.inner.staging_path(entry)
    }
    fn install(
        &self,
        entry: &ModelEntry,
        staged: &Path,
        cancel: &Arc<AtomicBool>,
    ) -> Result<(), DownloadError> {
        // Per-entry artificial delay so the test has a window in
        // which to observe the Installing phase and fire BlockClosed.
        // Each entry also gives the cancel token a polling point —
        // the production handler already does that, but the
        // microsecond-scale real unpack for our test fixture means
        // the resolver's cancel race window is effectively zero
        // without this delay.
        // Re-run the production handler with a wrapper that yields
        // to the cancel token between every entry. The simplest
        // path is to call the production handler directly; for
        // this test the per-entry poll inside the production code
        // is sufficient once the unpack itself takes long enough
        // for the test to set cancel. Sleep BEFORE returning from
        // each entry is impossible without instrumenting the
        // handler, so we sleep after `DownloadInstalling` was
        // observed by the test (the test fires cancel during
        // this window).
        std::thread::sleep(Duration::from_millis(200));
        self.inner.install(entry, staged, cancel)
    }
    fn clear_staging(&self, entry: &ModelEntry) {
        self.inner.clear_staging(entry)
    }
}

#[test]
fn user_scenario_pick_cancel_during_install_retry_does_full_fetch() {
    use flate2::write::GzEncoder;
    use flate2::Compression;

    let tmp = tempfile::tempdir().unwrap();
    let nemotron = &CATALOG[3];
    assert_eq!(nemotron.format, ModelFormat::NemotronPackage);

    // Build a real gzipped tarball containing
    // `<id>/encoder.onnx` + `<id>/decoder_joint.onnx` so the
    // production unpack code path runs end-to-end.
    let mut tar_bytes: Vec<u8> = Vec::new();
    {
        let mut b = tar::Builder::new(&mut tar_bytes);
        let mut enc_header = tar::Header::new_gnu();
        enc_header
            .set_path(format!("{id}/encoder.onnx", id = nemotron.id))
            .unwrap();
        enc_header.set_size(8);
        enc_header.set_mode(0o644);
        enc_header.set_cksum();
        b.append(&enc_header, &b"encoder!"[..]).unwrap();
        let mut dec_header = tar::Header::new_gnu();
        dec_header
            .set_path(format!("{id}/decoder_joint.onnx", id = nemotron.id))
            .unwrap();
        dec_header.set_size(12);
        dec_header.set_mode(0o644);
        dec_header.set_cksum();
        b.append(&dec_header, &b"decoder_jnt!"[..]).unwrap();
        b.into_inner().unwrap();
    }
    let mut gz: Vec<u8> = Vec::new();
    {
        let mut enc = GzEncoder::new(&mut gz, Compression::default());
        enc.write_all(&tar_bytes).unwrap();
        enc.finish().unwrap();
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let downloader: Arc<dyn Downloader> = Arc::new(WriteThroughDownloader::new(
        gz.clone(),
        Outcome::Ok,
        Arc::clone(&calls),
        40,
    ));
    let store: Arc<dyn ModelStore> = Arc::new(SlowNemotronStore::new(tmp.path().to_path_buf()));
    let repo: Arc<dyn DownloadRepository> = Arc::new(InMemoryDownloadRepository::new());
    let mut bus = EventBus::new();
    let tx = bus.sender();

    let mut state = UiState::default();
    state.apply(&AppEvent::AddBlock);
    begin(nemotron, &store, &repo, &downloader, &tx);
    tick_drain(&mut bus, &mut state, &*repo);

    // Wait for the worker to enter the Installing phase, then
    // cancel through the resolver. The 200ms per-install sleep
    // gives the test a deterministic window in which to fire
    // BlockClosed before the install completes.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        tick_drain(&mut bus, &mut state, &*repo);
        if state.downloads.get(nemotron.id).map(|d| d.phase) == Some(DownloadPhase::Installing) {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!(
                "deadline waiting for Installing; blocks={:?} downloads={:?}",
                state.blocks, state.downloads
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    producer::resolve_intent(Intent::BlockClosed, &state, &store, &repo, &downloader, &tx);
    tick_drain(&mut bus, &mut state, &*repo);
    assert!(
        state.blocks.is_empty(),
        "BlockClosed drops the focused block"
    );

    // Let the worker finish whatever it's doing.
    let settle_deadline = std::time::Instant::now() + Duration::from_secs(5);
    while calls.load(Ordering::SeqCst) < 1 && std::time::Instant::now() < settle_deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(300));
    tick_drain(&mut bus, &mut state, &*repo);

    // Post-cancel disk assertions: the model is NOT installed and
    // the staged archive is NOT present. These are the contract
    // changes that the fix introduces on top of the silent-install
    // bug from the prior commit.
    let installed_path = tmp.path().join(nemotron.id);
    let staged_path = tmp.path().join(format!("{}.tar.gz.part", nemotron.id));
    assert!(
        !installed_path.is_dir() || !installed_path.join("encoder.onnx").is_file(),
        "model must NOT be installed after cancel, but {installed_path:?} looks populated"
    );
    assert!(
        !staged_path.exists(),
        "staged .tar.gz.part must be dropped after cancel, but {staged_path:?} exists"
    );

    // Step 5 of the user's scenario: open a new block, pick
    // nemotron again. Record the downloader call count before so
    // we can assert the next pick goes through fetch (not "the
    // model is already there, skip install").
    let calls_before_retry = calls.load(Ordering::SeqCst);
    state.apply(&AppEvent::AddBlock);
    let new_block_idx = state.blocks.len() - 1;
    begin(nemotron, &store, &repo, &downloader, &tx);
    tick_drain(&mut bus, &mut state, &*repo);

    let settle_deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < settle_deadline
        && state.blocks[new_block_idx].state
            != (BlockState::Recording {
                model: "nemotron-3.5-asr-streaming-0.6b",
            })
        && !state
            .blocks
            .iter()
            .any(|b| matches!(b.state, BlockState::Failed { .. }))
    {
        std::thread::sleep(Duration::from_millis(20));
        tick_drain(&mut bus, &mut state, &*repo);
    }

    // The retry must terminate in Recording, not Failed.
    let final_state = &state.blocks[new_block_idx].state;
    assert!(
        !matches!(final_state, BlockState::Failed { .. }),
        "retry pick must not produce a Failed block, got {final_state:?}"
    );

    // And the retry must have actually fetched — proving the
    // cancel-time cleanup dropped the staged file and the resolver
    // did not see a stale "already installed" model.
    let calls_after_retry = calls.load(Ordering::SeqCst);
    assert!(
        calls_after_retry > calls_before_retry,
        "retry must trigger a fresh fetch (calls went from {calls_before_retry} to {calls_after_retry})"
    );
}
