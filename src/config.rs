use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::session::layout::SessionSource;

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

/// Stable identifier for a source used to key `source_overrides`. Format:
/// `device:input:<name>` / `device:output:<name>` /
/// `app:<bundle-or-name>@device:<device>`.
pub fn source_id(source: &SessionSource, kind: Option<AudioSessionKind>) -> String {
    match source {
        SessionSource::Microphone => format!(
            "device:{}:default",
            kind_str(kind.unwrap_or(AudioSessionKind::Input))
        ),
        SessionSource::System => format!(
            "device:{}:default",
            kind_str(kind.unwrap_or(AudioSessionKind::Output))
        ),
        SessionSource::App {
            name, device_name, ..
        } => {
            if device_name.is_empty() {
                format!("app:{name}")
            } else {
                format!("app:{name}@device:{device_name}")
            }
        }
    }
}

/// Variant of [`source_id`] keyed by an explicit device name. Preferred
/// over [`source_id`] when the actual selected device is known (the
/// generic Microphone/System variants collapse multiple devices to
/// `default`, which would conflate per-device overrides).
pub fn device_source_id(name: &str, kind: AudioSessionKind) -> String {
    format!("device:{}:{}", kind_str(kind), name)
}

fn kind_str(kind: AudioSessionKind) -> &'static str {
    match kind {
        AudioSessionKind::Input => "input",
        AudioSessionKind::Output => "output",
        AudioSessionKind::App => "app",
    }
}

/// Global default for a slot's per-slot settings. Every slot
/// starts with `SlotConfig::default_passthrough()` and reads
/// unset fields from here. The user can change defaults
/// globally via a settings UI (not yet implemented) or by
/// editing the config file directly. The picker cursor
/// (device / app) and agent routing are stored per-slot
/// (in `slot_picker_memo` / `pending_agent_overrides`) — they
/// are not part of this default since they index into the
/// live inventory rather than carry stable names.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DefaultSlotConfig {
    pub cloud_on: bool,
    pub language: String,
    pub model: String,
    pub path: String,
}

impl Default for DefaultSlotConfig {
    fn default() -> Self {
        Self {
            cloud_on: false,
            language: "en".into(),
            model: "distil-small.en".into(),
            path: "~/voice-bird/sessions".into(),
        }
    }
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
    /// the desktop client streams to. Defaults to the hosted production
    /// server.
    #[serde(default = "default_voicebird_server_url")]
    pub voicebird_server_url: String,

    /// Per-slot override for which cloud Agent to run on `g`.
    /// Keyed by `SlotId` (so the picker picks persist per slot).
    /// When absent, the picker cursor in §10 falls back to the
    /// most recently used agent (`last_character_id`).
    #[serde(default)]
    pub character_overrides: BTreeMap<String, String>,

    /// Most recently used Agent id, persisted between sessions.
    /// The §11 `g` key handler uses this as the default when a
    /// slot has no override.
    #[serde(default)]
    pub last_character_id: Option<String>,

    /// Set to `true` after the user accepts the one-time consent
    /// modal (§11) that asks before sending transcripts that came
    /// from a `cloud_on = false` recording. The flag sticks across
    /// launches — once accepted, the `g` key fires immediately
    /// for every future recording on every slot.
    #[serde(default)]
    pub dont_ask_character_upload: bool,

    /// Global default for a slot's per-slot settings. Each slot
    /// reads unset fields from here. The user customizes a slot
    /// by setting that field in the slot's `SlotConfig`; the
    /// `DefaultSlotConfig` is the source of truth for what
    /// "default" means.
    #[serde(default)]
    pub default_slot_config: DefaultSlotConfig,
}

