//! Download transport and progress throttling.

use std::fmt;
use std::fmt::Display;
use std::path::Path;

use futures::{Stream, StreamExt};
use tokio::fs;
use tokio::io::AsyncWriteExt;

use crate::bus::{AppEvent, EventSender};
use crate::db::downloads::CancelCheck;

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

#[async_trait::async_trait]
pub trait Downloader: Send + Sync + 'static {
    async fn fetch(
        &self,
        url: &str,
        staged: &Path,
        expected_sha: &str,
        cancel: &mut (dyn CancelCheck + Send),
        progress: &mut (dyn FnMut(u64, Option<u64>) + Send),
    ) -> Result<(), DownloadError>;
}

pub async fn stream_to<S, B, E>(
    src: S,
    staged: &Path,
    expected_sha: &str,
    cancel: &mut (dyn CancelCheck + Send),
    progress: &mut (dyn FnMut(u64, Option<u64>) + Send),
) -> Result<(), DownloadError>
where
    S: Stream<Item = Result<B, E>> + Send,
    B: AsRef<[u8]> + Send,
    E: Display + Send,
{
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    let mut out = fs::File::create(staged)
        .await
        .map_err(|e| DownloadError::Io(e.to_string()))?;
    futures::pin_mut!(src);
    let mut total_read: u64 = 0;
    loop {
        if cancel.is_cancelled() {
            drop(out);
            let _ = fs::remove_file(staged).await;
            return Err(DownloadError::Cancelled);
        }
        let Some(chunk) = src.next().await else {
            break;
        };
        if cancel.is_cancelled() {
            drop(out);
            let _ = fs::remove_file(staged).await;
            return Err(DownloadError::Cancelled);
        }
        let chunk = chunk.map_err(|e| DownloadError::Io(e.to_string()))?;
        let bytes = chunk.as_ref();
        out.write_all(bytes)
            .await
            .map_err(|e| DownloadError::Io(e.to_string()))?;
        hasher.update(bytes);
        total_read += bytes.len() as u64;
        progress(total_read, None);
    }
    out.flush()
        .await
        .map_err(|e| DownloadError::Io(e.to_string()))?;
    drop(out);
    let got = hex::encode(hasher.finalize());
    if got != expected_sha {
        let _ = fs::remove_file(staged).await;
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
#[async_trait::async_trait]
impl Downloader for HttpDownloader {
    async fn fetch(
        &self,
        url: &str,
        staged: &Path,
        expected_sha: &str,
        cancel: &mut (dyn CancelCheck + Send),
        progress: &mut (dyn FnMut(u64, Option<u64>) + Send),
    ) -> Result<(), DownloadError> {
        let resp = reqwest::get(url)
            .await
            .map_err(|e| DownloadError::Http(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(DownloadError::Status(status.as_u16()));
        }
        stream_to(resp.bytes_stream(), staged, expected_sha, cancel, progress).await
    }
}

pub struct Throttle {
    last_pct: i32,
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
            last_total: None,
            last_emit_ms: 0,
            last_emit_bytes: 0,
        }
    }
}
impl Throttle {
    /// Throttle intermediate progress; always emit when a known total is reached.
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

    pub(crate) fn should_emit(&mut self, bytes: u64, total: Option<u64>, now_ms: u128) -> bool {
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
            Some(t) if bytes == t => true,
            _ => {
                if now_ms.saturating_sub(self.last_emit_ms) >= Self::NO_TOTAL_TICK_MS {
                    // Don't bump last_emit_ms here — bytes_per_sec
                    // reads it to compute elapsed_ms. The
                    // bookkeeping moves into emit_progress so the
                    // timestamp captured for the rate matches the
                    // bytes/total it travels with on the event.
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
        self.last_total = total;
        self.last_emit_bytes = bytes;
        // Bookkeeping moved out of should_emit so the timestamp
        // captured here is the one bytes_per_sec just read.
        self.last_emit_ms = now_ms;
    }

    fn now_ms() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    }

    pub(crate) fn bytes_per_sec(&self, now_ms: u128, bytes: u64) -> u64 {
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

#[cfg(test)]
mod tests {
    use super::*;

    use crate::testing::{FixtureDownloader, Outcome};
    use futures::stream;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[tokio::test]
    async fn stream_to_writes_file_and_verifies_sha() {
        let tmp = tempfile::tempdir().unwrap();
        let staged = tmp.path().join("out.part");
        let cancel = AtomicBool::new(false);
        let mut seen_bytes: Vec<u64> = Vec::new();
        let mut progress = |bytes: u64, _total: Option<u64>| seen_bytes.push(bytes);
        stream_to(
            stream::empty::<Result<Vec<u8>, std::io::Error>>(),
            &staged,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            &mut { &cancel },
            &mut progress,
        )
        .await
        .unwrap();
        assert!(staged.is_file());
        assert!(seen_bytes.is_empty(), "no chunks for empty source");
    }

    #[tokio::test]
    async fn stream_to_hashes_all_chunks_and_reports_cumulative_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let staged = tmp.path().join("out.part");
        let mut cancel = AtomicBool::new(false);
        let mut progress = Vec::new();
        stream_to(
            stream::iter([Ok::<_, std::io::Error>(&b"ab"[..]), Ok(&b"c"[..])]),
            &staged,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            &mut cancel,
            &mut |bytes, total| progress.push((bytes, total)),
        )
        .await
        .unwrap();
        assert_eq!(fs::read(&staged).await.unwrap(), b"abc");
        assert_eq!(progress, [(2, None), (3, None)]);
    }

    #[tokio::test]
    async fn stream_to_removes_file_on_sha_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let staged = tmp.path().join("out.part");
        let mut cancel = AtomicBool::new(false);
        let result = stream_to(
            stream::iter([Ok::<_, std::io::Error>(b"abc")]),
            &staged,
            "deadbeef",
            &mut cancel,
            &mut |_, _| {},
        )
        .await;
        assert_eq!(
            result,
            Err(DownloadError::Sha256Mismatch {
                got: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
                    .into(),
                expected: "deadbeef".into(),
            })
        );
        assert!(!staged.exists());
    }

    #[tokio::test]
    async fn stream_to_preserves_stream_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let staged = tmp.path().join("out.part");
        let mut cancel = AtomicBool::new(false);
        let result = stream_to(
            stream::iter([Err::<Vec<u8>, _>("connection interrupted")]),
            &staged,
            "deadbeef",
            &mut cancel,
            &mut |_, _| {},
        )
        .await;
        assert_eq!(
            result,
            Err(DownloadError::Io("connection interrupted".into()))
        );
    }

    #[tokio::test]
    async fn stream_to_honors_cancellation_while_waiting_for_a_chunk() {
        let tmp = tempfile::tempdir().unwrap();
        let staged = tmp.path().join("out.part");
        let cancel = AtomicBool::new(false);
        let src = stream::once(async {
            tokio::task::yield_now().await;
            cancel.store(true, Ordering::Relaxed);
            Ok::<_, std::io::Error>(b"abc")
        });
        let mut progress = Vec::new();
        let result = stream_to(
            src,
            &staged,
            "deadbeef",
            &mut { &cancel },
            &mut |bytes, _| progress.push(bytes),
        )
        .await;
        assert_eq!(result, Err(DownloadError::Cancelled));
        assert!(!staged.exists());
        assert!(progress.is_empty(), "cancelled chunk must not be written");
    }

    #[tokio::test]
    async fn stream_to_short_circuits_on_cancel() {
        let tmp = tempfile::tempdir().unwrap();
        let staged = tmp.path().join("out.part");
        let cancel = AtomicBool::new(true);
        let res = stream_to(
            stream::iter([Ok::<_, std::io::Error>(vec![0u8; 1024])]),
            &staged,
            "deadbeef",
            &mut { &cancel },
            &mut |_, _| {},
        )
        .await;
        assert_eq!(res, Err(DownloadError::Cancelled));
        assert!(!staged.exists());
    }

    #[tokio::test]
    async fn prearmed_cancel_makes_fetch_return_cancelled() {
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
        )
        .await;
        assert_eq!(res, Err(DownloadError::Cancelled));
        assert_eq!(calls.load(Ordering::SeqCst), 1, "fetch was invoked once");
    }

    // `Throttle::should_emit` and `bytes_per_sec` are pub(crate) so
    // this test can drive them with a controlled `now_ms` — no
    // wall-clock waits. The first post-fix emit must report a
    // sensible rate: the bug was that should_emit updated
    // `last_emit_ms` BEFORE bytes_per_sec read it, so the first
    // emit after the gate saw elapsed_ms = 0 and reported 0 B/s.
    #[test]
    fn throttle_bytes_per_sec_grows_after_first_emit() {
        let mut throttle = Throttle::new();

        // First emit at t=1000 ms, 1 KiB downloaded. Gate is open
        // (last_emit_ms starts at 0 → elapsed is huge); bytes_per_sec
        // sees last_emit_ms = 0 → returns 0 (the documented "no prior
        // reference" rule, kept).
        assert!(throttle.should_emit(1024, None, 1_000));
        throttle.last_emit_ms = 1_000;
        throttle.last_emit_bytes = 1024;
        assert_eq!(throttle.bytes_per_sec(1_000, 1024), 0);

        // Second emit at t=1500 ms, 1 MiB downloaded. 1 MiB - 1 KiB
        // arrived in 500 ms → ~2 MB/s.
        assert!(throttle.should_emit(1024 * 1024, None, 1_500));
        let expected_2 = (1024u64 * 1024 - 1024) * 1000 / 500;
        assert_eq!(
            throttle.bytes_per_sec(1_500, 1024 * 1024),
            expected_2,
            "second emit must report the rate between the two timestamps"
        );
        throttle.last_emit_ms = 1_500;
        throttle.last_emit_bytes = 1024 * 1024;

        // Third emit at t=2000 ms, 2 MiB. 1 MiB / 500 ms → 2 MB/s.
        assert!(throttle.should_emit(2 * 1024 * 1024, None, 2_000));
        let expected_3 = 1024u64 * 1024 * 1000 / 500;
        assert_eq!(throttle.bytes_per_sec(2_000, 2 * 1024 * 1024), expected_3);
    }

    // Regression for the bug seen in the production log:
    // bytes_per_sec was 0 across all DownloadProgress events because
    // should_emit advanced `last_emit_ms` before bytes_per_sec read
    // it. The first emit (no prior reference) is the only legitimate
    // zero. After should_emit's gate opens the second time,
    // bytes_per_sec must see a non-zero elapsed window.
    #[test]
    fn throttle_first_emit_returns_zero_others_grow() {
        let mut throttle = Throttle::new();
        // Simulate the first emit landing (no prior timestamp):
        // should_emit gates it open; bytes_per_sec reports 0.
        assert!(throttle.should_emit(1024, None, 1_000));
        throttle.last_emit_ms = 1_000;
        throttle.last_emit_bytes = 1024;
        assert_eq!(throttle.bytes_per_sec(1_000, 1024), 0);

        // After the gate (NO_TOTAL_TICK_MS = 250 ms) opens,
        // bytes_per_sec must observe a non-zero rate.
        assert!(throttle.should_emit(2048, None, 1_500));
        let rate = throttle.bytes_per_sec(1_500, 2048);
        assert!(rate > 0, "rate after the gate must be non-zero; got {rate}");
    }

    // Gate timing: should_emit suppresses emits inside the 250 ms
    // cooldown when `total` is unknown.
    #[test]
    fn throttle_gate_suppresses_emits_inside_cooldown() {
        let mut throttle = Throttle::new();
        assert!(throttle.should_emit(1024, None, 1_000));
        throttle.last_emit_ms = 1_000;
        assert!(
            !throttle.should_emit(2048, None, 1_100),
            "100 ms later is inside the gate"
        );
        assert!(
            throttle.should_emit(4096, None, 1_260),
            "260 ms later is past the gate"
        );
    }

    #[test]
    fn throttle_completion_bypasses_cooldown_including_empty_downloads() {
        let mut throttle = Throttle::new();
        throttle.last_emit_ms = 1_000;
        throttle.last_pct = 100;
        assert!(!throttle.should_emit(0, None, 1_000));
        for total in [0, 100] {
            assert!(
                throttle.should_emit(total, Some(total), 1_000),
                "completion must emit even inside the cooldown: total={total}"
            );
        }
    }
}
