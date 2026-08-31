//! voicebird.app HTTP client (used by the desktop's Agent run path).
//!
//! The desktop never runs voicebird.app locally — it talks to the
//! hosted service over HTTPS. The cloud run path is opt-in and
//! requires:
//!
//!   - the user has set a Voice Bird API key (`AppConfig::voicebird_api_key`),
//!   - the configured server URL (`AppConfig::voicebird_server_url`)
//!     points at a `wss://...` host whose HTTP origin is reachable.
//!
//! The split between `mod http`, `mod agents`, and `mod run`:
//!   - `http` — small helpers (URL translation, header construction).
//!   - `agents` — `GET /api/agents` for the picker; the prompt
//!     template never leaves the server.
//!   - `run` — `POST /api/agent-runs` + SSE consumer for the
//!     `g`-key run path (§11).
//!
//! All three wrap `reqwest::blocking` (already in the tree from
//! `voicebird_engine`'s cloud handshake) so we don't pull a second
//! HTTP client.

pub mod agents;
pub mod http;
pub mod rooms;
pub mod run;

// Re-export the agent-run types so callers can
// `use voice_bird_cli::cloud::AgentRunState;` instead
// of reaching into the deeply-nested run module. The
// UI (App + ui.rs) needs these directly.
pub use run::{
    run_event_loop, run_event_loop_chan, should_run_now, spawn_agent_run, start,
    truncate_transcript, AgentRunError, AgentRunState, RunEvent, RunRequest,
    RunStartError, RunTrigger, AUTO_RUN_FLOOR_SECS, TRANSCRIPT_MAX_CHARS,
};
