use pretty_assertions::assert_eq;
use proptest::prelude::*;
use voice_bird_next::{
    consumer::ui_view::{Block, BlockState, DownloadPhase, DownloadState, UiView, MAX_SESSIONS},
    language::LANGUAGES,
    picker::{ListPicker, CATALOG},
    testing::render_to_string,
};

const IDLE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/snapshots/idle_100x30.txt"
);
const THREE_BLOCKS: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/snapshots/three_blocks_100x30.txt"
);
const PICKING: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/snapshots/picking_100x30.txt"
);
const DOWNLOADING_TWO_BLOCKS: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/snapshots/downloading_two_blocks_100x30.txt"
);
const FAILED: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/snapshots/failed_100x30.txt"
);
const FOUR_BLOCKS: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/snapshots/four_blocks_100x30.txt"
);
const SESSION_LIMIT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/snapshots/session_limit_100x30.txt"
);

#[test]
fn idle_100x30_matches_golden() {
    let out = render_to_string(&UiView::default(), 100, 30);
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(IDLE, &out).expect("write golden");
    }
    let expected = std::fs::read_to_string(IDLE).expect("read golden");
    assert_eq!(out, expected);
}

#[test]
fn three_blocks_100x30_matches_golden() {
    let ui_view = UiView {
        blocks: (1..=3)
            .map(|id| Block {
                id,
                state: BlockState::recording(&LANGUAGES[0]),
                ..Default::default()
            })
            .collect(),
        focus: 0,
        next_block_id: 4,
        ..Default::default()
    };
    let out = render_to_string(&ui_view, 100, 30);
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(THREE_BLOCKS, &out).expect("write golden");
    }
    let expected = std::fs::read_to_string(THREE_BLOCKS).expect("read golden");
    assert_eq!(out, expected);
}

#[test]
fn picking_100x30_matches_golden() {
    let ui_view = UiView {
        blocks: vec![Block {
            id: 1,
            state: BlockState::PickingLanguage(ListPicker::default()),

            ..Default::default()
        }],
        focus: 0,
        next_block_id: 2,
        ..Default::default()
    };
    let out = render_to_string(&ui_view, 100, 30);
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(PICKING, &out).expect("write golden");
    }
    let expected = std::fs::read_to_string(PICKING).expect("read golden");
    assert_eq!(out, expected);
}

#[test]
fn downloading_two_blocks_100x30_matches_golden() {
    // Keep this two-model layout independent of optional Core ML sidecars.
    static LANGUAGE: voice_bird_next::language::LanguageProfile =
        voice_bird_next::language::LanguageProfile {
            code: "en",
            live: &voice_bird_next::picker::DISTIL_SMALL_EN,
            refine: &voice_bird_next::picker::LARGE_V3_TURBO,
            required: &[
                &voice_bird_next::picker::DISTIL_SMALL_EN,
                &voice_bird_next::picker::LARGE_V3_TURBO,
            ],
        };
    let mut ui_view = UiView {
        blocks: (1..=2)
            .map(|id| Block {
                id,
                state: BlockState::Waiting {
                    language: &LANGUAGE,
                    pending: vec![LANGUAGE.live.id, LANGUAGE.refine.id],
                },
                ..Default::default()
            })
            .collect(),
        focus: 0,
        next_block_id: 3,
        ..Default::default()
    };
    for model in LANGUAGE.models() {
        ui_view.downloads.insert(
            model.id,
            DownloadState {
                attempt: 1,
                phase: DownloadPhase::Fetching,
                bytes: 50,
                total: Some(100),
                bytes_per_sec: 0,
            },
        );
    }
    let out = render_to_string(&ui_view, 100, 30);
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(DOWNLOADING_TWO_BLOCKS, &out).expect("write golden");
    }
    let expected = std::fs::read_to_string(DOWNLOADING_TWO_BLOCKS).expect("read golden");
    assert_eq!(out, expected);
}

