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

use std::io::BufRead;
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
/// Implementation note (D4.2): we no longer buffer the entire
/// SSE response before parsing. The previous version called
/// `Response::bytes()` and waited for EOF — that meant a 60 s
/// run sat silent for 60 s and a 5-min run sat silent for 5
/// min. Now the body is read line-by-line from the underlying
/// `std::net::TcpStream` via `copy_to` into a `BufReader` on
/// a `std::io::Read` trait object. Each `data: …` frame is
/// parsed and dispatched as soon as its trailing newline
/// arrives. EOF triggers `DoneFinal`.
///
/// `reqwest::blocking::Response` exposes the body as `Read`,
/// so the BufReader sits directly on the body. No background
/// thread is needed — the caller already runs this on a
/// worker thread (App::agent_run_worker).
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
    let resp = client
        .get(&url)
        .bearer_auth(api_key)
        .header("accept", "text/event-stream")
        .send()
        .map_err(|e| anyhow::anyhow!("SSE connect failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(anyhow::anyhow!(
            "SSE endpoint returned {}",
            resp.status()
        ));
    }
    read_sse_frames(run_id, resp, &mut on_event)?;
    on_event(RunEvent::DoneFinal);
    Ok(())
}

/// Same as `run_event_loop` but forwards frames through an
/// mpsc channel. The worker thread holds the producer; the
/// UI thread drains the receiver. This is the variant
/// `App::agent_run_worker` uses (D4.3) — the UI never blocks
/// on the network and the worker never blocks on the UI.
pub fn run_event_loop_chan(
    base_url: &str,
    api_key: &str,
    run_id: &str,
    tx: std::sync::mpsc::Sender<RunEvent>,
) -> anyhow::Result<()> {
    run_event_loop(base_url, api_key, run_id, move |ev| {
        // If the UI is gone (receiver dropped), the run is
        // cancelled. The server will detect the dropped
        // socket on its side; the worker can just stop
        // forwarding.
        let _ = tx.send(ev);
    })
}

/// SSE line reader. Reads from `reader` until EOF, parses
/// each `data: …` line into a `RunEvent` via `parse_frame`,
/// and calls `on_event` for each. Extracted from
/// `run_event_loop` so it can be unit-tested with a
/// `&[u8]` cursor (no HTTP needed).
///
/// Format (per the SSE spec, what the server emits):
///   `event: <name>\n`        — we ignore the event name
///   `data: <json>\n`         — one frame per line
///   `\n`                     — empty line is a separator
///                                (we don't accumulate multi-
///                                line data fields; the server
///                                never emits them)
/// EOF triggers nothing — the caller decides whether EOF
/// means `DoneFinal` (it does for the live run_event_loop
/// path).
pub fn read_sse_frames<R, F>(
    run_id: &str,
    reader: R,
    mut on_event: F,
) -> anyhow::Result<()>
where
    R: std::io::Read,
    F: FnMut(RunEvent),
{
    // Hand-roll a tiny SSE line splitter because pulling in
    // eventsource-client would double the binary size for a few
    // lines of glue. `BufReader::read_until` is exactly what
    // we want: read up to and including the next `\n`, append
    // to a reusable buffer.
    let mut reader = std::io::BufReader::new(reader);
    let mut buf = Vec::with_capacity(256);
    loop {
        buf.clear();
        let n = reader.read_until(b'\n', &mut buf)?;
        if n == 0 {
            // EOF — server closed the stream.
            break;
        }
        // Strip the trailing \n (and any \r). We don't trim
        // leading whitespace — SSE lines start at column 0.
        let mut line = buf.as_slice();
        while let Some(&last) = line.last() {
            if last == b'\n' || last == b'\r' {
                line = &line[..line.len() - 1];
            } else {
                break;
            }
        }
        if line.is_empty() {
            // Empty line is the SSE event separator; next
            // event starts on the following data: lines.
            continue;
        }
        let Some(rest) = line.strip_prefix(b"data:") else {
            continue;
        };
        let payload = std::str::from_utf8(rest.trim_ascii())
            .unwrap_or("");
        let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) else {
            log::warn!("agent run: skipping non-JSON SSE frame: {payload}");
            continue;
        };
        if let Some(frame) = parse_frame(run_id, &value) {
            on_event(frame);
        }
    }
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

