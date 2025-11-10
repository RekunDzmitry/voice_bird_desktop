use anyhow::{Context, Result};
use console::style;
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

        // CRITICAL FIX: Remove Sec-WebSocket-Extensions header to prevent compression negotiation
        // This ensures no compression is requested even if the library tries to add it
        request.headers_mut().remove("Sec-WebSocket-Extensions");

        println!(
            "{}",
            style("🔧 Manually removed Sec-WebSocket-Extensions header (prevents compression)").yellow()
        );

        // Connect to WebSocket with timeout (10 seconds)
        println!("{}", style("⏳ Attempting WebSocket connection...").dim());

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

        println!(
            "{}",
            style("📋 WebSocket Config:").cyan()
        );
        println!(
            "{}",
            style(format!("   Max message size: {} bytes", ws_config.max_message_size.unwrap_or(0))).dim()
        );
        println!(
            "{}",
            style(format!("   Max frame size: {} bytes", ws_config.max_frame_size.unwrap_or(0))).dim()
        );
        println!(
            "{}",
            style(format!("   Write buffer size: {} bytes", ws_config.max_write_buffer_size)).dim()
        );
        println!(
            "{}",
            style("   Compression: DISABLED (via default-features = false)").green().bold()
        );

        let connection_result = tokio::time::timeout(
            TokioDuration::from_secs(10),
            connect_async_with_config(request, Some(ws_config), false) // false = disable Nagle's algorithm
        ).await;

        let (ws_stream, _response) = match connection_result {
            Ok(Ok(stream)) => {
                println!("{}", style("✓ Connected to Voice Bird server!").green());

                // Log HTTP response details for debugging
                println!(
                    "{}",
                    style(format!("📡 HTTP Response: {}", stream.1.status())).dim()
                );

                // Check for WebSocket extension headers (especially compression)
                if let Some(extensions) = stream.1.headers().get("Sec-WebSocket-Extensions") {
                    println!(
                        "{}",
                        style(format!("   Extensions negotiated: {:?}", extensions)).dim()
                    );
                } else {
                    println!(
                        "{}",
                        style("   Extensions: None (compression disabled)").dim()
                    );
                }

                // Log protocol if present
                if let Some(protocol) = stream.1.headers().get("Sec-WebSocket-Protocol") {
                    println!(
                        "{}",
                        style(format!("   Protocol: {:?}", protocol)).dim()
                    );
                }

                stream
            }
            Ok(Err(e)) => {
                // WebSocket error (connection refused, invalid response, etc.)
                eprintln!(
                    "{}",
                    style(format!("✗ WebSocket connection failed: {}", e)).red().bold()
                );
                eprintln!("{}", style("").dim());
                eprintln!("{}", style("Possible causes:").yellow());
                eprintln!("{}", style("  1. Server endpoint '/api/audio/stream' doesn't exist").yellow());
                eprintln!("{}", style("  2. Server is not running or unreachable").yellow());
                eprintln!("{}", style("  3. Server rejected the WebSocket upgrade request").yellow());
                eprintln!("{}", style("  4. Network/firewall blocking the connection").yellow());
                eprintln!("{}", style("").dim());
                eprintln!("{}", style("Troubleshooting:").cyan());
                eprintln!("{}", style(format!("  - Verify server is running at: {}", server_url)).cyan());
                eprintln!("{}", style("  - Check server logs for incoming connection attempts").cyan());
                eprintln!("{}", style("  - Ensure WebSocket endpoint is implemented and accessible").cyan());
                eprintln!("{}", style("  - Test with: curl -i -N -H 'Connection: Upgrade' ...").cyan());
                return Err(anyhow::anyhow!("WebSocket connection failed: {}", e));
            }
            Err(_) => {
                // Timeout
                eprintln!(
                    "{}",
                    style("✗ Connection timeout (10 seconds)").red().bold()
                );
                eprintln!("{}", style("").dim());
                eprintln!("{}", style("The server did not respond within 10 seconds.").yellow());
                eprintln!("{}", style("").dim());
                eprintln!("{}", style("Most likely cause:").yellow());
                eprintln!("{}", style("  → The WebSocket endpoint '/api/audio/stream' is NOT implemented on the server").yellow().bold());
                eprintln!("{}", style("").dim());
                eprintln!("{}", style("Action required:").cyan());
                eprintln!("{}", style("  1. Check if your server has a WebSocket handler at '/api/audio/stream'").cyan());
                eprintln!("{}", style("  2. See SERVER_ENDPOINT_SPEC.md for implementation requirements").cyan());
                eprintln!("{}", style("  3. Verify server logs show NO incoming connection attempts").cyan());
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

        println!(
            "{}",
            style(format!("📤 Sending init message ({} bytes)...", init_json.len())).dim()
        );
        println!(
            "{}",
            style(format!("   Session ID: {}", session_id)).dim()
        );
        println!(
            "{}",
            style(format!("   Sample Rate: {} Hz", sample_rate)).dim()
        );
        println!(
            "{}",
            style(format!("   Channels: {}", channels)).dim()
        );

        if let Err(e) = write.send(Message::Text(init_json.clone())).await {
            eprintln!(
                "{}",
                style(format!("✗ Failed to send init message: {}", e)).red()
            );
            eprintln!(
                "{}",
                style(format!("   Error type: {:?}", e)).red()
            );
            return Err(anyhow::anyhow!("Failed to send init message: {}", e));
        }

        println!(
            "{}",
            style("✓ Init message sent successfully").green()
        );

        println!(
            "{}",
            style(format!("📡 Streaming started for device: {}", device_name)).cyan()
        );

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
                    Ok(Message::Ping(payload)) => {
                        // Server sent ping - send pong response via channel to keep connection alive
                        println!(
                            "{}",
                            style(format!("🏓 Received ping from server, responding with pong")).dim()
                        );
                        if let Err(e) = pong_tx.send(payload) {
                            eprintln!(
                                "{}",
                                style(format!("✗ Failed to send pong via channel: {}", e)).red()
                            );
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
                        eprintln!(
                            "{}",
                            style(format!("   Error details: {:?}", e)).red()
                        );

                        // Provide specific guidance for common errors
                        let error_msg = format!("{}", e);
                        if error_msg.contains("Reserved bits") {
                            eprintln!("{}", style("").dim());
                            eprintln!("{}", style("⚠️  COMPRESSION MISMATCH DETECTED").yellow().bold());
                            eprintln!("{}", style("   This error indicates the client and server have mismatched compression settings.").yellow());
                            eprintln!("{}", style("   Server has: perMessageDeflate: false (compression disabled)").yellow());
                            eprintln!("{}", style("   Action: Check 'Sec-WebSocket-Extensions' header in connection log above").yellow());
                            eprintln!("{}", style("").dim());
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

        println!(
            "{}",
            style("🎵 Beginning audio streaming loop...").cyan()
        );

        // Main loop: send audio chunks and respond to pings
        loop {
            // Check for pending pong responses (non-blocking)
            while let Ok(pong_payload) = pong_rx.try_recv() {
                if let Err(e) = write.send(Message::Pong(pong_payload)).await {
                    eprintln!(
                        "{}",
                        style(format!("✗ Failed to send pong: {}", e)).red()
                    );
                    break;
                }
                pong_count += 1;
                if pong_count % 5 == 0 {
                    println!(
                        "{}",
                        style(format!("🏓 Sent {} pong responses to keep connection alive", pong_count)).dim()
                    );
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
                        println!(
                            "{}",
                            style(format!(
                                "📤 Chunk #{}: {} samples ({} bytes) | Total: {:.1}s audio, {:.2} MB sent | Elapsed: {:.1}s",
                                chunk_count,
                                audio_chunk.len(),
                                bytes_len,
                                duration_secs,
                                total_bytes_sent as f64 / 1_000_000.0,
                                elapsed.as_secs_f32()
                            ))
                            .dim()
                        );
                    }

                    // Send as binary WebSocket frame
                    match write.send(Message::Binary(bytes)).await {
                        Ok(_) => {
                            if should_log {
                                println!(
                                    "{}",
                                    style(format!("   ✓ Chunk #{} sent successfully", chunk_count)).dim()
                                );
                            }

                            // Small delay to prevent overwhelming the WebSocket
                            sleep(TokioDuration::from_millis(10)).await;
                        }
                        Err(e) => {
                            let elapsed = start_time.elapsed();
                            eprintln!(
                                "{}",
                                style(format!(
                                    "✗ Failed to send audio chunk #{}: {}",
                                    chunk_count, e
                                ))
                                .red()
                            );
                            eprintln!(
                                "{}",
                                style(format!("   Error details: {:?}", e)).red()
                            );
                            eprintln!(
                                "{}",
                                style(format!(
                                    "   Sent {} chunks ({:.1}s of audio, {:.2} MB) before failure",
                                    chunk_count - 1,
                                    (total_samples - audio_chunk.len()) as f32 / (sample_rate * channels as u32) as f32,
                                    (total_bytes_sent - bytes_len as u64) as f64 / 1_000_000.0
                                ))
                                .red()
                            );
                            eprintln!(
                                "{}",
                                style(format!("   Elapsed time: {:.1}s", elapsed.as_secs_f32())).red()
                            );
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

                    println!(
                        "{}",
                        style("📊 Audio stream ended, finalizing...").cyan()
                    );
                    println!(
                        "{}",
                        style(format!(
                            "   Total chunks sent: {}",
                            chunk_count
                        ))
                        .dim()
                    );
                    println!(
                        "{}",
                        style(format!(
                            "   Total audio duration: {:.1}s",
                            duration_secs
                        ))
                        .dim()
                    );
                    println!(
                        "{}",
                        style(format!(
                            "   Total bytes sent: {:.2} MB",
                            total_bytes_sent as f64 / 1_000_000.0
                        ))
                        .dim()
                    );
                    println!(
                        "{}",
                        style(format!(
                            "   Elapsed time: {:.1}s",
                            elapsed.as_secs_f32()
                        ))
                        .dim()
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

        println!(
            "{}",
            style(format!("📤 Sending terminate message ({} bytes)...", terminate_json.len())).dim()
        );

        let _ = write.send(Message::Text(terminate_json)).await;

        let final_duration_secs = total_samples as f32 / (sample_rate * channels as u32) as f32;
        let elapsed = start_time.elapsed();

        println!(
            "{}",
            style(format!(
                "✓ Streaming completed for session {}",
                session_id
            ))
            .green()
        );
        println!(
            "{}",
            style(format!(
                "   Final stats: {} chunks, {:.1}s audio, {:.2} MB in {:.1}s",
                chunk_count,
                final_duration_secs,
                total_bytes_sent as f64 / 1_000_000.0,
                elapsed.as_secs_f32()
            ))
            .green()
        );

        // Wait for read task to finish (with timeout)
        let _ = tokio::time::timeout(TokioDuration::from_secs(5), read_handle).await;

        Ok(())
    }
}
