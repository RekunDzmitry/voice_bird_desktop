use pretty_assertions::assert_eq;
use proptest::prelude::*;
use voice_bird_next::{
    picker::{ModelPicker, PickerIntent, SessionMenu},
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
const FIVE_BLOCKS_CAPPED: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/snapshots/five_blocks_capped_100x30.txt"
);
const MENU_OPEN: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/snapshots/menu_open_100x30.txt");
const MENU_OPEN_MANY: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/snapshots/menu_open_many_100x30.txt"
);

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

                ..Default::default()
            },
            Block {
                id: 2,
                state: BlockState::Recording {
                    model: "distil-large-v3",
                },

                ..Default::default()
            },
            Block {
                id: 3,
                state: BlockState::Recording {
                    model: "large-v3-turbo",
                },

                ..Default::default()
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

            ..Default::default()
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

                ..Default::default()
            },
            Block {
                id: 2,
                state: BlockState::Waiting { model: "tiny.en" },

                ..Default::default()
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

            ..Default::default()
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

            ..Default::default()
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

#[test]
fn five_blocks_capped_100x30_matches_golden() {
    // Five sessions, menu closed. The cap keeps the window at 4
    // columns; block 1 is hidden (evicted as the oldest-focused
    // visible peer when block 5 was added).
    let mut s = UiState::default();
    for _ in 0..5 {
        s.apply(&voice_bird_next::bus::AppEvent::AddBlock);
    }
    // The reducer's clock has moved: dump the visible ids for
    // sanity (no assertion here — the golden does the rest).
    let visible_ids: Vec<u8> = s.blocks.iter().filter(|b| b.visible).map(|b| b.id).collect();
    assert_eq!(visible_ids, vec![2, 3, 4, 5], "block 1 hidden after 5th AddBlock");
    let out = render_to_string(&s, 100, 30);
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(FIVE_BLOCKS_CAPPED, &out).expect("write golden");
    }
    let expected = std::fs::read_to_string(FIVE_BLOCKS_CAPPED).expect("read golden");
    assert_eq!(out, expected);
}

#[test]
fn menu_open_100x30_matches_golden() {
    // Menu open over the same five sessions. The left panel claims
    // 18 columns; the remaining 4 visible blocks fill the rest.
    // Highlight is on the 5th row (block 5), which is the most-
    // recently-focused one and still visible.
    let mut s = UiState::default();
    for _ in 0..5 {
        s.apply(&voice_bird_next::bus::AppEvent::AddBlock);
    }
    s.menu = Some(SessionMenu::open_at(4));
    let out = render_to_string(&s, 100, 30);
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(MENU_OPEN, &out).expect("write golden");
    }
    let expected = std::fs::read_to_string(MENU_OPEN).expect("read golden");
    assert_eq!(out, expected);
}

#[test]
fn menu_open_many_100x30_matches_golden() {
    // 31 sessions on a 30-row terminal: the menu panel cannot fit
    // them all, so it scrolls. Selection is on session 27 (the
    // user-reported bug case) — the carousel window must contain
    // row 27 with `▶`, even though session 1 is no longer visible.
    let mut s = UiState::default();
    for _ in 0..31 {
        s.apply(&voice_bird_next::bus::AppEvent::AddBlock);
    }
    s.menu = Some(SessionMenu::open_at(26));
    let out = render_to_string(&s, 100, 30);
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(MENU_OPEN_MANY, &out).expect("write golden");
    }
    let expected = std::fs::read_to_string(MENU_OPEN_MANY).expect("read golden");
    assert_eq!(out, expected);
    // Sanity: the highlighted row IS in the rendered window.
    assert!(out.contains("▶ session 27"), "selected row not in window; got:\n{out}");
    // And the rows above/below the selection are also present.
    // `session 1` (the bare row, with the marker or two-space indent)
    // is NOT in the rendered window — that's the bug we're fixing.
    // (substring match on "session 1" would also match
    // "session 10".."session 19", so check for the row format.)
    assert!(
        !out.contains(" session 1\n") && !out.contains("▶ session 1\n"),
        "session 1 row leaked into window; got:\n{out}"
    );
    assert!(out.contains(" session 14"), "expected session 14 in window; got:\n{out}");
    assert!(out.contains("▶ session 27"), "expected session 27 highlighted; got:\n{out}");
    assert!(out.contains(" session 31"), "expected session 31 in window; got:\n{out}");
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
            blocks: (1..=blocks as u8)
                .map(|i| Block {
                    id: i,
                    state: BlockState::Recording { model: "tiny.en" },

                    ..Default::default()
                })
                .collect(),
            focus: 0,
            next_block_id: blocks as u8 + 1,
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

                ..Default::default()
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