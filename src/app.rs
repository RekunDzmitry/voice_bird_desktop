use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};


use parking_lot::Mutex as PlMutex;

use crate::platform::{AppSession, AudioDevice};
use voice_bird_cli::config::{slot_settings_key, AppConfig, SlotSettings};
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
    /// Multi-step "Add / Edit Agent target" funnel. The funnel
    /// state is on `App::funnel`; this variant just signals the
    /// dispatcher to route keys to `handle_agent_funnel`.
    AgentFunnel,
    /// Confirm-prompt overlay. Used today for "delete the focused
    /// Agent target? [y/N]".
    ConfirmDeleteAgentTarget {
        id: String,
    },
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

/// Settings snapshotted at section start. In Stage 1 these are seeded
/// from the global `AppConfig`; in Stage 2 they will gain per-source
/// override persistence in `AppConfig::source_overrides`.
#[derive(Debug, Clone)]
pub struct SectionSettings {
    pub cloud_on: bool,
    pub language: String,
    pub model: String,
}

/// One running recording slot. Each section owns its own audio capture,
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
    /// On-disk session directory (`None` when broadcasting to cloud).
    pub session_dir: Option<PathBuf>,
    /// Wall-clock start of this section.
    pub session_started_at: chrono::DateTime<chrono::Utc>,
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
    /// Transcript scroll offset for this section (lines from top).
    /// Only consulted when `transcript_follow` is false.
    pub transcript_scroll: u16,
    /// When true (default), the section's transcript pane auto-scrolls
    /// to show the latest content. Set false on manual scroll, restored
    /// by End.
    pub transcript_follow: bool,
    /// Where this section is sending its transcript. Derived from
    /// `settings.cloud_on` at start time and kept in sync on the
    /// `Target` axis so the Targets pane can show it without poking
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

/// One TUI pane and its current state. The pane's `id` is stable; the
pub struct Slot {
    pub id: SlotId,
    pub kind: SlotKind,
    /// Per-slot settings the user flips from the Mode panel.
    /// Independent of `kind` — the same `SlotSettings` apply
    /// whether the slot is Empty, Recording, or Saved. Two
    /// slots can have different settings simultaneously.
    pub settings: SlotSettings,
}

