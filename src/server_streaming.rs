use anyhow::{Context, Result};
use console::style;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::mpsc;
use tokio::time::{sleep, Duration as TokioDuration};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use futures_util::{SinkExt, StreamExt};
use http::header::HeaderValue;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

/// Message sent to initialize a streaming session
#[derive(Debug, Serialize)]
struct InitMessage {
    #[serde(rename = "type")]
    message_type: String,
    api_key: String,
    session_id: String,
    device_name: String,
    sample_rate: u32,
    channels: u16,
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
}

/// Service for streaming audio to Voice Bird server
pub struct ServerStreamingService;

impl ServerStreamingService {
    /// Stream audio to Voice Bird server via WebSocket
    ///
    /// # Arguments
    /// * `server_url` - Base URL of the Voice Bird server (e.g., https://voice-bird-app.com)
    /// * `api_key` - User's API key for authentication
    /// * `session_id` - Unique identifier for this recording session
    /// * `device_name` - Name of the audio device being recorded
    /// * `audio_rx` - Channel receiver for f32 audio samples
    /// * `sample_rate` - Audio sample rate
    /// * `channels` - Number of audio channels
    pub async fn stream_to_server(
        server_url: String,
        api_key: String,
        session_id: String,
        device_name: String,
        audio_rx: mpsc::Receiver<Vec<f32>>,
        sample_rate: u32,
        channels: u16,
    ) -> Result<()> {
        // Log connection attempt (sanitize API key)
        let api_key_preview = if api_key.len() > 10 {
            format!("{}****", &api_key[..10])
        } else {
            "****".to_string()
        };
        println!(
            "{}",
            style(format!("🔗 Connecting to Voice Bird server...")).cyan()
        );
        println!(
            "{}",
            style(format!("   Server: {}", server_url)).dim()
        );
        println!(
            "{}",
            style(format!("   API Key: {}", api_key_preview)).dim()
        );
        println!(
            "{}",
            style(format!("   Session: {}", session_id)).dim()
        );

        // Convert HTTP(S) URL to WebSocket URL
        let ws_url = server_url
            .replace("https://", "wss://")
            .replace("http://", "ws://");

        // Construct WebSocket endpoint
        let full_ws_url = format!("{}/api/audio/stream", ws_url);

        println!(
            "{}",
            style(format!("   WebSocket: {}", full_ws_url)).dim()
        );

        // Create WebSocket request
        let mut request = full_ws_url
            .into_client_request()
            .context("Failed to create WebSocket request")?;

        // Add Authorization header
        let auth_header_value = HeaderValue::from_str(&api_key)
            .context("Failed to create Authorization header value")?;
        request
            .headers_mut()
            .insert("Authorization", auth_header_value);

        // Connect to WebSocket
        let (ws_stream, _response) = match connect_async(request).await {
            Ok(stream) => {
                println!("{}", style("✓ Connected to Voice Bird server!").green());
                stream
            }
            Err(e) => {
                eprintln!(
                    "{}",
                    style(format!("✗ Failed to connect to server: {}", e)).red()
                );
                eprintln!(
                    "{}",
                    style("  Make sure the server URL is correct and the server is running").yellow()
                );
                return Err(anyhow::anyhow!("WebSocket connection failed: {}", e));
            }
        };

        let (mut write, mut read) = ws_stream.split();

        // Send initialization message
        let init_msg = InitMessage {
            message_type: "init".to_string(),
            api_key: api_key.clone(),
            session_id: session_id.clone(),
            device_name: device_name.clone(),
            sample_rate,
            channels,
        };

        let init_json = serde_json::to_string(&init_msg)
            .context("Failed to serialize init message")?;

        if let Err(e) = write.send(Message::Text(init_json)).await {
            eprintln!(
                "{}",
                style(format!("✗ Failed to send init message: {}", e)).red()
            );
            return Err(anyhow::anyhow!("Failed to send init message: {}", e));
        }

        println!(
            "{}",
            style(format!("📡 Streaming started for device: {}", device_name)).cyan()
        );

        // Spawn task to handle server responses
        let session_id_clone = session_id.clone();
        let read_handle = tokio::spawn(async move {
            while let Some(msg) = read.next().await {
                match msg {
                    Ok(Message::Text(text)) => {
                        // Parse server response
                        if let Ok(response) = serde_json::from_str::<ServerResponse>(&text) {
                            match response.message_type.as_str() {
                                "connected" => {
                                    println!(
                                        "{}",
                                        style(format!("✓ Server acknowledged connection: {}", response.message))
                                            .green()
                                    );
                                }
                                "error" => {
                                    eprintln!(
                                        "{}",
                                        style(format!("✗ Server error: {}", response.error)).red()
                                    );
                                }
                                "transcription" => {
                                    // Server sent transcription result
                                    println!(
                                        "{}",
                                        style(format!("📝 Transcription: {}", response.message)).cyan()
                                    );
                                }
                                _ => {
                                    // Unknown message type, just log it
                                    println!(
                                        "{}",
                                        style(format!("📨 Server: {}", text)).dim()
                                    );
                                }
                            }
                        }
                    }
                    Ok(Message::Close(_)) => {
                        println!(
                            "{}",
                            style(format!("🔌 Server closed connection for session {}", session_id_clone))
                                .yellow()
                        );
                        break;
                    }
                    Err(e) => {
                        eprintln!(
                            "{}",
                            style(format!("✗ WebSocket error: {}", e)).red()
                        );
                        break;
                    }
                    _ => {}
                }
            }
        });

        // Send audio data to server
        let mut chunk_count = 0u64;
        let mut total_samples = 0usize;

        while let Ok(audio_chunk) = audio_rx.recv() {
            chunk_count += 1;
            total_samples += audio_chunk.len();

            // Convert f32 samples to bytes (little-endian)
            let bytes: Vec<u8> = audio_chunk
                .iter()
                .flat_map(|&sample| sample.to_le_bytes())
                .collect();

            // Send as binary WebSocket frame
            match write.send(Message::Binary(bytes)).await {
                Ok(_) => {
                    // Log progress periodically (every 100 chunks)
                    if chunk_count % 100 == 0 {
                        let duration_secs = total_samples as f32 / (sample_rate * channels as u32) as f32;
                        println!(
                            "{}",
                            style(format!(
                                "📊 Streamed {} chunks ({:.1}s of audio)",
                                chunk_count, duration_secs
                            ))
                            .dim()
                        );
                    }

                    // Small delay to prevent overwhelming the WebSocket
                    sleep(TokioDuration::from_millis(10)).await;
                }
                Err(e) => {
                    eprintln!(
                        "{}",
                        style(format!(
                            "✗ Failed to send audio chunk #{}: {}",
                            chunk_count, e
                        ))
                        .red()
                    );
                    break;
                }
            }
        }

        // Send termination message
        let terminate_msg = TerminateMessage {
            message_type: "terminate".to_string(),
            session_id: session_id.clone(),
        };

        let terminate_json = serde_json::to_string(&terminate_msg)
            .context("Failed to serialize terminate message")?;

        let _ = write.send(Message::Text(terminate_json)).await;

        println!(
            "{}",
            style(format!(
                "✓ Streaming completed for session {} ({} chunks)",
                session_id, chunk_count
            ))
            .green()
        );

        // Wait for read task to finish (with timeout)
        let _ = tokio::time::timeout(TokioDuration::from_secs(5), read_handle).await;

        Ok(())
    }
}
