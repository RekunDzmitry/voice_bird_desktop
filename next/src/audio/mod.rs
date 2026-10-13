//! Synchronous capture backends owned entirely by the recording worker.
//! Native streams are deliberately not `Send`; PCM never enters the event bus.

use std::sync::Arc;
use std::time::Duration;

use crate::audio_sources::{AppTarget, DeviceKind, SourceSelection};

#[cfg(target_os = "macos")]
mod app_macos;
pub mod buffer;
#[cfg(target_os = "macos")]
mod mic;
#[cfg(target_os = "macos")]
mod native;
pub mod resample;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub enum CaptureSource {
    Mic { device_name: String },
    App { device_name: String, app: AppTarget },
}

impl CaptureSource {
    pub fn from_selection(selection: Option<&SourceSelection>) -> anyhow::Result<Self> {
        let Some(selection) = selection else {
            return Ok(Self::Mic {
                device_name: String::new(),
            });
        };
        let device = selection
            .device
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no audio device selected"))?;
        match device.kind {
            DeviceKind::Input => Ok(Self::Mic {
                device_name: device.name.clone(),
            }),
            DeviceKind::Output => Ok(Self::App {
                device_name: device.name.clone(),
                app: selection
                    .app
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("no application selected for output capture"))?,
            }),
        }
    }
}

pub trait AudioCapture: Send + Sync {
    fn open(&self, source: &CaptureSource) -> anyhow::Result<Box<dyn CaptureStream>>;
}

/// Chunks are normalized 16 kHz mono PCM. Timeout is not end-of-stream.
#[derive(Debug, PartialEq)]
pub enum CaptureChunk {
    Data(Vec<f32>),
    Timeout,
    End,
}

/// Create, use, finish and drop on the same blocking worker.
pub trait CaptureStream {
    fn next_chunk(&mut self, timeout: Duration) -> anyhow::Result<CaptureChunk>;
    /// Pending native PCM expressed as 16 kHz mono samples, excluding dequeued audio.
    /// Sampling the queue must not block capture callbacks or consume audio.
    fn queued_samples(&self) -> u64;
    /// Stops native capture and returns queued audio plus the resampler's tail.
    fn finish(&mut self) -> anyhow::Result<Vec<f32>> {
        Ok(Vec::new())
    }
}

struct SystemCapture;

impl AudioCapture for SystemCapture {
    fn open(&self, source: &CaptureSource) -> anyhow::Result<Box<dyn CaptureStream>> {
        #[cfg(target_os = "macos")]
        {
            match source {
                CaptureSource::Mic { device_name } => mic::open(device_name),
                CaptureSource::App { app, .. } => app_macos::open(app),
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = source;
            anyhow::bail!(
                "audio capture unavailable on this platform (native capture requires macOS)"
            )
        }
    }
}

pub fn system_capture() -> Arc<dyn AudioCapture> {
    Arc::new(SystemCapture)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_sources::{AudioDevice, AudioSourceSnapshot};

    #[test]
    fn selection_requires_an_app_for_output_but_defaults_to_mic_without_catalog() {
        assert_eq!(
            CaptureSource::from_selection(None).unwrap(),
            CaptureSource::Mic {
                device_name: String::new()
            }
        );
        let mut selection = SourceSelection {
            snapshot: AudioSourceSnapshot {
                devices: vec![],
                apps: vec![],
            },
            device: Some(AudioDevice {
                name: "Speakers".into(),
                kind: DeviceKind::Output,
            }),
            app: None,
            rev: 0,
        };
        assert!(CaptureSource::from_selection(Some(&selection))
            .unwrap_err()
            .to_string()
            .contains("application"));
        let app = AppTarget {
            id: "org.example.app".into(),
            name: "Example".into(),
            pid: 123,
        };
        selection.app = Some(app.clone());
        assert_eq!(
            CaptureSource::from_selection(Some(&selection)).unwrap(),
            CaptureSource::App {
                device_name: "Speakers".into(),
                app
            }
        );
        selection.device.as_mut().unwrap().kind = DeviceKind::Input;
        assert_eq!(
            CaptureSource::from_selection(Some(&selection)).unwrap(),
            CaptureSource::Mic {
                device_name: "Speakers".into()
            }
        );
    }
}
