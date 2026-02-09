use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat};
use std::sync::{Arc, Mutex, mpsc};

use crate::platform::AudioSession;
use crate::streaming;

/// Calculate RMS (Root Mean Square) audio level from samples
pub fn calculate_rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f32 = samples.iter().map(|&s| s * s).sum();
    (sum / samples.len() as f32).sqrt()
}

/// Simple audio converter for resampling and format conversion
pub struct AudioConverter {
    input_sample_rate: u32,
    input_channels: u16,
    output_sample_rate: u32,
}

impl AudioConverter {
    pub fn new(input_sample_rate: u32, input_channels: u16, output_sample_rate: u32) -> Self {
        Self {
            input_sample_rate,
            input_channels,
            output_sample_rate,
        }
    }

    pub fn output_sample_rate(&self) -> u32 {
        self.output_sample_rate
    }

    /// Convert audio: resample and convert to mono PCM16LE
    pub fn convert(&self, samples: &[f32]) -> Vec<u8> {
        // First, downmix to mono if stereo
        let mono_samples: Vec<f32> = if self.input_channels > 1 {
            samples
                .chunks(self.input_channels as usize)
                .map(|chunk| chunk.iter().sum::<f32>() / chunk.len() as f32)
                .collect()
        } else {
            samples.to_vec()
        };

        // Simple resampling (linear interpolation)
        let resampled = if self.input_sample_rate != self.output_sample_rate {
            let ratio = self.input_sample_rate as f64 / self.output_sample_rate as f64;
            let output_len = (mono_samples.len() as f64 / ratio) as usize;
            let mut output = Vec::with_capacity(output_len);

            for i in 0..output_len {
                let src_idx = i as f64 * ratio;
                let idx0 = src_idx.floor() as usize;
                let idx1 = (idx0 + 1).min(mono_samples.len() - 1);
                let frac = src_idx - idx0 as f64;

                let sample = mono_samples[idx0] * (1.0 - frac as f32)
                    + mono_samples[idx1] * frac as f32;
                output.push(sample);
            }
            output
        } else {
            mono_samples
        };

        // Convert to PCM16LE bytes
        resampled
            .iter()
            .flat_map(|&sample| {
                let clamped = sample.clamp(-1.0, 1.0);
                let pcm16 = (clamped * 32767.0) as i16;
                pcm16.to_le_bytes()
            })
            .collect()
    }
}

/// Recording context for a session
pub struct RecordingContext {
    pub stop_signal: Arc<Mutex<bool>>,
    pub audio_level: Arc<Mutex<f32>>,
}

/// Start recording from an input device (microphone)
pub fn start_input_recording(
    session: &AudioSession,
    server_url: String,
    api_key: String,
    session_id: String,
    ctx: RecordingContext,
) -> Result<()> {
    let host = cpal::default_host();

    let device = host
        .input_devices()
        .context("Failed to enumerate input devices")?
        .find(|d| {
            d.name()
                .ok()
                .as_ref()
                .map(|n| n.contains(&session.device_name) || session.device_name.contains(n))
                .unwrap_or(false)
        })
        .or_else(|| host.default_input_device())
        .context("No input device found")?;

    let config = device
        .default_input_config()
        .context("Failed to get input config")?;

    let sample_rate = config.sample_rate().0;
    let channels = config.channels();

    let (tx, rx) = mpsc::channel::<Vec<f32>>();
    let audio_level = ctx.audio_level.clone();
    let stop_signal = ctx.stop_signal.clone();

    // Spawn streaming thread
    let app_name = Some(session.app_name.clone());
    let device_name = session.device_name.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let _ = streaming::stream_to_server(
                server_url,
                api_key,
                session_id,
                device_name,
                app_name,
                true,
                rx,
                sample_rate,
                channels,
            )
            .await;
        });
    });

    // Build audio stream based on sample format
    let stream = match config.sample_format() {
        SampleFormat::F32 => build_input_stream_f32(&device, &config.into(), tx, audio_level, stop_signal)?,
        SampleFormat::I16 => build_input_stream_i16(&device, &config.into(), tx, audio_level, stop_signal)?,
        _ => return Err(anyhow::anyhow!("Unsupported sample format")),
    };

    stream.play().context("Failed to start audio stream")?;

    // Keep stream alive until stop signal
    loop {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if let Ok(stop) = ctx.stop_signal.lock() {
            if *stop {
                break;
            }
        }
    }

    Ok(())
}

fn build_input_stream_f32(
    device: &Device,
    config: &cpal::StreamConfig,
    tx: mpsc::Sender<Vec<f32>>,
    audio_level: Arc<Mutex<f32>>,
    stop_signal: Arc<Mutex<bool>>,
) -> Result<cpal::Stream> {
    let stream = device.build_input_stream(
        config,
        move |data: &[f32], _: &_| {
            if let Ok(stop) = stop_signal.lock() {
                if *stop {
                    return;
                }
            }

            let rms = calculate_rms(data);
            if let Ok(mut level) = audio_level.lock() {
                *level = rms;
            }

            let _ = tx.send(data.to_vec());
        },
        |err| eprintln!("Stream error: {}", err),
        None,
    )?;
    Ok(stream)
}

fn build_input_stream_i16(
    device: &Device,
    config: &cpal::StreamConfig,
    tx: mpsc::Sender<Vec<f32>>,
    audio_level: Arc<Mutex<f32>>,
    stop_signal: Arc<Mutex<bool>>,
) -> Result<cpal::Stream> {
    let stream = device.build_input_stream(
        config,
        move |data: &[i16], _: &_| {
            if let Ok(stop) = stop_signal.lock() {
                if *stop {
                    return;
                }
            }

            let samples: Vec<f32> = data.iter().map(|&s| s as f32 / i16::MAX as f32).collect();
            let rms = calculate_rms(&samples);
            if let Ok(mut level) = audio_level.lock() {
                *level = rms;
            }

            let _ = tx.send(samples);
        },
        |err| eprintln!("Stream error: {}", err),
        None,
    )?;
    Ok(stream)
}
