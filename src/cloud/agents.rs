//! Cloud Agent listing.
//!
//! The picker renders the user's configured Agents by calling
//! `GET /api/agents` with a Bearer API key. The prompt template
//! is intentionally NOT fetched — the server runs it; the desktop
//! only needs the id, name, and icon for the picker row.

use std::time::Duration;

use serde::Deserialize;

/// One Agent row for the picker. The `promptTemplate` field
/// that the web app exposes is deliberately omitted here — the
/// prompt never leaves voicebird.app.
#[derive(Debug, Clone)]
pub struct Agent {
    pub id: String,
    pub name: String,
    pub icon: Option<String>,
    pub is_built_in: bool,
}

/// Wire shape returned by `GET /api/agents`. Built-in
/// agents come from application code on the server; custom
/// agents come from the user's `ai_agents` table. The
/// desktop treats them identically.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ServerKind {
    #[allow(dead_code)]
    #[serde(rename = "custom")]
    Custom,
    #[serde(rename = "built-in")]
    BuiltIn,
}

/// Wire shape returned by `GET /api/agents`. The web app is
/// TypeScript and emits camelCase (`isBuiltIn`, `promptTemplate`,
/// `createdAt`, …); serde's `rename_all = "camelCase"` translates
/// the snake_case Rust fields to the wire field names so a
/// straightforward `serde_json::from_str` parses the response.
/// Without this rename, `reqwest`'s `json()` deserializer fails
/// on the first camelCase field and the caller surfaces
/// "response was not JSON" — which is misleading because the
/// body IS JSON, it just doesn't match the schema. The fields
/// the desktop doesn't read (`kind`, `isBuiltIn`, `createdAt`,
/// `updatedAt`) are silently dropped on parse.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServerAgent {
    id: String,
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    icon: Option<String>,
    prompt_template: String,
}

#[derive(Debug, Deserialize)]
struct AgentsResponse {
    agents: Vec<ServerAgent>,
}

/// Fetch the user's Agents list. Uses `reqwest::blocking` so
/// it can be invoked from the synchronous event loop without
/// spawning an async runtime; the request is small (one JSON
/// page) and the timeout is bounded.
pub fn fetch(base_url: &str, api_key: &str) -> anyhow::Result<Vec<Agent>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    let url = format!("{}/api/agents", base_url.trim_end_matches('/'));
    let resp = client
        .get(&url)
        .bearer_auth(api_key)
        .send()
        .map_err(|e| anyhow::anyhow!("agents list request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(anyhow::anyhow!(
            "agents list returned {}",
            resp.status()
        ));
    }
    let body: AgentsResponse = resp
        .json()
        // The body IS JSON in the typical case — the failure
        // mode is a schema mismatch (camelCase wire vs the
        // previous snake_case-only struct), which serde reports
        // as a parse error. Surface it as a parse error rather
        // than "was not JSON" so the user/operator can tell at a
        // glance that the server response shape drifted.
        .map_err(|e| anyhow::anyhow!("agents list response failed to parse: {e}"))?;
    let mut out = Vec::with_capacity(body.agents.len());
    for c in body.agents {
        // picker rendering to differentiate.
        let _ = c.description;
        out.push(Agent {
            id: c.id,
            name: c.name,
            icon: c.icon,
            is_built_in: false,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The web app emits camelCase (`promptTemplate`, `isBuiltIn`,
    /// …) — without `#[serde(rename_all = "camelCase")]` on
    /// `ServerAgent`, the `prompt_template` field deserializes
    /// from `null` and serde reports a parse error. This test
    /// pins the wire format so a future refactor that drops
    /// the rename surfaces here, not as a silent "Agents list
    /// unavailable" banner in production.
    #[test]
    fn parses_camelcase_wire_response() {
        let body = r#"{
            "agents": [
                {
                    "id": "dentist",
                    "kind": "built-in",
                    "name": "Dentist",
                    "description": "Clinical note",
                    "icon": "🦷",
                    "isBuiltIn": true,
                    "createdAt": null,
                    "updatedAt": null,
                    "promptTemplate": "You are a dental assistant."
                },
                {
                    "id": "summarizer",
                    "name": "Summarizer",
                    "promptTemplate": "Summarize the transcript."
                }
            ]
        }"#;
        let parsed: AgentsResponse = serde_json::from_str(body)
            .expect("camelCase wire response must parse");
        assert_eq!(parsed.agents.len(), 2);
        assert_eq!(parsed.agents[0].icon.as_deref(), Some("🦷"));
        assert_eq!(parsed.agents[0].name, "Dentist");

        assert_eq!(
            parsed.agents[0].prompt_template,
            "You are a dental assistant."
        );
        // Fields the desktop doesn't read are silently dropped —
        // assert optional fields default correctly.
        assert_eq!(
            parsed.agents[1].description, None,
            "missing description must default to None"
        );
        assert_eq!(
            parsed.agents[1].icon, None,
            "missing icon must default to None"
        );
    }

    /// Regression: before the camelCase rename, this same body
    /// failed to deserialize (prompt_template was missing).
    /// Pinned here so the wrong-shaped response surfaces as a
    /// clear serde error at fetch time, not as a confusing
    /// "response was not JSON" wrapper.
    #[test]
    fn snake_case_wire_response_is_rejected() {
        let body = r#"{
            "agents": [
                {
                    "id": "dentist",
                    "name": "Dentist",
                    "prompt_template": "..."
                }
            ]
        }"#;
        let parsed: Result<AgentsResponse, _> = serde_json::from_str(body);
        assert!(
            parsed.is_err(),
            "snake_case wire format must not silently match the camelCase struct"
        );
    }
}
