use anyhow::{Result, Context};
use crate::session::AudioSessionInfo;

#[cfg(windows)]
use windows::{
    Win32::Media::Audio::*,
    Win32::System::Com::*,
    Win32::System::Threading::*,
    Win32::Foundation::*,
    Win32::System::ProcessStatus::*,
    Win32::UI::Shell::PropertiesSystem::*,
    Win32::Devices::FunctionDiscovery::*,
    core::Interface,
};

#[cfg(windows)]
pub fn enumerate_audio_sessions() -> Result<Vec<AudioSessionInfo>> {
    unsafe {
        // Initialize COM if not already initialized
        let com_init_result = CoInitializeEx(None, COINIT_MULTITHREADED);
        let should_uninit_com = com_init_result.is_ok();

        let result = (|| -> Result<Vec<AudioSessionInfo>> {
            let mut all_sessions = Vec::new();

            // Create device enumerator
            let enumerator: IMMDeviceEnumerator = CoCreateInstance(
                &MMDeviceEnumerator,
                None,
                CLSCTX_ALL,
            ).ok().context("Failed to create device enumerator")?;

            // Get device collection (both input and output)
            let device_collection = enumerator
                .EnumAudioEndpoints(eAll, DEVICE_STATE_ACTIVE)
                .ok()
                .context("Failed to enumerate audio endpoints")?;

            let count = device_collection.GetCount().ok().context("Failed to get device count")?;

            for i in 0..count {
                let device = device_collection.Item(i)
                    .ok()
                    .context("Failed to get device")?;

                // Get device friendly name using property store
                let property_store: IPropertyStore = device.OpenPropertyStore(STGM_READ)
                    .ok()
                    .context("Failed to open property store")?;

                let prop_variant = property_store.GetValue(&PKEY_Device_FriendlyName)
                    .ok()
                    .context("Failed to get device friendly name")?;

                // Convert PROPVARIANT to string
                let device_name = prop_variant.to_string();

                // Determine if it's an input or output device
                let endpoint: IMMEndpoint = device.cast()
                    .ok()
                    .context("Failed to cast to IMMEndpoint")?;
                let endpoint_type = endpoint.GetDataFlow()
                    .ok()
                    .unwrap_or(eRender);
                let is_input = endpoint_type == eCapture;

                // Get session manager
                let audio_client: IAudioSessionManager2 = device.Activate(CLSCTX_ALL, None)
                    .ok()
                    .context("Failed to activate audio session manager")?;
                let session_enumerator = audio_client.GetSessionEnumerator()
                    .ok()
                    .context("Failed to get session enumerator")?;
                let session_count = session_enumerator.GetCount()
                    .ok()
                    .context("Failed to get session count")?;

                for j in 0..session_count {
                    let session_control = session_enumerator.GetSession(j)
                        .ok()
                        .context("Failed to get session")?;
                    let session_control2: IAudioSessionControl2 = session_control.cast()
                        .ok()
                        .context("Failed to cast to IAudioSessionControl2")?;

                    // Get process ID
                    let process_id = session_control2.GetProcessId()
                        .ok()
                        .context("Failed to get process ID")?;

                    // Skip system session (PID 0)
                    if process_id == 0 {
                        continue;
                    }

                    // Get process name
                    let app_name = get_process_name(process_id).unwrap_or_else(|_| format!("Process {}", process_id));

                    // Check if session is active (has audio)
                    let state = session_control2.GetState()
                        .ok()
                        .context("Failed to get session state")?;
                    if state == AudioSessionStateActive {
                        all_sessions.push(AudioSessionInfo {
                            device_name: device_name.clone(),
                            app_name,
                            process_id,
                            is_input,
                        });
                    }
                }
            }

            Ok(all_sessions)
        })();

        if should_uninit_com {
            CoUninitialize();
        }

        result
    }
}

#[cfg(windows)]
unsafe fn get_process_name(process_id: u32) -> Result<String> {
    let process_handle = OpenProcess(
        PROCESS_QUERY_INFORMATION | PROCESS_VM_READ,
        false,
        process_id,
    ).ok().context("Failed to open process")?;

    let mut exe_path = vec![0u16; 1024];

    let result = GetModuleFileNameExW(
        process_handle,
        HMODULE::default(),
        &mut exe_path,
    );

    CloseHandle(process_handle).ok();

    if result == 0 {
        return Err(anyhow::anyhow!("Failed to get process name"));
    }

    let path = String::from_utf16_lossy(&exe_path[..result as usize]);

    // Extract just the executable name (not full path)
    Ok(path.split('\\').last().unwrap_or(&path).to_string())
}

// Non-Windows platforms: return empty list
#[cfg(not(windows))]
pub fn enumerate_audio_sessions() -> Result<Vec<AudioSessionInfo>> {
    Ok(Vec::new())
}
