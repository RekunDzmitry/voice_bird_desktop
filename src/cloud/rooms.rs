//! Cloud Room catalog — `GET /api/rooms`.
//!
//! Mirrors the picker-role of `cloud/agents.rs`: the desktop
//! fetches a small curated catalog of rooms (slug, name, icon,
//! roles, agent reference) along with the caller's plan so it can
//! render locked rooms (🔒) without a second entitlements
//! endpoint.
//!
//! The free room is never expected from the server — the desktop
//! always prepends `Room::free_room()` locally so offline use
//! never depends on the cloud.

use std::time::Duration;

use serde::Deserialize;

use crate::room::{AgentRef, RoleDef, Room};

/// Wire shape returned by `GET /api/rooms`. The server emits
/// fields with the same names we declare here so no rename is
/// needed. Fields the desktop doesn't read (`kind` on roles,
/// `requiresPlan`) are silently dropped on parse.
#[derive(Debug, Deserialize)]
struct ServerRole {
    slug: String,
    name: String,
    #[serde(default)]
    #[allow(dead_code)]
    kind: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ServerAgentRef {
    id: String,
    name: String,
    #[serde(default)]
    icon: Option<String>,
}
#[derive(Debug, Deserialize)]
struct ServerRoom {
    slug: String,
    name: String,
    #[serde(default)]
    icon: Option<String>,
    #[serde(default)]
    roles: Vec<ServerRole>,
    #[serde(default)]
    agent: Option<ServerAgentRef>,
    #[serde(default, rename = "requiresPlan")]
    requires_plan: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RoomsResponse {
    rooms: Vec<ServerRoom>,
    #[serde(default)]
    plan: Option<String>,
}

/// Result of a successful `/api/rooms` fetch. `plan` is the
/// caller's effective plan ("pro" / "free" / "starter") — the
/// desktop treats anything other than "pro" as non-Pro.
#[derive(Debug, Clone)]
pub struct RoomsList {
    pub rooms: Vec<Room>,
    pub plan_is_pro: bool,
}

impl RoomsList {
    pub fn plan_is_pro(&self) -> bool {
        self.plan_is_pro
    }
}

/// Fetch the user's Rooms list. Uses `reqwest::blocking` so it
/// can be invoked from the synchronous event loop without
/// spawning an async runtime; the request is small (one JSON
/// page) and the timeout is bounded.
pub fn fetch(base_url: &str, api_key: &str) -> anyhow::Result<RoomsList> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    let url = format!("{}/api/rooms", base_url.trim_end_matches('/'));
    let resp = client
        .get(&url)
        .bearer_auth(api_key)
        .send()
        .map_err(|e| anyhow::anyhow!("rooms list request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(anyhow::anyhow!(
            "rooms list returned {}",
            resp.status()
        ));
    }
    let body: RoomsResponse = resp.json().map_err(|e| {
        anyhow::anyhow!("rooms list response failed to parse: {e}")
    })?;
    let mut out = Vec::with_capacity(body.rooms.len());
    for r in body.rooms {
        let roles = r
            .roles
            .into_iter()
            .map(|role| RoleDef {
                slug: role.slug,
                name: role.name,
            })
            .collect();
        let agent = r.agent.map(|a| AgentRef {
            id: a.id,
            name: a.name,
            icon: a.icon,
        });
        let requires_pro = matches!(
            r.requires_plan.as_deref(),
            Some("pro")
        );
        out.push(Room {
            slug: r.slug,
            name: r.name,
            icon: r.icon,
            roles,
            agent,
            requires_pro,
        });
    }
    let plan_is_pro = matches!(body.plan.as_deref(), Some("pro"));
    Ok(RoomsList { rooms: out, plan_is_pro })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire-response JSON contains fields the desktop
    /// doesn't read (`kind` on roles, the full `requiresPlan`
    /// string, etc.). They're silently dropped on parse — the
    /// struct has no matching field for `kind`. This test pins
    /// the wire format so a future server refactor that drops
    /// one of the *read* fields surfaces here as a serde error,
    /// not as a silent "Rooms list unavailable" banner.
    #[test]
    fn parses_wire_response_with_unknown_fields_dropped() {
        let body = r#"{
            "plan": "pro",
            "rooms": [
                {
                    "slug": "software-interview",
                    "name": "Software Interview",
                    "icon": "💼",
                    "roles": [
                        {"slug": "interviewer", "name": "Interviewer", "kind": "human"},
                        {"slug": "interviewee", "name": "Interviewee", "kind": "human"}
                    ],
                    "agent": {"id": "ai-interview-assistant", "name": "AI Interview Assistant", "icon": "🎯"},
                    "requiresPlan": "pro"
                },
                {
                    "slug": "doctor-appointment",
                    "name": "Doctor Appointment",
                    "icon": "🩺",
                    "roles": [
                        {"slug": "patient", "name": "Patient", "kind": "human"},
                        {"slug": "doctor", "name": "Doctor", "kind": "human"}
                    ],
                    "agent": {"id": "dentist", "name": "Dentist", "icon": "🦷"},
                    "requiresPlan": "pro"
                }
            ]
        }"#;
        let parsed: RoomsResponse = serde_json::from_str(body)
            .expect("wire response with unknown fields must parse");
        assert_eq!(parsed.rooms.len(), 2);
        assert_eq!(parsed.plan.as_deref(), Some("pro"));

