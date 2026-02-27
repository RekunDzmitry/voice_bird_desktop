use anyhow::{Result, Context};
use cpal::{Device, SampleFormat};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use hound::{WavWriter, WavSpec};
use std::sync::atomic::Ordering;
use crate::session::RecordingSession;
use crate::server_streaming::ServerStreamingService;
use crate::audio_buffer::{AudioPreBuffer, AudioConsumer};

/// Spawn a thread that connects to the WebSocket server and streams audio from the consumer.
/// `pre_buffer.stop()` is called automatically when the thread exits (including on panic)
/// via the `Drop` impl on `AudioPreBuffer`.
fn spawn_streaming_thread(
    consumer: AudioConsumer,
    pre_buffer: AudioPreBuffer,
    server_url: String,
    server_api_key: String,
    session_id: String,
    device_name: String,
    app_name: Option<String>,
    sample_rate: u32,
    channels: u16,
) {
    std::thread::spawn(move || {
        // pre_buffer is moved into this closure; its Drop impl will call stop()
        // even if the thread panics (e.g., Tokio runtime creation fails)
        let _pre_buffer = pre_buffer;

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            if let Err(e) = ServerStreamingService::stream_to_server(
                server_url,
                server_api_key,
                session_id,
                device_name,
                app_name,
                consumer,
                sample_rate,
                channels,
            ).await {
                log::error!("WebSocket streaming error: {}", e);
            }
        });
    });
}

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

    // Setup pre-buffer for audio capture before WebSocket is ready
    let pre_buffer = AudioPreBuffer::new();
    let producer_f32 = pre_buffer.producer();
    let producer_i16 = pre_buffer.producer();
    let producer_u16 = pre_buffer.producer();
    let consumer = pre_buffer.consumer();

    // Build and start audio stream FIRST (before WebSocket connection)
    let stream = match config.sample_format() {
        SampleFormat::F32 => {
            device.build_input_stream(
                &config.into(),
                move |data: &[f32], _: &_| {
                    if stop_signal.load(Ordering::Acquire) {
                        return;
                    }

                    let rms = calculate_rms(data);
                    audio_level.store(rms.to_bits(), Ordering::Relaxed);

                    if let Ok(mut buffer) = audio_buffer.lock() {
                        buffer.extend_from_slice(data);
                    }

                    // Push to pre-buffer (captured even before WebSocket is ready)
                    producer_f32.push(data.to_vec());
                },
                |err| log::error!("Stream error: {}", err),
                None,
            )?
        },
        SampleFormat::I16 => {
            device.build_input_stream(
                &config.into(),
                move |data: &[i16], _: &_| {
                    if stop_signal.load(Ordering::Acquire) {
                        return;
                    }

                    let samples: Vec<f32> = data.iter().map(|&s| s as f32 / i16::MAX as f32).collect();
                    let rms = calculate_rms(&samples);
                    audio_level.store(rms.to_bits(), Ordering::Relaxed);

                    if let Ok(mut buffer) = audio_buffer.lock() {
                        buffer.extend_from_slice(&samples);
                    }

                    // Push to pre-buffer (convert to f32)
                    producer_i16.push(samples);
                },
                |err| log::error!("Stream error: {}", err),
                None,
            )?
        },
        SampleFormat::U16 => {
            device.build_input_stream(
                &config.into(),
                move |data: &[u16], _: &_| {
                    if stop_signal.load(Ordering::Acquire) {
                        return;
                    }

                    let samples: Vec<f32> = data.iter()
                        .map(|&s| (s as f32 - 32768.0) / 32768.0)
                        .collect();
                    let rms = calculate_rms(&samples);
                    audio_level.store(rms.to_bits(), Ordering::Relaxed);

                    if let Ok(mut buffer) = audio_buffer.lock() {
                        buffer.extend_from_slice(&samples);
                    }

                    // Push to pre-buffer (already converted to f32)
                    producer_u16.push(samples);
                },
                |err| log::error!("Stream error: {}", err),
                None,
            )?
        },
        _ => {
            return Err(anyhow::anyhow!("Unsupported sample format"));
        }
    };

    // Start audio capture NOW — before WebSocket connection
    stream.play().context("Failed to start audio stream")?;
    log::info!("Audio capture started, samples are being pre-buffered");

    // THEN spawn WebSocket streaming thread (audio is already being captured)
    let (server_url, server_api_key) = server_config;
    let session_id = session.id.to_string();
    let device_name = session.device_name.clone();
    let app_name = Some(session.app_name.clone());
    let sample_rate = session.sample_rate;
    let channels = session.channels;

    spawn_streaming_thread(consumer, pre_buffer, server_url, server_api_key, session_id, device_name, app_name, sample_rate, channels);

    Ok(stream)
}

