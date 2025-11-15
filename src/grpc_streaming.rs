/// gRPC Audio Streaming Client for Voice Bird Desktop
/// Handles bidirectional streaming for real-time audio capture and transcription

use anyhow::{Context, Result};
use std::error::Error;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Channel, ClientTlsConfig};
use tonic::metadata::MetadataValue;
use tonic::{Request, Streaming};
use uuid::Uuid;

// Include the generated protobuf code
pub mod voicebird {
    tonic::include_proto!("voicebird");
}

use voicebird::{
    audio_streaming_client::AudioStreamingClient,
    AudioChunk, SessionMetadata, TranscriptionResponse,
};

/// Configuration for the gRPC audio streaming session
#[derive(Debug, Clone)]
pub struct StreamConfig {
    pub server_url: String,
    pub api_key: String,
    pub device_name: String,
    pub device_type: String, // "input" or "output"
    pub sample_rate: u32,
    pub channels: u32,
    pub codec: String, // "opus"
    pub bitrate: u32, // Opus bitrate in bps
    pub frame_duration_ms: u32, // Frame duration in milliseconds
}

/// gRPC Audio Streamer - manages bidirectional streaming connection
pub struct GrpcAudioStreamer {
    config: StreamConfig,
    session_id: String,
    client: Option<AudioStreamingClient<Channel>>,
    audio_sender: Option<mpsc::Sender<AudioChunk>>,
    sequence: u32,
}

impl GrpcAudioStreamer {
    /// Create a new gRPC audio streamer
    pub fn new(config: StreamConfig) -> Self {
        let session_id = Uuid::new_v4().to_string();

        log::info!("Initializing gRPC audio streamer");
        log::info!("   Session ID: {}", session_id);
        log::info!("   Server: {}", config.server_url);
        log::info!("   Device: {} ({})", config.device_name, config.device_type);

        Self {
            config,
            session_id,
            client: None,
            audio_sender: None,
            sequence: 0,
        }
    }

