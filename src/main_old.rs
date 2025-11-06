use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat};
use dialoguer::Select;
use console::style;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent},
    terminal::{self, ClearType},
    cursor, execute,
};
use anyhow::{Result, Context};
use std::sync::{Arc, Mutex};
use std::io::{stdout, Write};
use std::time::{Duration, Instant};
use hound::{WavWriter, WavSpec};
use chrono::Local;
use tokio::runtime::Runtime;
use tokio::time::{sleep, Duration as TokioDuration};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use http::HeaderValue;
use std::env;

#[cfg(windows)]
use windows::{
    Win32::Media::Audio::*,
    Win32::System::Com::*,
};

// WAVE format constants (in case they're not exported by the windows crate)
#[cfg(windows)]
const WAVE_FORMAT_IEEE_FLOAT: u32 = 0x0003;
#[cfg(windows)]
const WAVE_FORMAT_EXTENSIBLE: u32 = 0xFFFE;

struct DeviceInfo {
    name: String,
    is_default: bool,
}

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
    audio_duration_seconds: u32,
    session_duration_seconds: u32,
}

// Error message
#[derive(Debug, Deserialize)]
struct ErrorMessage {
    error: String,
}

// Transcription service for real-time STT
struct TranscriptionService {
    transcript_buffer: Arc<Mutex<Vec<String>>>,
    api_key: String,
}

impl TranscriptionService {
    fn new(api_key: String) -> Self {
        Self {
            transcript_buffer: Arc::new(Mutex::new(Vec::new())),
            api_key,
        }
    }

    fn get_transcript_buffer(&self) -> Arc<Mutex<Vec<String>>> {
        self.transcript_buffer.clone()
    }

