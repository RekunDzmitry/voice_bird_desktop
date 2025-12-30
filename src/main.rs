#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod session;
mod wasapi_sessions;
mod audio;
mod server_streaming;
mod opus_encoder;
mod audio_converter;
mod logger;
mod config;
mod commands;
mod state;
mod events;

use anyhow::Context;
use state::AppState;

fn main() {
    // Initialize file-based logging
    if let Err(e) = logger::init().context("Failed to initialize logger") {
        eprintln!("Failed to initialize logger: {}", e);
    }

    // Load .env file if present
    dotenvy::dotenv().ok();

    log::info!("Voice Bird Desktop starting...");

    tauri::Builder::default()
        .manage(AppState::new())
        .invoke_handler(tauri::generate_handler![
            commands::enumerate_sessions,
            commands::start_recording,
            commands::stop_session,
            commands::stop_all_sessions,
            commands::get_sessions_state,
            commands::get_server_config,
            commands::save_api_key,
            commands::clear_api_key,
            commands::get_api_key_status,
        ])
        .run(tauri::generate_context!())
        .expect("Error running Tauri application");
}
