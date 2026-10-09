//! Persistent inference workers; PCM and exclusive decoder state stay off the bus.
use std::any::Any;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use crate::audio::buffer::PcmSlice;
use crate::bus::{AppEvent, EventSender, TranscriptPass, TranscriptionJob, TranscriptionMetrics};
use crate::transcribe::{Transcriber, TranscriptOut};

const BACKEND_CONCURRENCY: usize = 2;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Key {
    recording: u64,
    lane: usize,
}

impl Key {
    fn for_request(request: &TranscriptionJob) -> Self {
        Self {
            recording: request.recording,
            lane: match request.pass {
                TranscriptPass::Live => 0,
                TranscriptPass::Refined => 1,
            },
        }
    }
}

struct Pending {
    task: InferenceTask,
    dispatched: bool,
}

#[derive(Default)]
struct RoundRobin {
    recordings: BTreeSet<u64>,
    last_served: Option<u64>,
}

#[derive(Default)]
struct State {
    pending: HashMap<Key, Pending>,
    active: HashMap<Key, TranscriptionJob>,
    cancelled: HashSet<u64>,
    ready: [RoundRobin; 2],
    worker_busy: [bool; 3],
    draining: bool,
    stopping: bool,
    initialization_error: Option<String>,
}

impl State {
    fn dispatch(&mut self, request: &TranscriptionJob) -> bool {
        let key = Key::for_request(request);
        let Some(pending) = self.pending.get_mut(&key) else {
            return false;
        };
        if self.stopping
            || self.cancelled.contains(&request.recording)
            || pending.dispatched
            || pending.task.request != *request
        {
            return false;
        }
        pending.dispatched = true;
        self.ready[key.lane].recordings.insert(key.recording);
        true
    }

    fn next_ready(&self, lane: usize) -> Option<Key> {
        let ready = &self.ready[lane];
        let available = |recording: &u64| {
            !self.active.contains_key(&Key {
                recording: *recording,
                lane,
            })
        };
        let recording = ready
            .last_served
            .and_then(|last| {
                use std::ops::Bound::{Excluded, Unbounded};
                ready
                    .recordings
                    .range((Excluded(last), Unbounded))
                    .find(|recording| available(recording))
            })
            .or_else(|| ready.recordings.iter().find(|recording| available(recording)))?;
        Some(Key {
            recording: *recording,
            lane,
        })
    }

    fn queue_jobs(&self, lane: usize) -> u32 {
        self.pending
            .keys()
            .filter(|key| key.lane == lane)
            .count()
            .try_into()
            .unwrap_or(u32::MAX)
    }

    fn take(&mut self, worker: u8) -> Option<(InferenceTask, u32)> {
        let limit = if self.draining { self.worker_busy.len() } else { BACKEND_CONCURRENCY };
        if self.stopping || self.active.len() >= limit {
            return None;
        }
        let worker_index = usize::from(worker - 1);
        if self.worker_busy[worker_index] {
            return None;
        }
        let preferred = if self.draining { 0 } else { usize::from(worker != 1) };
        let key = self.next_ready(preferred).or_else(|| {
            if self.draining { self.next_ready(1 - preferred) } else { None }
        })?;
        let lane = key.lane;
        // During recording, reserve a free backend slot for live work.
        // At Quit all three existing workers may drain either pass: keeping
        // final live hypotheses on a single lane can exceed the deadline.
        if !self.draining && lane == 1 && !self.worker_busy[0] && self.next_ready(0).is_some() {
            return None;
        }
        let pending = self.pending.remove(&key)?;
        self.ready[lane].recordings.remove(&key.recording);
        self.ready[lane].last_served = Some(key.recording);
        self.worker_busy[worker_index] = true;
        self.active.insert(key, pending.task.request);
        Some((pending.task, self.queue_jobs(lane)))
    }

    fn complete(&mut self, request: &TranscriptionJob, worker: u8) {
        self.active.remove(&Key::for_request(request));
        self.worker_busy[usize::from(worker - 1)] = false;
    }

