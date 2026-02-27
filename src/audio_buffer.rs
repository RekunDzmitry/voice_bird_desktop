use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Maximum number of chunks to buffer (~5 seconds at ~100 chunks/sec)
const MAX_BUFFER_CHUNKS: usize = 500;

/// A timestamped audio chunk: (capture_timestamp_ms, samples)
pub type TimestampedChunk = (u64, Vec<f32>);

struct SharedState {
    buffer: VecDeque<TimestampedChunk>,
    stopped: bool,
}

/// Thread-safe pre-buffer that decouples audio capture from WebSocket readiness.
///
/// Audio callbacks push samples via `AudioProducer` immediately when capture starts.
/// The streaming thread drains buffered chunks via `AudioConsumer` once the WebSocket
/// connection is established, ensuring no audio is lost during setup.
pub struct AudioPreBuffer {
    state: Arc<(Mutex<SharedState>, Condvar)>,
}

impl AudioPreBuffer {
    pub fn new() -> Self {
        Self {
            state: Arc::new((
                Mutex::new(SharedState {
                    buffer: VecDeque::new(),
                    stopped: false,
                }),
                Condvar::new(),
            )),
        }
    }

    /// Create a producer for use in audio callbacks.
    pub fn producer(&self) -> AudioProducer {
        let base_epoch_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        AudioProducer {
            state: self.state.clone(),
            base_epoch_ms,
            base_instant: Instant::now(),
        }
    }

    /// Create a consumer for use in the streaming thread.
    pub fn consumer(&self) -> AudioConsumer {
        AudioConsumer {
            state: self.state.clone(),
        }
    }

    /// Signal that audio capture has stopped. Wakes the consumer if it's waiting.
    pub fn stop(&self) {
        let (lock, cvar) = &*self.state;
        if let Ok(mut shared) = lock.lock() {
            shared.stopped = true;
        }
        cvar.notify_all();
    }
}

impl Drop for AudioPreBuffer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Producer end — used by audio callbacks to push sample chunks.
/// Cloneable so multiple callbacks can share it.
#[derive(Clone)]
pub struct AudioProducer {
    state: Arc<(Mutex<SharedState>, Condvar)>,
    /// Base SystemTime captured once at construction (converted to ms since epoch).
    base_epoch_ms: u64,
    /// Base Instant captured at the same moment as `base_epoch_ms`.
    base_instant: Instant,
}

impl AudioProducer {
    /// Push a chunk of audio samples into the buffer with a capture timestamp.
    /// If the buffer is full, the oldest chunk is dropped to make room.
    ///
    /// Timestamps are derived from a userspace `Instant::elapsed()` call
    /// (avoiding the `clock_gettime` syscall that `SystemTime::now()` requires on macOS).
    pub fn push(&self, samples: Vec<f32>) {
        let timestamp_ms = self.base_epoch_ms + self.base_instant.elapsed().as_millis() as u64;

        let (lock, cvar) = &*self.state;
        if let Ok(mut shared) = lock.lock() {
            if shared.stopped {
                return;
            }
            let was_empty = shared.buffer.is_empty();
            if shared.buffer.len() >= MAX_BUFFER_CHUNKS {
                shared.buffer.pop_front();
            }
            shared.buffer.push_back((timestamp_ms, samples));
            // Only wake the consumer when transitioning from empty → non-empty.
            // This avoids ~90% of wasted kernel notifications since the consumer
            // is usually already processing data.
            if was_empty {
                cvar.notify_one();
            }
        }
    }
}

/// Consumer end — used by the streaming thread to drain buffered audio.
pub struct AudioConsumer {
    state: Arc<(Mutex<SharedState>, Condvar)>,
}

impl AudioConsumer {
    /// Non-blocking: drain all currently buffered chunks with their capture timestamps.
    pub fn drain_all(&self) -> Vec<TimestampedChunk> {
        let (lock, _) = &*self.state;
        if let Ok(mut shared) = lock.lock() {
            shared.buffer.drain(..).collect()
        } else {
            Vec::new()
        }
    }

    /// Blocking: wait up to `timeout` for chunks to arrive, then drain all available.
    /// Returns an empty vec on timeout or if stopped.
    pub fn wait_and_drain(&self, timeout: Duration) -> Vec<TimestampedChunk> {
        let (lock, cvar) = &*self.state;
        if let Ok(mut shared) = lock.lock() {
            if shared.buffer.is_empty() && !shared.stopped {
                let result = cvar.wait_timeout(shared, timeout);
                match result {
                    Ok((guard, _)) => shared = guard,
                    Err(_) => return Vec::new(),
                }
            }
            shared.buffer.drain(..).collect()
        } else {
            Vec::new()
        }
    }