    async fn transcribe_stream(
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
        println!("{}", style(format!("DEBUG: API key preview: {}", api_key_preview)).yellow());

        // Connect to AssemblyAI Universal-Streaming WebSocket (v3) with Authorization header
        let url = format!(
            "wss://streaming.assemblyai.com/v3/ws?sample_rate={}",
            sample_rate
        );

        println!("{}", style(format!("DEBUG: Connecting to AssemblyAI WebSocket: {}", url)).yellow());

        // Create a custom request with Authorization header
        let mut request = url.into_client_request()
            .context("Failed to create WebSocket request")?;

        let auth_header_value = HeaderValue::from_str(&api_key)
            .context("Failed to create Authorization header value")?;

        request.headers_mut().insert("Authorization", auth_header_value);

        let (ws_stream, response) = connect_async(request)
            .await
            .context("Failed to connect to AssemblyAI WebSocket")?;

        println!("{}", style(format!("DEBUG: WebSocket connected! Response status: {:?}", response.status())).green());

        let (mut write, mut read) = ws_stream.split();

        println!("{}", style("DEBUG: WebSocket authenticated via Authorization header").green());

        // Spawn task to send audio data
        let write_handle = tokio::spawn(async move {
            let mut chunk_count = 0u64;
            println!("{}", style("DEBUG: Audio send task started").yellow());

            while let Ok(audio_chunk) = audio_rx.recv() {
                chunk_count += 1;

                // Convert i16 samples to raw PCM16 bytes (little-endian)
                // v3 API requires binary WebSocket frames, not JSON
                let bytes: Vec<u8> = audio_chunk
                    .iter()
                    .flat_map(|&sample| sample.to_le_bytes())
                    .collect();

                let bytes_len = bytes.len();
                let bytes_kb = bytes_len as f32 / 1024.0;

                // Log first few chunks with detailed info
                if chunk_count <= 5 {
                    println!("{}", style(format!("DEBUG: Chunk #{}: {} samples, {:.2} KB raw PCM16",
                        chunk_count, audio_chunk.len(), bytes_kb)).cyan());
                }

                // Send as binary WebSocket frame (NOT text/JSON)
                match write.send(Message::Binary(bytes)).await {
                    Ok(_) => {
                        if chunk_count % 10 == 0 {
                            println!("{}", style(format!("DEBUG: Sent {} audio chunks to AssemblyAI", chunk_count)).cyan());
                        }
                        // Add small delay to prevent overwhelming WebSocket (10ms backpressure)
                        sleep(TokioDuration::from_millis(10)).await;
                    }
                    Err(e) => {
                        eprintln!("{}", style(format!("ERROR: Failed to send audio chunk #{} ({:.2} KB): {}", chunk_count, bytes_kb, e)).red());
                        break;
                    }
                }
            }

            println!("{}", style(format!("DEBUG: Audio send task ending (sent {} chunks total)", chunk_count)).yellow());

            // Send termination message (v3 API format)
            match write
                .send(Message::Text(json!({"type": "Terminate"}).to_string()))
                .await
            {
                Ok(_) => println!("{}", style("DEBUG: Sent v3 termination message").yellow()),
                Err(e) => eprintln!("{}", style(format!("ERROR: Failed to send termination: {}", e)).red()),
            }
        });

        // Receive transcription results
        println!("{}", style("DEBUG: Starting message receive loop...").yellow());
        let mut message_count = 0u64;

        while let Some(msg) = read.next().await {
            message_count += 1;
            println!("{}", style(format!("DEBUG: Received message #{} from AssemblyAI", message_count)).cyan());

            match msg {
                Ok(Message::Text(text)) => {
                    println!("{}", style(format!("DEBUG: Message text (first 200 chars): {}",
                        if text.len() > 200 { &text[..200] } else { &text }
                    )).cyan());

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
                            println!("{}", style(format!("DEBUG: Parsed message type: {}", msg_type.message_type)).cyan());

                            match msg_type.message_type.as_str() {
                                "Begin" => {
                                    match serde_json::from_str::<BeginMessage>(&text) {
                                        Ok(begin_msg) => {
                                            println!("{}", style(format!("✓ Transcription session started (ID: {})", begin_msg.id)).green());
                                        }
                                        Err(e) => {
                                            eprintln!("{}", style(format!("ERROR: Failed to parse Begin message: {}", e)).red());
                                        }
                                    }
                                }
                                "Turn" => {
                                    match serde_json::from_str::<TurnMessage>(&text) {
                                        Ok(turn_msg) => {
                                            let status = if turn_msg.end_of_turn {
                                                format!("Turn #{} [FINAL]", turn_msg.turn_order)
                                            } else {
                                                format!("Turn #{} [partial]", turn_msg.turn_order)
                                            };

                                            println!("{}", style(format!("DEBUG: {} - '{}'", status, turn_msg.transcript)).green());

                                            // Add to transcript buffer if there's text
                                            if !turn_msg.transcript.is_empty() {
                                                if let Ok(mut buffer) = transcript_buffer.lock() {
                                                    // For end_of_turn, add as a new segment
                                                    // For partial turns, replace the last partial segment
                                                    if turn_msg.end_of_turn {
                                                        buffer.push(turn_msg.transcript.clone());
                                                        println!("{}", style(format!("DEBUG: Added final turn to buffer (now {} segments)", buffer.len())).green());
                                                    } else {
                                                        // Replace last segment if it exists and is from same turn
                                                        if buffer.is_empty() || turn_msg.turn_order == 1 {
                                                            buffer.push(turn_msg.transcript.clone());
                                                        } else {
                                                            let last_idx = buffer.len() - 1;
                                                            buffer[last_idx] = turn_msg.transcript.clone();
                                                        }
                                                        println!("{}", style(format!("DEBUG: Updated partial turn (buffer has {} segments)", buffer.len())).yellow());
                                                    }
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            eprintln!("{}", style(format!("ERROR: Failed to parse Turn message: {}", e)).red());
                                        }
                                    }
                                }
                                "Termination" => {
                                    match serde_json::from_str::<TerminationMessage>(&text) {
                                        Ok(term_msg) => {
                                            println!("{}", style(format!("DEBUG: Session terminated - Audio: {}s, Session: {}s",
                                                term_msg.audio_duration_seconds, term_msg.session_duration_seconds)).yellow());
                                            println!("{}", style("Transcription session ended").yellow());
                                        }
                                        Err(e) => {
                                            eprintln!("{}", style(format!("ERROR: Failed to parse Termination message: {}", e)).red());
                                        }
                                    }
                                    break;
                                }
                                _ => {
                                    println!("{}", style(format!("DEBUG: Unhandled message type: {}", msg_type.message_type)).yellow());
                                    eprintln!("{}", style(format!("DEBUG: Raw message: {}", text)).yellow());
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("{}", style(format!("ERROR: Failed to parse message type: {}", e)).red());
                            eprintln!("{}", style(format!("ERROR: Raw message: {}", text)).red());
                        }
                    }
                }
                Ok(Message::Close(frame)) => {
                    println!("{}", style(format!("DEBUG: WebSocket close frame received: {:?}", frame)).yellow());
                    break;
                }
                Err(e) => {
                    eprintln!("{}", style(format!("ERROR: WebSocket error: {}", e)).red());
                    break;
                }
                _ => {
                    println!("{}", style("DEBUG: Received non-text message").yellow());
                }
            }
        }

        println!("{}", style(format!("DEBUG: Message receive loop ended (received {} messages total)", message_count)).yellow());

        write_handle.abort();
        Ok(())
    }
}

fn collect_input_devices(host: &cpal::Host) -> Result<Vec<DeviceInfo>, String> {
    let default_input = host.default_input_device();
    let default_name = default_input
        .as_ref()
        .and_then(|d| d.name().ok());

    match host.input_devices() {
        Ok(devices) => {
            let device_list: Vec<DeviceInfo> = devices
                .filter_map(|device| {
                    device.name().ok().map(|name| {
                        let is_default = default_name
                            .as_ref()
                            .map(|default| default == &name)
                            .unwrap_or(false);
                        DeviceInfo { name, is_default }
                    })
                })
                .collect();

            if device_list.is_empty() {
                Err("No input devices found".to_string())
            } else {
                Ok(device_list)
            }
        }
        Err(e) => Err(format!("Error enumerating input devices: {}", e)),
    }
}

fn collect_output_devices(host: &cpal::Host) -> Result<Vec<DeviceInfo>, String> {
    let default_output = host.default_output_device();
    let default_name = default_output
        .as_ref()
        .and_then(|d| d.name().ok());

    match host.output_devices() {
        Ok(devices) => {
            let device_list: Vec<DeviceInfo> = devices
                .filter_map(|device| {
                    device.name().ok().map(|name| {
                        let is_default = default_name
                            .as_ref()
                            .map(|default| default == &name)
                            .unwrap_or(false);
                        DeviceInfo { name, is_default }
                    })
                })
                .collect();

            if device_list.is_empty() {
                Err("No output devices found".to_string())
            } else {
                Ok(device_list)
            }
        }
        Err(e) => Err(format!("Error enumerating output devices: {}", e)),
    }
}

// Calculate RMS (Root Mean Square) audio level from samples
fn calculate_rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }

    let sum: f32 = samples.iter().map(|&s| s * s).sum();
    (sum / samples.len() as f32).sqrt()
}

// Downmix stereo (or multi-channel) audio to mono
// AssemblyAI requires mono PCM16 audio
fn downmix_to_mono_i16(samples: &[i16], channels: u16) -> Vec<i16> {
    if channels == 1 {
        // Already mono, return as-is
        return samples.to_vec();
    }

    // Downmix by averaging all channels in each frame
    let channels = channels as usize;
    let frame_count = samples.len() / channels;
    let mut mono_samples = Vec::with_capacity(frame_count);

    for frame_idx in 0..frame_count {
        let start = frame_idx * channels;
        let end = start + channels;

        // Average all channels in this frame
        let sum: i32 = samples[start..end].iter().map(|&s| s as i32).sum();
        let avg = (sum / channels as i32) as i16;
        mono_samples.push(avg);
    }

    mono_samples
}

