//! Full recording lifecycle with isolated PCM/model fixtures; no native devices or network.
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use voice_bird_cli::bus::{AppEvent, EventBus, FocusMove, TranscriptPass};
use voice_bird_cli::consumer::recording::RecordingConsumer;
use voice_bird_cli::consumer::ui_view::BlockState;
use voice_bird_cli::consumer::{Consumer, Consumers};
use voice_bird_cli::db::Database;
use voice_bird_cli::input::Intent;
use voice_bird_cli::language::LANGUAGES;
use voice_bird_cli::producer::input::resolve_intent;
use voice_bird_cli::producer::model_watch::ModelWatcher;
use voice_bird_cli::producer::recording::{RecordingProducer, RecordingServices};
use voice_bird_cli::testing::{
    sample_source_snapshot, FixtureCapture, FixtureDownloader, FixtureSources, FixtureStore,
    FixtureTranscriberFactory, Outcome,
};
use voice_bird_cli::transcribe::TranscriptOut;

struct Harness {
    consumer: Consumer,
    producer: RecordingProducer,
    db: Database,
    bus: EventBus,
    capture: Arc<FixtureCapture>,
    store: Arc<FixtureStore>,
    history: Vec<AppEvent>,
    _tmp: tempfile::TempDir,
}

fn committed(text: &str, start: u64, end: u64) -> TranscriptOut {
    TranscriptOut::Committed {
        t_start_ms: start,
        t_end_ms: end,
        text: text.into(),
    }
}

impl Harness {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let bus = EventBus::new();
        let mut db = Database::open(&tmp.path().join("index.sqlite"), bus.sender()).unwrap();
        let store = Arc::new(FixtureStore::new(
            tmp.path().join("models"),
            &LANGUAGES[0]
                .models()
                .iter()
                .map(|model| model.id)
                .collect::<Vec<_>>(),
        ));
        ModelWatcher::new(store.clone())
            .check(&mut db, &bus.sender())
            .unwrap();
        let capture = Arc::new(FixtureCapture::new(vec![vec![0.1; 16_000]]));
        let engines = Arc::new(FixtureTranscriberFactory {
            live_steps: vec![vec![
                committed("hello world", 0, 900),
                TranscriptOut::Tentative {
                    text: "next words".into(),
                },
            ]],
            refine_steps: Vec::new(),
            live_finish: vec![committed("next words", 900, 1000)],
            refine_finish: vec![committed("Hello, world. Next words.", 0, 1000)],
        });
        let consumer = Consumer::new(Consumers::new(
            Arc::new(FixtureDownloader::new(
                vec![1; 64],
                Outcome::Ok,
                Arc::new(AtomicUsize::new(0)),
            )),
            store.clone(),
            Arc::new(FixtureSources(None)),
        ));
        let producer = RecordingProducer::new(RecordingServices {
            capture: capture.clone(),
            engines,
            sessions_root: tmp.path().join("sessions"),
        });
        Self {
            consumer,
            producer,
            db,
            bus,
            capture,
            store,
            history: Vec::new(),
            _tmp: tmp,
        }
    }

    fn process(&mut self, events: Vec<AppEvent>) {
        let accepted: Vec<_> = events
            .into_iter()
            .filter(|event| {
                let admitted = voice_bird_cli::db::apply(&mut self.db, event).unwrap();
                if !admitted {
                    if let AppEvent::TranscriptionRequested { request } = event {
                        self.producer.reject_unaccepted(request);
                    }
                }
                admitted
            })
            .collect();
        let tx = self.bus.sender();
        for event in &accepted {
            self.producer.produce(event, &mut self.db, &tx).unwrap();
        }
        self.consumer.consume(&accepted, &mut self.db, &tx);
        self.history.extend(accepted);
    }

    fn drain(&mut self) {
        loop {
            let events = self.bus.drain().collect::<Vec<_>>();
            if events.is_empty() {
                break;
            }
            self.process(events);
        }
    }

    fn take_queued_start(&mut self) -> AppEvent {
        self.process(vec![AppEvent::AddBlock]);
        self.process(vec![AppEvent::BeginLanguage {
            block: 1,
            language: &LANGUAGES[0],
            source_rev: None,
        }]);
        loop {
            let mut events = self.bus.drain().collect::<Vec<_>>();
            assert!(
                !events.is_empty(),
                "cached models must resolve a start request"
            );
            if let Some(index) = events
                .iter()
                .position(|event| matches!(event, AppEvent::RecordingRequested { .. }))
            {
                let request = events.remove(index);
                self.process(events);
                return request;
            }
            self.process(events);
        }
    }

    fn start(&mut self) {
        let request = self.take_queued_start();
        self.process(vec![request]);
        self.drain();
    }

    fn key(&mut self, intent: Intent) {
        resolve_intent(
            intent,
            &self.consumer.consumers.ui_view,
            &mut self.db,
            &self.bus.sender(),
        );
        self.drain();
    }

    async fn settle(&mut self, predicate: impl Fn(&Self) -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            self.drain();
            if predicate(self) {
                return;
            }
            let event = tokio::time::timeout_at(deadline, self.bus.recv())
                .await
                .unwrap_or_else(|_| panic!("recording did not settle: {:?}", self.history))
                .unwrap();
            self.process(vec![event]);
        }
    }

    fn sessions(&self) -> Vec<(u64, PathBuf)> {
        self.history
            .iter()
            .filter_map(|event| match event {
                AppEvent::RecordingStarted {
                    recording,
                    session_dir,
                    ..
                } => Some((*recording, session_dir.into())),
                _ => None,
            })
            .collect()
    }

    async fn quit(&mut self) {
        self.process(vec![AppEvent::Quit]);
        self.producer.begin_shutdown();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while !self.producer.shutdown_finished() {
            match tokio::time::timeout_at(deadline, self.bus.recv()).await {
                Ok(Some(event)) => self.process(vec![event]),
                Ok(None) | Err(_) => break,
            }
        }
        self.producer
            .finish_shutdown(&mut self.db, &self.bus.sender())
            .unwrap();
        self.drain();
    }
}