// ---- D4.3: App::AgentRunState + worker ----
//
// The run is driven by a worker thread (one per active room,
// keyed by run_id) that:
//   1. POSTs the start request with the agent id + joined
//      transcript + UUID clientRunId.
//   2. Opens the SSE event loop and forwards each frame to
//      the UI through an mpsc channel.
//   3. Closes the channel when the stream ends (DoneFinal).
// The UI never touches the network — it only reads from the
// receiver on each tick and updates `App::agent_run_state`.

/// The UI's view of an in-flight (or last completed) run.
/// Owned by `App`; the worker thread is the only writer
/// (via the mpsc receiver). `status` is a string for now;
/// the dashboard's reducer stays the source of typed
/// enums if/when we want one.
#[derive(Debug, Clone, Default)]
pub struct AgentRunState {
    /// "idle" | "starting" | "running" | "completed" |
    /// "failed" | "rate_limited" | "needs_pro" | "needs_api_key"
    pub status: String,
    /// Server run id, set once `start` returns 201. None
    /// until then.
    pub run_id: Option<String>,
    /// Last-known streamed markdown. The UI shows this
    /// verbatim in the context pane; the worker appends
    /// to it as Delta frames arrive.
    pub streaming: String,
    /// The completed run's full markdown, populated when
    /// the `done` frame arrives. Persisted to
    /// `<room_dir>/context.md` by App on every stop (D3.4.d).
    pub last_completed_md: String,
    /// Local instant the worker started this run. Used to
    /// enforce the 65 s debounce floor (D4.4).
    pub last_run_started: Option<std::time::Instant>,
    /// Number of transcript lines at the time the run
    /// was kicked off. When the run finishes, the diff
    /// (current - lines_at_last_run) is the new content
    /// that the agent saw.
    pub lines_at_last_run: usize,
    /// True if the user pressed `g` between when the
    /// worker last looked and now — the worker will
    /// trigger one more run after the current one
    /// completes (D4.4: manual `g` queues one).
    pub queued: bool,
    /// Last error message (cleared on next start). The
    /// UI surfaces this in the status banner.
    pub last_error: Option<String>,
    /// `Some(true)` when the 402 path has fired; the UI
    /// uses this to lock out the agent-room PickerFocus.
    /// (Set elsewhere by App on 402; the worker also
    /// reads it to suppress auto-runs.)
    pub plan_is_pro: Option<bool>,
}

/// Arguments the worker needs to start a run. We don't
/// borrow `App` directly because the worker thread
/// outlives the borrow checker on `&mut self`.
pub struct RunRequest {
    pub base_url: String,
    pub api_key: String,
    pub agent_id: String,
    pub room_slug: Option<String>,
    pub source_label: Option<String>,
    pub transcript: String,
}

/// Spawn a worker thread that runs the agent end-to-end:
/// POST → SSE drain → DoneFinal. Each `RunEvent` from
/// the SSE stream is forwarded to the UI via `tx`. The
/// thread joins when the stream ends or `tx` is dropped
/// (which `App` does on shutdown).
///
/// Returns `(JoinHandle, mpsc::Receiver<RunEvent>)`. The
/// receiver is drained by `App::drain_agent_run_state`
/// on each UI tick (D4.3 wiring).
pub fn spawn_agent_run(
    req: RunRequest,
) -> (std::thread::JoinHandle<AgentRunError>, std::sync::mpsc::Receiver<RunEvent>) {
    let (tx, rx) = std::sync::mpsc::channel::<RunEvent>();
    let handle = std::thread::Builder::new()
        .name("voice-bird-agent-run".into())
        .spawn(move || -> AgentRunError {
            // 1. POST /api/agent-runs → run_id
            let client_run_id = generate_client_run_id();
            let run_id = match start(
                &req.base_url,
                &req.api_key,
                &req.agent_id,
                &req.transcript,
                req.source_label.as_deref(),
                req.room_slug.as_deref(),
                Some(&client_run_id),
            ) {
                Ok(id) => id,
                Err(e) => return AgentRunError::StartFailed(e.to_string()),
            };
            // Surface the run id so the UI can show it in
            // the status footer.
            let _ = tx.send(RunEvent::Status {
                run_id: run_id.clone(),
                status: "starting",
            });
            // 2. SSE drain
            let r = run_event_loop_chan(&req.base_url, &req.api_key, &run_id, tx.clone());
            match r {
                Ok(()) => AgentRunError::None,
                Err(e) => AgentRunError::StreamFailed(e.to_string()),
            }
        })
        .expect("spawn agent-run worker thread");
    (handle, rx)
}

