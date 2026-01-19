use std::sync::{Arc, Mutex};
use uuid::Uuid;

use crate::config::AppConfig;
use crate::platform::AudioSession;

/// Application running mode
#[derive(Debug, Clone, PartialEq)]
pub enum AppMode {
    Normal,
    ConfigInput,
    Help,
}

/// Recording status
#[derive(Debug, Clone, PartialEq)]
pub enum RecordingStatus {
    Idle,
    Connecting,
    Streaming,
    Error(String),
}

/// Active recording session
pub struct ActiveSession {
    pub id: Uuid,
    pub session: AudioSession,
    pub stop_signal: Arc<Mutex<bool>>,
}

/// Main application state
pub struct App {
    /// Current mode
    pub mode: AppMode,

    /// Available audio sessions
    pub sessions: Vec<AudioSession>,

    /// Currently selected index in the session list
    pub selected_index: usize,

    /// Selected sessions for recording (by index)
    pub selected_sessions: Vec<usize>,

    /// Current recording status
    pub status: RecordingStatus,

    /// Real-time audio level (0.0 - 1.0)
    pub audio_level: Arc<Mutex<f32>>,

    /// Recording duration in seconds
    pub duration: f32,

    /// Recording start time
    pub start_time: Option<std::time::Instant>,

    /// Active recording sessions
    pub active_sessions: Vec<ActiveSession>,

    /// Application config
    pub config: AppConfig,

    /// API key input buffer (for config mode)
    pub api_key_input: String,

    /// Should the app quit?
    pub should_quit: bool,

    /// Status message to display
    pub status_message: Option<String>,
}

impl App {
    /// Create a new application instance
    pub fn new() -> Self {
        let config = AppConfig::load().unwrap_or_default();

        Self {
            mode: AppMode::Normal,
            sessions: Vec::new(),
            selected_index: 0,
            selected_sessions: Vec::new(),
            status: RecordingStatus::Idle,
            audio_level: Arc::new(Mutex::new(0.0)),
            duration: 0.0,
            start_time: None,
            active_sessions: Vec::new(),
            config,
            api_key_input: String::new(),
            should_quit: false,
            status_message: None,
        }
    }

    /// Refresh the list of audio sessions
    pub fn refresh_sessions(&mut self) {
        match crate::platform::enumerate_audio_sessions() {
            Ok(sessions) => {
                self.sessions = sessions;
                self.selected_sessions.clear();
                if self.selected_index >= self.sessions.len() {
                    self.selected_index = self.sessions.len().saturating_sub(1);
                }
            }
            Err(e) => {
                self.status_message = Some(format!("Failed to enumerate sessions: {}", e));
            }
        }
    }

    /// Move selection up
    pub fn select_previous(&mut self) {
        if self.selected_index > 0 {
            self.selected_index -= 1;
        }
    }

    /// Move selection down
    pub fn select_next(&mut self) {
        if self.selected_index < self.sessions.len().saturating_sub(1) {
            self.selected_index += 1;
        }
    }

    /// Toggle selection of current item
    pub fn toggle_selection(&mut self) {
        if self.sessions.is_empty() {
            return;
        }

        if let Some(pos) = self.selected_sessions.iter().position(|&i| i == self.selected_index) {
            self.selected_sessions.remove(pos);
        } else {
            self.selected_sessions.push(self.selected_index);
        }
    }

    /// Check if an index is selected
    pub fn is_selected(&self, index: usize) -> bool {
        self.selected_sessions.contains(&index)
    }

    /// Get current audio level
    pub fn get_audio_level(&self) -> f32 {
        self.audio_level.lock().map(|l| *l).unwrap_or(0.0)
    }

    /// Update duration from start time
    pub fn update_duration(&mut self) {
        if let Some(start) = self.start_time {
            self.duration = start.elapsed().as_secs_f32();
        }
    }

    /// Format duration as MM:SS
    pub fn format_duration(&self) -> String {
        let total_secs = self.duration as u32;
        let mins = total_secs / 60;
        let secs = total_secs % 60;
        format!("{:02}:{:02}", mins, secs)
    }

    /// Enter config input mode
    pub fn enter_config_mode(&mut self) {
        self.mode = AppMode::ConfigInput;
        self.api_key_input = self.config.api_key.clone().unwrap_or_default();
    }

    /// Save API key from input
    pub fn save_api_key(&mut self) {
        let key = self.api_key_input.trim().to_string();
        if key.is_empty() {
            self.config.api_key = None;
        } else {
            self.config.api_key = Some(key);
        }

        if let Err(e) = self.config.save() {
            self.status_message = Some(format!("Failed to save config: {}", e));
        } else {
            self.status_message = Some("API key saved".to_string());
        }

        self.mode = AppMode::Normal;
    }

    /// Cancel config input
    pub fn cancel_config(&mut self) {
        self.mode = AppMode::Normal;
        self.api_key_input.clear();
    }

    /// Toggle help display
    pub fn toggle_help(&mut self) {
        self.mode = if self.mode == AppMode::Help {
            AppMode::Normal
        } else {
            AppMode::Help
        };
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}
