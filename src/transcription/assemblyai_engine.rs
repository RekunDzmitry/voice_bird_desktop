use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{
        client::IntoClientRequest,
        handshake::client::Request,
        http::HeaderValue,
        Message,
    },
};

use super::{EngineConfig, EngineEvent, EngineHandle, Segment, TranscriptionEngine};

/// AssemblyAI Universal-Streaming v3 engine. Opens a WebSocket to
/// `wss://streaming.assemblyai.com/v3/ws`, forwards 16-kHz mono PCM as
/// binary frames, and maps incoming JSON turns onto `EngineEvent`s.
pub struct AssemblyAiEngine {
    #[allow(dead_code)]
    api_key: String,
}

impl AssemblyAiEngine {
    pub fn new(api_key: String) -> Self {
        Self { api_key }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum AaiMessage {
    Begin {
        #[allow(dead_code)]
        session_id: String,
    },
    Turn {
        transcript: String,
        end_of_turn: bool,
        #[serde(default)]
        audio_start_ms: u64,
        #[serde(default)]
        audio_end_ms: u64,
    },
    Termination {},
    Error {
        error: String,
    },
    #[serde(other)]
    Unknown,
}

fn ws_url(sample_rate: u32, language: &Option<String>) -> String {
    if let Ok(override_url) = std::env::var("ASSEMBLYAI_WS_URL_OVERRIDE") {
        return format!(
            "{}?sample_rate={}&format_turns=true{}",
            override_url,
            sample_rate,
            language
                .as_deref()
                .map(|l| format!("&language_code={l}"))
                .unwrap_or_default(),
        );
    }
    format!(
        "wss://streaming.assemblyai.com/v3/ws?sample_rate={}&format_turns=true{}",
        sample_rate,
        language
            .as_deref()
            .map(|l| format!("&language_code={l}"))
            .unwrap_or_default(),
    )
}

impl TranscriptionEngine for AssemblyAiEngine {
    fn start(&mut self, cfg: EngineConfig) -> anyhow::Result<EngineHandle> {
        let (api_key, language, sample_rate) = match cfg {
            EngineConfig::Cloud {
                api_key,
                language,
                sample_rate,
            } => (api_key, language, sample_rate),
            EngineConfig::Local { .. } => {
                anyhow::bail!("AssemblyAiEngine requires EngineConfig::Cloud");
            }
        };

        if api_key.is_empty() {
            anyhow::bail!("AssemblyAiEngine: api_key is empty");
        }
        if sample_rate != 16_000 {
            anyhow::bail!("AssemblyAiEngine requires 16 kHz PCM; got {sample_rate}");
        }

        let (pcm_tx, mut pcm_rx) = mpsc::channel::<Vec<f32>>(32);
        let (events_tx, events_rx) = broadcast::channel::<EngineEvent>(256);
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();

        let url = ws_url(sample_rate, &language);

        tokio::spawn(async move {
            // Build request with Authorization header.
            let mut req: Request = match url.clone().into_client_request() {
                Ok(r) => r,
                Err(e) => {
                    let _ = events_tx.send(EngineEvent::Error(format!("bad url: {e}")));
                    return;
                }
            };
            let auth = match HeaderValue::from_str(&api_key) {
                Ok(h) => h,
                Err(_) => {
                    let _ = events_tx.send(EngineEvent::Error("invalid api key header".into()));
                    return;
                }
            };
            req.headers_mut().insert("Authorization", auth);

            let (mut ws, _resp) = match connect_async(req).await {
                Ok(pair) => pair,
                Err(e) => {
                    let _ = events_tx.send(EngineEvent::Error(format!("connect: {e}")));
                    return;
                }
            };

            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    maybe_pcm = pcm_rx.recv() => {
                        let Some(chunk) = maybe_pcm else { break; };
                        // ~50 ms frames at 16 kHz mono = 800 samples = 1600 bytes.
                        const FRAME_SAMPLES: usize = 800;
                        for win in chunk.chunks(FRAME_SAMPLES) {
                            let mut bytes = Vec::with_capacity(win.len() * 2);
                            for &s in win {
                                let clamped = s.clamp(-1.0, 1.0);
                                let i = (clamped * i16::MAX as f32) as i16;
                                bytes.extend_from_slice(&i.to_le_bytes());
                            }
                            if let Err(e) = ws.send(Message::Binary(bytes)).await {
                                let _ = events_tx.send(EngineEvent::Error(
                                    format!("ws send: {e}"),
                                ));
                                break;
                            }
                        }
                    }
                    maybe_msg = ws.next() => {
                        let Some(msg) = maybe_msg else { break; };
                        let msg = match msg {
                            Ok(m) => m,
                            Err(e) => {
                                let _ = events_tx.send(EngineEvent::Error(
                                    format!("ws recv: {e}"),
                                ));
                                break;
                            }
                        };
                        match msg {
                            Message::Text(txt) => {
                                match serde_json::from_str::<AaiMessage>(&txt) {
                                    Ok(AaiMessage::Begin { .. }) => {
                                        let _ = events_tx.send(EngineEvent::ModelLoaded {
                                            name: "assemblyai-universal-v3".into(),
                                        });
                                    }
                                    Ok(AaiMessage::Turn {
                                        transcript,
                                        end_of_turn,
                                        audio_start_ms,
                                        audio_end_ms,
                                    }) => {
                                        if end_of_turn {
                                            let seg = Segment {
                                                t_start: Duration::from_millis(audio_start_ms),
                                                t_end: Duration::from_millis(audio_end_ms),
                                                text: transcript,
                                                tokens: Vec::new(),
                                            };
                                            let _ = events_tx.send(EngineEvent::Committed(seg));
                                        } else {
                                            let _ = events_tx.send(
                                                EngineEvent::Tentative(transcript),
                                            );
                                        }
                                    }
                                    Ok(AaiMessage::Termination {}) => break,
                                    Ok(AaiMessage::Error { error }) => {
                                        let _ = events_tx.send(EngineEvent::Error(error));
                                        break;
                                    }
                                    Ok(AaiMessage::Unknown) => {}
                                    Err(_) => {}
                                }
                            }
                            Message::Close(_) => break,
                            _ => {}
                        }
                    }
                }
            }

            let _ = ws
                .send(Message::Text(r#"{"type":"Terminate"}"#.into()))
                .await;
            let _ = ws.close(None).await;
        });

        Ok(EngineHandle {
            pcm_tx,
            events_rx,
            shutdown: shutdown_tx,
        })
    }
}
