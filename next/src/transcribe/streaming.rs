use crate::audio::buffer::PcmSlice;

use super::local_agreement::{pending, segments, step, Token};
use super::{
    Transcriber, TranscriptOut, LIVE_HOP_SAMPLES as HOP, REFINE_WINDOW_SAMPLES as REFINE_WINDOW,
};

const RATE: usize = 16_000;
const LIVE_CAP: usize = RATE * 30;
const LIVE_OVERLAP: usize = RATE / 5;
const REFINE_OVERLAP: usize = RATE;

pub(super) trait Decoder: Send {
    /// Decode PCM into tokens with absolute session timestamps.
    fn decode(&mut self, pcm: &[f32], offset_ms: u64) -> anyhow::Result<Vec<Token>>;
}

fn milliseconds(samples: u64) -> u64 {
    samples * 1000 / RATE as u64
}

pub(super) struct Live<D> {
    decoder: D,
    buffer: Vec<f32>,
    offset: u64,
    received: u64,
    next_run: u64,
    last_run: u64,
    previous: Vec<Token>,
    committed_upto: u64,
    tentative: String,
    finished: bool,
}

impl<D: Decoder> Live<D> {
    pub fn new(decoder: D) -> Self {
        Self {
            decoder,
            buffer: Vec::with_capacity(LIVE_CAP),
            offset: 0,
            received: 0,
            next_run: RATE as u64,
            last_run: 0,
            previous: Vec::new(),
            committed_upto: 0,
            tentative: String::new(),
            finished: false,
        }
    }

    fn trim(&mut self, count: usize) {
        self.buffer.drain(..count);
        self.offset += count as u64;
    }

    fn run(&mut self, output: &mut Vec<TranscriptOut>) -> anyhow::Result<()> {
        let current = self
            .decoder
            .decode(&self.buffer, milliseconds(self.offset))?;
        // Keep committed boundary tokens for timestamp-drift deduplication
        // even after their audio has been trimmed from the retained context.
        let agreement = step(&self.previous, &current, self.committed_upto);
        self.committed_upto = agreement.committed_upto;
        output.extend(agreement.committed);
        self.publish_tentative(agreement.tentative, output);
        self.previous = current;
        self.last_run = self.received;

        // Retain a little already-committed context, preserving absolute times
        // in previous hypotheses instead of resetting agreement after a trim.
        let keep_from = self.committed_upto * RATE as u64 / 1000;
        let keep_from = keep_from.saturating_sub(LIVE_OVERLAP as u64);
        let cut = keep_from
            .saturating_sub(self.offset)
            .min(self.buffer.len() as u64);
        self.trim(cut as usize);
        Ok(())
    }

    fn publish_tentative(&mut self, text: String, output: &mut Vec<TranscriptOut>) {
        if text != self.tentative {
            self.tentative = text.clone();
            output.push(TranscriptOut::Tentative { text });
        }
    }
}

impl<D: Decoder> Transcriber for Live<D> {
    fn feed(&mut self, mut pcm: &[f32]) -> anyhow::Result<Vec<TranscriptOut>> {
        anyhow::ensure!(!self.finished, "transcriber already finished");
        let mut output = Vec::new();
        while !pcm.is_empty() {
            // Audio-time scheduling also works when capture delivers a backlog
            // in one chunk: no input is skipped and inference never exceeds 30 s.
            let take = (self.next_run - self.received).min(pcm.len() as u64) as usize;
            let excess = (self.buffer.len() + take).saturating_sub(LIVE_CAP);
            if excess > 0 {
                self.trim(excess);
            }
            self.buffer.extend_from_slice(&pcm[..take]);
            self.received += take as u64;
            pcm = &pcm[take..];
            if self.received == self.next_run {
                self.run(&mut output)?;
                self.next_run += HOP as u64;
            }
        }
        Ok(output)
    }

    fn feed_chunks(&mut self, chunks: &[PcmSlice]) -> anyhow::Result<Vec<TranscriptOut>> {
        anyhow::ensure!(!self.finished, "transcriber already finished");
        for chunk in chunks {
            let pcm = chunk.samples();
            // Retain the newest 30 seconds, including *all* newly consumed
            // audio before decoding. Unlike feed(), a job never replays every
            // overdue hop when inference was slower than capture.
            let excess = (self.buffer.len() + pcm.len()).saturating_sub(LIVE_CAP);
            let cut = excess.min(self.buffer.len());
            self.trim(cut);
            let skipped = excess - cut;
            self.offset += skipped as u64;
            self.buffer.extend_from_slice(&pcm[skipped..]);
            self.received += pcm.len() as u64;
        }
        let mut output = Vec::new();
        if self.received >= self.next_run {
            // Advance before inference so an ordinary decode error cannot
            // make the next job revisit an obsolete audio-time deadline.
            self.next_run += ((self.received - self.next_run) / HOP as u64 + 1) * HOP as u64;
            self.run(&mut output)?;
        }
        Ok(output)
    }

