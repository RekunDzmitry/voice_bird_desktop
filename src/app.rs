use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::config::AppConfig;
use crate::platform::AudioSession;

/// Application running mode
#[derive(Debug, Clone, PartialEq)]
pub enum AppMode {
    Normal,
    ModelPicker, // wired in Stage 3's Task 18
    Help,
}

/// Recording status
#[derive(Debug, Clone)]
pub enum RecordingStatus {
    Idle,
    Recording,
    Error(String),
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

    /// Application config
    pub config: AppConfig,

    /// Should the app quit?
    pub should_quit: bool,

    /// Status message to display
    pub status_message: Option<String>,

    /// Shared error channel — recording threads write errors here
    pub error_channel: Arc<Mutex<Option<String>>>,

    /// Path to the current log file
    pub log_path: Option<PathBuf>,
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
            config,
            should_quit: false,
            status_message: None,
            error_channel: Arc::new(Mutex::new(None)),
            log_path: None,
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

    /// Check for errors from recording threads and update status
    pub fn check_error(&mut self) {
        if let Ok(mut err) = self.error_channel.lock() {
            if let Some(msg) = err.take() {
                self.status = RecordingStatus::Error(msg);
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

    /// Start recording — stub; real pipeline lands in Step 2.
    pub fn start_recording(&mut self) {
        self.status = RecordingStatus::Recording;
        self.start_time = Some(std::time::Instant::now());
    }

    /// Stop recording — stub; real pipeline lands in Step 2.
    pub fn stop_recording(&mut self) {
        self.status = RecordingStatus::Idle;
        self.start_time = None;
        self.duration = 0.0;

        if let Ok(mut level) = self.audio_level.lock() {
            *level = 0.0;
        }
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}
