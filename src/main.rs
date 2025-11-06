mod session;
mod wasapi_sessions;
mod ui;
mod audio;
mod transcription;

use anyhow::{Result, Context};
use console::style;
use std::env;
use session::{RecordingSession, SessionManager};
use ui::{App, AppMode, RecordingInputAction};

fn main() -> Result<()> {
    // Load .env file if present
    dotenvy::dotenv().ok();

    println!("{}", style("=== Voice Bird Desktop ===").bold().cyan());
    println!("{}", style("Initializing...").yellow());
    println!();

    // Check for API key
    let api_key = env::var("ASSEMBLYAI_API_KEY").ok();

    // Enumerate available audio sessions
    let available_sessions = wasapi_sessions::enumerate_audio_sessions()
        .context("Failed to enumerate audio sessions")?;

    if available_sessions.is_empty() {
        println!("{}", style("No active audio sessions found.").yellow());
        println!("{}", style("Make sure applications are playing/recording audio.").yellow());
        return Ok(());
    }

    println!("{}", style(format!("Found {} active audio session(s)", available_sessions.len())).green());

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
            println!("{}", style("Goodbye!").cyan());
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
        println!("{}", style("No sessions selected.").yellow());
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
                    audio::start_input_recording(&device, &mut recording_session, api_key.clone())
                        .map(|stream| (Some(stream), None))
                }
                Err(e) => {
                    eprintln!("{}", style(format!("Failed to get input device: {}", e)).red());
                    continue;
                }
            }
        } else {
            // Output device (loopback)
            #[cfg(windows)]
            {
                audio::start_output_recording(&session_info.device_name, &mut recording_session, api_key.clone())
                    .map(|cleanup| (None, Some(cleanup)))
            }
            #[cfg(not(windows))]
            {
                eprintln!("{}", style("Output recording not supported on this platform").red());
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
                eprintln!("{}", style(format!("Failed to start recording: {}", e)).red());
            }
        }
    }

    if session_manager.active_sessions.is_empty() {
        ui::restore_terminal(terminal)?;
        println!("{}", style("No recording sessions started.").yellow());
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
                println!();
                println!("{}", style("=== Saving Recordings ===").bold().green());
                println!();

                for (_id, session) in &session_manager.active_sessions {
                    let prefix = session.get_filename_prefix();

                    // Save audio
                    if let Ok(buffer) = session.audio_buffer.lock() {
                        if !buffer.is_empty() {
                            let audio_filename = format!("{}.wav", prefix);
                            match audio::save_audio_file(&buffer, session.sample_rate, session.channels, &audio_filename) {
                                Ok(_) => {
                                    println!("{} {}",
                                        style("✓ Audio saved:").green().bold(),
                                        style(&audio_filename).cyan()
                                    );
                                    let duration = buffer.len() as f32 / (session.sample_rate * session.channels as u32) as f32;
                                    println!("  Duration: {:.2}s, Samples: {}", duration, buffer.len());
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

                    // Save transcript
                    if let Ok(segments) = session.transcript_buffer.lock() {
                        if !segments.is_empty() {
                            let transcript_filename = format!("{}.txt", prefix);
                            match audio::save_transcript_file(&segments, &transcript_filename) {
                                Ok(_) => {
                                    println!("{} {}",
                                        style("✓ Transcript saved:").green().bold(),
                                        style(&transcript_filename).cyan()
                                    );
                                    println!("  Segments: {}", segments.len());
                                }
                                Err(e) => {
                                    eprintln!("{} {}",
                                        style("✗ Failed to save transcript:").red().bold(),
                                        e
                                    );
                                }
                            }
                        }
                    }

                    println!();
                }

                println!("{}", style("All sessions saved successfully!").green().bold());
                break;
            }
            RecordingInputAction::QuitWithoutSaving => {
                session_manager.stop_all();
                ui::restore_terminal(terminal)?;
                println!("{}", style("Exited without saving.").yellow());
                break;
            }
            RecordingInputAction::Continue => {
                // Continue recording
            }
        }
    }

    Ok(())
}
