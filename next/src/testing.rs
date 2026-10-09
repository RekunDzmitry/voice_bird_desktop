//! Test helpers. Compiled unconditionally (not `#[cfg(test)]`) so the
//! `tests/` integration crate can use them too.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::consumer::UiView;
use crate::db::downloads::CancelCheck;
use crate::download::{DownloadError, Downloader};
use crate::audio_sources::{
    AppTarget, AudioDevice, AudioSourceSnapshot, AudioSourcesCatalog, DeviceKind,
};
use crate::picker::ModelEntry;
use crate::transcription_models::ModelStore;
use crate::ui;
use ratatui::{backend::TestBackend, Terminal};
use tokio::io::AsyncWriteExt;

/// Fixed source snapshot, including the unavailable (`None`) fallback.
#[derive(Debug, Clone)]
pub struct FixtureSources(pub Option<AudioSourceSnapshot>);

impl AudioSourcesCatalog for FixtureSources {
    fn snapshot(&self) -> Option<AudioSourceSnapshot> {
        self.0.clone()
    }
}

pub fn sample_source_snapshot() -> AudioSourceSnapshot {
    AudioSourceSnapshot {
        devices: vec![
            AudioDevice {
                name: "Mic".into(),
                kind: DeviceKind::Input,
            },
            AudioDevice {
                name: "Speakers".into(),
                kind: DeviceKind::Output,
            },
        ],
        apps: vec![
            AppTarget {
                id: "com.apple.Safari".into(),
                name: "Safari".into(),
                pid: 101,
            },
            AppTarget {
                id: "com.spotify.client".into(),
                name: "Spotify".into(),
                pid: 202,
            },
        ],
    }
}