#[test]
fn failed_100x30_matches_golden() {
    let ui_view = UiView {
        blocks: vec![Block {
            id: 1,
            state: BlockState::Failed {
                language: &LANGUAGES[0],
                error: "HTTP 404".to_string(),
                pending: Vec::new(),
            },

            ..Default::default()
        }],
        focus: 0,
        next_block_id: 2,
        ..Default::default()
    };
    let out = render_to_string(&ui_view, 100, 30);
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(FAILED, &out).expect("write golden");
    }
    let expected = std::fs::read_to_string(FAILED).expect("read golden");
    assert_eq!(out, expected);
}

#[test]
fn picker_renders_languages_without_model_ids() {
    let ui_view = UiView {
        blocks: vec![Block {
            id: 1,
            state: BlockState::PickingLanguage(ListPicker::default()),
            ..Default::default()
        }],
        focus: 0,
        next_block_id: 2,
        ..Default::default()
    };
    let out = render_to_string(&ui_view, 100, 30);
    assert!(out.contains(&format!("\u{25b6} {}", LANGUAGES[0].code)));
    assert!(out.contains("pick a language"));
    for model in CATALOG {
        assert!(!out.contains(model.id));
    }
}

#[test]
fn four_blocks_100x30_matches_golden() {
    let mut s = UiView::default();
    for _ in 0..MAX_SESSIONS {
        s.apply(&voice_bird_next::bus::AppEvent::AddBlock);
    }
    let out = render_to_string(&s, 100, 30);
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(FOUR_BLOCKS, &out).expect("write golden");
    }
    let expected = std::fs::read_to_string(FOUR_BLOCKS).expect("read golden");
    assert_eq!(out, expected);
}

#[test]
fn session_limit_100x30_matches_golden() {
    let mut s = UiView::default();
    for _ in 0..=MAX_SESSIONS {
        s.apply(&voice_bird_next::bus::AppEvent::AddBlock);
    }
    assert_eq!(s.blocks.len(), MAX_SESSIONS);
    let out = render_to_string(&s, 100, 30);
    assert!(out
        .lines()
        .next()
        .unwrap()
        .contains("! session limit (4) reached; close a session to make room"));
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(SESSION_LIMIT, &out).expect("write golden");
    }
    let expected = std::fs::read_to_string(SESSION_LIMIT).expect("read golden");
    assert_eq!(out, expected);
}

