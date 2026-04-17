use anyhow::Result;
use cpal::traits::{DeviceTrait, HostTrait};
use serde::{Deserialize, Serialize};

/// Information about an audio session (microphone input device).
///
/// Project A is microphone-only. Loopback / system-audio capture is deferred
/// to a follow-up sub-project — see
/// `docs/superpowers/specs/2026-04-16-desktop-local-whisper-design.md`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioSession {
    pub device_name: String,
    pub app_name: String,
    pub process_id: u32,
}

/// Enumerate microphone input devices via cpal.
pub fn enumerate_audio_sessions() -> Result<Vec<AudioSession>> {
    let host = cpal::default_host();
    let mut sessions = Vec::new();

    if let Ok(devices) = host.input_devices() {
        for device in devices {
            let name = match device.name() {
                Ok(n) => n,
                Err(_) => continue,
            };
            sessions.push(AudioSession {
                device_name: name.clone(),
                app_name: name,
                process_id: 0,
            });
        }
    }

    Ok(sessions)
}
