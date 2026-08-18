//! Room session directory I/O.
//!
//! Each agent room activation creates
//! `<session_dir>/<ts>-room-<slug>/` containing:
//!   - `room.json` — the catalog snapshot at activation
//!   - one subdir per role (each holds a normal per-section
//!     session dir produced by `room_role_session_dir`)
//!   - `room-transcript.jsonl` — rewritten on every stop
//!   - `context.md` — last completed agent output (D4)
//!
//! `room.json` is written by `write_room_json`; the others are
//! managed by their respective write paths.

use std::path::Path;

use serde::Serialize;

use crate::room::Room;

#[derive(Debug, Serialize)]
pub struct RoomRoleSnapshot {
    pub slug: String,
    pub name: String,
}

#[derive(Debug, Serialize)]
pub struct RoomAgentSnapshot {
    pub id: String,
    pub name: String,
    pub icon: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RoomJson {
    pub room_slug: String,
    pub room_name: String,
    pub icon: Option<String>,
    pub agent: Option<RoomAgentSnapshot>,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub roles: Vec<RoomRoleSnapshot>,
    pub requires_pro: bool,
}

impl RoomJson {
    /// Build a `RoomJson` snapshot for a freshly-activated room.
    /// The caller passes the room's wall-clock start time so
    /// activation and snapshot agree on `started_at`.
    pub fn from_room(room: &Room, started_at: chrono::DateTime<chrono::Utc>) -> Self {
        let roles = room
            .roles
            .iter()
            .map(|r| RoomRoleSnapshot {
                slug: r.slug.clone(),
                name: r.name.clone(),
            })
            .collect();
        let agent = room.agent.as_ref().map(|a| RoomAgentSnapshot {
            id: a.id.clone(),
            name: a.name.clone(),
            icon: a.icon.clone(),
        });
        Self {
            room_slug: room.slug.clone(),
            room_name: room.name.clone(),
            icon: room.icon.clone(),
            agent,
            started_at,
            roles,
            requires_pro: room.requires_pro,
        }
    }
}

/// Write `room.json` into the room session directory. Returns
/// the path of the written file on success. The directory is
/// created (parents included) if it doesn't exist yet — agent
/// room activation runs before any role starts, so this is
/// often the first write into a fresh dir tree.
pub fn write_room_json(
    room_session_dir: &Path,
    room: &Room,
    started_at: chrono::DateTime<chrono::Utc>,
) -> std::io::Result<std::path::PathBuf> {
    std::fs::create_dir_all(room_session_dir)?;
    let json = RoomJson::from_room(room, started_at);
    let path = room_session_dir.join("room.json");
    let body = serde_json::to_string_pretty(&json)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    std::fs::write(&path, body)?;
    Ok(path)
}

/// Render one line of `room-transcript.jsonl` from a merged-timeline
/// entry. The format is `{"role":<string|null>,"at":<rfc3339>,"text":<string>}` —
/// the agent run path on the web side parses this back into the
/// role-labeled transcript the room view shows.
pub fn room_transcript_line(
    entry_at: chrono::DateTime<chrono::Utc>,
    role: Option<&str>,
    text: &str,
) -> std::io::Result<String> {
    serde_json::to_string(&serde_json::json!({
        "at": entry_at.to_rfc3339(),
        "role": role,
        "text": text,
    }))
    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
}

/// Rewrite the room-level transcript (`<room_dir>/room-transcript.jsonl`)
/// from a list of merged-timeline entries. Rewrite-on-stop avoids
/// the append race two parallel roles would otherwise create when
/// they both write to the same file. Returns the path written.
///
/// `entries` is typically `App::merged_timeline()` filtered to lines
/// that belong to the active room (every line qualifies while the
/// room is the only one in use).
pub fn write_room_transcript_jsonl(
    room_session_dir: &Path,
    entries: &[(chrono::DateTime<chrono::Utc>, Option<String>, String)],
) -> std::io::Result<std::path::PathBuf> {
    std::fs::create_dir_all(room_session_dir)?;
    let mut body = String::new();
    for (at, role, text) in entries {
        let line = room_transcript_line(*at, role.as_deref(), text)?;
        body.push_str(&line);
        body.push('\n');
    }
    let path = room_session_dir.join("room-transcript.jsonl");
    std::fs::write(&path, body)?;
    Ok(path)
}

mod tests {
    use super::*;
    use crate::room::{AgentRef, RoleDef, Room};