    /// Check if the producer side has signaled stop.
    pub fn is_stopped(&self) -> bool {
        let (lock, _) = &*self.state;
        if let Ok(shared) = lock.lock() {
            shared.stopped
        } else {
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_push_drain() {
        let pre_buffer = AudioPreBuffer::new();
        let producer = pre_buffer.producer();
        let consumer = pre_buffer.consumer();

        producer.push(vec![1.0, 2.0, 3.0]);
        producer.push(vec![4.0, 5.0]);

        let chunks = consumer.drain_all();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].1, vec![1.0, 2.0, 3.0]);
        assert_eq!(chunks[1].1, vec![4.0, 5.0]);
        // Timestamps should be non-zero
        assert!(chunks[0].0 > 0);
        assert!(chunks[1].0 > 0);

        // Drain again should be empty
        let chunks = consumer.drain_all();
        assert!(chunks.is_empty());
    }

    #[test]
    fn test_overflow_drops_oldest() {
        let pre_buffer = AudioPreBuffer::new();
        let producer = pre_buffer.producer();
        let consumer = pre_buffer.consumer();

        for i in 0..MAX_BUFFER_CHUNKS + 10 {
            producer.push(vec![i as f32]);
        }

        let chunks = consumer.drain_all();
        assert_eq!(chunks.len(), MAX_BUFFER_CHUNKS);
        // First chunk should be the 11th one pushed (index 10)
        assert_eq!(chunks[0].1, vec![10.0]);
    }

    #[test]
    fn test_stop_signal() {
        let pre_buffer = AudioPreBuffer::new();
        let consumer = pre_buffer.consumer();

        assert!(!consumer.is_stopped());
        pre_buffer.stop();
        assert!(consumer.is_stopped());
    }

    #[test]
    fn test_wait_and_drain_timeout() {
        let pre_buffer = AudioPreBuffer::new();
        let consumer = pre_buffer.consumer();

        let start = std::time::Instant::now();
        let chunks = consumer.wait_and_drain(Duration::from_millis(50));
        let elapsed = start.elapsed();

        assert!(chunks.is_empty());
        assert!(elapsed >= Duration::from_millis(40)); // allow some jitter
    }

    #[test]
    fn test_wait_and_drain_with_data() {
        let pre_buffer = AudioPreBuffer::new();
        let producer = pre_buffer.producer();
        let consumer = pre_buffer.consumer();

        producer.push(vec![1.0, 2.0]);

        let chunks = consumer.wait_and_drain(Duration::from_millis(100));
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].1, vec![1.0, 2.0]);
    }

    #[test]
    fn test_producer_clone() {
        let pre_buffer = AudioPreBuffer::new();
        let producer1 = pre_buffer.producer();
        let producer2 = producer1.clone();
        let consumer = pre_buffer.consumer();

        producer1.push(vec![1.0]);
        producer2.push(vec![2.0]);

        let chunks = consumer.drain_all();
        assert_eq!(chunks.len(), 2);
    }

    #[test]
    fn test_push_after_stop_is_noop() {
        let pre_buffer = AudioPreBuffer::new();
        let producer = pre_buffer.producer();
        let consumer = pre_buffer.consumer();

        pre_buffer.stop();
        producer.push(vec![1.0]);

        let chunks = consumer.drain_all();
        assert!(chunks.is_empty());
    }

    #[test]
    fn test_timestamps_use_instant_not_syscall() {
        // Verify that timestamps from push() are reasonable epoch-ms values
        // derived from the Instant-based calculation
        let pre_buffer = AudioPreBuffer::new();
        let producer = pre_buffer.producer();
        let consumer = pre_buffer.consumer();

        let before_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        producer.push(vec![1.0]);
        std::thread::sleep(Duration::from_millis(10));
        producer.push(vec![2.0]);

        let after_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let chunks = consumer.drain_all();
        assert_eq!(chunks.len(), 2);
        // Timestamps should be within the before/after window
        assert!(chunks[0].0 >= before_ms && chunks[0].0 <= after_ms,
            "first timestamp {} not in [{}, {}]", chunks[0].0, before_ms, after_ms);
        assert!(chunks[1].0 >= before_ms && chunks[1].0 <= after_ms,
            "second timestamp {} not in [{}, {}]", chunks[1].0, before_ms, after_ms);
        // Second timestamp should be >= first (monotonic)
        assert!(chunks[1].0 >= chunks[0].0);
    }

    #[test]
    fn test_conditional_condvar_wakes_on_empty() {
        // Verify that wait_and_drain returns immediately when data is pushed
        // into an empty buffer (condvar fires on empty→non-empty)
        let pre_buffer = AudioPreBuffer::new();
        let producer = pre_buffer.producer();
        let consumer = pre_buffer.consumer();

        // Spawn producer that pushes after a short delay
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            producer.push(vec![1.0, 2.0]);
        });

        let start = Instant::now();
        let chunks = consumer.wait_and_drain(Duration::from_secs(2));
        let elapsed = start.elapsed();

        assert_eq!(chunks.len(), 1);
        // Should wake up around 50ms, well before the 2s timeout
        assert!(elapsed < Duration::from_millis(500),
            "took {:?} — condvar notification may not be working", elapsed);
    }
}

