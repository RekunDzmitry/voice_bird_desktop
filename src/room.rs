//! Room — a predefined bundle of conversation roles plus an optional
//! watching agent.
//!
//! A Room replaces the old "Agent" picker: each slot in the TUI
//! represents one role, and an optional built-in agent watches the
//! merged role-labeled transcript. The Free Room is the offline
//! default — empty roles, no agent — and is hardcoded in the
//! desktop binary so cloud availability never gates it.

use serde::{Deserialize, Serialize};

/// What kind of source captures a role's audio. Mirrors the web
/// `lib/rooms.ts::RoomRoleSourceKind` exported shape, lowercased on
/// the wire (`device-input` / `device-output` / `app-loopback`).
///
/// `DeviceInput` — microphone pane in the picker.
/// `DeviceOutput` — system / loopback device pane.
/// `AppLoopback` — process-loopback capture with optional device catch-all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceKind {
    DeviceInput,
    DeviceOutput,
    AppLoopback,
}

impl SourceKind {
    /// Lowercase wire identifier. Used by tests and by anything that
    /// needs to render the source kind in the TUI.
    pub fn wire(self) -> &'static str {
        match self {
            SourceKind::DeviceInput => "device-input",
            SourceKind::DeviceOutput => "device-output",
            SourceKind::AppLoopback => "app-loopback",
        }
    }

    /// Parse from the same wire string the server emits. `None` when
    /// the value isn't a known kind — call-sites decide whether to
    /// fall back or surface an error.
    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "device-input" => Some(SourceKind::DeviceInput),
            "device-output" => Some(SourceKind::DeviceOutput),
            "app-loopback" => Some(SourceKind::AppLoopback),
            _ => None,
        }
    }
}

/// One named role in a room. Today every role is human-driven
/// (microphone, loopback, …); the `kind` field is reserved so a
/// future "AI role" can be added without a schema break.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleDef {
    pub slug: String,
    pub name: String,
}

/// Per-role capture expectation. Drives the TUI funnel modal:
/// each step filters the picker to devices / apps that match
/// `source_kind`. Mirrors the web's `RoomRoleConstraintDto`.
///
/// `required_app_slug` is honoured only when `source_kind ==
/// AppLoopback`. `device_required` distinguishes "must pick a
/// device" from "any device on the loopback is fine".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleConstraint {
    pub role_slug: String,
    pub source_kind: SourceKind,
    #[serde(default)]
    pub required_app_slug: Option<String>,
    pub device_required: bool,
}

/// Reference to a built-in agent that watches the merged
/// transcript. The desktop never fetches the prompt — only the
/// id (used in `POST /api/agent-runs`) and the display metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRef {
    pub id: String,
    pub name: String,
    pub icon: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Room {
    pub slug: String,
    pub name: String,
    pub icon: Option<String>,
    pub roles: Vec<RoleDef>,
    pub agent: Option<AgentRef>,
    pub requires_pro: bool,
    /// When true the room's recordings MUST run through the cloud
    /// pipeline. The TUI block the user from disabling cloud while
    /// the room is the active one — see `cloud_required_banner` in
    /// `crate::banner`.
    #[serde(default)]
    pub requires_cloud: bool,
    /// Default system prompt the desktop pre-fills on the funnel
    /// prompt step. The user can override per-session via
    /// `$EDITOR`; the override is stored in `RoomBindings`.
    #[serde(default)]
    pub prompt_template: String,
    /// Per-role capture expectations. Drives the funnel and the
    /// picker filter at activate time.
    #[serde(default)]
    pub role_constraints: Vec<RoleConstraint>,
}

impl Room {
    /// True when this room binds a built-in agent (so the TUI
    /// should render the merged-timeline + context-pane view).
    pub fn has_agent(&self) -> bool {
        self.agent.is_some()
    }

    /// True when this room has any role constraints. The Free Room
    /// returns false; agent rooms always return true. Used by the
    /// funnel wiring to decide whether the wizard is needed.
    pub fn has_role_constraints(&self) -> bool {
        !self.role_constraints.is_empty()
    }

    /// The "free" offline room. Hardcoded so the TUI works
    /// without ever talking to the server.
    pub fn free_room() -> Self {
        Self {
            slug: "free".to_string(),
            name: "Free Room".to_string(),
            icon: None,
            roles: Vec::new(),
            agent: None,
            requires_pro: false,
            requires_cloud: false,
            prompt_template: String::new(),
            role_constraints: Vec::new(),
        }
    }

    /// Visibility predicate for the picker. The Free Room
    /// (`slug == "free"`) is always visible — it's the
    /// offline default, and the picker must show something
    /// even when the user has never talked to the server.
    /// Cloud-required rooms hide whenever the user's
    /// display state is "cloud off" because they're not
    /// actionable offline.
    ///
    /// Mirrors the render-time filter in
    /// `ui::render_rooms_pane` and the navigation skip in
    /// `App::select_next` / `select_previous`, so the cursor,
    /// the rendered row, and the next-arrow target all
    /// agree on "what's visible".
    pub fn is_visible(&self, cloud_visible: bool) -> bool {
        self.slug == "free" || !self.requires_cloud || cloud_visible
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_room_is_offline_default() {
        let r = Room::free_room();
        assert_eq!(r.slug, "free");
        assert!(!r.has_agent());
        assert!(!r.requires_pro);
        assert!(!r.requires_cloud);
        assert!(r.roles.is_empty());
        assert!(r.role_constraints.is_empty());
    }

    #[test]
    fn has_agent_true_when_agent_set() {
        let r = Room {
            slug: "x".into(),
            name: "X".into(),
            icon: None,
            roles: vec![],
            agent: Some(AgentRef {
                id: "dentist".into(),
                name: "Dentist".into(),
                icon: Some("🦷".into()),
            }),
            requires_pro: true,
            requires_cloud: true,
            prompt_template: "x".into(),
            role_constraints: vec![],
        };
        assert!(r.has_agent());
        assert!(r.requires_cloud);
    }

    #[test]
    fn source_kind_wire_round_trip() {
        for kind in [
            SourceKind::DeviceInput,
            SourceKind::DeviceOutput,
            SourceKind::AppLoopback,
        ] {
            let wire = kind.wire();
            assert_eq!(SourceKind::from_wire(wire), Some(kind));
        }
        assert_eq!(SourceKind::from_wire("unknown"), None);
    }

    #[test]
    fn source_kind_serialises_lowercase_kebab() {
        let json = serde_json::to_string(&SourceKind::DeviceInput).unwrap();
        assert_eq!(json, "\"device-input\"");
        let parsed: SourceKind = serde_json::from_str("\"app-loopback\"").unwrap();
        assert_eq!(parsed, SourceKind::AppLoopback);
    }
}
