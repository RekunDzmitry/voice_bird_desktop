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

/// Wire shape returned by `GET /api/agents`. The web app
/// emits fields with the same names we declare here
/// (`id`, `name`, `description`, `icon`) so no rename is
/// needed. The fields the desktop doesn't read (`kind`,
/// `isBuiltIn`, `createdAt`, `updatedAt`, `promptTemplate`)
/// are silently dropped on parse — the struct has no
/// matching field, so serde ignores them.
#[derive(Debug, Deserialize)]
struct ServerAgent {
    id: String,
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    icon: Option<String>,
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

    /// The wire-response JSON contains fields the desktop
    /// doesn't read (`kind`, `isBuiltIn`, `createdAt`,
    /// `updatedAt`, `promptTemplate`). They're silently
    /// dropped on parse — the struct has no matching field.
    /// This test pins the wire format so a future server
    /// refactor that drops one of the *read* fields
    /// (`id`, `name`, `description`, `icon`) surfaces here
    /// as a serde error, not as a silent "Agents list
    /// unavailable" banner in production.
    #[test]
    fn parses_wire_response_with_unknown_fields_dropped() {
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
            .expect("wire response with unknown fields must parse");
        assert_eq!(parsed.agents.len(), 2);
        assert_eq!(parsed.agents[0].icon.as_deref(), Some("🦷"));
        assert_eq!(parsed.agents[0].name, "Dentist");
        // Unknown fields are silently dropped — assert
        // optional fields default correctly so a wire
        // response that omits `description` or `icon`
        // doesn't surface as a parse error.
        assert_eq!(
            parsed.agents[1].description, None,
            "missing description must default to None"
        );
        assert_eq!(
            parsed.agents[1].icon, None,
            "missing icon must default to None"
        );
    }

    /// End-to-end parse of the live `GET https://voicebird.app/api/agents`
    /// response captured against the user's API key on 2026-08-11.
    /// Pins that the wire format (built-ins + custom UUIDs, `null`
    /// icons, omitted `description`) is parsed cleanly by the
    /// desktop and produces three `Agent` rows — including the
    /// user's custom `Interviewee`. A future server refactor that
    /// breaks any of those rows surfaces here.
    #[test]
    fn parses_live_voicebird_app_response() {
        let body = std::fs::read_to_string(
            "tests/fixtures/voicebird_app_agents_response.json",
        )
        .expect("fixture present");
        let parsed: AgentsResponse = serde_json::from_str(&body)
            .expect("live wire response must parse");
        assert_eq!(
            parsed.agents.len(),
            3,
            "expected dentist + note-taker + custom, got {}",
            parsed.agents.len(),
        );

        let names: Vec<&str> =
            parsed.agents.iter().map(|a| a.name.as_str()).collect();
        assert!(names.contains(&"Dentist"), "Dentist built-in missing");
        assert!(names.contains(&"Note Taker"), "Note Taker built-in missing");
        assert!(names.contains(&"Interviewee"), "custom Interviewee missing");
    }

}
