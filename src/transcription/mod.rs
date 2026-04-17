pub mod local_agreement;
pub mod mock;
pub mod models;
pub mod whisper_kit_engine;
pub mod whisper_rs_engine;

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::session::writer::WrittenSegment;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Token {
    pub text: String,
    pub t_start_ms: u64,
    pub t_end_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Segment {
    pub t_start: Duration,
    pub t_end: Duration,
    pub text: String,
    pub tokens: Vec<Token>,
}

impl From<&Segment> for WrittenSegment {
    fn from(s: &Segment) -> Self {
        WrittenSegment {
            t_start_ms: s.t_start.as_millis() as u64,
            t_end_ms:   s.t_end.as_millis() as u64,
            text: s.text.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum EngineEvent {
    ModelLoaded { name: String },
    Committed(Segment),
    Tentative(String),
    Error(String),
}

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub model_path: std::path::PathBuf,
    pub language: Option<String>,
    pub sample_rate: u32,   // always 16_000
    pub hop_ms: u32,        // WhisperRsEngine only
    pub min_window_ms: u32, // WhisperRsEngine only
}

pub struct EngineHandle {
    pub pcm_tx: mpsc::Sender<Vec<f32>>,
    pub events_rx: broadcast::Receiver<EngineEvent>,
    pub shutdown: oneshot::Sender<()>,
}

pub trait TranscriptionEngine: Send {
    fn start(&mut self, cfg: EngineConfig) -> anyhow::Result<EngineHandle>;
}
