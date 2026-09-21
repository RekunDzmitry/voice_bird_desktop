//! Model catalog and picker.
//!
//! Pure data + a small reducer. No `KeyCode`, no terminal, no I/O. The bus
//! only ever sees resolved [`ModelEntry`]s — the key-to-entry resolver
//! lives in `main.rs`, not here.
//!
//! The catalog is defined inline as a `&'static [ModelEntry]` constant —
//! no dependency on `voice-bird-cli`, no dynamic loading. A future PR can
//! swap the source behind [`ModelPicker::catalog`] without changing the
//! picker API or the reducer.

/// On-disk format for one catalog row. New formats add a variant and a
/// handler in `transcription_models`; the rest of the pipeline stays format-agnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum ModelFormat {
    /// A single Whisper GGUF `.bin` file.
    WhisperGguf,
    /// A gzipped tarball containing an ONNX encoder + decoder pair.
    NemotronPackage,
}

/// One row of the catalog.
///
/// `download_url` / `download_sha256` carry the bytes-side metadata; the
/// `#[serde(skip_dump))]` keeps the on-disk event log free of the
/// 100-char HF URL — a reader of the JSONL wants the variant tag and a
/// stable model id, not the artifact location.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct ModelEntry {
    pub id: &'static str,
    pub size_mb: u32,
    pub language: &'static str,
    pub format: ModelFormat,
    #[serde(skip_serializing)]
    pub download_url: &'static str,
    #[serde(skip_serializing)]
    pub download_sha256: &'static str,
}

/// Inline copy of the six built-in SST models. Order matches the legacy
/// `voice_bird_cli::Catalog::builtin()` ordering; tests pin this list.
pub const CATALOG: &[ModelEntry] = &[
    ModelEntry {
        id: "distil-small.en",
        size_mb: 250,
        language: "en",
        format: ModelFormat::WhisperGguf,
        download_url: "https://huggingface.co/distil-whisper/distil-small.en/resolve/main/ggml-distil-small.en.bin",
        download_sha256: "7691eb11167ab7aaf6b3e05d8266f2fd9ad89c550e433f86ac266ebdee6c970a",
    },
    ModelEntry {
        id: "distil-large-v3",
        size_mb: 1_500,
        language: "multi",
        format: ModelFormat::WhisperGguf,
        download_url: "https://huggingface.co/distil-whisper/distil-large-v3-ggml/resolve/main/ggml-distil-large-v3.bin",
        download_sha256: "2883a11b90fb10ed592d826edeaee7d2929bf1ab985109fe9e1e7b4d2b69a298",
    },
    ModelEntry {
        id: "large-v3-turbo",
        size_mb: 1_600,
        language: "multi",
        format: ModelFormat::WhisperGguf,
        download_url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo.bin",
        download_sha256: "1fc70f774d38eb169993ac391eea357ef47c88757ef72ee5943879b7e8e2bc69",
    },
    ModelEntry {
        id: "nemotron-3.5-asr-streaming-0.6b",
        size_mb: 740,
        language: "multi",
        format: ModelFormat::NemotronPackage,
        download_url: "https://huggingface.co/smcleod/nemotron-3.5-asr-streaming-0.6b-int8/resolve/main/nemotron-3.5-asr-streaming-0.6b-int8.tar.gz",
        download_sha256: "d1d57d86212528fa03dfdbb88979f1dd637814dec6db31257a603739c73bd9d2",
    },
    ModelEntry {
        id: "base.en",
        size_mb: 150,
        language: "en",
        format: ModelFormat::WhisperGguf,
        download_url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin",
        download_sha256: "a03779c86df3323075f5e796cb2ce5029f00ec8869eee3fdfb897afe36c6d002",
    },
    ModelEntry {
        id: "tiny.en",
        size_mb: 75,
        language: "en",
        format: ModelFormat::WhisperGguf,
        download_url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-tiny.en.bin",
        download_sha256: "921e4cf8686fdd993dcd081a5da5b6c365bfde1162e72b08d75ac75289920b1f",
    },
];

/// Why the picker was opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerIntent {
    AddBlock,
}

/// Direction the picker moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum PickerMove {
    Up,
    Down,
}

/// Events the picker's own reducer folds.
pub enum PickerEvent {
    Moved(PickerMove),
    Picked,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelPicker {
    pub index: usize,
    intent: PickerIntent,
}

impl ModelPicker {
    pub fn open(intent: PickerIntent) -> Self {
        Self { index: 0, intent }
    }

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

    /// Index the picker WOULD land on after applying `direction`,
    /// without mutating. The resolver uses this to stamp `from_model`
    /// and `to_model` on `AppEvent::PickerMoved` so the event log
    /// records the visible move, not just the key. Saturation
    /// matches `apply`'s clamping: at the top, Up returns 0; at the
    /// bottom, Down returns the last index. Wrapping either direction
    /// would attribute moves to non-moving rows.
    pub fn peek_next(&self, direction: PickerMove) -> usize {
        match direction {
            PickerMove::Up => self.index.saturating_sub(1),
            PickerMove::Down => {
                if self.index + 1 < CATALOG.len() {
                    self.index + 1
                } else {
                    self.index
                }
            }
        }
    }

    pub fn catalog(&self) -> &'static [ModelEntry] {
        CATALOG
    }

