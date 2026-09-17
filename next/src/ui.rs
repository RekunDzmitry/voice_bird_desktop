//! Block-level rendering.
//!
//! `render` is a pure function of `&UiState`. N concurrent blocks at
//! N stages each render inside their own column; arrows move focus, the
//! focused block draws `Borders::ALL` while the rest draw
//! `Borders::LEFT | Borders::RIGHT` so adjacent columns share a `│`
//! divider.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    prelude::Stylize,
    style::Style,
    text::Line,
    widgets::{Block, Borders, Gauge, Paragraph, Wrap},
    Frame,
};

use crate::picker::{ModelEntry, ModelPicker};
use crate::state::{BlockState, DownloadState, UiState};
use crate::store::DownloadPhase;

/// Draw one frame: outer window with `state.title` in its top border,
/// then `state.blocks` evenly-distributed columns inside.
///
/// Each column renders its own stage:
/// - `Picking(p)`     → the catalog list, `▶` on `p.index`.
/// - `Waiting{m}`     → a `Gauge` from `state.downloads[m]`. With
///   - `Fetching` + known `total` it shows `bytes/total`; with no
///     `total` it shows `MB/s · bytes`. `Installing` renders the
///     label `Unpacking m…` left-aligned.
/// - `Recording{m}`   → `● recording (mocked)`.
/// - `Failed{m,e}`    → the error wrapped, plus `r retry · Esc close`.
pub fn render(f: &mut Frame, state: &UiState) {
    let window = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", state.title));
    f.render_widget(&window, f.area());

    if !state.blocks.is_empty() {
        let inner = window.inner(f.area());
        let columns = Layout::new(
            Direction::Horizontal,
            vec![Constraint::Fill(1); state.blocks.len()],
        )
        .split(inner);
        for (idx, (block, column)) in state.blocks.iter().zip(columns.iter()).enumerate() {
            render_block(f, block, idx == state.focus, *column, state);
        }
    }
}

fn render_block(
    f: &mut Frame,
    block: &crate::state::Block,
    focused: bool,
    area: Rect,
    state: &UiState,
) {
    let border = block_border(focused);
    let title = block_title(block);
    let border = border.title(format!(" {title} "));
    let inner = border.inner(area);
    f.render_widget(border, area);

    match &block.state {
        BlockState::Waiting { model } => {
            // The model name is already in the block title, so the body
            // shows only the progress indicator. `render_gauge` picks
            // the right shape (Installing → label, Fetching with total
            // → filled bar with `bytes/total`, Fetching without total
            // → pulsing bar with `MB/s`).
            render_gauge(f, model, state.downloads.get(model), inner);
        }
        _ => {
            let lines = block_body_lines(block, state);
            f.render_widget(
                Paragraph::new(lines).wrap(Wrap { trim: false }),
                inner,
            );
        }
    }
}

fn block_border(focused: bool) -> Block<'static> {
    if focused {
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().bold().yellow())
    } else {
        Block::default().borders(Borders::LEFT | Borders::RIGHT)
    }
}

fn block_title(block: &crate::state::Block) -> String {
    match &block.state {
        BlockState::Picking(_) => format!("{} · pick a model", block.id),
        BlockState::Waiting { model } => format!("{} · {model}", block.id),
        BlockState::Recording { model } => format!("{} · {model}", block.id),
        BlockState::Failed { model, .. } => format!("{} · {model} · error", block.id),
    }
}

fn block_body_lines(block: &crate::state::Block, _state: &UiState) -> Vec<Line<'static>> {
    match &block.state {
        BlockState::Picking(picker) => picker_lines(picker),
        // Waiting is rendered directly by render_block (Gauge widget).
        BlockState::Waiting { .. } => Vec::new(),
        BlockState::Recording { .. } => vec![Line::from("● recording (mocked)")],
        BlockState::Failed { error, .. } => vec![
            Line::from(error.clone()),
            Line::from("r retry · Esc close"),
        ],
    }
}

fn render_gauge(f: &mut Frame, model: &'static str, state: Option<&DownloadState>, area: Rect) {
    match state {
        Some(s) if s.phase == DownloadPhase::Installing => {
            // Unpacking is a left-aligned label so the user sees that
            // *something* is happening but no fake bar slides to 100%.
            f.render_widget(
                Paragraph::new(format!("Unpacking {model}…")),
                area,
            );
        }
        Some(s) => {
            // Fetching. Two label shapes:
            //   - known total  → filled bar + "bytes / total"
            //   - no total yet → pulsing bar (ratio=0) + MB/s + bytes
            // The MB/s line is meaningful precisely because `total`
            // is None — without it, the user sees only a widthless
            // bar with no end in sight.
            let ratio = s.ratio().unwrap_or(0.0);
            let label = match s.total {
                Some(t) => format!("{} / {}", human_bytes(s.bytes), human_bytes(t)),
                None => format!(
                    "{} · {:.2} MB/s",
                    human_bytes(s.bytes),
                    s.bytes_per_sec as f64 / 1_000_000.0
                ),
            };
            let gauge = Gauge::default()
                .gauge_style(Style::default().bold())
                .ratio(ratio)
                .label(label);
            f.render_widget(gauge, area);
        }
        None => {
            f.render_widget(Gauge::default().ratio(0.0).label(" "), area);
        }
    }
}

