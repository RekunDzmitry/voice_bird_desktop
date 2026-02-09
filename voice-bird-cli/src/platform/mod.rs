use serde::{Deserialize, Serialize};
use anyhow::Result;

#[cfg(windows)]
mod windows;

#[cfg(target_os = "macos")]
mod macos;

/// Information about an audio session (application producing audio)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioSession {
    pub device_name: String,
    pub app_name: String,
    pub process_id: u32,
    pub is_input: bool,
}

/// Enumerate all active audio sessions
#[cfg(windows)]
pub fn enumerate_audio_sessions() -> Result<Vec<AudioSession>> {
    windows::enumerate_audio_sessions()
}

#[cfg(target_os = "macos")]
pub fn enumerate_audio_sessions() -> Result<Vec<AudioSession>> {
    macos::enumerate_audio_sessions()
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn enumerate_audio_sessions() -> Result<Vec<AudioSession>> {
    // Fallback: return empty list on unsupported platforms
    Ok(Vec::new())
}

/// Start output recording (loopback capture)
#[cfg(windows)]
pub fn start_output_recording(
    session: &AudioSession,
    server_url: String,
    api_key: String,
    session_id: String,
    audio_level: std::sync::Arc<std::sync::Mutex<f32>>,
    stop_signal: std::sync::Arc<std::sync::Mutex<bool>>,
) -> Result<()> {
    windows::start_output_recording(session, server_url, api_key, session_id, audio_level, stop_signal)
}

#[cfg(target_os = "macos")]
pub fn start_output_recording(
    session: &AudioSession,
    server_url: String,
    api_key: String,
    session_id: String,
    audio_level: std::sync::Arc<std::sync::Mutex<f32>>,
    stop_signal: std::sync::Arc<std::sync::Mutex<bool>>,
) -> Result<()> {
    macos::start_output_recording(session, server_url, api_key, session_id, audio_level, stop_signal)
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn start_output_recording(
    _session: &AudioSession,
    _server_url: String,
    _api_key: String,
    _session_id: String,
    _audio_level: std::sync::Arc<std::sync::Mutex<f32>>,
    _stop_signal: std::sync::Arc<std::sync::Mutex<bool>>,
) -> Result<()> {
    Err(anyhow::anyhow!("Output recording not supported on this platform"))
}
