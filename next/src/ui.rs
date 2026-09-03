use ratatui::{
    layout::{Constraint, Direction, Layout},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use crate::state::UiState;

/// Draw one frame: outer window with `state.title` in its top border,
/// then `state.blocks` evenly-distributed columns inside (each with
/// `Borders::LEFT | Borders::RIGHT` so adjacent columns share a `│`
/// divider, and a ` N ` label so each block is visible on its own).
/// Pure function of `state`; never panics regardless of frame size.
pub fn render(f: &mut Frame, state: &UiState) {
    let window = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", state.title));
    f.render_widget(&window, f.area());

    if state.blocks == 0 {
        return;
    }
    let inner = window.inner(f.area());
    let columns =
        Layout::new(Direction::Horizontal, vec![Constraint::Fill(1); state.blocks]).split(inner);
    for (index, column) in columns.iter().enumerate() {
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
        assert!(lines[4].starts_with('└') && lines[4].ends_with('┘'));
        assert_eq!(lines[1], "││ 1              ││");
        for inner in &lines[2..4] {
            assert_eq!(*inner, "││                ││");
        }
    }

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
        assert_eq!(lines[1], "││ 1  ││ 2  ││ 3  ││");
        for inner in &lines[2..10] {
            assert_eq!(*inner, "││    ││    ││    ││");
        }
    }

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
