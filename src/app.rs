use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use parking_lot::Mutex as PlMutex;

use crate::platform::{AppSession, AudioDevice};
use voice_bird_cli::config::AppConfig;
use voice_bird_cli::session::layout::SessionSource;

/// Application running mode
#[derive(Debug, Clone, PartialEq)]
pub enum AppMode {
    Normal,
    ModelPicker, // wired in Stage 3's Task 18
    Help,
    /// Centered overlay prompting the user to paste a Voice Bird API key.
    /// Opened when `c` toggles cloud on with an empty key, or when the
    /// engine reports an auth failure during recording.
    ApiKeyModal,
    /// Centered overlay for editing the session output directory.
    PathModal,
}

/// Languages offered in the cloud-mode language selector. Local mode
/// always transcribes English regardless of this list — the selector
/// is hidden and the engine call is forced to "en".
pub const CLOUD_LANGUAGES: &[&str] = &["en", "es", "fr", "de", "it", "pt", "ja", "zh", "ru", "pl"];

/// Maximum number of parallel recording sections.
pub const MAX_SECTIONS: usize = 3;

/// Recording status
#[derive(Debug, Clone)]
pub enum RecordingStatus {
    Idle,
    Recording,
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
    pub t_end_ms: u64,
    pub text: String,
}

/// Transcript state preserved across a section stop, so the text stays
/// visible in the UI. When the user starts a new section in the same slot,
/// these Arcs are reattached so new segments append to existing content.
pub struct SavedTranscript {
    pub committed: Arc<PlMutex<Vec<CommittedLine>>>,
    pub refined: Arc<PlMutex<Vec<CommittedLine>>>,
    /// Column-title label from the stopped section (e.g. "mic · cloud:OFF").
    pub label: String,
}

/// Handles to a running recording pipeline.
pub struct RecordingRuntime {
    pub join: tokio::task::JoinHandle<()>,
    /// Producer task: pulls cpal frames, resamples, tees to WAV + engine.
    /// Aborted by `stop_recording` so the await on `join` does not hang
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

impl SectionSettings {
    /// Build a section's settings from the current global config. Stage 2
    /// will replace this with `AppConfig::effective_settings(&source)` so
    /// per-source overrides win when present.
    pub fn from_global_config(config: &AppConfig) -> Self {
        Self {
            cloud_on: config.cloud_broadcast_enabled,
            language: config.language.clone(),
            model: config.default_model.clone(),
        }
    }
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
}

/// Which pane the picker arrows / Enter key target. Devices is the
/// physical-input/output column on the left; Apps is the
/// per-application column on the right. Each pane has its own cursor
/// and scroll offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerFocus {
    Devices,
    Apps,
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

    /// Which pane the picker arrows / Enter target.
    pub picker_focus: PickerFocus,

    /// Scroll offset (rows from top) for each pane. The render path
    /// auto-clamps these so the cursor stays visible.
    pub device_scroll: u16,
    pub app_scroll: u16,

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

    /// Parallel recording slots. `Some` slots are actively recording.
    pub sections: [Option<Section>; MAX_SECTIONS],

    /// Slot the c/l/m/Tab keys currently target. Stage 3 will wire
    /// Tab-cycling; Stage 1 keeps this pinned to 0.
    pub focused_section: usize,

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
    /// the modal isn't open. Pre-filled with `config.session_dir`.
    pub path_buf: Option<String>,

    /// Feedback banner for the transcript-export flow ("Exporting…",
    /// "Exported ✓", or error). Displayed above the footer like the
    /// general error banner.
    pub export_banner: Option<String>,

    /// Error banner displayed above the footer. Set when an engine emits
    /// `EngineEvent::Error` (or when the pipeline fails to start); cleared
    /// on the next successful `start_recording`.
    pub banner: Option<String>,

    /// Transcript scroll offset (lines from the top). Only consulted when
    /// `transcript_follow` is false — otherwise the render path pins
    /// the view to the bottom automatically.
    pub transcript_scroll: u16,

    /// When true (default), the transcript auto-scrolls to show the latest
    /// content. Set false when the user manually scrolls up. Pressing
    /// End re-enables it.
    pub transcript_follow: bool,

    /// Per-slot transcript state preserved across section stop/start.
    /// When a section stops, its committed/refined Arcs move here so
    /// the text stays visible. Starting a new section in the same slot
    /// reattaches them — new segments append to existing content.
    pub transcript_saved: [Option<SavedTranscript>; MAX_SECTIONS],

    /// Empty fallback Arcs returned by `focused_*` accessors when no
    /// section is active — keeps the UI render path Arc-shaped without
    /// special-casing the empty state at every read site.
    empty_committed: Arc<PlMutex<Vec<CommittedLine>>>,
    empty_refined: Arc<PlMutex<Vec<CommittedLine>>>,
    empty_tentative: Arc<PlMutex<String>>,
}

