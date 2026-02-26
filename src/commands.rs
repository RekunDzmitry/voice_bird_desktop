use serde::{Deserialize, Serialize};
use tauri::{AppHandle, State};
use uuid::Uuid;

use crate::session::AudioSessionInfo;
use crate::state::AppState;

// === Data Transfer Objects ===

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionDto {
    pub id: String,
    pub device_name: String,
    pub app_name: String,
    pub is_input: bool,
    pub status: String,
    pub audio_level: f32,
    pub duration: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioSessionDto {
    pub device_name: String,
    pub app_name: String,
    pub process_id: u32,
    pub is_input: bool,
}

impl From<AudioSessionDto> for AudioSessionInfo {
    fn from(dto: AudioSessionDto) -> Self {
        AudioSessionInfo {
            device_name: dto.device_name,
            app_name: dto.app_name,
            process_id: dto.process_id,
            is_input: dto.is_input,
        }
    }
}

// === Tauri Commands ===

/// Enumerate available audio sessions (applications with active audio)
#[tauri::command]
pub fn enumerate_sessions() -> Result<Vec<AudioSessionDto>, String> {
    crate::wasapi_sessions::enumerate_audio_sessions()
        .map(|sessions| {
            sessions
                .into_iter()
                .map(|s| AudioSessionDto {
                    device_name: s.device_name,
                    app_name: s.app_name,
                    process_id: s.process_id,
                    is_input: s.is_input,
                })
                .collect()
        })
        .map_err(|e| e.to_string())
}

/// Start recording selected sessions
#[tauri::command]
pub fn start_recording(
    app: AppHandle,
    state: State<'_, AppState>,
    sessions: Vec<AudioSessionDto>,
) -> Result<Vec<String>, String> {
    let mut session_ids = Vec::new();

    for session_dto in sessions {
        let info: AudioSessionInfo = session_dto.into();

        match state.start_session(info.clone(), app.clone()) {
            Ok(id) => {
                log::info!("Started recording session: {} - {}", info.app_name, info.device_name);
                session_ids.push(id.to_string());
            }
            Err(e) => {
                log::error!("Failed to start session {}: {}", info.app_name, e);
            }
        }
    }

    if session_ids.is_empty() {
        return Err("Failed to start any recording sessions".to_string());
    }

    Ok(session_ids)
}

/// Stop recording a specific session
#[tauri::command]
pub fn stop_session(state: State<'_, AppState>, session_id: String) -> Result<(), String> {
    let uuid = Uuid::parse_str(&session_id).map_err(|e| e.to_string())?;
    state.stop_session(&uuid).map_err(|e| e.to_string())
}

/// Stop all recording sessions
#[tauri::command]
pub fn stop_all_sessions(state: State<'_, AppState>) -> Result<(), String> {
    state.stop_all().map_err(|e| e.to_string())
}

/// Get current state of all recording sessions
#[tauri::command]
pub fn get_sessions_state(state: State<'_, AppState>) -> Vec<SessionDto> {
    state.get_all_sessions_dto()
}

/// Get server configuration status
#[tauri::command]
pub fn get_server_config(state: State<'_, AppState>) -> Result<(String, bool), String> {
    if state.is_server_configured() {
        let url = state.get_server_url().unwrap_or_default();
        Ok((url, true))
    } else {
        Err("API key not configured".to_string())
    }
}

/// Save API key to config file and update runtime state
#[tauri::command]
pub fn save_api_key(state: State<'_, AppState>, api_key: String) -> Result<(), String> {
    // Validate API key is not empty
    let api_key = api_key.trim().to_string();
    if api_key.is_empty() {
        return Err("API key cannot be empty".to_string());
    }

    // Save to config file
    let mut config = crate::config::AppConfig::load().unwrap_or_default();
    config.api_key = Some(api_key.clone());
    config.save().map_err(|e| format!("Failed to save config: {}", e))?;

    // Update runtime state
    state
        .set_api_key(api_key)
        .map_err(|e| format!("Failed to update state: {}", e))?;

    log::info!("API key saved successfully");
    Ok(())
}

/// Clear API key from config file and runtime state
#[tauri::command]
pub fn clear_api_key(state: State<'_, AppState>) -> Result<(), String> {
    // Clear from config file
    let mut config = crate::config::AppConfig::load().unwrap_or_default();
    config.api_key = None;
    config.save().map_err(|e| format!("Failed to save config: {}", e))?;

    // Clear runtime state
    state
        .clear_api_key()
        .map_err(|e| format!("Failed to update state: {}", e))?;

    log::info!("API key cleared");
    Ok(())
}

/// Get API key configuration status (without exposing the actual key)
#[tauri::command]
pub fn get_api_key_status(state: State<'_, AppState>) -> (bool, String) {
    let configured = state.is_server_configured();
    let url = state.get_server_url().unwrap_or_default();
    (configured, url)
}

/// Get a masked version of the stored API key for display purposes.
/// Returns None if no key is configured, or a masked string like "sk-...a1b2".
#[tauri::command]
pub fn get_masked_api_key() -> Option<String> {
    let config = crate::config::AppConfig::load().unwrap_or_default();
    config.api_key.as_ref().filter(|k| !k.is_empty()).map(|key| {
        let len = key.len();
        if len <= 8 {
            "*".repeat(len)
        } else {
            let prefix = &key[..4];
            let suffix = &key[len - 4..];
            format!("{}...{}", prefix, suffix)
        }
    })
}
