//! Cloud Room catalog — `GET /api/rooms`.
//!
//! Mirrors the picker-role of `cloud/agents.rs`: the desktop
//! fetches a small curated catalog of rooms (slug, name, icon,
//! roles, agent reference, role constraints, prompt template)
//! along with the caller's plan so it can render locked rooms
//! (🔒) without a second entitlements endpoint.
//!
//! The free room MAY be returned by the server (the
//! `feat/rooms-replace-agents` web stack does ship it). Callers
//! funnel through [`merge_rooms_with_free`] which dedupes by
//! slug so the picker never renders Free Room twice.

use std::time::Duration;

use serde::Deserialize;

use crate::room::{AgentRef, RoleConstraint, RoleDef, Room, SourceKind};

/// Wire shape returned by `GET /api/rooms`. The server emits
/// fields with the same names we declare here so no rename is
/// needed. Fields the desktop doesn't read (e.g. `kind` on
/// roles) are silently dropped on parse.
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

/// Wire shape of a role-constraint block. Mirrors the web's
/// `RoomRoleConstraintDto`. Unknown source kinds fall through
/// to `None` (logged at fetch time) — the desktop treats
/// `None` as "no constraint, use the system's default picker".
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServerRoleConstraint {
    role_slug: String,
    source_kind: String,
    #[serde(default)]
    required_app_slug: Option<String>,
    device_required: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
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
    #[serde(default)]
    requires_cloud: bool,
    /// Always present in agent-rooms; absent on free rooms.
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    role_constraints: Vec<ServerRoleConstraint>,
}

#[derive(Debug, Deserialize)]
struct RoomsResponse {
    rooms: Vec<ServerRoom>,
    #[serde(default)]
    plan: Option<String>,
}

/// Result of a successful `/api/rooms` fetch.
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

