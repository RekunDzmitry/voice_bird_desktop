//! CPAL microphone stream. The worker owns its non-Send native keepalive.

use std::time::Duration;

use anyhow::{anyhow, Context};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SizedSample};

use super::native::{FrameSender, NativeFrames};
use super::{CaptureChunk, CaptureStream};

pub(super) fn open(device_name: &str) -> anyhow::Result<Box<dyn CaptureStream>> {
    let host = cpal::default_host();
    let device = if device_name.is_empty() {
        host.default_input_device()
            .ok_or_else(|| anyhow!("no default input device"))?
    } else {
        host.input_devices()
            .context("enumerate microphone devices")?
            .find(|device| matches!(device.name(), Ok(name) if name == device_name))
            .ok_or_else(|| anyhow!("input device not found: {device_name}"))?
    };
    let supported = device
        .default_input_config()
        .context("default microphone configuration")?;
    let (sender, frames) = NativeFrames::new(supported.sample_rate().0, supported.channels())?;
    let config = supported.config();
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => build::<f32>(&device, &config, sender),
        cpal::SampleFormat::I16 => build::<i16>(&device, &config, sender),
        cpal::SampleFormat::U16 => build::<u16>(&device, &config, sender),
        format => anyhow::bail!("unsupported microphone sample format: {format:?}"),
    }
    .context("build microphone stream")?;
    stream
        .play()
        .context("start microphone capture (check Microphone permission)")?;
    Ok(Box::new(MicStream {
        stream: Some(stream),
        frames,
    }))
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    sender: FrameSender,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let errors = sender.clone();
    device.build_input_stream(
        config,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            sender.send(
                data.iter()
                    .map(|sample| Sample::to_sample::<f32>(*sample))
                    .collect(),
            );
        },
        move |error| errors.fail(format!("microphone capture failed: {error}")),
        None,
    )
}

struct MicStream {
    stream: Option<cpal::Stream>,
    frames: NativeFrames,
}

impl CaptureStream for MicStream {
    fn next_chunk(&mut self, timeout: Duration) -> anyhow::Result<CaptureChunk> {
        self.frames.next_chunk(timeout)
    }

    fn queued_samples(&self) -> u64 {
        self.frames.queued_samples()
    }

    fn finish(&mut self) -> anyhow::Result<Vec<f32>> {
        // CPAL's Drop stops callbacks before releasing native capture resources.
        drop(self.stream.take());
        self.frames.finish()
    }
}
