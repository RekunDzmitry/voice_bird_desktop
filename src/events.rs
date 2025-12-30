use serde::Serialize;

/// Event emitted for real-time audio level updates
/// Emitted at ~20Hz (every 50ms) per session
#[derive(Clone, Serialize)]
pub struct AudioLevelEvent {
    pub session_id: String,
    pub level: f32, // 0.0 to 1.0
}

/// Event emitted when session status changes
#[derive(Clone, Serialize)]
pub struct SessionStatusEvent {
    pub session_id: String,
    pub status: String, // "Recording", "Stopped", "Error"
    pub message: Option<String>,
}
