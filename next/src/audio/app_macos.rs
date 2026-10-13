//! Per-application audio via objc2 ScreenCaptureKit (macOS 13+).
//! The selected output device is a UI label: SCK filters applications, not
//! hardware output routes, and never substitutes system-wide audio.

use std::marker::PhantomData;
use std::ptr::{self, NonNull};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{anyhow, Context};
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, AnyThread, DefinedClass};
use objc2_core_audio_types::{
    kAudioFormatFlagIsBigEndian, kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonInterleaved,
    kAudioFormatLinearPCM, AudioBuffer, AudioBufferList,
};
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::CGPreflightScreenCaptureAccess;
use objc2_core_media::{CMAudioFormatDescriptionGetStreamBasicDescription, CMSampleBuffer};
use objc2_foundation::{
    NSArray, NSError, NSObject, NSObjectProtocol, NSOperatingSystemVersion, NSProcessInfo, NSString,
};
use objc2_screen_capture_kit::{
    SCContentFilter, SCRunningApplication, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamDelegate, SCStreamOutput, SCStreamOutputType, SCWindow,
};

use super::native::{FrameSender, NativeFrames};
use super::{CaptureChunk, CaptureStream};
use crate::audio_sources::AppTarget;

const NATIVE_TIMEOUT: Duration = Duration::from_secs(5);

struct OutputIvars {
    sender: FrameSender,
}

define_class!(
    // NSObject has no additional subclassing requirements. Ivars are thread-safe
    // and callbacks can be called on SCK's dispatch queues, never on the UI.
    #[unsafe(super = NSObject)]
    #[thread_kind = AnyThread]
    #[ivars = OutputIvars]
    struct AudioOutput;

    unsafe impl NSObjectProtocol for AudioOutput {}

    unsafe impl SCStreamOutput for AudioOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn output(&self, _stream: &SCStream, sample: &CMSampleBuffer, kind: SCStreamOutputType) {
            if kind != SCStreamOutputType::Audio {
                return;
            }
            autoreleasepool(|_| match decode_sample(sample) {
                Ok(samples) if !samples.is_empty() => self.ivars().sender.send(samples),
                Ok(_) => {}
                Err(error) => self
                    .ivars()
                    .sender
                    .fail(format!("application audio decode failed: {error:#}")),
            });
        }
    }

    unsafe impl SCStreamDelegate for AudioOutput {
        #[unsafe(method(stream:didStopWithError:))]
        fn stopped(&self, _stream: &SCStream, error: &NSError) {
            self.ivars().sender.fail(format!(
                "application audio capture failed: {}",
                error.localizedDescription()
            ));
        }
    }
);

impl AudioOutput {
    fn new(sender: FrameSender) -> Retained<Self> {
        let this = Self::alloc().set_ivars(OutputIvars { sender });
        // The NSObject initializer has this exact signature.
        unsafe { msg_send![super(this), init] }
    }
}

// SCK produces an immutable snapshot on its completion queue. Transfer one
// retained reference to the worker; the callback never accesses it afterwards.
// SCShareableContent and its immutable arrays support off-main-thread reads.
struct ContentSnapshot(Retained<SCShareableContent>);
unsafe impl Send for ContentSnapshot {}

fn shareable_content() -> anyhow::Result<Retained<SCShareableContent>> {
    let (tx, rx) = mpsc::sync_channel(1);
    let completion = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            let result = if let Some(error) = unsafe { error.as_ref() } {
                Err(error.localizedDescription().to_string())
            } else {
                // Callback arguments are borrowed; retain before leaving the callback.
                unsafe { Retained::retain(content) }
                    .map(ContentSnapshot)
                    .ok_or_else(|| "ScreenCaptureKit returned no shareable content".to_string())
            };
            let _ = tx.try_send(result);
        },
    );
    unsafe {
        SCShareableContent::getShareableContentWithCompletionHandler(&completion);
    }
    rx.recv_timeout(NATIVE_TIMEOUT)
        .context("ScreenCaptureKit content query timed out")?
        .map(|snapshot| snapshot.0)
        .map_err(|error| anyhow!("ScreenCaptureKit content query failed: {error}"))
}

