use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::sync::mpsc;
use tokio::time::{sleep, Duration as TokioDuration};
use tokio_tungstenite::{connect_async_with_config, tungstenite::Message};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use futures_util::{SinkExt, StreamExt};
use http::header::HeaderValue;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio::sync::mpsc as tokio_mpsc; // For async channel communication

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
        log::info!("Connecting to Voice Bird server...");
        log::debug!("   Server: {}", server_url);
        log::debug!("   API Key: {}", api_key_preview);
        log::debug!("   Session: {}", session_id);

        // Convert HTTP(S) URL to WebSocket URL
        let ws_url = server_url
            .replace("https://", "wss://")
            .replace("http://", "ws://");

        // Construct WebSocket endpoint
        let full_ws_url = format!("{}/api/audio/stream", ws_url);

        log::debug!("   WebSocket: {}", full_ws_url);

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

        // CRITICAL FIX: Remove Sec-WebSocket-Extensions header to prevent compression negotiation
        // This ensures no compression is requested even if the library tries to add it
        request.headers_mut().remove("Sec-WebSocket-Extensions");

        log::warn!("Manually removed Sec-WebSocket-Extensions header (prevents compression)");

        // Connect to WebSocket with timeout (10 seconds)
        log::debug!("Attempting WebSocket connection...");

        // Configure WebSocket to match server settings
        // CRITICAL: Server has perMessageDeflate: false (compression disabled)
        // Using default-features = false in Cargo.toml disables compression support
        let ws_config = WebSocketConfig {
            max_message_size: Some(10 * 1024 * 1024), // 10MB - match server limit
            max_frame_size: Some(10 * 1024 * 1024),   // 10MB
            max_write_buffer_size: 10 * 1024 * 1024,  // 10MB write buffer
            accept_unmasked_frames: false,
            ..Default::default()
        };

        log::debug!("WebSocket Config:");
        log::debug!("   Max message size: {} bytes", ws_config.max_message_size.unwrap_or(0));
        log::debug!("   Max frame size: {} bytes", ws_config.max_frame_size.unwrap_or(0));
        log::debug!("   Write buffer size: {} bytes", ws_config.max_write_buffer_size);
        log::debug!("   Compression: DISABLED (via default-features = false)");

        let connection_result = tokio::time::timeout(
            TokioDuration::from_secs(10),
            connect_async_with_config(request, Some(ws_config), false) // false = disable Nagle's algorithm
        ).await;

        let (ws_stream, _response) = match connection_result {
            Ok(Ok(stream)) => {
                log::info!("Connected to Voice Bird server!");

                // Log HTTP response details for debugging
                log::debug!("HTTP Response: {}", stream.1.status());

                // Check for WebSocket extension headers (especially compression)
                if let Some(extensions) = stream.1.headers().get("Sec-WebSocket-Extensions") {
                    log::debug!("   Extensions negotiated: {:?}", extensions);
                } else {
                    log::debug!("   Extensions: None (compression disabled)");
                }

                // Log protocol if present
                if let Some(protocol) = stream.1.headers().get("Sec-WebSocket-Protocol") {
                    log::debug!("   Protocol: {:?}", protocol);
                }

                stream
            }
            Ok(Err(e)) => {
                // WebSocket error (connection refused, invalid response, etc.)
                log::error!("WebSocket connection failed: {}", e);
                log::error!("Possible causes:");
                log::error!("  1. Server endpoint '/api/audio/stream' doesn't exist");
                log::error!("  2. Server is not running or unreachable");
                log::error!("  3. Server rejected the WebSocket upgrade request");
                log::error!("  4. Network/firewall blocking the connection");
                log::info!("Troubleshooting:");
                log::info!("  - Verify server is running at: {}", server_url);
                log::info!("  - Check server logs for incoming connection attempts");
                log::info!("  - Ensure WebSocket endpoint is implemented and accessible");
                log::info!("  - Test with: curl -i -N -H 'Connection: Upgrade' ...");
                return Err(anyhow::anyhow!("WebSocket connection failed: {}", e));
            }
            Err(_) => {
                // Timeout
                log::error!("Connection timeout (10 seconds)");
                log::warn!("The server did not respond within 10 seconds.");
                log::warn!("Most likely cause:");
                log::warn!("  → The WebSocket endpoint '/api/audio/stream' is NOT implemented on the server");
                log::info!("Action required:");
                log::info!("  1. Check if your server has a WebSocket handler at '/api/audio/stream'");
                log::info!("  2. See SERVER_ENDPOINT_SPEC.md for implementation requirements");
                log::info!("  3. Verify server logs show NO incoming connection attempts");
                return Err(anyhow::anyhow!("Connection timeout - server endpoint may not exist"));
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

        log::debug!("Sending init message ({} bytes)...", init_json.len());
        log::debug!("   Session ID: {}", session_id);
        log::debug!("   Sample Rate: {} Hz", sample_rate);
        log::debug!("   Channels: {}", channels);

        if let Err(e) = write.send(Message::Text(init_json.clone())).await {
            log::error!("Failed to send init message: {}", e);
            log::error!("   Error type: {:?}", e);
            return Err(anyhow::anyhow!("Failed to send init message: {}", e));
        }

        log::info!("Init message sent successfully");

        log::info!("Streaming started for device: {}", device_name);

        // Create channel for sending pong responses from read task to write loop
        let (pong_tx, mut pong_rx) = tokio_mpsc::unbounded_channel::<Vec<u8>>();

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
                                    log::info!("Server acknowledged connection: {}", response.message);
                                }
                                "error" => {
                                    log::error!("Server error: {}", response.error);
                                }
                                "transcription" => {
                                    // Server sent transcription result
                                    log::info!("Transcription: {}", response.message);
                                }
                                _ => {
                                    // Unknown message type, just log it
                                    log::debug!("Server: {}", text);
                                }
                            }
                        }
                    }
                    Ok(Message::Ping(payload)) => {
                        // Server sent ping - send pong response via channel to keep connection alive
                        log::debug!("Received ping from server, responding with pong");
                        if let Err(e) = pong_tx.send(payload) {
                            log::error!("Failed to send pong via channel: {}", e);
                        }
                    }
                    Ok(Message::Close(_)) => {
                        log::warn!("Server closed connection for session {}", session_id_clone);
                        break;
                    }
                    Err(e) => {
                        log::error!("WebSocket error: {}", e);
                        log::error!("   Error details: {:?}", e);

                        // Provide specific guidance for common errors
                        let error_msg = format!("{}", e);
                        if error_msg.contains("Reserved bits") {
                            log::warn!("COMPRESSION MISMATCH DETECTED");
                            log::warn!("   This error indicates the client and server have mismatched compression settings.");
                            log::warn!("   Server has: perMessageDeflate: false (compression disabled)");
                            log::warn!("   Action: Check 'Sec-WebSocket-Extensions' header in connection log above");
                        }
                        break;
                    }
                    _ => {}
                }
            }
        });

        // Send audio data to server
        let mut chunk_count = 0u64;
        let mut total_samples = 0usize;
        let mut total_bytes_sent = 0u64;
        let mut pong_count = 0u32;
        let start_time = std::time::Instant::now();

        log::info!("Beginning audio streaming loop...");

        // Main loop: send audio chunks and respond to pings
        loop {
            // Check for pending pong responses (non-blocking)
            while let Ok(pong_payload) = pong_rx.try_recv() {
                if let Err(e) = write.send(Message::Pong(pong_payload)).await {
                    log::error!("Failed to send pong: {}", e);
                    break;
                }
                pong_count += 1;
                if pong_count % 5 == 0 {
                    log::debug!("Sent {} pong responses to keep connection alive", pong_count);
                }
            }

            // Try to receive audio chunk with timeout to allow pong handling
            match audio_rx.recv_timeout(std::time::Duration::from_millis(50)) {
                Ok(audio_chunk) => {
                    chunk_count += 1;
                    total_samples += audio_chunk.len();

                    // Convert f32 samples to bytes (little-endian)
                    let bytes: Vec<u8> = audio_chunk
                        .iter()
                        .flat_map(|&sample| sample.to_le_bytes())
                        .collect();

                    let bytes_len = bytes.len();
                    total_bytes_sent += bytes_len as u64;

                    // Log every chunk for first 10 chunks, then every 100 chunks
                    let should_log = chunk_count <= 10 || chunk_count % 100 == 0;

                    if should_log {
                        let elapsed = start_time.elapsed();
                        let duration_secs = total_samples as f32 / (sample_rate * channels as u32) as f32;
                        log::debug!(
                            "Chunk #{}: {} samples ({} bytes) | Total: {:.1}s audio, {:.2} MB sent | Elapsed: {:.1}s",
                            chunk_count,
                            audio_chunk.len(),
                            bytes_len,
                            duration_secs,
                            total_bytes_sent as f64 / 1_000_000.0,
                            elapsed.as_secs_f32()
                        );
                    }

                    // Send as binary WebSocket frame
                    match write.send(Message::Binary(bytes)).await {
                        Ok(_) => {
                            if should_log {
                                log::debug!("   Chunk #{} sent successfully", chunk_count);
                            }

                            // Small delay to prevent overwhelming the WebSocket
                            sleep(TokioDuration::from_millis(10)).await;
                        }
                        Err(e) => {
                            let elapsed = start_time.elapsed();
                            log::error!(
                                "Failed to send audio chunk #{}: {}",
                                chunk_count, e
                            );
                            log::error!("   Error details: {:?}", e);
                            log::error!(
                                "   Sent {} chunks ({:.1}s of audio, {:.2} MB) before failure",
                                chunk_count - 1,
                                (total_samples - audio_chunk.len()) as f32 / (sample_rate * channels as u32) as f32,
                                (total_bytes_sent - bytes_len as u64) as f64 / 1_000_000.0
                            );
                            log::error!("   Elapsed time: {:.1}s", elapsed.as_secs_f32());
                            break;
                        }
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // No audio chunk available, continue to check for pongs
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    // Audio sender disconnected, exit loop
                    let elapsed = start_time.elapsed();
                    let duration_secs = total_samples as f32 / (sample_rate * channels as u32) as f32;

                    log::info!("Audio stream ended, finalizing...");
                    log::info!("   Total chunks sent: {}", chunk_count);
                    log::info!("   Total audio duration: {:.1}s", duration_secs);
                    log::info!(
                        "   Total bytes sent: {:.2} MB",
                        total_bytes_sent as f64 / 1_000_000.0
                    );
                    log::info!("   Elapsed time: {:.1}s", elapsed.as_secs_f32());
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

        log::debug!("Sending terminate message ({} bytes)...", terminate_json.len());

        let _ = write.send(Message::Text(terminate_json)).await;

        let final_duration_secs = total_samples as f32 / (sample_rate * channels as u32) as f32;
        let elapsed = start_time.elapsed();

        log::info!("Streaming completed for session {}", session_id);
        log::info!(
            "   Final stats: {} chunks, {:.1}s audio, {:.2} MB in {:.1}s",
            chunk_count,
            final_duration_secs,
            total_bytes_sent as f64 / 1_000_000.0,
            elapsed.as_secs_f32()
        );

        // Wait for read task to finish (with timeout)
        let _ = tokio::time::timeout(TokioDuration::from_secs(5), read_handle).await;

        Ok(())
    }
}
