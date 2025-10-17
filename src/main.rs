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

// Save audio buffer to WAV file
fn save_audio_file(audio_buffer: &[f32], sample_rate: u32, channels: u16, wall_clock_duration: f32) -> Result<String> {
    // Generate timestamped filename
    let timestamp = Local::now().format("%Y-%m-%d_%H-%M-%S");
    let filename = format!("recording_{}.wav", timestamp);

    // Debug: Print WAV file parameters
    println!();
    println!("{}", style("=== WAV File Debug Info ===").bold().yellow());
    println!("Sample rate: {} Hz", sample_rate);
    println!("Channels: {}", channels);
    println!("Total samples in buffer: {}", audio_buffer.len());
    println!("Bits per sample: 32 (Float)");

    // Calculate expected values
    let total_frames = audio_buffer.len() / channels as usize;
    let audio_duration = total_frames as f32 / sample_rate as f32;
    println!("Total frames: {}", total_frames);
    println!("Audio duration: {:.3} seconds", audio_duration);
    println!("Wall-clock duration: {:.3} seconds", wall_clock_duration);

    // Calculate capture rate
    let capture_rate = if wall_clock_duration > 0.0 {
        (audio_duration / wall_clock_duration) * 100.0
    } else {
        0.0
    };

    if capture_rate < 95.0 {
        println!("{}", style(format!("⚠ WARNING: Only {:.1}% of audio captured! Possible sample dropping.", capture_rate)).red().bold());
    } else {
        println!("{}", style(format!("Capture rate: {:.1}%", capture_rate)).green());
    }

    println!("{}", style("========================").bold().yellow());
    println!();

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
    println!();

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

        // Get current audio level
        let level = if let Ok(l) = audio_level.lock() {
            *l
        } else {
            0.0
        };

        // Display audio level visualization
        execute!(
            stdout,
            cursor::MoveTo(0, 8),
            terminal::Clear(ClearType::CurrentLine)
        )?;

        print!("Level: {}", create_audio_bar(level, 50));
        print!("  {:.2}%", level * 100.0);
        stdout.flush()?;
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

    // Save audio file
    if let Ok(buffer) = audio_buffer.lock() {
        if buffer.is_empty() {
            println!("{}", style("No audio data recorded.").yellow());
        } else {
            println!("{}", style("Saving audio file...").cyan());
            match save_audio_file(&buffer, sample_rate, channels, elapsed.as_secs_f32()) {
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

    result
}

// Stream audio from output device using Windows WASAPI loopback
#[cfg(windows)]
fn stream_output_audio(_device: &Device, device_name: &str) -> Result<()> {
    println!();
    println!("{}", style("=== AUDIO STREAMING (Loopback) ===").bold().green());
    println!("Device: {}", style(device_name).cyan().bold());
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
        println!();

        // Shared audio level state
        let audio_level = Arc::new(Mutex::new(0.0f32));
        let audio_buffer = Arc::new(Mutex::new(Vec::<f32>::new()));

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

            // Get current audio level for visualization
            let level = if let Ok(l) = audio_level.lock() {
                *l
            } else {
                0.0
            };

            // Display audio level visualization
            execute!(
                stdout,
                cursor::MoveTo(0, 8),
                terminal::Clear(ClearType::CurrentLine)
            )?;

            print!("Level: {}", create_audio_bar(level, 50));
            print!("  {:.2}%", level * 100.0);
            stdout.flush()?;
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

        // Save audio file
        if let Ok(buffer) = audio_buffer.lock() {
            if buffer.is_empty() {
                println!("{}", style("No audio data recorded.").yellow());
            } else {
                println!("{}", style("Saving audio file...").cyan());
                match save_audio_file(&buffer, sample_rate, channels, elapsed.as_secs_f32()) {
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
