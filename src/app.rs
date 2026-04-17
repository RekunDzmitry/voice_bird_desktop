use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use uuid::Uuid;

use crate::config::AppConfig;
use crate::platform::AudioSession;
use crate::streaming::{InitSuccess, StreamError, UsageInfo};

/// Application running mode
#[derive(Debug, Clone, PartialEq)]
pub enum AppMode {
    Normal,
    ConfigInput,
    Help,
}

/// Structured recording errors with user-friendly messages
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum RecordingError {
    QuotaExceeded {
        message: String,
        usage: Option<UsageInfo>,
    },
    InvalidApiKey {
        message: String,
    },
    ConnectionFailed {
        message: String,
    },
    InitTimeout,
    NoApiKey,
    NoSelection,
    NoSessionsStarted,
    Other(String),
}

impl RecordingError {
    pub fn display_message(&self) -> String {
        match self {
            RecordingError::QuotaExceeded { usage, .. } => {
                if let Some(u) = usage {
                    let used_mins = (u.seconds_used / 60.0).round() as u32;
                    let limit_mins = (u.seconds_limit / 60.0).round() as u32;
                    format!(
                        "Quota exceeded: {}m/{}m used ({}). Upgrade at voicebird.app/pricing",
                        used_mins, limit_mins, u.plan
                    )
                } else {
                    "Quota exceeded. Upgrade at voicebird.app/pricing".to_string()
                }
            }
            RecordingError::InvalidApiKey { .. } => {
                "Invalid API key. Press 'c' to reconfigure.".to_string()
            }
            RecordingError::ConnectionFailed { message } => {
                format!("Connection failed: {}", message)
            }
            RecordingError::InitTimeout => {
                "Server initialization timed out. Check your connection.".to_string()
            }
            RecordingError::NoApiKey => {
                "API key not configured. Press 'c' to configure.".to_string()
            }
            RecordingError::NoSelection => {
                "No sessions selected. Press Space to select.".to_string()
            }
            RecordingError::NoSessionsStarted => {
                "Failed to start any sessions".to_string()
            }
            RecordingError::Other(msg) => msg.clone(),
        }
    }
}

impl PartialEq for RecordingError {
    fn eq(&self, other: &Self) -> bool {
        // Compare by display message for simplicity
        self.display_message() == other.display_message()
    }
}

impl From<StreamError> for RecordingError {
    fn from(err: StreamError) -> Self {
        match err {
            StreamError::QuotaExceeded { message, usage } => {
                RecordingError::QuotaExceeded { message, usage }
            }
            StreamError::InvalidApiKey { message } => {
                RecordingError::InvalidApiKey { message }
            }
            StreamError::ConnectionFailed { message } => {
                RecordingError::ConnectionFailed { message }
            }
            StreamError::InitTimeout => RecordingError::InitTimeout,
            StreamError::Other { message } => RecordingError::Other(message),
        }
    }
}

/// Recording status
#[derive(Debug, Clone, PartialEq)]
pub enum RecordingStatus {
    Idle,
    Connecting,
    Streaming { usage: Option<UsageInfo> },
    Error(RecordingError),
}

/// Active recording session
#[allow(dead_code)]
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

    /// Whether the API key is visible in config dialog
    pub api_key_visible: bool,

    /// Should the app quit?
    pub should_quit: bool,

    /// Status message to display
    pub status_message: Option<String>,

    /// Shared error channel — recording threads write errors here
    pub error_channel: Arc<Mutex<Option<String>>>,

    /// Path to the current log file
    pub log_path: Option<PathBuf>,

    /// Channel to receive init results from streaming threads
    pub init_result_rx: Option<mpsc::Receiver<Result<InitSuccess, StreamError>>>,
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
            api_key_visible: false,
            should_quit: false,
            status_message: None,
            error_channel: Arc::new(Mutex::new(None)),
            log_path: None,
            init_result_rx: None,
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
        self.api_key_input.clear();
        self.api_key_visible = false;
    }

    /// Toggle API key visibility in config dialog
    pub fn toggle_api_key_visibility(&mut self) {
        self.api_key_visible = !self.api_key_visible;
        self.status_message = Some(if self.api_key_visible {
            "Key visible".to_string()
        } else {
            "Key hidden".to_string()
        });
    }

    /// Get a masked version of the currently stored API key for display
    pub fn masked_stored_key(&self) -> Option<String> {
        self.config.api_key.as_ref().filter(|k| !k.is_empty()).map(|key| {
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

    /// Paste from clipboard, replacing current input entirely
    pub fn paste_from_clipboard(&mut self) {
        match arboard::Clipboard::new().and_then(|mut cb| cb.get_text()) {
            Ok(text) => {
                let trimmed = text.trim().to_string();
                if !trimmed.is_empty() {
                    self.api_key_input = trimmed;
                    self.status_message = Some("Pasted from clipboard".to_string());
                }
            }
            Err(e) => {
                log::warn!("Failed to paste from clipboard: {}", e);
                self.status_message = Some(format!("Clipboard error: {}", e));
            }
        }
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

    /// Check for errors from recording threads and update status
    pub fn check_error(&mut self) {
        if let Ok(mut err) = self.error_channel.lock() {
            if let Some(msg) = err.take() {
                self.status = RecordingStatus::Error(RecordingError::Other(msg));
            }
        }
    }

    /// Check for init results from streaming threads
    pub fn check_init_result(&mut self) {
        let result = if let Some(ref rx) = self.init_result_rx {
            match rx.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Disconnected) => {
                    // Channel closed while still Connecting
                    if self.status == RecordingStatus::Connecting {
                        Some(Err(StreamError::Other {
                            message: "Recording thread terminated unexpectedly".to_string(),
                        }))
                    } else {
                        None
                    }
                }
                Err(mpsc::TryRecvError::Empty) => None,
            }
        } else {
            None
        };

        if let Some(init_result) = result {
            match init_result {
                Ok(success) => {
                    self.status = RecordingStatus::Streaming { usage: success.usage };
                }
                Err(err) => {
                    log::error!("Init failed: {}", err);
                    self.status = RecordingStatus::Error(RecordingError::from(err));
                    // Stop active sessions since init failed
                    for session in &self.active_sessions {
                        if let Ok(mut stop) = session.stop_signal.lock() {
                            *stop = true;
                        }
                    }
                    self.active_sessions.clear();
                    self.start_time = None;
                    self.init_result_rx = None;
                }
            }
        }
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
