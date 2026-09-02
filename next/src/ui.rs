use ratatui::{
    layout::{Constraint, Direction, Layout},
    widgets::{Block, Borders},
    Frame,
};

use crate::state::UiState;

/// Draw one frame: an empty bordered "terminal window" filling the whole
/// frame, with `state.title` in the top border. If `state.blocks > 0`,
/// split the inner area into that many evenly-distributed columns; the
/// columns themselves carry no border (only the outer window does), and
/// the split reflows on every draw so it stays responsive to terminal
/// size. Pure function of `state`; never panics regardless of frame size.
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
    for column in columns.iter() {
        // Inner blocks render no border — the outer window supplies the
        // visual frame and the columns are an even split of its interior.
        f.render_widget(Block::default(), *column);
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
    /// One block fills the inner area: no inner border, just `│…│` rows.
    /// With width 20 / height 5 the outer consumes 2 rows + 2 cols,
    /// leaving an 18×1 interior column.
    #[test]
    fn one_block_fills_the_window() {
        let state = UiState {
            blocks: 1,
            ..Default::default()
        };
        let out = render_to_string(&state, 20, 5);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 5);
        // Outer top border carries the title.
        assert!(lines[0].starts_with("┌ Voice Bird ─") && lines[0].ends_with('┐'));
        // Outer bottom border.
        assert!(lines[4].starts_with('└') && lines[4].ends_with('┘'));
        // Interior rows are blank — no inner border, just `│` from the
        // outer window at the ends.
        for inner in &lines[1..4] {
            assert_eq!(
                *inner,
                format!("│{}│", " ".repeat(18)),
                "interior row should be blank inside the outer frame",
            );
        }
    }

    /// Three blocks split the inner area into three equal-width columns
    /// of full interior height, with no inner borders. With width 20 /
    /// height 11 the outer consumes 2 rows + 2 cols, leaving 18×9. Three
    /// `Constraint::Fill(1)` columns each get 6 cols × 9 rows.
    #[test]
    fn three_blocks_split_into_three_columns() {
        let state = UiState {
            blocks: 3,
            ..Default::default()
        };
        let out = render_to_string(&state, 20, 11);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 11);
        // Outer top and bottom borders intact.
        assert!(lines[0].starts_with('┌') && lines[0].ends_with('┐'));
        assert!(lines[10].starts_with('└') && lines[10].ends_with('┘'));
        // Interior rows: `│`, then 18 cells (six blank × three columns),
        // then `│`. No inner `┌`/`└` borders — the columns are unmarked.
        for inner in &lines[1..10] {
            assert_eq!(
                *inner,
                format!("│{}│", " ".repeat(18)),
                "interior row should be a single blank span across all columns",
            );
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