// Create audio level bar visualization
fn create_audio_bar(level: f32, width: usize) -> String {
    let filled = (level * width as f32) as usize;
    let filled = filled.min(width);
    let empty = width - filled;

    let bar = "█".repeat(filled) + &"░".repeat(empty);

    if level > 0.8 {
        style(bar).red().to_string()
    } else if level > 0.5 {
        style(bar).yellow().to_string()
    } else {
        style(bar).green().to_string()
    }
}

// Display transcript text in terminal (replacing audio bars)
fn display_transcript(transcript_segments: &[String], width: usize) {
    // Show last 5 segments
    let display_count = transcript_segments.len().min(5);
    let start_idx = transcript_segments.len().saturating_sub(display_count);

    for (i, segment) in transcript_segments[start_idx..].iter().enumerate() {
        // Truncate long segments to terminal width
        let display_text = if segment.len() > width {
            format!("{}...", &segment[..width - 3])
        } else {
            segment.clone()
        };

        // Color code: newer segments are brighter
        let styled = if i == display_count - 1 {
            style(&display_text).white().bold()
        } else {
            style(&display_text).dim()
        };

        print!("{}", styled);
        if i < display_count - 1 {
            print!(" ");
        }
    }
}

// Save transcript buffer to TXT file
fn save_transcript_file(transcript_segments: &[String], timestamp_str: &str) -> Result<String> {
    let filename = format!("transcript_{}.txt", timestamp_str);

    let mut file = std::fs::File::create(&filename)
        .context("Failed to create transcript file")?;

    // Write header
    use std::io::Write;
    writeln!(file, "Voice Bird Desktop - Transcript")?;
    writeln!(file, "Timestamp: {}", timestamp_str.replace('_', " "))?;
    writeln!(file, "Segments: {}", transcript_segments.len())?;
    writeln!(file, "{}", "=".repeat(50))?;
    writeln!(file)?;

    // Write all transcript segments
    for (i, segment) in transcript_segments.iter().enumerate() {
        writeln!(file, "[{}] {}", i + 1, segment)?;
    }

    // Write summary
    let word_count: usize = transcript_segments
        .iter()
        .map(|s| s.split_whitespace().count())
        .sum();
    writeln!(file)?;
    writeln!(file, "{}", "=".repeat(50))?;
    writeln!(file, "Total words: ~{}", word_count)?;

    Ok(filename)
}

// Save audio buffer to WAV file
fn save_audio_file(audio_buffer: &[f32], sample_rate: u32, channels: u16, timestamp_str: &str) -> Result<String> {
    let filename = format!("recording_{}.wav", timestamp_str);

    // Create WAV spec
    let spec = WavSpec {
        channels,
        sample_rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };

    // Write WAV file
    let mut writer = WavWriter::create(&filename, spec)
        .context("Failed to create WAV file")?;

    for &sample in audio_buffer {
        writer.write_sample(sample)
            .context("Failed to write audio sample")?;
    }

    writer.finalize()
        .context("Failed to finalize WAV file")?;

    Ok(filename)
}