    fn remove_pending(&mut self, key: Key) -> Option<InferenceTask> {
        let pending = self.pending.remove(&key)?;
        self.ready[key.lane].recordings.remove(&key.recording);
        Some(pending.task)
    }
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

impl Shared {
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
        let shared = Arc::new(Shared::default());
        let mut workers = Vec::with_capacity(3);
        for worker in 1..=3 {
            let worker_shared = shared.clone();
            match thread::Builder::new()
                .name(format!("inference-{worker}"))
                .spawn(move || worker_loop(worker_shared, worker))
            {
                Ok(handle) => workers.push(handle),
                Err(error) => {
                    // The public constructor is infallible. A failed pool
                    // rejects prepared jobs with real errors instead of using
                    // a partial pool or silently losing a refinement lane.
                    let mut state = shared.lock();
                    state.initialization_error =
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

    /// Store the private PCM/decoder handoff before publishing its gate request.
    pub fn prepare(&self, task: InferenceTask) {
        let key = Key::for_request(&task.request);
        let mut state = self.shared.lock();
        let error = if state.stopping {
            Some(
                state
                    .initialization_error
                    .clone()
                    .unwrap_or_else(|| "inference scheduler stopped".into()),
            )
        } else if state.cancelled.contains(&key.recording) {
            Some("inference recording cancelled".into())
        } else if state.pending.contains_key(&key) {
            Some("an inference job is already pending for this recording and pass".into())
        } else if state.active.get(&key) == Some(&task.request) {
            Some("this inference job is already running".into())
        } else {
            None
        };
        if let Some(error) = error {
            task.tx.publish(AppEvent::TranscriptionRequested {
                request: task.request,
            });
            reject(task, error, state.queue_jobs(key.lane));
            return;
        }
        let request = task.request;
        let tx = task.tx.clone();
        state.pending.insert(
            key,
            Pending {
                task,
                dispatched: false,
            },
        );
        // Publishing under the same lock prevents cancellation from overtaking
        // Requested. A fast bus consumer cannot observe a missing private job.
        tx.publish(AppEvent::TranscriptionRequested { request });
    }

    /// Activate exactly the privately stored request after its database gate.
    pub fn dispatch(&self, request: &TranscriptionJob) -> bool {
        let dispatched = self.shared.lock().dispatch(request);
        if dispatched {
            self.shared.changed.notify_all();
        }
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
        let key = Key::for_request(request);
        if state.pending.get(&key).is_some_and(|pending| {
            !pending.dispatched && pending.task.request == *request
        }) {
            let task = state.remove_pending(key).expect("matching pending request");
            reject(task, "transcription request rejected by database gate".into(),
                state.queue_jobs(key.lane));
        }
    }

    /// Terminally fence this recording and return jobs which never crossed the
    /// dispatch gate. Accepted jobs finish normally and keep their replies.
    pub fn cancel(&self, recording: u64) {
        let mut state = self.shared.lock();
        state.cancelled.insert(recording);
        for lane in 0..2 {
            let key = Key { recording, lane };
            if state
                .pending
                .get(&key)
                .is_some_and(|pending| !pending.dispatched)
            {
                if let Some(task) = state.remove_pending(key) {
                    reject(
                        task,
                        "inference job cancelled before dispatch".into(),
                        state.queue_jobs(lane),
                    );
                }
            }
        }
    }
}

impl Drop for InferenceScheduler {
    fn drop(&mut self) {
        let mut state = self.shared.lock();
        state.stopping = true;
        let pending = std::mem::take(&mut state.pending);
        for (_, pending) in pending {
            reject(pending.task, "inference scheduler stopped".into(), 0);
        }
        drop(state);
        self.shared.changed.notify_all();
        // Workers own Shared, not InferenceScheduler. The last scheduler Arc
        // always reaches this shutdown, and active inference finishes before
        // its decoder and reply channel are released.
        for handle in self.workers.drain(..) {
            let _ = handle.join();
        }
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
                    // Wake refinement workers after the live worker claims
                    // its reserved slot. They may now use the second slot.
                    shared.changed.notify_all();
                    break task;
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
    captured.load(Ordering::Acquire).saturating_sub(consumed_end)
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
    let result = catch_unwind(AssertUnwindSafe(|| -> anyhow::Result<Vec<TranscriptOut>> {
        let mut output = engine.feed_chunks(&chunks)?;
        if request.finishing {
            output.extend(engine.finish()?);
        }
        Ok(output)
    }));
    let inference_end = Instant::now();
    let transcription = TranscriptionMetrics {
        start_ms: elapsed_ms(origin, inference_start),
        end_ms: elapsed_ms(origin, inference_end),
        duration_ms: elapsed_ms(inference_start, inference_end),
        queue_size_start,
        queue_size_end: backlog(&captured, consumed_end),
    };
    let (engine, output) = match result {
        Ok(Ok(output)) => (Some(engine), Ok(output)),
        Ok(Err(error)) => (Some(engine), Err(format!("{error:#}"))),
        Err(panic) => (
            None,
            Err(format!("transcription worker panicked: {}", panic_message(panic.as_ref()))),
        ),
    };
    let mut state = shared.lock();
    state.complete(&request, worker);
    let queue_jobs = state.queue_jobs(Key::for_request(&request).lane);
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
            let (replies, results) = mpsc::channel();
            Self {
                scheduler: InferenceScheduler {
                    shared: Arc::new(Shared::default()),
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
    fn live_has_dispatch_priority_and_the_shared_backend_limit_is_two() {
        let h = Harness::new();
        h.queue(request(10, 1, TranscriptPass::Live));
        h.queue(request(20, 1, TranscriptPass::Refined));
        h.queue(request(30, 1, TranscriptPass::Refined));
        assert!(h.take(2).is_none(), "idle live worker has priority");
        let live = h.take(1).expect("live reserves the first backend slot");
        let refine = h.take(2).expect("refinement uses the second slot");
        assert!(h.take(3).is_none(), "two backends already running");
        h.queue(request(10, 2, TranscriptPass::Live));
        h.finish(1, live);
        assert!(h.take(3).is_none(), "next free slot belongs to queued live");
        let next_live = h.take(1).expect("next live job");
        h.finish(2, refine);
        let next_refine = h.take(3).expect("other refinement worker can run");
        h.finish(1, next_live);
        h.finish(3, next_refine);
    }

    #[test]
    fn shutdown_borrows_idle_lanes_without_overlapping_decoder_state() {
        let h = Harness::new();
        for recording in [10, 20, 30, 40] {
            h.queue(request(recording, 1, TranscriptPass::Live));
        }
        h.queue(request(50, 1, TranscriptPass::Refined));
        h.scheduler.begin_shutdown();
        let first = h.take(1).unwrap();
        let second = h.take(2).expect("refinement worker drains live");
        let third = h.take(3).expect("third existing worker also drains live");
        assert!(h.take(1).is_none(), "all three workers are busy");
        h.queue(request(10, 2, TranscriptPass::Live));
        h.finish(2, second);
        let fourth = h.take(2).unwrap();
        assert_eq!(fourth.0.request.recording, 40, "active decoder key cannot overlap");
        h.finish(3, third);
        let refinement = h.take(3).expect("unavailable live keys leave room for refinement");
        assert_eq!(refinement.0.request.pass, TranscriptPass::Refined);
        h.finish(3, refinement);
        assert!(h.take(3).is_none(), "pending state for recording 10 is still exclusive");
        h.finish(1, first);
        let next = h.take(3).expect("released state can migrate to an idle worker");
        assert_eq!(next.0.request.recording, 10);
        h.finish(2, fourth);
        h.finish(3, next);
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
                AppEvent::TranscriptionFailed { request, worker, transcription, .. } => {
                    Some((request, worker, transcription))
                }
                _ => None,
            })
            .collect();
        assert_eq!(failures, vec![(live, None, None), (refine, None, None), (late, None, None)]);
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
        assert_eq!(first.worker, 1);
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
        let mut output = HashMap::new();
        for _ in 0..3 {
            let result = results.recv_timeout(DEADLINE).expect("independent decoder result");
            let worker = result.worker;
            match result.request.pass {
                TranscriptPass::Live => assert_eq!(worker, 1),
                TranscriptPass::Refined => assert!([2, 3].contains(&worker)),
            }
            output.insert(result.request.job, result.output.unwrap());
        }
        for (job, sum, samples) in [(2, 6, 3), (3, 10, 1), (4, 100, 1)] {
            assert_eq!(
                output.remove(&job).unwrap(),
                vec![TranscriptOut::Committed {
                    t_start_ms: 0,
                    t_end_ms: samples,
                    text: format!("sum={sum};samples={samples}"),
                }]
            );
        }
        let events: Vec<_> = bus.drain().collect();
        for job in 1..=4 {
            let lifecycle: Vec<_> = events.iter().filter_map(|event| match event {
                AppEvent::TranscriptionRequested { request } if request.job == job => Some("queued"),
                AppEvent::TranscriptionStarted { request, .. } if request.job == job => Some("running"),
                AppEvent::TranscriptionFinished { request, .. } if request.job == job => Some("finished"),
                AppEvent::TranscriptionFailed { request, .. } if request.job == job => Some("failed"),
                _ => None,
            }).collect();
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
            (2, Failure::Panic, "transcription worker panicked: decoder panic", false),
            (3, Failure::Finish, "decoder could not flush", true),
        ] {
            let mut request = request(10, job, TranscriptPass::Refined);
            request.finishing = true;
            let mut next = task(request, bus.sender(), replies.clone());
            next.engine = Box::new(FailingEngine(failure));
            scheduler.prepare(next);
            assert!(scheduler.dispatch(&request));
            let result = results.recv_timeout(DEADLINE).expect("failed decoder reply");
            assert_eq!(result.output.unwrap_err(), expected);
            assert_eq!(result.engine.is_some(), state_returned);
            let lifecycle: Vec<_> = bus.drain().filter_map(|event| match event {
                AppEvent::TranscriptionRequested { .. } => Some("queued"),
                AppEvent::TranscriptionStarted { .. } => Some("running"),
                AppEvent::TranscriptionFailed { worker: Some(_), transcription: Some(_), .. } => Some("failed"),
                _ => None,
            }).collect();
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
        next.engine = Box::new(GatedEngine { entered: entered_tx, release: release_rx });
        scheduler.prepare(next);
        assert!(scheduler.dispatch(&request));
        entered_rx.recv_timeout(DEADLINE).expect("inference started");
        captured.store(25, Ordering::Release);
        release_tx.send(()).expect("release inference");
        let result = results.recv_timeout(DEADLINE).expect("inference result");
        assert_eq!(result.transcription.queue_size_start, 6);
        assert_eq!(result.transcription.queue_size_end, 21);
        assert_eq!(captured.load(Ordering::Acquire), 25);
        assert_eq!(result.output.unwrap(), Vec::<TranscriptOut>::new());
    }

    #[test]
    fn dropping_the_pool_joins_idle_workers_and_returns_prepared_decoder_state() {
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
        assert!(shared.upgrade().is_none(), "workers released their shared state");
    }

    #[test]
    fn a_pool_initialization_error_is_returned_instead_of_stranding_a_job() {
        let h = Harness::new();
        {
            let mut state = h.scheduler.shared.lock();
            state.stopping = true;
            state.initialization_error = Some("starting inference worker 2: unavailable".into());
        }
        let request = request(10, 1, TranscriptPass::Live);
        h.prepare(request);
        let result = h.result();
        assert_eq!(result.output.unwrap_err(), "starting inference worker 2: unavailable");
        assert_eq!(result.worker, 0);
        assert!(result.engine.is_some());
        assert!(!h.scheduler.dispatch(&request));
    }
}
