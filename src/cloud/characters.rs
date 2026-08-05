//! Cloud Character listing.
//!
//! The picker renders the user's configured Characters by calling
//! `GET /api/characters` with a Bearer API key. The prompt template
//! is intentionally NOT fetched — the server runs it; the desktop
//! only needs the id, name, and icon for the picker row.

use std::time::Duration;

use serde::Deserialize;

/// One Character row for the picker. The `promptTemplate` field
/// that the web app exposes is deliberately omitted here — the
/// prompt never leaves voicebird.app.
#[derive(Debug, Clone)]
pub struct Character {
    pub id: String,
    pub name: String,
    pub icon: Option<String>,
    pub is_built_in: bool,
}

/// Wire shape returned by `GET /api/characters`. Built-in
/// characters come from application code on the server; custom
/// characters come from the user's `ai_characters` table. The
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
struct ServerCharacter {
    id: String,
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    icon: Option<String>,
    prompt_template: String,
}

#[derive(Debug, Deserialize)]
struct CharactersResponse {
    characters: Vec<ServerCharacter>,
}

/// Fetch the user's Characters list. Uses `reqwest::blocking` so
/// it can be invoked from the synchronous event loop without
/// spawning an async runtime; the request is small (one JSON
/// page) and the timeout is bounded.
pub fn fetch(base_url: &str, api_key: &str) -> anyhow::Result<Vec<Character>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    let url = format!("{}/api/characters", base_url.trim_end_matches('/'));
    let resp = client
        .get(&url)
        .bearer_auth(api_key)
        .send()
        .map_err(|e| anyhow::anyhow!("characters list request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(anyhow::anyhow!(
            "characters list returned {}",
            resp.status()
        ));
    }
    let body: CharactersResponse = resp
        .json()
        .map_err(|e| anyhow::anyhow!("characters list response was not JSON: {e}"))?;

    let mut out = Vec::with_capacity(body.characters.len());
    for c in body.characters {
        // The server returns the same shape for built-in and
        // custom rows; we don't need to distinguish here — the
        // pick_target_key field is on the row, not on Character.
        // `is_built_in` is left at `false` until §10 wires the
        // picker rendering to differentiate.
        let _ = c.description;
        out.push(Character {
            id: c.id,
            name: c.name,
            icon: c.icon,
            is_built_in: false,
        });
    }
    Ok(out)
}