    fn finish(&mut self) -> anyhow::Result<Vec<TranscriptOut>> {
        if self.finished {
            return Ok(Vec::new());
        }
        let hypothesis = if !self.buffer.is_empty() && self.received != self.last_run {
            self.decoder
                .decode(&self.buffer, milliseconds(self.offset))?
        } else {
            std::mem::take(&mut self.previous)
        };
        let mut output = segments(pending(&self.previous, &hypothesis, self.committed_upto));
        self.publish_tentative(String::new(), &mut output);
        self.finished = true;
        self.buffer.clear();
        self.previous.clear();
        Ok(output)
    }
}

pub(super) struct Refine<D> {
    decoder: D,
    buffer: Vec<f32>,
    offset: u64,
    owned_start: u64,
    committed_upto: u64,
    previous: Vec<Token>,
    finished: bool,
}

impl<D: Decoder> Refine<D> {
    pub fn new(decoder: D) -> Self {
        Self {
            decoder,
            buffer: Vec::with_capacity(REFINE_WINDOW + REFINE_OVERLAP),
            offset: 0,
            owned_start: 0,
            committed_upto: 0,
            previous: Vec::new(),
            finished: false,
        }
    }

    fn run(&mut self, output: &mut Vec<TranscriptOut>) -> anyhow::Result<()> {
        let tokens = self
            .decoder
            .decode(&self.buffer, milliseconds(self.offset))?;
        // A one-second *backward* overlap supplies context without assigning
        // its text to both adjacent windows. Crossing words belong to the pass
        // in which they start; the end watermark rejects shifted repeats.
        let cutoff = milliseconds(self.owned_start).max(self.committed_upto);
        let remaining = pending(&self.previous, &tokens, cutoff);
        self.committed_upto = remaining
            .iter()
            .map(|t| t.t_end_ms)
            .max()
            .unwrap_or(self.committed_upto)
            .max(self.committed_upto);
        output.extend(segments(remaining));
        self.previous = tokens;
        let keep_from = milliseconds(self.offset + self.buffer.len() as u64).saturating_sub(1000);
        self.previous.retain(|token| token.t_end_ms >= keep_from);
        Ok(())
    }

    fn consume(&mut self, mut pcm: &[f32], output: &mut Vec<TranscriptOut>) -> anyhow::Result<()> {
        while !pcm.is_empty() {
            let context = (self.owned_start - self.offset) as usize;
            let required = context + REFINE_WINDOW;
            let take = (required - self.buffer.len()).min(pcm.len());
            self.buffer.extend_from_slice(&pcm[..take]);
            pcm = &pcm[take..];
            if self.buffer.len() == required {
                self.run(output)?;
                self.owned_start += REFINE_WINDOW as u64;
                let cut = self.buffer.len() - REFINE_OVERLAP;
                self.buffer.drain(..cut);
                self.offset += cut as u64;
            }
        }
        Ok(())
    }
}

impl<D: Decoder> Transcriber for Refine<D> {
    fn feed(&mut self, pcm: &[f32]) -> anyhow::Result<Vec<TranscriptOut>> {
        anyhow::ensure!(!self.finished, "transcriber already finished");
        let mut output = Vec::new();
        self.consume(pcm, &mut output)?;
        Ok(output)
    }

    fn feed_chunks(&mut self, chunks: &[PcmSlice]) -> anyhow::Result<Vec<TranscriptOut>> {
        anyhow::ensure!(!self.finished, "transcriber already finished");
        let mut output = Vec::new();
        for chunk in chunks {
            self.consume(chunk.samples(), &mut output)?;
        }
        Ok(output)
    }

