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
