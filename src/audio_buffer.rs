use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// Maximum number of chunks to buffer (~5 seconds at ~100 chunks/sec)
const MAX_BUFFER_CHUNKS: usize = 500;

struct SharedState {
    buffer: VecDeque<Vec<f32>>,
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
        AudioProducer {
            state: self.state.clone(),
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

/// Producer end — used by audio callbacks to push sample chunks.
/// Cloneable so multiple callbacks can share it.
#[derive(Clone)]
pub struct AudioProducer {
    state: Arc<(Mutex<SharedState>, Condvar)>,
}

impl AudioProducer {
    /// Push a chunk of audio samples into the buffer.
    /// If the buffer is full, the oldest chunk is dropped to make room.
    pub fn push(&self, samples: Vec<f32>) {
        let (lock, cvar) = &*self.state;
        if let Ok(mut shared) = lock.lock() {
            if shared.stopped {
                return;
            }
            if shared.buffer.len() >= MAX_BUFFER_CHUNKS {
                shared.buffer.pop_front();
            }
            shared.buffer.push_back(samples);
        }
        cvar.notify_one();
    }
}

/// Consumer end — used by the streaming thread to drain buffered audio.
pub struct AudioConsumer {
    state: Arc<(Mutex<SharedState>, Condvar)>,
}

impl AudioConsumer {
    /// Non-blocking: drain all currently buffered chunks.
    pub fn drain_all(&self) -> Vec<Vec<f32>> {
        let (lock, _) = &*self.state;
        if let Ok(mut shared) = lock.lock() {
            shared.buffer.drain(..).collect()
        } else {
            Vec::new()
        }
    }

    /// Blocking: wait up to `timeout` for chunks to arrive, then drain all available.
    /// Returns an empty vec on timeout or if stopped.
    pub fn wait_and_drain(&self, timeout: Duration) -> Vec<Vec<f32>> {
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
        assert_eq!(chunks[0], vec![1.0, 2.0, 3.0]);
        assert_eq!(chunks[1], vec![4.0, 5.0]);

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
        assert_eq!(chunks[0], vec![10.0]);
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
        assert_eq!(chunks[0], vec![1.0, 2.0]);
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
}