// Windows Process Loopback module for per-application audio capture
#[cfg(windows)]
mod process_loopback {
    use anyhow::{Result, Context};
    use std::sync::{Arc, Mutex, mpsc};
    use windows::{
        Win32::Media::Audio::*,
        Win32::Foundation::*,
        core::*,
    };

    // Virtual device ID for process loopback (Windows 10 Build 20348+)
    const VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK: &str = "VAD\\Process_Loopback";

    // PROPVARIANT VT_BLOB type constant
    const VT_BLOB: u16 = 65;

    /// PROPVARIANT structure for passing activation params
    /// We define this manually because windows-core's PROPVARIANT is opaque
    #[repr(C)]
    struct PropVariantBlob {
        vt: u16,
        reserved1: u16,
        reserved2: u16,
        reserved3: u16,
        cb_size: u32,
        p_blob_data: *const u8,
    }

    /// Completion handler for async audio interface activation
    #[windows::core::implement(IActivateAudioInterfaceCompletionHandler)]
    pub struct ActivationHandler {
        result_tx: Arc<Mutex<Option<mpsc::Sender<Result<IAudioClient>>>>>,
    }

    impl ActivationHandler {
        pub fn new(tx: mpsc::Sender<Result<IAudioClient>>) -> Self {
            Self {
                result_tx: Arc::new(Mutex::new(Some(tx))),
            }
        }
    }

    impl IActivateAudioInterfaceCompletionHandler_Impl for ActivationHandler_Impl {
        fn ActivateCompleted(
            &self,
            operation: Option<&IActivateAudioInterfaceAsyncOperation>,
        ) -> windows::core::Result<()> {
            let result = (|| -> Result<IAudioClient> {
                let operation = operation.ok_or_else(|| anyhow::anyhow!("No operation"))?;

                unsafe {
                    let mut hr_activate: HRESULT = HRESULT(0);
                    let mut activated_interface: Option<IUnknown> = None;

                    operation.GetActivateResult(
                        &mut hr_activate,
                        &mut activated_interface,
                    ).ok().context("Failed to get activation result")?;

                    hr_activate.ok().context("Activation failed")?;

                    let interface = activated_interface
                        .ok_or_else(|| anyhow::anyhow!("No interface returned"))?;

                    interface.cast::<IAudioClient>()
                        .context("Failed to cast to IAudioClient")
                }
            })();

            // Send result through channel
            if let Ok(mut guard) = self.result_tx.lock() {
                if let Some(tx) = guard.take() {
                    let _ = tx.send(result);
                }
            }

            Ok(())
        }
    }

    /// Activate audio client for process-specific loopback capture
    ///
    /// # Arguments
    /// * `process_id` - Target process ID to capture audio from
    ///
    /// # Returns
    /// * `IAudioClient` configured for the target process
    pub unsafe fn activate_process_loopback(process_id: u32) -> Result<IAudioClient> {
        log::info!("=== Process Loopback Activation ===");
        log::info!("  Target PID: {}", process_id);
        log::info!("  Activation type: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK");
        log::info!("  Mode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE");

        // Create activation parameters for process loopback
        let process_params = AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
            TargetProcessId: process_id,
            ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
        };

