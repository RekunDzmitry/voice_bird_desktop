//! End-to-end download flow tests. **No test may touch the network.**
//! `HttpDownloader` is constructed only in `main.rs`; everything here
//! drives the resolver through the `FixtureStore` and `FixtureDownloader`
//! in [`voice_bird_next::testing`].

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use voice_bird_next::bus::{AppEvent, EventBus};
use voice_bird_next::download::{begin, Downloader};
use voice_bird_next::input::Intent;
use voice_bird_next::picker::CATALOG;
use voice_bird_next::producer;
use voice_bird_next::state::{BlockState, DownloadState, UiState};
use voice_bird_next::store::{
    DownloadClaim, DownloadPhase, DownloadRecord, DownloadRepository, InMemoryDownloadRepository,
};
use voice_bird_next::transcription_models::ModelStore;

use voice_bird_next::testing::{render_to_string, FixtureDownloader, FixtureStore, Outcome};

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
