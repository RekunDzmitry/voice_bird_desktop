use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::time::Duration as TokioDuration;
use tokio_tungstenite::{connect_async_with_config, tungstenite::Message};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use futures_util::{SinkExt, StreamExt};
use http::header::HeaderValue;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio::sync::mpsc as tokio_mpsc; // For async channel communication
use tokio::sync::oneshot; // For init acknowledgment synchronization
use crate::audio_converter::SimpleAudioConverter;
use crate::audio_buffer::AudioConsumer;

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
    /// * `app_name` - Name of the application being recorded (e.g., "chrome.exe", "msedge.exe")
    /// * `audio_consumer` - AudioConsumer for receiving pre-buffered f32 audio samples
    /// * `sample_rate` - Audio sample rate
    /// * `channels` - Number of audio channels
    pub async fn stream_to_server(
        server_url: String,
        api_key: String,
        session_id: String,
        device_name: String,
        app_name: Option<String>,
        audio_consumer: AudioConsumer,
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

        // Create audio converter for pre-conversion (48kHz stereo → 16kHz mono)
        // This reduces bandwidth 6x and eliminates backend conversion overhead
        let audio_converter = SimpleAudioConverter::new(sample_rate, channels, 16000);
        let output_sample_rate = audio_converter.output_sample_rate();

        // Send initialization message with converted format
        let init_msg = InitMessage {
            message_type: "init".to_string(),
            api_key: api_key.clone(),
            session_id: session_id.clone(),
            device_name: device_name.clone(),
            device_type: Some("output".to_string()),  // "input" for microphones, "output" for system audio
            app_name,  // Application name (e.g., "chrome.exe", "msedge.exe") for source identification
            sample_rate: output_sample_rate,  // 16kHz after conversion
            channels: 1,                       // Mono after conversion
            audio_format: "pcm16le".to_string(),  // PCM16 little-endian after conversion
        };

        let init_json = serde_json::to_string(&init_msg)
            .context("Failed to serialize init message")?;

        log::debug!("Sending init message ({} bytes)...", init_json.len());
        log::debug!("   Init message JSON: {}", init_json);
        log::debug!("   Session ID: {}", session_id);
        log::debug!("   Sample Rate: {} Hz", sample_rate);
        log::debug!("   Channels: {}", channels);
        log::debug!("   Audio Format: f32le (raw PCM)");

        if let Err(e) = write.send(Message::Text(init_json.clone())).await {
            log::error!("Failed to send init message: {}", e);
            log::error!("   Error type: {:?}", e);
            return Err(anyhow::anyhow!("Failed to send init message: {}", e));
        }

        log::info!("Init message sent successfully");

        log::info!("Streaming started for device: {}", device_name);

        // Note: We send raw f32 PCM samples for transcription compatibility
        // This increases bandwidth but ensures reliable transcription with AssemblyAI
        log::info!("Audio streaming mode: Raw f32 PCM");
        log::info!("   Sample Rate: {} Hz", sample_rate);
        log::info!("   Channels: {}", channels);

        // Create channel for sending pong responses from read task to write loop
        let (pong_tx, mut pong_rx) = tokio_mpsc::unbounded_channel::<Vec<u8>>();

        // Create channel to signal when server confirms init was processed
        let (init_ack_tx, init_ack_rx) = oneshot::channel::<Result<(), String>>();
        let init_ack_tx = std::sync::Arc::new(std::sync::Mutex::new(Some(init_ack_tx)));
        let init_ack_tx_clone = init_ack_tx.clone();

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
                                "init_success" => {
                                    log::info!("Server confirmed session initialized: {}", response.message);
                                    // Signal that init was successful
                                    if let Ok(mut guard) = init_ack_tx_clone.lock() {
                                        if let Some(tx) = guard.take() {
                                            let _ = tx.send(Ok(()));
                                        }
                                    }
                                }
                                "error" => {
                                    // Server sends error messages in the "message" field, not "error" field
                                    let error_msg = if !response.message.is_empty() {
                                        response.message.clone()
                                    } else if !response.error.is_empty() {
                                        response.error.clone()
                                    } else {
                                        "Unknown error (no error message provided)".to_string()
                                    };
                                    log::error!("Server error: {}", error_msg);
                                    // Signal init failure if this is an init-related error
                                    if let Ok(mut guard) = init_ack_tx_clone.lock() {
                                        if let Some(tx) = guard.take() {
                                            let _ = tx.send(Err(error_msg));
                                        }
                                    }
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

        // Wait for server to confirm init was processed before streaming
        log::info!("Waiting for server to confirm session initialization...");
        match tokio::time::timeout(TokioDuration::from_secs(10), init_ack_rx).await {
            Ok(Ok(Ok(()))) => {
                log::info!("Server confirmed init, starting audio stream");
            }
            Ok(Ok(Err(e))) => {
                log::error!("Server rejected init: {}", e);
                return Err(anyhow::anyhow!("Session initialization rejected: {}", e));
            }
            Ok(Err(_)) => {
                log::error!("Init acknowledgment channel closed unexpectedly");
                return Err(anyhow::anyhow!("Init acknowledgment failed"));
            }
            Err(_) => {
                log::error!("Timeout waiting for server init acknowledgment (10s)");
                return Err(anyhow::anyhow!("Session initialization timeout"));
            }
        }

        log::info!("Beginning audio streaming loop (raw f32 PCM)...");

        // Drain all pre-buffered audio chunks accumulated during WebSocket setup
        let backlog = audio_consumer.drain_all();
        log::info!("Draining {} pre-buffered audio chunks", backlog.len());
        for audio_chunk in backlog {
            chunk_count += 1;
            total_samples += audio_chunk.len();

            let converted_bytes = audio_converter.convert(&audio_chunk);

            let timestamp_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            let mut packet = timestamp_ms.to_le_bytes().to_vec();
            packet.extend(&converted_bytes);

            total_bytes_sent += packet.len() as u64;

            if let Err(e) = write.send(Message::Binary(packet)).await {
                log::error!("Failed to send pre-buffered audio chunk #{}: {}", chunk_count, e);
                break;
            }
        }
        if chunk_count > 0 {
            log::info!("Pre-buffered audio sent: {} chunks", chunk_count);
        }

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

            // Check if audio capture has stopped
            if audio_consumer.is_stopped() {
                let remaining = audio_consumer.drain_all();
                for audio_chunk in &remaining {
                    chunk_count += 1;
                    total_samples += audio_chunk.len();

                    let converted_bytes = audio_converter.convert(audio_chunk);

                    let timestamp_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64;
                    let mut packet = timestamp_ms.to_le_bytes().to_vec();
                    packet.extend(&converted_bytes);

                    total_bytes_sent += packet.len() as u64;

                    if let Err(e) = write.send(Message::Binary(packet)).await {
                        log::error!("Failed to send final audio chunk #{}: {}", chunk_count, e);
                        break;
                    }
                }

                let elapsed = start_time.elapsed();
                let duration_secs = total_samples as f32 / (sample_rate * channels as u32) as f32;
                log::info!("Audio stream ended (capture stopped), finalizing...");
                log::info!("   Total audio chunks sent: {}", chunk_count);
                log::info!("   Total audio duration: {:.1}s", duration_secs);
                log::info!("   Total bytes sent: {:.2} KB", total_bytes_sent as f64 / 1_000.0);
                log::info!("   Elapsed time: {:.1}s", elapsed.as_secs_f32());
                break;
            }

            // Wait for audio chunks with timeout to allow pong handling
            let chunks = audio_consumer.wait_and_drain(std::time::Duration::from_millis(10));
            if chunks.is_empty() {
                continue;
            }

            let mut send_failed = false;
            for audio_chunk in chunks {
                chunk_count += 1;
                total_samples += audio_chunk.len();

                // Convert audio: 48kHz stereo f32 → 16kHz mono PCM16LE
                // This reduces bandwidth ~6x and eliminates backend conversion
                let converted_bytes = audio_converter.convert(&audio_chunk);

                // Prepend timestamp for latency measurement (8 bytes)
                let timestamp_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                let mut packet = timestamp_ms.to_le_bytes().to_vec();
                packet.extend(&converted_bytes);

                let packet_len = packet.len();
                total_bytes_sent += packet_len as u64;

                // Log every chunk for first 10 chunks, then every 100 chunks
                let should_log = chunk_count <= 10 || chunk_count % 100 == 0;

                if should_log {
                    let elapsed = start_time.elapsed();
                    let duration_secs = total_samples as f32 / (sample_rate * channels as u32) as f32;
                    log::debug!(
                        "Audio chunk #{}: {} samples ({} bytes) | Total: {:.1}s audio, {:.2} KB sent | Elapsed: {:.1}s",
                        chunk_count,
                        audio_chunk.len(),
                        packet_len,
                        duration_secs,
                        total_bytes_sent as f64 / 1_000.0,
                        elapsed.as_secs_f32()
                    );
                }

                // Send as binary WebSocket frame (packet includes 8-byte timestamp header)
                match write.send(Message::Binary(packet)).await {
                    Ok(_) => {}
                    Err(e) => {
                        let elapsed = start_time.elapsed();
                        log::error!(
                            "Failed to send audio chunk #{}: {}",
                            chunk_count, e
                        );
                        log::error!("   Error details: {:?}", e);
                        log::error!(
                            "   Sent {} chunks ({:.1}s of audio, {:.2} KB) before failure",
                            chunk_count - 1,
                            total_samples as f32 / (sample_rate * channels as u32) as f32,
                            (total_bytes_sent - packet_len as u64) as f64 / 1_000.0
                        );
                        log::error!("   Elapsed time: {:.1}s", elapsed.as_secs_f32());
                        send_failed = true;
                        break;
                    }
                }
            }
            if send_failed {
                break;
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
