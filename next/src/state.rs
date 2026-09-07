use crate::bus::AppEvent;
use crate::picker::{ModelPicker, PickerIntent};

/// A single inner column. Pure data — no threads, no handles, no clock.
/// `id` is the 1-based label shown to the user; stable for the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub id: u32,
    pub model: String,
}

/// Everything the UI needs to draw one frame. Plain data only: the event
/// loop updates it, `ui::render` reads it, tests construct it directly.
#[derive(Debug, Clone)]
pub struct UiState {
    /// Shown in the top border of the window.
    pub title: String,
    pub should_quit: bool,
    /// Inner bordered panels to draw, stacked horizontally. Carries the
    /// model id chosen for each block at creation time.
    pub blocks: Vec<Block>,
    /// Picker overlay state. `Some` while the picker is open (a `+`
    /// keystroke has been received and no `Enter`/`Esc` has resolved it).
    pub picker: Option<ModelPicker>,
    /// Next 1-based id to hand to a newly-pushed block. Monotonic across
    /// the session; never reused even after blocks are removed (out of
    /// scope for this PR).
    pub next_block_id: u32,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            title: "Voice Bird".to_string(),
            should_quit: false,
            blocks: Vec::new(),
            picker: None,
            next_block_id: 1,
        }
    }
}

impl UiState {
    /// Pure fold over an [`AppEvent`]. The bus is transport; this is where
    /// the event becomes a state mutation. Kept on `UiState` because the
    /// rule is "no handles in state", not "no reducer in the state module".
    ///
    /// `PickerMoved`/`PickerCancelled` are no-ops when the picker is
    /// closed — the bus is fire-and-forget and a future keystroke logger
    /// can subscribe without parsing keys.
    pub fn apply(&mut self, event: AppEvent) {
        match event {
            AppEvent::AddBlock => {
                self.picker = Some(ModelPicker::open(PickerIntent::AddBlock));
            }
            AppEvent::PickerMoved(mv) => {
                if let Some(p) = self.picker.as_mut() {
                    p.apply(crate::picker::PickerEvent::Moved(mv));
                }
            }
            AppEvent::ModelSelected(entry) => {
                if self.picker.is_some() {
                    let id = self.next_block_id;
                    self.blocks.push(Block {
                        id,
                        model: entry.id.to_string(),
                    });
                    self.next_block_id = id + 1;
                    self.picker = None;
                }
            }
            AppEvent::PickerCancelled => {
                self.picker = None;
            }
            AppEvent::Quit => self.should_quit = true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::picker::{ModelEntry, PickerMove, CATALOG};

    #[test]
    fn apply_quit_sets_should_quit() {
        let mut s = UiState::default();
        s.apply(AppEvent::Quit);
        assert!(s.should_quit);
    }

    #[test]
    fn apply_add_block_opens_picker() {
        let mut s = UiState::default();
        assert!(s.picker.is_none());
        s.apply(AppEvent::AddBlock);
        assert!(s.picker.is_some());
        assert!(s.blocks.is_empty());
    }

    #[test]
    fn apply_model_selected_pushes_block_and_clears_picker() {
        let mut s = UiState::default();
        s.apply(AppEvent::AddBlock);
        let entry: &'static ModelEntry = &CATALOG[0];
        s.apply(AppEvent::ModelSelected(entry));
        assert!(s.picker.is_none());
        assert_eq!(s.blocks.len(), 1);
        assert_eq!(s.blocks.last().unwrap().id, 1);
        assert_eq!(s.blocks.last().unwrap().model, entry.id);
        assert_eq!(s.next_block_id, 2);
    }

    #[test]
    fn apply_model_selected_is_noop_when_picker_closed() {
        let mut s = UiState::default();
        let entry: &'static ModelEntry = &CATALOG[1];
        s.apply(AppEvent::ModelSelected(entry));
        assert!(s.blocks.is_empty());
        assert_eq!(s.next_block_id, 1);
    }

    #[test]
    fn apply_picker_cancelled_clears_picker() {
        let mut s = UiState::default();
        s.apply(AppEvent::AddBlock);
        assert!(s.picker.is_some());
        s.apply(AppEvent::PickerCancelled);
        assert!(s.picker.is_none());
        assert!(s.blocks.is_empty());
    }

    #[test]
    fn apply_picker_moved_clamps_index() {
        let mut s = UiState::default();
        s.apply(AppEvent::AddBlock);
        // Down past last index clamps to last.
        for _ in 0..(CATALOG.len() + 5) {
            s.apply(AppEvent::PickerMoved(PickerMove::Down));
        }
        let p = s.picker.as_ref().unwrap();
        assert_eq!(p.index, CATALOG.len() - 1);
        // Up clamps at zero.
        for _ in 0..(CATALOG.len() + 5) {
            s.apply(AppEvent::PickerMoved(PickerMove::Up));
        }
        let p = s.picker.as_ref().unwrap();
        assert_eq!(p.index, 0);
    }

    #[test]
    fn apply_picker_moved_is_noop_when_picker_closed() {
        let mut s = UiState::default();
        s.apply(AppEvent::PickerMoved(PickerMove::Down));
        s.apply(AppEvent::PickerMoved(PickerMove::Up));
        assert!(s.picker.is_none());
        assert!(s.blocks.is_empty());
    }

    #[test]
    fn next_block_id_monotonic_across_multiple_adds() {
        let mut s = UiState::default();
        for expected in 1u32..=3 {
            s.apply(AppEvent::AddBlock);
            let entry = CATALOG.first().unwrap();
            s.apply(AppEvent::ModelSelected(entry));
            assert_eq!(s.blocks.last().unwrap().id, expected);
            assert_eq!(s.next_block_id, expected + 1);
        }
    }

    #[test]
    fn apply_leaves_other_fields_untouched() {
        let mut s = UiState {
            title: "Hello".to_string(),
            should_quit: true,
            blocks: Vec::new(),
            picker: None,
            next_block_id: 42,
        };
        s.apply(AppEvent::AddBlock);
        assert_eq!(s.title, "Hello");
        assert!(s.should_quit); // unchanged
        assert_eq!(s.next_block_id, 42); // not bumped yet — only on ModelSelected
        assert!(s.picker.is_some());
    }
}