//! Per-recording coordinators drain native capture independently of inference.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use tokio::task::JoinHandle;

use crate::audio::buffer::CaptureBuffer;
use crate::audio::{AudioCapture, CaptureChunk, CaptureSource};
use crate::bus::{AppEvent, EventSender, TranscriptionJob, TranscriptPass};
use crate::language::LanguageProfile;
use crate::session_store::SessionWriter;
use crate::transcribe::{Transcriber, TranscriberFactory, TranscriptOut};
use super::inference::{InferenceScheduler, InferenceTask};

pub struct RecordingServices {
    pub capture: Arc<dyn AudioCapture>,
    pub engines: Arc<dyn TranscriberFactory>,
    pub sessions_root: PathBuf,
}

struct Handle {
    recording: u64,
    stop: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

pub struct RecordingProducer {
    services: RecordingServices,
    registry: HashMap<u8, Handle>,
    scheduler: Arc<InferenceScheduler>,
}

pub struct RecordingRequest {
    pub block: u8,
    pub recording: u64,
    pub source: CaptureSource,
    pub language: &'static LanguageProfile,
    pub live_model: PathBuf,
    pub refine_model: PathBuf,
}

pub fn next_recording_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock precedes Unix epoch")
        .as_micros() as u64;
    NEXT.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |previous| {
        Some(now.max(previous + 1))
    })
    .unwrap()
    .max(now - 1)
        + 1
}

impl RecordingProducer {
    pub fn new(services: RecordingServices) -> Self {
        Self {
            services,
            registry: HashMap::new(),
            scheduler: InferenceScheduler::new(),
        }
    }

    pub fn contains(&self, block: u8) -> bool {
        self.registry.contains_key(&block)
    }

    pub fn blocks(&self) -> Vec<u8> {
        self.registry.keys().copied().collect()
    }

    pub fn start(&mut self, request: RecordingRequest, tx: &EventSender) {
        if self.contains(request.block) {
            return;
        }
        let block = request.block;
        let recording = request.recording;
        let capture = self.services.capture.clone();
        let engines = self.services.engines.clone();
        let root = self.services.sessions_root.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let scheduler = self.scheduler.clone();
        let worker_stop = stop.clone();
        let tx = tx.clone();
        let task = tokio::spawn(async move {
            let worker_tx = tx.clone();
            let result = tokio::task::spawn_blocking(move || {
                worker(request, capture, engines, root, worker_stop, worker_tx, scheduler)
            })
            .await;
            match result {
                Ok(Ok(())) => tx.publish(AppEvent::RecordingStopped { block, recording }),
                Ok(Err(error)) => tx.publish(AppEvent::RecordingFailed {
                    block,
                    recording,
                    error: format!("{error:#}"),
                }),
                Err(error) => tx.publish(AppEvent::RecordingFailed {
                    block,
                    recording,
                    error: format!("recording worker: {error}"),
                }),
            }
        });
        self.registry.insert(
            block,
            Handle {
                recording,
                stop,
                task,
            },
        );
    }

    pub fn stop(&mut self, block: u8) {
        if let Some(handle) = self.registry.get(&block) {
            handle.stop.store(true, Ordering::Release);
        }
    }

    /// Retain naturally ended sessions until the block leaves Recording; do not
    /// reopen a disconnected source automatically. A requested stop permits resume.
    pub fn complete(&mut self, block: u8, recording: u64, failed: bool) {
        if self.registry.get(&block).is_some_and(|handle| {
            handle.recording == recording && (failed || handle.stop.load(Ordering::Acquire))
        }) {
            self.registry.remove(&block);
        }
    }

    pub fn reap_stopped(&mut self) {
        self.registry.retain(|_, handle| {
            !(handle.stop.load(Ordering::Acquire) && handle.task.is_finished())
        });
    }

    /// Job requests must keep traversing the event/database gate while stopping.
    pub fn begin_shutdown(&mut self) {
        for handle in self.registry.values() {
            handle.stop.store(true, Ordering::Release);
        }
        self.scheduler.begin_shutdown();
    }

    pub fn shutdown_finished(&self) -> bool {
        self.registry.values().all(|handle| handle.task.is_finished())
    }

    pub fn finish_shutdown(&mut self, tx: &EventSender) {
        for (block, handle) in self.registry.drain() {
            if !handle.task.is_finished() {
                self.scheduler.cancel(handle.recording);
                tx.publish(AppEvent::RecordingFailed {
                    block,
                    recording: handle.recording,
                    error: "recording finalization exceeded shutdown deadline".into(),
                });
            }
        }
    }

