use pretty_assertions::assert_eq;
use proptest::prelude::*;
use voice_bird_next::{state::UiState, testing::render_to_string};

const GOLDEN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/snapshots/idle_100x30.txt");
const THREE_BLOCKS: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/snapshots/three_blocks_100x30.txt");

/// Golden snapshot of the idle window. Refresh with
/// `UPDATE_SNAPSHOTS=1 cargo test -p voice-bird-next` and review the diff.
#[test]
fn idle_100x30_matches_golden() {
    let out = render_to_string(&UiState::default(), 100, 30);
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(GOLDEN, &out).expect("write golden");
    }
    let expected = std::fs::read_to_string(GOLDEN).expect("read golden");
    assert_eq!(out, expected);
}

/// Golden snapshot of the window after three `'+'` presses. Refresh
/// alongside `idle_100x30.txt` with `UPDATE_SNAPSHOTS=1`.
#[test]
fn three_blocks_100x30_matches_golden() {
    let state = UiState {
        blocks: 3,
        ..Default::default()
    };
    let out = render_to_string(&state, 100, 30);
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(THREE_BLOCKS, &out).expect("write golden");
    }
    let expected = std::fs::read_to_string(THREE_BLOCKS).expect("read golden");
    assert_eq!(out, expected);
}

proptest! {
    #[test]
    fn render_never_panics_for_any_size(w in 1u16..200, h in 1u16..80) {
        let _ = render_to_string(&UiState::default(), w, h);
    }

    /// Random block counts across random terminal sizes must never panic.
    /// Layout::vertical clamps zero-height rows; ratatui skips the draw.
    #[test]
    fn render_never_panics_with_random_block_count(
        w in 1u16..200,
        h in 1u16..80,
        blocks in 0usize..20,
    ) {
        let state = UiState { blocks, ..Default::default() };
        let _ = render_to_string(&state, w, h);
    }
}