// Stream audio from input device
fn stream_audio(device: &Device, device_name: &str) -> Result<()> {
    println!();
    println!("{}", style("=== AUDIO STREAMING ===").bold().green());
    println!("Device: {}", style(device_name).cyan().bold());
    println!();

    // Check for API key for transcription
    let api_key = env::var("ASSEMBLYAI_API_KEY").ok();

    // Validate API key if present
    let transcription_enabled = if let Some(ref key) = api_key {
        let key_trimmed = key.trim();

        // Check for placeholder value
        if key_trimmed == "your-api-key-here" || key_trimmed.is_empty() {
            println!("{}", style("━".repeat(60)).yellow());
            println!("{}", style("⚠ INVALID API KEY CONFIGURATION").yellow().bold());
            println!("{}", style("  Your ASSEMBLYAI_API_KEY is set to a placeholder value.").yellow());
            println!();
            println!("{}", style("To enable transcription:").cyan().bold());
            println!("{}", style("  1. Get a free API key from: https://www.assemblyai.com/").cyan());
            println!("{}", style("  2. Open your .env file").cyan());
            println!("{}", style("  3. Replace 'your-api-key-here' with your actual API key").cyan());
            println!("{}", style("━".repeat(60)).yellow());
            println!();
            println!("{}", style("Falling back to audio level visualization...").yellow());
            println!();
            false
        } else if key_trimmed.len() < 20 {
            println!("{}", style("━".repeat(60)).yellow());
            println!("{}", style("⚠ SUSPICIOUS API KEY").yellow().bold());
            println!("{}", style(format!("  Your API key is only {} characters long.", key_trimmed.len())).yellow());
            println!("{}", style("  AssemblyAI keys are typically 32+ characters.").yellow());
            println!();
            println!("{}", style("Please verify your API key at: https://www.assemblyai.com/").cyan());
            println!("{}", style("━".repeat(60)).yellow());
            println!();
            println!("{}", style("Attempting connection anyway...").yellow());
            println!();
            true
        } else {
            println!("{}", style("✓ Transcription enabled (AssemblyAI)").green());
            println!("{}", style(format!("  API key: {}****", &key_trimmed[..15.min(key_trimmed.len())])).cyan());
            true
        }
    } else {
        println!("{}", style("⚠ Transcription disabled (set ASSEMBLYAI_API_KEY environment variable)").yellow());
        println!("{}", style("  Falling back to audio level visualization").yellow());
        false
    };

    println!();
    println!("{}", style("Press ESC to stop streaming...").yellow());
    println!();

    // Get default config
    let config = device.default_input_config()
        .context("Failed to get default input config")?;

    let sample_rate = config.sample_rate().0;
    let channels = config.channels();

    println!("Stream config: {} Hz, {} channels, format: {:?}",
        sample_rate,
        channels,
        config.sample_format()
    );
    println!("{}", style(format!("DEBUG - sample_rate: {}, channels: {}", sample_rate, channels)).yellow());

    if transcription_enabled && channels > 1 {
        println!("{}", style(format!("⚡ Audio will be downmixed from {} channels to MONO for AssemblyAI", channels)).cyan());
    }
    println!();

    // Initialize transcription service if enabled
    let (audio_tx, transcript_buffer) = if transcription_enabled {
        let (tx, rx) = std::sync::mpsc::channel::<Vec<i16>>();
        let service = TranscriptionService::new(api_key.unwrap());
        let buffer = service.get_transcript_buffer();

        // Start transcription in background
        let thread_handle = std::thread::Builder::new()
            .name("transcription-thread".to_string())
            .spawn(move || {
                println!("{}", style(format!("DEBUG: Transcription thread started (ID: {:?})", std::thread::current().id())).yellow());

                let result = std::panic::catch_unwind(|| {
                    let rt = match Runtime::new() {
                        Ok(rt) => {
                            println!("{}", style("DEBUG: Tokio runtime created successfully").green());
                            rt
                        }
                        Err(e) => {
                            eprintln!("{}", style(format!("ERROR: Failed to create Tokio runtime: {}", e)).red());
                            return;
                        }
                    };

                    rt.block_on(async {
                        println!("{}", style("DEBUG: Entering async transcription block...").yellow());
                        match TranscriptionService::transcribe_stream(
                            service.api_key.clone(),
                            rx,
                            sample_rate,
                            service.transcript_buffer.clone(),
                        )
                        .await
                        {
                            Ok(_) => println!("{}", style("DEBUG: Transcription stream completed successfully").green()),
                            Err(e) => eprintln!("{}", style(format!("ERROR: Transcription error: {}", e)).red()),
                        }
                    });
                });

                match result {
                    Ok(_) => println!("{}", style("DEBUG: Transcription thread ending normally").yellow()),
                    Err(e) => eprintln!("{}", style(format!("ERROR: Transcription thread panicked: {:?}", e)).red()),
                }
            });

        match thread_handle {
            Ok(_) => println!("{}", style("DEBUG: Transcription thread spawned successfully").green()),
            Err(e) => eprintln!("{}", style(format!("ERROR: Failed to spawn transcription thread: {}", e)).red()),
        }

        (Some(tx), Some(buffer))
    } else {
        (None, None)
    };

    // Shared audio level state
    let audio_level = Arc::new(Mutex::new(0.0f32));
    let audio_level_clone = audio_level.clone();

    // Shared audio buffer for recording
    let audio_buffer = Arc::new(Mutex::new(Vec::<f32>::new()));
    let audio_buffer_f32 = audio_buffer.clone();
    let audio_buffer_i16 = audio_buffer.clone();
    let audio_buffer_u16 = audio_buffer.clone();

    // Callback statistics
    let callback_count = Arc::new(Mutex::new(0u64));
    let callback_count_f32 = callback_count.clone();
    let callback_count_i16 = callback_count.clone();
    let callback_count_u16 = callback_count.clone();

    // Create transcription buffer for 1-second accumulation
    let transcription_buffer = Arc::new(Mutex::new(Vec::<i16>::new()));
    let transcription_buffer_f32 = transcription_buffer.clone();
    let transcription_buffer_i16 = transcription_buffer.clone();
    let transcription_buffer_u16 = transcription_buffer.clone();

    // Calculate target buffer size for 50ms of MONO samples (optimal latency per AssemblyAI docs)
    // AssemblyAI requires mono audio, so we'll downmix before sending
    // 50ms = sample_rate / 20 (e.g., 48000 / 20 = 2400 samples = ~4.8KB raw PCM16)
    let target_buffer_size = (sample_rate / 20) as usize;

    // Clone audio_tx and channels for each callback
    let audio_tx_f32 = audio_tx.clone();
    let audio_tx_i16 = audio_tx.clone();
    let audio_tx_u16 = audio_tx.clone();
    let channels_f32 = channels;
    let channels_i16 = channels;
    let channels_u16 = channels;

    // Build stream based on sample format
    let stream = match config.sample_format() {
        SampleFormat::F32 => {
            device.build_input_stream(
                &config.into(),
                move |data: &[f32], _: &_| {
                    let rms = calculate_rms(data);
                    if let Ok(mut level) = audio_level_clone.lock() {
                        *level = rms;
                    }
                    // Store audio samples
                    if let Ok(mut buffer) = audio_buffer_f32.lock() {
                        buffer.extend_from_slice(data);
                    }
                    // Accumulate samples for transcription (with 1-second buffering)
                    if let Some(ref tx) = audio_tx_f32 {
                        // Convert f32 to i16
                        let i16_samples: Vec<i16> = data
                            .iter()
                            .map(|&s| (s * i16::MAX as f32) as i16)
                            .collect();

                        // Downmix to mono (AssemblyAI requirement)
                        let mono_samples = downmix_to_mono_i16(&i16_samples, channels_f32);

                        if let Ok(mut trans_buffer) = transcription_buffer_f32.lock() {
                            trans_buffer.extend_from_slice(&mono_samples);

                            // Send when we have ~1 second of mono audio
                            if trans_buffer.len() >= target_buffer_size {
                                if let Err(e) = tx.send(trans_buffer.clone()) {
                                    eprintln!("{}", style(format!("ERROR: Failed to send audio to transcription: {}", e)).red());
                                }
                                trans_buffer.clear();
                            }
                        }
                    }
                    // Count callbacks
                    if let Ok(mut count) = callback_count_f32.lock() {
                        *count += 1;
                    }
                },
                |err| eprintln!("Stream error: {}", err),
                None,
            )?
        },
        SampleFormat::I16 => {
            device.build_input_stream(
                &config.into(),
                move |data: &[i16], _: &_| {
                    let samples: Vec<f32> = data.iter().map(|&s| s as f32 / i16::MAX as f32).collect();
                    let rms = calculate_rms(&samples);
                    if let Ok(mut level) = audio_level_clone.lock() {
                        *level = rms;
                    }
                    // Store audio samples
                    if let Ok(mut buffer) = audio_buffer_i16.lock() {
                        buffer.extend_from_slice(&samples);
                    }
                    // Accumulate samples for transcription (with 1-second buffering)
                    if let Some(ref tx) = audio_tx_i16 {
                        // Downmix to mono (AssemblyAI requirement)
                        let mono_samples = downmix_to_mono_i16(data, channels_i16);

                        if let Ok(mut trans_buffer) = transcription_buffer_i16.lock() {
                            trans_buffer.extend_from_slice(&mono_samples);

                            // Send when we have ~1 second of mono audio
                            if trans_buffer.len() >= target_buffer_size {
                                if let Err(e) = tx.send(trans_buffer.clone()) {
                                    eprintln!("{}", style(format!("ERROR: Failed to send audio to transcription: {}", e)).red());
                                }
                                trans_buffer.clear();
                            }
                        }
                    }
                    // Count callbacks
                    if let Ok(mut count) = callback_count_i16.lock() {
                        *count += 1;
                    }
                },
                |err| eprintln!("Stream error: {}", err),
                None,
            )?
        },
        SampleFormat::U16 => {
            device.build_input_stream(
                &config.into(),
                move |data: &[u16], _: &_| {
                    let samples: Vec<f32> = data.iter()
                        .map(|&s| (s as f32 - 32768.0) / 32768.0)
                        .collect();
                    let rms = calculate_rms(&samples);
                    if let Ok(mut level) = audio_level_clone.lock() {
                        *level = rms;
                    }
                    // Store audio samples
                    if let Ok(mut buffer) = audio_buffer_u16.lock() {
                        buffer.extend_from_slice(&samples);
                    }
                    // Accumulate samples for transcription (with 1-second buffering)
                    if let Some(ref tx) = audio_tx_u16 {
                        // Convert u16 to i16
                        let i16_samples: Vec<i16> = data
                            .iter()
                            .map(|&s| (s as i32 - 32768) as i16)
                            .collect();

                        // Downmix to mono (AssemblyAI requirement)
                        let mono_samples = downmix_to_mono_i16(&i16_samples, channels_u16);

                        if let Ok(mut trans_buffer) = transcription_buffer_u16.lock() {
                            trans_buffer.extend_from_slice(&mono_samples);

                            // Send when we have ~1 second of mono audio
                            if trans_buffer.len() >= target_buffer_size {
                                if let Err(e) = tx.send(trans_buffer.clone()) {
                                    eprintln!("{}", style(format!("ERROR: Failed to send audio to transcription: {}", e)).red());
                                }
                                trans_buffer.clear();
                            }
                        }
                    }
                    // Count callbacks
                    if let Ok(mut count) = callback_count_u16.lock() {
                        *count += 1;
                    }
                },
                |err| eprintln!("Stream error: {}", err),
                None,
            )?
        },
        _ => {
            return Err(anyhow::anyhow!("Unsupported sample format"));
        }
    };

    // Start the stream
    stream.play().context("Failed to start audio stream")?;

    // Track recording start time
    let start_time = Instant::now();

    // Enable raw mode for keyboard input
    terminal::enable_raw_mode().context("Failed to enable raw mode")?;

    let mut stdout = stdout();

    // Main streaming loop
    let result = loop {
        // Check for keyboard input (non-blocking with reduced timeout)
        if event::poll(Duration::from_millis(10)).context("Failed to poll events")? {
            if let Event::Key(KeyEvent { code: KeyCode::Esc, .. }) = event::read()? {
                break Ok(());
            }
        }

        // Display transcription or audio level
        if let Some(ref buffer) = transcript_buffer {
            if let Ok(segments) = buffer.lock() {
                if !segments.is_empty() {
                    execute!(
                        stdout,
                        cursor::MoveTo(0, 8),
                        terminal::Clear(ClearType::FromCursorDown)
                    )?;
                    print!("Transcript: ");
                    display_transcript(&segments, 100);
                    stdout.flush()?;
                }
            }
        } else {
            // Fallback to audio level visualization
            let level = if let Ok(l) = audio_level.lock() {
                *l
            } else {
                0.0
            };

            execute!(
                stdout,
                cursor::MoveTo(0, 8),
                terminal::Clear(ClearType::CurrentLine)
            )?;

            print!("Level: {}", create_audio_bar(level, 50));
            print!("  {:.2}%", level * 100.0);
            stdout.flush()?;
        }
    };

    // Cleanup
    drop(stream);
    terminal::disable_raw_mode().context("Failed to disable raw mode")?;

    // Calculate actual recording duration
    let elapsed = start_time.elapsed();

    println!();
    println!();
    println!("{}", style("Streaming stopped.").green());
    println!("{}", style(format!("Wall-clock recording time: {:.2} seconds", elapsed.as_secs_f32())).cyan());

    // Display callback statistics
    if let Ok(count) = callback_count.lock() {
        println!("{}", style(format!("Audio callbacks received: {}", count)).cyan());
        let expected_callbacks = (elapsed.as_secs_f32() * 100.0) as u64; // Rough estimate (depends on buffer size)
        println!("{}", style(format!("Estimated expected callbacks: ~{} (actual depends on buffer size)", expected_callbacks)).cyan());
    }

    // Generate timestamp for file naming
    let timestamp = Local::now().format("%Y-%m-%d_%H-%M-%S").to_string();

    // Save audio file
    if let Ok(buffer) = audio_buffer.lock() {
        if buffer.is_empty() {
            println!("{}", style("No audio data recorded.").yellow());
        } else {
            println!("{}", style("Saving audio file...").cyan());
            match save_audio_file(&buffer, sample_rate, channels, &timestamp) {
                Ok(filename) => {
                    println!("{} {}",
                        style("✓ Audio saved to:").green().bold(),
                        style(&filename).cyan().bold()
                    );
                    println!("  Duration: {:.2} seconds", buffer.len() as f32 / (sample_rate * channels as u32) as f32);
                    println!("  Samples: {}", buffer.len());
                }
                Err(e) => {
                    eprintln!("{} {}",
                        style("✗ Failed to save audio:").red().bold(),
                        e
                    );
                }
            }
        }
    }

    // Save transcript file if transcription was enabled
    if let Some(buffer) = transcript_buffer {
        if let Ok(segments) = buffer.lock() {
            if !segments.is_empty() {
                println!("{}", style("Saving transcript file...").cyan());
                match save_transcript_file(&segments, &timestamp) {
                    Ok(filename) => {
                        println!("{} {}",
                            style("✓ Transcript saved to:").green().bold(),
                            style(&filename).cyan().bold()
                        );
                        println!("  Segments: {}", segments.len());
                        let word_count: usize = segments
                            .iter()
                            .map(|s| s.split_whitespace().count())
                            .sum();
                        println!("  Words: ~{}", word_count);
                    }
                    Err(e) => {
                        eprintln!("{} {}",
                            style("✗ Failed to save transcript:").red().bold(),
                            e
                        );
                    }
                }
            } else {
                println!("{}", style("No transcript data recorded.").yellow());
            }
        }
    }

    result
}

