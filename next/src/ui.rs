use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    text::Line,
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};

use crate::picker::{ModelEntry, ModelPicker};
use crate::state::UiState;

/// Draw one frame: outer window with `state.title` in its top border,
/// then `state.blocks` evenly-distributed columns inside (each with
/// `Borders::LEFT | Borders::RIGHT` so adjacent columns share a `│`
/// divider). Each column header reads ` N · <model> `.
///
/// When `state.picker.is_some()` the existing window is preserved and a
/// centered, bordered overlay is drawn on top listing the catalog with a
/// `▶` marker on the currently-highlighted row.
///
/// Pure function of `state`; never panics regardless of frame size.
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
        for (block, column) in state.blocks.iter().zip(columns.iter()) {
            let label = format!(" {} · {} ", block.id, block.model);
            let column_block = Block::default().borders(Borders::LEFT | Borders::RIGHT);
            let p = Paragraph::new(label).block(column_block);
            f.render_widget(p, *column);
        }
    }

    if let Some(picker) = state.picker.as_ref() {
        render_picker_overlay(f, picker);
    }
}

/// Centered, bordered overlay listing the catalog with a `▶` marker on
/// the highlighted row. Width is the longest catalog id plus row chrome;
/// height is `CATALOG.len() + 2` (border). The area is clamped to the
/// frame so tiny terminals still get a usable overlay.
fn render_picker_overlay(f: &mut Frame, picker: &ModelPicker) {
    let catalog = picker.catalog();
    let title = picker.title();
    let rows: Vec<Line> = catalog
        .iter()
        .enumerate()
        .map(|(i, entry)| picker_row(i, picker.index, entry))
        .collect();
    let width = overlay_width(catalog, title);
    let height = (catalog.len() as u16 + 2).min(f.area().height);
    let area = centered_rect(width, height, f.area());
    f.render_widget(Clear, area);
    let block = Block::default().borders(Borders::ALL).title(format!(" {} ", title));
    let paragraph = Paragraph::new(rows).block(block).wrap(Wrap { trim: false });
    f.render_widget(paragraph, area);
}

/// One row of the picker overlay. The leading `▶` only on the selected
/// row; non-selected rows start with two spaces so columns line up.
fn picker_row(index: usize, selected: usize, entry: &ModelEntry) -> Line<'static> {
    let marker = if index == selected { "\u{25b6}" } else { "  " };
    Line::from(format!(
        "{} {:<32} {:>5} MB  {}",
        marker, entry.id, entry.size_mb, entry.language
    ))
}

/// Width the overlay wants before clamping. Longest catalog id + chrome
/// (`▶ ` + id padded to 32 + size column + language column).
fn overlay_width(catalog: &[ModelEntry], title: &str) -> u16 {
    let id_width = catalog
        .iter()
        .map(|e| e.id.chars().count())
        .max()
        .unwrap_or(0)
        .max(32);
    // `▶ ` (2) + id padded (32) + " " + size (up to 5 digits) + " MB" (3)
    // + " " + language (≤5) + borders (2) + slack for the title.
    let body = 2 + id_width + 1 + 5 + 3 + 1 + 5;
    let title_width = title.chars().count() + 4;
    (body.max(title_width) as u16 + 4).max(20)
}

/// Center a `w × h` rectangle inside `area`. Returns the original area
/// unchanged when the overlay is wider/taller than the frame so callers
/// never panic on tiny terminals.
fn centered_rect(w: u16, h: u16, area: Rect) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Rect::new(x, y, w, h)
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
        // 40 columns = corner + 38 inner cells + corner.
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
    fn one_block_fills_the_window_with_label() {
        let state = UiState {
            blocks: vec![Block {
                id: 1,
                model: "distil-small.en".to_string(),
            }],
            ..Default::default()
        };
        let out = render_to_string(&state, 20, 5);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 5);
        assert!(lines[0].starts_with("┌ Voice Bird ─") && lines[0].ends_with('┐'));
        assert!(lines[4].starts_with('└') && lines[4].ends_with('┘'));
        // Column inner width = 16 cells; label ` 1 · distil-small.en `
        // is 20 chars, so the paragraph truncates to the first 16.
        assert_eq!(lines[1], "││ 1 · distil-smal││");
        for inner in &lines[2..4] {
            assert_eq!(*inner, "││                ││");
        }
    }

    #[test]
    fn three_blocks_split_into_three_columns_with_labels() {
        let state = UiState {
            blocks: vec![
                Block {
                    id: 1,
                    model: "distil-small.en".to_string(),
                },
                Block {
                    id: 2,
                    model: "distil-large-v3".to_string(),
                },
                Block {
                    id: 3,
                    model: "large-v3-turbo".to_string(),
                },
            ],
            ..Default::default()
        };
        let out = render_to_string(&state, 30, 11);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 11);
        assert!(lines[0].starts_with('┌') && lines[0].ends_with('┐'));
        assert!(lines[10].starts_with('└') && lines[10].ends_with('┘'));
        // Each column header reads ` N · <model> `, padded by its borders.
        // Column inner widths are limited; Paragraph truncates long
        // labels to the first `inner_w` characters. The middle column
        // holds ` 2 · distil-large-v3 ` (20 chars) → 6-char inner
        // shows ` 2 · dis `; same for the right column. Inner rows
        // are blank but the cell grid is divided by borders.
        assert_eq!(lines[1], "││ 1 · di││ 2 · dis││ 3 · la││");
        for inner in &lines[2..10] {
            assert_eq!(*inner, "││       ││        ││       ││");
        }
    }

    #[test]
    fn many_blocks_in_a_tiny_terminal_do_not_panic() {
        let state = UiState {
            blocks: (1..=5)
                .map(|i| Block {
                    id: i,
                    model: "tiny.en".to_string(),
                })
                .collect(),
            ..Default::default()
        };
        let _ = render_to_string(&state, 3, 5);
    }

    #[test]
    fn picker_overlay_marks_highlighted_row() {
        use crate::picker::PickerIntent;
        let state = UiState {
            blocks: vec![Block {
                id: 1,
                model: "distil-small.en".to_string(),
            }],
            picker: Some(ModelPicker::open(PickerIntent::AddBlock)),
            ..Default::default()
        };
        let out = render_to_string(&state, 80, 20);
        assert!(
            out.contains("\u{25b6} distil-small.en"),
            "expected marker on the highlighted row; got:\n{out}"
        );
        // Non-selected rows lead with two spaces, not the marker.
        assert!(
            out.contains("  distil-large-v3"),
            "expected non-selected row to be padded; got:\n{out}"
        );
        assert!(
            out.contains("Pick a model (Esc to cancel)"),
            "expected picker title; got:\n{out}"
        );
    }
}