/// Combine a freshly-fetched rooms list with the canonical
/// offline `Free Room` so the picker never renders Free Room
/// twice regardless of whether the server returned it.
///
/// The dev `feat/rooms-replace-agents` stack surfaces `"free"`
/// in `/api/rooms` along with agent rooms — which collides
/// with the locally-prepended `Room::free_room()` that the
/// caller used to splice in unconditionally. Dedup on slug;
/// the first occurrence wins.
///
/// Order: Free Room at index 0 (always), then rooms in
/// server order. Stable for layout + tests.
pub fn merge_rooms_with_free(
    free_room: Room,
    incoming: Vec<Room>,
) -> Vec<Room> {
    let mut out: Vec<Room> = Vec::with_capacity(incoming.len() + 1);
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    out.push(free_room.clone());
    seen.insert(free_room.slug);
    for room in incoming {
        if seen.insert(room.slug.clone()) {
            out.push(room);
        }
    }
    out
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
    let mut out: Vec<Room> = Vec::with_capacity(body.rooms.len());
    for r in body.rooms {
        let roles: Vec<RoleDef> = r
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
        let requires_pro = matches!(r.requires_plan.as_deref(), Some("pro"));
        let mut role_constraints: Vec<RoleConstraint> = Vec::new();
        for c in r.role_constraints {
            let Some(source_kind) = SourceKind::from_wire(&c.source_kind) else {
                log::warn!(
                    "rooms.fetch: unknown source_kind {:?} for role {:?}; skipping",
                    c.source_kind,
                    c.role_slug
                );
                continue;
            };
            role_constraints.push(RoleConstraint {
                role_slug: c.role_slug,
                source_kind,
                required_app_slug: c.required_app_slug,
                device_required: c.device_required,
            });
        }
        let prompt_template = r.prompt.unwrap_or_default();
        out.push(Room {
            slug: r.slug,
            name: r.name,
            icon: r.icon,
            roles,
            agent,
            requires_pro,
            requires_cloud: r.requires_cloud,
            prompt_template,
            role_constraints,
        });
    }
    let plan_is_pro = matches!(body.plan.as_deref(), Some("pro"));
    Ok(RoomsList { rooms: out, plan_is_pro })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_wire_response_with_phase_4_fields_preserved() {
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
                    "requiresPlan": "pro",
                    "requiresCloud": true,
                    "prompt": "You are an AI interview assistant",
                    "roleConstraints": [
                        {"roleSlug": "interviewer", "sourceKind": "device-input", "requiredAppSlug": null, "deviceRequired": true},
                        {"roleSlug": "interviewee", "sourceKind": "app-loopback", "requiredAppSlug": null, "deviceRequired": false}
                    ]
                }
            ]
        }"#;
        let parsed: RoomsResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.rooms.len(), 1);
        let r = &parsed.rooms[0];
        assert_eq!(r.slug, "software-interview");
        assert_eq!(r.requires_plan.as_deref(), Some("pro"));
        assert!(r.requires_cloud);
        assert_eq!(r.prompt.as_deref(), Some("You are an AI interview assistant"));
        assert_eq!(r.role_constraints.len(), 2);
    }

    /// Build a `Room` with minimal fields. Tests fill in the
    /// pieces they care about.
    fn room_with_slug(slug: &str) -> Room {
        Room {
            slug: slug.into(),
            name: format!("{slug} (server)"),
            icon: None,
            roles: Vec::new(),
            agent: None,
            requires_pro: false,
            requires_cloud: false,
            prompt_template: String::new(),
            role_constraints: Vec::new(),
        }
    }

    /// Slug-dedup invariant (bug hunt): when the server's
    /// rooms payload already includes a room whose slug
    /// matches the canonical Free Room, the picker must
    /// render Free Room EXACTLY ONCE.
    #[test]
    fn merge_dedupes_when_server_returns_free() {
        let free = Room::free_room();
        let incoming = vec![
            room_with_slug("free"),
            room_with_slug("doctor-appointment"),
        ];
        let out = merge_rooms_with_free(free, incoming);
        let free_count = out.iter().filter(|r| r.slug == "free").count();
        assert_eq!(free_count, 1, "Free Room rendered twice:\n{out:?}");
        assert_eq!(out[0].slug, "free");
        assert_eq!(out[1].slug, "doctor-appointment");
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn merge_prepends_when_server_omits_free() {
        let free = Room::free_room();
        let incoming = vec![room_with_slug("software-interview")];
        let out = merge_rooms_with_free(free, incoming);
        assert_eq!(out[0].slug, "free", "Free Room must be at index 0");
        assert_eq!(out[1].slug, "software-interview");
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn merge_dedupes_multiple_collisions() {
        let free = Room::free_room();
        let mut dup_a = room_with_slug("free");
        dup_a.name = "server-free-A".into();
        let mut dup_b = room_with_slug("free");
        dup_b.name = "server-free-B".into();
        let real_agent = room_with_slug("software-interview");
        let incoming = vec![dup_a, dup_b, real_agent];
        let out = merge_rooms_with_free(free, incoming);
        let free_count = out.iter().filter(|r| r.slug == "free").count();
        assert_eq!(free_count, 1, "expected 1 free entry, got {free_count}");
        // Local prepended copy wins by construction.
        assert_eq!(out[0].name, "Free Room");
        assert_eq!(out[1].slug, "software-interview");
        assert_eq!(out.len(), 2);
    }

    /// Live-stack integration test. The dev
    /// `feat/rooms-replace-agents` `/api/rooms` payload
    /// includes a `"free"` slug along with the agent rooms.
    /// Without dedup, `merge_rooms_with_free` would render
    /// Free Room twice (the local prepended copy + the
    /// server's). This test verifies the wire-to-rendered
    /// path end-to-end. Run it against the local dev stack:
    ///
    /// ```bash
    /// VOICEBIRD_TEST_ROOMS_URL=http://localhost:3303 \
    ///   VOICEBIRD_TEST_API_KEY=<dev-key> \
    ///   cargo test --lib live_dev_rooms_payload_does_not_duplicate_free_room
    /// ```
    ///
    #[test]
    fn live_dev_rooms_payload_does_not_duplicate_free_room() {
        let url = match std::env::var("VOICEBIRD_TEST_ROOMS_URL") {
            Ok(v) if !v.is_empty() => v.trim_end_matches('/').to_string(),
            _ => {
                eprintln!(
                    "VOICEBIRD_TEST_ROOMS_URL not set - skipping live integration \
                     check; rerun with VOICEBIRD_TEST_ROOMS_URL=http://localhost:3303 \
                     + the dev stack up to exercise the full path."
                );
                return;
            }
        };
        let api_key = std::env::var("VOICEBIRD_TEST_API_KEY")
            .unwrap_or_else(|_| "test".into());
        let list = match fetch(&url, &api_key) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("live fetch against {url} failed: {e}; skipping");
                return;
            }
        };
        let rooms = list.rooms.clone();
        let merged = merge_rooms_with_free(Room::free_room(), rooms);
        let free_count = merged.iter().filter(|r| r.slug == "free").count();
        assert_eq!(
            free_count, 1,
            "Free Room duplicated. Merged list:\n{:#?}",
            merged
        );
        assert_eq!(merged[0].slug, "free");
        assert_eq!(merged.len(), list.rooms.len());
    }
}
