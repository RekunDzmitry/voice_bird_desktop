//! macOS system-audio loopback via ScreenCaptureKit.
//!
//! Captures system audio system-wide using `SCStream` with a tiny (2x2) video
//! frame the stream API requires. The audio sample handler turns each
//! `CMSampleBuffer` delivered by SCK into an interleaved `Vec<f32>` and sends
//! it through the same `mpsc::Sender<Vec<f32>>` used by the mic path — the
//! resampler + engine pipeline downstream is format-agnostic.
//!
//! Requires macOS 13+ (ScreenCaptureKit audio capture) and the "Screen &
//! System Audio Recording" privacy permission on first run.

use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use tokio::sync::mpsc;

use core_foundation::base::TCFType;
use core_media_rs::cm_sample_buffer::CMSampleBuffer;
use core_media_rs::cm_format_description::CMFormatDescriptionRef;
use screencapturekit::{
    shareable_content::SCShareableContent,
    stream::{
        configuration::SCStreamConfiguration,
        content_filter::SCContentFilter,
        output_trait::SCStreamOutputTrait,
        output_type::SCStreamOutputType,
        SCStream,
    },
};

use crate::audio::capture::{CaptureHandle, CaptureInfo};

// ---------------------------------------------------------------------------
// AudioStreamBasicDescription (mirrored from <CoreAudioTypes/CoreAudioTypes.h>)
// ---------------------------------------------------------------------------
//
// `core-media-rs` does not (yet) expose CMAudioFormatDescriptionGetStreamBasic
// Description through its safe API, so we drop down to a raw FFI call. The
// struct layout is stable ABI and has not changed in years.
#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
#[allow(non_snake_case)]
struct AudioStreamBasicDescription {
    mSampleRate: f64,
    mFormatID: u32,
    mFormatFlags: u32,
    mBytesPerPacket: u32,
    mFramesPerPacket: u32,
    mBytesPerFrame: u32,
    mChannelsPerFrame: u32,
    mBitsPerChannel: u32,
    mReserved: u32,
}

const K_AUDIO_FORMAT_FLAG_IS_FLOAT: u32 = 1 << 0;
const K_AUDIO_FORMAT_FLAG_IS_NON_INTERLEAVED: u32 = 1 << 5;

extern "C" {
    fn CMAudioFormatDescriptionGetStreamBasicDescription(
        desc: CMFormatDescriptionRef,
    ) -> *const AudioStreamBasicDescription;
}

/// RAII handle that keeps an `SCStream` alive while the capture is running.
///
/// `Drop` calls `stop_capture()` — the `App` keeps this value on its owning
/// thread (same pattern as `cpal::Stream`) and dropping it halts capture.
///
/// NOT required to be `Send` — the App keeps it pinned to the main thread
/// the same way it does `cpal::Stream` today. The wrapped `SCStream` happens
/// to be `Send`, but we don't rely on that.
pub struct LoopbackKeepAlive {
    stream: Option<SCStream>,
}

impl Drop for LoopbackKeepAlive {
    fn drop(&mut self) {
        if let Some(s) = self.stream.take() {
            if let Err(e) = s.stop_capture() {
                log::warn!("loopback: stop_capture error: {:?}", e);
            }
            drop(s);
        }
    }
}

/// Output handler glued into `SCStream`. Converts each audio sample buffer to
/// interleaved `f32` and pushes it into the mpsc channel that feeds
/// `CaptureHandle.frames_rx`.
struct AudioOutput {
    tx: mpsc::Sender<Vec<f32>>,
    /// Observed format from the first audio sample buffer we saw. Written
    /// once under lock and read by `capture()` to fill `CaptureInfo`.
    format: Arc<Mutex<Option<ObservedFormat>>>,
}

#[derive(Clone, Copy)]
#[allow(dead_code)] // `sample_rate` is logged once and otherwise informational.
struct ObservedFormat {
    sample_rate: u32,
    channels: u16,
    is_float: bool,
    is_non_interleaved: bool,
    bits_per_channel: u32,
}