        // Unknown fields default cleanly so a wire response that
        // omits optional fields doesn't surface as a parse error.
        let minimal = r#"{ "rooms": [] }"#;
        let minimal_parsed: RoomsResponse = serde_json::from_str(minimal)
            .expect("minimal rooms response must parse");
        assert!(minimal_parsed.rooms.is_empty());
        assert!(minimal_parsed.plan.is_none());
    }

    #[test]
    fn fetch_translates_wire_to_room_struct() {
        let body = r#"{
            "plan": "pro",
            "rooms": [
                {
                    "slug": "doctor-appointment",
                    "name": "Doctor Appointment",
                    "icon": "🩺",
                    "roles": [
                        {"slug": "patient", "name": "Patient", "kind": "human"},
                        {"slug": "doctor", "name": "Doctor", "kind": "human"}
                    ],
                    "agent": {"id": "dentist", "name": "Dentist", "icon": "🦷"},
                    "requiresPlan": "pro"
                }
            ]
        }"#;
        let resp: RoomsResponse = serde_json::from_str(body).unwrap();
        let mut out = Vec::with_capacity(resp.rooms.len());
        for r in resp.rooms {
            let roles = r
                .roles
                .into_iter()
                .map(|role| RoleDef { slug: role.slug, name: role.name })
                .collect();
            let agent = r.agent.map(|a| AgentRef {
                id: a.id,
                name: a.name,
                icon: a.icon,
            });
            let requires_pro = matches!(r.requires_plan.as_deref(), Some("pro"));
            out.push(Room {
                slug: r.slug,
                name: r.name,
                icon: r.icon,
                roles,
                agent,
                requires_pro,
            });
        }
        assert_eq!(out.len(), 1);
        let r = &out[0];
        assert_eq!(r.slug, "doctor-appointment");
        assert_eq!(r.roles.len(), 2);
        assert_eq!(r.roles[0].slug, "patient");
        assert!(r.agent.is_some());
        assert!(r.requires_pro);
    }

    #[test]
    fn plan_pro_when_string_says_pro() {
        let body = r#"{ "plan": "pro", "rooms": [] }"#;
        let resp: RoomsResponse = serde_json::from_str(body).unwrap();
        assert!(matches!(resp.plan.as_deref(), Some("pro")));
    }

    #[test]
    fn plan_free_when_string_says_free() {
        let body = r#"{ "plan": "free", "rooms": [] }"#;
        let resp: RoomsResponse = serde_json::from_str(body).unwrap();
        assert!(matches!(resp.plan.as_deref(), Some("free")));
    }

    #[test]
    fn plan_defaults_when_missing() {
        let body = r#"{ "rooms": [] }"#;
        let resp: RoomsResponse = serde_json::from_str(body).unwrap();
        assert!(resp.plan.is_none());
    }
}
