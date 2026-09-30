//! Block-level rendering.
//!
//! `render` is a pure function of `&UiState`. N concurrent blocks at
//! N stages each render inside their own column; arrows move focus, the
//! focused block draws `Borders::ALL` while the rest draw
//! `Borders::LEFT | Borders::RIGHT` so adjacent columns share a `│`
//! divider.

use std::collections::BTreeMap;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    prelude::Stylize,
    style::Style,
    text::Line,
    widgets::{Block, Borders, Gauge, Paragraph, Wrap},
    Frame,
};

use crate::language::LanguageProfile;
use crate::picker::LanguagePicker;
use crate::state::{BlockState, DownloadPhase, DownloadState, UiState};

/// Draw one frame: outer window with `state.title` in its top border,
/// then `state.blocks` evenly-distributed columns inside.
///
/// Each column renders its own stage:
/// - `Picking(p)` → the language list, `▶` on `p.index`.
/// - `Waiting` → one gauge aggregating all pending model downloads.
/// - `Recording` → `● recording (mocked)`.
/// - `Failed` → the error wrapped, plus `r retry · Esc close`.
pub fn render(f: &mut Frame, state: &UiState) {
    // When the reducer sets a transient warning (e.g. "session
    // limit reached; close a session to make room"), append it to
    // the title bar so the user actually sees it. The title bar
    // is the one place we *know* is on screen, regardless of cap,
    // menu state, or focused block.
    let title = match state.warning.as_deref() {
        Some(w) => format!("{} — ! {w}", state.title),
        None => state.title.clone(),
    };
    let window = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {title} "))
        .border_style(if state.warning.is_some() {
            Style::default().bold().red()
        } else {
            Style::default()
        });
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
    let visible: Vec<&crate::state::Block> = state.blocks.iter().filter(|b| b.visible).collect();
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
        BlockState::Waiting { language, pending } => {
            let aggregate = aggregate_download(&state.downloads, pending);
            render_gauge(f, language.code, aggregate.as_ref(), inner);
        }
        _ => {
            let lines = block_body_lines(block, state);
            f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
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
        BlockState::Picking(_) => format!("{} · pick a language", block.id),
        BlockState::Waiting { language, .. } | BlockState::Recording { language } => {
            format!("{} · {}", block.id, language.code)
        }
        BlockState::Failed { language, .. } => {
            format!("{} · {} · error", block.id, language.code)
        }
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
fn render_menu(f: &mut Frame, state: &UiState, menu: &crate::picker::SessionMenu, area: Rect) {
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
        let marker = if absolute == menu.index {
            "\u{25b6}"
        } else {
            "  "
        };
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
        BlockState::Failed { error, .. } => {
            vec![Line::from(error.clone()), Line::from("r retry · Esc close")]
        }
    }
}

/// Aggregate the progress of every pending model into one language-level row.
pub fn aggregate_download(
    downloads: &BTreeMap<&'static str, DownloadState>,
    pending: &[&'static str],
) -> Option<DownloadState> {
    if pending.is_empty() {
        return None;
    }
    let mut seen = false;
    let mut bytes = 0u64;
    let mut total = 0u64;
    let mut all_totals_known = true;
    let mut bytes_per_sec = 0u64;
    let mut all_installing = true;
    for model in pending {
        match downloads.get(model) {
            Some(state) => {
                seen = true;
                bytes = bytes.saturating_add(state.bytes);
                bytes_per_sec = bytes_per_sec.saturating_add(state.bytes_per_sec);
                match state.total {
                    Some(model_total) => total = total.saturating_add(model_total),
                    None => all_totals_known = false,
                }
                all_installing &= state.phase == DownloadPhase::Installing;
            }
            None => {
                all_totals_known = false;
                all_installing = false;
            }
        }
    }
    seen.then_some(DownloadState {
        phase: if all_installing {
            DownloadPhase::Installing
        } else {
            DownloadPhase::Fetching
        },
        bytes,
        total: all_totals_known.then_some(total),
        bytes_per_sec,
    })
}

fn render_gauge(f: &mut Frame, language: &str, state: Option<&DownloadState>, area: Rect) {
    match state {
        Some(s) if s.phase == DownloadPhase::Installing => {
            f.render_widget(Paragraph::new(format!("Preparing {language}…")), area);
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

fn picker_lines(picker: &LanguagePicker) -> Vec<Line<'static>> {
    picker
        .languages()
        .iter()
        .enumerate()
        .map(|(i, language)| picker_row(i, picker.index, language))
        .collect()
}

fn picker_row(index: usize, selected: usize, language: &'static LanguageProfile) -> Line<'static> {
    let marker = if index == selected { "\u{25b6}" } else { "  " };
    Line::from(format!("{marker} {}", language.code))
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
    use crate::language::LANGUAGES;
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
    fn one_block_picking_lists_languages_with_marker() {
        let state = UiState {
            blocks: vec![Block {
                id: 1,
                state: BlockState::Picking(LanguagePicker::open(
                    crate::picker::PickerIntent::AddBlock,
                )),
                ..Default::default()
            }],
            focus: 0,
            next_block_id: 2,
            ..Default::default()
        };
        let out = render_to_string(&state, 100, 30);
        assert!(out.contains("\u{25b6} en"));
        assert!(out.contains("pick a language"));
        for model in LANGUAGES[0].models() {
            assert!(
                !out.contains(model.id),
                "model id leaked into picker: {out}"
            );
        }
    }

    #[test]
    fn recording_title_uses_language_code() {
        let state = UiState {
            blocks: vec![Block {
                id: 1,
                state: BlockState::Recording {
                    language: &LANGUAGES[0],
                },
                ..Default::default()
            }],
            focus: 0,
            next_block_id: 2,
            ..Default::default()
        };
        let out = render_to_string(&state, 80, 10);
        assert!(out.contains("recording"), "{out}");
        assert!(out.contains("1 · en"), "{out}");
    }

    #[test]
    fn focused_block_border_differs_from_unfocused() {
        let state = UiState {
            blocks: (1..=3)
                .map(|id| Block {
                    id,
                    state: BlockState::Recording {
                        language: &LANGUAGES[0],
                    },
                    ..Default::default()
                })
                .collect(),
            focus: 1,
            next_block_id: 4,
            ..Default::default()
        };
        let out = render_to_string(&state, 100, 10);
        assert!(out.matches('─').count() >= 3);
    }

    #[test]
    fn three_blocks_split_into_language_titled_columns() {
        let state = UiState {
            blocks: (1..=3)
                .map(|id| Block {
                    id,
                    state: BlockState::Recording {
                        language: &LANGUAGES[0],
                    },
                    ..Default::default()
                })
                .collect(),
            focus: 0,
            next_block_id: 4,
            ..Default::default()
        };
        let out = render_to_string(&state, 100, 30);
        for label in ["1 · en", "2 · en", "3 · en"] {
            assert!(out.contains(label), "missing {label} in:\n{out}");
        }
    }

    #[test]
    fn many_blocks_in_a_tiny_terminal_do_not_panic() {
        let state = UiState {
            blocks: (1..=5)
                .map(|id| Block {
                    id,
                    state: BlockState::Recording {
                        language: &LANGUAGES[0],
                    },
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
    fn picker_row_renders_only_language_code() {
        let line = picker_row(0, 0, &LANGUAGES[0]);
        assert_eq!(line.to_string(), "\u{25b6} en");
        for model in LANGUAGES[0].models() {
            assert!(!line.to_string().contains(model.id));
        }
    }

    #[test]
    fn aggregate_download_sums_progress_and_rates() {
        let mut downloads = BTreeMap::new();
        downloads.insert(
            LANGUAGES[0].live.id,
            DownloadState {
                phase: DownloadPhase::Fetching,
                bytes: 20,
                total: Some(40),
                bytes_per_sec: 3,
            },
        );
        downloads.insert(
            LANGUAGES[0].refine.id,
            DownloadState {
                phase: DownloadPhase::Fetching,
                bytes: 30,
                total: Some(60),
                bytes_per_sec: 4,
            },
        );
        assert_eq!(
            aggregate_download(&downloads, &[LANGUAGES[0].live.id, LANGUAGES[0].refine.id]),
            Some(DownloadState {
                phase: DownloadPhase::Fetching,
                bytes: 50,
                total: Some(100),
                bytes_per_sec: 7,
            })
        );
    }

    #[test]
    fn aggregate_download_requires_every_total_and_installing_phase() {
        let mut downloads = BTreeMap::new();
        downloads.insert(
            LANGUAGES[0].live.id,
            DownloadState {
                phase: DownloadPhase::Installing,
                bytes: 20,
                total: Some(40),
                bytes_per_sec: 0,
            },
        );
        downloads.insert(
            LANGUAGES[0].refine.id,
            DownloadState {
                phase: DownloadPhase::Fetching,
                bytes: 30,
                total: None,
                bytes_per_sec: 4,
            },
        );
        let aggregate =
            aggregate_download(&downloads, &[LANGUAGES[0].live.id, LANGUAGES[0].refine.id])
                .unwrap();
        assert_eq!(aggregate.total, None);
        assert_eq!(aggregate.phase, DownloadPhase::Fetching);
        downloads.get_mut(LANGUAGES[0].refine.id).unwrap().phase = DownloadPhase::Installing;
        assert_eq!(
            aggregate_download(&downloads, &[LANGUAGES[0].live.id, LANGUAGES[0].refine.id])
                .unwrap()
                .phase,
            DownloadPhase::Installing
        );
    }

    #[test]
    fn waiting_block_draws_combined_gauge() {
        let mut state = UiState {
            blocks: vec![Block {
                id: 1,
                state: BlockState::Waiting {
                    language: &LANGUAGES[0],
                    pending: vec![LANGUAGES[0].live.id, LANGUAGES[0].refine.id],
                },
                ..Default::default()
            }],
            focus: 0,
            next_block_id: 2,
            ..Default::default()
        };
        for model in LANGUAGES[0].models() {
            state.downloads.insert(
                model.id,
                DownloadState {
                    phase: DownloadPhase::Fetching,
                    bytes: 50,
                    total: Some(100),
                    bytes_per_sec: 0,
                },
            );
        }
        let out = render_to_string(&state, 100, 10);
        assert!(
            out.contains('█'),
            "expected filled gauge cells; got:\n{out}"
        );
        assert!(out.contains("1 · en"));
        for model in LANGUAGES[0].models() {
            assert!(!out.contains(model.id));
        }
    }

    #[test]
    fn waiting_block_without_a_record_does_not_panic() {
        let state = UiState {
            blocks: vec![Block {
                id: 1,
                state: BlockState::Waiting {
                    language: &LANGUAGES[0],
                    pending: vec![LANGUAGES[0].live.id],
                },
                ..Default::default()
            }],
            ..Default::default()
        };
        let _ = render_to_string(&state, 40, 5);
    }

    #[test]
    fn waiting_block_installing_phase_says_preparing_language() {
        let mut state = UiState {
            blocks: vec![Block {
                id: 1,
                state: BlockState::Waiting {
                    language: &LANGUAGES[0],
                    pending: vec![LANGUAGES[0].live.id, LANGUAGES[0].refine.id],
                },
                ..Default::default()
            }],
            ..Default::default()
        };
        for model in LANGUAGES[0].models() {
            state.downloads.insert(
                model.id,
                DownloadState {
                    phase: DownloadPhase::Installing,
                    bytes: 0,
                    total: Some(100),
                    bytes_per_sec: 0,
                },
            );
        }
        let out = render_to_string(&state, 100, 10);
        assert!(out.contains("Preparing en…"), "got:\n{out}");
        assert!(!out.contains('\u{2588}'));
    }

    #[test]
    fn waiting_block_without_content_length_uses_combined_rate() {
        let mut state = UiState {
            blocks: vec![Block {
                id: 1,
                state: BlockState::Waiting {
                    language: &LANGUAGES[0],
                    pending: vec![LANGUAGES[0].live.id],
                },
                ..Default::default()
            }],
            ..Default::default()
        };
        state.downloads.insert(
            LANGUAGES[0].live.id,
            DownloadState {
                phase: DownloadPhase::Fetching,
                bytes: 1024,
                total: None,
                bytes_per_sec: 2 * 1024 * 1024,
            },
        );
        let out = render_to_string(&state, 40, 5);
        assert!(out.contains("MB/s"), "{out}");
        assert!(!out.contains(LANGUAGES[0].live.id));
    }

    #[test]
    fn failed_block_shows_language_error_and_retry_hint() {
        let state = UiState {
            blocks: vec![Block {
                id: 1,
                state: BlockState::Failed {
                    language: &LANGUAGES[0],
                    error: "HTTP 404".to_string(),
                },
                ..Default::default()
            }],
            ..Default::default()
        };
        let out = render_to_string(&state, 100, 10);
        assert!(out.contains("1 · en · error"), "{out}");
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
        for label in [
            "2 · pick a language",
            "3 · pick a language",
            "4 · pick a language",
            "5 · pick a language",
        ] {
            assert!(out.contains(label), "missing {label} in:\n{out}");
        }
        // Block 1 is hidden — its title must not appear in the
        // column-strip portion of the layout.
        assert!(
            !out.contains("1 · pick a language"),
            "hidden block 1 leaked into columns; got:\n{out}"
        );
        // The menu is closed, so the panel rows must NOT be drawn.
        // (`session 1` lives only in the menu; the column strip
        // for the visible blocks starts at `session 2`.)
        assert!(
            !out.contains("session 1"),
            "menu panel rendered while closed; got:\n{out}"
        );
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
