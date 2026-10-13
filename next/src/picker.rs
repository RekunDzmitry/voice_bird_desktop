//! Model catalog and language picker.
//!
//! Pure data + a small reducer. No `KeyCode`, no terminal, no I/O. The
//! download catalog remains model-oriented, while the picker exposes only
//! [`crate::language::LanguageProfile`] rows.
//!
//! The catalog is defined as named constants plus a stable slice. Language
//! profiles refer to the constants directly, so adding a language cannot
//! silently misspell a model id.

/// On-disk format for one catalog row. New formats add a variant and a
/// handler in `transcription_models`; the rest of the pipeline stays format-agnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum ModelFormat {
    /// A single Whisper GGUF `.bin` file.
    WhisperGguf,
    /// A gzipped tarball containing an ONNX encoder + decoder pair.
    NemotronPackage,
    /// A ZIP containing a compiled Core ML Whisper encoder.
    CoreMlEncoder,
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

/// Whisper decoder used by the live pass. Native encoder assets have separate
/// catalog rows so cached decoder weights never need downloading again.
pub const DISTIL_SMALL_EN: ModelEntry = ModelEntry {
    id: "distil-small.en",
    size_mb: 250,
    language: "en",
    format: ModelFormat::WhisperGguf,
    download_url: "https://huggingface.co/distil-whisper/distil-small.en/resolve/main/ggml-distil-small.en.bin",
    download_sha256: "7691eb11167ab7aaf6b3e05d8266f2fd9ad89c550e433f86ac266ebdee6c970a",
};

pub const DISTIL_LARGE_V3: ModelEntry = ModelEntry {
    id: "distil-large-v3",
    size_mb: 1_500,
    language: "multi",
    format: ModelFormat::WhisperGguf,
    download_url: "https://huggingface.co/distil-whisper/distil-large-v3-ggml/resolve/main/ggml-distil-large-v3.bin",
    download_sha256: "2883a11b90fb10ed592d826edeaee7d2929bf1ab985109fe9e1e7b4d2b69a298",
};

pub const LARGE_V3_TURBO: ModelEntry = ModelEntry {
    id: "large-v3-turbo",
    size_mb: 1_600,
    language: "multi",
    format: ModelFormat::WhisperGguf,
    download_url:
        "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo.bin",
    download_sha256: "1fc70f774d38eb169993ac391eea357ef47c88757ef72ee5943879b7e8e2bc69",
};

pub const NEMOTRON_STREAMING: ModelEntry = ModelEntry {
    id: "nemotron-3.5-asr-streaming-0.6b",
    size_mb: 740,
    language: "multi",
    format: ModelFormat::NemotronPackage,
    download_url: "https://huggingface.co/smcleod/nemotron-3.5-asr-streaming-0.6b-int8/resolve/main/nemotron-3.5-asr-streaming-0.6b-int8.tar.gz",
    download_sha256: "d1d57d86212528fa03dfdbb88979f1dd637814dec6db31257a603739c73bd9d2",
};

pub const BASE_EN: ModelEntry = ModelEntry {
    id: "base.en",
    size_mb: 150,
    language: "en",
    format: ModelFormat::WhisperGguf,
    download_url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin",
    download_sha256: "a03779c86df3323075f5e796cb2ce5029f00ec8869eee3fdfb897afe36c6d002",
};

pub const TINY_EN: ModelEntry = ModelEntry {
    id: "tiny.en",
    size_mb: 75,
    language: "en",
    format: ModelFormat::WhisperGguf,
    download_url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-tiny.en.bin",
    download_sha256: "921e4cf8686fdd993dcd081a5da5b6c365bfde1162e72b08d75ac75289920b1f",
};

