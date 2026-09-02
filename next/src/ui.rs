use ratatui::{
    layout::{Constraint, Layout},
    widgets::{Block, Borders},
    Frame,
};

use crate::state::UiState;

/// Draw one frame: an empty bordered "terminal window" filling the whole
/// frame, with `state.title` in the top border. If `state.blocks > 0`,
/// stack that many empty bordered panels inside; the layout reflows on
/// every draw so it stays responsive to terminal size. Pure function of
/// `state`; never panics regardless of the frame size.
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

    let inner = window.inner(f.area());
    let rows = Layout::vertical(vec![Constraint::Fill(1); state.blocks]).split(inner);
    for row in rows.iter() {
        f.render_widget(Block::default().borders(Borders::ALL), *row);
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

    /// One block fits inside the outer border: the inner area gets its
    /// own border on all four sides. With width 20 / height 5 the outer
    /// consumes 2 rows + 2 cols, leaving 16×1 — but `Constraint::Fill(1)`
    /// clamps zero-height rows without panicking, so use 20×5 with the
    /// expectations driven by exact row shape rather than the per-row height.
    #[test]
    fn one_block_renders_inside_the_window() {
        let state = UiState {
            blocks: 1,
            ..Default::default()
        };
        let out = render_to_string(&state, 20, 5);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[0], format!("┌ Voice Bird {}┐", "─".repeat(6)));
        // Inner block top/bottom borders live at the rows where the
        // outer interior begins and ends.
        assert!(lines[1].contains('┌') && lines[1].contains('┐'));
        assert!(lines[4].contains('└') && lines[4].contains('┘'));
    }

    #[test]
    fn three_blocks_stack_vertically() {
        let state = UiState {
            blocks: 3,
            ..Default::default()
        };
        // 20 wide × 11 tall: outer consumes 2 rows + 2 cols, 9 inner rows
        // split into 3 rows of 3 each. Every inner row is bordered.
        let out = render_to_string(&state, 20, 11);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 11);
        let corners_per_line = |s: &str| {
            (s.starts_with('┌') || s.starts_with('│') || s.starts_with('└'))
                && (s.ends_with('┐') || s.ends_with('│') || s.ends_with('┘'))
        };
        // Outer frame on row 0 and row 10.
        assert!(lines[0].starts_with('┌') && lines[0].ends_with('┐'));
        assert!(lines[10].starts_with('└') && lines[10].ends_with('┘'));
        // 9 inner lines = 3 blocks × 3 rows; each line is bordered.
        for line in &lines[1..10] {
            assert!(corners_per_line(line), "inner line not bordered: {line:?}");
        }
    }

    /// Even on a 3-wide, 1-tall area with 5 blocks, `Layout::vertical`
    /// clamps zero-height rows and ratatui skips the draw — we just must
    /// not panic.
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
