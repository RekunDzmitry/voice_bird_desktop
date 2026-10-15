//! Bounded, nonblocking callback bridge. Overflow is an explicit recording
//! failure instead of silent audio loss or an unbounded inference backlog.

use parking_lot::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use super::{resample::Resampler, CaptureChunk};

// Callback sizes vary by device. Bound PCM duration, not just callback count,
// so normal live/refine inference latency can span many small callbacks.
const QUEUE_SECONDS: usize = 30;
const QUEUE_CHUNKS: usize = 8192;

#[derive(Clone)]
pub(super) struct FrameSender {
    tx: mpsc::SyncSender<Vec<f32>>,
    error: Arc<Mutex<Option<String>>>,
    max_samples: usize,
    queued_samples: Arc<AtomicUsize>,
    max_queued_samples: usize,
}

impl FrameSender {
    pub fn fail(&self, error: impl Into<String>) {
        let mut slot = self.error.lock();
        if slot.is_none() {
            *slot = Some(error.into());
        }
    }

    pub fn send(&self, samples: Vec<f32>) {
        if samples.len() > self.max_samples {
            self.fail("native audio callback exceeded one second of audio");
            return;
        }
        let count = samples.len();
        if count == 0 {
            return;
        }
        if self
            .queued_samples
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |queued| {
                queued
                    .checked_add(count)
                    .filter(|total| *total <= self.max_queued_samples)
            })
            .is_err()
        {
            self.fail("audio capture queue overrun: transcription could not keep up");
            return;
        }
        match self.tx.try_send(samples) {
            Ok(()) => {}
            Err(error) => {
                self.queued_samples.fetch_sub(count, Ordering::AcqRel);
                if matches!(error, mpsc::TrySendError::Full(_)) {
                    self.fail("audio capture queue overrun: transcription could not keep up");
                }
            }
        }
    }
}

pub(super) struct NativeFrames {
    rx: mpsc::Receiver<Vec<f32>>,
    error: Arc<Mutex<Option<String>>>,
    queued_samples: Arc<AtomicUsize>,
    normalization_divisor: u64,
    resampler: Resampler,
    finished: bool,
}

impl NativeFrames {
    pub fn new(sample_rate: u32, channels: u16) -> anyhow::Result<(FrameSender, Self)> {
        let resampler = Resampler::new(sample_rate, channels)?;
        let (tx, rx) = mpsc::sync_channel(QUEUE_CHUNKS);
        let error = Arc::new(Mutex::new(None));
        let queued_samples = Arc::new(AtomicUsize::new(0));
        let max_samples = sample_rate as usize * usize::from(channels);
        let sender = FrameSender {
            tx,
            error: error.clone(),
            max_samples,
            queued_samples: queued_samples.clone(),
            max_queued_samples: max_samples * QUEUE_SECONDS,
        };
        Ok((
            sender,
            Self {
                rx,
                error,
                queued_samples,
                normalization_divisor: u64::from(sample_rate) * u64::from(channels),
                resampler,
                finished: false,
            },
        ))
    }

    pub fn queued_samples(&self) -> u64 {
        let queued = self.queued_samples.load(Ordering::Acquire) as u128;
        (queued * 16_000 / u128::from(self.normalization_divisor)) as u64
    }

    fn check_error(&self) -> anyhow::Result<()> {
        let error = self.error.lock();
        if let Some(error) = error.as_ref() {
            anyhow::bail!("{error}");
        }
        Ok(())
    }