fn selected_app(
    content: &SCShareableContent,
    app: &AppTarget,
) -> anyhow::Result<Retained<SCRunningApplication>> {
    let apps = unsafe { content.applications() };
    let id = NSString::from_str(&app.id);
    let name = NSString::from_str(&app.name);
    // PID disambiguates multiple instances of the same bundle. Bundle identity
    // still permits a selected application to restart before capture opens.
    let target = apps
        .iter()
        .find(|candidate| unsafe {
            app.pid != 0
                && candidate.processID() as u32 == app.pid
                && (app.id.is_empty() || candidate.bundleIdentifier().isEqualToString(&id))
        })
        .or_else(|| {
            apps.iter().find(|candidate| unsafe {
                !app.id.is_empty() && candidate.bundleIdentifier().isEqualToString(&id)
            })
        })
        .or_else(|| {
            apps.iter().find(|candidate| unsafe {
                (app.id.is_empty() || candidate.bundleIdentifier().is_empty())
                    && candidate.applicationName().isEqualToString(&name)
            })
        });
    target.ok_or_else(|| {
        anyhow!(
            "application '{}' ({}) not found among running apps; refresh the source picker",
            app.name,
            app.id
        )
    })
}

pub(super) fn open(app: &AppTarget) -> anyhow::Result<Box<dyn CaptureStream>> {
    anyhow::ensure!(
        NSProcessInfo::processInfo().isOperatingSystemAtLeastVersion(NSOperatingSystemVersion {
            majorVersion: 13,
            minorVersion: 0,
            patchVersion: 0
        },),
        "application audio capture requires macOS 13 or newer"
    );
    if !CGPreflightScreenCaptureAccess() {
        anyhow::bail!("Screen Recording permission required: grant Voice Bird or its terminal access in System Settings > Privacy & Security > Screen Recording, then restart");
    }
    autoreleasepool(|_| {
        let content = shareable_content()?;
        let display = unsafe { content.displays() }
            .firstObject()
            .ok_or_else(|| anyhow!("no displays available for ScreenCaptureKit"))?;
        let app = selected_app(&content, app)?;
        let included = NSArray::from_slice(&[&*app]);
        let windows = NSArray::<SCWindow>::from_slice(&[]);
        let filter = unsafe {
            SCContentFilter::initWithDisplay_includingApplications_exceptingWindows(
                SCContentFilter::alloc(),
                &display,
                &included,
                &windows,
            )
        };
        let config = unsafe {
            let config = SCStreamConfiguration::new();
            config.setWidth(2);
            config.setHeight(2);
            config.setQueueDepth(3);
            config.setCapturesAudio(true);
            config.setExcludesCurrentProcessAudio(true);
            config.setSampleRate(48_000);
            config.setChannelCount(2);
            config
        };
        let (sender, frames) = NativeFrames::new(48_000, 2)?;
        let output = AudioOutput::new(sender);
        let queue = DispatchQueue::new("com.voice-bird.next.application-audio", None);
        let stream = unsafe {
            let stream = SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &config,
                Some(ProtocolObject::from_ref(&*output)),
            );
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    ProtocolObject::from_ref(&*output),
                    SCStreamOutputType::Audio,
                    Some(&queue),
                )
                .map_err(|error| {
                    anyhow!(
                        "add ScreenCaptureKit audio output: {}",
                        error.localizedDescription()
                    )
                })?;
            stream
        };
        // Keepalive exists before startup so an error still stops native capture.
        let capture = AppStream {
            stream,
            _output: output,
            queue,
            frames,
            stopped: false,
            _worker_only: PhantomData,
        };
        completion_result(|completion| unsafe {
            capture
                .stream
                .startCaptureWithCompletionHandler(Some(completion));
        })
        .context("start application audio capture")?;
        Ok(Box::new(capture) as Box<dyn CaptureStream>)
    })
}

