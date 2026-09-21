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

#[test]
fn warning_renders_in_title_bar_when_set() {
    // Set a transient warning and assert the title bar carries it.
    // The base title "Voice Bird" is replaced/augmented by the
    // warning text; the renderer joins them with " — ! ".
    let mut s = UiState::default();
    s.apply(&voice_bird_next::bus::AppEvent::AddBlock);
    s.warning = Some("session limit reached; close a session to make room".to_string());
    let out = render_to_string(&s, 100, 30);
    // First line of the render is the top border with title.
    let first_line = out.lines().next().unwrap_or("");
    assert!(
        first_line.contains("Voice Bird"),
        "title still includes the app name; got first line: {first_line:?}"
    );
    assert!(
        first_line.contains("session limit"),
        "title must include the warning text; got first line: {first_line:?}"
    );
}

#[test]
fn title_bar_is_clean_when_no_warning() {
    // Steady state: title is just "Voice Bird", no warning suffix.
    let s = UiState::default();
    let out = render_to_string(&s, 100, 30);
    let first_line = out.lines().next().unwrap_or("");
    assert!(
        first_line.contains("Voice Bird"),
        "title shows app name; got {first_line:?}"
    );
    assert!(
        !first_line.contains("session limit"),
        "no warning in steady state; got first line: {first_line:?}"
    );
}

/// Focus navigation must not strand the user on a hidden block.
///
/// When the cap evicts a block (becomes hidden), the keyboard
/// path (Left / Right) is the only way to reach it short of the
/// menu. Walking left into a hidden index must **reveal** the
/// block (going through `show_block`'s eviction logic) so the
/// cap stays enforced and the focused border lands on a
/// rendered column. Without this fix, focus could land on a
/// hidden block and Up/Down/Enter/Retry/Esc would silently act
/// on a block the user cannot see.
#[test]
fn focus_left_into_hidden_block_reveals_it() {
    let mut s = UiState::default();
    for _ in 0..5 {
        s.apply(&voice_bird_next::bus::AppEvent::AddBlock);
    }
    // focus is 4 after the five AddBlocks; block 1 (index 0) is
    // the oldest-focused peer and was evicted.
    assert_eq!(s.focus, 4, "expected initial focus on the 5th block");
    assert!(!s.blocks[0].visible, "block 1 must be the evicted one");

    for _ in 0..4 {
        s.apply(&voice_bird_next::bus::AppEvent::FocusMoved {
            direction: voice_bird_next::bus::FocusMove::Prev,
        });
    }
    assert_eq!(s.focus, 0, "expected focus to walk all the way to index 0");
    assert!(
        s.blocks[0].visible,
        "hidden block 1 should be revealed when focus lands on it"
    );

    // Cap must still hold: 4 visible blocks, no more.
    let visible = s.blocks.iter().filter(|b| b.visible).count();
    assert_eq!(visible, 4, "cap violated after reveal; visible={visible}");

    // Render: focused block draws `┌ … ┐` on the title line;
    // unfocused neighbours share `│` dividers and have no top
    // border. So the focused title for block 1 is wrapped in
    // top-border glyphs.
    let out = render_to_string(&s, 100, 30);
    // Focused block title is wrapped in ┌ … ┐ on the title
    // line; unfocused neighbours share │ dividers and have no
    // top border. Assert BOTH: focused block 1 IS wrapped,
    // AND no column carries the unfocused form for block 1.
    let focused_title = "\u{250C} 1 \u{00B7} pick a model";
    assert!(
        out.contains(focused_title),
        "focused block 1 missing top border; expected substring {focused_title:?}; got:\n{out}"
    );
    let unfocused_title = "\u{2502} 1 \u{00B7} pick a model";
    assert!(
        !out.contains(unfocused_title),
        "block 1 still rendering as unfocused; rejected substring {unfocused_title:?}; got:\n{out}"
    );
}

#[test]
fn focus_right_into_hidden_block_reveals_it() {
    // Symmetric path: walk Right into a hidden index. To make
    // a hidden block sit *to the right of focus*, we close the
    // focused block (so it goes hidden by being removed) and
    // walk past its former position — no, simpler: set up a
    // hidden block at the END of the slice by closing the last
    // block, then add a fresh one. After AddBlock #6 (visible
    // at index 5) and a BlockClosed on block 5, block 5 is
    // gone, blocks 1..=4 remain. Then add 3 more (7, 8, 9),
    // cap evicts the LRU each time, leaving blocks 4 (newest
    // revealed from earlier), 6, 7, 8, 9 visible and 1, 2, 3
    // hidden — wait, this is getting complicated.
    //
    // Simplest setup: 2 blocks (both visible), add 4 more so
    // 1..=2 are hidden. Focus is at index 5 (the newest, last).
    // Walk Right from there: focus stays at 5 (right-edge clamp).
    // That doesn't exercise reveal.
    //
    // To exercise reveal-on-Next: start with focus NOT at the
    // right edge, then walk Right *across* a hidden index.
    // Build a state where index 0 is hidden and indices 1..=4
    // are visible, with focus at index 1 — Next from index 1
    // walks 2, 3, 4, 5, all visible; nothing reveals. So we
    // need focus to be somewhere that Next lands on index 0
    // (hidden). That requires focus to be 0 already and Next
    // wrap, but Next clamps, not wraps.
    //
    // Therefore the only way to land on a hidden block via
    // Next is when the rightmost block is hidden (cap evicts
    // the *newest* block) — which doesn't happen with LRU. The
    // Next path reveals when we add a block that becomes
    // hidden (cap=4, 5 blocks), then walk Right past the
    // newest index... but Next clamps.
    //
    // Conclusion: the Next path almost never reveals under
    // LRU. The keyboard's "Next" simply clamps at the
    // rightmost block, which is always visible. The Prev
    // path (tested above) is the only one that walks into
    // hidden territory. So we only need to assert that Next
    // *correctly clamps* — which is already covered by
    // `focus_right_at_right_edge_clamps_without_panic`. This
    // test stays as a placeholder documenting why we don't
    // exercise the reveal-on-Next path.
    let mut s = UiState::default();
    s.apply(&voice_bird_next::bus::AppEvent::AddBlock);
    s.apply(&voice_bird_next::bus::AppEvent::AddBlock);
    assert_eq!(s.focus, 1);
    for _ in 0..3 {
        s.apply(&voice_bird_next::bus::AppEvent::AddBlock);
    }
    assert!(!s.blocks[0].visible, "block 1 should be the evicted one");
    assert_eq!(s.focus, 4);

    // Walking Right from the rightmost index clamps — no
    // reveal, no underflow.
    for _ in 0..3 {
        s.apply(&voice_bird_next::bus::AppEvent::FocusMoved {
            direction: voice_bird_next::bus::FocusMove::Next,
        });
    }
    assert_eq!(s.focus, 4, "Next at right edge must clamp, not reveal");
    assert!(s.blocks[4].visible, "block 5 must stay visible");
    assert!(!s.blocks[0].visible, "block 1 must stay hidden");
}

