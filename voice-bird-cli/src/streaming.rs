use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::mpsc;
use tokio::time::Duration as TokioDuration;
use tokio_tungstenite::{connect_async_with_config, tungstenite::Message};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use futures_util::{SinkExt, StreamExt};
use http::header::HeaderValue;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio::sync::mpsc as tokio_mpsc;
use tokio::sync::oneshot;

use crate::audio::AudioConverter;

/// Usage information returned by the server
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct UsageInfo {
    #[serde(default)]
    pub seconds_remaining: f64,
    #[serde(default)]
    pub seconds_limit: f64,
    #[serde(default)]
    pub seconds_used: f64,
    #[serde(default)]
    pub plan: String,
}

/// Successful init response from server
#[derive(Debug, Clone)]
pub struct InitSuccess {
    pub usage: Option<UsageInfo>,
}

/// Errors that can occur during stream initialization
#[derive(Debug, Clone)]
pub enum StreamError {
    QuotaExceeded { message: String, usage: Option<UsageInfo> },
    InvalidApiKey { message: String },
    ConnectionFailed { message: String },
    InitTimeout,
    Other { message: String },
}

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StreamError::QuotaExceeded { message, .. } => write!(f, "{}", message),
            StreamError::InvalidApiKey { message } => write!(f, "{}", message),
            StreamError::ConnectionFailed { message } => write!(f, "{}", message),
            StreamError::InitTimeout => write!(f, "Session initialization timeout"),
            StreamError::Other { message } => write!(f, "{}", message),
        }
    }
}

/// Message sent to initialize a streaming session
#[derive(Debug, Serialize)]
struct InitMessage {
    #[serde(rename = "type")]
    message_type: String,
    api_key: String,
    session_id: String,
    device_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    device_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    app_name: Option<String>,
    sample_rate: u32,
    channels: u16,
    audio_format: String,
}

/// Message sent to terminate a streaming session
#[derive(Debug, Serialize)]
struct TerminateMessage {
    #[serde(rename = "type")]
    message_type: String,
    session_id: String,
}

/// Server response messages
#[derive(Debug, Deserialize)]
struct ServerResponse {
    #[serde(rename = "type")]
    message_type: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    error: String,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    usage: Option<UsageInfo>,
}

