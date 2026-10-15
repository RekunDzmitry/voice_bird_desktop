//! Synchronous 16 kHz mono transcription executed by dedicated inference workers.
//!
//! Decoder weights are shared per factory; mutable state belongs exclusively
//! to one recording/pass. Private PCM slices never cross the event bus.

use std::path::Path;
use std::sync::Arc;

use crate::audio::buffer::PcmSlice;

pub(crate) const DECODER_THREADS: usize = 2;
pub(crate) const LIVE_HOP_SAMPLES: usize = 12_000;
pub(crate) const REFINE_WINDOW_SAMPLES: usize = 80_000;

#[cfg(all(feature = "local-engine", target_os = "macos"))]
mod coreml;
#[cfg(any(feature = "local-engine", test))]
mod local_agreement;
#[cfg(any(feature = "local-engine", test))]
mod streaming;
#[cfg(feature = "local-engine")]
mod whisper;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TranscriptOut {
    Tentative {
        text: String,
    },
    Committed {
        t_start_ms: u64,
        t_end_ms: u64,
        text: String,
    },
}

pub trait Transcriber: Send {
    fn feed(&mut self, pcm: &[f32]) -> anyhow::Result<Vec<TranscriptOut>>;
    /// Consume an ordered job's PCM, coalescing overdue live hypotheses.
    fn feed_chunks(&mut self, chunks: &[PcmSlice]) -> anyhow::Result<Vec<TranscriptOut>>;
    fn finish(&mut self) -> anyhow::Result<Vec<TranscriptOut>>;
}

pub trait TranscriberFactory: Send + Sync {
    fn live(&self, model: &Path, language: &str) -> anyhow::Result<Box<dyn Transcriber>>;
    fn refine(&self, model: &Path, language: &str) -> anyhow::Result<Box<dyn Transcriber>>;
}

pub fn system_engines() -> Arc<dyn TranscriberFactory> {
    Arc::new(SystemEngines::default())
}

#[derive(Default)]
struct SystemEngines {
    #[cfg(feature = "local-engine")]
    weights: whisper::WeightCache,
}

impl TranscriberFactory for SystemEngines {
    fn live(&self, model: &Path, language: &str) -> anyhow::Result<Box<dyn Transcriber>> {
        #[cfg(feature = "local-engine")]
        {
            Ok(Box::new(streaming::Live::new(whisper::Decoder::open(
                self.weights.get(model)?,
                language,
                false,
            )?)))
        }
        #[cfg(not(feature = "local-engine"))]
        {
            let _ = (model, language);
            anyhow::bail!("local engine disabled")
        }
    }

    fn refine(&self, model: &Path, language: &str) -> anyhow::Result<Box<dyn Transcriber>> {
        #[cfg(feature = "local-engine")]
        {
            Ok(Box::new(streaming::Refine::new(whisper::Decoder::open(
                self.weights.get(model)?,
                language,
                true,
            )?)))
        }
        #[cfg(not(feature = "local-engine"))]
        {
            let _ = (model, language);
            anyhow::bail!("local engine disabled")
        }
    }
}

#[cfg(all(test, not(feature = "local-engine")))]
mod tests {
    use super::*;

    #[test]
    fn disabled_factory_rejects_both_passes_explicitly() {
        let factory = system_engines();
        let path = Path::new("not-opened.gguf");
        for result in [factory.live(path, "en"), factory.refine(path, "en")] {
            assert_eq!(
                result.err().expect("disabled engine").to_string(),
                "local engine disabled"
            );
        }
    }
}