    pub fn intent(&self) -> PickerIntent {
        self.intent
    }
}

/// Left-hand session menu. Mirrors [`ModelPicker`]'s reducer shape
/// (saturating at both ends, never wraps) and reuses [`PickerMove`]
/// for its direction events so the clamp semantics live in one place.
///
/// `index` is a position into [`crate::state::UiState::blocks`] — the
/// menu lists *all* sessions in creation order, not just the visible
/// ones, so the user can reach a hidden block through it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMenu {
    /// Position inside `UiState.blocks`. The reducer's `show_block`
    /// helper looks the id up by index, so a stale position simply
    /// misses and the resolver no-ops — no panics on a race.
    pub index: usize,
}

impl SessionMenu {
    /// Open the menu at the requested row. The caller owns the bounds
    /// check on `blocks`; the reducer's `apply` arm for `MenuOpened`
    /// passes `state.focus` (clamped to `len`).
    pub fn open_at(index: usize) -> Self {
        Self { index }
    }

    /// Apply a move event. Saturates at both ends; never wraps. The
    /// menu's `index` is relative to the full session list (not the
    /// visible subset), so the clamp lives at the menu layer rather
    /// than at the resolver.
    pub fn apply(&mut self, event: PickerEvent, len: usize) {
        if len == 0 {
            // No rows: keep `index` at zero so the next opening of
            // the menu starts there. Nothing renders anyway.
            self.index = 0;
            return;
        }
        let last = len - 1;
        match event {
            PickerEvent::Moved(PickerMove::Up) => {
                self.index = self.index.saturating_sub(1);
            }
            PickerEvent::Moved(PickerMove::Down) => {
                if self.index < last {
                    self.index += 1;
                }
            }
            // Picked / Cancelled do not move the highlight.
            PickerEvent::Picked | PickerEvent::Cancelled => {}
        }
        // Belt-and-braces: if the caller fed a `len` smaller than
        // `index` (e.g. the focused block was removed while the menu
        // was open), clamp back inside.
        self.index = self.index.min(last);
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

    #[test]
    fn apply_picked_after_close_is_safe() {
        let mut p = ModelPicker::open(PickerIntent::AddBlock);
        let first = p.apply(PickerEvent::Picked).unwrap();
        let second = p.apply(PickerEvent::Picked).unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(first.id, CATALOG[0].id);
    }

    #[test]
    fn catalog_rows_carry_urls_and_shas() {
        for entry in CATALOG {
            assert!(!entry.download_url.is_empty(), "empty url on {}", entry.id);
            assert!(!entry.download_sha256.is_empty(), "empty sha on {}", entry.id);
        }
    }

    #[test]
    fn sha256_fields_are_64_lowercase_hex() {
        for entry in CATALOG {
            assert_eq!(entry.download_sha256.len(), 64, "sha not 64 chars on {}", entry.id);
            assert!(
                entry.download_sha256.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                "sha not lowercase hex on {}",
                entry.id
            );
        }
    }

    #[test]
    fn every_row_has_a_nonempty_url() {
        for entry in CATALOG {
            assert!(entry.download_url.starts_with("https://"), "{}", entry.id);
        }
    }

    #[test]
    fn session_menu_open_at_sets_initial_index() {
        let menu = SessionMenu::open_at(2);
        assert_eq!(menu.index, 2);
    }

    #[test]
    fn session_menu_apply_up_saturates_at_zero() {
        let mut menu = SessionMenu::open_at(0);
        menu.apply(PickerEvent::Moved(PickerMove::Up), 4);
        assert_eq!(menu.index, 0);
    }

    #[test]
    fn session_menu_apply_down_clamps_at_last() {
        let mut menu = SessionMenu::open_at(3);
        menu.apply(PickerEvent::Moved(PickerMove::Down), 4);
        assert_eq!(menu.index, 3);
    }

    #[test]
    fn session_menu_apply_down_then_up_returns_to_previous() {
        let mut menu = SessionMenu::open_at(1);
        menu.apply(PickerEvent::Moved(PickerMove::Down), 5);
        assert_eq!(menu.index, 2);
        menu.apply(PickerEvent::Moved(PickerMove::Up), 5);
        assert_eq!(menu.index, 1);
    }

    #[test]
    fn session_menu_apply_picked_or_cancelled_does_not_move_highlight() {
        let mut menu = SessionMenu::open_at(2);
        menu.apply(PickerEvent::Picked, 5);
        assert_eq!(menu.index, 2);
        menu.apply(PickerEvent::Cancelled, 5);
        assert_eq!(menu.index, 2);
    }

    #[test]
    fn session_menu_with_zero_rows_keeps_index_at_zero() {
        let mut menu = SessionMenu::open_at(7);
        menu.apply(PickerEvent::Moved(PickerMove::Down), 0);
        assert_eq!(menu.index, 0);
    }

    #[test]
    fn session_menu_clamps_a_stale_index_when_len_shrinks() {
        // Race: a block closed while the menu was open, and the index
        // was on the last row. The next move must land on the new last
        // row, not panic on an out-of-bounds access.
        let mut menu = SessionMenu::open_at(4);
        menu.apply(PickerEvent::Moved(PickerMove::Down), 3);
        assert_eq!(menu.index, 2);
    }
}