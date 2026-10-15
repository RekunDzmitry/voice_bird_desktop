//! Executes claimed model fetches and publishes worker progress and results.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::task::JoinHandle;

use crate::bus::{AppEvent, EventSender};
use crate::db::downloads::CancelProbe;
use crate::download::{truncate_error, DownloadError, Downloader};
use crate::picker::ModelEntry;

pub fn start(
    downloader: Arc<dyn Downloader>,
    entry: &'static ModelEntry,
    probe: CancelProbe,
    attempt: u32,
    staged: PathBuf,
    tx: EventSender,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let url = entry.download_url;
        let sha = entry.download_sha256;
        let model = entry.id;
        let mut throttle = Throttle::new();
        let mut probe = probe;
        let result = {
            let mut progress = |bytes: u64, total: Option<u64>| {
                throttle.call(attempt, bytes, total, &tx, model);
            };
            downloader
                .fetch(url, &staged, sha, &mut probe, &mut progress)
                .await
        };
        match result {
            Ok(()) => {
                if let Some(total) = throttle.last_total() {
                    throttle.call(attempt, total, Some(total), &tx, model);
                }
                tx.publish(AppEvent::DownloadFetched { attempt, model });
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

struct Throttle {
    last_pct: i32,
    last_total: Option<u64>,
    last_emit_ms: u128,
    last_emit_bytes: u64,
}

impl Throttle {
    fn new() -> Self {
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
    fn call(
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

    fn last_total(&self) -> Option<u64> {
        self.last_total
    }

    const NO_TOTAL_TICK_MS: u128 = 250;
}

#[cfg(test)]
mod tests {
    use super::*;

    // Drive the throttle with a controlled `now_ms` — no wall-clock waits.
    // The first post-fix emit must report a
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
