//! Download transport and orchestration.

use std::fmt;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::thread::JoinHandle;

use crate::bus::{AppEvent, EventSender};
use crate::db::downloads::{CancelCheck, CancelProbe, Claim};
use crate::db::{downloads, Database};
use crate::language::LanguageProfile;
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
        self.last_bytes = bytes;
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

/// Start or join the download for one model known to be missing.
///
/// Called after availability checks by [`begin_language`] or after the
/// watcher detects a model lost by an active block. Worker events can still
/// overtake a language selection on the shared bus; the table's subsequent
/// `DownloadStatusChanged` event reconciles them.
pub fn ensure_model(
    entry: &'static ModelEntry,
    store: Arc<dyn ModelStore>,
    db: &mut Database,
    downloader: Arc<dyn Downloader>,
    tx: &EventSender,
) {
    tx.publish(AppEvent::DownloadRequested(entry));
    let row = match downloads::get(db, entry.id) {
        Ok(row) => row,
        Err(error) => {
            tx.publish(AppEvent::DownloadFailed {
                attempt: 0,
                model: entry.id,
                error: truncate_error(&format!("downloads table: {error}")),
            });
            return;
        }
    };
    match downloads::decide(row.as_ref()) {
        Claim::Start { attempt } | Claim::Restart { attempt } => {
            start_or_fail(entry, store, db, downloader, attempt, tx)
        }
        Claim::Join => {}
    }
}

/// Select a language after checking each model exactly once, then ensure every
/// missing model has an active download.
pub fn begin_language(
    block: u8,
    language: &'static LanguageProfile,
    store: Arc<dyn ModelStore>,
    db: &mut Database,
    downloader: Arc<dyn Downloader>,
    tx: &EventSender,
) {
    let models = language
        .models()
        .map(|model| (model, store.is_available(model)));
    let pending = models
        .iter()
        .filter_map(|(model, available)| (!available).then_some(model.id))
        .collect();
    tx.publish(AppEvent::LanguageSelected {
        block,
        language,
        pending,
    });
    for (model, available) in models {
        if available {
            tx.publish(AppEvent::ModelAlreadyCached(model));
        } else {
            ensure_model(model, store.clone(), db, downloader.clone(), tx);
        }
    }
}

