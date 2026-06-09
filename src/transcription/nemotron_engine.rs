use std::time::Duration;

use parakeet_rs::{Nemotron, NemotronMode};
use tokio::sync::{broadcast, mpsc, oneshot};

use super::{EngineConfig, EngineEvent, EngineHandle, Segment, Token, TranscriptionEngine};

const NEMOTRON_SAMPLE_RATE: u32 = 16_000;
const NEMOTRON_CHUNK_SAMPLES: usize = 8_960;

#[derive(Default)]
pub struct NemotronEngine;

impl TranscriptionEngine for NemotronEngine {
    fn start(&mut self, cfg: EngineConfig) -> anyhow::Result<EngineHandle> {
        let (pcm_tx, mut pcm_rx) = mpsc::channel::<Vec<f32>>(32);
        let (events_tx, events_rx) = broadcast::channel::<EngineEvent>(256);
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();

        let (model_path, language) = match cfg {
            EngineConfig::Local {
                model_path,
                language,
                ..
            } => (model_path, language),
            EngineConfig::Cloud { .. } => {
                anyhow::bail!("NemotronEngine requires EngineConfig::Local")
            }
        };

        std::thread::spawn(move || {
            let mut model = match Nemotron::from_pretrained(&model_path, None) {
                Ok(model) => model,
                Err(e) => {
                    let _ = events_tx.send(EngineEvent::Error(format!("load Nemotron model: {e}")));
                    return;
                }
            };

            if let Some(lang) = language
                .as_deref()
                .filter(|s| !s.is_empty())
                .and_then(nemotron_language)
            {
                if model.mode() == NemotronMode::Multilingual {
                    if let Err(e) = model.set_target_lang(lang) {
                        let _ = events_tx.send(EngineEvent::Error(format!(
                            "set Nemotron language {lang}: {e}"
                        )));
                        return;
                    }
                }
            }

            model.reset();
            let _ = events_tx.send(EngineEvent::ModelLoaded {
                name: model_path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
            });

            let mut buffer = Vec::<f32>::new();
            let mut sample_cursor = 0usize;
            let mut last_text = String::new();

            loop {
                if shutdown_rx.try_recv().is_ok() {
                    break;
                }

                let end_of_stream = match pcm_rx.blocking_recv() {
                    Some(chunk) => {
                        buffer.extend_from_slice(&chunk);
                        false
                    }
                    None => true,
                };

                while buffer.len() >= NEMOTRON_CHUNK_SAMPLES {
                    let chunk: Vec<f32> = buffer.drain(..NEMOTRON_CHUNK_SAMPLES).collect();
                    if let Err(e) = transcribe_and_emit(
                        &mut model,
                        &events_tx,
                        &chunk,
                        &mut sample_cursor,
                        &mut last_text,
                    ) {
                        let _ = events_tx.send(EngineEvent::Error(format!("Nemotron chunk: {e}")));
                        return;
                    }
                }

                if end_of_stream {
                    if !buffer.is_empty() {
                        buffer.resize(NEMOTRON_CHUNK_SAMPLES, 0.0);
                        let _ = transcribe_and_emit(
                            &mut model,
                            &events_tx,
                            &buffer,
                            &mut sample_cursor,
                            &mut last_text,
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

fn transcribe_and_emit(
    model: &mut Nemotron,
    events_tx: &broadcast::Sender<EngineEvent>,
    chunk: &[f32],
    sample_cursor: &mut usize,
    last_text: &mut String,
) -> anyhow::Result<()> {
    let text = model.transcribe_chunk(chunk)?.trim().to_string();
    let start = *sample_cursor;
    *sample_cursor += chunk.len();

    if text.is_empty() || text == *last_text {
        let _ = events_tx.send(EngineEvent::Tentative(text));
        return Ok(());
    }

    let t_start_ms = samples_to_ms(start);
    let t_end_ms = samples_to_ms(*sample_cursor);
    let token = Token {
        text: text.clone(),
        t_start_ms,
        t_end_ms,
    };
    let segment = Segment {
        t_start: Duration::from_millis(t_start_ms),
        t_end: Duration::from_millis(t_end_ms),
        text: text.clone(),
        tokens: vec![token],
    };
    *last_text = text;
    let _ = events_tx.send(EngineEvent::Committed(segment));
    let _ = events_tx.send(EngineEvent::Tentative(String::new()));
    Ok(())
}

fn samples_to_ms(samples: usize) -> u64 {
    (samples as u64 * 1_000) / NEMOTRON_SAMPLE_RATE as u64
}

fn nemotron_language(language: &str) -> Option<&'static str> {
    match language {
        "auto" => Some("auto"),
        "en" => Some("en-US"),
        "es" => Some("es-ES"),
        "fr" => Some("fr-FR"),
        "de" => Some("de-DE"),
        "it" => Some("it-IT"),
        "pt" => Some("pt-BR"),
        "ja" => Some("ja-JP"),
        "zh" => Some("zh-CN"),
        "ru" => Some("ru-RU"),
        "pl" => Some("pl-PL"),
        _ if language.len() == 5 && language.as_bytes()[2] == b'-' => {
            Some(Box::leak(language.to_string().into_boxed_str()))
        }
        _ => None,
    }
}
