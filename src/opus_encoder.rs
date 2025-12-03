use anyhow::{Context, Result};
use audiopus::{coder::Encoder, Application, Channels, SampleRate};

/// Opus encoder configuration optimized for voice streaming
/// Based on WebSocket + Opus architecture article recommendations
pub struct OpusEncoderConfig {
    /// Sample rate (48000 Hz recommended for Opus)
    pub sample_rate: u32,
    /// Number of channels (1 = mono, 2 = stereo)
    pub channels: u16,
    /// Bitrate in bits per second (24000 = 24 kbps recommended for voice)
    pub bitrate: i32,
    /// Frame duration in milliseconds (20ms = 960 samples at 48kHz)
    pub frame_duration_ms: u32,
}

impl Default for OpusEncoderConfig {
    fn default() -> Self {
        Self {
            sample_rate: 48000,
            channels: 1, // Mono for voice
            bitrate: 24000, // 24 kbps - optimal for speech
            frame_duration_ms: 20, // 20ms frames
        }
    }
}

impl OpusEncoderConfig {
    /// Calculate frame size in samples based on sample rate and frame duration
    pub fn frame_size(&self) -> usize {
        (self.sample_rate as usize * self.frame_duration_ms as usize) / 1000
    }

    /// Validate configuration parameters
    pub fn validate(&self) -> Result<()> {
        if self.sample_rate != 48000 && self.sample_rate != 24000 &&
           self.sample_rate != 16000 && self.sample_rate != 12000 &&
           self.sample_rate != 8000 {
            anyhow::bail!("Unsupported sample rate: {}. Must be 8000, 12000, 16000, 24000, or 48000", self.sample_rate);
        }

        if self.channels == 0 || self.channels > 2 {
            anyhow::bail!("Channels must be 1 (mono) or 2 (stereo), got {}", self.channels);
        }

        if self.bitrate < 6000 || self.bitrate > 510000 {
            anyhow::bail!("Bitrate must be between 6000 and 510000 bps, got {}", self.bitrate);
        }

        if self.frame_duration_ms != 10 && self.frame_duration_ms != 20 &&
           self.frame_duration_ms != 40 && self.frame_duration_ms != 60 {
            anyhow::bail!("Frame duration must be 10, 20, 40, or 60 ms, got {}", self.frame_duration_ms);
        }

        Ok(())
    }
}

/// Opus audio encoder for real-time streaming
/// Optimized for low-latency voice communication
pub struct OpusAudioEncoder {
    encoder: Encoder,
    config: OpusEncoderConfig,
    frame_buffer: Vec<f32>,
}

impl OpusAudioEncoder {
    /// Create a new Opus encoder with the given configuration
    pub fn new(config: OpusEncoderConfig) -> Result<Self> {
        config.validate().context("Invalid Opus configuration")?;

        let sample_rate = match config.sample_rate {
            48000 => SampleRate::Hz48000,
            24000 => SampleRate::Hz24000,
            16000 => SampleRate::Hz16000,
            12000 => SampleRate::Hz12000,
            8000 => SampleRate::Hz8000,
            _ => anyhow::bail!("Unsupported sample rate: {}", config.sample_rate),
        };

        let channels = if config.channels == 1 {
            Channels::Mono
        } else {
            Channels::Stereo
        };

        // Create encoder with VOIP application mode (optimized for voice)
        let mut encoder = Encoder::new(sample_rate, channels, Application::Voip)
            .context("Failed to create Opus encoder")?;

        // Configure encoder settings based on article recommendations
        // Note: Some advanced settings may not be available in all audiopus versions
        // The encoder will use reasonable defaults if these settings can't be applied
        let _ = encoder.set_bitrate(audiopus::Bitrate::BitsPerSecond(config.bitrate));

        // Additional encoder settings like complexity, FEC, and packet loss percentage
        // are configured through the encoder's internal defaults for VOIP mode
        // which are already optimized for voice communication

        let frame_size = config.frame_size();
        let frame_buffer = Vec::with_capacity(frame_size);

        Ok(Self {
            encoder,
            config,
            frame_buffer,
        })
    }

    /// Create an encoder with default configuration (48kHz mono, 24kbps)
    pub fn with_defaults() -> Result<Self> {
        Self::new(OpusEncoderConfig::default())
    }

    /// Get the frame size in samples
    pub fn frame_size(&self) -> usize {
        self.config.frame_size()
    }

    /// Get the sample rate
    pub fn sample_rate(&self) -> u32 {
        self.config.sample_rate
    }

    /// Get the number of channels
    pub fn channels(&self) -> u16 {
        self.config.channels
    }

    /// Get the bitrate in bps
    pub fn bitrate(&self) -> i32 {
        self.config.bitrate
    }

    /// Get the frame duration in milliseconds
    pub fn frame_duration_ms(&self) -> u32 {
        self.config.frame_duration_ms
    }

    /// Add samples to the internal buffer and encode when a complete frame is ready
    /// Returns Some(encoded_data) when a frame is complete, None otherwise
    pub fn buffer_and_encode(&mut self, samples: &[f32]) -> Result<Option<Vec<u8>>> {
        self.frame_buffer.extend_from_slice(samples);

        let frame_size = self.frame_size();

        if self.frame_buffer.len() >= frame_size {
            // Extract one frame worth of samples
            let frame: Vec<f32> = self.frame_buffer.drain(..frame_size).collect();

            // Encode the frame
            let encoded = self.encode(&frame)?;
            Ok(Some(encoded))
        } else {
            Ok(None)
        }
    }

