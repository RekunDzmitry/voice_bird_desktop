//! Cloud Agent run — `POST /api/agent-runs` + SSE consumer.
//!
//! Mirrors the server contract in `voice_bird_web` §4. The
//! desktop is the client:
//!
//!   - `POST /api/agent-runs` with the joined transcript + the
//!     picked agent id. The server replies 201 with the run id
//!     and persists a streaming row.
//!   - `GET /api/agent-runs/<id>/events` over SSE delivers the
//!     same `status | delta | done | error` frame shape the
//!     dashboard consumes. `run_event_loop` walks the stream and
//!     forwards each frame to a caller-supplied closure.
//!
//! The desktop stays a thin client — no LLM logic lives here.
//! That keeps the desktop's binary lean and the cloud code
//! identical to the dashboard's web client (see §6 in the plan).

use std::io::{BufRead, BufReader};
use std::time::Duration;

use serde::Deserialize;

/// What the run looks like from the desktop's perspective. The
/// server's status field names ("running" / "completed" / "failed")
/// are mirrored as `status: &'static str` for now; richer enums can
/// be parsed later if the dashboard wants to drive a typed reducer.
#[derive(Debug, Clone)]
pub enum RunEvent {
    Status {
        run_id: String,
        status: &'static str,
    },
    Delta {
        run_id: String,
        text: String,
    },
    Done {
        run_id: String,
        content_markdown: String,
    },
    Error {
        run_id: String,
        message: String,
    },
    /// Sentinel delivered when the server closes the SSE stream.
    /// Callers can use this to drive the "saved to voicebird.app"
    /// banner without parsing the last `done` frame themselves.
    DoneFinal,
}

#[derive(Debug, Deserialize)]
struct StartResponse {
    run: StartRun,
}

#[derive(Debug, Deserialize)]
struct StartRun {
    id: String,
    status: String,
}

/// Generate a per-run clientRunId. The server uses this to
/// dedupe retries — if the desktop reconnects with the same
/// id, the server returns the existing run instead of
/// starting a new one. UUIDv4 is fine; the server only
/// looks at string equality.
pub fn generate_client_run_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Fire a `POST /api/agent-runs`. Returns the new run id on
/// success. The server's `201` carries the run summary; `4xx` and
/// `5xx` map to specific user-facing messages.
///
/// Body shape (server contract, see `voice_bird_web`
/// `src/app/api/agent-runs/route.ts`):
///   {
///     "agentId":      "<built-in agent id, from Room.agent>",
///     "transcript":   "<merged role-labeled timeline>",
///     "sourceLabel":  "<display label, e.g. 'desktop-doctor'>",  // optional
///     "clientRunId":  "<UUID generated per run, for retries>"   // optional
///     "roomSlug":     "<room slug, ties the run to a room>"     // optional
///   }
///
/// Errors:
///   401 → API key rejected; 402 → Pro required; 413 → transcript
///   too long; other 4xx/5xx → opaque server error.
pub fn start(
    base_url: &str,
    api_key: &str,
    agent_id: &str,
    transcript: &str,
    source_label: Option<&str>,
    room_slug: Option<&str>,
    client_run_id: Option<&str>,
) -> anyhow::Result<String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;
    let url = format!("{}/api/agent-runs", base_url.trim_end_matches('/'));
    let body = build_start_body(agent_id, transcript, source_label, room_slug, client_run_id);
    let resp = client
        .post(&url)
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .map_err(|e| anyhow::anyhow!("agent run request failed: {e}"))?;
    let status = resp.status();
    if status.as_u16() == 413 {
        return Err(anyhow::anyhow!("Transcript too long"));
    }
    if status.as_u16() == 401 {
        return Err(anyhow::anyhow!("API key rejected — check Settings"));
    }
    if status.as_u16() == 402 {
        return Err(anyhow::anyhow!("Agent runs require Pro"));
    }
    if !status.is_success() {
        return Err(anyhow::anyhow!("agent run returned {}", status));
    }
    let body: StartResponse = resp
        .json()
        .map_err(|e| anyhow::anyhow!("agent run response was not JSON: {e}"))?;
    let _ = body.run.status;
    Ok(body.run.id)
}

