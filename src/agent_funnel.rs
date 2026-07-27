//! Multi-step funnel for adding or editing a user-configured
//! Agent target.
//!
//! The funnel drives the TUI's "Add Agent" / "Edit Agent" keys
//! (`a` / `e` in the Targets pane). It owns its own state machine
//! so the TUI just dispatches key events to it and renders the
//! current step on screen.
//!
//! Steps:
//!
//! 1. **Pick connection kind** — today only Kafka. Numbered
//!    choices for forward-compatibility.
//! 2. **Name** — short label the picker shows
//!    (`Agent: <name>`).
//! 3. **Broker endpoint** — librdkafka `bootstrap.servers`.
//! 4. **Topic** — destination topic the segment JSON lines
//!    get published to.
//! 5. **`acks` level** — All / One / Zero.
//! 6. **Verify** — runs the round-trip probe against the
//!    configured broker. Shows the result; user advances to Save
//!    on success or back to the form on failure.
//! 7. **Save** — caller commits the resulting `AgentTargetConfig`
//!    to `App::config` and `App::agent_targets` and closes the
//!    modal.
//!
//! Each step has its own key bindings. The renderer (see
//! `ui::render_agent_funnel`) paints the current step + a footer
//! with the keys for that step. The verify step is the only
//! one that talks to the network — every other step is pure
//! form state.

use std::time::Duration;

use crate::config::{AgentConnection, AgentTargetConfig, KafkaAcks, KafkaAgentConnection};

/// Funnel step indicator. The order of variants is the
/// user-visible order; the renderer indexes by `step as usize`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentFunnelStep {
    PickConnectionKind = 0,
    Name = 1,
    Endpoint = 2,
    Topic = 3,
    Acks = 4,
    Verify = 5,
    Save = 6,
}

impl AgentFunnelStep {
    pub const COUNT: usize = 7;

    pub fn as_index(self) -> usize {
        self as usize
    }
}

/// Result of the verify step. The renderer surfaces this on
/// both the Verify screen and the Save screen so the user can
/// revisit the outcome before committing.
#[derive(Debug, Clone, PartialEq)]
pub enum VerifyOutcome {
    Pending,
    InProgress,
    Ok { elapsed: Duration },
    Err { message: String },
}
/// (no local kind enum — the funnel reuses
/// `crate::config::AgentConnectionKind` so there's
/// a single source of truth for the "what transport the user
/// picked" concept. Kept as a comment to flag the removal
/// for anyone who lands here from a search.)

/// Funnel state. Lives on `App`; mutated by `main.rs`'s key
/// dispatcher and read by `ui.rs`'s renderer.
#[derive(Debug, Clone)]
pub struct AgentFunnel {
    /// The id of the target being edited. `None` on the Add
    /// path; `Some` on the Edit path so the Save step can
    /// preserve the existing id.
    pub editing_id: Option<String>,
    pub step: AgentFunnelStep,

    /// Form values the user has filled in so far. The
    /// renderer shows the relevant field for the current
    pub kind: crate::config::AgentConnectionKind,
    pub name: String,
    pub endpoint: String,
    pub topic: String,
    pub acks: KafkaAcks,

    /// Verify-step state. `None` until the user runs a probe.
    pub verify: VerifyOutcome,
}

impl AgentFunnel {
    /// Open the funnel for adding a brand-new target. The id is
    /// minted at Save time so the user can cancel cleanly.
    pub fn new_add() -> Self {
        Self {
            editing_id: None,
            step: AgentFunnelStep::PickConnectionKind,
            kind: crate::config::AgentConnectionKind::Kafka,
            name: String::new(),
            endpoint: String::new(),
            topic: String::new(),
            acks: KafkaAcks::All,
            verify: VerifyOutcome::Pending,
        }
    }

    /// Open the funnel pre-filled with an existing target.
    /// The id is preserved so Save overwrites the row in place
    /// rather than appending a new one.
    pub fn new_edit(existing: &AgentTargetConfig) -> Self {
        let (endpoint, topic, acks) = match &existing.connection {
            AgentConnection::Kafka(k) => (k.endpoint.clone(), k.topic.clone(), k.acks),
        };
        Self {
            editing_id: Some(existing.id.clone()),
            step: AgentFunnelStep::PickConnectionKind,
            kind: crate::config::AgentConnectionKind::Kafka,
            name: existing.name.clone(),
            endpoint,
            topic,
            acks,
            verify: VerifyOutcome::Pending,
        }
    }

