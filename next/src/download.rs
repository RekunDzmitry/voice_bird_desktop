//! Download transport and orchestration.
//!
//! [`Downloader`] is the trait a thread runs to fetch bytes to a
//! staging file. [`HttpDownloader`] is the live reqwest-based impl,
//! gated behind the `net` feature. Tests use the
//! [`crate::testing::FixtureDownloader`], which keeps the flow off the
//! network entirely.
//!
//! [`begin`] is the single entry point for Enter and for retry. It
//! owns both the "is it on disk" and the "is it already downloading"
//! decisions because both touch state the reducer must not see.
//!
//! [`CancelRegistry`] tracks the cancel flag per in-flight download so
//! closing the last waiter actually stops the thread and clears the
//! staging file.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::bus::{AppEvent, EventSender};
use crate::model_store::ModelStore;
use crate::picker::ModelEntry;
use crate::store::DownloadRepository;

/// Failure vocabulary for the download pipeline. Seven variants cover
/// the observed failure modes without falling back to `anyhow` —
/// tests match on the variant, not on a substring.
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

/// Truncate a possibly-long error message to what the user actually
/// sees on a single line in a narrow column.
pub fn truncate_error(s: &str) -> String {
    const MAX: usize = 160;
    if s.chars().count() <= MAX {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(MAX).collect();
        format!("{truncated}…")
    }
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// Fetch bytes from `url` into a staging file, verify the SHA, report
/// progress. The downloader is the only thing that talks to the
/// network; everything else (throttle, install, cancel) is format
/// agnostic.
pub trait Downloader: Send + Sync + 'static {
    fn fetch(
        &self,
        url: &str,
        staged: &Path,
        expected_sha: &str,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(u64, Option<u64>),
    ) -> Result<(), DownloadError>;
}

/// Stream `src` into `staged` while hashing and watching the cancel
/// flag. Used directly by tests against a `&[u8]` cursor; the live
/// HttpDownloader is the only place that calls `reqwest::blocking::get`.
pub fn stream_to<R: std::io::Read>(
    mut src: R,
    staged: &Path,
    expected_sha: &str,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<(), DownloadError> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    let mut out = fs::File::create(staged).map_err(|e| DownloadError::Io(e.to_string()))?;
    let mut buf = [0u8; 1 << 16];
    let mut total_read: u64 = 0;
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = fs::remove_file(staged);
            return Err(DownloadError::Cancelled);
        }
        let n = src.read(&mut buf).map_err(|e| DownloadError::Io(e.to_string()))?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n]).map_err(|e| DownloadError::Io(e.to_string()))?;
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

/// The live HTTP downloader. Only constructed in `main.rs` so tests
/// never touch the network.
#[cfg(feature = "net")]
pub struct HttpDownloader;

