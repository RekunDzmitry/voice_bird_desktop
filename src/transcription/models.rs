use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};
use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use tar::Archive;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelFormat {
    WhisperGguf,
    NemotronPackage,
}

#[derive(Debug, Clone)]
pub struct ModelEntry {
    pub id: &'static str,
    pub size_mb: u32,
    pub language: &'static str,
    pub format: ModelFormat,
    pub gguf_url: &'static str,
    pub gguf_sha256: &'static str,
    pub coreml_url: Option<&'static str>,
    pub coreml_sha256: Option<&'static str>,
    pub is_default: bool,
}

pub struct Catalog(Vec<ModelEntry>);

impl Catalog {
    pub fn builtin() -> Self {
        Catalog(vec![
            ModelEntry {
                id: "distil-small.en",
                size_mb: 250,
                language: "en",
                format: ModelFormat::WhisperGguf,
                gguf_url: "https://huggingface.co/distil-whisper/distil-small.en/resolve/main/ggml-distil-small.en.bin",
                gguf_sha256: "<FILL>",
                coreml_url: Some("https://huggingface.co/distil-whisper/distil-small.en/resolve/main/ggml-distil-small.en-encoder.mlmodelc.zip"),
                coreml_sha256: Some("<FILL>"),
                is_default: true,
            },
            ModelEntry {
                id: "distil-large-v3",
                size_mb: 1_500,
                language: "multi",
                format: ModelFormat::WhisperGguf,
                gguf_url: "https://huggingface.co/distil-whisper/distil-large-v3/resolve/main/ggml-distil-large-v3.bin",
                gguf_sha256: "<FILL>",
                coreml_url: None,
                coreml_sha256: None,
                is_default: false,
            },
            ModelEntry {
                id: "large-v3-turbo",
                size_mb: 1_600,
                language: "multi",
                format: ModelFormat::WhisperGguf,
                gguf_url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo.bin",
                gguf_sha256: "<FILL>",
                coreml_url: Some("https://huggingface.co/argmaxinc/whisperkit-coreml/resolve/main/openai_whisper-large-v3-turbo.zip"),
                coreml_sha256: Some("<FILL>"),
                is_default: false,
            },
            ModelEntry {
                id: "nemotron-3.5-asr-streaming-0.6b",
                size_mb: 740,
                language: "multi",
                format: ModelFormat::NemotronPackage,
                gguf_url: "https://huggingface.co/smcleod/nemotron-3.5-asr-streaming-0.6b-int8/resolve/main/nemotron-3.5-asr-streaming-0.6b-int8.tar.gz",
                gguf_sha256: "<FILL>",
                coreml_url: None,
                coreml_sha256: None,
                is_default: false,
            },
            ModelEntry {
                id: "base.en",
                size_mb: 150,
                language: "en",
                format: ModelFormat::WhisperGguf,
                gguf_url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin",
                gguf_sha256: "<FILL>",
                coreml_url: None,
                coreml_sha256: None,
                is_default: false,
            },
            ModelEntry {
                id: "tiny.en",
                size_mb: 75,
                language: "en",
                format: ModelFormat::WhisperGguf,
                gguf_url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-tiny.en.bin",
                gguf_sha256: "<FILL>",
                coreml_url: None,
                coreml_sha256: None,
                is_default: false,
            },
        ])
    }

    pub fn all(&self) -> &[ModelEntry] {
        &self.0
    }

    pub fn get(&self, id: &str) -> Option<&ModelEntry> {
        self.0.iter().find(|m| m.id == id)
    }
}

pub fn validate_local_language(model_id: &str, language: &str) -> Result<(), String> {
    let lang = language.trim();
    if lang.is_empty() || lang == "en" || lang == "auto" {
        return Ok(());
    }

    let catalog = Catalog::builtin();
    let Some(entry) = catalog.get(model_id) else {
        return Err(format!(
            "Model '{model_id}' is not supported by this release; pick one from the model picker."
        ));
    };

    if entry.language == "en" {
        return Err(format!(
            "Model '{}' is English-only; pick distil-large-v3, large-v3-turbo, or nemotron-3.5-asr-streaming-0.6b for {}.",
            entry.id, lang
        ));
    }

    Ok(())
}

pub fn cache_dir() -> anyhow::Result<PathBuf> {
    let base = dirs::cache_dir().ok_or_else(|| anyhow!("no cache dir"))?;
    Ok(base.join("voice-bird").join("models"))
}

pub fn gguf_path(id: &str) -> anyhow::Result<PathBuf> {
    Ok(cache_dir()?.join(format!("{id}.gguf")))
}

pub fn nemotron_model_dir(id: &str) -> anyhow::Result<PathBuf> {
    Ok(cache_dir()?.join(id))
}

pub fn model_path(id: &str) -> anyhow::Result<PathBuf> {
    let catalog = Catalog::builtin();
    match catalog.get(id).map(|m| m.format) {
        Some(ModelFormat::NemotronPackage) => nemotron_model_dir(id),
        _ => gguf_path(id),
    }
}

pub fn is_nemotron_model(id: &str) -> bool {
    let catalog = Catalog::builtin();
    matches!(
        catalog.get(id).map(|m| m.format),
        Some(ModelFormat::NemotronPackage)
    )
}