    pub fn dispatch(&self, request: &TranscriptionJob) {
        self.scheduler.dispatch(request);
    }

    pub fn reject_unaccepted(&self, request: &TranscriptionJob) {
        self.scheduler.reject_unaccepted(request);
    }
}

impl Drop for RecordingProducer {
    fn drop(&mut self) {
        for handle in self.registry.values() {
            handle.stop.store(true, Ordering::Release);
            self.scheduler.cancel(handle.recording);
        }
    }
}

fn worker(
    request: RecordingRequest,
    capture: Arc<dyn AudioCapture>,
    engines: Arc<dyn TranscriberFactory>,
    root: PathBuf,
    stop: Arc<AtomicBool>,
    tx: EventSender,
    scheduler: Arc<InferenceScheduler>,
) -> Result<()> {
    let mut session = SessionWriter::create(
        &root,
        request.block,
        request.recording,
        &request.source,
        request.language.code,
        request.language.live.id,
        request.language.refine.id,
    )?;
    tx.publish(AppEvent::RecordingStarted {
        block: request.block,
        recording: request.recording,
        session_dir: session.path().to_string_lossy().into_owned(),
        source: request.source.clone(),
        language: request.language.code.into(),
        started_at: session.started_at().into(),
    });
    let result = capture_and_transcribe(
        &request,
        capture,
        engines.as_ref(),
        stop,
        &tx,
        scheduler,
        &mut session,
    );
    let finalization = session.finalize(if result.is_ok() { "stopped" } else { "failed" });
    match (result, finalization) {
        (Err(error), Err(finalize)) => {
            Err(error.context(format!("session finalization: {finalize:#}")))
        }
        (Err(error), Ok(())) => Err(error),
        (Ok(()), result) => result,
    }
}

fn capture_and_transcribe(
    request: &RecordingRequest,
    capture: Arc<dyn AudioCapture>,
    engines: &dyn TranscriberFactory,
    stop: Arc<AtomicBool>,
    tx: &EventSender,
    scheduler: Arc<InferenceScheduler>,
    session: &mut SessionWriter,
) -> Result<()> {
    if stop.load(Ordering::Acquire) {
        return Ok(());
    }
    let buffer = Arc::new(CaptureBuffer::default());
    let capture_buffer = buffer.clone();
    let capture_stop = stop.clone();
    let source = request.source.clone();
    let capture_thread = std::thread::Builder::new()
        .name(format!("capture-{}", request.recording))
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
                if capture_stop.load(Ordering::Acquire) {
                    return Ok(());
                }
                let mut stream = capture.open(&source)?;
                capture_buffer.opened();
                let capture_result = (|| -> Result<()> {
                    while !capture_stop.load(Ordering::Acquire) {
                        match stream.next_chunk(Duration::from_millis(100))? {
                            CaptureChunk::Data(pcm) => capture_buffer.append(pcm)?,
                            CaptureChunk::Timeout => {}
                            CaptureChunk::End => break,
                        }
                    }
                    Ok(())
                })();
                // Stop callbacks even after a buffer/read error; retain any usable tail.
                let tail_result = stream.finish().and_then(|tail| capture_buffer.append(tail));
                capture_result.and(tail_result)
            }));
            let error = match result {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(format!("{error:#}")),
                Err(_) => Some("audio capture worker panicked".into()),
            };
            capture_buffer.finish(error);
        })?;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
        loop {
            let pending = buffer.pending(0);
            if pending.origin.is_some() {
                break;
            }
            if pending.finished {
                return buffer.take_error().map_or(Ok(()), |error| Err(anyhow::anyhow!(error)));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        // Capture already owns the native stream: state/encoder initialization
        // cannot leave an apparently recording session without queued audio.
        let live = engines.live(&request.live_model, request.language.code)?;
        let refine = engines.refine(&request.refine_model, request.language.code)?;
        coordinate(request, [live, refine], &buffer, &stop, tx, &scheduler, session)
    }))
    .unwrap_or_else(|_| Err(anyhow::anyhow!("recording coordinator panicked")));
    stop.store(true, Ordering::Release);
    if result.is_err() {
        scheduler.cancel(request.recording);
    }
    let joined = capture_thread.join().map_err(|_| anyhow::anyhow!("audio capture worker panicked"));
    result.and(joined)
}

struct PassState {
    engine: Option<Box<dyn Transcriber>>,
    active: Option<u64>,
    done: bool,
    next_sample: u64,
}

