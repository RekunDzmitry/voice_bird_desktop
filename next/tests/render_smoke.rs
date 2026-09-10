use pretty_assertions::assert_eq;
use proptest::prelude::*;
use voice_bird_next::{picker::ModelPicker, state::{Block, UiState}, testing::render_to_string};

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

/// Golden snapshot of the window after three `+ → Enter` presses (each
/// adding a block bound to a different catalog model). Refresh alongside
/// `idle_100x30.txt` with `UPDATE_SNAPSHOTS=1`.
#[test]
fn three_blocks_100x30_matches_golden() {
    let state = UiState {
        blocks: vec![
            Block { id: 1, model: "distil-small.en".to_string() },
            Block { id: 2, model: "distil-large-v3".to_string() },
            Block { id: 3, model: "large-v3-turbo".to_string() },
        ],
        next_block_id: 4,
        ..Default::default()
    };
    let out = render_to_string(&state, 100, 30);
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(THREE_BLOCKS, &out).expect("write golden");
    }
    let expected = std::fs::read_to_string(THREE_BLOCKS).expect("read golden");
    assert_eq!(out, expected);
}

/// Snapshot assertion that the picker overlay renders with the marker on
/// the highlighted catalog row. Not a golden — explicit substring checks
/// so the test survives cosmetic whitespace tweaks inside the paragraph.
#[test]
fn picker_overlay_renders_catalog_with_marker() {
    use voice_bird_next::picker::PickerIntent;
    let state = UiState {
        blocks: vec![Block {
            id: 1,
            model: "distil-small.en".to_string(),
        }],
        picker: Some(ModelPicker::open(PickerIntent::AddBlock)),
        ..Default::default()
    };
    let out = render_to_string(&state, 100, 30);
    assert!(
        out.contains("\u{25b6} distil-small.en"),
        "expected marker on first catalog row; got:\n{out}"
    );
    assert!(
        out.contains("  distil-large-v3"),
        "expected second catalog row padded; got:\n{out}"
    );
    assert!(
        out.contains("Pick a model (Esc to cancel)"),
        "expected picker title in overlay; got:\n{out}"
    );
    // The existing block must still be visible behind the overlay.
    assert!(
        out.contains("1 · distil-small.en"),
        "expected block label behind overlay; got:\n{out}"
    );
}

proptest! {
    #[test]
    fn render_never_panics_for_any_size(w in 1u16..200, h in 1u16..80) {
        let _ = render_to_string(&UiState::default(), w, h);
    }

    /// Random block counts across random terminal sizes must never panic.
    /// The Direction::Horizontal layout clamps zero-width columns;
    /// ratatui skips the draw.
    #[test]
    fn render_never_panics_with_random_block_count(
        w in 1u16..200,
        h in 1u16..80,
        blocks in 0usize..20,
    ) {
        let state = UiState {
            blocks: (1..=blocks as u32)
                .map(|i| Block {
                    id: i,
                    model: "tiny.en".to_string(),
                })
                .collect(),
            ..Default::default()
        };
        let _ = render_to_string(&state, w, h);
    }
}