#[tokio::test]
async fn cached_models_capture_live_text_and_close_flushes_refinement_and_metadata() {
    let mut h = Harness::new();
    h.start();
    h.settle(|h| {
        matches!(&h.consumer.consumers.ui_view.blocks[0].state,
        BlockState::Recording { lines, tentative, session_dir: Some(_), .. }
            if lines == &["hello world"] && tentative == "next words")
    })
    .await;
    let (recording, dir) = h.sessions()[0].clone();
    assert!(dir.join("live.jsonl").is_file());
    assert!(dir.join("transcript.jsonl").is_file());
    assert_eq!(h.capture.calls.load(Ordering::SeqCst), 1);
    // Repeated reconciliation must not open another source.
    h.process(vec![AppEvent::FocusMoved {
        direction: FocusMove::Next,
    }]);
    assert_eq!(h.capture.calls.load(Ordering::SeqCst), 1);
    h.process(vec![AppEvent::BlockClosed { block: 1 }]);
    h.settle(|h| {
        h.history.contains(&AppEvent::RecordingStopped {
            block: 1,
            recording,
        })
    })
    .await;
    assert!(h.consumer.consumers.ui_view.blocks.is_empty());
    assert_eq!(
        std::fs::read_to_string(dir.join("transcript.txt")).unwrap(),
        "Hello, world. Next words.\n"
    );
    let live = std::fs::read_to_string(dir.join("live.jsonl")).unwrap();
    assert!(live.contains("hello world") && live.contains("next words"));
    let meta: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("meta.json")).unwrap()).unwrap();
    assert_eq!(meta["status"], "stopped");
    assert_eq!(meta["language"], "en");
    assert_eq!(meta["live_model"], LANGUAGES[0].live.id);
    assert_eq!(meta["refine_model"], LANGUAGES[0].refine.id);
    assert!(meta["ended_at"].is_string());
    h.quit().await;
}

