//! Persistent inference workers; PCM and exclusive decoder state stay off the bus.
use std::any::Any;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use crate::audio::buffer::PcmSlice;
use crate::bus::{AppEvent, EventSender, TranscriptPass, TranscriptionJob, TranscriptionMetrics};
use crate::db::inference_queue::Queue;
use crate::producer::inference_limits::InferenceLimits;
use crate::transcribe::{Transcriber, TranscriptOut};

pub struct InferenceTask {
    pub request: TranscriptionJob,
    pub engine: Box<dyn Transcriber>,
    pub chunks: Vec<PcmSlice>,
    pub origin: Instant,
    pub captured: Arc<AtomicU64>,
    pub consumed_end: u64,
    pub tx: EventSender,
    pub replies: mpsc::Sender<InferenceResult>,
}

pub struct InferenceResult {
    pub request: TranscriptionJob,
    pub engine: Option<Box<dyn Transcriber>>,
    pub output: Result<Vec<TranscriptOut>, String>,
    pub transcription: TranscriptionMetrics,
    /// Zero means the job was rejected before any worker started it.
    pub worker: u8,
}

struct State {
    db: Option<Queue>,
    tasks: Vec<Option<InferenceTask>>,
    limits: InferenceLimits,
    draining: bool,
    stopping: bool,
    terminal_error: Option<String>,
}

impl State {
    fn new(limits: InferenceLimits) -> Self {
        match Queue::new() {
            Ok(db) => Self {
                db: Some(db),
                tasks: Vec::new(),
                limits,
                draining: false,
                stopping: false,
                terminal_error: None,
            },
            Err(error) => Self {
                db: None,
                tasks: Vec::new(),
                limits,
                draining: false,
                stopping: true,
                terminal_error: Some(format!("initializing inference queue: {error}")),
            },
        }
    }
}

impl State {
    fn db(&self) -> &Queue {
        self.db.as_ref().expect("initialized inference queue")
    }

    fn db_mut(&mut self) -> &mut Queue {
        self.db.as_mut().expect("initialized inference queue")
    }

    /// Stop the pool and reject queued tasks; the caller still owns any unqueued task.
    fn fail_queue(&mut self, error: rusqlite::Error) {
        let error = format!("inference queue: {error}");
        self.stopping = true;
        self.terminal_error = Some(error.clone());
        for task in self.tasks.iter_mut().filter_map(Option::take) {
            reject(task, error.clone(), 0);
        }
    }

    fn dispatch(&mut self, request: &TranscriptionJob) -> bool {
        if self.stopping {
            return false;
        }
        match self.db().dispatch(request) {
            Ok(dispatched) => dispatched,
            Err(error) => {
                self.fail_queue(error);
                false
            }
        }
    }

    fn queue_jobs(&mut self, pass: TranscriptPass) -> u32 {
        if self.stopping {
            return 0;
        }
        match self.db().count(pass) {
            Ok(count) => count,
            Err(error) => {
                self.fail_queue(error);
                0
            }
        }
    }

    fn take(&mut self, worker: u8) -> Option<(InferenceTask, u32)> {
        if self.stopping {
            return None;
        }
        match self.take_ready(worker) {
            Ok(task) => task,
            Err(error) => {
                self.fail_queue(error);
                None
            }
        }
    }

    fn take_ready(&mut self, worker: u8) -> rusqlite::Result<Option<(InferenceTask, u32)>> {
        let limit = if self.draining {
            self.limits.shutdown()
        } else {
            self.limits.recording()
        };
        if worker == 0
            || usize::from(worker) > self.limits.workers()
            || self.db().active_count()? >= limit
            || self.db().worker_busy(worker)?
        {
            return Ok(None);
        }
        let (pass, recording) = match self.db().next_ready(TranscriptPass::Live)? {
            Some((recording, _)) => (TranscriptPass::Live, recording),
            None => match self.db().next_ready(TranscriptPass::Refined)? {
                Some((recording, _)) => (TranscriptPass::Refined, recording),
                None => return Ok(None),
            },
        };
        let slot = self.db_mut().claim(recording, pass, worker)?;
        let task = self.tasks[slot]
            .take()
            .expect("claimed private inference task");
        let queue_jobs = self.queue_jobs(pass);
        if self.stopping {
            reject(task, self.terminal_error.clone().expect("queue error"), 0);
            return Ok(None);
        }
        Ok(Some((task, queue_jobs)))
    }

    fn complete(&mut self, request: &TranscriptionJob, worker: u8) -> rusqlite::Result<()> {
        self.db().complete(request, worker)
    }

    fn remove_pending(
        &mut self,
        recording: u64,
        pass: TranscriptPass,
    ) -> rusqlite::Result<Option<InferenceTask>> {
        Ok(self.db().remove(recording, pass)?.map(|slot| {
            self.tasks[slot]
                .take()
                .expect("queued private inference task")
        }))
    }
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

impl Shared {
    fn new(limits: InferenceLimits) -> Self {
        Self {
            state: Mutex::new(State::new(limits)),
            changed: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }
}

pub struct InferenceScheduler {
    shared: Arc<Shared>,
    workers: Vec<JoinHandle<()>>,
}

impl InferenceScheduler {
    pub fn new() -> Arc<Self> {
        Self::with_limits(InferenceLimits::detect())
    }

