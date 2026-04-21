use std::time::Duration;
use tempfile::TempDir;
use tokio::runtime::Builder;

use voice_bird::session::finalize::{finalize, SessionMeta};
use voice_bird::session::writer::SegmentWriter;
use voice_bird::transcription::{
    mock::{MockEngine, MockEvent},
    EngineConfig, EngineEvent, TranscriptionEngine,
};

#[test]
fn mock_engine_events_land_in_jsonl_and_finalize() {
    let rt = Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let dir = TempDir::new().unwrap();
        let jsonl = dir.path().join("transcript.jsonl");

        let mut engine = MockEngine::new(vec![
            MockEvent::Tentative("he".into()),
            MockEvent::Committed {
                t_start_ms: 0,
                t_end_ms: 500,
                text: "hello".into(),
            },
            MockEvent::Committed {
                t_start_ms: 500,
                t_end_ms: 1100,
                text: "world".into(),
            },
        ]);

        let handle = engine
            .start(EngineConfig {
                model_path: "mock".into(),
                language: None,
                sample_rate: 16_000,
                hop_ms: 750,
                min_window_ms: 1000,
            })
            .unwrap();

        // drive mock
        for _ in 0..3 {
            handle.pcm_tx.send(vec![0.0; 16]).await.unwrap();
        }

        let mut writer = SegmentWriter::open(&jsonl).unwrap();
        let mut rx = handle.events_rx;
        let mut commits = 0;
        while commits < 2 {
            let ev = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .unwrap();
            if let EngineEvent::Committed(seg) = ev {
                writer.append(&(&seg).into()).unwrap();
                commits += 1;
            }
        }
        drop(writer);

        finalize(
            &jsonl,
            &dir.path().join("transcript.json"),
            &dir.path().join("transcript.txt"),
            &dir.path().join("meta.json"),
            &SessionMeta::default(),
        )
        .unwrap();

        let txt = std::fs::read_to_string(dir.path().join("transcript.txt")).unwrap();
        assert_eq!(txt, "hello\nworld\n");
    });
}
