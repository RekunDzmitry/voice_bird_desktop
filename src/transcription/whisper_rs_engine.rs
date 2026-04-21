use std::time::Duration;

use tokio::sync::{broadcast, mpsc, oneshot};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use super::{
    local_agreement::{step, AgreementOutput},
    EngineConfig, EngineEvent, EngineHandle, Token, TranscriptionEngine,
};

#[derive(Default)]
pub struct WhisperRsEngine;

impl TranscriptionEngine for WhisperRsEngine {
    fn start(&mut self, cfg: EngineConfig) -> anyhow::Result<EngineHandle> {
        let (pcm_tx, mut pcm_rx) = mpsc::channel::<Vec<f32>>(32);
        let (events_tx, events_rx) = broadcast::channel::<EngineEvent>(256);
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();

        let model_path = cfg.model_path.clone();
        let language = cfg.language.clone();
        let hop_ms = cfg.hop_ms as u64;
        let min_window_ms = cfg.min_window_ms as u64;

        std::thread::spawn(move || {
            let ctx = match WhisperContext::new_with_params(
                model_path.to_string_lossy().as_ref(),
                WhisperContextParameters::default(),
            ) {
                Ok(c) => c,
                Err(e) => {
                    let _ = events_tx.send(EngineEvent::Error(format!("load model: {e}")));
                    return;
                }
            };
            let _ = events_tx.send(EngineEvent::ModelLoaded {
                name: model_path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into(),
            });

            let mut buffer: Vec<f32> = Vec::new();
            let mut prev_hypothesis: Vec<Token> = Vec::new();
            let mut committed_upto = Duration::from_millis(0);
            let mut last_run = std::time::Instant::now()
                .checked_sub(std::time::Duration::from_millis(hop_ms))
                .unwrap_or_else(std::time::Instant::now);

            // Reuse one state across inferences — `create_state()` runs the
            // full Metal/ggml init, which is expensive and noisy. Combined
            // with `set_no_context(true)` below, reuse is safe: each call
            // decodes from scratch without the prior state's tokens.
            let mut state = match ctx.create_state() {
                Ok(s) => s,
                Err(e) => {
                    let _ = events_tx
                        .send(EngineEvent::Error(format!("create state: {e}")));
                    return;
                }
            };

            loop {
                if shutdown_rx.try_recv().is_ok() {
                    break;
                }

                // `end_of_stream` forces one final inference pass when the
                // PCM sender is dropped, so short utterances do not fall
                // through a closed window without ever running whisper.
                let mut end_of_stream = false;
                match pcm_rx.blocking_recv() {
                    Some(chunk) => buffer.extend_from_slice(&chunk),
                    None => end_of_stream = true,
                }

                // Cap buffer at 30 s
                let max = (16_000 * 30) as usize;
                if buffer.len() > max {
                    let cut = buffer.len() - max;
                    buffer.drain(..cut);
                    // Shift committed_upto by the cut amount (approximate)
                }

                let buf_ms = (buffer.len() as u64 * 1000) / 16_000;
                if buf_ms < min_window_ms {
                    if end_of_stream {
                        break;
                    }
                    continue;
                }
                if !end_of_stream
                    && last_run.elapsed() < std::time::Duration::from_millis(hop_ms)
                {
                    continue;
                }
                last_run = std::time::Instant::now();

                let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
                // true: each `full()` starts with a clean decode context,
                // which is what we want for a sliding-window re-transcribe.
                // Also makes state reuse safe (no KV carry-over).
                params.set_no_context(true);
                params.set_print_progress(false);
                params.set_print_realtime(false);
                params.set_print_special(false);
                params.set_print_timestamps(false);
                params.set_token_timestamps(true);
                if let Some(ref lang) = language {
                    params.set_language(Some(lang.as_str()));
                }

                // whisper.cpp rejects inputs shorter than ~1 s; pad the
                // tail with silence so we can transcribe short utterances.
                const WHISPER_MIN_SAMPLES: usize = 16_000 + 1_600; // 1.1 s
                let padded;
                let inference_input: &[f32] = if buffer.len() < WHISPER_MIN_SAMPLES {
                    padded = {
                        let mut v = Vec::with_capacity(WHISPER_MIN_SAMPLES);
                        v.extend_from_slice(&buffer);
                        v.resize(WHISPER_MIN_SAMPLES, 0.0);
                        v
                    };
                    &padded
                } else {
                    &buffer
                };

                let inf_start = std::time::Instant::now();
                if let Err(e) = state.full(params, inference_input) {
                    let _ = events_tx.send(EngineEvent::Error(format!("whisper full: {e}")));
                    continue;
                }
                let inf_ms = inf_start.elapsed().as_millis() as u64;
                let buf_ms_now = (inference_input.len() as u64 * 1000) / 16_000;
                // Ratio > 1.0 means inference is slower than real-time for
                // this buffer length — backlog will grow.
                log::info!(
                    "whisper-rs inference: buf={}ms took={}ms (rt_ratio={:.2})",
                    buf_ms_now,
                    inf_ms,
                    inf_ms as f64 / buf_ms_now.max(1) as f64
                );

                let n_segments = state.full_n_segments().unwrap_or(0);
                let mut hypothesis: Vec<Token> = Vec::new();
                for i in 0..n_segments {
                    let n_tokens = state.full_n_tokens(i).unwrap_or(0);
                    for t in 0..n_tokens {
                        let txt = match state.full_get_token_text(i, t) {
                            Ok(s) => s,
                            Err(_) => continue,
                        };
                        // Filter whisper special tokens like [_BEG_], [_TT_0], etc.
                        if txt.starts_with("[_") {
                            continue;
                        }
                        let data = match state.full_get_token_data(i, t) {
                            Ok(d) => d,
                            Err(_) => continue,
                        };
                        let t0 = data.t0.max(0) as u64;
                        let t1 = data.t1.max(0) as u64;
                        hypothesis.push(Token {
                            text: txt,
                            t_start_ms: t0 * 10, // whisper.cpp timestamps are in 10 ms units
                            t_end_ms: t1 * 10,
                        });
                    }
                }

                if end_of_stream {
                    // No future pass to cross-check against, so commit the
                    // entire remaining hypothesis as one segment. This
                    // catches short utterances where the hop never fires a
                    // second inference.
                    if !hypothesis.is_empty() {
                        let text = hypothesis
                            .iter()
                            .map(|t| t.text.trim())
                            .filter(|s| !s.is_empty())
                            .collect::<Vec<_>>()
                            .join(" ");
                        if !text.is_empty() {
                            let t_start = Duration::from_millis(
                                hypothesis.first().unwrap().t_start_ms,
                            );
                            let t_end = Duration::from_millis(
                                hypothesis.last().unwrap().t_end_ms.max(
                                    hypothesis.first().unwrap().t_end_ms,
                                ),
                            );
                            let seg = super::Segment {
                                t_start,
                                t_end,
                                text,
                                tokens: hypothesis.clone(),
                            };
                            let _ = events_tx.send(EngineEvent::Committed(seg));
                        }
                    }
                } else {
                    let out: AgreementOutput =
                        step(&prev_hypothesis, &hypothesis, committed_upto);
                    committed_upto = out.new_committed_upto;

                    for seg in out.committed_segments {
                        let _ = events_tx.send(EngineEvent::Committed(seg));
                    }
                    let _ = events_tx.send(EngineEvent::Tentative(out.tentative_text));
                }
                prev_hypothesis = hypothesis;

                // Trim buffer up to committed_upto - 200 ms to bound memory/compute.
                let keep_from_ms = committed_upto.as_millis().saturating_sub(200) as u64;
                let keep_from_samples = ((keep_from_ms * 16_000) / 1000) as usize;
                if keep_from_samples > 0 && keep_from_samples < buffer.len() {
                    buffer.drain(..keep_from_samples);
                    committed_upto =
                        committed_upto.saturating_sub(Duration::from_millis(keep_from_ms));
                    prev_hypothesis.clear(); // reset after trim
                }

                if end_of_stream {
                    break;
                }
            }
        });

        Ok(EngineHandle {
            pcm_tx,
            events_rx,
            shutdown: shutdown_tx,
        })
    }
}
