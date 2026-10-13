//! Stateful downmix and FFT resampling to 16 kHz mono, including stop-time flush.

use rubato::{FftFixedInOut, Resampler as RubatoResampler};

const TARGET_SR: u32 = 16_000;

pub struct Resampler {
    channels: u16,
    input_sr: u32,
    inner: Option<FftFixedInOut<f32>>,
    chunk_size_in: usize,
    leftover: Vec<f32>,
    output: Vec<Vec<f32>>,
    delay_remaining: usize,
    input_frames: u64,
    output_frames: u64,
    finished: bool,
}

impl Resampler {
    pub fn new(input_sr: u32, channels: u16) -> anyhow::Result<Self> {
        anyhow::ensure!(
            input_sr > 0 && channels > 0,
            "invalid audio sample rate or channel count"
        );
        let requested = 1024.max(input_sr as usize / 50);
        let inner = if input_sr == TARGET_SR {
            None
        } else {
            Some(FftFixedInOut::new(
                input_sr as usize,
                TARGET_SR as usize,
                requested,
                1,
            )?)
        };
        let chunk_size_in = inner.as_ref().map_or(requested, |r| r.input_frames_next());
        let delay_remaining = inner.as_ref().map_or(0, |r| r.output_delay());
        let output = inner
            .as_ref()
            .map_or_else(Vec::new, |r| r.output_buffer_allocate(true));
        Ok(Self {
            channels,
            input_sr,
            inner,
            chunk_size_in,
            leftover: Vec::with_capacity(chunk_size_in),
            output,
            delay_remaining,
            input_frames: 0,
            output_frames: 0,
            finished: false,
        })
    }

    pub fn process(&mut self, interleaved: &[f32]) -> anyhow::Result<Vec<f32>> {
        anyhow::ensure!(!self.finished, "resampler already finished");
        let channels = usize::from(self.channels);
        anyhow::ensure!(
            interleaved.len().is_multiple_of(channels),
            "incomplete interleaved audio frame"
        );
        self.input_frames += (interleaved.len() / channels) as u64;
        if self.inner.is_none() {
            let mono = downmix(interleaved, channels);
            self.output_frames += mono.len() as u64;
            return Ok(mono);
        }
        // Downmix directly into the staging buffer; no copy per FFT chunk.
        self.leftover.extend(
            interleaved
                .chunks_exact(channels)
                .map(|frame| frame.iter().sum::<f32>() / channels as f32),
        );
        let mut out =
            Vec::with_capacity(self.leftover.len() * TARGET_SR as usize / self.input_sr as usize);
        let mut consumed = 0;
        while self.leftover.len() - consumed >= self.chunk_size_in {
            let input = [&self.leftover[consumed..consumed + self.chunk_size_in]];
            let (_, written) =
                self.inner
                    .as_mut()
                    .unwrap()
                    .process_into_buffer(&input, &mut self.output, None)?;
            let skip = self.delay_remaining.min(written);
            self.delay_remaining -= skip;
            out.extend_from_slice(&self.output[0][skip..written]);
            consumed += self.chunk_size_in;
        }
        self.leftover.drain(..consumed);
        self.output_frames += out.len() as u64;
        Ok(out)
    }

    /// Zero-pad the last input block and drain filter delay, then trim padding.
    /// Removing initial FFT delay keeps transcript timestamps tied to input time.
    pub fn finish(&mut self) -> anyhow::Result<Vec<f32>> {
        if self.finished {
            return Ok(Vec::new());
        }
        self.finished = true;
        if self.inner.is_none() {
            return Ok(Vec::new());
        }
        let target = self.input_frames * u64::from(TARGET_SR) / u64::from(self.input_sr);
        let remaining = target.saturating_sub(self.output_frames) as usize;
        let mut out = Vec::with_capacity(remaining);
        self.leftover.resize(self.chunk_size_in, 0.0);
        while out.len() < remaining {
            let input = [self.leftover.as_slice()];
            let (_, written) =
                self.inner
                    .as_mut()
                    .unwrap()
                    .process_into_buffer(&input, &mut self.output, None)?;
            let skip = self.delay_remaining.min(written);
            self.delay_remaining -= skip;
            let take = (remaining - out.len()).min(written - skip);
            out.extend_from_slice(&self.output[0][skip..skip + take]);
            self.leftover.fill(0.0);
        }
        self.output_frames += out.len() as u64;
        self.leftover.clear();
        Ok(out)
    }
}

fn downmix(interleaved: &[f32], channels: usize) -> Vec<f32> {
    if channels == 1 {
        return interleaved.to_vec();
    }
    interleaved
        .chunks_exact(channels)
        .map(|frame| frame.iter().sum::<f32>() / channels as f32)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_when_already_16k_mono() {
        let input: Vec<f32> = (0..16_000).map(|i| (i as f32 / 16_000.0).sin()).collect();
        let mut r = Resampler::new(16_000, 1).unwrap();
        assert_eq!(r.process(&input).unwrap(), input);
        assert!(r.finish().unwrap().is_empty());
    }

    #[test]
    fn downsample_48k_to_16k_preserves_duration() {
        let input: Vec<f32> = (0..48_000)
            .map(|i| (i as f32 / 48_000.0 * 440.0 * std::f32::consts::TAU).sin())
            .collect();
        let mut r = Resampler::new(48_000, 1).unwrap();
        let mut out = r.process(&input).unwrap();
        out.extend(r.finish().unwrap());
        assert_eq!(out.len(), 16_000);
        // Verify actual audio survived, not only sample count.
        assert!(out[1000..15000].iter().any(|s| s.abs() > 0.9));
    }

    #[test]
    fn stereo_downmix_to_mono() {
        let input: Vec<f32> = (0..32_000)
            .map(|i| if i % 2 == 0 { 1.0 } else { -1.0 })
            .collect();
        let mut r = Resampler::new(16_000, 2).unwrap();
        let out = r.process(&input).unwrap();
        assert_eq!(out, vec![0.0; 16_000]);
    }

    #[test]
    fn flush_keeps_short_tail_and_is_idempotent() {
        let mut r = Resampler::new(48_000, 1).unwrap();
        assert!(r.process(&vec![1.0; 300]).unwrap().is_empty());
        let tail = r.finish().unwrap();
        assert_eq!(tail.len(), 100);
        assert!(tail[20..80].iter().all(|sample| *sample > 0.9));
        assert!(r.finish().unwrap().is_empty());
        assert!(r.process(&[0.0]).is_err());
    }

    #[test]
    fn chunk_boundaries_do_not_change_resampling() {
        let input: Vec<f32> = (0..44_100).map(|i| (i as f32 * 0.04).sin()).collect();
        let mut one = Resampler::new(44_100, 1).unwrap();
        let mut expected = one.process(&input).unwrap();
        expected.extend(one.finish().unwrap());
        let mut many = Resampler::new(44_100, 1).unwrap();
        let mut actual = Vec::new();
        for chunk in input.chunks(137) {
            actual.extend(many.process(chunk).unwrap());
        }
        actual.extend(many.finish().unwrap());
        assert_eq!(actual, expected);
        assert_eq!(actual.len(), 16_000);
    }
}
