mod session;
mod wasapi_sessions;
mod ui;
mod audio;
mod grpc_streaming;
mod grpc_service;
mod opus_encoder;

use anyhow::{Result, Context};
use std::env;
use session::{RecordingSession, SessionManager};
use ui::{App, AppMode, RecordingInputAction};

fn main() -> Result<()> {
    // Initialize logger
    env_logger::Builder::from_default_env()
        .filter_level(log::LevelFilter::Info)
        .init();

    // Load .env file if present
    dotenvy::dotenv().ok();

    log::info!("Voice Bird Desktop starting...");
    log::info!("Initializing...");

    // Load Voice Bird server configuration (required)
    let server_config = match (
        env::var("VOICE_BIRD_SERVER_URL").ok(),
        env::var("VOICE_BIRD_API_KEY").ok(),
    ) {
        (Some(url), Some(key)) if !url.is_empty() && !key.is_empty() => {
            log::info!("Voice Bird server configuration loaded");
            (url, key)
        }
        _ => {
            log::error!("Voice Bird server not configured!");
            log::error!("Required environment variables:");
            log::error!("  - VOICE_BIRD_SERVER_URL");
            log::error!("  - VOICE_BIRD_API_KEY");
            log::error!("Create a .env file with these values.");
            return Err(anyhow::anyhow!("Voice Bird server configuration missing"));
        }
    };

    // Enumerate available audio sessions
    let available_sessions = wasapi_sessions::enumerate_audio_sessions()
        .context("Failed to enumerate audio sessions")?;

    if available_sessions.is_empty() {
        log::warn!("No active audio sessions found.");
        log::warn!("Make sure applications are playing/recording audio.");
        return Ok(());
    }

    log::info!("Found {} active audio session(s)", available_sessions.len());

    // Initialize terminal UI
    let mut terminal = ui::init_terminal()?;

    // Create app state
    let mut app = App::new(available_sessions);

    // Session browser loop
    loop {
        terminal.draw(|f| ui::render_session_browser(f, &mut app))?;

        let should_quit = ui::handle_session_browser_input(&mut app)?;

        if should_quit {
            ui::restore_terminal(terminal)?;
            log::info!("Application shutting down");
            return Ok(());
        }

        if app.mode == AppMode::Recording {
            break;
        }
    }

    // Get selected sessions
    let selected_session_infos = app.get_selected_sessions();

    if selected_session_infos.is_empty() {
        ui::restore_terminal(terminal)?;
        log::warn!("No sessions selected");
        return Ok(());
    }

    // Create recording sessions
    let mut session_manager = SessionManager::new();
    let host = cpal::default_host();

    for session_info in selected_session_infos {
        // Create recording session
        let mut recording_session = RecordingSession::new(
            session_info.clone(),
            48000, // Default sample rate, will be updated
            2,     // Default channels, will be updated
        );

        // Start recording based on device type
        let stream_result = if session_info.is_input {
            // Input device (microphone)
            match audio::get_input_device_by_name(&host, &session_info.device_name) {
                Ok(device) => {
                    audio::start_input_recording(&device, &mut recording_session, server_config.clone())
                        .map(|stream| (Some(stream), None))
                }
                Err(e) => {
                    log::error!("Failed to get input device: {}", e);
                    continue;
                }
            }
        } else {
            // Output device (loopback)
            #[cfg(windows)]
            {
                audio::start_output_recording(&session_info.device_name, &mut recording_session, None, Some(server_config.clone()))
                    .map(|cleanup| (None, Some(cleanup)))
            }
            #[cfg(not(windows))]
            {
                log::error!("Output recording not supported on this platform");
                continue;
            }
        };

        match stream_result {
            Ok((stream, cleanup)) => {
                recording_session.start_recording();
                session_manager.add_session(recording_session);

                // Keep stream alive (will be handled by session manager in real implementation)
                std::mem::forget(stream);
                std::mem::forget(cleanup);
            }
            Err(e) => {
                log::error!("Failed to start recording: {}", e);
            }
        }
    }

    if session_manager.active_sessions.is_empty() {
        ui::restore_terminal(terminal)?;
        log::warn!("No recording sessions started");
        return Ok(());
    }

    // Recording dashboard loop
    loop {
        let sessions: Vec<&RecordingSession> = session_manager.get_all_sessions();

        terminal.draw(|f| ui::render_recording_dashboard(f, &sessions))?;

        match ui::handle_recording_input()? {
            RecordingInputAction::StopAndSave => {
                // Stop all sessions
                session_manager.stop_all();

                // Wait a moment for audio buffers to finish
                std::thread::sleep(std::time::Duration::from_millis(500));

                ui::restore_terminal(terminal)?;

                // Save all recordings
                log::info!("Saving recordings...");

                for (_id, session) in &session_manager.active_sessions {
                    let prefix = session.get_filename_prefix();

                    // Save audio
                    if let Ok(buffer) = session.audio_buffer.lock() {
                        if !buffer.is_empty() {
                            let audio_filename = format!("{}.wav", prefix);
                            match audio::save_audio_file(&buffer, session.sample_rate, session.channels, &audio_filename) {
                                Ok(_) => {
                                    let duration = buffer.len() as f32 / (session.sample_rate * session.channels as u32) as f32;
                                    log::info!("Audio saved: {} (Duration: {:.2}s, Samples: {})",
                                        audio_filename, duration, buffer.len());
                                }
                                Err(e) => {
                                    log::error!("Failed to save audio: {}", e);
                                }
                            }
                        }
                    }

                    // Save transcript
                    if let Ok(segments) = session.transcript_buffer.lock() {
                        if !segments.is_empty() {
                            let transcript_filename = format!("{}.txt", prefix);
                            match audio::save_transcript_file(&segments, &transcript_filename) {
                                Ok(_) => {
                                    log::info!("Transcript saved: {} (Segments: {})",
                                        transcript_filename, segments.len());
                                }
                                Err(e) => {
                                    log::error!("Failed to save transcript: {}", e);
                                }
                            }
                        }
                    }
                }

                log::info!("All sessions saved successfully!");
                break;
            }
            RecordingInputAction::QuitWithoutSaving => {
                session_manager.stop_all();
                ui::restore_terminal(terminal)?;
                log::warn!("Exited without saving");
                break;
            }
            RecordingInputAction::Continue => {
                // Continue recording
            }
        }
    }

    Ok(())
}