impl SCStreamOutputTrait for AudioOutput {
    fn did_output_sample_buffer(
        &self,
        sample_buffer: CMSampleBuffer,
        of_type: SCStreamOutputType,
    ) {
        if !matches!(of_type, SCStreamOutputType::Audio) {
            // Video frames from the 2x2 dummy config — drop them.
            return;
        }

        // Snapshot the format once. SCK normally delivers Float32 interleaved
        // at 48 kHz, but we query the actual ASBD to be safe.
        if self.format.lock().map(|g| g.is_none()).unwrap_or(false) {
            if let Ok(fd) = sample_buffer.get_format_description() {
                let ptr = fd.as_concrete_TypeRef();
                unsafe {
                    let asbd_ptr = CMAudioFormatDescriptionGetStreamBasicDescription(ptr);
                    if !asbd_ptr.is_null() {
                        let asbd = *asbd_ptr;
                        let observed = ObservedFormat {
                            sample_rate: asbd.mSampleRate as u32,
                            channels: asbd.mChannelsPerFrame as u16,
                            is_float: (asbd.mFormatFlags & K_AUDIO_FORMAT_FLAG_IS_FLOAT) != 0,
                            is_non_interleaved: (asbd.mFormatFlags
                                & K_AUDIO_FORMAT_FLAG_IS_NON_INTERLEAVED)
                                != 0,
                            bits_per_channel: asbd.mBitsPerChannel,
                        };
                        if let Ok(mut slot) = self.format.lock() {
                            *slot = Some(observed);
                        }
                        log::info!(
                            "loopback: audio format observed — rate={} ch={} float={} non_interleaved={} bits={}",
                            observed.sample_rate,
                            observed.channels,
                            observed.is_float,
                            observed.is_non_interleaved,
                            observed.bits_per_channel
                        );
                    }
                }
            }
        }

        let observed = match self.format.lock().ok().and_then(|g| *g) {
            Some(o) => o,
            None => return, // Haven't seen format yet; skip this frame.
        };

        // Decode the AudioBufferList into interleaved f32.
        let abl = match sample_buffer.get_audio_buffer_list() {
            Ok(a) => a,
            Err(e) => {
                log::warn!("loopback: get_audio_buffer_list: {:?}", e);
                return;
            }
        };

        let interleaved = match decode_buffer_list(&abl, &observed) {
            Some(v) => v,
            None => return,
        };

        // Non-blocking send — if the consumer is behind, drop the frame.
        // This matches the mic path's best-effort semantics.
        let _ = self.tx.try_send(interleaved);
    }
}

/// Convert an AudioBufferList into an interleaved `Vec<f32>` in channel order.
///
/// Handles the common SCK case: Float32, either interleaved (one buffer with
/// all channels) or non-interleaved (one buffer per channel). Falls back to
/// returning `None` for any unsupported format — the resampler would error
/// anyway on garbage data.
fn decode_buffer_list(
    abl: &core_audio_types_rs::audio_buffer_list::AudioBufferList,
    fmt: &ObservedFormat,
) -> Option<Vec<f32>> {
    if !fmt.is_float || fmt.bits_per_channel != 32 {
        // We only handle Float32 PCM for now. SCK documents Float32 as the
        // default; if we ever see something else, log once and bail.
        log::warn!(
            "loopback: unsupported audio format (float={} bits={}); dropping frame",
            fmt.is_float,
            fmt.bits_per_channel
        );
        return None;
    }

    let channels = fmt.channels.max(1) as usize;

    if !fmt.is_non_interleaved {
        // Interleaved: AudioBufferList has exactly one AudioBuffer whose
        // data is already in the format we want.
        let buf = abl.get(0)?;
        let bytes = buf.data();
        let sample_count = bytes.len() / 4;
        let mut out = Vec::<f32>::with_capacity(sample_count);
        // SAFETY: The buffer is Float32 samples, aligned on 16 bytes by the
        // flag we pass into CMSampleBufferGetAudioBufferListWithRetainedBlock
        // Buffer. Copy via f32 reads for correctness regardless of alignment.
        for chunk in bytes.chunks_exact(4) {
            let bits = u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            out.push(f32::from_bits(bits));
        }
        Some(out)
    } else {
        // Non-interleaved: `channels` buffers, each contains all frames for
        // its channel. Interleave in channel order.
        if abl.num_buffers() < channels {
            return None;
        }
        // Samples per channel (derive from the first buffer's byte size).
        let first = abl.get(0)?;
        let frames_per_channel = (first.data_bytes_size as usize) / 4;
        let mut out = vec![0.0_f32; frames_per_channel * channels];
        for ch in 0..channels {
            let buf = abl.get(ch)?;
            let bytes = buf.data();
            for (i, chunk) in bytes.chunks_exact(4).enumerate() {
                if i >= frames_per_channel {
                    break;
                }
                let bits = u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                out[i * channels + ch] = f32::from_bits(bits);
            }
        }
        Some(out)
    }
}

