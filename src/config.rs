use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Lives in lib so both `platform::AudioSession` (in the bin crate) and
/// `AppConfig` (in the lib crate) can reference it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioSessionKind {
    Input,
    Output,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppConfig {
    pub default_model: String,
    pub language: String,
    pub session_dir: String,
    pub hop_ms: u32,
    pub min_window_ms: u32,
    #[serde(rename = "engine_prefer")]
    pub engine_prefer: String,
    pub audio_default_source: String,
    /// Device name chosen by the user. `None` = use the OS default
    /// input. Missing-from-config (old configs) deserializes to `None`.
    #[serde(default)]
    pub input_device: Option<String>,
    /// Kind of the saved device. `None` for old configs / default input.
    /// Lets `start_recording` pick the right capture path without having
    /// to re-enumerate and match by name first.
    #[serde(default)]
    pub input_device_kind: Option<AudioSessionKind>,
    /// Optional background refinement model. When set, a second whisper
    /// engine runs in parallel on wider non-overlapping windows with beam
    /// search and emits higher-quality segments that replace the streaming
    /// output in the UI. `None` disables refinement.
    #[serde(default)]
    pub refinement_model: Option<String>,
    /// Window length (ms) of audio fed to each refinement pass.
    #[serde(default = "default_refinement_window_ms")]
    pub refinement_window_ms: u32,
    /// Beam size for refinement. 1 = greedy (fastest, lowest quality).
    #[serde(default = "default_refinement_beam_size")]
    pub refinement_beam_size: u8,
    /// AssemblyAI API key. Stored in plaintext in config.toml — file
    /// permissions are the only protection. Empty string = unset.
    #[serde(default)]
    pub assemblyai_api_key: String,
}

fn default_refinement_window_ms() -> u32 {
    20_000
}

fn default_refinement_beam_size() -> u8 {
    5
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            default_model: "distil-small.en".into(),
            language: "en".into(),
            session_dir: "~/voice-bird/sessions".into(),
            hop_ms: 750,
            min_window_ms: 1000,
            engine_prefer: "auto".into(),
            audio_default_source: "microphone".into(),
            input_device: None,
            input_device_kind: None,
            refinement_model: None,
            refinement_window_ms: default_refinement_window_ms(),
            refinement_beam_size: default_refinement_beam_size(),
            assemblyai_api_key: String::new(),
        }
    }
}

impl AppConfig {
    pub fn config_path() -> anyhow::Result<PathBuf> {
        let base = dirs::config_dir().ok_or_else(|| anyhow::anyhow!("no config dir"))?;
        Ok(base.join("voice-bird").join("config.toml"))
    }

    pub fn load() -> anyhow::Result<Self> {
        let path = Self::config_path()?;
        if path.exists() {
            Self::load_from(&path)
        } else {
            Ok(Self::default())
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::config_path()?;
        self.save_to(&path)
    }

    pub fn load_from(path: &Path) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&s)?)
    }

    pub fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::write(path, toml::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn session_dir_expanded(&self) -> String {
        if let Some(rest) = self.session_dir.strip_prefix("~/") {
            if let Some(home) = dirs::home_dir() {
                return home.join(rest).to_string_lossy().into_owned();
            }
        }
        self.session_dir.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn defaults_when_file_missing() {
        let c = AppConfig::default();
        assert_eq!(c.default_model, "distil-small.en");
        assert_eq!(c.hop_ms, 750);
        assert_eq!(c.engine_prefer, "auto");
    }

    #[test]
    fn assemblyai_api_key_roundtrips_through_toml() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        let mut c = AppConfig::default();
        c.assemblyai_api_key = "sk-fake-12345".into();
        c.save_to(&path).unwrap();
        let loaded = AppConfig::load_from(&path).unwrap();
        assert_eq!(loaded.assemblyai_api_key, "sk-fake-12345");
    }

    #[test]
    fn missing_assemblyai_api_key_deserializes_to_empty_string() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        // Write an old-style config without the field.
        std::fs::write(
            &path,
            r#"
default_model = "distil-small.en"
language = "en"
session_dir = "~/voice-bird/sessions"
hop_ms = 750
min_window_ms = 1000
engine_prefer = "auto"
audio_default_source = "microphone"
refinement_window_ms = 20000
refinement_beam_size = 5
"#,
        )
        .unwrap();
        let loaded = AppConfig::load_from(&path).unwrap();
        assert_eq!(loaded.assemblyai_api_key, "");
    }

    #[test]
    fn roundtrip_through_toml() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        let c = AppConfig {
            default_model: "large-v3-turbo".into(),
            language: "auto".into(),
            session_dir: "~/foo".into(),
            hop_ms: 600,
            min_window_ms: 800,
            engine_prefer: "whisperkit".into(),
            audio_default_source: "system".into(),
            input_device: Some("MacBook Pro Microphone".into()),
            input_device_kind: Some(AudioSessionKind::Input),
            refinement_model: Some("large-v3-turbo".into()),
            refinement_window_ms: 20_000,
            refinement_beam_size: 5,
            assemblyai_api_key: "sk-test".into(),
        };
        c.save_to(&path).unwrap();
        let loaded = AppConfig::load_from(&path).unwrap();
        assert_eq!(loaded, c);
    }
}
