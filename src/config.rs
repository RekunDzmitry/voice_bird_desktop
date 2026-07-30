use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};


/// Lives in lib so both `platform::AudioSession` (in the bin crate) and
/// `AppConfig` (in the lib crate) can reference it.
///
/// `App` is per-application audio capture (ScreenCaptureKit on macOS,
/// WASAPI process loopback on Windows). On those platforms the session's
/// `device_name` carries the bundle identifier (or PID-stringified
/// fallback) and `app_name` carries the human-readable label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioSessionKind {
    Input,
    Output,
    App,
}

/// Per-slot settings the user can flip from the Mode panel: Cloud
/// on/off, transcription language (cloud-only), local model, and
/// the on-disk session output path. Each slot owns its own copy and
/// changes to one slot do not affect any other — two slots can record
/// with different cloud/language/model/path simultaneously.
///
/// Persisted in `AppConfig::slot_settings` keyed by [`slot_settings_key`]
/// (the slot's numeric id as a decimal string). The key type is `String`
/// instead of `SlotId` so this module stays free of the binary's
/// `app::SlotId` definition — `config.rs` is in the lib crate, `app.rs`
/// is in the bin crate, and the slot identifier is the only field
/// the two modules need to share.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SlotSettings {
    pub cloud_on: bool,
    pub language: String,
    pub model: String,
    pub path: String,
}

impl Default for SlotSettings {
    fn default() -> Self {
        Self {
            cloud_on: false,
            language: "en".into(),
            model: "distil-small.en".into(),
            path: "~/voice-bird/sessions".into(),
        }
    }
}

/// String key for one slot's row in `AppConfig::slot_settings`. Stable
/// across the slot's lifetime — `app::SlotId(2)` is always "2" here.
/// Kept as a free function (not a method on `SlotSettings`) so the
/// lib crate can format it without depending on the binary's `SlotId`.
pub fn slot_settings_key(slot_id: u32) -> String {
    slot_id.to_string()
}


#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppConfig {
    pub hop_ms: u32,
    pub min_window_ms: u32,
    #[serde(rename = "engine_prefer")]
    pub engine_prefer: String,
    pub audio_default_source: String,
    /// Device name chosen by the user. `None` = use the OS default
    /// input. Missing-from-config (old configs) deserializes to `None`.
    #[serde(default)]
    pub input_device: Option<String>,
    /// Kind of the saved device. `None` for old configs / default input.
    /// Lets `start_section` pick the right capture path without having
    /// to re-enumerate and match by name first.
    #[serde(default)]
    pub input_device_kind: Option<AudioSessionKind>,
    /// Last app the picker's Apps pane was on. `None` = no app paired.
    /// Bundle id (macOS) or PID-stringified value (Windows). Restored at
    /// next launch so the user's app cursor lands where they left it.
    #[serde(default)]
    pub last_app_id: Option<String>,
    /// Optional background refinement model. When set, a second whisper
    /// engine runs in parallel on wider non-overlapping windows with beam
    /// search and emits higher-quality segments that replace the streaming
    /// output in the UI. `None` disables refinement.
    #[serde(default)]
    pub refinement_model: Option<String>,
    /// Window length (ms) of audio fed to each refinement pass.
    #[serde(default = "default_refinement_window_ms")]
    pub refinement_window_ms: u32,
    /// Beam size for refinement. 1 = greedy (fastest, lowest quality).
    #[serde(default = "default_refinement_beam_size")]
    pub refinement_beam_size: u8,
    /// Voice Bird Web API key. Stored in plaintext in config.toml — file
    /// permissions are the only protection. Empty string = unset.
    #[serde(default)]
    pub voicebird_api_key: String,

    /// WebSocket URL of the Voice Bird Web `/api/audio/stream` endpoint
    /// the desktop client streams to when `cloud_broadcast_enabled` is
    /// true. Defaults to the hosted production server.
    #[serde(default = "default_voicebird_server_url")]
    pub voicebird_server_url: String,


    /// Decimal `slot_id` → `SlotSettings`. The runtime layer
    /// (`App::new`) seeds slot 1 with any persisted entry, falling
    /// back to `SlotSettings::default()` on first run.
    /// back to `SlotSettings::default()` on first run.
    #[serde(default)]
    pub slot_settings: BTreeMap<String, SlotSettings>,

    /// User-configured Agent targets. Each entry carries the
    /// `Connection` (e.g. Kafka broker + topic) the TUI uses when
    /// pushing committed transcript segments. The built-in
    /// oh-my-pi / MCP target lives on `App::agent` instead — these
    /// are additively user-defined.
    #[serde(default)]
    pub agent_targets: Vec<AgentTargetConfig>,
}