fn coordinate(
    request: &RecordingRequest,
    engines: [Box<dyn Transcriber>; 2],
    buffer: &CaptureBuffer,
    stop: &AtomicBool,
    tx: &EventSender,
    scheduler: &InferenceScheduler,
    session: &mut SessionWriter,
) -> Result<()> {
    let [live, refine] = engines;
    let mut passes = [
        PassState { engine: Some(live), active: None, done: false, next_sample: 16_000 },
        PassState { engine: Some(refine), active: None, done: false, next_sample: 80_000 },
    ];
    let (replies, results) = mpsc::channel();
    let mut tentative = String::new();
    let mut first_error = None;
    loop {
        for (index, pass) in passes.iter_mut().enumerate() {
            if pass.done {
                buffer.discard(index);
                continue;
            }
            if pass.active.is_some() {
                continue;
            }
            let pending = buffer.pending(index);
            let Some(origin) = pending.origin else {
                if pending.finished {
                    return Err(anyhow::anyhow!(
                        buffer.take_error().unwrap_or_else(|| "capture ended before opening".into())
                    ));
                }
                continue;
            };
            if !pending.finished && pending.end < pass.next_sample {
                continue;
            }
            let end = if index == 0 {
                pending.end
            } else {
                pending.end.min(pending.start + 80_000)
            };
            let finishing = pending.finished && end == pending.end;
            let job = next_recording_id();
            let job_request = TranscriptionJob {
                block: request.block,
                recording: request.recording,
                job,
                pass: if index == 0 { TranscriptPass::Live } else { TranscriptPass::Refined },
                t_start_ms: pending.start * 1000 / 16_000,
                t_end_ms: end * 1000 / 16_000,
                sample_count: end - pending.start,
                queued_at_ms: origin.elapsed().as_millis() as u64,
                finishing,
            };
            let chunks = buffer.take(index, end);
            scheduler.prepare(InferenceTask {
                request: job_request,
                engine: pass.engine.take().expect("idle pass owns its engine"),
                chunks,
                origin,
                captured: buffer.captured.clone(),
                consumed_end: end,
                tx: tx.clone(),
                replies: replies.clone(),
            });
            pass.active = Some(job);
            pass.next_sample = end + if index == 0 { 12_000 } else { 80_000 };
        }
        if passes.iter().all(|pass| pass.done) {
            let capture_error = buffer.take_error();
            return first_error.or(capture_error).map_or(Ok(()), |error| Err(anyhow::anyhow!(error)));
        }
        match results.recv_timeout(Duration::from_millis(20)) {
            Ok(result) => {
                let index = usize::from(result.request.pass == TranscriptPass::Refined);
                let pass = &mut passes[index];
                if pass.active != Some(result.request.job) {
                    continue;
                }
                pass.active = None;
                pass.engine = result.engine;
                match result.output {
                    Ok(output) => {
                        emit(&result.request, result.worker, result.transcription,
                            output, session, tx, &mut tentative)?;
                        pass.done = result.request.finishing;
                    }
                    Err(error) => {
                        first_error.get_or_insert(error);
                        stop.store(true, Ordering::Release);
                        pass.done = true;
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(anyhow::anyhow!("inference workers disconnected"));
            }
        }
    }
}

fn emit(
    job: &TranscriptionJob,
    worker: u8,
    transcription: crate::bus::TranscriptionMetrics,
    output: Vec<TranscriptOut>,
    session: &mut SessionWriter,
    tx: &EventSender,
    tentative: &mut String,
) -> Result<()> {
    for out in output {
        match out {
            TranscriptOut::Tentative { text }
                if job.pass == TranscriptPass::Live && *tentative != text =>
            {
                tentative.clone_from(&text);
                tx.publish(AppEvent::TranscriptTentative {
                    block: job.block,
                    recording: job.recording,
                    job: job.job,
                    worker,
                    text,
                    transcription,
                });
            }
            TranscriptOut::Committed { t_start_ms, t_end_ms, text } => {
                session.append(job.pass, t_start_ms, t_end_ms, &text)?;
                tx.publish(AppEvent::TranscriptCommitted {
                    block: job.block,
                    recording: job.recording,
                    job: job.job,
                    worker,
                    pass: job.pass,
                    t_start_ms,
                    t_end_ms,
                    text,
                    transcription,
                });
            }
            TranscriptOut::Tentative { .. } => {}
        }
    }
    Ok(())
}
