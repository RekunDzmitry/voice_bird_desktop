/// gRPC Service wrapper for Voice Bird server streaming
/// Provides a similar interface to server_streaming.rs but uses gRPC instead of WebSocket

use anyhow::{Context, Result};
use std::sync::mpsc;
use crate::grpc_streaming::{GrpcAudioStreamer, StreamConfig};
use crate::opus_encoder::{OpusAudioEncoder, OpusEncoderConfig};

/// Service for streaming audio to Voice Bird server via gRPC
pub struct GrpcStreamingService;

impl GrpcStreamingService {
    /// Stream audio to Voice Bird server via gRPC
    ///
    /// # Arguments
    /// * `server_url` - gRPC server URL (e.g., http://localhost:50051)
    /// * `api_key` - User's API key for authentication
    /// * `session_id` - Unique identifier for this recording session (will be overridden by GrpcAudioStreamer)
    /// * `device_name` - Name of the audio device being recorded
    /// * `audio_rx` - Channel receiver for f32 audio samples
    /// * `sample_rate` - Audio sample rate
    /// * `channels` - Number of audio channels
    pub async fn stream_to_server(
        server_url: String,
        api_key: String,
        _session_id: String, // Not used - GrpcAudioStreamer generates its own
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

        log::info!("Connecting to Voice Bird gRPC server...");
        log::debug!("   Server: {}", server_url);
        log::debug!("   API Key: {}", api_key_preview);
        log::debug!("   Device: {}", device_name);
        log::debug!("   Sample Rate: {} Hz", sample_rate);
        log::debug!("   Channels: {}", channels);

        // Determine device type from name
        let device_type = if device_name.contains("Microphone")
            || device_name.contains("Input")
            || device_name.contains("Mic")
        {
            "input"
        } else {
            "output"
        }
        .to_string();

        // Create Opus encoder configuration
        // Note: Opus works best at 48kHz, but we'll use the device's sample rate
        let opus_config = OpusEncoderConfig {
            sample_rate: 48000, // Opus standard sample rate
            channels: 1, // Mono for voice streaming
            bitrate: 24000, // 24 kbps - optimal for speech
            frame_duration_ms: 20, // 20ms frames
        };

        let mut opus_encoder = OpusAudioEncoder::new(opus_config)
            .context("Failed to create Opus encoder")?;

        log::debug!("Opus Encoder Configuration:");
        log::debug!("   Sample Rate: {} Hz", opus_encoder.sample_rate());
        log::debug!("   Channels: {} (mono)", opus_encoder.channels());
        log::debug!("   Bitrate: {} kbps", opus_encoder.bitrate() / 1000);
        log::debug!("   Frame Duration: {}ms", opus_encoder.frame_duration_ms());
        log::debug!("   Frame Size: {} samples", opus_encoder.frame_size());

        // Create stream config
        let config = StreamConfig {
            server_url,
            api_key,
            device_name,
            device_type,
            sample_rate: opus_encoder.sample_rate(),
            channels: opus_encoder.channels() as u32,
            codec: "opus".to_string(),
            bitrate: opus_encoder.bitrate() as u32,
            frame_duration_ms: opus_encoder.frame_duration_ms(),
        };

        // Create gRPC streamer
        let mut streamer = GrpcAudioStreamer::new(config);

        // Connect to server and get response stream
        log::info!("Starting gRPC Connection Sequence");

        let response_stream = match streamer.connect().await {
            Ok(stream) => {
                log::info!("Connection Successful!");
                stream
            }
            Err(e) => {
                log::error!("CONNECTION FAILED");
                log::error!("Error Details:");
                log::error!("  {}", e);

                // Print the full error chain
                let mut current_error = e.source();
                let mut level = 1;
                while let Some(source) = current_error {
                    log::error!("  {} Caused by: {}", "└─".repeat(level), source);
                    current_error = source.source();
                    level += 1;
                }

                log::warn!("Troubleshooting Tips:");
                log::warn!("  1. Verify the web server (voice_bird) is running");
                log::warn!("  2. Check the server URL is correct (default: http://localhost:50051)");
                log::warn!("  3. Ensure no firewall is blocking port 50051");
                log::warn!("  4. Verify the API key is valid");
                log::warn!("  5. Check server logs for any errors");

                return Err(e.context("Failed to establish gRPC connection - see details above"));
            }
        };

        log::info!("Streaming started - Session: {}", streamer.session_id());

        // Spawn task to handle transcription responses
        let response_handle = tokio::spawn(async move {
            if let Err(e) = GrpcAudioStreamer::handle_responses(response_stream).await {
                log::error!("Response handler error: {}", e);
            }
        });

        // Send audio data to server
        let mut chunk_count = 0u64;
        let mut total_samples = 0usize;
        let mut total_bytes_sent = 0u64;
        let start_time = std::time::Instant::now();

        log::info!("Beginning audio streaming...");

        // Main streaming loop
        let mut opus_chunk_count = 0u64;
        loop {
            match audio_rx.recv() {
                Ok(audio_chunk) => {
                    chunk_count += 1;
                    total_samples += audio_chunk.len();

                    // Encode audio with Opus (buffer and encode when frame is ready)
                    match opus_encoder.buffer_and_encode(&audio_chunk) {
                        Ok(Some(opus_packet)) => {
                            opus_chunk_count += 1;
                            let bytes_len = opus_packet.len();
                            total_bytes_sent += bytes_len as u64;

                            // Send Opus-encoded chunk to server
                            if let Err(e) = streamer.send_audio_chunk(opus_packet).await {
                                let elapsed = start_time.elapsed();
                                log::error!(
                                    "Failed to send audio chunk #{}: {}",
                                    opus_chunk_count, e
                                );
                                log::error!(
                                    "   Sent {} Opus packets ({:.1}s of audio, {:.2} MB) before failure",
                                    opus_chunk_count - 1,
                                    total_samples as f32 / opus_encoder.sample_rate() as f32,
                                    total_bytes_sent as f64 / 1_000_000.0
                                );
                                log::error!("   Elapsed time: {:.1}s", elapsed.as_secs_f32());
                                break;
                            }

                            // Log every 50 Opus packets
                            if opus_chunk_count % 50 == 0 {
                                let elapsed = start_time.elapsed();
                                let duration_secs = total_samples as f32 / opus_encoder.sample_rate() as f32;
                                let compression_ratio = (total_samples * 4) as f64 / total_bytes_sent as f64;
                                log::debug!(
                                    "Opus Packet #{}: {} bytes | Total: {:.1}s audio, {:.2} MB sent | Compression: {:.1}x | Elapsed: {:.1}s",
                                    opus_chunk_count,
                                    bytes_len,
                                    duration_secs,
                                    total_bytes_sent as f64 / 1_000_000.0,
                                    compression_ratio,
                                    elapsed.as_secs_f32()
                                );
                            }
                        }
                        Ok(None) => {
                            // Buffering - no packet ready yet, continue
                        }
                        Err(e) => {
                            log::error!("Opus encoding error: {}", e);
                            break;
                        }
                    }
                }
                Err(mpsc::RecvError) => {
                    // Audio sender disconnected, exit loop
                    let elapsed = start_time.elapsed();
                    let duration_secs =
                        total_samples as f32 / (sample_rate * channels as u32) as f32;

                    log::info!("Audio stream ended");
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

        // Close the streamer
        streamer.close();

        log::info!("gRPC streaming completed - Session: {}", streamer.session_id());

        // Wait for response handler to finish (with timeout)
        let _ = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            response_handle,
        )
        .await;

        Ok(())
    }
}