/// Stable identifier for a user-configured Agent target. The TUI
/// mints one when the user picks `a`dd; the value is a UUIDv4.
pub type AgentTargetId = String;

/// Ids that the segment dispatcher in the consumer task
/// inside `App::start_section` compares against as a
/// special case (e.g. `"default"` routes to the legacy
/// MCP-backed `ServerState`). Letting a user-configured
/// target claim the same id would silently route its
/// segments to the wrong destination, so we reject both
/// hand-edited config rows and funnel-saved configs that
/// use them.
pub const RESERVED_AGENT_TARGET_IDS: &[&str] = &["default"];

pub fn is_reserved_agent_target_id(id: &str) -> bool {
    RESERVED_AGENT_TARGET_IDS.contains(&id)
}

/// Transport the user-configured Agent target uses to
/// publish committed segments. Today only Kafka is wired;
/// the enum is open so the funnel
/// ("1. Kafka  2. ...  3. ...") keeps working when we add more.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentConnectionKind {
    Kafka,
}

impl AgentConnectionKind {
    /// Every variant of `AgentConnectionKind`. The funnel
    /// iterates this to render the "Connection kind" picker
    /// and to decide what transport a saved config row
    /// referred to.
    pub const ALL: [Self; 1] = [Self::Kafka];

    /// Human-readable label rendered in the funnel.
    pub fn label(self) -> &'static str {
        match self {
            Self::Kafka => "Kafka",
        }
    }
}
/// `acks` mode passed to librdkafka. We default to `All` so a
/// committed segment is durable before `push_segment` returns `Ok`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KafkaAcks {
    /// No broker ack (fire-and-forget). Fastest, but can lose
    /// segments on broker failure. Not recommended for transcript.
    Zero,
    /// Leader-only ack. Survives leader re-election; loses
    /// messages on leader crash before replication.
    One,
    /// Wait for all in-sync replicas. Default.
    All,
}

impl Default for KafkaAcks {
    fn default() -> Self {
        KafkaAcks::All
    }
}

impl KafkaAcks {
    /// librdkafka string form (`"0"`, `"1"`, `"all"`).
    pub fn as_str(self) -> &'static str {
        match self {
            KafkaAcks::Zero => "0",
            KafkaAcks::One => "1",
            KafkaAcks::All => "all",
        }
    }
}

/// Wire security for a Kafka Agent target, mapping 1:1 onto
/// librdkafka's `security.protocol`. Voice transcripts are
/// sensitive — anything but a localhost broker should run at
/// least `ssl`. Defaults to `plaintext` for backward
/// compatibility with configs written before this field existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum KafkaSecurityProtocol {
    /// No encryption, no authentication. Only sane for localhost
    /// or an otherwise-trusted network.
    #[default]
    Plaintext,
    /// TLS-encrypted, no client authentication.
    Ssl,
    /// SASL authentication over an unencrypted connection.
    /// Credentials AND transcripts travel in the clear — prefer
    /// `sasl_ssl` unless the network is trusted.
    SaslPlaintext,
    /// SASL authentication over TLS. The right choice for any
    /// non-localhost broker.
    SaslSsl,
}

impl KafkaSecurityProtocol {
    /// Every variant, in the order the funnel's picker renders.
    pub const ALL: [Self; 4] = [
        Self::Plaintext,
        Self::Ssl,
        Self::SaslPlaintext,
        Self::SaslSsl,
    ];

    /// librdkafka `security.protocol` string form.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Plaintext => "plaintext",
            Self::Ssl => "ssl",
            Self::SaslPlaintext => "sasl_plaintext",
            Self::SaslSsl => "sasl_ssl",
        }
    }

    /// Human-readable label rendered in the funnel.
    pub fn label(self) -> &'static str {
        match self {
            Self::Plaintext => "Plaintext (localhost only)",
            Self::Ssl => "SSL/TLS",
            Self::SaslPlaintext => "SASL over plaintext",
            Self::SaslSsl => "SASL over SSL/TLS (recommended)",
        }
    }

    /// Whether this protocol needs SASL credentials
    /// (mechanism + username + password).
    pub fn uses_sasl(self) -> bool {
        matches!(self, Self::SaslPlaintext | Self::SaslSsl)
    }
}

