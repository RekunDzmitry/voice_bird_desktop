//! Full recording lifecycle with isolated PCM/model fixtures; no native devices or network.
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use voice_bird_next::bus::{AppEvent, EventBus, TranscriptPass};
use voice_bird_next::consumer::ui_view::BlockState;
use voice_bird_next::consumer::{Consumer, Consumers};
use voice_bird_next::db::Database;
use voice_bird_next::input::Intent;
use voice_bird_next::language::LANGUAGES;
use voice_bird_next::producer::input::resolve_intent;
use voice_bird_next::producer::model_watch::ModelWatcher;
use voice_bird_next::producer::recording::RecordingServices;
use voice_bird_next::testing::{
    sample_source_snapshot, FixtureCapture, FixtureDownloader, FixtureSources, FixtureStore,
    FixtureTranscriberFactory, Outcome,
};
use voice_bird_next::transcribe::TranscriptOut;

struct Harness {
    consumer: Consumer,
    db: Database,
    bus: EventBus,
    capture: Arc<FixtureCapture>,
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
            &LANGUAGES[0].models().iter().map(|model| model.id).collect::<Vec<_>>(),
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
            store,
            Arc::new(FixtureSources(None)),
            RecordingServices {
                capture: capture.clone(),
                engines,
                sessions_root: tmp.path().join("sessions"),
            },
        ));
        Self {
            consumer,
            db,
            bus,
            capture,
            history: Vec::new(),
            _tmp: tmp,
        }
    }

    fn process(&mut self, events: Vec<AppEvent>) {
        let accepted: Vec<_> = events
            .into_iter()
            .filter(|event| {
                let admitted = voice_bird_next::db::apply(&mut self.db, event).unwrap();
                if !admitted {
                    if let AppEvent::TranscriptionRequested { request } = event {
                        self.consumer.consumers.recording.producer.reject_unaccepted(request);
                    }
                }
                admitted
            })
            .collect();
        self.consumer
            .consume(&accepted, &mut self.db, &self.bus.sender());
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

    fn start(&mut self) {
        self.process(vec![AppEvent::AddBlock]);
        self.process(vec![AppEvent::BeginLanguage {
            block: 1,
            language: &LANGUAGES[0],
            source_rev: None,
        }]);
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
        self.consumer.consumers.recording.producer.begin_shutdown();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while !self.consumer.consumers.recording.producer.shutdown_finished() {
            match tokio::time::timeout_at(deadline, self.bus.recv()).await {
                Ok(Some(event)) => self.process(vec![event]),
                Ok(None) | Err(_) => break,
            }
        }
        self.consumer.consumers.recording.producer.finish_shutdown(&self.bus.sender());
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
    h.process(vec![AppEvent::MenuOpened, AppEvent::MenuClosed]);
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
    h.settle(|h| !h.sessions().is_empty()).await;
    let (old_id, old_dir) = h.sessions()[0].clone();
    h.process(vec![AppEvent::ModelMissing(LANGUAGES[0].live)]);
    assert!(matches!(
        h.consumer.consumers.ui_view.blocks[0].state,
        BlockState::Waiting { .. }
    ));
    // The fixture model remains installed: the queued cached reply resumes
    // immediately, even before the first worker has observed its stop flag.
    h.settle(|h| h.sessions().len() == 2).await;
    let (new_id, new_dir) = h.sessions()[1].clone();
    assert_ne!(old_id, new_id);
    assert_ne!(old_dir, new_dir);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(
            &std::fs::read(old_dir.join("meta.json")).unwrap()
        )
        .unwrap()["status"],
        "stopped"
    );
    h.process(vec![
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
            transcription: voice_bird_next::bus::TranscriptionMetrics::default(),
            job: 0,
            worker: 1,
        },
    ]);
    assert!(matches!(h.consumer.consumers.ui_view.blocks[0].state,
        BlockState::Recording { recording: Some(id), .. } if id == new_id));
    h.quit().await;
    assert!(new_dir.join("transcript.txt").is_file());
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
async fn quit_finalizes_visible_and_hidden_blocks() {
    let mut h = Harness::new();
    h.start();
    h.process(vec![
        AppEvent::AddBlock,
        AppEvent::BeginLanguage {
            block: 2,
            language: &LANGUAGES[0],
            source_rev: None,
        },
    ]);
    h.drain();
    h.consumer.consumers.ui_view.blocks[0].visible = false;
    h.settle(|h| h.sessions().len() == 2 && h.capture.calls.load(Ordering::SeqCst) == 2)
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
        voice_bird_next::audio::CaptureSource::App {
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