/// Render `ui_view` into a `w`×`h` in-memory terminal and return the cell
/// grid as text, one line per row.
pub fn render_to_string(ui_view: &UiView, w: u16, h: u16) -> String {
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal.draw(|f| ui::render(f, ui_view)).expect("draw");
    let buf = terminal.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..h {
        for x in 0..w {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

/// `CancelCheck` impl for `AtomicBool`. Lets a test pre-arm a cancel
/// (e.g. to verify "no event on cancel" or "fetch short-circuits")
/// by `AtomicBool::store(true, Relaxed)` before calling into the
/// pipeline. Production code uses [`crate::db::downloads::CancelProbe`]
/// instead, which observes the SQLite row.
impl CancelCheck for AtomicBool {
    fn is_cancelled(&mut self) -> bool {
        AtomicBool::load(self, Ordering::Relaxed)
    }
}

/// `CancelCheck` impl for `&AtomicBool`. Same semantics as the owned
/// variant — `Relaxed` ordering is fine because cancellation is a
/// single-writer / single-reader flag with no compound state.
impl CancelCheck for &AtomicBool {
    fn is_cancelled(&mut self) -> bool {
        AtomicBool::load(self, Ordering::Relaxed)
    }
}

/// In-memory `ModelStore` for integration tests. Lets a test declare
/// which models are present (`present: &["tiny.en"]`) and marks installed
/// models present for subsequent availability checks.
pub struct FixtureStore {
    pub root: PathBuf,
    pub present: Mutex<Vec<&'static str>>,
}

impl FixtureStore {
    pub fn new(root: PathBuf, present: &[&'static str]) -> Self {
        Self {
            root,
            present: Mutex::new(present.to_vec()),
        }
    }
}

impl ModelStore for FixtureStore {
    fn is_available(&self, entry: &ModelEntry) -> bool {
        self.present.lock().unwrap().contains(&entry.id)
    }

    fn installed_path(&self, entry: &ModelEntry) -> PathBuf {
        crate::transcription_models::handler_for(entry.format).installed_path(&self.root, entry.id)
    }

    fn staging_path(&self, entry: &ModelEntry, attempt: u32) -> Result<PathBuf, DownloadError> {
        // Per-attempt suffix so two concurrent attempts of the
        // same model write to different paths; mirrors the
        // real handlers' naming.
        Ok(self.root.join(format!("{}.{}.part", entry.id, attempt)))
    }

    fn install(
        &self,
        entry: &ModelEntry,
        staged: &Path,
        cancel: &mut dyn CancelCheck,
    ) -> Result<(), DownloadError> {
        // The fixture install is a Vec push — no filesystem work to
        // interrupt. Honor cancel so a test that wants to verify the
        // "no event on cancel" contract can set the token before the
        // resolver reaches install.
        if cancel.is_cancelled() {
            return Err(DownloadError::Cancelled);
        }
        // Mark the model present so subsequent is_available checks
        // observe the post-install state. The integration test wants
        // to assert the resolver reached `install`.
        self.present.lock().unwrap().push(entry.id);
        // Drop any staged file we created during the test.
        let _ = std::fs::remove_file(staged);
        Ok(())
    }

    fn discard_inflight(&self, entry: &ModelEntry) {
        // Walk every per-attempt artifact that might still be on
        // disk. Attempts are bounded in practice (the store's
        // monotonic counter increments by 1 per Restart); capping
        // at attempt=8 is generous for tests that drive the
        // cancel-immediate-retry path several times in a row.
        for attempt in 1..=8u32 {
            let _ = std::fs::remove_file(self.staging_path(entry, attempt).unwrap());
            let tmp = self.root.join(format!("{}.{}.tmp", entry.id, attempt));
            if tmp.is_dir() {
                let _ = std::fs::remove_dir_all(&tmp);
            }
        }
    }
}

/// A downloader that streams `bytes` in 64 KiB chunks, calling the
/// progress closure each chunk. Tests configure `outcome` to fail
/// (e.g. `Cancelled`) and `delay_ms` to interleave cancellation with
/// progress.
pub struct FixtureDownloader {
    pub bytes: Vec<u8>,
    pub outcome: Outcome,
    pub calls: Arc<AtomicUsize>,
    /// When true, the fetched bytes don't need to match the catalog SHA.
    /// Success tests use this because their synthetic bytes aren't model files.
    pub skip_sha_verify: bool,
    /// Per-chunk delay. Lets a test interleave cancellation before
    /// the task completes. `0` for the fast path.
    pub delay_ms: u64,
}

impl FixtureDownloader {
    pub fn new(bytes: Vec<u8>, outcome: Outcome, calls: Arc<AtomicUsize>) -> Self {
        Self {
            bytes,
            outcome,
            calls,
            skip_sha_verify: true,
            delay_ms: 0,
        }
    }

    /// Construct a downloader that verifies the SHA against `expected_sha`.
    pub fn with_sha_verify(bytes: Vec<u8>, outcome: Outcome, calls: Arc<AtomicUsize>) -> Self {
        Self {
            bytes,
            outcome,
            calls,
            skip_sha_verify: false,
            delay_ms: 0,
        }
    }

    /// Configure the per-chunk delay in milliseconds.
    pub fn with_delay(mut self, delay_ms: u64) -> Self {
        self.delay_ms = delay_ms;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    Cancelled,
    ShaMismatch,
}

#[async_trait::async_trait]
impl Downloader for FixtureDownloader {
    async fn fetch(
        &self,
        _url: &str,
        staged: &Path,
        expected_sha: &str,
        cancel: &mut (dyn CancelCheck + Send),
        progress: &mut (dyn FnMut(u64, Option<u64>) + Send),
    ) -> Result<(), DownloadError> {
        use sha2::{Digest, Sha256};
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.outcome == Outcome::Cancelled || cancel.is_cancelled() {
            let _ = tokio::fs::remove_file(staged).await;
            return Err(DownloadError::Cancelled);
        }
        let mut out = tokio::fs::File::create(staged)
            .await
            .map_err(|e| DownloadError::Io(e.to_string()))?;
        let total = Some(self.bytes.len() as u64);
        let mut hasher = Sha256::new();
        let mut total_read: u64 = 0;
        for chunk in self.bytes.chunks(1 << 16) {
            if self.delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
            }
            if cancel.is_cancelled() {
                drop(out);
                let _ = tokio::fs::remove_file(staged).await;
                return Err(DownloadError::Cancelled);
            }
            out.write_all(chunk)
                .await
                .map_err(|e| DownloadError::Io(e.to_string()))?;
            hasher.update(chunk);
            total_read += chunk.len() as u64;
            progress(total_read, total);
        }
        if cancel.is_cancelled() {
            drop(out);
            let _ = tokio::fs::remove_file(staged).await;
            return Err(DownloadError::Cancelled);
        }
        out.flush()
            .await
            .map_err(|e| DownloadError::Io(e.to_string()))?;
        drop(out);
        let got = hex::encode(hasher.finalize());
        if self.outcome == Outcome::ShaMismatch || (!self.skip_sha_verify && got != expected_sha) {
            let _ = tokio::fs::remove_file(staged).await;
            return Err(DownloadError::Sha256Mismatch {
                got,
                expected: expected_sha.to_string(),
            });
        }
        Ok(())
    }
}

/// Scripted normalized PCM, followed by an idle stream until requested stop.
pub struct FixtureCapture {
    pub sources: Mutex<Vec<crate::audio::CaptureSource>>,
    pub calls: AtomicUsize,
    chunks: Vec<Vec<f32>>,
    failures: Mutex<std::collections::VecDeque<String>>,
}

impl FixtureCapture {
    pub fn new(chunks: Vec<Vec<f32>>) -> Self {
        Self {
            sources: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
            chunks,
            failures: Mutex::new(std::collections::VecDeque::new()),
        }
    }

    pub fn fail_next_open(&self, error: impl Into<String>) {
        self.failures.lock().unwrap().push_back(error.into());
    }
}

impl crate::audio::AudioCapture for FixtureCapture {
    fn open(&self, source: &crate::audio::CaptureSource) -> anyhow::Result<Box<dyn crate::audio::CaptureStream>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.sources.lock().unwrap().push(source.clone());
        if let Some(error) = self.failures.lock().unwrap().pop_front() {
            anyhow::bail!(error);
        }
        let (keepalive, wait) = std::sync::mpsc::channel();
        Ok(Box::new(FixtureStream {
            chunks: self.chunks.clone().into(),
            _keepalive: keepalive,
            wait,
        }))
    }
}

struct FixtureStream {
    chunks: std::collections::VecDeque<Vec<f32>>,
    _keepalive: std::sync::mpsc::Sender<()>,
    wait: std::sync::mpsc::Receiver<()>,
}

impl crate::audio::CaptureStream for FixtureStream {
    fn next_chunk(&mut self, timeout: Duration) -> anyhow::Result<crate::audio::CaptureChunk> {
        if let Some(pcm) = self.chunks.pop_front() {
            return Ok(crate::audio::CaptureChunk::Data(pcm));
        }
        let _ = self.wait.recv_timeout(timeout);
        Ok(crate::audio::CaptureChunk::Timeout)
    }

    fn queued_samples(&self) -> u64 {
        self.chunks.iter().map(|chunk| chunk.len() as u64).sum()
    }
}

/// Separate live/refine outputs at each feed and at finalization.
#[derive(Default)]
pub struct FixtureTranscriberFactory {
    pub live_steps: Vec<Vec<crate::transcribe::TranscriptOut>>,
    pub refine_steps: Vec<Vec<crate::transcribe::TranscriptOut>>,
    pub live_finish: Vec<crate::transcribe::TranscriptOut>,
    pub refine_finish: Vec<crate::transcribe::TranscriptOut>,
}

pub struct FixtureTranscriber {
    steps: std::collections::VecDeque<Vec<crate::transcribe::TranscriptOut>>,
    finish: Vec<crate::transcribe::TranscriptOut>,
}

impl crate::transcribe::TranscriberFactory for FixtureTranscriberFactory {
    fn live(&self, _: &Path, _: &str) -> anyhow::Result<Box<dyn crate::transcribe::Transcriber>> {
        Ok(Box::new(FixtureTranscriber {
            steps: self.live_steps.clone().into(), finish: self.live_finish.clone(),
        }))
    }

    fn refine(&self, _: &Path, _: &str) -> anyhow::Result<Box<dyn crate::transcribe::Transcriber>> {
        Ok(Box::new(FixtureTranscriber {
            steps: self.refine_steps.clone().into(), finish: self.refine_finish.clone(),
        }))
    }
}

impl crate::transcribe::Transcriber for FixtureTranscriber {
    fn feed(&mut self, _: &[f32]) -> anyhow::Result<Vec<crate::transcribe::TranscriptOut>> {
        Ok(self.steps.pop_front().unwrap_or_default())
    }

    fn feed_chunks(&mut self, _: &[crate::audio::buffer::PcmSlice]) -> anyhow::Result<Vec<crate::transcribe::TranscriptOut>> {
        Ok(self.steps.pop_front().unwrap_or_default())
    }

    fn finish(&mut self) -> anyhow::Result<Vec<crate::transcribe::TranscriptOut>> {
        Ok(std::mem::take(&mut self.finish))
    }
}

/// Isolated, idle recording collaborators for download/source lifecycle tests.
pub fn recording_services(root: &Path) -> crate::producer::recording::RecordingServices {
    crate::producer::recording::RecordingServices {
        capture: Arc::new(FixtureCapture::new(Vec::new())),
        engines: Arc::new(FixtureTranscriberFactory::default()),
        sessions_root: root.join("sessions"),
    }
}
