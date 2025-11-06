use anyhow::{Context, Result};
use console::style;
use serde::Deserialize;
use serde_json::json;
use std::sync::{Arc, Mutex};
use tokio::time::{sleep, Duration as TokioDuration};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use futures_util::{SinkExt, StreamExt};
use http::header::HeaderValue;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

// AssemblyAI Universal-Streaming API v3 message structures

// Generic message wrapper to extract the type field
#[derive(Debug, Deserialize)]
struct MessageType {
    #[serde(rename = "type")]
    message_type: String,
}

// Begin message - session started
#[derive(Debug, Deserialize)]
struct BeginMessage {
    #[serde(rename = "type")]
    #[allow(dead_code)]
    message_type: String,
    id: String,
    #[allow(dead_code)]
    expires_at: u64,
}

// Word object within Turn messages
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct Word {
    text: String,
    start: u32,
    end: u32,
    confidence: f64,
    word_is_final: bool,
}

// Turn message - transcription results
#[derive(Debug, Deserialize)]
struct TurnMessage {
    #[serde(rename = "type")]
    #[allow(dead_code)]
    message_type: String,
    turn_order: u32,
    #[allow(dead_code)]
    turn_is_formatted: bool,
    end_of_turn: bool,
    transcript: String,
    #[allow(dead_code)]
    end_of_turn_confidence: f64,
    #[serde(default)]
    #[allow(dead_code)]
    words: Vec<Word>,
}

// Termination message - session ended
#[derive(Debug, Deserialize)]
struct TerminationMessage {
    #[serde(rename = "type")]
    #[allow(dead_code)]
    message_type: String,
    #[allow(dead_code)]
    audio_duration_seconds: u32,
    #[allow(dead_code)]
    session_duration_seconds: u32,
}

// Error message
#[derive(Debug, Deserialize)]
struct ErrorMessage {
    error: String,
}

/// Transcription service for real-time speech-to-text via AssemblyAI
pub struct TranscriptionService;

