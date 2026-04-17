use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use parking_lot::Mutex as PlMutex;

use crate::config::AppConfig;
use crate::platform::AudioSession;
use voice_bird::session::layout::SessionSource;

/// Application running mode
#[derive(Debug, Clone, PartialEq)]
pub enum AppMode {
    Normal,
    ModelPicker, // wired in Stage 3's Task 18
    Help,
}

/// Recording status
#[derive(Debug, Clone)]
pub enum RecordingStatus {
    Idle,
    Recording,
    Error(String),
}

/// A single committed (finalized) transcript line.
pub struct CommittedLine {
    pub t_start_ms: u64,
    pub text: String,
}

/// Handles to the currently running recording pipeline.
pub struct RecordingRuntime {
    pub shutdown_tx: tokio::sync::oneshot::Sender<()>,
    pub join: tokio::task::JoinHandle<()>,
}

/// Main application state
pub struct App {
    /// Current mode
    pub mode: AppMode,

    /// Available audio sessions
    pub sessions: Vec<AudioSession>,

    /// Currently selected index in the session list
    pub selected_index: usize,

    /// Selected sessions for recording (by index)
    pub selected_sessions: Vec<usize>,

    /// Current recording status
    pub status: RecordingStatus,

    /// Real-time audio level (0.0 - 1.0)
    pub audio_level: Arc<Mutex<f32>>,

    /// Recording duration in seconds
    pub duration: f32,

    /// Recording start time
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

    /// Committed (finalized) transcript lines for the current session.
    pub committed: Arc<PlMutex<Vec<CommittedLine>>>,

    /// Latest tentative (in-progress) transcript text.
    pub tentative: Arc<PlMutex<String>>,

    /// Handles for the in-flight recording pipeline (shutdown + join).
    pub runtime: Option<RecordingRuntime>,

    /// On-disk directory for the active session, if any.
    pub session_dir: Option<PathBuf>,

