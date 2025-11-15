use anyhow::{Result, Context};
use cpal::{Device, SampleFormat};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use hound::{WavWriter, WavSpec};
use crate::session::RecordingSession;
use crate::grpc_service::GrpcStreamingService;
use std::sync::mpsc;

// Calculate RMS (Root Mean Square) audio level from samples
pub fn calculate_rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }

    let sum: f32 = samples.iter().map(|&s| s * s).sum();
    (sum / samples.len() as f32).sqrt()
}

// Downmix stereo (or multi-channel) audio to mono
#[allow(dead_code)]
pub fn downmix_to_mono_i16(samples: &[i16], channels: u16) -> Vec<i16> {
    if channels == 1 {
        return samples.to_vec();
    }

    let channels = channels as usize;
    let frame_count = samples.len() / channels;
    let mut mono_samples = Vec::with_capacity(frame_count);

    for frame_idx in 0..frame_count {
        let start = frame_idx * channels;
        let end = start + channels;

        let sum: i32 = samples[start..end].iter().map(|&s| s as i32).sum();
        let avg = (sum / channels as i32) as i16;
        mono_samples.push(avg);
    }

    mono_samples
}

// Save audio buffer to WAV file
pub fn save_audio_file(audio_buffer: &[f32], sample_rate: u32, channels: u16, filename: &str) -> Result<()> {
    let spec = WavSpec {
        channels,
        sample_rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };

    let mut writer = WavWriter::create(filename, spec)
        .context("Failed to create WAV file")?;

    for &sample in audio_buffer {
        writer.write_sample(sample)
            .context("Failed to write audio sample")?;
    }

    writer.finalize()
        .context("Failed to finalize WAV file")?;

    Ok(())
}

// Save transcript buffer to TXT file
pub fn save_transcript_file(transcript_segments: &[String], filename: &str) -> Result<()> {
    use std::io::Write;
    let mut file = std::fs::File::create(filename)
        .context("Failed to create transcript file")?;

    writeln!(file, "Voice Bird Desktop - Transcript")?;
    writeln!(file, "{}", "=".repeat(50))?;
    writeln!(file)?;

    for (i, segment) in transcript_segments.iter().enumerate() {
        writeln!(file, "[{}] {}", i + 1, segment)?;
    }

    let word_count: usize = transcript_segments
        .iter()
        .map(|s| s.split_whitespace().count())
        .sum();
    writeln!(file)?;
    writeln!(file, "{}", "=".repeat(50))?;
    writeln!(file, "Total words: ~{}", word_count)?;

    Ok(())
}

// Get device by name from host
pub fn get_input_device_by_name(host: &cpal::Host, name: &str) -> Result<Device> {
    host.input_devices()
        .context("Failed to enumerate input devices")?
        .find(|d| d.name().ok().as_ref().map(|n| n == name).unwrap_or(false))
        .context(format!("Input device '{}' not found", name))
}

#[allow(dead_code)]
pub fn get_output_device_by_name(host: &cpal::Host, name: &str) -> Result<Device> {
    host.output_devices()
        .context("Failed to enumerate output devices")?
        .find(|d| d.name().ok().as_ref().map(|n| n == name).unwrap_or(false))
        .context(format!("Output device '{}' not found", name))
}