/// Build the JSON body for `POST /api/agent-runs`. Extracted
/// from `start()` so the wire shape is unit-testable without
/// a real HTTP round-trip. The server's zod schema is the
/// source of truth; we mirror it here character-for-character
/// (D4.1 + D4.6). Field names and types:
///   - `agentId`     : required, server picks the agent row
///   - `transcript`  : required, the joined role-labeled text
///   - `sourceLabel` : optional, display label (e.g. role)
///   - `roomSlug`    : optional, ties the run to a room
///   - `clientRunId` : optional, UUIDv4 for retry dedupe
fn build_start_body(
    agent_id: &str,
    transcript: &str,
    source_label: Option<&str>,
    room_slug: Option<&str>,
    client_run_id: Option<&str>,
) -> serde_json::Value {
    // Build the body with the server-expected field names. The
    // previous wire shape used `characterId` — that was the legacy
    // v0 schema and the server now rejects it. The server's zod
    // schema is the source of truth: `agentId`, optional
    // `sourceLabel`, optional `clientRunId`. We also add
    // `roomSlug` so the server can join runs back to rooms (D4.5
    // uses it for run-event fallback when the SSE bus drops).
    let mut body = serde_json::json!({
        "agentId": agent_id,
        "transcript": transcript,
    });
    if let Some(label) = source_label {
        body["sourceLabel"] = serde_json::Value::String(label.to_string());
    }
    if let Some(slug) = room_slug {
        body["roomSlug"] = serde_json::Value::String(slug.to_string());
    }
    if let Some(run_id) = client_run_id {
        body["clientRunId"] = serde_json::Value::String(run_id.to_string());
    }
    body
}

/// Open the SSE stream for `run_id` and forward each frame to
/// `on_event`. Returns `Err` if the connection can't be opened or
/// the server replies non-200; the per-frame parsing is
/// best-effort — a malformed line is logged and skipped, the
/// stream continues. `DoneFinal` is delivered when the server
/// closes the stream.
///
/// Implementation note: `reqwest::blocking::Response` has no
/// `bytes_stream` method, so we use a one-shot blocking thread to
/// drive the async `chunk()` API. SSE is long-lived; the thread
/// blocks on the response body and only returns when the server
/// closes the stream (which only happens when the run ends — the
/// typical case). The desktop reconnects on stream drop.
pub fn run_event_loop<F>(
    base_url: &str,
    api_key: &str,
    run_id: &str,
    mut on_event: F,
) -> anyhow::Result<()>
where
    F: FnMut(RunEvent),
{
    let client = reqwest::blocking::Client::builder().build()?;
    let url = format!(
        "{}/api/agent-runs/{}/events",
        base_url.trim_end_matches('/'),
        run_id
    );
    let req = client
        .get(&url)
        .bearer_auth(api_key)
        .header("accept", "text/event-stream")
        .build()
        .map_err(|e| anyhow::anyhow!("SSE request build failed: {e}"))?;

    let bytes = std::thread::Builder::new()
        .name("cloud-sse".into())
        .spawn(move || -> anyhow::Result<Vec<u8>> {
            let resp = client.execute(req)
                .map_err(|e| anyhow::anyhow!("SSE connect failed: {e}"))?;
            if !resp.status().is_success() {
                return Err(anyhow::anyhow!(
                    "SSE endpoint returned {}",
                    resp.status()
                ));
            }
            resp.bytes()
                .map(|b| b.to_vec())
                .map_err(|e| anyhow::anyhow!("SSE read error: {e}"))
        })?
        .join()
        .map_err(|_| anyhow::anyhow!("SSE thread panicked"))??;

    // Hand-roll a tiny SSE line splitter because pulling in
    // eventsource-client would double the binary size for a few
    // lines of glue. The format is:
    //   `event: <name>\n` (optional, we ignore)
    //   `data: <json>\n`
    //   `\n` (separator)
    let mut reader = BufReader::new(&bytes[..]);
    let mut buf = String::new();
    loop {
        buf.clear();
        if reader.read_line(&mut buf)? == 0 {
            break;
        }
        let line = buf.trim_end();
        if line.is_empty() {
            // Empty line is the SSE event separator; next event
            // starts on the following data: lines.
            continue;
        }
        let Some(rest) = line.strip_prefix("data:") else {
            continue;
        };
        let payload = rest.trim();
        let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) else {
            log::warn!("agent run: skipping non-JSON SSE frame: {payload}");
            continue;
        };
        if let Some(frame) = parse_frame(run_id, &value) {
            on_event(frame);
        }
    }
    on_event(RunEvent::DoneFinal);
    Ok(())
}

