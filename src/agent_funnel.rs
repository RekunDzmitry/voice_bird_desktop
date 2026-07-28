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
//! 6. **Security** — `security.protocol`: plaintext / ssl /
//!    sasl_plaintext / sasl_ssl. Picking a non-SASL protocol
//!    skips the three SASL steps below.
//! 7. **SASL mechanism** — PLAIN / SCRAM-SHA-256 / SCRAM-SHA-512.
//! 8. **SASL username**.
//! 9. **SASL password env var** — the NAME of the environment
//!    variable holding the password. The secret itself never
//!    enters the funnel or the config file.
//! 10. **Verify** — runs the round-trip probe against the
//!     configured broker. Shows the result; user advances to Save
//!     on success or back to the form on failure.
//! 11. **Save** — caller commits the resulting `AgentTargetConfig`
//!     to `App::config` and `App::agent_targets` and closes the
//!     modal.
//!
//! Each step has its own key bindings. The renderer (see
//! `ui::render_agent_funnel`) paints the current step + a footer
//! with the keys for that step. The verify step is the only
//! one that talks to the network — every other step is pure
//! form state.

use std::time::Duration;

use crate::config::{
    AgentConnection, AgentTargetConfig, KafkaAcks, KafkaAgentConnection, KafkaSaslMechanism,
    KafkaSecurityProtocol,
};

/// Funnel step indicator. The order of variants is the
/// user-visible order; the renderer indexes by `step as usize`.
/// The three `Sasl*` steps only show when the picked
/// security protocol uses SASL — `advance`/`back` skip them
/// otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentFunnelStep {
    PickConnectionKind = 0,
    Name = 1,
    Endpoint = 2,
    Topic = 3,
    Acks = 4,
    Security = 5,
    SaslMechanism = 6,
    SaslUsername = 7,
    SaslPasswordEnv = 8,
    Verify = 9,
    Save = 10,
}

impl AgentFunnelStep {
    pub const COUNT: usize = 11;

    pub fn as_index(self) -> usize {
        self as usize
    }

    fn from_index(i: usize) -> Self {
        match i {
            0 => AgentFunnelStep::PickConnectionKind,
            1 => AgentFunnelStep::Name,
            2 => AgentFunnelStep::Endpoint,
            3 => AgentFunnelStep::Topic,
            4 => AgentFunnelStep::Acks,
            5 => AgentFunnelStep::Security,
            6 => AgentFunnelStep::SaslMechanism,
            7 => AgentFunnelStep::SaslUsername,
            8 => AgentFunnelStep::SaslPasswordEnv,
            9 => AgentFunnelStep::Verify,
            _ => AgentFunnelStep::Save,
        }
    }