#[tokio::test]
async fn model_loss_stops_and_immediate_resume_uses_a_distinct_session() {
    let mut h = Harness::new();
    h.start();
    h.settle(|h| {
        matches!(&h.consumer.consumers.ui_view.blocks[0].state,
            BlockState::Recording { lines, .. } if lines == &["hello world"])
    })
    .await;
    let (old_id, old_dir) = h.sessions()[0].clone();
    h.process(vec![AppEvent::ModelMissing(LANGUAGES[0].live)]);
    assert!(matches!(
        h.consumer.consumers.ui_view.blocks[0].state,
        BlockState::Waiting { .. }
    ));
    // Cached models make the block eligible again before its stopped worker
    // has acknowledged termination; the replacement must wait for that event.
    h.settle(|h| h.sessions().len() == 2).await;
    let (new_id, new_dir) = h.sessions()[1].clone();
    assert_ne!(old_id, new_id);
    assert_ne!(old_dir, new_dir);
    let stopped = h
        .history
        .iter()
        .position(|event| matches!(event, AppEvent::RecordingStopped { recording, .. } if *recording == old_id))
        .unwrap();
    let replacement = h
        .history
        .iter()
        .position(|event| matches!(event, AppEvent::RecordingRequested { request } if request.recording == new_id))
        .unwrap();
    assert!(stopped < replacement);
    let replacement_request = h.history[replacement].clone();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(
            &std::fs::read(old_dir.join("meta.json")).unwrap()
        )
        .unwrap()["status"],
        "stopped"
    );
    h.process(vec![
        replacement_request,
        AppEvent::RecordingStopRequested {
            block: 1,
            recording: old_id,
        },
        AppEvent::RecordingFailed {
            block: 1,
            recording: old_id,
            error: "late old failure".into(),
        },
        AppEvent::TranscriptCommitted {
            block: 1,
            recording: old_id,
            pass: TranscriptPass::Live,
            t_start_ms: 0,
            t_end_ms: 1,
            text: "stale".into(),
            transcription: voice_bird_cli::bus::TranscriptionMetrics::default(),
            job: 0,
            worker: 1,
        },
    ]);
    assert!(matches!(h.consumer.consumers.ui_view.blocks[0].state,
        BlockState::Recording { recording: Some(id), .. } if id == new_id));
    h.settle(|h| {
        matches!(&h.consumer.consumers.ui_view.blocks[0].state,
            BlockState::Recording { recording: Some(id), lines, tentative, .. }
                if *id == new_id && lines == &["hello world"] && tentative == "next words")
    })
    .await;
    assert_eq!(h.capture.calls.load(Ordering::SeqCst), 2);
    h.quit().await;
    assert!(new_dir.join("transcript.txt").is_file());
}

#[tokio::test]
async fn rebuilt_consumer_preserves_queued_start_stop_until_matching_terminal() {
    let mut h = Harness::new();
    let request = h.take_queued_start();
    let old_id = match &request {
        AppEvent::RecordingRequested { request } => request.recording,
        _ => unreachable!(),
    };
    h.consumer.consumers.recording = RecordingConsumer::new(h.store.clone());
    h.process(vec![AppEvent::ModelMissing(LANGUAGES[0].live)]);
    assert!(matches!(
        h.consumer.consumers.ui_view.blocks[0].state,
        BlockState::Waiting { .. }
    ));
    // Resume before the original start or its exact stop reaches bus admission.
    h.process(vec![AppEvent::ModelAlreadyCached(LANGUAGES[0].live)]);
    let queued = h.bus.drain().collect::<Vec<_>>();
    assert_eq!(
        queued
            .iter()
            .filter(|event| matches!(event,
                AppEvent::RecordingStopRequested { block: 1, recording } if *recording == old_id))
            .count(),
        1,
        "reconstruction must retain the pending identity and queue its exact stop"
    );
    assert!(
        !queued
            .iter()
            .any(|event| matches!(event, AppEvent::RecordingRequested { .. })),
        "replacement must wait for the original worker's finalized terminal event"
    );
    assert!(h.sessions().is_empty());
    assert_eq!(h.capture.calls.load(Ordering::SeqCst), 0);
    h.process(vec![request]);
    h.settle(|h| {
        h.history.iter().any(|event| {
            matches!(event,
            AppEvent::TranscriptCommitted { recording, pass: TranscriptPass::Live, text, .. }
                if *recording == old_id && text == "hello world")
        })
    })
    .await;
    assert_eq!(h.sessions().len(), 1);
    assert_eq!(h.capture.calls.load(Ordering::SeqCst), 1);
    h.process(queued);
    h.settle(|h| h.sessions().len() == 2).await;
    let (old_recording, old_dir) = h.sessions()[0].clone();
    let (new_id, new_dir) = h.sessions()[1].clone();
    assert_eq!(old_recording, old_id);
    assert_ne!(old_id, new_id);
    assert_ne!(old_dir, new_dir);
    let stopped = h
        .history
        .iter()
        .position(|event| {
            matches!(event,
            AppEvent::RecordingStopped { block: 1, recording } if *recording == old_id)
        })
        .unwrap();
    let replacement = h
        .history
        .iter()
        .position(|event| {
            matches!(event,
            AppEvent::RecordingRequested { request } if request.recording == new_id)
        })
        .unwrap();
    assert!(stopped < replacement);
    assert_eq!(
        std::fs::read_to_string(old_dir.join("transcript.txt")).unwrap(),
        "Hello, world. Next words.\n"
    );
    let meta: serde_json::Value =
        serde_json::from_slice(&std::fs::read(old_dir.join("meta.json")).unwrap()).unwrap();
    assert_eq!(meta["status"], "stopped");
    h.settle(|h| {
        matches!(&h.consumer.consumers.ui_view.blocks[0].state,
            BlockState::Recording { recording: Some(id), lines, .. }
                if *id == new_id && lines == &["hello world"])
    })
    .await;
    assert_eq!(h.capture.calls.load(Ordering::SeqCst), 2);
    h.quit().await;
    assert_eq!(
        std::fs::read_to_string(new_dir.join("transcript.txt")).unwrap(),
        "Hello, world. Next words.\n"
    );
}

