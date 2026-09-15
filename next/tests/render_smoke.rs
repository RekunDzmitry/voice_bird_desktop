use pretty_assertions::assert_eq;
use proptest::prelude::*;
use voice_bird_next::{
    picker::{ModelPicker, PickerIntent},
    state::{Block, BlockState, DownloadState, UiState},
    store::DownloadPhase,
    testing::render_to_string,
};

const IDLE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/snapshots/idle_100x30.txt");
const THREE_BLOCKS: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/snapshots/three_blocks_100x30.txt");
const PICKING: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/snapshots/picking_100x30.txt");
const DOWNLOADING_TWO_BLOCKS: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/snapshots/downloading_two_blocks_100x30.txt"
);
const FAILED: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/snapshots/failed_100x30.txt");

#[test]
fn idle_100x30_matches_golden() {
    let out = render_to_string(&UiState::default(), 100, 30);
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(IDLE, &out).expect("write golden");
    }
    let expected = std::fs::read_to_string(IDLE).expect("read golden");
    assert_eq!(out, expected);
}

#[test]
fn three_blocks_100x30_matches_golden() {
    let state = UiState {
        blocks: vec![
            Block {
                id: 1,
                state: BlockState::Recording {
                    model: "distil-small.en",
                },
            },
            Block {
                id: 2,
                state: BlockState::Recording {
                    model: "distil-large-v3",
                },
            },
            Block {
                id: 3,
                state: BlockState::Recording {
                    model: "large-v3-turbo",
                },
            },
        ],
        focus: 0,
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

#[test]
fn picking_100x30_matches_golden() {
    let state = UiState {
        blocks: vec![Block {
            id: 1,
            state: BlockState::Picking(ModelPicker::open(PickerIntent::AddBlock)),
        }],
        focus: 0,
        next_block_id: 2,
        ..Default::default()
    };
    let out = render_to_string(&state, 100, 30);
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(PICKING, &out).expect("write golden");
    }
    let expected = std::fs::read_to_string(PICKING).expect("read golden");
    assert_eq!(out, expected);
}

#[test]
fn downloading_two_blocks_100x30_matches_golden() {
    let mut state = UiState {
        blocks: vec![
            Block {
                id: 1,
                state: BlockState::Waiting { model: "tiny.en" },
            },
            Block {
                id: 2,
                state: BlockState::Waiting { model: "tiny.en" },
            },
        ],
        focus: 0,
        next_block_id: 3,
        ..Default::default()
    };
    state.downloads.insert(
        "tiny.en",
        DownloadState {
            phase: DownloadPhase::Fetching,
            bytes: 50,
            total: Some(100),
            bytes_per_sec: 0,
        },
    );
    let out = render_to_string(&state, 100, 30);
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(DOWNLOADING_TWO_BLOCKS, &out).expect("write golden");
    }
    let expected = std::fs::read_to_string(DOWNLOADING_TWO_BLOCKS).expect("read golden");
    assert_eq!(out, expected);
}

#[test]
fn failed_100x30_matches_golden() {
    let state = UiState {
        blocks: vec![Block {
            id: 1,
            state: BlockState::Failed {
                model: "tiny.en",
                error: "HTTP 404".to_string(),
            },
        }],
        focus: 0,
        next_block_id: 2,
        ..Default::default()
    };
    let out = render_to_string(&state, 100, 30);
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(FAILED, &out).expect("write golden");
    }
    let expected = std::fs::read_to_string(FAILED).expect("read golden");
    assert_eq!(out, expected);
}

#[test]
fn picker_renders_catalog_inside_focused_block() {
    let state = UiState {
        blocks: vec![Block {
            id: 1,
            state: BlockState::Picking(ModelPicker::open(PickerIntent::AddBlock)),
        }],
        focus: 0,
        next_block_id: 2,
        ..Default::default()
    };
    let out = render_to_string(&state, 100, 30);
    assert!(out.contains("\u{25b6} distil-small.en"));
    assert!(out.contains("  distil-large-v3"));
    assert!(out.contains("pick a model"));
}

proptest! {
    #[test]
    fn render_never_panics_for_any_size(w in 1u16..200, h in 1u16..80) {
        let _ = render_to_string(&UiState::default(), w, h);
    }

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
                    state: BlockState::Recording { model: "tiny.en" },
                })
                .collect(),
            focus: 0,
            next_block_id: blocks as u32 + 1,
            ..Default::default()
        };
        let _ = render_to_string(&state, w, h);
    }

    #[test]
    fn render_never_panics_with_random_download_state(
        w in 1u16..200,
        h in 1u16..80,
        bytes in 0u64..2_000_000_000u64,
        total_raw in 0u64..2_000_000_000u64,
    ) {
        let total = if total_raw == 0 {
            None
        } else if total_raw % 2 == 0 {
            Some(total_raw)
        } else {
            Some(0)
        };
        let mut state = UiState {
            blocks: vec![Block {
                id: 1,
                state: BlockState::Waiting { model: "tiny.en" },
            }],
            focus: 0,
            next_block_id: 2,
            ..Default::default()
        };
        state.downloads.insert(
            "tiny.en",
            DownloadState {
                phase: DownloadPhase::Fetching,
                bytes,
                total,
                bytes_per_sec: 0,
            },
        );
        let _ = render_to_string(&state, w, h);
    }
}