/// Claim a fresh attempt and spawn the worker. If `downloads::start`
/// fails (disk full, lock timeout, write error), publish
/// `DownloadFailed` with the underlying error and return without
/// spawning — otherwise the worker would start with no row in the
/// table, the attempt gate would reject every event it publishes,
/// and the UI would be stuck on `DownloadRequested` forever.
fn start_or_fail(
    entry: &'static ModelEntry,
    store: Arc<dyn ModelStore>,
    db: &mut Database,
    downloader: Arc<dyn Downloader>,
    attempt: u32,
    tx: &EventSender,
) {
    let attempt = match downloads::start(db, entry.id) {
        Ok(a) => a,
        Err(e) => {
            tx.publish(AppEvent::DownloadClaimFailed {
                attempt,
                model: entry.id,
                error: truncate_error(&format!("downloads table: {e}")),
            });
            return;
        }
    };
    spawn(
        entry,
        store,
        downloader,
        downloads::probe(db, entry.id, attempt),
        attempt,
        tx.clone(),
    );
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

    use crate::testing::{FixtureDownloader, Outcome};
    use std::io::Cursor;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

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
    // Regression for the silent `unwrap_or(attempt)` bug: when
    // `downloads::start` fails (disk full, lock timeout, write
    // error), the orchestrator must publish `DownloadFailed` with
    // the underlying error and return without spawning a worker.
    // Otherwise the worker would start without a row in the table,
    // the attempt gate would reject every event it publishes, and
    // the UI would be stuck on `DownloadRequested` forever.
    //
    // Drive the failure with a `Database` whose writer connection
    // is opened `SQLITE_OPEN_READ_ONLY`: every `execute()` write
    // returns `SQLITE_READONLY`. This mirrors the production
    // failure mode without an OS-level chmod dance and without
    // consuming the `Connection` via `close` (which would prevent
    // us from embedding it back into `Database::conn`).
    #[test]
    fn start_or_fail_surfaces_db_write_failure_as_download_failed() {
        use crate::bus::EventBus;
        use crate::db::Database;
        use crate::picker::{ModelEntry, CATALOG};

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("downloads.sqlite");
        // Bootstrap: open the file, run the migration, drop the
        let bootstrap = rusqlite::Connection::open(&path).unwrap();
        bootstrap
            .execute_batch(<crate::db::downloads::DownloadsTable as crate::db::Table>::DEFINITION)
            .unwrap();
        bootstrap.close().map_err(|(_, e)| e).unwrap();
        // Re-open with `SQLITE_OPEN_READ_ONLY`. Every subsequent
        // write from `downloads::start` returns `SQLITE_READONLY`.
        let readonly = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .expect("open read-only");
        let mut bus = EventBus::new();
        let tiny: &'static ModelEntry = &CATALOG[5];
        let store: Arc<dyn crate::transcription_models::ModelStore> = Arc::new(
            crate::testing::FixtureStore::new(tmp.path().to_path_buf(), &[]),
        );
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut db = Database::from_connection_for_test(readonly, path.clone(), bus.sender());
        let downloader: Arc<dyn Downloader> = Arc::new(crate::testing::FixtureDownloader::new(
            Vec::new(),
            Outcome::Ok,
            Arc::clone(&calls),
        ));

        // Call the private helper directly so we know exactly
        // where the failure surfaces. The same `start_or_fail` is
        // invoked by `ensure_model` for both `Claim::Start` and
        // `Claim::Restart`.
        start_or_fail(
            tiny,
            store,
            &mut db,
            downloader,
            /* attempt = */ 1,
            &bus.sender(),
        );

        // The fetcher must not have been touched — the failure
        // happens at the row-write step, before `spawn` runs.
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "worker must not spawn when downloads::start fails"
        );

        // The bus must carry `DownloadClaimFailed` (the
        // pre-persistence variant — there's no row yet to gate
        // against) with the model id and an error message
        // derived from the underlying SQL error.
        let events: Vec<AppEvent> = bus.drain().collect();
        let failed = events.iter().find_map(|ev| match ev {
            AppEvent::DownloadClaimFailed { model, error, .. } => Some((*model, error.clone())),
            _ => None,
        });
        let (model, error) = failed.expect("DownloadClaimFailed must be published");
        assert_eq!(model, tiny.id);
        assert!(
            !error.is_empty(),
            "DownloadClaimFailed must carry the underlying SQL error message"
        );
        assert!(
            error.contains("downloads table"),
            "error must be prefixed by the table name; got {error:?}"
        );
    }

    // End-to-end regression: a failed downloads-table claim must pass through
    // the bus/table/state pipeline and fail the waiting language block.
    #[test]
    fn db_write_failure_surfaces_to_ui_via_full_drain_apply_flow() {
        use crate::bus::EventBus;
        use crate::db::Database;
        use crate::language::LANGUAGES;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("downloads.sqlite");
        // Bootstrap the schema on disk so the orchestrator's
        // `downloads::get` succeeds and `decide` returns
        // `Claim::Start`. We then re-open read-only so the
        // orchestrator's subsequent `downloads::start` write
        // fails with SQLITE_READONLY.
        // Bootstrap the schema on disk AND pre-populate a row at
        // `attempt: 3` in the Failed state so the orchestrator's
        // `downloads::get` returns it, `decide` proposes
        // `Claim::Start { attempt: 4 }`, and the read-only
        // re-open causes the subsequent `downloads::start` write
        // to fail with SQLITE_READONLY. The expected failure
        // event carries `attempt: 4`; the derived
        // `DownloadStatusChanged` must forward the same `4`.
        {
            let bootstrap = rusqlite::Connection::open(&path).unwrap();
            bootstrap
                .execute_batch(
                    <crate::db::downloads::DownloadsTable as crate::db::Table>::DEFINITION,
                )
                .unwrap();
            bootstrap
                .execute(
                    "INSERT INTO downloads (model, attempt, status, error, created_at, updated_at) \
                     VALUES (?1, ?2, ?3, NULL, ?4, ?4)",
                    rusqlite::params![
                        LANGUAGES[0].live.id,
                        3u32,
                        "Failed",
                        "2026-09-29T00:00:00.000Z",
                    ],
                )
                .unwrap();
            bootstrap.close().map_err(|(_, e)| e).unwrap();
        }
        let readonly = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .expect("open read-only");
        let mut bus = EventBus::new();
        let mut db = Database::from_connection_for_test(readonly, path.clone(), bus.sender());
        let mut state = crate::state::UiState::default();
        state.apply(&AppEvent::AddBlock);
        let _ = crate::db::downloads::apply(&mut db, &AppEvent::AddBlock);

        let language = &LANGUAGES[0];
        crate::producer::resolve_intent(
            crate::input::Intent::Confirm,
            &state,
            &mut db,
            &bus.sender(),
        );
        let events: Vec<AppEvent> = bus.drain().collect();
        assert!(
            events.iter().any(|event| matches!(
                event,
                AppEvent::BeginLanguage { block: 1, language: selected, source_rev: None }
                    if *selected == language
            )),
            "Confirm must publish BeginLanguage; got {events:?}"
        );

        // Drive the language orchestrator directly with the read-only database.
        let store: Arc<dyn crate::transcription_models::ModelStore> = Arc::new(
            crate::testing::FixtureStore::new(tmp.path().to_path_buf(), &[]),
        );
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let downloader: Arc<dyn Downloader> = Arc::new(crate::testing::FixtureDownloader::new(
            Vec::new(),
            Outcome::Ok,
            Arc::clone(&calls),
        ));
        begin_language(1, language, store, &mut db, downloader, &bus.sender());
        let mut drained: Vec<AppEvent> = bus.drain().collect();

        // Worker must not have spawned.
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "worker must not spawn when downloads::start fails"
        );

        // Now drain whatever the orchestrator published and run
        // it through the full table + state pipeline. This is the
        // production loop body: drain → for ev in events { apply →
        for ev in &drained {
            let _ = crate::db::downloads::apply(&mut db, ev);
            state.apply(ev);
        }
        // Re-drain for any further events published by the
        // table reducer (e.g. `DownloadStatusChanged` from
        // `downloads::apply::DownloadClaimFailed`), and fold
        // those into the audit-log assertion below. The UI
        // state ignores these observability events so we
        // don't re-apply them.
        let mut post_apply: Vec<AppEvent> = bus.drain().collect();
        drained.append(&mut post_apply);

        let block = state
            .blocks
            .first()
            .expect("block must still exist after the failure");
        match &block.state {
            crate::state::BlockState::Failed {
                language: failed_language,
                error,
                ..
            } => {
                assert_eq!(*failed_language, language);
                assert!(
                    error.contains("downloads table"),
                    "error must surface the underlying table failure; got {error:?}"
                );
            }
            other => panic!(
                "block must be Failed after a DB-write failure surfaces through the bus; got {other:?}"
            ),
        }
        // Derived audit event must forward the orchestrator's
        // predicted attempt (4 in this scenario — `decide`
        // proposed `attempt: 4` for the Failed row at attempt 3
        // pre-populated above). Without the forward, the log
        // records `DownloadClaimFailed { attempt: 4 }` followed
        // by `DownloadStatusChanged { attempt: 0 }`, breaking
        // attempt correlation and contradicting the table-side
        // invariant that every status change carries the
        // orchestrator's claim attempt.
        let status_changes: Vec<_> = drained
            .iter()
            .filter_map(|ev| match ev {
                AppEvent::DownloadStatusChanged { attempt, to, .. } => Some((*attempt, *to)),
                _ => None,
            })
            .collect();
        assert!(
            status_changes
                .iter()
                .any(|(a, t)| *a == 4 && *t == crate::bus::DownloadStatus::Failed),
            "DownloadStatusChanged must forward attempt=4 (Failed); got {status_changes:?}"
        );
    }
}
