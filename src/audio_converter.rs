//! Audio format conversion for low-latency streaming
//!
//! Converts desktop audio (typically 48kHz stereo f32) to AssemblyAI format (16kHz mono i16)
//! to reduce bandwidth and eliminate backend conversion overhead.

use rubato::{FftFixedIn, Resampler};
use std::sync::Mutex;

/// Audio converter that transforms captured audio to AssemblyAI-compatible format
/// Input: 48kHz stereo f32
/// Output: 16kHz mono i16 (PCM16LE)
pub struct AudioConverter {
    resampler: Mutex<FftFixedIn<f32>>,
    input_sample_rate: u32,
    output_sample_rate: u32,
    input_channels: u16,
}

impl AudioConverter {
    /// Create a new audio converter
    ///
    /// # Arguments
    /// * `input_sample_rate` - Input sample rate (e.g., 48000)
    /// * `input_channels` - Number of input channels (1 or 2)
    /// * `output_sample_rate` - Output sample rate (16000 for AssemblyAI)
    pub fn new(input_sample_rate: u32, input_channels: u16, output_sample_rate: u32) -> Result<Self, String> {
        // Calculate resampling ratio
        let ratio = output_sample_rate as f64 / input_sample_rate as f64;

        // Create FFT-based resampler for high quality
        // chunk_size should be reasonable for real-time processing
        let chunk_size = 1024;

        let resampler = FftFixedIn::<f32>::new(
            input_sample_rate as usize,
            output_sample_rate as usize,
            chunk_size,
            2,  // sub_chunks for smoother output
            1,  // Always resample mono (we downmix first)
        ).map_err(|e| format!("Failed to create resampler: {}", e))?;

        log::info!(
            "AudioConverter initialized: {}Hz {}ch → {}Hz mono (ratio: {:.4})",
            input_sample_rate, input_channels, output_sample_rate, ratio
        );

        Ok(Self {
            resampler: Mutex::new(resampler),
            input_sample_rate,
            output_sample_rate,
            input_channels,
        })
    }

    /// Create converter for typical desktop audio to AssemblyAI format
    /// 48kHz stereo → 16kHz mono
    pub fn for_assemblyai(input_sample_rate: u32, input_channels: u16) -> Result<Self, String> {
        Self::new(input_sample_rate, input_channels, 16000)
    }

    /// Convert audio chunk from input format to output format
    ///
    /// Returns PCM16LE bytes ready for transmission
    pub fn convert(&self, samples: &[f32]) -> Result<Vec<u8>, String> {
        if samples.is_empty() {
            return Ok(Vec::new());
        }

        // Step 1: Downmix stereo to mono
        let mono_samples = if self.input_channels > 1 {
            downmix_to_mono(samples, self.input_channels)
        } else {
            samples.to_vec()
        };

        // Step 2: Resample if needed
        let resampled = if self.input_sample_rate != self.output_sample_rate {
            self.resample(&mono_samples)?
        } else {
            mono_samples
        };

        // Step 3: Convert f32 to i16 bytes (PCM16LE)
        let bytes = f32_to_pcm16le(&resampled);

        Ok(bytes)
    }

    /// Resample mono f32 audio
    fn resample(&self, samples: &[f32]) -> Result<Vec<f32>, String> {
        let mut resampler = self.resampler.lock()
            .map_err(|e| format!("Failed to lock resampler: {}", e))?;

        // rubato expects Vec<Vec<f32>> for multi-channel, but we're mono
        let input_frames = vec![samples.to_vec()];

        // Process the audio
        let output_frames = resampler.process(&input_frames, None)
            .map_err(|e| format!("Resampling failed: {}", e))?;

        // Extract mono channel
        Ok(output_frames.into_iter().next().unwrap_or_default())
    }

    /// Get output sample rate
    pub fn output_sample_rate(&self) -> u32 {
        self.output_sample_rate
    }
}

/// Downmix multi-channel audio to mono by averaging all channels
fn downmix_to_mono(samples: &[f32], channels: u16) -> Vec<f32> {
    if channels == 1 {
        return samples.to_vec();
    }

    let channels = channels as usize;
    let frame_count = samples.len() / channels;
    let mut mono = Vec::with_capacity(frame_count);

    for frame_idx in 0..frame_count {
        let start = frame_idx * channels;
        let end = (start + channels).min(samples.len());

        let sum: f32 = samples[start..end].iter().sum();
        let avg = sum / channels as f32;
        mono.push(avg);
    }

    mono
}