#[tokio::test]
async fn capture_failure_is_persisted_and_explicit_retry_recovers() {
    let mut h = Harness::new();
    h.capture
        .fail_next_open("Screen Recording permission required");
    h.start();
    h.settle(|h| {
        matches!(&h.consumer.consumers.ui_view.blocks[0].state,
        BlockState::Failed { error, .. } if error.contains("Screen Recording permission required"))
    })
    .await;
    let (_, failed_dir) = h.sessions()[0].clone();
    let meta: serde_json::Value =
        serde_json::from_slice(&std::fs::read(failed_dir.join("meta.json")).unwrap()).unwrap();
    assert_eq!(meta["status"], "failed");
    h.key(Intent::Retry);
    h.settle(|h| {
        h.sessions().len() == 2
            && matches!(&h.consumer.consumers.ui_view.blocks[0].state,
        BlockState::Recording { lines, .. } if lines == &["hello world"])
    })
    .await;
    assert_eq!(h.capture.calls.load(Ordering::SeqCst), 2);
    h.quit().await;
    for (_, dir) in h.sessions() {
        assert!(dir.join("transcript.txt").is_file());
    }
}

#[tokio::test]
async fn quit_finalizes_all_four_sessions() {
    let mut h = Harness::new();
    h.start();
    for block in 2..=4 {
        h.process(vec![
            AppEvent::AddBlock,
            AppEvent::BeginLanguage {
                block,
                language: &LANGUAGES[0],
                source_rev: None,
            },
        ]);
        h.drain();
    }
    h.settle(|h| h.sessions().len() == 4 && h.capture.calls.load(Ordering::SeqCst) == 4)
        .await;
    h.quit().await;
    for (id, dir) in h.sessions() {
        assert!(h.history.iter().any(|event| matches!(event, AppEvent::RecordingStopped { recording, .. } if *recording == id)));
        let meta: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("meta.json")).unwrap()).unwrap();
        assert_eq!(meta["status"], "stopped");
        assert_eq!(
            std::fs::read_to_string(dir.join("transcript.txt")).unwrap(),
            "Hello, world. Next words.\n"
        );
    }
}