#[test]
fn title_bar_is_clean_when_no_warning() {
    // Steady state: title is just "Voice Bird", no warning suffix.
    let s = UiView::default();
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

#[test]
fn focus_moves_between_session_columns() {
    let mut s = UiView::default();
    for _ in 0..MAX_SESSIONS {
        s.apply(&voice_bird_next::bus::AppEvent::AddBlock);
    }
    for expected in (0..MAX_SESSIONS - 1).rev() {
        s.apply(&voice_bird_next::bus::AppEvent::FocusMoved {
            direction: voice_bird_next::bus::FocusMove::Prev,
        });
        assert_eq!(s.focus, expected);
        let out = render_to_string(&s, 100, 30);
        assert!(out.contains(&format!("┌ {} · pick a language", expected + 1)));
    }
    for expected in 1..MAX_SESSIONS {
        s.apply(&voice_bird_next::bus::AppEvent::FocusMoved {
            direction: voice_bird_next::bus::FocusMove::Next,
        });
        assert_eq!(s.focus, expected);
        let out = render_to_string(&s, 100, 30);
        assert!(out.contains(&format!("┌ {} · pick a language", expected + 1)));
    }
}

#[test]
fn focus_left_at_left_edge_saturates_without_panic() {
    let mut s = UiView::default();
    s.apply(&voice_bird_next::bus::AppEvent::AddBlock);
    assert_eq!(s.focus, 0);
    s.apply(&voice_bird_next::bus::AppEvent::FocusMoved {
        direction: voice_bird_next::bus::FocusMove::Prev,
    });
    assert_eq!(s.focus, 0, "Prev at index 0 must saturate, not wrap");
}

#[test]
fn focus_right_at_right_edge_clamps_without_panic() {
    let mut s = UiView::default();
    s.apply(&voice_bird_next::bus::AppEvent::AddBlock);
    assert_eq!(s.focus, 0);
    s.apply(&voice_bird_next::bus::AppEvent::FocusMoved {
        direction: voice_bird_next::bus::FocusMove::Next,
    });
    assert_eq!(s.focus, 0, "Next at last index must clamp, not overflow");
}

proptest! {
    #[test]
    fn render_never_panics_for_any_size(w in 1u16..200, h in 1u16..80) {
        let _ = render_to_string(&UiView::default(), w, h);
    }

    #[test]
    fn render_never_panics_with_random_block_count(
        w in 1u16..200,
        h in 1u16..80,
        blocks in 0usize..=MAX_SESSIONS,
    ) {
        let ui_view = UiView {
            blocks: (1..=blocks as u8)
                .map(|i| Block {
                    id: i,
                    state: BlockState::recording(&LANGUAGES[0]),

                    ..Default::default()
                })
                .collect(),
            focus: 0,
            next_block_id: blocks as u8 + 1,
            ..Default::default()
        };
        let _ = render_to_string(&ui_view, w, h);
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
        let mut ui_view = UiView {
            blocks: vec![Block {
                id: 1,
                state: BlockState::Waiting {
                    language: &LANGUAGES[0],
                    pending: vec![LANGUAGES[0].live.id],
                },

                ..Default::default()
            }],
            focus: 0,
            next_block_id: 2,
            ..Default::default()
        };
        ui_view.downloads.insert(
            LANGUAGES[0].live.id,
            DownloadState {
                attempt: 1,
                phase: DownloadPhase::Fetching,
                bytes,
                total,
                bytes_per_sec: 0,
            },
        );
        let _ = render_to_string(&ui_view, w, h);
    }
}

fn source_block_view(state: BlockState, device: Option<usize>, app: Option<usize>) -> UiView {
    let snapshot = voice_bird_next::testing::sample_source_snapshot();
    let source = voice_bird_next::audio_sources::SourceSelection {
        device: device.map(|index| snapshot.devices[index].clone()),
        app: app.map(|index| snapshot.apps[index].clone()),
        snapshot,
        rev: 0,
    };
    UiView {
        blocks: vec![Block {
            id: 1,
            state,
            source: Some(source),
        }],
        ..Default::default()
    }
}

fn source_golden(name: &str, ui_view: UiView) {
    let out = render_to_string(&ui_view, 100, 30);
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/snapshots")
        .join(format!("{name}_100x30.txt"));
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(&path, &out).expect("write golden");
    }
    assert_eq!(out, std::fs::read_to_string(path).expect("read golden"));
}

#[test]
fn picking_device_100x30_matches_golden() {
    source_golden(
        "picking_device",
        source_block_view(BlockState::PickingDevice(Default::default()), None, None),
    );
}

#[test]
fn picking_app_100x30_matches_golden() {
    source_golden(
        "picking_app",
        source_block_view(BlockState::PickingApp(Default::default()), Some(1), None),
    );
}

#[test]
fn device_picker_hides_retained_selections_and_restores_device_cursor() {
    let ui_view = source_block_view(
        BlockState::PickingDevice(ListPicker { index: 1 }),
        Some(1),
        Some(1),
    );
    let out = render_to_string(&ui_view, 100, 30);
    let title = out
        .lines()
        .find(|line| line.contains("pick a device"))
        .unwrap();
    assert!(title.contains("1 · pick a device"), "{out}");
    assert!(!title.contains("Speakers"), "{out}");
    assert!(!title.contains("Spotify"), "{out}");
    assert!(out.contains("▶ out Speakers"), "{out}");
    assert!(out.contains("in  Mic"), "{out}");
    assert!(!out.contains("Safari"), "{out}");
    assert!(!out.contains("Spotify"), "{out}");
}

