use anyhow::Result;
use std::sync::{Arc, Mutex, mpsc};

use super::AudioSession;
use crate::audio::calculate_rms;
use crate::streaming;

use screencapturekit::prelude::*;

const DEFAULT_SAMPLE_RATE: u32 = 48000;
const DEFAULT_CHANNELS: u16 = 2;
const MIN_MACOS_MAJOR_VERSION: u32 = 13;

/// Check that macOS version supports SCStream audio capture (requires 13.0+)
fn check_macos_version() -> Result<String> {
    let output = std::process::Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .map_err(|e| anyhow::anyhow!("Failed to check macOS version: {}", e))?;

    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let major: u32 = version
        .split('.')
        .next()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    if major < MIN_MACOS_MAJOR_VERSION {
        return Err(anyhow::anyhow!(
            "macOS {}.0 (Ventura) or later is required for audio capture.\n\
             Current version: {}\n\
             ScreenCaptureKit audio capture was introduced in macOS 13.0.",
            MIN_MACOS_MAJOR_VERSION,
            version
        ));
    }

    Ok(version)
}

/// Enumerate audio sessions on macOS using ScreenCaptureKit
pub fn enumerate_audio_sessions() -> Result<Vec<AudioSession>> {
    let version = check_macos_version()?;

    log::info!("Enumerating macOS audio sessions (macOS {})", version);

    let content = SCShareableContent::get()
        .map_err(|e| {
            log::error!("SCShareableContent::get() failed: {:?}", e);
            anyhow::anyhow!(
                "Screen Recording permission is required for audio capture.\n\n\
                 Grant permission in:\n  \
                 System Settings > Privacy & Security > Screen Recording\n\n\
                 After granting permission, restart the application.\n\
                 Error: {:?}", e
            )
        })?;

    let mut sessions = Vec::new();

    // Add option to capture all system audio
    sessions.push(AudioSession {
        device_name: "System Audio - All Applications".to_string(),
        app_name: "All Applications".to_string(),
        process_id: 0,
        is_input: false,
    });

    // List running applications
    for app in content.applications() {
        let app_name = app.application_name();
        if app_name.is_empty() {
            continue;
        }

        let bundle_id = app.bundle_identifier();
        let process_id = app.process_id() as u32;

        // Skip system processes
        let skip_bundles = [
            "com.apple.finder",
            "com.apple.dock",
            "com.apple.controlcenter",
            "com.apple.notificationcenterui",
            "com.apple.loginwindow",
            "com.apple.WindowManager",
            "com.apple.SystemUIServer",
        ];

        if skip_bundles.iter().any(|b| bundle_id.contains(b)) {
            continue;
        }

        if bundle_id.contains("voicebird") || bundle_id.contains("voice_bird") {
            continue;
        }

        if process_id == 0 {
            continue;
        }

        sessions.push(AudioSession {
            device_name: format!("System Audio - {}", app_name),
            app_name,
            process_id,
            is_input: false,
        });
    }

    Ok(sessions)
}

