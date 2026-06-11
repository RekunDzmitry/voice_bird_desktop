//! End-to-end pipeline tests. Gated on the `engine-smoke` feature since
//! they download `tiny.en` (~75 MB) on first run and invoke real whisper.
//!
//! Run with: `cargo test --features engine-smoke --test e2e_pipeline`
//!
//! What these cover that unit tests do not:
//!   - Streaming engine → SegmentWriter → finalize → transcript.txt text.
//!   - Refinement engine emits a Committed on a windowed input.
//!   - Refinement engine flushes the tail when the PCM channel closes.
//!   - Streaming + refinement run in parallel on the same PCM stream and
//!     both produce their own JSONL output (the App's dual-engine layout).

// Local whisper engines don't exist on cloud-only Windows.
#![cfg(all(feature = "engine-smoke", not(windows)))]

use std::path::PathBuf;
use std::time::Duration;

use tempfile::TempDir;
use tokio::runtime::Builder;

use voice_bird_cli::session::finalize::{finalize, SessionMeta};
use voice_bird_cli::session::writer::SegmentWriter;
use voice_bird_cli::transcription::{
    refinement_engine::RefinementEngine, whisper_rs_engine::WhisperRsEngine, EngineConfig,
    EngineEvent, TranscriptionEngine,
};

const FIXTURE_WAV: &str = "tests/fixtures/hello_world_16k.wav";
const FIXTURE_KEYWORDS: &[&str] = &["hello", "world"];

fn load_fixture_pcm() -> Vec<f32> {
    let reader = hound::WavReader::open(FIXTURE_WAV).unwrap();
    reader
        .into_samples::<i16>()
        .map(|s| s.unwrap() as f32 / i16::MAX as f32)
        .collect()
}

/// Download tiny.en if not cached. Returns the local gguf path.
fn ensure_tiny_en() -> PathBuf {
    let cache = voice_bird_cli::transcription::models::gguf_path("tiny.en").unwrap();
    if !cache.exists() {
        let entry = voice_bird_cli::transcription::models::Catalog::builtin()
            .get("tiny.en")
            .unwrap()
            .clone();
        voice_bird_cli::transcription::models::download_model_with_verify(&entry, &mut |_, _| {})
            .unwrap();
    }
    cache
}

/// Fuzzy check: at least one of the expected keywords appears in `text`
/// (case-insensitive). Whisper output varies across models/params —
/// strict equality would be too brittle.
fn fuzzy_match(text: &str, expected: &[&str]) -> bool {
    let lower = text.to_lowercase();
    expected.iter().any(|kw| lower.contains(&kw.to_lowercase()))
}

/// Loop `pcm` enough times to cover at least `target_ms` milliseconds
/// at 16 kHz mono. Used to build inputs long enough for the refinement
/// engine's 20 s default window.
fn loop_to_duration_ms(pcm: &[f32], target_ms: u64) -> Vec<f32> {
    let target_samples = ((target_ms * 16_000) / 1000) as usize;
    let mut out = Vec::with_capacity(target_samples + pcm.len());
    while out.len() < target_samples {
        out.extend_from_slice(pcm);
    }
    out
}

#[test]
fn streaming_engine_writes_jsonl_and_finalizes_to_text() {
    let model = ensure_tiny_en();
    let samples = load_fixture_pcm();
    let dir = TempDir::new().unwrap();
    let jsonl = dir.path().join("transcript.jsonl");

    let rt = Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let mut engine = WhisperRsEngine::default();
        let handle = engine
            .start(EngineConfig::Local {
                model_path: model,
                language: Some("en".into()),
                sample_rate: 16_000,
                hop_ms: 750,
                min_window_ms: 1000,
            })
            .unwrap();

        // Feed in 500 ms chunks mimicking the producer cadence.
        for chunk in samples.chunks(8_000) {
            handle.pcm_tx.send(chunk.to_vec()).await.unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // Closing the channel triggers the end-of-stream flush.
        drop(handle.pcm_tx);

        let mut writer = SegmentWriter::open(&jsonl).unwrap();
        let mut rx = handle.events_rx;
        let mut got_commit = false;
        while let Ok(Ok(evt)) = tokio::time::timeout(Duration::from_secs(60), rx.recv()).await {
            if let EngineEvent::Committed(seg) = evt {
                writer.append(&(&seg).into()).unwrap();
                got_commit = true;
            }
        }
        drop(writer);
        assert!(got_commit, "streaming engine emitted no Committed events");

        finalize(
            &jsonl,
            &dir.path().join("transcript.json"),
            &dir.path().join("transcript.txt"),
            &dir.path().join("meta.json"),
            &SessionMeta::default(),
        )
        .unwrap();

        let txt = std::fs::read_to_string(dir.path().join("transcript.txt")).unwrap();
        assert!(
            fuzzy_match(&txt, FIXTURE_KEYWORDS),
            "transcript.txt = {:?}, expected any of {:?}",
            txt,
            FIXTURE_KEYWORDS
        );
    });
}

#[test]
fn refinement_engine_emits_committed_on_windowed_audio() {
    let model = ensure_tiny_en();
    let base = load_fixture_pcm();
    // Loop to 25 s so we cross the default 20 s window at least once.
    let samples = loop_to_duration_ms(&base, 25_000);

    let rt = Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let eng = RefinementEngine {
            model_path: model,
            language: Some("en".into()),
            // Shorter window so the test doesn't take too long. Still
            // large enough to exercise the windowed path (not just the
            // tail-flush path tested separately below).
            window_ms: 10_000,
            beam_size: 1,
        };
        let handle = eng.start().unwrap();

        for chunk in samples.chunks(16_000) {
            handle.pcm_tx.send(chunk.to_vec()).await.unwrap();
        }
        drop(handle.pcm_tx);

        let mut rx = handle.events_rx;
        let mut commits: Vec<String> = Vec::new();
        while let Ok(Ok(evt)) = tokio::time::timeout(Duration::from_secs(120), rx.recv()).await {
            if let EngineEvent::Committed(seg) = evt {
                commits.push(seg.text);
            }
        }

        assert!(
            !commits.is_empty(),
            "refinement engine emitted no Committed segments"
        );
        let joined = commits.join(" ");
        assert!(
            fuzzy_match(&joined, FIXTURE_KEYWORDS),
            "refined text = {:?}, expected any of {:?}",
            joined,
            FIXTURE_KEYWORDS
        );
    });
}