        let activation_params = AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: process_params,
            },
        };

        // Create PROPVARIANT as a blob manually
        let params_size = std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>();
        let params_ptr = &activation_params as *const _ as *const u8;

        let prop_variant = PropVariantBlob {
            vt: VT_BLOB,
            reserved1: 0,
            reserved2: 0,
            reserved3: 0,
            cb_size: params_size as u32,
            p_blob_data: params_ptr,
        };

        // Create channel for receiving async result
        let (tx, rx) = mpsc::channel::<Result<IAudioClient>>();
        let handler: IActivateAudioInterfaceCompletionHandler =
            ActivationHandler::new(tx).into();

        // Activate audio interface asynchronously
        let device_id = HSTRING::from(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK);

        // Cast our custom PropVariantBlob to PROPVARIANT pointer for the API call
        let prop_variant_ptr = &prop_variant as *const PropVariantBlob as *const PROPVARIANT;

        log::info!("  Calling ActivateAudioInterfaceAsync...");

        ActivateAudioInterfaceAsync(
            &device_id,
            &IAudioClient::IID,
            Some(&*prop_variant_ptr),
            &handler,
        ).ok().context("Failed to start async activation")?;

        log::info!("  Waiting for async activation to complete (10s timeout)...");

        // Wait for completion (with timeout)
        let result = rx.recv_timeout(std::time::Duration::from_secs(10))
            .context("Activation timeout")?;

        match &result {
            Ok(_) => log::info!("  Process loopback SUCCESSFULLY activated for PID: {}", process_id),
            Err(e) => log::error!("  Process loopback activation FAILED for PID {}: {}", process_id, e),
        }

        result
    }

    /// Check if Windows version supports process loopback
    /// Requires Windows 10 Build 20348 or later
    pub fn supports_process_loopback() -> bool {
        use windows::Win32::System::SystemInformation::*;

        unsafe {
            let mut version_info: OSVERSIONINFOW = std::mem::zeroed();
            version_info.dwOSVersionInfoSize = std::mem::size_of::<OSVERSIONINFOW>() as u32;

            // GetVersionExW is deprecated but still works for build number check
            if GetVersionExW(&mut version_info).is_ok() {
                // Windows 10 Build 20348+ supports process loopback
                version_info.dwBuildNumber >= 20348
            } else {
                // Assume modern Windows if we can't check
                true
            }
        }
    }
}

