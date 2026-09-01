/// Everything the UI needs to draw one frame. Plain data only: the event
/// loop updates it, `ui::render` reads it, tests construct it directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiState {
    /// Shown in the top border of the window.
    pub title: String,
    pub should_quit: bool,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            title: "Voice Bird".to_string(),
            should_quit: false,
        }
    }
}
