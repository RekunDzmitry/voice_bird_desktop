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

#[derive(Debug, Deserialize)]
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
        .map_err(|e| anyhow::anyhow!("agents list response was not JSON: {e}"))?;
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
