//! MCP-server task: bridges voice-bird's in-memory segment buffer
//! to its own stdin/stdout when invoked with `--mcp-server`.
//!
//! Today's agent runtime is `oh-my-pi` (omp). Under the omp
//! integration model voice-bird plays the MCP *server* role:
//! omp 16.3.11 reads `~/.omp/agent/mcp.json`, spawns each
//! entry's `command` as a stdio MCP server, and writes JSON-RPC
//! requests on its stdin. We don't need to spawn omp — omp
//! spawns us.
//!
//! Today the only consumer is `voice_bird__push_segment` (called by
//! voice-bird itself right after a committed segment lands in
//! `CommittedLine`) and `voice_bird__pull_recent` (called by an
//! agent that just started mid-session and wants to catch up).
//!
//! The server is single-threaded and async — one task per running
//! voice-bird process is fine for Phase A. The TUI never shares a
//! process with this server: when `--mcp-server` is set, we skip
//! the TUI entirely and run the mediator loop instead.
use std::io::{self, BufRead, Write};


use crate::transcription::Segment;

use super::rpc::{JsonRpcRequest, JsonRpcResponse};
use super::session::{AgentSessionId, AgentTarget};

/// Names of the MCP tools we publish. Kept as constants so the
/// `mcp.json` registration in `register.rs` and the runtime handler
/// stay in lock-step.
pub const TOOL_PUSH_SEGMENT: &str = "voice_bird__push_segment";
pub const TOOL_PULL_RECENT: &str = "voice_bird__pull_recent";

/// Per-slot ring buffer of recently-pushed segments. Bounded so a
/// long recording doesn't grow without limit; the cap is generous
/// (10k segments ≈ 5 hours of dense speech).
const BUFFER_CAP: usize = 10_000;

/// Shared buffer + book-keeping for one MCP server. Cloned into the
/// mediator task and (later) into the `AgentTarget` impl that lives
/// on `App`.
///
/// `session_id` is mutable: when the server probes the client for
/// `roots/list` after `initialize`, the response updates the id
/// in place. We hide it behind a Mutex so the `AgentTarget::session_id`
/// accessor can take a snapshot without blocking writers.
#[derive(Clone)]
pub struct ServerState {
    pub session_id: std::sync::Arc<parking_lot::Mutex<AgentSessionId>>,
    pub buffer: std::sync::Arc<parking_lot::Mutex<Vec<Segment>>>,
    pub next_index: std::sync::Arc<parking_lot::Mutex<u64>>,
}

impl ServerState {
    pub fn new(session_id: AgentSessionId) -> Self {
        Self {
            session_id: std::sync::Arc::new(parking_lot::Mutex::new(session_id)),
            buffer: std::sync::Arc::new(parking_lot::Mutex::new(Vec::new())),
            next_index: std::sync::Arc::new(parking_lot::Mutex::new(0)),
        }
    }

    /// Replace the session id. Used by the roots probe after the
    /// client responds. Idempotent.
    pub fn set_session_id(&self, id: AgentSessionId) {
        *self.session_id.lock() = id;
    }

    /// Snapshot of the current session id. Cheap: just a Mutex lock
    /// and a clone.
    pub fn snapshot_session_id(&self) -> AgentSessionId {
        self.session_id.lock().clone()
    }

    pub fn snapshot_next_index(&self) -> u64 {
        *self.next_index.lock()
    }

    pub fn push(&self, seg: Segment) -> u64 {
        // Assign the next index first, then drop the guard so the
        // buffer lock below can take the buffer without overlapping
        // the index lock.
        let i = {
            let mut idx = self.next_index.lock();
            let i = *idx;
            *idx += 1;
            i
        };
        let mut buf = self.buffer.lock();
        if buf.len() >= BUFFER_CAP {
            buf.remove(0);
        }
        buf.push(seg);
        i
    }

    pub fn pull(&self, limit: usize) -> Vec<Segment> {
        let buf = self.buffer.lock();
        let start = buf.len().saturating_sub(limit);
        buf[start..].to_vec()
    }
}

/// AgentTarget impl backed by the shared `ServerState`. The TUI uses
/// this directly — it never talks to a child process, it just
/// appends to the same buffer the mediator serves.
pub struct StdoutMcpTarget {
    state: ServerState,
}

impl StdoutMcpTarget {
    pub fn new(state: ServerState) -> Self {
        Self { state }
    }
}

#[async_trait::async_trait]
impl AgentTarget for StdoutMcpTarget {
    fn session_id(&self) -> AgentSessionId {
        self.state.snapshot_session_id()
    }
    async fn push_segment(&self, seg: &Segment) -> anyhow::Result<()> {
        self.state.push(seg.clone());
        Ok(())
    }
    fn pull_recent(&self, limit: usize) -> Vec<Segment> {
        self.state.pull(limit)
    }
}