    /// The Kafka connection the form currently describes. Used
    /// by both the verify step and the save step.
    pub fn kafka_connection(&self) -> KafkaAgentConnection {
        KafkaAgentConnection {
            endpoint: self.endpoint.trim().to_string(),
            topic: self.topic.trim().to_string(),
            client_id: None,
            acks: self.acks,
        }
    }

    /// The full `AgentTargetConfig` the funnel will hand to
    /// `App::upsert_agent_target` on Save. The id is either the
    /// preserved edit-id or a fresh UUIDv4.
    pub fn to_config(&self) -> AgentTargetConfig {
        let id = self
            .editing_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        AgentTargetConfig {
            id,
            name: self.name.trim().to_string(),
            connection: AgentConnection::Kafka(self.kafka_connection()),
        }
    }

    /// Can the user advance from the current step? Used by the
    /// renderer to disable the "Next" hint and by the dispatch
    /// loop to no-op Enter on a half-filled step.
    pub fn can_advance(&self) -> bool {
        match self.step {
            AgentFunnelStep::PickConnectionKind => true,
            AgentFunnelStep::Name => !self.name.trim().is_empty(),
            AgentFunnelStep::Endpoint => {
                let e = self.endpoint.trim();
                !e.is_empty() && e.contains(':')
            }
            AgentFunnelStep::Topic => !self.topic.trim().is_empty(),
            AgentFunnelStep::Acks => true,
            AgentFunnelStep::Verify => {
                matches!(self.verify, VerifyOutcome::Ok { .. })
            }
            AgentFunnelStep::Save => true,
        }
    }

    /// Advance to the next step. The caller is responsible for
    /// checking `can_advance`; this method unconditionally
    /// bumps the step counter and clamps at the end.
    pub fn advance(&mut self) {
        let next = (self.step.as_index() + 1).min(AgentFunnelStep::COUNT - 1);
        self.step = match next {
            0 => AgentFunnelStep::PickConnectionKind,
            1 => AgentFunnelStep::Name,
            2 => AgentFunnelStep::Endpoint,
            3 => AgentFunnelStep::Topic,
            4 => AgentFunnelStep::Acks,
            5 => AgentFunnelStep::Verify,
            _ => AgentFunnelStep::Save,
        };
    }

    /// Step backward without losing form values. Used when the
    /// verify step fails and the user wants to fix the broker
    /// endpoint.
    pub fn back(&mut self) {
        if self.step.as_index() == 0 {
            return;
        }
        let prev = self.step.as_index() - 1;
        self.step = match prev {
            0 => AgentFunnelStep::PickConnectionKind,
            1 => AgentFunnelStep::Name,
            2 => AgentFunnelStep::Endpoint,
            3 => AgentFunnelStep::Topic,
            4 => AgentFunnelStep::Acks,
            5 => AgentFunnelStep::Verify,
            _ => AgentFunnelStep::Save,
        };
    }

    /// Push a character into the active text field. Only
    /// meaningful on the Name / Endpoint / Topic steps; the
    /// other steps ignore the result.
    pub fn type_char(&mut self, ch: char) {
        match self.step {
            AgentFunnelStep::Name => self.name.push(ch),
            AgentFunnelStep::Endpoint => self.endpoint.push(ch),
            AgentFunnelStep::Topic => self.topic.push(ch),
            _ => {}
        }
    }

