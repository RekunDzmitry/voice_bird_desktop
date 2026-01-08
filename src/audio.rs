use anyhow::{Result, Context};
use cpal::{Device, SampleFormat};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use hound::{WavWriter, WavSpec};
use crate::session::RecordingSession;
use crate::server_streaming::ServerStreamingService;
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

    // Spawn WebSocket streaming thread
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            if let Err(e) = ServerStreamingService::stream_to_server(
                server_url,
                server_api_key,
                session_id,
                device_name,
                rx,
                sample_rate,
                channels,
            ).await {
                log::error!("WebSocket streaming error: {}", e);
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

    // Setup server streaming if server config is provided
    let server_tx = if let Some((server_url, server_api_key)) = server_config {
        let (tx, rx) = mpsc::channel::<Vec<f32>>();
        let session_id = session.id.to_string();
        let device_name = session.device_name.clone();

        // Spawn WebSocket streaming thread
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                if let Err(e) = ServerStreamingService::stream_to_server(
                    server_url,
                    server_api_key,
                    session_id,
                    device_name,
                    rx,
                    sample_rate,
                    channels,
                ).await {
                    log::error!("WebSocket streaming error: {}", e);
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

                // Start capturing
                audio_client.Start()
                    .ok().context("Failed to start audio stream")?;

                log::info!("=== Audio Capture Started ===");
                log::info!("  PID: {}", process_id);

                let actual_channels = num_channels;
                let mut packet_count: u64 = 0;

                // Audio capture loop
                loop {
                    if let Ok(stop) = stop_signal.lock() {
                        if *stop {
                            break;
                        }
                    }

                    // 5ms sleep for low latency
                    std::thread::sleep(std::time::Duration::from_millis(5));

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

                            packet_count += 1;
                            // Log every 200 packets (roughly every 1 second at 5ms intervals)
                            if packet_count % 200 == 0 {
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

    // Setup server streaming if server config is provided
    let server_tx = if let Some((server_url, server_api_key)) = server_config {
        let (tx, rx) = mpsc::channel::<Vec<f32>>();
        let session_id = session.id.to_string();
        let device_name = session.device_name.clone();

        // Spawn WebSocket streaming thread
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                if let Err(e) = ServerStreamingService::stream_to_server(
                    server_url,
                    server_api_key,
                    session_id,
                    device_name,
                    rx,
                    sample_rate,
                    channels,
                ).await {
                    log::error!("WebSocket streaming error: {}", e);
                }
            });
        });

        Some(tx)
    } else {
        None
    };

    // Spawn thread to process audio packets
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

                audio_client.Start()
                    .ok().context("Failed to start audio stream")?;

                loop {
                    if let Ok(stop) = stop_signal.lock() {
                        if *stop {
                            break;
                        }
                    }

                    std::thread::sleep(std::time::Duration::from_millis(5));

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

                            if let Some(ref tx) = server_tx {
                                let _ = tx.send(float_buffer.to_vec());
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
    use screencapturekit::sc_shareable_content::SCShareableContent;
    use screencapturekit::sc_stream_configuration::SCStreamConfiguration;
    use screencapturekit::sc_content_filter::{SCContentFilter, InitParams};
    use screencapturekit::sc_stream::SCStream;
    use screencapturekit::sc_output_handler::{SCStreamOutputType, StreamOutput};
    use screencapturekit::cm_sample_buffer::CMSampleBuffer;
    use std::sync::mpsc;

    const DEFAULT_SAMPLE_RATE: u32 = 48000;
    const DEFAULT_CHANNELS: u16 = 2;

    // Check screen recording permission by attempting to get shareable content
    let content = SCShareableContent::current()
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

    // Setup server streaming if server config is provided
    let server_tx = if let Some((server_url, server_api_key)) = server_config {
        let (tx, rx) = mpsc::channel::<Vec<f32>>();
        let session_id = session.id.to_string();
        let device_name_clone = session.device_name.clone();

        // Spawn WebSocket streaming thread
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                if let Err(e) = ServerStreamingService::stream_to_server(
                    server_url,
                    server_api_key,
                    session_id,
                    device_name_clone,
                    rx,
                    DEFAULT_SAMPLE_RATE,
                    DEFAULT_CHANNELS,
                ).await {
                    log::error!("WebSocket streaming error: {}", e);
                }
            });
        });

        Some(tx)
    } else {
        None
    };

    // Create content filter based on device_name
    let filter = if device_name.contains("All Applications") || device_name.contains("System Audio") {
        // Capture all system audio via display
        let display = content.displays()
            .first()
            .ok_or_else(|| anyhow::anyhow!("No displays found"))?
            .clone();

        SCContentFilter::new(InitParams::Display(display))
    } else {
        // Try to find specific application by name
        let app = content.applications()
            .into_iter()
            .find(|app| {
                app.application_name()
                    .map(|name| device_name.contains(&name))
                    .unwrap_or(false)
            })
            .ok_or_else(|| anyhow::anyhow!("Application '{}' not found", device_name))?;

        SCContentFilter::new(InitParams::Application(app))
    };

    // Configure stream for audio-only capture
    let config = SCStreamConfiguration {
        captures_audio: true,
        sample_rate: DEFAULT_SAMPLE_RATE,
        channel_count: DEFAULT_CHANNELS as u32,
        excludes_current_process_audio: true,
        // Minimal video settings (required but we ignore video)
        width: 1,
        height: 1,
        ..Default::default()
    };

    // Create output handler for audio samples
    struct AudioOutputHandler {
        audio_level: std::sync::Arc<std::sync::Mutex<f32>>,
        audio_buffer: std::sync::Arc<std::sync::Mutex<Vec<f32>>>,
        stop_signal: std::sync::Arc<std::sync::Mutex<bool>>,
        server_tx: Option<mpsc::Sender<Vec<f32>>>,
    }

    impl StreamOutput for AudioOutputHandler {
        fn did_output_sample_buffer(&self, sample: CMSampleBuffer, of_type: SCStreamOutputType) {
            // Only process audio samples
            if of_type != SCStreamOutputType::Audio {
                return;
            }

            // Check stop signal
            if let Ok(stop) = self.stop_signal.lock() {
                if *stop {
                    return;
                }
            }

            // Extract audio samples from CMSampleBuffer
            // The buffer contains interleaved f32 samples at the configured sample rate
            if let Some(audio_data) = sample.get_audio_buffer_list() {
                let samples: Vec<f32> = audio_data.into_iter().collect();

                if samples.is_empty() {
                    return;
                }

                // Calculate RMS and update audio level
                let rms = crate::audio::calculate_rms(&samples);
                if let Ok(mut level) = self.audio_level.lock() {
                    *level = rms;
                }

                // Store in local buffer (for potential WAV export)
                if let Ok(mut buffer) = self.audio_buffer.lock() {
                    buffer.extend_from_slice(&samples);
                }

                // Send to server streaming service
                if let Some(ref tx) = self.server_tx {
                    let _ = tx.send(samples);
                }
            }
        }
    }

    let handler = AudioOutputHandler {
        audio_level,
        audio_buffer,
        stop_signal: stop_signal.clone(),
        server_tx,
    };

    // Create and start the stream
    let mut stream = SCStream::new(filter, config, handler);
    stream.start_capture()
        .map_err(|e| anyhow::anyhow!("Failed to start screen capture: {:?}", e))?;

    // Return cleanup closure that stops the stream
    let stop_signal_cleanup = stop_signal.clone();
    Ok(Box::new(move || {
        if let Ok(mut stop) = stop_signal_cleanup.lock() {
            *stop = true;
        }
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
