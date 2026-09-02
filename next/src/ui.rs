use ratatui::{
    layout::{Constraint, Direction, Layout},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use crate::state::UiState;

/// Draw one frame: an empty bordered "terminal window" filling the whole
/// frame, with `state.title` in the top border. If `state.blocks > 0`,
/// split the inner area into that many evenly-distributed columns; each
/// column renders just its left and right edges (`Borders::LEFT |
/// Borders::RIGHT`), so adjacent columns share a `│` divider, and a
/// small ` N ` label at the top of the column makes each block visible
/// on its own. The split reflows on every draw so it stays responsive
/// to terminal size. Pure function of `state`; never panics regardless
/// of frame size.
pub fn render(f: &mut Frame, state: &UiState) {
    // Borrow the window so `inner(area)` is still available afterwards.
    let window = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", state.title));
    f.render_widget(&window, f.area());

    // Skip the layout split when there's nothing to render — keeps the
    // 0-block frame byte-identical to the pre-bus golden and matches the
    // project rule of doing only what each path needs.
    if state.blocks == 0 {
        return;
    }

    // Split the inner area horizontally into N even columns. `Direction`
    // is spelled explicitly (default-axis is also horizontal, but writing
    // it here keeps the axis choice obvious to readers and to any future
    // refactor that wraps this in a helper).
    let inner = window.inner(f.area());
    let columns =
        Layout::new(Direction::Horizontal, vec![Constraint::Fill(1); state.blocks]).split(inner);
    for (index, column) in columns.iter().enumerate() {
        // `Borders::LEFT | Borders::RIGHT` keeps the top, bottom and the
        // outer frame intact while drawing only the column's two vertical
        // edges. Adjacent columns share a divider this way; the outermost
        // columns draw a `│` against the outer window's frame, which is
        // exactly what makes the split visible at all.
        let column_block = Block::default().borders(Borders::LEFT | Borders::RIGHT);
        let label = Paragraph::new(format!(" {} ", index + 1));
        f.render_widget(label.block(column_block), *column);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    /// One block: outer window + one column with left/right dividers
    /// and the label `" 1 "`. With width 20 / height 5 the outer
    /// consumes 2 rows + 2 cols, leaving an 18×3 interior. The column
    /// block draws `│` at cols 1 and 18, so interior rows are
    /// `││…││` (outer `│` + col-left + 16 cells + col-right + outer `│`).
    #[test]
    fn one_block_fills_the_window_with_label() {
        let state = UiState {
            blocks: 1,
            ..Default::default()
        };
        let out = render_to_string(&state, 20, 5);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 5);
        assert!(lines[0].starts_with("┌ Voice Bird ─") && lines[0].ends_with('┐'));
        // First interior row carries the column label " 1 ".
        assert_eq!(lines[1], "││ 1              ││");
        // Remaining interior rows are blank between the four `│`s.
        for inner in &lines[2..4] {
            assert_eq!(*inner, "││                ││");
        }
    }

    /// Three blocks: outer window + three columns each with left/right
    /// dividers and labels `" 1 "`, `" 2 "`, `" 3 "`. With width 20 /
    /// height 11 the outer consumes 2 rows + 2 cols, leaving an 18×9
    /// interior. Three `Constraint::Fill(1)` columns each get 6 cols.
    /// Adjacent columns share a divider — middle column is bounded by
    /// `││` on both sides from the neighbours.
    #[test]
    fn three_blocks_split_into_three_columns_with_labels() {
        let state = UiState {
            blocks: 3,
            ..Default::default()
        };
        let out = render_to_string(&state, 20, 11);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 11);
        assert!(lines[0].starts_with('┌') && lines[0].ends_with('┐'));
        assert!(lines[10].starts_with('└') && lines[10].ends_with('┘'));
        // First interior row carries the three column labels.
        assert_eq!(lines[1], "││ 1  ││ 2  ││ 3  ││");
        // Remaining interior rows are blank between the dividers.
        for inner in &lines[2..10] {
            assert_eq!(*inner, "││    ││    ││    ││");
        }
    }

    /// Even on a 3-wide, 1-tall area with 5 blocks, the
    /// `Direction::Horizontal` layout clamps zero-width columns and
    /// ratatui skips the draw — we just must not panic.
    #[test]
    fn many_blocks_in_a_tiny_terminal_do_not_panic() {
        for (w, h) in [(1, 1), (3, 1), (2, 3), (4, 4)] {
            let state = UiState {
                blocks: 5,
                ..Default::default()
            };
            let _ = render_to_string(&state, w, h);
        }
    }
}
