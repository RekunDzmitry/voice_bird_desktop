//! Startup concurrency heuristics, not memory-allocation guarantees or benchmark optima.

use sysinfo::System;

use crate::transcribe::DECODER_THREADS;

const GIB: u64 = 1024 * 1024 * 1024;
const RESERVED_MEMORY_BYTES: u64 = 4 * GIB;
const MEMORY_PER_CALL_BYTES: u64 = 2 * GIB;
// Shared accelerators can contend even when CPU and RAM budgets are plentiful.
const MAX_CALLS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InferenceLimits {
    workers: usize,
    recording: usize,
    shutdown: usize,
}

impl InferenceLimits {
    pub fn detect() -> Self {
        let cpu_threads = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1);
        let mut system = System::new();
        system.refresh_memory();
        Self::for_system(cpu_threads, system.total_memory())
    }

    /// Derive startup budgets from logical CPU threads and total RAM in bytes.
    /// Unknown RAM (zero) and undersized hosts retain one inference slot so
    /// transcription can progress; recording reserves one decoder's CPU budget.
    pub fn for_system(cpu_threads: usize, memory_bytes: u64) -> Self {
        let full_cpu = (cpu_threads / DECODER_THREADS).max(1);
        let recording_cpu = (cpu_threads.saturating_sub(DECODER_THREADS) / DECODER_THREADS).max(1);
        // Cap before converting to usize, including on 32-bit hosts.
        let memory = (memory_bytes.saturating_sub(RESERVED_MEMORY_BYTES) / MEMORY_PER_CALL_BYTES)
            .clamp(1, MAX_CALLS as u64) as usize;
        let shutdown = full_cpu.min(memory).min(MAX_CALLS);
        let recording = recording_cpu.min(shutdown);
        Self {
            workers: shutdown,
            recording,
            shutdown,
        }
    }

    pub fn workers(&self) -> usize {
        self.workers
    }

    pub fn recording(&self) -> usize {
        self.recording
    }

    pub fn shutdown(&self) -> usize {
        self.shutdown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_limits(cpu: usize, ram: u64, recording: usize, shutdown: usize) {
        let limits = InferenceLimits::for_system(cpu, ram);
        assert_eq!(limits.recording(), recording, "cpu={cpu}, ram={ram}");
        assert_eq!(limits.shutdown(), shutdown, "cpu={cpu}, ram={ram}");
        assert_eq!(limits.workers(), shutdown, "cpu={cpu}, ram={ram}");
    }

    #[test]
    fn unknown_or_insufficient_ram_allows_only_one_call() {
        for ram in [0, 1, 4 * GIB - 1, 4 * GIB, 6 * GIB - 1] {
            assert_limits(usize::MAX, ram, 1, 1);
        }
    }

    #[test]
    fn cpu_pair_boundaries_and_recording_reservation() {
        for (cpu, recording, shutdown) in [
            (0, 1, 1),
            (1, 1, 1),
            (2, 1, 1),
            (3, 1, 1),
            (4, 1, 2),
            (5, 1, 2),
            (6, 2, 3),
            (7, 2, 3),
            (8, 3, 4),
            (15, 6, 7),
            (16, 7, 8),
            (17, 7, 8),
            (18, 8, 8),
        ] {
            assert_limits(cpu, u64::MAX, recording, shutdown);
        }
    }

    #[test]
    fn ram_budget_changes_only_at_complete_two_gib_slots() {
        for slots in 2..=8 {
            let threshold = RESERVED_MEMORY_BYTES + slots * MEMORY_PER_CALL_BYTES;
            assert_limits(
                usize::MAX,
                threshold - 1,
                slots as usize - 1,
                slots as usize - 1,
            );
            assert_limits(usize::MAX, threshold, slots as usize, slots as usize);
            assert_limits(usize::MAX, threshold + 1, slots as usize, slots as usize);
        }
        assert_limits(usize::MAX, 6 * GIB, 1, 1);
    }

    #[test]
    fn memory_and_cpu_budgets_jointly_bound_concurrency() {
        assert_limits(8, 8 * GIB, 2, 2);
        assert_limits(8, 10 * GIB, 3, 3);
        assert_limits(8, 12 * GIB, 3, 4);
        assert_limits(8, u64::MAX, 3, 4);
    }

    #[test]
    fn extreme_profiles_are_capped_and_preserve_ordering() {
        assert_limits(usize::MAX, u64::MAX, 8, 8);
        for cpu in [0, 1, 2, 3, 4, 8, 16, 18, usize::MAX] {
            for ram in [0, 1, 4 * GIB, 8 * GIB - 1, 8 * GIB, 20 * GIB, u64::MAX] {
                let limits = InferenceLimits::for_system(cpu, ram);
                assert!(1 <= limits.recording());
                assert!(limits.recording() <= limits.shutdown());
                assert!(limits.shutdown() <= limits.workers());
                assert!(limits.workers() <= MAX_CALLS);
            }
        }
    }
}
