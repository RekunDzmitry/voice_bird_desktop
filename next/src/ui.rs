//! Block-level rendering.
//!
//! `render` is a pure function of `&UiView`. N concurrent blocks at
//! N stages each render inside their own column; arrows move focus, the
//! focused block draws `Borders::ALL` while the rest draw
//! `Borders::LEFT | Borders::RIGHT` so adjacent columns share a `│`
//! divider.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    prelude::Stylize,
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Gauge, Paragraph, Wrap},
    Frame,
};

use crate::audio_sources::DeviceKind;
use crate::consumer::ui_view::{BlockState, DownloadPhase, DownloadState, UiView};
use crate::language::{LanguageProfile, LANGUAGES};
use crate::picker::ListPicker;

/// Draw one frame: outer window with `ui_view.title` in its top border,
/// then `ui_view.blocks` evenly-distributed columns inside.
///
/// Each column renders its own stage:
/// - `PickingLanguage(p)` → the language list, `▶` on `p.index`.
/// - `Waiting` → one gauge per model role.
/// - `Recording` → committed transcript tail, tentative text, and session path.
/// - `Failed` → the error wrapped, plus `r retry · Esc close`.
pub fn render(f: &mut Frame, ui_view: &UiView) {
    // When the reducer sets a transient warning (e.g. "session
    // limit reached; close a session to make room"), append it to
    // the title bar so the user actually sees it. The title bar
    // is always on screen, regardless of the focused block.
    let title = match ui_view.warning.as_deref() {
        Some(w) => format!("{} — ! {w}", ui_view.title),
        None => ui_view.title.clone(),
    };
    let window = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {title} "))
        .border_style(if ui_view.warning.is_some() {
            Style::default().bold().red()
        } else {
            Style::default()
        });
    f.render_widget(&window, f.area());

    let inner = window.inner(f.area());

    // The reducer caps sessions; every block gets a full-width column.
    if !ui_view.blocks.is_empty() {
        let columns = Layout::new(
            Direction::Horizontal,
            vec![Constraint::Fill(1); ui_view.blocks.len()],
        )
        .split(inner);
        for (index, (block, column)) in ui_view.blocks.iter().zip(columns.iter()).enumerate() {
            render_block(f, block, index == ui_view.focus, *column, ui_view);
        }
    }
}

