//! Windows per-application audio capture via WASAPI process loopback.
//!
//! Replays the resurrected design from commit `b555b95` (deleted `src/audio.rs`),
//! adapted to the modular [`crate::audio::capture::CaptureHandle`] shape so the
//! resampler and engine don't need to know which platform-specific backend
//! produced the frames.
//!
//! Process loopback requires Windows 10 Build 20348 or later (also covers all
//! Windows 11 builds). The activation handler design and the
//! `AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS` plumbing are taken verbatim from the
//! original implementation; the surrounding capture loop now feeds an mpsc
//! channel that matches what cpal and ScreenCaptureKit produce.
//!
//! NOTE: This module compiles on Windows only. The author primarily develops
//! on macOS — please verify on a Windows host before relying on this in
//! production. End-to-end smoke covers: activate process loopback for a
//! known-audible PID (e.g. Chrome playing a YouTube tab), receive non-zero
//! RMS frames, observe the `frames_rx` channel filling, and confirm clean
//! shutdown when the keep-alive drops.

#![cfg(target_os = "windows")]

use std::sync::{mpsc as std_mpsc, Arc, Mutex};
use std::thread::JoinHandle;

use anyhow::{anyhow, Context, Result};
use tokio::sync::mpsc;
use windows::core::HSTRING;
use windows::Win32::Foundation::HRESULT;
use windows::Win32::Media::Audio::{
    ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
    IAudioCaptureClient, IAudioClient, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
    AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, WAVEFORMATEX,
};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;

use crate::audio::capture::{CaptureHandle, CaptureInfo, CaptureKeepAlive};

// ---------------------------------------------------------------------------
// PROPVARIANT-as-blob shim
// ---------------------------------------------------------------------------
//
// The `AUDIOCLIENT_ACTIVATION_PARAMS` blob has to be passed inside a
// PROPVARIANT (vt = VT_BLOB). The `windows-rs` `PROPVARIANT` is opaque, so
// we lay one out manually and cast to the API type at the call site. Layout
// matches the documented PROPVARIANT BLOB form.
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

const VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK: &str = "VAD\\Process_Loopback";

// ---------------------------------------------------------------------------
// Activation completion handler
// ---------------------------------------------------------------------------

#[windows::core::implement(IActivateAudioInterfaceCompletionHandler)]
struct ActivationHandler {
    result_tx: Arc<Mutex<Option<std_mpsc::Sender<Result<IAudioClient>>>>>,
}

