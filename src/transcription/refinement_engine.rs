use std::path::PathBuf;
use std::time::Duration;

use tokio::sync::{broadcast, mpsc, oneshot};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use super::{EngineEvent, EngineHandle, Segment, Token};

/// Background refinement engine. Runs on non-overlapping windows with
/// beam search for higher-quality transcripts than the real-time
/// streaming engine. Emits `Committed` segments only — no `Tentative`,
/// since refinement output replaces streaming output wholesale.
pub struct RefinementEngine {
    pub model_path: PathBuf,
    pub language: Option<String>,
    pub window_ms: u32,
    pub beam_size: u8,
}

impl RefinementEngine {
    pub fn start(self) -> anyhow::Result<EngineHandle> {
        let (pcm_tx, mut pcm_rx) = mpsc::channel::<Vec<f32>>(32);
        let (events_tx, events_rx) = broadcast::channel::<EngineEvent>(256);
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();

        let model_path = self.model_path;
        let language = self.language;
        let window_ms = self.window_ms.max(5_000) as u64;
        let beam_size = self.beam_size.max(1) as i32;
        // Carry a small overlap across chunks so sentences spanning a
        // window boundary aren't split mid-word.
        const OVERLAP_MS: u64 = 1_000;

        std::thread::spawn(move || {
            let ctx = match WhisperContext::new_with_params(
                model_path.to_string_lossy().as_ref(),
                WhisperContextParameters::default(),
            ) {
                Ok(c) => c,
                Err(e) => {
                    let _ = events_tx
                        .send(EngineEvent::Error(format!("refinement load model: {e}")));
                    return;
                }
            };
            let _ = events_tx.send(EngineEvent::ModelLoaded {
                name: format!(
                    "refine:{}",
                    model_path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                ),
            });

            let mut state = match ctx.create_state() {
                Ok(s) => s,
                Err(e) => {
                    let _ = events_tx.send(EngineEvent::Error(format!(
                        "refinement create state: {e}"
                    )));
                    return;
                }
            };

            let mut buffer: Vec<f32> = Vec::new();
            // Absolute session time of buffer[0].
            let mut abs_offset_ms: u64 = 0;

            loop {
                if shutdown_rx.try_recv().is_ok() {
                    break;
                }

                let mut end_of_stream = false;
                match pcm_rx.blocking_recv() {
                    Some(chunk) => buffer.extend_from_slice(&chunk),
                    None => end_of_stream = true,
                }

                let window_samples = ((window_ms * 16_000) / 1000) as usize;
                let overlap_samples = ((OVERLAP_MS * 16_000) / 1000) as usize;

                // Process as many full windows as the buffer holds. Each
                // pass emits one refined segment.
                while buffer.len() >= window_samples {
                    let take = (window_samples + overlap_samples).min(buffer.len());
                    let slice: Vec<f32> = buffer[..take].to_vec();

                    run_pass(
                        &mut state,
                        &slice,
                        abs_offset_ms,
                        // The refined segment's absolute range is the
                        // *committed* window, not the overlap tail.
                        abs_offset_ms + window_ms,
                        &language,
                        beam_size,
                        &events_tx,
                    );

                    buffer.drain(..window_samples);
                    abs_offset_ms += window_ms;
                }

                if end_of_stream {
                    // Flush whatever's left if it's long enough to be worth
                    // decoding (< 1 s will be rejected by whisper.cpp, and
                    // very short tails produce hallucinations).
                    const MIN_FLUSH_MS: u64 = 2_000;
                    let tail_ms = (buffer.len() as u64 * 1000) / 16_000;
                    if tail_ms >= MIN_FLUSH_MS {
                        let slice: Vec<f32> = buffer.clone();
                        run_pass(
                            &mut state,
                            &slice,
                            abs_offset_ms,
                            abs_offset_ms + tail_ms,
                            &language,
                            beam_size,
                            &events_tx,
                        );
                    }
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

#[allow(clippy::too_many_arguments)]
fn run_pass(
    state: &mut whisper_rs::WhisperState,
    input: &[f32],
    abs_t_start_ms: u64,
    abs_t_end_ms: u64,
    language: &Option<String>,
    beam_size: i32,
    events_tx: &broadcast::Sender<EngineEvent>,
) {
    let mut params = FullParams::new(SamplingStrategy::BeamSearch {
        beam_size,
        patience: -1.0,
    });
    params.set_no_context(true);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_special(false);
    params.set_print_timestamps(false);
    params.set_token_timestamps(true);
    if let Some(ref lang) = language {
        params.set_language(Some(lang.as_str()));
    }

    // whisper.cpp rejects < 1 s inputs.
    const WHISPER_MIN_SAMPLES: usize = 16_000 + 1_600;
    let padded;
    let inference_input: &[f32] = if input.len() < WHISPER_MIN_SAMPLES {
        padded = {
            let mut v = Vec::with_capacity(WHISPER_MIN_SAMPLES);
            v.extend_from_slice(input);
            v.resize(WHISPER_MIN_SAMPLES, 0.0);
            v
        };
        &padded
    } else {
        input
    };

    let inf_start = std::time::Instant::now();
    if let Err(e) = state.full(params, inference_input) {
        let _ = events_tx.send(EngineEvent::Error(format!("refinement full: {e}")));
        return;
    }
    let inf_ms = inf_start.elapsed().as_millis() as u64;
    let buf_ms_now = (inference_input.len() as u64 * 1000) / 16_000;
    log::info!(
        "refinement inference: buf={}ms took={}ms (rt_ratio={:.2})",
        buf_ms_now,
        inf_ms,
        inf_ms as f64 / buf_ms_now.max(1) as f64
    );

    let n_segments = state.full_n_segments().unwrap_or(0);
    let mut tokens: Vec<Token> = Vec::new();
    for i in 0..n_segments {
        let n_tokens = state.full_n_tokens(i).unwrap_or(0);
        for t in 0..n_tokens {
            let txt = match state.full_get_token_text(i, t) {
                Ok(s) => s,
                Err(_) => continue,
            };
            if txt.starts_with("[_") {
                continue;
            }
            let data = match state.full_get_token_data(i, t) {
                Ok(d) => d,
                Err(_) => continue,
            };
            let t0 = data.t0.max(0) as u64 * 10;
            let t1 = data.t1.max(0) as u64 * 10;
            tokens.push(Token {
                text: txt,
                t_start_ms: abs_t_start_ms + t0,
                t_end_ms: abs_t_start_ms + t1,
            });
        }
    }

    if tokens.is_empty() {
        return;
    }

    let text = tokens
        .iter()
        .map(|t| t.text.trim())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if text.is_empty() {
        return;
    }

    let seg = Segment {
        t_start: Duration::from_millis(abs_t_start_ms),
        t_end: Duration::from_millis(abs_t_end_ms),
        text,
        tokens,
    };
    let _ = events_tx.send(EngineEvent::Committed(seg));
}
