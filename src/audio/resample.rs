use rubato::{FftFixedInOut, Resampler as RubatoResampler};

const TARGET_SR: u32 = 16_000;

pub struct Resampler {
    channels: u16,
    inner: Option<FftFixedInOut<f32>>,
    chunk_size_in: usize,
    leftover: Vec<f32>,
}

impl Resampler {
    pub fn new(input_sr: u32, channels: u16) -> anyhow::Result<Self> {
        let requested = 1024.max((input_sr as usize) / 50);
        let (inner, chunk_size_in) = if input_sr == TARGET_SR {
            (None, requested)
        } else {
            let r = FftFixedInOut::new(
                input_sr as usize,
                TARGET_SR as usize,
                requested,
                1, // output channels — we downmix to mono upstream
            )?;
            // rubato 0.14 derives the actual required input chunk size from the
            // sample-rate ratio; the value passed to `new` is only a hint.
            let actual = r.input_frames_next();
            (Some(r), actual)
        };
        Ok(Self {
            channels,
            inner,
            chunk_size_in,
            leftover: Vec::new(),
        })
    }

    pub fn process(&mut self, interleaved: &[f32]) -> anyhow::Result<Vec<f32>> {
        let mono = downmix(interleaved, self.channels);

        if self.inner.is_none() {
            return Ok(mono);
        }

        let mut buf = std::mem::take(&mut self.leftover);
        buf.extend_from_slice(&mono);

        let mut out = Vec::new();
        while buf.len() >= self.chunk_size_in {
            let chunk = &buf[..self.chunk_size_in];
            let input_channels = vec![chunk.to_vec()];
            let resampled = self
                .inner
                .as_mut()
                .unwrap()
                .process(&input_channels, None)?;
            out.extend_from_slice(&resampled[0]);
            buf.drain(..self.chunk_size_in);
        }
        self.leftover = buf;
        Ok(out)
    }
}

fn downmix(interleaved: &[f32], channels: u16) -> Vec<f32> {
    if channels <= 1 {
        return interleaved.to_vec();
    }
    let ch = channels as usize;
    let mut out = Vec::with_capacity(interleaved.len() / ch);
    for frame in interleaved.chunks_exact(ch) {
        let sum: f32 = frame.iter().sum();
        out.push(sum / ch as f32);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_when_already_16k_mono() {
        let input: Vec<f32> = (0..16_000).map(|i| (i as f32 / 16_000.0).sin()).collect();
        let mut r = Resampler::new(16_000, 1).unwrap();
        let out = r.process(&input).unwrap();
        assert!((out.len() as i64 - input.len() as i64).abs() < 32);
    }

    #[test]
    fn downsample_48k_to_16k_preserves_duration() {
        let sr_in = 48_000;
        let len = 48_000; // 1 second
        let input: Vec<f32> = (0..len)
            .map(|i| (i as f32 / 48_000.0 * 440.0 * std::f32::consts::TAU).sin())
            .collect();
        let mut r = Resampler::new(sr_in, 1).unwrap();
        let out = r.process(&input).unwrap();
        // Expect ~16_000 samples (±5%)
        let expected = 16_000;
        let diff = (out.len() as i64 - expected).abs();
        assert!(
            diff < (expected as f32 * 0.05) as i64,
            "out.len = {}, expected ~{}",
            out.len(),
            expected
        );
    }

    #[test]
    fn stereo_downmix_to_mono() {
        // interleaved [L,R,L,R,...]
        let input: Vec<f32> = (0..16_000 * 2)
            .map(|i| if i % 2 == 0 { 1.0 } else { -1.0 })
            .collect();
        let mut r = Resampler::new(16_000, 2).unwrap();
        let out = r.process(&input).unwrap();
        // Downmix should produce ~0 for all samples (1 + -1)/2 = 0
        assert!(out.iter().all(|&s| s.abs() < 0.01));
    }
}