#[test]
fn app_picker_hides_retained_app_and_restores_app_cursor() {
    let ui_view = source_block_view(
        BlockState::PickingApp(ListPicker { index: 1 }),
        Some(1),
        Some(1),
    );
    let out = render_to_string(&ui_view, 100, 30);
    let title = out
        .lines()
        .find(|line| line.contains("pick an app"))
        .unwrap();
    assert!(title.contains("1 · Speakers · pick an app"), "{out}");
    assert!(!title.contains("Spotify"), "{out}");
    assert!(out.contains("▶ Spotify"), "{out}");
    assert!(out.contains("Safari"), "{out}");
    assert!(!out.contains("out Speakers"), "{out}");
    assert!(!out.contains("in  Mic"), "{out}");
}

#[test]
fn recording_with_source_100x30_matches_golden() {
    source_golden(
        "recording_with_source",
        source_block_view(BlockState::recording(&LANGUAGES[0]), Some(1), Some(1)),
    );
}

#[test]
fn recording_with_transcript_100x30_matches_golden() {
    let mut state = BlockState::recording(&LANGUAGES[0]);
    if let BlockState::Recording {
        lines,
        tentative,
        session_dir,
        ..
    } = &mut state
    {
        *lines = vec![
            "Welcome to Voice Bird.".into(),
            "Refined text replaces the earlier live words.".into(),
            "New live words remain visible as recording continues.".into(),
        ];
        *tentative = "The next sentence is still arriving…".into();
        *session_dir = Some("/local/data/sessions/2026-10-08_12-30-00-b1-mic".into());
    }
    source_golden(
        "recording_with_transcript",
        source_block_view(state, Some(0), None),
    );
}

#[test]
fn small_recording_view_keeps_wrapped_tail_and_tentative_visible() {
    let mut state = BlockState::recording(&LANGUAGES[0]);
    if let BlockState::Recording {
        lines, tentative, ..
    } = &mut state
    {
        *lines = vec!["older words ".repeat(30), "latest committed words".into()];
        *tentative = "next words".into();
    }
    let out = render_to_string(&source_block_view(state, Some(0), None), 30, 12);
    assert!(out.contains("latest committed words"), "{out}");
    assert!(out.contains("next words"), "{out}");
}

#[test]
fn input_source_title_omits_app() {
    let ui_view = source_block_view(BlockState::recording(&LANGUAGES[0]), Some(0), None);
    let out = render_to_string(&ui_view, 100, 30);
    assert!(out.contains("1 · Mic · en"), "{out}");
    assert!(!out.contains("Spotify"), "{out}");
}

#[test]
fn empty_app_picker_explains_required_app_and_back_navigation() {
    let mut ui_view = source_block_view(BlockState::PickingApp(Default::default()), Some(1), None);
    ui_view.blocks[0]
        .source
        .as_mut()
        .unwrap()
        .snapshot
        .apps
        .clear();
    let out = render_to_string(&ui_view, 100, 30);
    assert!(out.contains("no running apps"), "{out}");
    assert!(out.contains("⌫ back · Esc close"), "{out}");
}

#[test]
fn long_source_lists_keep_selection_and_back_visible_on_resize() {
    let mut ui_view = source_block_view(
        BlockState::PickingApp(voice_bird_next::picker::ListPicker { index: 30 }),
        Some(1),
        None,
    );
    let source = ui_view.blocks[0].source.as_mut().unwrap();
    source.snapshot.apps = (0..40)
        .map(|index| voice_bird_next::audio_sources::AppTarget {
            id: format!("app.{index}"),
            name: format!("App {index}"),
            pid: index,
        })
        .collect();
    for height in [8, 12, 30] {
        let out = render_to_string(&ui_view, 100, height);
        assert!(out.contains("▶ App 30"), "{out}");
        assert!(out.contains("⌫ back · Esc close"), "{out}");
        assert!(!out.contains("App 0 "), "{out}");
    }
}