// Start recording audio from an input device
pub fn start_input_recording(
    device: &Device,
    session: &mut RecordingSession,
    server_config: (String, String),
) -> Result<cpal::Stream> {
    let config = device.default_input_config()
        .context("Failed to get default input config")?;

    session.sample_rate = config.sample_rate().0;
    session.channels = config.channels();

    let audio_level = session.audio_level.clone();
    let audio_buffer = session.audio_buffer.clone();
    let stop_signal = session.stop_signal.clone();

    // Setup server streaming
    let (server_url, server_api_key) = server_config;
    let (tx, rx) = mpsc::channel::<Vec<f32>>();
    let session_id = session.id.to_string();
    let device_name = session.device_name.clone();
    let sample_rate = session.sample_rate;
    let channels = session.channels;

    // Spawn gRPC streaming thread
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            if let Err(e) = GrpcStreamingService::stream_to_server(
                server_url,
                server_api_key,
                session_id,
                device_name,
                rx,
                sample_rate,
                channels,
            ).await {
                log::error!("gRPC streaming error: {}", e);
            }
        });
    });

    let server_tx = tx;

    let server_tx_f32 = server_tx.clone();
    let server_tx_i16 = server_tx.clone();
    let server_tx_u16 = server_tx.clone();

    let stream = match config.sample_format() {
        SampleFormat::F32 => {
            device.build_input_stream(
                &config.into(),
                move |data: &[f32], _: &_| {
                    // Check stop signal
                    if let Ok(stop) = stop_signal.lock() {
                        if *stop {
                            return;
                        }
                    }

                    let rms = calculate_rms(data);
                    if let Ok(mut level) = audio_level.lock() {
                        *level = rms;
                    }

                    if let Ok(mut buffer) = audio_buffer.lock() {
                        buffer.extend_from_slice(data);
                    }

                    // Send to server streaming service
                    let _ = server_tx_f32.send(data.to_vec());
                },
                |err| log::error!("Stream error: {}", err),
                None,
            )?
        },
        SampleFormat::I16 => {
            device.build_input_stream(
                &config.into(),
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

                    if let Ok(mut buffer) = audio_buffer.lock() {
                        buffer.extend_from_slice(&samples);
                    }

                    // Send to server streaming service (convert to f32)
                    let _ = server_tx_i16.send(samples);
                },
                |err| log::error!("Stream error: {}", err),
                None,
            )?
        },
        SampleFormat::U16 => {
            device.build_input_stream(
                &config.into(),
                move |data: &[u16], _: &_| {
                    if let Ok(stop) = stop_signal.lock() {
                        if *stop {
                            return;
                        }
                    }

                    let samples: Vec<f32> = data.iter()
                        .map(|&s| (s as f32 - 32768.0) / 32768.0)
                        .collect();
                    let rms = calculate_rms(&samples);
                    if let Ok(mut level) = audio_level.lock() {
                        *level = rms;
                    }

                    if let Ok(mut buffer) = audio_buffer.lock() {
                        buffer.extend_from_slice(&samples);
                    }

                    // Send to server streaming service (already converted to f32)
                    let _ = server_tx_u16.send(samples);
                },
                |err| log::error!("Stream error: {}", err),
                None,
            )?
        },
        _ => {
            return Err(anyhow::anyhow!("Unsupported sample format"));
        }
    };

    stream.play().context("Failed to start audio stream")?;
    Ok(stream)
}

