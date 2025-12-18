use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter};
use uuid::Uuid;
use anyhow::Result;
use crate::session::{AudioSessionInfo, RecordingSession};
use crate::audio;
use crate::config::AppConfig;
use crate::events::AudioLevelEvent;

/// Default server URL if not set in environment
const DEFAULT_SERVER_URL: &str = "https://voicebird.app";

/// Thread-safe application state managed by Tauri
pub struct AppState {
    pub sessions: Arc<Mutex<HashMap<Uuid, RecordingSession>>>,
    pub server_config: Arc<Mutex<Option<(String, String)>>>,
}

impl AppState {
    pub fn new() -> Self {
        // Get server URL from environment or use default
        let server_url = std::env::var("VOICE_BIRD_SERVER_URL")
            .unwrap_or_else(|_| DEFAULT_SERVER_URL.to_string());

        // Try to load API key from config file first, then fallback to .env
        let api_key = AppConfig::load()
            .ok()
            .and_then(|c| c.api_key)
            .or_else(|| std::env::var("VOICE_BIRD_API_KEY").ok());

        let config = api_key
            .filter(|k| !k.is_empty())
            .map(|key| (server_url, key));

        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            server_config: Arc::new(Mutex::new(config)),
        }
    }

    /// Check if server is configured
    pub fn is_server_configured(&self) -> bool {
        self.server_config
            .lock()
            .map(|c| c.is_some())
            .unwrap_or(false)
    }

    /// Get server URL if configured
    pub fn get_server_url(&self) -> Option<String> {
        self.server_config
            .lock()
            .ok()
            .and_then(|c| c.as_ref().map(|(url, _)| url.clone()))
    }

    /// Set API key at runtime (updates server config)
    pub fn set_api_key(&self, api_key: String) -> Result<()> {
        let url = std::env::var("VOICE_BIRD_SERVER_URL")
            .unwrap_or_else(|_| DEFAULT_SERVER_URL.to_string());

        let mut config = self
            .server_config
            .lock()
            .map_err(|_| anyhow::anyhow!("Lock poisoned"))?;
        *config = Some((url, api_key));
        Ok(())
    }

    /// Clear API key at runtime
    pub fn clear_api_key(&self) -> Result<()> {
        let mut config = self
            .server_config
            .lock()
            .map_err(|_| anyhow::anyhow!("Lock poisoned"))?;
        *config = None;
        Ok(())
    }

    /// Start a new recording session
    pub fn start_session(
        &self,
        session_info: AudioSessionInfo,
        app_handle: AppHandle,
    ) -> Result<Uuid> {
        let server_config = self
            .server_config
            .lock()
            .map_err(|_| anyhow::anyhow!("Lock poisoned"))?
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Server not configured"))?;

        let mut session = RecordingSession::new(session_info.clone(), 48000, 2);

        let session_id = session.id;

        // Clone Arc references for the audio level callback
        let audio_level = session.audio_level.clone();
        let stop_signal = session.stop_signal.clone();
        let session_id_str = session_id.to_string();

        // Start audio capture based on device type
        let host = cpal::default_host();

        if session_info.is_input {
            let device = audio::get_input_device_by_name(&host, &session_info.device_name)?;
            let stream = audio::start_input_recording(&device, &mut session, server_config)?;
            std::mem::forget(stream); // Keep stream alive
        } else {
            #[cfg(any(windows, target_os = "macos"))]
            {
                let cleanup = audio::start_output_recording(
                    &session_info.device_name,
                    &mut session,
                    None,
                    Some(server_config),
                )?;
                std::mem::forget(cleanup);
            }
            #[cfg(not(any(windows, target_os = "macos")))]
            {
                return Err(anyhow::anyhow!(
                    "Output recording is only supported on Windows and macOS"
                ));
            }
        }

        session.start_recording();

        // Spawn audio level emission loop
        let app_handle_clone = app_handle.clone();

        std::thread::spawn(move || {
            loop {
                if let Ok(stop) = stop_signal.lock() {
                    if *stop {
                        break;
                    }
                }

                if let Ok(level) = audio_level.lock() {
                    let _ = app_handle_clone.emit(
                        "audio-level",
                        AudioLevelEvent {
                            session_id: session_id_str.clone(),
                            level: *level,
                        },
                    );
                }

                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        });

        // Store session
        self.sessions
            .lock()
            .map_err(|_| anyhow::anyhow!("Lock poisoned"))?
            .insert(session_id, session);

        Ok(session_id)
    }

    /// Stop a specific session
    pub fn stop_session(&self, id: &Uuid) -> Result<()> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| anyhow::anyhow!("Lock poisoned"))?;

        if let Some(session) = sessions.get_mut(id) {
            session.stop_recording();
        }

        Ok(())
    }

    /// Stop all sessions
    pub fn stop_all(&self) -> Result<()> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| anyhow::anyhow!("Lock poisoned"))?;

        for session in sessions.values_mut() {
            session.stop_recording();
        }

        // Clear stopped sessions
        sessions.clear();

        Ok(())
    }

    /// Get all sessions as DTOs for frontend
    pub fn get_all_sessions_dto(&self) -> Vec<crate::commands::SessionDto> {
        let sessions = match self.sessions.lock() {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };

        sessions
            .values()
            .map(|s| crate::commands::SessionDto {
                id: s.id.to_string(),
                device_name: s.device_name.clone(),
                app_name: s.app_name.clone(),
                is_input: s.is_input,
                status: format!("{:?}", s.get_status()),
                audio_level: s.get_audio_level(),
                duration: s.get_duration(),
            })
            .collect()
    }
}