impl ActivationHandler {
    fn new(tx: std_mpsc::Sender<Result<IAudioClient>>) -> Self {
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
            let operation =
                operation.ok_or_else(|| anyhow!("ActivateCompleted: no operation"))?;
            unsafe {
                let mut hr_activate: HRESULT = HRESULT(0);
                let mut activated_interface: Option<windows::core::IUnknown> = None;
                operation
                    .GetActivateResult(&mut hr_activate, &mut activated_interface)
                    .ok()
                    .context("GetActivateResult")?;
                hr_activate.ok().context("activation result HRESULT")?;
                let interface = activated_interface
                    .ok_or_else(|| anyhow!("GetActivateResult: no interface"))?;
                interface
                    .cast::<IAudioClient>()
                    .context("cast to IAudioClient")
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

/// Activate an `IAudioClient` configured to capture audio from
/// `process_id` (and its descendant processes) via WASAPI process
/// loopback. Blocks up to 10 s waiting for the async activation.
unsafe fn activate_process_loopback(process_id: u32) -> Result<IAudioClient> {
    log::info!("wasapi: process loopback activation for PID {}", process_id);

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

    let params_size = std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32;
    let params_ptr = &activation_params as *const _ as *const u8;
    let prop_variant = PropVariantBlob {
        vt: VT_BLOB,
        reserved1: 0,
        reserved2: 0,
        reserved3: 0,
        cb_size: params_size,
        p_blob_data: params_ptr,
    };

    let (tx, rx) = std_mpsc::channel::<Result<IAudioClient>>();
    let handler: IActivateAudioInterfaceCompletionHandler = ActivationHandler::new(tx).into();

    let device_id = HSTRING::from(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK);
    let prop_variant_ptr = &prop_variant as *const PropVariantBlob as *const PROPVARIANT;

    ActivateAudioInterfaceAsync(
        &device_id,
        &IAudioClient::IID,
        Some(&*prop_variant_ptr),
        &handler,
    )
    .ok()
    .context("ActivateAudioInterfaceAsync")?;

    rx.recv_timeout(std::time::Duration::from_secs(10))
        .context("process loopback activation timeout")?
}

// ---------------------------------------------------------------------------
// Public capture handle + RAII keep-alive
// ---------------------------------------------------------------------------

/// RAII handle that signals the capture thread to stop and waits for it to
/// finish. Dropping this is the documented "stop capture" mechanism.
pub struct WasapiKeepAlive {
    stop: Arc<Mutex<bool>>,
    join: Option<JoinHandle<()>>,
}

impl Drop for WasapiKeepAlive {
    fn drop(&mut self) {
        if let Ok(mut g) = self.stop.lock() {
            *g = true;
        }
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// Start per-application audio capture for `pid_str` (a stringified
/// process id). Returns the same [`CaptureHandle`] shape as the macOS
/// loopback path so the resampler/engine pipeline can stay agnostic.
pub fn capture_app(pid_str: &str) -> Result<CaptureHandle> {
    let pid: u32 = pid_str
        .parse()
        .map_err(|e| anyhow!("invalid pid '{pid_str}': {e}"))?;
    log::info!("wasapi: capture_app pid={}", pid);

    let (tx, rx) = mpsc::channel::<Vec<f32>>(64);
    let stop = Arc::new(Mutex::new(false));
    let stop_for_thread = stop.clone();

    let join = std::thread::spawn(move || {
        unsafe {
            // COM must live on the capture thread for the entire session.
            if CoInitializeEx(None, COINIT_MULTITHREADED).is_err() {
                log::error!("wasapi: CoInitializeEx failed");
                return;
            }
            let res = run_capture_loop(pid, &tx, &stop_for_thread);
            if let Err(e) = res {
                log::error!("wasapi: capture loop ended with error: {e:?}");
            }
            CoUninitialize();
        }
    });

    Ok(CaptureHandle {
        frames_rx: rx,
        info: CaptureInfo {
            // Process loopback delivers the system-mix format. We pin to
            // a fixed 48 kHz stereo Float32 — matching the buried code's
            // fixed format — because GetMixFormat() returns garbage on
            // process-loopback clients in current Windows builds.
            sample_rate: 48_000,
            channels: 2,
        },
        stream: CaptureKeepAlive::Wasapi(WasapiKeepAlive {
            stop,
            join: Some(join),
        }),
    })
}

unsafe fn run_capture_loop(
    pid: u32,
    tx: &mpsc::Sender<Vec<f32>>,
    stop: &Arc<Mutex<bool>>,
) -> Result<()> {
    let audio_client = activate_process_loopback(pid)?;

    // Fixed 48 kHz stereo Float32 — see CaptureInfo above.
    let format = WAVEFORMATEX {
        wFormatTag: 3, // WAVE_FORMAT_IEEE_FLOAT
        nChannels: 2,
        nSamplesPerSec: 48_000,
        nAvgBytesPerSec: 48_000 * 2 * 4,
        nBlockAlign: 2 * 4,
        wBitsPerSample: 32,
        cbSize: 0,
    };

    audio_client
        .Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK,
            10_000_000, // 1 s buffer (in 100ns ticks)
            0,
            &format,
            None,
        )
        .ok()
        .context("IAudioClient::Initialize")?;

    let capture_client: IAudioCaptureClient = audio_client
        .GetService()
        .ok()
        .context("IAudioClient::GetService(IAudioCaptureClient)")?;

    audio_client.Start().ok().context("IAudioClient::Start")?;
    log::info!("wasapi: capture started for pid={pid}");

    let channels = format.nChannels as usize;
    let mut packet_count: u64 = 0;
    loop {
        if let Ok(g) = stop.lock() {
            if *g {
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
            let mut num_frames: u32 = 0;
            let mut flags: u32 = 0;
            if capture_client
                .GetBuffer(
                    &mut buffer_ptr as *mut *mut u8,
                    &mut num_frames,
                    &mut flags,
                    None,
                    None,
                )
                .is_ok()
                && num_frames > 0
            {
                let sample_count = (num_frames as usize) * channels;
                let slice = std::slice::from_raw_parts(buffer_ptr as *const f32, sample_count);
                let owned = slice.to_vec();
                let _ = tx.try_send(owned);
                packet_count += 1;
                if packet_count % 200 == 0 {
                    log::debug!("wasapi[pid={pid}] packets={packet_count}");
                }
                let _ = capture_client.ReleaseBuffer(num_frames);
            } else {
                break;
            }
        }
    }
    let _ = audio_client.Stop();
    log::info!("wasapi: capture stopped for pid={pid}");
    Ok(())
}
