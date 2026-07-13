//! JSON-RPC 2.0 frame parser + writer for the MCP transport.
//!
//! MCP-over-stdio is one newline-delimited JSON object per line.
//! Filled in by commit 4 of the omp-integration rollout; this stub
//! keeps `src/omp/mod.rs` compilable from commit 1 onward.

#![allow(dead_code)]

use serde::{Deserialize, Serialize};

/// JSON-RPC 2.0 envelope. `id` is intentionally a `serde_json::Value`
/// because the spec allows string / number / null and omp's MCP
/// client uses Snowflake IDs (16-char hex strings). The MCP client
/// also sends notifications (no `id` field), so the parser treats
/// the absence of `id` as a notification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    #[serde(default = "default_jsonrpc")]
    pub jsonrpc: String,
    /// Request id. Absent for notifications; JSON-RPC 2.0 forbids
    /// `id: null` for *requests* but allows it for *responses* to
    /// indicate a parse error. We accept any of number, string, null.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<serde_json::Value>,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

fn default_jsonrpc() -> String {
    "2.0".into()
}

/// JSON-RPC 2.0 response envelope. Matches the request's `id` type
/// (string / number / null) so the client can correlate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    #[serde(default = "default_jsonrpc")]
    pub jsonrpc: String,
    /// `None` for responses to notifications — the spec forbids
    /// sending any reply to a notification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl JsonRpcResponse {
    pub fn ok(id: impl Into<serde_json::Value>, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: Some(id.into()),
            result: Some(result),
            error: None,
        }
    }
    pub fn err(
        id: impl Into<serde_json::Value>,
        code: i32,
        message: impl Into<String>,
    ) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: Some(id.into()),
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
    /// Reply to a notification is forbidden by the spec, but if we ever
    /// need to surface a parse error to a malformed frame, this is the
    /// shape the spec mandates (id = null, no `result`, only `error`).
    pub fn err_null(code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: None,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}