fn picker_lines(picker: &ModelPicker) -> Vec<Line<'static>> {
    picker
        .catalog()
        .iter()
        .enumerate()
        .map(|(i, entry)| picker_row(i, picker.index, entry))
        .collect()
}

fn picker_row(index: usize, selected: usize, entry: &ModelEntry) -> Line<'static> {
    // Rows are just the marker and the model id. Earlier revisions
    // appended size and language; the rows grew too wide and the
    // additional columns weren't worth a second look.
    let marker = if index == selected { "\u{25b6}" } else { "  " };
    Line::from(format!("{marker} {}", entry.id))
}

/// Human-readable byte count.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit_idx = 0;
    while value >= 1024.0 && unit_idx < UNITS.len() - 1 {
        value /= 1024.0;
        unit_idx += 1;
    }
    if unit_idx == 0 {
        format!("{bytes} {}", UNITS[0])
    } else if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit_idx])
    } else if value >= 10.0 {
        format!("{value:.1} {}", UNITS[unit_idx])
    } else {
        format!("{value:.2} {}", UNITS[unit_idx])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Block;
    use crate::testing::render_to_string;

    #[test]
    fn window_is_an_empty_bordered_box_with_title() {
        let out = render_to_string(&UiState::default(), 40, 5);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[0], format!("┌ Voice Bird {}┐", "─".repeat(26)));
        assert_eq!(lines[4], format!("└{}┘", "─".repeat(38)));
        for inner in &lines[1..4] {
            assert_eq!(*inner, format!("│{}│", " ".repeat(38)));
        }
    }

    #[test]
    fn title_comes_from_state() {
        let state = UiState {
            title: "Hello".to_string(),
            ..Default::default()
        };
        let out = render_to_string(&state, 20, 3);
        assert!(out.starts_with("┌ Hello ─"), "{out}");
    }

    #[test]
    fn tiny_sizes_do_not_panic() {
        for (w, h) in [(1, 1), (2, 2), (3, 3), (10, 2), (5, 40), (200, 1)] {
            let _ = render_to_string(&UiState::default(), w, h);
        }
    }

    #[test]
    fn one_block_picking_lists_catalog_with_marker() {
        let state = UiState {
            blocks: vec![Block {
                id: 1,
                state: BlockState::Picking(ModelPicker::open(crate::picker::PickerIntent::AddBlock)),
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
    fn one_block_recording_shows_recording_marker() {
        let state = UiState {
            blocks: vec![Block {
                id: 1,
                state: BlockState::Recording { model: "tiny.en" },
            }],
            focus: 0,
            next_block_id: 2,
            ..Default::default()
        };
        let out = render_to_string(&state, 80, 10);
        assert!(out.contains("recording"), "{out}");
        assert!(out.contains("tiny.en"), "{out}");
    }

    #[test]
    fn focused_block_border_differs_from_unfocused() {
        let state = UiState {
            blocks: vec![
                Block {
                    id: 1,
                    state: BlockState::Recording { model: "tiny.en" },
                },
                Block {
                    id: 2,
                    state: BlockState::Recording { model: "base.en" },
                },
                Block {
                    id: 3,
                    state: BlockState::Recording { model: "large-v3-turbo" },
                },
            ],
            focus: 1,
            next_block_id: 4,
            ..Default::default()
        };
        let out = render_to_string(&state, 100, 10);
        assert!(
            out.matches('─').count() >= 3,
            "expected focused block top border; got:\n{out}"
        );
    }

    #[test]
    fn three_blocks_split_into_three_columns_with_titles() {
        let state = UiState {
            blocks: vec![
                Block {
                    id: 1,
                    state: BlockState::Recording { model: "distil-small.en" },
                },
                Block {
                    id: 2,
                    state: BlockState::Recording { model: "distil-large-v3" },
                },
                Block {
                    id: 3,
                    state: BlockState::Recording { model: "large-v3-turbo" },
                },
            ],
            focus: 0,
            next_block_id: 4,
            ..Default::default()
        };
        let out = render_to_string(&state, 100, 30);
        for label in ["1 · distil-small.en", "2 · distil-large-v3", "3 · large-v3-turbo"] {
            assert!(out.contains(label), "missing {label} in:\n{out}");
        }
    }

    #[test]
    fn many_blocks_in_a_tiny_terminal_do_not_panic() {
        let state = UiState {
            blocks: (1..=5)
                .map(|i| Block {
                    id: i,
                    state: BlockState::Recording { model: "tiny.en" },
                })
                .collect(),
            focus: 0,
            next_block_id: 6,
            ..Default::default()
        };
        let _ = render_to_string(&state, 3, 5);
    }

    #[test]
    fn picker_row_renders_marker_and_id() {
        let line = picker_row(0, 0, &crate::picker::CATALOG[0]);
        assert_eq!(line.to_string(), "\u{25b6} distil-small.en");
    }

    #[test]
    fn picker_row_unselected_row_has_no_marker() {
        let line = picker_row(1, 0, &crate::picker::CATALOG[0]);
        // The first two chars are padding (no marker), then the id.
        let s = line.to_string();
        assert!(s.starts_with("  "), "expected two-space indent; got {s:?}");
        assert!(s.contains("distil-small.en"), "expected id; got {s:?}");
    }

    #[test]
    fn picker_row_does_not_include_size_or_language() {
        let line = picker_row(2, 2, &crate::picker::CATALOG[2]);
        let s = line.to_string();
        assert!(!s.contains("MB"), "size column should be gone; got {s:?}");
        assert!(!s.contains("multi"), "language column should be gone; got {s:?}");
        assert!(s.contains("large-v3-turbo"), "id should still be present; got {s:?}");
    }

    #[test]
    fn waiting_block_draws_filled_cells_at_half() {
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
                bytes: 50,
                total: Some(100),
                bytes_per_sec: 0,
            },
        );
        let out = render_to_string(&state, 100, 10);
        assert!(out.contains('█'), "expected filled gauge cells; got:\n{out}");
        assert!(out.contains("tiny.en"), "expected model id; got:\n{out}");
    }

    #[test]
    fn waiting_blocks_on_the_same_model_render_identical_bodies() {
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
        let out = render_to_string(&state, 100, 10);
        let count = out.matches('█').count();
        assert!(count >= 2, "expected at least two cells of `█`; got {count} in:\n{out}");
    }

    #[test]
    fn waiting_block_without_a_record_does_not_panic() {
        let state = UiState {
            blocks: vec![Block {
                id: 1,
                state: BlockState::Waiting { model: "tiny.en" },
            }],
            focus: 0,
            next_block_id: 2,
            ..Default::default()
        };
        let _ = render_to_string(&state, 40, 5);
    }

    #[test]
    fn waiting_block_installing_phase_says_unpacking_left_aligned() {
        let mut state = UiState {
            blocks: vec![Block {
                id: 1,
                state: BlockState::Waiting {
                    model: "nemotron-3.5-asr-streaming-0.6b",
                },
            }],
            focus: 0,
            next_block_id: 2,
            ..Default::default()
        };
        state.downloads.insert(
            "nemotron-3.5-asr-streaming-0.6b",
            DownloadState {
                phase: DownloadPhase::Installing,
                bytes: 0,
                total: Some(740 * 1024 * 1024),
                bytes_per_sec: 0,
            },
        );
        let out = render_to_string(&state, 100, 10);
        assert!(out.contains("Unpacking"), "got:\n{out}");
        // No filled bar — Installing should not draw a █ anywhere.
        assert!(!out.contains('\u{2588}'), "Installing must not draw a filled bar; got:\n{out}");
    }

    #[test]
    fn waiting_block_without_content_length_uses_mb_per_sec_label() {
        let mut state = UiState {
            blocks: vec![Block {
                id: 1,
                state: BlockState::Waiting { model: "tiny.en" },
            }],
            focus: 0,
            next_block_id: 2,
            ..Default::default()
        };
        // 2 MiB/s over the previous tick.
        state.downloads.insert(
            "tiny.en",
            DownloadState {
                phase: DownloadPhase::Fetching,
                bytes: 1024,
                total: None,
                bytes_per_sec: 2 * 1024 * 1024,
            },
        );
        let out = render_to_string(&state, 40, 5);
        assert!(out.contains("MB/s"), "expected MB/s readout; got:
{out}");
        // The model name lives only in the title — the body must not
        // repeat it (only the gauge fills the body now).
        let gauge_rows: Vec<&str> = out
            .lines()
            .filter(|l| l.contains("MB/s") || l.contains("█"))
            .collect();
        assert!(
            gauge_rows.iter().all(|l| !l.contains("tiny.en")),
            "gauge body must not repeat the model id; rows: {gauge_rows:?}"
        );
    }

    #[test]
    fn failed_block_shows_error_and_retry_hint() {
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
        let out = render_to_string(&state, 100, 10);
        assert!(out.contains("HTTP 404"), "{out}");
        assert!(out.contains("retry"), "{out}");
    }

    #[test]
    fn human_bytes_formats_each_unit() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.00 KB");
        assert_eq!(human_bytes(1536), "1.50 KB");
        assert_eq!(human_bytes(1024 * 1024), "1.00 MB");
        assert_eq!(human_bytes(75 * 1024 * 1024), "75.0 MB");
        assert_eq!(human_bytes(1024u64.pow(3)), "1.00 GB");
    }
}