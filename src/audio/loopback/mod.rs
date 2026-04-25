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