/// Ids that the segment dispatcher in the consumer task
/// inside `App::start_section` compares against as a
/// special case (e.g. `"default"` routes to the legacy
/// MCP-backed `ServerState`). Letting a user-configured

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
            character_overrides: BTreeMap::new(),
            last_character_id: None,
            dont_ask_character_upload: false,
            default_slot_config: DefaultSlotConfig::default(),
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
        let cfg: Self = toml::from_str(&s)?;
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

    /// Expand `~/` in a path string to the user's home directory.
    /// Pure string transform — no IO. Used by `App::export_transcript`
    /// and `start_section` to resolve a per-slot path (or the
    /// default) into an absolute filesystem path.
    pub fn expand_tilde(path: &str) -> String {
        if let Some(rest) = path.strip_prefix("~/") {
            if let Some(home) = dirs::home_dir() {
                return home.join(rest).to_string_lossy().into_owned();
            }
        }
        path.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn defaults_when_file_missing() {
        let c = AppConfig::default();
        assert_eq!(c.default_slot_config.model, "distil-small.en");
        assert_eq!(c.hop_ms, 750);
        assert_eq!(c.engine_prefer, "auto");
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
        // Write an old-style config without the new fields.
        std::fs::write(
            &path,
            r#"
default_model = "distil-small.en"
language = "en"
session_dir = "~/voice-bird/sessions"
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
        assert!(!loaded.default_slot_config.cloud_on);
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
            character_overrides: BTreeMap::new(),
            last_character_id: None,
            dont_ask_character_upload: false,
            default_slot_config: DefaultSlotConfig::default(),
        };
        c.save_to(&path).unwrap();
        let loaded = AppConfig::load_from(&path).unwrap();
        assert_eq!(loaded, c);
    }

    /// `default_slot_config` must round-trip through toml:
    /// a non-default value is preserved on save + load.
    #[test]
    fn default_slot_config_round_trips_through_toml() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        let mut c = AppConfig::default();
        c.default_slot_config.cloud_on = true;
        c.default_slot_config.language = "pl".into();
        c.default_slot_config.model = "large-v3-turbo".into();
        c.default_slot_config.path = "~/sessions/team-A".into();
        c.save_to(&path).unwrap();
        let loaded = AppConfig::load_from(&path).unwrap();
        assert!(loaded.default_slot_config.cloud_on);
        assert_eq!(loaded.default_slot_config.language, "pl");
        assert_eq!(loaded.default_slot_config.model, "large-v3-turbo");
        assert_eq!(loaded.default_slot_config.path, "~/sessions/team-A");
    }

    #[test]
    fn source_id_distinguishes_kind_and_app_variants() {
        // Microphone with explicit Input kind:
        let mic = source_id(&SessionSource::Microphone, Some(AudioSessionKind::Input));
        assert_eq!(mic, "device:input:default");
        // System with explicit Output kind:
        let sys = source_id(&SessionSource::System, Some(AudioSessionKind::Output));
        assert_eq!(sys, "device:output:default");
        // App variant ignores the kind argument; key includes device pairing.
        let zoom = source_id(
            &SessionSource::App {
                id: "us.zoom.xos".into(),
                name: "Zoom".into(),
                device_name: "MacBook Pro Speakers".into(),
            },
            None,
        );
        assert_eq!(zoom, "app:Zoom@device:MacBook Pro Speakers");
        // Device-specific keys via device_source_id:
        let epos = device_source_id("EPOS PC 8 USB", AudioSessionKind::Input);
        assert_eq!(epos, "device:input:EPOS PC 8 USB");
    }

    #[test]
    fn app_source_key_separates_same_app_on_different_devices() {
        let zoom_speakers = source_id(
            &SessionSource::App {
                id: "us.zoom.xos".into(),
                name: "Zoom".into(),
                device_name: "MacBook Pro Speakers".into(),
            },
            None,
        );
        let zoom_airpods = source_id(
            &SessionSource::App {
                id: "us.zoom.xos".into(),
                name: "Zoom".into(),
                device_name: "AirPods Pro".into(),
            },
            None,
        );
        assert_ne!(zoom_speakers, zoom_airpods);
    }
    /// `expand_tilde` is a pure string transform — no IO.
    /// It expands `~/foo` to `$HOME/foo` and leaves other
    /// paths untouched. Tests cover both branches.
    #[test]
    fn expand_tilde_replaces_home_prefix() {
        let path = AppConfig::expand_tilde("~/doc/sessions");
        if let Some(home) = dirs::home_dir() {
            assert_eq!(path, home.join("doc/sessions").to_string_lossy());
        } else {
            // No HOME — graceful fallback.
            assert_eq!(path, "~/doc/sessions");
        }
    }

    #[test]
    fn expand_tilde_leaves_absolute_paths_alone() {
        let abs = "/etc/voice-bird/sessions";
        assert_eq!(
            AppConfig::expand_tilde(abs),
            abs
        );
    }

    /// §9b: per-slot Agent override + last-used agent id +
    /// `dont_ask_character_upload` consent flag must all survive a
    /// save/load round trip. Old config.toml files written before
    /// these fields existed must still parse — the
    /// `#[serde(default)]` attributes give every missing field its
    /// default.
    #[test]
    fn character_run_fields_round_trip_through_toml() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        let mut c = AppConfig::default();
        c.character_overrides
            .insert("2".into(), "uuid-prod-events".into());
        c.character_overrides
            .insert("3".into(), "uuid-zoom-bridge".into());
        c.last_character_id = Some("uuid-zoom-bridge".into());
        c.dont_ask_character_upload = true;
        c.save_to(&path).unwrap();
        let loaded = AppConfig::load_from(&path).unwrap();
        assert_eq!(loaded.character_overrides.len(), 2);
        assert_eq!(
            loaded.character_overrides.get("2").map(String::as_str),
            Some("uuid-prod-events")
        );
        assert_eq!(loaded.last_character_id.as_deref(), Some("uuid-zoom-bridge"));
        assert!(loaded.dont_ask_character_upload);
    }

    /// Old config.toml files (pre-§9b) load with the new fields
    /// at their defaults — `#[serde(default)]` makes the missing
    /// keys behave like absent.
    #[test]
    fn character_run_fields_default_when_missing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "default_model = \"tiny.en\"\nlanguage = \"en\"\n\
             session_dir = \"~/sessions\"\n\
             hop_ms = 750\n\
             min_window_ms = 1000\n\
             engine_prefer = \"auto\"\n\
             audio_default_source = \"microphone\"\n\
             voicebird_server_url = \"wss://voicebird.app/api/audio/stream\"\n\
             voicebird_api_key = \"\"\n\
             cloud_broadcast_enabled = false\n\
             source_overrides = {}\n",
        )
        .unwrap();
        let loaded = AppConfig::load_from(&path).unwrap();
        assert!(loaded.character_overrides.is_empty());
        assert!(loaded.last_character_id.is_none());
        assert!(!loaded.dont_ask_character_upload);
    }
}
