use anyhow::Result;
use chrono::Local;
use std::fs;
use std::path::PathBuf;

/// Initialize file-based logging for the CLI.
///
/// Logs to `~/.voice-bird-cli/logs/voice_bird_cli_YYYY-MM-DD_HH-MM-SS.log`.
/// All levels (DEBUG+) go to file only — nothing to terminal (TUI owns the screen).
/// Returns the log file path so the app can display it.
pub fn init() -> Result<PathBuf> {
    let logs_dir = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?
        .join(".voice-bird-cli")
        .join("logs");

    fs::create_dir_all(&logs_dir)?;

    let log_filename = format!(
        "voice_bird_cli_{}.log",
        Local::now().format("%Y-%m-%d_%H-%M-%S")
    );
    let log_path = logs_dir.join(log_filename);

    let log_file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&log_path)?;

    fern::Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                "[{} {} {}] {}",
                Local::now().format("%Y-%m-%d %H:%M:%S"),
                record.level(),
                record.target(),
                message
            ))
        })
        .level(log::LevelFilter::Debug)
        .chain(log_file)
        .apply()
        .map_err(|e| anyhow::anyhow!("Failed to initialize logger: {}", e))?;

    Ok(log_path)
}
