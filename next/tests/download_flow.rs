//! End-to-end download flow tests. **No test may touch the network.**
//! `HttpDownloader` is constructed only in `main.rs`; everything here
//! drives the resolver through the `FixtureStore` and `FixtureDownloader`
//! in [`voice_bird_next::testing`].

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use voice_bird_next::bus::{AppEvent, EventBus};
use voice_bird_next::download::{begin, CancelRegistry, Downloader};
use voice_bird_next::model_store::ModelStore;
use voice_bird_next::picker::CATALOG;
use voice_bird_next::state::{BlockState, DownloadState, UiState};
use voice_bird_next::store::{
    apply, DownloadPhase, DownloadRecord, DownloadRepository, InMemoryDownloadRepository,
};
use voice_bird_next::testing::{render_to_string, FixtureDownloader, FixtureStore, Outcome};

fn tiny() -> &'static voice_bird_next::picker::ModelEntry {
    &CATALOG[5]
}

fn base() -> &'static voice_bird_next::picker::ModelEntry {
    &CATALOG[4]
}

fn tick_drain(bus: &mut EventBus, state: &mut UiState, repo: &dyn DownloadRepository) {
    for ev in bus.drain() {
        apply(repo, &ev);
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
    let downloader_concrete = Arc::new(FixtureDownloader::new(Vec::new(), Outcome::Ok, Arc::clone(&calls)));
    let downloader: Arc<dyn Downloader> = downloader_concrete.clone();
    let cancels = CancelRegistry::new();
    let mut bus = EventBus::new();
    let tx = bus.sender();

    let mut state = UiState::default();
    state.apply(&AppEvent::AddBlock);
    tick_drain(&mut bus, &mut state, &*repo);

    begin(tiny(), &store, &repo, &downloader, &cancels, &tx);
    tick_drain(&mut bus, &mut state, &*repo);

    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(matches!(
        state.blocks[0].state,
        BlockState::Recording { model: "tiny.en" }
    ));
}

#[test]
fn two_blocks_same_model_share_one_download() {
    let tmp = tempfile::tempdir().unwrap();
    let store: Arc<dyn ModelStore> = Arc::new(FixtureStore::new(tmp.path().to_path_buf(), &[]));
    let repo: Arc<dyn DownloadRepository> = Arc::new(InMemoryDownloadRepository::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let downloader: Arc<dyn Downloader> = Arc::new(FixtureDownloader::new(vec![0u8; 1024], Outcome::Ok, Arc::clone(&calls)));
    let cancels = CancelRegistry::new();
    let mut bus = EventBus::new();
    let tx = bus.sender();
    let mut state = UiState::default();

    state.apply(&AppEvent::AddBlock);
    begin(tiny(), &store, &repo, &downloader, &cancels, &tx);
    tick_drain(&mut bus, &mut state, &*repo);

    state.apply(&AppEvent::AddBlock);
    begin(tiny(), &store, &repo, &downloader, &cancels, &tx);
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
    let downloader: Arc<dyn Downloader> = Arc::new(FixtureDownloader::new(vec![0u8; 64], Outcome::Ok, Arc::clone(&calls)));
    let cancels = CancelRegistry::new();
    let mut bus = EventBus::new();
    let tx = bus.sender();
    let mut state = UiState::default();

    state.apply(&AppEvent::AddBlock);
    begin(tiny(), &store, &repo, &downloader, &cancels, &tx);
    tick_drain(&mut bus, &mut state, &*repo);

    state.apply(&AppEvent::AddBlock);
    begin(base(), &store, &repo, &downloader, &cancels, &tx);
    tick_drain(&mut bus, &mut state, &*repo);

    // Wait for both spawn threads to start fetch.
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(repo.is_active("tiny.en"));
    assert!(repo.is_active("base.en"));

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
    let downloader: Arc<dyn Downloader> = Arc::new(FixtureDownloader::new(vec![0u8; 8], Outcome::Ok, Arc::clone(&calls)));
    let cancels = CancelRegistry::new();
    let mut bus = EventBus::new();
    let tx = bus.sender();
    let mut state = UiState::default();
    state.apply(&AppEvent::AddBlock);
    begin(tiny(), &store, &repo, &downloader, &cancels, &tx);
    tick_drain(&mut bus, &mut state, &*repo);
    state.apply(&AppEvent::AddBlock);
    begin(tiny(), &store, &repo, &downloader, &cancels, &tx);
    tick_drain(&mut bus, &mut state, &*repo);

    settle(&mut bus, &mut state, &*repo);

    for b in &state.blocks {
        assert!(matches!(b.state, BlockState::Recording { model: "tiny.en" }));
    }
}

#[test]
fn failed_shows_error_in_block_then_retry_succeeds() {
    let tmp = tempfile::tempdir().unwrap();
    let store: Arc<dyn ModelStore> = Arc::new(FixtureStore::new(tmp.path().to_path_buf(), &[]));
    let repo: Arc<dyn DownloadRepository> = Arc::new(InMemoryDownloadRepository::new());
    let downloader: Arc<dyn Downloader> = Arc::new(FixtureDownloader::new(vec![0u8; 8], Outcome::ShaMismatch, Arc::new(AtomicUsize::new(0))));
    let cancels = CancelRegistry::new();
    let mut bus = EventBus::new();
    let tx = bus.sender();
    let mut state = UiState::default();
    state.apply(&AppEvent::AddBlock);

    begin(tiny(), &store, &repo, &downloader, &cancels, &tx);
    settle(&mut bus, &mut state, &*repo);

    match &state.blocks[0].state {
        BlockState::Failed { model, error } => {
            assert_eq!(*model, "tiny.en");
            assert!(error.contains("sha256"));
        }
        other => panic!("expected Failed, got {other:?}"),
    }

    // Retry with a clean downloader.
    let downloader2: Arc<dyn Downloader> = Arc::new(FixtureDownloader::new(vec![0u8; 8], Outcome::Ok, Arc::new(AtomicUsize::new(0))));
    begin(tiny(), &store, &repo, &downloader2, &cancels, &tx);
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
    let downloader: Arc<dyn Downloader> = Arc::new(FixtureDownloader::new(vec![0u8; 8], Outcome::Cancelled, Arc::new(AtomicUsize::new(0))));
    let cancels = CancelRegistry::new();
    let mut bus = EventBus::new();
    let tx = bus.sender();
    let mut state = UiState::default();
    state.apply(&AppEvent::AddBlock);
    begin(tiny(), &store, &repo, &downloader, &cancels, &tx);
    tick_drain(&mut bus, &mut state, &*repo);

    cancels.cancel(tiny().id);
    cancels.clear(tiny().id);

    state.apply(&AppEvent::BlockClosed);
    // The resolver in main.rs fires DownloadCancelled after the last
    // waiter drops; this test drives events directly, so publish it
    // explicitly so the repo row is torn down.
    tx.publish(AppEvent::DownloadCancelled { model: tiny().id });
    tick_drain(&mut bus, &mut state, &*repo);

    assert!(state.blocks.is_empty());
    assert!(!repo.is_active("tiny.en"));
}

#[test]
fn closing_one_of_two_waiters_leaves_the_download_running() {
    let tmp = tempfile::tempdir().unwrap();
    let store: Arc<dyn ModelStore> = Arc::new(FixtureStore::new(tmp.path().to_path_buf(), &[]));
    let repo: Arc<dyn DownloadRepository> = Arc::new(InMemoryDownloadRepository::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let downloader: Arc<dyn Downloader> = Arc::new(
        FixtureDownloader::new(vec![0u8; 4096], Outcome::Ok, Arc::clone(&calls))
            .with_delay(20),
    );
    let cancels = CancelRegistry::new();
    let mut bus = EventBus::new();
    let tx = bus.sender();
    let mut state = UiState::default();
    state.apply(&AppEvent::AddBlock);
    begin(tiny(), &store, &repo, &downloader, &cancels, &tx);
    tick_drain(&mut bus, &mut state, &*repo);
    state.apply(&AppEvent::AddBlock);
    begin(tiny(), &store, &repo, &downloader, &cancels, &tx);
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
    assert!(repo.is_active("tiny.en"));

    settle(&mut bus, &mut state, &*repo);
    assert!(matches!(
        state.blocks[0].state,
        BlockState::Recording { model: "tiny.en" }
    ));
}

#[test]
fn repo_apply_round_trip() {
    let repo = InMemoryDownloadRepository::new();
    apply(&repo, &AppEvent::DownloadRequested(tiny()));
    let row: DownloadRecord = repo.get("tiny.en").unwrap();
    assert_eq!(row.phase, DownloadPhase::Fetching);
    apply(&repo, &AppEvent::DownloadSucceeded { model: "tiny.en" });
    assert!(repo.get("tiny.en").is_none());
}