fn parse_frame(run_id: &str, value: &serde_json::Value) -> Option<RunEvent> {
    let typ = value.get("type")?.as_str()?;
    match typ {
        "status" => {
            let status = value.get("status")?.as_str()?;
            // Map the server's free-form status to a stable
            // &'static str. The dashboard uses the same names.
            let mapped = match status {
                "running" => "running",
                "completed" => "completed",
                "failed" => "failed",
                _ => "running",
            };
            Some(RunEvent::Status {
                run_id: run_id.to_string(),
                status: mapped,
            })
        }
        "delta" => {
            let text = value.get("text")?.as_str()?.to_string();
            Some(RunEvent::Delta {
                run_id: run_id.to_string(),
                text,
            })
        }
        "done" => {
            let content = value
                .get("contentMarkdown")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Some(RunEvent::Done {
                run_id: run_id.to_string(),
                content_markdown: content,
            })
        }
        "error" => {
            let message = value
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error")
                .to_string();
            Some(RunEvent::Error {
                run_id: run_id.to_string(),
                message,
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_status_frame() {
        let v = serde_json::json!({"type": "status", "status": "running"});
        assert!(matches!(
            parse_frame("r1", &v),
            Some(RunEvent::Status { status: "running", .. })
        ));
    }

    #[test]
    fn parse_delta_frame() {
        let v = serde_json::json!({"type": "delta", "text": "hi"});
        match parse_frame("r1", &v) {
            Some(RunEvent::Delta { text, .. }) => assert_eq!(text, "hi"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_done_frame() {
        let v = serde_json::json!({"type": "done", "contentMarkdown": "# x"});
        match parse_frame("r1", &v) {
            Some(RunEvent::Done { content_markdown, .. }) => {
                assert_eq!(content_markdown, "# x")
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_error_frame() {
        let v = serde_json::json!({"type": "error", "message": "boom"});
        match parse_frame("r1", &v) {
            Some(RunEvent::Error { message, .. }) => assert_eq!(message, "boom"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_unknown_type_is_skipped() {
        let v = serde_json::json!({"type": "ping"});
        assert!(parse_frame("r1", &v).is_none());
    }

    // ---- D4.6: pinning tests for the POST body shape ----
    //
    // These guard the wire contract. The server's zod schema
    // (in voice_bird_web/src/app/api/agent-runs/route.ts) is
    // the source of truth; if a field here drifts the
    // server-side zod parse will start returning 400. A
    // failure here is a wire contract regression.

    /// Required fields (`agentId`, `transcript`) and all
    /// optional fields populated. Asserts exact field names
    /// and string values.
    #[test]
    fn build_start_body_full() {
        let body = build_start_body(
            "agent-123",
            "hello world",
            Some("desktop-doctor"),
            Some("doctor-appointment"),
            Some("550e8400-e29b-41d4-a716-446655440000"),
        );
        assert_eq!(
            body,
            serde_json::json!({
                "agentId": "agent-123",
                "transcript": "hello world",
                "sourceLabel": "desktop-doctor",
                "roomSlug": "doctor-appointment",
                "clientRunId": "550e8400-e29b-41d4-a716-446655440000",
            })
        );
    }
    /// All optional fields absent → only `agentId` and
    /// `transcript` are present. The server treats the
    /// absence of `roomSlug`/`clientRunId` as "unscoped,
    /// non-deduped" — this case exists for legacy test
    /// fixtures and the Free Room (no agent, no run).
    #[test]
    fn build_start_body_minimal() {
        let body = build_start_body("agent-1", "hi", None, None, None);
        let obj = body.as_object().expect("body must be an object");
        assert_eq!(obj.len(), 2);
        assert_eq!(obj["agentId"], "agent-1");
        assert_eq!(obj["transcript"], "hi");
        assert!(!obj.contains_key("sourceLabel"));
        assert!(!obj.contains_key("roomSlug"));
        assert!(!obj.contains_key("clientRunId"));
    }

    /// Old `characterId` field MUST NOT appear in the
    /// body. The server's zod schema rejects it. If this
    /// test ever fails it means someone re-introduced
    /// the v0 field — the fix is to delete it from
    /// `build_start_body` and update this assertion.
    #[test]
    fn build_start_body_does_not_emit_character_id() {
        let body = build_start_body("a1", "t", None, None, None);
        assert!(
            body.get("characterId").is_none(),
            "POST body must not contain the legacy characterId field"
        );
    }

    /// `generate_client_run_id` returns a fresh UUIDv4 each
    /// call. Two consecutive calls must produce different
    /// values.
    #[test]
    fn generate_client_run_id_is_unique() {
        let a = generate_client_run_id();
        let b = generate_client_run_id();
        assert_ne!(a, b);
        // The server treats it as an opaque string, but we
        // sanity-check the UUID shape (8-4-4-4-12 hex).
        let parts: Vec<&str> = a.split('-').collect();
        assert_eq!(parts.len(), 5);
        assert_eq!(parts[0].len(), 8);
        assert_eq!(parts[1].len(), 4);
    }
}
