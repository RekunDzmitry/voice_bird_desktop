use voice_bird::transcription::{
    assemblyai_engine::AssemblyAiEngine, EngineConfig, TranscriptionEngine,
};

#[test]
fn rejects_local_variant() {
    let mut e = AssemblyAiEngine::new("sk-x".into());
    let err = e
        .start(EngineConfig::Local {
            model_path: "/dev/null".into(),
            language: None,
            sample_rate: 16_000,
            hop_ms: 750,
            min_window_ms: 1000,
        })
        .err()
        .expect("expected Err on Local variant");
    assert!(
        err.to_string().to_lowercase().contains("cloud"),
        "got: {err}",
    );
}

#[test]
fn rejects_empty_api_key() {
    let mut e = AssemblyAiEngine::new(String::new());
    let err = e
        .start(EngineConfig::Cloud {
            api_key: String::new(),
            language: None,
            sample_rate: 16_000,
        })
        .err()
        .expect("expected Err on empty key");
    assert!(err.to_string().contains("api_key"));
}

#[test]
fn rejects_non_16khz_sample_rate() {
    let mut e = AssemblyAiEngine::new("sk-x".into());
    let err = e
        .start(EngineConfig::Cloud {
            api_key: "sk-x".into(),
            language: None,
            sample_rate: 44_100,
        })
        .err()
        .expect("expected Err on wrong sample rate");
    assert!(err.to_string().contains("16 kHz"));
}