fn render_block(
    f: &mut Frame,
    block: &crate::consumer::ui_view::Block,
    focused: bool,
    area: Rect,
    ui_view: &UiView,
) {
    let border = block_border(focused);
    let title = block_title(block);
    let border = border.title(format!(" {title} "));
    let inner = border.inner(area);
    f.render_widget(border, area);

    match &block.state {
        BlockState::PickingDevice(cursor) | BlockState::PickingApp(cursor) => {
            render_source_picker(f, block, cursor.index, inner);
        }
        BlockState::Waiting { language, pending } => {
            render_waiting(f, language, pending, ui_view, inner);
        }
        BlockState::Recording {
            lines,
            tentative,
            session_dir,
            ..
        } => {
            render_recording(f, lines, tentative, session_dir.as_deref(), inner);
        }
        _ => {
            let lines = block_body_lines(block);
            f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
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

fn block_title(block: &crate::consumer::ui_view::Block) -> String {
    let mut title = block.id.to_string();
    if let Some(source) = &block.source {
        let visible_labels = match &block.state {
            BlockState::PickingDevice(_) => 0,
            BlockState::PickingApp(_) => usize::from(source.device.is_some()),
            _ => usize::MAX,
        };
        for label in source.labels().take(visible_labels) {
            title.push_str(" · ");
            title.push_str(label);
        }
    }
    title.push_str(" · ");
    match &block.state {
        BlockState::PickingDevice(_) => title.push_str("pick a device"),
        BlockState::PickingApp(_) => title.push_str("pick an app"),
        BlockState::PickingLanguage(_) => title.push_str("pick a language"),
        BlockState::Waiting { language, .. } | BlockState::Recording { language, .. } => {
            title.push_str(language.code);
        }
        BlockState::Failed { language, .. } => {
            title.push_str(language.code);
            title.push_str(" · error");
        }
    }
    title
}

fn render_source_picker(
    f: &mut Frame,
    block: &crate::consumer::ui_view::Block,
    selected: usize,
    area: Rect,
) {
    let Some(source) = &block.source else { return };
    let picking_device = matches!(block.state, BlockState::PickingDevice(_));
    let total = if picking_device {
        source.snapshot.devices.len()
    } else {
        source.snapshot.apps.len()
    };
    let height = area.height.saturating_sub(2) as usize;
    // Keep the selected source centered while fitting the available rows.
    let rows = height.min(total);
    let start = selected
        .saturating_sub(rows / 2)
        .min(total.saturating_sub(rows));
    let end = start + rows;
    let mut lines = Vec::with_capacity(end - start + 2);
    if total == 0 && !picking_device {
        lines.push(Line::from("no running apps"));
    }
    for index in start..end {
        let marker = if index == selected { "▶" } else { " " };
        let line = if picking_device {
            let device = &source.snapshot.devices[index];
            let kind = match device.kind {
                DeviceKind::Input => "in ",
                DeviceKind::Output => "out",
            };
            format!("{marker} {kind} {}", device.name)
        } else {
            format!("{marker} {}", source.snapshot.apps[index].name)
        };
        lines.push(Line::from(line));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(if picking_device {
        "Enter select · Esc close"
    } else {
        "⌫ back · Esc close"
    }));
    f.render_widget(Paragraph::new(lines), area);
}

fn block_body_lines(block: &crate::consumer::ui_view::Block) -> Vec<Line<'static>> {
    match &block.state {
        BlockState::PickingLanguage(picker) => {
            let mut lines = picker_lines(picker);
            if block.source.is_some() {
                lines.push(Line::from(""));
                lines.push(Line::from("⌫ back · Esc close"));
            }
            lines
        }
        BlockState::PickingDevice(_) | BlockState::PickingApp(_) => Vec::new(),
        // Waiting is rendered directly by render_block (Gauge widget).
        BlockState::Waiting { .. } => Vec::new(),
        BlockState::Recording { .. } => Vec::new(),
        BlockState::Failed { error, .. } => {
            vec![Line::from(error.clone()), Line::from("r retry · Esc close")]
        }
    }
}

/// Wrap before choosing the tail, so long older lines cannot push new text
/// below the viewport. Width follows terminal cells, not UTF-8 bytes.
fn transcript_rows(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    let mut rows = Vec::new();
    for line in text.lines() {
        let mut row = String::new();
        let mut cells = 0;
        for ch in line.chars() {
            let mut bytes = [0; 4];
            let ch_width = Span::raw(ch.encode_utf8(&mut bytes) as &str).width();
            if cells + ch_width > width && !row.is_empty() {
                rows.push(std::mem::take(&mut row));
                cells = 0;
            }
            row.push(ch);
            cells += ch_width;
        }
        rows.push(row);
    }
    rows
}

fn render_recording(
    f: &mut Frame,
    committed: &[String],
    tentative: &str,
    session_dir: Option<&str>,
    area: Rect,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let height = usize::from(area.height);
    let show_header = height >= 2;
    let show_path = height >= 4 && session_dir.is_some();
    let available = height - usize::from(show_header) - usize::from(show_path);
    let pending = transcript_rows(tentative, usize::from(area.width));
    let pending_count = pending.len().min(available.div_ceil(2));
    let committed_count = available - pending_count;
    let rows: Vec<_> = committed
        .iter()
        .flat_map(|line| transcript_rows(line, usize::from(area.width)))
        .collect();
    let committed_start = rows.len().saturating_sub(committed_count);
    let pending_start = pending.len().saturating_sub(pending_count);
    let mut lines = Vec::with_capacity(height);
    if show_header {
        lines.push(Line::from("● recording"));
    }
    lines.extend(rows.into_iter().skip(committed_start).map(Line::from));
    lines.extend(
        pending
            .into_iter()
            .skip(pending_start)
            .map(|line| Line::from(line).dim()),
    );
    if show_path {
        while lines.len() < height - 1 {
            lines.push(Line::from(""));
        }
        let name = session_dir
            .and_then(|path| std::path::Path::new(path).file_name())
            .map(|name| name.to_string_lossy())
            .unwrap_or_default();
        lines.push(Line::from(format!("sessions/{name}")).dim());
    }
    f.render_widget(Paragraph::new(lines), area);
}

fn render_waiting(
    f: &mut Frame,
    language: &'static LanguageProfile,
    pending: &[&'static str],
    ui_view: &UiView,
    area: Rect,
) {
    let models = language.models();
    let rows = Layout::new(
        Direction::Vertical,
        models
            .iter()
            .map(|_| Constraint::Ratio(1, models.len() as u32)),
    )
    .split(area);
    for (&model, row) in models.iter().zip(rows.iter()) {
        let role = if model.id == language.live.id {
            "live"
        } else if model.id == language.refine.id {
            "refine"
        } else if model.id.strip_suffix("-encoder") == Some(language.live.id) {
            "live encoder"
        } else if model.id.strip_suffix("-encoder") == Some(language.refine.id) {
            "refine encoder"
        } else {
            "asset"
        };
        render_model_gauge(
            f,
            role,
            pending.contains(&model.id),
            ui_view.downloads.get(model.id),
            *row,
        );
    }
}

fn render_model_gauge(
    f: &mut Frame,
    role: &str,
    pending: bool,
    state: Option<&DownloadState>,
    area: Rect,
) {
    if !pending {
        f.render_widget(
            Gauge::default()
                .gauge_style(Style::default().bold())
                .ratio(1.0)
                .label(format!("{role} · ready")),
            area,
        );
        return;
    }

    match state {
        Some(s) if s.phase == DownloadPhase::Installing => {
            f.render_widget(
                Gauge::default()
                    .gauge_style(Style::default().bold())
                    .ratio(s.ratio().unwrap_or(0.0))
                    .label(format!("{role} · Preparing…")),
                area,
            );
        }
        Some(s) => {
            let ratio = s.ratio().unwrap_or(0.0);
            let progress = match s.total {
                Some(t) => format!("{} / {}", human_bytes(s.bytes), human_bytes(t)),
                None => format!(
                    "{} · {:.2} MB/s",
                    human_bytes(s.bytes),
                    s.bytes_per_sec as f64 / 1_000_000.0
                ),
            };
            f.render_widget(
                Gauge::default()
                    .gauge_style(Style::default().bold())
                    .ratio(ratio)
                    .label(format!("{role} · {progress}")),
                area,
            );
        }
        None => {
            f.render_widget(Gauge::default().ratio(0.0).label(role.to_string()), area);
        }
    }
}

fn picker_lines(picker: &ListPicker) -> Vec<Line<'static>> {
    LANGUAGES
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
    use crate::consumer::ui_view::Block;
    use crate::language::LANGUAGES;
    use crate::testing::render_to_string;

    #[test]
    fn window_is_an_empty_bordered_box_with_title() {
        let out = render_to_string(&UiView::default(), 40, 5);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[0], format!("┌ Voice Bird {}┐", "─".repeat(26)));
        assert_eq!(lines[4], format!("└{}┘", "─".repeat(38)));
        for inner in &lines[1..4] {
            assert_eq!(*inner, format!("│{}│", " ".repeat(38)));
        }
    }

    #[test]
    fn title_comes_from_ui_view() {
        let ui_view = UiView {
            title: "Hello".to_string(),
            ..Default::default()
        };
        let out = render_to_string(&ui_view, 20, 3);
        assert!(out.starts_with("┌ Hello ─"), "{out}");
    }

    #[test]
    fn tiny_sizes_do_not_panic() {
        for (w, h) in [(1, 1), (2, 2), (3, 3), (10, 2), (5, 40), (200, 1)] {
            let _ = render_to_string(&UiView::default(), w, h);
        }
    }

    #[test]
    fn one_block_picking_lists_languages_with_marker() {
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
        let ui_view = UiView {
            blocks: vec![Block {
                id: 1,
                state: BlockState::recording(&LANGUAGES[0]),
                ..Default::default()
            }],
            focus: 0,
            next_block_id: 2,
            ..Default::default()
        };
        let out = render_to_string(&ui_view, 80, 10);
        assert!(out.contains("recording"), "{out}");
        assert!(out.contains("1 · en"), "{out}");
    }

    #[test]
    fn focused_block_border_differs_from_unfocused() {
        let ui_view = UiView {
            blocks: (1..=3)
                .map(|id| Block {
                    id,
                    state: BlockState::recording(&LANGUAGES[0]),
                    ..Default::default()
                })
                .collect(),
            focus: 1,
            next_block_id: 4,
            ..Default::default()
        };
        let out = render_to_string(&ui_view, 100, 10);
        assert!(out.matches('─').count() >= 3);
    }

    #[test]
    fn three_blocks_split_into_language_titled_columns() {
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
        for label in ["1 · en", "2 · en", "3 · en"] {
            assert!(out.contains(label), "missing {label} in:\n{out}");
        }
    }

    #[test]
    fn many_blocks_in_a_tiny_terminal_do_not_panic() {
        let ui_view = UiView {
            blocks: (1..=5)
                .map(|id| Block {
                    id,
                    state: BlockState::recording(&LANGUAGES[0]),
                    ..Default::default()
                })
                .collect(),
            focus: 0,
            next_block_id: 6,
            ..Default::default()
        };
        let _ = render_to_string(&ui_view, 3, 5);
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
    fn waiting_block_draws_separate_role_gauges() {
        let mut ui_view = UiView {
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
        let out = render_to_string(&ui_view, 100, 10);
        assert!(
            out.contains('█'),
            "expected filled gauge cells; got:\n{out}"
        );
        assert!(out.contains("1 · en"));
        assert!(out.contains("live · 50 B / 100 B"), "{out}");
        assert!(out.contains("refine · 50 B / 100 B"), "{out}");
        for model in LANGUAGES[0].models() {
            assert!(!out.contains(model.id));
        }
    }

    #[test]
    fn waiting_block_shows_completed_role_as_ready() {
        let ui_view = UiView {
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
        let out = render_to_string(&ui_view, 40, 8);
        assert!(out.contains("live"), "{out}");
        assert!(out.contains("refine · ready"), "{out}");
    }

    #[test]
    fn waiting_block_without_content_length_uses_model_rate() {
        let mut ui_view = UiView {
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
        ui_view.downloads.insert(
            LANGUAGES[0].live.id,
            DownloadState {
                attempt: 1,
                phase: DownloadPhase::Fetching,
                bytes: 1024,
                total: None,
                bytes_per_sec: 2 * 1024 * 1024,
            },
        );
        let out = render_to_string(&ui_view, 40, 8);
        assert!(out.contains("MB/s"), "{out}");
        assert!(out.contains("live"), "{out}");
        assert!(out.contains("refine · ready"), "{out}");
        assert!(!out.contains(LANGUAGES[0].live.id));
    }

    #[test]
    fn failed_block_shows_language_error_and_retry_hint() {
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
            ..Default::default()
        };
        let out = render_to_string(&ui_view, 100, 10);
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
}