/// The student retains the complete, frozen small.en teacher encoder:
/// https://huggingface.co/distil-whisper/distil-small.en (teacher identity)
/// https://huggingface.co/papers/2311.00430 (§7.2 and Figure 1: copied/frozen).
/// Only the encoder is shared; the distil-small.en decoder is unchanged.
pub const DISTIL_SMALL_EN_ENCODER: ModelEntry = ModelEntry {
    id: "distil-small.en-encoder",
    size_mb: 163,
    language: "en",
    format: ModelFormat::CoreMlEncoder,
    download_url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.en-encoder.mlmodelc.zip",
    download_sha256: "b2ef1c506378b825b4b4341979a93e1656b5d6c129f17114cfb8fb78aabc2f89",
};

pub const LARGE_V3_TURBO_ENCODER: ModelEntry = ModelEntry {
    id: "large-v3-turbo-encoder",
    size_mb: 1_174,
    language: "multi",
    format: ModelFormat::CoreMlEncoder,
    download_url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo-encoder.mlmodelc.zip",
    download_sha256: "84bedfe895bd7b5de6e8e89a0803dfc5addf8c0c5bc4c937451716bf7cf7988a",
};

pub const CATALOG: &[ModelEntry] = &[
    DISTIL_SMALL_EN,
    DISTIL_LARGE_V3,
    LARGE_V3_TURBO,
    NEMOTRON_STREAMING,
    BASE_EN,
    TINY_EN,
    DISTIL_SMALL_EN_ENCODER,
    LARGE_V3_TURBO_ENCODER,
];

/// Direction the picker moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum PickerMove {
    Up,
    Down,
}

pub(crate) fn step(index: usize, len: usize, direction: PickerMove) -> usize {
    let last = len.saturating_sub(1);
    match direction {
        PickerMove::Up => index.saturating_sub(1).min(last),
        PickerMove::Down => index.saturating_add(1).min(last),
    }
}

/// Selection state for a caller-owned list, with saturating movement.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListPicker {
    pub index: usize,
}

impl ListPicker {
    pub fn apply(&mut self, direction: PickerMove, len: usize) {
        self.index = step(self.index, len, direction);
    }
}

/// Events the picker's own reducer folds.
pub enum PickerEvent {
    Moved(PickerMove),
    Picked,
    Cancelled,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_picker_saturates_at_both_ends() {
        let mut picker = ListPicker { index: 0 };
        picker.apply(PickerMove::Up, 3);
        assert_eq!(picker.index, 0);
        for _ in 0..5 {
            picker.apply(PickerMove::Down, 3);
        }
        assert_eq!(picker.index, 2);
        for _ in 0..5 {
            picker.apply(PickerMove::Up, 3);
        }
        assert_eq!(picker.index, 0);
    }

    #[test]
    fn list_picker_moves_down_then_up() {
        let mut picker = ListPicker { index: 1 };
        picker.apply(PickerMove::Down, 3);
        assert_eq!(picker.index, 2);
        picker.apply(PickerMove::Up, 3);
        assert_eq!(picker.index, 1);
    }

    #[test]
    fn list_picker_clamps_empty_and_shrinking_lists() {
        for direction in [PickerMove::Up, PickerMove::Down] {
            let mut picker = ListPicker { index: usize::MAX };
            picker.apply(direction, 0);
            assert_eq!(picker.index, 0);

            picker.index = 4;
            picker.apply(direction, 3);
            assert_eq!(picker.index, 2);
        }
    }

    #[test]
    fn catalog_rows_carry_urls_and_shas() {
        for entry in CATALOG {
            assert!(!entry.download_url.is_empty(), "empty url on {}", entry.id);
            assert!(
                !entry.download_sha256.is_empty(),
                "empty sha on {}",
                entry.id
            );
        }
    }

    #[test]
    fn sha256_fields_are_64_lowercase_hex() {
        for entry in CATALOG {
            assert_eq!(
                entry.download_sha256.len(),
                64,
                "sha not 64 chars on {}",
                entry.id
            );
            assert!(
                entry
                    .download_sha256
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
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
}
