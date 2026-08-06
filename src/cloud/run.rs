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

/// Fire a `POST /api/agent-runs`. Returns the new run id on
/// success. The server's `201` carries the run summary; `4xx` and
/// `5xx` map to specific user-facing messages.
pub fn start(
    base_url: &str,
    api_key: &str,
    agent_id: &str,
    transcript: &str,
    source_label: Option<&str>,
) -> anyhow::Result<String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;
    let url = format!("{}/api/agent-runs", base_url.trim_end_matches('/'));
    let mut body = serde_json::json!({
        "characterId": agent_id,
        "transcript": transcript,
    });
    if let Some(label) = source_label {
        body["sourceLabel"] = serde_json::Value::String(label.to_string());
    }
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
}