// Stream audio from output device using Windows WASAPI loopback
#[cfg(windows)]
fn stream_output_audio(_device: &Device, device_name: &str) -> Result<()> {
    println!();
    println!("{}", style("=== AUDIO STREAMING (Loopback) ===").bold().green());
    println!("Device: {}", style(device_name).cyan().bold());
    println!();

    // Check for API key for transcription
    let api_key = env::var("ASSEMBLYAI_API_KEY").ok();

    // Validate API key if present
    let transcription_enabled = if let Some(ref key) = api_key {
        let key_trimmed = key.trim();

        // Check for placeholder value
        if key_trimmed == "your-api-key-here" || key_trimmed.is_empty() {
            println!("{}", style("━".repeat(60)).yellow());
            println!("{}", style("⚠ INVALID API KEY CONFIGURATION").yellow().bold());
            println!("{}", style("  Your ASSEMBLYAI_API_KEY is set to a placeholder value.").yellow());
            println!();
            println!("{}", style("To enable transcription:").cyan().bold());
            println!("{}", style("  1. Get a free API key from: https://www.assemblyai.com/").cyan());
            println!("{}", style("  2. Open your .env file").cyan());
            println!("{}", style("  3. Replace 'your-api-key-here' with your actual API key").cyan());
            println!("{}", style("━".repeat(60)).yellow());
            println!();
            println!("{}", style("Falling back to audio level visualization...").yellow());
            println!();
            false
        } else if key_trimmed.len() < 20 {
            println!("{}", style("━".repeat(60)).yellow());
            println!("{}", style("⚠ SUSPICIOUS API KEY").yellow().bold());
            println!("{}", style(format!("  Your API key is only {} characters long.", key_trimmed.len())).yellow());
            println!("{}", style("  AssemblyAI keys are typically 32+ characters.").yellow());
            println!();
            println!("{}", style("Please verify your API key at: https://www.assemblyai.com/").cyan());
            println!("{}", style("━".repeat(60)).yellow());
            println!();
            println!("{}", style("Attempting connection anyway...").yellow());
            println!();
            true
        } else {
            println!("{}", style("✓ Transcription enabled (AssemblyAI)").green());
            println!("{}", style(format!("  API key: {}****", &key_trimmed[..15.min(key_trimmed.len())])).cyan());
            true
        }
    } else {
        println!("{}", style("⚠ Transcription disabled (set ASSEMBLYAI_API_KEY environment variable)").yellow());
        println!("{}", style("  Falling back to audio level visualization").yellow());
        false
    };

    println!();
    println!("{}", style("Press ESC to stop streaming...").yellow());
    println!();

    unsafe {
        // Initialize COM (if not already initialized)
        // Note: cpal may have already initialized COM, so we accept both S_OK and error results
        let com_init_result = CoInitializeEx(None, COINIT_MULTITHREADED);
        let should_uninit_com = com_init_result.is_ok();

        // Create device enumerator
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(
            &MMDeviceEnumerator,
            None,
            CLSCTX_ALL,
        ).ok()
        .context("Failed to create device enumerator")?;

        // Get the output device for loopback
        let mm_device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)
            .ok()
            .context("Failed to get default output device")?;

        // Activate audio client
        let audio_client: IAudioClient = mm_device.Activate(CLSCTX_ALL, None)
            .ok()
            .context("Failed to activate audio client")?;

        // Get the mix format
        let format_ptr = audio_client.GetMixFormat()
            .ok()
            .context("Failed to get mix format")?;
        let format = &*format_ptr;

        // Copy packed struct fields to local variables to avoid unaligned references
        let sample_rate = format.nSamplesPerSec;
        let channels = format.nChannels;
        let format_tag = format.wFormatTag;

        // Initialize audio client in loopback mode
        audio_client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK,
            10000000, // 1 second buffer
            0,
            format,
            None,
        ).ok()
        .context("Failed to initialize audio client")?;

        // Get buffer size
        let _buffer_frame_count = audio_client.GetBufferSize()
            .ok()
            .context("Failed to get buffer size")?;

        // Get capture client
        let capture_client: IAudioCaptureClient = audio_client.GetService()
            .ok()
            .context("Failed to get capture client")?;

        // Start the audio stream
        audio_client.Start()
            .ok()
            .context("Failed to start audio stream")?;

        println!("Stream config: {} Hz, {} channels, format tag: {}",
            sample_rate,
            channels,
            format_tag
        );
        println!("{}", style(format!("DEBUG - sample_rate: {}, channels: {}", sample_rate, channels)).yellow());

        if transcription_enabled && channels > 1 {
            println!("{}", style(format!("⚡ Audio will be downmixed from {} channels to MONO for AssemblyAI", channels)).cyan());
        }
        println!();

        // Initialize transcription service if enabled
        let (audio_tx, transcript_buffer) = if transcription_enabled {
            let (tx, rx) = std::sync::mpsc::channel::<Vec<i16>>();
            let service = TranscriptionService::new(api_key.unwrap());
            let buffer = service.get_transcript_buffer();

            // Start transcription in background
            let thread_handle = std::thread::Builder::new()
                .name("transcription-thread".to_string())
                .spawn(move || {
                    println!("{}", style(format!("DEBUG: Transcription thread started (ID: {:?})", std::thread::current().id())).yellow());

                    let result = std::panic::catch_unwind(|| {
                        let rt = match Runtime::new() {
                            Ok(rt) => {
                                println!("{}", style("DEBUG: Tokio runtime created successfully").green());
                                rt
                            }
                            Err(e) => {
                                eprintln!("{}", style(format!("ERROR: Failed to create Tokio runtime: {}", e)).red());
                                return;
                            }
                        };

                        rt.block_on(async {
                            println!("{}", style("DEBUG: Entering async transcription block...").yellow());
                            match TranscriptionService::transcribe_stream(
                                service.api_key.clone(),
                                rx,
                                sample_rate,
                                service.transcript_buffer.clone(),
                            )
                            .await
                            {
                                Ok(_) => println!("{}", style("DEBUG: Transcription stream completed successfully").green()),
                                Err(e) => eprintln!("{}", style(format!("ERROR: Transcription error: {}", e)).red()),
                            }
                        });
                    });

                    match result {
                        Ok(_) => println!("{}", style("DEBUG: Transcription thread ending normally").yellow()),
                        Err(e) => eprintln!("{}", style(format!("ERROR: Transcription thread panicked: {:?}", e)).red()),
                    }
                });

            match thread_handle {
                Ok(_) => println!("{}", style("DEBUG: Transcription thread spawned successfully").green()),
                Err(e) => eprintln!("{}", style(format!("ERROR: Failed to spawn transcription thread: {}", e)).red()),
            }

            (Some(tx), Some(buffer))
        } else {
            (None, None)
        };

        // Shared audio level state
        let audio_level = Arc::new(Mutex::new(0.0f32));
        let audio_buffer = Arc::new(Mutex::new(Vec::<f32>::new()));

        // Create transcription buffer for 1-second accumulation
        let transcription_buffer = Arc::new(Mutex::new(Vec::<i16>::new()));

        // Calculate target buffer size for 50ms of MONO samples (optimal latency per AssemblyAI docs)
        // AssemblyAI requires mono audio, so we'll downmix before sending
        // 50ms = sample_rate / 20 (e.g., 48000 / 20 = 2400 samples = ~4.8KB raw PCM16)
        let target_buffer_size = (sample_rate / 20) as usize;

        // Packet statistics
        let packet_count = Arc::new(Mutex::new(0u64));

        // Track recording start time
        let start_time = Instant::now();

        // Enable raw mode for keyboard input
        terminal::enable_raw_mode().context("Failed to enable raw mode")?;

        let mut stdout = stdout();

        // Main streaming loop
        let result = loop {
            // Check for keyboard input (non-blocking with reduced timeout)
            if event::poll(Duration::from_millis(10))? {
                if let Event::Key(KeyEvent { code: KeyCode::Esc, .. }) = event::read()? {
                    break Ok(());
                }
            }

            // Process ALL available packets (drain the buffer)
            loop {
                let packet_size = capture_client.GetNextPacketSize()
                    .ok()
                    .context("Failed to get next packet size")?;

                if packet_size == 0 {
                    break; // No more packets available
                }

                let mut buffer_ptr: *mut u8 = std::ptr::null_mut();
                let mut num_frames_available = 0u32;
                let mut flags = 0u32;

                capture_client.GetBuffer(
                    &mut buffer_ptr as *mut *mut u8,
                    &mut num_frames_available,
                    &mut flags,
                    None,
                    None,
                ).ok()
                .context("Failed to get buffer")?;

                if num_frames_available > 0 {
                    // Convert buffer to f32 samples
                    let sample_count = (num_frames_available * channels as u32) as usize;

                    // Assuming float format (most common for loopback)
                    if format_tag as u32 == WAVE_FORMAT_IEEE_FLOAT || format_tag as u32 == WAVE_FORMAT_EXTENSIBLE {
                        let float_buffer = std::slice::from_raw_parts(
                            buffer_ptr as *const f32,
                            sample_count
                        );

                        // Calculate RMS level for visualization
                        let rms = calculate_rms(float_buffer);
                        if let Ok(mut level) = audio_level.lock() {
                            *level = rms;
                        }

                        // Store audio samples directly without intermediate Vec
                        if let Ok(mut buffer) = audio_buffer.lock() {
                            buffer.extend_from_slice(float_buffer);
                        }

                        // Accumulate samples for transcription (with 1-second buffering)
                        if let Some(ref tx) = audio_tx {
                            // Convert f32 to i16
                            let i16_samples: Vec<i16> = float_buffer
                                .iter()
                                .map(|&s| (s * i16::MAX as f32) as i16)
                                .collect();

                            // Downmix to mono (AssemblyAI requirement)
                            let mono_samples = downmix_to_mono_i16(&i16_samples, channels);

                            if let Ok(mut trans_buffer) = transcription_buffer.lock() {
                                trans_buffer.extend_from_slice(&mono_samples);

                                // Send when we have ~1 second of mono audio
                                if trans_buffer.len() >= target_buffer_size {
                                    if let Err(e) = tx.send(trans_buffer.clone()) {
                                        eprintln!("{}", style(format!("ERROR: Failed to send audio to transcription: {}", e)).red());
                                    }
                                    trans_buffer.clear();
                                }
                            }
                        }
                    }

                    // Count packets processed
                    if let Ok(mut count) = packet_count.lock() {
                        *count += 1;
                    }
                }

                capture_client.ReleaseBuffer(num_frames_available)
                    .ok()
                    .context("Failed to release buffer")?;
            }

            // Display transcription or audio level
            if let Some(ref buffer) = transcript_buffer {
                if let Ok(segments) = buffer.lock() {
                    if !segments.is_empty() {
                        execute!(
                            stdout,
                            cursor::MoveTo(0, 8),
                            terminal::Clear(ClearType::FromCursorDown)
                        )?;
                        print!("Transcript: ");
                        display_transcript(&segments, 100);
                        stdout.flush()?;
                    }
                }
            } else {
                // Fallback to audio level visualization
                let level = if let Ok(l) = audio_level.lock() {
                    *l
                } else {
                    0.0
                };

                execute!(
                    stdout,
                    cursor::MoveTo(0, 8),
                    terminal::Clear(ClearType::CurrentLine)
                )?;

                print!("Level: {}", create_audio_bar(level, 50));
                print!("  {:.2}%", level * 100.0);
                stdout.flush()?;
            }
        };

        // Cleanup
        audio_client.Stop().ok();
        terminal::disable_raw_mode().context("Failed to disable raw mode")?;

        // Calculate actual recording duration
        let elapsed = start_time.elapsed();

        println!();
        println!();
        println!("{}", style("Streaming stopped.").green());
        println!("{}", style(format!("Wall-clock recording time: {:.2} seconds", elapsed.as_secs_f32())).cyan());

        // Display packet statistics
        if let Ok(count) = packet_count.lock() {
            println!("{}", style(format!("Audio packets processed: {}", count)).cyan());
            let expected_packets = (elapsed.as_secs_f32() * 100.0) as u64; // Rough estimate (depends on packet size)
            println!("{}", style(format!("Estimated expected packets: ~{} (actual depends on packet size)", expected_packets)).cyan());
        }

        // Generate timestamp for file naming
        let timestamp = Local::now().format("%Y-%m-%d_%H-%M-%S").to_string();

        // Save audio file
        if let Ok(buffer) = audio_buffer.lock() {
            if buffer.is_empty() {
                println!("{}", style("No audio data recorded.").yellow());
            } else {
                println!("{}", style("Saving audio file...").cyan());
                match save_audio_file(&buffer, sample_rate, channels, &timestamp) {
                    Ok(filename) => {
                        println!("{} {}",
                            style("✓ Audio saved to:").green().bold(),
                            style(&filename).cyan().bold()
                        );
                        println!("  Duration: {:.2} seconds", buffer.len() as f32 / (sample_rate * channels as u32) as f32);
                        println!("  Samples: {}", buffer.len());
                    }
                    Err(e) => {
                        eprintln!("{} {}",
                            style("✗ Failed to save audio:").red().bold(),
                            e
                        );
                    }
                }
            }
        }

        // Save transcript file if transcription was enabled
        if let Some(buffer) = transcript_buffer {
            if let Ok(segments) = buffer.lock() {
                if !segments.is_empty() {
                    println!("{}", style("Saving transcript file...").cyan());
                    match save_transcript_file(&segments, &timestamp) {
                        Ok(filename) => {
                            println!("{} {}",
                                style("✓ Transcript saved to:").green().bold(),
                                style(&filename).cyan().bold()
                            );
                            println!("  Segments: {}", segments.len());
                            let word_count: usize = segments
                                .iter()
                                .map(|s| s.split_whitespace().count())
                                .sum();
                            println!("  Words: ~{}", word_count);
                        }
                        Err(e) => {
                            eprintln!("{} {}",
                                style("✗ Failed to save transcript:").red().bold(),
                                e
                            );
                        }
                    }
                } else {
                    println!("{}", style("No transcript data recorded.").yellow());
                }
            }
        }

        CoTaskMemFree(Some(format_ptr as *const _ as *const _));

        // Only uninitialize COM if we were the ones who initialized it
        if should_uninit_com {
            CoUninitialize();
        }

        result
    }
}