    /// Start-of-session UTC timestamp, if any.
    pub session_started_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl App {
    /// Create a new application instance
    pub fn new() -> Self {
        let config = AppConfig::load().unwrap_or_default();

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime");

        Self {
            mode: AppMode::Normal,
            sessions: Vec::new(),
            selected_index: 0,
            selected_sessions: Vec::new(),
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
            committed: Arc::new(PlMutex::new(Vec::new())),
            tentative: Arc::new(PlMutex::new(String::new())),
            runtime: None,
            session_dir: None,
            session_started_at: None,
        }
    }

    /// Refresh the list of audio sessions
    pub fn refresh_sessions(&mut self) {
        match crate::platform::enumerate_audio_sessions() {
            Ok(sessions) => {
                self.sessions = sessions;
                self.selected_sessions.clear();
                if self.selected_index >= self.sessions.len() {
                    self.selected_index = self.sessions.len().saturating_sub(1);
                }
            }
            Err(e) => {
                self.status_message = Some(format!("Failed to enumerate sessions: {}", e));
            }
        }
    }

    /// Move selection up
    pub fn select_previous(&mut self) {
        if self.selected_index > 0 {
            self.selected_index -= 1;
        }
    }

    /// Move selection down
    pub fn select_next(&mut self) {
        if self.selected_index < self.sessions.len().saturating_sub(1) {
            self.selected_index += 1;
        }
    }

    /// Toggle selection of current item
    pub fn toggle_selection(&mut self) {
        if self.sessions.is_empty() {
            return;
        }

        if let Some(pos) = self.selected_sessions.iter().position(|&i| i == self.selected_index) {
            self.selected_sessions.remove(pos);
        } else {
            self.selected_sessions.push(self.selected_index);
        }
    }

    /// Check if an index is selected
    pub fn is_selected(&self, index: usize) -> bool {
        self.selected_sessions.contains(&index)
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

    /// Toggle help display
    pub fn toggle_help(&mut self) {
        self.mode = if self.mode == AppMode::Help {
            AppMode::Normal
        } else {
            AppMode::Help
        };
    }

    /// Start recording a new session driven by `MockEngine`.
    ///
    /// Stage 3 replaces the mock + tick-driver with real cpal input +
    /// `WhisperRsEngine`. The shape of this function stays the same.
    pub fn start_recording(&mut self, source: SessionSource) {
        let now = chrono::Utc::now();
        let session_dir = voice_bird::session::layout::session_dir(
            std::path::Path::new(&self.config.session_dir_expanded()),
            now,
            &source,
        );
        if let Err(e) = std::fs::create_dir_all(&session_dir) {
            self.status = RecordingStatus::Error(format!("create session dir: {e}"));
            return;
        }

        self.session_dir = Some(session_dir.clone());
        self.session_started_at = Some(now);

        // Clear live state
        self.committed.lock().clear();
        *self.tentative.lock() = String::new();

        // Stub: script a mock event stream so we can see it rendered.
        use voice_bird::transcription::mock::{MockEngine, MockEvent};
        use voice_bird::transcription::TranscriptionEngine;
        let mut engine = MockEngine::new(vec![
            MockEvent::ModelLoaded("mock".into()),
            MockEvent::Tentative("warming up".into()),
            MockEvent::Committed {
                t_start_ms: 0,
                t_end_ms: 1000,
                text: "Welcome to Voice Bird".into(),
            },
        ]);

        let cfg = voice_bird::transcription::EngineConfig {
            model_path: std::path::PathBuf::from("mock"),
            language: None,
            sample_rate: 16_000,
            hop_ms: 750,
            min_window_ms: 1000,
        };

        let handle = match engine.start(cfg) {
            Ok(h) => h,
            Err(e) => {
                self.status = RecordingStatus::Error(format!("engine start: {e}"));
                return;
            }
        };

        let committed = self.committed.clone();
        let tentative = self.tentative.clone();
        let writer_path = session_dir.join("transcript.jsonl");

        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        let mut events_rx = handle.events_rx;
        let pcm_tx = handle.pcm_tx.clone();

        let join = self.rt.spawn(async move {
            let mut writer = match voice_bird::session::writer::SegmentWriter::open(&writer_path) {
                Ok(w) => w,
                Err(e) => {
                    log::error!("writer: {e}");
                    return;
                }
            };

            // Drive mock with dummy PCM ticks so the scripted events fire.
            let pcm_tx2 = pcm_tx.clone();
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(std::time::Duration::from_millis(500));
                loop {
                    ticker.tick().await;
                    if pcm_tx2.send(vec![0.0; 16]).await.is_err() {
                        break;
                    }
                }
            });

            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    evt = events_rx.recv() => match evt {
                        Ok(voice_bird::transcription::EngineEvent::ModelLoaded { name }) => {
                            log::info!("engine loaded: {}", name);
                        }
                        Ok(voice_bird::transcription::EngineEvent::Tentative(s)) => {
                            *tentative.lock() = s;
                        }
                        Ok(voice_bird::transcription::EngineEvent::Committed(seg)) => {
                            let written = (&seg).into();
                            if let Err(e) = writer.append(&written) {
                                log::error!("writer append: {e}");
                                break;
                            }
                            committed.lock().push(CommittedLine {
                                t_start_ms: seg.t_start.as_millis() as u64,
                                text: seg.text,
                            });
                            tentative.lock().clear();
                        }
                        Ok(voice_bird::transcription::EngineEvent::Error(e)) => {
                            log::error!("engine error: {e}");
                            break;
                        }
                        Err(_) => break,
                    }
                }
            }
        });

        self.runtime = Some(RecordingRuntime { shutdown_tx, join });
        self.status = RecordingStatus::Recording;
        self.start_time = Some(std::time::Instant::now());
    }

    /// Stop the active recording and finalize the session files.
    pub fn stop_recording(&mut self) {
        if let Some(rt) = self.runtime.take() {
            let _ = rt.shutdown_tx.send(());
            // best-effort await
            let _ = self.rt.block_on(async move {
                let _ = rt.join.await;
            });
        }

        if let (Some(dir), Some(started)) =
            (self.session_dir.take(), self.session_started_at.take())
        {
            let ended = chrono::Utc::now();
            let meta = voice_bird::session::finalize::SessionMeta {
                version: env!("CARGO_PKG_VERSION").into(),
                // Task 15 replaces this with `self.config.default_model`.
                model: "mock".into(),
                engine: "mock".into(),
                source: "mic".into(),
                device: "mock".into(),
                started_at: started.to_rfc3339(),
                ended_at: ended.to_rfc3339(),
                duration_ms: (ended - started).num_milliseconds().max(0) as u64,
            };
            if let Err(e) = voice_bird::session::finalize::finalize(
                &dir.join("transcript.jsonl"),
                &dir.join("transcript.json"),
                &dir.join("transcript.txt"),
                &dir.join("meta.json"),
                &meta,
            ) {
                log::error!("finalize: {e}");
            }
        }

        self.status = RecordingStatus::Idle;
        self.start_time = None;
        self.duration = 0.0;

        if let Ok(mut level) = self.audio_level.lock() {
            *level = 0.0;
        }
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}