fn completion_result(
    start: impl FnOnce(&block2::DynBlock<dyn Fn(*mut NSError)>),
) -> anyhow::Result<()> {
    let (tx, rx) = mpsc::sync_channel(1);
    let completion = RcBlock::new(move |error: *mut NSError| {
        let result = unsafe { error.as_ref() }.map_or(Ok(()), |error| {
            Err(error.localizedDescription().to_string())
        });
        let _ = tx.try_send(result);
    });
    start(&completion);
    rx.recv_timeout(NATIVE_TIMEOUT)
        .context("ScreenCaptureKit operation timed out")?
        .map_err(|error| anyhow!("{error}"))
}

struct AppStream {
    stream: Retained<SCStream>,
    _output: Retained<AudioOutput>,
    queue: DispatchRetained<DispatchQueue>,
    frames: NativeFrames,
    stopped: bool,
    _worker_only: PhantomData<Rc<()>>,
}

impl CaptureStream for AppStream {
    fn next_chunk(&mut self, timeout: Duration) -> anyhow::Result<CaptureChunk> {
        self.frames.next_chunk(timeout)
    }

    fn queued_samples(&self) -> u64 {
        self.frames.queued_samples()
    }

    fn finish(&mut self) -> anyhow::Result<Vec<f32>> {
        if !self.stopped {
            completion_result(|completion| unsafe {
                self.stream
                    .stopCaptureWithCompletionHandler(Some(completion));
            })
            .context("stop application audio capture")?;
            self.stopped = true;
            // Drain already scheduled callbacks before draining queued PCM.
            self.queue.exec_sync(|| {});
        }
        self.frames.finish()
    }
}

impl Drop for AppStream {
    fn drop(&mut self) {
        if !self.stopped {
            unsafe {
                self.stream.stopCaptureWithCompletionHandler(None);
            }
        }
    }
}

// SCK is configured to at most two buffers: one stereo interleaved buffer or
// two mono planes. This struct has AudioBufferList's ABI plus one extra slot.
#[repr(C)]
struct StereoBufferList {
    count: u32,
    buffers: [AudioBuffer; 2],
}

