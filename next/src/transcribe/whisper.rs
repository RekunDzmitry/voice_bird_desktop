use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use parking_lot::Mutex;
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState,
};

use super::local_agreement::Token;
use super::streaming;

const MIN_INPUT: usize = 17_600;

/// Immutable native model weights shared by every decoder from this factory.
/// State allocation is deliberately outside the cache lock.
#[derive(Default)]
pub(super) struct WeightCache {
    contexts: Mutex<HashMap<PathBuf, Arc<WhisperContext>>>,
}

impl WeightCache {
    pub fn get(&self, model: &Path) -> Result<Arc<WhisperContext>> {
        let model = model.canonicalize().context("resolve whisper model path")?;
        #[cfg(target_os = "macos")]
        {
            let stem = model.file_stem().and_then(|stem| stem.to_str())
                .context("model stem is not valid UTF-8")?;
            let encoder = model.with_file_name(format!("{stem}-encoder.mlmodelc"));
            anyhow::ensure!(
                crate::transcription_models::coreml_bundle_present(&encoder),
                "required Core ML encoder is missing or incomplete: {}",
                encoder.display()
            );
        }
        let mut contexts = self.contexts.lock();
        if let Some(context) = contexts.get(&model) {
            return Ok(context.clone());
        }
        whisper_rs::install_whisper_log_trampoline();
        let path = model.to_str().context("model path is not valid UTF-8")?;
        let mut params = WhisperContextParameters::default();
        params.flash_attn(true);
        let context = Arc::new(
            WhisperContext::new_with_params(path, params)
                .with_context(|| format!("load whisper model {}", model.display()))?
        );
        contexts.insert(model, context.clone());
        Ok(context)
    }
}

/// Separate mutable inference state for each recording/pass, shared weights.
pub(super) struct Decoder {
    context: Arc<WhisperContext>,
    state: WhisperState,
    language: String,
    refined: bool,
    padded: Vec<f32>,
}

impl Decoder {
    pub fn open(context: Arc<WhisperContext>, language: &str, refined: bool) -> Result<Self> {
        #[cfg(target_os = "macos")]
        let state = super::coreml::create_state(&context)
            .context("create whisper state and load required encoder")?;
        #[cfg(not(target_os = "macos"))]
        let state = context.create_state().context("create whisper state")?;
        Ok(Self {
            context,
            state,
            language: language.to_owned(),
            refined,
            padded: Vec::with_capacity(MIN_INPUT),
        })
    }
}

impl streaming::Decoder for Decoder {
    fn decode(&mut self, pcm: &[f32], offset_ms: u64) -> Result<Vec<Token>> {
        if pcm.is_empty() {
            return Ok(Vec::new());
        }
        let strategy = if self.refined {
            SamplingStrategy::BeamSearch {
                beam_size: 5,
                patience: -1.0,
            }
        } else {
            SamplingStrategy::Greedy { best_of: 1 }
        };
        let mut params = FullParams::new(strategy);
        params.set_n_threads(2);
        params.set_no_context(true);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_special(false);
        params.set_print_timestamps(false);
        params.set_token_timestamps(true);
        params.set_language(Some(&self.language));
        let input = if pcm.len() < MIN_INPUT {
            self.padded.clear();
            self.padded.extend_from_slice(pcm);
            self.padded.resize(MIN_INPUT, 0.0);
            self.padded.as_slice()
        } else {
            pcm
        };
        self.state
            .full(params, input)
            .context("whisper inference")?;

        let end_ms = offset_ms + pcm.len() as u64 * 1000 / 16_000;
        let mut tokens: Vec<Token> = Vec::new();
        let mut bytes = Vec::new();
        let mut pending_start = offset_ms;
        let mut pending_end = offset_ms;
        for segment in 0..self
            .state
            .full_n_segments()
            .context("read whisper segment count")?
        {
            let segment_start = self
                .state
                .full_get_segment_t0(segment)
                .context("read segment start")?
                .max(0) as u64
                * 10;
            let segment_end = self
                .state
                .full_get_segment_t1(segment)
                .context("read segment end")?
                .max(0) as u64
                * 10;
            for index in 0..self
                .state
                .full_n_tokens(segment)
                .context("read whisper token count")?
            {
                let data = self
                    .state
                    .full_get_token_data(segment, index)
                    .context("read whisper token data")?;
                // Id filtering covers timestamp, language, task and EOT tokens,
                // not just the legacy string spelling of special tokens.
                if data.id >= self.context.token_eot() {
                    continue;
                }
                let piece = self
                    .context
                    .token_to_cstr(data.id)
                    .context("read whisper token bytes")?
                    .to_bytes();
                if piece.starts_with(b"[_") || piece.starts_with(b"<|") {
                    continue;
                }
                let start = offset_ms
                    + if data.t0 >= 0 {
                        data.t0 as u64 * 10
                    } else {
                        segment_start
                    };
                let end = offset_ms
                    + if data.t1 >= 0 {
                        data.t1 as u64 * 10
                    } else {
                        segment_end
                    };
                if bytes.is_empty() {
                    pending_start = start
                        .min(end_ms)
                        .max(tokens.last().map_or(offset_ms, |t| t.t_start_ms));
                    pending_end = pending_start;
                }
                pending_end = pending_end.max(end.min(end_ms)).max(pending_start);
                bytes.extend_from_slice(piece);
                match std::str::from_utf8(&bytes) {
                    Ok(_) => {
                        // Byte-level BPE can split a UTF-8 character between
                        // tokens. Keep its pieces together instead of failing
                        // or inserting replacement characters in non-English.
                        tokens.push(Token {
                            text: String::from_utf8(std::mem::take(&mut bytes))
                                .context("decode whisper token UTF-8")?,
                            t_start_ms: pending_start,
                            t_end_ms: pending_end,
                        });
                    }
                    Err(error) if error.error_len().is_none() => {}
                    Err(error) => return Err(error).context("decode whisper token UTF-8"),
                }
            }
        }
        anyhow::ensure!(bytes.is_empty(), "whisper returned incomplete UTF-8 token");
        Ok(tokens)
    }
}
