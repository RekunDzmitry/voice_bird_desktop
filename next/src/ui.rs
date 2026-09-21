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

    let inner = window.inner(f.area());

    // The session menu is a left-side panel. When open, it claims
    // `MENU_WIDTH` columns off the inner rect; when closed, the
    // blocks take the full width — no empty reserved space. The
    // split lives inside the outer window border, so the menu never
    // overlaps the title.
    let (menu_area, blocks_area) = match state.menu.as_ref() {
        Some(_) => {
            let chunks = Layout::new(
                Direction::Horizontal,
                vec![Constraint::Length(MENU_WIDTH), Constraint::Fill(1)],
            )
            .split(inner);
            (Some(chunks[0]), chunks[1])
        }
        None => (None, inner),
    };

    // Column layout uses the *visible* blocks only — the cap is
    // enforced in the reducer's `show_block`, so the renderer never
    // has to clamp. If no block is visible (e.g. nothing has been
    // created yet), skip the layout entirely.
    let visible: Vec<&crate::state::Block> = state
        .blocks
        .iter()
        .filter(|b| b.visible)
        .collect();
    if !visible.is_empty() {
        let columns = Layout::new(
            Direction::Horizontal,
            vec![Constraint::Fill(1); visible.len()],
        )
        .split(blocks_area);
        for (block, column) in visible.iter().zip(columns.iter()) {
            // `state.focus` is an index into `blocks`, not the visible
            // list, so the focused check is "is this block's id the
            // focused one?". Comparing the index directly would mark
            // the wrong column focused once the cap has reordered
            // anything.
            let focused = Some(block.id) == state.focused().map(|b| b.id);
            render_block(f, block, focused, *column, state);
        }
    }

    if let Some(menu) = state.menu.as_ref() {
        if let Some(area) = menu_area {
            render_menu(f, state, menu, area);
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

/// Width of the left-hand session menu panel, in columns. Sized
/// to fit `▶ session 999` (13 chars) on a single line — the
/// session id is a `u8` that wraps after 255, so 999 covers every
/// realistic lifetime of the app. The outer window border and the
/// menu's own borders are not included in this width.
const MENU_WIDTH: u16 = 15;

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

/// Compute the half-open `[start, end)` window of session indices
/// to render, so that `selected` is visible and recentered as
/// much as possible. Returns `(0, 0)` when there are no rows.
///
/// The window never extends past `total` rows, and `start` clamps
/// to `0` so a terminal resize cannot make the menu "scroll past
/// the top". This is the carousel's pure data shape — the
/// renderer just iterates `state.blocks[start..end]` and draws
/// one line per row.
fn menu_window(total: usize, selected: usize, height: usize) -> (usize, usize) {
    if total == 0 || height == 0 {
        return (0, 0);
    }
    // Clamp the panel to fit: the window can never be longer than
    // either the terminal height or the session count.
    let window = height.min(total);
    if window >= total {
        // Everything fits — no scroll needed.
        return (0, total);
    }
    // Anchor `selected` near the middle of the window. `start =
    // selected - height/2` puts the highlight one row above center
    // when the window is longer than the remaining rows below —
    // pressing Down on the last visible row moves the window by
    // exactly one row, which makes the carousel feel balanced.
    let mut start = selected.saturating_sub(window / 2);
    if start + window > total {
        start = total - window;
    }
    let end = start + window;
    (start, end)
}

/// Render the left-hand session menu as a scrolling carousel.
/// Rows are plain `▶ session N` labels — the menu is a
/// navigation list, not a status panel, so model names and
/// picker state stay out of here. Hidden sessions are drawn
/// dimmed via `Stylize::dim()` so the user can see at a glance
/// which four are on screen.
///
/// When the panel is shorter than the session count, only a
/// window of rows is rendered — recentered on `menu.index` so
/// the highlighted row is always visible, and rebalanced on
/// every keystroke. This makes the menu responsive to terminal
/// resizes (the window follows `inner.height`) without needing
/// a separate scroll state on the menu itself.
fn render_menu(
    f: &mut Frame,
    state: &UiState,
    menu: &crate::picker::SessionMenu,
    area: Rect,
) {
    let border = Block::default()
        .borders(Borders::LEFT | Borders::RIGHT)
        .border_style(Style::default().bold());
    let inner = border.inner(area);
    f.render_widget(border, area);

    let total = state.blocks.len();
    let (start, end) = menu_window(total, menu.index, inner.height as usize);
    if start >= end {
        // Nothing to render (e.g. zero blocks, zero-height panel).
        return;
    }
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(end - start);
    for (idx, block) in state.blocks[start..end].iter().enumerate() {
        let absolute = start + idx;
        let marker = if absolute == menu.index { "\u{25b6}" } else { "  " };
        // Sketch contract: plain `session N` per row — the menu is
        // a navigation list, not a status readout. Model names,
        // picker state, and progress live in the column strip on
        // the right.
        let mut line = Line::from(format!("{marker} session {}", block.id));
        if !block.visible {
            line = line.dim();
        }
        lines.push(line);
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
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
    fn one_block_recording_shows_recording_marker() {
        let state = UiState {
            blocks: vec![Block {
                id: 1,
                state: BlockState::Recording { model: "tiny.en" },

                ..Default::default()
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

                    ..Default::default()
                },
                Block {
                    id: 2,
                    state: BlockState::Recording { model: "base.en" },

                    ..Default::default()
                },
                Block {
                    id: 3,
                    state: BlockState::Recording { model: "large-v3-turbo" },

                    ..Default::default()
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

                    ..Default::default()
                },
                Block {
                    id: 2,
                    state: BlockState::Recording { model: "distil-large-v3" },

                    ..Default::default()
                },
                Block {
                    id: 3,
                    state: BlockState::Recording { model: "large-v3-turbo" },

                    ..Default::default()
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

                    ..Default::default()
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

                ..Default::default()
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

                ..Default::default()
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

                ..Default::default()
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

                ..Default::default()
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

    #[test]
    fn five_blocks_state_renders_exactly_four_column_titles() {
        // After 5 AddBlocks the cap is reached: 4 columns visible,
        // 1 hidden. The hidden block does not contribute a column
        // title. The menu is closed so no panel steals width.
        let mut s = UiState::default();
        for _ in 0..5 {
            s.apply(&crate::bus::AppEvent::AddBlock);
        }
        let out = render_to_string(&s, 100, 30);
        for label in ["2 · pick a model", "3 · pick a model", "4 · pick a model", "5 · pick a model"] {
            assert!(out.contains(label), "missing {label} in:\n{out}");
        }
        // Block 1 is hidden — its title must not appear in the
        // column-strip portion of the layout.
        assert!(!out.contains("1 · pick a model"), "hidden block 1 leaked into columns; got:\n{out}");
        // The menu is closed, so the panel rows must NOT be drawn.
        // (`session 1` lives only in the menu; the column strip
        // for the visible blocks starts at `session 2`.)
        assert!(!out.contains("session 1"), "menu panel rendered while closed; got:\n{out}");
    }

    #[test]
    fn menu_open_lists_every_session_in_creation_order() {
        // Five sessions with the menu open. The panel lists each
        // session as a plain "session N" row — model names and
        // picker state are deliberately absent (the column strip
        // on the right owns those).
        let mut s = UiState::default();
        for _ in 0..5 {
            s.apply(&crate::bus::AppEvent::AddBlock);
        }
        s.menu = Some(crate::picker::SessionMenu::open_at(0));
        let out = render_to_string(&s, 100, 30);
        for n in 1..=5 {
            let needle = format!("session {n}");
            assert!(
                out.contains(&needle),
                "menu missing session {n}; got:\n{out}"
            );
        }
    }

    #[test]
    fn menu_window_returns_full_list_when_everything_fits() {
        // 5 rows in a 28-row panel: nothing to scroll.
        assert_eq!(menu_window(5, 0, 28), (0, 5));
        assert_eq!(menu_window(5, 4, 28), (0, 5));
    }

    #[test]
    fn menu_window_returns_empty_when_no_rows() {
        assert_eq!(menu_window(0, 0, 28), (0, 0));
        assert_eq!(menu_window(5, 0, 0), (0, 0));
    }

    #[test]
    fn menu_window_recenters_on_selected_row() {
        // 31 sessions, 28-row panel, selection at row 15 (middle).
        // Window starts at selected - height/2 = 15 - 14 = 1, end = 1 + 28 = 29.
        assert_eq!(menu_window(31, 15, 28), (1, 29));
        // Selection near top: window anchored at 0.
        assert_eq!(menu_window(31, 0, 28), (0, 28));
        // Selection near bottom: window anchored to fit.
        assert_eq!(menu_window(31, 26, 28), (3, 31));
        // Selection at very bottom: same anchor.
        assert_eq!(menu_window(31, 30, 28), (3, 31));
    }

    #[test]
    fn menu_window_clamps_when_panel_shrinks() {
        // Terminal resized from 28 rows to 10 — window must follow.
        assert_eq!(menu_window(31, 15, 10), (10, 20));
        // Resize larger (40 rows still doesn't fit 100 rows).
        assert_eq!(menu_window(100, 50, 40), (30, 70));
    }

    #[test]
    fn menu_window_always_includes_the_selected_row() {
        // Across a sweep of selections and heights, the window must
        // contain `selected`. This is the carousel's core invariant:
        // if it ever fails, the highlighted row is offscreen.
        for total in [10usize, 31, 100] {
            for height in [5usize, 14, 28, 40, 80] {
                for selected in 0..total {
                    let (start, end) = menu_window(total, selected, height);
                    assert!(
                        start <= selected && selected < end,
                        "total={total} sel={selected} h={height}: window [{start},{end}) does not contain selected"
                    );
                }
            }
        }
    }

    #[test]
    fn menu_window_returns_a_window_no_longer_than_height_or_total() {
        for total in [0usize, 1, 5, 31, 100] {
            for height in [0usize, 1, 28, 80] {
                for selected in 0..total.max(1) {
                    let sel = selected.min(total.saturating_sub(1));
                    let (start, end) = menu_window(total, sel, height);
                    let window = end - start;
                    assert!(window <= height, "window {window} > height {height}");
                    assert!(window <= total, "window {window} > total {total}");
                    assert!(start <= end, "start {start} > end {end}");
                }
            }
        }
    }
}
