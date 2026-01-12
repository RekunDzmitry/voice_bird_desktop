// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod session;
mod wasapi_sessions;
mod audio;
mod server_streaming;
mod opus_encoder;
mod audio_converter;
mod logger;

use std::env;
use tauri::Manager;

fn main() {
    // Initialize file-based logging
    logger::init().ok();

    // Load .env file if present
    dotenvy::dotenv().ok();

    log::info!("Voice Bird Desktop (GUI) starting...");

    tauri::Builder::default()
        .setup(|app| {
            let _window = app.get_webview_window("main").unwrap();

            // Check permissions and show status
            match check_permissions() {
                Ok(status) => {
                    log::info!("Permission check: {}", status);
                }
                Err(e) => {
                    log::error!("Permission check failed: {}", e);
                }
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            check_permissions_command,
            get_audio_sessions,
            start_recording,
            get_settings,
            save_settings
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[tauri::command]
fn check_permissions_command() -> Result<String, String> {
    check_permissions().map_err(|e| e.to_string())
}

#[tauri::command]
fn get_audio_sessions() -> Result<Vec<String>, String> {
    match wasapi_sessions::enumerate_audio_sessions() {
        Ok(sessions) => {
            Ok(sessions.iter().map(|s| {
                if s.app_name.is_empty() {
                    s.device_name.clone()
                } else {
                    format!("{} - {}", s.app_name, s.device_name)
                }
            }).collect())
        }
        Err(e) => Err(format!("Failed to get audio sessions: {}", e))
    }
}

#[tauri::command]
fn start_recording(sessions: Vec<String>) -> Result<String, String> {
    log::info!("Starting recording for {} session(s)", sessions.len());
    for session in &sessions {
        log::info!("  - {}", session);
    }
    Ok(format!("Recording started for {} session(s)", sessions.len()))
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Settings {
    api_key: String,
    server_url: String,
}

#[tauri::command]
fn get_settings() -> Result<Settings, String> {
    let api_key = env::var("VOICE_BIRD_API_KEY").unwrap_or_default();
    let server_url = env::var("VOICE_BIRD_SERVER_URL").unwrap_or_else(|_| "https://api.voicebird.io".to_string());

    Ok(Settings { api_key, server_url })
}

#[tauri::command]
fn save_settings(api_key: String, server_url: String) -> Result<String, String> {
    use std::fs;
    use std::io::Write;

    // Create or update .env file
    let env_content = format!(
        "VOICE_BIRD_API_KEY={}\nVOICE_BIRD_SERVER_URL={}\n",
        api_key, server_url
    );

    fs::write(".env", env_content)
        .map_err(|e| format!("Failed to save settings: {}", e))?;

    // Update environment variables for current session
    env::set_var("VOICE_BIRD_API_KEY", &api_key);
    env::set_var("VOICE_BIRD_SERVER_URL", &server_url);

    log::info!("Settings saved successfully");

    Ok("Settings saved successfully".to_string())
}

fn check_permissions() -> Result<String, String> {
    // Try to enumerate audio sessions to check permissions
    match wasapi_sessions::enumerate_audio_sessions() {
        Ok(sessions) => {
            if sessions.is_empty() {
                Ok("Permissions OK - No active audio sessions found".to_string())
            } else {
                Ok(format!("Permissions OK - Found {} audio session(s)", sessions.len()))
            }
        }
        Err(e) => {
            let error_msg = format!("{}", e);
            if error_msg.contains("Screen Recording permission") || error_msg.contains("условия") {
                Err("Screen Recording permission required. Please grant permission in System Settings > Privacy & Security > Screen Recording".to_string())
            } else {
                Err(format!("Error checking permissions: {}", e))
            }
        }
    }
}