impl App {
    /// Create a new application instance
    pub fn new() -> Self {
        let mut config = AppConfig::load().unwrap_or_default();

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

        #[cfg_attr(not(windows), allow(unused_mut))]
        let mut app = Self {
            mode: AppMode::Normal,
            devices: Vec::new(),
            apps: Vec::new(),
            selected_device_index: 0,
            selected_app_index: None,
            picker_focus: PickerFocus::Devices,
            device_scroll: 0,
            app_scroll: 0,
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
            sections: [None, None, None],
            focused_section: 0,
            picker: None,
            config_was_loaded_from_disk,
            api_key_buf: None,
            path_buf: None,
            export_banner: None,
            banner: banner_on_launch,
            transcript_scroll: 0,
            transcript_follow: true,
            transcript_saved: [None, None, None],
            empty_committed: Arc::new(PlMutex::new(Vec::new())),
            empty_refined: Arc::new(PlMutex::new(Vec::new())),
            empty_tentative: Arc::new(PlMutex::new(String::new())),
        };

        // Windows first run (or missing key): land directly in the API-key
        // modal — cloud is the only mode, so the key is the only thing the
        // user must provide before recording.
        #[cfg(windows)]
        if app.config.voicebird_api_key.is_empty() {
            app.open_api_key_modal();
        }

        app
    }

    // -- Section accessors --------------------------------------------------

    /// Currently focused section (the one `c`/`l`/`m`/`s` operate on),
    /// or `None` if no section is running in that slot.
    pub fn focused(&self) -> Option<&Section> {
        self.sections
            .get(self.focused_section)
            .and_then(|s| s.as_ref())
    }

    /// Mutable variant of [`focused`].
    pub fn focused_mut(&mut self) -> Option<&mut Section> {
        self.sections
            .get_mut(self.focused_section)
            .and_then(|s| s.as_mut())
    }

