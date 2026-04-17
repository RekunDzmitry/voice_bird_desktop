//! Streaming stub.
//!
//! Project A Stage 1 drops the server-streaming transport (WebSocket to the
//! cloud) because transcription is moving fully on-device. Stage 2 replaces
//! this module with a local session writer + engine trait. Until then we
//! keep the public surface that `app.rs` / `main.rs` import (so the build
//! stays green), but the actual stream routine just drains the receiver
//! until `stop_signal` is set and reports a synthetic init success.

use std::fmt;
use std::sync::mpsc;

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Usage information returned by the server.
///
/// Preserved for structural compatibility with `RecordingStatus::Streaming`;
/// populated with zeros until Stage 2 / 3 wires real local metrics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct UsageInfo {
    #[serde(default)]
    pub seconds_remaining: f64,
    #[serde(default)]
    pub seconds_limit: f64,
    #[serde(default)]
    pub seconds_used: f64,
    #[serde(default)]
    pub plan: String,
}

/// Successful init response.
#[derive(Debug, Clone)]
pub struct InitSuccess {
    pub usage: Option<UsageInfo>,
}

/// Errors that can occur during stream initialization.
#[derive(Debug, Clone)]
pub enum StreamError {
    QuotaExceeded { message: String, usage: Option<UsageInfo> },
    InvalidApiKey { message: String },
    ConnectionFailed { message: String },
    InitTimeout,
    Other { message: String },
}

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StreamError::QuotaExceeded { message, .. } => write!(f, "{}", message),
            StreamError::InvalidApiKey { message } => write!(f, "{}", message),
            StreamError::ConnectionFailed { message } => write!(f, "{}", message),
            StreamError::InitTimeout => write!(f, "Session initialization timeout"),
            StreamError::Other { message } => write!(f, "{}", message),
        }
    }
}

impl std::error::Error for StreamError {}

/// Stub streaming routine: drains audio samples and immediately reports
/// init success. Stage 2 replaces this with a local session writer that
/// encodes to WAV + hands frames to the transcription engine trait.
#[allow(clippy::too_many_arguments)]
pub async fn stream_to_server(
    _server_url: String,
    _api_key: String,
    _session_id: String,
    _device_name: String,
    _app_name: Option<String>,
    _is_input: bool,
    audio_rx: mpsc::Receiver<Vec<f32>>,
    _sample_rate: u32,
    _channels: u16,
    init_result_tx: mpsc::Sender<Result<InitSuccess, StreamError>>,
) -> Result<()> {
    // Report init success immediately so the UI transitions Connecting → Streaming.
    let _ = init_result_tx.send(Ok(InitSuccess { usage: None }));

    // Drain the receiver until it disconnects (i.e., the audio stream is
    // torn down). We drop the samples on the floor — real processing lands
    // in Stage 2.
    while let Ok(_samples) = audio_rx.recv() {
        // no-op
    }

    Ok(())
}
