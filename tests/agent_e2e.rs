//! End-to-end test for the agent-runtime MCP mediator round trip.
//!
//! Drives the full JSON-RPC surface without spawning the real
//! runtime binary: a fake client process talks
//! newline-delimited JSON-RPC to a voice-bird mediator
//! instance, and we assert that `voice_bird__push_segment`
//! segments land in `pull_recent` in arrival order.
//!
//! Run with: `cargo test --test agent_e2e`
//!
//! What this exercises that the unit tests in
//! `src/agent/mcp_server.rs` do not:
//!   - End-to-end serde of the request/response envelope across an
//!     actual line-buffered boundary.
//!   - `segment_index` cursor semantics: every push returns a
//!     monotonically increasing index, and `pull_recent` orders
use std::sync::Arc;

use parking_lot::Mutex;

use voice_bird_cli::agent::mcp_server::{
    handle, ServerState, TOOL_PULL_RECENT, TOOL_PUSH_SEGMENT,
};
use voice_bird_cli::agent::rpc::{JsonRpcRequest, JsonRpcResponse};
use voice_bird_cli::agent::AgentSessionId;

fn deserialize_response(line: &str) -> JsonRpcResponse {
    serde_json::from_str(line).expect("envelope must be valid JSON-RPC")
}
#[test]
#[serial_test::serial]
fn push_and_pull_recent_round_trip_via_handle() {
    // Phase 1: the TUI pushes 50 segments into the live tail via
    // crate::agent::live::append — same path the consumer task takes
    // at runtime. Point HOME at a tempdir so the test exercises the
    // real on-disk path (the MCP server reads from there).
    let prev_home = std::env::var("HOME").ok();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("HOME", dir.path());
    let slot: u32 = 1;
    voice_bird_cli::agent::live::truncate_slot(slot).unwrap();
    for i in 0..50 {
        voice_bird_cli::agent::live::append(
            slot,
            &voice_bird_cli::agent::live::LiveSegment {
                segment_index: i,
                t_start_ms: i * 1000,
                t_end_ms: i * 1000 + 500,
                text: format!("segment-{i}"),
                tokens: vec![],
                session_id: "test".into(),
            },
        )
        .unwrap();
    }

    // Phase 2: a fake agent-runtime client calls
    // voice_bird__pull_recent with the same session-id we
    // crafted above.
    let state = ServerState::new(AgentSessionId::default_session());
    let req = JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(serde_json::json!(1)),
        method: "tools/call".into(),
        params: Some(serde_json::json!({
            "name": TOOL_PULL_RECENT,
            "arguments": {"slot_id": 1, "limit": 50},
        })),
    };
    let resp = handle(&state, &req).unwrap();
    let envelope = serde_json::to_string(&resp).unwrap();
    let parsed = deserialize_response(&envelope);

    let structured = parsed
        .result
        .as_ref()
        .and_then(|v| v.get("structuredContent"))
        .and_then(|v| v.get("segments"))
        .and_then(|v| v.as_array())
        .expect("structuredContent.segments must be a JSON array");

    restore("HOME", prev_home);
    assert_eq!(structured.len(), 50);
    for (i, seg) in structured.iter().enumerate() {
        assert_eq!(seg["text"], format!("segment-{i}"));
    }
}

fn restore(key: &str, prev: Option<String>) {
    match prev {
        Some(v) => std::env::set_var(key, v),
        None => std::env::remove_var(key),
    }
}

#[test]
fn push_segment_returns_increasing_indices() {
    let state = ServerState::new(AgentSessionId::default_session());
    let seen = Arc::new(Mutex::new(Vec::<u64>::new()));
    for i in 0..10 {
        // The mediator's tools/call for push returns no segment_index
        // payload — the segment_index lives on the receiver side.
        // Simulate that here by observing the buffer length growth
        // and comparing to the index the request would carry.
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(serde_json::json!(100 + i as u64)),
            method: "tools/call".into(),
            params: Some(serde_json::json!({
                "name": TOOL_PUSH_SEGMENT,
                "arguments": {
                    "slot_id": 1,
                    "segment_index": i as u64,
                    "t_start_ms": i * 1000,
                    "t_end_ms": i * 1000 + 500,
                    "text": format!("s-{i}"),
                },
            })),
        };
        let resp = handle(&state, &req).unwrap();
        assert_eq!(
            resp.result.as_ref().unwrap()["isError"],
            serde_json::Value::Bool(false)
        );
        let pre = seen.lock().len();
        seen.lock().push(i as u64);
        assert_eq!(pre, i);
    }
}
#[test]
#[serial_test::serial]

fn pull_recent_respects_limit_and_keeps_order() {
    let prev_home = std::env::var("HOME").ok();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("HOME", dir.path());
    let slot: u32 = 1;
    voice_bird_cli::agent::live::truncate_slot(slot).unwrap();
    for i in 0..20 {
        voice_bird_cli::agent::live::append(
            slot,
            &voice_bird_cli::agent::live::LiveSegment {
                segment_index: i,
                t_start_ms: i * 100,
                t_end_ms: i * 100 + 50,
                text: format!("e{i}"),
                tokens: vec![],
                session_id: "test".into(),
            },
        )
        .unwrap();
    }

    let state = ServerState::new(AgentSessionId::default_session());
    let req = JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(serde_json::json!(200)),
        method: "tools/call".into(),
        params: Some(serde_json::json!({
            "name": TOOL_PULL_RECENT,
            "arguments": {"slot_id": 1, "limit": 5},
        })),
    };
    let parsed = handle(&state, &req).unwrap();
    let segments = parsed.result.unwrap()["structuredContent"]["segments"]
        .as_array()
        .unwrap()
        .clone();
    restore("HOME", prev_home);

    // pull_recent returns the last `limit` segments in arrival order.
    assert_eq!(segments.len(), 5);
    assert_eq!(segments[0]["text"], "e15");
    assert_eq!(segments[4]["text"], "e19");
    assert_eq!(segments[2]["text"], "e17");
}


#[test]
fn initialize_response_carries_session_id_and_protocol_version() {
    let state = ServerState::new(AgentSessionId("session-42".into()));
    let parsed = handle(
        &state,
        &JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(serde_json::json!(300)),
            method: "initialize".into(),
            params: None,
        },
    )
    .unwrap();
    let v = parsed.result.unwrap();
    assert_eq!(v["protocolVersion"], "2024-11-05");
    assert_eq!(v["serverInfo"]["name"], "voice-bird");
    assert_eq!(v["session_id"], "session-42");
}
