// Local inference engines (and their helpers) are gated off Windows:
// Windows is cloud-only since 0.4.0, so whisper-rs / parakeet-rs / sysinfo
// never enter the Windows dependency graph (see Cargo.toml).
#[cfg(not(windows))]
pub mod auto_select;
pub mod local_agreement;
pub mod mock;
pub mod models;
#[cfg(not(windows))]
pub mod nemotron_engine;
#[cfg(not(windows))]
pub mod refinement_engine;
pub mod voicebird_engine;
pub mod whisper_kit_engine;
#[cfg(not(windows))]
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
            t_end_ms: s.t_end.as_millis() as u64,
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
        /// WebSocket URL of the Voice Bird Web `/api/audio/stream`
        /// endpoint to stream PCM to.
        server_url: String,
        /// Device label sent to voicebird.app in the init handshake;
        /// surfaces in the live-session card so users can tell which
        /// audio source is being streamed when multiple stream.
        device_name: String,
        /// Source-application label sent in the init handshake.
        /// Empty for mic / system captures (UI falls back to
        /// `device_name`); the app's display name (e.g. "Chrome",
        /// "Safari") for `SessionSource::App` loopback captures so
        /// each (device, app) pair gets its own row in the
        /// Transcriptions tab.
        app_name: String,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineKind {
    WhisperRs,
    WhisperKit,
    Nemotron,
    VoiceBirdWeb,
}

/// Select the transcription engine for a recording. Returns an error for
/// cases that should surface to the user (e.g. cloud broadcast enabled but
/// no key, or local mode requested on cloud-only Windows).
///
/// When `cloud_broadcast_enabled` is true, the cloud Voice Bird Web
/// engine is selected unconditionally — it bypasses local Whisper and
/// streams PCM to the user's voicebird.app account. `prefer` is only
/// consulted for local-engine selection (whisperkit / whisper_rs).
pub fn try_select_engine(
    prefer: &str,
    cloud_broadcast_enabled: bool,
    voicebird_api_key: &str,
    voicebird_server_url: &str,
    sidecar_path: Option<&std::path::Path>,
) -> Result<(EngineKind, Box<dyn TranscriptionEngine>), String> {
    if cloud_broadcast_enabled {
        if voicebird_api_key.is_empty() {
            return Err(
                "Live broadcast enabled but no Voice Bird API key — open settings (press ',')"
                    .into(),
            );
        }
        if voicebird_server_url.is_empty() {
            return Err(
                "Live broadcast enabled but no Voice Bird server URL — open settings (press ',')"
                    .into(),
            );
        }
        return Ok((
            EngineKind::VoiceBirdWeb,
            Box::new(voicebird_engine::VoiceBirdEngine::new(
                voicebird_api_key.to_string(),
                voicebird_server_url.to_string(),
            )),
        ));
    }

    // Windows has no local engines (cloud-only since 0.4.0). This branch is
    // defensive — the app forces cloud on at config load and clamps per-source
    // settings — but it guarantees a coherent error if a hand-edited config
    // slips through with cloud off.
    #[cfg(windows)]
    {
        let _ = (prefer, sidecar_path);
        Err("Local transcription is not supported on Windows — Windows is cloud-only. \
             Press 'c' to set your Voice Bird API key."
            .into())
    }

    #[cfg(not(windows))]
    {
        #[cfg(target_os = "macos")]
        {
            if prefer == "whisperkit" || prefer == "auto" {
                if let Some(path) = sidecar_path {
                    if path.exists() {
                        return Ok((
                            EngineKind::WhisperKit,
                            Box::new(whisper_kit_engine::WhisperKitEngine::new(
                                path.to_path_buf(),
                            )),
                        ));
                    }
                }
            }
        }
        let _ = (prefer, sidecar_path);
        Ok((
            EngineKind::WhisperRs,
            Box::<whisper_rs_engine::WhisperRsEngine>::default(),
        ))
    }
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
    // project dev fallback (running via `cargo run`, target/debug/voice-bird-cli)
    let dev = dir.join("../../whisperkit-helper/.build/release/voice-bird-whisperkit");
    if dev.exists() {
        return Some(dev);
    }
    None
}

#[cfg(test)]
mod select_tests {
    use super::*;

    const TEST_URL: &str = "wss://example.test/api/audio/stream";

    #[test]
    fn broadcast_with_key_returns_cloud_engine() {
        let res = try_select_engine("auto", true, "vb-fake", TEST_URL, None);
        let (kind, _engine) = res.expect("expected Ok");
        assert_eq!(kind, EngineKind::VoiceBirdWeb);
    }

    #[test]
    fn broadcast_without_key_returns_err() {
        let err = try_select_engine("auto", true, "", TEST_URL, None)
            .err()
            .unwrap();
        assert!(err.to_lowercase().contains("api key"));
    }

    #[test]
    fn broadcast_without_url_returns_err() {
        let err = try_select_engine("auto", true, "vb-fake", "", None)
            .err()
            .unwrap();
        assert!(err.to_lowercase().contains("server url"));
    }

    #[cfg(not(windows))]
    #[test]
    fn local_path_ignores_credentials() {
        let (kind, _) = try_select_engine("whisper_rs", false, "", "", None).unwrap();
        assert_eq!(kind, EngineKind::WhisperRs);
    }

    #[cfg(not(windows))]
    #[test]
    fn local_path_when_broadcast_off_even_with_creds() {
        // When broadcast is off, creds are irrelevant and we land on the
        // local engine (whisper_rs without a sidecar path).
        let (kind, _) = try_select_engine("whisper_rs", false, "vb-fake", TEST_URL, None).unwrap();
        assert_eq!(kind, EngineKind::WhisperRs);
    }

    #[cfg(windows)]
    #[test]
    fn windows_without_cloud_returns_err() {
        // Windows has no local engines; asking for one must surface the
        // cloud-only error rather than panic or silently pick anything.
        let err = try_select_engine("auto", false, "", "", None).err().unwrap();
        assert!(err.to_lowercase().contains("cloud-only"));
    }
}