/// SASL mechanism for the `sasl_*` security protocols. PLAIN and
/// SCRAM are built into librdkafka; GSSAPI/Kerberos is deliberately
/// not offered (it would drag in a system libsasl2 dependency and
/// break the static, self-contained binary).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum KafkaSaslMechanism {
    /// Username/password in the clear inside the SASL exchange —
    /// pair with `sasl_ssl` so TLS covers it.
    #[default]
    Plain,
    // Explicit renames: derived kebab-case would give
    // "scram-sha256"; pin the canonical mechanism spelling.
    #[serde(rename = "scram-sha-256")]
    ScramSha256,
    #[serde(rename = "scram-sha-512")]
    ScramSha512,
}

impl KafkaSaslMechanism {
    /// Every variant, in the order the funnel's picker renders.
    pub const ALL: [Self; 3] = [Self::Plain, Self::ScramSha256, Self::ScramSha512];

    /// librdkafka `sasl.mechanism` string form.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Plain => "PLAIN",
            Self::ScramSha256 => "SCRAM-SHA-256",
            Self::ScramSha512 => "SCRAM-SHA-512",
        }
    }
}

/// Connection details for a Kafka-flavoured Agent target. Endpoint
/// is a librdkafka bootstrap list (e.g. `"localhost:9092"` or
/// `"broker-1:9092,broker-2:9092"`); topic is the destination
/// partition key the segment JSON lines get published to.
///
/// The SASL password is referenced by environment-variable NAME
/// (`sasl_password_env`), never stored in `config.toml` — the config
/// file syncs/backs up too easily to hold a secret.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KafkaAgentConnection {
    pub endpoint: String,
    pub topic: String,
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub acks: KafkaAcks,
    #[serde(default)]
    pub security_protocol: KafkaSecurityProtocol,
    /// Only consulted when `security_protocol` uses SASL; `None`
    /// there means [`KafkaSaslMechanism::Plain`].
    #[serde(default)]
    pub sasl_mechanism: Option<KafkaSaslMechanism>,
    #[serde(default)]
    pub sasl_username: Option<String>,
    /// NAME of the environment variable that holds the SASL
    /// password (e.g. `VOICE_BIRD_KAFKA_PASSWORD`). Resolved when
    /// the producer/consumer is built, so the secret itself never
    /// touches the config file.
    #[serde(default)]
    pub sasl_password_env: Option<String>,
}

impl KafkaAgentConnection {
    /// Validate the SASL field combination. `Ok(())` for non-SASL
    /// protocols regardless of the (ignored) SASL fields, so old
    /// configs and hand-edits stay loadable.
    pub fn validate_security(&self) -> anyhow::Result<()> {
        if !self.security_protocol.uses_sasl() {
            return Ok(());
        }
        let proto = self.security_protocol.as_str();
        if self
            .sasl_username
            .as_deref()
            .is_none_or(|u| u.trim().is_empty())
        {
            anyhow::bail!("security protocol '{proto}' requires sasl_username");
        }
        if self
            .sasl_password_env
            .as_deref()
            .is_none_or(|v| v.trim().is_empty())
        {
            anyhow::bail!(
                "security protocol '{proto}' requires sasl_password_env \
                 (the NAME of an environment variable holding the password)"
            );
        }
        Ok(())
    }
}

/// One user-configured Agent target. `id` is the stable key
/// referenced by the TUI's Targets list cursor and by
/// `App::agent_targets`; `name` is the user-facing label rendered
/// in the picker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentTargetConfig {
    pub id: AgentTargetId,
    pub name: String,
    #[serde(flatten)]
    pub connection: AgentConnection,
}

/// Tagged union of every Agent transport. `#[serde(tag = "kind")]`
/// so the TOML reads as
/// `[[agent_targets]]` / `kind = "kafka"` / `endpoint = "..."`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum AgentConnection {
    Kafka(KafkaAgentConnection),
}

impl AgentConnection {
    pub fn kind(&self) -> AgentConnectionKind {
        match self {
            AgentConnection::Kafka(_) => AgentConnectionKind::Kafka,
        }
    }
}

fn default_voicebird_server_url() -> String {
    "wss://voicebird.app/api/audio/stream".into()
}

fn default_refinement_window_ms() -> u32 {
    20_000
}

