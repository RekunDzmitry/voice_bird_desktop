//! Download transport and orchestration.

use std::fmt;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::thread::JoinHandle;

use crate::bus::{AppEvent, EventSender};
use crate::db::downloads::{CancelCheck, CancelProbe, Claim, Downloads};
use crate::picker::ModelEntry;
use crate::transcription_models::ModelStore;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadError {
    NoCacheDir,
    Io(String),
    Http(String),
    Status(u16),
    Sha256Mismatch { got: String, expected: String },
    Install(String),
    Cancelled,
}

impl fmt::Display for DownloadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DownloadError::NoCacheDir => f.write_str("no cache directory available"),
            DownloadError::Io(s) => write!(f, "io: {s}"),
            DownloadError::Http(s) => write!(f, "http: {s}"),
            DownloadError::Status(code) => write!(f, "HTTP {code}"),
            DownloadError::Sha256Mismatch { got, expected } => {
                write!(f, "sha256 mismatch (got {got}, expected {expected})")
            }
            DownloadError::Install(s) => write!(f, "install: {s}"),
            DownloadError::Cancelled => f.write_str("cancelled"),
        }
    }
}

impl std::error::Error for DownloadError {}

pub fn truncate_error(s: &str) -> String {
    const MAX: usize = 160;
    if s.chars().count() <= MAX {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(MAX).collect();
        format!("{truncated}…")
    }
}

pub trait Downloader: Send + Sync + 'static {
    fn fetch(
        &self,
        url: &str,
        staged: &Path,
        expected_sha: &str,
        cancel: &mut dyn CancelCheck,
        progress: &mut dyn FnMut(u64, Option<u64>),
    ) -> Result<(), DownloadError>;
}

pub fn stream_to<R: std::io::Read>(
    mut src: R,
    staged: &Path,
    expected_sha: &str,
    cancel: &mut dyn CancelCheck,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<(), DownloadError> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    let mut out = fs::File::create(staged).map_err(|e| DownloadError::Io(e.to_string()))?;
    let mut buf = [0u8; 1 << 16];
    let mut total_read: u64 = 0;
    loop {
        if cancel.is_cancelled() {
            let _ = fs::remove_file(staged);
            return Err(DownloadError::Cancelled);
        }
        let n = src
            .read(&mut buf)
            .map_err(|e| DownloadError::Io(e.to_string()))?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])
            .map_err(|e| DownloadError::Io(e.to_string()))?;
        hasher.update(&buf[..n]);
        total_read += n as u64;
        progress(total_read, None);
    }
    drop(out);
    let got = hex::encode(hasher.finalize());
    if got != expected_sha {
        let _ = fs::remove_file(staged);
        return Err(DownloadError::Sha256Mismatch {
            got,
            expected: expected_sha.to_string(),
        });
    }
    Ok(())
}

#[cfg(feature = "net")]
pub struct HttpDownloader;

#[cfg(feature = "net")]
impl Downloader for HttpDownloader {
    fn fetch(
        &self,
        url: &str,
        staged: &Path,
        expected_sha: &str,
        cancel: &mut dyn CancelCheck,
        progress: &mut dyn FnMut(u64, Option<u64>),
    ) -> Result<(), DownloadError> {
        let resp = reqwest::blocking::get(url).map_err(|e| DownloadError::Http(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(DownloadError::Status(status.as_u16()));
        }
        stream_to(resp, staged, expected_sha, cancel, progress)
    }
}

pub struct Throttle {
    last_pct: i32,
    last_bytes: u64,
    last_total: Option<u64>,
    last_emit_ms: u128,
    last_emit_bytes: u64,
}

impl Throttle {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Default for Throttle {
    fn default() -> Self {
        Self {
            last_pct: -1,
            last_bytes: 0,
            last_total: None,
            last_emit_ms: 0,
            last_emit_bytes: 0,
        }
    }
}
#[allow(dead_code)]
impl Throttle {
    pub fn call(
        &mut self,
        attempt: u32,
        bytes: u64,
        total: Option<u64>,
        tx: &EventSender,
        model: &'static str,
    ) {
        let now_ms = Self::now_ms();
        if self.should_emit(bytes, total, now_ms) {
            self.emit_progress(attempt, bytes, total, now_ms, tx, model);
        }
    }

