//! Source-independent download transport and error handling.

use std::fmt;
use std::fmt::Display;
use std::path::Path;

use futures::{Stream, StreamExt};
use tokio::fs;
use tokio::io::AsyncWriteExt;

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

pub(crate) fn truncate_error(s: &str) -> String {
    const MAX: usize = 160;
    if s.chars().count() <= MAX {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(MAX).collect();
        format!("{truncated}…")
    }
}

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
}
