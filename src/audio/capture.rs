//! Thin cpal wrapper that streams interleaved f32 frames from the default
//! input device over an mpsc channel.
//!
//! `cpal::Stream` is `!Send`, so it cannot be moved into a `tokio::spawn`
//! task. Callers keep the `Stream` on the owning thread (typically the `App`
//! struct) and only move the `frames_rx` receiver into the async producer
//! task. [`CaptureHandle::split`] makes this ergonomic.
use anyhow::{anyhow, Context};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use tokio::sync::mpsc;

/// Metadata about the cpal stream: its device-native sample rate and
/// channel count. Used by the resampler to normalize to 16 kHz mono.
#[derive(Debug, Clone, Copy)]
pub struct CaptureInfo {
    pub sample_rate: u32,
    pub channels: u16,
}

/// Keep-alive handle for whatever backend is producing frames. The `App`
/// pins this to its owning thread (same as a bare `cpal::Stream` used to be)
/// and dropping it cleanly stops capture.
///
/// NOTE: intentionally NOT required to be `Send`. `cpal::Stream` is `!Send`,
/// so the whole enum is `!Send` by auto-trait inference, which matches what
/// the `App` expects. The macOS SCK variant's underlying `SCStream` happens
/// to be `Send` by itself but we don't rely on that.
pub enum CaptureKeepAlive {
    Cpal(cpal::Stream),
    #[cfg(target_os = "macos")]
    Sck(crate::audio::loopback::loopback_macos::LoopbackKeepAlive),
}

/// Returned by [`capture_default_input`] / [`capture_input`] /
/// [`crate::audio::loopback::capture_loopback`]. Holds the receiver end of
/// the frames channel plus the live backend keep-alive. Call
/// [`CaptureHandle::split`] to hand the receiver to an async task while
/// keeping the keep-alive on the calling (Send-free) thread.
pub struct CaptureHandle {
    pub frames_rx: mpsc::Receiver<Vec<f32>>,
    pub info: CaptureInfo,
    pub stream: CaptureKeepAlive,
}

impl CaptureHandle {
    /// Consume the handle and split it into the `Send` parts (the receiver
    /// and metadata) and the `!Send` keep-alive, which the caller must keep
    /// alive on the current thread for capture to continue.
    pub fn split(self) -> (mpsc::Receiver<Vec<f32>>, CaptureInfo, CaptureKeepAlive) {
        (self.frames_rx, self.info, self.stream)
    }
}

/// Open the default input device, start capturing, and return a handle
/// whose `frames_rx` yields interleaved f32 frames at the device's native
/// sample rate and channel count.
pub fn capture_default_input() -> anyhow::Result<CaptureHandle> {
    capture_input(None)
}

/// Open a specific input device by cpal name (or the default if `None`),
/// start capturing, and return a handle whose `frames_rx` yields interleaved
/// f32 frames at the device's native sample rate and channel count.
///
/// If `name` is `Some` but no device matches, returns an error — callers
/// can choose to fall back to `capture_input(None)` and surface a banner.
pub fn capture_input(name: Option<&str>) -> anyhow::Result<CaptureHandle> {
    let host = cpal::default_host();
    let device = match name {
        None => host
            .default_input_device()
            .ok_or_else(|| anyhow!("no default input device"))?,
        Some(want) => {
            let mut found = None;
            if let Ok(devices) = host.input_devices() {
                for d in devices {
                    if matches!(d.name(), Ok(n) if n == want) {
                        found = Some(d);
                        break;
                    }
                }
            }
            found.ok_or_else(|| anyhow!("input device not found: {want}"))?
        }
    };
    let config = device
        .default_input_config()
        .context("default input config")?;
    let sample_rate = config.sample_rate().0;
    let channels = config.channels();
    let format = config.sample_format();

    let (tx, rx) = mpsc::channel::<Vec<f32>>(64);

    let err_fn = |e| log::error!("cpal stream error: {e}");

    let stream = match format {
        cpal::SampleFormat::F32 => {
            let tx = tx.clone();
            device.build_input_stream(
                &config.into(),
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    let _ = tx.blocking_send(data.to_vec());
                },
                err_fn,
                None,
            )
        }
        cpal::SampleFormat::I16 => {
            let tx = tx.clone();
            device.build_input_stream(
                &config.into(),
                move |data: &[i16], _: &cpal::InputCallbackInfo| {
                    let v: Vec<f32> =
                        data.iter().map(|s| *s as f32 / i16::MAX as f32).collect();
                    let _ = tx.blocking_send(v);
                },
                err_fn,
                None,
            )
        }
        cpal::SampleFormat::U16 => {
            let tx = tx.clone();
            device.build_input_stream(
                &config.into(),
                move |data: &[u16], _: &cpal::InputCallbackInfo| {
                    let v: Vec<f32> = data
                        .iter()
                        .map(|s| (*s as f32 - 32768.0) / 32768.0)
                        .collect();
                    let _ = tx.blocking_send(v);
                },
                err_fn,
                None,
            )
        }
        f => return Err(anyhow!("unsupported sample format: {f:?}")),
    }
    .context("build input stream")?;

    stream.play().context("stream.play()")?;

    Ok(CaptureHandle {
        frames_rx: rx,
        info: CaptureInfo {
            sample_rate,
            channels,
        },
        stream: CaptureKeepAlive::Cpal(stream),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_input_with_unknown_name_returns_err() {
        match capture_input(Some("___definitely_not_a_real_device___")) {
            Ok(_) => panic!("expected Err for bogus device name"),
            Err(e) => {
                let msg = format!("{e}");
                assert!(
                    msg.contains("not found"),
                    "error message should mention 'not found', got: {msg}"
                );
            }
        }
    }
}
