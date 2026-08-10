//! Where a recording section's transcript is going.
//!
//! `Stdout` is the local path: the section writes `audio.wav`,
//! `transcript.jsonl`, `transcript.json`, and `transcript.txt` into a
//! timestamped directory under the configured `session_dir`.
//!
//! Server streaming is no longer a target — it lives on
//! `Section::settings::cloud_on` as a per-section transport flag.
//! Pairing `cloud_on = true` with `Target::Stdout` is valid: the
//! local files are produced AND the audio is streamed to
//! voicebird.app.
//!
//! The previous `Agent { session_id }` variant (MCP stdio routing
//! to a local oh-my-pi runtime) is gone. Agents are now Characters
//! at voicebird.app, and the run path lives in
//! `src/cloud/run.rs`. The enum stays as a single variant so the
//! `meta.json` on-disk format keeps parsing.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Target {
    /// Local persistence: the section writes `audio.wav`,
    /// `transcript.jsonl`, `transcript.json`, and `transcript.txt`
    /// into a timestamped directory under `session_dir`. The ASR
    /// engine itself is chosen independently by `cloud_on`:
    /// cloud off → local Whisper; cloud on → Voice Bird Web
    /// (PCM streams to voicebird.app and committed segments
    /// round-trip back into the same local writer). Stdout
    /// guarantees a copy on disk in either case.
    Stdout,
}

impl Default for Target {
    fn default() -> Self {
        Target::Stdout
    }
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Stdout")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_stdout() {
        assert_eq!(Target::default(), Target::Stdout);
    }

    #[test]
    fn display_is_user_readable() {
        assert_eq!(Target::Stdout.to_string(), "Stdout");
    }

    #[test]
    fn round_trips_through_json() {
        let json = serde_json::to_string(&Target::Stdout).unwrap();
        let back: Target = serde_json::from_str(&json).unwrap();
        assert_eq!(back, Target::Stdout);
    }
}