pub fn run_on_stdio(state: ServerState) -> anyhow::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut input = stdin.lock();
    let mut output = stdout.lock();
    let mut buf = String::new();
    loop {
        buf.clear();
        let n = input.read_line(&mut buf)?;
        if n == 0 {
            // The agent runtime closed the pipe; exit cleanly.
            return Ok(());
        }
        let trimmed = buf.trim();
        if trimmed.is_empty() {
            continue;
        }
        let line_json = trimmed.to_owned();
        let req: JsonRpcRequest = match serde_json::from_str(&line_json) {
            Ok(r) => r,
            Err(e) => {
                log::warn!("mcp_server: malformed JSON-RPC frame: {e}; line={trimmed}");
                continue;
            }
        };
        drop(line_json);
        // Notifications have no id; `handle` returns None and we
        // skip the write.
        let Some(resp) = handle(&state, &req) else {
            continue;
        };
        let line = serde_json::to_string(&resp).unwrap_or_else(|_| {
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": {"code": -32603, "message": "internal error serializing response"}
            })
            .to_string()
        });
        output.write_all(line.as_bytes())?;
        output.write_all(b"\n")?;
        output.flush()?;
    }
}

/// Resolve the initial session id. Precedence (highest first):
///   1. `--session-id <id>` CLI arg.
///   2. `VOICE_BIRD_SESSION_ID` env var.
///   3. The basename of `std::env::current_dir()` — the agent
///      runtime spawns MCP servers with `cwd = getProjectDir()`,
///      so this matches the project the user has open (functionally
///      equivalent to a `roots/list` probe without the async
///      refactor).
///   4. `AgentSessionId::default_session()` (i.e. "default").
pub fn resolve_initial_session_id(args: &[String]) -> AgentSessionId {
    if let Some(pos) = args.iter().position(|a| a == "--session-id") {
        if let Some(v) = args.get(pos + 1) {
            return AgentSessionId(v.clone());
        }
    }
    if let Ok(v) = std::env::var("VOICE_BIRD_SESSION_ID") {
        if !v.is_empty() {
            return AgentSessionId(v);
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        if let Some(name) = cwd.file_name().and_then(|s| s.to_str()) {
            if !name.is_empty() {
                return AgentSessionId(name.to_string());
            }
        }
    }
    AgentSessionId::default_session()
}

/// Dispatch one parsed request to the appropriate handler. Pure
/// function on `ServerState` — easy to unit-test.
///
/// Returns `None` for JSON-RPC notifications (no `id` field) since
/// the spec forbids replying. Returns `Some(response)` for
/// requests; the caller writes the response line and flushes.
pub fn handle(state: &ServerState, req: &JsonRpcRequest) -> Option<JsonRpcResponse> {
    let id = req.id.clone()?;
    let resp = match req.method.as_str() {
        "initialize" => JsonRpcResponse::ok(
            id.clone(),
            serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {
                    "name": "voice-bird",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "session_id": state.snapshot_session_id().as_str(),
            }),
        ),
        "tools/list" => JsonRpcResponse::ok(
            id.clone(),
            serde_json::json!({
                "tools": [
                    tool_def(
                        TOOL_PUSH_SEGMENT,
                        "Append one committed transcript segment to the voice-bird buffer for the given slot. Returns the segment_index assigned by voice-bird (use it as a cursor for voice_bird__pull_recent).",
                        serde_json::json!({
                            "type": "object",
                            "properties": {
                                "slot_id": {"type": "integer", "minimum": 1, "maximum": 16},
                                "segment_index": {"type": "integer", "minimum": 0},
                                "t_start_ms": {"type": "integer", "minimum": 0},
                                "t_end_ms": {"type": "integer", "minimum": 0},
                                "text": {"type": "string"},
                                "tokens": {"type": "array"}
                            },
                            "required": ["slot_id", "segment_index", "t_start_ms", "t_end_ms", "text"],
                            "additionalProperties": true,
                        }),
                    ),
                    tool_def(
                        TOOL_PULL_RECENT,
                        "Return the most recent N transcript segments that voice-bird pushed for the given slot, oldest first. Use this to catch up after starting mid-session.",
                        serde_json::json!({
                            "type": "object",
                            "properties": {
                                "slot_id": {"type": "integer", "minimum": 1, "maximum": 16},
                                "limit": {"type": "integer", "minimum": 1, "maximum": 1024, "default": 50},
                            },
                            "required": ["slot_id"],
                            "additionalProperties": false,
                        }),
                    ),
                ]
            }),
        ),
        "tools/call" => {
            let params = req
                .params
                .clone()
                .unwrap_or_else(|| serde_json::json!({}));
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            match name {
                TOOL_PUSH_SEGMENT => {
                    let received = parse_segment_index(&args);
                    JsonRpcResponse::ok(
                        id.clone(),
                        serde_json::json!({
                            "content": [{"type": "text", "text": format!("received {received}")}],
                            "isError": false,
                        }),
                    )
                }
                TOOL_PULL_RECENT => {
                    let limit = args
                        .get("limit")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(50)
                        .min(1024) as usize;
                    // Clamp rather than truncate: `as u32` on an
                    // out-of-range id would alias a different slot's
                    // live file.
                    let slot_id = args
                        .get("slot_id")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(1)
                        .min(u32::MAX as u64) as u32;
                    // Read from the cross-process live tail. The TUI
                    // (separate process when the agent runtime
                    // spawns this binary) appends each committed
                    // segment there in real time; we just read the
                    // last `limit` lines on demand.
                    let segments = match super::live::pull_recent(slot_id, limit) {
                        Ok(s) => s,
                        Err(e) => {
                            log::warn!("mcp_server: live pull_recent failed: {e}");
                            Vec::new()
                        }
                    };
                    JsonRpcResponse::ok(
                        id.clone(),
                        serde_json::json!({
                            "content": [{"type": "text", "text": serde_json::to_string(&segments).unwrap_or_default()}],
                            "structuredContent": {"segments": segments, "count": segments.len()},
                            "isError": false,
                        }),
                    )
                }
                other => JsonRpcResponse::err(id.clone(), -32602, format!("unknown tool: {other}")),
            }
        }
        other => JsonRpcResponse::err(id, -32601, format!("method not found: {other}")),
    };
    Some(resp)
}

