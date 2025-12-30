use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppConfig {
    pub api_key: Option<String>,
}

impl AppConfig {
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
        log::info!("Config saved to: {}", path.display());
        Ok(())
    }

    /// Get the config file path
    fn config_path() -> Result<PathBuf> {
        let app_data = dirs::config_dir()
            .ok_or_else(|| anyhow::anyhow!("Could not find config directory"))?;
        Ok(app_data.join("VoiceBirdDesktop").join("config.json"))
    }

    /// Get the config directory path (for display purposes)
    pub fn config_dir() -> Option<PathBuf> {
        dirs::config_dir().map(|p| p.join("VoiceBirdDesktop"))
    }
}
