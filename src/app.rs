use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use parking_lot::Mutex as PlMutex;

use crate::platform::{AppSession, AudioDevice};
use voice_bird_cli::config::{AppConfig, DefaultSlotConfig};
use voice_bird_cli::room::{Room, RoleDef};
use voice_bird_cli::session::layout::SessionSource;
use voice_bird_cli::session::target::Target;
/// Application running mode
#[derive(Debug, Clone, PartialEq)]
pub enum AppMode {
    Normal,
    ModelPicker, // wired in Stage 3's Task 18
    Help,
    /// Status overlay: the most recent Agent events (target saved,
    /// verify ok/fail, broker errors, dropped segments), newest
    /// first, each with a timestamp. Opened with `t`; `?` stays
    /// help-only.
    Status,
    /// Centered overlay prompting the user to paste a Voice Bird API key.
    /// Opened when `c` toggles cloud on with an empty key, or when the
    /// engine reports an auth failure during recording.
    ApiKeyModal,
    /// Centered overlay prompting the user to pick a local
    /// session output path. Opened with `p` from Normal mode.
    PathModal,
 }

/// Languages offered in the cloud-mode language selector. Local mode
/// always transcribes English regardless of this list — the selector
/// is hidden and the engine call is forced to "en".
pub const CLOUD_LANGUAGES: &[&str] = &["en", "es", "fr", "de", "it", "pt", "ja", "zh", "ru", "pl"];

/// Hard upper bound on parallel recording slots. Slots start at 1 and
/// grow on user demand (`+`); they never shrink below 1. The cap is
/// here to defend against a runaway key auto-repeat building a Vec
/// that overruns the screen — practical UI tops out around 4.
pub const MAX_SECTIONS: usize = 8;

/// Recording status
#[derive(Debug, Clone)]
pub enum RecordingStatus {
    Idle,
    Error(String),
}

/// Progress reported by a running model download.
#[derive(Debug, Clone)]
pub struct DownloadProgress {
    pub model_id: String,
    pub bytes: u64,
    pub total: Option<u64>,
    pub error: Option<String>,
}

/// State for the first-run model picker overlay.
pub struct PickerState {
    pub index: usize,
    /// When present, a download is in progress (or just finished with an
    /// error). Wrapped in `Arc<PlMutex<...>>` so the background download
    /// thread can mutate the same progress that the render path reads.
    pub downloading: Option<Arc<PlMutex<DownloadProgress>>>,
}

/// A single committed (finalized) transcript line.
#[derive(Clone)]
pub struct CommittedLine {
    pub t_start_ms: u64,
    pub text: String,
}

/// Transcript state preserved across a section stop, so the text stays
/// visible in the UI. When the user starts a new section in the same slot,
/// these Arcs are reattached so new segments append to existing content.
/// `Clone` lets `App::resume_section` snapshot the saved metadata
/// without disturbing the `SlotKind` enum (which still contains a
/// non-`Clone` `Section` in its `Recording` arm and therefore
/// itself cannot derive `Clone`).
#[derive(Clone)]
pub struct SavedTranscript {
    pub committed: Arc<PlMutex<Vec<CommittedLine>>>,
    pub refined: Arc<PlMutex<Vec<CommittedLine>>>,
    /// Column-title label from the stopped section (e.g. "mic · cloud:OFF").
    pub label: String,
    pub target: Target,
    /// Source the stopped section was capturing (mic / system / app).
    /// Informational snapshot: `App::resume_section` does NOT read
    /// it back — resume re-derives the source from the current
    /// Devices+Apps pickers and rewrites this field so the slot
    /// reflects what the resumed section actually used.
    pub source: SessionSource,
    /// Settings the stopped section was using (cloud on/off,
    /// language, model). Informational snapshot, like `source`:
    /// resume applies the live `effective_settings_for(source)`
    /// and rewrites this field, so post-stop changes (language,
    /// cloud toggle) take effect on the resumed section.
    pub settings: SectionSettings,
    /// Wall-clock start of the original recording — the merged
    /// timeline anchors lines to this moment.
    pub session_started_at: chrono::DateTime<chrono::Utc>,
    /// Optional role binding (set when the slot was provisioned
    /// by an agent room).
    pub role: Option<RoleDef>,
}
pub struct RecordingRuntime {
    pub join: tokio::task::JoinHandle<()>,
    /// Producer task: pulls cpal frames, resamples, tees to WAV + engine.
    /// Aborted by `stop_section` so the await on `join` does not hang
    /// when the cpal channel `recv()` is blocked.
    pub producer: Option<tokio::task::JoinHandle<()>>,
    /// Background refinement consumer join. `None` when no refinement
    /// model is configured or when it failed to load.
    pub refinement_join: Option<tokio::task::JoinHandle<()>>,
}
/// Per-slot settings. Every slot owns one; idle slots read
/// these directly, recording slots snapshot them into
/// `Section.settings` at start time. Each `Option` field is
/// `None` = "use the global `DefaultSlotConfig`";
/// `Some(value)` = "this slot overrides the default".
/// Toggling cloud, cycling language, cycling model, or saving
/// a custom output path on a slot customizes the relevant
/// field. The picker cursor (device / app) and agent routing
/// are stored separately in `slot_picker_memo` and
/// `pending_agent_overrides` — they index into the live
/// inventory rather than carry device names directly.
#[derive(Debug, Clone)]
pub struct SlotConfig {
    pub cloud_on: Option<bool>,
    pub language: Option<String>,
    pub model: Option<String>,
    pub path: Option<String>,
}

impl SlotConfig {
    /// All-None = "uses every field from the default".
    pub fn default_passthrough() -> Self {
        Self {
            cloud_on: None,
            language: None,
            model: None,
            path: None,
        }
    }
}

/// Snapshot of the slot's `SlotConfig` taken at section start.
/// Holds the EFFECTIVE values (after applying the default) so the
/// running engine reads from a stable, non-defaulting view.
#[derive(Debug, Clone)]
pub struct SectionSettings {
    pub cloud_on: bool,
    pub language: String,
    pub model: String,
}

impl SectionSettings {
    /// Convert a (slot_config, default) into the effective
    /// snapshot the section will use.
    pub fn effective(slot: &SlotConfig, default: &DefaultSlotConfig) -> Self {
        Self {
            cloud_on: slot.cloud_on.unwrap_or(default.cloud_on),
            language: slot
                .language
                .clone()
                .unwrap_or_else(|| default.language.clone()),
            model: slot
                .model
                .clone()
                .unwrap_or_else(|| default.model.clone()),
        }
    }
}
/// engine, and transcript buffers — multiple sections can record in
/// parallel, each with independent settings (Stage 2+).
pub struct Section {
    /// What this section is recording (mic / system / per-app — Stage 4+).
    pub source: SessionSource,
    /// Settings active for this section's lifetime (cloud/language/model).
    pub settings: SectionSettings,
    /// Tokio task handles for producer/consumer/refinement.
    pub runtime: RecordingRuntime,
    /// Live capture keep-alive (cpal `Stream` or SCK `SCStream`). Pinned
    /// to the App-owning thread because `cpal::Stream` is `!Send`.
    pub _capture_stream: voice_bird_cli::audio::capture::CaptureKeepAlive,
    /// Committed (finalized) transcript lines.
    pub committed: Arc<PlMutex<Vec<CommittedLine>>>,
    /// Refined transcript lines from the background refinement engine.
    pub refined: Arc<PlMutex<Vec<CommittedLine>>>,
    /// Latest tentative (in-progress) transcript text.
    pub tentative: Arc<PlMutex<String>>,
    /// Which engine is actually running ("whisperkit" / "whisper_rs" /
    /// "voicebird"). Persisted into `meta.json` at stop.
    pub engine_kind: String,
    /// True while a cloud engine is actively transmitting audio.
    pub cloud_active: bool,
    /// Shared slot the consumer task writes engine errors into. Drained
    /// by `App::check_engine_error` on each tick.
    pub engine_error_channel: Arc<Mutex<Option<String>>>,
    /// Wall-clock time when the cloud reminder banner should be hidden
    /// (3 s after recording start). `None` for local sections.
    pub cloud_reminder_until: Option<std::time::Instant>,
    /// On-disk session directory (`None` when broadcasting to cloud).
    pub session_dir: Option<PathBuf>,
    /// Optional role binding. `start_section` copies this from
    /// `slot.role` so the running section knows which human
    /// `Role:` label prefix.
    pub role: Option<RoleDef>,
    /// Wall-clock start of this section.
    pub session_started_at: chrono::DateTime<chrono::Utc>,
    /// Transcript scroll offset for this section (lines from top).
    /// Only consulted when `transcript_follow` is false.
    pub transcript_scroll: u16,
    /// When true (default), the section's transcript pane auto-scrolls
    /// to show the latest content. Set false on manual scroll, restored
    /// by End.
    pub transcript_follow: bool,
    /// Where this section is sending its transcript. Derived from
    /// `settings.cloud_on` at start time and kept in sync on the
    /// `Target` axis so the Agents pane can show it without poking
    /// into the per-section settings.
    pub target: Target,
}

/// Stable identifier for a `Slot`. Allocated once when the slot is
/// created and reused for the slot's lifetime. The id stays valid even
/// if the slot's `kind` cycles between `Empty`, `Recording`, and
/// `Saved` — render code and key handlers can hold a `SlotId` and trust
/// it to keep addressing the same physical pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SlotId(pub u32);

impl std::fmt::Display for SlotId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// The state of a single transcript slot. `Empty` is the rest state
/// the user starts in; `Recording` is an actively-running section;
/// `Saved` is a stopped section whose transcript is kept visible in
/// the UI until cleared with `x` or overwritten by a fresh start.
pub enum SlotKind {
    Empty,
    Recording { section: Section },
    Saved { saved: SavedTranscript },
}

impl std::fmt::Debug for SlotKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SlotKind::Empty => f.write_str("Empty"),
            SlotKind::Recording { section } => f
                .debug_struct("Recording")
                .field("source", &section.source)
                .field("target", &section.target)
                .field("cloud_on", &section.settings.cloud_on)
                .finish(),
            SlotKind::Saved { saved } => f
                .debug_struct("Saved")
                .field("label", &saved.label)
                .field("target", &saved.target)
                .finish(),
        }
    }
}
pub struct Slot {
    pub id: SlotId,
    pub kind: SlotKind,
    /// Optional role binding. `Some` for slots provisioned by an
    /// agent room — the slot renders its role name in the title
    /// and `start_section` records the role on the section so
    /// `merged_timeline` can label entries. Free Room slots are
    /// `None`.
    pub role: Option<RoleDef>,
    /// Per-slot settings. Each slot starts with
    /// `SlotConfig::default_passthrough()` (every field inherits
    /// from `App::default_slot_config`). The c/l/m/P keys and the
    /// device/app picker mutate the FOCUSED slot's config; idle
    /// slots read it directly, recording slots snapshot it into
    /// `Section.settings` at start time.
    pub config: SlotConfig,
}
impl Slot {
    pub fn empty(id: SlotId) -> Self {
        Self {
            id,
            kind: SlotKind::Empty,
            role: None,
            config: SlotConfig::default_passthrough(),
        }
    }

    /// Read-only view of the running section, if any. Used by the
    /// accessors that answer "what's in the focused slot".
    pub fn as_section(&self) -> Option<&Section> {
        match &self.kind {
            SlotKind::Recording { section } => Some(section),
            _ => None,
        }
    }

    /// The target this slot is currently (or was last) routing to.
    /// `None` only when the slot has never been used.
    pub fn target(&self) -> Option<Target> {
        match &self.kind {
            SlotKind::Empty => None,
            SlotKind::Recording { section } => Some(section.target.clone()),
            SlotKind::Saved { saved } => Some(saved.target.clone()),
        }
    }
}
impl std::fmt::Debug for Slot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Slot")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .finish()
    }
}
/// Which pane the picker arrows / Enter key target. Devices is the
/// physical-input/output column on the left; Apps is the
/// per-application column in the middle; Agents is the cloud-prompt
/// picker on the right (today a single `Stdout` row while the cloud
/// agent list lands in §10). Each pane has its own cursor and scroll
/// offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerFocus {
    Devices,
    Apps,
    Rooms,
}
/// Per-slot memo of the picker cursor. The Devices / Apps cursors on
/// `App` always reflect the FOCUSED slot's state; this struct holds
/// the last cursor position seen for a slot that's not currently
/// focused, so Tab/Shift-Tab back to it shows what was last selected.
/// Created on first focus, updated on every Tab-away from the slot.
/// In-memory only — not persisted across restarts.
#[derive(Debug, Clone)]
pub struct PickerSelection {
    pub device_idx: usize,
    pub app_idx: Option<usize>,
    pub focus: PickerFocus,
}

/// Main application state
pub struct App {
    /// Current mode
    pub mode: AppMode,


    /// Capturable input/output devices (left pane of the picker).
    pub devices: Vec<AudioDevice>,

    /// Per-application capture targets (middle pane of the picker).
    pub apps: Vec<AppSession>,

    /// Cursor in the Devices pane.
    pub selected_device_index: usize,

    /// Cursor in the Apps pane. `None` = no app paired (run device alone).
    pub selected_app_index: Option<usize>,

    /// Cursor in the Rooms pane. The list of rooms is one row per
    /// catalog entry (Free Room at index 0, then cloud-fetched rooms).
    /// Always `Some(idx)` while the TUI runs.
    pub selected_room_index: usize,

    /// Which pane the picker arrows / Enter target.
    pub picker_focus: PickerFocus,

    /// Scroll offset (rows from top) for each pane. The render path
    /// auto-clamps these so the cursor stays visible.
    pub device_scroll: u16,
    pub app_scroll: u16,
    pub room_scroll: u16,

    /// Aggregate recording status (Recording iff any section is running;
    /// Error iff the focused section has erred; Idle otherwise).
    pub status: RecordingStatus,

    /// Real-time audio level (0.0 - 1.0)
    pub audio_level: Arc<Mutex<f32>>,

    /// Recording duration in seconds (driven by `start_time`).
    pub duration: f32,

    /// Earliest section's start time. Drives `format_duration`.
    pub start_time: Option<std::time::Instant>,

    /// Application config
    pub config: AppConfig,

    /// Should the app quit?
    pub should_quit: bool,

    /// Status message to display
    pub status_message: Option<String>,

    /// Shared error channel — recording threads write errors here
    pub error_channel: Arc<Mutex<Option<String>>>,

    /// Path to the current log file
    pub log_path: Option<PathBuf>,

    /// Always-running tokio runtime for the recording pipeline.
    pub rt: tokio::runtime::Runtime,

    /// Parallel recording slots. Each entry owns its own id, kind
    /// (`Empty` / `Recording` / `Saved`), and — for the non-empty
    /// variants — the per-section state. The Vec starts at 1 slot;
    /// `+` appends new slots (allocating fresh ids via
    /// `next_slot_id`), `-` removes the focused one.
    pub slots: Vec<Slot>,

    /// Slot the c/l/m/Tab keys currently target. Stable across the
    /// slot's lifetime — never a positional index.
    pub focused_slot: SlotId,

    /// Monotonic counter for new slot ids. Starts at 2 because slot
    /// 1 is always present at startup. Bumped every time `add_slot`
    /// appends a new entry; never reuses freed ids so any handle a
    /// caller is holding stays valid even after a `-`.
    pub next_slot_id: u32,

    /// First-run model picker state; `Some` while in `AppMode::ModelPicker`.
    pub picker: Option<PickerState>,

    /// True iff `config.toml` already existed on disk at startup. When
    /// false, the model picker refuses to Esc-cancel — first run must pick.
    pub config_was_loaded_from_disk: bool,

    /// In-flight text buffer for the API-key modal. `None` when the
    /// modal isn't open. Repurposed from the old per-field settings
    /// editor — the modal is now the only text-input flow in the TUI.
    pub api_key_buf: Option<String>,
    /// In-flight text buffer for the output-path modal. `None` when
    /// the modal isn't open. Pre-filled with the focused slot's
    /// customized path, or the default if no customization.
    pub path_buf: Option<String>,

    /// Feedback banner for the transcript-export flow ("Exporting…",
    /// "Exported ✓", or error). Displayed above the footer like the
    /// general error banner.
    pub export_banner: Option<String>,

    /// Error banner displayed above the footer. Set when an engine emits
    /// `EngineEvent::Error` (or when the pipeline fails to start); cleared
    /// on the next successful `start_section`.
    pub banner: Option<String>,

    /// Transcript scroll offset (lines from the top). Only consulted when
    /// `transcript_follow` is false — otherwise the render path pins
    /// the view to the bottom automatically.
    pub transcript_scroll: u16,

    /// When true (default), the transcript auto-scrolls to show the latest
    /// content. Set false when the user manually scrolls up. Pressing
    /// End re-enables it.
    pub transcript_follow: bool,

    /// Empty fallback Arcs returned by `focused_*` accessors when no
    /// section is active — keeps the UI render path Arc-shaped without
    /// special-casing the empty state at every read site.
    empty_committed: Arc<PlMutex<Vec<CommittedLine>>>,
    empty_tentative: Arc<PlMutex<String>>,

    /// Per-slot pending target override. The Agents picker writes
    /// to this when the user picks a row; the next `start_section`
    /// consults it and applies it instead of the default
    pub active_room: usize,
    /// Path of the active room session dir (`<ts>-room-<slug>/`)
    /// when an agent room is active. `None` when Free Room is
    /// active (which has no parent dir) or before the first
    /// activation.
    pub room_session_dir: Option<std::path::PathBuf>,
    /// Last-known `plan` value from `/api/rooms`.
    /// start.
    pub pending_target_overrides: std::collections::BTreeMap<SlotId, Target>,

    /// Cloud Agents fetched from `GET /api/agents`. Populated by
    /// `App::refresh_agents` when the user has cloud on AND an API
    /// Rooms catalog fetched from `GET /api/rooms`. Index 0 is
    /// always `Room::free_room()` so the TUI never needs a
    /// "no rooms" branch — Free Room is the offline default
    /// and is hardcoded locally.
    pub rooms: Vec<Room>,
    /// Index into `rooms` of the currently active room. Drives
    /// the slot provisioning (Free Room → one empty slot;
    /// agent room → one empty slot per role) and the merged
    /// timeline view.
    /// Last-known `plan` value from `/api/rooms`. `None` until
    /// the first successful fetch (or after a 5xx); `Some(true)`
    /// when the user is on Pro and agent rooms unlock,
    /// `Some(false)` when they're not. The 402 response from
    /// agent runs also forces this to `Some(false)`.
    pub plan_is_pro: Option<bool>,
    /// Entries are pruned when a slot is removed (its id is
    /// never reused). In-memory only — not persisted across restarts.
    pub slot_picker_memo: std::collections::BTreeMap<SlotId, PickerSelection>,

    /// Rolling log of app events (verify ok/fail, push failures,
    /// dropped segments), newest at the back. Behind an
    /// `Arc<PlMutex<...>>` so any background task can
    /// append while the render thread reads. Surfaced by the
    /// `t` status overlay ([`AppMode::Status`\]). Renamed
    /// from `agent_events` in §8.
    pub app_events: Arc<PlMutex<VecDeque<AppEvent>>>,

    /// Global default for a slot's per-slot settings. Each slot
    /// reads unset fields from here. The c/l/m/P keys mutate
    /// the focused slot's `SlotConfig`; this field is the
    /// background defaults. The user can customize it directly
    /// (settings UI not yet implemented) but per-slot writes
    /// never mutate it.
    pub default_slot_config: voice_bird_cli::config::DefaultSlotConfig,

    /// Live state of any in-flight agent run. Written by
    /// `App::drain_agent_run_state` from the worker's
    /// mpsc channel. Rendered by the agent-room TUI
    /// branch (D5.1).
    pub agent_run_state: voice_bird_cli::cloud::run::AgentRunState,

    /// Handle to the in-flight agent-run worker (if any).
    /// Set by `App::start_agent_run`, cleared when the
    /// worker finishes or when `App` shuts down. The
    /// worker is single-shot per run: when it returns,
    /// `drain_agent_run_state` joins it and resets this
    /// to `None`.
    pub agent_run_worker: Option<(
        std::thread::JoinHandle<voice_bird_cli::cloud::run::AgentRunError>,
        std::sync::mpsc::Receiver<voice_bird_cli::cloud::run::RunEvent>,
    )>,
 }
/// One row in the `t` status overlay.
#[derive(Debug, Clone)]
pub struct AppEvent {
    /// Local wall-clock time the event was recorded.
    pub at: chrono::DateTime<chrono::Local>,
    pub message: String,
}

/// One row in the merged role-labeled timeline. Built by
/// `App::merged_timeline` (D3.3) and consumed by the room
/// view TUI (D5). The per-frame cost is O(N lines) which is
/// fine at TUI transcript sizes.
#[derive(Debug, Clone)]
pub struct TimelineEntry {
    /// Absolute wall-clock time = `session_started_at +
    /// committed_line.t_start_ms`.
    pub at: chrono::DateTime<chrono::Utc>,
    /// Role label (`None` for free-room slots).
    pub role: Option<String>,
    pub slot: SlotId,
    pub text: String,
}

