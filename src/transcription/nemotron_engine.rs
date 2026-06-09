use anyhow::Context;
use parakeet_rs::Nemotron;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, oneshot};

use super::{EngineConfig, EngineEvent, EngineHandle, Segment, Token, TranscriptionEngine};

const NEMOTRON_SAMPLE_RATE: usize = 16_000;
const NEMOTRON_CHUNK_SAMPLES: usize = 8_960; // 560 ms at 16 kHz.

pub struct NemotronEngine;

impl TranscriptionEngine for NemotronEngine {
    fn start(&mut self, cfg: EngineConfig) -> anyhow::Result<EngineHandle> {
        let (pcm_tx, mut pcm_rx) = mpsc::channel::<Vec<f32>>(32);
        let (events_tx, events_rx) = broadcast::channel::<EngineEvent>(256);
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();

        let (model_dir, language) = match cfg {
            EngineConfig::Local {
                model_path,
                language,
                sample_rate,
                ..
            } => {
                if sample_rate != NEMOTRON_SAMPLE_RATE as u32 {
                    anyhow::bail!("Nemotron requires 16 kHz PCM, got {sample_rate}");
                }
                (model_path, language)
            }
            EngineConfig::Cloud { .. } => anyhow::bail!("NemotronEngine requires local config"),
        };

        let mut model = Nemotron::from_pretrained(&model_dir, None)
            .with_context(|| format!("load Nemotron model from {}", model_dir.display()))?;
        if let Some(language) = language
            .as_deref()
            .filter(|s| !s.is_empty() && *s != "auto")
        {
            model
                .set_target_lang(language)
                .with_context(|| format!("set Nemotron target language to {language}"))?;
        }
        let _ = events_tx.send(EngineEvent::ModelLoaded {
            name: model_dir
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
        });

        let events_for_task = events_tx.clone();
        tokio::spawn(async move {
            let mut buffer = Vec::<f32>::new();
            let mut offset_samples = 0usize;
            let mut last_committed = String::new();

            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    chunk = pcm_rx.recv() => {
                        let Some(chunk) = chunk else { break };
                        buffer.extend_from_slice(&chunk);

                        while buffer.len() >= NEMOTRON_CHUNK_SAMPLES {
                            let audio: Vec<f32> = buffer.drain(..NEMOTRON_CHUNK_SAMPLES).collect();
                            match model.transcribe_chunk(&audio) {
                                Ok(text) => {
                                    let text = text.trim().to_string();
                                    if text.is_empty() {
                                        continue;
                                    }
                                    if text == last_committed {
                                        let _ = events_for_task.send(EngineEvent::Tentative(text));
                                        offset_samples += NEMOTRON_CHUNK_SAMPLES;
                                        continue;
                                    }

                                    let t_start_ms = ((offset_samples as u64) * 1000) / NEMOTRON_SAMPLE_RATE as u64;
                                    let t_end_ms = (((offset_samples + NEMOTRON_CHUNK_SAMPLES) as u64) * 1000)
                                        / NEMOTRON_SAMPLE_RATE as u64;
                                    let segment = Segment {
                                        t_start: Duration::from_millis(t_start_ms),
                                        t_end: Duration::from_millis(t_end_ms),
                                        text: text.clone(),
                                        tokens: vec![Token {
                                            text: text.clone(),
                                            t_start_ms,
                                            t_end_ms,
                                        }],
                                    };
                                    last_committed = text;
                                    let _ = events_for_task.send(EngineEvent::Committed(segment));
                                }
                                Err(e) => {
                                    let _ = events_for_task.send(EngineEvent::Error(format!(
                                        "nemotron transcribe: {e}"
                                    )));
                                }
                            }
                            offset_samples += NEMOTRON_CHUNK_SAMPLES;
                        }
                    }
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
