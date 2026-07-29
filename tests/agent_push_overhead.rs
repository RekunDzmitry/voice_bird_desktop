//! Micro-benchmark for the #32 async_trait migration.
//!
//! Compares per-call dispatch overhead of the post-#32 path (await
//! `AgentTarget::push_segment` on the caller's runtime) against a
//! faithful reconstruction of the pre-#32 bridge (spawn a dedicated
//! OS thread + one-shot current-thread runtime per call, join via
//! mpsc). The target itself is a no-op so the numbers isolate the
//! dispatch mechanics, not broker I/O.
//!
//! `#[ignore]`d so normal `cargo test` stays fast and can't flake on
//! a loaded machine; run with:
//!
//! ```bash
//! cargo test --test agent_push_overhead -- --ignored --nocapture
//! ```

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use voice_bird_cli::agent::{AgentSessionId, AgentTarget};
use voice_bird_cli::transcription::Segment;

struct NoopTarget {
    pushes: AtomicUsize,
}

#[async_trait::async_trait]
impl AgentTarget for NoopTarget {
    fn session_id(&self) -> AgentSessionId {
        AgentSessionId("bench".into())
    }
    async fn push_segment(&self, _segment: &Segment) -> anyhow::Result<()> {
        self.pushes.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
    fn pull_recent(&self, _limit: usize) -> Vec<Segment> {
        Vec::new()
    }
}

fn seg() -> Segment {
    Segment {
        t_start: Duration::from_millis(0),
        t_end: Duration::from_millis(500),
        text: "bench".into(),
        tokens: Vec::new(),
    }
}

#[test]
#[ignore = "micro-benchmark; run explicitly with -- --ignored --nocapture"]
fn async_dispatch_beats_thread_bridge_by_50_percent() {
    const ITERS: usize = 2_000;
    let target: Arc<dyn AgentTarget> = Arc::new(NoopTarget {
        pushes: AtomicUsize::new(0),
    });
    let segment = seg();

    // Post-#32 path: one runtime, await the trait future directly —
    // exactly what the recorder's consumer task does per segment.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let async_elapsed = {
        let target = target.clone();
        let segment = segment.clone();
        let started = Instant::now();
        rt.block_on(async move {
            for _ in 0..ITERS {
                target.push_segment(&segment).await.unwrap();
            }
        });
        started.elapsed()
    };

    // Pre-#32 bridge: per call, spawn an OS thread that builds a
    // one-shot current-thread runtime, block_on the same future,
    // and hand the result back over an mpsc channel.
    let bridge_elapsed = {
        let started = Instant::now();
        for _ in 0..ITERS {
            let target = target.clone();
            let segment = segment.clone();
            let (tx, rx) = std::sync::mpsc::channel::<anyhow::Result<()>>();
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("build one-shot tokio runtime");
                let r = rt.block_on(async { target.push_segment(&segment).await });
                let _ = tx.send(r);
            });
            rx.recv().unwrap().unwrap();
        }
        started.elapsed()
    };

    let per_call_async = async_elapsed.as_nanos() / ITERS as u128;
    let per_call_bridge = bridge_elapsed.as_nanos() / ITERS as u128;
    println!(
        "push_segment dispatch overhead over {ITERS} calls:\n\
         async trait (post-#32): {per_call_async} ns/call\n\
         thread bridge (pre-#32): {per_call_bridge} ns/call\n\
         reduction: {:.1}%",
        100.0 * (1.0 - per_call_async as f64 / per_call_bridge as f64)
    );

    // Acceptance (#32): at least a 50% drop in per-segment overhead.
    assert!(
        per_call_async * 2 <= per_call_bridge,
        "async path ({per_call_async} ns/call) is not >=50% cheaper than \
         the thread bridge ({per_call_bridge} ns/call)"
    );
}