    pub fn finalize(
        &mut self,
        attempt: u32,
        bytes: u64,
        total: Option<u64>,
        tx: &EventSender,
        model: &'static str,
    ) {
        let now_ms = Self::now_ms();
        self.emit_progress(attempt, bytes, total, now_ms, tx, model);
    }

    fn should_emit(&mut self, bytes: u64, total: Option<u64>, now_ms: u128) -> bool {
        match total {
            Some(t) if t > 0 => {
                let pct = ((bytes as f64 / t as f64) * 100.0) as i32;
                if pct != self.last_pct || bytes == t {
                    self.last_pct = pct;
                    true
                } else {
                    false
                }
            }
            _ => {
                if now_ms.saturating_sub(self.last_emit_ms) >= Self::NO_TOTAL_TICK_MS {
                    self.last_emit_ms = now_ms;
                    true
                } else {
                    false
                }
            }
        }
    }

    fn emit_progress(
        &mut self,
        attempt: u32,
        bytes: u64,
        total: Option<u64>,
        now_ms: u128,
        tx: &EventSender,
        model: &'static str,
    ) {
        tx.publish(AppEvent::DownloadProgress {
            attempt,
            model,
            bytes,
            total,
            bytes_per_sec: self.bytes_per_sec(now_ms, bytes),
        });
        self.last_bytes = bytes;
        self.last_total = total;
        self.last_emit_bytes = bytes;
    }

    fn now_ms() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    }

    fn bytes_per_sec(&self, now_ms: u128, bytes: u64) -> u64 {
        if self.last_emit_ms == 0 {
            return 0;
        }
        let elapsed_ms = now_ms.saturating_sub(self.last_emit_ms);
        if elapsed_ms == 0 || bytes < self.last_emit_bytes {
            return 0;
        }
        let delta = bytes - self.last_emit_bytes;
        ((delta as u128 * 1000) / elapsed_ms) as u64
    }

    pub fn last_total(&self) -> Option<u64> {
        self.last_total
    }

