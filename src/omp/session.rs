//! [`OmpTarget`] trait — the abstraction over "push a committed
//! transcript segment into the user's oh-my-pi session".
//!
//! Implementations are intentionally narrow. Phase A ships one
//! concrete backend (`McpServerTarget` in [`super`]) which spawns the
//! omp child and serialises JSON-RPC frames over its stdin/stdout. Any
//! future backend (Kafka, IPC, unix socket) implements the same four
//! methods and slots into [`OmpStatus`] without touching the rest of
//! the binary.

use crate::transcription::Segment;

/// Stable identifier for an omp session we have attached to. Today we
/// only support one session per `App`, so the value is `"default"`,
/// but the type stays a `String` so the wire protocol can carry an
/// explicit id when Phase B grows multi-session support.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OmpSessionId(pub String);

impl OmpSessionId {
    pub fn default_session() -> Self {
        OmpSessionId("default".into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for OmpSessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Detected state of the local `omp` install. `App::omp` carries one of
/// these so the rest of the program can branch without re-running
/// detection on every key press.
#[derive(Debug, Clone, PartialEq)]
pub enum OmpStatus {
    /// `omp` was found; `path` is the absolute executable and `version`
    /// is the reported version (e.g. `"16.3.11"`).
    Ready { path: std::path::PathBuf, version: String },
    /// `omp` was not found on this machine. The Targets pane still
    /// renders the Omp chip but in a dimmed / disabled state and the
    /// status bar surfaces a hint.
    NotFound,
}

/// Backend-agnostic interface to a single running omp session.
///
/// All four methods are cheap to call (the trait is invoked at most a
/// few times per second — once per committed segment + occasional
/// spawn / drop). Implementations own their own threading and
/// serialisation.
pub trait OmpTarget: Send {
    /// `omp` session this target is attached to.
    fn session_id(&self) -> &OmpSessionId;

    /// Push a committed segment to the omp session. Returns
    /// `Ok(())` once the segment has been handed to the transport
    /// (queued, written to the MCP server's stdin, etc.). A transport
    /// error is logged but not surfaced to the UI — losing one
    /// segment is recoverable; blocking the recording pipeline is
    /// not.
    fn push_segment(&self, segment: &Segment) -> anyhow::Result<()>;

    /// Tentative text accumulated by the engine. Sent as a separate
    /// event so the omp agent can see live progress without committing
    /// a final segment. Best-effort; failures are silent.
    fn push_tentative(&self, _text: &str) -> anyhow::Result<()> {
        Ok(())
    }

    /// Drain the local buffer of segments pushed so far and return
    /// them in arrival order. Used by the `voice_bird__pull_recent`
    /// MCP tool so omp agents can catch up after starting mid-session.
    fn pull_recent(&self, limit: usize) -> Vec<Segment>;

    /// Drop the underlying transport (SIGTERM the omp child, remove
    /// the entry from `mcp.json`, close a Kafka producer, etc.).
    /// Idempotent.
    fn shutdown(&self) {}
}