    fn is_sasl_step(self) -> bool {
        matches!(
            self,
            AgentFunnelStep::SaslMechanism
                | AgentFunnelStep::SaslUsername
                | AgentFunnelStep::SaslPasswordEnv
        )
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
    /// step; the others stay buffered here.
    pub kind: crate::config::AgentConnectionKind,
    pub name: String,
    pub endpoint: String,
    pub topic: String,
    pub acks: KafkaAcks,
    pub security_protocol: KafkaSecurityProtocol,
    pub sasl_mechanism: KafkaSaslMechanism,
    pub sasl_username: String,
    /// NAME of the env var holding the SASL password — the secret
    /// itself is resolved at connect time and never enters the
    /// funnel or the config file.
    pub sasl_password_env: String,

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
            security_protocol: KafkaSecurityProtocol::Plaintext,
            sasl_mechanism: KafkaSaslMechanism::Plain,
            sasl_username: String::new(),
            sasl_password_env: String::new(),
            verify: VerifyOutcome::Pending,
        }
    }

    /// Open the funnel pre-filled with an existing target.
    /// The id is preserved so Save overwrites the row in place
    /// rather than appending a new one.
    pub fn new_edit(existing: &AgentTargetConfig) -> Self {
        let AgentConnection::Kafka(k) = &existing.connection;
        Self {
            editing_id: Some(existing.id.clone()),
            step: AgentFunnelStep::PickConnectionKind,
            kind: crate::config::AgentConnectionKind::Kafka,
            name: existing.name.clone(),
            endpoint: k.endpoint.clone(),
            topic: k.topic.clone(),
            acks: k.acks,
            security_protocol: k.security_protocol,
            sasl_mechanism: k.sasl_mechanism.unwrap_or_default(),
            sasl_username: k.sasl_username.clone().unwrap_or_default(),
            sasl_password_env: k.sasl_password_env.clone().unwrap_or_default(),
            verify: VerifyOutcome::Pending,
        }
    }

    /// The Kafka connection the form currently describes. Used
    /// by both the verify step and the save step. SASL fields are
    /// `None` for non-SASL protocols even if the user filled them
    /// in and then switched back to plaintext — the saved config
    /// only carries what the protocol actually uses.
    pub fn kafka_connection(&self) -> KafkaAgentConnection {
        let sasl = self.security_protocol.uses_sasl();
        KafkaAgentConnection {
            endpoint: self.endpoint.trim().to_string(),
            topic: self.topic.trim().to_string(),
            client_id: None,
            acks: self.acks,
            security_protocol: self.security_protocol,
            sasl_mechanism: sasl.then_some(self.sasl_mechanism),
            sasl_username: sasl.then(|| self.sasl_username.trim().to_string()),
            sasl_password_env: sasl.then(|| self.sasl_password_env.trim().to_string()),
        }
    }

    /// 1-based position of the current step among the steps this
    /// form will actually visit, plus the total count. Non-SASL
    /// protocols skip the three SASL steps, so the renderer can't
    /// just use the enum index for its "step X/Y" header.
    pub fn step_position(&self) -> (usize, usize) {
        let sasl = self.security_protocol.uses_sasl();
        let visible = |s: AgentFunnelStep| sasl || !s.is_sasl_step();
        let total = (0..AgentFunnelStep::COUNT)
            .filter(|&i| visible(AgentFunnelStep::from_index(i)))
            .count();
        let position = (0..=self.step.as_index())
            .filter(|&i| visible(AgentFunnelStep::from_index(i)))
            .count();
        (position, total)
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
            AgentFunnelStep::Security => true,
            AgentFunnelStep::SaslMechanism => true,
            AgentFunnelStep::SaslUsername => !self.sasl_username.trim().is_empty(),
            // Only require the env var NAME here; whether it is
            // actually set in the environment is checked by the
            // verify step, which can show a real error message.
            AgentFunnelStep::SaslPasswordEnv => !self.sasl_password_env.trim().is_empty(),
            AgentFunnelStep::Verify => {
                matches!(self.verify, VerifyOutcome::Ok { .. })
            }
            AgentFunnelStep::Save => true,
        }
    }

    /// Advance to the next step, skipping the SASL steps when the
    /// picked protocol doesn't use SASL. The caller is responsible
    /// for checking `can_advance`; this method unconditionally
    /// bumps the step counter and clamps at the end.
    pub fn advance(&mut self) {
        let mut next = (self.step.as_index() + 1).min(AgentFunnelStep::COUNT - 1);
        if !self.security_protocol.uses_sasl() {
            while AgentFunnelStep::from_index(next).is_sasl_step() {
                next += 1;
            }
        }
        self.step = AgentFunnelStep::from_index(next);
    }

    /// Step backward without losing form values, skipping the SASL
    /// steps when the picked protocol doesn't use SASL. Used when
    /// the verify step fails and the user wants to fix the broker
    /// endpoint.
    pub fn back(&mut self) {
        if self.step.as_index() == 0 {
            return;
        }
        let mut prev = self.step.as_index() - 1;
        if !self.security_protocol.uses_sasl() {
            while AgentFunnelStep::from_index(prev).is_sasl_step() {
                prev -= 1;
            }
        }
        self.step = AgentFunnelStep::from_index(prev);
        // Stepping back to an editable step means the user is
        // about to change the form, which invalidates any prior
        // verify outcome (R5). Stepping back to Verify itself is
        // a no-op for the outcome (the user is back on the
        // step whose outcome it is).
        if !matches!(self.step, AgentFunnelStep::Verify) {
            self.verify = VerifyOutcome::Pending;
        }
    }

    /// Push a character into the active text field. Only
    /// meaningful on the Name / Endpoint / Topic steps; the
    /// other steps ignore the result. Editing any text field
    /// invalidates a prior verify outcome: the connection chosen
    /// by the form may no longer match the connection that was
    /// probed, so the green "OK" must be cleared (R5).
    pub fn type_char(&mut self, ch: char) {
        match self.step {
            AgentFunnelStep::Name => self.name.push(ch),
            AgentFunnelStep::Endpoint => self.endpoint.push(ch),
            AgentFunnelStep::Topic => self.topic.push(ch),
            AgentFunnelStep::SaslUsername => self.sasl_username.push(ch),
            AgentFunnelStep::SaslPasswordEnv => self.sasl_password_env.push(ch),
            _ => return,
        }
        self.verify = VerifyOutcome::Pending;
    }

    /// Pop the last character of the active text field. No-op
    /// on non-text steps. Editing any text field invalidates a
    /// prior verify outcome (R5).
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
            AgentFunnelStep::SaslUsername => {
                self.sasl_username.pop();
            }
            AgentFunnelStep::SaslPasswordEnv => {
                self.sasl_password_env.pop();
            }
            _ => return,
        }
        self.verify = VerifyOutcome::Pending;
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
            security_protocol: Default::default(),
            sasl_mechanism: None,
            sasl_username: None,
            sasl_password_env: None,
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

    /// Plaintext (the default) skips the three SASL steps in both
    /// directions: Security advances straight to Verify, and Verify
    /// steps back to Security. The "step X/Y" header sees 8 steps.
    #[test]
    fn plaintext_walk_skips_sasl_steps() {
        let mut f = AgentFunnel::new_add();
        f.step = AgentFunnelStep::Security;
        assert_eq!(f.step_position(), (6, 8));
        f.advance();
        assert_eq!(f.step, AgentFunnelStep::Verify);
        assert_eq!(f.step_position(), (7, 8));
        f.back();
        assert_eq!(f.step, AgentFunnelStep::Security);
    }

    /// SASL protocols visit mechanism → username → password-env
    /// between Security and Verify, and the text steps gate
    /// `can_advance` on non-empty values. The header sees 11 steps.
    #[test]
    fn sasl_walk_visits_credential_steps() {
        let mut f = AgentFunnel::new_add();
        f.security_protocol = KafkaSecurityProtocol::SaslSsl;
        f.step = AgentFunnelStep::Security;
        assert_eq!(f.step_position(), (6, 11));

        f.advance();
        assert_eq!(f.step, AgentFunnelStep::SaslMechanism);
        assert!(f.can_advance(), "mechanism has a default; always advanceable");

        f.advance();
        assert_eq!(f.step, AgentFunnelStep::SaslUsername);
        assert!(!f.can_advance());
        f.type_char('u');
        assert!(f.can_advance());

        f.advance();
        assert_eq!(f.step, AgentFunnelStep::SaslPasswordEnv);
        assert!(!f.can_advance());
        for ch in "KAFKA_PW".chars() {
            f.type_char(ch);
        }
        assert!(f.can_advance());

        f.advance();
        assert_eq!(f.step, AgentFunnelStep::Verify);
        assert_eq!(f.step_position(), (10, 11));
        f.back();
        assert_eq!(f.step, AgentFunnelStep::SaslPasswordEnv);
    }

    /// `kafka_connection` only carries SASL fields when the
    /// protocol uses SASL. Filling the credentials and then
    /// switching back to plaintext must not leak them into the
    /// saved config row.
    #[test]
    fn kafka_connection_strips_sasl_fields_for_non_sasl_protocols() {
        let mut f = AgentFunnel::new_add();
        f.security_protocol = KafkaSecurityProtocol::SaslSsl;
        f.sasl_username = " user ".into();
        f.sasl_password_env = " PW_ENV ".into();
        let conn = f.kafka_connection();
        assert_eq!(conn.security_protocol, KafkaSecurityProtocol::SaslSsl);
        assert_eq!(conn.sasl_mechanism, Some(KafkaSaslMechanism::Plain));
        // Text fields are trimmed like name/endpoint/topic.
        assert_eq!(conn.sasl_username.as_deref(), Some("user"));
        assert_eq!(conn.sasl_password_env.as_deref(), Some("PW_ENV"));

        f.security_protocol = KafkaSecurityProtocol::Plaintext;
        let conn = f.kafka_connection();
        assert_eq!(conn.security_protocol, KafkaSecurityProtocol::Plaintext);
        assert!(conn.sasl_mechanism.is_none());
        assert!(conn.sasl_username.is_none());
        assert!(conn.sasl_password_env.is_none());
    }

    /// The Edit funnel pre-fills the security fields from an
    /// existing SASL-secured target.
    #[test]
    fn new_edit_prefills_security_fields() {
        use crate::config::{AgentConnection, AgentTargetConfig, KafkaAgentConnection};
        let existing = AgentTargetConfig {
            id: "sec-1".into(),
            name: "secure".into(),
            connection: AgentConnection::Kafka(KafkaAgentConnection {
                endpoint: "broker:9093".into(),
                topic: "events".into(),
                client_id: None,
                acks: KafkaAcks::All,
                security_protocol: KafkaSecurityProtocol::SaslSsl,
                sasl_mechanism: Some(KafkaSaslMechanism::ScramSha512),
                sasl_username: Some("svc".into()),
                sasl_password_env: Some("VB_PW".into()),
            }),
        };
        let f = AgentFunnel::new_edit(&existing);
        assert_eq!(f.security_protocol, KafkaSecurityProtocol::SaslSsl);
        assert_eq!(f.sasl_mechanism, KafkaSaslMechanism::ScramSha512);
        assert_eq!(f.sasl_username, "svc");
        assert_eq!(f.sasl_password_env, "VB_PW");
    }

    /// R5 (PR #31 round-3 review): editing any form field must
    /// reset a stale `VerifyOutcome`. Otherwise: verify OK →
    /// [←] back → edit the endpoint → forward again shows a
    /// green "OK" for a connection that was never probed, and
    /// (once R4 lands) Enter reaches Save without re-verifying.
    #[test]
    fn editing_form_resets_verify_outcome() {
        let mut f = AgentFunnel::new_add();
        f.step = AgentFunnelStep::Endpoint;
        f.endpoint = "old-broker:9092".into();

        f.verify = VerifyOutcome::Ok {
            elapsed: Duration::from_millis(5),
        };
        f.type_char('x');
        assert!(
            matches!(f.verify, VerifyOutcome::Pending),
            "type_char must reset a stale verify outcome to Pending (R5), got {:?}",
            f.verify
        );

        f.verify = VerifyOutcome::Ok {
            elapsed: Duration::from_millis(5),
        };
        f.backspace();
        assert!(
            matches!(f.verify, VerifyOutcome::Pending),
            "backspace must reset a stale verify outcome to Pending (R5), got {:?}",
            f.verify
        );
    }
}
