//! First-run heuristic that picks a local Whisper model based on system
//! specs. The result is written to `AppConfig::default_model` and saved
//! to disk so subsequent launches skip detection. Manual override via the
//! model picker (`m`) remains available.

use sysinfo::System;

/// Inspect the host and return the catalog id of the model to start with.
/// Pure-function inner [`pick`] is unit-testable; this wrapper just feeds
/// it real system values.
pub fn pick_default_model() -> &'static str {
    let mut sys = System::new();
    sys.refresh_memory();
    let bytes = sys.total_memory();
    // sysinfo reports `total_memory` in bytes since 0.30. Convert to GiB
    // with truncating division — a 16 GiB machine reports something like
    // 17 179 869 184 bytes, so 17 GiB > 16 GiB threshold lands cleanly.
    let ram_gb = (bytes / (1024 * 1024 * 1024)) as u32;
    let apple_silicon = cfg!(all(target_os = "macos", target_arch = "aarch64"));
    pick(ram_gb, apple_silicon)
}

/// Map (RAM in whole GiB, is-Apple-Silicon) onto a model id from the
/// [`crate::transcription::models::Catalog`]. The thresholds were chosen
/// to keep the streaming engine real-time on the bottom rung of each
/// bucket — distil-small.en still streams comfortably with 16 GiB; the
/// 1.5 GB multilingual / turbo models are reserved for high-RAM Apple
/// Silicon where Metal makes them tractable.
fn pick(ram_gb: u32, apple_silicon: bool) -> &'static str {
    if ram_gb < 8 {
        "tiny.en"
    } else if ram_gb < 16 {
        "base.en"
    } else if ram_gb < 32 {
        "distil-small.en"
    } else if apple_silicon {
        "large-v3-turbo"
    } else {
        "distil-small.en"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_tiny_for_low_ram() {
        assert_eq!(pick(4, false), "tiny.en");
        assert_eq!(pick(7, true), "tiny.en");
    }

    #[test]
    fn picks_base_for_mid_ram() {
        assert_eq!(pick(8, false), "base.en");
        assert_eq!(pick(15, true), "base.en");
    }

    #[test]
    fn picks_distil_small_for_16gb() {
        assert_eq!(pick(16, false), "distil-small.en");
        assert_eq!(pick(31, true), "distil-small.en");
    }

    #[test]
    fn picks_large_turbo_for_apple_silicon_high_ram() {
        assert_eq!(pick(32, true), "large-v3-turbo");
        assert_eq!(pick(64, true), "large-v3-turbo");
    }

    #[test]
    fn picks_distil_small_for_x86_high_ram() {
        // No Metal acceleration on x86; large multilingual models would
        // be slower than real-time, so we stay on distil-small.en.
        assert_eq!(pick(32, false), "distil-small.en");
        assert_eq!(pick(128, false), "distil-small.en");
    }

    #[test]
    fn pick_default_model_returns_a_known_catalog_id() {
        // Smoke-test: whatever the host has, the picked id must exist
        // in the catalog so AppConfig::default_model stays in sync.
        let id = pick_default_model();
        let catalog = crate::transcription::models::Catalog::builtin();
        assert!(
            catalog.get(id).is_some(),
            "auto-pick returned unknown id: {id}"
        );
    }
}
