//! System-audio loopback capture.
//!
//! Exposes the same [`crate::audio::capture::CaptureHandle`] shape as the
//! mic path so the rest of the pipeline (resampler → engine) is agnostic.
//!
//! - macOS: ScreenCaptureKit audio-only capture (see `loopback_macos`).
//! - Windows / Linux: not yet wired — returns an explanatory error so the
//!   UI can surface it as a banner.

#[cfg(not(target_os = "macos"))]
use anyhow::anyhow;
use anyhow::Result;

use crate::audio::capture::CaptureHandle;

#[cfg(target_os = "macos")]
pub mod loopback_macos;

#[cfg(target_os = "windows")]
pub mod loopback_windows;

/// Capture system audio playing on the output device `name`. If `name` is
/// `None`, captures the default output.
pub fn capture_loopback(name: Option<&str>) -> Result<CaptureHandle> {
    #[cfg(target_os = "macos")]
    {
        loopback_macos::capture(name)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = name;
        Err(anyhow!(
            "loopback capture not yet wired on this platform"
        ))
    }
}

/// Capture audio produced by a single application, identified by bundle
/// identifier on macOS or PID on Windows. Returns the same
/// [`CaptureHandle`] shape as the mic and system loopback paths so the
/// rest of the pipeline (resampler → engine) is agnostic.
pub fn capture_app(identifier: &str) -> Result<CaptureHandle> {
    #[cfg(target_os = "macos")]
    {
        loopback_macos::capture_app(identifier)
    }
    #[cfg(target_os = "windows")]
    {
        loopback_windows::capture_app(identifier)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = identifier;
        Err(anyhow!(
            "per-app capture not yet wired on this platform"
        ))
    }
}
