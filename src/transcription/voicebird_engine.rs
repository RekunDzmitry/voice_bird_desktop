use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, handshake::client::Request, Message},
};

use super::{EngineConfig, EngineEvent, EngineHandle, Segment, TranscriptionEngine};

/// Voice Bird Web cloud engine. Opens a WebSocket to the user-configured
/// `/api/audio/stream` endpoint of a voice_bird_web deployment, performs
/// the JSON `init` handshake, forwards 16-kHz mono PCM as binary frames
/// (PCM16LE), and maps transcript JSON messages onto `EngineEvent`s. The
/// voice_bird_web server in turn fans audio out to its configured
/// upstream transcription provider and pushes results back over the same
/// WebSocket — see voice_bird_web/server.ts and streamSessionRegistry.ts.
pub struct VoiceBirdEngine {
    api_key: String,
    server_url: String,
}

impl VoiceBirdEngine {
    pub fn new(api_key: String, server_url: String) -> Self {
        Self { api_key, server_url }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMessage {
    Connected {
        #[allow(dead_code)]
        #[serde(default)]
        message: Option<String>,
    },
    InitSuccess {
        #[allow(dead_code)]
        session_id: String,
        #[allow(dead_code)]
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        transcription_available: Option<bool>,
        #[serde(default)]
        transcription_error: Option<String>,
    },
    Transcript {
        #[serde(default)]
        text: String,
        #[serde(default)]
        is_final: bool,
        #[serde(default)]
        audio_start_ms: Option<u64>,
        #[serde(default)]
        audio_end_ms: Option<u64>,
    },
    TerminateSuccess {},
    Error {
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        code: Option<String>,
    },
    #[serde(other)]
    Unknown,
}

fn ws_url_override() -> Option<String> {
    std::env::var("VOICEBIRD_WS_URL_OVERRIDE").ok()
}

impl TranscriptionEngine for VoiceBirdEngine {
    fn start(&mut self, cfg: EngineConfig) -> anyhow::Result<EngineHandle> {
        let (api_key_cfg, language, sample_rate, server_url_cfg, device_name) = match cfg {
            EngineConfig::Cloud {
                api_key,
                language,
                sample_rate,
                server_url,
                device_name,
            } => (api_key, language, sample_rate, server_url, device_name),
            EngineConfig::Local { .. } => {
                anyhow::bail!("VoiceBirdEngine requires EngineConfig::Cloud");
            }
        };

        // Engine-level api_key/url passed at construction win over the
        // per-call config only if the latter is empty (preserves the same
        // pattern AssemblyAiEngine used for keys).
        let api_key = if api_key_cfg.is_empty() {
            self.api_key.clone()
        } else {
            api_key_cfg
        };
        let server_url = ws_url_override()
            .or_else(|| (!server_url_cfg.is_empty()).then(|| server_url_cfg.clone()))
            .unwrap_or_else(|| self.server_url.clone());

        if api_key.is_empty() {
            anyhow::bail!("VoiceBirdEngine: api_key is empty");
        }
        if server_url.is_empty() {
            anyhow::bail!("VoiceBirdEngine: server_url is empty");
        }
        if sample_rate != 16_000 {
            anyhow::bail!("VoiceBirdEngine requires 16 kHz PCM; got {sample_rate}");
        }

        let (pcm_tx, mut pcm_rx) = mpsc::channel::<Vec<f32>>(32);
        let (events_tx, events_rx) = broadcast::channel::<EngineEvent>(256);
        // The trait requires a `shutdown` oneshot in EngineHandle, but this
        // engine does not listen on it: `oneshot::Receiver` resolves with
        // `Err(RecvError)` the moment the matching `Sender` is dropped, and
        // callers (see `app.rs::start_recording`) routinely drop the handle
        // they get back without holding on to `.shutdown`. Listening here
        // would break the loop before the first PCM frame ever arrives,
        // sending `terminate` immediately and producing zero-byte sessions
        // on the server. PCM-channel closure (driven by `stop_recording`
        // aborting the producer task) is the actual shutdown signal —
        // matching the pattern used for the refinement engine in `app.rs`.
        let (shutdown_tx, _shutdown_rx) = oneshot::channel::<()>();

        let session_id = uuid::Uuid::new_v4().to_string();
        let lang_for_init = language.clone().unwrap_or_else(|| "en".into());

        tokio::spawn(async move {
            let req: Request = match server_url.clone().into_client_request() {
                Ok(r) => r,
                Err(e) => {
                    let _ = events_tx.send(EngineEvent::Error(format!("bad url: {e}")));
                    return;
                }
            };

            let (mut ws, _resp) = match connect_async(req).await {
                Ok(pair) => pair,
                Err(e) => {
                    let _ = events_tx.send(EngineEvent::Error(format!("connect: {e}")));
                    return;
                }
            };

            // Send the init message immediately. The server validates the
            // API key, sets up an upstream transcription session, and
            // replies with `init_success` (or an `error`).
            let init = serde_json::json!({
                "type": "init",
                "api_key": api_key,
                "session_id": session_id,
                "device_name": device_name,
                "device_type": "desktop",
                "app_name": env!("CARGO_PKG_NAME"),
                "sample_rate": sample_rate,
                "channels": 1,
                "audio_format": "pcm16le",
                "language": lang_for_init,
            });
            if let Err(e) = ws.send(Message::Text(init.to_string())).await {
                let _ = events_tx.send(EngineEvent::Error(format!("ws send init: {e}")));
                return;
            }

            // Wait for `init_success` before forwarding any PCM. The server's
            // init handler is async (validates the API key, initializes the
            // DB stream row, opens the upstream AssemblyAI socket) and
            // `ws.sessionMetadata` is only populated after that finishes.
            // Binary frames arriving during that window are rejected with
            // `Session not initialized. Send init message first.` and the
            // engine bails. Backpressure on `pcm_rx` (capacity 32) keeps the
            // producer from running away while we wait.
            let mut handshake_ok = false;
            while !handshake_ok {
                let Some(msg) = ws.next().await else { return; };
                let msg = match msg {
                    Ok(m) => m,
                    Err(e) => {
                        let _ = events_tx
                            .send(EngineEvent::Error(format!("ws recv: {e}")));
                        return;
                    }
                };
                match msg {
                    Message::Text(txt) => match serde_json::from_str::<ServerMessage>(&txt) {
                        Ok(ServerMessage::Connected { .. }) => {}
                        Ok(ServerMessage::InitSuccess {
                            transcription_available,
                            transcription_error,
                            ..
                        }) => {
                            if transcription_available == Some(false) {
                                let m = transcription_error.unwrap_or_else(|| {
                                    "transcription unavailable on server".into()
                                });
                                let _ = events_tx.send(EngineEvent::Error(m));
                                return;
                            }
                            let _ = events_tx.send(EngineEvent::ModelLoaded {
                                name: "voice-bird-web".into(),
                            });
                            handshake_ok = true;
                        }
                        Ok(ServerMessage::Error { message, code }) => {
                            let m = message.unwrap_or_else(|| {
                                code.unwrap_or_else(|| "server error".into())
                            });
                            let _ = events_tx.send(EngineEvent::Error(m));
                            return;
                        }
                        Ok(_) => {
                            log::warn!(
                                "voicebird ws: unexpected message before init_success — raw: {txt}"
                            );
                        }
                        Err(e) => {
                            log::warn!(
                                "voicebird ws: parse error during handshake: {e} — raw: {txt}"
                            );
                        }
                    },
                    Message::Close(_) => return,
                    _ => {}
                }
            }

            loop {
                tokio::select! {
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
                                match serde_json::from_str::<ServerMessage>(&txt) {
                                    Ok(ServerMessage::Connected { .. }) => {}
                                    Ok(ServerMessage::InitSuccess {
                                        transcription_available,
                                        transcription_error,
                                        ..
                                    }) => {
                                        if transcription_available == Some(false) {
                                            let msg = transcription_error.unwrap_or_else(|| {
                                                "transcription unavailable on server".into()
                                            });
                                            let _ = events_tx.send(EngineEvent::Error(msg));
                                            break;
                                        }
                                        let _ = events_tx.send(EngineEvent::ModelLoaded {
                                            name: "voice-bird-web".into(),
                                        });
                                    }
                                    Ok(ServerMessage::Transcript {
                                        text,
                                        is_final,
                                        audio_start_ms,
                                        audio_end_ms,
                                    }) => {
                                        if is_final {
                                            let seg = Segment {
                                                t_start: Duration::from_millis(
                                                    audio_start_ms.unwrap_or(0),
                                                ),
                                                t_end: Duration::from_millis(
                                                    audio_end_ms.unwrap_or(0),
                                                ),
                                                text,
                                                tokens: Vec::new(),
                                            };
                                            let _ = events_tx.send(EngineEvent::Committed(seg));
                                        } else {
                                            let _ = events_tx.send(EngineEvent::Tentative(text));
                                        }
                                    }
                                    Ok(ServerMessage::TerminateSuccess {}) => break,
                                    Ok(ServerMessage::Error { message, code }) => {
                                        let m = message.unwrap_or_else(|| {
                                            code.unwrap_or_else(|| "server error".into())
                                        });
                                        let _ = events_tx.send(EngineEvent::Error(m));
                                        break;
                                    }
                                    Ok(ServerMessage::Unknown) => {
                                        log::warn!(
                                            "voicebird ws: unknown server message shape — raw: {txt}"
                                        );
                                    }
                                    Err(e) => {
                                        log::warn!(
                                            "voicebird ws: parse error: {e} — raw: {txt}"
                                        );
                                    }
                                }
                            }
                            Message::Close(_) => break,
                            _ => {}
                        }
                    }
                }
            }

            let terminate = serde_json::json!({
                "type": "terminate",
                "session_id": session_id,
            });
            let _ = ws.send(Message::Text(terminate.to_string())).await;
            let _ = ws.close(None).await;
        });

        Ok(EngineHandle {
            pcm_tx,
            events_rx,
            shutdown: shutdown_tx,
        })
    }
}
