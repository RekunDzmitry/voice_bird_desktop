//! `oh-my-pi` (omp) integration surface.
//!
//! Voice-bird-cli plays the role of MCP *server*. A user's `omp` process
//! is the MCP *client*; voice-bird-cli registers itself in
//! `~/.omp/agent/mcp.json` on first launch and exposes tools
//! `voice_bird__push_segment` and `voice_bird__pull_recent` on its
//! stdin/stdout when omp spawns it via `--mcp-server`. See
//! `docs/plans/omp-integration.md` for the full rationale and the
//! commit-by-commit rollout this module implements.
//!
//! The transport is hidden behind the [`OmpTarget`] trait so that
//! single-user desktop Phase A stays simple, and multi-agent / multi-
//! project Phase B can swap transports (Kafka, collab relay, etc.)
//! without touching the slot model or the renderer.

pub mod install;
pub mod mcp_server;
pub mod register;
pub mod rpc;
pub mod session;

pub use install::{detect, OmpDetection, OmpDetectionSource};
pub use mcp_server::resolve_initial_session_id;
pub use session::{OmpSessionId, OmpStatus, OmpTarget};