// Start recording audio from output device via Windows Process Loopback
// This captures audio from a specific process, not the entire device
#[cfg(windows)]
pub fn start_output_recording(
    device_name: &str,
    process_id: u32,
    session: &mut RecordingSession,
    _api_key: Option<String>,
    server_config: Option<(String, String)>,
) -> Result<Box<dyn FnOnce() + Send>> {
    use windows::{
        Win32::Media::Audio::*,
        Win32::System::Com::*,
    };

    log::info!(
        "Starting process loopback recording for '{}' (PID: {})",
        device_name, process_id
    );

    // Try process loopback - available on Windows 10 Build 20348+ (including Windows 11)
    // We attempt process loopback directly and fall back to device loopback if it fails.
    log::info!("=== Starting Process Loopback Capture ===");
    log::info!("  Target PID: {}", process_id);

    // Default audio format for process loopback
    // Process loopback provides 32-bit float stereo at system sample rate
    let sample_rate: u32 = 48000;
    let channels: u16 = 2;

    session.sample_rate = sample_rate;
    session.channels = channels;

    let audio_level = session.audio_level.clone();
    let audio_buffer = session.audio_buffer.clone();
    let stop_signal = session.stop_signal.clone();

    // Setup pre-buffer only when streaming is configured (avoids silent memory accumulation)
    let (producer, streaming_ctx) = if let Some((server_url, server_api_key)) = server_config {
        let pre_buffer = AudioPreBuffer::new();
        let producer = pre_buffer.producer();
        let consumer = pre_buffer.consumer();
        (Some(producer), Some((consumer, pre_buffer, server_url, server_api_key)))
    } else {
        (None, None)
    };

    // Spawn thread to process audio packets
    // All COM operations must happen within this thread
    // Audio capture starts HERE, before WebSocket connection
    std::thread::spawn(move || {
        unsafe {
            // Initialize COM for this thread
            if CoInitializeEx(None, COINIT_MULTITHREADED).is_err() {
                log::error!("Failed to initialize COM in audio thread");
                return;
            }

            let result = (|| -> Result<()> {
                // Activate process-specific audio client
                let audio_client = process_loopback::activate_process_loopback(process_id)?;

                log::info!("Process loopback activated for PID: {}", process_id);

                // For process loopback, use a fixed format: 48kHz stereo 32-bit float
                // This is the standard format for Windows process loopback capture
                let sample_rate_hz: u32 = 48000;
                let num_channels: u16 = 2;
                let bits_per_sample: u16 = 32;

                // Create WAVEFORMATEX for IEEE float format
                let format = WAVEFORMATEX {
                    wFormatTag: 3, // WAVE_FORMAT_IEEE_FLOAT
                    nChannels: num_channels,
                    nSamplesPerSec: sample_rate_hz,
                    nAvgBytesPerSec: sample_rate_hz * num_channels as u32 * (bits_per_sample as u32 / 8),
                    nBlockAlign: num_channels * (bits_per_sample / 8),
                    wBitsPerSample: bits_per_sample,
                    cbSize: 0,
                };

                log::info!("Using fixed format for process loopback:");
                log::info!("  Sample Rate: {} Hz", sample_rate_hz);
                log::info!("  Channels: {}", num_channels);
                log::info!("  Bits per sample: {}", bits_per_sample);
                log::info!("  Format: IEEE Float");

                // Initialize audio client for loopback capture
                audio_client.Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_LOOPBACK,
                    10000000, // 1 second buffer
                    0,
                    &format,
                    None,
                ).ok().context("Failed to initialize audio client")?;

                log::info!("Audio client initialized successfully");

                // Get capture client interface
                let capture_client: IAudioCaptureClient = audio_client.GetService()
                    .ok().context("Failed to get capture client")?;

                // Start capturing FIRST — before WebSocket connection
                audio_client.Start()
                    .ok().context("Failed to start audio stream")?;

                log::info!("=== Audio Capture Started ===");
                log::info!("  PID: {}", process_id);

                let actual_channels = num_channels;
                let mut packet_count: u64 = 0;

                // Audio capture loop
                loop {
                    if stop_signal.load(std::sync::atomic::Ordering::Acquire) {
                        break;
                    }

                    // 10ms sleep balances latency vs CPU usage with 1-second buffer
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
                            let sample_count = (num_frames_available * actual_channels as u32) as usize;
                            let float_buffer = std::slice::from_raw_parts(
                                buffer_ptr as *const f32,
                                sample_count
                            );

                            let rms = calculate_rms(float_buffer);
                            audio_level.store(rms.to_bits(), std::sync::atomic::Ordering::Relaxed);

                            if let Ok(mut buffer) = audio_buffer.lock() {
                                buffer.extend_from_slice(float_buffer);
                            }

                            // Push to pre-buffer if streaming is configured
                            if let Some(ref producer) = producer {
                                producer.push(float_buffer.to_vec());
                            }

                            packet_count += 1;
                            // Log every 100 packets (roughly every 1 second at 10ms intervals)
                            if packet_count % 100 == 0 {
                                log::debug!("[PID {}] Captured {} packets, RMS: {:.4}", process_id, packet_count, rms);
                            }

                            capture_client.ReleaseBuffer(num_frames_available).ok();
                        }
                    }
                }

                // Cleanup
                audio_client.Stop().ok();

                log::info!("Audio capture stopped for PID: {}", process_id);

                Ok(())
            })();

            if let Err(e) = result {
                log::error!("Process loopback error: {}", e);
            }

            // Uninitialize COM for this thread
            CoUninitialize();
        }
    });

    // Spawn WebSocket streaming thread AFTER audio capture has started
    if let Some((consumer, pre_buffer, server_url, server_api_key)) = streaming_ctx {
        let session_id = session.id.to_string();
        let device_name_clone = session.device_name.clone();
        let app_name = Some(session.app_name.clone());

        spawn_streaming_thread(consumer, pre_buffer, server_url, server_api_key, session_id, device_name_clone, app_name, sample_rate, channels);
    }

    Ok(Box::new(move || {}))
}