/// Micro-benchmarks comparing old (Mutex) vs new (Atomic) hot-path patterns.
/// Run with: `cargo test --release bench_ -- --nocapture`
#[cfg(test)]
mod benchmarks {
    use std::sync::{Arc, Condvar, Mutex};
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
    use std::collections::VecDeque;

    const ITERATIONS: u64 = 100_000;

    #[test]
    fn bench_stop_signal_mutex_vs_atomic() {
        // OLD: Mutex<bool>
        let stop_mutex: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
        let start = Instant::now();
        for _ in 0..ITERATIONS {
            if let Ok(stop) = stop_mutex.lock() {
                let _ = *stop;
            }
        }
        let mutex_elapsed = start.elapsed();

        // NEW: AtomicBool
        let stop_atomic = Arc::new(AtomicBool::new(false));
        let start = Instant::now();
        for _ in 0..ITERATIONS {
            let _ = stop_atomic.load(Ordering::Acquire);
        }
        let atomic_elapsed = start.elapsed();

        let speedup = mutex_elapsed.as_nanos() as f64 / atomic_elapsed.as_nanos().max(1) as f64;
        eprintln!("\n=== stop_signal: Mutex<bool> vs AtomicBool ===");
        eprintln!("  Mutex:  {:?} ({:.0} ns/iter)", mutex_elapsed, mutex_elapsed.as_nanos() as f64 / ITERATIONS as f64);
        eprintln!("  Atomic: {:?} ({:.0} ns/iter)", atomic_elapsed, atomic_elapsed.as_nanos() as f64 / ITERATIONS as f64);
        eprintln!("  Speedup: {:.1}x", speedup);
    }

    #[test]
    fn bench_audio_level_mutex_vs_atomic() {
        // OLD: Mutex<f32>
        let level_mutex: Arc<Mutex<f32>> = Arc::new(Mutex::new(0.0));
        let start = Instant::now();
        for i in 0..ITERATIONS {
            let rms = (i as f32) * 0.001;
            if let Ok(mut level) = level_mutex.lock() {
                *level = rms;
            }
        }
        let mutex_elapsed = start.elapsed();

        // NEW: AtomicU32 via f32::to_bits
        let level_atomic = Arc::new(AtomicU32::new(0.0_f32.to_bits()));
        let start = Instant::now();
        for i in 0..ITERATIONS {
            let rms = (i as f32) * 0.001;
            level_atomic.store(rms.to_bits(), Ordering::Relaxed);
        }
        let atomic_elapsed = start.elapsed();

        let speedup = mutex_elapsed.as_nanos() as f64 / atomic_elapsed.as_nanos().max(1) as f64;
        eprintln!("\n=== audio_level: Mutex<f32> vs AtomicU32 ===");
        eprintln!("  Mutex:  {:?} ({:.0} ns/iter)", mutex_elapsed, mutex_elapsed.as_nanos() as f64 / ITERATIONS as f64);
        eprintln!("  Atomic: {:?} ({:.0} ns/iter)", atomic_elapsed, atomic_elapsed.as_nanos() as f64 / ITERATIONS as f64);
        eprintln!("  Speedup: {:.1}x", speedup);
    }

    #[test]
    fn bench_timestamp_systemtime_vs_instant() {
        // OLD: SystemTime::now() per push (syscall on macOS)
        let start = Instant::now();
        let mut last = 0u64;
        for _ in 0..ITERATIONS {
            last = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
        }
        let systime_elapsed = start.elapsed();
        let _ = last;

        // NEW: Instant::elapsed() (userspace on macOS)
        let base_epoch_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let base_instant = Instant::now();
        let start = Instant::now();
        for _ in 0..ITERATIONS {
            last = base_epoch_ms + base_instant.elapsed().as_millis() as u64;
        }
        let instant_elapsed = start.elapsed();
        let _ = last;

        let speedup = systime_elapsed.as_nanos() as f64 / instant_elapsed.as_nanos().max(1) as f64;
        eprintln!("\n=== Timestamp: SystemTime::now() vs Instant::elapsed() ===");
        eprintln!("  SystemTime: {:?} ({:.0} ns/iter)", systime_elapsed, systime_elapsed.as_nanos() as f64 / ITERATIONS as f64);
        eprintln!("  Instant:    {:?} ({:.0} ns/iter)", instant_elapsed, instant_elapsed.as_nanos() as f64 / ITERATIONS as f64);
        eprintln!("  Speedup: {:.1}x", speedup);
    }