#[tokio::test]
async fn fifth_add_block_cannot_start_another_recording() {
    for snapshot in [None, Some(sample_source_snapshot())] {
        let mut h = Harness::new();
        h.consumer.consumers.audio_sources =
            voice_bird_cli::consumer::audio_sources::AudioSourcesConsumer::new(Arc::new(
                FixtureSources(snapshot),
            ));
        h.start();
        for block in 2..=4 {
            h.process(vec![
                AppEvent::AddBlock,
                AppEvent::BeginLanguage {
                    block,
                    language: &LANGUAGES[0],
                    source_rev: None,
                },
            ]);
            h.drain();
        }
        h.settle(|h| h.sessions().len() == 4 && h.capture.calls.load(Ordering::SeqCst) == 4)
            .await;
        let focus = h.consumer.consumers.ui_view.focus;
        let identities: Vec<_> = h
            .consumer
            .consumers
            .ui_view
            .blocks
            .iter()
            .map(|block| match &block.state {
                BlockState::Recording {
                    recording: Some(recording),
                    ..
                } => (block.id, *recording),
                state => panic!("session must be recording: {state:?}"),
            })
            .collect();
        let sessions = h.sessions();

        // Both enumeration outcomes return to the same authoritative reducer gate.
        h.key(Intent::AddBlock);
        h.settle(|h| h.consumer.consumers.ui_view.warning.is_some())
            .await;
        let view = &h.consumer.consumers.ui_view;
        assert_eq!(view.blocks.len(), 4);
        assert_eq!(
            view.warning.as_deref(),
            Some("session limit (4) reached; close a session to make room")
        );
        assert_eq!(view.focus, focus);
        for (block, (id, recording)) in view.blocks.iter().zip(&identities) {
            assert_eq!(block.id, *id);
            assert!(matches!(
                &block.state,
                BlockState::Recording {
                    recording: Some(current),
                    ..
                } if current == recording
            ));
        }
        assert_eq!(h.sessions(), sessions);
        assert_eq!(h.capture.calls.load(Ordering::SeqCst), 4);
        let conn = rusqlite::Connection::open(h._tmp.path().join("index.sqlite")).unwrap();
        let rows: u64 = conn
            .query_row("SELECT COUNT(*) FROM recordings", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 4);
        h.quit().await;
        assert_eq!(h.capture.calls.load(Ordering::SeqCst), 4);
    }
}

#[tokio::test]
async fn selected_output_app_is_captured_and_persisted_without_default_mic_fallback() {
    let mut h = Harness::new();
    let snapshot = sample_source_snapshot();
    h.process(vec![AppEvent::AddSourceBlock {
        snapshot: snapshot.clone(),
    }]);
    h.key(Intent::PickerNext);
    h.key(Intent::Confirm);
    h.key(Intent::Confirm);
    h.key(Intent::Confirm);
    h.settle(|h| h.capture.calls.load(Ordering::SeqCst) == 1 && !h.sessions().is_empty())
        .await;
    assert_eq!(
        h.capture.sources.lock().unwrap()[0],
        voice_bird_cli::audio::CaptureSource::App {
            device_name: snapshot.devices[1].name.clone(),
            app: snapshot.apps[0].clone(),
        }
    );
    h.quit().await;
    let dir = &h.sessions()[0].1;
    let meta: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("meta.json")).unwrap()).unwrap();
    assert_eq!(meta["source"]["kind"], "app");
    assert_eq!(meta["source"]["device_name"], "Speakers");
    assert_eq!(meta["source"]["app"]["id"], "com.apple.Safari");
}

#[tokio::test]
async fn queued_start_and_stop_in_one_batch_finalize_without_reopening_capture() {
    let mut h = Harness::new();
    let request = h.take_queued_start();
    let recording = match &request {
        AppEvent::RecordingRequested { request } => request.recording,
        _ => unreachable!(),
    };
    h.process(vec![
        request,
        AppEvent::RecordingStopRequested {
            block: 1,
            recording,
        },
    ]);
    h.settle(|h| {
        h.history.contains(&AppEvent::RecordingStopped {
            block: 1,
            recording,
        })
    })
    .await;
    h.process(vec![AppEvent::FocusMoved {
        direction: FocusMove::Next,
    }]);
    h.drain();
    assert!(matches!(
        h.consumer.consumers.ui_view.blocks[0].state,
        BlockState::Recording {
            recording: None,
            ..
        }
    ));
    h.quit().await;
    assert_eq!(h.sessions().len(), 1);
    let meta: serde_json::Value =
        serde_json::from_slice(&std::fs::read(h.sessions()[0].1.join("meta.json")).unwrap())
            .unwrap();
    assert_eq!(meta["status"], "stopped");
}

