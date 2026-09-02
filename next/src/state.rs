use crate::bus::AppEvent;

/// Everything the UI needs to draw one frame. Plain data only: the event
/// loop updates it, `ui::render` reads it, tests construct it directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiState {
    /// Shown in the top border of the window.
    pub title: String,
    pub should_quit: bool,
    /// Number of inner bordered panels to draw, stacked vertically.
    /// A count for now; becomes `Vec<BlockState>` when blocks carry content.
    pub blocks: usize,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            title: "Voice Bird".to_string(),
            should_quit: false,
            blocks: 0,
        }
    }
}

impl UiState {
    /// Pure fold over an [`AppEvent`]. The bus is transport; this is where
    /// the event becomes a state mutation. Kept on `UiState` because the
    /// rule is "no handles in state", not "no reducer in the state module".
    pub fn apply(&mut self, event: AppEvent) {
        match event {
            AppEvent::Quit => self.should_quit = true,
            AppEvent::AddBlock => self.blocks += 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_quit_sets_should_quit() {
        let mut s = UiState::default();
        s.apply(AppEvent::Quit);
        assert!(s.should_quit);
    }

    #[test]
    fn apply_add_block_increments_count() {
        let mut s = UiState::default();
        for expected in 1..=5 {
            s.apply(AppEvent::AddBlock);
            assert_eq!(s.blocks, expected);
        }
    }

    #[test]
    fn apply_leaves_other_fields_untouched() {
        let mut s = UiState {
            title: "Hello".to_string(),
            should_quit: true,
            blocks: 7,
        };
        s.apply(AppEvent::AddBlock);
        assert_eq!(s.title, "Hello");
        assert!(s.should_quit); // unchanged
        assert_eq!(s.blocks, 8);
    }
}