/// Convert f32 samples to PCM16LE bytes
fn f32_to_pcm16le(samples: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(samples.len() * 2);

    for &sample in samples {
        // Clamp to [-1.0, 1.0] and scale to i16 range
        let clamped = sample.clamp(-1.0, 1.0);
        let scaled = (clamped * 32767.0) as i16;

        // Little-endian byte order
        bytes.extend_from_slice(&scaled.to_le_bytes());
    }

    bytes
}

/// Simple audio converter without rubato (uses naive decimation)
/// Faster but lower quality - use for testing or when rubato isn't available
pub struct SimpleAudioConverter {
    input_sample_rate: u32,
    output_sample_rate: u32,
    input_channels: u16,
    decimation_factor: usize,
}

impl SimpleAudioConverter {
    pub fn new(input_sample_rate: u32, input_channels: u16, output_sample_rate: u32) -> Self {
        let decimation_factor = (input_sample_rate / output_sample_rate) as usize;

        log::info!(
            "SimpleAudioConverter: {}Hz {}ch → {}Hz mono (decimation: {}x)",
            input_sample_rate, input_channels, output_sample_rate, decimation_factor
        );

        Self {
            input_sample_rate,
            output_sample_rate,
            input_channels,
            decimation_factor,
        }
    }

    /// Convert audio using simple decimation
    pub fn convert(&self, samples: &[f32]) -> Vec<u8> {
        if samples.is_empty() {
            return Vec::new();
        }

        // Step 1: Downmix to mono
        let mono = if self.input_channels > 1 {
            downmix_to_mono(samples, self.input_channels)
        } else {
            samples.to_vec()
        };

        // Step 2: Decimate with averaging (acts as low-pass filter to prevent aliasing)
        // Instead of taking every Nth sample (which causes aliasing), we average groups
        // of N samples. This is a simple box filter that attenuates high frequencies.
        let decimated: Vec<f32> = if self.decimation_factor > 1 {
            mono.chunks(self.decimation_factor)
                .map(|chunk| chunk.iter().sum::<f32>() / chunk.len() as f32)
                .collect()
        } else {
            mono
        };

        // Step 3: Convert to PCM16LE
        f32_to_pcm16le(&decimated)
    }

    pub fn output_sample_rate(&self) -> u32 {
        self.output_sample_rate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_downmix_stereo() {
        // Stereo: [L1, R1, L2, R2] -> Mono: [(L1+R1)/2, (L2+R2)/2]
        let stereo = vec![0.5, 0.5, 1.0, 0.0, -0.5, 0.5];
        let mono = downmix_to_mono(&stereo, 2);

        assert_eq!(mono.len(), 3);
        assert!((mono[0] - 0.5).abs() < 0.001);  // (0.5 + 0.5) / 2
        assert!((mono[1] - 0.5).abs() < 0.001);  // (1.0 + 0.0) / 2
        assert!((mono[2] - 0.0).abs() < 0.001);  // (-0.5 + 0.5) / 2
    }

    #[test]
    fn test_f32_to_pcm16le() {
        let samples = vec![0.0, 1.0, -1.0, 0.5];
        let bytes = f32_to_pcm16le(&samples);

        assert_eq!(bytes.len(), 8);  // 4 samples * 2 bytes each

        // Check 0.0 -> 0
        assert_eq!(i16::from_le_bytes([bytes[0], bytes[1]]), 0);

        // Check 1.0 -> 32767
        assert_eq!(i16::from_le_bytes([bytes[2], bytes[3]]), 32767);

        // Check -1.0 -> -32767
        assert_eq!(i16::from_le_bytes([bytes[4], bytes[5]]), -32767);
    }

    #[test]
    fn test_simple_converter() {
        let converter = SimpleAudioConverter::new(48000, 2, 16000);

        // Create 48 stereo samples (24 frames)
        let input: Vec<f32> = (0..48).map(|i| (i as f32) / 48.0).collect();
        let output = converter.convert(&input);

        // 24 frames / 3 decimation = 8 mono samples * 2 bytes = 16 bytes
        assert_eq!(output.len(), 16);
    }
}