    fn with_limits(limits: InferenceLimits) -> Arc<Self> {
        let shared = Arc::new(Shared::new(limits));
        let mut workers = Vec::with_capacity(limits.workers());
        for worker in 1..=limits.workers() as u8 {
            let worker_shared = shared.clone();
            if shared.lock().stopping {
                break;
            }
            match thread::Builder::new()
                .name(format!("inference-{worker}"))
                .spawn(move || worker_loop(worker_shared, worker))
            {
                Ok(handle) => workers.push(handle),
                Err(error) => {
                    // The public constructor is infallible. A failed pool
                    // rejects prepared jobs with real errors instead of using
                    // a partial pool with less capacity than its budgets promise.
                    let mut state = shared.lock();
                    state.terminal_error =
                        Some(format!("starting inference worker {worker}: {error}"));
                    state.stopping = true;
                    drop(state);
                    shared.changed.notify_all();
                    for handle in workers.drain(..) {
                        let _ = handle.join();
                    }
                    break;
                }
            }
        }
        Arc::new(Self { shared, workers })
    }

    /// Startup hardware-derived budgets; workers have no pass affinity.
    pub fn limits(&self) -> InferenceLimits {
        self.shared.lock().limits
    }

    /// Store the private PCM/decoder handoff before publishing its gate request.
    pub fn prepare(&self, task: InferenceTask) {
        let mut state = self.shared.lock();
        let error = if state.stopping {
            Some(
                state
                    .terminal_error
                    .clone()
                    .unwrap_or_else(|| "inference scheduler stopped".into()),
            )
        } else {
            match (|| -> rusqlite::Result<Option<String>> {
                if state.db().cancelled(task.request.recording)? {
                    Ok(Some("inference recording cancelled".into()))
                } else if state
                    .db()
                    .pending(task.request.recording, task.request.pass)?
                    .is_some()
                {
                    Ok(Some(
                        "an inference job is already pending for this recording and pass".into(),
                    ))
                } else if state.db().active_matches(&task.request)? {
                    Ok(Some("this inference job is already running".into()))
                } else {
                    Ok(None)
                }
            })() {
                Ok(error) => error,
                Err(error) => {
                    state.fail_queue(error);
                    state.terminal_error.clone()
                }
            }
        };
        if let Some(error) = error {
            task.tx.publish(AppEvent::TranscriptionRequested {
                request: task.request,
            });
            let queue_jobs = state.queue_jobs(task.request.pass);
            reject(task, error, queue_jobs);
            self.shared.changed.notify_all();
            return;
        }
        let request = task.request;
        let tx = task.tx.clone();
        let slot = state
            .tasks
            .iter()
            .position(Option::is_none)
            .unwrap_or(state.tasks.len());
        if let Err(error) = state.db().insert(&request, slot) {
            state.fail_queue(error);
            tx.publish(AppEvent::TranscriptionRequested { request });
            reject(task, state.terminal_error.clone().expect("queue error"), 0);
            self.shared.changed.notify_all();
            return;
        }
        if slot == state.tasks.len() {
            state.tasks.push(Some(task));
        } else {
            state.tasks[slot] = Some(task);
        }
        // Publishing under the same lock prevents cancellation from overtaking
        // Requested. A fast bus consumer cannot observe a missing private job.
        tx.publish(AppEvent::TranscriptionRequested { request });
    }

    /// Activate exactly the privately stored request after its database gate.
    pub fn dispatch(&self, request: &TranscriptionJob) -> bool {
        let dispatched = self.shared.lock().dispatch(request);
        self.shared.changed.notify_all();
        dispatched
    }

    pub fn begin_shutdown(&self) {
        self.shared.lock().draining = true;
        self.shared.changed.notify_all();
    }

    /// Return a private job rejected by admission, without cancelling a replay
    /// of an already-dispatched request.
    pub fn reject_unaccepted(&self, request: &TranscriptionJob) {
        let mut state = self.shared.lock();
        if state.stopping {
            return;
        }
        let result = (|| -> rusqlite::Result<()> {
            if state
                .db()
                .pending(request.recording, request.pass)?
                .is_some_and(|pending| !pending.dispatched && pending.request == *request)
            {
                let task = state
                    .remove_pending(request.recording, request.pass)?
                    .expect("matching pending request");
                let queue_jobs = state.queue_jobs(request.pass);
                reject(
                    task,
                    "transcription request rejected by database gate".into(),
                    queue_jobs,
                );
            }
            Ok(())
        })();
        if let Err(error) = result {
            state.fail_queue(error);
        }
        self.shared.changed.notify_all();
    }

