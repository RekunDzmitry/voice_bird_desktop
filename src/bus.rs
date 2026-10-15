//! Application event bus.
//!
//! Producers and consumers publish typed events through a Tokio unbounded
//! channel. The loop logs every event, gates it through SQLite, and routes
//! accepted events to consumers. Publishing is synchronous; receiving wakes the loop.
//!
//! ## Why not `Copy`?
//!
//! `DownloadFailed` carries a `String` so the rendered error line is
//! actionable ("HTTP 404" beats "network error"). That forfeits `Copy`
//! for the whole enum. `Eq` survives — every payload stays integral;
//! keep it that way. The moment a payload gains an `f64` (a ratio, a
//! duration) the derive breaks and every `assert_eq!` in this file's
//! tests stops compiling, which is why progress is `bytes`/`total` and
//! never a pre-computed ratio.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::mpsc;

use crate::audio_sources::{AppTarget, AudioDevice, AudioSourceSnapshot, FunnelStep};
use crate::language::LanguageProfile;
use crate::picker::{ModelEntry, PickerMove};

/// Direction focus travelled between blocks. The reducer saturates at
/// both ends; the key layer never has to reason about wraparound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum FocusMove {
    Prev,
    Next,
}

/// Lifecycle states for one model download. The store/table owns
/// the row, and every transition emits a
/// [`AppEvent::DownloadStatusChanged`] on the bus so the JSONL event
/// log records the same lifecycle the SQL table does. Defined here
/// — alongside the events that carry it — so the table and the bus
/// cannot disagree on the spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum DownloadStatus {
    Downloading,
    Installing,
    Cancelling,
    Cancelled,
    Succeeded,
    Failed,
    Interrupted,
}

/// Transcription stage; PCM and floating-point timestamps never enter the bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum TranscriptPass {
    Live,
    Refined,
}

/// One inference job's processing span, excluding capture, publication and disk I/O.
/// Times are monotonic milliseconds since capture opened. Queue sizes are
/// pending 16 kHz mono samples for this pass, excluding the current job and context.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct TranscriptionMetrics {
    pub start_ms: u64,
    pub end_ms: u64,
    pub duration_ms: u64,
    pub queue_size_start: u64,
    pub queue_size_end: u64,
}

/// Immutable identity and captured-audio extent for one inference job.
/// Decoder state and PCM remain private to the inference scheduler.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct TranscriptionJob {
    pub block: u8,
    pub recording: u64,
    pub job: u64,
    pub pass: TranscriptPass,
    pub t_start_ms: u64,
    pub t_end_ms: u64,
    pub sample_count: u64,
    pub queued_at_ms: u64,
    pub finishing: bool,
}

/// Immutable capture command; worker services and handles never enter the bus.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RecordingRequest {
    pub block: u8,
    pub recording: u64,
    pub source: crate::audio::CaptureSource,
    pub language: &'static LanguageProfile,
    pub live_model: PathBuf,
    pub refine_model: PathBuf,
}

