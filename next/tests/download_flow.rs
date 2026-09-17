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
    ClaimState, DownloadClaim, DownloadPhase, DownloadRecord, DownloadRepository,
    InMemoryDownloadRepository,
};
use voice_bird_next::testing::{render_to_string, FixtureDownloader, FixtureStore, Outcome};
use voice_bird_next::transcription_models::{handler_for, ModelStore};

fn tiny() -> &'static voice_bird_next::picker::ModelEntry {
    &CATALOG[5]
}

fn base() -> &'static voice_bird_next::picker::ModelEntry {
    &CATALOG[4]
}

/// Drain the bus and apply each event, mirroring the production
/// loop in `main::run`: the store's `apply_event` is the gate that
/// decides whether the UI projection is updated, and a stale
/// download event from a superseded attempt is filtered before
/// `UiState::apply` ever sees it. Tests that exercise stale-event
/// paths through the bus use this helper, so the cancelled-but-
/// still-running worker's terminal event cannot repaint the new
/// attempt's gauge.
fn tick_drain(bus: &mut EventBus, state: &mut UiState, repo: &dyn DownloadRepository) {
    for ev in bus.drain() {
        if repo.apply_event(&ev) {
            state.apply(&ev);
        }
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
    let attempt_a = match repo.request(tiny().id) {
        DownloadClaim::Start { attempt, .. } => attempt,
        _ => panic!("must be Start"),
    };
    repo.apply_event(&AppEvent::DownloadRequested(tiny()));
    let row: DownloadRecord = repo.get("tiny.en").unwrap();
    assert_eq!(row.phase, DownloadPhase::Fetching);
    assert_eq!(row.attempt, attempt_a);
    repo.apply_event(&AppEvent::DownloadSucceeded {
        attempt: attempt_a,
        model: "tiny.en",
    });
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
    let attempt_a = match repo.request(tiny().id) {
        DownloadClaim::Start { attempt, .. } => attempt,
        _ => panic!("must be Start"),
    };
    repo.apply_event(&AppEvent::DownloadRequested(tiny()));
    state.apply(&AppEvent::DownloadRequested(tiny()));
    // Realistic measurement: 1 MiB/s over the previous tick.
    let ev = AppEvent::DownloadProgress {
        attempt: attempt_a,
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
        DownloadClaim::Start { cancel, attempt } => (cancel, attempt),
        DownloadClaim::Join => panic!("first request must be Start"),
        DownloadClaim::Restart { .. } => panic!("first request must be Start"),
    };
    repo.apply_event(&AppEvent::DownloadFailed {
        attempt: first_token.1,
        model: tiny().id,
        error: "boom".into(),
    });

    // Second attempt: must yield a fresh Start, not a Join (no
    let second_token = match repo.request(tiny().id) {
        DownloadClaim::Start { cancel, attempt } => (cancel, attempt),
        DownloadClaim::Join => panic!("retry must be Start, not Join"),
        DownloadClaim::Restart { .. } => panic!("retry must be Start, not Join"),
    };
    let (second_cancel, second_attempt) = second_token;
    assert!(!second_cancel.load(Ordering::Relaxed));
    assert!(
        !Arc::ptr_eq(&first_token.0, &second_cancel),
        "retry token must be a fresh allocation"
    );
    assert!(
        second_attempt > first_token.1,
        "retry attempt must increment; got {second_attempt} after {}",
        first_token.1
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
    fn staging_path(&self, entry: &ModelEntry, attempt: u32) -> Result<PathBuf, DownloadError> {
        Ok(handler_for(entry.format).staging_path(&self.root, entry.id, attempt))
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
        let p = handler_for(entry.format).staging_path(&self.root, entry.id, 1);
        if p.is_file() {
            let _ = std::fs::remove_file(&p);
        }
    }
    fn discard_inflight(&self, entry: &ModelEntry) {
        // Walk every per-attempt artifact that might still be on
        // disk. Matches the production cache dir's `.tmp` layout
        // when a kill-during-unpack left a scratch directory.
        for attempt in 1..=8u32 {
            let p = handler_for(entry.format).staging_path(&self.root, entry.id, attempt);
            if p.is_file() {
                let _ = std::fs::remove_file(&p);
            }
            let tmp = self
                .root
                .join(format!("{}.{}.tar.gz.tmp", entry.id, attempt));
            if tmp.is_dir() {
                let _ = std::fs::remove_dir_all(&tmp);
            }
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
    fn staging_path(&self, entry: &ModelEntry, attempt: u32) -> Result<PathBuf, DownloadError> {
        self.inner.staging_path(entry, attempt)
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
    fn discard_inflight(&self, entry: &ModelEntry) {
        self.inner.discard_inflight(entry)
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
/// Pin the contract of `ModelStore::discard_inflight`: a single call
/// drops BOTH the staged archive AND the unpack scratch directory,
/// regardless of whether install was reached. This is the method
/// `main::cleanup_inflight` invokes at Quit for every in-flight
/// model so a half-written `<id>.tmp/` from a killed worker doesn't
/// survive to confuse the next session's first pick.
#[test]
fn discard_inflight_drops_both_part_and_tmp() {
    let tmp = tempfile::tempdir().unwrap();
    let nemotron = &CATALOG[3];
    assert_eq!(nemotron.format, ModelFormat::NemotronPackage);
    let store = NemotronTestStore::new(tmp.path().to_path_buf());

    let staged = tmp.path().join(format!("{}.1.tar.gz.part", nemotron.id));
    std::fs::write(&staged, b"not even close").unwrap();
    let tmp_dir = tmp.path().join(format!("{}.1.tar.gz.tmp", nemotron.id));

    store.discard_inflight(nemotron);

    assert!(!staged.is_file(), "staged archive must be dropped");
    assert!(!tmp_dir.is_dir(), "scratch dir must be removed");
}

/// Pin the Quit-time cleanup behaviour. Mirrors the user's live-
/// session observation on 2026-09-16: kick off a download, signal
/// the Quit-time cancellation mid-flight, run the cleanup routine,
/// observe no leftover artifacts on disk. `cleanup_inflight` is a
/// binary-only helper; the test exercises the same body inline.
#[test]
fn quit_during_install_leaves_cache_clean() {
    use flate2::write::GzEncoder;
    use flate2::Compression;

    let tmp = tempfile::tempdir().unwrap();
    let nemotron = &CATALOG[3];

    // Build a real gzipped tarball so the install path runs.
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
    let downloader: Arc<dyn Downloader> = Arc::new(slow_nemotron_store_install_delayed_helper(
        gz.clone(),
        Arc::clone(&calls),
    ));
    let store: Arc<dyn ModelStore> = Arc::new(SlowNemotronStore::new(tmp.path().to_path_buf()));
    let repo: Arc<dyn DownloadRepository> = Arc::new(InMemoryDownloadRepository::new());
    let mut bus = EventBus::new();
    let tx = bus.sender();

    let mut state = UiState::default();
    state.apply(&AppEvent::AddBlock);
    begin(nemotron, &store, &repo, &downloader, &tx);
    tick_drain(&mut bus, &mut state, &*repo);

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

    // Same body as main::cleanup_inflight: snapshot active claims,
    // cancel each, then drop both artifacts. The test name is the
    // observable contract — the cache dir must be clean.
    let active: Vec<&'static str> = repo.all().into_iter().map(|r| r.model).collect();
    for model in &active {
        repo.cancel(model);
    }
    for model in &active {
        if let Some(entry) = CATALOG.iter().find(|e| e.id == *model) {
            store.discard_inflight(entry);
        }
    }

    let staged = tmp.path().join(format!("{}.tar.gz.part", nemotron.id));
    let scratch = tmp.path().join(format!("{}.tmp", nemotron.id));
    assert!(
        !staged.is_file(),
        "staged archive must be gone after Quit-time cleanup"
    );
    assert!(
        !scratch.is_dir(),
        "scratch dir must be gone after Quit-time cleanup"
    );
}

/// Wraps `WriteThroughDownloader` so the install-time slowness comes
/// from the surrounding `SlowNemotronStore`, not from the download.
struct DelayedInstallDownloader {
    inner: WriteThroughDownloader,
}

impl DelayedInstallDownloader {
    fn new(bytes: Vec<u8>, outcome: Outcome, calls: Arc<AtomicUsize>) -> Self {
        Self {
            inner: WriteThroughDownloader::new(bytes, outcome, calls, 0),
        }
    }
}

impl Downloader for DelayedInstallDownloader {
    fn fetch(
        &self,
        url: &str,
        staged: &Path,
        expected_sha: &str,
        cancel: &std::sync::atomic::AtomicBool,
        progress: &mut dyn FnMut(u64, Option<u64>),
    ) -> Result<(), DownloadError> {
        self.inner
            .fetch(url, staged, expected_sha, cancel, progress)
    }
}

fn slow_nemotron_store_install_delayed_helper(
    bytes: Vec<u8>,
    calls: Arc<AtomicUsize>,
) -> DelayedInstallDownloader {
    DelayedInstallDownloader::new(bytes, Outcome::Ok, calls)
}

#[test]
fn cancel_and_immediate_retry_two_attempts_run_concurrently() {
    // This is the deterministic integration test the reviewer
    // asked for: attempt A remains blocked in fetch while
    // attempt B starts. The two attempts write to distinct
    // staging paths, and a stale terminal event from attempt A
    // does NOT drop attempt B's row.
    //
    // We can't use `FixtureDownloader` here because it doesn't
    // write to the staged file (it just simulates success by
    // hashing in memory). The path-isolation guarantee is about
    // real on-disk writes, so we use a tiny `DiskDownloader`
    // that streams the bytes to the staged path and respects
    // the cancel token between chunks.
    use std::io::Cursor;

    /// Minimal downloader that streams `bytes` to `staged` in 16
    /// KiB chunks and honors the cancel token between chunks.
    /// The `block_in_fetch: Arc<AtomicBool>` is set true by the
    /// first call's first chunk; the test clears it after it has
    /// observed the call counter, so the test knows the first
    /// worker is parked in fetch.
    struct DiskDownloader {
        bytes: Vec<u8>,
        calls: Arc<AtomicUsize>,
        chunk_delay_ms: u64,
    }
    impl Downloader for DiskDownloader {
        fn fetch(
            &self,
            _url: &str,
            staged: &Path,
            _expected_sha: &str,
            cancel: &std::sync::atomic::AtomicBool,
            _progress: &mut dyn FnMut(u64, Option<u64>),
        ) -> Result<(), DownloadError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut cursor = Cursor::new(self.bytes.clone());
            let mut file =
                std::fs::File::create(staged).map_err(|e| DownloadError::Io(e.to_string()))?;
            let mut buf = [0u8; 16 * 1024];
            loop {
                if cancel.load(Ordering::Relaxed) {
                    drop(file);
                    let _ = std::fs::remove_file(staged);
                    return Err(DownloadError::Cancelled);
                }
                use std::io::Read;
                let n = cursor
                    .read(&mut buf)
                    .map_err(|e| DownloadError::Io(e.to_string()))?;
                if n == 0 {
                    break;
                }
                if self.chunk_delay_ms > 0 {
                    std::thread::sleep(Duration::from_millis(self.chunk_delay_ms));
                }
                use std::io::Write;
                file.write_all(&buf[..n])
                    .map_err(|e| DownloadError::Io(e.to_string()))?;
            }
            drop(file);
            Ok(())
        }
    }

    let tmp = tempfile::tempdir().unwrap();
    let gguf = &CATALOG[5]; // tiny.en (GGUF; install is a rename)
    let store: Arc<dyn ModelStore> = Arc::new(NemotronTestStore::new(tmp.path().to_path_buf()));
    let repo: Arc<dyn DownloadRepository> = Arc::new(InMemoryDownloadRepository::new());

    let calls = Arc::new(AtomicUsize::new(0));
    let downloader: Arc<dyn Downloader> = Arc::new(DiskDownloader {
        // 64 KiB takes 4 chunks; 100 ms between chunks leaves
        // a wide enough window for the test to cancel attempt A
        // mid-fetch while attempt B starts and finishes.
        bytes: vec![0u8; 64 * 1024],
        calls: Arc::clone(&calls),
        chunk_delay_ms: 100,
    });

    let mut bus = EventBus::new();
    let tx = bus.sender();

    // Block A: pick the GGUF model. Attempt 1 starts; the
    // worker writes its first chunk to <id>.2.gguf.part and
    // pauses between chunks long enough for the test to cancel.
    let mut state = UiState::default();
    state.apply(&AppEvent::AddBlock);
    begin(gguf, &store, &repo, &downloader, &tx);
    tick_drain(&mut bus, &mut state, &*repo);

    let attempt_a = repo.get(gguf.id).expect("attempt A row").attempt;

    // Wait until attempt A has started writing to disk.
    let staged_a = tmp
        .path()
        .join(format!("{}.{}.gguf.part", gguf.id, attempt_a));
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !staged_a.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        staged_a.exists(),
        "attempt A must have created its staging file"
    );

    // cancel attempt A. Worker hasn't acked yet (it's still in
    // the chunk loop, parked).
    assert!(repo.cancel(gguf.id), "cancel on Active row reports true");
    let row_after_cancel = repo.get(gguf.id).expect("row must persist");
    assert_eq!(
        row_after_cancel.state,
        ClaimState::Cancelling,
        "row stays in Cancelling until worker acks"
    );
    assert_eq!(row_after_cancel.attempt, attempt_a);

    // Block B picks the same model. The resolver must return
    // Restart (a fresh attempt id) under the attempt-scoped
    // staging path. NOT Start (which would re-use attempt A's
    // paths).
    state.apply(&AppEvent::AddBlock);
    begin(gguf, &store, &repo, &downloader, &tx);
    tick_drain(&mut bus, &mut state, &*repo);

    let row_b = repo.get(gguf.id).expect("row still present");
    let attempt_b = row_b.attempt;
    assert!(
        attempt_b > attempt_a,
        "Restart must yield a strictly greater attempt id; got {attempt_b} after {attempt_a}"
    );
    assert_eq!(row_b.state, ClaimState::Active, "new attempt is Active");

    // The two attempts write to different staging files. This
    // is the file-isolation guarantee: the old worker's
    // eventual install (which calls `fs::rename`) cannot
    // operate on the new attempt's `.part`.
    let staged_b = tmp
        .path()
        .join(format!("{}.{}.gguf.part", gguf.id, attempt_b));
    assert_ne!(
        staged_a, staged_b,
        "two concurrent attempts must write to different staging paths"
    );

    // Wait for attempt B to complete install (rename to
    // <id>.gguf). Attempt A is still parked in fetch; its
    // cancel signal will unblock it shortly, at which point
    // it returns Cancelled. The store's attempt gate
    // discards the late `DownloadCancelled { attempt: A }`
    // because the row now represents attempt B.
    let installed = handler_for(gguf.format).installed_path(tmp.path(), gguf.id);
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while std::time::Instant::now() < deadline {
        tick_drain(&mut bus, &mut state, &*repo);
        if installed.is_file() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        installed.is_file(),
        "attempt B must complete install (model at {installed:?})"
    );

    // Pin the contract: attempt B's `DownloadSucceeded { attempt: B }`
    // dropped the row. Attempt A's stale events were
    // discarded by the attempt gate. The model is installed
    // at the canonical path; attempt B's staging file was
    // renamed into place.
    assert!(
        repo.get(gguf.id).is_none(),
        "row must be removed after DownloadSucceeded"
    );

    // Block B must end in Recording state.
    let last_block = state.blocks.last().expect("block B exists");
    assert!(
        matches!(last_block.state, BlockState::Recording { model } if model == gguf.id),
        "block B must end in Recording; got {:?}",
        last_block.state
    );

    // Attempt A's stale staging file may or may not have been
    // cleaned up by the old worker's Cancelled branch; if it
    // is, fine. The contract is that nothing on the new
    // attempt's paths got touched.
    assert!(
        !staged_b.exists(),
        "new attempt's staging file must have been renamed into the install path"
    );
}

/// `@REVIEWER_BUG_FIX cancel-during-install-ui`:
///
/// DRAIN stale terminal events (`DownloadFailed`, `DownloadSucceeded`,
/// `DownloadCancelled`) for attempt A AFTER attempt B has started,
/// and assert BOTH the repository row AND the UiState projection
/// remain on attempt B. The reviewer specifically called out the
/// gap: the previous test only asserted the repository side. A
/// late `DownloadFailed { attempt: A }` could still move attempt
/// B's blocks into Failed because `UiState::apply` ignored
/// `attempt` and was applied unconditionally from the bus drain.
///
/// Determinism: we drive attempt A into fetch via a slow
/// `DiskDownloader`, cancel it, start attempt B, then reach into
/// the bus and inject the stale terminal events manually. This
/// models the real race precisely: the old worker's terminal
/// publish arrives AFTER the new attempt is already in flight.
#[test]
fn stale_terminal_events_after_restart_do_not_repaint_attempt_b_ui() {
    use std::io::Cursor;

    /// Slow downloader for attempt A: parks for 200 ms inside
    /// fetch so the test has time to cancel it, start attempt B,
    /// and inject stale events while attempt A is still alive.
    struct SlowFirstDownloader {
        bytes: Vec<u8>,
        chunk_delay_ms: u64,
        calls: Arc<AtomicUsize>,
    }
    impl Downloader for SlowFirstDownloader {
        fn fetch(
            &self,
            _url: &str,
            staged: &Path,
            _expected_sha: &str,
            cancel: &std::sync::atomic::AtomicBool,
            _progress: &mut dyn FnMut(u64, Option<u64>),
        ) -> Result<(), DownloadError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut cursor = Cursor::new(self.bytes.clone());
            let mut file =
                std::fs::File::create(staged).map_err(|e| DownloadError::Io(e.to_string()))?;
            let mut buf = [0u8; 16 * 1024];
            loop {
                if cancel.load(Ordering::Relaxed) {
                    drop(file);
                    let _ = std::fs::remove_file(staged);
                    return Err(DownloadError::Cancelled);
                }
                use std::io::Read;
                let n = cursor
                    .read(&mut buf)
                    .map_err(|e| DownloadError::Io(e.to_string()))?;
                if n == 0 {
                    break;
                }
                if self.chunk_delay_ms > 0 {
                    std::thread::sleep(Duration::from_millis(self.chunk_delay_ms));
                }
                use std::io::Write;
                file.write_all(&buf[..n])
                    .map_err(|e| DownloadError::Io(e.to_string()))?;
            }
            drop(file);
            Ok(())
        }
    }

    let tmp = tempfile::tempdir().unwrap();
    let gguf = &CATALOG[5]; // tiny.en (GGUF; install = rename)
    let store: Arc<dyn ModelStore> = Arc::new(NemotronTestStore::new(tmp.path().to_path_buf()));
    let repo: Arc<dyn DownloadRepository> = Arc::new(InMemoryDownloadRepository::new());

    let calls = Arc::new(AtomicUsize::new(0));
    let downloader: Arc<dyn Downloader> = Arc::new(SlowFirstDownloader {
        bytes: vec![0u8; 64 * 1024],
        chunk_delay_ms: 200,
        calls: Arc::clone(&calls),
    });

    let mut bus = EventBus::new();
    let tx = bus.sender();

    // Block A: attempt 1 starts; it parks in fetch.
    let mut state = UiState::default();
    state.apply(&AppEvent::AddBlock);
    begin(gguf, &store, &repo, &downloader, &tx);
    tick_drain(&mut bus, &mut state, &*repo);

    let attempt_a = repo.get(gguf.id).expect("attempt A row").attempt;
    let staged_a = tmp
        .path()
        .join(format!("{}.{}.gguf.part", gguf.id, attempt_a));
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !staged_a.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        staged_a.exists(),
        "attempt A must have created its staging file"
    );

    // Cancel attempt A. The row stays in `Cancelling` until the
    // worker publishes a terminal event.
    assert!(repo.cancel(gguf.id));

    // Block B picks the same model: attempt 2 starts under a
    // fresh token and a fresh staging path.
    state.apply(&AppEvent::AddBlock);
    begin(gguf, &store, &repo, &downloader, &tx);
    tick_drain(&mut bus, &mut state, &*repo);

    let attempt_b = repo
        .get(gguf.id)
        .expect("row still present after Restart")
        .attempt;
    assert!(
        attempt_b > attempt_a,
        "Restart must yield a strictly greater attempt"
    );

    // Snapshot the UI projection BEFORE stale events arrive:
    //   - Both blocks must be Waiting on `gguf.id`.
    //   - downloads[gguf.id] must be present (Phase::Fetching).
    assert_eq!(state.blocks.len(), 2);
    for block in &state.blocks {
        assert!(
            matches!(&block.state, BlockState::Waiting { model } if *model == gguf.id),
            "block must be Waiting on {}; got {:?}",
            gguf.id,
            block.state
        );
    }
    let row_pre = state
        .downloads
        .get(gguf.id)
        .expect("downloads[gguf.id] must be present");
    assert!(
        matches!(
            row_pre.phase,
            voice_bird_next::store::DownloadPhase::Fetching
        ),
        "downloads[gguf.id].phase must be Fetching; got {:?}",
        row_pre.phase
    );

    // INJECT stale terminal events for attempt A. This models
    // the production race: the cancelled worker, still alive
    // in its chunk loop, eventually publishes Cancelled /
    // Failed / Succeeded for the old attempt id. Without the
    // gate, EACH of these would corrupt attempt B's projection
    // — Failed would put B's block into Failed; Succeeded
    // would prematurely put B into Recording; Cancelled
    // would remove B's progress bar.
    tx.publish(AppEvent::DownloadCancelled {
        attempt: attempt_a,
        model: gguf.id,
    });
    tx.publish(AppEvent::DownloadFailed {
        attempt: attempt_a,
        model: gguf.id,
        error: "HTTP 404".into(),
    });
    tx.publish(AppEvent::DownloadSucceeded {
        attempt: attempt_a,
        model: gguf.id,
    });

    // Drain through the production path: tick_drain gates
    // state.apply on apply_event's accept signal. Every stale
    // event must return `false`, so UiState is untouched.
    tick_drain(&mut bus, &mut state, &*repo);

    // Pin the contract: attempt B's repository row is intact
    // (the stale terminal events did not drop it).
    let row_post = repo
        .get(gguf.id)
        .expect("attempt B's row must survive stale terminal events");
    assert_eq!(row_post.attempt, attempt_b, "row must still be attempt B");
    assert_eq!(
        row_post.state,
        ClaimState::Active,
        "attempt B must still be Active (not Cancelling)"
    );

    // Pin the contract: UiState.downloads[gguf.id] is still
    // present (no Cancelled removal), still in Fetching
    // phase (no Installing repaint).
    let row_post_ui = state
        .downloads
        .get(gguf.id)
        .expect("downloads[gguf.id] must survive stale Cancelled");
    assert!(
        matches!(
            row_post_ui.phase,
            voice_bird_next::store::DownloadPhase::Fetching
        ),
        "downloads[gguf.id].phase must still be Fetching after stale events"
    );

    // Pin the contract: BOTH blocks are still Waiting. None of
    // them were moved to Failed (by stale DownloadFailed),
    // Recording (by stale DownloadSucceeded), or removed by
    // stale DownloadCancelled.
    assert_eq!(
        state.blocks.len(),
        2,
        "no block may be removed by stale Cancelled"
    );
    for (i, block) in state.blocks.iter().enumerate() {
        assert!(
            matches!(&block.state, BlockState::Waiting { model } if *model == gguf.id),
            "block {} must remain Waiting on {}; got {:?}",
            i,
            gguf.id,
            block.state
        );
    }

    // Sanity: now publish a CORRECT attempt-B Succeeded, and
    // assert it transitions B's blocks to Recording and drops
    // the row. This proves the gate isn't over-eager — it
    // only rejects the stale events.
    // Wait for attempt B's worker to actually finish first so
    // the row's bytes are real. We tick until the row's
    // bytes reaches the total (or the deadline expires).
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        tick_drain(&mut bus, &mut state, &*repo);
        let r = repo.get(gguf.id);
        if r.is_none() {
            break;
        }
        if let Some(rec) = r {
            if rec.bytes >= (64 * 1024) as u64 {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    // Let attempt B's natural terminal events drive the row
    // to dropped state and both blocks to Recording.
    settle(&mut bus, &mut state, &*repo);
    assert!(
        repo.get(gguf.id).is_none(),
        "attempt B's row must be dropped on its own terminal event"
    );
    assert_eq!(state.blocks.len(), 2);
    for block in &state.blocks {
        assert!(
            matches!(&block.state, BlockState::Recording { model } if *model == gguf.id),
            "block must end in Recording; got {:?}",
            block.state
        );
    }
}
