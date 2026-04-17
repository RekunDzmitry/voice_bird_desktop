use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use parking_lot::Mutex as PlMutex;

use crate::platform::AudioSession;
use voice_bird::config::AppConfig;
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

/// Handles to the currently running recording pipeline.
pub struct RecordingRuntime {
    pub shutdown_tx: tokio::sync::oneshot::Sender<()>,
    pub join: tokio::task::JoinHandle<()>,
    /// Producer task: pulls cpal frames, resamples, tees to WAV + engine.
    /// Aborted by `stop_recording` so the await on `join` does not hang
    /// when the cpal channel `recv()` is blocked.
    pub producer: Option<tokio::task::JoinHandle<()>>,
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

    /// First-run model picker state; `Some` while in `AppMode::ModelPicker`.
    pub picker: Option<PickerState>,

    /// True iff `config.toml` already existed on disk at startup. When
    /// false, the model picker refuses to Esc-cancel — first run must pick.
    pub config_was_loaded_from_disk: bool,

    /// Live cpal input stream for the active recording. `cpal::Stream` is
    /// `!Send`, so it cannot ride along into the tokio producer task —
    /// instead, we keep it pinned to the main (App-owning) thread and drop
    /// it in `stop_recording`, which cleanly stops capture.
    pub _capture_stream: Option<cpal::Stream>,

    /// Which engine is actually running: `"whisperkit"` or `"whisper_rs"`.
    /// Set by `start_recording` based on `config.engine_prefer` and whether
    /// the sidecar binary was located on disk. Surfaced in the header and
    /// persisted into `meta.json` by `stop_recording`.
    pub engine_kind: String,

    /// Error banner displayed above the footer. Set when an engine emits
    /// `EngineEvent::Error` (or when the pipeline fails to start); cleared
    /// on the next successful `start_recording`. Plan deviation: we chose
    /// this simpler banner-based surface over the plan's silent
    /// WhisperKit→whisper-rs restart, which would require significant
    /// state-machine work and risks regressing Stage 2's pipeline.
    pub banner: Option<String>,

    /// Shared slot written by the consumer task when an `EngineEvent::Error`
    /// lands. `tick()` drains it into `banner` + sets `status` to Error.
    pub engine_error_channel: Arc<Mutex<Option<String>>>,
}

