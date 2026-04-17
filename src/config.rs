use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// Application configuration stored in user's config directory
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppConfig {
    pub api_key: Option<String>,
    pub server_url: Option<String>,
}

impl AppConfig {
    /// Default Voice Bird server URL
    pub const DEFAULT_SERVER_URL: &'static str = "https://voicebird.app";

    /// Load config from file, or return default if not found
    pub fn load() -> Result<Self> {
        let path = Self::config_path()?;
        if path.exists() {
            let contents = fs::read_to_string(&path)?;
            Ok(serde_json::from_str(&contents)?)
        } else {
            Ok(Self::default())
        }
    }

    /// Save config to file
    pub fn save(&self) -> Result<()> {
        let path = Self::config_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Get the config file path (shared with desktop app)
    fn config_path() -> Result<PathBuf> {
        let app_data = dirs::config_dir()
            .ok_or_else(|| anyhow::anyhow!("Could not find config directory"))?;
        Ok(app_data.join("VoiceBirdDesktop").join("config.json"))
    }

    /// Get the server URL (with fallback to default)
    pub fn server_url(&self) -> String {
        self.server_url
            .clone()
            .unwrap_or_else(|| Self::DEFAULT_SERVER_URL.to_string())
    }

    /// Check if API key is configured
    pub fn has_api_key(&self) -> bool {
        self.api_key.as_ref().map(|k| !k.is_empty()).unwrap_or(false)
    }
}
