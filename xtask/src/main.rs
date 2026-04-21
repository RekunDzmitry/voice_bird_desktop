//! Workspace helper binary. Two tasks:
//!
//! * `build-sidecar` — run `swift build -c release` in `whisperkit-helper/`
//!   and copy the resulting `voice-bird-whisperkit` into `target/release/`
//!   so the main binary's `sidecar_path()` probes find it. macOS-only.
//!
//! * `check-catalog` — bail if `src/transcription/models.rs` still has
//!   any `<FILL>` placeholder SHA-256 digests. The digests are deferred
//!   to a user-side step (downloading every model + summing), so this
//!   task is expected to fail on pristine checkouts.

fn main() -> anyhow::Result<()> {
    let task = std::env::args().nth(1).unwrap_or_default();
    match task.as_str() {
        "build-sidecar" => build_sidecar(),
        "check-catalog" => check_catalog(),
        _ => {
            eprintln!("usage: xtask {{build-sidecar|check-catalog}}");
            std::process::exit(1);
        }
    }
}

fn build_sidecar() -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        let status = std::process::Command::new("swift")
            .arg("build")
            .arg("-c")
            .arg("release")
            .current_dir("whisperkit-helper")
            .status()?;
        anyhow::ensure!(status.success(), "swift build failed");
        std::fs::copy(
            "whisperkit-helper/.build/release/voice-bird-whisperkit",
            "target/release/voice-bird-whisperkit",
        )?;
        println!("Sidecar built at target/release/voice-bird-whisperkit");
    }
    #[cfg(not(target_os = "macos"))]
    {
        eprintln!("build-sidecar is macOS-only; skipping.");
    }
    Ok(())
}

fn check_catalog() -> anyhow::Result<()> {
    let src = std::fs::read_to_string("src/transcription/models.rs")?;
    if src.contains("<FILL") {
        anyhow::bail!(
            "models catalog still has <FILL> placeholders; \
             replace with real SHA-256 digests"
        );
    }
    Ok(())
}