fn default_refinement_beam_size() -> u8 {
    5
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            hop_ms: 750,
            min_window_ms: 1000,
            engine_prefer: "auto".into(),
            audio_default_source: "microphone".into(),
            input_device: None,
            input_device_kind: None,
            last_app_id: None,
            refinement_model: None,
            refinement_window_ms: default_refinement_window_ms(),
            refinement_beam_size: default_refinement_beam_size(),
            voicebird_api_key: String::new(),
            voicebird_server_url: default_voicebird_server_url(),
            slot_settings: BTreeMap::new(),
            agent_targets: Vec::new(),
        }
    }
}

impl AppConfig {
    /// Resolve the on-disk config path.
    ///
    /// Production: `<dirs::config_dir>/voice-bird/config.toml` —
    /// the same path the user (and the TUI) have always read from.
    ///
    /// Tests: an env-var override (`VOICE_BIRD_TEST_CONFIG_PATH`)
    /// lets the test suite point every `App::new()` / `save()` at
    /// a process-local tempdir instead of the developer's real
    /// config. This un-flakes the two banner tests that asserted
    /// `app.banner.is_none()` (they fail when the developer's real
    /// `cloud_broadcast_enabled = true` + `voicebird_api_key = ""`
    /// triggers the on-launch banner) and stops the
    /// `c_toggle…_per_source_override` test from persisting its
    /// in-memory `sk-test` key and a flipped cloud flag over the
    /// user's real config on every `cargo test` run. The env var
    /// is unset in production — the override is inert.
    pub fn config_path() -> anyhow::Result<PathBuf> {
        if let Ok(p) = std::env::var("VOICE_BIRD_TEST_CONFIG_PATH") {
            if !p.is_empty() {
                return Ok(PathBuf::from(p));
            }
        }
        let base = dirs::config_dir().ok_or_else(|| anyhow::anyhow!("no config dir"))?;
        Ok(base.join("voice-bird").join("config.toml"))
    }

    pub fn load() -> anyhow::Result<Self> {
        let path = Self::config_path()?;
        if path.exists() {
            Self::load_from(&path)
        } else {
            Ok(Self::default())
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::config_path()?;
        self.save_to(&path)
    }

    pub fn load_from(path: &Path) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(path)?;
        let mut cfg: Self = toml::from_str(&s)?;
        // Drop any agent_target rows whose id collides with
        // an internal destination (e.g. `"default"`) — letting
        // one through would silently route its segments to
        // the legacy MCP buffer instead of the configured
        // broker. Log a warning so the user can fix the TOML
        // by hand.
        let before = cfg.agent_targets.len();
        cfg.agent_targets.retain(|t| {
            let keep = !is_reserved_agent_target_id(&t.id);
            if !keep {
                log::warn!("config: dropping agent_target with reserved id {:?}", t.id);
            }
            keep
        });
        if cfg.agent_targets.len() != before {
            // Persist the cleaned config so the user can see
            // what was dropped. Best-effort: a save failure
            // here is non-fatal — the in-memory cfg is still
            // correct for this session.
            if let Err(e) = cfg.save_to(path) {
                log::warn!(
                    "config: failed to persist cleaned config ({} agent_targets dropped): {e}",
                    before - cfg.agent_targets.len(),
                );
            }
        }
        Ok(cfg)
    }