    /// Connect to the gRPC server and initialize the stream
    pub async fn connect(&mut self) -> Result<Streaming<TranscriptionResponse>> {
        log::debug!("[CONNECT] Step 1: Parsing server URL: {}", self.config.server_url);

        // Detect if URL uses HTTPS
        let is_https = self.config.server_url.starts_with("https://");

        if is_https {
            log::debug!("   Using HTTPS with TLS encryption");
        } else {
            log::debug!("   Using HTTP (insecure) - suitable for localhost only");
        }

        // Create gRPC channel with TLS support
        let channel = match Channel::from_shared(self.config.server_url.clone()) {
            Ok(mut endpoint) => {
                log::debug!("[CONNECT] Step 1: URL parsed successfully");

                // Configure TLS for HTTPS connections
                if is_https {
                    log::debug!("[CONNECT] Step 2a: Configuring TLS...");

                    // Create TLS config - tonic 0.11 uses webpki-roots by default with tls-roots feature
                    let tls_config = ClientTlsConfig::new();

                    endpoint = match endpoint.tls_config(tls_config) {
                        Ok(ep) => {
                            log::debug!("[CONNECT] Step 2a: TLS configured");
                            ep
                        }
                        Err(e) => {
                            log::error!("[CONNECT] Step 2a FAILED: TLS configuration error");
                            log::error!("   Error: {}", e);
                            return Err(anyhow::anyhow!("TLS configuration failed: {}", e));
                        }
                    };
                }

                log::debug!("[CONNECT] Step 2: Attempting {} connection to server...",
                         if is_https { "HTTPS" } else { "HTTP" });

                match endpoint.connect().await {
                    Ok(ch) => {
                        log::debug!("[CONNECT] Step 2: {} connection established",
                                if is_https { "HTTPS" } else { "HTTP" });
                        ch
                    }
                    Err(e) => {
                        log::error!("[CONNECT] Step 2 FAILED: Connection error");
                        log::error!("   Error type: {:?}", e);
                        log::error!("   Error message: {}", e);
                        log::error!("   Target: {}", self.config.server_url);
                        log::error!("   Possible causes:");
                        log::error!("   - Server is not running");
                        log::error!("   - Wrong host/port (check server URL)");
                        log::error!("   - Firewall blocking connection");
                        log::error!("   - Network connectivity issues");
                        if is_https {
                            log::error!("   - TLS certificate validation failed");
                            log::error!("   - TLS handshake error");
                        }
                        return Err(anyhow::anyhow!("Connection failed: {}", e));
                    }
                }
            }
            Err(e) => {
                log::error!("[CONNECT] Step 1 FAILED: Invalid server URL");
                log::error!("   Error: {}", e);
                log::error!("   URL: {}", self.config.server_url);
                log::error!("   Expected format: http://hostname:port or https://hostname:port");
                return Err(anyhow::anyhow!("Invalid server URL '{}': {}", self.config.server_url, e));
            }
        };

        log::info!("Connected to gRPC server");

        log::debug!("[CONNECT] Step 3: Creating gRPC client");
        // Create authenticated client
        let mut client = AudioStreamingClient::new(channel);
        log::debug!("[CONNECT] Step 3: Client created");

        // Create channel for sending audio chunks
        log::debug!("[CONNECT] Step 4: Setting up audio streaming channel (buffer size: 100)");
        let (tx, rx) = mpsc::channel::<AudioChunk>(100);
        self.audio_sender = Some(tx.clone());
        log::debug!("[CONNECT] Step 4: Channel created");

        // Send initial chunk with session metadata
        log::debug!("[CONNECT] Step 5: Preparing session metadata");
        let initial_chunk = AudioChunk {
            session_id: self.session_id.clone(),
            audio_data: vec![], // Empty for first chunk
            sequence: 0,
            timestamp: chrono::Utc::now().timestamp_millis() as u64,
            metadata: Some(SessionMetadata {
                device_name: self.config.device_name.clone(),
                device_type: self.config.device_type.clone(),
                sample_rate: self.config.sample_rate,
                channels: self.config.channels,
                codec: self.config.codec.clone(),
                bitrate: self.config.bitrate,
                frame_duration_ms: self.config.frame_duration_ms,
            }),
        };

        log::debug!("[CONNECT] Step 6: Queuing initial metadata chunk");
        tx.send(initial_chunk)
            .await
            .context("Failed to send initial chunk to channel")?;

        log::info!("Sent session initialization");

        // Create request stream
        log::debug!("[CONNECT] Step 7: Creating request stream wrapper");
        let request_stream = ReceiverStream::new(rx);

        // Create request with authentication metadata
        log::debug!("[CONNECT] Step 8: Adding authentication metadata");
        let mut request = Request::new(request_stream);
        let metadata = request.metadata_mut();

        let auth_value = match MetadataValue::try_from(&self.config.api_key) {
            Ok(val) => {
                log::debug!("[CONNECT] Step 8: API key validated");
                val
            }
            Err(e) => {
                log::error!("[CONNECT] Step 8 FAILED: Invalid API key format");
                log::error!("   Error: {}", e);
                log::error!("   API key length: {}", self.config.api_key.len());
                log::error!("   Hint: API key must contain only ASCII characters");
                return Err(anyhow::anyhow!("Invalid API key format: {}", e));
            }
        };
        metadata.insert("authorization", auth_value);

        // Start bidirectional streaming
        log::debug!("[CONNECT] Step 9: Initiating bidirectional stream with server...");
        log::debug!("   This will send the metadata and wait for server acknowledgment");

        let response = match client.stream_audio(request).await {
            Ok(resp) => {
                log::debug!("[CONNECT] Step 9: Server accepted stream request");
                resp
            }
            Err(e) => {
                log::error!("[CONNECT] Step 9 FAILED: Server rejected stream request");
                log::error!("   gRPC Status: {:?}", e.code());
                log::error!("   Message: {}", e.message());
                log::error!("   Possible causes:");
                log::error!("   - Invalid API key (unauthorized)");
                log::error!("   - Server endpoint not found (check URL path)");
                log::error!("   - Server internal error");
                log::error!("   - Incompatible protocol version");
                if let Some(source) = e.source() {
                    log::error!("   Source error: {}", source);
                }
                return Err(anyhow::anyhow!("gRPC stream request failed ({}): {}", e.code(), e.message()));
            }
        };

        log::info!("Stream established");

        self.client = Some(client);
        Ok(response.into_inner())
    }

