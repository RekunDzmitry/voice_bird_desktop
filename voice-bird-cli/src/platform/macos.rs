use anyhow::Result;
use std::sync::{Arc, Mutex, mpsc};

use super::AudioSession;
use crate::audio::calculate_rms;
use crate::streaming;

use screencapturekit::shareable_content::SCShareableContent;
use screencapturekit::stream::configuration::SCStreamConfiguration;
use screencapturekit::stream::content_filter::SCContentFilter;
use screencapturekit::stream::SCStream;
use screencapturekit::stream::output_trait::SCStreamOutputTrait;
use screencapturekit::stream::output_type::SCStreamOutputType;
use screencapturekit::output::CMSampleBuffer;

const DEFAULT_SAMPLE_RATE: u32 = 48000;
const DEFAULT_CHANNELS: u16 = 2;

/// Enumerate audio sessions on macOS using ScreenCaptureKit
pub fn enumerate_audio_sessions() -> Result<Vec<AudioSession>> {
    let content = SCShareableContent::get()
        .map_err(|e| anyhow::anyhow!(
            "Screen Recording permission required.\n\
            Please grant permission in:\n\
            System Preferences > Privacy & Security > Screen Recording\n\
            Error: {:?}", e
        ))?;

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
    let content = SCShareableContent::get()
        .map_err(|e| anyhow::anyhow!("Screen Recording permission required: {:?}", e))?;

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
        .first()
        .ok_or_else(|| anyhow::anyhow!("No displays found"))?
        .clone();

    // Create content filter
    let filter = if session.app_name.contains("All Applications") {
        SCContentFilter::new()
            .with_display_excluding_windows(&display, &[])
    } else {
        let app = content.applications()
            .into_iter()
            .find(|app| {
                let name = app.application_name();
                !name.is_empty() && session.app_name.contains(&name)
            })
            .ok_or_else(|| anyhow::anyhow!("Application not found"))?;
        SCContentFilter::new()
            .with_display_including_application_excepting_windows(&display, &[&app], &[])
    };

    // Configure stream
    let config = SCStreamConfiguration::new()
        .set_captures_audio(true)
        .map_err(|e| anyhow::anyhow!("Failed to set captures_audio: {:?}", e))?
        .set_sample_rate(DEFAULT_SAMPLE_RATE)
        .map_err(|e| anyhow::anyhow!("Failed to set sample_rate: {:?}", e))?
        .set_channel_count(DEFAULT_CHANNELS as u8)
        .map_err(|e| anyhow::anyhow!("Failed to set channel_count: {:?}", e))?
        .set_excludes_current_process_audio(true)
        .map_err(|e| anyhow::anyhow!("Failed to set excludes_current_process_audio: {:?}", e))?
        .set_width(1)
        .map_err(|e| anyhow::anyhow!("Failed to set width: {:?}", e))?
        .set_height(1)
        .map_err(|e| anyhow::anyhow!("Failed to set height: {:?}", e))?;

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

            if let Ok(audio_data) = sample.get_audio_buffer_list() {
                let samples: Vec<f32> = audio_data.into_iter().collect();
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

    let mut stream = SCStream::new(filter, &config);
    stream.add_output_handler(handler, SCStreamOutputType::Audio);
    stream.start_capture()
        .map_err(|e| anyhow::anyhow!("Failed to start capture: {:?}", e))?;

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