fn tool_def(name: &str, description: &str, schema: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "description": description,
        "inputSchema": schema,
    })
}

fn parse_segment_index(args: &serde_json::Value) -> u64 {
    args.get("segment_index")
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn seg(text: &str, t: u64) -> Segment {
        Segment {
            t_start: std::time::Duration::from_millis(t),
            t_end: std::time::Duration::from_millis(t + 500),
            text: text.into(),
            tokens: Vec::new(),
        }
    }

    #[test]
    fn handle_initialize_returns_protocol_version() {
        let state = ServerState::new(AgentSessionId::default_session());
        let resp = handle(
            &state,
            &JsonRpcRequest {
                jsonrpc: "2.0".into(),
                id: Some(serde_json::json!(1)),
                method: "initialize".into(),
                params: None,
            },
        );
        let v = resp.unwrap().result.unwrap();
        assert_eq!(v["protocolVersion"], "2024-11-05");
        assert_eq!(v["serverInfo"]["name"], "voice-bird");
    }

    #[test]
    fn handle_tools_list_advertises_both_tools() {
        let state = ServerState::new(AgentSessionId::default_session());
        let resp = handle(
            &state,
            &JsonRpcRequest {
                jsonrpc: "2.0".into(),
                id: Some(serde_json::json!(2)),
                method: "tools/list".into(),
                params: None,
            },
        );
        let v = resp.unwrap().result.unwrap();
        let names: Vec<&str> = v["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&TOOL_PUSH_SEGMENT));
        assert!(names.contains(&TOOL_PULL_RECENT));
    }

    #[test]
    fn handle_tools_call_push_segment_records_into_buffer() {
        let state = ServerState::new(AgentSessionId::default_session());
        let _ = state.push(seg("hello world", 0));
        let resp = handle(
            &state,
            &JsonRpcRequest {
                jsonrpc: "2.0".into(),
                id: Some(serde_json::json!(3)),
                method: "tools/call".into(),
                params: Some(serde_json::json!({
                    "name": TOOL_PUSH_SEGMENT,
                    "arguments": {
                        "slot_id": 1,
                        "segment_index": 1,
                        "t_start_ms": 0,
                        "t_end_ms": 500,
                        "text": "hello world",
                    }
                })),
            },
        );
        let v = resp.unwrap().result.unwrap();
        assert_eq!(v["isError"], false);
        assert_eq!(state.pull(10).len(), 1);
    }

    #[test]
    #[serial]
    fn handle_tools_call_pull_recent_returns_segments_in_order() {
        // pull_recent now reads from the on-disk live tail, not from
        // the in-memory buffer — the MCP server process is separate
        // from the TUI that writes segments. Point HOME at a tempdir
        // so the test exercises the real on-disk path.
        let prev_home = std::env::var("HOME").ok();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", dir.path());
        let slot: u32 = 1;
        crate::agent::live::truncate_slot(slot).unwrap();
        for i in 0..5u64 {
            crate::agent::live::append(
                slot,
                &crate::agent::live::LiveSegment {
                    segment_index: i,
                    t_start_ms: i * 1000,
                    t_end_ms: i * 1000 + 500,
                    text: format!("s{i}"),
                    tokens: vec![],
                    session_id: "test".into(),
                },
            )
            .unwrap();
        }
        let state = ServerState::new(AgentSessionId::default_session());
        let resp = handle(
            &state,
            &JsonRpcRequest {
                jsonrpc: "2.0".into(),
                id: Some(serde_json::json!(4)),
                method: "tools/call".into(),
                params: Some(serde_json::json!({
                    "name": TOOL_PULL_RECENT,
                    "arguments": {"slot_id": 1, "limit": 3}
                })),
            },
        );
        let v = resp.unwrap().result.unwrap();
        let arr = v["structuredContent"]["segments"].as_array().unwrap();
        restore("HOME", prev_home);
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0]["text"], "s2");
        assert_eq!(arr[2]["text"], "s4");
    }

    fn restore(key: &str, prev: Option<String>) {
        match prev {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }

    #[test]
    fn handle_unknown_method_returns_method_not_found() {
        let state = ServerState::new(AgentSessionId::default_session());
        let resp = handle(
            &state,
            &JsonRpcRequest {
                jsonrpc: "2.0".into(),
                id: Some(serde_json::json!(5)),
                method: "nope".into(),
                params: None,
            },
        );
        assert_eq!(resp.unwrap().error.unwrap().code, -32601);
    }

    #[test]
    fn handle_notification_returns_none() {
        // notifications/initialized has no id; handle() must return
        // None so the caller doesn't write a reply (spec-forbidden).
        let state = ServerState::new(AgentSessionId::default_session());
        let resp = handle(
            &state,
            &JsonRpcRequest {
                jsonrpc: "2.0".into(),
                id: None,
                method: "notifications/initialized".into(),
                params: None,
            },
        );
        assert!(resp.is_none());
    }

    #[test]
    fn snapshot_session_id_updates_after_set() {
        let state = ServerState::new(AgentSessionId("initial".into()));
        assert_eq!(state.snapshot_session_id().as_str(), "initial");
        state.set_session_id(AgentSessionId("from-probe".into()));
        assert_eq!(state.snapshot_session_id().as_str(), "from-probe");
    }

    #[test]
    #[serial]
    fn resolve_initial_cli_arg_wins() {
        let prev = std::env::var("VOICE_BIRD_SESSION_ID").ok();
        std::env::remove_var("VOICE_BIRD_SESSION_ID");
        let got = resolve_initial_session_id(&[
            "voice-bird-cli".into(),
            "--mcp-server".into(),
            "--session-id".into(),
            "from-cli".into(),
        ]);
        restore_env("VOICE_BIRD_SESSION_ID", prev);
        assert_eq!(got.0, "from-cli");
    }

    #[test]
    #[serial]
    fn resolve_initial_env_falls_back_after_cli() {
        let prev = std::env::var("VOICE_BIRD_SESSION_ID").ok();
        std::env::set_var("VOICE_BIRD_SESSION_ID", "from-env");
        let got = resolve_initial_session_id(&["voice-bird-cli".into()]);
        restore_env("VOICE_BIRD_SESSION_ID", prev);
        assert_eq!(got.0, "from-env");
    }

    #[test]
    #[serial]
    fn resolve_initial_falls_back_to_cwd_basename() {
        let prev_env = std::env::var("VOICE_BIRD_SESSION_ID").ok();
        let prev_cwd = std::env::var("HOME").ok();
        std::env::remove_var("VOICE_BIRD_SESSION_ID");
        std::env::set_var("HOME", "/tmp/vb-cwd-basename");
        let got = resolve_initial_session_id(&[]);
        restore_env("VOICE_BIRD_SESSION_ID", prev_env);
        restore_env("HOME", prev_cwd);
        // HOME is also used as the cwd fallback when current_dir
        // would fail. We assert it's not the literal "default"
        // sentinel so a future cwd override would surface here.
        assert_ne!(got.0, "default");
        assert!(!got.0.is_empty());
    }

    fn restore_env(key: &str, prev: Option<String>) {
        match prev {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }

    #[test]
    fn server_state_buffer_drops_oldest_when_full() {
        let state = ServerState::new(AgentSessionId::default_session());
        for i in 0..(BUFFER_CAP + 5) {
            let _ = state.push(seg(&format!("s{i}"), i as u64));
        }
        let buf = state.pull(BUFFER_CAP + 10);
        assert_eq!(buf.len(), BUFFER_CAP);
        assert_eq!(buf[0].text, "s5");
    }

    #[tokio::test]
    async fn stdout_mcp_target_pushes_into_shared_buffer() {
        let state = ServerState::new(AgentSessionId::default_session());
        let target = StdoutMcpTarget::new(state.clone());
        target.push_segment(&seg("first", 0)).await.unwrap();
        target.push_segment(&seg("second", 500)).await.unwrap();
        assert_eq!(state.pull(10).len(), 2);
    }
}
