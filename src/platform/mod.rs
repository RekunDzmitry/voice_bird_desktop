use anyhow::Result;
use cpal::traits::{DeviceTrait, HostTrait};
use serde::{Deserialize, Serialize};

pub use voice_bird::config::AudioSessionKind;

/// Information about an audio device the user can record from.
///
/// `Input` entries go through cpal directly. `Output` entries route through
/// the platform's loopback capture path (ScreenCaptureKit on macOS, WASAPI
/// loopback on Windows, PulseAudio monitor on Linux).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioSession {
    pub device_name: String,
    pub app_name: String,
    pub process_id: u32,
    #[serde(default = "default_input_kind")]
    pub kind: AudioSessionKind,
}

fn default_input_kind() -> AudioSessionKind {
    AudioSessionKind::Input
}

/// Enumerate both input (mic) and output (playback) devices.
///
/// Inputs come from `host.input_devices()`. Outputs are the union of
/// `host.output_devices()` and `host.devices()` — cpal filters pure
/// output-only devices (e.g. "Mac mini Speakers" on macOS) out of
/// `output_devices()` when they report zero supported output configs,
/// so we also sweep `devices()` for anything that isn't already listed
/// as an input.
///
/// A device name can legitimately appear as both Input and Output
/// (e.g. a USB headset with mic + speaker). We intentionally don't
/// dedup those — they're different capture targets.
pub fn enumerate_audio_sessions() -> Result<Vec<AudioSession>> {
    let host = cpal::default_host();
    let mut sessions = Vec::new();

    let mut input_names: Vec<String> = Vec::new();
    if let Ok(devices) = host.input_devices() {
        for device in devices {
            let name = match device.name() {
                Ok(n) => n,
                Err(_) => continue,
            };
            input_names.push(name.clone());
            sessions.push(AudioSession {
                device_name: name.clone(),
                app_name: name,
                process_id: 0,
                kind: AudioSessionKind::Input,
            });
        }
    }

    let mut output_names: Vec<String> = Vec::new();
    if let Ok(devices) = host.output_devices() {
        for device in devices {
            if let Ok(name) = device.name() {
                output_names.push(name);
            }
        }
    }
    // Fallback sweep: anything `devices()` knows about that isn't an input.
    // Covers pure-output devices that cpal excludes from `output_devices()`
    // because `supported_output_configs` returned zero (common on macOS).
    if let Ok(all) = host.devices() {
        for device in all {
            if let Ok(name) = device.name() {
                if input_names.iter().any(|n| n == &name) {
                    continue;
                }
                if !output_names.iter().any(|n| n == &name) {
                    output_names.push(name);
                }
            }
        }
    }
    for name in output_names {
        sessions.push(AudioSession {
            device_name: name.clone(),
            app_name: name,
            process_id: 0,
            kind: AudioSessionKind::Output,
        });
    }

    Ok(sessions)
}
