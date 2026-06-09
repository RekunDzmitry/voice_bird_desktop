use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone)]
pub struct ModelEntry {
    pub id: &'static str,
    pub size_mb: u32,
    pub language: &'static str,
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
                id: "distil-small.en", size_mb: 250, language: "en",
                gguf_url: "https://huggingface.co/distil-whisper/distil-small.en/resolve/main/ggml-distil-small.en.bin",
                gguf_sha256: "<FILL>",
                coreml_url: Some("https://huggingface.co/distil-whisper/distil-small.en/resolve/main/coreml-distil-small.en.zip"),
                coreml_sha256: Some("<FILL>"),
                is_default: true,
            },
            ModelEntry {
                id: "distil-large-v3", size_mb: 1_500, language: "multi",
                gguf_url: "https://huggingface.co/distil-whisper/distil-large-v3/resolve/main/ggml-distil-large-v3.bin",
                gguf_sha256: "<FILL>",
                coreml_url: Some("https://huggingface.co/distil-whisper/distil-large-v3/resolve/main/coreml-distil-large-v3.zip"),
                coreml_sha256: Some("<FILL>"),
                is_default: false,
            },
            ModelEntry {
                id: "large-v3-turbo", size_mb: 1_600, language: "multi",
                gguf_url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo.bin",
                gguf_sha256: "<FILL>",
                coreml_url: Some("https://huggingface.co/argmaxinc/whisperkit-coreml/resolve/main/openai_whisper-large-v3-turbo.zip"),
                coreml_sha256: Some("<FILL>"),
                is_default: false,
            },
            ModelEntry {
                id: "base.en", size_mb: 150, language: "en",
                gguf_url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin",
                gguf_sha256: "<FILL>",
                coreml_url: None, coreml_sha256: None,
                is_default: false,
            },
            ModelEntry {
                id: "tiny.en", size_mb: 75, language: "en",
                gguf_url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-tiny.en.bin",
                gguf_sha256: "<FILL>",
                coreml_url: None, coreml_sha256: None,
                is_default: false,
            },
        ])
    }

    pub fn get(&self, id: &str) -> Option<&ModelEntry> {
        self.0.iter().find(|m| m.id == id)
    }

    pub fn default_id(&self) -> &'static str {
        self.0
            .iter()
            .find(|m| m.is_default)
            .map(|m| m.id)
            .unwrap_or("distil-small.en")
    }

    pub fn all(&self) -> &[ModelEntry] {
        &self.0
    }
}

/// Reject combinations where a non-English language is requested but the
/// configured local Whisper model is English-only. Without this, users
/// selecting Russian/Polish/etc with a `.en` ggml file silently get
/// gibberish English output. Returns `Ok(())` for known good combinations
/// and for anything we can't verify (missing catalog entry, "auto" /
/// English / empty language).
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
            "Model '{}' is English-only; pick distil-large-v3 or large-v3-turbo for {}.",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_default_and_required_ids() {
        let catalog = Catalog::builtin();
        let default = catalog.default_id();
        assert_eq!(default, "distil-small.en");
        for id in [
            "distil-small.en",
            "distil-large-v3",
            "large-v3-turbo",
            "base.en",
            "tiny.en",
        ] {
            assert!(catalog.get(id).is_some(), "missing {id}");
        }
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
    }

    #[test]
    fn validate_local_language_passes_through_english_and_auto() {
        assert!(validate_local_language("tiny.en", "en").is_ok());
        assert!(validate_local_language("tiny.en", "auto").is_ok());
        assert!(validate_local_language("tiny.en", "").is_ok());
    }

    #[test]
    fn validate_local_language_rejects_unknown_model_id() {
        let err = validate_local_language("custom-user-model", "ru").unwrap_err();
        assert!(err.contains("custom-user-model"));
        assert!(err.contains("not supported"));
    }
}