/// Stream audio to Voice Bird server via WebSocket
#[allow(clippy::too_many_arguments)]
pub async fn stream_to_server(
    server_url: String,
    api_key: String,
    session_id: String,
    device_name: String,
    app_name: Option<String>,
    is_input: bool,
    audio_rx: mpsc::Receiver<Vec<f32>>,
    sample_rate: u32,
    channels: u16,
    init_result_tx: mpsc::Sender<Result<InitSuccess, StreamError>>,
) -> Result<()> {
    // Convert HTTP(S) URL to WebSocket URL
    let ws_url = server_url
        .replace("https://", "wss://")
        .replace("http://", "ws://");

    let full_ws_url = format!("{}/api/audio/stream", ws_url);

    // Create WebSocket request
    let mut request = full_ws_url
        .into_client_request()
        .context("Failed to create WebSocket request")?;

    // Add Authorization header
    let auth_header_value = HeaderValue::from_str(&api_key)
        .context("Failed to create Authorization header value")?;
    request.headers_mut().insert("Authorization", auth_header_value);
    request.headers_mut().remove("Sec-WebSocket-Extensions");

    // Configure WebSocket
    let ws_config = WebSocketConfig {
        max_message_size: Some(10 * 1024 * 1024),
        max_frame_size: Some(10 * 1024 * 1024),
        max_write_buffer_size: 10 * 1024 * 1024,
        accept_unmasked_frames: false,
        ..Default::default()
    };

    // Connect with timeout
    let connection_result = tokio::time::timeout(
        TokioDuration::from_secs(10),
        connect_async_with_config(request, Some(ws_config), false),
    ).await;

    let (ws_stream, _response) = match connection_result {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => {
            let err = StreamError::ConnectionFailed { message: format!("WebSocket connection failed: {}", e) };
            let _ = init_result_tx.send(Err(err));
            return Err(anyhow::anyhow!("WebSocket connection failed: {}", e));
        }
        Err(_) => {
            let err = StreamError::ConnectionFailed { message: "Connection timeout".to_string() };
            let _ = init_result_tx.send(Err(err));
            return Err(anyhow::anyhow!("Connection timeout"));
        }
    };

    let (mut write, mut read) = ws_stream.split();

    // Create audio converter (48kHz stereo -> 16kHz mono)
    let audio_converter = AudioConverter::new(sample_rate, channels, 16000);
    let output_sample_rate = audio_converter.output_sample_rate();

    // Send initialization message
    let init_msg = InitMessage {
        message_type: "init".to_string(),
        api_key: api_key.clone(),
        session_id: session_id.clone(),
        device_name: device_name.clone(),
        device_type: Some(if is_input { "input" } else { "output" }.to_string()),
        app_name,
        sample_rate: output_sample_rate,
        channels: 1,
        audio_format: "pcm16le".to_string(),
    };

    let init_json = serde_json::to_string(&init_msg)
        .context("Failed to serialize init message")?;

    write.send(Message::Text(init_json)).await
        .context("Failed to send init message")?;

    // Create channels for coordination
    let (pong_tx, mut pong_rx) = tokio_mpsc::unbounded_channel::<Vec<u8>>();

    // Internal oneshot carries full server response info for init
    let (init_ack_tx, init_ack_rx) = oneshot::channel::<Result<InitSuccess, StreamError>>();
    let init_ack_tx = std::sync::Arc::new(std::sync::Mutex::new(Some(init_ack_tx)));
    let init_ack_tx_clone = init_ack_tx.clone();

    // Spawn task to handle server responses
    let session_id_clone = session_id.clone();
    let _read_handle = tokio::spawn(async move {
        while let Some(msg) = read.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    if let Ok(response) = serde_json::from_str::<ServerResponse>(&text) {
                        match response.message_type.as_str() {
                            "init_success" => {
                                if let Ok(mut guard) = init_ack_tx_clone.lock() {
                                    if let Some(tx) = guard.take() {
                                        let _ = tx.send(Ok(InitSuccess {
                                            usage: response.usage,
                                        }));
                                    }
                                }
                            }
                            "error" => {
                                let error_msg = if !response.message.is_empty() {
                                    response.message.clone()
                                } else {
                                    response.error.clone()
                                };
                                let stream_err = match response.code.as_deref() {
                                    Some("QUOTA_EXCEEDED") => StreamError::QuotaExceeded {
                                        message: error_msg,
                                        usage: response.usage,
                                    },
                                    Some("INVALID_API_KEY") => StreamError::InvalidApiKey {
                                        message: error_msg,
                                    },
                                    _ => StreamError::Other {
                                        message: error_msg,
                                    },
                                };
                                if let Ok(mut guard) = init_ack_tx_clone.lock() {
                                    if let Some(tx) = guard.take() {
                                        let _ = tx.send(Err(stream_err));
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                Ok(Message::Ping(payload)) => {
                    let _ = pong_tx.send(payload);
                }
                Ok(Message::Close(_)) => {
                    break;
                }
                Err(_) => {
                    break;
                }
                _ => {}
            }
        }
        drop(session_id_clone);
    });

    // Wait for server to confirm init
    let init_result = match tokio::time::timeout(TokioDuration::from_secs(10), init_ack_rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err(StreamError::Other { message: "Init acknowledgment channel closed".to_string() }),
        Err(_) => Err(StreamError::InitTimeout),
    };

    // Report init result to the UI thread
    match init_result {
        Ok(ref success) => {
            let _ = init_result_tx.send(Ok(success.clone()));
        }
        Err(ref err) => {
            let _ = init_result_tx.send(Err(err.clone()));
            return Err(anyhow::anyhow!("{}", err));
        }
    }

    // Main audio streaming loop
    loop {
        // Handle pong responses
        while let Ok(pong_payload) = pong_rx.try_recv() {
            if write.send(Message::Pong(pong_payload)).await.is_err() {
                break;
            }
        }

        // Receive and send audio
        match audio_rx.recv_timeout(std::time::Duration::from_millis(10)) {
            Ok(audio_chunk) => {
                let converted_bytes = audio_converter.convert(&audio_chunk);

                // Prepend timestamp
                let timestamp_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                let mut packet = timestamp_ms.to_le_bytes().to_vec();
                packet.extend(&converted_bytes);

                if write.send(Message::Binary(packet)).await.is_err() {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                break;
            }
        }
    }

    // Send termination message
    let terminate_msg = TerminateMessage {
        message_type: "terminate".to_string(),
        session_id,
    };

    let terminate_json = serde_json::to_string(&terminate_msg)?;
    let _ = write.send(Message::Text(terminate_json)).await;

    Ok(())
}