    fn two_role_room() -> Room {
        Room {
            slug: "doctor-appointment".into(),
            name: "Doctor Appointment".into(),
            icon: Some("🩺".into()),
            roles: vec![
                RoleDef { slug: "patient".into(), name: "Patient".into() },
                RoleDef { slug: "doctor".into(), name: "Doctor".into() },
            ],
            agent: Some(AgentRef {
                id: "dentist".into(),
                name: "Dentist".into(),
                icon: Some("🦷".into()),
            }),
            requires_pro: true,
        }
    }

    #[test]
    fn room_json_snapshot_round_trips() {
        let room = two_role_room();
        let started = chrono::Utc::now();
        let json = RoomJson::from_room(&room, started);
        let body = serde_json::to_string(&json).unwrap();
        // The snapshot carries everything the room view TUI
        // needs: slug, name, icon, agent, roles, pro flag,
        // and the activation timestamp.
        assert!(body.contains("\"room_slug\":\"doctor-appointment\""));
        assert!(body.contains("\"agent\""));
        assert!(body.contains("\"id\":\"dentist\""));
        assert!(body.contains("\"slug\":\"patient\""));
        assert!(body.contains("\"slug\":\"doctor\""));
        assert!(body.contains("\"requires_pro\":true"));
        assert!(body.contains("started_at"));
    }

    #[test]
    fn room_json_without_agent_omits_agent_field() {
        // Free Room: no agent binding. The snapshot still
        // serializes cleanly with `agent: null`.
        let free = Room::free_room();
        let json = RoomJson::from_room(&free, chrono::Utc::now());
        let body = serde_json::to_string(&json).unwrap();
        assert!(body.contains("\"room_slug\":\"free\""));
        assert!(body.contains("\"agent\":null"));
        assert!(body.contains("\"roles\":[]"));
    }

    #[test]
    fn write_room_json_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let room = two_role_room();
        let path = write_room_json(
            dir.path(),
            &room,
            chrono::Utc::now(),
        )
        .unwrap();
        assert!(path.exists());
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("doctor-appointment"));
    }

    #[test]
    fn room_transcript_jsonl_round_trip() {
        use std::str::FromStr;
        let dir = tempfile::tempdir().unwrap();
        let entries = vec![
            (
                chrono::DateTime::<chrono::Utc>::from_str("2026-05-13T10:00:00Z")
                    .unwrap(),
                Some("Patient".to_string()),
                "Hi doctor".to_string(),
            ),
            (
                chrono::DateTime::<chrono::Utc>::from_str("2026-05-13T10:00:02Z")
                    .unwrap(),
                Some("Doctor".to_string()),
                "How are you?".to_string(),
            ),
        ];
        let path = write_room_transcript_jsonl(dir.path(), &entries).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"role\":\"Patient\""), "{lines:?}");
        assert!(lines[0].contains("\"text\":\"Hi doctor\""), "{lines:?}");
        assert!(lines[1].contains("\"role\":\"Doctor\""), "{lines:?}");
        assert!(lines[1].contains("\"text\":\"How are you?\""), "{lines:?}");
        // Rewrite-on-stop semantics: the second write overwrites
        // the first; the file never accumulates stale rows.
        let one_entry = vec![entries[0].clone()];
        write_room_transcript_jsonl(dir.path(), &one_entry).unwrap();
        let body2 = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body2.lines().count(), 1);
    }
}