pub fn verify_sha256(path: &Path, expected_hex: &str) -> anyhow::Result<()> {
    let data = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let mut h = Sha256::new();
    h.update(&data);
    let got = hex::encode(h.finalize());
    if got != expected_hex {
        return Err(anyhow!(
            "sha256 mismatch for {}: got {} expected {}",
            path.display(),
            got,
            expected_hex
        ));
    }
    Ok(())
}

pub fn download_with_verify(
    url: &str,
    dest: &Path,
    expected_sha: &str,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> anyhow::Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let resp = reqwest::blocking::get(url)?.error_for_status()?;
    let total = resp.content_length();
    let mut downloaded = 0u64;
    let mut out = std::fs::File::create(dest)?;
    let mut src = resp;
    let mut buf = [0u8; 1 << 16];
    loop {
        let n = std::io::Read::read(&mut src, &mut buf)?;
        if n == 0 {
            break;
        }
        std::io::Write::write_all(&mut out, &buf[..n])?;
        downloaded += n as u64;
        progress(downloaded, total);
    }
    drop(out);
    if !expected_sha.starts_with("<FILL") {
        verify_sha256(dest, expected_sha)?;
    }
    Ok(())
}

pub fn download_model_with_verify(
    entry: &ModelEntry,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> anyhow::Result<()> {
    match entry.format {
        ModelFormat::WhisperGguf => download_with_verify(
            entry.gguf_url,
            &gguf_path(entry.id)?,
            entry.gguf_sha256,
            progress,
        ),
        ModelFormat::NemotronPackage => {
            let archive_path = cache_dir()?.join(format!("{}.tar.gz", entry.id));
            download_with_verify(entry.gguf_url, &archive_path, entry.gguf_sha256, progress)?;
            progress(0, None);
            unpack_nemotron_archive(&archive_path, &nemotron_model_dir(entry.id)?)?;
            progress(1, Some(1));
            Ok(())
        }
    }
}

fn unpack_nemotron_archive(archive_path: &Path, dest_dir: &Path) -> anyhow::Result<()> {
    let tmp_dir = dest_dir.with_extension("tmp");
    if tmp_dir.exists() {
        std::fs::remove_dir_all(&tmp_dir)?;
    }
    if dest_dir.exists() {
        std::fs::remove_dir_all(dest_dir)?;
    }
    std::fs::create_dir_all(&tmp_dir)?;

    let archive = std::fs::File::open(archive_path)?;
    let decoder = GzDecoder::new(archive);
    Archive::new(decoder).unpack(&tmp_dir)?;

    let model_dir = locate_nemotron_dir(&tmp_dir).ok_or_else(|| {
        anyhow!("Nemotron package did not contain encoder.onnx and decoder_joint.onnx")
    })?;
    if let Some(parent) = dest_dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(model_dir, dest_dir)?;
    let _ = std::fs::remove_dir_all(tmp_dir);
    Ok(())
}

fn locate_nemotron_dir(root: &Path) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if dir.join("encoder.onnx").exists() && dir.join("decoder_joint.onnx").exists() {
            return Some(dir);
        }
        let entries = std::fs::read_dir(&dir).ok()?;
        for entry in entries.flatten() {
            if entry.file_type().ok()?.is_dir() {
                stack.push(entry.path());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_default_and_required_ids() {
        let catalog = Catalog::builtin();
        assert!(catalog.all().iter().any(|m| m.is_default));
        assert!(catalog.get("distil-small.en").is_some());
        assert!(catalog.get("large-v3-turbo").is_some());
        assert!(catalog.get("nemotron-3.5-asr-streaming-0.6b").is_some());
    }

    #[test]
    fn nemotron_uses_directory_model_path() {
        let path = model_path("nemotron-3.5-asr-streaming-0.6b").unwrap();
        assert!(path.ends_with("nemotron-3.5-asr-streaming-0.6b"));
        assert!(is_nemotron_model("nemotron-3.5-asr-streaming-0.6b"));
    }

    #[test]
    fn locate_nemotron_dir_uses_parakeet_rs_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("nested").join("model");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("encoder.onnx"), b"encoder").unwrap();
        std::fs::write(nested.join("decoder_joint.onnx"), b"decoder").unwrap();

        assert_eq!(locate_nemotron_dir(tmp.path()).unwrap(), nested);
    }

    #[test]
    fn sha256_verify_detects_mismatch() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"hello").unwrap();
        let wrong = "0".repeat(64);
        assert!(verify_sha256(tmp.path(), &wrong).is_err());
    }

    #[test]
    fn validate_local_language_rejects_english_model_for_russian() {
        let err = validate_local_language("tiny.en", "ru").unwrap_err();
        assert!(err.contains("tiny.en"));
        assert!(err.contains("ru"));
    }

    #[test]
    fn validate_local_language_accepts_multilingual_for_russian() {
        assert!(validate_local_language("distil-large-v3", "ru").is_ok());
        assert!(validate_local_language("large-v3-turbo", "pl").is_ok());
        assert!(validate_local_language("nemotron-3.5-asr-streaming-0.6b", "pl").is_ok());
    }

    #[test]
    fn validate_local_language_passes_through_english_and_auto() {
        assert!(validate_local_language("tiny.en", "en").is_ok());
        assert!(validate_local_language("tiny.en", "auto").is_ok());
    }

    #[test]
    fn validate_local_language_rejects_unknown_model_id() {
        let err = validate_local_language("custom-user-model", "ru").unwrap_err();
        assert!(err.contains("custom-user-model"));
        assert!(err.contains("not supported"));
    }
}