#[cfg(feature = "net")]
impl Downloader for HttpDownloader {
    fn fetch(
        &self,
        url: &str,
        staged: &Path,
        expected_sha: &str,
        cancel: &AtomicBool,
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

// ---------------------------------------------------------------------------
// Throttle
// ---------------------------------------------------------------------------

/// Throttle progress callbacks to keep event traffic bounded. Without
/// this, a 1.6 GB download at 64 KiB chunks would emit ~25 000 events;
/// the throttle caps it at ~101 (one per percentage point, or every
/// 250 ms when no `total` is known). The last progress is always
/// emitted so the bar reaches 100% / final byte count.
pub struct Throttle {
    last_pct: i32,
    last_bytes: u64,
    last_total: Option<u64>,
    last_emit_ms: u128,
    /// Bytes recorded on the most recent progress emit. Used to compute
    /// `bytes_per_sec` between emits. Zero on a fresh throttle or right
    /// after the `BytesVerified` flip (when phase changes mid-stream).
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
        bytes: u64,
        total: Option<u64>,
        tx: &EventSender,
        model: &'static str,
    ) {
        match total {
            Some(t) if t > 0 => {
                let pct = ((bytes as f64 / t as f64) * 100.0) as i32;
                if pct != self.last_pct || bytes == t {
                    self.last_pct = pct;
                    self.last_bytes = bytes;
                    self.last_total = Some(t);
                    // Known total: bytes_per_sec isn't needed for the
                    // gauge label, but keep the previous value so a
                    // mid-stream phase flip doesn't show 0.0 MB/s.
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis())
                        .unwrap_or(0);
                    tx.publish(AppEvent::DownloadProgress {
                        model,
                        bytes,
                        total,
                        bytes_per_sec: self.bytes_per_sec(now, bytes),
                    });
                    self.last_emit_bytes = bytes;
                }
            }
            _ => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0);
                if now.saturating_sub(self.last_emit_ms) >= 250 {
                    let bps = self.bytes_per_sec(now, bytes);
                    tx.publish(AppEvent::DownloadProgress {
                        model,
                        bytes,
                        total,
                        bytes_per_sec: bps,
                    });
                    self.last_emit_ms = now;
                    self.last_bytes = bytes;
                    self.last_emit_bytes = bytes;
                }
            }
        }
    }

    pub fn flush(
        &mut self,
        bytes: u64,
        total: Option<u64>,
        tx: &EventSender,
        model: &'static str,
    ) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let bps = self.bytes_per_sec(now, bytes);
        tx.publish(AppEvent::DownloadProgress {
            model,
            bytes,
            total,
            bytes_per_sec: bps,
        });
        self.last_bytes = bytes;
        self.last_total = total;
        self.last_emit_bytes = bytes;
    }

    /// Bytes per second measured across the previous emit window. Zero
    /// on the very first tick (no prior reference); the renderer treats
    /// 0 as "no measurement yet".
    fn bytes_per_sec(&self, now_ms: u128, bytes: u64) -> u64 {
        if self.last_emit_ms == 0 {
            return 0;
        }
        let elapsed_ms = now_ms.saturating_sub(self.last_emit_ms);
        if elapsed_ms == 0 || bytes < self.last_emit_bytes {
            return 0;
        }
        let delta = bytes - self.last_emit_bytes;
        // 1000 * delta / elapsed_ms, but watch u128 -> u64 truncation
        // (delta fits in u64; the multiplication can overflow u128 only
        // on a multi-million GB/s download, which we will not see).
        ((delta as u128 * 1000) / elapsed_ms) as u64
    }

    pub fn last_total(&self) -> Option<u64> {
        self.last_total
    }
}


// ---------------------------------------------------------------------------
// CancelRegistry
// ---------------------------------------------------------------------------

/// Per-model cancel flags. `main.rs` fires `cancel(model)` when the
/// reducer reports that `BlockClosed` dropped the last waiter; the
/// in-flight thread observes the flag, aborts the chunk loop and
/// publishes nothing (the record is already gone).
#[derive(Default)]
pub struct CancelRegistry {
    inner: Mutex<HashMap<&'static str, Arc<AtomicBool>>>,
}

impl CancelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn token(&self, model: &'static str) -> Arc<AtomicBool> {
        let mut g = self.inner.lock().expect("cancel registry poisoned");
        g.entry(model)
            .or_insert_with(|| Arc::new(AtomicBool::new(false)))
            .clone()
    }

    pub fn cancel(&self, model: &'static str) {
        if let Some(flag) = self.inner.lock().expect("cancel registry poisoned").get(model) {
            flag.store(true, Ordering::Relaxed);
        }
    }

    pub fn clear(&self, model: &'static str) {
        self.inner.lock().expect("cancel registry poisoned").remove(model);
    }
}

// ---------------------------------------------------------------------------
// begin — the resolver's single entry point
// ---------------------------------------------------------------------------

/// Single entry point for Enter and for retry. Owns both the
/// "present or missing" and the "already downloading" decisions,
/// because both touch state the reducer must not see.
pub fn begin(
    entry: &'static ModelEntry,
    store: &Arc<dyn ModelStore>,
    repo: &Arc<dyn DownloadRepository>,
    downloader: &Arc<dyn Downloader>,
    cancels: &CancelRegistry,
    tx: &EventSender,
) {
    if store.is_available(entry) {
        tx.publish(AppEvent::RecordingStarted(entry));
        return;
    }
    tx.publish(AppEvent::DownloadRequested(entry));
    // Dedup: the second block to want this model joins the first
    // block's download instead of racing it onto the same staging
    // path.
    if !repo.is_active(entry.id) {
        spawn(
            entry,
            store.clone(),
            downloader.clone(),
            cancels.token(entry.id),
            tx.clone(),
        );
    }
}