/// Start output recording on macOS using ScreenCaptureKit
pub fn start_output_recording(
    session: &AudioSession,
    server_url: String,
    api_key: String,
    session_id: String,
    audio_level: Arc<Mutex<f32>>,
    stop_signal: Arc<Mutex<bool>>,
) -> Result<()> {
    let macos_version = check_macos_version()?;

    log::info!(
        "Starting macOS output recording: app={}, device={}, macOS {}",
        session.app_name, session.device_name, macos_version
    );

    let content = SCShareableContent::get()
        .map_err(|e| {
            log::error!("SCShareableContent::get() failed in start_output_recording: {:?}", e);
            anyhow::anyhow!(
                "Screen Recording permission is required for audio capture.\n\n\
                 Grant permission in:\n  \
                 System Settings > Privacy & Security > Screen Recording\n\n\
                 After granting permission, restart the application.\n\
                 Error: {:?}", e
            )
        })?;

    let (tx, rx) = mpsc::channel::<Vec<f32>>();

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
                false,
                rx,
                DEFAULT_SAMPLE_RATE,
                DEFAULT_CHANNELS,
            ).await;
        });
    });

    let display = content.displays()
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("No displays found"))?;

    // Create content filter
    // NOTE: We use application-based filters (exclusion strategy for specific apps) instead
    // of inclusion-based filters because including a single app can cause
    // "Start stream failed" on macOS when the app has no visible windows or the
    // graphics context is invalid. Excluding all OTHER apps is more reliable.
    let apps = content.applications();
    let filter = if session.app_name.contains("All Applications") {
        // Capture all system audio: include all applications on the display
        let all_apps: Vec<_> = apps.iter().collect();
        SCContentFilter::create()
            .with_display(&display)
            .with_including_applications(&all_apps, &[])
            .build()
    } else {
        // Capture specific application: exclude all OTHER apps from the display
        let target_app = apps
            .iter()
            .find(|app| {
                let name = app.application_name();
                !name.is_empty() && session.app_name.contains(&name)
            })
            .ok_or_else(|| anyhow::anyhow!("Application '{}' not found", session.app_name))?;

        log::info!("Found target application: {} (pid: {})", target_app.application_name(), target_app.process_id());

        let excluded_apps: Vec<_> = apps
            .iter()
            .filter(|app| app.process_id() != target_app.process_id())
            .collect();

        SCContentFilter::create()
            .with_display(&display)
            .with_excluding_applications(&excluded_apps, &[])
            .build()
    };

    // Configure stream — audio only, minimal video (2x2 to avoid kCGErrorInvalidContext)
    let config = SCStreamConfiguration::new()
        .with_width(2)
        .with_height(2)
        .with_captures_audio(true)
        .with_sample_rate(DEFAULT_SAMPLE_RATE as i32)
        .with_channel_count(DEFAULT_CHANNELS as i32)
        .with_excludes_current_process_audio(true);

    // Output handler
    struct AudioHandler {
        audio_level: Arc<Mutex<f32>>,
        stop_signal: Arc<Mutex<bool>>,
        tx: mpsc::Sender<Vec<f32>>,
    }

    impl SCStreamOutputTrait for AudioHandler {
        fn did_output_sample_buffer(&self, sample: CMSampleBuffer, of_type: SCStreamOutputType) {
            if of_type != SCStreamOutputType::Audio {
                return;
            }

            if let Ok(stop) = self.stop_signal.lock() {
                if *stop {
                    return;
                }
            }

            if let Some(audio_data) = sample.audio_buffer_list() {
                let mut samples = Vec::new();
                for buf in &audio_data {
                    let bytes = buf.data();
                    // Reinterpret bytes as f32 samples (CoreAudio uses 32-bit float)
                    let float_samples: &[f32] = unsafe {
                        std::slice::from_raw_parts(
                            bytes.as_ptr() as *const f32,
                            bytes.len() / std::mem::size_of::<f32>(),
                        )
                    };
                    samples.extend_from_slice(float_samples);
                }
                if samples.is_empty() {
                    return;
                }

                let rms = calculate_rms(&samples);
                if let Ok(mut level) = self.audio_level.lock() {
                    *level = rms;
                }

                let _ = self.tx.send(samples);
            }
        }
    }

    let handler = AudioHandler {
        audio_level,
        stop_signal: stop_signal.clone(),
        tx,
    };

    let mut stream = SCStream::new(&filter, &config);
    stream.add_output_handler(handler, SCStreamOutputType::Audio);
    stream.start_capture()
        .map_err(|e| {
            log::error!("SCStream::start_capture() failed: {:?}", e);
            let err_str = format!("{:?}", e);
            let hint = if err_str.contains("CoreGraphicsErrorDomain")
                || err_str.contains("1003")
                || err_str.contains("Start stream failed")
            {
                format!(
                    "Failed to start audio capture (macOS {}).\n\n\
                     This typically means Screen Recording permission was not fully granted.\n\n\
                     Troubleshooting steps:\n  \
                     1. Open System Settings > Privacy & Security > Screen Recording\n  \
                     2. Remove voice-bird-cli from the list if present, then re-add it\n  \
                     3. Ensure the toggle is ON\n  \
                     4. Completely quit and restart the application\n  \
                     5. If using a terminal (iTerm, Terminal.app), the TERMINAL itself\n     \
                        may need Screen Recording permission, not just voice-bird-cli\n\n\
                     On macOS 15+ (Sequoia/Tahoe): Plain CLI binaries may not receive\n  \
                     Screen Recording permissions correctly. Use the npm package\n  \
                     (npm i -g voice-bird-cli) which wraps the binary in a .app bundle\n  \
                     for proper TCC attribution, or set VOICE_BIRD_NO_OPEN=1 to bypass\n  \
                     the .app launcher and grant permission to your terminal instead.\n\n\
                     Raw error: {}",
                    macos_version, err_str
                )
            } else {
                format!("Failed to start capture (macOS {}): {}", macos_version, err_str)
            };
            anyhow::anyhow!(hint)
        })?;

    // Keep stream alive until stop signal
    loop {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if let Ok(stop) = stop_signal.lock() {
            if *stop {
                break;
            }
        }
    }

    Ok(())
}