    fn finish(&mut self) -> anyhow::Result<Vec<TranscriptOut>> {
        if self.finished {
            return Ok(Vec::new());
        }
        let mut output = Vec::new();
        if self.offset + self.buffer.len() as u64 > self.owned_start {
            self.run(&mut output)?;
        }
        self.finished = true;
        self.buffer.clear();
        self.previous.clear();
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::collections::VecDeque;
    use std::sync::Arc;

    type Calls = Arc<Mutex<Vec<(usize, u64)>>>;

    struct Script {
        results: VecDeque<anyhow::Result<Vec<Token>>>,
        calls: Calls,
    }

    impl Decoder for Script {
        fn decode(&mut self, pcm: &[f32], offset: u64) -> anyhow::Result<Vec<Token>> {
            self.calls.lock().push((pcm.len(), offset));
            self.results.pop_front().unwrap_or(Ok(Vec::new()))
        }
    }

    fn script(results: Vec<Vec<Token>>) -> (Script, Calls) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        (
            Script {
                results: results.into_iter().map(Ok).collect(),
                calls: calls.clone(),
            },
            calls,
        )
    }

    fn token(text: &str, start: u64, end: u64) -> Token {
        Token {
            text: text.into(),
            t_start_ms: start,
            t_end_ms: end,
        }
    }

