use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use voice_bird_next::audio::{AudioCapture, CaptureChunk, CaptureSource, CaptureStream};
use voice_bird_next::bus::{AppEvent, EventBus};
use voice_bird_next::db::{self, Database};
use voice_bird_next::language::LANGUAGES;
use voice_bird_next::producer::recording::{next_recording_id, RecordingProducer, RecordingRequest, RecordingServices};
use voice_bird_next::testing::FixtureTranscriberFactory;
use voice_bird_next::transcribe::{Transcriber, TranscriberFactory};

struct ProgressCapture(Arc<AtomicU64>);
struct ProgressStream { samples: Arc<AtomicU64>, delivered: bool }
impl AudioCapture for ProgressCapture {
    fn open(&self, _: &CaptureSource) -> anyhow::Result<Box<dyn CaptureStream>> {
        Ok(Box::new(ProgressStream { samples: self.0.clone(), delivered: false }))
    }
}
impl CaptureStream for ProgressStream {
    fn next_chunk(&mut self, _: Duration) -> anyhow::Result<CaptureChunk> {
        if !self.delivered {
            self.delivered = true;
            self.samples.fetch_add(16_000, Ordering::Release);
            Ok(CaptureChunk::Data(vec![0.25; 16_000]))
        } else {
            std::thread::sleep(Duration::from_millis(10));
            Ok(CaptureChunk::Timeout)
        }
    }
    fn queued_samples(&self) -> u64 { 0 }
}
struct LoadingFactory {
    entered: mpsc::Sender<()>,
    release: Mutex<mpsc::Receiver<()>>,
    fixture: FixtureTranscriberFactory,
}
impl TranscriberFactory for LoadingFactory {
    fn live(&self, model: &Path, language: &str) -> anyhow::Result<Box<dyn Transcriber>> {
        self.entered.send(())?;
        self.release.lock().unwrap().recv()?;
        self.fixture.live(model, language)
    }
    fn refine(&self, model: &Path, language: &str) -> anyhow::Result<Box<dyn Transcriber>> {
        self.fixture.refine(model, language)
    }
}

#[tokio::test]
async fn capture_drains_source_while_model_initialization_is_blocked() {
    let tmp = tempfile::tempdir().unwrap();
    let mut bus = EventBus::new();
    let mut db = Database::open(&tmp.path().join("sessions.sqlite"), bus.sender()).unwrap();
    let samples = Arc::new(AtomicU64::new(0));
    let (entered, started) = mpsc::channel();
    let (release, waiting) = mpsc::channel();
    let mut producer = RecordingProducer::new(RecordingServices {
        capture: Arc::new(ProgressCapture(samples.clone())),
        engines: Arc::new(LoadingFactory { entered, release: Mutex::new(waiting), fixture: FixtureTranscriberFactory::default() }),
        sessions_root: tmp.path().join("sessions"),
    });
    let recording = next_recording_id();
    producer.start(RecordingRequest {
        block: 1, recording, source: CaptureSource::Mic { device_name: "test input".into() },
        language: &LANGUAGES[0], live_model: "live.gguf".into(), refine_model: "refine.gguf".into(),
    }, &bus.sender());
    tokio::task::spawn_blocking(move || started.recv_timeout(Duration::from_secs(2))).await.unwrap().unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
    while samples.load(Ordering::Acquire) < 16_000 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let captured_during_loading = samples.load(Ordering::Acquire);
    producer.stop(1);
    release.send(()).unwrap();
    assert_eq!(captured_during_loading, 16_000, "model initialization blocked source draining");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let event = tokio::time::timeout_at(deadline, bus.recv()).await.unwrap().unwrap();
        let admitted = db::apply(&mut db, &event).unwrap();
        if !admitted {
            if let AppEvent::TranscriptionRequested { request } = event { producer.reject_unaccepted(&request); }
            continue;
        }
        match event {
            AppEvent::TranscriptionRequested { request } => producer.dispatch(&request),
            AppEvent::RecordingStopped { block, recording: id } if id == recording => {
                producer.complete(block, id, false);
                break;
            }
            AppEvent::RecordingFailed { error, .. } => panic!("recording failed: {error}"),
            _ => {}
        }
    }
}
