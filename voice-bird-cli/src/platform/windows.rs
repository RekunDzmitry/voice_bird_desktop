use anyhow::{Context, Result};
use std::sync::{Arc, Mutex, mpsc};
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
use windows_core::{HRESULT, IUnknown, PROPVARIANT};

use super::AudioSession;
use crate::audio::calculate_rms;
use crate::streaming;

/// Enumerate all active audio sessions on Windows
pub fn enumerate_audio_sessions() -> Result<Vec<AudioSession>> {
    unsafe {
        let com_init_result = CoInitializeEx(None, COINIT_MULTITHREADED);
        let should_uninit_com = com_init_result.is_ok();

        let result = (|| -> Result<Vec<AudioSession>> {
            let mut all_sessions = Vec::new();

            let enumerator: IMMDeviceEnumerator = CoCreateInstance(
                &MMDeviceEnumerator,
                None,
                CLSCTX_ALL,
            ).ok().context("Failed to create device enumerator")?;

            let device_collection = enumerator
                .EnumAudioEndpoints(eAll, DEVICE_STATE_ACTIVE)
                .ok()
                .context("Failed to enumerate audio endpoints")?;

            let count = device_collection.GetCount().ok().context("Failed to get device count")?;

            for i in 0..count {
                let device = match device_collection.Item(i).ok() {
                    Some(d) => d,
                    None => continue,
                };

                let property_store: IPropertyStore = match device.OpenPropertyStore(STGM_READ).ok() {
                    Some(ps) => ps,
                    None => continue,
                };

                let prop_variant = match property_store.GetValue(&PKEY_Device_FriendlyName).ok() {
                    Some(pv) => pv,
                    None => continue,
                };

                let device_name = prop_variant.to_string();

                let endpoint: IMMEndpoint = match device.cast().ok() {
                    Some(e) => e,
                    None => continue,
                };
                let endpoint_type = endpoint.GetDataFlow().ok().unwrap_or(eRender);
                let is_input = endpoint_type == eCapture;

                let audio_client: IAudioSessionManager2 = match device.Activate(CLSCTX_ALL, None).ok() {
                    Some(ac) => ac,
                    None => continue,
                };

                let session_enumerator = match audio_client.GetSessionEnumerator().ok() {
                    Some(se) => se,
                    None => continue,
                };

                let session_count = session_enumerator.GetCount().ok().unwrap_or(0);

                for j in 0..session_count {
                    let session_control = match session_enumerator.GetSession(j).ok() {
                        Some(sc) => sc,
                        None => continue,
                    };

                    let session_control2: IAudioSessionControl2 = match session_control.cast().ok() {
                        Some(sc2) => sc2,
                        None => continue,
                    };

                    let process_id = session_control2.GetProcessId().ok().unwrap_or(0);
                    if process_id == 0 {
                        continue;
                    }

                    let app_name = get_process_name(process_id)
                        .unwrap_or_else(|_| format!("Process {}", process_id));

                    let state = session_control2.GetState().ok().unwrap_or(AudioSessionStateExpired);
                    if state == AudioSessionStateActive {
                        all_sessions.push(AudioSession {
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

    let _ = CloseHandle(process_handle);

    if result == 0 {
        return Err(anyhow::anyhow!("Failed to get process name"));
    }

    let path = String::from_utf16_lossy(&exe_path[..result as usize]);
    Ok(path.split('\\').next_back().unwrap_or(&path).to_string())
}

// Virtual device ID for process loopback
const VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK: &str = "VAD\\Process_Loopback";
const VT_BLOB: u16 = 65;

#[repr(C)]
struct PropVariantBlob {
    vt: u16,
    reserved1: u16,
    reserved2: u16,
    reserved3: u16,
    cb_size: u32,
    p_blob_data: *const u8,
}

#[windows::core::implement(IActivateAudioInterfaceCompletionHandler)]
struct ActivationHandler {
    #[allow(clippy::arc_with_non_send_sync)]
    result_tx: Arc<Mutex<Option<mpsc::Sender<Result<IAudioClient>>>>>,
}

impl ActivationHandler {
    #[allow(clippy::arc_with_non_send_sync)]
    fn new(tx: mpsc::Sender<Result<IAudioClient>>) -> Self {
        Self {
            result_tx: Arc::new(Mutex::new(Some(tx))),
        }
    }
}

impl IActivateAudioInterfaceCompletionHandler_Impl for ActivationHandler_Impl {
    fn ActivateCompleted(
        &self,
        operation: Option<&IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        let result = (|| -> Result<IAudioClient> {
            let operation = operation.ok_or_else(|| anyhow::anyhow!("No operation"))?;

            unsafe {
                let mut hr_activate: HRESULT = HRESULT(0);
                let mut activated_interface: Option<IUnknown> = None;

                operation.GetActivateResult(
                    &mut hr_activate,
                    &mut activated_interface,
                ).ok().context("Failed to get activation result")?;

                hr_activate.ok().context("Activation failed")?;

                let interface = activated_interface
                    .ok_or_else(|| anyhow::anyhow!("No interface returned"))?;

                interface.cast::<IAudioClient>()
                    .context("Failed to cast to IAudioClient")
            }
        })();

        if let Ok(mut guard) = self.result_tx.lock() {
            if let Some(tx) = guard.take() {
                let _ = tx.send(result);
            }
        }

        Ok(())
    }
}

unsafe fn activate_process_loopback(process_id: u32) -> Result<IAudioClient> {
    let process_params = AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
        TargetProcessId: process_id,
        ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
    };

    let activation_params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: process_params,
        },
    };

    let params_size = std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>();
    let params_ptr = &activation_params as *const _ as *const u8;

    let prop_variant = PropVariantBlob {
        vt: VT_BLOB,
        reserved1: 0,
        reserved2: 0,
        reserved3: 0,
        cb_size: params_size as u32,
        p_blob_data: params_ptr,
    };

    let (tx, rx) = mpsc::channel::<Result<IAudioClient>>();
    let handler: IActivateAudioInterfaceCompletionHandler = ActivationHandler::new(tx).into();

    let device_id = windows::core::HSTRING::from(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK);
    let prop_variant_ptr = &prop_variant as *const PropVariantBlob as *const PROPVARIANT;

    ActivateAudioInterfaceAsync(
        &device_id,
        &IAudioClient::IID,
        Some(&*prop_variant_ptr),
        &handler,
    ).ok().context("Failed to start async activation")?;

    rx.recv_timeout(std::time::Duration::from_secs(10))
        .context("Activation timeout")?
}

/// Start output recording using Windows Process Loopback
pub fn start_output_recording(
    session: &AudioSession,
    server_url: String,
    api_key: String,
    session_id: String,
    audio_level: Arc<Mutex<f32>>,
    stop_signal: Arc<Mutex<bool>>,
) -> Result<()> {
    let process_id = session.process_id;
    let app_name = Some(session.app_name.clone());
    let device_name = session.device_name.clone();

    let sample_rate: u32 = 48000;
    let channels: u16 = 2;

    let (tx, rx) = mpsc::channel::<Vec<f32>>();

    // Spawn streaming thread
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let _ = streaming::stream_to_server(
                server_url,
                api_key,
                session_id,
                device_name,
                app_name,
                false,
                rx,
                sample_rate,
                channels,
            ).await;
        });
    });

    // Capture thread
    std::thread::spawn(move || {
        unsafe {
            if CoInitializeEx(None, COINIT_MULTITHREADED).is_err() {
                return;
            }

            let result = (|| -> Result<()> {
                let audio_client = activate_process_loopback(process_id)?;

                let format = WAVEFORMATEX {
                    wFormatTag: 3, // WAVE_FORMAT_IEEE_FLOAT
                    nChannels: channels,
                    nSamplesPerSec: sample_rate,
                    nAvgBytesPerSec: sample_rate * channels as u32 * 4,
                    nBlockAlign: channels * 4,
                    wBitsPerSample: 32,
                    cbSize: 0,
                };

                audio_client.Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_LOOPBACK,
                    10000000,
                    0,
                    &format,
                    None,
                ).ok().context("Failed to initialize audio client")?;

                let capture_client: IAudioCaptureClient = audio_client.GetService()
                    .ok().context("Failed to get capture client")?;

                audio_client.Start().ok().context("Failed to start capture")?;

                loop {
                    if let Ok(stop) = stop_signal.lock() {
                        if *stop {
                            break;
                        }
                    }

                    std::thread::sleep(std::time::Duration::from_millis(5));

                    loop {
                        let packet_size = capture_client.GetNextPacketSize().ok().unwrap_or(0);
                        if packet_size == 0 {
                            break;
                        }

                        let mut buffer_ptr: *mut u8 = std::ptr::null_mut();
                        let mut num_frames = 0u32;
                        let mut flags = 0u32;

                        if capture_client.GetBuffer(
                            &mut buffer_ptr,
                            &mut num_frames,
                            &mut flags,
                            None,
                            None,
                        ).is_ok() && num_frames > 0 {
                            let sample_count = (num_frames * channels as u32) as usize;
                            let float_buffer = std::slice::from_raw_parts(
                                buffer_ptr as *const f32,
                                sample_count,
                            );

                            let rms = calculate_rms(float_buffer);
                            if let Ok(mut level) = audio_level.lock() {
                                *level = rms;
                            }

                            let _ = tx.send(float_buffer.to_vec());

                            let _ = capture_client.ReleaseBuffer(num_frames);
                        }
                    }
                }

                let _ = audio_client.Stop();
                Ok(())
            })();

            if let Err(e) = result {
                eprintln!("Capture error: {}", e);
            }

            CoUninitialize();
        }
    });

    Ok(())
}