    /// Terminally fence this recording and return jobs which never crossed the
    /// dispatch gate. Accepted jobs finish normally and keep their replies.
    pub fn cancel(&self, recording: u64) {
        let mut state = self.shared.lock();
        if state.stopping {
            return;
        }
        let result = (|| -> rusqlite::Result<()> {
            state.db().cancel(recording)?;
            while let Some(slot) = state.db().remove_unaccepted(recording)? {
                let task = state.tasks[slot]
                    .take()
                    .expect("cancelled private inference task");
                let queue_jobs = state.queue_jobs(task.request.pass);
                reject(
                    task,
                    "inference job cancelled before dispatch".into(),
                    queue_jobs,
                );
            }
            Ok(())
        })();
        if let Err(error) = result {
            state.fail_queue(error);
        }
        self.shared.changed.notify_all();
    }
}

impl Drop for InferenceScheduler {
    fn drop(&mut self) {
        let mut state = self.shared.lock();
        state.stopping = true;
        if let Some(db) = &state.db {
            if let Err(error) = db.clear_pending() {
                state.fail_queue(error);
            }
        }
        for task in state.tasks.iter_mut().filter_map(Option::take) {
            reject(task, "inference scheduler stopped".into(), 0);
        }
        drop(state);
        self.shared.changed.notify_all();
        // Quit already gave coordinators a bounded drain window. Never extend
        // it by waiting for a decode here. Unfinished workers detach and retain
        // Shared and their exclusive decoder until execution returns, then exit.
        self.workers.clear();
    }
}

fn worker_loop(shared: Arc<Shared>, worker: u8) {
    loop {
        let (task, queue_jobs) = {
            let mut state = shared.lock();
            loop {
                if state.stopping {
                    return;
                }
                if let Some(task) = state.take(worker) {
                    // Wake other idle workers to use any remaining backend budget.
                    shared.changed.notify_all();
                    break task;
                } else if state.stopping {
                    shared.changed.notify_all();
                    return;
                }
                state = shared
                    .changed
                    .wait(state)
                    .unwrap_or_else(|error| error.into_inner());
            }
        };
        execute(&shared, task, worker, queue_jobs);
    }
}

fn elapsed_ms(origin: Instant, time: Instant) -> u64 {
    time.saturating_duration_since(origin)
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn backlog(captured: &AtomicU64, consumed_end: u64) -> u64 {
    captured
        .load(Ordering::Acquire)
        .saturating_sub(consumed_end)
}

fn execute(shared: &Shared, task: InferenceTask, worker: u8, queue_jobs: u32) {
    let InferenceTask {
        request,
        mut engine,
        chunks,
        origin,
        captured,
        consumed_end,
        tx,
        replies,
    } = task;
    let start_ms = elapsed_ms(origin, Instant::now());
    let queue_wait_ms = start_ms.saturating_sub(request.queued_at_ms);
    tx.publish(AppEvent::TranscriptionStarted {
        request,
        worker,
        start_ms,
        queue_jobs,
        queue_size_start: backlog(&captured, consumed_end),
    });
    let inference_start = Instant::now();
    let queue_size_start = backlog(&captured, consumed_end);
    let result = catch_unwind(AssertUnwindSafe(
        || -> anyhow::Result<Vec<TranscriptOut>> {
            let mut output = engine.feed_chunks(&chunks)?;
            if request.finishing {
                output.extend(engine.finish()?);
            }
            Ok(output)
        },
    ));
    let inference_end = Instant::now();
    let transcription = TranscriptionMetrics {
        start_ms: elapsed_ms(origin, inference_start),
        end_ms: elapsed_ms(origin, inference_end),
        duration_ms: elapsed_ms(inference_start, inference_end),
        queue_size_start,
        queue_size_end: backlog(&captured, consumed_end),
    };
    let (engine, mut output) = match result {
        Ok(Ok(output)) => (Some(engine), Ok(output)),
        Ok(Err(error)) => (Some(engine), Err(format!("{error:#}"))),
        Err(panic) => (
            None,
            Err(format!(
                "transcription worker panicked: {}",
                panic_message(panic.as_ref())
            )),
        ),
    };
    let mut state = shared.lock();
    if let Err(error) = state.complete(&request, worker) {
        output = Err(format!("completing inference queue job: {error}"));
        state.fail_queue(error);
    }
    let queue_jobs = state.queue_jobs(request.pass);
    match &output {
        Ok(_) => tx.publish(AppEvent::TranscriptionFinished {
            request,
            worker,
            transcription,
            queue_wait_ms,
            queue_jobs,
        }),
        Err(error) => tx.publish(AppEvent::TranscriptionFailed {
            request,
            worker: Some(worker),
            transcription: Some(transcription),
            queue_wait_ms,
            queue_jobs,
            error: error.clone(),
        }),
    }
    drop(state);
    let _ = replies.send(InferenceResult {
        request,
        engine,
        output,
        transcription,
        worker,
    });
    shared.changed.notify_all();
}

fn panic_message(panic: &(dyn Any + Send)) -> &str {
    panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .unwrap_or("non-string panic payload")
}

fn reject(task: InferenceTask, error: String, queue_jobs: u32) {
    let time_ms = elapsed_ms(task.origin, Instant::now());
    let queue_size = backlog(&task.captured, task.consumed_end);
    task.tx.publish(AppEvent::TranscriptionFailed {
        request: task.request,
        worker: None,
        transcription: None,
        queue_wait_ms: time_ms.saturating_sub(task.request.queued_at_ms),
        queue_jobs,
        error: error.clone(),
    });
    let _ = task.replies.send(InferenceResult {
        request: task.request,
        engine: Some(task.engine),
        output: Err(error),
        transcription: TranscriptionMetrics {
            start_ms: time_ms,
            end_ms: time_ms,
            duration_ms: 0,
            queue_size_start: queue_size,
            queue_size_end: queue_size,
        },
        worker: 0,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::EventBus;
    use std::time::Duration;

    const DEADLINE: Duration = Duration::from_secs(5);

    #[derive(Default)]
    struct Accumulator {
        sum: f32,
        samples: u64,
    }

    impl Transcriber for Accumulator {
        fn feed(&mut self, pcm: &[f32]) -> anyhow::Result<Vec<TranscriptOut>> {
            self.sum += pcm.iter().sum::<f32>();
            self.samples += pcm.len() as u64;
            Ok(Vec::new())
        }

        fn feed_chunks(&mut self, chunks: &[PcmSlice]) -> anyhow::Result<Vec<TranscriptOut>> {
            for chunk in chunks {
                self.feed(chunk.samples())?;
            }
            Ok(Vec::new())
        }

        fn finish(&mut self) -> anyhow::Result<Vec<TranscriptOut>> {
            Ok(vec![TranscriptOut::Committed {
                t_start_ms: 0,
                t_end_ms: self.samples,
                text: format!("sum={};samples={}", self.sum, self.samples),
            }])
        }
    }

    fn request(recording: u64, job: u64, pass: TranscriptPass) -> TranscriptionJob {
        TranscriptionJob {
            block: 1,
            recording,
            job,
            pass,
            t_start_ms: 0,
            t_end_ms: 1,
            sample_count: 1,
            queued_at_ms: 0,
            finishing: false,
        }
    }

    fn task(
        request: TranscriptionJob,
        tx: EventSender,
        replies: mpsc::Sender<InferenceResult>,
    ) -> InferenceTask {
        InferenceTask {
            request,
            engine: Box::new(Accumulator::default()),
            chunks: vec![PcmSlice {
                pcm: Arc::new(vec![1.0]),
                range: 0..1,
            }],
            origin: Instant::now(),
            captured: Arc::new(AtomicU64::new(1)),
            consumed_end: 1,
            tx,
            replies,
        }
    }

    // Drive the production dispatcher synchronously to make scheduling order
    // deterministic, independent of OS thread wakeup order.
    struct Harness {
        scheduler: InferenceScheduler,
        bus: EventBus,
        replies: mpsc::Sender<InferenceResult>,
        results: mpsc::Receiver<InferenceResult>,
    }

    impl Harness {
        fn new() -> Self {
            Self::with_limits(InferenceLimits::for_system(6, 16 * 1024 * 1024 * 1024))
        }

        fn with_limits(limits: InferenceLimits) -> Self {
            let (replies, results) = mpsc::channel();
            Self {
                scheduler: InferenceScheduler {
                    shared: Arc::new(Shared::new(limits)),
                    workers: Vec::new(),
                },
                bus: EventBus::new(),
                replies,
                results,
            }
        }

        fn prepare(&self, request: TranscriptionJob) {
            self.scheduler
                .prepare(task(request, self.bus.sender(), self.replies.clone()));
        }

        fn queue(&self, request: TranscriptionJob) {
            self.prepare(request);
            assert!(self.scheduler.dispatch(&request));
        }

        fn take(&self, worker: u8) -> Option<(InferenceTask, u32)> {
            self.scheduler.shared.lock().take(worker)
        }

        fn finish(&self, worker: u8, taken: (InferenceTask, u32)) {
            execute(&self.scheduler.shared, taken.0, worker, taken.1);
        }

        fn result(&self) -> InferenceResult {
            self.results.recv_timeout(DEADLINE).expect("terminal reply")
        }
    }

    #[test]
    fn round_robin_serves_other_recordings_before_a_replenished_recording() {
        let h = Harness::new();
        for recording in [10, 20, 30] {
            h.queue(request(recording, 1, TranscriptPass::Live));
        }
        let first = h.take(1).expect("first live job");
        assert_eq!(first.0.request.recording, 10);
        h.queue(request(10, 2, TranscriptPass::Live));
        h.finish(1, first);
        for expected in [20, 30, 10] {
            let taken = h.take(1).expect("next live recording");
            assert_eq!(taken.0.request.recording, expected);
            h.finish(1, taken);
        }
    }

    #[test]
    fn every_worker_prioritizes_live_and_respects_the_recording_budget() {
        let h = Harness::new();
        h.queue(request(10, 1, TranscriptPass::Live));
        h.queue(request(20, 1, TranscriptPass::Refined));
        h.queue(request(30, 1, TranscriptPass::Refined));
        let live = h.take(2).expect("any worker takes queued live first");
        assert_eq!(live.0.request.pass, TranscriptPass::Live);
        let refine = h.take(1).expect("any other worker takes refinement");
        assert_eq!(refine.0.request.pass, TranscriptPass::Refined);
        assert!(h.take(3).is_none(), "recording concurrency budget is full");
        h.queue(request(10, 2, TranscriptPass::Live));
        h.finish(2, live);
        let next_live = h
            .take(3)
            .expect("next available worker prioritizes queued live");
        assert_eq!(next_live.0.request.pass, TranscriptPass::Live);
        h.finish(1, refine);
        let next_refine = h.take(2).expect("released worker handles refinement");
        assert_eq!(next_refine.0.request.pass, TranscriptPass::Refined);
        h.finish(3, next_live);
        h.finish(2, next_refine);
    }

    #[test]
    fn multiple_recordings_run_live_jobs_concurrently_without_lane_affinity() {
        let h = Harness::new();
        h.queue(request(10, 1, TranscriptPass::Live));
        h.queue(request(20, 1, TranscriptPass::Live));
        h.queue(request(30, 1, TranscriptPass::Refined));
        let first = h.take(3).expect("third worker serves live");
        let second = h.take(2).expect("second live recording runs concurrently");
        assert_eq!(first.0.request.recording, 10);
        assert_eq!(second.0.request.recording, 20);
        assert_eq!(second.0.request.pass, TranscriptPass::Live);
        assert!(h.take(1).is_none(), "both recording slots are occupied");
        h.finish(3, first);
        let refined = h.take(3).expect("the same worker can switch passes");
        assert_eq!(refined.0.request.pass, TranscriptPass::Refined);
        h.finish(2, second);
        h.finish(3, refined);
    }

    #[test]
    fn shutdown_uses_its_budget_without_overlapping_decoder_state() {
        let h = Harness::new();
        for recording in [10, 20, 30, 40] {
            h.queue(request(recording, 1, TranscriptPass::Live));
        }
        h.queue(request(50, 1, TranscriptPass::Refined));
        h.scheduler.begin_shutdown();
        let first = h.take(1).unwrap();
        let second = h.take(2).expect("second worker drains live");
        let third = h.take(3).expect("third existing worker also drains live");
        assert!(h.take(1).is_none(), "all three workers are busy");
        h.queue(request(10, 2, TranscriptPass::Live));
        h.finish(2, second);
        let fourth = h.take(2).unwrap();
        assert_eq!(
            fourth.0.request.recording, 40,
            "active decoder key cannot overlap"
        );
        h.finish(3, third);
        let refinement = h
            .take(3)
            .expect("unavailable live keys leave room for refinement");
        assert_eq!(refinement.0.request.pass, TranscriptPass::Refined);
        h.finish(3, refinement);
        assert!(
            h.take(3).is_none(),
            "pending state for recording 10 is still exclusive"
        );
        h.finish(1, first);
        let next = h
            .take(3)
            .expect("released state can migrate to an idle worker");
        assert_eq!(next.0.request.recording, 10);
        h.finish(2, fourth);
        h.finish(3, next);
    }

    #[test]
    fn hardware_budgets_bound_recording_and_shutdown_admission() {
        for (cpus, memory_gib) in [(1, 8), (6, 16), (16, 64), (32, 8), (32, 0)] {
            let limits = InferenceLimits::for_system(cpus, memory_gib * 1024 * 1024 * 1024);
            let h = Harness::with_limits(limits);
            for recording in 1..=limits.workers() as u64 + 1 {
                h.queue(request(recording, 1, TranscriptPass::Live));
            }
            let mut active = Vec::new();
            for worker in 1..=limits.workers() as u8 {
                if let Some(taken) = h.take(worker) {
                    active.push((worker, taken));
                }
            }
            assert_eq!(active.len(), limits.recording());
            assert!(h.take(limits.workers() as u8 + 1).is_none());
            h.scheduler.begin_shutdown();
            for worker in 1..=limits.workers() as u8 {
                if let Some(taken) = h.take(worker) {
                    active.push((worker, taken));
                }
            }
            assert_eq!(active.len(), limits.shutdown());
            for (worker, taken) in active {
                h.finish(worker, taken);
            }
            let next = h
                .take(1)
                .expect("completion releases capacity for queued work");
            assert_eq!(next.0.request.recording, limits.shutdown() as u64 + 1);
            h.finish(1, next);
        }
    }

    #[test]
    fn dispatch_rejects_mismatches_duplicates_and_pending_overflow() {
        let h = Harness::new();
        let first = request(10, 1, TranscriptPass::Live);
        h.prepare(first);
        let mut mismatched = first;
        mismatched.sample_count += 1;
        assert!(!h.scheduler.dispatch(&mismatched));
        let second = request(10, 2, TranscriptPass::Live);
        h.prepare(second);
        let rejected = h.result();
        assert_eq!(rejected.request, second);
        assert_eq!(rejected.worker, 0);
        assert!(rejected.output.unwrap_err().contains("already pending"));
        assert!(rejected.engine.is_some());
        assert!(h.scheduler.dispatch(&first));
        assert!(!h.scheduler.dispatch(&first));
        let taken = h.take(1).expect("original private task was retained");
        assert_eq!(taken.0.request, first);
        assert!(!h.scheduler.dispatch(&first));
        h.finish(1, taken);
        assert!(!h.scheduler.dispatch(&first));
    }

    #[test]
    fn admission_rejection_returns_state_without_interrupting_accepted_replays() {
        let h = Harness::new();
        let first = request(10, 1, TranscriptPass::Live);
        h.prepare(first);
        let mismatch = request(10, 2, TranscriptPass::Live);
        h.scheduler.reject_unaccepted(&mismatch);
        assert!(h.results.try_recv().is_err());
        h.scheduler.reject_unaccepted(&first);
        let rejected = h.result();
        assert_eq!(rejected.request, first);
        assert_eq!(rejected.worker, 0);
        assert!(rejected.output.is_err());
        assert!(rejected.engine.is_some());
        assert!(!h.scheduler.dispatch(&first));
        let accepted = request(10, 3, TranscriptPass::Live);
        h.queue(accepted);
        h.scheduler.reject_unaccepted(&accepted);
        let taken = h.take(1).expect("accepted replay must remain runnable");
        h.finish(1, taken);
        assert!(h.result().output.is_ok());
    }

    #[test]
    fn same_recording_and_pass_never_overlap_but_other_recordings_can_run() {
        let h = Harness::new();
        h.queue(request(10, 1, TranscriptPass::Refined));
        let first = h.take(2).expect("first refinement");
        h.queue(request(10, 2, TranscriptPass::Refined));
        assert!(h.take(3).is_none(), "same decoder key remains exclusive");
        h.queue(request(20, 1, TranscriptPass::Refined));
        let other = h.take(3).expect("another recording is independent");
        assert_eq!(other.0.request.recording, 20);
        h.finish(2, first);
        let second = h.take(2).expect("same key can run after completion");
        assert_eq!(second.0.request.job, 2);
        h.finish(3, other);
        h.finish(2, second);
    }

    #[test]
    fn cancellation_returns_undispatched_jobs_and_fences_late_submissions() {
        let mut h = Harness::new();
        let live = request(10, 1, TranscriptPass::Live);
        let refine = request(10, 2, TranscriptPass::Refined);
        h.prepare(live);
        h.prepare(refine);
        let accepted = request(20, 1, TranscriptPass::Refined);
        h.queue(accepted);
        h.scheduler.cancel(10);
        h.scheduler.cancel(20);
        for expected in [live, refine] {
            let result = h.result();
            assert_eq!(result.request, expected);
            assert_eq!(result.worker, 0);
            assert!(result.engine.is_some());
            assert!(result.output.unwrap_err().contains("cancelled"));
            assert!(!h.scheduler.dispatch(&expected));
        }
        let late = request(10, 3, TranscriptPass::Live);
        h.prepare(late);
        assert_eq!(h.result().request, late);
        assert!(!h.scheduler.dispatch(&late));
        let taken = h.take(2).expect("previously accepted job still completes");
        assert_eq!(taken.0.request, accepted);
        h.finish(2, taken);
        assert_eq!(h.result().request, accepted);
        let failures: Vec<_> = h
            .bus
            .drain()
            .filter_map(|event| match event {
                AppEvent::TranscriptionFailed {
                    request,
                    worker,
                    transcription,
                    ..
                } => Some((request, worker, transcription)),
                _ => None,
            })
            .collect();
        assert_eq!(
            failures,
            vec![(live, None, None), (refine, None, None), (late, None, None)]
        );
    }

    #[test]
    fn persistent_pool_returns_exclusive_state_and_reports_zero_output_jobs() {
        let scheduler = InferenceScheduler::new();
        let mut bus = EventBus::new();
        let (replies, results) = mpsc::channel();
        let live = request(10, 1, TranscriptPass::Live);
        let mut first = task(live, bus.sender(), replies.clone());
        first.chunks[0].pcm = Arc::new(vec![1.0, 2.0]);
        first.chunks[0].range = 0..2;
        scheduler.prepare(first);
        assert!(scheduler.dispatch(&live));
        let first = results.recv_timeout(DEADLINE).expect("initial live result");
        assert_eq!(first.output.unwrap(), Vec::<TranscriptOut>::new());
        let mut jobs = vec![
            (request(10, 2, TranscriptPass::Live), 3.0, first.engine),
            (request(10, 3, TranscriptPass::Refined), 10.0, None),
            (request(20, 4, TranscriptPass::Live), 100.0, None),
        ];
        for (mut request, sample, engine) in jobs.drain(..) {
            request.finishing = true;
            let mut next = task(request, bus.sender(), replies.clone());
            next.chunks[0].pcm = Arc::new(vec![sample]);
            if let Some(engine) = engine {
                next.engine = engine;
            }
            scheduler.prepare(next);
            assert!(scheduler.dispatch(&request));
        }
        let mut output = Vec::new();
        for _ in 0..3 {
            let result = results
                .recv_timeout(DEADLINE)
                .expect("independent decoder result");
            output.push((result.request.job, result.output.unwrap()));
        }
        output.sort_by_key(|(job, _)| *job);
        for ((actual_job, actual), (job, sum, samples)) in
            output.into_iter().zip([(2, 6, 3), (3, 10, 1), (4, 100, 1)])
        {
            assert_eq!(actual_job, job);
            assert_eq!(
                actual,
                vec![TranscriptOut::Committed {
                    t_start_ms: 0,
                    t_end_ms: samples,
                    text: format!("sum={sum};samples={samples}"),
                }]
            );
        }
        let events: Vec<_> = bus.drain().collect();
        for job in 1..=4 {
            let lifecycle: Vec<_> = events
                .iter()
                .filter_map(|event| match event {
                    AppEvent::TranscriptionRequested { request } if request.job == job => {
                        Some("queued")
                    }
                    AppEvent::TranscriptionStarted { request, .. } if request.job == job => {
                        Some("running")
                    }
                    AppEvent::TranscriptionFinished { request, .. } if request.job == job => {
                        Some("finished")
                    }
                    AppEvent::TranscriptionFailed { request, .. } if request.job == job => {
                        Some("failed")
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(lifecycle, ["queued", "running", "finished"]);
        }
    }

    enum Failure {
        Feed,
        Finish,
        Panic,
    }

    struct FailingEngine(Failure);

    impl Transcriber for FailingEngine {
        fn feed(&mut self, _: &[f32]) -> anyhow::Result<Vec<TranscriptOut>> {
            unreachable!("scheduler uses feed_chunks")
        }

        fn feed_chunks(&mut self, _: &[PcmSlice]) -> anyhow::Result<Vec<TranscriptOut>> {
            match self.0 {
                Failure::Feed => anyhow::bail!("decoder rejected input"),
                Failure::Panic => panic!("decoder panic"),
                Failure::Finish => Ok(Vec::new()),
            }
        }

        fn finish(&mut self) -> anyhow::Result<Vec<TranscriptOut>> {
            anyhow::bail!("decoder could not flush")
        }
    }

    #[test]
    fn ordinary_errors_return_state_panics_discard_it_and_both_release_the_key() {
        let scheduler = InferenceScheduler::new();
        let mut bus = EventBus::new();
        let (replies, results) = mpsc::channel();
        for (job, failure, expected, state_returned) in [
            (1, Failure::Feed, "decoder rejected input", true),
            (
                2,
                Failure::Panic,
                "transcription worker panicked: decoder panic",
                false,
            ),
            (3, Failure::Finish, "decoder could not flush", true),
        ] {
            let mut request = request(10, job, TranscriptPass::Refined);
            request.finishing = true;
            let mut next = task(request, bus.sender(), replies.clone());
            next.engine = Box::new(FailingEngine(failure));
            scheduler.prepare(next);
            assert!(scheduler.dispatch(&request));
            let result = results
                .recv_timeout(DEADLINE)
                .expect("failed decoder reply");
            assert_eq!(result.output.unwrap_err(), expected);
            assert_eq!(result.engine.is_some(), state_returned);
            let lifecycle: Vec<_> = bus
                .drain()
                .filter_map(|event| match event {
                    AppEvent::TranscriptionRequested { .. } => Some("queued"),
                    AppEvent::TranscriptionStarted { .. } => Some("running"),
                    AppEvent::TranscriptionFailed {
                        worker: Some(_),
                        transcription: Some(_),
                        ..
                    } => Some("failed"),
                    _ => None,
                })
                .collect();
            assert_eq!(lifecycle, ["queued", "running", "failed"]);
        }
    }

    struct GatedEngine {
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }

    impl Transcriber for GatedEngine {
        fn feed(&mut self, _: &[f32]) -> anyhow::Result<Vec<TranscriptOut>> {
            unreachable!("scheduler uses feed_chunks")
        }

        fn feed_chunks(&mut self, _: &[PcmSlice]) -> anyhow::Result<Vec<TranscriptOut>> {
            self.entered.send(())?;
            self.release.recv_timeout(DEADLINE)?;
            Ok(Vec::new())
        }

        fn finish(&mut self) -> anyhow::Result<Vec<TranscriptOut>> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn dropping_the_pool_does_not_wait_for_orphaned_refinement() {
        let scheduler =
            InferenceScheduler::with_limits(InferenceLimits::for_system(2, 8 * 1024 * 1024 * 1024));
        let shared = Arc::downgrade(&scheduler.shared);
        let mut bus = EventBus::new();
        let (replies, results) = mpsc::channel();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let active = request(10, 1, TranscriptPass::Refined);
        let mut next = task(active, bus.sender(), replies);
        next.engine = Box::new(GatedEngine {
            entered: entered_tx,
            release: release_rx,
        });
        scheduler.prepare(next);
        assert!(scheduler.dispatch(&active));
        entered_rx
            .recv_timeout(DEADLINE)
            .expect("refinement started");
        // A recording write error can drop the coordinator's reply receiver
        // while its accepted refinement job is still decoding.
        drop(results);

        let (replies, results) = mpsc::channel();
        let pending = request(20, 2, TranscriptPass::Live);
        scheduler.prepare(task(pending, bus.sender(), replies));
        assert!(scheduler.dispatch(&pending));
        scheduler.begin_shutdown();
        let (dropped_tx, dropped_rx) = mpsc::channel();
        let shutdown = thread::spawn(move || {
            drop(scheduler);
            dropped_tx.send(()).unwrap();
        });
        let dropped = dropped_rx.recv_timeout(Duration::from_secs(1));
        let rejected = results
            .recv_timeout(DEADLINE)
            .expect("pending job rejected");
        // Release the decoder before asserting, so even a blocking-drop
        // regression cannot strand a test worker.
        release_tx.send(()).expect("release refinement");
        shutdown.join().expect("shutdown thread");
        wait_for_workers(&shared);

        assert!(
            dropped.is_ok(),
            "scheduler teardown waited for an orphaned refinement decode"
        );
        assert_eq!(rejected.request, pending);
        assert_eq!(rejected.worker, 0);
        assert_eq!(rejected.output.unwrap_err(), "inference scheduler stopped");
        assert!(rejected.engine.is_some());
        let events: Vec<_> = bus.drain().collect();
        assert!(events.iter().any(|event| matches!(
            event,
            AppEvent::TranscriptionFinished { request, .. } if *request == active
        )));
        assert!(!events.iter().any(|event| matches!(
            event,
            AppEvent::TranscriptionStarted { request, .. } if *request == pending
        )));
    }

    fn wait_for_workers(shared: &std::sync::Weak<Shared>) {
        let deadline = Instant::now() + DEADLINE;
        while shared.strong_count() != 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(
            shared.upgrade().is_none(),
            "workers released their shared state"
        );
    }

    #[test]
    fn capture_progress_during_inference_is_measured_without_current_job_samples() {
        let scheduler = InferenceScheduler::new();
        let bus = EventBus::new();
        let (replies, results) = mpsc::channel();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let request = request(10, 1, TranscriptPass::Live);
        let mut next = task(request, bus.sender(), replies);
        let captured = Arc::new(AtomicU64::new(10));
        next.captured = captured.clone();
        next.consumed_end = 4;
        next.engine = Box::new(GatedEngine {
            entered: entered_tx,
            release: release_rx,
        });
        scheduler.prepare(next);
        assert!(scheduler.dispatch(&request));
        entered_rx
            .recv_timeout(DEADLINE)
            .expect("inference started");
        captured.store(25, Ordering::Release);
        release_tx.send(()).expect("release inference");
        let result = results.recv_timeout(DEADLINE).expect("inference result");
        assert_eq!(result.transcription.queue_size_start, 6);
        assert_eq!(result.transcription.queue_size_end, 21);
        assert_eq!(captured.load(Ordering::Acquire), 25);
        assert_eq!(result.output.unwrap(), Vec::<TranscriptOut>::new());
    }

    #[test]
    fn dropping_the_pool_releases_idle_workers_and_returns_prepared_decoder_state() {
        let scheduler = InferenceScheduler::new();
        let shared = Arc::downgrade(&scheduler.shared);
        let bus = EventBus::new();
        let (replies, results) = mpsc::channel();
        let prepared = request(10, 1, TranscriptPass::Live);
        scheduler.prepare(task(prepared, bus.sender(), replies));
        drop(scheduler);
        let result = results.recv_timeout(DEADLINE).expect("shutdown reply");
        assert_eq!(result.request, prepared);
        assert_eq!(result.worker, 0);
        assert_eq!(result.output.unwrap_err(), "inference scheduler stopped");
        assert!(result.engine.is_some());
        wait_for_workers(&shared);
    }

    #[test]
    fn a_pool_initialization_error_is_returned_instead_of_stranding_a_job() {
        let h = Harness::new();
        {
            let mut state = h.scheduler.shared.lock();
            state.stopping = true;
            state.terminal_error = Some("starting inference worker 2: unavailable".into());
        }
        let request = request(10, 1, TranscriptPass::Live);
        h.prepare(request);
        let result = h.result();
        assert_eq!(
            result.output.unwrap_err(),
            "starting inference worker 2: unavailable"
        );
        assert_eq!(result.worker, 0);
        assert!(result.engine.is_some());
        assert!(!h.scheduler.dispatch(&request));
    }

    #[test]
    fn failed_queue_write_returns_all_pending_decoders_without_execution() {
        let mut h = Harness::new();
        let accepted = request(10, 1, TranscriptPass::Live);
        h.queue(accepted);
        h.scheduler
            .shared
            .lock()
            .db()
            .abort_inserts_for_test()
            .unwrap();
        let failed = request(20, 2, TranscriptPass::Refined);
        h.prepare(failed);
        for expected in [accepted, failed] {
            let result = h.result();
            assert_eq!(result.request, expected);
            assert_eq!(result.worker, 0);
            assert!(result.engine.is_some());
            assert!(result.output.unwrap_err().contains("inference queue"));
        }
        for worker in 1..=h.scheduler.limits().workers() as u8 {
            assert!(h.take(worker).is_none());
        }
        let late = request(30, 3, TranscriptPass::Live);
        h.prepare(late);
        assert_eq!(h.result().request, late);
        assert!(!h.scheduler.dispatch(&late));
        assert!(!h
            .bus
            .drain()
            .any(|event| matches!(event, AppEvent::TranscriptionStarted { .. })));
    }
}
