use std::sync::{Arc, Mutex};
use std::collections::HashMap;
use uuid::Uuid;
use chrono::Local;

#[derive(Debug, Clone, PartialEq)]
pub enum SessionStatus {
    Idle,
    Recording,
    #[allow(dead_code)]
    Paused,
    Stopped,
}

#[derive(Debug, Clone)]
pub struct AudioSessionInfo {
    pub device_name: String,
    pub app_name: String,
    pub process_id: u32,
    pub is_input: bool, // true for microphone, false for output/loopback
}

pub struct RecordingSession {
    pub id: Uuid,
    pub device_name: String,
    pub app_name: String,
    #[allow(dead_code)]
    pub process_id: u32,
    #[allow(dead_code)]
    pub is_input: bool,
    pub status: Arc<Mutex<SessionStatus>>,
    pub audio_level: Arc<Mutex<f32>>,
    pub audio_buffer: Arc<Mutex<Vec<f32>>>,
    pub transcript_buffer: Arc<Mutex<Vec<String>>>,
    pub sample_rate: u32,
    pub channels: u16,
    pub start_time: Option<std::time::Instant>,
    pub stop_signal: Arc<Mutex<bool>>,
}

impl RecordingSession {
    pub fn new(session_info: AudioSessionInfo, sample_rate: u32, channels: u16) -> Self {
        Self {
            id: Uuid::new_v4(),
            device_name: session_info.device_name,
            app_name: session_info.app_name,
            process_id: session_info.process_id,
            is_input: session_info.is_input,
            status: Arc::new(Mutex::new(SessionStatus::Idle)),
            audio_level: Arc::new(Mutex::new(0.0)),
            audio_buffer: Arc::new(Mutex::new(Vec::new())),
            transcript_buffer: Arc::new(Mutex::new(Vec::new())),
            sample_rate,
            channels,
            start_time: None,
            stop_signal: Arc::new(Mutex::new(false)),
        }
    }

    pub fn start_recording(&mut self) {
        if let Ok(mut status) = self.status.lock() {
            *status = SessionStatus::Recording;
        }
        self.start_time = Some(std::time::Instant::now());
    }

    pub fn stop_recording(&mut self) {
        if let Ok(mut status) = self.status.lock() {
            *status = SessionStatus::Stopped;
        }
        if let Ok(mut signal) = self.stop_signal.lock() {
            *signal = true;
        }
    }

    pub fn get_status(&self) -> SessionStatus {
        self.status.lock().map(|s| s.clone()).unwrap_or(SessionStatus::Idle)
    }

    pub fn get_audio_level(&self) -> f32 {
        self.audio_level.lock().map(|l| *l).unwrap_or(0.0)
    }

    pub fn get_duration(&self) -> f32 {
        self.start_time.map(|t| t.elapsed().as_secs_f32()).unwrap_or(0.0)
    }

    pub fn get_filename_prefix(&self) -> String {
        let timestamp = Local::now().format("%Y-%m-%d_%H-%M-%S").to_string();
        let safe_app_name = self.app_name.replace(&[' ', '.', '/', '\\'][..], "_");
        let safe_device = self.device_name.split('(').next().unwrap_or(&self.device_name).trim().replace(' ', "_");
        format!("recording_{}_{}_{}",  safe_app_name, safe_device, timestamp)
    }
}

#[allow(dead_code)]
pub enum SessionCommand {
    Start(AudioSessionInfo),
    Stop(Uuid),
    StopAll,
}

pub struct SessionManager {
    pub active_sessions: HashMap<Uuid, RecordingSession>,
}

impl SessionManager {
    pub fn new() -> Self {
        Self {
            active_sessions: HashMap::new(),
        }
    }

    pub fn add_session(&mut self, session: RecordingSession) -> Uuid {
        let id = session.id;
        self.active_sessions.insert(id, session);
        id
    }

    #[allow(dead_code)]
    pub fn remove_session(&mut self, id: &Uuid) -> Option<RecordingSession> {
        self.active_sessions.remove(id)
    }

    #[allow(dead_code)]
    pub fn get_session(&self, id: &Uuid) -> Option<&RecordingSession> {
        self.active_sessions.get(id)
    }

    #[allow(dead_code)]
    pub fn get_session_mut(&mut self, id: &Uuid) -> Option<&mut RecordingSession> {
        self.active_sessions.get_mut(id)
    }

    pub fn get_all_sessions(&self) -> Vec<&RecordingSession> {
        self.active_sessions.values().collect()
    }

    pub fn stop_all(&mut self) {
        for session in self.active_sessions.values_mut() {
            session.stop_recording();
        }
    }
}