/// Start system-audio loopback capture.
///
/// `name` is logged for diagnostics but otherwise ignored — ScreenCaptureKit
/// captures system-wide, not per-output-device.
pub fn capture(name: Option<&str>) -> Result<CaptureHandle> {
    log::info!(
        "loopback: capturing system audio (requested device: {:?})",
        name
    );

    // --- 1. Content filter: any available display ------------------------
    let content = SCShareableContent::get()
        .map_err(|e| anyhow!("SCShareableContent::get failed: {:?} — if this is a permission error, grant 'Screen & System Audio Recording' in System Settings > Privacy & Security > Screen Recording and restart voice-bird", e))?;
    let mut displays = content.displays();
    if displays.is_empty() {
        return Err(anyhow!(
            "no displays available for ScreenCaptureKit (screen recording permission may be denied)"
        ));
    }
    let display = displays.remove(0);
    let filter = SCContentFilter::new().with_display_excluding_windows(&display, &[]);

    // --- 2. Stream configuration: audio on, video minimal ----------------
    // `core-foundation::CFError` doesn't impl `std::error::Error`, so we
    // can't use anyhow's `.context(..)` directly; format the debug repr.
    let config = SCStreamConfiguration::new()
        .set_captures_audio(true)
        .map_err(|e| anyhow!("set_captures_audio: {:?}", e))?
        .set_excludes_current_process_audio(true)
        .map_err(|e| anyhow!("set_excludes_current_process_audio: {:?}", e))?
        // Video is required but we don't consume it; 2x2 keeps overhead trivial.
        .set_width(2)
        .map_err(|e| anyhow!("set_width: {:?}", e))?
        .set_height(2)
        .map_err(|e| anyhow!("set_height: {:?}", e))?;

    // --- 3. Channel for f32 frames ---------------------------------------
    let (tx, rx) = mpsc::channel::<Vec<f32>>(64);
    let format_slot: Arc<Mutex<Option<ObservedFormat>>> = Arc::new(Mutex::new(None));

    // --- 4. Stream + audio output handler --------------------------------
    let mut stream = SCStream::new(&filter, &config);
    let audio_output = AudioOutput {
        tx,
        format: format_slot.clone(),
    };
    if stream
        .add_output_handler(audio_output, SCStreamOutputType::Audio)
        .is_none()
    {
        return Err(anyhow!(
            "failed to add audio output handler to SCStream"
        ));
    }

    // --- 5. Start capture ------------------------------------------------
    stream.start_capture().map_err(|e| {
        anyhow!(
            "screen recording permission denied — grant it in System Settings > Privacy & Security > Screen Recording, then restart voice-bird (underlying error: {:?})",
            e
        )
    })?;

    // SCK is asynchronous — we don't get an ASBD until the first sample
    // buffer lands. We'd rather not block `start_recording` on that, so we
    // return a conservative default (the SCK documented default: 48 kHz
    // stereo Float32) in `CaptureInfo`. The resampler handles whatever the
    // producer task actually ships; if SCK decides to deliver 44.1 kHz mono
    // (rare but possible), the samples would be resampled from a slightly
    // wrong rate. TODO: push the observed ASBD back into the resampler so
    // it retunes after the first frame, or block here with a short timeout.
    let info = CaptureInfo {
        sample_rate: 48_000,
        channels: 2,
    };

    Ok(CaptureHandle {
        frames_rx: rx,
        info,
        stream: crate::audio::capture::CaptureKeepAlive::Sck(LoopbackKeepAlive {
            stream: Some(stream),
        }),
    })
}