/// Fallback: Device loopback for older Windows versions
/// Captures all audio from the default output device (not per-application)
#[cfg(windows)]
fn start_output_recording_device_loopback(
    session: &mut RecordingSession,
    server_config: Option<(String, String)>,
) -> Result<Box<dyn FnOnce() + Send>> {
    use windows::{
        Win32::Media::Audio::*,
        Win32::System::Com::*,
    };

    log::warn!("!!! DEVICE LOOPBACK MODE !!!");
    log::warn!("  Capturing ALL audio from default output device");
    log::warn!("  All sessions using device loopback will receive IDENTICAL audio");
    log::warn!("  This is a fallback mode - process loopback is not available");

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

    // Setup pre-buffer only when streaming is configured (avoids silent memory accumulation)
    let (producer, streaming_ctx) = if let Some((server_url, server_api_key)) = server_config {
        let pre_buffer = AudioPreBuffer::new();
        let producer = pre_buffer.producer();
        let consumer = pre_buffer.consumer();
        (Some(producer), Some((consumer, pre_buffer, server_url, server_api_key)))
    } else {
        (None, None)
    };

    // Spawn thread to process audio packets — capture starts HERE
    std::thread::spawn(move || {
        unsafe {
            if CoInitializeEx(None, COINIT_MULTITHREADED).is_err() {
                log::error!("Failed to initialize COM in audio thread");
                return;
            }

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

                // Start capturing FIRST — before WebSocket connection
                audio_client.Start()
                    .ok().context("Failed to start audio stream")?;

                loop {
                    if stop_signal.load(std::sync::atomic::Ordering::Acquire) {
                        break;
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
                            audio_level.store(rms.to_bits(), std::sync::atomic::Ordering::Relaxed);

                            if let Ok(mut buffer) = audio_buffer.lock() {
                                buffer.extend_from_slice(float_buffer);
                            }

                            // Push to pre-buffer if streaming is configured
                            if let Some(ref producer) = producer {
                                producer.push(float_buffer.to_vec());
                            }

                            capture_client.ReleaseBuffer(num_frames_available).ok();
                        }
                    }
                }

                audio_client.Stop().ok();
                CoTaskMemFree(Some(format_ptr as *const _ as *const _));

                Ok(())
            })();

            if let Err(e) = result {
                log::error!("Audio recording error: {}", e);
            }

            CoUninitialize();
        }
    });

    // Spawn WebSocket streaming thread AFTER audio capture has started
    if let Some((consumer, pre_buffer, server_url, server_api_key)) = streaming_ctx {
        let session_id = session.id.to_string();
        let device_name = session.device_name.clone();
        let app_name = Some(session.app_name.clone());

        spawn_streaming_thread(consumer, pre_buffer, server_url, server_api_key, session_id, device_name, app_name, sample_rate, channels);
    }

    Ok(Box::new(move || {}))
}