    /// Number of slots currently recording.
    pub fn active_section_count(&self) -> usize {
        self.sections.iter().filter(|s| s.is_some()).count()
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
    /// setting if one is running, else the global config default.
    pub fn display_cloud_on(&self) -> bool {
        self.focused()
            .map(|s| s.settings.cloud_on)
            .unwrap_or(self.config.cloud_broadcast_enabled)
    }

    /// Language as displayed in the Mode panel: focused section's
    /// setting if running, else the global config default.
    pub fn display_language(&self) -> String {
        self.focused()
            .map(|s| s.settings.language.clone())
            .unwrap_or_else(|| self.config.language.clone())
    }

    /// Model id as displayed in the Mode panel: focused section's
    /// setting if running, else the global config default.
    pub fn display_model(&self) -> String {
        self.focused()
            .map(|s| s.settings.model.clone())
            .unwrap_or_else(|| self.config.default_model.clone())
    }

    /// Committed-transcript Arc for the focused section, or saved
    /// transcript, or an empty fallback when nothing is available.
    pub fn focused_committed(&self) -> Arc<PlMutex<Vec<CommittedLine>>> {
        self.focused()
            .map(|s| s.committed.clone())
            .or_else(|| {
                self.transcript_saved
                    .get(self.focused_section)
                    .and_then(|t| t.as_ref().map(|t| t.committed.clone()))
            })
            .unwrap_or_else(|| self.empty_committed.clone())
    }

    pub fn focused_refined(&self) -> Arc<PlMutex<Vec<CommittedLine>>> {
        self.focused()
            .map(|s| s.refined.clone())
            .or_else(|| {
                self.transcript_saved
                    .get(self.focused_section)
                    .and_then(|t| t.as_ref().map(|t| t.refined.clone()))
            })
            .unwrap_or_else(|| self.empty_refined.clone())
    }

    pub fn focused_tentative(&self) -> Arc<PlMutex<String>> {
        self.focused()
            .map(|s| s.tentative.clone())
            .unwrap_or_else(|| self.empty_tentative.clone())
    }

    /// Open the API-key modal, seeding the buffer with whatever key is
    /// currently saved (so backspace can edit it rather than starting
    /// from scratch). Used by the `c` toggle and by auth-error recovery.
    pub fn open_api_key_modal(&mut self) {
        self.api_key_buf = Some(self.config.voicebird_api_key.clone());
        self.mode = AppMode::ApiKeyModal;
    }

    /// Open the output-path modal, seeding the buffer with the
    /// current `session_dir` from config. Local-only concept — the 'p'
    /// key doesn't exist on cloud-only Windows.
    #[cfg(not(windows))]
    pub fn open_path_modal(&mut self) {
        self.path_buf = Some(self.config.session_dir.clone());
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

        let base_dir = self.config.session_dir_expanded();
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

    /// Cycle the focused section forward (Tab). Wraps from slot 2 → 0.
    pub fn focus_next(&mut self) {
        self.focused_section = (self.focused_section + 1) % MAX_SECTIONS;
    }

    /// Cycle the focused section backward (Shift-Tab).
    pub fn focus_prev(&mut self) {
        self.focused_section = (self.focused_section + MAX_SECTIONS - 1) % MAX_SECTIONS;
    }

    /// Pick the lowest-numbered free slot, or `None` if all are running.
    pub fn next_free_slot(&self) -> Option<usize> {
        self.sections.iter().position(|s| s.is_none())
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
        }
        log::debug!(
            "picker: ↑ focus={:?} dev_idx={} (={:?}) app_idx={:?} (={:?})",
            self.picker_focus,
            self.selected_device_index,
            self.devices
                .get(self.selected_device_index)
                .map(|d| (d.name.clone(), d.kind)),
            self.selected_app_index,
            self.selected_app_index
                .and_then(|i| self.apps.get(i))
                .map(|a| a.name.clone()),
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
        }
        log::debug!(
            "picker: ↓ focus={:?} dev_idx={} (={:?}) app_idx={:?} (={:?})",
            self.picker_focus,
            self.selected_device_index,
            self.devices
                .get(self.selected_device_index)
                .map(|d| (d.name.clone(), d.kind)),
            self.selected_app_index,
            self.selected_app_index
                .and_then(|i| self.apps.get(i))
                .map(|a| a.name.clone()),
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

    /// Get current audio level
    pub fn get_audio_level(&self) -> f32 {
        self.audio_level.lock().map(|l| *l).unwrap_or(0.0)
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
        for slot in self.sections.iter() {
            if let Some(section) = slot.as_ref() {
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

    /// Public start hook used by the existing key handler. Resolves the
    /// effective per-source settings (override if saved, else globals)
    /// and routes to `start_section` on the focused slot. Failures land
    /// in `banner` + `status` so the UI can surface them.
    pub fn start_recording(&mut self, source: SessionSource) {
        let settings = self.effective_settings_for(&source);
        let slot = self.focused_section;
        if let Err(msg) = self.start_section(slot, source, settings) {
            self.banner = Some(msg.clone());
            self.status = RecordingStatus::Error(msg);
        }
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

    /// Start recording a section in `slot` with explicit settings.
    /// Returns `Err(message)` for failures the UI should surface as a
    /// banner; `Ok(())` once the producer/consumer tasks are spawned.
    ///
    /// Pre-flight checks (missing API key) may also call
    /// `open_api_key_modal` — those return `Err` so the caller can
    /// avoid clobbering the modal with a banner.
    pub fn start_section(
        &mut self,
        slot: usize,
        source: SessionSource,
        mut settings: SectionSettings,
    ) -> Result<(), String> {
        clamp_section_settings_for_platform(&mut settings);

        if slot >= MAX_SECTIONS {
            return Err(format!("invalid section slot: {slot}"));
        }
        if self.sections[slot].is_some() {
            return Err(format!("section slot {slot} already running"));
        }

        if settings.cloud_on && self.config.voicebird_api_key.is_empty() {
            self.banner = Some("Cloud is on but no API key — press 'c' to paste one".into());
            self.status = RecordingStatus::Error("no api key".into());
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

        let now = chrono::Utc::now();
        // Local-first persistence: when broadcasting, the recording lives
        // entirely on voicebird.app and we skip creating a local session
        // directory. `session_dir` stays None, which finalize-on-stop checks.
        let session_dir: Option<std::path::PathBuf> = if settings.cloud_on {
            None
        } else {
            let dir = voice_bird_cli::session::layout::session_dir(
                std::path::Path::new(&self.config.session_dir_expanded()),
                now,
                &source,
            );
            if let Err(e) = std::fs::create_dir_all(&dir) {
                return Err(format!("create session dir: {e}"));
            }
            Some(dir)
        };

        // Per-section live state. Reattach preserved transcript if the
        // slot had one from a prior stop; otherwise start fresh.
        let (committed, refined) = if let Some(saved) = self.transcript_saved[slot].take() {
            (saved.committed, saved.refined)
        } else {
            (
                Arc::new(PlMutex::new(Vec::new())),
                Arc::new(PlMutex::new(Vec::new())),
            )
        };
        let tentative: Arc<PlMutex<String>> = Arc::new(PlMutex::new(String::new()));
        let engine_error_channel: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

        // Reset focused-section transcript scroll when the focused slot starts.
        if slot == self.focused_section {
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

        // --- 6. Consumer task: engine events → live state + JSONL ---------
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
                            t_end_ms: elapsed_ms,
                            text: seg.text,
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
                                t_end_ms: seg.t_end.as_millis() as u64,
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
        };
        self.sections[slot] = Some(section);

        // Aggregate App-level recording state. With multiple sections,
        // start_time tracks the EARLIEST active section's start.
        self.status = RecordingStatus::Recording;
        let now_inst = std::time::Instant::now();
        self.start_time = Some(match self.start_time {
            Some(prev) if prev < now_inst => prev,
            _ => now_inst,
        });
        Ok(())
    }

    /// Stop the active recording in `slot` and finalize its session files.
    /// No-op if the slot is empty.
    pub fn stop_section(&mut self, slot: usize) {
        log::info!("stop_section[{slot}]: entered");
        if slot >= MAX_SECTIONS {
            log::warn!("stop_section[{slot}]: invalid slot, refusing");
            return;
        }
        // Preserve the transcript before taking the section so the text
        // stays visible in the UI after stop.
        if let Some(section) = self.sections[slot].as_ref() {
            let label = section_column_label(slot, Some(section));
            self.transcript_saved[slot] = Some(SavedTranscript {
                committed: section.committed.clone(),
                refined: section.refined.clone(),
                label,
            });
        }

        let Some(section) = self.sections[slot].take() else {
            log::info!("stop_section[{slot}]: slot was empty (no-op)");
            return;
        };
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
    pub fn clear_slot_transcript(&mut self, slot: usize) {
        self.transcript_saved[slot] = None;
        if let Some(section) = self.sections[slot].as_ref() {
            section.committed.lock().clear();
            section.refined.lock().clear();
            section.tentative.lock().clear();
        }
    }

    /// Stop the focused section. Backwards-compatible shim for callers
    /// that haven't been updated to the per-section API yet.
    pub fn stop_recording(&mut self) {
        let slot = self.focused_section;
        self.stop_section(slot);
    }

    /// Stop every active section. Used at quit.
    pub fn stop_all_sections(&mut self) {
        for slot in 0..MAX_SECTIONS {
            if self.sections[slot].is_some() {
                self.stop_section(slot);
            }
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
            // If a section is focused, treat the picked model as a
            // per-source override for that section (applies to its
            // next start). Otherwise — idle path — update the global
            // default so all unconfigured sources inherit it.
            if self.focused().is_some() {
                if let Some(section) = self.focused_mut() {
                    section.settings.model = model_id;
                }
                self.persist_focused_settings();
            } else {
                self.config.default_model = model_id;
                if let Err(e) = self.config.save() {
                    log::error!("config save: {e}");
                    let mut g = progress_arc.lock();
                    g.error = Some(format!("config save: {e}"));
                    return;
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
pub fn section_column_label(slot: usize, section: Option<&Section>) -> String {
    let n = slot + 1;
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
        config.cloud_broadcast_enabled = true;
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

    // ── existing generic tests ───────────────────────────────────────

    #[test]
    fn auth_error_detection_matches_common_phrases() {
        assert!(looks_like_auth_error("Unauthorized"));
        assert!(looks_like_auth_error("invalid API key"));
        assert!(looks_like_auth_error("InitSuccess: false — bad key"));
        assert!(looks_like_auth_error("forbidden"));
        assert!(!looks_like_auth_error("connection reset by peer"));
        assert!(!looks_like_auth_error("audio format unsupported"));
    }

    #[test]
    fn fresh_app_has_no_active_sections() {
        let app = App::new();
        assert_eq!(app.active_section_count(), 0);
        assert!(app.focused().is_none());
        assert_eq!(app.focused_engine_kind(), "");
        assert!(!app.focused_cloud_active());
        // Empty fallbacks for the focused-* Arcs.
        assert!(app.focused_committed().lock().is_empty());
        assert!(app.focused_refined().lock().is_empty());
        assert!(app.focused_tentative().lock().is_empty());
    }

    #[test]
    fn settings_from_global_config_snapshots_relevant_fields() {
        let mut config = AppConfig::default();
        config.cloud_broadcast_enabled = true;
        config.language = "ru".into();
        config.default_model = "tiny.en".into();
        let s = SectionSettings::from_global_config(&config);
        assert!(s.cloud_on);
        assert_eq!(s.language, "ru");
        assert_eq!(s.model, "tiny.en");
    }

    // Runs on every platform: asserts the forcing on Windows and the
    // no-op everywhere else, since the helpers branch on cfg! at runtime.
    #[test]
    fn cloud_only_platform_invariants() {
        let mut config = AppConfig::default();
        config.cloud_broadcast_enabled = false;
        enforce_cloud_only_platform(&mut config);
        assert_eq!(config.cloud_broadcast_enabled, cfg!(windows));

        let mut settings = SectionSettings::from_global_config(&config);
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
        app.config.session_dir = dir.path().to_string_lossy().to_string();

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
        app.config.session_dir = dir.path().to_string_lossy().to_string();

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
        app.config.session_dir = dir.path().to_string_lossy().to_string();
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