// Start recording audio from output device via WASAPI loopback (Windows only)
#[cfg(windows)]
pub fn start_output_recording(
    _device_name: &str,
    session: &mut RecordingSession,
    _api_key: Option<String>,
    server_config: Option<(String, String)>,
) -> Result<Box<dyn FnOnce() + Send>> {
    use windows::{
        Win32::Media::Audio::*,
        Win32::System::Com::*,
    };

    // Get sample rate and channels first
    let (sample_rate, channels) = unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED).ok();

        let enumerator: IMMDeviceEnumerator = CoCreateInstance(
            &MMDeviceEnumerator,
            None,
            CLSCTX_ALL,
        ).ok().context("Failed to create device enumerator")?;

        let mm_device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)
            .ok().context("Failed to get default output device")?;

        let audio_client: IAudioClient = mm_device.Activate(CLSCTX_ALL, None)
            .ok().context("Failed to activate audio client")?;

        let format_ptr = audio_client.GetMixFormat()
            .ok().context("Failed to get mix format")?;
        let format = &*format_ptr;

        let sr = format.nSamplesPerSec;
        let ch = format.nChannels;

        CoTaskMemFree(Some(format_ptr as *const _ as *const _));
        CoUninitialize();

        (sr, ch)
    };

    session.sample_rate = sample_rate;
    session.channels = channels;

    let audio_level = session.audio_level.clone();
    let audio_buffer = session.audio_buffer.clone();
    let stop_signal = session.stop_signal.clone();

    // Setup server streaming if server config is provided
    let server_tx = if let Some((server_url, server_api_key)) = server_config {
        let (tx, rx) = mpsc::channel::<Vec<f32>>();
        let session_id = session.id.to_string();
        let device_name = session.device_name.clone();

        // Spawn gRPC streaming thread
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                if let Err(e) = GrpcStreamingService::stream_to_server(
                    server_url,
                    server_api_key,
                    session_id,
                    device_name,
                    rx,
                    sample_rate,
                    channels,
                ).await {
                    log::error!("gRPC streaming error: {}", e);
                }
            });
        });

        Some(tx)
    } else {
        None
    };

    // Spawn thread to process audio packets
    // All COM operations must happen within this thread
    std::thread::spawn(move || {
        unsafe {
            // Initialize COM for this thread
            if CoInitializeEx(None, COINIT_MULTITHREADED).is_err() {
                log::error!("Failed to initialize COM in audio thread");
                return;
            }

            // Create all COM objects in this thread
            let result = (|| -> Result<()> {
                let enumerator: IMMDeviceEnumerator = CoCreateInstance(
                    &MMDeviceEnumerator,
                    None,
                    CLSCTX_ALL,
                ).ok().context("Failed to create device enumerator")?;

                let mm_device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)
                    .ok().context("Failed to get default output device")?;

                let audio_client: IAudioClient = mm_device.Activate(CLSCTX_ALL, None)
                    .ok().context("Failed to activate audio client")?;

                let format_ptr = audio_client.GetMixFormat()
                    .ok().context("Failed to get mix format")?;
                let format = &*format_ptr;

                audio_client.Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_LOOPBACK,
                    10000000,
                    0,
                    format,
                    None,
                ).ok().context("Failed to initialize audio client")?;

                let capture_client: IAudioCaptureClient = audio_client.GetService()
                    .ok().context("Failed to get capture client")?;

                audio_client.Start()
                    .ok().context("Failed to start audio stream")?;

                // Audio capture loop
                loop {
                    if let Ok(stop) = stop_signal.lock() {
                        if *stop {
                            break;
                        }
                    }

                    std::thread::sleep(std::time::Duration::from_millis(10));

                    loop {
                        let packet_size = capture_client.GetNextPacketSize().ok().unwrap_or(0);
                        if packet_size == 0 {
                            break;
                        }

                        let mut buffer_ptr: *mut u8 = std::ptr::null_mut();
                        let mut num_frames_available = 0u32;
                        let mut flags = 0u32;

                        if capture_client.GetBuffer(
                            &mut buffer_ptr as *mut *mut u8,
                            &mut num_frames_available,
                            &mut flags,
                            None,
                            None,
                        ).is_ok() && num_frames_available > 0 {
                            let sample_count = (num_frames_available * channels as u32) as usize;
                            let float_buffer = std::slice::from_raw_parts(
                                buffer_ptr as *const f32,
                                sample_count
                            );

                            let rms = calculate_rms(float_buffer);
                            if let Ok(mut level) = audio_level.lock() {
                                *level = rms;
                            }

                            if let Ok(mut buffer) = audio_buffer.lock() {
                                buffer.extend_from_slice(float_buffer);
                            }

                            // Send to server streaming service
                            if let Some(ref tx) = server_tx {
                                let _ = tx.send(float_buffer.to_vec());
                            }

                            capture_client.ReleaseBuffer(num_frames_available).ok();
                        }
                    }
                }

                // Cleanup
                audio_client.Stop().ok();
                CoTaskMemFree(Some(format_ptr as *const _ as *const _));

                Ok(())
            })();

            if let Err(e) = result {
                log::error!("Audio recording error: {}", e);
            }

            // Uninitialize COM for this thread
            CoUninitialize();
        }
    });

    Ok(Box::new(move || {}))
}

#[cfg(not(windows))]
pub fn start_output_recording(
    _device_name: &str,
    _session: &mut RecordingSession,
    _api_key: Option<String>,
    _server_config: Option<(String, String)>,
) -> Result<Box<dyn FnOnce() + Send>> {
    Err(anyhow::anyhow!("Output recording is only supported on Windows"))
}
