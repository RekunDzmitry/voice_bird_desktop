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

/// Returned by [`capture_default_input`]. Holds the receiver end of the
/// frames channel plus the live `cpal::Stream`. Call [`CaptureHandle::split`]
/// to hand the receiver to an async task while keeping the stream on the
/// calling (Send-free) thread.
pub struct CaptureHandle {
    pub frames_rx: mpsc::Receiver<Vec<f32>>,
    pub info: CaptureInfo,
    pub stream: cpal::Stream,
}

impl CaptureHandle {
    /// Consume the handle and split it into the `Send` parts (the receiver
    /// and metadata) and the `!Send` `cpal::Stream`, which the caller must
    /// keep alive on the current thread for capture to continue.
    pub fn split(self) -> (mpsc::Receiver<Vec<f32>>, CaptureInfo, cpal::Stream) {
        (self.frames_rx, self.info, self.stream)
    }
}

/// Open the default input device, start capturing, and return a handle
/// whose `frames_rx` yields interleaved f32 frames at the device's native
/// sample rate and channel count.
pub fn capture_default_input() -> anyhow::Result<CaptureHandle> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| anyhow!("no default input device"))?;
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
        stream,
    })
}
