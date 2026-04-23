use tokio::sync::{broadcast, mpsc, oneshot};

use super::{EngineConfig, EngineEvent, EngineHandle, TranscriptionEngine};

/// AssemblyAI Universal-Streaming v3 engine. Opens a WebSocket to
/// `wss://streaming.assemblyai.com/v3/ws`, forwards 16-kHz mono PCM as
/// binary frames, and maps incoming JSON turns onto `EngineEvent`s.
pub struct AssemblyAiEngine {
    api_key: String,
}

impl AssemblyAiEngine {
    pub fn new(api_key: String) -> Self {
        Self { api_key }
    }
}

impl TranscriptionEngine for AssemblyAiEngine {
    fn start(&mut self, cfg: EngineConfig) -> anyhow::Result<EngineHandle> {
        let (api_key, language, sample_rate) = match cfg {
            EngineConfig::Cloud { api_key, language, sample_rate } =>
                (api_key, language, sample_rate),
            EngineConfig::Local { .. } => {
                anyhow::bail!("AssemblyAiEngine requires EngineConfig::Cloud");
            }
        };

        if api_key.is_empty() {
            anyhow::bail!("AssemblyAiEngine: api_key is empty");
        }
        if sample_rate != 16_000 {
            anyhow::bail!(
                "AssemblyAiEngine requires 16 kHz PCM; got {sample_rate}",
            );
        }

        // Connection / pumping wired up in Tasks 7–10.
        let (pcm_tx, _pcm_rx) = mpsc::channel::<Vec<f32>>(32);
        let (_events_tx, events_rx) = broadcast::channel::<EngineEvent>(256);
        let (shutdown_tx, _shutdown_rx) = oneshot::channel::<()>();

        let _ = (&self.api_key, api_key, language); // avoid unused warnings until wired up
        anyhow::bail!("AssemblyAiEngine: not implemented yet (see Task 7)");

        #[allow(unreachable_code)]
        Ok(EngineHandle { pcm_tx, events_rx, shutdown: shutdown_tx })
    }
}
