use anyhow::Result;
use chrono::Local;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::PathBuf;

/// Initialize file-based logging with clean terminal output
///
/// All log levels (debug, info, warn, error) are written to a log file
/// in the `logs/` directory with timestamp. Terminal remains clean for UI.
pub fn init() -> Result<()> {
    // Create logs directory if it doesn't exist
    let logs_dir = PathBuf::from("logs");
    fs::create_dir_all(&logs_dir)?;

    // Create log file with timestamp
    let log_filename = format!(
        "voice_bird_{}.log",
        Local::now().format("%Y-%m-%d_%H-%M-%S")
    );
    let log_path = logs_dir.join(log_filename);

    // Open log file for writing
    let log_file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&log_path)?;

    // Configure logging
    fern::Dispatch::new()
        // Format log messages
        .format(|out, message, record| {
            out.finish(format_args!(
                "[{} {} {}] {}",
                Local::now().format("%Y-%m-%d %H:%M:%S"),
                record.level(),
                record.target(),
                message
            ))
        })
        // Log all levels to file
        .level(log::LevelFilter::Debug)
        // Write to file
        .chain(log_file)
        // Apply the configuration
        .apply()?;

    // Print startup message to terminal (this goes to stdout, not through logger)
    println!("Voice Bird Desktop - Audio Streaming Client");
    println!("Logging to: {}", log_path.display());
    println!();

    Ok(())
}

/// Print a user-friendly message to terminal (bypasses logging)
pub fn print_info(message: &str) {
    println!("{}", message);
}

/// Print a user-friendly error to terminal (bypasses logging)
pub fn print_error(message: &str) {
    eprintln!("ERROR: {}", message);
}

/// Print a user-friendly warning to terminal (bypasses logging)
pub fn print_warning(message: &str) {
    println!("WARNING: {}", message);
}

/// Print connection status to terminal
pub fn print_connection_status(status: &str, details: &str) {
    println!("🔌 {}: {}", status, details);
}

/// Print streaming statistics to terminal
pub fn print_stats(device: &str, packets: u64, duration: f32, bytes: f64) {
    println!(
        "📊 {} | Packets: {} | Duration: {:.1}s | Data: {:.2} KB",
        device, packets, duration, bytes
    );
}
