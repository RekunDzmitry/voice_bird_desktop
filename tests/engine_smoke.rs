// Local whisper engines don't exist on cloud-only Windows.
#![cfg(all(feature = "engine-smoke", not(windows)))]

use std::time::Duration;
use voice_bird_cli::transcription::{
    whisper_rs_engine::WhisperRsEngine, EngineConfig, EngineEvent, TranscriptionEngine,
};

#[test]
fn whisper_rs_produces_non_empty_transcript_for_fixture() {
    // Downloads tiny.en on demand. Path is cached; test is slow first time.
    let tiny = voice_bird_cli::transcription::models::Catalog::builtin()
        .get("tiny.en")
        .unwrap()
        .clone();
    let cache = voice_bird_cli::transcription::models::gguf_path("tiny.en").unwrap();
    if !cache.exists() {
        voice_bird_cli::transcription::models::download_model_with_verify(&tiny, &mut |_, _| {})
            .unwrap();
    }

    let spec = hound::WavReader::open("tests/fixtures/hello_world_16k.wav").unwrap();
    let samples: Vec<f32> = spec
        .into_samples::<i16>()
        .map(|s| s.unwrap() as f32 / i16::MAX as f32)
        .collect();

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut engine = WhisperRsEngine::default();
        let handle = engine
            .start(EngineConfig::Local {
                model_path: cache,
                language: Some("en".into()),
                sample_rate: 16_000,
                hop_ms: 750,
                min_window_ms: 1000,
            })
            .unwrap();

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
        assert!(
            transcript.to_lowercase().contains("hello"),
            "transcript = {:?}",
            transcript
        );
    });
}

#[cfg(feature = "engine-smoke-voicebird")]
#[test]
fn voicebird_produces_committed_event_for_fixture() {
    use std::time::Duration;
    use voice_bird_cli::transcription::{
        voicebird_engine::VoiceBirdEngine, EngineConfig, EngineEvent, TranscriptionEngine,
    };

    let key = std::env::var("VOICEBIRD_API_KEY")
        .expect("VOICEBIRD_API_KEY must be set for this smoke test");
    let url = std::env::var("VOICEBIRD_WS_URL")
        .expect("VOICEBIRD_WS_URL must be set (e.g. wss://voicebird.app/api/audio/stream)");

    let spec = hound::WavReader::open("tests/fixtures/hello_world_16k.wav").unwrap();
    let samples: Vec<f32> = spec
        .into_samples::<i16>()
        .map(|s| s.unwrap() as f32 / i16::MAX as f32)
        .collect();

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut engine = VoiceBirdEngine::new(key.clone(), url.clone());
        let handle = engine
            .start(EngineConfig::Cloud {
                api_key: key,
                language: Some("en".into()),
                sample_rate: 16_000,
                server_url: url,
                device_name: "smoke-test".into(),
                app_name: String::new(),
            })
            .unwrap();

        for chunk in samples.chunks(8_000) {
            handle.pcm_tx.send(chunk.to_vec()).await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
        let _ = handle.shutdown.send(());

        let mut rx = handle.events_rx;
        let mut saw_committed = false;
        while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
            if matches!(ev, EngineEvent::Committed(_)) {
                saw_committed = true;
                break;
            }
        }
        assert!(saw_committed, "did not receive a Committed event");
    });
}
