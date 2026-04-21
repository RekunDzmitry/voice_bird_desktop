use std::time::Duration;

use tokio::sync::{broadcast, mpsc, oneshot};

use super::{EngineConfig, EngineEvent, EngineHandle, Segment, TranscriptionEngine};

#[derive(Debug, Clone)]
pub enum MockEvent {
    ModelLoaded(String),
    Tentative(String),
    Committed { t_start_ms: u64, t_end_ms: u64, text: String },
}

pub struct MockEngine {
    script: Vec<MockEvent>,
}

impl MockEngine {
    pub fn new(script: Vec<MockEvent>) -> Self {
        Self { script }
    }
}

impl TranscriptionEngine for MockEngine {
    fn start(&mut self, _cfg: EngineConfig) -> anyhow::Result<EngineHandle> {
        let (pcm_tx, mut pcm_rx) = mpsc::channel::<Vec<f32>>(16);
        let (events_tx, events_rx) = broadcast::channel::<EngineEvent>(64);
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
        let script = std::mem::take(&mut self.script);

        tokio::spawn(async move {
            let mut iter = script.into_iter();
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    msg = pcm_rx.recv() => {
                        if msg.is_none() { break; }
                        let Some(evt) = iter.next() else { continue; };
                        let out = match evt {
                            MockEvent::ModelLoaded(name) => EngineEvent::ModelLoaded { name },
                            MockEvent::Tentative(text)   => EngineEvent::Tentative(text),
                            MockEvent::Committed { t_start_ms, t_end_ms, text } =>
                                EngineEvent::Committed(Segment {
                                    t_start: Duration::from_millis(t_start_ms),
                                    t_end:   Duration::from_millis(t_end_ms),
                                    text,
                                    tokens:  Vec::new(),
                                }),
                        };
                        let _ = events_tx.send(out);
                    }
                }
            }
        });

        Ok(EngineHandle { pcm_tx, events_rx, shutdown: shutdown_tx })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn emits_scripted_events_in_order() {
        let script = vec![
            MockEvent::ModelLoaded("mock".into()),
            MockEvent::Tentative("hel".into()),
            MockEvent::Tentative("hello".into()),
            MockEvent::Committed {
                t_start_ms: 0, t_end_ms: 1000, text: "hello".into(),
            },
        ];
        let mut engine = MockEngine::new(script);
        let handle = engine.start(test_cfg()).unwrap();
        let mut rx = handle.events_rx;

        // Drive the script by sending any PCM; MockEngine emits one script
        // step per received PCM chunk.
        for _ in 0..4 {
            handle.pcm_tx.send(vec![0.0; 16]).await.unwrap();
        }

        let e1 = rx.recv().await.unwrap();
        assert!(matches!(e1, EngineEvent::ModelLoaded { .. }));
        let e2 = rx.recv().await.unwrap();
        assert!(matches!(e2, EngineEvent::Tentative(s) if s == "hel"));
        let e3 = rx.recv().await.unwrap();
        assert!(matches!(e3, EngineEvent::Tentative(s) if s == "hello"));
        let e4 = rx.recv().await.unwrap();
        assert!(matches!(e4, EngineEvent::Committed(seg) if seg.text == "hello"));
    }

    fn test_cfg() -> EngineConfig {
        EngineConfig {
            model_path: std::path::PathBuf::from("/dev/null"),
            language: None,
            sample_rate: 16_000,
            hop_ms: 750,
            min_window_ms: 1000,
        }
    }
}