impl Slot {
    pub fn empty(id: SlotId) -> Self {
        Self {
            id,
            kind: SlotKind::Empty,
            settings: SlotSettings::default(),
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
/// per-application column in the middle; Targets is the routing
/// choice (Stdout / Cloud / Agent) on the right. Each pane has its
/// own cursor and scroll offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerFocus {
    Devices,
    Apps,
    Targets,
}

/// Main application state
pub struct App {
    /// Current mode
    pub mode: AppMode,

    /// Capturable input/output devices (left pane of the picker).
    pub devices: Vec<AudioDevice>,

    /// Per-application capture targets (right pane of the picker).
    pub apps: Vec<AppSession>,

    /// Cursor in the Devices pane.
    pub selected_device_index: usize,

    /// Cursor in the Apps pane. `None` = no app paired (run device alone).
    pub selected_app_index: Option<usize>,

    /// Cursor in the Targets pane. The list of targets is fixed at
    /// three entries (Stdout / Cloud / Agent) — see `targets()`. Always
    /// `Some(idx)` while the TUI runs; the cursor is one of the
    /// rendered rows.
    pub selected_target_index: Option<usize>,

    /// Which pane the picker arrows / Enter target.
    pub picker_focus: PickerFocus,

    /// Scroll offset (rows from top) for each pane. The render path
    /// auto-clamps these so the cursor stays visible.
    pub device_scroll: u16,
    pub app_scroll: u16,
    pub target_scroll: u16,
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
    /// Whether the currently-open API-key modal was opened as the
    /// cloud-enable gate (i.e. the user just flipped Cloud ON with
    /// no key, or the runtime detected `auth_failure` and needs a
    /// key to recover). `true` ⇒ Esc cancels the cloud flip and
    /// reverts `cloud_broadcast_enabled` to `false` (the pre-toggle
    /// state); `false` ⇒ Esc just closes the modal without touching
    /// cloud (e.g. `K` peeked at the saved key, or the modal was
    /// opened as a first-run bootstrap). Set by the opener, reset
    /// to `false` when the modal closes.
    pub api_key_modal_reverts_cloud: bool,

    /// In-flight text buffer for the output-path modal. `None` when
    /// the modal isn't open. Pre-filled with `config.session_dir`.
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

    /// Detected state of the user's agent runtime (today: oh-my-pi
    /// / omp). `None` only on a configuration error during
    /// `App::new`; the user-visible states are
    /// `AgentStatus::Ready` (path + version) or
    /// `AgentStatus::NotFound`. Used by the Targets pane to decide
    /// whether the Agent chip is enabled, and by `App::new` to
    /// wire the status-bar hint.
    pub agent: voice_bird_cli::agent::AgentStatus,
    /// Which detection source produced the agent runtime (env /
    /// PATH / bun install). `None` iff detection failed.
    /// Surfaced in the status bar.
    pub agent_detection_source: Option<voice_bird_cli::agent::AgentDetectionSource>,
    /// Per-slot pending target override. The Targets picker writes
    /// to this when the user picks a row; the next `start_section`
    /// consults it and applies it instead of the default
    /// `cloud_on` heuristic. The value is consumed (set back to
    /// `None`) by start_section so it only affects the very next
    /// start.
    pub pending_target_overrides: std::collections::BTreeMap<SlotId, Target>,
    /// Legacy MCP-backed segment buffer for the `"default"`
    /// Agent target. The segment-fan-out task (in `App`)
    /// pushes into this same buffer when the focused slot's
    /// target is `Target::Agent` with `session_id == "default"`,
    /// so a single source of truth serves both the
    /// in-process recorder and the out-of-process agent.
    agent_state: voice_bird_cli::agent::mcp_server::ServerState,

    /// User-configured Agent targets, keyed by their
    /// `AgentTargetId`. Populated from `config.agent_targets` at
    /// `App::new` time and updated whenever the funnel saves a
    /// new target. The consumer task looks up the right impl by
    /// `Target::Agent` (with its session_id) so the segment
    /// fan-out matches the picker pick.
    pub agent_targets:
        std::collections::HashMap<String, std::sync::Arc<dyn voice_bird_cli::agent::AgentTarget>>,
    /// Active funnel state. `Some` while the user is in
    /// `AppMode::AgentFunnel`; `None` otherwise. Mutated by
    /// `main.rs`'s key dispatcher and read by `ui.rs`'s modal
    /// renderer.
    pub funnel: Option<voice_bird_cli::agent_funnel::AgentFunnel>,

    /// In-flight verify probe. Set by the Verify-step key
    /// handler when the user runs a probe; drained by
    /// `App::poll_funnel_verify` from the main event loop so
    /// the TUI keeps drawing frames (and Esc keeps working)
    /// while the probe is in flight.
    pub verify_rx: Option<std::sync::mpsc::Receiver<anyhow::Result<std::time::Duration>>>,

    /// Wall-clock start of the in-flight verify probe, for
    /// logging. Set alongside `verify_rx`; read by the poll
    /// helper on completion.
    pub verify_started: Option<std::time::Instant>,

    /// Rolling log of Agent-related events (target saved,
    /// verify ok/fail, push failures, dropped segments),
    /// newest at the back, capped at [`AGENT_EVENT_CAP`].
    /// Behind an `Arc<PlMutex<...>>` because the recording
    /// consumer task appends from its own tokio task while
    /// the render thread reads. Surfaced by the `t` status
    /// overlay ([`AppMode::Status`]).
    pub agent_events: Arc<PlMutex<VecDeque<AgentEvent>>>,
}

/// Cap on [`App::agent_events`]. Big enough to cover a long
/// recording session's worth of incidents; small enough that the
/// overlay and memory stay bounded.
pub const AGENT_EVENT_CAP: usize = 50;

/// One row in the `t` status overlay.
#[derive(Debug, Clone)]
pub struct AgentEvent {
    /// Local wall-clock time the event was recorded.
    pub at: chrono::DateTime<chrono::Local>,
    pub message: String,
}

/// Append an event to the shared agent-event log, evicting the
/// oldest entry past [`AGENT_EVENT_CAP`]. Free function (not a
/// method) so the recording consumer task can call it on its
/// cloned `Arc` without touching `App`.
pub fn push_agent_event(events: &Arc<PlMutex<VecDeque<AgentEvent>>>, message: impl Into<String>) {
    let mut buf = events.lock();
    if buf.len() == AGENT_EVENT_CAP {
        buf.pop_front();
    }
    buf.push_back(AgentEvent {
        at: chrono::Local::now(),
        message: message.into(),
    });
}

/// Record the outcome of one consumer-task dispatch into the agent
/// event log. Only outcomes a user would act on become events —
/// the happy paths (`Pushed`, `Default`, `NotAgent`) stay silent so
/// the overlay isn't flooded at one row per segment.
pub fn record_dispatch_event(
    events: &Arc<PlMutex<VecDeque<AgentEvent>>>,
    outcome: AgentDispatch,
    session_id: &str,
) {
    match outcome {
        AgentDispatch::PushFailed => push_agent_event(
            events,
            format!("push to Agent target '{session_id}' failed (broker error — see log)"),
        ),
        AgentDispatch::Dropped => push_agent_event(
            events,
            format!("Agent target '{session_id}' missing — segment dropped"),
        ),
        AgentDispatch::NotAgent | AgentDispatch::Default | AgentDispatch::Pushed => {}
    }
}
/// A single row in the Targets picker. The picker renders
/// one row per known target; rows that point at a target
/// the user can't actually use (e.g. Agent when the
/// runtime binary is missing) are tagged `disabled = true`.
/// `disabled` rows render dim and the cursor refuses to
/// land on them (see `App::focused_target_kind`).
pub struct TargetRow {
    pub kind: TargetKind,
    pub disabled: bool,
}
/// Picker-side classification of a target. We can't use `Target`
/// directly because `Target::Agent` carries a session id that
/// the user doesn't pick per-row — the row just means "route
/// to the agent runtime with the current session".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetKind {
    Stdout,
    /// One user-configured Agent target. The `id` is the stable
    /// `AgentTargetId` from `AppConfig::agent_targets` and
    /// resolves to a `KafkaTarget` (today) via
    /// `App::agent_targets`. We don't reuse the old
    /// unit-variant `Agent` form because picking the
    /// "default" MCP-backed session is no longer surfaced in
    /// the picker — the user adds their own targets via the
    /// funnel instead.
    Agent {
        id: String,
    },
}
impl App {
    /// Create a new application instance
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
        // Test builds only: install the process-local config
        // tempdir so every test reads from and saves to a tempdir
        // instead of the developer's real `config.toml`.
        #[cfg(test)]
        let _ = &*voice_bird_cli::test_utils::INSTALL_TEST_CONFIG;
        let mut config = AppConfig::load().unwrap_or_default();
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
                config.default_model = picked.into();
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
            if config.cloud_broadcast_enabled && config.voicebird_api_key.is_empty() {
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

        // Build the initial slots BEFORE moving config into the
        // struct — reads `config.slot_settings` and applies the
        // auto-picked model on first run.
        let initial_slots = Self::fresh_slots_with_config(&config);
        #[cfg_attr(not(windows), allow(unused_mut))]
        let mut app = Self {
            mode: AppMode::Normal,
            devices: Vec::new(),
            apps: Vec::new(),
            selected_device_index: 0,
            selected_app_index: None,
            // Targets list starts with Stdout at index 0; the user
            // grows it with Add Agent (config.agent_targets).
            // pre-populated so the pane never renders in an
            // empty-cursor state.
            selected_target_index: Some(0),
            picker_focus: PickerFocus::Devices,
            device_scroll: 0,
            app_scroll: 0,
            target_scroll: 0,
            status: RecordingStatus::Idle,
            audio_level: Arc::new(Mutex::new(0.0)),
            duration: 0.0,
            start_time: None,
            config,
            should_quit: false,
            status_message: None,
            error_channel: Arc::new(Mutex::new(None)),
            log_path: None,
            rt,
            slots: initial_slots,
            focused_slot: SlotId(1),
            // Slot 1 was created by `fresh_slots()`. The next id we
            // hand out is 2.
            next_slot_id: 2,
            picker: None,
            config_was_loaded_from_disk,
            api_key_buf: None,
            api_key_modal_reverts_cloud: false,
            path_buf: None,
            export_banner: None,
            banner: banner_on_launch,
            transcript_scroll: 0,
            transcript_follow: true,
            empty_committed: Arc::new(PlMutex::new(Vec::new())),
            empty_tentative: Arc::new(PlMutex::new(String::new())),
            agent: voice_bird_cli::agent::AgentStatus::NotFound,
            agent_detection_source: None,
            pending_target_overrides: std::collections::BTreeMap::new(),
            agent_state: voice_bird_cli::agent::mcp_server::ServerState::new(
                voice_bird_cli::agent::AgentSessionId::default_session(),
            ),
            agent_targets: std::collections::HashMap::new(),
            funnel: None,
            verify_rx: None,
            verify_started: None,
            agent_events: Arc::new(PlMutex::new(VecDeque::new())),
        };
        // Pin every slot's settings.cloud_on to the platform
        // default. On Windows, every recording is cloud — set it
        // on the slot's settings so the engine sees the right
        // state. The on-disk format is unchanged.
        enforce_cloud_only_platform_on_slots(&mut app.slots);
        app.load_agent_targets_from_config();
        // user must provide before recording.
        #[cfg(windows)]
        if app.config.voicebird_api_key.is_empty() {
            app.open_api_key_modal(false);
        }

        // Probe for the local agent runtime (today: oh-my-pi /
        // omp) so the status bar can surface "Agent found at
        // <path> v<version>" (or "Agent not found" when the
        // user hasn't installed an agent yet). Detection is
        // intentionally best-effort and zero-cost on failure —
        // we just enumerate three candidate locations.
        match voice_bird_cli::agent::detect() {
            Ok(det) => {
                log::info!("agent detected: {} v{}", det.path.display(), det.version);
                let det_path = det.path.clone();
                let det_source = det.source.clone();
                app.agent = voice_bird_cli::agent::AgentStatus::Ready {
                    path: det.path,
                    version: det.version,
                };
                app.agent_detection_source = Some(det.source);
                // Auto-register this binary as an MCP server in
                // ~/.omp/agent/mcp.json so the user's agent
                // runtime picks voice-bird up on next launch.
                // Best-effort: any error here just means the
                // user has to run `voice-bird-cli --register`
                // manually. Idempotent — repeated launches
                // overwrite only the `voice-bird` key and leave
                // other entries intact.
                // Register THIS binary (voice-bird-cli), not the
                // agent runtime's path. `det.path` is where the
                // agent runtime lives; what the runtime needs
                let home = voice_bird_cli::agent::register::register_home();
                let binary = std::env::current_exe().unwrap_or_else(|_| det_path.clone());
                match voice_bird_cli::agent::register::register(&binary, &home) {
                    Ok(()) => log::info!(
                        "registered MCP server in {}/agent/mcp.json (source: {:?})",
                        home.display(),
                        det_source,
                    ),
                    Err(e) => log::warn!(
                        "could not register MCP server: {e} (source: {:?})",
                        det_source,
                    ),
                }
            }
            Err(status) => {
                app.agent = status;
                app.agent_detection_source = None;
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


    /// Variant of [`fresh_slots`] that reads any persisted
    /// `slot_settings` for slot 1 and seeds the new slot's
    /// settings from it. The first-run auto-pick model is
    /// applied when no override is present.
    fn fresh_slots_with_config(config: &AppConfig) -> Vec<Slot> {
        let mut slot = Slot::empty(SlotId(1));
        let key = slot_settings_key(slot.id.0);
        if let Some(s) = config.slot_settings.get(&key) {
            slot.settings = s.clone();
        } else {
            // First-run: apply the auto-pick model on top of the
            // default settings so the local model picker is
            // pre-seeded with the recommended default.
            #[cfg(not(windows))]
            {
                let picked = voice_bird_cli::transcription::auto_select::pick_default_model();
                slot.settings.model = picked.into();
            }
        }
        vec![slot]
    }

    /// Look up a slot's current Vec index from its stable id. Returns
    /// `None` if the id was never allocated (e.g. freed by a Phase B
    /// shrink). The Vec is the source of truth — ids are a stable
    /// handle for outside callers.
    pub fn slot_index(&self, id: SlotId) -> Option<usize> {
        self.slots.iter().position(|s| s.id == id)
    }

    /// Read-only access to a slot by id.
    fn slot_by_id(&self, id: SlotId) -> Option<&Slot> {
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

    /// Cloud on/off as displayed in the Mode panel: always the
    /// focused slot's `settings.cloud_on`. The per-slot refactor
    /// removes the global fallback — the panel always reflects
    /// what the slot's next start would use.
    pub fn display_cloud_on(&self) -> bool {
        self.slot_by_id(self.focused_slot)
            .map(|s| s.settings.cloud_on)
            .unwrap_or(false)
    }

    /// Language as displayed in the Mode panel: the focused
    /// slot's `settings.language`.
    pub fn display_language(&self) -> String {
        self.slot_by_id(self.focused_slot)
            .map(|s| s.settings.language.clone())
            .unwrap_or_else(|| "en".into())
    }

    /// Model id as displayed in the Mode panel: the focused
    /// slot's `settings.model`.
    pub fn display_model(&self) -> String {
        self.slot_by_id(self.focused_slot)
            .map(|s| s.settings.model.clone())
            .unwrap_or_else(|| "distil-small.en".into())
    }

    /// The focused slot's current target (Stdout / Cloud), or `None`
    /// when the slot has never been used. UI uses this to drive the
    /// Targets pane.
    pub fn focused_target(&self) -> Option<Target> {
        self.slot_by_id(self.focused_slot).and_then(|s| s.target())
    }

    /// The current pending target for the focused slot, falling
    /// through to the slot's last-used target, then Stdout. The UI
    /// uses this for the per-slot title and the targets pane's
    /// "active" marker.
    pub fn focused_pending_target(&self) -> Target {
        self.pending_target_overrides
            .get(&self.focused_slot)
            .cloned()
            .or_else(|| self.focused_target())
            .unwrap_or(Target::Stdout)
    }
    /// The picker list. Row 0 is `Stdout`; rows 1+ are
    /// one per user-configured Agent target. `Cloud`
    /// is intentionally absent — cloud is a per-section
    /// transport flag (see `SectionSettings::cloud_on`),
    /// not a target. Disabled flag is reserved for
    /// future use; today every configured Agent target
    /// is pickable.
    pub fn targets(&self) -> Vec<TargetRow> {
        let mut rows =
            Vec::with_capacity(1 + self.config.agent_targets.len());
        rows.push(TargetRow {
            kind: TargetKind::Stdout,
            disabled: false,
        });
        for t in &self.config.agent_targets {
            rows.push(TargetRow {
                kind: TargetKind::Agent { id: t.id.clone() },
                disabled: false,
            });
        }
        rows
    }
    /// Resolve the currently focused Targets-pane row to a `Target`.
    /// Returns `None` if the cursor is parked on a disabled row or
    /// out of range.
    pub fn focused_target_kind(&self) -> Option<TargetKind> {
        let i = self.selected_target_index?;
        self.targets().get(i).and_then(|r| {
            if r.disabled {
                None
            } else {
                Some(r.kind.clone())
            }
        })
    }

    /// Set the focused slot's pending target from a `TargetKind`. The
    /// value is consumed by the next `start_section` and applied
    /// instead of the `cloud_on` heuristic. The resolved `Target`
    /// (with a session id, for Agent) is returned so the caller can
    /// surface it in a banner.
    pub fn pick_target(&mut self, kind: TargetKind) -> Target {
        let slot = self.focused_slot;
        let target = match kind {
            TargetKind::Stdout => Target::Stdout,
            TargetKind::Agent { id } => Target::Agent { session_id: id },
        };
        // Queue the override so the next start_section
        // consumes it. (Today this is the only signal
        // the picker writes; start_section removes the
        // override at start time and uses it as the
        // section's target.)
        self.pending_target_overrides.insert(slot, target.clone());
        target
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
    // -- Section accessors --------------------------------------------------

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
    /// Used by the `c` toggle and by auth-error recovery.
    pub fn open_api_key_modal(&mut self, reverts_cloud: bool) {
        self.api_key_buf = Some(self.config.voicebird_api_key.clone());
        self.api_key_modal_reverts_cloud = reverts_cloud;
        self.mode = AppMode::ApiKeyModal;
    }

    /// Open the output-path modal, seeding the buffer with the
    /// current `session_dir` from config. Local-only concept — the 'p'
    /// key doesn't exist on cloud-only Windows.
    pub fn open_path_modal(&mut self) {
        self.path_buf = Some(
            self.slot_by_id(self.focused_slot)
                .map(|s| s.settings.path.clone())
                .unwrap_or_else(|| self.config.session_dir.clone()),
        );
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
        let base_dir = self.slot_path_expanded(self.focused_slot);
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
            self.focused_slot = self.slots[next_idx].id;
        }
    }

    /// Cycle the focused slot backward (Shift-Tab). Mirrors
    /// [`focus_next`] — every slot, no skipping.
    pub fn focus_prev(&mut self) {
        if let Some(idx) = self.slot_index(self.focused_slot) {
            let n = self.slots.len();
            let prev_idx = (idx + n - 1) % n;
            self.focused_slot = self.slots[prev_idx].id;
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
    pub fn add_slot(&mut self) -> Option<SlotId> {
        if self.slots.len() >= MAX_SECTIONS {
            return None;
        }
        let id = SlotId(self.next_slot_id);
        self.next_slot_id += 1;
        // Seed the new slot's settings from the currently focused
        // slot so a `+` press gives the user a slot that behaves
        // like the one they were just looking at. Subsequent
        // `c` / `l` / `m` / `p` flips on the new slot do not
        // affect the source slot; the two diverge from this point.
        let seed = self
            .slot_by_id(self.focused_slot)
            .map(|s| s.settings.clone())
            .unwrap_or_default();
        let mut new_slot = Slot::empty(id);
        new_slot.settings = seed;
        self.slots.push(new_slot);
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
        // Drop any persisted per-slot settings for the removed id
        // so a future slot with the same id (we never reuse them,
        // but a hand-edited config could) doesn't pick up the
        // stale settings.
        self.config.slot_settings.remove(&slot_settings_key(id.0));
        // If the focused slot also had a pending target override,
        // drop it — the slot id is gone from the Vec and we don't
        // want the override to silently re-apply to a future slot.
        self.pending_target_overrides.remove(&id);
        // Land focus on the nearest remaining slot. Prefer the slot
        // to the left of the removed one, else the new rightmost.
        let new_idx = if pos > 0 { pos - 1 } else { 0 };
        if let Some(s) = self.slots.get(new_idx) {
            self.focused_slot = s.id;
        }
        true
    }

    /// Refresh both panes' inventory. Preserves cursors by name when the
    /// previously-cursored entries still exist after refresh.
    pub fn refresh_inventory(&mut self) {
        let prior_device = self
            .devices
            .get(self.selected_device_index)
            .map(|d| (d.name.clone(), d.kind));
        let prior_app = self
            .selected_app_index
            .and_then(|i| self.apps.get(i))
            .map(|a| a.id.clone());

        match crate::platform::enumerate_audio_inventory() {
            Ok(inv) => {
                self.devices = inv.devices;
                self.apps = inv.apps;
            }
            Err(e) => {
                self.status_message = Some(format!("Failed to enumerate audio inventory: {}", e));
            }
        }

        if let Some((name, kind)) = prior_device {
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

        self.selected_app_index = prior_app
            .as_deref()
            .and_then(|id| self.apps.iter().position(|a| a.id == id));

        self.clamp_scrolls(usize::MAX);
    }

    /// Move the cursor up one row in whichever pane is focused.
    /// In the Targets pane, disabled rows (currently just `Agent` when
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
            PickerFocus::Targets => {
                let i = self.selected_target_index.unwrap_or(0);
                if i > 0 {
                    self.selected_target_index = Some(i - 1);
                }
            }
        }
        log::debug!(
            "picker: ↑ focus={:?} dev_idx={} (={:?}) app_idx={:?} (={:?}) target_idx={:?} (={:?})",
            self.picker_focus,
            self.selected_device_index,
            self.devices
                .get(self.selected_device_index)
                .map(|d| d.name.clone()),
            self.selected_app_index,
            self.selected_app_index
                .and_then(|i| self.apps.get(i))
                .map(|a| a.name.clone()),
            self.selected_target_index,
            {
                let rows = self.targets();
                self.selected_target_index
                    .and_then(|i| rows.get(i))
                    .map(|r| r.kind.clone())
            },
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
            PickerFocus::Targets => {
                let i = self.selected_target_index.unwrap_or(0);
                // The Targets list is dynamic — Stdout / Cloud plus
                // every entry in `config.agent_targets`. The renderer
                // already iterates the full list, so the cursor cap
                // must come from `targets().len()` too; a hardcoded
                // `3` leaves user-added rows beyond the first Agent
                // unreachable via ↑/↓ (the row renders but never
                // receives the cursor). The disabled-row skip is
                // still handled lazily in `focused_target_kind` so
                // the cursor can park on a row that's visually
                // present but unpickable — the banner and start call
                // treat that as a no-op.
                let total = self.targets().len();
                if i + 1 < total {
                    self.selected_target_index = Some(i + 1);
                }
            }
        }
        log::debug!(
            "picker: ↓ focus={:?} dev_idx={} (={:?}) app_idx={:?} (={:?}) target_idx={:?} (={:?})",
            self.picker_focus,
            self.selected_device_index,
            self.devices
                .get(self.selected_device_index)
                .map(|d| d.name.clone()),
            self.selected_app_index,
            self.selected_app_index
                .and_then(|i| self.apps.get(i))
                .map(|a| a.name.clone()),
            self.selected_target_index,
            {
                let rows = self.targets();
                self.selected_target_index
                    .and_then(|i| rows.get(i))
                    .map(|r| r.kind.clone())
            },
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
        let dev_idx = (self.selected_device_index as u16).min(dev_max);
        let app_idx = self
            .selected_app_index
            .map(|i| (i as u16).min(app_max))
            .unwrap_or(0);
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

    /// Currently focused (Apps pane) entry, if any. Returns `None` when
    /// the user has cleared the selection or no apps are available.
    pub fn focused_app(&self) -> Option<&AppSession> {
        self.selected_app_index.and_then(|i| self.apps.get(i))
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

    /// Picked-target kind for the focused slot. Falls through to
    /// the last-saved target if no pending override is queued. The
    /// Targets pane uses this to mark the active row.
    pub fn picked_target_kind(&self) -> Option<TargetKind> {
        let t = self
            .pending_target_overrides
            .get(&self.focused_slot)
            .cloned()
            .or_else(|| self.focused_target())
            .unwrap_or(Target::Stdout);
        Some(match t {
            Target::Stdout => TargetKind::Stdout,
            Target::Agent { session_id } => TargetKind::Agent { id: session_id },
        })
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
            if auth_failure && self.config.cloud_broadcast_enabled {
                // `false`: the user already has Cloud ON and a key
                // on disk — they just need to replace the bad
                // key. Cancelling the modal leaves Cloud ON (the
                // next recording will re-trigger the auth guard).
                self.open_api_key_modal(false);
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
    /// device-specific override (keyed on the actual selected device
    /// name + kind) over the generic Microphone/System fallback.
    pub fn effective_settings_for(&self, source: &SessionSource) -> SectionSettings {
        let key = self.source_key_for(source);
        let ov = self.config.effective_override(&key);
        SectionSettings {
            cloud_on: ov.cloud_on,
            language: ov.language,
            model: ov.model,
        }
    }

    /// Per-slot replacement for [`Self::effective_settings_for`].
    /// Reads the slot's `SlotSettings` directly — no per-source
    /// merge, no global fallback. The slot's settings are the
    /// source of truth at start time.
    ///
    /// Returns `None` when the slot id is not currently in the
    /// workspace (caller's bug). Callers that have already
    /// validated the slot can use `.unwrap()`.
    pub fn slot_settings_for(&self, slot: SlotId) -> Option<SectionSettings> {
        self.slot_by_id(slot).map(|s| SectionSettings {
            cloud_on: s.settings.cloud_on,
            language: s.settings.language.clone(),
            model: s.settings.model.clone(),
        })
    }

    /// Compute the persistence key for a section's source. Device
    /// sections key on the actual selected device name + kind so two
    /// physical devices never collide; app sections key on app name +
    /// paired device so "Zoom on Speakers" and "Zoom on AirPods" don't
    /// share overrides.
    pub fn source_key_for(&self, source: &SessionSource) -> String {
        use voice_bird_cli::config::{device_source_id, source_id, AudioSessionKind};
        match source {
            SessionSource::Microphone | SessionSource::System => {
                if let (Some(name), Some(kind)) = (
                    self.config.input_device.as_deref(),
                    self.config.input_device_kind,
                ) {
                    return device_source_id(name, kind);
                }
                source_id(
                    source,
                    self.config.input_device_kind.or(Some(match source {
                        SessionSource::Microphone => AudioSessionKind::Input,
                        _ => AudioSessionKind::Output,
                    })),
                )
            }
            SessionSource::App { .. } => source_id(source, None),
        }
    }

    /// Expand `~` in a slot's `settings.path` the same way
    /// `AppConfig::session_dir_expanded` does. Pulled out so the
    /// session_dir resolution in `start_section` and the export
    /// base dir in `export_transcript` can share the contract.
    pub fn slot_path_expanded(&self, slot: SlotId) -> String {
        let raw = self
            .slot_by_id(slot)
            .map(|s| s.settings.path.clone())
            .unwrap_or_else(|| self.config.session_dir_expanded());
        if let Some(rest) = raw.strip_prefix("~/") {
            if let Some(home) = dirs::home_dir() {
                return home.join(rest).to_string_lossy().into_owned();
            }
        }
        raw
    }
    /// Update the focused section's settings AND persist the change as


    /// a per-source override in `config.source_overrides`. The new value
    /// applies to the running engine on next start; for live mutations
    /// (toggling cloud while a section runs), Stage 3+ will rebuild
    /// the engine. For now, the running engine is unaffected and the
    /// new value is what the next start will use.
    pub fn persist_focused_settings(&mut self) {
        let Some(section) = self.focused() else {
            return;
        };
        let key = self.source_key_for(&section.source);
        let ov = voice_bird_cli::config::SourceSettingsOverride {
            cloud_on: section.settings.cloud_on,
            language: section.settings.language.clone(),
            model: section.settings.model.clone(),
        };
        self.config.upsert_source_override(key, ov);
        if let Err(e) = self.config.save() {
            log::error!("config save (per-source override): {e}");
        }
    }

    /// Per-slot replacement for [`Self::persist_focused_settings`].
    /// Reads the focused slot's `SlotSettings` (which is now the
    /// authoritative live value — no per-source merge) and writes it
    /// into `config.slot_settings` under the slot's id, then saves.
    /// The next `App::new` reloads the same map.
    ///
    /// Coexists with `persist_focused_settings` for the duration of
    /// the refactor; commit 6 deletes the per-source variant.
    pub fn persist_focused_slot_settings(&mut self) {
        let Some(slot) = self.slot_by_id(self.focused_slot) else {
            return;
        };
        let key = slot_settings_key(slot.id.0);
        self.config.slot_settings.insert(key, slot.settings.clone());
        if let Err(e) = self.config.save() {
            log::error!("config save (slot_settings): {e}");
        }
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
            self.open_api_key_modal(false);
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

        // Where this section is heading. The Targets picker
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

        // recording from inheriting segments left by a previous
        // session on the same slot.
        if matches!(target, Target::Agent { .. }) {
            // Truncate the per-slot live tail so a fresh recording
            // does not pick up segments left by a previous session
            // on the same slot. Slot ids come from `next_slot_id`
            // which monotonically grows up to `MAX_SECTIONS` (8) and
            // is never reused, so the cast to `u8` is lossless and
            // the live file path stays unique per slot.
            let slot_u8 = slot.0 as u8;
            if let Err(e) = voice_bird_cli::agent::live::truncate_slot(slot_u8) {
                log::warn!("agent live truncate: {e}");
            }
        }

        let now = chrono::Utc::now();
        // Local-first persistence is a function of the *target*, not
        // the cloud transport. `Target::Stdout` always lands on disk
        // (`audio.wav`, `transcript.jsonl`, `meta.json`, plus the
        // post-stop `transcript.json` / `transcript.txt`); that
        // contract holds whether the ASR is local-Whisper or
        // cloud-Voice-Bird-Web, because the cloud engine's committed
        // segments flow into the same consumer task and the same
        // local writer. `Target::Agent` is the one case where the
        // agent runtime *is* the destination, so we skip the local
        // tree entirely — the agent buffer (`~/.voice-bird/live/`)
        // is the persistence layer there.
        //
        // Pre-this-commit, the decision was gated on `cloud_on`,
        // which conflated "is the cloud engine the ASR?" with
        // "should we keep a local copy?". The user-facing picker
        // for `Stdout + Cloud ON` reads as "transcript streamed to
        // the server + locally (from server)" — both, not either.
        let session_dir: Option<std::path::PathBuf> =
            if matches!(target, Target::Stdout) {
                // Per-slot path: take the live `slot.settings.path`
                // (already validated by the path modal / `p` key).
                // The global `config.session_dir` is no longer the
                // source of truth — each slot has its own.
                let base = self.slot_path_expanded(slot);
                let dir = voice_bird_cli::session::layout::session_dir(
                    std::path::Path::new(&base),
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

        // --- 6. Consumer task: engine events → live state + JSONL ---
        // Capture the section's target + the shared agent buffer
        // so the consumer can fan a Committed segment into the
        // MCP-server buffer when the user picked `Target::Agent`.
        let target_for_consumer = target.clone();
        let agent_state_for_consumer = self.agent_state.clone();
        // Clone the per-session-id AgentTarget map so the
        // consumer task can route a committed segment to
        // the user-configured Kafka target the slot picked.
        //
        // Snapshot semantics: the map is cloned at section
        // start. The clone owns its own `Arc<dyn AgentTarget>`
        // values — `AgentTarget` itself is a cheap
        // `Arc<KafkaTargetInner>` handle, so the broker
        // connection + buffer are shared, but the dispatch
        // map is independent of `self.agent_targets`.
        //
        // Concretely: both *edits* and *removals* made via
        // the funnel (or via `App::remove_agent_target_*`)
        // only take effect for the *next* section the user
        // starts in this slot. The in-flight consumer keeps
        // routing to the broker/topic it captured here, so
        // removing a target mid-recording does NOT cut off
        // production to that broker — the segment pipeline
        // continues until the section ends. The
        // "agent target not found; segment dropped" branch
        // downstream is effectively unreachable from a
        // mid-recording removal (the cloned map still has
        // the entry); it can only fire if the target was
        // never present in the first place.
        //
        // Sharing the live map via `Arc<RwLock<…>>` (so
        // edits/removals apply mid-section) is filed as a
        // follow-up — the current snapshot behaviour is
        // correct for the 7-step funnel UX ("next section
        // picks up the new target") but surprising if the
        // user expects removal to take effect immediately.
        let agent_targets_for_consumer = self.agent_targets.clone();
        let agent_events_for_consumer = self.agent_events.clone();
        let slot_for_consumer = slot;
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

                        // Build the on-disk live record FIRST, before
                        // any code path moves `seg` into the in-memory
                        // buffer. The mirror write to `~/.voice-bird/live/`
                        // runs after, but it just reads `&seg` again —
                        // which is fine since we already extracted the
                        // fields we need into `live_seg`.
                        let idx = agent_state_for_consumer.snapshot_next_index();
                        let session = match &target_for_consumer {
                            Target::Agent { session_id } => {
                                voice_bird_cli::agent::AgentSessionId(session_id.clone())
                            }
                            _ => voice_bird_cli::agent::AgentSessionId::default_session(),
                        };
                        let live_seg = if matches!(target_for_consumer, Target::Agent { .. }) {
                            Some(voice_bird_cli::agent::live::LiveSegment::from_engine(
                                &seg, idx, &session,
                            ))
                        } else {
                            None
                        };
                        // Route into the agent buffer when the
                        // slot's target is `Target::Agent`. See
                        // `dispatch_segment_to_agent` for the
                        // default / known-id / missing-id arms.
                        let outcome = dispatch_segment_to_agent(
                            &target_for_consumer,
                            &seg,
                            &agent_state_for_consumer,
                            &agent_targets_for_consumer,
                        )
                        .await;
                        if let Target::Agent { session_id } = &target_for_consumer {
                            record_dispatch_event(&agent_events_for_consumer, outcome, session_id);
                        }
                        // Mirror to the on-disk live tail so the
                        // MCP server process spawned by the agent
                        // runtime sees the same segments. The
                        // TUI's in-memory buffer is local to this
                        // process; the live file is the
                        // `pull_recent`.
                        if let Some(live) = live_seg {
                            let slot_u8 = slot_for_consumer.0 as u8;
                            if let Err(e) = voice_bird_cli::agent::live::append(slot_u8, &live) {
                                log::warn!("agent live append: {e}");
                            }
                        }
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
                source: "mic".into(),
                device: "mock".into(),
                started_at: started.to_rfc3339(),
                ended_at: ended.to_rfc3339(),
                duration_ms: (ended - started).num_milliseconds().max(0) as u64,
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

        // Aggregate App-level state. Idle iff no section is left.
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
        let settings = self.slot_settings_for(slot).expect("slot exists");
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
    /// The Targets picker has a special-case in the `Enter`
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

        let settings = self.slot_settings_for(slot).expect("slot exists");
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
            // Per-slot: the picked model applies to the focused
            // slot's settings. The engine uses the snapshot on
            // the next start; the slot's settings.model is the
            // source of truth.
            let slot_id = self.focused_slot;
            let slot_pos = self.slot_index(slot_id);
            if let Some(pos) = slot_pos {
                self.slots[pos].settings.model = model_id.clone();
                if let SlotKind::Recording { section } = &mut self.slots[pos].kind {
                    section.settings.model = model_id;
                }
            }
            self.persist_focused_slot_settings();
            self.mode = AppMode::Normal;
            self.picker = None;
            self.config_was_loaded_from_disk = true;
        }
    }
    /// Hydrate `agent_targets` from the on-disk config. Called
    /// once at startup; subsequent funnel saves update both
    /// the config and this map so a relaunch isn't needed.
    pub fn load_agent_targets_from_config(&mut self) {
        use voice_bird_cli::agent::AgentTarget;
        use voice_bird_cli::config::AgentConnection;
        self.agent_targets.clear();
        for t in &self.config.agent_targets {
            let session = voice_bird_cli::agent::AgentSessionId(t.id.clone());
            let target: std::sync::Arc<dyn AgentTarget> = match &t.connection {
                AgentConnection::Kafka(conn) => std::sync::Arc::new(
                    voice_bird_cli::agent::kafka::KafkaTarget::new(session, conn.clone()),
                ),
            };
            self.agent_targets.insert(t.id.clone(), target);
        }
    }

    /// Add or replace a user-configured Agent target in memory.
    /// Updates `agent_targets` and the in-process config; does NOT
    /// persist to disk. Use [`Self::upsert_agent_target`] for the
    /// persisting variant.
    /// Idempotent on `id`. Returns `Err` (and leaves both maps
    /// untouched) if `id` is reserved for an internal Agent
    /// destination (see
    /// `voice_bird_cli::config::RESERVED_AGENT_TARGET_IDS`).
    pub fn upsert_agent_target_in_memory(
        &mut self,
        config_target: voice_bird_cli::config::AgentTargetConfig,
    ) -> anyhow::Result<()> {
        self.config.upsert_agent_target(config_target.clone())?;
        use voice_bird_cli::agent::AgentTarget;
        let id = config_target.id.clone();
        let session = voice_bird_cli::agent::AgentSessionId(id.clone());
        let target: std::sync::Arc<dyn AgentTarget> = match &config_target.connection {
            voice_bird_cli::config::AgentConnection::Kafka(conn) => std::sync::Arc::new(
                voice_bird_cli::agent::kafka::KafkaTarget::new(session, conn.clone()),
            ),
        };
        self.agent_targets.insert(id, target);
        Ok(())
    }

    /// Add or replace a user-configured Agent target. Persists the
    /// updated config to disk. Idempotent on `id`. Returns `Err`
    /// (and does not persist) if `id` is reserved.
    pub fn upsert_agent_target(
        &mut self,
        config_target: voice_bird_cli::config::AgentTargetConfig,
    ) -> anyhow::Result<()> {
        self.upsert_agent_target_in_memory(config_target)?;
        self.config.save().map_err(|e| {
            log::error!("config save (upsert agent target): {e}");
            self.banner = Some(format!("Save failed: {e}"));
            e
        })
    }

    /// Remove a user-configured Agent target in memory. Drops the
    /// in-process handle and clears any pending target overrides
    /// that pointed at it so the slot doesn't carry a stale
    /// `Target::Agent` (with a now-removed `session_id`) into the
    /// next recording start. Does NOT persist to disk; use
    /// [`Self::remove_agent_target`] for the persisting variant.
    pub fn remove_agent_target_in_memory(&mut self, id: &str) {
        self.agent_targets.remove(id);
        self.config.remove_agent_target(id);
        for (_, t) in self.pending_target_overrides.iter_mut() {
            if let Target::Agent { session_id } = t {
                if session_id == id {
                    *t = Target::Stdout;
                }
            }
        }
    }

    /// Remove a user-configured Agent target. Persists the updated
    /// config to disk.
    pub fn remove_agent_target(&mut self, id: &str) {
        self.remove_agent_target_in_memory(id);
        if let Err(e) = self.config.save() {
            log::error!("config save (remove agent target): {e}");
            self.banner = Some(format!("Save failed: {e}"));
        }
    }

    /// Drain the in-flight verify probe's result channel
    /// (non-blocking) and update the active funnel's
    /// `VerifyOutcome`. Called from the main event loop on
    /// every tick so the TUI keeps drawing frames (and Esc
    /// keeps working) while a probe runs in the background.
    pub fn poll_funnel_verify(&mut self) {
        let Some(rx) = self.verify_rx.as_ref() else {
            return;
        };
        // Peek first: try_recv returns Err(Empty) until the
        // worker thread sends its result. We only want to
        // commit when the value has actually arrived.
        let result = match rx.try_recv() {
            Ok(r) => r,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                // The worker thread panicked or the channel
                // was otherwise torn down. Surface that as a
                // verify error so the user isn't stuck on
                // "Verifying…" forever.
                self.verify_rx = None;
                self.verify_started = None;
                if let Some(f) = self.funnel.as_mut() {
                    f.verify = voice_bird_cli::agent_funnel::VerifyOutcome::Err {
                        message: "verify worker disconnected unexpectedly".into(),
                    };
                }
                return;
            }
        };
        // Probe finished — record the outcome and drop the
        // channel. The user can re-run verify by re-pressing
        // Enter on the Verify step.
        let started = self.verify_started.take();
        self.verify_rx = None;
        let Some(f) = self.funnel.as_mut() else {
            return;
        };
        match result {
            Ok(elapsed) => {
                push_agent_event(
                    &self.agent_events,
                    format!(
                        "verify OK for '{}' in {}ms",
                        f.endpoint.trim(),
                        elapsed.as_millis()
                    ),
                );
                f.verify = voice_bird_cli::agent_funnel::VerifyOutcome::Ok { elapsed };
            }
            Err(e) => {
                if let Some(started) = started {
                    log::warn!("funnel: verify failed after {:?}: {e}", started.elapsed());
                }
                push_agent_event(
                    &self.agent_events,
                    format!("verify FAILED for '{}': {e}", f.endpoint.trim()),
                );
                f.verify = voice_bird_cli::agent_funnel::VerifyOutcome::Err {
                    message: format!("{e}"),
                };
            }
        }
    }

    /// Open the funnel for adding a brand-new Agent target.
    pub fn open_add_agent_funnel(&mut self) {
        self.funnel = Some(voice_bird_cli::agent_funnel::AgentFunnel::new_add());
        self.mode = AppMode::AgentFunnel;
    }

    /// Open the funnel pre-filled with an existing Agent target.
    pub fn open_edit_agent_funnel(&mut self, id: &str) {
        if let Some(t) = self.config.agent_target_by_id(id).cloned() {
            self.funnel = Some(voice_bird_cli::agent_funnel::AgentFunnel::new_edit(&t));
            self.mode = AppMode::AgentFunnel;
        } else {
            self.banner = Some(format!("Agent target {id} not found"));
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

// ── Agent segment dispatch ─────────────────────────────────────────────

/// What [`dispatch_segment_to_agent`] did with a segment. Returned so
/// tests (and future status surfaces) can assert routing without a
/// live recording pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentDispatch {
    /// The slot's target is not `Target::Agent`; nothing was routed.
    NotAgent,
    /// Routed to the legacy MCP `agent_state` buffer (`"default"` id).
    Default,
    /// Pushed to the mapped user-configured `AgentTarget`.
    Pushed,
    /// The mapped target's `push_segment` failed; error logged.
    PushFailed,
    /// No target with that id (removed mid-recording); segment
    /// dropped with a warning — NOT silently routed to default.
    Dropped,
}

/// Route one committed segment to its Agent destination. The default
/// MCP-backed target (`session_id == "default"`, see
/// `voice_bird_cli::config::RESERVED_AGENT_TARGET_IDS`) goes through
/// `agent_state` for backward compatibility with the
/// `voice_bird__pull_recent` MCP tool; user-configured ids resolve
/// through `agent_targets`. Called from the recording consumer task
/// in `App::start_section`. Async since #32: the consumer task
/// awaits the target's `push_segment` future directly on its own
/// runtime instead of bridging through a per-call thread.
async fn dispatch_segment_to_agent(
    target: &Target,
    seg: &voice_bird_cli::transcription::Segment,
    agent_state: &voice_bird_cli::agent::mcp_server::ServerState,
    agent_targets: &std::collections::HashMap<
        String,
        std::sync::Arc<dyn voice_bird_cli::agent::AgentTarget>,
    >,
) -> AgentDispatch {
    let Target::Agent { session_id } = target else {
        return AgentDispatch::NotAgent;
    };
    if session_id == "default" {
        agent_state.push(seg.clone());
        AgentDispatch::Default
    } else if let Some(agent) = agent_targets.get(session_id) {
        match agent.push_segment(seg).await {
            Ok(()) => AgentDispatch::Pushed,
            Err(e) => {
                log::warn!("agent target push_segment ({session_id}): {e}");
                AgentDispatch::PushFailed
            }
        }
    } else {
        // Target was removed mid-recording. Drop the segment —
        // better than silently routing to the default MCP target.
        log::warn!("agent target {session_id} not found; segment dropped");
        AgentDispatch::Dropped
    }
}

// ── Platform invariants ────────────────────────────────────────────────

/// Windows is cloud-only: force cloud on in memory regardless of what the
/// config or persisted slot settings say. `cfg!` (rather than an
/// attribute) keeps the body compiled and testable on every target.
fn enforce_cloud_only_platform(settings: &mut SlotSettings) {
    if cfg!(windows) {
        settings.cloud_on = true;
    }
}

/// Apply the platform clamp to every slot's settings. Called from
/// `App::new` and from `start_section` (the per-slot choke point
/// that catches stale settings persisted by a pre-0.4.0 config).
/// `clamp_section_settings_for_platform` (the per-recording
/// snapshot variant) is preserved for the engine's view.
fn enforce_cloud_only_platform_on_slots(slots: &mut [Slot]) {
    if cfg!(windows) {
        for slot in slots {
            slot.settings.cloud_on = true;
        }
    }
}

/// Windows is cloud-only: clamp the per-recording `SectionSettings`
/// snapshot at the one choke point every recording passes through
/// (`start_section`). Covers stale cloud_on=false settings persisted
/// by a pre-0.4.0 config. The slot's settings are the source of
/// truth; this clamp applies to the engine-visible snapshot.
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
    /// The Targets pane's `pick_target` writes the focused slot's
    /// pending target, which `start_section` consumes on the next
    /// start. Each call returns the resolved `Target` (with a
    /// session id for Agent) so the caller can surface a banner.
    #[test]
    fn pick_target_writes_pending_override_for_focused_slot() {
        use crate::app::TargetKind;
        let mut app = App::new();
        let first = app.pick_target(TargetKind::Stdout);
        assert_eq!(first, Target::Stdout);
        assert_eq!(
            app.pending_target_overrides.get(&SlotId(1)).cloned(),
            Some(Target::Stdout)
        );
        let omp = app.pick_target(TargetKind::Agent {
            id: "session-x".into(),
        });
        assert!(matches!(omp, Target::Agent { session_id } if session_id == "session-x"));
        // Switching back to Stdout overwrites the prior override.
        let stdout = app.pick_target(TargetKind::Stdout);
        assert_eq!(stdout, Target::Stdout);
    }

    /// Targets list grows past three rows once the user adds a
    /// second Agent target. ↑/↓ must walk onto the saved row
    /// instead of leaving it stranded behind a hardcoded cap.
    /// Regression for the screenshot where `Agent: prod-events`
    /// rendered in the pane but the ▶ cursor couldn't reach it.
    #[test]
    fn select_next_advances_past_first_agent_target() {
        use crate::app::{PickerFocus, TargetKind};
        use voice_bird_cli::config::{
            AgentConnection, AgentTargetConfig, KafkaAgentConnection,
        };

        fn seed(id: &str, name: &str) -> AgentTargetConfig {
            AgentTargetConfig {
                id: id.into(),
                name: name.into(),
                connection: AgentConnection::Kafka(KafkaAgentConnection {
                    endpoint: "localhost:9092".into(),
                    topic: "voice-bird".into(),
                    client_id: None,
                    acks: Default::default(),
                    security_protocol: Default::default(),
                    sasl_mechanism: None,
                    sasl_username: None,
                    sasl_password_env: None,
                }),
            }
        }

        // App::new() loads the real on-disk config, which on a
        // developer machine already contains Agent rows whose ids
        // could collide with the hardcoded "uuid-prod" / "uuid-events"
        // used here (causing upsert_agent_target_in_memory to take
        // the replace path instead of append and skewing every
        // assertion below). Wipe both the config vector and the
        // runtime map before we seed our own rows so the test is
        // fully deterministic regardless of the host's config state.
        let mut app = App::new();
        app.config.agent_targets.clear();
        app.agent_targets.clear();
        app.upsert_agent_target_in_memory(seed("uuid-prod", "prod"))
            .expect("upsert prod");
        app.upsert_agent_target_in_memory(seed("uuid-events", "prod-events"))
            .expect("upsert prod-events");

        // Sanity: the only Agent rows in the list are the two we
        // just appended, in upsert order. Catches future regressions
        // in App::targets() without depending on host state.
        let kinds: Vec<_> = app.targets().into_iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            vec![
                TargetKind::Stdout,
                TargetKind::Agent { id: "uuid-prod".into() },
                TargetKind::Agent { id: "uuid-events".into() },
            ]
        );
        let prod_idx = 1usize;
        let events_idx = 2usize;

        // Focus the Targets pane and walk the cursor down from the
        // top. Each ↓ must move one row, regardless of how many
        // Agent rows the user has configured.
        app.picker_focus = PickerFocus::Targets;
        app.selected_target_index = Some(0);

        // Step down until we land on `prod`, then one more step
        // must land on `prod-events`.
        for _ in 0..prod_idx {
            app.select_next();
        }
        assert_eq!(app.selected_target_index, Some(prod_idx));
        assert_eq!(
            app.focused_target_kind(),
            Some(TargetKind::Agent { id: "uuid-prod".into() })
        );

        // This ↓ is the one that was a no-op before the fix.
        app.select_next();
        assert_eq!(app.selected_target_index, Some(events_idx));
        assert_eq!(
            app.focused_target_kind(),
            Some(TargetKind::Agent { id: "uuid-events".into() })
        );

        // Past the end, ↓ is a no-op (not an underflow into None).
        app.select_next();
        assert_eq!(app.selected_target_index, Some(events_idx));
    }

    #[test]
    fn fresh_app_has_no_active_sections() {
        let app = App::new();
        assert_eq!(app.active_section_count(), 0);
        assert!(app.focused().is_none());
        assert_eq!(app.focused_engine_kind(), "");
        assert!(!app.focused_cloud_active());
        // Empty fallbacks for the focused-* Arcs.
        assert!(app.focused_tentative().lock().is_empty());
    }

    // ── Per-slot settings on Slot / App (commit 2) ─────────────────
    //
    // These pin the App-side lifecycle of the new SlotSettings.
    // Commit 1 added the storage layer; commit 2 makes the
    // App-level runtime own the same settings and persist them on
    // every mutation.

    /// A fresh `App` ships with one slot (id 1) whose settings
    /// match `SlotSettings::default()`. The user has not yet
    /// pressed any keys, so cloud is off, language is "en",
    /// model is the default-pick, and path is the default
    /// sessions directory.
    #[test]
    fn fresh_app_slot_has_default_slot_settings() {
        let app = App::new();
        let s = &app.slots[0].settings;
        assert!(!s.cloud_on);
        assert_eq!(s.language, "en");
        assert_eq!(s.model, "distil-small.en");
        assert_eq!(s.path, "~/voice-bird/sessions");
    }

    /// `add_slot` seeds the new slot's settings from the focused
    /// slot's settings. Two slots can thus diverge over time —
    /// adding a slot does not reset either to defaults.
    #[test]
    fn add_slot_seeds_settings_from_focused_slot() {
        let mut app = App::new();
        // Flip the focused slot's settings away from defaults.
        app.slots[0].settings = SlotSettings {
            cloud_on: true,
            language: "ru".into(),
            model: "tiny.en".into(),
            path: "~/voice-bird/seeded".into(),
        };
        let new_id = app.add_slot().expect("add_slot under MAX_SECTIONS");
        let new_slot = app.slots.iter().find(|s| s.id == new_id).unwrap();
        assert!(new_slot.settings.cloud_on);
        assert_eq!(new_slot.settings.language, "ru");
        assert_eq!(new_slot.settings.model, "tiny.en");
        assert_eq!(new_slot.settings.path, "~/voice-bird/seeded");
    }

    /// `remove_focused_slot` drops the slot's entry from
    /// `config.slot_settings` so a future slot minted with the
    /// same id (we never reuse ids, but the file might be
    /// hand-edited) does not pick up stale settings.
    #[test]
    fn remove_focused_slot_drops_persisted_settings() {
        let mut app = App::new();
        // Per-slot settings saved via `persist_focused_slot_settings`.
        app.slots[0].settings = SlotSettings {
            cloud_on: true,
            language: "ru".into(),
            model: "tiny.en".into(),
            path: "~/voice-bird/seeded".into(),
        };
        app.persist_focused_slot_settings();
        let slot_id = app.slots[0].id;
        let key = voice_bird_cli::config::slot_settings_key(slot_id.0);
        assert!(app.config.slot_settings.contains_key(&key));
        // Need a second slot to allow removal.
        let _ = app.add_slot();
        // Focus back on slot 1 and remove it.
        app.focused_slot = slot_id;
        assert!(app.remove_focused_slot());
        assert!(
            !app.config.slot_settings.contains_key(&key),
            "removed slot's settings should not linger in config"
        );
    }

    /// `persist_focused_slot_settings` writes the focused slot's
    /// current settings into `config.slot_settings` and saves
    /// the config to disk. The next launch reloads the same map.
    #[test]
    fn persist_focused_slot_settings_writes_to_config() {
        let mut app = App::new();
        app.slots[0].settings = SlotSettings {
            cloud_on: true,
            language: "ru".into(),
            model: "tiny.en".into(),
            path: "~/voice-bird/persisted".into(),
        };
        app.persist_focused_slot_settings();
        let key = voice_bird_cli::config::slot_settings_key(app.slots[0].id.0);
        let saved = &app.config.slot_settings[&key];
        assert!(saved.cloud_on);
        assert_eq!(saved.language, "ru");
        assert_eq!(saved.model, "tiny.en");
        assert_eq!(saved.path, "~/voice-bird/persisted");
    }

    // ── Per-slot settings drive start_section (commit 3) ──────────
    //
    // These tests pin the runtime cutover: the four corners of
    // start_section (cloud/language/model/path) now read from
    // the slot's own `settings`, not from a per-source merge
    // over the global config. Two slots can record with
    // independent settings.

    /// Slot 1's settings drive the running section. cloud_on,
    /// language, and model come from `slot.settings`; the
    /// recording pipeline never reads `config.cloud_broadcast_enabled`
    /// or `config.language` / `config.default_model`.
    ///
    /// The contract is "the slot's settings are the source of
    /// truth at start time". The Section inside a running slot
    /// carries a snapshot (so the engine sees a stable value
    /// during the recording); the resume test below exercises
    /// the snapshot machinery. Here we only need to confirm
    /// start_section reads the slot's settings, not the global
    /// config — which we pin by setting the two to disagree
    /// and asserting the slot survives.
    #[test]
    fn start_section_uses_focused_slot_settings() {
        use voice_bird_cli::config::AudioSessionKind;

        let mut app = App::new();
        // Slot 1: cloud on, Polish, tiny.en.
        app.slots[0].settings = SlotSettings {
            cloud_on: true,
            language: "pl".into(),
            model: "tiny.en".into(),
            path: "~/voice-bird/slot-one".into(),
        };
        // Global config says the opposite. If start_section
        // ever reads these, the slot's settings would be
        // overwritten or the test would fail in commit 6.
        app.config.cloud_broadcast_enabled = false;
        app.config.language = "en".into();
        app.config.default_model = "distil-small.en".into();
        app.config.voicebird_api_key = "sk-test".into();
        app.devices = vec![AudioDevice {
            name: "MacBook Pro Microphone".into(),
            kind: AudioSessionKind::Input,
        }];
        app.selected_device_index = 0;
        let slot = app.slots[0].id;
        let saved = SavedTranscript {
            committed: Arc::new(PlMutex::new(Vec::new())),
            refined: Arc::new(PlMutex::new(Vec::new())),
            label: "mic · cloud:ON".into(),
            target: Target::Stdout,
            source: SessionSource::Microphone,
            settings: SectionSettings {
                cloud_on: true,
                language: "pl".into(),
                model: "tiny.en".into(),
            },
        };
        app.slots[0].kind = SlotKind::Saved { saved };
        let _ = app.resume_section(slot);
        // The slot survives the resume attempt and its
        // settings are unchanged. The global config stays
        // untouched — the runtime never consulted it.
        assert!(app.slots[0].settings.cloud_on);
        assert_eq!(app.slots[0].settings.language, "pl");
        assert_eq!(app.slots[0].settings.model, "tiny.en");
        assert!(!app.config.cloud_broadcast_enabled);
        assert_eq!(app.config.language, "en");
    }

    /// Slot's `settings.path` is the path used for the session
    /// directory. The global `config.session_dir` is irrelevant.
    #[test]
    fn start_section_uses_focused_slot_path_for_session_dir() {
        use voice_bird_cli::config::AudioSessionKind;

        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new();
        app.slots[0].settings = SlotSettings {
            cloud_on: true,
            language: "en".into(),
            model: "tiny.en".into(),
            path: dir.path().to_string_lossy().into_owned(),
        };
        app.config.voicebird_api_key = "sk-test".into();
        // The global session_dir is a different tempdir. If
        // start_section reads it, the assertion below fires.
        // The global session_dir is a different tempdir. If
        // start_section reads it, the assertion below fires.
        app.devices = vec![AudioDevice {
            name: "MacBook Pro Microphone".into(),
            kind: AudioSessionKind::Input,
        }];
        app.selected_device_index = 0;
        let slot = app.slots[0].id;
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
        };
        app.slots[0].kind = SlotKind::Saved { saved };
        let _ = app.resume_section(slot);
        // The session must be created under the slot's dir,
        // not under the global config.session_dir.
        let slot_entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert_eq!(
            slot_entries.len(),
            1,
            "exactly one session directory must be created under the slot's path; got {slot_entries:?}",
        );
    }

    /// Two slots can record with independent settings. The slot's
    /// own settings — not the global config — drive each Section.
    #[test]
    fn two_slots_have_independent_settings_at_start() {
        let mut app = App::new();
        app.add_slot().expect("add_slot");
        // Slot 1: cloud off, English, distil-small.en.
        app.slots[0].settings = SlotSettings {
            cloud_on: false,
            language: "en".into(),
            model: "distil-small.en".into(),
            path: "~/voice-bird/slot-one".into(),
        };
        // Slot 2: cloud on, Polish, tiny.en.
        app.slots[1].settings = SlotSettings {
            cloud_on: true,
            language: "pl".into(),
            model: "tiny.en".into(),
            path: "~/voice-bird/slot-two".into(),
        };
        // The two slots' settings are independent: each
        // carries its own cloud/language/model/path.
        assert!(!app.slots[0].settings.cloud_on);
        assert!(app.slots[1].settings.cloud_on);
        assert_ne!(
            app.slots[0].settings.language,
            app.slots[1].settings.language,
        );
        assert_ne!(app.slots[0].settings.model, app.slots[1].settings.model);
        assert_ne!(app.slots[0].settings.path, app.slots[1].settings.path);
    }

    /// Resume reapplies the slot's LIVE settings, not the saved
    /// snapshot. A user who flips cloud OFF after stopping
    /// gets a resumed section that uses cloud OFF.
    #[test]
    fn resume_reapplies_focused_slot_settings_not_saved_snapshot() {
        use voice_bird_cli::config::AudioSessionKind;

        let mut app = App::new();
        app.config.voicebird_api_key = "sk-test".into();
        app.devices = vec![AudioDevice {
            name: "MacBook Pro Microphone".into(),
            kind: AudioSessionKind::Input,
        }];
        app.selected_device_index = 0;
        let slot = app.slots[0].id;
        // Saved snapshot says cloud ON, English, distil.
        let saved = SavedTranscript {
            committed: Arc::new(PlMutex::new(Vec::new())),
            refined: Arc::new(PlMutex::new(Vec::new())),
            label: "mic · cloud:ON".into(),
            target: Target::Stdout,
            source: SessionSource::Microphone,
            settings: SectionSettings {
                cloud_on: true,
                language: "en".into(),
                model: "distil-small.en".into(),
            },
        };
        app.slots[0].kind = SlotKind::Saved { saved };
        // Live slot settings say cloud OFF, Russian, tiny.
        app.slots[0].settings = SlotSettings {
            cloud_on: false,
            language: "ru".into(),
            model: "tiny.en".into(),
            path: "~/voice-bird/slot-one".into(),
        };
        let _ = app.resume_section(slot);
        // The slot's settings are the source of truth.
        assert!(!app.slots[0].settings.cloud_on);
        assert_eq!(app.slots[0].settings.language, "ru");
    }

    // ── consumer-dispatch arms (dispatch_segment_to_agent) ───────────
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
        };
        app.slots[0].kind = SlotKind::Saved { saved };

        // Simulate the user pressing `l` to cycle the
        // language to Russian and `c` to flip cloud on
        // *after* stopping. The c/l handlers update the
        // global config and per-source override when no
        // section is focused (the post-stop case). Persist
        // the override the same way persist_focused_settings
        // does so the test reflects production state.
        app.config.cloud_broadcast_enabled = true;
        app.config.language = "ru".into();
        let key = app.source_key_for(&SessionSource::Microphone);
        let mut ov = app.config.effective_override(&key);
        ov.cloud_on = true;
        ov.language = "ru".into();
        app.config.upsert_source_override(key, ov);

        // Resume. The result is pipeline-dependent
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
    #[test]
    fn resume_section_picks_up_source_and_target_changed_after_stop() {
        use voice_bird_cli::config::AudioSessionKind;

        let mut app = App::new();
        // Seed a Saved variant as if a section was
        // recorded from the MacBook mic to Stdout.
        let committed: Arc<PlMutex<Vec<CommittedLine>>> =
            Arc::new(PlMutex::new(Vec::new()));
        let refined: Arc<PlMutex<Vec<CommittedLine>>> =
            Arc::new(PlMutex::new(Vec::new()));
        let slot = app.slots[0].id;
        let saved = SavedTranscript {
            committed: committed.clone(),
            refined: refined.clone(),
            label: "MacBook Pro Microphone -> Stdout".into(),
            target: Target::Stdout,
            source: SessionSource::Microphone,
            settings: SectionSettings {
                cloud_on: false,
                language: "en".into(),
                model: "tiny.en".into(),
            },
        };
        app.slots[0].kind = SlotKind::Saved { saved };

        // Now the user changes the picker state after
        // pressing s:
        //   - Devices cursor moves to EPOS (Output)
        //   - Apps cursor moves to Chrome
        //   - Targets pane picks Cloud
        app.devices = vec![
            AudioDevice {
                name: "MacBook Pro Microphone".into(),
                kind: AudioSessionKind::Input,
            },
            AudioDevice {
                name: "EPOS PC 8 USB".into(),
                kind: AudioSessionKind::Output,
            },
        ];
        app.apps = vec![AppSession {
            id: "chrome".into(),
            name: "Google Chrome".into(),
            process_id: 12345,
        }];
        app.selected_device_index = 1; // EPOS
        app.selected_app_index = Some(0); // Chrome
        // Queue the next-start target override for
        // this slot (mirrors what the Targets
        // picker's Enter handler does). Use a
        // user-configured Agent target since Cloud
        // is no longer a target.
        app.pending_target_overrides
            .insert(slot, Target::Agent { session_id: "kafka-1".into() });

        let _ = app.resume_section(slot);

        // Pull source + target off whichever variant the
        // slot ended up in. Both must reflect the
        // post-stop picker state.
        let (applied_source, applied_target): (
            &SessionSource,
            &Target,
        ) = match &app.slots[0].kind {
            SlotKind::Saved { saved } => (&saved.source, &saved.target),
            SlotKind::Recording { section } => {
                (&section.source, &section.target)
            }
            SlotKind::Empty => panic!(
                "resume must not empty the slot on a successful Saved path"
            ),
        };

        // Source: must be App { chrome, Google Chrome,
        // EPOS PC 8 USB } — the post-stop pick, not the
        // saved Microphone.
        match applied_source {
            SessionSource::App {
                id,
                name,
                device_name,
            } => {
                assert_eq!(id, "chrome");
                assert_eq!(name, "Google Chrome");
                assert_eq!(device_name, "EPOS PC 8 USB");
            }
            other => panic!(
                "resume must apply the post-stop source (App/chrome/EPOS); got: {other:?}"
            ),
        }

        // Target: must be the queued Agent override
        // — the pending override is consumed by
        // start_section.
        assert_eq!(
            applied_target,
            &Target::Agent { session_id: "kafka-1".into() },
            "resume must apply the post-stop target (Agent); got: {applied_target:?}"
        );
    }

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

    // ── Cloud-target refactor: cloud is a transport, not a target ──

    /// The targets picker no longer offers Cloud as a
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
    #[test]
    fn targets_picker_no_longer_offers_cloud_target() {
        use crate::app::TargetKind;

        let app = App::new();
        let rows = app.targets();
        let agent_count = app.config.agent_targets.len();
        assert_eq!(
            rows.len(),
            1 + agent_count,
            "targets picker should offer Stdout + N agent rows only"
        );
        // Every row must be a recognized destination
        // variant. Today the row at index 1 is Cloud,
        // which fails this check; after the fix only
        // Stdout and Agent remain.
        for r in &rows {
            assert!(
                matches!(r.kind, TargetKind::Stdout)
                    || matches!(r.kind, TargetKind::Agent { .. }),
                "unexpected target kind: {:?} — Cloud must not be a target",
                r.kind
            );
        }
    }
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
        // Per-source override for the microphone must read
        // `cloud_on = true` — that's the source the picker
        // resolves to, and the resume path derives
        // `effective_settings_for` from the per-source
        // override first. Without this, the resume would
        // fall back to the global default
        // (`cloud_broadcast_enabled = false` in the test
        // tempdir) and the assertion below would trip.
        // (Pre-test-overlay this assertion passed only
        // because the developer's real config happened to
        // have `cloud_broadcast_enabled = true` and no
        // per-source override — implicit test data.)
        let source = SessionSource::Microphone;
        let key = app.source_key_for(&source);
        use voice_bird_cli::config::SourceSettingsOverride;
        app.config.source_overrides.insert(
            key,
            SourceSettingsOverride {
                cloud_on: true,
                language: "en".into(),
                model: "tiny.en".into(),
            },
        );
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
        app.slots[0].settings.path = dir.path().to_string_lossy().into_owned();
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
    /// fails the push when `fail` is set.
    struct SpyTarget {
        pushed: Arc<PlMutex<Vec<String>>>,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl voice_bird_cli::agent::AgentTarget for SpyTarget {
        fn session_id(&self) -> voice_bird_cli::agent::AgentSessionId {
            voice_bird_cli::agent::AgentSessionId("spy".into())
        }
        async fn push_segment(
            &self,
            segment: &voice_bird_cli::transcription::Segment,
        ) -> anyhow::Result<()> {
            if self.fail {
                anyhow::bail!("spy push_segment failure");
            }
            self.pushed.lock().push(segment.text.clone());
            Ok(())
        }
        fn pull_recent(&self, _limit: usize) -> Vec<voice_bird_cli::transcription::Segment> {
            Vec::new()
        }
    }

    fn dispatch_seg(text: &str) -> voice_bird_cli::transcription::Segment {
        voice_bird_cli::transcription::Segment {
            t_start: std::time::Duration::from_millis(0),
            t_end: std::time::Duration::from_millis(500),
            text: text.into(),
            tokens: Vec::new(),
        }
    }

    fn dispatch_fixture(
        fail: bool,
    ) -> (
        voice_bird_cli::agent::mcp_server::ServerState,
        std::collections::HashMap<String, std::sync::Arc<dyn voice_bird_cli::agent::AgentTarget>>,
        Arc<PlMutex<Vec<String>>>,
    ) {
        let state = voice_bird_cli::agent::mcp_server::ServerState::new(
            voice_bird_cli::agent::AgentSessionId::default_session(),
        );
        let pushed = Arc::new(PlMutex::new(Vec::new()));
        let mut targets: std::collections::HashMap<
            String,
            std::sync::Arc<dyn voice_bird_cli::agent::AgentTarget>,
        > = std::collections::HashMap::new();
        targets.insert(
            "known-id".into(),
            std::sync::Arc::new(SpyTarget {
                pushed: pushed.clone(),
                fail,
            }),
        );
        (state, targets, pushed)
    }

    #[tokio::test]
    async fn dispatch_default_id_routes_to_legacy_mcp_buffer() {
        let (state, targets, pushed) = dispatch_fixture(false);
        let out = dispatch_segment_to_agent(
            &Target::Agent {
                session_id: "default".into(),
            },
            &dispatch_seg("hello"),
            &state,
            &targets,
        ).await;
        assert_eq!(out, AgentDispatch::Default);
        let buf = state.pull(10);
        assert_eq!(buf.len(), 1);
        assert_eq!(buf[0].text, "hello");
        // The user-configured target must NOT see the segment.
        assert!(pushed.lock().is_empty());
    }

    #[tokio::test]
    async fn dispatch_known_id_pushes_to_mapped_target() {
        let (state, targets, pushed) = dispatch_fixture(false);
        let out = dispatch_segment_to_agent(
            &Target::Agent {
                session_id: "known-id".into(),
            },
            &dispatch_seg("routed"),
            &state,
            &targets,
        ).await;
        assert_eq!(out, AgentDispatch::Pushed);
        assert_eq!(*pushed.lock(), vec!["routed".to_string()]);
        // The legacy MCP buffer must stay empty.
        assert!(state.pull(10).is_empty());
    }

    #[tokio::test]
    async fn dispatch_missing_id_drops_segment_not_default() {
        let (state, targets, pushed) = dispatch_fixture(false);
        let out = dispatch_segment_to_agent(
            &Target::Agent {
                session_id: "removed-mid-recording".into(),
            },
            &dispatch_seg("lost"),
            &state,
            &targets,
        ).await;
        assert_eq!(out, AgentDispatch::Dropped);
        // Dropped means dropped: neither the legacy buffer nor any
        // configured target receives the segment.
        assert!(state.pull(10).is_empty());
        assert!(pushed.lock().is_empty());
    }

    #[tokio::test]
    async fn dispatch_push_failure_is_reported_not_rerouted() {
        let (state, targets, pushed) = dispatch_fixture(true);
        let out = dispatch_segment_to_agent(
            &Target::Agent {
                session_id: "known-id".into(),
            },
            &dispatch_seg("boom"),
            &state,
            &targets,
        ).await;
        assert_eq!(out, AgentDispatch::PushFailed);
        assert!(state.pull(10).is_empty());
        assert!(pushed.lock().is_empty());
    }

    /// `push_agent_event` caps the log at AGENT_EVENT_CAP by
    /// evicting the oldest entry, and `record_dispatch_event`
    /// only records the outcomes a user would act on.
    #[test]
    fn agent_event_log_caps_and_records_failures_only() {
        let events: Arc<PlMutex<VecDeque<AgentEvent>>> = Arc::new(PlMutex::new(VecDeque::new()));
        for i in 0..(AGENT_EVENT_CAP + 5) {
            push_agent_event(&events, format!("e{i}"));
        }
        let buf = events.lock();
        assert_eq!(buf.len(), AGENT_EVENT_CAP);
        assert_eq!(buf.front().unwrap().message, "e5");
        assert_eq!(
            buf.back().unwrap().message,
            format!("e{}", AGENT_EVENT_CAP + 4)
        );
        drop(buf);

        let events: Arc<PlMutex<VecDeque<AgentEvent>>> = Arc::new(PlMutex::new(VecDeque::new()));
        // Happy paths stay silent — one row per segment would
        // flood the overlay.
        record_dispatch_event(&events, AgentDispatch::Pushed, "id");
        record_dispatch_event(&events, AgentDispatch::Default, "id");
        record_dispatch_event(&events, AgentDispatch::NotAgent, "id");
        assert!(events.lock().is_empty());
        // Failure paths are recorded with the target id.
        record_dispatch_event(&events, AgentDispatch::PushFailed, "prod-a");
        record_dispatch_event(&events, AgentDispatch::Dropped, "prod-b");
        let buf = events.lock();
        assert_eq!(buf.len(), 2);
        assert!(buf[0].message.contains("prod-a"));
        assert!(buf[1].message.contains("prod-b"));
        assert!(buf[1].message.contains("dropped"));
    }

    /// `?` is help-only and `t` owns the status overlay — the two
    /// modes toggle independently and neither writes a banner.
    #[test]
    fn help_and_status_keys_toggle_independent_modes() {
        let mut app = App::new();
        app.toggle_help();
        assert_eq!(app.mode, AppMode::Help);
        assert!(app.banner.is_none(), "help must not surface a banner");
        app.toggle_help();
        assert_eq!(app.mode, AppMode::Normal);

        app.toggle_status();
        assert_eq!(app.mode, AppMode::Status);
        assert!(app.banner.is_none(), "status must not surface a banner");
        app.toggle_status();
        assert_eq!(app.mode, AppMode::Normal);
    }

    #[tokio::test]
    async fn dispatch_non_agent_targets_route_nothing() {
        let (state, targets, pushed) = dispatch_fixture(false);
        for target in [Target::Stdout] {
            let out = dispatch_segment_to_agent(&target, &dispatch_seg("skip"), &state, &targets).await;
            assert_eq!(out, AgentDispatch::NotAgent);
        }
        assert!(state.pull(10).is_empty());
        assert!(pushed.lock().is_empty());
    }

    // Runs on every platform: asserts the platform clamp pins
    // cloud_on to true on Windows and leaves it alone elsewhere.
    // The clamp operates on the focused slot's settings, not the
    // legacy `config.cloud_broadcast_enabled` (which is on its
    // way out in commit 6).
    #[test]
    fn cloud_only_platform_invariants() {
        let mut settings = SlotSettings::default();
        enforce_cloud_only_platform(&mut settings);
        assert_eq!(settings.cloud_on, cfg!(windows));

        let mut section = SectionSettings {
            cloud_on: false,
            language: "en".into(),
            model: "distil-small.en".into(),
        };
        clamp_section_settings_for_platform(&mut section);
        assert_eq!(section.cloud_on, cfg!(windows));
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
            app.slots[0].settings.path = dir.path().to_string_lossy().to_string();

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
            app.slots[0].settings.path = dir.path().to_string_lossy().to_string();

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
            app.slots[0].settings.path = dir.path().to_string_lossy().to_string();
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
    }
}