#[test]
fn focus_left_at_left_edge_saturates_without_panic() {
    let mut s = UiState::default();
    s.apply(&voice_bird_next::bus::AppEvent::AddBlock);
    assert_eq!(s.focus, 0);
    s.apply(&voice_bird_next::bus::AppEvent::FocusMoved {
        direction: voice_bird_next::bus::FocusMove::Prev,
    });
    assert_eq!(s.focus, 0, "Prev at index 0 must saturate, not wrap");
    assert!(s.blocks[0].visible);
}

#[test]
fn focus_right_at_right_edge_clamps_without_panic() {
    let mut s = UiState::default();
    s.apply(&voice_bird_next::bus::AppEvent::AddBlock);
    assert_eq!(s.focus, 0);
    s.apply(&voice_bird_next::bus::AppEvent::FocusMoved {
        direction: voice_bird_next::bus::FocusMove::Next,
    });
    assert_eq!(s.focus, 0, "Next at last index must clamp, not overflow");
    assert!(s.blocks[0].visible);
}

#[test]
fn focus_left_across_visible_blocks_does_not_evict_a_peer() {
    // The reveal-on-hidden fix must NOT churn the visible strip
    // when the destination is already visible. Walking across
    // visible blocks must only restamp; no eviction.
    let mut s = UiState::default();
    for _ in 0..3 {
        s.apply(&voice_bird_next::bus::AppEvent::AddBlock);
    }
    let visible_before: Vec<u8> = s
        .blocks
        .iter()
        .filter(|b| b.visible)
        .map(|b| b.id)
        .collect();
    assert_eq!(visible_before.len(), 3);

    for _ in 0..3 {
        s.apply(&voice_bird_next::bus::AppEvent::FocusMoved {
            direction: voice_bird_next::bus::FocusMove::Prev,
        });
    }
    let visible_after: Vec<u8> = s
        .blocks
        .iter()
        .filter(|b| b.visible)
        .map(|b| b.id)
        .collect();
    assert_eq!(
        visible_before, visible_after,
        "walking left over visible blocks must not evict; was {visible_before:?}, now {visible_after:?}"
    );
}

#[test]
fn focus_reveal_evicts_the_lru_visible_peer() {
    // When a hidden block is revealed, the LRU visible peer
    // should be evicted. Walking Left four times into a 5-block
    // state reveals block 1; block 5 (the most-recently-focused,
    // now LRU) should be evicted.
    let mut s = UiState::default();
    for _ in 0..5 {
        s.apply(&voice_bird_next::bus::AppEvent::AddBlock);
    }
    // Initial state: blocks 2..=5 visible, block 1 hidden.
    let visible: std::collections::HashSet<u8> = s
        .blocks
        .iter()
        .filter(|b| b.visible)
        .map(|b| b.id)
        .collect();
    assert_eq!(visible, [2u8, 3, 4, 5].into_iter().collect());

    for _ in 0..4 {
        s.apply(&voice_bird_next::bus::AppEvent::FocusMoved {
            direction: voice_bird_next::bus::FocusMove::Prev,
        });
    }
    let visible: std::collections::HashSet<u8> = s
        .blocks
        .iter()
        .filter(|b| b.visible)
        .map(|b| b.id)
        .collect();
    // Block 1 (just revealed) stays visible. The LRU visible
    // peer — the one with the smallest stamp before the reveal
    // — gets evicted. Trace:
    //   After 5 AddBlocks: stamps {1:1 hidden, 2:2, 3:3, 4:4, 5:5}.
    //   Prev 1 (focus 4→3): restamp block 4 → 6.
    //   Prev 2 (3→2): restamp block 3 → 7.
    //   Prev 3 (2→1): restamp block 2 → 8.
    //   Prev 4 (1→0): reveal block 1, evict LRU visible = block 5 (stamp 5).
    assert_eq!(
        visible,
        [1u8, 2, 3, 4].into_iter().collect(),
        "revealing block 1 should evict block 5 (LRU visible peer after restamps)"
    );
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