/// Process-wide increasing IDs for recording sessions and transcription jobs,
/// seeded from Unix epoch microseconds.
pub fn next_monotonic_id() -> u64 {
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

/// Everything that can happen in the app, as plain data.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "event")]
pub enum AppEvent {
    /// Language-picker fallback reply to `RequestBlock`.
    AddBlock,
    /// Ask the source catalog which picker a new block should open.
    RequestBlock,
    AddSourceBlock {
        snapshot: AudioSourceSnapshot,
    },
    /// Explicit source transition, accepted by the SQLite step/revision gate.
    SourceStepChanged {
        block: u8,
        from: FunnelStep,
        to: FunnelStep,
        rev: u32,
        device: Option<AudioDevice>,
        app: Option<AppTarget>,
    },
    SourceStepRejected {
        block: u8,
        from: FunnelStep,
        to: FunnelStep,
        rev: u32,
        actual: Option<(FunnelStep, u32)>,
    },
    /// `←` / `→`: move focus between blocks.
    FocusMoved {
        direction: FocusMove,
    },
    /// `↑` / `↓` while the focused block is `PickingLanguage`: move the
    /// highlight inside that block's language list. The language
    /// codes are stamped by the resolver for the event log.
    PickerMoved {
        direction: PickerMove,
        from_language: Option<&'static str>,
        to_language: Option<&'static str>,
    },
    /// The language consumer resolved model availability from SQLite.
    /// The block id is explicit because focus may move before this reply is
    /// reduced.
    LanguageSelected {
        block: u8,
        language: &'static LanguageProfile,
        pending: Vec<&'static str>,
    },
    /// Remove the named block after its queued transitions have been gated.
    BlockClosed {
        block: u8,
    },
    /// Model was available in SQLite when the language was selected. The
    /// reducer fans this availability out to any existing waiter; the
    /// selection itself excludes the model from its `pending` list.
    ModelAlreadyCached(&'static ModelEntry),
    /// A changed disk observation, persisted before publication. SQLite rejects
    /// observations superseded by a newer scan or accepted installation.
    ModelAvailabilityChanged {
        model: &'static ModelEntry,
        available: bool,
    },
    /// An accepted absent observation concerns a model a session counts ready.
    /// Returns through the bus before the projection stops recording and the
    /// language consumer publishes a model request.
    ModelMissing(&'static ModelEntry),

    /// One model needed by the selected language is not on disk yet.
    /// Seeds the shared projection; the download consumer claims or joins work.
    DownloadRequested {
        model: &'static ModelEntry,
        /// Intended attempt, captured before the request's next bus pass.
        /// A terminal outcome for this attempt must not trigger an implicit retry.
        attempt: u32,
    },
    /// Progress on a download. `bytes_per_sec` is a per-tick average
    /// over the throttle window — used by the renderer to label the
    /// gauge when `total` is `None`. `attempt` disambiguates events
    /// from concurrent attempts of the same model: the store
    /// discards any event whose attempt does not match the row's
    /// current attempt.
    DownloadProgress {
        attempt: u32,
        model: &'static str,
        bytes: u64,
        total: Option<u64>,
        bytes_per_sec: u64,
    },
    /// The downloader verified the staged bytes. SQLite claims Installing once
    /// for the current Downloading attempt before the store consumer installs.
    DownloadFetched {
        attempt: u32,
        model: &'static str,
    },
    /// Bytes verified; the format handler is unpacking.
    DownloadInstalling {
        attempt: u32,
        model: &'static str,
    },
    /// Fans out to EVERY block waiting on `model`.
    DownloadSucceeded {
        attempt: u32,
        model: &'static str,
    },
    /// `error` carries an actionable message.
    DownloadFailed {
        attempt: u32,
        model: &'static str,
        error: String,
    },
    /// A request failed before a worker could spawn, while preparing staging
    /// metadata or claiming a row in the downloads table (disk full, lock
    /// timeout, write error). The table reducer accepts this variant even when
    /// no row exists for `model`, so the UI receives the failure instead of seeing
    /// the event rejected by the attempt gate. The reducer treats
    /// it identically to [`AppEvent::DownloadFailed`]: any waiting
    /// block flips to `Failed`, the downloads entry is removed.
    DownloadClaimFailed {
        attempt: u32,
        model: &'static str,
        error: String,
    },
    /// Last waiter for `model` closed. Removes the repo row and the
    /// UiView.downloads entry; the in-flight task observes the
    /// cancellation row separately.
    DownloadCancelled {
        attempt: u32,
        model: &'static str,
    },
    /// Emitted after the downloads table persists a lifecycle transition, or
    /// to replay its current outcome for a request overtaken by that transition.
    /// The reducer reconciles outcomes that raced ahead of `LanguageSelected`;
    /// the event log records the same lifecycle the SQL table does.
    ///
    /// `model` is an `Arc<str>` (not `&'static str`) because the transition
    /// publisher may have read it from the database instead of the catalog.
    /// `error` is populated for `Failed` so a late waiter receives the same
    /// actionable message as blocks that observed the worker event directly.
    DownloadStatusChanged {
        model: std::sync::Arc<str>,
        attempt: u32,
        from: Option<DownloadStatus>,
        to: DownloadStatus,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Emitted when an event's attempt or lifecycle stage no longer matches
    /// the table's current row (a Restart superseded the old worker, or a
    /// duplicate/cancelled handoff can no longer begin installation).
    /// Visibility — the event log now records stale-event drops
    /// instead of silently filtering them. `rejected` is the
    /// variant name of the dropped event (e.g. `"DownloadProgress"`).
    DownloadEventRejected {
        model: std::sync::Arc<str>,
        rejected: &'static str,
        event_attempt: u32,
        row_attempt: Option<u32>,
    },
    /// Consumer-resolved capture work, logged and gated before worker execution.
    RecordingRequested {
        request: Arc<RecordingRequest>,
    },
    /// Stop only this recording identity, never a replacement on the same block.
    RecordingStopRequested {
        block: u8,
        recording: u64,
    },
    RecordingStarted {
        block: u8,
        recording: u64,
        session_dir: String,
        source: crate::audio::CaptureSource,
        language: String,
        started_at: String,
    },
    /// SQLite admits this job before the scheduler may dispatch its PCM.
    TranscriptionRequested {
        request: TranscriptionJob,
    },
    TranscriptionStarted {
        request: TranscriptionJob,
        worker: u8,
        start_ms: u64,
        queue_jobs: u32,
        queue_size_start: u64,
    },
    /// Emitted for every completed inference call, including calls with no text.
    TranscriptionFinished {
        request: TranscriptionJob,
        worker: u8,
        transcription: TranscriptionMetrics,
        queue_wait_ms: u64,
        queue_jobs: u32,
    },
    TranscriptionFailed {
        request: TranscriptionJob,
        worker: Option<u8>,
        transcription: Option<TranscriptionMetrics>,
        queue_wait_ms: u64,
        queue_jobs: u32,
        error: String,
    },
    TranscriptTentative {
        block: u8,
        recording: u64,
        job: u64,
        worker: u8,
        text: String,
        transcription: TranscriptionMetrics,
    },
    TranscriptCommitted {
        block: u8,
        recording: u64,
        job: u64,
        worker: u8,
        pass: TranscriptPass,
        t_start_ms: u64,
        t_end_ms: u64,
        text: String,
        transcription: TranscriptionMetrics,
    },
    RecordingStopped {
        block: u8,
        recording: u64,
    },
    RecordingFailed {
        block: u8,
        recording: u64,
        error: String,
    },
    /// `q` / Ctrl-C: quit. Always honoured, including mid-download.
    Quit,

    // -----------------------------------------------------------------
    // Events are routed to consumers only after SQLite accepts them.
    // Follow-up stages publish new events instead of calling another consumer.
    // -----------------------------------------------------------------
    /// Resolve model availability for a language and publish `LanguageSelected`
    /// plus model requests. Downloads begin when those requests are consumed.
    BeginLanguage {
        block: u8,
        language: &'static LanguageProfile,
        source_rev: Option<u32>,
    },
    /// Quit-time cleanup: drop the staged archive and unpack
    /// scratch directory for `model`. The consumer asks the store to discard it.
    DiscardInflight {
        model: std::sync::Arc<str>,
    },
}

/// Cloneable producer handle. Producers only need this — `publish` is the
/// whole API they see, and cloning is the only way to get one.
#[derive(Debug, Clone)]
pub struct EventSender(mpsc::UnboundedSender<AppEvent>);

impl EventSender {
    /// Best-effort: send only fails when the bus is gone, i.e. the loop is
    /// shutting down — dropping the event is correct then.
    pub fn publish(&self, event: AppEvent) {
        let _ = self.0.send(event);
    }
}

/// Single-consumer pub/sub over Tokio's unbounded channel.
pub struct EventBus {
    sender: mpsc::UnboundedSender<AppEvent>,
    receiver: mpsc::UnboundedReceiver<AppEvent>,
}

impl EventBus {
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();
        Self { sender, receiver }
    }

    pub fn sender(&self) -> EventSender {
        EventSender(self.sender.clone())
    }

    pub fn drain(&mut self) -> impl Iterator<Item = AppEvent> + '_ {
        std::iter::from_fn(|| self.receiver.try_recv().ok())
    }

    pub async fn recv(&mut self) -> Option<AppEvent> {
        self.receiver.recv().await
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_yields_events_in_publish_order() {
        let mut bus = EventBus::new();
        let tx = bus.sender();
        tx.publish(AppEvent::AddBlock);
        tx.publish(AppEvent::AddBlock);
        tx.publish(AppEvent::Quit);
        let got: Vec<AppEvent> = bus.drain().collect();
        assert_eq!(
            got,
            vec![AppEvent::AddBlock, AppEvent::AddBlock, AppEvent::Quit]
        );
    }

    #[test]
    fn second_drain_is_empty() {
        let mut bus = EventBus::new();
        bus.sender().publish(AppEvent::Quit);
        let _ = bus.drain().count();
        assert_eq!(bus.drain().count(), 0);
    }

    #[test]
    fn two_cloned_senders_interleave_in_publish_order() {
        let mut bus = EventBus::new();
        let a = bus.sender();
        let b = bus.sender();
        a.publish(AppEvent::AddBlock);
        b.publish(AppEvent::AddBlock);
        a.publish(AppEvent::AddBlock);
        b.publish(AppEvent::Quit);
        let got: Vec<AppEvent> = bus.drain().collect();
        assert_eq!(
            got,
            vec![
                AppEvent::AddBlock,
                AppEvent::AddBlock,
                AppEvent::AddBlock,
                AppEvent::Quit,
            ]
        );
    }

    #[test]
    fn publish_after_drop_is_a_silent_no_op() {
        use std::panic::{catch_unwind, AssertUnwindSafe};

        let bus = EventBus::new();
        let tx = bus.sender();
        drop(bus);

        assert!(
            tx.0.send(AppEvent::Quit).is_err(),
            "underlying channel should report disconnected after the bus is dropped",
        );

        let result = catch_unwind(AssertUnwindSafe(|| {
            tx.publish(AppEvent::AddBlock);
            tx.publish(AppEvent::Quit);
        }));
        assert!(result.is_ok(), "publish after drop must not panic");
    }

    #[test]
    fn dropped_sender_does_not_deliver_to_a_fresh_bus() {
        let stale = EventBus::new();
        let stale_tx = stale.sender();
        drop(stale);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            stale_tx.publish(AppEvent::AddBlock);
        }));
        assert!(result.is_ok());
        assert!(stale_tx.0.send(AppEvent::AddBlock).is_err());

        let mut fresh = EventBus::new();
        assert_eq!(fresh.drain().count(), 0);
    }

    #[test]
    fn download_status_changed_serializes_under_the_event_tag() {
        use std::sync::Arc;
        let ev = AppEvent::DownloadStatusChanged {
            model: Arc::from("tiny.en"),
            attempt: 2,
            from: Some(DownloadStatus::Cancelling),
            to: DownloadStatus::Cancelled,
            error: None,
        };
        let json = serde_json::to_string(&ev).expect("serialize");
        assert!(
            json.contains("\"event\":\"DownloadStatusChanged\""),
            "{json}"
        );
        assert!(json.contains("\"to\":\"Cancelled\""), "{json}");
    }

    #[test]
    fn language_events_serialize_codes_without_model_metadata() {
        let language = &crate::language::LANGUAGES[0];
        let selected = AppEvent::LanguageSelected {
            block: 3,
            language,
            pending: language.models().iter().map(|model| model.id).collect(),
        };
        let selected: serde_json::Value =
            serde_json::to_value(selected).expect("serialize language selection");
        assert_eq!(selected["event"], "LanguageSelected");
        assert_eq!(selected["block"], 3);
        assert_eq!(selected["language"]["code"], language.code);
        assert!(selected["language"].get("live").is_none());
        assert!(selected["language"].get("refine").is_none());

        let command = serde_json::to_value(AppEvent::BeginLanguage {
            block: 3,
            language,
            source_rev: None,
        })
        .expect("serialize language command");
        assert_eq!(command["event"], "BeginLanguage");
        assert_eq!(command["block"], 3);
        assert_eq!(command["language"]["code"], language.code);

        let missing = serde_json::to_value(AppEvent::ModelMissing(language.live))
            .expect("serialize missing model");
        assert_eq!(missing["event"], "ModelMissing");
        assert_eq!(missing["id"], language.live.id);
    }

    #[test]
    fn availability_observations_serialize_model_metadata_and_boolean() {
        let model = &crate::picker::CATALOG[0];
        let observation = serde_json::to_value(AppEvent::ModelAvailabilityChanged {
            model,
            available: false,
        })
        .expect("serialize model observation");
        assert_eq!(observation["event"], "ModelAvailabilityChanged");
        assert_eq!(observation["model"]["id"], model.id);
        assert_eq!(observation["model"]["size_mb"], model.size_mb);
        assert_eq!(observation["available"], false);
    }
}