impl TranscriptionService {
    /// Start a transcription stream
    ///
    /// # Arguments
    /// * `api_key` - AssemblyAI API key
    /// * `audio_rx` - Channel receiver for mono PCM16 audio samples
    /// * `sample_rate` - Audio sample rate (16kHz minimum)
    /// * `transcript_buffer` - Shared buffer to store transcript segments
    pub async fn transcribe_stream(
        api_key: String,
        audio_rx: std::sync::mpsc::Receiver<Vec<i16>>,
        sample_rate: u32,
        transcript_buffer: Arc<Mutex<Vec<String>>>,
    ) -> Result<()> {
        // Log API key info (sanitized)
        let api_key_preview = if api_key.len() > 10 {
            format!("{}****", &api_key[..10])
        } else {
            "****".to_string()
        };
        println!("{}", style(format!("🔑 API key preview: {}", api_key_preview)).yellow());

        // Connect to AssemblyAI Universal-Streaming WebSocket (v3) with Authorization header
        let url = format!(
            "wss://streaming.assemblyai.com/v3/ws?sample_rate={}",
            sample_rate
        );

        println!("{}", style(format!("🔌 Connecting to AssemblyAI WebSocket...", )).cyan());

        // Create a custom request with Authorization header
        let mut request = url.into_client_request()
            .context("Failed to create WebSocket request")?;

        let auth_header_value = HeaderValue::from_str(&api_key)
            .context("Failed to create Authorization header value")?;

        request.headers_mut().insert("Authorization", auth_header_value);

        let (ws_stream, _response) = connect_async(request)
            .await
            .context("Failed to connect to AssemblyAI WebSocket")?;

        println!("{}", style("✓ WebSocket connected!").green());

        let (mut write, mut read) = ws_stream.split();

        // Spawn task to send audio data
        let write_handle = tokio::spawn(async move {
            let mut chunk_count = 0u64;

            while let Ok(audio_chunk) = audio_rx.recv() {
                chunk_count += 1;

                // Convert i16 samples to raw PCM16 bytes (little-endian)
                // v3 API requires binary WebSocket frames, not JSON
                let bytes: Vec<u8> = audio_chunk
                    .iter()
                    .flat_map(|&sample| sample.to_le_bytes())
                    .collect();

                // Send as binary WebSocket frame (NOT text/JSON)
                match write.send(Message::Binary(bytes)).await {
                    Ok(_) => {
                        // Add small delay to prevent overwhelming WebSocket (10ms backpressure)
                        sleep(TokioDuration::from_millis(10)).await;
                    }
                    Err(e) => {
                        eprintln!("{}", style(format!("❌ Failed to send audio chunk #{}: {}", chunk_count, e)).red());
                        break;
                    }
                }
            }

            // Send termination message (v3 API format)
            let _ = write
                .send(Message::Text(json!({"type": "Terminate"}).to_string()))
                .await;
        });

        // Receive transcription results
        while let Some(msg) = read.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    // First try to parse as error response (has error field instead of type)
                    if let Ok(error_response) = serde_json::from_str::<ErrorMessage>(&text) {
                        eprintln!("{}", style("━".repeat(60)).red());
                        eprintln!("{}", style("❌ API ERROR").red().bold());
                        eprintln!("{}", style(format!("   Error: {}", error_response.error)).red());
                        eprintln!("{}", style("━".repeat(60)).red());
                        eprintln!();
                        eprintln!("{}", style("Possible causes:").yellow().bold());
                        eprintln!("{}", style("  1. API key is invalid or expired").yellow());
                        eprintln!("{}", style("  2. API key is still set to placeholder 'your-api-key-here'").yellow());
                        eprintln!("{}", style("  3. Extra whitespace in .env file").yellow());
                        eprintln!();
                        eprintln!("{}", style("To fix this:").cyan().bold());
                        eprintln!("{}", style("  1. Get a valid API key from: https://www.assemblyai.com/").cyan());
                        eprintln!("{}", style("  2. Open your .env file and set:").cyan());
                        eprintln!("{}", style("     ASSEMBLYAI_API_KEY=<your-actual-key>").cyan());
                        eprintln!("{}", style("  3. Make sure there are no spaces around the = sign").cyan());
                        eprintln!("{}", style("━".repeat(60)).red());
                        break;
                    }

                    // Extract the message type first
                    match serde_json::from_str::<MessageType>(&text) {
                        Ok(msg_type) => {
                            match msg_type.message_type.as_str() {
                                "Begin" => {
                                    if let Ok(begin_msg) = serde_json::from_str::<BeginMessage>(&text) {
                                        println!("{}", style(format!("✓ Transcription session started (ID: {})", begin_msg.id)).green());
                                    }
                                }
                                "Turn" => {
                                    if let Ok(turn_msg) = serde_json::from_str::<TurnMessage>(&text) {
                                        // Add to transcript buffer if there's text
                                        if !turn_msg.transcript.is_empty() {
                                            if let Ok(mut buffer) = transcript_buffer.lock() {
                                                // For end_of_turn, add as a new segment
                                                // For partial turns, replace the last partial segment
                                                if turn_msg.end_of_turn {
                                                    buffer.push(turn_msg.transcript.clone());
                                                } else {
                                                    // Replace last segment if it exists and is from same turn
                                                    if buffer.is_empty() || turn_msg.turn_order == 1 {
                                                        buffer.push(turn_msg.transcript.clone());
                                                    } else {
                                                        let last_idx = buffer.len() - 1;
                                                        buffer[last_idx] = turn_msg.transcript.clone();
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                "Termination" => {
                                    if let Ok(_term_msg) = serde_json::from_str::<TerminationMessage>(&text) {
                                        println!("{}", style("Transcription session ended").yellow());
                                    }
                                    break;
                                }
                                _ => {
                                    // Ignore unknown message types
                                }
                            }
                        }
                        Err(_) => {
                            // Ignore parse errors for unknown message formats
                        }
                    }
                }
                Ok(Message::Close(_)) => {
                    break;
                }
                Err(e) => {
                    eprintln!("{}", style(format!("❌ WebSocket error: {}", e)).red());
                    break;
                }
                _ => {
                    // Ignore non-text messages
                }
            }
        }

        write_handle.abort();
        Ok(())
    }
}

/// Convert multi-channel f32 samples to mono i16 PCM16
pub fn convert_to_mono_pcm16(samples: &[f32], channels: u16) -> Vec<i16> {
    let channels = channels as usize;
    if channels == 1 {
        // Already mono, just convert to i16
        samples.iter().map(|&s| (s * i16::MAX as f32) as i16).collect()
    } else {
        // Downmix to mono
        let frame_count = samples.len() / channels;
        let mut mono_samples = Vec::with_capacity(frame_count);

        for frame_idx in 0..frame_count {
            let start = frame_idx * channels;
            let end = start + channels;

            let sum: f32 = samples[start..end].iter().sum();
            let avg = sum / channels as f32;
            let sample_i16 = (avg * i16::MAX as f32) as i16;
            mono_samples.push(sample_i16);
        }

        mono_samples
    }
}