    const NO_TOTAL_TICK_MS: u128 = 250;
}

pub fn begin(
    entry: &'static ModelEntry,
    store: Arc<dyn ModelStore>,
    downloads: &mut Downloads,
    downloader: Arc<dyn Downloader>,
    tx: &EventSender,
) {
    if store.is_available(entry) {
        tx.publish(AppEvent::ModelAlreadyCached(entry));
        tx.publish(AppEvent::RecordingStarted(entry));
        return;
    }
    tx.publish(AppEvent::DownloadRequested(entry));
    let row = match downloads.get(entry.id) {
        Ok(r) => r,
        Err(e) => {
            tx.publish(AppEvent::DownloadFailed {
                attempt: 0,
                model: entry.id,
                error: truncate_error(&format!("downloads table: {e}")),
            });
            return;
        }
    };
    match crate::db::downloads::decide(row.as_ref()) {
        Claim::Start { attempt } => {
            let attempt = downloads.start(entry.id).unwrap_or(attempt);
            spawn(
                entry,
                store.clone(),
                downloader.clone(),
                downloads.probe(entry.id, attempt),
                attempt,
                tx.clone(),
            );
        }
        Claim::Restart { attempt } => {
            let attempt = downloads.start(entry.id).unwrap_or(attempt);
            spawn(
                entry,
                store.clone(),
                downloader.clone(),
                downloads.probe(entry.id, attempt),
                attempt,
                tx.clone(),
            );
        }
        Claim::Join => {}
    }
}

pub fn spawn(
    entry: &'static ModelEntry,
    store: Arc<dyn ModelStore>,
    downloader: Arc<dyn Downloader>,
    probe: CancelProbe,
    attempt: u32,
    tx: EventSender,
) -> JoinHandle<()> {
    let url = entry.download_url;
    let sha = entry.download_sha256;
    let model = entry.id;
    let staged = match store.staging_path(entry, attempt) {
        Ok(p) => p,
        Err(e) => {
            tx.publish(AppEvent::DownloadFailed {
                attempt,
                model,
                error: truncate_error(&e.to_string()),
            });
            return std::thread::spawn(|| {});
        }
    };
    let format = entry.format;
    std::thread::spawn(move || {
        let mut throttle = Throttle::new();
        let mut probe = probe;
        struct Progress<'a> {
            tx: &'a EventSender,
            model: &'static str,
            attempt: u32,
            throttle: &'a mut Throttle,
        }
        impl<'a> Progress<'a> {
            fn call(&mut self, bytes: u64, total: Option<u64>) {
                self.throttle
                    .call(self.attempt, bytes, total, self.tx, self.model);
            }
        }
        let result = {
            let mut p = Progress {
                tx: &tx,
                model,
                attempt,
                throttle: &mut throttle,
            };
            let mut bridge = |bytes: u64, total: Option<u64>| p.call(bytes, total);
            downloader.fetch(url, &staged, sha, &mut probe, &mut bridge)
        };
        match result {
            Ok(()) => {
                if let Some(total) = throttle.last_total() {
                    throttle.finalize(attempt, total, Some(total), &tx, model);
                }
                if crate::transcription_models::handler_for(format).install_is_slow() {
                    tx.publish(AppEvent::DownloadInstalling { attempt, model });
                }
                let install_result = store.install(entry, &staged, &mut probe);
                match install_result {
                    Ok(()) => {
                        tx.publish(AppEvent::DownloadSucceeded { attempt, model });
                    }
                    Err(DownloadError::Cancelled) => {
                        tx.publish(AppEvent::DownloadCancelled { attempt, model });
                    }
                    Err(e) => tx.publish(AppEvent::DownloadFailed {
                        attempt,
                        model,
                        error: truncate_error(&e.to_string()),
                    }),
                }
            }
            Err(DownloadError::Cancelled) => {
                tx.publish(AppEvent::DownloadCancelled { attempt, model });
            }
            Err(e) => tx.publish(AppEvent::DownloadFailed {
                attempt,
                model,
                error: truncate_error(&e.to_string()),
            }),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::picker::CATALOG;
    use crate::testing::{FixtureDownloader, Outcome};
    use std::io::Cursor;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    fn tiny() -> &'static ModelEntry {
        &CATALOG[5]
    }

    #[test]
    fn stream_to_writes_file_and_verifies_sha() {
        let tmp = tempfile::tempdir().unwrap();
        let staged = tmp.path().join("out.part");
        let cancel = AtomicBool::new(false);
        let mut seen_bytes: Vec<u64> = Vec::new();
        let mut progress = |bytes: u64, _total: Option<u64>| seen_bytes.push(bytes);
        stream_to(
            Cursor::new(Vec::<u8>::new()),
            &staged,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            &mut { &cancel },
            &mut progress,
        )
        .unwrap();
        assert!(staged.is_file());
        assert!(seen_bytes.is_empty(), "no chunks for empty source");
    }

    #[test]
    fn stream_to_short_circuits_on_cancel() {
        let tmp = tempfile::tempdir().unwrap();
        let staged = tmp.path().join("out.part");
        let cancel = AtomicBool::new(true);
        let res = stream_to(
            Cursor::new(vec![0u8; 1024]),
            &staged,
            "deadbeef",
            &mut { &cancel },
            &mut |_, _| {},
        );
        assert_eq!(res, Err(DownloadError::Cancelled));
        assert!(!staged.exists());
    }

    #[test]
    fn prearmed_cancel_makes_fetch_return_cancelled() {
        let tmp = tempfile::tempdir().unwrap();
        let staged = tmp.path().join("out.part");
        let cancel = AtomicBool::new(true);
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let downloader = FixtureDownloader::new(vec![0u8; 1024], Outcome::Ok, Arc::clone(&calls));
        let res = downloader.fetch(
            "https://example.invalid/x",
            &staged,
            "deadbeef",
            &mut { &cancel },
            &mut |_, _| {},
        );
        assert_eq!(res, Err(DownloadError::Cancelled));
        assert_eq!(calls.load(Ordering::SeqCst), 1, "fetch was invoked once");
    }
}
