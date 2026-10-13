//! Immutable PCM slices shared by capture and independently scheduled passes.
use std::collections::VecDeque;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;

const MAX_SAMPLES: u64 = 30 * 16_000;

#[derive(Clone, Debug)]
pub struct PcmSlice {
    pub pcm: Arc<Vec<f32>>,
    pub range: Range<usize>,
}

impl PcmSlice {
    pub fn samples(&self) -> &[f32] {
        &self.pcm[self.range.clone()]
    }
}

struct Chunk {
    start: u64,
    pcm: Arc<Vec<f32>>,
}

struct State {
    chunks: VecDeque<Chunk>,
    cursors: [u64; 2],
    end: u64,
    origin: Option<Instant>,
    finished: bool,
    error: Option<String>,
}

/// Two consumers retain audio independently; reclaim only after both have read it.
pub struct CaptureBuffer {
    state: Mutex<State>,
    pub captured: Arc<AtomicU64>,
}

pub struct PendingAudio {
    pub origin: Option<Instant>,
    pub start: u64,
    pub end: u64,
    pub finished: bool,
}

impl Default for CaptureBuffer {
    fn default() -> Self {
        Self {
            state: Mutex::new(State {
                chunks: VecDeque::new(),
                cursors: [0; 2],
                end: 0,
                origin: None,
                finished: false,
                error: None,
            }),
            captured: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl CaptureBuffer {
    pub fn opened(&self) {
        self.state.lock().origin = Some(Instant::now());
    }

    pub fn append(&self, pcm: Vec<f32>) -> anyhow::Result<()> {
        if pcm.is_empty() {
            return Ok(());
        }
        let mut state = self.state.lock();
        let count = pcm.len() as u64;
        let retained = state.end - *state.cursors.iter().min().unwrap();
        anyhow::ensure!(
            retained + count <= MAX_SAMPLES,
            "audio transcription buffer overrun: inference could not keep up"
        );
        let start = state.end;
        state.chunks.push_back(Chunk {
            start,
            pcm: Arc::new(pcm),
        });
        state.end += count;
        self.captured.store(state.end, Ordering::Release);
        Ok(())
    }

    pub fn finish(&self, error: Option<String>) {
        let mut state = self.state.lock();
        state.finished = true;
        state.error = error;
    }

    pub fn pending(&self, pass: usize) -> PendingAudio {
        let state = self.state.lock();
        PendingAudio {
            origin: state.origin,
            start: state.cursors[pass],
            end: state.end,
            finished: state.finished,
        }
    }

    pub fn take_error(&self) -> Option<String> {
        self.state.lock().error.take()
    }

    pub fn discard(&self, pass: usize) {
        let mut state = self.state.lock();
        state.cursors[pass] = state.end;
        reclaim(&mut state);
    }

    pub fn take(&self, pass: usize, end: u64) -> Vec<PcmSlice> {
        let mut state = self.state.lock();
        let start = state.cursors[pass];
        assert!(start <= end && end <= state.end);
        let mut slices = Vec::with_capacity(state.chunks.len());
        for chunk in &state.chunks {
            let chunk_end = chunk.start + chunk.pcm.len() as u64;
            if chunk_end <= start {
                continue;
            }
            if chunk.start >= end {
                break;
            }
            slices.push(PcmSlice {
                pcm: chunk.pcm.clone(),
                range: (start.saturating_sub(chunk.start) as usize)
                    ..((end.min(chunk_end) - chunk.start) as usize),
            });
        }
        state.cursors[pass] = end;
        reclaim(&mut state);
        slices
    }
}

fn reclaim(state: &mut State) {
    let consumed = *state.cursors.iter().min().unwrap();
    while state
        .chunks
        .front()
        .is_some_and(|chunk| chunk.start + chunk.pcm.len() as u64 <= consumed)
    {
        state.chunks.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples(slices: &[PcmSlice]) -> Vec<f32> {
        slices
            .iter()
            .flat_map(|slice| slice.samples().iter().copied())
            .collect()
    }

    #[test]
    fn independent_cursors_preserve_order_and_split_frames_without_copying_pcm() {
        let buffer = CaptureBuffer::default();
        buffer.append(vec![1.0, 2.0, 3.0]).unwrap();
        buffer.append(vec![4.0, 5.0]).unwrap();
        let live = buffer.take(0, 5);
        let first_refine = buffer.take(1, 2);
        assert_eq!(samples(&live), [1.0, 2.0, 3.0, 4.0, 5.0]);
        assert_eq!(samples(&first_refine), [1.0, 2.0]);
        assert!(Arc::ptr_eq(&live[0].pcm, &first_refine[0].pcm));
        assert_eq!(samples(&buffer.take(1, 5)), [3.0, 4.0, 5.0]);
        buffer.append(vec![6.0]).unwrap();
        assert_eq!(samples(&buffer.take(0, 6)), [6.0]);
        assert_eq!(samples(&buffer.take(1, 6)), [6.0]);
    }

    #[test]
    fn slowest_pass_bounds_retention_and_releases_capacity_after_catchup() {
        let buffer = CaptureBuffer::default();
        buffer.append(vec![0.0; MAX_SAMPLES as usize]).unwrap();
        buffer.take(0, MAX_SAMPLES);
        assert!(buffer.append(vec![1.0]).is_err());
        buffer.take(1, MAX_SAMPLES);
        buffer.append(vec![1.0]).unwrap();
        assert_eq!(buffer.pending(1).end, MAX_SAMPLES + 1);
    }
}