// macOS implementation using ScreenCaptureKit
// Note: process_id is used on macOS via ScreenCaptureKit's application filtering
#[cfg(target_os = "macos")]
pub fn start_output_recording(
    device_name: &str,
    _process_id: u32,  // Used by ScreenCaptureKit's SCContentFilter::Application
    session: &mut RecordingSession,
    _api_key: Option<String>,
    server_config: Option<(String, String)>,
) -> Result<Box<dyn FnOnce() + Send>> {
    use screencapturekit::shareable_content::SCShareableContent;
    use screencapturekit::stream::configuration::SCStreamConfiguration;
    use screencapturekit::stream::content_filter::SCContentFilter;
    use screencapturekit::stream::SCStream;
    use screencapturekit::stream::output_type::SCStreamOutputType;
    use screencapturekit::stream::output_trait::SCStreamOutputTrait;
    use screencapturekit::output::CMSampleBuffer;
    use crate::audio_buffer::AudioProducer;

    const DEFAULT_SAMPLE_RATE: u32 = 48000;
    const DEFAULT_CHANNELS: u16 = 2;

    // Check screen recording permission by attempting to get shareable content
    let content = SCShareableContent::get()
        .map_err(|e| anyhow::anyhow!(
            "Screen Recording permission required.\n\
            Please grant permission in:\n\
            System Preferences > Privacy & Security > Screen Recording\n\
            Then restart the application.\n\
            Error: {:?}", e
        ))?;

    // Set session parameters
    session.sample_rate = DEFAULT_SAMPLE_RATE;
    session.channels = DEFAULT_CHANNELS;

    let audio_level = session.audio_level.clone();
    let audio_buffer = session.audio_buffer.clone();
    let stop_signal = session.stop_signal.clone();

    // Setup pre-buffer only when streaming is configured (avoids silent memory accumulation)
    let (producer, streaming_ctx) = if let Some((ref server_url, ref server_api_key)) = server_config {
        let pre_buffer = AudioPreBuffer::new();
        let producer = pre_buffer.producer();
        let consumer = pre_buffer.consumer();
        (Some(producer), Some((consumer, pre_buffer, server_url.clone(), server_api_key.clone())))
    } else {
        (None, None)
    };

    // Create content filter based on device_name
    // NOTE: We use application-based filters instead of window-based filters because
    // `with_display_excluding_windows(&display, &[])` causes kCGErrorInvalidContext (1003)
    // on certain macOS versions when the empty windows array produces an invalid graphics context.
    // See: https://federicoterzi.com/blog/screencapturekit-failing-to-capture-the-entire-display/
    let display = content.displays()
        .first()
        .ok_or_else(|| anyhow::anyhow!("No displays found"))?
        .clone();

    let applications = content.applications();

    let filter = if device_name.contains("All Applications") || device_name.contains("System Audio") {
        // Capture all system audio: include all applications on the display
        let app_refs: Vec<&_> = applications.iter().collect();
        SCContentFilter::new()
            .with_display_including_application_excepting_windows(&display, &app_refs, &[])
    } else {
        // Capture specific application audio: exclude all OTHER apps from the display
        let target_app = applications
            .iter()
            .find(|app| {
                let name = app.application_name();
                !name.is_empty() && device_name.contains(&name)
            })
            .ok_or_else(|| anyhow::anyhow!("Application '{}' not found", device_name))?;

        log::info!("Found target application: {} (pid: {})", target_app.application_name(), target_app.process_id());

        // Build exclusion list: all apps except the target
        let excluded_apps: Vec<&_> = applications
            .iter()
            .filter(|app| app.process_id() != target_app.process_id())
            .collect();

        SCContentFilter::new()
            .with_display_excluding_applications_excepting_windows(&display, &excluded_apps, &[])
    };

    // Configure stream for audio-only capture (builder pattern)
    let config = SCStreamConfiguration::new()
        .set_captures_audio(true)
        .map_err(|e| anyhow::anyhow!("Failed to set captures_audio: {:?}", e))?
        .set_sample_rate(DEFAULT_SAMPLE_RATE)
        .map_err(|e| anyhow::anyhow!("Failed to set sample_rate: {:?}", e))?
        .set_channel_count(DEFAULT_CHANNELS as u8)
        .map_err(|e| anyhow::anyhow!("Failed to set channel_count: {:?}", e))?
        .set_excludes_current_process_audio(true)
        .map_err(|e| anyhow::anyhow!("Failed to set excludes_current_process_audio: {:?}", e))?
        .set_width(2)
        .map_err(|e| anyhow::anyhow!("Failed to set width: {:?}", e))?
        .set_height(2)
        .map_err(|e| anyhow::anyhow!("Failed to set height: {:?}", e))?;

    // Create output handler for audio samples — writes to AudioProducer if streaming
    struct AudioOutputHandler {
        audio_level: std::sync::Arc<std::sync::atomic::AtomicU32>,
        audio_buffer: std::sync::Arc<std::sync::Mutex<Vec<f32>>>,
        stop_signal: std::sync::Arc<std::sync::atomic::AtomicBool>,
        producer: Option<AudioProducer>,
    }

    impl SCStreamOutputTrait for AudioOutputHandler {
        fn did_output_sample_buffer(&self, sample: CMSampleBuffer, of_type: SCStreamOutputType) {
            // Only process audio samples
            if of_type != SCStreamOutputType::Audio {
                return;
            }

            // Check stop signal
            if self.stop_signal.load(std::sync::atomic::Ordering::Acquire) {
                return;
            }

            // Extract audio samples from CMSampleBuffer
            // The buffer contains interleaved f32 samples at the configured sample rate
            if let Ok(audio_data) = sample.get_audio_buffer_list() {
                // Extract f32 samples from all audio buffers
                let mut samples = Vec::new();
                for buf in audio_data.buffers() {
                    let byte_data = buf.data();
                    // Safety: ScreenCaptureKit delivers f32 PCM; verify alignment as a safety net
                    debug_assert!(byte_data.as_ptr() as usize % std::mem::align_of::<f32>() == 0,
                        "SCK audio buffer not aligned to f32 boundary");
                    let float_slice = unsafe {
                        std::slice::from_raw_parts(
                            byte_data.as_ptr() as *const f32,
                            byte_data.len() / std::mem::size_of::<f32>(),
                        )
                    };
                    samples.extend_from_slice(float_slice);
                }

                if samples.is_empty() {
                    return;
                }

                // Calculate RMS and update audio level
                let rms = crate::audio::calculate_rms(&samples);
                self.audio_level.store(rms.to_bits(), std::sync::atomic::Ordering::Relaxed);

                // Store in local buffer (for potential WAV export)
                if let Ok(mut buffer) = self.audio_buffer.lock() {
                    buffer.extend_from_slice(&samples);
                }

                // Push to pre-buffer if streaming is configured
                if let Some(ref producer) = self.producer {
                    producer.push(samples);
                }
            }
        }
    }

    let handler = AudioOutputHandler {
        audio_level,
        audio_buffer,
        stop_signal: stop_signal.clone(),
        producer,
    };

    // Create and start the stream — audio capture begins NOW, before WebSocket
    let mut stream = SCStream::new(&filter, &config);
    stream.add_output_handler(handler, SCStreamOutputType::Audio);
    stream.start_capture()
        .map_err(|e| anyhow::anyhow!("Failed to start screen capture: {:?}", e))?;

    log::info!("ScreenCaptureKit audio capture started, samples are being pre-buffered");

    // THEN spawn WebSocket streaming thread (audio is already being captured)
    if let Some((consumer, pre_buffer, server_url, server_api_key)) = streaming_ctx {
        let session_id = session.id.to_string();
        let device_name_clone = session.device_name.clone();
        let app_name = Some(session.app_name.clone());

        spawn_streaming_thread(consumer, pre_buffer, server_url, server_api_key, session_id, device_name_clone, app_name, DEFAULT_SAMPLE_RATE, DEFAULT_CHANNELS);
    }

    // Return cleanup closure that stops the stream
    let stop_signal_cleanup = stop_signal.clone();
    Ok(Box::new(move || {
        stop_signal_cleanup.store(true, Ordering::Release);
        // Stream will be dropped here, stopping capture
        drop(stream);
    }))
}

// Fallback for unsupported platforms
#[cfg(not(any(windows, target_os = "macos")))]
pub fn start_output_recording(
    _device_name: &str,
    _process_id: u32,
    _session: &mut RecordingSession,
    _api_key: Option<String>,
    _server_config: Option<(String, String)>,
) -> Result<Box<dyn FnOnce() + Send>> {
    Err(anyhow::anyhow!("Output recording is only supported on Windows and macOS"))
}
