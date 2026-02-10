use anyhow::Result;
use std::sync::{Arc, Mutex, mpsc};

use super::AudioSession;
use crate::audio::calculate_rms;
use crate::streaming;

use screencapturekit::sc_shareable_content::SCShareableContent;
use screencapturekit::sc_stream_configuration::SCStreamConfiguration;
use screencapturekit::sc_content_filter::{SCContentFilter, InitParams};
use screencapturekit::sc_stream::SCStream;
use screencapturekit::sc_output_handler::{SCStreamOutputType, StreamOutput};
use screencapturekit::cm_sample_buffer::CMSampleBuffer;

const DEFAULT_SAMPLE_RATE: u32 = 48000;
const DEFAULT_CHANNELS: u16 = 2;

/// Enumerate audio sessions on macOS using ScreenCaptureKit
pub fn enumerate_audio_sessions() -> Result<Vec<AudioSession>> {
    let content = SCShareableContent::current()
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
        let app_name = match app.application_name() {
            Some(name) => name,
            None => continue,
        };

        let bundle_id = app.bundle_identifier().unwrap_or_default();
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
    let content = SCShareableContent::current()
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

    // Create content filter
    let filter = if session.app_name.contains("All Applications") {
        let display = content.displays()
            .first()
            .ok_or_else(|| anyhow::anyhow!("No displays found"))?
            .clone();
        SCContentFilter::new(InitParams::Display(display))
    } else {
        let app = content.applications()
            .into_iter()
            .find(|app| {
                app.application_name()
                    .map(|name| session.app_name.contains(&name))
                    .unwrap_or(false)
            })
            .ok_or_else(|| anyhow::anyhow!("Application not found"))?;
        SCContentFilter::new(InitParams::Application(app))
    };

    // Configure stream
    let config = SCStreamConfiguration {
        captures_audio: true,
        sample_rate: DEFAULT_SAMPLE_RATE,
        channel_count: DEFAULT_CHANNELS as u32,
        excludes_current_process_audio: true,
        width: 1,
        height: 1,
        ..Default::default()
    };

    // Output handler
    struct AudioHandler {
        audio_level: Arc<Mutex<f32>>,
        stop_signal: Arc<Mutex<bool>>,
        tx: mpsc::Sender<Vec<f32>>,
    }

    impl StreamOutput for AudioHandler {
        fn did_output_sample_buffer(&self, sample: CMSampleBuffer, of_type: SCStreamOutputType) {
            if of_type != SCStreamOutputType::Audio {
                return;
            }

            if let Ok(stop) = self.stop_signal.lock() {
                if *stop {
                    return;
                }
            }

            if let Some(audio_data) = sample.get_audio_buffer_list() {
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

    let mut stream = SCStream::new(filter, config, handler);
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