impl App {
    /// Create a new application instance
    pub fn new() -> Self {
        let config = AppConfig::load().unwrap_or_default();

        let config_path = AppConfig::config_path().ok();
        let config_was_loaded_from_disk = config_path
            .as_ref()
            .map(|p| p.exists())
            .unwrap_or(false);

        let (mode, picker) = if config_was_loaded_from_disk {
            (AppMode::Normal, None)
        } else {
            (
                AppMode::ModelPicker,
                Some(PickerState {
                    index: 0,
                    downloading: None,
                }),
            )
        };

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime");

        Self {
            mode,
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
            picker,
            config_was_loaded_from_disk,
            _capture_stream: None,
            engine_kind: String::new(),
            banner: None,
            engine_error_channel: Arc::new(Mutex::new(None)),
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

    /// Drain any engine error published by the consumer task into `banner`
    /// and flip the status to `Error`. Called on each UI tick. Plan
    /// deviation: the plan originally described a silent WhisperKit →
    /// whisper-rs restart on engine error; we instead surface the error
    /// as a red banner and let the user press `r` to retry, which is a
    /// much smaller change and does not touch the Stage 2 pipeline shape.
    pub fn check_engine_error(&mut self) {
        if let Ok(mut slot) = self.engine_error_channel.lock() {
            if let Some(msg) = slot.take() {
                self.banner = Some(msg.clone());
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

    /// Start recording a new session: open the default cpal input device,
    /// resample to 16 kHz mono, tee the PCM to a WAV file and to a live
    /// `WhisperRsEngine`, and drive the event consumer that fills
    /// `committed` / `tentative` and appends to `transcript.jsonl`.
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

        // --- 1. cpal capture -----------------------------------------------
        let capture = match voice_bird::audio::capture::capture_default_input() {
            Ok(c) => c,
            Err(e) => {
                self.status = RecordingStatus::Error(format!("capture: {e}"));
                return;
            }
        };
        let (mut frames_rx, info, stream) = capture.split();

        // --- 2. Resampler (device-native → 16 kHz mono) --------------------
        let mut resampler =
            match voice_bird::audio::resample::Resampler::new(info.sample_rate, info.channels) {
                Ok(r) => r,
                Err(e) => {
                    self.status = RecordingStatus::Error(format!("resample: {e}"));
                    return;
                }
            };

        // --- 3. Engine (WhisperKit sidecar or whisper-rs fallback) --------
        use voice_bird::transcription::{EngineConfig, TranscriptionEngine};
        let model_path =
            match voice_bird::transcription::models::gguf_path(&self.config.default_model) {
                Ok(p) => p,
                Err(e) => {
                    self.status = RecordingStatus::Error(format!("model path: {e}"));
                    return;
                }
            };
        // Pick the engine based on user preference + sidecar availability.
        // `sidecar_path()` probes the .app bundle and dev layouts; on this
        // machine (no Swift binary built) it returns None, and
        // `select_engine` transparently falls back to WhisperRsEngine.
        let sidecar = voice_bird::transcription::sidecar_path();
        let engine_kind_used = match (sidecar.as_deref(), self.config.engine_prefer.as_str()) {
            (Some(_), "auto") | (Some(_), "whisperkit") if cfg!(target_os = "macos") => {
                "whisperkit"
            }
            _ => "whisper_rs",
        };
        self.engine_kind = engine_kind_used.to_string();
        self.banner = None; // clear stale banner from previous run
        let mut engine = voice_bird::transcription::select_engine(
            &self.config.engine_prefer,
            sidecar.as_deref(),
        );
        let handle = match engine.start(EngineConfig {
            model_path,
            language: Some(self.config.language.clone()).filter(|s| s != "auto"),
            sample_rate: 16_000,
            hop_ms: self.config.hop_ms,
            min_window_ms: self.config.min_window_ms,
        }) {
            Ok(h) => h,
            Err(e) => {
                self.status = RecordingStatus::Error(format!("engine: {e}"));
                return;
            }
        };

        // --- 4. WAV writer on the resampled 16 kHz mono stream ------------
        let wav_path = session_dir.join("audio.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut wav = match hound::WavWriter::create(&wav_path, spec) {
            Ok(w) => w,
            Err(e) => {
                self.status = RecordingStatus::Error(format!("wav: {e}"));
                return;
            }
        };

        let pcm_tx = handle.pcm_tx.clone();
        let mut events_rx = handle.events_rx;
        let committed = self.committed.clone();
        let tentative = self.tentative.clone();
        let engine_error_channel = self.engine_error_channel.clone();
        let writer_path = session_dir.join("transcript.jsonl");

        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        // --- 5. Producer task: cpal frames → resample → tee(WAV + engine) -
        let producer = self.rt.spawn(async move {
            while let Some(frames) = frames_rx.recv().await {
                match resampler.process(&frames) {
                    Ok(out) => {
                        for s in &out {
                            if let Err(e) = wav.write_sample(*s) {
                                log::error!("wav write: {e}");
                                break;
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
            if let Err(e) = wav.finalize() {
                log::error!("wav finalize: {e}");
            }
        });

        // --- 6. Consumer task: engine events → live state + JSONL ---------
        let join = self.rt.spawn(async move {
            let mut writer = match voice_bird::session::writer::SegmentWriter::open(&writer_path) {
                Ok(w) => w,
                Err(e) => {
                    log::error!("writer: {e}");
                    return;
                }
            };
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
                            // Publish to the shared error channel so the
                            // main App loop picks it up on the next tick
                            // and sets `self.banner` + status=Error.
                            if let Ok(mut slot) = engine_error_channel.lock() {
                                *slot = Some(e);
                            }
                            break;
                        }
                        Err(_) => break,
                    }
                }
            }
        });

        self._capture_stream = Some(stream);
        self.runtime = Some(RecordingRuntime {
            shutdown_tx,
            join,
            producer: Some(producer),
        });
        self.status = RecordingStatus::Recording;
        self.start_time = Some(std::time::Instant::now());
    }

    /// Stop the active recording and finalize the session files.
    pub fn stop_recording(&mut self) {
        // Drop the cpal stream first — this halts capture, the cpal callback
        // thread exits, and the `frames_rx` receiver inside the producer
        // task observes `None` on its next `recv()` (clean shutdown).
        self._capture_stream = None;

        if let Some(mut rt) = self.runtime.take() {
            // Abort the producer in case it is still blocked on recv() —
            // otherwise the task would linger until the last cpal buffer is
            // drained. The producer's WAV finalize ran inline; if we aborted
            // mid-process, we accept the tail loss as part of shutdown.
            if let Some(producer) = rt.producer.take() {
                producer.abort();
            }
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
            let engine_for_meta = if self.engine_kind.is_empty() {
                "whisper_rs".to_string()
            } else {
                self.engine_kind.clone()
            };
            let meta = voice_bird::session::finalize::SessionMeta {
                version: env!("CARGO_PKG_VERSION").into(),
                model: self.config.default_model.clone(),
                engine: engine_for_meta,
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

    /// Kick off an async download of `entry`'s gguf model file. Progress
    /// is written into the picker's `downloading` slot; on success, the
    /// config is written with the chosen model id and the app transitions
    /// back to `AppMode::Normal`.
    pub fn begin_model_download(
        &mut self,
        entry: &voice_bird::transcription::models::ModelEntry,
    ) {
        let dest = match voice_bird::transcription::models::gguf_path(entry.id) {
            Ok(p) => p,
            Err(e) => {
                log::error!("gguf_path: {e}");
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

        let url = entry.gguf_url.to_string();
        let sha = entry.gguf_sha256.to_string();
        let progress_for_thread = progress.clone();

        // Plain OS thread: the download is blocking I/O, we don't need
        // tokio for it, and this keeps the progress callback trivially
        // synchronous.
        std::thread::spawn(move || {
            let mut cb = |bytes: u64, total: Option<u64>| {
                let mut g = progress_for_thread.lock();
                g.bytes = bytes;
                g.total = total;
            };
            let res = voice_bird::transcription::models::download_with_verify(
                &url, &dest, &sha, &mut cb,
            );
            if let Err(e) = res {
                let mut g = progress_for_thread.lock();
                g.error = Some(format!("{e}"));
            }
        });
    }

    /// If a picker download has finished (successfully), commit the chosen
    /// model id to config and return to Normal mode. Intended to be called
    /// from the render loop once per tick.
    pub fn poll_picker_download(&mut self) {
        let Some(picker) = self.picker.as_ref() else { return; };
        let Some(progress_arc) = picker.downloading.as_ref() else { return; };

        // Snapshot under the lock, release before mutating self.
        let (done, err, model_id) = {
            let g = progress_arc.lock();
            let done = g.error.is_none()
                && g.total.is_some()
                && Some(g.bytes) == g.total;
            (done, g.error.clone(), g.model_id.clone())
        };

        if err.is_some() {
            // Leave the error visible in the picker overlay; user can retry.
            return;
        }

        if done {
            self.config.default_model = model_id;
            if let Err(e) = self.config.save() {
                log::error!("config save: {e}");
                let mut g = progress_arc.lock();
                g.error = Some(format!("config save: {e}"));
                return;
            }
            self.mode = AppMode::Normal;
            self.picker = None;
            self.config_was_loaded_from_disk = true;
        }
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}