#[test]
fn refinement_engine_flushes_tail_when_pcm_channel_closes() {
    let model = ensure_tiny_en();
    // The engine skips tail flushes < 2 s to dodge whisper hallucinations;
    // loop the 1.5 s fixture to ~3 s so the tail-flush branch fires.
    let samples = loop_to_duration_ms(&load_fixture_pcm(), 3_000);

    let rt = Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let eng = RefinementEngine {
            model_path: model,
            language: Some("en".into()),
            // Window deliberately larger than the input — the only way
            // a segment can be emitted is via the end-of-stream flush.
            window_ms: 20_000,
            beam_size: 1,
        };
        let handle = eng.start().unwrap();

        for chunk in samples.chunks(8_000) {
            handle.pcm_tx.send(chunk.to_vec()).await.unwrap();
        }
        drop(handle.pcm_tx);

        let mut rx = handle.events_rx;
        let mut commits: Vec<String> = Vec::new();
        while let Ok(Ok(evt)) = tokio::time::timeout(Duration::from_secs(60), rx.recv()).await {
            if let EngineEvent::Committed(seg) = evt {
                commits.push(seg.text);
            }
        }

        assert!(
            !commits.is_empty(),
            "refinement tail flush emitted no Committed segments"
        );
        let joined = commits.join(" ");
        assert!(
            fuzzy_match(&joined, FIXTURE_KEYWORDS),
            "tail-flush text = {:?}, expected any of {:?}",
            joined,
            FIXTURE_KEYWORDS
        );
    });
}

#[test]
fn dual_pipeline_writes_both_jsonls_in_parallel() {
    let model = ensure_tiny_en();
    let base = load_fixture_pcm();
    // 15 s of audio — enough to let both engines emit, and to cross
    // the 10 s refinement window at least once.
    let samples = loop_to_duration_ms(&base, 15_000);

    let dir = TempDir::new().unwrap();
    let streaming_jsonl = dir.path().join("transcript.jsonl");
    let refined_jsonl = dir.path().join("transcript.refined.jsonl");

    let rt = Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut streaming = WhisperRsEngine::default();
        let streaming_handle = streaming
            .start(EngineConfig::Local {
                model_path: model.clone(),
                language: Some("en".into()),
                sample_rate: 16_000,
                hop_ms: 750,
                min_window_ms: 1000,
            })
            .unwrap();

        let refinement = RefinementEngine {
            model_path: model,
            language: Some("en".into()),
            window_ms: 10_000,
            beam_size: 1,
        };
        let refinement_handle = refinement.start().unwrap();

        // Move (don't clone) so when the producer drops them, the
        // engines' pcm_rx observes a closed channel and runs its
        // end-of-stream path. A clone would leave the handle still
        // holding a sender and the engines would idle forever.
        let streaming_tx = streaming_handle.pcm_tx;
        let refinement_tx = refinement_handle.pcm_tx;

        // Producer: tee PCM to both engines, matching what App does.
        let producer = tokio::spawn(async move {
            for chunk in samples.chunks(8_000) {
                let v = chunk.to_vec();
                // Send to refinement first (larger buffer, less likely
                // to fall behind) so we don't double the latency.
                let _ = refinement_tx.send(v.clone()).await;
                let _ = streaming_tx.send(v).await;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            drop(streaming_tx);
            drop(refinement_tx);
        });

        // Streaming consumer.
        let mut streaming_rx = streaming_handle.events_rx;
        let streaming_path = streaming_jsonl.clone();
        let streaming_consumer = tokio::spawn(async move {
            let mut writer = SegmentWriter::open(&streaming_path).unwrap();
            let mut count = 0;
            while let Ok(Ok(evt)) =
                tokio::time::timeout(Duration::from_secs(60), streaming_rx.recv()).await
            {
                if let EngineEvent::Committed(seg) = evt {
                    writer.append(&(&seg).into()).unwrap();
                    count += 1;
                }
            }
            count
        });

        // Refinement consumer.
        let mut refinement_rx = refinement_handle.events_rx;
        let refined_path = refined_jsonl.clone();
        let refinement_consumer = tokio::spawn(async move {
            let mut writer = SegmentWriter::open(&refined_path).unwrap();
            let mut count = 0;
            while let Ok(Ok(evt)) =
                tokio::time::timeout(Duration::from_secs(120), refinement_rx.recv()).await
            {
                if let EngineEvent::Committed(seg) = evt {
                    writer.append(&(&seg).into()).unwrap();
                    count += 1;
                }
            }
            count
        });

        let _ = producer.await;
        let streaming_count = streaming_consumer.await.unwrap();
        let refinement_count = refinement_consumer.await.unwrap();

        assert!(
            streaming_count > 0,
            "streaming engine produced 0 committed segments"
        );
        assert!(
            refinement_count > 0,
            "refinement engine produced 0 committed segments"
        );

        // Both JSONLs exist and are non-empty.
        for path in [&streaming_jsonl, &refined_jsonl] {
            let bytes = std::fs::read(path).unwrap();
            assert!(!bytes.is_empty(), "{:?} was empty", path);
        }
    });
}
