//! Capturable devices and running apps, independent of the picker and UI.
//!
//! Snapshots are refreshed when a block is requested. Only macOS currently
//! supplies one; other platforms keep the language-first block flow.

use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum DeviceKind {
    Input,
    Output,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AudioDevice {
    pub name: String,
    pub kind: DeviceKind,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AppTarget {
    pub id: String,
    pub name: String,
    pub pid: u32,
}

/// Point-in-time enumeration of audio devices and running apps, refreshed per new block.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AudioSourceSnapshot {
    pub devices: Vec<AudioDevice>,
    pub apps: Vec<AppTarget>,
}

/// The source funnel's persisted, revision-gated position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum FunnelStep {
    Device,
    App,
    Language,
    Committed,
}

impl FunnelStep {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Device => "Device",
            Self::App => "App",
            Self::Language => "Language",
            Self::Committed => "Committed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "Device" => Some(Self::Device),
            "App" => Some(Self::App),
            "Language" => Some(Self::Language),
            "Committed" => Some(Self::Committed),
            _ => None,
        }
    }
}

/// UI mirror of the current source selections and persisted revision.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SourceSelection {
    pub snapshot: AudioSourceSnapshot,
    pub device: Option<AudioDevice>,
    pub app: Option<AppTarget>,
    pub rev: u32,
}

impl SourceSelection {
    /// Chosen device and app labels, in block-title order.
    pub fn labels(&self) -> impl Iterator<Item = &str> {
        self.device
            .iter()
            .map(|device| device.name.as_str())
            .chain(self.app.iter().map(|app| app.name.as_str()))
    }
}

pub trait SourceCatalog: Send + Sync {
    /// `None` means source enumeration is unavailable on this host.
    fn snapshot(&self) -> Option<AudioSourceSnapshot>;
}

/// Language-first fallback, also useful for deterministic tests on macOS.
pub struct NoSources;

impl SourceCatalog for NoSources {
    fn snapshot(&self) -> Option<AudioSourceSnapshot> {
        None
    }
}

pub fn system_sources() -> Arc<dyn SourceCatalog> {
    #[cfg(target_os = "macos")]
    {
        Arc::new(MacSources)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Arc::new(NoSources)
    }
}

#[cfg(target_os = "macos")]
pub struct MacSources;

#[cfg(target_os = "macos")]
impl SourceCatalog for MacSources {
    fn snapshot(&self) -> Option<AudioSourceSnapshot> {
        macos::snapshot()
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::collections::{BTreeSet, HashSet};

    use cpal::traits::{DeviceTrait, HostTrait};
    use objc2::rc::autoreleasepool;
    use objc2_app_kit::{NSApplicationActivationPolicy, NSWorkspace};

    use super::{AppTarget, AudioDevice, AudioSourceSnapshot, DeviceKind};

    pub(super) fn snapshot() -> Option<AudioSourceSnapshot> {
        let host = cpal::default_host();
        let mut inputs = BTreeSet::new();
        let mut outputs = BTreeSet::new();
        let mut available = false;

        if let Ok(devices) = host.input_devices() {
            available = true;
            for device in devices {
                if let Ok(name) = device.name() {
                    inputs.insert(name);
                }
            }
        }
        if let Ok(devices) = host.output_devices() {
            available = true;
            for device in devices {
                if let Ok(name) = device.name() {
                    outputs.insert(name);
                }
            }
        }
        // cpal can omit output-only devices with no supported output configs.
        // Sweep all devices as well, excluding names already known as inputs.
        // Explicit output enumeration still preserves duplex devices in both axes.
        if let Ok(devices) = host.devices() {
            available = true;
            for device in devices {
                if let Ok(name) = device.name() {
                    if !inputs.contains(&name) {
                        outputs.insert(name);
                    }
                }
            }
        }
        if !available {
            return None;
        }

        let devices = inputs
            .into_iter()
            .map(|name| AudioDevice {
                name,
                kind: DeviceKind::Input,
            })
            .chain(outputs.into_iter().map(|name| AudioDevice {
                name,
                kind: DeviceKind::Output,
            }))
            .collect();
        Some(AudioSourceSnapshot {
            devices,
            apps: running_apps(),
        })
    }

    /// NSWorkspace includes minimized and tray apps without Screen Recording
    /// permission, unlike SCShareableContent's shareable-window list.
    fn running_apps() -> Vec<AppTarget> {
        // The source worker has no Cocoa run loop to drain autoreleased objects.
        autoreleasepool(|_| {
            let workspace = NSWorkspace::sharedWorkspace();
            let running = workspace.runningApplications();
            let mut apps = Vec::new();
            let mut seen = HashSet::new();

            for index in 0..running.count() {
                let app = running.objectAtIndex(index);
                if app.activationPolicy() != NSApplicationActivationPolicy::Regular {
                    continue;
                }
                let Some(name) = app.localizedName() else {
                    continue;
                };
                let name = name.to_string();
                if name.is_empty() {
                    continue;
                }
                let id = app
                    .bundleIdentifier()
                    .map(|bundle| bundle.to_string())
                    .filter(|bundle| !bundle.is_empty())
                    .unwrap_or_else(|| name.clone());
                if !seen.insert(id.clone()) {
                    continue;
                }
                apps.push(AppTarget {
                    id,
                    name,
                    pid: app.processIdentifier().max(0) as u32,
                });
            }

            apps.sort_by_cached_key(|app| app.name.to_lowercase());
            apps
        })
    }
}