// Non-Windows platforms: output streaming not supported
#[cfg(not(windows))]
fn stream_output_audio(_device: &Device, _device_name: &str) -> Result<()> {
    println!("{}", style("Output device streaming is only supported on Windows").yellow());
    println!("{}", style("Please select an input device (microphone) to stream").yellow());
    Ok(())
}

fn main() -> Result<()> {
    // Load .env file if present (for API keys like ASSEMBLYAI_API_KEY)
    dotenvy::dotenv().ok();

    println!("{}", style("=== Audio Device Selector ===").bold().cyan());
    println!();

    let host = cpal::default_host();
    println!("Using audio host: {}", style(format!("{:?}", host.id())).green());
    println!();

    // Step 1: Select device type
    let device_types = vec!["Input Device (Microphone)", "Output Device (Speaker)"];

    let device_type_selection = Select::new()
        .with_prompt("Select device type")
        .items(&device_types)
        .default(0)
        .interact_opt();

    let device_type_index = match device_type_selection {
        Ok(Some(index)) => index,
        Ok(None) => {
            println!("{}", style("Selection cancelled").red());
            return Ok(());
        }
        Err(e) => {
            eprintln!("{}", style(format!("Error: {}", e)).red());
            return Ok(());
        }
    };

    println!();

    // Step 2: Collect devices based on type
    let devices = if device_type_index == 0 {
        collect_input_devices(&host)
    } else {
        collect_output_devices(&host)
    };

    let device_list = match devices {
        Ok(list) => list,
        Err(e) => {
            eprintln!("{}", style(e).red());
            return Ok(());
        }
    };

    // Step 3: Prepare device names for selection
    let device_names: Vec<String> = device_list
        .iter()
        .map(|d| {
            if d.is_default {
                format!("{} [DEFAULT]", d.name)
            } else {
                d.name.clone()
            }
        })
        .collect();

    // Step 4: Let user select a device
    let device_selection = Select::new()
        .with_prompt("Select audio device")
        .items(&device_names)
        .default(0)
        .interact_opt();

    let device_index = match device_selection {
        Ok(Some(index)) => index,
        Ok(None) => {
            println!("{}", style("Selection cancelled").red());
            return Ok(());
        }
        Err(e) => {
            eprintln!("{}", style(format!("Error: {}", e)).red());
            return Ok(());
        }
    };

    let selected_device = &device_list[device_index];

    // Step 5: Get the actual device and stream based on type
    if device_type_index == 0 {
        // Input device (microphone)
        let device = host.input_devices()
            .context("Failed to enumerate input devices")?
            .find(|d| d.name().ok().as_ref() == Some(&selected_device.name))
            .context("Selected device not found")?;

        stream_audio(&device, &selected_device.name)?;
    } else {
        // Output device (speaker/loopback)
        let device = host.output_devices()
            .context("Failed to enumerate output devices")?
            .find(|d| d.name().ok().as_ref() == Some(&selected_device.name))
            .context("Selected device not found")?;

        stream_output_audio(&device, &selected_device.name)?;
    }

    Ok(())
}