impl App {
    /// Snapshot the current picker cursor under the given slot id.
    /// Called immediately before focus moves away from that slot.
    /// The cursor fields on `App` keep holding the slot's state
    /// until the caller switches `focused_slot`, at which point
    /// `restore_picker_for` loads the next slot's memo back in.
    fn memoize_picker_for(&mut self, slot: SlotId) {
        self.slot_picker_memo.insert(
            slot,
            PickerSelection {
                device_idx: self.selected_device_index,
                app_idx: self.selected_app_index,
                focus: self.picker_focus,
            },
        );
    }

    /// Load the memoized picker cursor for the given slot id into
    /// the cursor fields on `App`. No-op if the slot has no memo
    /// (first time the user Tabs to it) — the cursor simply keeps
    /// whatever value it had, which is the natural "first focus
    /// inherits current state" behavior.
    fn restore_picker_for(&mut self, slot: SlotId) {
        if let Some(p) = self.slot_picker_memo.get(&slot).cloned() {
            // Clamp the restored indices against the current
            // inventory — a refresh may have shrunk `devices` /
            // `apps` between Tab-away and Tab-back.
            let dev_idx = if self.devices.is_empty() {
                0
            } else {
                p.device_idx.min(self.devices.len() - 1)
            };
            let app_idx = p.app_idx.and_then(|i| {
                if self.apps.is_empty() {
                    None
                } else {
                    Some(i.min(self.apps.len() - 1))
                }
            });
            self.selected_device_index = dev_idx;
            self.selected_app_index = app_idx;
            self.picker_focus = p.focus;
        }
    }

    /// Start a fresh agent run for the currently active
    /// room (which must have an agent). Spawns the
    /// worker thread, replaces any in-flight worker
    /// (D4.4: the older worker is joined and dropped
    /// — the new request supersedes it), and resets
    /// the streaming buffer. No-op when the active
    /// room has no agent (Free Room).
    ///
    /// Returns the previous JoinHandle (if any) so
    /// callers can join on shutdown.
    pub fn start_agent_run(&mut self, transcript: String) {
        use voice_bird_cli::cloud::run::{
            spawn_agent_run, AgentRunState, RunRequest,
        };
        // Free Room has no agent — start_agent_run is
        // a no-op there. The TUI guards against calling
        // it for Free Room, but we double-check.
        let room = match self.rooms.get(self.active_room) {
            Some(r) if r.has_agent() => r,
            _ => return,
        };
        let agent = match room.agent.as_ref() {
            Some(a) => a,
            None => return,
        };
        // If a worker is already in flight, join it
        let base_url = self.config.voicebird_server_url.clone();
        let api_key = self.config.voicebird_api_key.clone();
        let req = RunRequest {
            base_url,
            api_key,
            agent_id: agent.id.clone(),
            room_slug: Some(room.slug.clone()),
            source_label: Some("desktop".into()),
            transcript,
        };
        self.agent_run_state = AgentRunState {
            status: "starting".into(),
            streaming: String::new(),
            last_completed_md: std::mem::take(
                &mut self.agent_run_state.last_completed_md,
            ),
            last_run_started: Some(std::time::Instant::now()),
            lines_at_last_run: self.merged_timeline().len(),
            queued: false,
            last_error: None,
            run_id: None,
            plan_is_pro: self.agent_run_state.plan_is_pro,
        };
        self.agent_run_worker = Some(spawn_agent_run(req));
    }

    /// Drain the worker channel into `self.agent_run_state`.
    /// Called once per UI tick (D4.3 wiring). Non-blocking —
    /// the mpsc `try_recv` returns `Empty` when no frames
    /// are ready. When the worker joins (the JoinHandle
    /// reports `is_finished`), we mark the run as completed
    /// and clear `agent_run_worker`.
    pub fn drain_agent_run_state(&mut self) {
        use voice_bird_cli::cloud::run::{AgentRunError, RunEvent};
        let Some((handle, rx)) = self.agent_run_worker.as_ref() else {
            return;
        };
        // Drain every available frame (non-blocking).
        loop {
            match rx.try_recv() {
                Ok(RunEvent::Status { run_id, status }) => {
                    self.agent_run_state.run_id = Some(run_id);
                    self.agent_run_state.status = status.to_string();
                }
                Ok(RunEvent::Delta { text, .. }) => {
                    self.agent_run_state.streaming.push_str(&text);
                }
                Ok(RunEvent::Done {
                    content_markdown, ..
                }) => {
                    self.agent_run_state.last_completed_md =
                        content_markdown;
                    self.agent_run_state.streaming.clear();
                    self.agent_run_state.status = "completed".into();
                }
                Ok(RunEvent::Error { message, .. }) => {
                    self.agent_run_state.last_error = Some(message);
                    self.agent_run_state.status = "failed".into();
                }
                Ok(RunEvent::DoneFinal) => {
                    // Server closed the stream. The
                    // "completed" / "failed" status was
                    // already set by the matching
                    // Delta/Done/Error frame; this is
                    // just the EOF marker. We leave the
                    // status as-is — the worker thread
                    // is about to exit.
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    // Worker dropped the Sender (it
                    // returned from run_event_loop).
                    // Mark done; the join below will
                    // observe the AgentRunError.
                    if self.agent_run_state.status == "starting"
                        || self.agent_run_state.status == "running"
                    {
                        self.agent_run_state.status =
                            "failed".into();
                    }
                    break;
                }
            }
        }
        // If the worker has finished, join it and clear
        // the slot. We check `is_finished` first so we
        // never block the UI thread on `join()`.
        if handle.is_finished() {
            let (handle, rx) = self.agent_run_worker.take().unwrap();
            drop(rx);
            match handle.join() {
                Ok(AgentRunError::None) => {
                    // already handled by the matching
                    // Done / Error frame
                }
                Ok(AgentRunError::StartFailed(msg)) => {
                    use voice_bird_cli::cloud::run::{
                        classify_run_start_error, RunStartError,
                    };
                    self.agent_run_state.last_error = Some(msg.clone());
                    let typed = classify_run_start_error(&msg);
                    // 402 → user is on free tier. Mark
                    // plan_is_pro = Some(false) so the
                    // next auto-run is suppressed (D4.4
                    // reads this).
                    if typed == RunStartError::ProRequired {
                        self.agent_run_state.plan_is_pro = Some(false);
                    }
                    // 401 → bad API key. Open the
                    // api-key modal so the user can
                    // re-enter it. The modal is keyed
                    // off `api_key_buf.is_some()`.
                    if typed == RunStartError::BadApiKey {
                        self.open_api_key_modal();
                    }
                    self.agent_run_state.status = classify_start_error(
                        self.agent_run_state
                            .last_error
                            .as_deref()
                            .unwrap_or(""),
                    );
                }
                Ok(AgentRunError::StreamFailed(msg)) => {
                    self.agent_run_state.last_error = Some(msg);
                    self.agent_run_state.status = "failed".into();
                }
                Err(_) => {
                    // Worker panicked. Mark failed
                    // and let the user retry.
                    self.agent_run_state.status = "failed".into();
                }
             }
         }
    }

    /// Decide whether to start an agent run right now and,
    /// if so, start it. This is the single entry point
    /// every callsite (D4.4 auto path, the `g` keybind,
    /// the `stop_section` final-run hook) uses — the
    /// decision logic lives in `cloud::run::should_run_now`
    /// so it's pure-testable without an App.
    ///
    /// `trigger`:
    ///   - Auto: a new line was added. Respect the 65 s
    ///     floor and the queue.
    ///   - Manual: the user pressed `g`. Bypasses the
    ///     floor; if a run is in flight, sets `queued = true`
    ///     and the worker will re-run after the current
    ///     run completes.
    ///   - Stop: the user stopped the recording. Forces
    ///     a final run; bypasses the floor.
    ///
    /// `transcript` is the full merged role-labeled
    /// timeline, pre-truncated via `truncate_transcript`
    /// to fit the server's 200 000-char cap with margin.
    pub fn trigger_agent_run(
        &mut self,
        trigger: voice_bird_cli::cloud::run::RunTrigger,
        mut transcript: String,
    ) {
        use voice_bird_cli::cloud::run::{
            should_run_now, truncate_transcript, AgentRunError, RunTrigger,
        };
        // Manual: if a worker is already in flight, just
        // queue a one-shot and return. The worker will
        // re-run after it finishes (D4.3: drain sets
        // status to "completed" and the next tick
        // consults `queued`).
        if trigger == RunTrigger::Manual
            && self.agent_run_worker.is_some()
        {
            self.agent_run_state.queued = true;
            return;
        }
        let now = std::time::Instant::now();
        let decision = should_run_now(
            now,
            self.agent_run_state.last_run_started,
            self.agent_run_state.queued,
            self.agent_run_state.plan_is_pro,
            trigger,
        );
        if !decision {
            return;
        }
        // If the previous worker is still around (the
        // user pressed `g` while one was alive but
        // not yet DoneFinal), join it before spawning
        // the new one.
        if let Some((handle, rx)) = self.agent_run_worker.take() {
            drop(rx);
            let _ = handle.join();
        }
        // Truncate so the server's 200 000-char cap is
        // never hit (we ship 180 000 max).
        transcript = truncate_transcript(&transcript).into_owned();
        // Reset the queue flag — we're firing now.
        self.agent_run_state.queued = false;
        self.start_agent_run(transcript);
    }

    pub fn new() -> Self {
        // Test builds only: install the process-local config
        // tempdir before the first `AppConfig::load()`, so every
        // test that constructs an `App` reads from and saves to
        // the tempdir instead of the developer's real
        // `config.toml`. The deref MUST be cfg(test)-gated:
        // `INSTALL_TEST_CONFIG`'s init unconditionally redirects
        // `AppConfig::config_path()` to a fresh empty tempdir, so
        // running it in production would make every launch boot
        // from defaults (API key, devices, agent targets all
        // "gone") and write settings into /tmp.
        #[cfg(test)]
        let _ = &*voice_bird_cli::test_utils::INSTALL_TEST_CONFIG;
        let mut config = AppConfig::load().unwrap_or_default();
        // Windows is cloud-only: force cloud on in memory
        // regardless of what the on-disk config says. Covers
        // configs copied from another OS or hand-edited;
        // the on-disk format is unchanged.
        enforce_cloud_only_platform(&mut config);
        let config_path = AppConfig::config_path().ok();
        let mut config_was_loaded_from_disk =
            config_path.as_ref().map(|p| p.exists()).unwrap_or(false);

        // First launch: auto-pick a local model from system specs, persist
        // immediately, and skip the manual model-picker overlay. The user
        // can still override later with `m`. We treat the config as
        // "loaded from disk" once it exists so the picker behaves like a
        // normal navigation, not a first-run gate.
        if !config_was_loaded_from_disk {
            // No local models on cloud-only Windows — just persist the
            // defaults; the API-key modal below replaces the model picker
            // as the first-run gate.
            #[cfg(not(windows))]
            {
                let picked = voice_bird_cli::transcription::auto_select::pick_default_model();
                log::info!("first run: auto-picked local model = {picked}");
                config.default_slot_config.model = picked.into();
            }
            if let Err(e) = config.save() {
                log::error!("config save (first run): {e}");
            } else {
                config_was_loaded_from_disk = true;
            }
        }

        // Only the macOS screen-recording check below mutates this.
        #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
        let mut banner_on_launch: Option<String> =
            if config.default_slot_config.cloud_on && config.voicebird_api_key.is_empty() {
                Some("Cloud is on but no API key — press 'c' to paste one".into())
            } else {
                None
            };

        // macOS: warn early if Screen Recording permission is missing.
        // Without it, system-audio loopback and per-app capture both fail
        // at start, and SCShareableContent returns a near-empty app list.
        // TCC decisions don't propagate to a running process, so the
        // remedy is "grant + restart".
        #[cfg(target_os = "macos")]
        {
            if !voice_bird_cli::audio::loopback::loopback_macos::screen_recording_permission_granted(
            ) {
                banner_on_launch = Some(
                    "Screen Recording permission required for system / per-app audio — \
                     System Settings → Privacy & Security → Screen Recording → enable \
                     your terminal (or voice-bird), then restart"
                        .into(),
                );
            }
        }

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime");

        #[cfg_attr(not(windows), allow(unused_mut))]
        // Hardcode the Free Room as index 0 so the TUI never
        // has to special-case "no rooms". Cloud-fetched rooms
        // get appended by `refresh_rooms`.
        let mut rooms: Vec<Room> = vec![Room::free_room()];
        let active_room: usize = 0;
        let room_session_dir: Option<std::path::PathBuf> = None;
        let mut app = Self {
            mode: AppMode::Normal,
            devices: Vec::new(),
            apps: Vec::new(),
            selected_device_index: 0,
            selected_app_index: None,
            // Rooms list starts with Free Room at index 0; the user
            // grows it via the picker once refresh_rooms succeeds.
            selected_room_index: 0,
            picker_focus: PickerFocus::Devices,
            device_scroll: 0,
            app_scroll: 0,
            room_scroll: 0,
            status: RecordingStatus::Idle,
            audio_level: Arc::new(Mutex::new(0.0)),
            duration: 0.0,
            start_time: None,
            config: config.clone(),
            default_slot_config: config.default_slot_config.clone(),
            should_quit: false,
            status_message: None,
            error_channel: Arc::new(Mutex::new(None)),
            log_path: None,
            rt,
            slots: Self::fresh_slots(),
            focused_slot: SlotId(1),
            // Slot 1 was created by `fresh_slots()`. The next id we
            // hand out is 2.
            next_slot_id: 2,
            picker: None,
            config_was_loaded_from_disk,
            api_key_buf: None,
            path_buf: None,
            export_banner: None,
            banner: banner_on_launch,
            transcript_scroll: 0,
            transcript_follow: true,
            empty_committed: Arc::new(PlMutex::new(Vec::new())),
            empty_tentative: Arc::new(PlMutex::new(String::new())),
            pending_target_overrides: std::collections::BTreeMap::new(),
            rooms,
            active_room,
            plan_is_pro: None,
            slot_picker_memo: std::collections::BTreeMap::new(),
            app_events: Arc::new(PlMutex::new(VecDeque::new())),
            room_session_dir,
            agent_run_state: Default::default(),
            agent_run_worker: None,
        };
        app.refresh_rooms();
        // Re-activate the user's last room. If the slug no longer
        // exists in the catalog, fall back to Free Room and clear
        // the field so we don't keep reporting a missing room on
        // every launch.
        if let Some(slug) = app.config.last_room_slug.clone() {
            if let Some(idx) = app.rooms.iter().position(|r| r.slug == slug) {
                let _ = app.activate_room(idx);
            } else {
                app.config.last_room_slug = None;
            }
        }
        app
    }

    // -- Slot helpers -------------------------------------------------------

    /// Build the initial slot Vec. The TUI starts with a single empty
    /// slot (id 1) — the user expands the workspace with `+`. The
    /// id is stable across the app's lifetime: appended slots get
    /// fresh ids from `next_slot_id`, never renumbering the existing
    /// ones, so any handle a caller is holding stays valid.
    fn fresh_slots() -> Vec<Slot> {
        vec![Slot::empty(SlotId(1))]
    }

    /// Look up a slot's current Vec index from its stable id. Returns
    /// `None` if the id was never allocated (e.g. freed by a Phase B
    /// shrink). The Vec is the source of truth — ids are a stable
    /// handle for outside callers.
    pub fn slot_index(&self, id: SlotId) -> Option<usize> {
        self.slots.iter().position(|s| s.id == id)
    }

    /// Read-only access to a slot by id.
    pub fn slot_by_id(&self, id: SlotId) -> Option<&Slot> {
        self.slots.iter().find(|s| s.id == id)
    }

    // -- Section accessors --------------------------------------------------

    /// Currently focused section (the one `c`/`l`/`m`/`s` operate on),
    /// or `None` if no section is running in that slot.
    pub fn focused(&self) -> Option<&Section> {
        self.slot_by_id(self.focused_slot)
            .and_then(|s| s.as_section())
    }

    /// Mutable variant of [`focused`].
    pub fn focused_mut(&mut self) -> Option<&mut Section> {
        let id = self.focused_slot;
        self.slots
            .iter_mut()
            .find(|s| s.id == id)
            .and_then(|s| match &mut s.kind {
                SlotKind::Recording { section } => Some(section),
                _ => None,
            })
    }

