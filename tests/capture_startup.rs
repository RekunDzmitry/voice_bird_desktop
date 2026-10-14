use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use voice_bird_cli::audio::buffer::PcmSlice;
use voice_bird_cli::audio::{AudioCapture, CaptureChunk, CaptureSource, CaptureStream};
use voice_bird_cli::bus::{
    next_monotonic_id, AppEvent, EventBus, RecordingRequest, TranscriptPass,
};
use voice_bird_cli::db::{self, Database};
use voice_bird_cli::language::LANGUAGES;
use voice_bird_cli::producer::recording::{RecordingProducer, RecordingServices};
use voice_bird_cli::transcribe::{Transcriber, TranscriberFactory, TranscriptOut};

struct ProgressCapture {
    samples: Arc<AtomicU64>,
    enabled: Arc<AtomicBool>,
}
struct ProgressStream {
    samples: Arc<AtomicU64>,
    enabled: Arc<AtomicBool>,
    delivered: bool,
}
impl AudioCapture for ProgressCapture {
    fn open(&self, _: &CaptureSource) -> anyhow::Result<Box<dyn CaptureStream>> {
        Ok(Box::new(ProgressStream {
            samples: self.samples.clone(),
            enabled: self.enabled.clone(),
            delivered: false,
        }))
    }
}
impl CaptureStream for ProgressStream {
    fn next_chunk(&mut self, _: Duration) -> anyhow::Result<CaptureChunk> {
        if !self.delivered && self.enabled.load(Ordering::Acquire) {
            self.delivered = true;
            Ok(CaptureChunk::Data(vec![0.25; 31 * 16_000]))
        } else {
            // A second read proves the previous chunk was appended successfully.
            if self.delivered {
                self.samples.store(31 * 16_000, Ordering::Release);
            }
            std::thread::sleep(Duration::from_millis(10));
            Ok(CaptureChunk::Timeout)
        }
    }
    fn queued_samples(&self) -> u64 {
        0
    }
}
struct LoadingFactory {
    pass: TranscriptPass,
    entered: mpsc::Sender<()>,
    release: Mutex<mpsc::Receiver<()>>,
}

impl LoadingFactory {
    fn load(&self, pass: TranscriptPass) -> anyhow::Result<Box<dyn Transcriber>> {
        if self.pass == pass {
            self.entered.send(())?;
            self.release.lock().unwrap().recv()?;
        }
        Ok(Box::new(SampleCounter(0)))
    }
}

impl TranscriberFactory for LoadingFactory {
    fn live(&self, _: &Path, _: &str) -> anyhow::Result<Box<dyn Transcriber>> {
        self.load(TranscriptPass::Live)
    }
    fn refine(&self, _: &Path, _: &str) -> anyhow::Result<Box<dyn Transcriber>> {
        self.load(TranscriptPass::Refined)
    }
}

struct SampleCounter(usize);

impl Transcriber for SampleCounter {
    fn feed(&mut self, pcm: &[f32]) -> anyhow::Result<Vec<TranscriptOut>> {
        self.0 += pcm.len();
        Ok(Vec::new())
    }

    fn feed_chunks(&mut self, chunks: &[PcmSlice]) -> anyhow::Result<Vec<TranscriptOut>> {
        for chunk in chunks {
            self.0 += chunk.samples().len();
        }
        Ok(Vec::new())
    }

    fn finish(&mut self) -> anyhow::Result<Vec<TranscriptOut>> {
        Ok(vec![TranscriptOut::Committed {
            t_start_ms: 0,
            t_end_ms: self.0 as u64 * 1000 / 16_000,
            text: format!("Captured {} samples.", self.0),
        }])
    }
}

#[tokio::test]
async fn capture_drains_source_while_model_initialization_is_blocked() {
    capture_during_initialization(TranscriptPass::Live).await;
}

#[tokio::test]
async fn capture_retains_startup_backlog_until_refinement_initialization_finishes() {
    capture_during_initialization(TranscriptPass::Refined).await;
}

async fn capture_during_initialization(pass: TranscriptPass) {
    let tmp = tempfile::tempdir().unwrap();
    let mut bus = EventBus::new();
    let mut db = Database::open(&tmp.path().join("sessions.sqlite"), bus.sender()).unwrap();
    let samples = Arc::new(AtomicU64::new(0));
    let enabled = Arc::new(AtomicBool::new(false));
    let (entered, started) = mpsc::channel();
    let (release, waiting) = mpsc::channel();
    let mut producer = RecordingProducer::new(RecordingServices {
        capture: Arc::new(ProgressCapture {
            samples: samples.clone(),
            enabled: enabled.clone(),
        }),
        engines: Arc::new(LoadingFactory {
            pass,
            entered,
            release: Mutex::new(waiting),
        }),
        sessions_root: tmp.path().join("sessions"),
    });
    let recording = next_monotonic_id();
    let start = AppEvent::RecordingRequested {
        request: Arc::new(RecordingRequest {
            block: 1,
            recording,
            source: CaptureSource::Mic {
                device_name: "test input".into(),
            },
            language: &LANGUAGES[0],
            live_model: "live.gguf".into(),
            refine_model: "refine.gguf".into(),
        }),
    };
    assert!(db::apply(&mut db, &start).unwrap());
    producer.produce(&start, &mut db, &bus.sender()).unwrap();
    tokio::task::spawn_blocking(move || started.recv_timeout(Duration::from_secs(2)))
        .await
        .unwrap()
        .unwrap();
    enabled.store(true, Ordering::Release);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while samples.load(Ordering::Acquire) < 31 * 16_000 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let captured_during_loading = samples.load(Ordering::Acquire);
    let stop = AppEvent::RecordingStopRequested {
        block: 1,
        recording,
    };
    assert!(db::apply(&mut db, &stop).unwrap());
    producer.produce(&stop, &mut db, &bus.sender()).unwrap();
    release.send(()).unwrap();
    assert_eq!(
        captured_during_loading,
        31 * 16_000,
        "model initialization blocked source draining"
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut session_dir = None;
    let mut committed = [false; 2];
    loop {
        let event = tokio::time::timeout_at(deadline, bus.recv())
            .await
            .unwrap()
            .unwrap();
        let admitted = db::apply(&mut db, &event).unwrap();
        if !admitted {
            if let AppEvent::TranscriptionRequested { request } = event {
                producer.reject_unaccepted(&request);
            }
            continue;
        }
        producer.produce(&event, &mut db, &bus.sender()).unwrap();
        match event {
            AppEvent::RecordingStarted {
                session_dir: dir, ..
            } => {
                session_dir = Some(std::path::PathBuf::from(dir));
            }
            AppEvent::TranscriptCommitted { pass, text, .. } => {
                assert_eq!(text, "Captured 496000 samples.");
                committed[usize::from(pass == TranscriptPass::Refined)] = true;
            }
            AppEvent::RecordingStopped { recording: id, .. } if id == recording => {
                break;
            }
            AppEvent::RecordingFailed { error, .. } => panic!("recording failed: {error}"),
            _ => {}
        }
    }
    assert_eq!(committed, [true; 2]);
    let session_dir = session_dir.expect("recording session directory");
    assert_eq!(
        std::fs::read_to_string(session_dir.join("transcript.txt")).unwrap(),
        "Captured 496000 samples.\n"
    );
    let metadata: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(session_dir.join("meta.json")).unwrap())
            .unwrap();
    assert_eq!(metadata["status"], "stopped");
}