    /// Pop the last character of the active text field. No-op
    /// on non-text steps.
    pub fn backspace(&mut self) {
        match self.step {
            AgentFunnelStep::Name => {
                self.name.pop();
            }
            AgentFunnelStep::Endpoint => {
                self.endpoint.pop();
            }
            AgentFunnelStep::Topic => {
                self.topic.pop();
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Add funnel starts on the first step with all
    /// text fields empty and a fresh UUID is generated
    /// at Save time. Pin the contract so a future
    /// refactor that mints the id at construction time
    /// is forced to revisit this test.
    #[test]
    fn new_add_starts_on_first_step_with_empty_form() {
        let f = AgentFunnel::new_add();
        assert_eq!(f.step, AgentFunnelStep::PickConnectionKind);
        assert!(f.editing_id.is_none());
        assert_eq!(f.kind, crate::config::AgentConnectionKind::Kafka);
        assert!(f.name.is_empty());
        assert!(f.endpoint.is_empty());
        assert!(f.topic.is_empty());
        assert_eq!(f.acks, crate::config::KafkaAcks::All);
        assert!(matches!(f.verify, VerifyOutcome::Pending));
    }

    /// The Edit funnel pre-fills every text field from
    /// the existing target. The id is preserved so Save
    /// overwrites the row in place rather than
    /// appending a new one.
    #[test]
    fn new_edit_prefills_from_existing() {
        use crate::config::{AgentConnection, AgentTargetConfig, KafkaAgentConnection};
        let conn = KafkaAgentConnection {
            endpoint: "broker:9092".into(),
            topic: "events".into(),
            client_id: Some("svc".into()),
            acks: crate::config::KafkaAcks::One,
        };
        let existing = AgentTargetConfig {
            id: "abc-123".into(),
            name: "prod".into(),
            connection: AgentConnection::Kafka(conn),
        };
        let f = AgentFunnel::new_edit(&existing);
        assert_eq!(f.editing_id.as_deref(), Some("abc-123"));
        assert_eq!(f.name, "prod");
        assert_eq!(f.endpoint, "broker:9092");
        assert_eq!(f.topic, "events");
        assert_eq!(f.acks, crate::config::KafkaAcks::One);
    }

    /// `can_advance` enforces "fill the form before
    /// advancing" on the text steps. The Endpoint step
    /// requires a `host:port` shape; the other text
    /// steps just require non-empty values.
    #[test]
    fn can_advance_enforces_required_fields() {
        let mut f = AgentFunnel::new_add();
        f.step = AgentFunnelStep::Name;
        assert!(!f.can_advance());
        f.name = "events".into();
        assert!(f.can_advance());

        f.step = AgentFunnelStep::Endpoint;
        assert!(!f.can_advance());
        f.endpoint = "localhost".into(); // missing port
        assert!(!f.can_advance());
        f.endpoint = "localhost:9092".into();
        assert!(f.can_advance());

        f.step = AgentFunnelStep::Topic;
        assert!(!f.can_advance());
        f.topic = "voice-bird".into();
        assert!(f.can_advance());
    }

    /// Advance / back are tight inverses — they never
    /// move the step off the [0, COUNT) range, so the
    /// "Save" / "first" boundaries can't be over-stepped
    /// by a misbehaving dispatcher.
    #[test]
    fn advance_and_back_clamps_to_range() {
        let mut f = AgentFunnel::new_add();
        f.back();
        assert_eq!(f.step, AgentFunnelStep::PickConnectionKind);
        for _ in 0..20 {
            f.advance();
        }
        assert_eq!(f.step, AgentFunnelStep::Save);
        f.advance();
        assert_eq!(f.step, AgentFunnelStep::Save);
    }

    /// `to_config` mints a fresh UUID on Add and reuses
    /// the preserved id on Edit. Trims whitespace so
    /// stray spaces in the form don't leak into the
    /// TOML config.
    #[test]
    fn to_config_mints_or_reuses_id() {
        let add = AgentFunnel::new_add();
        let add_cfg = add.to_config();
        assert!(!add_cfg.id.is_empty());
        assert_eq!(add_cfg.id.len(), 36); // UUIDv4

        let mut edit = AgentFunnel::new_edit(&add_cfg);
        edit.name = "  spaced  ".into();
        edit.endpoint = "  broker:9092  ".into();
        let edit_cfg = edit.to_config();
        assert_eq!(edit_cfg.id, add_cfg.id);
        assert_eq!(edit_cfg.name, "spaced");
        let endpoint = match &edit_cfg.connection {
            crate::config::AgentConnection::Kafka(k) => &k.endpoint,
        };
        assert_eq!(endpoint, "broker:9092");
    }
}
