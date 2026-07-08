//! Where a recording section's transcript is going.
//!
//! `Stdout` is the local path: the section writes `audio.wav`,
//! `transcript.jsonl`, `transcript.json`, and `transcript.txt` into a
//! timestamped directory under the configured `session_dir`. `Cloud`
//! streams audio to the Voice Bird Web backend and skips local files.
//!
//! Phase A wires the enum into the data model; the agent-flavoured
//! variants (Claude / Cursor / etc.) are reserved for Phase C and
//! intentionally absent here so today's rendering surface stays small
//! and provable.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Target {
    /// Local Whisper inference — transcript is written to disk and
    /// stays on the user's machine.
    Stdout,
    /// Audio is streamed to the Voice Bird Web backend. No local
    /// `transcript.*` files; the API key in `config.toml` is used for
    /// authentication.
    Cloud,
}

impl Default for Target {
    fn default() -> Self {
        Target::Stdout
    }
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Target::Stdout => f.write_str("Stdout"),
            Target::Cloud => f.write_str("Cloud"),
        }
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
        assert_eq!(Target::Cloud.to_string(), "Cloud");
    }

    #[test]
    fn round_trips_through_serde() {
        let json = serde_json::to_string(&Target::Cloud).unwrap();
        let back: Target = serde_json::from_str(&json).unwrap();
        assert_eq!(back, Target::Cloud);
    }
}
