//! `agent` integration surface.
//!
//! Today the agent transport is the user's local `oh-my-pi` (omp)
//! installation: voice-bird-cli registers itself as an MCP *server*
//! in `~/.omp/agent/mcp.json` on first launch and exposes tools
//! `voice_bird__push_segment` and `voice_bird__pull_recent` on its
//! stdin/stdout when omp spawns it via `--mcp-server`. The
//! identifier the TUI surfaces is "Agent" so the picker reads
//! generically — future transports (Kafka, collab relay, etc.)
//! plug into the same [`AgentTarget`] trait without renaming
//! anything in the slot model or the renderer.
//!
//! See `docs/plans/omp-integration.md` for the original rollout
//! notes this module implements.

pub mod install;
pub mod kafka;
pub mod live;
pub mod mcp_server;
pub mod register;
pub mod rpc;
pub mod session;

pub use install::{detect, AgentDetection, AgentDetectionSource};
pub use mcp_server::resolve_initial_session_id;
pub use session::{AgentSessionId, AgentStatus, AgentTarget};