    fn committed(output: &[TranscriptOut]) -> Vec<&str> {
        output
            .iter()
            .filter_map(|o| match o {
                TranscriptOut::Committed { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn live_finish_commits_only_tail_after_trim_and_is_idempotent() {
        let old = token(" hello", 0, 600);
        let tail = token(" world", 800, 1100);
        let (decoder, calls) = script(vec![
            vec![old.clone()],
            vec![old.clone(), tail.clone()],
            vec![old, tail],
        ]);
        let mut live = Live::new(decoder);
        assert_eq!(
            live.feed(&vec![0.0; RATE]).unwrap(),
            vec![TranscriptOut::Tentative {
                text: "hello".into()
            }]
        );
        let stable = live.feed(&vec![0.0; HOP]).unwrap();
        assert_eq!(committed(&stable), ["hello"]);
        live.feed(&[0.0; 100]).unwrap();
        let tail = live.finish().unwrap();
        assert_eq!(committed(&tail), ["world"]);
        assert!(live.finish().unwrap().is_empty());
        assert!(live.feed(&[0.0]).is_err());
        assert_eq!(calls.lock()[2].1, 400);
    }

    #[test]
    fn trimmed_boundary_context_allows_live_agreement_to_advance() {
        let old = token(" old", 400, 650);
        let (decoder, _) = script(vec![
            vec![old.clone(), token(" draft", 800, 1000)],
            vec![old, token(" new", 800, 1000)],
            vec![token(" old", 650, 750), token(" new", 850, 1050)],
        ]);
        let mut live = Live::new(decoder);
        live.feed(&vec![0.0; RATE]).unwrap();
        assert_eq!(committed(&live.feed(&vec![0.0; HOP]).unwrap()), ["old"]);
        assert_eq!(committed(&live.feed(&vec![0.0; HOP]).unwrap()), ["new"]);
        assert!(committed(&live.finish().unwrap()).is_empty());
    }

    #[test]
    fn live_hops_and_cap_keep_absolute_time_for_large_chunks() {
        let (decoder, calls) = script(Vec::new());
        let mut live = Live::new(decoder);
        live.feed(&vec![0.0; RATE - 1]).unwrap();
        assert!(calls.lock().is_empty());
        live.feed(&vec![0.0; RATE * 34 + 1]).unwrap();
        let calls = calls.lock();
        assert_eq!(calls[0], (RATE, 0));
        assert_eq!(calls[1], (RATE + HOP, 0));
        assert!(calls.iter().all(|(size, _)| *size <= LIVE_CAP));
        assert_eq!(calls.last().unwrap(), &(LIVE_CAP, 4750));
    }

    #[test]
    fn finish_flushes_short_utterances_without_waiting_for_minimum() {
        let (decoder, calls) = script(vec![vec![token(" brief", 0, 400)]]);
        let mut live = Live::new(decoder);
        assert!(live.feed(&vec![0.0; RATE / 2]).unwrap().is_empty());
        assert_eq!(committed(&live.finish().unwrap()), ["brief"]);
        assert_eq!(*calls.lock(), [(RATE / 2, 0)]);
    }

    #[test]
    fn refine_overlap_owns_each_word_once_and_flushes_subsecond_tail() {
        let crossing = token(" boundary", 4700, 5200);
        let (decoder, calls) = script(vec![
            vec![token(" first", 0, 1000), crossing],
            vec![token(" boundary", 4800, 5300), token(" second", 5500, 6200)],
            vec![token(" second", 5500, 6200), token(" tail", 10100, 10300)],
        ]);
        let mut refine = Refine::new(decoder);
        assert_eq!(
            committed(&refine.feed(&vec![0.0; RATE * 10]).unwrap()),
            ["first", "boundary", "second"]
        );
        refine.feed(&vec![0.0; RATE / 2]).unwrap();
        assert_eq!(committed(&refine.finish().unwrap()), ["tail"]);
        assert_eq!(
            *calls.lock(),
            [(RATE * 5, 0), (RATE * 6, 4000), (RATE * 3 / 2, 9000)]
        );
        assert!(refine.finish().unwrap().is_empty());
    }

    #[test]
    fn refine_full_window_does_not_decode_overlap_only_on_finish() {
        let (decoder, calls) = script(vec![vec![token(" complete", 0, 4000)]]);
        let mut refine = Refine::new(decoder);
        refine.feed(&vec![0.0; REFINE_WINDOW]).unwrap();
        assert!(refine.finish().unwrap().is_empty());
        assert_eq!(*calls.lock(), [(REFINE_WINDOW, 0)]);
    }

    #[test]
    fn inference_errors_reach_worker_for_live_and_refine() {
        for refined in [false, true] {
            let (mut decoder, _) = script(Vec::new());
            decoder
                .results
                .push_back(Err(anyhow::anyhow!("decoder failed")));
            let mut transcriber: Box<dyn Transcriber> = if refined {
                Box::new(Refine::new(decoder))
            } else {
                Box::new(Live::new(decoder))
            };
            assert_eq!(
                transcriber
                    .feed(&vec![0.0; REFINE_WINDOW])
                    .unwrap_err()
                    .to_string(),
                "decoder failed"
            );
        }
    }

    fn chunk(pcm: Vec<f32>) -> PcmSlice {
        let end = pcm.len();
        PcmSlice {
            pcm: Arc::new(pcm),
            range: 0..end,
        }
    }

    #[test]
    fn live_batch_decodes_latest_pcm_once_and_keeps_absolute_time() {
        let (decoder, calls) = script(Vec::new());
        let mut live = Live::new(decoder);
        live.feed_chunks(&[chunk(vec![1.0; RATE / 2])]).unwrap();
        assert!(calls.lock().is_empty());
        live.feed_chunks(&[
            chunk(vec![2.0; RATE * 10]),
            chunk(vec![3.0; RATE * 24 + RATE / 2]),
        ])
        .unwrap();
        assert_eq!(*calls.lock(), [(LIVE_CAP, 5000)]);
        assert_eq!(live.buffer.len(), LIVE_CAP);
        assert_eq!(live.buffer[0], 2.0);
        assert_eq!(live.buffer[LIVE_CAP - 1], 3.0);
        assert_eq!(live.received, (RATE * 35) as u64);
        live.feed(&vec![4.0; RATE / 2]).unwrap();
        assert_eq!(*calls.lock(), [(LIVE_CAP, 5000), (LIVE_CAP, 5500)]);
    }

    #[test]
    fn live_batch_agreement_keeps_committed_tail_and_finish_skips_repeat_decode() {
        let first = token(" first", 0, 500);
        let tail = token(" tail", 1500, 1900);
        let (decoder, calls) = script(vec![vec![first.clone()], vec![first, tail]]);
        let mut live = Live::new(decoder);
        live.feed_chunks(&[chunk(vec![0.0; RATE])]).unwrap();
        let output = live.feed_chunks(&[chunk(vec![0.0; RATE])]).unwrap();
        assert_eq!(committed(&output), ["first"]);
        assert_eq!(committed(&live.finish().unwrap()), ["tail"]);
        assert_eq!(*calls.lock(), [(RATE, 0), (RATE * 2, 0)]);
        assert!(live.feed_chunks(&[]).is_err());
    }

    #[test]
    fn refine_batch_preserves_ordered_windows_and_partial_tail() {
        let (decoder, calls) = script(vec![
            vec![token(" first", 100, 500)],
            vec![token(" second", 5200, 5800)],
            vec![token(" tail", 10100, 10300)],
        ]);
        let mut refine = Refine::new(decoder);
        let output = refine
            .feed_chunks(&[
                chunk(vec![0.0; RATE * 3]),
                chunk(vec![0.0; RATE * 7 + RATE / 2]),
            ])
            .unwrap();
        assert_eq!(committed(&output), ["first", "second"]);
        assert_eq!(committed(&refine.finish().unwrap()), ["tail"]);
        assert_eq!(
            *calls.lock(),
            [(RATE * 5, 0), (RATE * 6, 4000), (RATE * 3 / 2, 9000)]
        );
    }
}
