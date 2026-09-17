//! Test helpers. Compiled unconditionally (not `#[cfg(test)]`) so the
//! `tests/` integration crate can use them too.

use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::download::{DownloadError, Downloader};
use crate::picker::ModelEntry;
use crate::state::UiState;
use crate::transcription_models::{handler_for, ModelStore};
use crate::ui;
use ratatui::{backend::TestBackend, Terminal};

/// Render `state` into a `w`×`h` in-memory terminal and return the cell
/// grid as text, one line per row.
pub fn render_to_string(state: &UiState, w: u16, h: u16) -> String {
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal.draw(|f| ui::render(f, state)).expect("draw");
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

/// In-memory `ModelStore` for integration tests. Lets a test declare
/// which models are present (`present: &["tiny.en"]`) and tracks
/// `install` calls so the assertions can confirm what the resolver
/// actually did.
pub struct FixtureStore {
    pub root: PathBuf,
    pub present: Mutex<Vec<&'static str>>,
    pub installed: Mutex<Vec<&'static str>>,
    pub clear_staging_calls: Mutex<Vec<&'static str>>,
}

impl FixtureStore {
    pub fn new(root: PathBuf, present: &[&'static str]) -> Self {
        Self {
            root,
            present: Mutex::new(present.to_vec()),
            installed: Mutex::new(Vec::new()),
            clear_staging_calls: Mutex::new(Vec::new()),
        }
    }
}

impl ModelStore for FixtureStore {
    fn is_available(&self, entry: &ModelEntry) -> bool {
        self.present.lock().unwrap().contains(&entry.id)
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
        _staged: &Path,
        cancel: &Arc<AtomicBool>,
    ) -> Result<(), DownloadError> {
        // The fixture install is a Vec push — no filesystem work to
        // interrupt. Honor cancel so a test that wants to verify the
        // "no event on cancel" contract can set the token before the
        // resolver reaches install.
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(DownloadError::Cancelled);
        }
        // Mark the model present so subsequent is_available checks
        // observe the post-install state. The integration test wants
        // to assert the resolver reached `install`.
        let _ = std::fs::remove_file(self.staging_path(entry, 1).unwrap());
        self.present.lock().unwrap().push(entry.id);
        // Drop any staged file we created during the test.
        let _ = std::fs::remove_file(self.staging_path(entry, 1).unwrap());
        Ok(())
    }

    fn clear_staging(&self, entry: &ModelEntry) {
        self.clear_staging_calls.lock().unwrap().push(entry.id);
        let _ = std::fs::remove_file(self.staging_path(entry, 1).unwrap());
    }

    fn discard_inflight(&self, entry: &ModelEntry) {
        // Record the call so tests can assert Quit-time cleanup ran.
        self.clear_staging_calls.lock().unwrap().push(entry.id);
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
    /// When , the fetched bytes don't need to match
    /// . Tests for the success path need this because
    /// synthetic bytes never hash to the real catalog sha.
    pub skip_sha_verify: bool,
    /// Per-chunk delay. Lets a test interleave cancellation before
    /// the thread completes. `0` for the fast path.
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

    /// Construct a downloader that verifies the sha against .
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

impl Downloader for FixtureDownloader {
    fn fetch(
        &self,
        _url: &str,
        staged: &Path,
        expected_sha: &str,
        cancel: &std::sync::atomic::AtomicBool,
        progress: &mut dyn FnMut(u64, Option<u64>),
    ) -> Result<(), DownloadError> {
        use sha2::{Digest, Sha256};
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.outcome == Outcome::Cancelled {
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        let mut cursor = Cursor::new(&self.bytes);
        let total = Some(self.bytes.len() as u64);
        let mut hasher = Sha256::new();
        let mut buf = [0u8; 1 << 16];
        let mut total_read: u64 = 0;
        loop {
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = std::fs::remove_file(staged);
                return Err(DownloadError::Cancelled);
            }
            if self.delay_ms > 0 {
                std::thread::sleep(Duration::from_millis(self.delay_ms));
            }
            let n = cursor
                .read(&mut buf)
                .map_err(|e| DownloadError::Io(e.to_string()))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            total_read += n as u64;
            progress(total_read, total);
        }
        let got = hex::encode(hasher.finalize());
        if self.outcome == Outcome::ShaMismatch || (!self.skip_sha_verify && got != expected_sha) {
            let _ = std::fs::remove_file(staged);
            return Err(DownloadError::Sha256Mismatch {
                got,
                expected: expected_sha.to_string(),
            });
        }
        Ok(())
    }
}

#[allow(dead_code)]
fn _handler_used(
    f: crate::picker::ModelFormat,
) -> &'static dyn crate::transcription_models::ModelFormatHandler {
    handler_for(f)
}
