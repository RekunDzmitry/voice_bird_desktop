#![cfg(feature = "engine-smoke")]

use std::time::Duration;
use voice_bird::transcription::{
    whisper_rs_engine::WhisperRsEngine,
    EngineConfig, EngineEvent, TranscriptionEngine,
};

#[test]
fn whisper_rs_produces_non_empty_transcript_for_fixture() {
    // Downloads tiny.en on demand. Path is cached; test is slow first time.
    let tiny = voice_bird::transcription::models::Catalog::builtin()
        .get("tiny.en").unwrap().clone();
    let cache = voice_bird::transcription::models::gguf_path("tiny.en").unwrap();
    if !cache.exists() {
        voice_bird::transcription::models::download_with_verify(
            tiny.gguf_url, &cache, tiny.gguf_sha256, &mut |_, _| {},
        ).unwrap();
    }

    let spec = hound::WavReader::open("tests/fixtures/hello_world_16k.wav").unwrap();
    let samples: Vec<f32> = spec.into_samples::<i16>()
        .map(|s| s.unwrap() as f32 / i16::MAX as f32).collect();

    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let mut engine = WhisperRsEngine::default();
        let handle = engine.start(EngineConfig {
            model_path: cache,
            language: Some("en".into()),
            sample_rate: 16_000,
            hop_ms: 750, min_window_ms: 1000,
        }).unwrap();

        // Feed in 500ms chunks
        for chunk in samples.chunks(8_000) {
            handle.pcm_tx.send(chunk.to_vec()).await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        drop(handle.pcm_tx);

        let mut transcript = String::new();
        let mut rx = handle.events_rx;
        while let Ok(Ok(evt)) = tokio::time::timeout(Duration::from_secs(30), rx.recv()).await {
            if let EngineEvent::Committed(seg) = evt {
                transcript.push_str(&seg.text);
                transcript.push(' ');
            }
        }
        assert!(transcript.to_lowercase().contains("hello"), "transcript = {:?}", transcript);
    });
}