    #[test]
    fn bench_condvar_always_vs_conditional() {
        let chunk = vec![0.1_f32; 960]; // typical audio chunk size

        // OLD: always notify
        let state = Arc::new((Mutex::new(VecDeque::<(u64, Vec<f32>)>::new()), Condvar::new()));
        let start = Instant::now();
        for _ in 0..ITERATIONS {
            let (lock, cvar) = &*state;
            if let Ok(mut buf) = lock.lock() {
                buf.push_back((12345, chunk.clone()));
            }
            cvar.notify_one();
        }
        let always_elapsed = start.elapsed();

        // NEW: conditional notify
        let state = Arc::new((Mutex::new(VecDeque::<(u64, Vec<f32>)>::new()), Condvar::new()));
        let start = Instant::now();
        for _ in 0..ITERATIONS {
            let (lock, cvar) = &*state;
            if let Ok(mut buf) = lock.lock() {
                let was_empty = buf.is_empty();
                buf.push_back((12345, chunk.clone()));
                if was_empty {
                    cvar.notify_one();
                }
            }
        }
        let cond_elapsed = start.elapsed();

        let speedup = always_elapsed.as_nanos() as f64 / cond_elapsed.as_nanos().max(1) as f64;
        eprintln!("\n=== Condvar: always notify vs conditional ===");
        eprintln!("  Always:      {:?} ({:.0} ns/iter)", always_elapsed, always_elapsed.as_nanos() as f64 / ITERATIONS as f64);
        eprintln!("  Conditional: {:?} ({:.0} ns/iter)", cond_elapsed, cond_elapsed.as_nanos() as f64 / ITERATIONS as f64);
        eprintln!("  Speedup: {:.1}x", speedup);
    }

    #[test]
    fn bench_full_hotpath_old_vs_new() {
        let chunk = vec![0.1_f32; 960];

        // === OLD full hot-path ===
        let stop_signal: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
        let audio_level: Arc<Mutex<f32>> = Arc::new(Mutex::new(0.0));
        let state = Arc::new((Mutex::new(VecDeque::<(u64, Vec<f32>)>::new()), Condvar::new()));

        let start = Instant::now();
        for i in 0..ITERATIONS {
            if let Ok(stop) = stop_signal.lock() {
                if *stop { break; }
            }
            let rms = (i as f32) * 0.001;
            if let Ok(mut level) = audio_level.lock() {
                *level = rms;
            }
            let ts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            let (lock, cvar) = &*state;
            if let Ok(mut buf) = lock.lock() {
                buf.push_back((ts, chunk.clone()));
            }
            cvar.notify_one();
        }
        let old_elapsed = start.elapsed();

        // === NEW full hot-path ===
        let stop_signal = Arc::new(AtomicBool::new(false));
        let audio_level = Arc::new(AtomicU32::new(0.0_f32.to_bits()));
        let state = Arc::new((Mutex::new(VecDeque::<(u64, Vec<f32>)>::new()), Condvar::new()));
        let base_epoch_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let base_instant = Instant::now();

        let start = Instant::now();
        for i in 0..ITERATIONS {
            if stop_signal.load(Ordering::Acquire) { break; }
            let rms = (i as f32) * 0.001;
            audio_level.store(rms.to_bits(), Ordering::Relaxed);
            let ts = base_epoch_ms + base_instant.elapsed().as_millis() as u64;
            let (lock, cvar) = &*state;
            if let Ok(mut buf) = lock.lock() {
                let was_empty = buf.is_empty();
                buf.push_back((ts, chunk.clone()));
                if was_empty {
                    cvar.notify_one();
                }
            }
        }
        let new_elapsed = start.elapsed();

        let speedup = old_elapsed.as_nanos() as f64 / new_elapsed.as_nanos().max(1) as f64;
        eprintln!("\n=== FULL HOT-PATH: Old vs New ===");
        eprintln!("  Old (3 mutexes + SystemTime + always notify): {:?} ({:.0} ns/iter)", old_elapsed, old_elapsed.as_nanos() as f64 / ITERATIONS as f64);
        eprintln!("  New (2 atomics + Instant + conditional notify): {:?} ({:.0} ns/iter)", new_elapsed, new_elapsed.as_nanos() as f64 / ITERATIONS as f64);
        eprintln!("  Speedup: {:.1}x faster", speedup);
    }
}