    /// Send an audio chunk to the server
    pub async fn send_audio_chunk(&mut self, audio_data: Vec<u8>) -> Result<()> {
        let sender = self.audio_sender
            .as_ref()
            .context("Stream not initialized - call connect() first")?;

        self.sequence += 1;

        let chunk = AudioChunk {
            session_id: self.session_id.clone(),
            audio_data,
            sequence: self.sequence,
            timestamp: chrono::Utc::now().timestamp_millis() as u64,
            metadata: None, // Only sent in first chunk
        };

        sender
            .send(chunk)
            .await
            .context("Failed to send audio chunk")?;

        Ok(())
    }

    /// Process incoming transcription responses
    pub async fn handle_responses(
        mut response_stream: Streaming<TranscriptionResponse>
    ) -> Result<()> {
        log::info!("Listening for transcriptions...");

        while let Some(response) = response_stream.message().await? {
            match response.response {
                Some(voicebird::transcription_response::Response::Status(status)) => {
                    log::info!("Status: {}", status.message);
                }
                Some(voicebird::transcription_response::Response::Transcript(transcript)) => {
                    let marker = if transcript.is_final { "✓" } else { "..." };
                    let confidence = (transcript.confidence * 100.0) as u32;

                    log::info!(
                        "{} [{}%] {}",
                        marker,
                        confidence,
                        transcript.text
                    );
                }
                Some(voicebird::transcription_response::Response::Error(error)) => {
                    log::error!("Server error: {} ({})", error.message, error.code);
                    if let Some(details) = error.details {
                        log::error!("   Details: {}", details);
                    }
                }
                None => {
                    // Empty response, continue
                }
            }
        }

        log::info!("Stream ended");
        Ok(())
    }

    /// Get the current session ID
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Get current sequence number
    #[allow(dead_code)]
    pub fn sequence(&self) -> u32 {
        self.sequence
    }

    /// Close the audio sender channel (signals end of stream)
    pub fn close(&mut self) {
        if let Some(sender) = self.audio_sender.take() {
            drop(sender);
            log::info!("Closed audio stream");
        }
    }
}

/// Helper function to convert f32 audio samples to bytes (little-endian)
pub fn f32_samples_to_bytes(samples: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(samples.len() * 4);
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    bytes
}

/// Helper function to convert i16 audio samples to f32 PCM bytes
#[allow(dead_code)]
pub fn i16_samples_to_f32_bytes(samples: &[i16]) -> Vec<u8> {
    let f32_samples: Vec<f32> = samples
        .iter()
        .map(|&s| s as f32 / i16::MAX as f32)
        .collect();
    f32_samples_to_bytes(&f32_samples)
}

/// Helper function to convert u16 audio samples to f32 PCM bytes
#[allow(dead_code)]
pub fn u16_samples_to_f32_bytes(samples: &[u16]) -> Vec<u8> {
    let f32_samples: Vec<f32> = samples
        .iter()
        .map(|&s| (s as f32 / u16::MAX as f32) * 2.0 - 1.0)
        .collect();
    f32_samples_to_bytes(&f32_samples)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_f32_to_bytes_conversion() {
        let samples = vec![0.0_f32, 0.5, -0.5, 1.0, -1.0];
        let bytes = f32_samples_to_bytes(&samples);

        // Each f32 is 4 bytes
        assert_eq!(bytes.len(), samples.len() * 4);

        // Verify roundtrip
        for (i, sample) in samples.iter().enumerate() {
            let byte_slice = &bytes[i * 4..(i + 1) * 4];
            let reconstructed = f32::from_le_bytes([
                byte_slice[0],
                byte_slice[1],
                byte_slice[2],
                byte_slice[3],
            ]);
            assert_eq!(reconstructed, *sample);
        }
    }

    #[test]
    fn test_i16_to_f32_conversion() {
        let samples = vec![0_i16, 16384, -16384, i16::MAX, i16::MIN];
        let bytes = i16_samples_to_f32_bytes(&samples);

        // Each f32 is 4 bytes
        assert_eq!(bytes.len(), samples.len() * 4);

        // Verify values are in normalized range [-1.0, 1.0]
        for i in 0..samples.len() {
            let byte_slice = &bytes[i * 4..(i + 1) * 4];
            let f32_value = f32::from_le_bytes([
                byte_slice[0],
                byte_slice[1],
                byte_slice[2],
                byte_slice[3],
            ]);
            assert!(f32_value >= -1.0 && f32_value <= 1.0);
        }
    }
}
