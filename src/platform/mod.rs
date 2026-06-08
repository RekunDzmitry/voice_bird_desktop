use anyhow::Result;
use cpal::traits::{DeviceTrait, HostTrait};
use serde::{Deserialize, Serialize};

pub use voice_bird_cli::config::AudioSessionKind;

/// A capturable physical audio device. Always either an input (mic) or
/// an output (loopback target). The App dimension is its own list — see
/// [`AppSession`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioDevice {
    pub name: String,
    pub kind: AudioSessionKind, // Input | Output (App is rejected here)
}

/// A per-application capture target. `id` is the bundle identifier
/// (macOS) or PID-stringified value (Windows); `name` is the
/// human-readable label shown in the picker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSession {
    pub id: String,
    pub name: String,
    pub process_id: u32,
}

/// Two-axis inventory of capturable audio. The picker treats each axis
/// independently: the user picks a device (mic or output speakers) and
/// optionally pairs it with an app for filtered capture.
#[derive(Debug, Clone, Default)]
pub struct AudioInventory {
    pub devices: Vec<AudioDevice>,
    pub apps: Vec<AppSession>,
}

/// Enumerate inputs (mic), outputs (playback), and per-app capture
/// targets into split lists.
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
pub fn enumerate_audio_inventory() -> Result<AudioInventory> {
    let host = cpal::default_host();
    let mut devices: Vec<AudioDevice> = Vec::new();

    let mut input_names: Vec<String> = Vec::new();
    if let Ok(devs) = host.input_devices() {
        for device in devs {
            let name = match device.name() {
                Ok(n) => n,
                Err(_) => continue,
            };
            input_names.push(name.clone());
            devices.push(AudioDevice {
                name,
                kind: AudioSessionKind::Input,
            });
        }
    }

    let mut output_names: Vec<String> = Vec::new();
    if let Ok(devs) = host.output_devices() {
        for device in devs {
            if let Ok(name) = device.name() {
                output_names.push(name);
            }
        }
    }
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
        devices.push(AudioDevice {
            name,
            kind: AudioSessionKind::Output,
        });
    }

    let apps = enumerate_app_sessions().unwrap_or_default();
    Ok(AudioInventory { devices, apps })
}

/// Enumerate per-application audio capture targets.
///
/// macOS: walks `NSWorkspace.runningApplications` (no special permission
/// required) and filters to apps with `activationPolicy ==
/// NSApplicationActivationPolicyRegular` (= 0) so daemons and helper agents
/// are excluded. Earlier versions used `SCShareableContent::applications()`,
/// but Apple's contract there is "apps with at least one shareable window",
/// which silently drops audio-producing apps in the tray (Spotify, Music
/// minimized, …) and collapses to a near-empty list when Screen Recording
/// permission is missing on the host process.
///
/// Each entry's `id` carries the bundle identifier (or the localized name
/// when bundleIdentifier is empty) and is the key
/// `loopback_macos::capture_app` looks up against `SCShareableContent`
/// at capture time.
#[cfg(target_os = "macos")]
fn enumerate_app_sessions() -> Result<Vec<AppSession>> {
    use std::collections::HashSet;
    use std::ffi::CStr;
    use std::os::raw::c_char;

    use objc::runtime::Object;
    use objc::{class, msg_send, sel, sel_impl};

    const NS_APP_ACTIVATION_POLICY_REGULAR: i64 = 0;

    unsafe fn nsstring_to_string(s: *mut Object) -> String {
        if s.is_null() {
            return String::new();
        }
        let utf8: *const c_char = msg_send![s, UTF8String];
        if utf8.is_null() {
            return String::new();
        }
        CStr::from_ptr(utf8).to_string_lossy().into_owned()
    }

    let mut out: Vec<AppSession> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    unsafe {
        let workspace: *mut Object = msg_send![class!(NSWorkspace), sharedWorkspace];
        if workspace.is_null() {
            return Ok(out);
        }
        let apps: *mut Object = msg_send![workspace, runningApplications];
        if apps.is_null() {
            return Ok(out);
        }
        let count: usize = msg_send![apps, count];
        for i in 0..count {
            let app: *mut Object = msg_send![apps, objectAtIndex: i];
            if app.is_null() {
                continue;
            }
            let policy: i64 = msg_send![app, activationPolicy];
            if policy != NS_APP_ACTIVATION_POLICY_REGULAR {
                continue;
            }
            let name = nsstring_to_string(msg_send![app, localizedName]);
            if name.is_empty() {
                continue;
            }
            let bundle = nsstring_to_string(msg_send![app, bundleIdentifier]);
            let pid: i32 = msg_send![app, processIdentifier];
            let id = if bundle.is_empty() {
                name.clone()
            } else {
                bundle
            };
            if !seen.insert(id.clone()) {
                continue;
            }
            out.push(AppSession {
                id,
                name,
                process_id: pid.max(0) as u32,
            });
        }
    }

    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(out)
}

/// Windows: enumerate audio-producing processes by walking
/// `IAudioSessionEnumerator` of the default render endpoint. Each session
/// reports a process id; we map pid → process name via OpenProcess +
/// GetModuleFileNameExW. Sessions with `pid == 0` (the system mix) are
/// skipped — process loopback can't target them.
#[cfg(target_os = "windows")]
fn enumerate_app_sessions() -> Result<Vec<AppSession>> {
    use std::collections::HashSet;
    use windows::core::Interface;
    use windows::Win32::Foundation::{CloseHandle, HMODULE};
    use windows::Win32::Media::Audio::{
        eConsole, eRender, IAudioSessionControl2, IAudioSessionEnumerator, IAudioSessionManager2,
        IMMDeviceEnumerator, MMDeviceEnumerator,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
    };
    use windows::Win32::System::ProcessStatus::GetModuleFileNameExW;
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ,
    };

    let mut out: Vec<AppSession> = Vec::new();
    let mut seen_pids: HashSet<u32> = HashSet::new();

    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let result: Result<()> = (|| {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let endpoint = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
            let session_mgr: IAudioSessionManager2 = endpoint.Activate(CLSCTX_ALL, None)?;
            let session_enum: IAudioSessionEnumerator = session_mgr.GetSessionEnumerator()?;
            let count = session_enum.GetCount()?; // returns i32
            for i in 0..count {
                let Ok(session) = session_enum.GetSession(i) else {
                    continue;
                };
                let Ok(session2) = session.cast::<IAudioSessionControl2>() else {
                    continue;
                };
                let pid = session2.GetProcessId().unwrap_or(0);
                if pid == 0 {
                    continue;
                }
                if !seen_pids.insert(pid) {
                    continue;
                }
                let mut name = format!("PID {pid}");
                if let Ok(handle) =
                    OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, false, pid)
                {
                    let mut buf: [u16; 512] = [0; 512];
                    let n = GetModuleFileNameExW(handle, HMODULE::default(), &mut buf);
                    if n > 0 {
                        let s = String::from_utf16_lossy(&buf[..n as usize]);
                        if let Some(stem) = std::path::Path::new(&s)
                            .file_stem()
                            .and_then(|os| os.to_str())
                        {
                            name = stem.to_string();
                        }
                    }
                    let _ = CloseHandle(handle);
                }
                out.push(AppSession {
                    id: pid.to_string(),
                    name,
                    process_id: pid,
                });
            }
            Ok(())
        })();
        CoUninitialize();
        if let Err(e) = result {
            log::warn!("platform: Windows app enumeration failed: {e:?}");
            return Ok(Vec::new());
        }
    }
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(out)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn enumerate_app_sessions() -> Result<Vec<AppSession>> {
    Ok(Vec::new())
}
