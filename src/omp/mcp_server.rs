//! MCP-server task: bridges voice-bird's in-memory segment buffer to
//! its own stdin/stdout when invoked with `--mcp-server`.
//!
//! Under the `oh-my-pi` integration model voice-bird plays the MCP
//! *server* role. omp 16.3.11 reads `~/.omp/agent/mcp.json`, spawns
//! each entry's `command` as a stdio MCP server, and writes JSON-RPC
//! requests on its stdin. We don't need to spawn omp ourselves —
//! omp spawns us.
//!
//! Today the only consumer is `voice_bird__push_segment` (called by
//! voice-bird itself right after a committed segment lands in
//! `CommittedLine`) and `voice_bird__pull_recent` (called by an omp
//! agent that just started mid-session and wants to catch up).
//!
//! The server is single-threaded and async — one task per running
//! voice-bird process is fine for Phase A. The TUI never shares a
//! process with this server: when `--mcp-server` is set, we skip
//! the TUI entirely and run the mediator loop instead.

use std::io::{self, BufRead, Write};

use parking_lot::Mutex;

use crate::transcription::Segment;

use super::rpc::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
use super::session::{OmpSessionId, OmpTarget};

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
/// mediator task and (later) into the `OmpTarget` impl that lives
/// on `App`.
#[derive(Clone)]
pub struct ServerState {
    pub session_id: OmpSessionId,
    pub buffer: std::sync::Arc<Mutex<Vec<Segment>>>,
    pub next_index: std::sync::Arc<Mutex<u64>>,
}

impl ServerState {
    pub fn new(session_id: OmpSessionId) -> Self {
        Self {
            session_id,
            buffer: std::sync::Arc::new(Mutex::new(Vec::new())),
            next_index: std::sync::Arc::new(Mutex::new(0)),
        }
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

/// OmpTarget impl backed by the shared `ServerState`. The TUI uses
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

impl OmpTarget for StdoutMcpTarget {
    fn session_id(&self) -> &OmpSessionId {
        &self.state.session_id
    }
    fn push_segment(&self, seg: &Segment) -> anyhow::Result<()> {
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
            // omp closed the pipe; exit cleanly.
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
        let resp = handle(&state, &req);
        let line = serde_json::to_string(&resp).unwrap_or_else(|_| {
            serde_json::to_string(&JsonRpcResponse::err(
                req.id,
                -32603,
                "internal error serializing response",
            ))
            .unwrap_or_default()
        });
        output.write_all(line.as_bytes())?;
        output.write_all(b"\n")?;
        output.flush()?;
    }
}

/// Dispatch one parsed request to the appropriate handler. Pure
/// function on `ServerState` — easy to unit-test.
pub fn handle(state: &ServerState, req: &JsonRpcRequest) -> JsonRpcResponse {
    match req.method.as_str() {
        "initialize" => JsonRpcResponse::ok(
            req.id,
            serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {
                    "name": "voice-bird",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "session_id": state.session_id.as_str(),
            }),
        ),
        "tools/list" => JsonRpcResponse::ok(
            req.id,
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
                        req.id,
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
                    let segments = state.pull(limit);
                    JsonRpcResponse::ok(
                        req.id,
                        serde_json::json!({
                            "content": [{"type": "text", "text": serde_json::to_string(&segments).unwrap_or_default()}],
                            "structuredContent": {"segments": segments, "count": segments.len()},
                            "isError": false,
                        }),
                    )
                }
                other => JsonRpcResponse::err(req.id, -32602, format!("unknown tool: {other}")),
            }
        }
        other => JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: req.id,
            result: None,
            error: Some(JsonRpcError {
                code: -32601,
                message: format!("method not found: {other}"),
                data: None,
            }),
        },
    }
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
        let state = ServerState::new(OmpSessionId::default_session());
        let resp = handle(
            &state,
            &JsonRpcRequest {
                jsonrpc: "2.0".into(),
                id: 1,
                method: "initialize".into(),
                params: None,
            },
        );
        let v = resp.result.unwrap();
        assert_eq!(v["protocolVersion"], "2024-11-05");
        assert_eq!(v["serverInfo"]["name"], "voice-bird");
    }

    #[test]
    fn handle_tools_list_advertises_both_tools() {
        let state = ServerState::new(OmpSessionId::default_session());
        let resp = handle(
            &state,
            &JsonRpcRequest {
                jsonrpc: "2.0".into(),
                id: 2,
                method: "tools/list".into(),
                params: None,
            },
        );
        let v = resp.result.unwrap();
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
        let state = ServerState::new(OmpSessionId::default_session());
        let _ = state.push(seg("hello world", 0));
        let resp = handle(
            &state,
            &JsonRpcRequest {
                jsonrpc: "2.0".into(),
                id: 3,
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
        let v = resp.result.unwrap();
        assert_eq!(v["isError"], false);
        assert_eq!(state.pull(10).len(), 1);
    }

    #[test]
    fn handle_tools_call_pull_recent_returns_segments_in_order() {
        let state = ServerState::new(OmpSessionId::default_session());
        for i in 0..5 {
            let _ = state.push(seg(&format!("s{i}"), i * 1000));
        }
        let resp = handle(
            &state,
            &JsonRpcRequest {
                jsonrpc: "2.0".into(),
                id: 4,
                method: "tools/call".into(),
                params: Some(serde_json::json!({
                    "name": TOOL_PULL_RECENT,
                    "arguments": {"slot_id": 1, "limit": 3}
                })),
            },
        );
        let v = resp.result.unwrap();
        let arr = v["structuredContent"]["segments"].as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0]["text"], "s2");
        assert_eq!(arr[2]["text"], "s4");
    }

    #[test]
    fn handle_unknown_method_returns_method_not_found() {
        let state = ServerState::new(OmpSessionId::default_session());
        let resp = handle(
            &state,
            &JsonRpcRequest {
                jsonrpc: "2.0".into(),
                id: 5,
                method: "nope".into(),
                params: None,
            },
        );
        assert_eq!(resp.error.unwrap().code, -32601);
    }

    #[test]
    fn server_state_buffer_drops_oldest_when_full() {
        let state = ServerState::new(OmpSessionId::default_session());
        for i in 0..(BUFFER_CAP + 5) {
            let _ = state.push(seg(&format!("s{i}"), i as u64));
        }
        let buf = state.pull(BUFFER_CAP + 10);
        assert_eq!(buf.len(), BUFFER_CAP);
        assert_eq!(buf[0].text, "s5");
    }

    #[test]
    fn stdout_mcp_target_pushes_into_shared_buffer() {
        let state = ServerState::new(OmpSessionId::default_session());
        let target = StdoutMcpTarget::new(state.clone());
        target
            .push_segment(&seg("first", 0))
            .unwrap();
        target
            .push_segment(&seg("second", 500))
            .unwrap();
        assert_eq!(state.pull(10).len(), 2);
    }
}
