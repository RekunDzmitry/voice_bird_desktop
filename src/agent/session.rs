//! [`AgentTarget`] trait — the abstraction over "push a committed
//! transcript segment into the user's agent session".
//!
//! Phase A ships one concrete backend (`McpServerTarget` in
//! [`super`]) which today runs over the user's local `oh-my-pi`
//! (omp) install via MCP stdio JSON-RPC. Any future backend
//! (Kafka, IPC, unix socket) implements the same four methods
//! and slots into [`AgentStatus`] without touching the rest of
//! the binary.

use crate::transcription::Segment;

/// Stable identifier for an agent session. Today we only support
/// one session per `App`, so the value is `"default"`,
/// but the type stays a `String` so the wire protocol can carry an
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AgentSessionId(pub String);

impl AgentSessionId {
    pub fn default_session() -> Self {
        AgentSessionId("default".into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AgentSessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Detected state of the local agent runtime. `App::agent` carries
/// one of these so the rest of the program can branch without
/// re-running detection on every key press. Today the runtime
/// is `oh-my-pi` (omp); future runtimes will return a different
/// backend-shaped status.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentStatus {
    /// `oh-my-pi` was found; `path` is the absolute executable and
    /// `version` is the reported version (e.g. `"16.3.11"`).
    Ready { path: std::path::PathBuf, version: String },
    /// The agent runtime was not found on this machine. The
    /// Targets pane still renders the Agent chip but in a dimmed
    /// / disabled state and the status bar surfaces a hint.
    NotFound,
}

/// Backend-agnostic interface to a single running agent session.
///
/// All four methods are cheap to call (the trait is invoked at most
/// a few times per second — once per committed segment + occasional
/// spawn / drop). Implementations own their own threading and
/// serialisation.
pub trait AgentTarget: Send + Sync {
    /// Agent session this target is attached to. Returns an owned
    /// value because the id may mutate at runtime (the stdio MCP
    /// server updates it after the roots probe); the alternative
    /// `&AgentSessionId` would force a thread-local cache and lose
    /// the snapshot semantics. Callers that need a stable id
    /// should clone the returned value.
    fn session_id(&self) -> AgentSessionId;

    /// Push a committed segment to the agent session. Returns
    /// `Ok(())` once the segment has been handed to the transport
    /// (queued, written to the MCP server's stdin, etc.). A
    /// transport error is logged but not surfaced to the UI —
    /// losing one segment is recoverable; blocking the recording
    /// pipeline is not.
    fn push_segment(&self, segment: &Segment) -> anyhow::Result<()>;

    /// Tentative text accumulated by the engine. Sent as a separate
    /// event so the agent runtime can see live progress without
    /// committing a final segment. Best-effort; failures are
    /// silent.
    fn push_tentative(&self, _text: &str) -> anyhow::Result<()> {
        Ok(())
    }

    /// Drain the local buffer of segments pushed so far and return
    /// them in arrival order. Used by the `voice_bird__pull_recent`
    /// MCP tool so the agent can catch up after starting mid-session.
    fn pull_recent(&self, limit: usize) -> Vec<Segment>;

    /// Drop the underlying transport (SIGTERM the runtime child,
    /// remove the entry from `mcp.json`, close a Kafka producer,
    /// etc.). Idempotent.
    fn shutdown(&self) {}
}
