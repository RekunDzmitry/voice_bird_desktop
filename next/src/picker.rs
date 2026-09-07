//! Model-picker overlay.
//!
//! Pure data + a small reducer. No `KeyCode`, no terminal, no I/O. The bus
//! only ever sees resolved [`ModelEntry`]s — the key-to-entry resolver
//! lives in `main.rs`, not here.
//!
//! The catalog is defined inline as a `&'static [ModelEntry]` constant —
//! no dependency on `voice-bird-cli`, no dynamic loading. A future PR can
//! swap the source behind [`ModelPicker::catalog`] without changing the
//! picker API or the reducer.

/// One row of the catalog: the model id (matches the snapshot test label),
/// its on-disk size in megabytes, and the language tag rendered in the row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelEntry {
    pub id: &'static str,
    pub size_mb: u32,
    pub language: &'static str,
}

/// Inline copy of the six built-in SST models. Order matches the legacy
/// `voice_bird_cli::Catalog::builtin()` ordering; tests pin this list.
pub const CATALOG: &[ModelEntry] = &[
    ModelEntry {
        id: "distil-small.en",
        size_mb: 250,
        language: "en",
    },
    ModelEntry {
        id: "distil-large-v3",
        size_mb: 1500,
        language: "multi",
    },
    ModelEntry {
        id: "large-v3-turbo",
        size_mb: 1600,
        language: "multi",
    },
    ModelEntry {
        id: "nemotron-3.5-asr-streaming-0.6b",
        size_mb: 740,
        language: "multi",
    },
    ModelEntry {
        id: "base.en",
        size_mb: 150,
        language: "en",
    },
    ModelEntry {
        id: "tiny.en",
        size_mb: 75,
        language: "en",
    },
];

/// Why the picker was opened. Today only `AddBlock`; future flows
/// (`ChangeModel`, `AddRefinement`) extend the enum without touching the
/// reducer's other arms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerIntent {
    AddBlock,
}

/// Direction the picker moved. Key-agnostic on purpose: `Up`/`Down` come
/// from `j`/`k`/`↑`/`↓` at the input layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerMove {
    Up,
    Down,
}

/// Events the picker's own reducer folds. The bus carries the resolved
/// variants (`PickerMoved`, `ModelSelected`, `PickerCancelled`); `PickerEvent`
/// is the internal vocabulary.
pub enum PickerEvent {
    Moved(PickerMove),
    Picked,
    Cancelled,
}

/// State of the open picker. Plain data; the reducer in `UiState::apply`
/// drives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelPicker {
    pub index: usize,
    intent: PickerIntent,
}

impl ModelPicker {
    /// Open the picker for a given intent. Index starts at zero — the
    /// first row of [`CATALOG`] is the highlighted default.
    pub fn open(intent: PickerIntent) -> Self {
        Self { index: 0, intent }
    }

    /// Fold one [`PickerEvent`]. Returns `Some(&'static ModelEntry)` only
    /// on `Picked`; the caller (the main loop) is expected to publish
    /// `AppEvent::ModelSelected(entry)` so the bus sees the resolved entry.
    pub fn apply(&mut self, event: PickerEvent) -> Option<&'static ModelEntry> {
        match event {
            PickerEvent::Moved(PickerMove::Up) => {
                if self.index > 0 {
                    self.index -= 1;
                }
                None
            }
            PickerEvent::Moved(PickerMove::Down) => {
                if self.index + 1 < CATALOG.len() {
                    self.index += 1;
                }
                None
            }
            PickerEvent::Picked => CATALOG.get(self.index),
            PickerEvent::Cancelled => None,
        }
    }

    /// Title shown in the picker's top border.
    pub fn title(&self) -> &'static str {
        "Pick a model (Esc to cancel)"
    }

    /// Catalog rendered by the picker.
    pub fn catalog(&self) -> &'static [ModelEntry] {
        CATALOG
    }

    /// Intent the picker was opened for. Used by the UI to choose labels;
    /// the reducer already branched on it when storing the picker.
    pub fn intent(&self) -> PickerIntent {
        self.intent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_contains_expected_six_entries() {
        assert_eq!(CATALOG.len(), 6);
        assert_eq!(CATALOG[0].id, "distil-small.en");
        assert_eq!(CATALOG[1].id, "distil-large-v3");
        assert_eq!(CATALOG[2].id, "large-v3-turbo");
        assert_eq!(CATALOG[3].id, "nemotron-3.5-asr-streaming-0.6b");
        assert_eq!(CATALOG[4].id, "base.en");
        assert_eq!(CATALOG[5].id, "tiny.en");
    }

    #[test]
    fn open_starts_at_zero() {
        let p = ModelPicker::open(PickerIntent::AddBlock);
        assert_eq!(p.index, 0);
        assert_eq!(p.intent(), PickerIntent::AddBlock);
    }

    #[test]
    fn apply_moved_up_at_zero_stays_at_zero() {
        let mut p = ModelPicker::open(PickerIntent::AddBlock);
        assert!(p.apply(PickerEvent::Moved(PickerMove::Up)).is_none());
        assert_eq!(p.index, 0);
    }

    #[test]
    fn apply_moved_down_at_last_clamps() {
        let mut p = ModelPicker::open(PickerIntent::AddBlock);
        for _ in 0..(CATALOG.len() + 5) {
            assert!(p.apply(PickerEvent::Moved(PickerMove::Down)).is_none());
        }
        assert_eq!(p.index, CATALOG.len() - 1);
    }

    #[test]
    fn apply_moved_down_then_up_returns_to_previous() {
        let mut p = ModelPicker::open(PickerIntent::AddBlock);
        p.apply(PickerEvent::Moved(PickerMove::Down));
        p.apply(PickerEvent::Moved(PickerMove::Down));
        assert_eq!(p.index, 2);
        p.apply(PickerEvent::Moved(PickerMove::Up));
        assert_eq!(p.index, 1);
    }

    #[test]
    fn apply_picked_returns_catalog_entry() {
        let mut p = ModelPicker::open(PickerIntent::AddBlock);
        p.apply(PickerEvent::Moved(PickerMove::Down));
        p.apply(PickerEvent::Moved(PickerMove::Down));
        let entry = p.apply(PickerEvent::Picked).expect("picked");
        assert_eq!(entry.id, CATALOG[2].id);
    }

    #[test]
    fn apply_cancelled_returns_none() {
        let mut p = ModelPicker::open(PickerIntent::AddBlock);
        assert!(p.apply(PickerEvent::Cancelled).is_none());
    }

    /// A picked event after a closed picker is structurally impossible —
    /// the reducer never calls `apply(Picked)` once `picker = None` —
    /// but the picker itself must remain safe regardless of caller state.
    #[test]
    fn apply_picked_after_close_is_safe() {
        // We can't make the picker forget its intent, but we can confirm
        // repeated picks return the same entry deterministically.
        let mut p = ModelPicker::open(PickerIntent::AddBlock);
        let first = p.apply(PickerEvent::Picked).unwrap();
        let second = p.apply(PickerEvent::Picked).unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(first.id, CATALOG[0].id);
    }

    #[test]
    fn title_and_catalog_accessors() {
        let p = ModelPicker::open(PickerIntent::AddBlock);
        assert_eq!(p.title(), "Pick a model (Esc to cancel)");
        assert_eq!(p.catalog().len(), CATALOG.len());
    }
}