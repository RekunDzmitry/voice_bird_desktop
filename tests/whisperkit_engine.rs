#![cfg(all(target_os = "macos", feature = "engine-smoke"))]

use std::path::PathBuf;
use std::time::Duration;
use voice_bird_cli::transcription::{
    whisper_kit_engine::WhisperKitEngine, EngineConfig, EngineEvent, TranscriptionEngine,
};

#[test]
fn sidecar_starts_and_emits_ready() {
    let sidecar: PathBuf = "whisperkit-helper/.build/release/voice-bird-whisperkit".into();
    if !sidecar.exists() {
        eprintln!(
            "sidecar binary not built; skipping. \
             Run `cargo run -p xtask -- build-sidecar` first."
        );
        return;
    }

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut engine = WhisperKitEngine::new(sidecar);
        let handle = engine
            .start(EngineConfig::Local {
                // WhisperKit takes a model id, not a gguf path — the
                // sidecar decodes the handshake's `"model"` string and
                // forwards it to `WhisperKit(model:)`.
                model_path: "tiny.en".into(),
                language: Some("en".into()),
                sample_rate: 16_000,
                hop_ms: 750,
                min_window_ms: 1000,
            })
            .unwrap();

        let mut rx = handle.events_rx;
        let got = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(got, EngineEvent::ModelLoaded { .. }));
    });
}