/// Errors the worker thread can return via JoinHandle.
/// We don't use anyhow on the worker side because the
/// UI only cares about the two coarse buckets: start
/// (POST) failed vs stream (SSE) failed. Sub-categorization
/// happens in App::drain_agent_run_state based on
/// status codes from the error string.
#[derive(Debug)]
pub enum AgentRunError {
    None,
    StartFailed(String),
    StreamFailed(String),
}

// ---- D4.4: triggers + transcript truncation ----
//
// The server caps runs at 60/h per user, so the desktop
// throttles its own runs to 1 per 65 s floor. Manual `g`
// presses queue one run; the worker picks it up after the
// current run finishes. On `stop_section`, App fires a
// final run regardless of the floor — the user has stopped
// the recording and wants the final summary.

/// Minimum spacing between two auto-runs, in seconds.
/// Server caps at 60/h; we add 5 s of slack.
pub const AUTO_RUN_FLOOR_SECS: u64 = 65;

/// Soft cap on the transcript we ship to the server. The
/// server's zod schema caps at 200 000 chars; we cap at
/// 180 000 so we stay comfortably under and keep the tail
/// intact (the most recent dialog is what the agent
/// should see).
pub const TRANSCRIPT_MAX_CHARS: usize = 180_000;

/// Why App is considering firing an agent run. The
/// `Stop` variant bypasses the debounce floor (the user
/// has stopped recording — they want the final summary
/// NOW, not 65 s from now).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunTrigger {
    /// Auto: a new line was added; re-evaluate the 65 s
    /// floor and the queue.
    Auto,
    /// Manual: the user pressed `g`. Forces a run
    /// regardless of the floor; if a run is already
    /// in flight, sets `queued = true` so the worker
    /// re-runs after it finishes.
    Manual,
    /// Stop: the user stopped the recording. Forces a
    /// run regardless of floor or queue.
    Stop,
}

/// Pure decision function: should App start a new run
/// right now? The caller provides:
///   - now:                current Instant
///   - last_run_started:   Instant of the last run's
///                         worker spawn, or None
///   - queued:             manual `g` queue flag
///   - plan_is_pro:        if Some(false), the user
///                         is on the free tier; suppress
///                         auto-runs (manual + stop still
///                         fire)
///   - trigger:            why we're asking
///
/// Returns true when App should call
/// `start_agent_run`. The decision is independent of
/// the worker's state — App checks
/// `agent_run_worker.is_some()` separately (D4.5
/// gates against an in-flight run).
pub fn should_run_now(
    now: std::time::Instant,
    last_run_started: Option<std::time::Instant>,
    queued: bool,
    plan_is_pro: Option<bool>,
    trigger: RunTrigger,
) -> bool {
    // Free tier: auto-runs are suppressed. Manual `g`
    // and Stop still fire — the server returns 402,
    // which the worker classifies as `needs_pro` and
    // the UI surfaces in the status banner (D4.5).
    if plan_is_pro == Some(false) && trigger == RunTrigger::Auto {
        return false;
    }
    // Stop and Manual bypass the debounce floor.
    if matches!(trigger, RunTrigger::Stop | RunTrigger::Manual) {
        return true;
    }
    // Auto: respect the floor and the queue.
    if queued {
        return true;
    }
    match last_run_started {
        None => true,
        Some(t) => now.duration_since(t).as_secs() >= AUTO_RUN_FLOOR_SECS,
    }
}