/// Spawn one download thread. Resolves the staging path, fetches with
/// throttled progress, publishes `DownloadInstalling` for slow formats,
/// installs, publishes `DownloadSucceeded`. Any error publishes
/// `DownloadFailed` with the message truncated to 160 chars.
///
/// `Cancelled` publishes nothing — the record is already gone.
pub fn spawn(
    entry: &'static ModelEntry,
    store: Arc<dyn ModelStore>,
    downloader: Arc<dyn Downloader>,
    cancel: Arc<AtomicBool>,
    tx: EventSender,
) -> JoinHandle<()> {
    let url = entry.download_url;
    let sha = entry.download_sha256;
    let model = entry.id;
    let staged = match store.staging_path(entry) {
        Ok(p) => p,
        Err(e) => {
            tx.publish(AppEvent::DownloadFailed {
                model,
                error: truncate_error(&e.to_string()),
            });
            return std::thread::spawn(|| {});
        }
    };
    let format = entry.format;
    std::thread::spawn(move || {
        let mut throttle = Throttle::new();
        // Bridge the &mut dyn FnMut the downloader wants to a closure
        // that can talk to the throttle. `Progress` holds the mutable
        // borrow to the throttle; the inner closure defers to it.
        struct Progress<'a> {
            tx: &'a EventSender,
            model: &'static str,
            throttle: &'a mut Throttle,
        }
        impl<'a> Progress<'a> {
            fn call(&mut self, bytes: u64, total: Option<u64>) {
                self.throttle.call(bytes, total, self.tx, self.model);
            }
        }
        let result = {
            let mut p = Progress {
                tx: &tx,
                model,
                throttle: &mut throttle,
            };
            let mut bridge = |bytes: u64, total: Option<u64>| p.call(bytes, total);
            downloader.fetch(url, &staged, sha, &cancel, &mut bridge)
        };
        match result {


            Ok(()) => {
                if let Some(total) = throttle.last_total() {
                    throttle.flush(total, Some(total), &tx, model);
                }
                if crate::model_store::handler_for(format).install_is_slow() {
                    tx.publish(AppEvent::DownloadInstalling { model });
                }
                match store.install(entry, &staged) {
                    Ok(()) => {
                        tx.publish(AppEvent::DownloadSucceeded { model });
                    }
                    Err(e) => tx.publish(AppEvent::DownloadFailed {
                        model,
                        error: truncate_error(&e.to_string()),
                    }),
                }
            }
            Err(DownloadError::Cancelled) => {
                // No event — the reducer already dropped the record.
            }
            Err(e) => tx.publish(AppEvent::DownloadFailed {
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
    use std::io::Cursor;
    use std::sync::atomic::AtomicUsize;
    use tempfile::TempDir;

    fn tiny() -> &'static ModelEntry {
        &CATALOG[5]
    }

    fn sha_of(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(bytes);
        hex::encode(h.finalize())
    }

    #[test]
    fn stream_to_writes_and_verifies_known_sha() {
        let tmp = TempDir::new().unwrap();
        let staged = tmp.path().join("x.part");
        let bytes = b"hello world";
        let cancel = AtomicBool::new(false);
        stream_to(Cursor::new(bytes), &staged, &sha_of(bytes), &cancel, &mut |_, _| {})
            .unwrap();
        assert_eq!(fs::read(&staged).unwrap(), bytes);
    }

    #[test]
    fn stream_to_sha_mismatch_removes_staged_file() {
        let tmp = TempDir::new().unwrap();
        let staged = tmp.path().join("x.part");
        let cancel = AtomicBool::new(false);
        let res = stream_to(
            Cursor::new(b"hello"),
            &staged,
            &sha_of(b"goodbye"),
            &cancel,
            &mut |_, _| {},
        );
        assert!(matches!(res, Err(DownloadError::Sha256Mismatch { .. })));
        assert!(!staged.exists(), "staged file must be cleaned up");
    }

    #[test]
    fn stream_to_reports_monotonic_progress_with_total() {
        let tmp = TempDir::new().unwrap();
        let staged = tmp.path().join("x.part");
        let bytes: Vec<u8> = (0..1024u32).map(|i| (i % 251) as u8).collect();
        let cancel = AtomicBool::new(false);
        let mut progress_log: Vec<(u64, Option<u64>)> = Vec::new();
        stream_to(
            Cursor::new(&bytes),
            &staged,
            &sha_of(&bytes),
            &cancel,
            &mut |b, t| progress_log.push((b, t)),
        )
        .unwrap();
        let last = progress_log.last().unwrap();
        assert_eq!(last.0, bytes.len() as u64);
        assert_eq!(last.1, None); // stream_to doesn't track total itself
        let mut prev = 0u64;
        for (b, _) in &progress_log {
            assert!(*b >= prev);
            prev = *b;
        }
    }

    #[test]
    fn stream_to_without_content_length_reports_none_total() {
        let tmp = TempDir::new().unwrap();
        let staged = tmp.path().join("x.part");
        let cancel = AtomicBool::new(false);
        let mut progress_log: Vec<(u64, Option<u64>)> = Vec::new();
        stream_to(
            Cursor::new(b"abcdef"),
            &staged,
            &sha_of(b"abcdef"),
            &cancel,
            &mut |b, t| progress_log.push((b, t)),
        )
        .unwrap();
        assert!(progress_log.iter().all(|(_, t)| t.is_none()));
    }

    #[test]
    fn stream_to_honours_cancel_and_removes_staged_file() {
        let tmp = TempDir::new().unwrap();
        let staged = tmp.path().join("x.part");
        let cancel = AtomicBool::new(true);
        let res = stream_to(
            Cursor::new(b"hello"),
            &staged,
            &sha_of(b"hello"),
            &cancel,
            &mut |_, _| {},
        );
        assert_eq!(res, Err(DownloadError::Cancelled));
        assert!(!staged.exists());
    }

    #[test]
    fn display_messages_are_single_line() {
        for e in [
            DownloadError::NoCacheDir,
            DownloadError::Io("x".into()),
            DownloadError::Http("x".into()),
            DownloadError::Status(404),
            DownloadError::Sha256Mismatch {
                got: "a".into(),
                expected: "b".into(),
            },
            DownloadError::Cancelled,
        ] {
            let s = e.to_string();
            assert!(!s.contains('\n'), "{s:?}");
        }
    }

    #[test]
    fn truncate_caps_long_messages() {
        let long = "x".repeat(500);
        let t = truncate_error(&long);
        assert!(t.chars().count() <= 161);
        assert!(t.ends_with('…'));
    }

    #[test]
    fn truncate_leaves_short_messages_alone() {
        assert_eq!(truncate_error("hello"), "hello");
    }

    #[test]
    fn throttle_emits_at_most_once_per_percentage_point() {
        let (tx, rx) = std::sync::mpsc::channel();
        let tx = EventSender(tx);
        let mut th = Throttle::new();
        let total = 1_600_000_000u64;
        let n = 25_000u64;
        let chunk = total / n;
        for i in 0..n {
            th.call(i * chunk, Some(total), &tx, "tiny.en");
        }
        // Final flush — the chunk loop ends with bytes=total-1, so we
        // call once more at bytes=total to push the bar to 100%.
        th.call(total, Some(total), &tx, "tiny.en");
        let mut emitted = 0;
        while rx.try_recv().is_ok() {
            emitted += 1;
        }
        assert!(emitted <= 102, "got {emitted} events");
        assert!(emitted >= 100, "got {emitted} events; expected ~101");
    }

    #[test]
    fn throttle_without_total_emits_on_elapsed_time() {
        let (tx, rx) = std::sync::mpsc::channel();
        let tx = EventSender(tx);
        let mut th = Throttle::new();
        for _ in 0..50 {
            th.call(100, None, &tx, "tiny.en");
        }
        let mut n = 0;
        while rx.try_recv().is_ok() {
            n += 1;
        }
        assert!(n <= 1, "got {n} emissions");
        th.flush(100, None, &tx, "tiny.en");
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn cancel_registry_token_is_stable_per_model() {
        let r = CancelRegistry::new();
        let a = r.token("tiny.en");
        let b = r.token("tiny.en");
        assert!(Arc::ptr_eq(&a, &b));
        assert!(!a.load(Ordering::Relaxed));
        r.cancel("tiny.en");
        assert!(a.load(Ordering::Relaxed));
        assert!(b.load(Ordering::Relaxed));
        r.clear("tiny.en");
        let c = r.token("tiny.en");
        assert!(!Arc::ptr_eq(&a, &c));
    }

    /// The downloader's behaviour when invoked twice. Used by the dedup
    /// test in tests/download_flow.rs.
    #[allow(dead_code)]
    pub struct PanicDoubleDownloader {
        pub calls: Arc<AtomicUsize>,
    }
    impl Downloader for PanicDoubleDownloader {
        fn fetch(
            &self,
            _url: &str,
            _staged: &Path,
            _sha: &str,
            _cancel: &AtomicBool,
            _progress: &mut dyn FnMut(u64, Option<u64>),
        ) -> Result<(), DownloadError> {
            let prev = self.calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(prev, 0, "downloader called twice");
            Err(DownloadError::Cancelled)
        }
    }

    // Suppress unused-tiny import warning by referencing it.
    #[allow(dead_code)]
    fn _tiny_used() -> &'static ModelEntry {
        tiny()
    }
}