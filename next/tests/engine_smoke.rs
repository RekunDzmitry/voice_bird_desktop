#![cfg(feature = "local-engine")]

use std::io::Cursor;

use voice_bird_next::language::LANGUAGES;
use voice_bird_next::transcribe::{system_engines, TranscriptOut};
use voice_bird_next::transcription_models::{CacheDirStore, ModelStore};

/// Opt in with:
/// cargo test -p voice-bird-next --test engine_smoke -- --ignored --nocapture
/// Requires previously installed English models and encoder assets; never downloads.
#[test]
#[ignore = "requires installed English models/encoder assets and native whisper inference"]
fn installed_whisper_transcribes_bundled_speech_live_and_refined() {
    let store = CacheDirStore::new().expect("resolve installed model cache");
    let language = &LANGUAGES[0];
    for model in language.models() {
        assert!(store.is_available(model), "install {} before this opt-in smoke", model.id);
    }
    let wav = hound::WavReader::new(Cursor::new(include_bytes!(
        "../../tests/fixtures/hello_world_16k.wav"
    )))
    .expect("bundled WAV");
    let spec = wav.spec();
    assert_eq!(
        (spec.sample_rate, spec.channels, spec.bits_per_sample),
        (16_000, 1, 16)
    );
    let samples: Vec<f32> = wav
        .into_samples::<i16>()
        .map(|sample| sample.expect("valid PCM") as f32 / 32768.0)
        .collect();
    let duration_ms = samples.len() as u64 * 1000 / 16_000;
    let factory = system_engines();
    for (model, refined) in [(language.live, false), (language.refine, true)] {
        let path = store.installed_path(model);
        let mut engine = if refined {
            factory.refine(&path, language.code)
        } else {
            factory.live(&path, language.code)
        }
        .expect("load real whisper context");
        let mut output = Vec::new();
        for chunk in samples.chunks(8_000) {
            output.extend(engine.feed(chunk).expect("decode speech"));
        }
        output.extend(engine.finish().expect("flush speech tail"));
        let mut text = String::new();
        for event in output {
            if let TranscriptOut::Committed {
                t_start_ms,
                t_end_ms,
                text: segment,
            } = event
            {
                assert!(
                    t_start_ms <= t_end_ms && t_end_ms <= duration_ms,
                    "invalid absolute timestamps: {t_start_ms}..{t_end_ms} / {duration_ms}"
                );
                text.push_str(&segment);
                text.push(' ');
            }
        }
        println!(
            "{}: {}",
            if refined { "refined" } else { "live" },
            text.trim()
        );
        assert!(
            text.to_lowercase().contains("hello"),
            "expected fixture speech, got {text:?}"
        );
        assert!(
            engine.finish().expect("repeat finish").is_empty(),
            "finish duplicated committed speech"
        );
    }
}
