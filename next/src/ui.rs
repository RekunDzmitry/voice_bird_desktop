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
/// - `Waiting{m}`     → a `Gauge` from `state.downloads[m]` plus a
///   - `human_bytes` line; `Installing` says
///   - `Unpacking…`.
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
            if inner.height >= 2 {
                let rows = Layout::new(
                    Direction::Vertical,
                    vec![Constraint::Length(1), Constraint::Length(1)],
                )
                .split(inner);
                render_gauge(f, model, state.downloads.get(model), rows[0]);
                let line = human_bytes_line(model, state.downloads.get(model));
                f.render_widget(Paragraph::new(line), rows[1]);
            } else if inner.height == 1 {
                render_gauge(f, model, state.downloads.get(model), inner);
            }
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
            .border_style(Style::default().bold())
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

fn render_gauge(f: &mut Frame, _model: &'static str, state: Option<&DownloadState>, area: Rect) {
    let (ratio, label) = match state {
        Some(s) => match s.phase {
            DownloadPhase::Installing => (1.0_f64, "Unpacking…".to_string()),
            DownloadPhase::Fetching => (
                s.ratio().unwrap_or(0.0),
                match s.total {
                    Some(t) => format!("{} / {}", human_bytes(s.bytes), human_bytes(t)),
                    None => format!("{} …", human_bytes(s.bytes)),
                },
            ),
        },
        None => (0.0, " ".to_string()),
    };
    let gauge = Gauge::default()
        .gauge_style(Style::default().bold())
        .ratio(ratio)
        .label(label);
    f.render_widget(gauge, area);
}

fn human_bytes_line(model: &'static str, state: Option<&DownloadState>) -> Line<'static> {
    match state {
        None => Line::from("…"),
        Some(s) => match s.total {
            Some(t) => Line::from(format!(
                "{}  {} / {}",
                model,
                human_bytes(s.bytes),
                human_bytes(t)
            )),
            None => Line::from(format!("{}  {} …", model, human_bytes(s.bytes))),
        },
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
    let marker = if index == selected { "\u{25b6}" } else { "  " };
    Line::from(format!("{marker} {}", entry.id))
}

pub fn picker_row_sized(
    index: usize,
    selected: usize,
    entry: &ModelEntry,
    width: usize,
) -> Line<'static> {
    let marker = if index == selected { "\u{25b6}" } else { "  " };
    let id_only = format!("{marker} {}", entry.id);
    if id_only.chars().count() > width {
        return Line::from(marker.to_string());
    }
    let size_str = format!("{:>5} MB", entry.size_mb);
    let lang_str = entry.language.to_string();
    let full = format!("{id_only} {size_str}  {lang_str}");
    if full.chars().count() <= width {
        Line::from(full)
    } else {
        Line::from(id_only)
    }
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
    fn picker_row_in_a_narrow_column_drops_size_and_language() {
        let line = picker_row_sized(0, 0, &crate::picker::CATALOG[0], 20);
        assert_eq!(line.to_string(), "\u{25b6} distil-small.en");
    }

    #[test]
    fn picker_row_in_a_very_narrow_column_drops_the_id_too() {
        let line = picker_row_sized(0, 0, &crate::picker::CATALOG[0], 4);
        assert_eq!(line.to_string(), "\u{25b6}");
    }

    #[test]
    fn picker_row_wide_column_adds_size_and_language() {
        let line = picker_row_sized(2, 2, &crate::picker::CATALOG[2], 60);
        let s = line.to_string();
        assert!(s.contains("1600"), "{s}");
        assert!(s.contains("MB"), "{s}");
        assert!(s.contains("multi"), "{s}");
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
    fn waiting_block_installing_phase_says_unpacking() {
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
            },
        );
        let out = render_to_string(&state, 100, 10);
        assert!(out.contains("Unpacking"), "got:\n{out}");
    }

    #[test]
    fn waiting_block_without_content_length_still_renders() {
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
                bytes: 1024,
                total: None,
            },
        );
        let _ = render_to_string(&state, 20, 5);
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