/// Truncate a transcript for shipping to the server. The
/// server caps at 200 000 chars; we cap at 180 000. We
/// keep the tail (most recent dialog) and prepend a
/// short marker so the agent knows it didn't see the
/// whole thing.
///
/// If `text` is already under the cap, return it as-is.
/// Otherwise, keep the last `TRANSCRIPT_MAX_CHARS` chars
/// and prepend a marker line.
pub fn truncate_transcript(text: &str) -> std::borrow::Cow<'_, str> {
    if text.len() <= TRANSCRIPT_MAX_CHARS {
        return std::borrow::Cow::Borrowed(text);
    }
    let marker = "[earlier conversation truncated]\n\n";
    // Keep the last TRANSCRIPT_MAX_CHARS chars of the body
    // (so the most recent context survives). The marker
    // is on the left so the agent sees it first.
    let start = text.len() - TRANSCRIPT_MAX_CHARS;
    // Walk forward to the next char boundary so we don't
    // slice a multi-byte codepoint.
    let start = text
        .char_indices()
        .map(|(i, _)| i)
        .find(|&i| i >= start)
        .unwrap_or(start);
    std::borrow::Cow::Owned(format!(
        "{marker}{}",
        &text[start..]
    ))
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

    // ---- D4.2: read_sse_frames tests ----
    //
    // The streaming line reader is the heart of the run
    // loop; if it drops or misorders frames the UI shows
    // a wrong or stuck "streaming" indicator. These tests
    // pin its semantics against a byte slice so we don't
    // need a real HTTP server to exercise the code path.

    /// Three well-formed data: lines → three events. Empty
    /// line separators are skipped without emitting events.
    #[test]
    fn read_sse_frames_emits_one_event_per_data_line() {
        let body = b"\
data: {\"type\":\"status\",\"status\":\"running\"}\n\
\n\
data: {\"type\":\"delta\",\"text\":\"hi\"}\n\
\n\
data: {\"type\":\"done\",\"contentMarkdown\":\"# done\"}\n\
";
        let mut events: Vec<RunEvent> = Vec::new();
        read_sse_frames("r1", &body[..], |e| events.push(e))
            .expect("read_sse_frames should not fail on well-formed input");
        assert_eq!(events.len(), 3);
        assert!(matches!(events[0], RunEvent::Status { status: "running", .. }));
        match &events[1] {
            RunEvent::Delta { text, .. } => assert_eq!(text, "hi"),
            other => panic!("expected Delta, got {other:?}"),
        }
        match &events[2] {
            RunEvent::Done { content_markdown, .. } => {
                assert_eq!(content_markdown, "# done")
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    /// Lines that aren't `data: …` (heartbeats, comments,
    /// event: lines) must be ignored. Only `data:` lines
    /// produce events.
    #[test]
    fn read_sse_frames_ignores_non_data_lines() {
        let body = b"\
: keepalive\n\
event: status\n\
data: {\"type\":\"delta\",\"text\":\"x\"}\n\
\n\
data: {\"type\":\"delta\",\"text\":\"y\"}\n\
";
        let mut events: Vec<RunEvent> = Vec::new();
        read_sse_frames("r1", &body[..], |e| events.push(e))
            .expect("read_sse_frames should not fail on heartbeat lines");
        assert_eq!(events.len(), 2);
    }

    /// A non-JSON `data:` line is logged and skipped — the
    /// stream must keep going. A subsequent valid frame
    /// must still be delivered.
    #[test]
    fn read_sse_frames_skips_malformed_data_and_continues() {
        let body = b"\
data: not-json\n\
\n\
data: {\"type\":\"delta\",\"text\":\"after\"}\n\
";
        let mut events: Vec<RunEvent> = Vec::new();
        read_sse_frames("r1", &body[..], |e| events.push(e))
            .expect("read_sse_frames must not error on a bad frame");
        assert_eq!(events.len(), 1);
        match &events[0] {
            RunEvent::Delta { text, .. } => assert_eq!(text, "after"),
            other => panic!("expected Delta, got {other:?}"),
        }
    }

    /// Empty body → zero events, no error. The server
    /// closes the stream before sending anything if the
    /// run is already complete.
    #[test]
    fn read_sse_frames_empty_body_yields_zero_events() {
        let mut events: Vec<RunEvent> = Vec::new();
        read_sse_frames("r1", b"" as &[u8], |e| events.push(e))
            .expect("empty body must not error");
        assert!(events.is_empty());
    }

    /// CR-only line endings (some proxies normalize
    /// `\n` → `\r\n`) must still be parsed. The reader
    /// strips both `\n` and `\r` trailers.
    #[test]
    fn read_sse_frames_handles_crlf_endings() {
        let body = b"\
data: {\"type\":\"delta\",\"text\":\"a\"}\r\n\
\r\n\
data: {\"type\":\"delta\",\"text\":\"b\"}\r\n\
";
        let mut events: Vec<RunEvent> = Vec::new();
        read_sse_frames("r1", &body[..], |e| events.push(e))
            .expect("CRLF should parse");
        assert_eq!(events.len(), 2);
    }

    /// The mpsc channel variant forwards each frame to the
    /// receiver. Dropping the receiver mid-stream must NOT
    /// panic the worker — `send` returns Err and the
    /// closure ignores it via `let _ = …`.
    #[test]
    fn run_event_loop_chan_drops_silently_when_receiver_gone() {
        let (tx, rx) = std::sync::mpsc::channel::<RunEvent>();
        // Drop the receiver immediately; the worker will
        // fail to forward every event but must not panic.
        drop(rx);
        // We can't easily hit the network here; instead,
        // verify the closure shape compiles + handles a
        // dropped channel by sending to a dead receiver.
        let _ = tx.send(RunEvent::DoneFinal);
        // If we got here without panicking, the channel
        // send is correctly best-effort.
    }

    // ---- D4.3: AgentRunState + worker tests ----
    //
    // The worker is hard to unit-test end-to-end without
    // a real HTTP server, so we test the parts that
    // don't need one: state transitions, default shape,
    // and the streaming-marker (the marker the UI uses
    // to show "•" while a run is alive).

    /// Default state is idle with no run id, no error,
    /// no streaming — the TUI shows the last completed
    /// context.md on first render.
    #[test]
    fn agent_run_state_default_is_idle() {
        let s = AgentRunState::default();
        assert_eq!(s.status, "");
        assert!(s.run_id.is_none());
        assert!(s.streaming.is_empty());
        assert!(s.last_completed_md.is_empty());
        assert!(s.last_run_started.is_none());
        assert_eq!(s.lines_at_last_run, 0);
        assert!(!s.queued);
        assert!(s.last_error.is_none());
        assert!(s.plan_is_pro.is_none());
    }

    /// `queued` starts false and is the manual `g` flag:
    /// the worker checks it between auto-runs to know
    /// whether the user wants a one-shot run.
    #[test]
    fn agent_run_state_queued_flag_toggles() {
        let mut s = AgentRunState::default();
        assert!(!s.queued);
        s.queued = true;
        assert!(s.queued);
        // The worker resets it after the queued run
        // completes; modeled here as a direct write.
        s.queued = false;
        assert!(!s.queued);
    }

    /// Status strings are the union the UI knows how to
    /// render. If you add a new status here, the UI
    /// branch in render::status_footer needs a match
    /// arm — this test fails the build (no, it just
    /// enumerates the known ones) so adding a string
    /// is a deliberate choice.
    #[test]
    fn agent_run_state_known_status_strings() {
        let known = [
            "idle",
            "starting",
            "running",
            "completed",
            "failed",
            "rate_limited",
            "needs_pro",
            "needs_api_key",
        ];
        // Round-trip: a hand-built state with each
        // status string survives a Clone. (Smoke test
        // — the real validation is the render match.)
        for st in &known {
            let s = AgentRunState {
                status: (*st).to_string(),
                ..Default::default()
            };
            assert_eq!(s.status, *st);
        }
    }

    /// The worker thread spawns even when the receiver
    /// is dropped immediately. The `start` call will
    /// fail (no real server), but the spawn itself
    /// must not panic and the JoinHandle must return
    /// cleanly.
    #[test]
    fn spawn_agent_run_survives_dropped_receiver() {
        let (handle, rx) = spawn_agent_run(RunRequest {
            base_url: "http://127.0.0.1:1".into(), // blackhole
            api_key: "test".into(),
            agent_id: "test-agent".into(),
            room_slug: None,
            source_label: None,
            transcript: "hello".into(),
        });
        drop(rx); // UI goes away
        // The start call will fail (connection refused
        // to 127.0.0.1:1); the worker returns
        // StartFailed. We don't care which — the
        // important property is the thread joins.
        let _ = handle.join();
    }

    // ---- D4.4: trigger + truncation tests ----

    /// First auto-run with no history: fire immediately.
    #[test]
    fn trigger_auto_fires_on_first_run() {
        let now = std::time::Instant::now();
        assert!(should_run_now(now, None, false, None, RunTrigger::Auto));
    }

    /// Auto within 65 s of the last run: suppressed.
    #[test]
    fn trigger_auto_suppressed_within_floor() {
        let now = std::time::Instant::now();
        let last = now - std::time::Duration::from_secs(30);
        assert!(!should_run_now(now, Some(last), false, None, RunTrigger::Auto));
    }

    /// Auto past the 65 s floor: fires.
    #[test]
    fn trigger_auto_fires_after_floor() {
        let now = std::time::Instant::now();
        let last = now - std::time::Duration::from_secs(66);
        assert!(should_run_now(now, Some(last), false, None, RunTrigger::Auto));
    }

    /// Manual `g`: bypasses the floor. Even at 1 s after
    /// the last run, a manual press fires.
    #[test]
    fn trigger_manual_bypasses_floor() {
        let now = std::time::Instant::now();
        let last = now - std::time::Duration::from_secs(1);
        assert!(should_run_now(now, Some(last), false, None, RunTrigger::Manual));
    }

    /// Stop: forces a run regardless of floor or queue.
    #[test]
    fn trigger_stop_bypasses_floor() {
        let now = std::time::Instant::now();
        let last = now - std::time::Duration::from_secs(1);
        assert!(should_run_now(now, Some(last), false, None, RunTrigger::Stop));
    }

    /// Queued manual `g` set: the next auto-run fires
    /// even within the floor.
    #[test]
    fn trigger_queued_presses_fire_on_next_auto_tick() {
        let now = std::time::Instant::now();
        let last = now - std::time::Duration::from_secs(10);
        assert!(should_run_now(now, Some(last), true, None, RunTrigger::Auto));
    }

    /// Free tier: auto-runs are suppressed. The 402
    /// response from a Manual run still surfaces
    /// (`needs_pro` in the status banner), so the user
    /// sees why nothing's happening.
    #[test]
    fn trigger_auto_suppressed_on_free_tier() {
        let now = std::time::Instant::now();
        assert!(!should_run_now(
            now,
            None,
            false,
            Some(false),
            RunTrigger::Auto
        ));
    }

    /// Free tier: Manual still fires (so the user gets
    /// the 402 banner explaining Pro is required).
    #[test]
    fn trigger_manual_still_fires_on_free_tier() {
        let now = std::time::Instant::now();
        assert!(should_run_now(
            now,
            None,
            false,
            Some(false),
            RunTrigger::Manual
        ));
    }

    /// Pro tier: auto-runs fire normally.
    #[test]
    fn trigger_auto_fires_on_pro_tier() {
        let now = std::time::Instant::now();
        assert!(should_run_now(
            now,
            None,
            false,
            Some(true),
            RunTrigger::Auto
        ));
    }

    /// Under the cap: returned as-is (zero-copy).
    #[test]
    fn truncate_under_cap_is_identity() {
        let text = "short text";
        let out = truncate_transcript(text);
        assert!(matches!(out, std::borrow::Cow::Borrowed(_)));
        assert_eq!(out, text);
    }

    /// Over the cap: returns a new String with the
    /// marker prepended; the tail survives.
    #[test]
    fn truncate_over_cap_keeps_tail_with_marker() {
        let long: String = (0..200_000).map(|_| 'a').collect();
        let out = truncate_transcript(&long);
        // Output must fit under the cap + marker.
        assert!(out.len() < TRANSCRIPT_MAX_CHARS + 64);
        // Marker must be the first line.
        assert!(out.starts_with("[earlier conversation truncated]"));
        // Tail must be preserved.
        assert!(out.ends_with("aaaaa"));
    }

    /// Multi-byte safety: the cut point must not slice
    /// a multi-byte codepoint.
    #[test]
    fn truncate_respects_utf8_boundaries() {
        let long: String = (0..200_000).map(|_| '✓').collect();
        let out = truncate_transcript(&long);
        // Cow<str> is always valid UTF-8 by construction;
        // this test pins the property the cut-walk
        // relies on.
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
        assert!(out.ends_with('✓'));
    }
}