    pub fn next_chunk(&mut self, timeout: Duration) -> anyhow::Result<CaptureChunk> {
        self.check_error()?;
        if self.finished {
            return Ok(CaptureChunk::End);
        }
        let start = Instant::now();
        loop {
            match self
                .rx
                .recv_timeout(timeout.saturating_sub(start.elapsed()))
            {
                Ok(samples) => {
                    self.queued_samples
                        .fetch_sub(samples.len(), Ordering::AcqRel);
                    self.check_error()?;
                    let pcm = self.resampler.process(&samples)?;
                    if !pcm.is_empty() {
                        return Ok(CaptureChunk::Data(pcm));
                    }
                    if start.elapsed() >= timeout {
                        return Ok(CaptureChunk::Timeout);
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    self.check_error()?;
                    return Ok(CaptureChunk::Timeout);
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.check_error()?;
                    let tail = self.finish()?;
                    return Ok(if tail.is_empty() {
                        CaptureChunk::End
                    } else {
                        CaptureChunk::Data(tail)
                    });
                }
            }
        }
    }

    /// The caller first stops the native stream so this queue cannot refill.
    pub fn finish(&mut self) -> anyhow::Result<Vec<f32>> {
        self.check_error()?;
        if self.finished {
            return Ok(Vec::new());
        }
        self.finished = true;
        let mut out = Vec::new();
        while let Ok(samples) = self.rx.try_recv() {
            self.queued_samples
                .fetch_sub(samples.len(), Ordering::AcqRel);
            out.extend(self.resampler.process(&samples)?);
        }
        out.extend(self.resampler.finish()?);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_snapshot_normalizes_without_consuming_and_excludes_dequeued_audio() {
        let (tx, mut stream) = NativeFrames::new(48_000, 2).unwrap();
        tx.send(vec![0.5; 9_600]);
        tx.send(vec![0.5; 4_800]);
        assert_eq!(stream.queued_samples(), 2_400);
        assert_eq!(stream.queued_samples(), 2_400);

        let CaptureChunk::Data(first) = stream.next_chunk(Duration::ZERO).unwrap() else {
            panic!("missing first queued chunk");
        };
        assert_eq!(stream.queued_samples(), 800);
        drop(tx);
        let tail = stream.finish().unwrap();
        assert_eq!(first.len() + tail.len(), 2_400);
        assert_eq!(stream.queued_samples(), 0);
        assert!(stream.finish().unwrap().is_empty());
        assert_eq!(stream.queued_samples(), 0);
    }

    #[test]
    fn timeout_and_end_are_distinct_and_end_flushes() {
        let (tx, mut stream) = NativeFrames::new(48_000, 1).unwrap();
        assert_eq!(
            stream.next_chunk(Duration::ZERO).unwrap(),
            CaptureChunk::Timeout
        );
        tx.send(vec![1.0; 300]);
        drop(tx);
        let CaptureChunk::Data(tail) = stream.next_chunk(Duration::from_millis(1)).unwrap() else {
            panic!("missing flushed tail")
        };
        assert_eq!(tail.len(), 100);
        assert_eq!(
            stream.next_chunk(Duration::ZERO).unwrap(),
            CaptureChunk::End
        );
    }

    #[test]
    fn callback_error_propagates_even_when_queue_is_full() {
        let (tx, mut stream) = NativeFrames::new(16_000, 1).unwrap();
        for _ in 0..QUEUE_SECONDS {
            tx.send(vec![0.0; 16_000]);
        }
        tx.fail("microphone disconnected");
        assert_eq!(
            stream.next_chunk(Duration::ZERO).unwrap_err().to_string(),
            "microphone disconnected"
        );
    }

    #[test]
    fn queue_overflow_fails_instead_of_dropping_audio_silently() {
        let (tx, mut stream) = NativeFrames::new(16_000, 1).unwrap();
        for _ in 0..=QUEUE_SECONDS {
            tx.send(vec![0.0; 16_000]);
        }
        assert!(stream
            .next_chunk(Duration::ZERO)
            .unwrap_err()
            .to_string()
            .contains("queue overrun"));
    }

    #[test]
    fn small_callbacks_survive_inference_latency_and_release_their_budget() {
        let (tx, mut stream) = NativeFrames::new(16_000, 1).unwrap();
        for index in 0..200 {
            tx.send(vec![index as f32; 160]);
        }
        let mut pcm = Vec::new();
        for _ in 0..200 {
            let CaptureChunk::Data(chunk) = stream.next_chunk(Duration::ZERO).unwrap() else {
                panic!("missing queued audio");
            };
            pcm.extend(chunk);
        }
        for (index, chunk) in pcm.chunks_exact(160).enumerate() {
            assert!(chunk.iter().all(|sample| *sample == index as f32));
        }
        for _ in 0..QUEUE_SECONDS {
            tx.send(vec![0.5; 16_000]);
        }
        drop(tx);
        assert_eq!(stream.finish().unwrap(), vec![0.5; QUEUE_SECONDS * 16_000]);
    }
}