    /// Number of slots currently recording.
    pub fn active_section_count(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| matches!(s.kind, SlotKind::Recording { .. }))
            .count()
    }

    /// Engine kind label for the focused section. Empty string when idle.
    pub fn focused_engine_kind(&self) -> &str {
        self.focused().map(|s| s.engine_kind.as_str()).unwrap_or("")
    }

    /// Whether the focused section is actively broadcasting to cloud.
    pub fn focused_cloud_active(&self) -> bool {
        self.focused().map(|s| s.cloud_active).unwrap_or(false)
    }

    /// Cloud-reminder expiry of the focused section, if any.
    pub fn focused_cloud_reminder_until(&self) -> Option<std::time::Instant> {
        self.focused().and_then(|s| s.cloud_reminder_until)
    }

    /// Cloud on/off as displayed in the Mode panel: focused section's
    /// setting if one is running, else the per-source override for
    /// the picker-resolved source (so the badge reflects what would
    /// actually take effect if the user pressed Enter on this slot),
    /// else the global config default. Falls through to global only
    /// when no picker source can be resolved.
    /// Cloud ON/OFF as displayed in the Mode panel.
    ///
    /// Resolution order:
    /// 1. Focused recording section's `settings.cloud_on` (the
    ///    value the running engine is using).
    /// 2. Focused slot's `slot.config.cloud_on` if customized.
    /// 3. Picker-resolved source's slot customizations (when
    ///    no slot is focused, the source settles).
    /// 4. `default_slot_config.cloud_on` (the global default).
    pub fn display_cloud_on(&self) -> bool {
        if let Some(s) = self.focused() {
            return s.settings.cloud_on;
        }
        if let Some(slot) = self.slot_by_id(self.focused_slot) {
            if let Some(c) = slot.config.cloud_on {
                return c;
            }
        }
        self.default_slot_config.cloud_on
    }

    /// Language as displayed in the Mode panel: focused section's
    /// Language as displayed in the Mode panel: focused
    /// section's setting if running, else the focused slot's
    /// customized `slot.config.language`, else the global
    /// `default_slot_config.language`.
    pub fn display_language(&self) -> String {
        if let Some(s) = self.focused() {
            return s.settings.language.clone();
        }
        if let Some(slot) = self.slot_by_id(self.focused_slot) {
            if let Some(lang) = &slot.config.language {
                return lang.clone();
            }
        }
        self.default_slot_config.language.clone()
    }

    /// Model id as displayed in the Mode panel: focused
    /// section's setting if running, else the focused slot's
    /// customized `slot.config.model`, else the global
    /// `default_slot_config.model`.
    pub fn display_model(&self) -> String {
        if let Some(s) = self.focused() {
            return s.settings.model.clone();
        }
        if let Some(slot) = self.slot_by_id(self.focused_slot) {
            if let Some(m) = &slot.config.model {
                return m.clone();
            }
        }
        self.default_slot_config.model.clone()
    }

    /// The focused slot's current target (Stdout / Cloud), or `None`
    /// when the slot has never been used. UI uses this to drive the
    /// Agents pane.
    pub fn focused_target(&self) -> Option<Target> {
        self.slot_by_id(self.focused_slot).and_then(|s| s.target())
    }

    /// The current pending target for the focused slot, falling
    /// through to the slot's last-used target, then Stdout. The UI
    /// uses this for the per-slot title and the agents pane's
    /// "active" marker.
    pub fn focused_pending_target(&self) -> Target {
        self.pending_target_overrides
            .get(&self.focused_slot)
            .cloned()
            .or_else(|| self.focused_target())
            .unwrap_or(Target::Stdout)
    }
    /// The currently active room. Returns `&Room::free_room()` if
    /// `active_room` is somehow out of range (defensive — `App::new`
    /// and `activate_room` both keep it valid).
    pub fn active_room(&self) -> &Room {
        self.rooms
            .get(self.active_room)
            .unwrap_or_else(|| self.rooms.first().expect("Free Room at index 0"))
    }

    /// Activate the room at `idx`. Refuses (returns `Err`) when any
    /// slot is currently recording — switching mid-session would
    /// span different room sessions and corrupt the merged
    /// timeline.
    ///
    /// On success, the focused slot set is replaced: Free Room
    /// gets a single empty slot; agent rooms get one empty slot
    /// per role. The cursor lands on the first slot.
    pub fn activate_room(&mut self, idx: usize) -> Result<(), String> {
        if idx >= self.rooms.len() {
            return Err(format!("room index {idx} out of range"));
        }
        if self.active_section_count() > 0 {
            return Err("stop recording before switching rooms".to_string());
        }
        // Locked Pro rooms can be activated for display, but
        // agent runs against them will hit 402. We let the
        // TUI surface the lock with 🔒 and skip the call.
        self.active_room = idx;
        let room = self.active_room().clone();
        if room.slug == "free" {
            self.slots = Self::fresh_slots();
        } else {
            // Write room.json up front so the per-role session
            // dirs land inside a directory the operator can
            // identify. IO failures are surfaced as a banner but
            // don't fail the activation — the user can still
            // record; the per-role finalize path will surface
            // write errors itself.
            let started_at = chrono::Utc::now();
            let base = voice_bird_cli::config::AppConfig::expand_tilde(
                &self.config.default_slot_config.path,
            );
            let base = std::path::PathBuf::from(base);
            let room_dir = voice_bird_cli::session::layout::room_session_dir(
                &base,
                started_at,
                &room.slug,
            );
            if let Err(e) = voice_bird_cli::room_fs::write_room_json(
                &room_dir,
                &room,
                started_at,
            ) {
                self.banner = Some(format!(
                    "could not write room.json: {e}"
                ));
            }
            self.room_session_dir = Some(room_dir);
            let next_id = self.next_slot_id;
            self.slots = room
                .roles
                .iter()
                .enumerate()
                .map(|(i, role)| {
                    let id = SlotId(next_id + i as u32);
                    let mut slot = Slot::empty(id);
                    slot.role = Some(role.clone());
                    slot
                })
                .collect();
            self.next_slot_id = next_id + self.slots.len() as u32;
        }
        Ok(())
    }

    /// Re-fetch the cloud Rooms list from voicebird.app and append
    /// the catalog entries to `self.rooms` (Free Room stays at
    /// index 0). No-op when the user has no API key set or the
    /// server URL is empty. On HTTP failure, the existing
    /// `self.rooms` (Free Room only) is preserved and a banner
    /// explains the failure — picker emptiness alone is too
    /// quiet and looks identical to "fetch hasn't run yet".
    pub fn refresh_rooms(&mut self) {
        if self.config.voicebird_api_key.is_empty()
            || self.config.voicebird_server_url.is_empty()
        {
            self.rooms.truncate(1);
            return;
        }
        let base = voice_bird_cli::cloud::http::rest_base_url(
            &self.config.voicebird_server_url,
        );
        match voice_bird_cli::cloud::rooms::fetch(
            &base,
            &self.config.voicebird_api_key,
        ) {
            Ok(list) => {
                log::info!("refresh_rooms: fetched {} rooms", list.rooms.len());
                self.plan_is_pro = Some(list.plan_is_pro());
                let mut rooms = vec![Room::free_room()];
                rooms.extend(list.rooms);
                self.rooms = rooms;
            }
            Err(e) => {
                log::warn!("refresh_rooms: fetch failed: {e}");
                // Keep the existing rooms (at minimum Free Room)
                // and surface the failure as a banner.
                self.banner = Some(format!(
                    "Rooms list unavailable: {e}. \
                     Check the API key and voicebird_server_url."
                ));
            }
        }
    }



    /// Committed-transcript Arc for the focused section, or saved
    /// transcript, or an empty fallback when nothing is available.
    pub fn focused_committed(&self) -> Arc<PlMutex<Vec<CommittedLine>>> {
        self.focused()
            .map(|s| s.committed.clone())
            .or_else(|| {
                self.slot_by_id(self.focused_slot)
                    .and_then(|s| match &s.kind {
                        SlotKind::Saved { saved } => Some(saved.committed.clone()),
                        _ => None,
                    })
            })
            .unwrap_or_else(|| self.empty_committed.clone())
    }

    pub fn focused_tentative(&self) -> Arc<PlMutex<String>> {
        self.focused()
            .map(|s| s.tentative.clone())
            .unwrap_or_else(|| self.empty_tentative.clone())
    }

    /// Merge every running + saved section's transcript lines
    /// into a single role-labeled timeline. The merge key is
    /// the absolute wall-clock time so the user can read the
    /// conversation as it actually happened, even if the roles
    /// started at slightly different times.
    ///
    /// Refined lines (when present) win over raw at the same
    /// `t_start_ms` — the agent run path always shows the best
    /// available text per role per moment. Empty-text lines are
    /// filtered (a refined line that's a placeholder doesn't
    /// pollute the view).
    pub fn merged_timeline(&self) -> Vec<TimelineEntry> {
        let mut out: Vec<TimelineEntry> = Vec::new();
        for slot in &self.slots {
            let (role_label, started, refined_lines, committed_lines) = match &slot.kind {
                SlotKind::Recording { section } => (
                    section.role.as_ref().map(|r| r.name.clone()),
                    section.session_started_at,
                    section
                        .refined
                        .lock()
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>(),
                    section
                        .committed
                        .lock()
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>(),
                ),
                SlotKind::Saved { saved } => (
                    saved.role.as_ref().map(|r| r.name.clone()),
                    saved.session_started_at,
                    saved
                        .refined
                        .lock()
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>(),
                    saved
                        .committed
                        .lock()
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>(),
                ),
                SlotKind::Empty => continue,
            };
            for line in &refined_lines {
                if line.text.trim().is_empty() {
                    continue;
                }
                let at = started
                    + chrono::Duration::milliseconds(line.t_start_ms as i64);
                out.push(TimelineEntry {
                    at,
                    role: role_label.clone(),
                    slot: slot.id,
                    text: line.text.clone(),
                });
            }
            for line in &committed_lines {
                if line.text.trim().is_empty() {
                    continue;
                }
                let at = started
                    + chrono::Duration::milliseconds(line.t_start_ms as i64);
                out.push(TimelineEntry {
                    at,
                    role: role_label.clone(),
                    slot: slot.id,
                    text: line.text.clone(),
                });
            }
        }
        out.sort_by(|a, b| a.at.cmp(&b.at).then(a.slot.0.cmp(&b.slot.0)));
        out
    }

    /// Open the API-key modal, seeding the buffer with whatever key is
    /// currently saved (so backspace can edit it rather than starting
    /// from scratch). `reverts_cloud` is set on the App so the Esc
    /// arm can decide whether cancelling the modal should also
    /// revert the just-toggled `cloud_broadcast_enabled` to its
    /// pre-toggle value:
    ///  - `true`  — caller flipped Cloud ON with no key; Esc must
    ///    unwind the flip (the pre-R-key world had no other way
    ///    to exit the cloud-enable flow).
    ///  - `false` — caller just wants to look at / edit the saved
    ///    key (e.g. `K` peek, first-run bootstrap, auth-recovery
    ///    pre-flight). Esc closes the modal silently.
    ///
    /// Capture the saved key into `api_key_buf` and transition
    /// to `AppMode::ApiKeyModal`. The modal is the only text-input
    /// flow in the TUI — opens for cloud-enable (after `c`)
    /// and auth-error recovery alike. Esc closes the modal
    /// without side effects; the slot's per-slot `cloud_on`
    /// (or the default) keeps the user's last toggle, ready
    /// to re-flip on next `c` press.
    pub fn open_api_key_modal(&mut self) {
        self.api_key_buf = Some(self.config.voicebird_api_key.clone());
        self.mode = AppMode::ApiKeyModal;
    }

    /// Open the output-path modal, seeding the buffer with the
    /// current `session_dir` from config. Local-only concept — the 'p'
    /// key doesn't exist on cloud-only Windows.
    #[cfg(not(windows))]
    pub fn open_path_modal(&mut self) {
        // Pre-fill with the focused slot's customized path,
        // or the default if the slot has no customization.
        let initial = self
            .slot_by_id(self.focused_slot)
            .and_then(|s| s.config.path.clone())
            .unwrap_or_else(|| self.default_slot_config.path.clone());
        self.path_buf = Some(initial);
        self.mode = AppMode::PathModal;
    }

    /// Export the most recent un-exported local transcript to the
    /// Voice Bird cloud. Idempotent — if the session already has a
    /// `.uploaded` marker, returns early with a "Already uploaded"
    /// message. Local-only concept — the 'e' key doesn't exist on
    /// cloud-only Windows.
    #[cfg(not(windows))]
    pub fn export_transcript(&mut self) {
        // Clear any previous export banner
        self.export_banner = None;

        // Look in the focused slot's customized output path,
        // or the default slot's path if no customization.
        // `expand_tilde` resolves `~/` to the home dir so
        // the default `~/voice-bird/sessions` lands at the
        // user's actual home directory rather than a literal
        // `./~/voice-bird/...` relative path.
        let base_dir = voice_bird_cli::config::AppConfig::expand_tilde(
            &self
                .slot_by_id(self.focused_slot)
                .and_then(|s| s.config.path.clone())
                .unwrap_or_else(|| self.default_slot_config.path.clone()),
        );
        let base = std::path::Path::new(&base_dir);

        // Find the most recent session that has a transcript.json.
        let Some(session_dir) = find_latest_session(base) else {
            self.export_banner = Some("No sessions found to export".into());
            return;
        };

        // Client-side idempotency: if already uploaded, stop here.
        if session_dir.join(".uploaded").exists() {
            self.export_banner = Some("Already exported \u{2713}".into());
            return;
        }

        let transcript_path = session_dir.join("transcript.json");

        // Read and parse the transcript
        let transcript_json = match std::fs::read_to_string(&transcript_path) {
            Ok(s) => s,
            Err(e) => {
                self.export_banner = Some(format!("Failed to read transcript: {e}"));
                return;
            }
        };

        let transcript: voice_bird_cli::session::finalize::FinalTranscriptValue =
            match serde_json::from_str(&transcript_json) {
                Ok(v) => v,
                Err(e) => {
                    self.export_banner = Some(format!("Failed to parse transcript: {e}"));
                    return;
                }
            };

        // Build the plain-text content from segments
        let content: String = transcript
            .segments
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");

        // Derive HTTP URL from the WebSocket server URL
        let http_url = ws_url_to_http(&self.config.voicebird_server_url);
        let api_url = format!("{http_url}/api/transcripts/upload");

        // Build the session_id from the directory name (slug)
        let session_id = session_dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        // Derive a title: source + date
        let source = transcript.meta.source.clone();
        let date = transcript
            .meta
            .started_at
            .chars()
            .take(10)
            .collect::<String>();
        let title = format!("{source} — {date}");

        // POST to the server
        let body = serde_json::json!({
            "session_id": session_id,
            "title": title,
            "content": content,
            "segments": transcript.segments,
            "meta": {
                "session_id": session_id,
                "version": transcript.meta.version,
                "model": transcript.meta.model,
                "engine": transcript.meta.engine,
                "source": transcript.meta.source,
                "device": transcript.meta.device,
                "started_at": transcript.meta.started_at,
                "ended_at": transcript.meta.ended_at,
                "duration_sec": transcript.meta.duration_ms / 1000,
                "device_type": "audioinput",
                "language": "en",
            }
        });

        let api_key = self.config.voicebird_api_key.clone();

        let result: Result<serde_json::Value, String> =
            std::thread::spawn(move || {
                match ureq::post(&api_url)
                    .set("Authorization", &format!("Bearer {api_key}"))
                    .set("Content-Type", "application/json")
                    .timeout(std::time::Duration::from_secs(15))
                    .send_json(&body)
                {
                    Ok(resp) => {
                        let json: serde_json::Value = resp.into_json().unwrap_or_default();
                        Ok(json)
                    }
                    Err(ureq::Error::Status(_, resp)) => {
                        let msg = resp.into_string().unwrap_or_default();
                        Err(format!("Server error: {msg}"))
                    }
                    Err(e) => Err(format!("Network error: {e}")),
                }
            })
            .join()
            .unwrap_or(Err("Export thread panicked".into()));

        match result {
            Ok(json) => {
                let dup = json
                    .get("duplicate")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let id = json
                    .get("transcription_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");

                // Write the .uploaded marker for idempotency
                let marker = session_dir.join(".uploaded");
                let _ = std::fs::write(&marker, id);

                if dup {
                    self.export_banner = Some("Already exported ✓".into());
                } else {
                    self.export_banner = Some(format!("Exported ✓ — {id}"));
                }
            }
            Err(msg) => {
                self.export_banner = Some(format!("Export failed: {msg}"));
            }
        }
    }

    /// Scroll the transcript up by `n` lines. Disables auto-follow.
    /// When a section is focused, mutates that section's scroll state;
    /// otherwise falls back to the App-level scroll (idle preview).
    pub fn scroll_transcript_up(&mut self, n: u16) {
        if let Some(s) = self.focused_mut() {
            s.transcript_scroll = s.transcript_scroll.saturating_sub(n);
            s.transcript_follow = false;
        } else {
            self.transcript_scroll = self.transcript_scroll.saturating_sub(n);
            self.transcript_follow = false;
        }
    }

    /// Scroll the transcript down by `n` lines. Disables auto-follow —
    /// the render path clamps the offset against the total line count.
    pub fn scroll_transcript_down(&mut self, n: u16) {
        if let Some(s) = self.focused_mut() {
            s.transcript_scroll = s.transcript_scroll.saturating_add(n);
            s.transcript_follow = false;
        } else {
            self.transcript_scroll = self.transcript_scroll.saturating_add(n);
            self.transcript_follow = false;
        }
    }

    /// Jump to the top and pin there.
    pub fn scroll_transcript_home(&mut self) {
        if let Some(s) = self.focused_mut() {
            s.transcript_scroll = 0;
            s.transcript_follow = false;
        } else {
            self.transcript_scroll = 0;
            self.transcript_follow = false;
        }
    }

    /// Re-enable auto-follow so the view tracks the latest content.
    pub fn scroll_transcript_end(&mut self) {
        if let Some(s) = self.focused_mut() {
            s.transcript_follow = true;
        } else {
            self.transcript_follow = true;
        }
    }

    /// Cycle the focused slot forward (Tab). Pure positional
    /// cycling — every slot, including Empty, is reachable.
    /// Empty slots land the picker on a focused but unstarted
    /// slot so the user can press Enter to start a recording
    /// or `pick_target` to queue a routing override without
    /// having to click into the column first.
    /// Wraps from the last slot back to the first.
    pub fn focus_next(&mut self) {
        if let Some(idx) = self.slot_index(self.focused_slot) {
            let n = self.slots.len();
            let next_idx = (idx + 1) % n;
            let next_slot = self.slots[next_idx].id;
            // Save the outgoing slot's cursor state so Tab-back
            // restores it. Restore the incoming slot's memoized
            // cursor (or keep the current one if no memo yet —
            // first focus inherits whatever the cursor is at).
            self.memoize_picker_for(self.focused_slot);
            self.focused_slot = next_slot;
            self.restore_picker_for(next_slot);
        }
    }

    /// Cycle the focused slot backward (Shift-Tab). Mirrors
    /// [`focus_next`] — every slot, no skipping.
    pub fn focus_prev(&mut self) {
        if let Some(idx) = self.slot_index(self.focused_slot) {
            let n = self.slots.len();
            let prev_idx = (idx + n - 1) % n;
            let prev_slot = self.slots[prev_idx].id;
            self.memoize_picker_for(self.focused_slot);
            self.focused_slot = prev_slot;
            self.restore_picker_for(prev_slot);
        }
    }
    /// Pick the first Empty slot, or `None` if every existing slot
    /// is busy. Callers that need to grow past `len()` should call
    /// `add_slot` instead — this stays narrow on purpose.
    pub fn next_free_slot(&self) -> Option<SlotId> {
        self.slots
            .iter()
            .find(|s| matches!(s.kind, SlotKind::Empty))
            .map(|s| s.id)
    }

    /// Append a new empty slot to the workspace and focus it.
    /// Returns the new slot's id. Refuses (and returns `None`) when
    /// the slot count is already at `MAX_SECTIONS` — the cap exists
    /// to keep the screen readable, not as a feature limitation.
    /// The new slot inherits the focused slot's current picker
    /// cursor (so `+` followed by Enter records from the same
    /// device/app) — memoize the outgoing slot first so Tab back
    /// to it still works.
    pub fn add_slot(&mut self) -> Option<SlotId> {
        if self.slots.len() >= MAX_SECTIONS {
            return None;
        }
        let id = SlotId(self.next_slot_id);
        self.next_slot_id += 1;
        // Snapshot the current cursor under the outgoing slot so
        // Tab back to it after `+` restores its state.
        self.memoize_picker_for(self.focused_slot);
        self.slots.push(Slot::empty(id));
        self.focused_slot = id;
        Some(id)
    }

    /// Remove the focused slot. Refuses (returns `false`) when the
    /// focused slot is currently recording — the user must `s`top
    /// the section first to avoid orphaning a tokio task. Also
    /// refuses when only one slot is left (the TUI always shows
    /// at least one slot).
    pub fn remove_focused_slot(&mut self) -> bool {
        if self.slots.len() <= 1 {
            return false;
        }
        let id = self.focused_slot;
        let pos = match self.slot_index(id) {
            Some(p) => p,
            None => return false,
        };
        if matches!(self.slots[pos].kind, SlotKind::Recording { .. }) {
            return false;
        }
        self.slots.remove(pos);
        // Drop the focused slot's per-slot state — the slot id is
        // gone and `next_slot_id` won't reuse it, so the memo would
        // otherwise be leaked for the rest of the session.
        self.pending_target_overrides.remove(&id);
        self.slot_picker_memo.remove(&id);
        // Land focus on the nearest remaining slot. Prefer the slot
        // to the left of the removed one, else the new rightmost.
        let new_idx = if pos > 0 { pos - 1 } else { 0 };
        if let Some(s) = self.slots.get(new_idx) {
            self.focused_slot = s.id;
            // Restore the new focused slot's memoized cursor so
            // `-` doesn't accidentally reset the cursor to what
            // the removed slot was showing.
            self.restore_picker_for(s.id);
        }
        true
    }

    /// Refresh both panes' inventory. Preserves cursors by name when the
    /// focused slot's cursor is re-resolved in place; per-slot
    /// memos for non-focused slots are also re-resolved so a later
    /// Tab back to them lands on the same device/app the user
    /// picked, even if the OS audio inventory reordered (e.g. a
    /// USB headset reappearing at a different index).
    pub fn refresh_inventory(&mut self) {
        use voice_bird_cli::config::AudioSessionKind;
         // Snapshot the focused slot's current device/app names
        // BEFORE replacing `devices` / `apps`, plus the per-slot
        // memo's device/app names for every other slot. Both are
        // re-resolved by name after the rebuild.
        let focused_prior_device = self
            .devices
            .get(self.selected_device_index)
            .map(|d| (d.name.clone(), d.kind));
        let focused_prior_app = self
            .selected_app_index
            .and_then(|i| self.apps.get(i))
            .map(|a| a.id.clone());
        let memo_priors: std::collections::BTreeMap<
            SlotId,
            (Option<(String, AudioSessionKind)>, Option<String>),
        > = self
            .slot_picker_memo
            .iter()
            .filter(|(slot, _)| **slot != self.focused_slot)
            .map(|(slot, sel)| {
                let dev = self
                    .devices
                    .get(sel.device_idx)
                    .map(|d| (d.name.clone(), d.kind));
                let app = sel
                    .app_idx
                    .and_then(|i| self.apps.get(i))
                    .map(|a| a.id.clone());
                (*slot, (dev, app))
            })
            .collect();

        match crate::platform::enumerate_audio_inventory() {
            Ok(inv) => {
                self.devices = inv.devices;
                self.apps = inv.apps;
            }
            Err(e) => {
                self.status_message =
                    Some(format!("Failed to enumerate audio inventory: {}", e));
            }
        }

        // Re-resolve focused slot.
        if let Some((name, kind)) = focused_prior_device {
            if let Some(i) = self
                .devices
                .iter()
                .position(|d| d.name == name && d.kind == kind)
            {
                self.selected_device_index = i;
            } else if self.selected_device_index >= self.devices.len() {
                self.selected_device_index = self.devices.len().saturating_sub(1);
            }
        }
        self.selected_app_index = focused_prior_app
            .as_deref()
            .and_then(|id| self.apps.iter().position(|a| a.id == id));

        // Re-resolve each non-focused slot's memo by name.
        for (slot, (dev, app)) in memo_priors {
            if let Some(sel) = self.slot_picker_memo.get_mut(&slot) {
                if let Some((name, kind)) = dev {
                    if let Some(i) = self
                        .devices
                        .iter()
                        .position(|d| d.name == name && d.kind == kind)
                    {
                        sel.device_idx = i;
                    }
                    // If the device disappeared, leave the index
                    // alone — `restore_picker_for` clamps against
                    // the new `devices.len()`.
                }
                if let Some(id) = app {
                    sel.app_idx = self.apps.iter().position(|a| a.id == id);
                    // Same clamping story for apps.
                }
            }
        }

        self.clamp_scrolls(usize::MAX);
    }

    /// Move the cursor up one row in whichever pane is focused.
    /// In the Agents pane, disabled rows (currently just `Agent` when
    /// the binary is missing) are skipped so the cursor never parks
    /// on a row that can't be picked.
    pub fn select_previous(&mut self) {
        match self.picker_focus {
            PickerFocus::Devices => {
                if self.selected_device_index > 0 {
                    self.selected_device_index -= 1;
                }
            }
            PickerFocus::Apps => {
                let i = self.selected_app_index.unwrap_or(0);
                let next = i.saturating_sub(1);
                if !self.apps.is_empty() {
                    self.selected_app_index = Some(next);
                }
            }
            PickerFocus::Rooms => {
                if self.selected_room_index > 0 {
                    self.selected_room_index -= 1;
                }
            }
        }
        log::debug!(
            "picker: ↑ focus={:?} dev_idx={} (={:?}) app_idx={:?} (={:?}) room_idx={} (={:?})",
            self.picker_focus,
            self.selected_device_index,
            self.devices
                .get(self.selected_device_index)
                .map(|d| d.name.clone()),
            self.selected_app_index,
            self.selected_app_index
                .and_then(|i| self.apps.get(i))
                .map(|a| a.name.clone()),
            self.selected_room_index,
            self.rooms.get(self.selected_room_index).map(|r| r.slug.clone()),
        );
    }

    /// Move the cursor down one row in whichever pane is focused.
    pub fn select_next(&mut self) {
        match self.picker_focus {
            PickerFocus::Devices => {
                if self.selected_device_index + 1 < self.devices.len() {
                    self.selected_device_index += 1;
                }
            }
            PickerFocus::Apps => {
                if self.apps.is_empty() {
                    return;
                }
                let i = self.selected_app_index.unwrap_or(0);
                if i + 1 < self.apps.len() {
                    self.selected_app_index = Some(i + 1);
                } else if self.selected_app_index.is_none() {
                    self.selected_app_index = Some(0);
                }
            }
            PickerFocus::Rooms => {
                if self.rooms.is_empty() {
                    return;
                }
                if self.selected_room_index + 1 < self.rooms.len() {
                    self.selected_room_index += 1;
                }
            }
        }
        log::debug!(
            "picker: ↓ focus={:?} dev_idx={} (={:?}) app_idx={:?} (={:?}) room_idx={} (={:?})",
            self.picker_focus,
            self.selected_device_index,
            self.devices
                .get(self.selected_device_index)
                .map(|d| d.name.clone()),
            self.selected_app_index,
            self.selected_app_index
                .and_then(|i| self.apps.get(i))
                .map(|a| a.name.clone()),
            self.selected_room_index,
            self.rooms.get(self.selected_room_index).map(|r| r.slug.clone()),
        );
    }
    /// Clamp pane scroll offsets so the cursor row stays inside the
    /// viewport. `visible` is the inner row count of one pane (rough
    /// upper bound is fine — the render path also clamps); pass
    /// `usize::MAX` after a refresh to just clamp by list length.
    pub fn clamp_scrolls(&mut self, visible: usize) {
        let v = visible.max(1) as u16;
        let dev_max = self.devices.len().saturating_sub(1) as u16;
        let app_max = self.apps.len().saturating_sub(1) as u16;
        let room_max = self.rooms.len().saturating_sub(1) as u16;
        let dev_idx = (self.selected_device_index as u16).min(dev_max);
        let app_idx = self
            .selected_app_index
            .map(|i| (i as u16).min(app_max))
            .unwrap_or(0);
        let room_idx = (self.selected_room_index as u16).min(room_max);
        if dev_idx < self.device_scroll {
            self.device_scroll = dev_idx;
        } else if dev_idx >= self.device_scroll.saturating_add(v) {
            self.device_scroll = dev_idx + 1 - v;
        }
        if app_idx < self.app_scroll {
            self.app_scroll = app_idx;
        } else if app_idx >= self.app_scroll.saturating_add(v) {
            self.app_scroll = app_idx + 1 - v;
        }
        if room_idx < self.room_scroll {
            self.room_scroll = room_idx;
        } else if room_idx >= self.room_scroll.saturating_add(v) {
            self.room_scroll = room_idx + 1 - v;
        }
    }

    /// Clear any app selection (Space key in the Apps pane). After this,
    /// Enter starts a section using the focused device alone.
    pub fn clear_app_selection(&mut self) {
        self.selected_app_index = None;
        self.app_scroll = 0;
    }

    /// Currently focused (Devices pane) entry, if any.
    pub fn focused_device(&self) -> Option<&AudioDevice> {
        self.devices.get(self.selected_device_index)
    }

    /// Per-slot Devices pane entry. For the focused slot, returns
    /// the live cursor (same as [`focused_device`]). For a
    /// non-focused slot, returns the device the slot's
    /// `slot_picker_memo` points at — the one that will be
    /// restored when the user Tabs back. This is what the slot
    /// title renders, so non-focused slots stay frozen at their
    /// last pick while the user moves the cursor elsewhere.
    pub fn slot_device(&self, slot_id: SlotId) -> Option<&AudioDevice> {
        let idx = if slot_id == self.focused_slot {
            self.selected_device_index
        } else {
            self.slot_picker_memo
                .get(&slot_id)
                .map(|p| p.device_idx)
                .unwrap_or(self.selected_device_index)
        };
        self.devices.get(idx)
    }

    /// the user has cleared the selection or no apps are available.
    pub fn focused_app(&self) -> Option<&AppSession> {
        self.selected_app_index.and_then(|i| self.apps.get(i))
    }

    /// Per-slot Apps pane entry. For the focused slot, returns the
    pub fn slot_app(&self, slot_id: SlotId) -> Option<&AppSession> {
        let idx = if slot_id == self.focused_slot {
            self.selected_app_index
        } else {
            // Non-focused slot with no memo: fall back to the
            // global cursor (i.e. the first focus on this slot
            // inherits whatever the cursor is currently on).
            self.slot_picker_memo
                .get(&slot_id)
                .and_then(|p| p.app_idx)
                .or(self.selected_app_index)
        };
        idx.and_then(|i| self.apps.get(i))
    }
    /// Resolve the source the focused Devices + Apps pickers
    /// would resolve to on Enter. Wraps `resolve_picker_source`
    /// (the canonical picker→source match) with the error
    /// strings the TUI surfaces as banners for the three
    /// invalid-picker configurations.
    ///
    /// Single source of truth for the source-resolution
    /// logic + error message catalog. The Enter handler in
    /// `main.rs` (which calls `try_start_new_section`) and
    /// the resume path (`resume_section` for paused slots)
    /// both go through here, so the error strings can never
    /// drift between the two flows. `resolve_picker_source`
    /// is the lower-level helper for callers that don't need
    /// a `Result` (e.g. the idle `c` toggle, which writes
    /// the per-source override regardless of which None
    /// variant applies).
    pub fn resolve_focused_source(&self) -> Result<SessionSource, String> {
        use voice_bird_cli::config::AudioSessionKind;
        let dev = self.focused_device().cloned().ok_or_else(|| {
            "No audio device selected — press [r] to refresh".to_string()
        })?;
        let app_pick = self.focused_app().cloned();
        // Reject Input + App: per-app loopback can't pair
        // with a mic capture, and silently recording the mic
        // would surprise the user. Same check as
        // `try_start_new_section` and the Enter handler.
        if matches!(dev.kind, AudioSessionKind::Input) && app_pick.is_some() {
            return Err(
                "Mic + per-app capture isn't supported — pick an output device or [Space] to clear the app"
                    .to_string(),
            );
        }
        // Delegate the (kind, app) → SessionSource map to
        // `resolve_picker_source` so the two functions
        // can't disagree on which picker combinations
        // produce which source — and so a future
        // `AudioSessionKind::Loopback` variant (or similar)
        // only has to be added in one place.
        self.resolve_picker_source().ok_or_else(|| {
            // `resolve_picker_source` returns `None` only
            // when the device kind is `AudioSessionKind::App`
            // (the Devices pane never emits that, but we
            // defend against it).
            "Unexpected device kind — press [r] to refresh".to_string()
        })
    }

    /// Row index of the device currently picked for the focused slot.
    /// The picker is always pinned to the slot that's recording (or
    /// about to record), so this matches what `start_section` will
    /// consume. Falls back to the last-saved device from config when
    /// the slot has never been started — that's still "picked" from
    /// the user's perspective.
    pub fn picked_device_idx(&self) -> Option<usize> {
        // First preference: the live cursor.
        if let Some(d) = self.focused_device() {
            // Re-resolve by name+kind so the row survives a `r`efresh
            // that reordered the inventory.
            if let Some(saved) = self.config.input_device.as_deref() {
                if d.name == saved && Some(d.kind) == self.config.input_device_kind {
                    return Some(self.selected_device_index);
                }
            }
            return Some(self.selected_device_index);
        }
        // Fallback: the saved device from config.
        if let Some(name) = self.config.input_device.clone() {
            let kind = self.config.input_device_kind;
            if let Some(i) = self
                .devices
                .iter()
                .position(|d| d.name == name && Some(d.kind) == kind)
            {
                return Some(i);
            }
        }
        None
    }

    /// Picked-app row index in the rendered Apps list. Accounts for
    /// the synthetic "(no app — device only)" row that sits at
    /// visible row 0 of the pane — the apps Vec itself starts at 0
    /// but the first rendered row is the synthesis. So:
    /// `None` → no opinion yet (no app saved, none focused),
    /// `Some(0)` → (no app) is picked,
    /// `Some(k+1)` → app[k] is picked.
    pub fn picked_app_idx(&self) -> Option<usize> {
        if let Some(i) = self.selected_app_index {
            // +1 to skip the synthetic "no app" row at the top of
            // the pane.
            return Some(i + 1);
        }
        if let Some(id) = self.config.last_app_id.as_deref() {
            if let Some(i) = self.apps.iter().position(|a| a.id == id) {
                return Some(i + 1);
            }
        }
        // (no app) is the default — surface it as row 0.
        if !self.apps.is_empty() {
            return Some(0);
        }
        None
    }


    /// Update duration from start time
    pub fn update_duration(&mut self) {
        if let Some(start) = self.start_time {
            self.duration = start.elapsed().as_secs_f32();
        }
    }

    /// Format duration as MM:SS
    pub fn format_duration(&self) -> String {
        let total_secs = self.duration as u32;
        let mins = total_secs / 60;
        let secs = total_secs % 60;
        format!("{:02}:{:02}", mins, secs)
    }

    /// Check for errors from recording threads and update status
    pub fn check_error(&mut self) {
        if let Ok(mut err) = self.error_channel.lock() {
            if let Some(msg) = err.take() {
                self.status = RecordingStatus::Error(msg);
            }
        }
    }

    /// Drain any engine error published by any active section's consumer
    /// task into `banner` and flip the status to `Error`. Called on each
    /// UI tick. The original silent WhisperKit→whisper-rs restart was
    /// intentionally not implemented; we surface the error as a red
    /// banner and let the user press `r` to retry.
    pub fn check_engine_error(&mut self) {
        // Drain all sections, but only one banner — the most recent error
        // wins (last write of multiple in the same tick survives).
        let mut drained: Option<String> = None;
        for slot in self.slots.iter() {
            if let SlotKind::Recording { section } = &slot.kind {
                if let Ok(mut err) = section.engine_error_channel.lock() {
                    if let Some(msg) = err.take() {
                        drained = Some(msg);
                    }
                }
            }
        }
        if let Some(msg) = drained {
            let auth_failure = looks_like_auth_error(&msg);
            self.banner = Some(msg.clone());
            self.status = RecordingStatus::Error(msg);
            // Server rejected the saved key — surface the paste modal
            // immediately so the user can replace it without hunting
            // for a key binding.
            // Auth errors only happen when a recording was
            // streaming to the cloud. The default slot
            // config is the on-disk default — recorded
            // sections may have had a per-slot override,
            // but the auth failure happens during a
            // recording, so the user is at least
            // configured to WANT cloud. Open the modal.
            if auth_failure && self.default_slot_config.cloud_on {
                // `false`: the user already has Cloud ON and a key
                // on disk — they just need to replace the bad
                // key. Cancelling the modal leaves Cloud ON (the
                self.open_api_key_modal();
            }
        }
    }

    /// Toggle help display
    pub fn toggle_help(&mut self) {
        self.mode = if self.mode == AppMode::Help {
            AppMode::Normal
        } else {
            AppMode::Help
        };
    }

    /// Toggle the `t` status overlay (recent Agent events).
    pub fn toggle_status(&mut self) {
        self.mode = if self.mode == AppMode::Status {
            AppMode::Normal
        } else {
            AppMode::Status
        };
    }

    /// Resolve the effective settings for a given source. Prefers a
    /// Compute the effective settings for a section's source.
    /// Per-slot: the section's slot owns the customization;
    /// `slot.config` + `default_slot_config` is the single
    /// source of truth. (Called with the focused slot's source
    /// — see `start_section` / `resume_section`.)
    pub fn effective_settings_for(&self, source: &SessionSource) -> SectionSettings {
        let _ = source;
        let slot = self
            .slot_by_id(self.focused_slot)
            .expect("focused slot must exist");
        SectionSettings::effective(&slot.config, &self.default_slot_config)
    }
    /// Start recording a section in `slot` with explicit settings.
    /// Returns `Err(message)` for failures the UI should surface as a
    /// banner; `Ok(())` once the producer/consumer tasks are spawned.
    ///
    /// Pre-flight checks (missing API key) may also call
    /// `open_api_key_modal` — those return `Err` so the caller can
    /// avoid clobbering the modal with a banner.
    pub fn start_section(
        &mut self,
        slot: SlotId,
        source: SessionSource,
        mut settings: SectionSettings,
    ) -> Result<(), String> {
        clamp_section_settings_for_platform(&mut settings);

        // Look up the slot up front. An invalid id is the caller's
        // bug — usually a freed Phase B id — so we refuse cleanly.
        let pos = self
            .slot_index(slot)
            .ok_or_else(|| format!("invalid section slot: {slot}"))?;
        if matches!(self.slots[pos].kind, SlotKind::Recording { .. }) {
            return Err(format!("section slot {slot} already running"));
        }

        if settings.cloud_on && self.config.voicebird_api_key.is_empty() {
            self.banner = Some("Cloud is on but no API key — press 'c' to paste one".into());
            self.status = RecordingStatus::Error("no api key".into());
            // `false`: cloud was ON before this guard fired; the
            // user's intent (Cloud ON) is unchanged. We just need
            // a key to start. Esc closes the modal and the banner
            // stays as "missing api key" — the user can press 'c'
            // to re-open or toggle cloud OFF.
            self.open_api_key_modal();
            return Err("missing api key".into());
        }

        // Local mode is locked to English. The cloud-mode language selector
        // can leave settings.language set to something else (e.g. user
        // toggled cloud off after picking "ru"); shadow the value here so
        // the local engine receives "en".
        let effective_language = if settings.cloud_on {
            settings.language.clone()
        } else {
            "en".to_string()
        };

        // Where this section is heading. The Agents picker
        // writes a per-slot override when the user picks a
        // row; that overrides the cloud_on heuristic so the
        // agent target works alongside the existing Cloud /
        // Stdout switch. The override is consumed at start so
        // it only affects this one start.
        // The target is whatever the user (or a prior
        // pending override) picked. There's no implicit
        // "Cloud" fallback anymore — cloud is a
        // per-section transport flag (settings.cloud_on),
        // not a target. The default for a fresh slot
        // with no override is Stdout.
        let target = self
            .pending_target_overrides
            .remove(&slot)
            .unwrap_or(Target::Stdout);


        let now = chrono::Utc::now();
        // Local-first persistence is a function of the *target*, not
        // the cloud transport. `Target::Stdout` always lands on disk
        // (`audio.wav`, `transcript.jsonl`, `meta.json`, plus the
        // post-stop `transcript.json` / `transcript.txt`); that
        // contract holds whether the ASR is local-Whisper or
        // cloud-Voice-Bird-Web, because the cloud engine's committed
        // segments flow into the same consumer task and the same
        // Per-slot path: the focused slot's customized
        // output path, or the default if no customization.
        let path_str = voice_bird_cli::config::AppConfig::expand_tilde(
            &self
                .slot_by_id(slot)
                .and_then(|s| s.config.path.clone())
                .unwrap_or_else(|| self.default_slot_config.path.clone()),
        );
        let session_dir: Option<std::path::PathBuf> =
            if matches!(target, Target::Stdout) {
                let dir = voice_bird_cli::session::layout::session_dir(
                    std::path::Path::new(&path_str),
                    now,
                    &source,
                );
                if let Err(e) = std::fs::create_dir_all(&dir) {
                    return Err(format!("create session dir: {e}"));
                }
                Some(dir)
            } else {
                None
            };
        // Per-section live state. Reattach preserved transcript if the
        // slot had a Saved variant; otherwise start fresh.
        let (committed, refined) = match &self.slots[pos].kind {
            SlotKind::Saved { saved } => (saved.committed.clone(), saved.refined.clone()),
            _ => (
                Arc::new(PlMutex::new(Vec::new())),
                Arc::new(PlMutex::new(Vec::new())),
            ),
        };
        let tentative: Arc<PlMutex<String>> = Arc::new(PlMutex::new(String::new()));
        let engine_error_channel: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

        // Reset focused-section transcript scroll when the focused slot starts.
        if slot == self.focused_slot {
            self.transcript_scroll = 0;
            self.transcript_follow = true;
        }

        // --- 1. Capture ----------------------------------------------------
        // Branch on the SessionSource variant: Microphone → cpal mic
        // capture, System → platform-specific output-device loopback,
        // App → per-app loopback (device-agnostic on both macOS and
        // Windows; the paired device is informational + lives in the
        // session slug / persistence key).
        let device_for_log = self.config.input_device.as_deref();
        log::info!(
            "start_section[{slot}]: source = {:?}, device = {:?}",
            source,
            device_for_log
        );

        let capture_result = match &source {
            SessionSource::Microphone => {
                voice_bird_cli::audio::capture::capture_input(self.config.input_device.as_deref())
            }
            SessionSource::System => voice_bird_cli::audio::loopback::capture_loopback(
                self.config.input_device.as_deref(),
            ),
            SessionSource::App { id, .. } => voice_bird_cli::audio::loopback::capture_app(id),
        };
        let capture = match capture_result {
            Ok(c) => c,
            Err(e)
                if matches!(source, SessionSource::Microphone)
                    && self.config.input_device.is_some() =>
            {
                let want = self.config.input_device.as_deref().unwrap_or("");
                log::warn!("selected input device unavailable ({e}); falling back to default");
                self.banner = Some(format!("input '{}' not found — using default", want));
                match voice_bird_cli::audio::capture::capture_input(None) {
                    Ok(c) => c,
                    Err(e2) => {
                        return Err(format!("capture: {e2}"));
                    }
                }
            }
            Err(e) => {
                return Err(format!("capture: {e}"));
            }
        };
        let (mut frames_rx, info, stream) = capture.split();

        // --- 2. Resampler (device-native → 16 kHz mono) --------------------
        let mut resampler = match voice_bird_cli::audio::resample::Resampler::new(
            info.sample_rate,
            info.channels,
        ) {
            Ok(r) => r,
            Err(e) => {
                return Err(format!("resample: {e}"));
            }
        };

        // --- 3. Engine (Voice Bird Web cloud, WhisperKit sidecar, or whisper-rs fallback) --------
        use voice_bird_cli::transcription::EngineConfig;
        let catalog = voice_bird_cli::transcription::models::Catalog::builtin();
        if catalog.get(&settings.model).is_none() {
            let msg = format!(
                "Model '{}' is not supported by this release; pick one from the model picker.",
                settings.model
            );
            self.banner = Some(msg.clone());
            return Err(msg);
        }
        let sidecar = voice_bird_cli::transcription::sidecar_path();
        let prefer = self.config.engine_prefer.clone();
        let api_key = self.config.voicebird_api_key.clone();
        let server_url = self.config.voicebird_server_url.clone();
        // Only the (not(windows)) Nemotron swap below mutates these.
        #[cfg_attr(windows, allow(unused_mut))]
        let (mut engine_kind_used, mut engine) =
            match voice_bird_cli::transcription::try_select_engine(
                &prefer,
                settings.cloud_on,
                &api_key,
                &server_url,
                sidecar.as_deref(),
            ) {
                Ok(pair) => pair,
                Err(msg) => {
                    self.banner = Some(msg.clone());
                    return Err("engine selection failed".into());
                }
            };
        let cloud_active = matches!(
            engine_kind_used,
            voice_bird_cli::transcription::EngineKind::VoiceBirdWeb,
        );
        let cloud_reminder_until = if cloud_active {
            Some(std::time::Instant::now() + std::time::Duration::from_secs(3))
        } else {
            None
        };
        self.banner = None; // clear stale banner from previous run

        // English-only-model guard. Local Whisper engines respect whatever
        // language we pass via params.set_language, but English-only ggml
        // models (e.g. distil-small.en, base.en, tiny.en) will produce
        // gibberish if asked to transcribe Russian / Polish / etc.
        if !matches!(
            engine_kind_used,
            voice_bird_cli::transcription::EngineKind::VoiceBirdWeb
        ) {
            if let Err(msg) = voice_bird_cli::transcription::models::validate_local_language(
                &settings.model,
                &effective_language,
            ) {
                self.banner = Some(msg.clone());
                return Err(msg);
            }
        }

        let model_path = if matches!(
            engine_kind_used,
            voice_bird_cli::transcription::EngineKind::VoiceBirdWeb
        ) {
            std::path::PathBuf::new() // unused for cloud
        } else {
            match voice_bird_cli::transcription::models::model_path(&settings.model) {
                Ok(p) => p,
                Err(e) => {
                    return Err(format!("model path: {e}"));
                }
            }
        };
        // Local engine only — never reached on cloud-only Windows
        // (cloud_active is always true there).
        #[cfg(not(windows))]
        if !cloud_active
            && voice_bird_cli::transcription::models::is_nemotron_model(&settings.model)
        {
            engine_kind_used = voice_bird_cli::transcription::EngineKind::Nemotron;
            engine =
                Box::<voice_bird_cli::transcription::nemotron_engine::NemotronEngine>::default();
        }

        let engine_kind = match engine_kind_used {
            voice_bird_cli::transcription::EngineKind::WhisperRs => "whisper_rs".to_string(),
            voice_bird_cli::transcription::EngineKind::WhisperKit => "whisperkit".to_string(),
            voice_bird_cli::transcription::EngineKind::Nemotron => "nemotron".to_string(),
            voice_bird_cli::transcription::EngineKind::VoiceBirdWeb => "voicebird".to_string(),
        };

        let engine_cfg = if cloud_active {
            // Surface the actual (device, app) pair the user picked in the
            // init handshake so the Transcriptions tab on voicebird.app
            // can render one row per pair: app-loopback captures show as
            // e.g. "Chrome on EPOS PC 8 USB", mic / system captures fall
            // back to the device label alone.
            let (device_name, app_name) = match &source {
                SessionSource::Microphone | SessionSource::System => {
                    let dev = self
                        .config
                        .input_device
                        .clone()
                        .or_else(|| {
                            self.devices
                                .get(self.selected_device_index)
                                .map(|d| d.name.clone())
                        })
                        .unwrap_or_else(|| "voice-bird-desktop".into());
                    (dev, String::new())
                }
                SessionSource::App {
                    name, device_name, ..
                } => (device_name.clone(), name.clone()),
            };
            EngineConfig::Cloud {
                api_key: api_key.clone(),
                language: Some(effective_language.clone()).filter(|s| s != "auto"),
                sample_rate: 16_000,
                server_url: server_url.clone(),
                device_name,
                app_name,
            }
        } else {
            EngineConfig::Local {
                model_path,
                language: Some(effective_language.clone()).filter(|s| s != "auto"),
                sample_rate: 16_000,
                hop_ms: self.config.hop_ms,
                min_window_ms: self.config.min_window_ms,
            }
        };
        let handle = {
            let _enter = self.rt.enter();
            engine.start(engine_cfg)
        };
        let handle = match handle {
            Ok(h) => h,
            Err(e) => {
                return Err(format!("engine: {e}"));
            }
        };

        // --- 4. WAV writer on the resampled 16 kHz mono stream ------------
        // Skipped when broadcasting — local audio is not persisted.
        let mut wav: Option<hound::WavWriter<std::io::BufWriter<std::fs::File>>> =
            if let Some(dir) = session_dir.as_ref() {
                let spec = hound::WavSpec {
                    channels: 1,
                    sample_rate: 16_000,
                    bits_per_sample: 32,
                    sample_format: hound::SampleFormat::Float,
                };
                match hound::WavWriter::create(dir.join("audio.wav"), spec) {
                    Ok(w) => Some(w),
                    Err(e) => {
                        return Err(format!("wav: {e}"));
                    }
                }
            } else {
                None
            };

        let pcm_tx = handle.pcm_tx.clone();
        let mut events_rx = handle.events_rx;
        let committed_for_consumer = committed.clone();
        let tentative_for_consumer = tentative.clone();
        let engine_error_for_consumer = engine_error_channel.clone();
        // Skipped when broadcasting — no JSONL writer needed.
        let writer_path: Option<std::path::PathBuf> =
            session_dir.as_ref().map(|d| d.join("transcript.jsonl"));

        // Wall-clock anchor for the streaming consumer's elapsed timestamps.
        let session_start = std::time::Instant::now();

        // --- 4b. Optional refinement engine (beam-search on wider windows) -
        // Refinement is a local-whisper concept; on cloud-only Windows the
        // module doesn't exist, so the pair is statically (None, None).
        #[cfg(windows)]
        let (refinement_pcm_tx, refinement_handle): (
            Option<tokio::sync::mpsc::Sender<Vec<f32>>>,
            Option<voice_bird_cli::transcription::EngineHandle>,
        ) = (None, None);
        #[cfg(not(windows))]
        let (refinement_pcm_tx, refinement_handle) = if cloud_active {
            (None, None)
        } else {
            self.config
                .refinement_model
                .as_ref()
                .and_then(|id| {
                    let path = voice_bird_cli::transcription::models::model_path(id).ok()?;
                    if !voice_bird_cli::transcription::models::is_model_available(id) {
                        log::warn!(
                            "refinement_model '{}' set but not available at {} — disabled",
                            id,
                            path.display()
                        );
                        return None;
                    }
                    let eng = voice_bird_cli::transcription::refinement_engine::RefinementEngine {
                        model_path: path,
                        // Refinement only runs in local mode (the
                        // surrounding `if !cloud_active` block); local
                        // mode is locked to English.
                        language: Some("en".into()),
                        window_ms: self.config.refinement_window_ms,
                        beam_size: self.config.refinement_beam_size,
                    };
                    match eng.start() {
                        Ok(h) => Some((h.pcm_tx.clone(), h)),
                        Err(e) => {
                            log::error!("refinement engine start: {e}");
                            None
                        }
                    }
                })
                .map(|(tx, h)| (Some(tx), Some(h)))
                .unwrap_or((None, None))
        };

        // --- 5. Producer task: cpal frames → resample → tee(WAV + engines) -
        let refinement_pcm_tx_for_producer = refinement_pcm_tx.clone();
        let producer = self.rt.spawn(async move {
            while let Some(frames) = frames_rx.recv().await {
                match resampler.process(&frames) {
                    Ok(out) => {
                        if let Some(w) = wav.as_mut() {
                            for s in &out {
                                if let Err(e) = w.write_sample(*s) {
                                    log::error!("wav write: {e}");
                                    break;
                                }
                            }
                        }
                        if let Some(ref rtx) = refinement_pcm_tx_for_producer {
                            if rtx.send(out.clone()).await.is_err() {
                                log::warn!("refinement channel closed");
                            }
                        }
                        if pcm_tx.send(out).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        log::error!("resample: {e}");
                        break;
                    }
                }
            }
            if let Some(w) = wav.take() {
                if let Err(e) = w.finalize() {
                    log::error!("wav finalize: {e}");
                }
            }
        });

        // --- 6. Consumer task: engine events → committed lines ---
        let join = self.rt.spawn(async move {
            let mut writer = if let Some(p) = writer_path.as_ref() {
                match voice_bird_cli::session::writer::SegmentWriter::open(p) {
                    Ok(w) => Some(w),
                    Err(e) => {
                        log::error!("writer: {e}");
                        return;
                    }
                }
            } else {
                None
            };
            while let Ok(evt) = events_rx.recv().await {
                match evt {
                    voice_bird_cli::transcription::EngineEvent::ModelLoaded { name } => {
                        log::info!("engine loaded: {}", name);
                    }
                    voice_bird_cli::transcription::EngineEvent::Tentative(s) => {
                        *tentative_for_consumer.lock() = s;
                    }
                    voice_bird_cli::transcription::EngineEvent::Committed(seg) => {
                        if let Some(w) = writer.as_mut() {
                            let written = (&seg).into();
                            if let Err(e) = w.append(&written) {
                                log::error!("writer append: {e}");
                                break;
                            }
                        }
                        let elapsed_ms = session_start.elapsed().as_millis() as u64;
                        committed_for_consumer.lock().push(CommittedLine {
                            t_start_ms: elapsed_ms,
                            text: seg.text.clone(),
                        });
                        tentative_for_consumer.lock().clear();
                     }
                     voice_bird_cli::transcription::EngineEvent::Error(e) => {
                        log::error!("engine error: {e}");
                        if let Ok(mut slot) = engine_error_for_consumer.lock() {
                            *slot = Some(e);
                        }
                        break;
                    }
                }
            }
        });

        // --- 6b. Refinement consumer task (separate writer, separate JSONL) -
        let refinement_join = if let Some(h) = refinement_handle {
            let mut r_events_rx = h.events_rx;
            let refined_for_consumer = refined.clone();
            let committed_for_refinement = committed.clone();
            // Refinement only spawns when not broadcasting, so session_dir
            // is guaranteed Some here.
            let r_writer_path = session_dir
                .as_ref()
                .expect("session_dir present whenever refinement runs")
                .join("transcript.refined.jsonl");
            let r_join = self.rt.spawn(async move {
                let mut writer =
                    match voice_bird_cli::session::writer::SegmentWriter::open(&r_writer_path) {
                        Ok(w) => w,
                        Err(e) => {
                            log::error!("refinement writer: {e}");
                            return;
                        }
                    };
                while let Ok(evt) = r_events_rx.recv().await {
                    match evt {
                        voice_bird_cli::transcription::EngineEvent::ModelLoaded { name } => {
                            log::info!("refinement loaded: {}", name);
                        }
                        voice_bird_cli::transcription::EngineEvent::Committed(seg) => {
                            let written = (&seg).into();
                            if let Err(e) = writer.append(&written) {
                                log::error!("refined append: {e}");
                                break;
                            }
                            refined_for_consumer.lock().push(CommittedLine {
                                t_start_ms: seg.t_start.as_millis() as u64,
                                text: seg.text,
                            });
                            committed_for_refinement.lock().clear();
                        }
                        voice_bird_cli::transcription::EngineEvent::Tentative(_) => {}
                        voice_bird_cli::transcription::EngineEvent::Error(e) => {
                            log::error!("refinement error: {e}");
                        }
                    }
                }
            });
            // Drop the refinement engine's internal shutdown sender — the
            // engine thread exits naturally when its pcm channel closes.
            drop(h.shutdown);
            Some(r_join)
        } else {
            None
        };

        // Drop the producer-side clone: the producer task owns its own
        // copy. Keeping a second sender alive would prevent the refinement
        // engine from observing channel-close on stop.
        drop(refinement_pcm_tx);

        let section = Section {
            source,
            settings,
            runtime: RecordingRuntime {
                join,
                producer: Some(producer),
                refinement_join,
            },
            _capture_stream: stream,
            role: self.slots[pos].role.clone(),
            committed,
            refined,
            tentative,
            session_dir,
            session_started_at: now,
            engine_kind,
            cloud_active,
            engine_error_channel,
            cloud_reminder_until,
            transcript_scroll: 0,
            transcript_follow: true,
            target,
        };
        // Park the running section in the slot. `pos` was validated at
        // the top of the function, so a successful match here is the
        // only way the Recording variant can land.
        if let Some(slot_ref) = self.slots.get_mut(pos) {
            slot_ref.kind = SlotKind::Recording { section };
        }
        let now_inst = std::time::Instant::now();
        self.start_time = Some(match self.start_time {
            Some(prev) if prev < now_inst => prev,
            _ => now_inst,
        });
        Ok(())
    }

    /// Stop the active recording in `slot` and finalize its session files.
    /// No-op if the slot is empty.
    pub fn stop_section(&mut self, slot: SlotId) {
        log::info!("stop_section[{slot}]: entered");
        let pos = match self.slot_index(slot) {
            Some(p) => p,
            None => {
                log::warn!("stop_section[{slot}]: invalid slot, refusing");
                return;
            }
        };

        // Take the section out so we can finalize its session files.
        // We replace the slot with a Saved variant carrying the same
        // transcripts so the UI keeps showing the text after stop.
        let section = match std::mem::replace(&mut self.slots[pos].kind, SlotKind::Empty) {
            SlotKind::Recording { section } => section,
            other => {
                // Empty or already Saved: nothing to stop, put the
                // slot back exactly as it was.
                self.slots[pos].kind = other;
                log::info!("stop_section[{slot}]: slot was empty (no-op)");
                return;
            }
        };
        let label = section_column_label(slot, Some(&section));
        let target = section.target;
        let saved = SavedTranscript {
            committed: section.committed.clone(),
            refined: section.refined.clone(),
            label,
            target,
            // Snapshot the source + settings so App::resume_section
            // can re-enter start_section with the same input
            // pipeline (mic/system/app, cloud/language/model)
            // instead of asking the user to re-pick.
            source: section.source.clone(),
            settings: section.settings.clone(),
            session_started_at: chrono::Utc::now(),
            role: None,
        };
        self.slots[pos].kind = SlotKind::Saved { saved };

        log::info!(
            "stop_section[{slot}]: stopping source = {:?}",
            section.source
        );

        // Drop the cpal stream first — this halts capture, the cpal callback
        // thread exits, and the `frames_rx` receiver inside the producer
        // task observes `None` on its next `recv()` (clean shutdown).
        drop(section._capture_stream);

        let mut runtime = section.runtime;
        if let Some(producer) = runtime.producer.take() {
            producer.abort();
        }
        let refinement_join = runtime.refinement_join.take();
        let _ = self.rt.block_on(async move {
            let _ = runtime.join.await;
            if let Some(rj) = refinement_join {
                let _ = rj.await;
            }
        });

        if let Some(dir) = section.session_dir {
            let started = section.session_started_at;
            let ended = chrono::Utc::now();
            let engine_for_meta = if section.engine_kind.is_empty() {
                "whisper_rs".to_string()
            } else {
                section.engine_kind.clone()
            };
            let meta = voice_bird_cli::session::finalize::SessionMeta {
                version: env!("CARGO_PKG_VERSION").into(),
                model: section.settings.model.clone(),
                engine: engine_for_meta,
                source: source_to_string(&section.source),
                device: device_name_for_source(&section.source),
                started_at: started.to_rfc3339(),
                ended_at: ended.to_rfc3339(),
                duration_ms: (ended - started).num_milliseconds().max(0) as u64,
                role: section.role.as_ref().map(|r| r.name.clone()),
                room_slug: self
                    .active_room()
                    .slug
                    .clone()
                    .into_option_if_not_free(),
            };
            if let Err(e) = voice_bird_cli::session::finalize::finalize(
                &dir.join("transcript.jsonl"),
                &dir.join("transcript.json"),
                &dir.join("transcript.txt"),
                &dir.join("meta.json"),
                &meta,
            ) {
                log::error!("finalize: {e}");
            }
        }

        // Rewrite the room-level transcript on every stop. The
        // merged timeline at this moment covers every running
        // + saved role-bound section, so the file always
        // reflects the most recent view of the conversation.
        if let Some(room_dir) = self.room_session_dir.as_ref() {
            let entries: Vec<(chrono::DateTime<chrono::Utc>, Option<String>, String)> =
                self.merged_timeline()
                    .into_iter()
                    .map(|e| (e.at, e.role, e.text))
                    .collect();
            if let Err(e) =
                voice_bird_cli::room_fs::write_room_transcript_jsonl(
                    room_dir, &entries,
                )
            {
                log::error!("room transcript write: {e}");
            }
            // D3.4.d: rewrite context.md on every stop. D4 will
            // populate the body with the last completed agent
            // run output; for now we write a placeholder so the
            // file always exists when an agent room is active.
            let placeholder = String::from(
                "_No agent run has completed yet for this room._\n",
            );
            if let Err(e) = voice_bird_cli::room_fs::write_room_context_md(
                room_dir,
                &placeholder,
                chrono::Utc::now(),
            ) {
                log::error!("room context.md write: {e}");
            }
        }
        if self.active_section_count() == 0 {
            self.status = RecordingStatus::Idle;
            self.start_time = None;
            self.duration = 0.0;
        }

        if let Ok(mut level) = self.audio_level.lock() {
            *level = 0.0;
        }
    }

    /// Clear the transcript for the given slot — both any live section's
    /// committed/refined data and the saved (preserved) transcript.
    pub fn clear_slot_transcript(&mut self, slot: SlotId) {
        let Some(pos) = self.slot_index(slot) else {
            return;
        };
        match &mut self.slots[pos].kind {
            SlotKind::Recording { section } => {
                section.committed.lock().clear();
                section.refined.lock().clear();
                section.tentative.lock().clear();
            }
            SlotKind::Saved { .. } => {
                self.slots[pos].kind = SlotKind::Empty;
            }
            SlotKind::Empty => {}
        }
    }

    /// Resume capture in `slot` from a previously stopped
    /// (Saved) section. Re-derives the source, target, and
    /// settings from the current picker state instead of
    /// reading them off the saved snapshot, so any changes
    /// the user made after pressing `s` (device, app,
    /// target, language, cloud toggle) take effect on the
    /// resumed section.
    ///
    /// - Source comes from `App::resolve_focused_source`,
    ///   which mirrors the Devices+Apps picker resolution
    ///   in `main.rs`'s Enter handler.
    /// - Target comes from `focused_pending_target`, which
    ///   reads the per-slot `pending_target_overrides`
    ///   entry if the user picked a new target, falling
    ///   back to the saved target.
    /// - Settings come from `effective_settings_for(source)`,
    ///   which reads the per-source override first and
    ///   falls back to the global config.
    ///
    /// The resolved source, target, and settings are
    /// persisted back to the saved snapshot so the slot
    /// reflects what was used regardless of whether
    /// `start_section` succeeds. The committed/refined
    /// Arcs on the saved variant are kept intact:
    /// `start_section` reads them off the slot's `Saved`
    /// arm and reuses them, so the visible transcript
    /// text survives the resume.
    ///
    /// Returns `Err` with a banner-ready message when the
    /// slot is `Empty` (nothing to resume), already
    /// `Recording` (no double-start), or when the current
    /// picker state is invalid (no device, mic + app,
    /// unexpected device kind). The caller in
    /// `handle_normal_mode` surfaces the message verbatim.
    pub fn resume_section(&mut self, slot: SlotId) -> Result<(), String> {
        let pos = self
            .slot_index(slot)
            .ok_or_else(|| format!("invalid section slot: {slot}"))?;
        // Refuse double-start: the recording pipeline (cpal
        // stream + tokio tasks) is already running and
        // re-entering start_section would race the live
        // audio capture. The caller surfaces this as a
        // banner so the key is never silently dead.
        if matches!(self.slots[pos].kind, SlotKind::Recording { .. }) {
            return Err(format!("slot {slot} is already recording"));
        }
        // Empty means nothing to resume. Surface that specific
        // message rather than the more generic "no device
        // selected" error the picker resolution below would
        // produce for an untouched slot.
        if matches!(self.slots[pos].kind, SlotKind::Empty) {
            return Err("nothing to resume — slot is empty".into());
        }
        // Re-derive source, target, and settings from the
        // current picker state. The saved snapshot's
        // *committed* and *refined* Arcs are kept intact:
        // start_section reads them off the slot's `Saved`
        // variant to re-attach them to the new Section, so
        // the visible text survives the resume. Only
        // source, target, and settings are rewritten to
        // the live values.
        let source = self.resolve_focused_source()?;
        let target = self.focused_pending_target();
        let settings = self.effective_settings_for(&source);
        // Slot is Saved here (Empty and Recording both
        // returned above). Persist the new source, target,
        // and settings back to the saved snapshot so the
        // slot reflects what was used regardless of
        // whether start_section succeeds.
        if let SlotKind::Saved { saved } = &mut self.slots[pos].kind {
            saved.source = source.clone();
            saved.target = target.clone();
            saved.settings = settings.clone();
        } else {
            // Defensive: the guards above make this branch
            // unreachable today. Surface an error instead of
            // panicking so a future refactor that changes
            // slot state mid-resume degrades to a banner,
            // not a dead TUI.
            return Err(format!("slot {slot} changed state during resume"));
        }
        // Queue the target so start_section consumes it
        // (start_section removes the override at start
        // time and uses it as the section's target). Only
        // insert when the user hadn't already queued an
        // override, and take the insert back if
        // start_section fails on an early guard (e.g.
        // missing API key) before consuming it — otherwise
        // a failed resume would leave a stale override
        // queued on the slot.
        let had_override = self.pending_target_overrides.contains_key(&slot);
        if !had_override {
            self.pending_target_overrides.insert(slot, target);
        }
        let result = self.start_section(slot, source, settings);
        if result.is_err() && !had_override {
            self.pending_target_overrides.remove(&slot);
        }
        result
    }

    /// Resolve the source the picker would pick on Enter.
    /// `None` if no device is selected or the device kind
    /// is the rejected `AudioSessionKind::App` variant.
    /// Pure read of picker state — no mutation, no
    /// persistence. Used by `try_start_new_section` and by
    /// the idle `c` toggle in `main.rs`, so the toggle
    /// writes its per-source override for the same source
    /// the next start will resolve to.
    pub fn resolve_picker_source(&self) -> Option<SessionSource> {
        use voice_bird_cli::config::AudioSessionKind;
        let dev = self.devices.get(self.selected_device_index)?;
        let app_pick = self.focused_app().cloned();
        match (dev.kind, app_pick) {
            (AudioSessionKind::Input, _) => Some(SessionSource::Microphone),
            (AudioSessionKind::Output, None) => Some(SessionSource::System),
            (AudioSessionKind::Output, Some(a)) => Some(SessionSource::App {
                id: a.id,
                name: a.name,
                device_name: dev.name.clone(),
            }),
            (AudioSessionKind::App, _) => None,
        }
    }

    /// Try to start a brand-new recording in the next free
    /// slot. Extracted from `main.rs`'s `Enter` handler so
    /// the workflow is unit-testable and the message
    /// contract is owned by `App` (not free-form `String`s
    /// scattered in the key dispatcher).
    ///
    /// Flow: pick the first Empty slot, resolve the source
    /// from the current Devices + Apps picker, persist the
    /// picks to config (so a refresh preserves them), then
    /// hand off to `start_section`. Returns the same `Err`
    /// messages as before for invalid picker state, so the
    /// caller in `main.rs` can surface them as banners
    /// unchanged.
    ///
    /// The Agents picker has a special-case in the `Enter`
    /// handler that *applies* the picked target first; that
    /// lives in `main.rs` (it's a UI concern) and is called
    /// *before* this method. By the time we get here, the
    /// picked target (if any) is already queued in
    /// `pending_target_overrides` and `start_section` will
    /// consume it.
    pub fn try_start_new_section(&mut self) -> Result<(), String> {
        // No Empty slot. Distinguish between the two cases:
        //   - Some slot is actively Recording — the user
        //     needs to [s]top one to free a slot.
        //   - All non-Empty slots are Saved (paused) —
        //     [R] resumes a paused slot in place, [x]
        //     clears, [-] removes. The old hardcoded
        //     "all 3 sections are recording" message
        //     was factually wrong in the second case and
        //     hid the new R key.
        let Some(slot) = self.next_free_slot() else {
            let total = self.slots.len();
            let recording = self.active_section_count();
            log::info!("keys: Enter → refused (no free slot, {recording} recording, {total} total)");
            let msg = if recording > 0 {
                format!("All {total} slots are full — stop the recording with [s] first")
            } else {
                "No empty slots — press [R] to resume a paused slot, [x] to clear, [-] to remove"
                    .to_string()
            };
            return Err(msg);
        };
        // Resolve the source through the SAME function
        // (`resolve_focused_source`) that the resume path
        // uses — so the Enter and R flows share one
        // picker→source match and one catalog of error
        // strings. Pre-refactor, the three error paths
        // (no-device / mic+app / unexpected-kind) plus
        // the inline `resolve_picker_source` call were
        // re-coded here, each able to drift independently
        // of `resolve_focused_source`.
        let source = self.resolve_focused_source()?;
        // `resolve_focused_source` returned Ok, so the
        // focused device is real — fetch a clone for the
        // persist step below.
        let dev = self
            .focused_device()
            .cloned()
            .expect("resolve_focused_source returned Ok");
        let app_pick = self.focused_app().cloned();
        log::info!(
            "keys: Enter → slot={} focused_slot_before={} dev=({:?}, {:?}) app={:?}",
            slot,
            self.focused_slot,
            dev.name,
            dev.kind,
            app_pick.as_ref().map(|a| (a.name.clone(), a.id.clone())),
        );

        // Persist the selected device + last app id.
        let name_changed = self.config.input_device.as_deref() != Some(dev.name.as_str());
        let kind_changed = self.config.input_device_kind != Some(dev.kind);
        let app_id_changed =
            self.config.last_app_id.as_deref() != app_pick.as_ref().map(|a| a.id.as_str());
        if name_changed || kind_changed || app_id_changed {
            self.config.input_device = Some(dev.name.clone());
            self.config.input_device_kind = Some(dev.kind);
            self.config.last_app_id = app_pick.as_ref().map(|a| a.id.clone());
            if let Err(e) = self.config.save() {
                log::error!("config save: {e}");
            }
        }

        // Start in the chosen slot; route through the
        // per-section API so settings come from
        // `effective_settings_for(source)`.
        let settings = self.effective_settings_for(&source);
        self.focused_slot = slot;
        log::info!(
            "keys: Enter → resolved source={:?}; calling start_section[{}]",
            source,
            slot
        );
        self.start_section(slot, source, settings)
    }

    /// Stop every active section. Used at quit.
    pub fn stop_all_sections(&mut self) {
        // Collect ids first so we don't hold a borrow on `self.slots`
        // while mutating each one through `stop_section`.
        let recording_ids: Vec<SlotId> = self
            .slots
            .iter()
            .filter(|s| matches!(s.kind, SlotKind::Recording { .. }))
            .map(|s| s.id)
            .collect();
        for slot in recording_ids {
            self.stop_section(slot);
        }
    }

    /// Kick off an async download of `entry`'s gguf model file. Progress
    /// is written into the picker's `downloading` slot; on success, the
    /// config is written with the chosen model id and the app transitions
    /// back to `AppMode::Normal`.
    pub fn begin_model_download(
        &mut self,
        entry: &voice_bird_cli::transcription::models::ModelEntry,
    ) {
        let _dest = match voice_bird_cli::transcription::models::model_path(entry.id) {
            Ok(p) => p,
            Err(e) => {
                log::error!("model_path: {e}");
                if let Some(picker) = self.picker.as_mut() {
                    picker.downloading = Some(Arc::new(PlMutex::new(DownloadProgress {
                        model_id: entry.id.into(),
                        bytes: 0,
                        total: None,
                        error: Some(format!("{e}")),
                    })));
                }
                return;
            }
        };

        let progress = Arc::new(PlMutex::new(DownloadProgress {
            model_id: entry.id.into(),
            bytes: 0,
            total: None,
            error: None,
        }));

        if let Some(picker) = self.picker.as_mut() {
            picker.downloading = Some(progress.clone());
        }

        let entry = entry.clone();
        let progress_for_thread = progress.clone();

        std::thread::spawn(move || {
            let mut cb = |bytes: u64, total: Option<u64>| {
                let mut g = progress_for_thread.lock();
                g.bytes = bytes;
                g.total = total;
            };
            let res =
                voice_bird_cli::transcription::models::download_model_with_verify(&entry, &mut cb);
            if let Err(e) = res {
                let mut g = progress_for_thread.lock();
                g.error = Some(format!("{e}"));
            } else {
                let mut g = progress_for_thread.lock();
                g.total = Some(g.bytes);
            }
        });
    }

    /// If a picker download has finished (successfully), commit the chosen
    /// model id to config and return to Normal mode. Intended to be called
    /// from the render loop once per tick.
    pub fn poll_picker_download(&mut self) {
        // Clone the Arc up front so we don't hold a &self borrow on
        // `self.picker` across the &mut self section below.
        let progress_arc = match self.picker.as_ref().and_then(|p| p.downloading.clone()) {
            Some(a) => a,
            None => return,
        };

        // Snapshot under the lock, release before mutating self.
        let (done, err, model_id) = {
            let g = progress_arc.lock();
            let done = g.error.is_none() && g.total.is_some() && Some(g.bytes) == g.total;
            (done, g.error.clone(), g.model_id.clone())
        };

        if err.is_some() {
            return;
        }

        if done {
            // Mirror the picked model into the focused slot's
            // customization so the next start sees the same
            // value. The slot's `config` is the single source
            // of truth for what the slot will use.
            if let Some(slot) = self
                .slots
                .iter_mut()
                .find(|s| s.id == self.focused_slot)
            {
                slot.config.model = Some(model_id.clone());
            }
            if self.focused().is_some() {
                if let Some(section) = self.focused_mut() {
                    section.settings.model = model_id;
                }
            }
            self.mode = AppMode::Normal;
            self.picker = None;
            self.config_was_loaded_from_disk = true;
        }
    }


}