#[tokio::test]
async fn natural_capture_end_survives_consumer_rebuild_until_explicit_resume() {
    use voice_bird_cli::audio::{AudioCapture, CaptureChunk, CaptureSource, CaptureStream};

    struct EndingCapture(Arc<AtomicUsize>);
    struct EndingStream(bool);

    impl AudioCapture for EndingCapture {
        fn open(&self, _: &CaptureSource) -> anyhow::Result<Box<dyn CaptureStream>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(EndingStream(false)))
        }
    }

    impl CaptureStream for EndingStream {
        fn next_chunk(&mut self, _: Duration) -> anyhow::Result<CaptureChunk> {
            if self.0 {
                Ok(CaptureChunk::End)
            } else {
                self.0 = true;
                Ok(CaptureChunk::Data(vec![0.1; 16_000]))
            }
        }

        fn queued_samples(&self) -> u64 {
            0
        }
    }

    let mut h = Harness::new();
    let calls = Arc::new(AtomicUsize::new(0));
    h.producer = RecordingProducer::new(RecordingServices {
        capture: Arc::new(EndingCapture(calls.clone())),
        engines: Arc::new(FixtureTranscriberFactory {
            refine_finish: vec![committed("Naturally finalized.", 0, 1000)],
            ..Default::default()
        }),
        sessions_root: h._tmp.path().join("sessions"),
    });
    let request = h.take_queued_start();
    let recording = match &request {
        AppEvent::RecordingRequested { request } => request.recording,
        _ => unreachable!(),
    };
    h.process(vec![request.clone()]);
    h.settle(|h| {
        h.history.contains(&AppEvent::RecordingStopped {
            block: 1,
            recording,
        })
    })
    .await;
    let old_dir = h.sessions()[0].1.clone();
    assert_eq!(
        std::fs::read_to_string(old_dir.join("transcript.txt")).unwrap(),
        "Naturally finalized.\n"
    );
    h.consumer.consumers.recording = RecordingConsumer::new(h.store.clone());
    h.process(vec![
        request,
        AppEvent::FocusMoved {
            direction: FocusMove::Next,
        },
    ]);
    h.drain();
    assert!(matches!(
        h.consumer.consumers.ui_view.blocks[0].state,
        BlockState::Recording {
            recording: None,
            ..
        }
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(h.sessions().len(), 1);
    // Leaving Recording clears natural-end suppression; explicit resume gets a fresh identity.
    h.process(vec![AppEvent::ModelMissing(LANGUAGES[0].live)]);
    assert!(matches!(
        h.consumer.consumers.ui_view.blocks[0].state,
        BlockState::Waiting { .. }
    ));
    h.process(vec![AppEvent::ModelAlreadyCached(LANGUAGES[0].live)]);
    h.settle(|h| {
        h.sessions().len() == 2
            && h.history.iter().any(|event| {
                matches!(event,
                AppEvent::RecordingStopped { recording: id, .. }
                    if *id == h.sessions()[1].0)
            })
    })
    .await;
    let (new_id, new_dir) = h.sessions()[1].clone();
    assert_ne!(new_id, recording);
    assert_ne!(new_dir, old_dir);
    assert_eq!(
        std::fs::read_to_string(new_dir.join("transcript.txt")).unwrap(),
        "Naturally finalized.\n"
    );
    h.quit().await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(h.sessions().len(), 2);
    let conn = rusqlite::Connection::open(h._tmp.path().join("index.sqlite")).unwrap();
    let row: (String, bool) = conn
        .query_row(
            "SELECT status, ended_at IS NOT NULL FROM recordings WHERE id = ?1",
            [recording],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(row, ("stopped".into(), true));
}

#[tokio::test]
async fn queued_start_during_shutdown_fails_without_opening_capture() {
    let mut h = Harness::new();
    let request = h.take_queued_start();
    let recording = match &request {
        AppEvent::RecordingRequested { request } => request.recording,
        _ => unreachable!(),
    };
    h.producer.begin_shutdown();
    h.process(vec![request]);
    h.settle(|h| {
        matches!(&h.consumer.consumers.ui_view.blocks[0].state,
            BlockState::Failed { error, .. } if error.contains("during shutdown"))
    })
    .await;
    assert!(h.history.iter().any(|event| {
        matches!(event, AppEvent::RecordingFailed { recording: id, .. } if *id == recording)
    }));
    assert_eq!(h.capture.calls.load(Ordering::SeqCst), 0);
    assert!(h.sessions().is_empty());
    assert!(h.producer.shutdown_finished());
    h.quit().await;
}
