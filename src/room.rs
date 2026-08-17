//! Room — a predefined bundle of conversation roles plus an optional
//! watching agent.
//!
//! A Room replaces the old "Agent" picker: each slot in the TUI
//! represents one role, and an optional built-in agent watches the
//! merged role-labeled transcript. The Free Room is the offline
//! default — empty roles, no agent — and is hardcoded in the
//! desktop binary so cloud availability never gates it.

/// One named role in a room. Today every role is human-driven
/// (microphone, loopback, …); the `kind` field is reserved so a
/// future "AI role" can be added without a schema break.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleDef {
    pub slug: String,
    pub name: String,
}

/// Reference to a built-in agent that watches the merged
/// transcript. The desktop never fetches the prompt — only the
/// id (used in `POST /api/agent-runs`) and the display metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRef {
    pub id: String,
    pub name: String,
    pub icon: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Room {
    pub slug: String,
    pub name: String,
    pub icon: Option<String>,
    pub roles: Vec<RoleDef>,
    pub agent: Option<AgentRef>,
    pub requires_pro: bool,
}

impl Room {
    /// True when this room binds a built-in agent (so the TUI
    /// should render the merged-timeline + context-pane view).
    pub fn has_agent(&self) -> bool {
        self.agent.is_some()
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
        }
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
        assert!(r.roles.is_empty());
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
        };
        assert!(r.has_agent());
    }
}