/// Build the column-title label for a section (or "(empty)" / "(paused)"
/// placeholders). Mirrors `section_column_title` in ui.rs so the saved-
/// transcript path can reconstruct the label without depending on the ui
/// module.
pub fn section_column_label(slot: SlotId, section: Option<&Section>) -> String {
    let n = slot.0;
    match section {
        None => format!(" [{n}] (empty) "),
        Some(s) => {
            let label = match &s.source {
                SessionSource::Microphone => "mic",
                SessionSource::System => "system",
                SessionSource::App {
                    name, device_name, ..
                } => {
                    if device_name.is_empty() {
                        name.as_str()
                    } else {
                        // Leak a short-lived String is fine here — this is
                        // called once per stop for display labels.
                        return format!(
                            " [{n}] {name} on {device_name} · cloud:{cloud} · {lang} · {model} ",
                            cloud = if s.settings.cloud_on { "ON" } else { "OFF" },
                            lang = if s.settings.cloud_on {
                                s.settings.language.as_str()
                            } else {
                                "en"
                            },
                            model = s.settings.model,
                        );
                    }
                }
            };
            format!(
                " [{n}] {label} · cloud:{cloud} · {lang} · {model} ",
                cloud = if s.settings.cloud_on { "ON" } else { "OFF" },
                lang = if s.settings.cloud_on {
                    s.settings.language.as_str()
                } else {
                    "en"
                },
                model = s.settings.model,
            )
        }
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

// ── Platform invariants ────────────────────────────────────────────────


/// Windows is cloud-only: force cloud on in memory regardless of what the
/// config says (covers configs copied from another OS or hand-edited). The
/// on-disk format stays identical across platforms. `cfg!` (rather than an
/// attribute) keeps the body compiled and testable on every target.
fn enforce_cloud_only_platform(config: &mut AppConfig) {
    if cfg!(windows) {
        config.default_slot_config.cloud_on = true;
    }
}

/// Windows is cloud-only: clamp the per-source setting at the one choke
/// point every recording passes through (`start_section`). Covers stale
/// cloud_on=false overrides persisted by a pre-0.4.0 config.
fn clamp_section_settings_for_platform(settings: &mut SectionSettings) {
    if cfg!(windows) {
        settings.cloud_on = true;
    }
}

// ── Export helpers ─────────────────────────────────────────────────────

/// Find the most recent session directory that has a `transcript.json`.
/// Skips directories that don't contain transcript.json but does NOT
/// filter by `.uploaded` — the caller decides what to do with that.
/// Returns `None` if no sessions exist.
#[cfg(not(windows))]
fn find_latest_session(base: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut dirs: Vec<std::path::PathBuf> = match std::fs::read_dir(base) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .map(|e| e.path())
            .collect(),
        Err(_) => return None,
    };

    // Sort by name descending (timestamps are ISO 8601 formatted, so
    // lexicographic sort = chronological sort).
    dirs.sort_by(|a, b| b.file_name().cmp(&a.file_name()));

    for d in dirs {
        if d.join("transcript.json").exists() {
            return Some(d);
        }
    }
    None
}

 /// Render the `source` field of `SessionMeta` for the active
/// `SessionSource`. Replaces the legacy `source: "mic"` hardcode
/// — the meta file now reflects what the user actually recorded
/// from.
fn source_to_string(source: &voice_bird_cli::session::layout::SessionSource) -> String {
    use voice_bird_cli::session::layout::SessionSource;
    match source {
        SessionSource::Microphone => "mic".into(),
        SessionSource::System => "system".into(),
        SessionSource::App { name, .. } => name.clone(),
    }
}

/// Render the `device` field of `SessionMeta` for the active
/// `SessionSource`. The legacy `device: "mock"` hardcode is
/// gone — for mic/system we report the focused device from the
/// picker; for per-app capture we report the app's display name.
fn device_name_for_source(
    source: &voice_bird_cli::session::layout::SessionSource,
) -> String {
    use voice_bird_cli::session::layout::SessionSource;
    match source {
        SessionSource::Microphone | SessionSource::System => String::new(),
        SessionSource::App { name, .. } => name.clone(),
    }
}

/// Helper for the meta-write site: turn a `String` into
/// `Option<String>` only when it's neither the Free Room slug
/// nor empty. Keeps the meta file lean — Free Room sessions
/// don't carry a `room_slug` field, agent rooms do.
trait IntoOptionIfNotFree {
    fn into_option_if_not_free(self) -> Option<String>;
}
impl IntoOptionIfNotFree for String {
    fn into_option_if_not_free(self) -> Option<String> {
        if self.is_empty() || self == "free" {
            None
        } else {
            Some(self)
        }
    }
}


/// Derive an HTTP base URL from the WebSocket server URL.
/// E.g., `wss://voicebird.app/api/audio/stream` → `https://voicebird.app`
/// `ws://localhost:3000/api/audio/stream`     → `http://localhost:3000`
#[cfg(not(windows))]
fn ws_url_to_http(ws_url: &str) -> String {
    let (scheme, rest) = if let Some(r) = ws_url.strip_prefix("wss://") {
        ("https", r)
    } else if let Some(r) = ws_url.strip_prefix("ws://") {
        ("http", r)
    } else {
        ("https", ws_url)
    };

    // Take only the host[:port] part — drop any path
    let host = rest.split('/').next().unwrap_or(rest);

    format!("{scheme}://{host}")
}

/// Heuristic match against engine error messages for "the server rejected
/// our credentials." Voice Bird Web returns plain-English errors over the
/// init handshake (e.g. "InitSuccess: false — invalid api key"), so a
/// substring scan is good enough — false positives just re-prompt for a
/// key the user can immediately re-confirm by pressing Enter.
fn looks_like_auth_error(msg: &str) -> bool {
    let m = msg.to_lowercase();
    m.contains("unauthor")
        || m.contains("api key")
        || m.contains("api_key")
        || m.contains("invalid key")
        || m.contains("forbidden")
        || m.contains("initsuccess: false")
}

/// Map a start-error string from `start()` to a UI status
/// string. The string the worker surfaces is a free-form
/// `anyhow` message ("Transcript too long", "API key
/// rejected — check Settings", "Agent runs require Pro",
/// or "agent run returned <code>"). The UI cares about
/// the coarse bucket; we substring-match against the
/// known error strings.
fn classify_start_error(msg: &str) -> String {
    let m = msg.to_lowercase();
    if m.contains("api key") {
        "needs_api_key".into()
    } else if m.contains("pro") {
        "needs_pro".into()
    } else if m.contains("too long") {
        "transcript_too_long".into()
    } else if m.contains("rate") || m.contains("429") {
        "rate_limited".into()
    } else {
        "failed".into()
    }
}

#[cfg(test)]
mod classify_start_error_tests {
    use super::classify_start_error;

    #[test]
    fn api_key_string_maps_to_needs_api_key() {
        assert_eq!(
            classify_start_error("API key rejected — check Settings"),
            "needs_api_key"
        );
    }

    #[test]
    fn pro_string_maps_to_needs_pro() {
        assert_eq!(
            classify_start_error("Agent runs require Pro"),
            "needs_pro"
        );
    }

    #[test]
    fn too_long_string_maps_to_transcript_too_long() {
        assert_eq!(
            classify_start_error("Transcript too long"),
            "transcript_too_long"
        );
    }

    #[test]
    fn unknown_string_maps_to_failed() {
        assert_eq!(
            classify_start_error("agent run returned 500"),
            "failed"
        );
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    // `App::new()` itself derefs `voice_bird_cli::test_utils::INSTALL_TEST_CONFIG`
    // on its first call (the only way to make the test tempdir
    // appear before any test in this module's `App::new()`).
    // No per-module `_TEST_CONFIG` is needed — the `LazyLock`
    // is touched by every test that constructs an `App`.

    #[test]
    fn auth_error_detection_matches_common_phrases() {
        assert!(looks_like_auth_error("Unauthorized"));
        assert!(looks_like_auth_error("invalid API key"));
        assert!(looks_like_auth_error("InitSuccess: false — bad key"));
        assert!(looks_like_auth_error("forbidden"));
        assert!(!looks_like_auth_error("connection reset by peer"));
        assert!(!looks_like_auth_error("audio format unsupported"));
    }
    /// The Agents pane's `pick_target` writes the focused slot's
    /// pending target, which `start_section` consumes on the next
    /// start. Each call returns the resolved `Target` (with a
    /// second Agent target. ↑/↓ must walk onto the saved row
    /// instead of leaving it stranded behind a hardcoded cap.
    /// Regression for the screenshot where `Agent: prod-events`
    /// rendered in the pane but the ▶ cursor couldn't reach it.
    #[test]
    fn fresh_app_has_no_active_sections() {
        let app = App::new();
        assert_eq!(app.active_section_count(), 0);
        assert!(app.focused().is_none());
        assert_eq!(app.focused_engine_kind(), "");
        assert!(!app.focused_cloud_active());
        // Empty fallbacks for the focused-* Arcs.
        assert!(app.focused_committed().lock().is_empty());
        assert!(app.focused_tentative().lock().is_empty());
    }

    // ── resume_section state matrix (R key) ──────────────────────────

    /// An `Empty` slot must surface a banner-ready `Err` so the
    /// `R` key is never silently dead. The message is what
    /// `handle_normal_mode` writes into the status banner.
    #[test]
    fn resume_section_on_empty_slot_returns_nothing_to_resume() {
        let mut app = App::new();
        let slot = app.slots[0].id;
        let err = app.resume_section(slot).unwrap_err();
        assert!(
            err.contains("nothing to resume"),
            "expected a 'nothing to resume' message; got: {err}"
        );
        // The slot stays Empty — no side effects on failure.
        assert!(matches!(app.slots[0].kind, SlotKind::Empty));
    }

    /// A `Saved` slot must reach `start_section`. The point of
    /// the test is to prove the resume path *enters*
    /// start_section with the saved source — not that the
    /// pipeline spins up in CI. The test is pipeline-aware:
    /// on hosts where cpal finds a real device the slot may
    /// transition to `Recording`; on hosts where it does
    /// not, the slot stays `Saved` and `start_section`
    /// returns `Err` from the capture step. Both outcomes
    /// are acceptable — what matters is that resume did
    /// not short-circuit on the resume-time guards.
    #[test]
    fn resume_section_on_saved_slot_delegates_to_start_section() {
        let mut app = App::new();
        // Seed a Saved variant carrying the metadata we want
        // resume to feed into start_section. The committed
        // arc holds one synthetic line so we can also assert
        // it's preserved when start_section reattaches it.
        let committed: Arc<PlMutex<Vec<CommittedLine>>> =
            Arc::new(PlMutex::new(vec![CommittedLine {
                t_start_ms: 0,
                text: "preserved line".into(),
            }]));
        let refined: Arc<PlMutex<Vec<CommittedLine>>> =
            Arc::new(PlMutex::new(Vec::new()));
        let slot = app.slots[0].id;
        let saved = SavedTranscript {
            committed: committed.clone(),
            refined: refined.clone(),
            label: "mic · cloud:OFF".into(),
            target: Target::Stdout,
            source: SessionSource::Microphone,
            settings: SectionSettings {
                cloud_on: false,
                language: "en".into(),
                model: "tiny.en".into(),
            },
            session_started_at: chrono::Utc::now(),
            role: None,
        };
        app.slots[0].kind = SlotKind::Saved { saved };

        let result = app.resume_section(slot);
        if let Err(err) = &result {
            // On hosts where capture fails (no cpal device),
            // the error must NOT be the resume-time
            // short-circuits — those would mean resume never
            // reached the pipeline.
            assert!(
                !err.contains("nothing to resume"),
                "resume must delegate to start_section, not short-circuit: {err}"
            );
            assert!(
                !err.contains("already recording"),
                "resume must not be refused as a double-start from Saved: {err}"
            );
        }
        // The captured committed Arc is still alive (start_section
        // reattaches it before either failing on capture or
        // transitioning the slot to `Recording`) — proves the
        // visible text survives the resume attempt.
        assert_eq!(
            committed.lock().len(),
            1,
            "preserved line must remain in the saved committed arc after resume"
        );
    }

    /// The `Recording` short-circuit is a one-line `matches!`
    /// guard against re-entering the live pipeline. We can't
    /// construct a real `Section` in a unit test (it holds a
    /// `!Send` cpal stream), so this test only proves the
    /// state read on a fresh `Empty` slot is `Empty` — i.e.
    /// the guard does not trigger for the wrong reason on the
    /// happy path. The actual `Recording` arm is covered by
    /// the integration tests in `tests/`.
    #[test]
    fn resume_section_does_not_falsely_refuse_empty_as_recording() {
        let mut app = App::new();
        let slot = app.slots[0].id;
        // On an Empty slot, the guard's !Recording branch is
        // what we take. If the guard were inverted, this would
        // hit the "already recording" branch and fail the
        // assertion below.
        let err = app.resume_section(slot).unwrap_err();
        assert!(
            !err.contains("already recording"),
            "Empty slot must not be mis-classified as Recording: {err}"
        );
    }

    /// A `Saved` slot's resume must pick up settings the user
    /// changed *after* pressing `s`. The use case: the user
    /// records a section in English, stops, then cycles the
    /// cloud language to Russian and resumes — the next
    /// captured audio must be transcribed in Russian, not
    /// English.
    ///
    /// Currently red: `resume_section` extracts the saved
    /// snapshot's `settings` verbatim and hands them to
    /// `start_section`, so any post-stop change to the
    /// settings is ignored. The fix in the next commit switches to
    /// `App::effective_settings_for(source)`, which reads
    /// the live per-source override first and falls back
    /// to the global config, and persists the new
    /// settings back onto the saved snapshot so the
    /// resulting slot reflects what was used.
    ///
    /// The test is pipeline-aware: on hosts where cpal
    /// finds a real device the slot transitions to
    /// `Recording`; otherwise it stays `Saved`. The
    /// assertion reads the settings from whichever
    /// variant the slot ended up in.
    #[test]
    fn resume_section_picks_up_settings_changed_after_stop() {
        let mut app = App::new();
        // Populate the Devices picker so the
        // resolve_focused_source path in resume_section
        // has a device to read.
        use voice_bird_cli::config::AudioSessionKind;
        app.devices = vec![AudioDevice {
            name: "MacBook Pro Microphone".into(),
            kind: AudioSessionKind::Input,
        }];
        app.selected_device_index = 0;
        // Seed a Saved variant as if a section was
        // recorded in English, cloud off.
        let committed: Arc<PlMutex<Vec<CommittedLine>>> =
            Arc::new(PlMutex::new(Vec::new()));
        let refined: Arc<PlMutex<Vec<CommittedLine>>> =
            Arc::new(PlMutex::new(Vec::new()));
        let slot = app.slots[0].id;
        let saved = SavedTranscript {
            committed: committed.clone(),
            refined: refined.clone(),
            label: "mic · cloud:OFF".into(),
            target: Target::Stdout,
            source: SessionSource::Microphone,
            settings: SectionSettings {
                cloud_on: false,
                language: "en".into(),
                model: "tiny.en".into(),
            },
            session_started_at: chrono::Utc::now(),
            role: None,
        };
        app.slots[0].kind = SlotKind::Saved { saved };

        // Simulate the user pressing `l` to cycle the
        // language to Russian and `c` to flip cloud on
        // *after* stopping. The c/l handlers update the
        // focused slot's `slot.config` (the post-stop
        // state) — that's the single source of truth for
        // what the next start will see.
        let slot_obj = app
            .slots
            .iter_mut()
            .find(|s| s.id == slot)
            .expect("focused slot must exist");
        slot_obj.config.cloud_on = Some(true);
        slot_obj.config.language = Some("ru".into());
        // (capture may succeed or fail in the test env),
        // so we only assert on the slot's post-call
        // settings — not on Ok/Err.
        let _ = app.resume_section(slot);

        // The settings on the slot — whether it stayed
        // Saved (capture failed) or transitioned to
        // Recording (capture succeeded) — must reflect
        // the post-stop config change, not the original
        // saved snapshot.
        let applied: &SectionSettings = match &app.slots[0].kind {
            SlotKind::Saved { saved } => &saved.settings,
            SlotKind::Recording { section } => &section.settings,
            SlotKind::Empty => {
                panic!("resume must not empty the slot on a successful Saved path")
            }
        };
        assert_eq!(
            applied.language, "ru",
            "resume must apply the post-stop language change, \
             not the original saved snapshot's language (en)"
        );
        assert!(
            applied.cloud_on,
            "resume must apply the post-stop cloud toggle, \
             not the original saved snapshot's cloud_off"
        );
    }
    /// With two `Saved` slots, pressing `R` must resume the
    /// focused slot only. The non-focused slot stays
    /// `Saved` and is untouched.
    ///
    /// Currently green: `resume_section` takes a `SlotId`
    /// and operates only on that slot, so this is
    /// documenting existing behavior with a regression
    /// guard.
    #[test]
    fn resume_section_only_resumes_focused_slot_when_multiple_saved() {
        let mut app = App::new();
        // Add a second slot so we have two to seed.
        let slot_b = app.add_slot().expect("add_slot under MAX_SECTIONS");
        let slot_a = app.slots[0].id;

        // Seed both slots as Saved with distinct
        // settings, so we can tell them apart after
        // resume and confirm only the focused one was
        // touched.
        for (slot, lang) in [(slot_a, "en"), (slot_b, "de")] {
            let pos = app.slot_index(slot).unwrap();
            let saved = SavedTranscript {
                committed: Arc::new(PlMutex::new(Vec::new())),
                refined: Arc::new(PlMutex::new(Vec::new())),
                label: format!("mic · cloud:OFF · {lang}"),
                target: Target::Stdout,
                source: SessionSource::Microphone,
                settings: SectionSettings {
                    cloud_on: false,
                    language: lang.into(),
                    model: "tiny.en".into(),
                },
            session_started_at: chrono::Utc::now(),
            role: None,
        };
            app.slots[pos].kind = SlotKind::Saved { saved };
        }

        // Focus slot A. (add_slot advances focus to the
        // new slot, so we put it back on A explicitly.)
        app.focused_slot = slot_a;

        let _ = app.resume_section(slot_a);

        // Slot B (non-focused) must remain Saved with
        // its original German language, identical
        // structure — not transitioned, not modified
        // by a resume call on slot A.
        let b_state = &app.slots[app.slot_index(slot_b).unwrap()].kind;
        match b_state {
            SlotKind::Saved { saved } => {
                assert_eq!(
                    saved.settings.language, "de",
                    "non-focused slot B's language must not be modified by \
                     resume_section on the focused slot"
                );
                assert_eq!(saved.label, "mic · cloud:OFF · de");
            }
            other => panic!(
                "non-focused slot B must remain Saved, got: {other:?}"
            ),
        }
    }

    /// A `Saved` slot's resume must pick up not just settings
    /// but also picker-level changes the user made after
    /// pressing `s`: a different device (source kind), a
    /// different app, and a different target.
    ///
    /// User scenario: record from the MacBook mic to
    /// Stdout, stop, then change the picker to an output
    /// device (EPOS) with Chrome selected and the target
    /// set to Cloud, then resume. The resumed section
    /// must use the post-stop source and target, not the
    /// saved snapshot's.
    ///
    /// Currently red: `resume_section` pulls the source
    /// verbatim off the Saved variant and hands it to
    /// `start_section`, so any post-stop change to the
    /// Devices or Apps picker is silently ignored. (The
    /// target IS handled correctly already —
    /// `start_section` consumes `pending_target_overrides`
    /// — so the test's target assertion would pass today
    /// even without the source fix. We assert on target
    /// anyway as a regression guard.)
    ///
    /// The test is pipeline-aware: the slot may stay
    /// `Saved` (capture fails in headless) or transition
    /// to `Recording` (capture succeeds). Both outcomes
    /// are acceptable; the assertion reads source/target
    /// off whichever variant the slot ended up in.

    // ── try_start_new_section state matrix (Enter key) ───────────

    /// Enter pressed with all slots non-Empty (one Saved
    /// slot) must NOT silently re-start the slot, and the
    /// error message must clearly tell the user how to
    /// resume — not the misleading "all 3 sections are
    /// recording" line that today implies no slots can be
    /// touched.
    ///
    /// The user scenario: record, stop (s), then press
    /// Enter expecting to start a NEW session. The slot
    /// is paused, so the right action is [R] to resume
    /// the existing section, or [x] to clear, or [-] to
    /// remove the slot. The error must mention R so the
    /// user discovers the resume key.
    ///
    /// Currently red: try_start_new_section returns
    /// "All 3 sections are recording — stop one first
    /// ([s])" which (a) is factually wrong (no slot is
    /// Recording), (b) hardcodes "3" regardless of slot
    /// count, and (c) doesn't mention the R key the
    /// user actually wants. The fix in the next commit
    /// rewrites the message to mention R and to reflect
    /// the actual state (slots non-Empty, none actively
    /// recording).
    #[test]
    fn try_start_new_section_with_paused_slot_says_use_r_to_resume() {
        use voice_bird_cli::config::AudioSessionKind;

        let mut app = App::new();
        // Seed a Saved slot so the App has no Empty
        // slots — next_free_slot returns None.
        let _slot = app.slots[0].id;
        app.slots[0].kind = SlotKind::Saved {
            saved: SavedTranscript {
                committed: Arc::new(PlMutex::new(Vec::new())),
                refined: Arc::new(PlMutex::new(Vec::new())),
                label: "mic · cloud:OFF".into(),
                target: Target::Stdout,
                source: SessionSource::Microphone,
                settings: SectionSettings {
                    cloud_on: false,
                    language: "en".into(),
                    model: "tiny.en".into(),
                },
            session_started_at: chrono::Utc::now(),
            role: None,
        },
        };
        // Populate a device so we don't fail on the
        // "no device" branch before the "no free slot"
        // branch.
        app.devices = vec![AudioDevice {
            name: "MacBook Pro Microphone".into(),
            kind: AudioSessionKind::Input,
        }];
        app.selected_device_index = 0;

        let err = app.try_start_new_section().unwrap_err();
        // The new contract: the message must point the
        // user at R (resume) since pressing Enter was the
        // wrong key for a paused slot.
        assert!(
            err.contains('R') || err.to_lowercase().contains("resume"),
            "Enter on a paused slot must point the user at R to resume; got: {err}"
        );
        // And it must NOT use the misleading
        // "all 3 sections are recording" line — the
        // slot is paused, not recording.
        assert!(
            !err.contains("all 3 sections are recording"),
            "Enter on a paused slot must not claim sections are recording; got: {err}"
        );
    }

    /// Enter pressed on a fresh App (one Empty slot) must
    /// reach `start_section` — the test is pipeline-aware
    /// (capture may succeed or fail in the test env).
    /// The point: the "no free slot" branch is only
    /// taken when no slot is Empty; with the default
    /// App::new() state we have one Empty slot, so the
    /// method should NOT return the "no free slot"
    /// error.
    ///
    /// Currently green: the refactor preserved this
    /// behavior. It's a regression guard so the fix
    /// doesn't break the happy path.
    #[test]
    fn try_start_new_section_with_empty_slot_does_not_refuse_no_free_slot() {
        let mut app = App::new();
        // Default App::new() ships one Empty slot. The
        // method should reach start_section (which may
        // fail on capture in headless tests, but the
        // error must NOT be the "no free slot" branch).
        let result = app.try_start_new_section();
        if let Err(err) = &result {
            assert!(
                !err.contains("All 3 sections are recording"),
                "fresh App with an Empty slot must not hit the no-free-slot branch; got: {err}"
            );
            assert!(
                !err.to_lowercase().contains("all 3 sections"),
                "fresh App must not see the 'all 3' message; got: {err}"
            );
        }
    }

    /// Pre-fix bug: `App::selected_device_index` and
    /// `selected_app_index` were single global fields shared by
    /// every slot. Tabbing between slots changed the cursor once
    /// and every slot saw the same value — the picker state wasn't
    /// slot-specific.
    ///
    /// Post-fix: each slot memoizes its last picker cursor in
    /// `slot_picker_memo`. `focus_next` saves the outgoing slot's
    /// state and restores the incoming slot's memo. `+` inherits
    /// the current cursor; `-` cleans up the removed slot's memo
    /// and restores the new focused slot's.
    #[cfg(not(windows))]
    #[test]
    fn focus_next_memoizes_picker_state_per_slot() {
        use crate::platform::{AppSession, AudioDevice};
        use voice_bird_cli::config::AudioSessionKind;

        let mut app = App::new();

        // Seed inventory with two devices and two apps so the
        // cursor can land on either.
        app.devices = vec![
            AudioDevice {
                name: "MacBook Pro Microphone".into(),
                kind: AudioSessionKind::Input,
            },
            AudioDevice {
                name: "USB Headset".into(),
                kind: AudioSessionKind::Input,
            },
        ];
        app.apps = vec![
            AppSession {
                id: "us.zoom.xos".into(),
                name: "Zoom".into(),
                process_id: 1,
            },
            AppSession {
                id: "com.google.Chrome".into(),
                name: "Google Chrome".into(),
                process_id: 2,
            },
        ];

        // Default: slot 1 focused, cursor at device 0, no app.
        let slot_a = app.focused_slot;
        app.selected_device_index = 0;
        app.selected_app_index = None;
        app.picker_focus = PickerFocus::Devices;

        // Add slot B and put the cursor on a different device+app.
        let slot_b = app.add_slot().expect("add_slot under MAX_SECTIONS");
        app.selected_device_index = 1;
        app.selected_app_index = Some(1);
        app.picker_focus = PickerFocus::Apps;

        // Tab back to slot A — the cursor must restore to
        // whatever slot A had when we left it (device 0, no app).
        app.focus_next();
        assert_eq!(
            app.focused_slot, slot_a,
            "Tab from slot B must focus slot A"
        );
        assert_eq!(
            app.selected_device_index, 0,
            "slot A's memoized device must be restored; got {}",
            app.selected_device_index
        );
        assert_eq!(
            app.selected_app_index, None,
            "slot A's memoized app must be restored (None); got {:?}",
            app.selected_app_index
        );
        assert_eq!(
            app.picker_focus,
            PickerFocus::Devices,
            "slot A's memoized pane focus must be restored"
        );

        // Tab forward again to slot B — the cursor must restore
        // to whatever slot B had (device 1, app 1, Apps pane).
        app.focus_next();
        assert_eq!(app.focused_slot, slot_b);
        assert_eq!(
            app.selected_device_index, 1,
            "slot B's memoized device must be restored; got {}",
            app.selected_device_index
        );
        assert_eq!(
            app.selected_app_index,
            Some(1),
            "slot B's memoized app must be restored; got {:?}",
            app.selected_app_index
        );
        assert_eq!(
            app.picker_focus,
            PickerFocus::Apps,
            "slot B's memoized pane focus must be restored"
        );

        // Independence check: change slot A's cursor while focused
        // on A, then Tab to B (B's memo was captured before this
        // change) and assert B is untouched, then Tab back to A
        // and assert A kept the new value.
        // After the prior round-trip, focused_slot is slot_b —
        // Tab back to slot A first.
        app.focus_next();
        assert_eq!(app.focused_slot, slot_a);
        // Now mutate slot A's cursor.
        app.selected_device_index = 1;
        app.selected_app_index = None;
        app.picker_focus = PickerFocus::Devices;
        // Tab to B — slot B's memo must restore to whatever it
        // was at the previous Tab-away (device 1, app Some(1),
        // Apps pane), NOT slot A's just-edited values.
        app.focus_next();
        assert_eq!(app.focused_slot, slot_b);
        assert_eq!(
            app.selected_device_index, 1,
            "slot B's memoized device must be unchanged after slot A edits"
        );
        assert_eq!(
            app.selected_app_index,
            Some(1),
            "slot B's memoized app must be unchanged after slot A edits"
        );
        assert_eq!(
            app.picker_focus,
            PickerFocus::Apps,
            "slot B's memoized pane focus must be unchanged"
        );
        // Tab back to A — slot A's NEW cursor must survive.
        app.focus_next();
        assert_eq!(app.focused_slot, slot_a);
        assert_eq!(
            app.selected_device_index, 1,
            "slot A's NEW device must survive Tab round-trip"
        );
        assert_eq!(
            app.selected_app_index, None,
            "slot A's NEW app selection (None) must survive Tab round-trip"
        );
        // Removing slot A must drop its memo, leave slot B
        // focused with its own memo restored.
        app.remove_focused_slot();
        assert_eq!(
            app.focused_slot, slot_b,
            "removing slot A must focus slot B"
        );
        assert_eq!(
            app.selected_device_index, 1,
            "slot B's memo must be restored after slot A removal"
        );
        assert_eq!(
            app.selected_app_index,
            Some(1),
            "slot B's memoized app must be restored after slot A removal"
        );
        assert!(
            !app.slot_picker_memo.contains_key(&slot_a),
            "removed slot's memo must be dropped (not leaked)"
        );
    }

    // ── Cloud-target refactor: cloud is a transport, not a target ──

    /// The agents picker no longer offers Cloud as a
    /// destination. With cloud removed from the
    /// `Target` enum, `App::targets()` returns exactly
    /// one row per known destination: Stdout (row 0)
    /// plus one row per user-configured Agent target.
    /// The "Cloud as a target" row is gone — cloud
    /// becomes a per-section *transport* flag (cloud_on
    /// on the section's settings) rather than a
    /// destination in its own right.
    ///
    /// Currently red: `targets()` returns
    /// `2 + N agent_rows` (Stdout + Cloud + agents).
    /// After the fix it returns `1 + N agent_rows`.
    /// Resume a Saved slot whose saved target is
    /// Stdout but whose saved `cloud_on` is true
    /// (the user wants server streaming for a
    /// local-files session). The resume path must
    /// honor cloud_on=true, not clobber it from
    /// target.
    ///
    /// Currently red: `start_section` runs
    /// `settings.cloud_on = matches!(target, Target::Cloud)`,
    /// which evaluates to false for any non-Cloud
    /// target (including Stdout). So a user-saved
    /// cloud_on=true on a Stdout session is
    /// silently forced to false on resume. The
    /// fix drops the clobber line.
    #[test]
    fn resume_honors_cloud_on_even_when_saved_target_is_stdout() {
        use voice_bird_cli::config::AudioSessionKind;

        let mut app = App::new();
        app.config.voicebird_api_key = "sk-test".into();
        app.devices = vec![AudioDevice {
            name: "MacBook Pro Microphone".into(),
            kind: AudioSessionKind::Input,
        }];
        app.selected_device_index = 0;
        let slot = app.slots[0].id;
        // The slot's customization says cloud_on = true.
        // The new model: slot.config is the single source
        // of truth, not a per-source override map.
        let slot_obj = app
            .slots
            .iter_mut()
            .find(|s| s.id == slot)
            .expect("focused slot must exist");
        slot_obj.config.cloud_on = Some(true);
        // Saved as: target=Stdout, cloud_on=true
        // (user wants server streaming + local
        // files).
        let saved = SavedTranscript {
            committed: Arc::new(PlMutex::new(Vec::new())),
            refined: Arc::new(PlMutex::new(Vec::new())),
            label: "mic · cloud:ON".into(),
            target: Target::Stdout,
            source: SessionSource::Microphone,
            settings: SectionSettings {
                cloud_on: true,
                language: "en".into(),
                model: "tiny.en".into(),
            },
            session_started_at: chrono::Utc::now(),
            role: None,
        };
        app.slots[0].kind = SlotKind::Saved { saved };

        let _ = app.resume_section(slot);

        let applied_cloud_on = match &app.slots[0].kind {
            SlotKind::Saved { saved } => saved.settings.cloud_on,
            SlotKind::Recording { section } =>
                section.settings.cloud_on,
            SlotKind::Empty => panic!(
                "resume must not empty the slot"
            ),
        };
        assert!(
            applied_cloud_on,
            "resume must honor the saved cloud_on=true \
             even when the saved target was Stdout; the \
             cloud flag is a transport, not a function \
             of the target"
        );
    }
    /// `start_section` must persist a local session directory for
    /// `Target::Stdout` even when the cloud transport is on.
    ///
    /// Today (pre-fix) `cloud_on = true` flips `session_dir` to
    /// `None` unconditionally — the recording lives entirely on
    /// voicebird.app and the user has no local copy. That's the
    /// right behaviour for `Target::Agent` (the agent runtime
    /// is the destination) but wrong for `Target::Stdout`, where
    /// the picker label explicitly advertises a local-on-disk
    /// transcript (`audio.wav`, `transcript.jsonl`, `meta.json`).
    /// The user wants the cloud engine to do the ASR AND a
    /// local copy to land on disk.
    ///
    /// We can't inspect `Section::session_dir` directly from
    /// outside `start_section` (it stays a local variable
    /// until the `Recording` variant is constructed, and the
    /// cpal capture in the test environment is expected to
    /// fail with "no input device"). The directory is created
    /// BEFORE capture opens, so a successful `create_dir_all`
    /// call is observable on disk even when `start_section`
    /// returns `Err` downstream. We assert exactly that: a
    /// single `2026-…-mic` directory under
    /// `app.config.session_dir` post-resume.
    #[test]
    fn start_section_stdout_target_with_cloud_on_creates_local_session_dir() {
        use voice_bird_cli::config::AudioSessionKind;

        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new();
        app.default_slot_config.path = dir.path().to_string_lossy().into_owned();
        app.config.voicebird_api_key = "sk-test".into();
        // Devices picker so `resolve_focused_source` returns
        // `Microphone` and the session slug picks the `-mic`
        // suffix.
        app.devices = vec![AudioDevice {
            name: "MacBook Pro Microphone".into(),
            kind: AudioSessionKind::Input,
        }];
        app.selected_device_index = 0;
        let slot = app.slots[0].id;
        // Seed a Saved variant with target=Stdout, cloud_on=true.
        // The label must match what the picker would have written
        // for this combination.
        let saved = SavedTranscript {
            committed: Arc::new(PlMutex::new(Vec::new())),
            refined: Arc::new(PlMutex::new(Vec::new())),
            label: "mic · cloud:ON".into(),
            target: Target::Stdout,
            source: SessionSource::Microphone,
            settings: SectionSettings {
                cloud_on: true,
                language: "en".into(),
                model: "tiny.en".into(),
            },
            session_started_at: chrono::Utc::now(),
            role: None,
        };
        app.slots[0].kind = SlotKind::Saved { saved };

        // Resume delegates to start_section. We don't care
        // whether the call returns Ok or Err — what matters
        // is the side effect on disk.
        let _ = app.resume_section(slot);

        // Exactly one session directory was created under the
        // configured session_dir. The slug is
        // `<timestamp>-<source-suffix>`, with the source suffix
        // being `mic` for `SessionSource::Microphone`.
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "Stdout + cloud_on=true must create exactly one local \
             session directory (slug = `<ts>-<source>`); got {entries:?}",
        );
        let name = entries[0].file_name();
        let name = name.to_string_lossy();
        assert!(
            name.ends_with("-mic"),
            "session dir slug must end with `-mic` for the microphone \
             source; got {name:?}",
        );
    }


    // Runs on every platform: asserts the forcing on Windows and the
    // no-op everywhere else, since the helpers branch on cfg! at runtime.
    #[test]
    fn cloud_only_platform_invariants() {
        let mut config = AppConfig::default();
        config.default_slot_config.cloud_on = false;
        enforce_cloud_only_platform(&mut config);
        assert_eq!(config.default_slot_config.cloud_on, cfg!(windows));

        let mut settings = SectionSettings {
            cloud_on: config.default_slot_config.cloud_on,
            language: config.default_slot_config.language.clone(),
            model: config.default_slot_config.model.clone(),
        };
        settings.cloud_on = false;
        clamp_section_settings_for_platform(&mut settings);
        assert_eq!(settings.cloud_on, cfg!(windows));
    }

    // Everything below exercises the local-session export path
    // (ws_url_to_http / find_latest_session / export_transcript), which
    // doesn't exist on cloud-only Windows.
    #[cfg(not(windows))]
    mod local_export {
        use super::*;

        // ── ws_url_to_http tests (phase 1) ────────────────────────────────

        #[test]
        fn ws_url_wss_with_path() {
            assert_eq!(
                ws_url_to_http("wss://voicebird.app/api/audio/stream"),
                "https://voicebird.app"
            );
        }

        #[test]
        fn ws_url_ws_with_port_produces_http() {
            assert_eq!(
                ws_url_to_http("ws://127.0.0.1:3000"),
                "http://127.0.0.1:3000"
            );
        }

        #[test]
        fn ws_url_ws_with_path_produces_http() {
            assert_eq!(
                ws_url_to_http("ws://localhost:9999/api/audio/stream"),
                "http://localhost:9999"
            );
        }

        #[test]
        fn ws_url_no_scheme_falls_through_to_https() {
            assert_eq!(ws_url_to_http("voicebird.app"), "https://voicebird.app");
        }

        #[test]
        fn ws_url_preserves_port() {
            assert_eq!(
                ws_url_to_http("wss://voicebird.app:8080/path"),
                "https://voicebird.app:8080"
            );
        }

        #[test]
        fn ws_url_bare_host() {
            assert_eq!(
                ws_url_to_http("wss://voicebird.app"),
                "https://voicebird.app"
            );
        }

        // ── find_latest_session tests (phase 1) ───────────────────────

        #[test]
        fn find_latest_empty_dir() {
            let dir = tempfile::tempdir().unwrap();
            assert!(find_latest_session(dir.path()).is_none());
        }

        #[test]
        fn find_latest_dir_does_not_exist() {
            let path = std::path::Path::new("/tmp/voice-bird-nonexistent-test-dir-xyzzy");
            assert!(find_latest_session(path).is_none());
        }

        #[test]
        fn find_latest_one_session() {
            let dir = tempfile::tempdir().unwrap();
            let session = dir.path().join("2026-05-13_10-00-00-mic");
            std::fs::create_dir(&session).unwrap();
            std::fs::write(session.join("transcript.json"), "{}").unwrap();

            let found = find_latest_session(dir.path()).unwrap();
            assert_eq!(found, session);
        }

        #[test]
        fn find_latest_picks_latest_by_name() {
            let dir = tempfile::tempdir().unwrap();
            let older = dir.path().join("2026-05-13_09-00-00-mic");
            let newer = dir.path().join("2026-05-13_12-00-00-mic");
            std::fs::create_dir(&older).unwrap();
            std::fs::create_dir(&newer).unwrap();
            std::fs::write(older.join("transcript.json"), "{}").unwrap();
            std::fs::write(newer.join("transcript.json"), "{}").unwrap();

            let found = find_latest_session(dir.path()).unwrap();
            assert_eq!(
                found, newer,
                "should pick lexicographically latest (= newest timestamp)"
            );
        }

        #[test]
        fn find_latest_includes_uploaded() {
            let dir = tempfile::tempdir().unwrap();
            let uploaded = dir.path().join("2026-05-13_12-00-00-mic");
            let fresh = dir.path().join("2026-05-13_09-00-00-mic");
            std::fs::create_dir(&uploaded).unwrap();
            std::fs::create_dir(&fresh).unwrap();
            std::fs::write(uploaded.join("transcript.json"), "{}").unwrap();
            std::fs::write(uploaded.join(".uploaded"), "x").unwrap();
            std::fs::write(fresh.join("transcript.json"), "{}").unwrap();

            let found = find_latest_session(dir.path()).unwrap();
            assert_eq!(
                found, uploaded,
                "should return the most recent session even if uploaded"
            );
        }

        #[test]
        fn find_latest_all_uploaded_returns_latest() {
            let dir = tempfile::tempdir().unwrap();
            for i in 0..3 {
                let session = dir.path().join(format!("2026-05-13_1{i}-00-00-mic"));
                std::fs::create_dir(&session).unwrap();
                std::fs::write(session.join("transcript.json"), "{}").unwrap();
                std::fs::write(session.join(".uploaded"), format!("id-{i}")).unwrap();
            }
            // find_latest_session doesn't care about .uploaded — it returns
            // the most recent session with transcript.json regardless.
            let found = find_latest_session(dir.path()).unwrap();
            assert_eq!(
                found.file_name().unwrap().to_string_lossy(),
                "2026-05-13_12-00-00-mic"
            );
        }

        #[test]
        fn find_latest_no_transcript_json_skips() {
            let dir = tempfile::tempdir().unwrap();
            let session = dir.path().join("2026-05-13_10-00-00-mic");
            std::fs::create_dir(&session).unwrap();
            // Only transcript.jsonl, no transcript.json
            std::fs::write(session.join("transcript.jsonl"), "...").unwrap();
            assert!(find_latest_session(dir.path()).is_none());
        }

        #[test]
        fn find_latest_ignores_plain_files() {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("README.txt"), "hello").unwrap();
            let session = dir.path().join("2026-05-13_10-00-00-mic");
            std::fs::create_dir(&session).unwrap();
            std::fs::write(session.join("transcript.json"), "{}").unwrap();

            let found = find_latest_session(dir.path()).unwrap();
            assert_eq!(found, session);
        }

        // ── export_transcript banner-only tests (phase 1) ─────────────────

        /// Helper: write a minimal valid transcript.json into `session_dir`.
        fn write_minimal_transcript(session_dir: &std::path::Path) {
            let meta = voice_bird_cli::session::finalize::SessionMeta {
                version: "1.0".into(),
                model: "whisper-large-v3".into(),
                engine: "whisperkit".into(),
                source: "mic".into(),
                device: "MacBook Pro Microphone".into(),
                started_at: "2026-05-13T10:00:00Z".into(),
                ended_at: "2026-05-13T10:05:00Z".into(),
                duration_ms: 300_000,
                role: None,
                room_slug: None,
            };
            let segments: Vec<voice_bird_cli::session::writer::WrittenSegment> =
                vec![voice_bird_cli::session::writer::WrittenSegment {
                    t_start_ms: 0,
                    t_end_ms: 2500,
                    text: "Hello world".into(),
                }];
            let json = serde_json::json!({
                "segments": segments,
                "meta": meta,
            });
            std::fs::write(session_dir.join("transcript.json"), json.to_string()).unwrap();
        }

        #[test]
        fn export_banner_no_sessions() {
            let dir = tempfile::tempdir().unwrap();
            let mut app = App::new();
            app.default_slot_config.path = dir.path().to_string_lossy().to_string();

            app.export_transcript();

            assert!(app.export_banner.is_some());
            assert!(
                app.export_banner
                    .as_deref()
                    .unwrap()
                    .contains("No sessions"),
                "got: {:?}",
                app.export_banner
            );
        }

        #[test]
        fn export_banner_unreadable_transcript() {
            let dir = tempfile::tempdir().unwrap();
            let session = dir.path().join("2026-05-13_10-00-00-mic");
            std::fs::create_dir(&session).unwrap();
            std::fs::write(session.join("transcript.json"), "not json {{{{{").unwrap();

            let mut app = App::new();
            app.default_slot_config.path = dir.path().to_string_lossy().to_string();

            app.export_transcript();

            assert!(app.export_banner.is_some());
            let msg = app.export_banner.as_deref().unwrap();
            assert!(
                msg.contains("Failed to parse"),
                "expected parse failure, got: {msg:?}"
            );
        }

        // ── integration tests with mock HTTP (phases 3-4) ────────────────

        /// Spawn a single-shot mock HTTP server on an OS-assigned port.
        /// The handler thread accepts one connection, reads the request into
        /// `captured`, and replies with the given status + JSON body.
        struct MockHttp {
            port: u16,
            captured: Arc<Mutex<Option<String>>>,
            handle: Option<std::thread::JoinHandle<()>>,
        }

        impl MockHttp {
            fn start(status: u16, response_body: &'static str) -> Self {
                let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
                let c = Arc::clone(&captured);
                let body = response_body.to_string();

                let listener = TcpListener::bind("127.0.0.1:0").unwrap();
                let port = listener.local_addr().unwrap().port();
                listener.set_nonblocking(false).ok();

                let handle = std::thread::spawn(move || {
                    listener.set_nonblocking(true).ok();

                    // Busy-wait for a connection with 10ms polling;
                    // give up after ~2s to avoid hanging tests that
                    // short-circuit before any HTTP call.
                    let started = std::time::Instant::now();
                    let mut stream = loop {
                        match listener.accept() {
                            Ok(s) => break s.0,
                            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                std::thread::sleep(std::time::Duration::from_millis(10));
                                if started.elapsed() > std::time::Duration::from_secs(2) {
                                    return;
                                }
                            }
                            Err(_) => return,
                        }
                    };

                    // We have a connection. Switch back to blocking for r/w.
                    stream.set_nonblocking(false).ok();
                    stream
                        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                        .ok();
                    stream
                        .set_write_timeout(Some(std::time::Duration::from_secs(5)))
                        .ok();

                    let mut raw = String::new();
                    let mut content_length: usize = 0;

                    {
                        let mut reader = BufReader::new(&stream);

                        // request line
                        let mut line = String::new();
                        if reader.read_line(&mut line).is_ok() {
                            raw.push_str(&line);
                        }

                        // headers
                        loop {
                            line.clear();
                            if reader.read_line(&mut line).is_err() {
                                break;
                            }
                            if line == "\r\n" || line == "\n" {
                                raw.push_str(&line);
                                break;
                            }
                            let lower = line.to_lowercase();
                            if lower.starts_with("content-length:") {
                                content_length = line
                                    .split(':')
                                    .nth(1)
                                    .unwrap_or("0")
                                    .trim()
                                    .parse()
                                    .unwrap_or(0);
                            }
                            raw.push_str(&line);
                        }

                        // body
                        if content_length > 0 {
                            let mut buf = vec![0u8; content_length];
                            if reader.read_exact(&mut buf).is_ok() {
                                raw.push_str(&String::from_utf8_lossy(&buf));
                            }
                        }
                    } // reader dropped — releases &borrow on stream

                    *c.lock().unwrap() = Some(raw);

                    let resp = format!(
                            "HTTP/1.1 {status} OK\r\nContent-Length: {len}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
                            len = body.len()
                        );
                    let _ = stream.write_all(resp.as_bytes());
                });

                MockHttp {
                    port,
                    captured,
                    handle: Some(handle),
                }
            }

            fn captured_request(&self) -> Option<String> {
                self.captured.lock().unwrap().clone()
            }
        }

        impl Drop for MockHttp {
            fn drop(&mut self) {
                if let Some(h) = self.handle.take() {
                    let _ = h.join();
                }
            }
        }

        fn app_with_mock_server(dir: &tempfile::TempDir, port: u16) -> App {
            let mut app = App::new();
            app.default_slot_config.path = dir.path().to_string_lossy().to_string();
            app.config.voicebird_server_url = format!("ws://127.0.0.1:{port}/api/audio/stream");
            app.config.voicebird_api_key = "test-key-abc123".into();
            app
        }

        /// Helper: assert the captured raw HTTP request contains all the
        /// expected substrings.
        fn assert_request_contains(raw: &Option<String>, expected: &[&str]) {
            let raw = raw.as_deref().expect("mock server captured no request");
            for s in expected {
                assert!(
                    raw.contains(s),
                    "expected request to contain {s:?}\nfull request:\n{raw}"
                );
            }
        }

        #[test]
        fn integration_happy_path() {
            let dir = tempfile::tempdir().unwrap();
            let session = dir.path().join("2026-05-13_10-00-00-mic");
            std::fs::create_dir(&session).unwrap();
            write_minimal_transcript(&session);

            let mock = MockHttp::start(201, r#"{"success":true,"transcription_id":"abc-123"}"#);
            let mut app = app_with_mock_server(&dir, mock.port);

            app.export_transcript();

            assert!(app.export_banner.is_some());
            let msg = app.export_banner.as_deref().unwrap();
            assert!(
                msg.starts_with("Exported"),
                "expected success banner, got: {msg:?}"
            );
            assert!(msg.contains("abc-123"), "banner should contain id: {msg}");

            assert!(session.join(".uploaded").exists());
            let marker_content = std::fs::read_to_string(session.join(".uploaded")).unwrap();
            assert_eq!(marker_content, "abc-123");

            let raw = mock.captured_request();
            assert_request_contains(
                &raw,
                &[
                    "POST /api/transcripts/upload HTTP/1.1",
                    "Authorization: Bearer test-key-abc123",
                    "Content-Type: application/json",
                    "\"session_id\":\"2026-05-13_10-00-00-mic\"",
                    "\"title\":\"mic \u{2014} 2026-05-13\"",
                    "\"text\":\"Hello world\"",
                ],
            );
        }

        #[test]
        fn integration_client_side_idempotency() {
            let dir = tempfile::tempdir().unwrap();
            let session = dir.path().join("2026-05-13_10-00-00-mic");
            std::fs::create_dir(&session).unwrap();
            write_minimal_transcript(&session);
            // Pre-create the .uploaded marker
            std::fs::write(session.join(".uploaded"), "previous-export").unwrap();

            let mock = MockHttp::start(200, r#"{"ok":true}"#);
            let mut app = app_with_mock_server(&dir, mock.port);

            app.export_transcript();

            assert_eq!(
                app.export_banner.as_deref(),
                Some("Already exported \u{2713}")
            );
            assert!(
                mock.captured_request().is_none(),
                "no HTTP call expected when .uploaded marker exists"
            );
        }

        #[test]
        fn integration_server_duplicate_response() {
            let dir = tempfile::tempdir().unwrap();
            let session = dir.path().join("2026-05-13_10-00-00-mic");
            std::fs::create_dir(&session).unwrap();
            write_minimal_transcript(&session);

            let mock = MockHttp::start(
                200,
                r#"{"success":true,"transcription_id":"dup-1","duplicate":true}"#,
            );
            let mut app = app_with_mock_server(&dir, mock.port);

            app.export_transcript();

            assert_eq!(
                app.export_banner.as_deref(),
                Some("Already exported \u{2713}")
            );
            assert!(session.join(".uploaded").exists());
            assert_eq!(
                std::fs::read_to_string(session.join(".uploaded")).unwrap(),
                "dup-1"
            );
        }

        #[test]
        fn integration_network_refused() {
            let dir = tempfile::tempdir().unwrap();
            let session = dir.path().join("2026-05-13_10-00-00-mic");
            std::fs::create_dir(&session).unwrap();
            write_minimal_transcript(&session);

            let dead_socket = TcpListener::bind("127.0.0.1:0").unwrap();
            let dead_port = dead_socket.local_addr().unwrap().port();
            drop(dead_socket);

            let mut app = app_with_mock_server(&dir, dead_port);

            app.export_transcript();

            let msg = app.export_banner.as_deref().unwrap();
            assert!(
                msg.starts_with("Export failed"),
                "expected error, got: {msg:?}"
            );
            assert!(!session.join(".uploaded").exists());
        }

        #[test]
        fn integration_mock_returns_401() {
            let dir = tempfile::tempdir().unwrap();
            let session = dir.path().join("2026-05-13_10-00-00-mic");
            std::fs::create_dir(&session).unwrap();
            write_minimal_transcript(&session);

            let mock = MockHttp::start(401, r#"{"error":"Invalid or revoked API key"}"#);
            let mut app = app_with_mock_server(&dir, mock.port);

            app.export_transcript();

            let msg = app.export_banner.as_deref().unwrap();
            assert!(
                msg.contains("Invalid or revoked"),
                "expected auth error, got: {msg:?}"
            );
            assert!(!session.join(".uploaded").exists());
        }

        #[test]
        fn integration_export_multiple_sessions_picks_latest() {
            let dir = tempfile::tempdir().unwrap();

            // Older session
            let older = dir.path().join("2026-05-13_08-00-00-system");
            std::fs::create_dir(&older).unwrap();
            let meta_old = voice_bird_cli::session::finalize::SessionMeta {
                started_at: "2026-05-13T08:00:00Z".into(),
                source: "system".into(),
                version: "1.0".into(),
                model: "whisper-large-v3".into(),
                engine: "whisperkit".into(),
                device: "BlackHole".into(),
                ended_at: "2026-05-13T08:05:00Z".into(),
                duration_ms: 300_000,
                role: None,
                room_slug: None,
            };
            let seg = voice_bird_cli::session::writer::WrittenSegment {
                t_start_ms: 0,
                t_end_ms: 1000,
                text: "older".into(),
            };
            std::fs::write(
                older.join("transcript.json"),
                serde_json::json!({"segments":[seg],"meta":meta_old}).to_string(),
            )
            .unwrap();

            let newer = dir.path().join("2026-05-13_12-00-00-mic");
            std::fs::create_dir(&newer).unwrap();
            write_minimal_transcript(&newer);

            let mock = MockHttp::start(201, r#"{"success":true,"transcription_id":"newest"}"#);
            let mut app = app_with_mock_server(&dir, mock.port);

            app.export_transcript();

            let raw = mock.captured_request();
            assert!(
                raw.as_deref()
                    .unwrap_or("")
                    .contains("2026-05-13_12-00-00-mic"),
                "should export the most recent session"
            );
            assert!(
                !raw.as_deref()
                    .unwrap_or("")
                    .contains("2026-05-13_08-00-00-system"),
                "should NOT export the older session"
            );
            assert_eq!(
                app.export_banner.as_deref(),
                Some("Exported \u{2713} \u{2014} newest")
            );
        }

        // ---- E2E: Free Room is offline-safe ----
        //
        // The Free Room has no agent, so calling
        // trigger_agent_run on it must NOT spawn a
        // worker. The user gets the same "no agent"
        // banner that the g-key path surfaces. The
        // Plan §E contract says Free Room must "fully
        // offline" — the agent path is opt-in only.

        /// E2E: Free Room + no API key. trigger_agent_run
        /// returns without spawning a worker or
        /// mutating agent_run_state (the no-op guard
        /// at the top of start_agent_run short-circuits
        /// before the spawn_agent_run call).
        #[test]
        fn free_room_trigger_agent_run_is_noop() {
            let dir = tempfile::tempdir().unwrap();
            let _ = dir; // bind to silence unused warnings
            let mut app = App::new();
            // Free Room is at index 0 by App::new()'s
            // contract.
            assert_eq!(app.active_room, 0);
            assert!(!app.active_room().has_agent());
            app.trigger_agent_run(
                voice_bird_cli::cloud::RunTrigger::Auto,
                "patient: hello".to_string(),
            );
            assert!(
                app.agent_run_worker.is_none(),
                "Free Room must NOT spawn a worker"
            );
            assert_eq!(app.agent_run_state.status, "");
            assert!(app.agent_run_state.last_run_started.is_none());
        }

        /// E2E: Free Room + manual g trigger. Even with
        /// RunTrigger::Manual, Free Room is a no-op.
        #[test]
        fn free_room_manual_trigger_is_noop() {
            let mut app = App::new();
            app.trigger_agent_run(
                voice_bird_cli::cloud::RunTrigger::Manual,
                "any".into(),
            );
            assert!(app.agent_run_worker.is_none());
        }
    }
 }