fn decode_sample(sample: &CMSampleBuffer) -> anyhow::Result<Vec<f32>> {
    unsafe {
        if sample.num_samples() == 0 {
            return Ok(Vec::new());
        }
        anyhow::ensure!(
            sample.is_valid() && sample.data_is_ready(),
            "invalid or unready audio sample buffer"
        );
        let description = sample
            .format_description()
            .ok_or_else(|| anyhow!("audio format description missing"))?;
        let format = CMAudioFormatDescriptionGetStreamBasicDescription(&description)
            .as_ref()
            .ok_or_else(|| anyhow!("audio stream description missing"))?;
        anyhow::ensure!(
            format.mFormatID == kAudioFormatLinearPCM
                && format.mFormatFlags & kAudioFormatFlagIsFloat != 0
                && format.mBitsPerChannel == 32
                && format.mSampleRate == 48_000.0
                && format.mChannelsPerFrame == 2,
            "unsupported ScreenCaptureKit audio format: {format:?}"
        );
        let planar = format.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0;
        anyhow::ensure!(
            format.mBytesPerFrame == if planar { 4 } else { 8 },
            "unsupported PCM frame layout"
        );
        let empty = AudioBuffer {
            mNumberChannels: 0,
            mDataByteSize: 0,
            mData: ptr::null_mut(),
        };
        let mut list = StereoBufferList {
            count: 0,
            buffers: [empty; 2],
        };
        let mut needed = 0;
        let mut block = ptr::null_mut();
        let status = sample.audio_buffer_list_with_retained_block_buffer(
            &mut needed,
            (&mut list as *mut StereoBufferList).cast::<AudioBufferList>(),
            std::mem::size_of::<StereoBufferList>(),
            None,
            None,
            0,
            &mut block,
        );
        // The API returns a +1 CMBlockBuffer. It must outlive all borrowed PCM
        // byte slices, and also be released when decode/validation fails.
        let _retained = NonNull::new(block).map(|block| CFRetained::from_raw(block));
        anyhow::ensure!(
            status == 0,
            "read audio buffer list failed (OSStatus {status})"
        );
        let expected = if planar { 2 } else { 1 };
        anyhow::ensure!(
            list.count == expected && needed <= std::mem::size_of::<StereoBufferList>(),
            "invalid audio buffer list size"
        );
        let mut bytes: [&[u8]; 2] = [&[]; 2];
        for (index, buffer) in list.buffers[..expected as usize].iter().enumerate() {
            anyhow::ensure!(
                buffer.mNumberChannels == if planar { 1 } else { 2 },
                "invalid audio buffer channel count"
            );
            anyhow::ensure!(
                buffer.mDataByteSize as usize <= 48_000 * 2 * 4,
                "audio buffer exceeds one second"
            );
            if buffer.mDataByteSize != 0 {
                anyhow::ensure!(!buffer.mData.is_null(), "missing audio buffer data");
                bytes[index] = std::slice::from_raw_parts(
                    buffer.mData.cast::<u8>(),
                    buffer.mDataByteSize as usize,
                );
            }
        }
        decode_buffers(
            &bytes[..expected as usize],
            planar,
            format.mFormatFlags & kAudioFormatFlagIsBigEndian != 0,
        )
    }
}

fn decode_buffers(buffers: &[&[u8]], planar: bool, big_endian: bool) -> anyhow::Result<Vec<f32>> {
    let sample = |bytes: &[u8]| {
        let word = [bytes[0], bytes[1], bytes[2], bytes[3]];
        if big_endian {
            f32::from_be_bytes(word)
        } else {
            f32::from_le_bytes(word)
        }
    };
    if planar {
        anyhow::ensure!(
            buffers.len() == 2
                && buffers[0].len() == buffers[1].len()
                && buffers[0].len().is_multiple_of(4),
            "incomplete planar PCM frame"
        );
        let mut out = Vec::with_capacity(buffers[0].len() / 2);
        for (left, right) in buffers[0].chunks_exact(4).zip(buffers[1].chunks_exact(4)) {
            out.push(sample(left));
            out.push(sample(right));
        }
        Ok(out)
    } else {
        anyhow::ensure!(
            buffers.len() == 1 && buffers[0].len().is_multiple_of(8),
            "incomplete stereo PCM frame"
        );
        Ok(buffers[0].chunks_exact(4).map(sample).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm_decode_preserves_interleaved_and_planar_channel_order() {
        let left: Vec<u8> = [1.0_f32, 0.5]
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect();
        let right: Vec<u8> = [-1.0_f32, -0.5]
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect();
        assert_eq!(
            decode_buffers(&[&left, &right], true, false).unwrap(),
            vec![1.0, -1.0, 0.5, -0.5]
        );
        let interleaved: Vec<u8> = [1.0_f32, -1.0, 0.5, -0.5]
            .into_iter()
            .flat_map(f32::to_be_bytes)
            .collect();
        assert_eq!(
            decode_buffers(&[&interleaved], false, true).unwrap(),
            vec![1.0, -1.0, 0.5, -0.5]
        );
    }

    #[test]
    fn pcm_decode_rejects_mismatched_planes_and_partial_frames() {
        assert!(decode_buffers(&[&[0; 8], &[0; 4]], true, false).is_err());
        assert!(decode_buffers(&[&[0; 6]], false, false).is_err());
        assert!(decode_buffers(&[&[0; 6], &[0; 6]], true, false).is_err());
    }
}