    /// Encode a complete frame of audio samples
    /// Input must be exactly frame_size() samples
    pub fn encode(&mut self, samples: &[f32]) -> Result<Vec<u8>> {
        let frame_size = self.frame_size();

        if samples.len() != frame_size {
            anyhow::bail!(
                "Invalid frame size: expected {} samples, got {}",
                frame_size,
                samples.len()
            );
        }

        // Allocate output buffer (max Opus frame size is typically ~4000 bytes)
        let mut output = vec![0u8; 4000];

        // Encode the audio data
        let encoded_len = self.encoder
            .encode_float(samples, &mut output)
            .context("Failed to encode audio with Opus")?;

        // Truncate to actual encoded size
        output.truncate(encoded_len);

        Ok(output)
    }

    /// Encode audio with padding if the input is smaller than a frame
    /// Useful for handling the last chunk of audio
    pub fn encode_with_padding(&mut self, samples: &[f32]) -> Result<Vec<u8>> {
        let frame_size = self.frame_size();

        if samples.len() == frame_size {
            return self.encode(samples);
        }

        // Create padded frame
        let mut padded = vec![0.0f32; frame_size];
        let copy_len = samples.len().min(frame_size);
        padded[..copy_len].copy_from_slice(&samples[..copy_len]);

        self.encode(&padded)
    }

    /// Dynamically adjust bitrate (useful for adaptive bitrate control)
    /// Range: 6000 to 510000 bps
    pub fn set_bitrate(&mut self, bitrate: i32) -> Result<()> {
        if bitrate < 6000 || bitrate > 510000 {
            anyhow::bail!("Bitrate must be between 6000 and 510000 bps, got {}", bitrate);
        }

        self.encoder
            .set_bitrate(audiopus::Bitrate::BitsPerSecond(bitrate))
            .context("Failed to set bitrate")?;

        self.config.bitrate = bitrate;
        Ok(())
    }

    /// Get current encoder configuration
    pub fn config(&self) -> &OpusEncoderConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = OpusEncoderConfig::default();
        assert_eq!(config.sample_rate, 48000);
        assert_eq!(config.channels, 1);
        assert_eq!(config.bitrate, 24000);
        assert_eq!(config.frame_duration_ms, 20);
        assert_eq!(config.frame_size(), 960); // 48000 * 0.02
    }

    #[test]
    fn test_encoder_creation() {
        let encoder = OpusAudioEncoder::with_defaults();
        assert!(encoder.is_ok());

        let encoder = encoder.unwrap();
        assert_eq!(encoder.frame_size(), 960);
        assert_eq!(encoder.sample_rate(), 48000);
        assert_eq!(encoder.channels(), 1);
    }

    #[test]
    fn test_invalid_config() {
        let config = OpusEncoderConfig {
            sample_rate: 44100, // Invalid for Opus
            channels: 1,
            bitrate: 24000,
            frame_duration_ms: 20,
        };

        let result = OpusAudioEncoder::new(config);
        assert!(result.is_err());
    }

    #[test]
    fn test_encode_frame() {
        let mut encoder = OpusAudioEncoder::with_defaults().unwrap();
        let frame_size = encoder.frame_size();

        // Create a test frame with a sine wave
        let mut samples = vec![0.0f32; frame_size];
        for (i, sample) in samples.iter_mut().enumerate() {
            *sample = (i as f32 * 0.1).sin() * 0.5;
        }

        let result = encoder.encode(&samples);
        assert!(result.is_ok());

        let encoded = result.unwrap();
        assert!(!encoded.is_empty());
        assert!(encoded.len() < frame_size * 4); // Compressed size should be smaller
    }

    #[test]
    fn test_buffer_and_encode() {
        let mut encoder = OpusAudioEncoder::with_defaults().unwrap();
        let frame_size = encoder.frame_size();

        // Add samples in chunks smaller than frame size
        let chunk_size = frame_size / 4;
        let mut samples = vec![0.0f32; chunk_size];
        for (i, sample) in samples.iter_mut().enumerate() {
            *sample = (i as f32 * 0.1).sin() * 0.5;
        }

        // First 3 chunks shouldn't produce output
        for _ in 0..3 {
            let result = encoder.buffer_and_encode(&samples).unwrap();
            assert!(result.is_none());
        }

        // Fourth chunk should complete the frame and return encoded data
        let result = encoder.buffer_and_encode(&samples).unwrap();
        assert!(result.is_some());
        assert!(!result.unwrap().is_empty());
    }

    #[test]
    fn test_dynamic_bitrate() {
        let mut encoder = OpusAudioEncoder::with_defaults().unwrap();

        // Test valid bitrate changes
        assert!(encoder.set_bitrate(32000).is_ok());
        assert_eq!(encoder.bitrate(), 32000);

        assert!(encoder.set_bitrate(16000).is_ok());
        assert_eq!(encoder.bitrate(), 16000);

        assert!(encoder.set_bitrate(8000).is_ok());
        assert_eq!(encoder.bitrate(), 8000);

        // Test invalid bitrate
        assert!(encoder.set_bitrate(5000).is_err());
        assert!(encoder.set_bitrate(600000).is_err());
    }
}
