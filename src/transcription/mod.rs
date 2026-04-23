pub mod local_agreement;
pub mod mock;
pub mod models;
pub mod refinement_engine;
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
pub enum EngineConfig {
    Local {
        model_path: std::path::PathBuf,
        language: Option<String>,
        sample_rate: u32,   // always 16_000
        hop_ms: u32,        // whisper-rs only
        min_window_ms: u32, // whisper-rs only
    },
    Cloud {
        api_key: String,
        language: Option<String>,
        sample_rate: u32,
    },
}

pub struct EngineHandle {
    pub pcm_tx: mpsc::Sender<Vec<f32>>,
    pub events_rx: broadcast::Receiver<EngineEvent>,
    pub shutdown: oneshot::Sender<()>,
}

pub trait TranscriptionEngine: Send {
    fn start(&mut self, cfg: EngineConfig) -> anyhow::Result<EngineHandle>;
}

/// Build a transcription engine based on user preference and whether the
/// WhisperKit sidecar binary is available on disk. `prefer` comes from
/// `config.toml`'s `engine_prefer` field (`"auto"`, `"whisperkit"`, or
/// `"whisper_rs"`). On macOS, `"auto"` and `"whisperkit"` pick the
/// sidecar iff `sidecar_path` is `Some` and points to an existing file;
/// otherwise we fall back to `whisper-rs` transparently.
pub fn select_engine(
    prefer: &str,
    sidecar_path: Option<&std::path::Path>,
) -> Box<dyn TranscriptionEngine> {
    #[cfg(target_os = "macos")]
    {
        if prefer == "whisperkit" || prefer == "auto" {
            if let Some(path) = sidecar_path {
                if path.exists() {
                    return Box::new(whisper_kit_engine::WhisperKitEngine::new(
                        path.to_path_buf(),
                    ));
                }
            }
        }
    }
    // Silence the unused-parameter warning on non-macOS builds.
    let _ = (prefer, sidecar_path);
    Box::new(whisper_rs_engine::WhisperRsEngine::default())
}

/// Locate the `voice-bird-whisperkit` Swift sidecar binary. We probe, in
/// order: the macOS `.app` bundle layout (Resources/), a sibling next to
/// the current executable (release layout), and the dev `.build/release`
/// produced by `cargo run -p xtask -- build-sidecar`. Returns `None` on
/// non-macOS or if no candidate exists.
pub fn sidecar_path() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    // .app bundle layout
    let bundle = dir.join("../Resources/voice-bird-whisperkit");
    if bundle.exists() {
        return Some(bundle);
    }
    // sibling binary (dev / release layout)
    let sibling = dir.join("voice-bird-whisperkit");
    if sibling.exists() {
        return Some(sibling);
    }
    // project dev fallback (running via `cargo run`, target/debug/voice-bird)
    let dev = dir.join("../../whisperkit-helper/.build/release/voice-bird-whisperkit");
    if dev.exists() {
        return Some(dev);
    }
    None
}