    pub fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let body = toml::to_string_pretty(self)?;
        let out = if self.voicebird_api_key.is_empty() {
            body
        } else {
            format!("# Contains secrets. Do not share.\n{body}")
        };
        std::fs::write(path, out)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            // Best-effort: non-fatal if setting perms fails (e.g., on a
            // filesystem that doesn't support Unix modes).
            let _ = std::fs::set_permissions(path, perms);
        }
        Ok(())
    }
    /// Look up an Agent target by its stable id. Returns `None` if

    /// no target with that id has been configured (the user removed
    /// it, or the config file was hand-edited and lost the row).
    /// it, or the config file was hand-edited and lost the row).
    pub fn agent_target_by_id(&self, id: &str) -> Option<&AgentTargetConfig> {
        self.agent_targets.iter().find(|t| t.id == id)
    }

    /// Mutable variant of [`Self::agent_target_by_id`].
    pub fn agent_target_by_id_mut(&mut self, id: &str) -> Option<&mut AgentTargetConfig> {
        self.agent_targets.iter_mut().find(|t| t.id == id)
    }

    /// Insert-or-replace by id. Caller is responsible for minting
    /// the id on the add path; this method just enforces uniqueness
    /// so two targets can't share a session, and rejects reserved
    /// ids (e.g. `"default"`) so a user-configured target can't
    /// silently shadow the legacy MCP-backed session.
    /// Returns `Err` on a reserved id; the caller is expected to
    /// surface the failure to the user.
    pub fn upsert_agent_target(&mut self, target: AgentTargetConfig) -> anyhow::Result<()> {
        if is_reserved_agent_target_id(&target.id) {
            return Err(anyhow::anyhow!(
                "agent target id {:?} is reserved for an internal destination",
                target.id
            ));
        }
        if let Some(slot) = self.agent_targets.iter_mut().find(|t| t.id == target.id) {
            *slot = target;
        } else {
            self.agent_targets.push(target);
        }
        Ok(())
    }

    /// Drop the target with the given id. Returns true if anything
    /// was removed.
    pub fn remove_agent_target(&mut self, id: &str) -> bool {
        let before = self.agent_targets.len();
        self.agent_targets.retain(|t| t.id != id);
        self.agent_targets.len() != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn defaults_when_file_missing() {
        let c = AppConfig::default();
        assert_eq!(c.hop_ms, 750);
        assert_eq!(c.engine_prefer, "auto");
        // The slot settings map is empty on first run; the runtime
        // seeds slot 1 from SlotSettings::default() on construction.
        assert!(c.slot_settings.is_empty());
    }

    #[test]
    fn voicebird_api_key_roundtrips_through_toml() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        let c = AppConfig {
            voicebird_api_key: "vb-fake-12345".into(),
            ..AppConfig::default()
        };
        c.save_to(&path).unwrap();
        let loaded = AppConfig::load_from(&path).unwrap();
        assert_eq!(loaded.voicebird_api_key, "vb-fake-12345");
    }

    #[test]
    fn missing_voicebird_fields_deserialize_to_defaults() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        // Write a minimal config without the optional fields.
        std::fs::write(
            &path,
            r#"
hop_ms = 750
min_window_ms = 1000
engine_prefer = "auto"
audio_default_source = "microphone"
refinement_window_ms = 20000
refinement_beam_size = 5
"#,
        )
        .unwrap();
        let loaded = AppConfig::load_from(&path).unwrap();
        assert_eq!(loaded.voicebird_api_key, "");
        assert_eq!(loaded.voicebird_server_url, default_voicebird_server_url());
        // The legacy global fields are gone; the dropped keys are
        // silently ignored by the lenient TOML parser.
        assert!(loaded.slot_settings.is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn save_sets_0600_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        let c = AppConfig::default();
        c.save_to(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected 0600, got {:o}", mode);
    }

    #[test]
    fn save_with_secret_prepends_warning_comment() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        let c = AppConfig {
            voicebird_api_key: "vb-secret".into(),
            ..AppConfig::default()
        };
        c.save_to(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with("# Contains secrets. Do not share.\n"),
            "missing warning header; file was:\n{text}",
        );
    }

    #[test]
    fn save_without_secret_has_no_warning_comment() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        let c = AppConfig::default(); // empty api key
        c.save_to(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("Contains secrets"));
    }

    #[test]
    fn roundtrip_through_toml() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        let c = AppConfig {
            hop_ms: 600,
            min_window_ms: 800,
            engine_prefer: "whisperkit".into(),
            audio_default_source: "system".into(),
            input_device: Some("MacBook Pro Microphone".into()),
            input_device_kind: Some(AudioSessionKind::Input),
            last_app_id: Some("us.zoom.xos".into()),
            refinement_model: Some("large-v3-turbo".into()),
            refinement_window_ms: 20_000,
            refinement_beam_size: 5,
            voicebird_api_key: "vb-test".into(),
            voicebird_server_url: "wss://example.test/api/audio/stream".into(),
            slot_settings: BTreeMap::new(),
            agent_targets: Vec::new(),
        };
        c.save_to(&path).unwrap();
        let loaded = AppConfig::load_from(&path).unwrap();
        assert_eq!(loaded, c);
    }

    /// `agent_targets` survives a save/load round trip.
    /// The TOML form uses `[[agent_targets]]` with a
    /// tagged `kind = "kafka"` field; the parser must
    /// reconstruct the `AgentConnection::Kafka` variant
    /// exactly, including the `acks` enum string.
    #[test]
    fn agent_targets_round_trip_through_toml() {
        use crate::config::{AgentConnection, AgentTargetConfig, KafkaAcks, KafkaAgentConnection};
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        let mut c = AppConfig::default();
        c.agent_targets.push(AgentTargetConfig {
            id: "abc-123".into(),
            name: "prod".into(),
            connection: AgentConnection::Kafka(KafkaAgentConnection {
                endpoint: "broker-1:9092,broker-2:9092".into(),
                topic: "voice-bird-events".into(),
                client_id: Some("svc".into()),
                acks: KafkaAcks::All,
                security_protocol: Default::default(),
                sasl_mechanism: None,
                sasl_username: None,
                sasl_password_env: None,
            }),
        });
        c.save_to(&path).unwrap();
        let loaded = AppConfig::load_from(&path).unwrap();
        assert_eq!(loaded.agent_targets, c.agent_targets);
        // The `acks` string is the on-disk wire format —
        // pin it so a future refactor that changes
        // `KafkaAcks::as_str` doesn't silently break
        // existing config.toml files.
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(
            body.contains("acks = \"all\""),
            "acks wire format drift: {body}"
        );
    }

    /// A SASL-secured target round-trips through TOML with all
    /// four security fields intact, and the wire strings are
    /// pinned (librdkafka gets them verbatim via `as_str`, but
    /// the config file uses the serde snake/kebab-case forms).
    #[test]
    fn sasl_agent_target_round_trips_through_toml() {
        use crate::config::{
            AgentConnection, AgentTargetConfig, KafkaAcks, KafkaAgentConnection,
            KafkaSaslMechanism, KafkaSecurityProtocol,
        };
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        let mut c = AppConfig::default();
        c.agent_targets.push(AgentTargetConfig {
            id: "sec-1".into(),
            name: "secure".into(),
            connection: AgentConnection::Kafka(KafkaAgentConnection {
                endpoint: "broker:9093".into(),
                topic: "events".into(),
                client_id: None,
                acks: KafkaAcks::All,
                security_protocol: KafkaSecurityProtocol::SaslSsl,
                sasl_mechanism: Some(KafkaSaslMechanism::ScramSha256),
                sasl_username: Some("svc-user".into()),
                sasl_password_env: Some("VB_KAFKA_PW".into()),
            }),
        });
        c.save_to(&path).unwrap();
        let loaded = AppConfig::load_from(&path).unwrap();
        assert_eq!(loaded.agent_targets, c.agent_targets);
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(
            body.contains("security_protocol = \"sasl_ssl\""),
            "security_protocol wire format drift: {body}"
        );
        assert!(
            body.contains("sasl_mechanism = \"scram-sha-256\""),
            "sasl_mechanism wire format drift: {body}"
        );
        // The password itself must never appear — only the env
        // var NAME is stored.
        assert!(
            body.contains("sasl_password_env = \"VB_KAFKA_PW\""),
            "sasl_password_env missing: {body}"
        );
    }

    /// Configs written before the security fields existed load
    /// with plaintext defaults (backward compatibility).
    #[test]
    fn pre_security_config_rows_default_to_plaintext() {
        use crate::config::KafkaSecurityProtocol;
        let toml_row = r#"
            [[agent_targets]]
            id = "old-1"
            name = "legacy"
            kind = "kafka"
            endpoint = "localhost:9092"
            topic = "voice-bird"
        "#;
        #[derive(serde::Deserialize)]
        struct Wrapper {
            agent_targets: Vec<AgentTargetConfig>,
        }
        let w: Wrapper = toml::from_str(toml_row).unwrap();
        let AgentConnection::Kafka(k) = &w.agent_targets[0].connection;
        assert_eq!(k.security_protocol, KafkaSecurityProtocol::Plaintext);
        assert!(k.sasl_mechanism.is_none());
        assert!(k.sasl_username.is_none());
        assert!(k.sasl_password_env.is_none());
    }

    /// `validate_security` rejects SASL protocols missing a
    /// username or password env-var name, and ignores stale SASL
    /// fields on non-SASL protocols.
    #[test]
    fn validate_security_enforces_sasl_fields() {
        use crate::config::{KafkaSaslMechanism, KafkaSecurityProtocol};
        let base = KafkaAgentConnection {
            endpoint: "b:9093".into(),
            topic: "t".into(),
            client_id: None,
            acks: KafkaAcks::All,
            security_protocol: KafkaSecurityProtocol::SaslSsl,
            sasl_mechanism: Some(KafkaSaslMechanism::Plain),
            sasl_username: Some("u".into()),
            sasl_password_env: Some("PW_ENV".into()),
        };
        assert!(base.validate_security().is_ok());

        let mut no_user = base.clone();
        no_user.sasl_username = None;
        assert!(no_user.validate_security().is_err());
        let mut blank_user = base.clone();
        blank_user.sasl_username = Some("  ".into());
        assert!(blank_user.validate_security().is_err());

        let mut no_pw = base.clone();
        no_pw.sasl_password_env = None;
        assert!(no_pw.validate_security().is_err());

        // Missing mechanism is fine — it defaults to PLAIN.
        let mut no_mech = base.clone();
        no_mech.sasl_mechanism = None;
        assert!(no_mech.validate_security().is_ok());

        // Plaintext ignores half-filled SASL fields entirely.
        let mut plain = base.clone();
        plain.security_protocol = KafkaSecurityProtocol::Plaintext;
        plain.sasl_username = None;
        assert!(plain.validate_security().is_ok());
    }

    /// `upsert_agent_target` updates in place when the id
    /// matches, and appends otherwise. Pin both branches.
    #[test]
    fn upsert_agent_target_replaces_by_id() {
        use crate::config::{AgentConnection, AgentTargetConfig, KafkaAcks, KafkaAgentConnection};
        let mut c = AppConfig::default();
        let target = AgentTargetConfig {
            id: "id-1".into(),
            name: "first".into(),
            connection: AgentConnection::Kafka(KafkaAgentConnection {
                endpoint: "b1:9092".into(),
                topic: "t1".into(),
                client_id: None,
                acks: KafkaAcks::All,
                security_protocol: Default::default(),
                sasl_mechanism: None,
                sasl_username: None,
                sasl_password_env: None,
            }),
        };
        c.upsert_agent_target(target.clone()).unwrap();
        assert_eq!(c.agent_targets.len(), 1);
        // Mutate, then upsert with the same id; the row
        // should be replaced, not appended.
        let mut updated = target.clone();
        updated.name = "renamed".into();
        c.upsert_agent_target(updated).unwrap();
        assert_eq!(c.agent_targets.len(), 1);
        assert_eq!(c.agent_targets[0].name, "renamed");
        // Removing by id should clear the slot.
        assert!(c.remove_agent_target("id-1"));
        assert!(c.agent_targets.is_empty());
        // Removing a missing id is a no-op that returns false.
        assert!(!c.remove_agent_target("missing"));
    }

    #[test]
    fn upsert_agent_target_rejects_reserved_id() {
        // The consumer-dispatch branch in
        // `App::start_section_consumer_task` treats
        // `session_id == "default"` as a special case
        // (route to the legacy MCP buffer) and any other
        // unknown id as "drop with a warning". Letting a
        // user-configured target claim `"default"` would
        // silently route its segments to the MCP buffer
        // — so `upsert_agent_target` must reject it.
        let mut c = AppConfig::default();
        let bad = AgentTargetConfig {
            id: "default".into(),
            name: "evil".into(),
            connection: AgentConnection::Kafka(KafkaAgentConnection {
                endpoint: "b1:9092".into(),
                topic: "t1".into(),
                client_id: None,
                acks: KafkaAcks::All,
                security_protocol: Default::default(),
                sasl_mechanism: None,
                sasl_username: None,
                sasl_password_env: None,
            }),
        };
        assert!(c.upsert_agent_target(bad).is_err());
        // And nothing was inserted.
        assert!(c.agent_targets.is_empty());
        // Non-reserved ids still go through.
        let good = AgentTargetConfig {
            id: "real-uuid".into(),
            name: "ok".into(),
            connection: AgentConnection::Kafka(KafkaAgentConnection {
                endpoint: "b1:9092".into(),
                topic: "t1".into(),
                client_id: None,
                acks: KafkaAcks::All,
                security_protocol: Default::default(),
                sasl_mechanism: None,
                sasl_username: None,
                sasl_password_env: None,
            }),
        };
        assert!(c.upsert_agent_target(good).is_ok());
        assert_eq!(c.agent_targets.len(), 1);
    }

    #[test]
    fn load_from_drops_reserved_id_rows() {
        // A hand-edited config with `id = "default"` would
        // silently shadow the MCP-backed session if it
        // loaded cleanly. `load_from` must filter it out
        // and persist the cleaned config so the user can
        // see what was dropped.
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        let body = "\
hop_ms = 750\n\
min_window_ms = 1000\n\
engine_prefer = \"auto\"\n\
audio_default_source = \"microphone\"\n\
refinement_window_ms = 20000\n\
refinement_beam_size = 5\n\
voicebird_api_key = \"\"\n\
voicebird_server_url = \"wss://voicebird.app/api/audio/stream\"\n\
\n\
[[agent_targets]]\n\
id = \"default\"\n\
name = \"evil\"\n\
kind = \"kafka\"\n\
endpoint = \"b1:9092\"\n\
topic = \"t1\"\n\
acks = \"all\"\n\
\n\
[[agent_targets]]\n\
id = \"real-uuid\"\n\
name = \"ok\"\n\
kind = \"kafka\"\n\
endpoint = \"b1:9092\"\n\
topic = \"t1\"\n\
acks = \"all\"\n\
";
        std::fs::write(&path, body).unwrap();
        let c = AppConfig::load_from(&path).unwrap();
        // The reserved row was dropped; the real one survived.
        assert_eq!(c.agent_targets.len(), 1);
        assert_eq!(c.agent_targets[0].id, "real-uuid");
        // The on-disk config was rewritten without the
        // reserved row.
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(!on_disk.contains("id = \"default\""));
        assert!(on_disk.contains("id = \"real-uuid\""));
    }

    // ── Per-slot settings (SlotSettings, slot_settings map) ─────────────
    //
    // These pin the storage layer for the per-slot settings refactor:
    // each slot owns its own Cloud/Language/Model/Path settings, and
    // AppConfig persists them in a `slot_settings` map keyed by SlotId.
    // AppConfig persists them in a `slot_settings` map keyed by SlotId.
    // The field and the BTreeMap are added in commit 1;
    // `slot_settings` is the new source of truth for runtime code.

    #[test]
    fn slot_settings_default_matches_legacy_appconfig_defaults() {
        let s = SlotSettings::default();
        // Cloud off, English, auto-pickable default model, default path.
        assert!(!s.cloud_on);
        assert_eq!(s.language, "en");
        assert_eq!(s.model, "distil-small.en");
        assert_eq!(s.path, "~/voice-bird/sessions");
    }

    #[test]
    fn slot_settings_round_trips_through_toml() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        let mut c = AppConfig::default();
        c.slot_settings.insert(
            slot_settings_key(2),
            SlotSettings {
                cloud_on: true,
                language: "ru".into(),
                model: "tiny.en".into(),
                path: "~/voice-bird/slot-two".into(),
            },
        );
        c.save_to(&path).unwrap();
        let loaded = AppConfig::load_from(&path).unwrap();
        assert_eq!(loaded.slot_settings.len(), 1);
        let saved = &loaded.slot_settings[&slot_settings_key(2)];
        assert!(saved.cloud_on);
        assert_eq!(saved.language, "ru");
        assert_eq!(saved.model, "tiny.en");
        assert_eq!(saved.path, "~/voice-bird/slot-two");
    }

    #[test]
    fn slot_settings_keeps_each_slot_independent() {
        // Two slots, two distinct settings. Editing one must not
        // touch the other.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        let mut c = AppConfig::default();
        c.slot_settings.insert(
            slot_settings_key(1),
            SlotSettings {
                cloud_on: false,
                language: "en".into(),
                model: "distil-small.en".into(),
                path: "~/voice-bird/slot-one".into(),
            },
        );
        c.slot_settings.insert(
            slot_settings_key(2),
            SlotSettings {
                cloud_on: true,
                language: "ru".into(),
                model: "tiny.en".into(),
                path: "~/voice-bird/slot-two".into(),
            },
        );
        c.save_to(&path).unwrap();
        let loaded = AppConfig::load_from(&path).unwrap();
        assert_eq!(loaded.slot_settings[&slot_settings_key(1)].cloud_on, false);
        assert_eq!(loaded.slot_settings[&slot_settings_key(2)].cloud_on, true);
        assert_ne!(
            loaded.slot_settings[&slot_settings_key(1)].path,
            loaded.slot_settings[&slot_settings_key(2)].path,
        );
    }

    #[test]
    fn slot_settings_absent_entry_returns_default() {
        // A slot not yet in the map gets the default settings.
        // The runtime layer (App) supplies this via
        // SlotSettings::default(); the storage layer only
        // round-trips existing entries.
        let c = AppConfig::default();
        assert!(c.slot_settings.is_empty());
    }
}
