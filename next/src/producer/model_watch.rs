//! Re-check installed models used by active blocks on each loop tick.

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::bus::{AppEvent, EventSender};
use crate::consumer::ui_view::{Block, UiView};
use crate::transcription_models::ModelStore;

pub struct ModelWatcher {
    store: Arc<dyn ModelStore>,
}

impl ModelWatcher {
    pub fn new(store: Arc<dyn ModelStore>) -> Self {
        Self { store }
    }

    /// Publish once per distinct model counted as installed but missing on disk.
    /// Reducing the event adds it to pending, excluding it from the next check.
    pub fn check(&self, state: &UiView, tx: &EventSender) {
        let mut checked = BTreeSet::new();
        for model in state.blocks.iter().flat_map(Block::ready_models) {
            if checked.insert(model.id) && !self.store.is_available(model) {
                tx.publish(AppEvent::ModelMissing(model));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::EventBus;
    use crate::language::LANGUAGES;
    use crate::picker::ListPicker;
    use crate::consumer::ui_view::BlockState;
    use crate::testing::FixtureStore;

    #[test]
    fn missing_model_is_shared_and_not_republished_after_reduction() {
        let tmp = tempfile::tempdir().unwrap();
        let language = &LANGUAGES[0];
        let store = Arc::new(FixtureStore::new(
            tmp.path().to_path_buf(),
            &language.models().map(|model| model.id),
        ));
        let watcher = ModelWatcher::new(store.clone());
        let mut state = UiView {
            blocks: vec![
                Block::new(1, BlockState::Recording { language }),
                Block::new(2, BlockState::Recording { language }),
            ],
            ..UiView::default()
        };
        state.blocks[1].visible = false;
        let mut bus = EventBus::new();
        watcher.check(&state, &bus.sender());
        assert_eq!(bus.drain().collect::<Vec<_>>(), vec![]);

        store.present.lock().expect("fixture store poisoned").retain(|id| *id != language.live.id);
        watcher.check(&state, &bus.sender());
        let events: Vec<_> = bus.drain().collect();
        assert_eq!(events, vec![AppEvent::ModelMissing(language.live)]);
        for event in events {
            state.apply(&event);
        }
        watcher.check(&state, &bus.sender());
        assert_eq!(bus.drain().collect::<Vec<_>>(), vec![]);
    }

    #[test]
    fn pending_and_inactive_models_are_not_checked() {
        let tmp = tempfile::tempdir().unwrap();
        let language = &LANGUAGES[0];
        let store = Arc::new(FixtureStore::new(tmp.path().to_path_buf(), &[]));
        let watcher = ModelWatcher::new(store);
        let state = UiView {
            blocks: vec![
                Block::new(1, BlockState::Picking(ListPicker::default())),
                Block::new(2, BlockState::Failed {
                    language,
                    error: "failed".to_string(),
                    pending: vec![],
                }),
                Block::new(3, BlockState::Waiting {
                    language,
                    pending: vec![language.refine.id],
                }),
            ],
            ..UiView::default()
        };
        let mut bus = EventBus::new();
        watcher.check(&state, &bus.sender());
        assert_eq!(bus.drain().collect::<Vec<_>>(), vec![AppEvent::ModelMissing(language.live)]);
    }
}
