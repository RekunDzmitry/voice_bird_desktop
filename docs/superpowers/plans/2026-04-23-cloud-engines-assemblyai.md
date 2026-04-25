# AssemblyAI Cloud Engine + In-App Settings — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an opt-in AssemblyAI Universal-Streaming cloud transcription engine and a full-screen in-app settings view for editing config (including the new API key), while preserving the "local by default" privacy stance.

**Architecture:** One new `TranscriptionEngine` implementation that streams PCM over WebSocket to AssemblyAI; `EngineConfig` is split into a `Local { model_path, ... }` / `Cloud { api_key, ... }` enum. A new `AppMode::Settings` drives a full-screen settings view rendered from a new `settings_view` module. A `CLOUD` badge on the header and a recording-time reminder surface when a cloud engine is active.

**Tech Stack:** Rust 2021, tokio, ratatui, crossterm, tokio-tungstenite (new), rustls (new), serde/serde_json.

**Design doc:** `docs/superpowers/specs/2026-04-23-cloud-engines-assemblyai-design.md`

---

## File Structure

| File | Status | Responsibility |
|---|---|---|
| `Cargo.toml` | modify | Add `tokio-tungstenite`, `rustls`, `webpki-roots`, `url`; new `engine-smoke-assemblyai` feature |
| `src/config.rs` | modify | Add `assemblyai_api_key` field; `0600` chmod on save; secret warning comment |
| `src/transcription/mod.rs` | modify | `EngineConfig` → enum; `select_engine` gains `"assemblyai"` branch; register new module |
| `src/transcription/whisper_rs_engine.rs` | modify | Match `EngineConfig::Local` |
| `src/transcription/whisper_kit_engine.rs` | modify | Match `EngineConfig::Local` |
| `src/transcription/mock.rs` | modify | Match both variants; update `test_cfg` |
| `src/transcription/assemblyai_engine.rs` | create | New engine: WebSocket, PCM conversion, event mapping |
| `src/app.rs` | modify | `AppMode::Settings`; `is_cloud_engine` flag; cloud-engine branch in `start_recording` |
| `src/settings_view.rs` | create | Settings view render + key handlers |
| `src/ui.rs` | modify | CLOUD badge in header; delegate settings rendering; 3-second recording reminder |
| `src/main.rs` | modify | Route keys to settings handler when `AppMode::Settings`; `,` opens settings |
| `src/lib.rs` | modify | (no new pub mods — settings_view lives in bin crate) |
| `tests/assemblyai_engine.rs` | create | Mock WebSocket server + unit/integration tests for the engine |
| `tests/engine_smoke.rs` | modify | Add `engine-smoke-assemblyai` gated smoke test using real AssemblyAI |
| `README.md` | modify | Reframe "local by default"; add Cloud engines (optional) subsection |
| `CLAUDE.md` | modify | Update project description and architecture list |

`src/settings_view.rs` is intentionally a sibling module in the bin crate (next to `src/app.rs`, `src/ui.rs`) to match the existing flat-file layout. We do NOT convert `src/ui.rs` to `src/ui/mod.rs`.

---

## Task 1: Add Cargo dependencies

**Files:**
- Modify: `Cargo.toml`

- [ ] **Step 1: Add tokio-tungstenite + TLS deps to `[dependencies]`**

Add under the `# Async` block in `Cargo.toml`:

```toml
# WebSocket client for AssemblyAI cloud engine. Uses rustls (consistent
# with reqwest already in-tree) rather than native-tls to keep a single
# TLS backend across the crate.
tokio-tungstenite = { version = "0.21", default-features = false, features = ["connect", "rustls-tls-webpki-roots"] }
url = "2"
```

`rustls` and `webpki-roots` are transitively pulled in via `tokio-tungstenite`'s `rustls-tls-webpki-roots` feature — no direct declaration needed.

- [ ] **Step 2: Add the engine-smoke-assemblyai feature**

In the `[features]` section:

```toml
[features]
engine-smoke = []
# Requires ASSEMBLYAI_API_KEY env var at test time; hits the real service.
engine-smoke-assemblyai = []
```

- [ ] **Step 3: Verify build**

Run: `cargo build`
Expected: compiles with new deps pulled.

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "deps: add tokio-tungstenite (rustls) for AssemblyAI engine"
```

---

## Task 2: AppConfig — add `assemblyai_api_key` field

**Files:**
- Modify: `src/config.rs`

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `src/config.rs`:

```rust
    #[test]
    fn assemblyai_api_key_roundtrips_through_toml() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        let mut c = AppConfig::default();
        c.assemblyai_api_key = "sk-fake-12345".into();
        c.save_to(&path).unwrap();
        let loaded = AppConfig::load_from(&path).unwrap();
        assert_eq!(loaded.assemblyai_api_key, "sk-fake-12345");
    }

    #[test]
    fn missing_assemblyai_api_key_deserializes_to_empty_string() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        // Write an old-style config without the field.
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
        assert_eq!(loaded.assemblyai_api_key, "");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib config::tests::assemblyai_api_key`
Expected: FAIL (`assemblyai_api_key` field does not exist).

- [ ] **Step 3: Add the field to AppConfig**

In `src/config.rs`, add to the `AppConfig` struct (place directly after `refinement_beam_size`):

```rust
    /// AssemblyAI API key. Stored in plaintext in config.toml — file
    /// permissions are the only protection. Empty string = unset.
    #[serde(default)]
    pub assemblyai_api_key: String,
```

And to `impl Default for AppConfig`, add:

```rust
            assemblyai_api_key: String::new(),
```

(directly after `refinement_beam_size: default_refinement_beam_size(),`).

- [ ] **Step 4: Fix the pre-existing roundtrip test**

`roundtrip_through_toml` constructs `AppConfig` with explicit fields. Add one more field:

```rust
            refinement_beam_size: 5,
            assemblyai_api_key: "sk-test".into(),
```

- [ ] **Step 5: Run tests**

Run: `cargo test --lib config`
Expected: all config tests pass.

- [ ] **Step 6: Commit**

```bash
git add src/config.rs
git commit -m "config: add assemblyai_api_key field with serde default"
```

---

## Task 3: AppConfig — `0600` chmod on Unix save + secret warning comment

**Files:**
- Modify: `src/config.rs`

- [ ] **Step 1: Write the failing test**

Add to `#[cfg(test)] mod tests` in `src/config.rs`:

```rust
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
        let mut c = AppConfig::default();
        c.assemblyai_api_key = "sk-secret".into();
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib config::tests`
Expected: FAIL (no chmod, no warning header).

- [ ] **Step 3: Implement**

Replace the existing `save_to` body in `src/config.rs`:

```rust
    pub fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let body = toml::to_string_pretty(self)?;
        let out = if self.assemblyai_api_key.is_empty() {
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
```

- [ ] **Step 4: Run tests**

Run: `cargo test --lib config::tests`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add src/config.rs
git commit -m "config: chmod 0600 + secret warning header on save"
```

---

## Task 4: Convert `EngineConfig` struct → enum (Local-only first)

**Files:**
- Modify: `src/transcription/mod.rs`
- Modify: `src/transcription/whisper_rs_engine.rs`
- Modify: `src/transcription/whisper_kit_engine.rs`
- Modify: `src/transcription/mock.rs`
- Modify: `src/app.rs` (call site in `start_recording`)
- Modify: `tests/engine_smoke.rs`

This task intentionally introduces only the `Local` variant. Cloud is added in Task 5 so the diff stays reviewable.

- [ ] **Step 1: Replace the `EngineConfig` struct with an enum**

In `src/transcription/mod.rs`, replace:

```rust
#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub model_path: std::path::PathBuf,
    pub language: Option<String>,
    pub sample_rate: u32,   // always 16_000
    pub hop_ms: u32,        // WhisperRsEngine only
    pub min_window_ms: u32, // WhisperRsEngine only
}
```

with:

```rust
#[derive(Debug, Clone)]
pub enum EngineConfig {
    Local {
        model_path: std::path::PathBuf,
        language: Option<String>,
        sample_rate: u32,   // always 16_000
        hop_ms: u32,        // whisper-rs only
        min_window_ms: u32, // whisper-rs only
    },
}
```

- [ ] **Step 2: Update WhisperRsEngine to match the Local variant**

In `src/transcription/whisper_rs_engine.rs`, inside `start`, replace the top-of-function destructuring:

```rust
        let model_path = cfg.model_path.clone();
        let language = cfg.language.clone();
        let hop_ms = cfg.hop_ms as u64;
        let min_window_ms = cfg.min_window_ms as u64;
```

with:

```rust
        let (model_path, language, hop_ms, min_window_ms) = match cfg {
            EngineConfig::Local {
                model_path,
                language,
                hop_ms,
                min_window_ms,
                ..
            } => (model_path, language, hop_ms as u64, min_window_ms as u64),
        };
```

- [ ] **Step 3: Update WhisperKitEngine to match the Local variant**

In `src/transcription/whisper_kit_engine.rs`, in `start`, find where `cfg` fields are used (search for `cfg.model_path`, `cfg.language`, `cfg.sample_rate`) and replace with a destructure at the top:

```rust
        let (model_path, language, sample_rate) = match cfg {
            EngineConfig::Local {
                model_path,
                language,
                sample_rate,
                ..
            } => (model_path, language, sample_rate),
        };
```

Then replace each subsequent `cfg.model_path` / `cfg.language` / `cfg.sample_rate` with the bound locals.

- [ ] **Step 4: Update MockEngine test_cfg**

In `src/transcription/mock.rs`, replace the `test_cfg` helper:

```rust
    fn test_cfg() -> EngineConfig {
        EngineConfig::Local {
            model_path: std::path::PathBuf::from("/dev/null"),
            language: None,
            sample_rate: 16_000,
            hop_ms: 750,
            min_window_ms: 1000,
        }
    }
```

(The `MockEngine::start` body does not touch `cfg`, so no further change there.)

- [ ] **Step 5: Update the app.rs call site**

In `src/app.rs`, around line 463, replace the `EngineConfig { ... }` struct literal with:

```rust
        let handle = match engine.start(EngineConfig::Local {
            model_path,
            language: Some(self.config.language.clone()).filter(|s| s != "auto"),
            sample_rate: 16_000,
            hop_ms: self.config.hop_ms,
            min_window_ms: self.config.min_window_ms,
        }) {
```

- [ ] **Step 6: Update tests/engine_smoke.rs**

Replace:

```rust
        let handle = engine.start(EngineConfig {
            model_path: cache,
            language: Some("en".into()),
            sample_rate: 16_000,
            hop_ms: 750, min_window_ms: 1000,
        }).unwrap();
```

with:

```rust
        let handle = engine.start(EngineConfig::Local {
            model_path: cache,
            language: Some("en".into()),
            sample_rate: 16_000,
            hop_ms: 750, min_window_ms: 1000,
        }).unwrap();
```

- [ ] **Step 7: Search for any remaining `EngineConfig {` construction sites**

Run: `grep -rn "EngineConfig {" src tests` (via the Grep tool).
Expected: no results. Fix any that remain by converting to `EngineConfig::Local { ... }`.

- [ ] **Step 8: Build + run all tests**

Run: `cargo test`
Expected: all tests pass.

- [ ] **Step 9: Commit**

```bash
git add src/transcription/mod.rs src/transcription/whisper_rs_engine.rs \
    src/transcription/whisper_kit_engine.rs src/transcription/mock.rs \
    src/app.rs tests/engine_smoke.rs
git commit -m "refactor(engine): convert EngineConfig struct to enum (Local variant)"
```

---

## Task 5: Add `Cloud` variant to `EngineConfig`; make existing engines reject it

**Files:**
- Modify: `src/transcription/mod.rs`
- Modify: `src/transcription/whisper_rs_engine.rs`
- Modify: `src/transcription/whisper_kit_engine.rs`
- Modify: `src/transcription/mock.rs`

- [ ] **Step 1: Write the failing test**

Add to `src/transcription/mock.rs` `mod tests`:

```rust
    #[tokio::test]
    async fn mock_engine_rejects_cloud_variant() {
        let mut engine = MockEngine::new(vec![]);
        let cfg = EngineConfig::Cloud {
            api_key: "sk-x".into(),
            language: None,
            sample_rate: 16_000,
        };
        let err = engine.start(cfg).err().expect("expected Err on Cloud");
        assert!(
            err.to_string().to_lowercase().contains("cloud"),
            "error should mention cloud variant; got: {err}",
        );
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib transcription::mock::tests::mock_engine_rejects_cloud_variant`
Expected: FAIL (variant does not exist).

- [ ] **Step 3: Extend the enum**

In `src/transcription/mod.rs`, add the variant:

```rust
#[derive(Debug, Clone)]
pub enum EngineConfig {
    Local {
        model_path: std::path::PathBuf,
        language: Option<String>,
        sample_rate: u32,
        hop_ms: u32,
        min_window_ms: u32,
    },
    Cloud {
        api_key: String,
        language: Option<String>,
        sample_rate: u32,
    },
}
```

- [ ] **Step 4: Make WhisperRsEngine reject Cloud**

In `src/transcription/whisper_rs_engine.rs`, change the match in `start`:

```rust
        let (model_path, language, hop_ms, min_window_ms) = match cfg {
            EngineConfig::Local {
                model_path,
                language,
                hop_ms,
                min_window_ms,
                ..
            } => (model_path, language, hop_ms as u64, min_window_ms as u64),
            EngineConfig::Cloud { .. } => {
                anyhow::bail!("WhisperRsEngine requires EngineConfig::Local");
            }
        };
```

- [ ] **Step 5: Make WhisperKitEngine reject Cloud**

Same pattern in `src/transcription/whisper_kit_engine.rs`:

```rust
        let (model_path, language, sample_rate) = match cfg {
            EngineConfig::Local { model_path, language, sample_rate, .. } =>
                (model_path, language, sample_rate),
            EngineConfig::Cloud { .. } => {
                anyhow::bail!("WhisperKitEngine requires EngineConfig::Local");
            }
        };
```

- [ ] **Step 6: Make MockEngine reject Cloud**

In `src/transcription/mock.rs`, replace the `fn start` signature body:

```rust
    fn start(&mut self, cfg: EngineConfig) -> anyhow::Result<EngineHandle> {
        match cfg {
            EngineConfig::Local { .. } => {}
            EngineConfig::Cloud { .. } => {
                anyhow::bail!("MockEngine requires EngineConfig::Local");
            }
        }
        let (pcm_tx, mut pcm_rx) = mpsc::channel::<Vec<f32>>(16);
        /* …existing body unchanged… */
```

- [ ] **Step 7: Run tests**

Run: `cargo test`
Expected: all pass, including the new `mock_engine_rejects_cloud_variant`.

- [ ] **Step 8: Commit**

```bash
git add src/transcription/mod.rs src/transcription/whisper_rs_engine.rs \
    src/transcription/whisper_kit_engine.rs src/transcription/mock.rs
git commit -m "engine: add EngineConfig::Cloud variant; local engines reject it"
```

---

## Task 6: Scaffold `AssemblyAiEngine` and register module

**Files:**
- Create: `src/transcription/assemblyai_engine.rs`
- Modify: `src/transcription/mod.rs`

- [ ] **Step 1: Create the new module**

Create `src/transcription/assemblyai_engine.rs` with:

```rust
use tokio::sync::{broadcast, mpsc, oneshot};

use super::{EngineConfig, EngineEvent, EngineHandle, TranscriptionEngine};

/// AssemblyAI Universal-Streaming v3 engine. Opens a WebSocket to
/// `wss://streaming.assemblyai.com/v3/ws`, forwards 16-kHz mono PCM as
/// binary frames, and maps incoming JSON turns onto `EngineEvent`s.
pub struct AssemblyAiEngine {
    api_key: String,
}

impl AssemblyAiEngine {
    pub fn new(api_key: String) -> Self {
        Self { api_key }
    }
}

impl TranscriptionEngine for AssemblyAiEngine {
    fn start(&mut self, cfg: EngineConfig) -> anyhow::Result<EngineHandle> {
        let (api_key, language, sample_rate) = match cfg {
            EngineConfig::Cloud { api_key, language, sample_rate } =>
                (api_key, language, sample_rate),
            EngineConfig::Local { .. } => {
                anyhow::bail!("AssemblyAiEngine requires EngineConfig::Cloud");
            }
        };

        if api_key.is_empty() {
            anyhow::bail!("AssemblyAiEngine: api_key is empty");
        }
        if sample_rate != 16_000 {
            anyhow::bail!(
                "AssemblyAiEngine requires 16 kHz PCM; got {sample_rate}",
            );
        }

        // Connection / pumping wired up in Tasks 7–10.
        let (pcm_tx, _pcm_rx) = mpsc::channel::<Vec<f32>>(32);
        let (_events_tx, events_rx) = broadcast::channel::<EngineEvent>(256);
        let (shutdown_tx, _shutdown_rx) = oneshot::channel::<()>();

        let _ = (&self.api_key, api_key, language); // avoid unused warnings until wired up
        anyhow::bail!("AssemblyAiEngine: not implemented yet (see Task 7)");

        #[allow(unreachable_code)]
        Ok(EngineHandle { pcm_tx, events_rx, shutdown: shutdown_tx })
    }
}
```

- [ ] **Step 2: Register the module**

In `src/transcription/mod.rs`, add to the `pub mod ...;` block at the top:

```rust
pub mod assemblyai_engine;
```

- [ ] **Step 3: Add a test that the engine rejects bad config**

Create `tests/assemblyai_engine.rs`:

```rust
use voice_bird::transcription::{
    assemblyai_engine::AssemblyAiEngine, EngineConfig, TranscriptionEngine,
};

#[test]
fn rejects_local_variant() {
    let mut e = AssemblyAiEngine::new("sk-x".into());
    let err = e
        .start(EngineConfig::Local {
            model_path: "/dev/null".into(),
            language: None,
            sample_rate: 16_000,
            hop_ms: 750,
            min_window_ms: 1000,
        })
        .err()
        .expect("expected Err on Local variant");
    assert!(
        err.to_string().to_lowercase().contains("cloud"),
        "got: {err}",
    );
}

#[test]
fn rejects_empty_api_key() {
    let mut e = AssemblyAiEngine::new(String::new());
    let err = e
        .start(EngineConfig::Cloud {
            api_key: String::new(),
            language: None,
            sample_rate: 16_000,
        })
        .err()
        .expect("expected Err on empty key");
    assert!(err.to_string().contains("api_key"));
}

#[test]
fn rejects_non_16khz_sample_rate() {
    let mut e = AssemblyAiEngine::new("sk-x".into());
    let err = e
        .start(EngineConfig::Cloud {
            api_key: "sk-x".into(),
            language: None,
            sample_rate: 44_100,
        })
        .err()
        .expect("expected Err on wrong sample rate");
    assert!(err.to_string().contains("16 kHz"));
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test --test assemblyai_engine rejects_`
Expected: three rejection tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/transcription/assemblyai_engine.rs src/transcription/mod.rs tests/assemblyai_engine.rs
git commit -m "engine: scaffold AssemblyAiEngine with config validation"
```

---

## Task 7: AssemblyAI engine — WebSocket connection + Begin event

**Files:**
- Modify: `src/transcription/assemblyai_engine.rs`
- Modify: `tests/assemblyai_engine.rs`

We now replace the `bail!("not implemented yet")` placeholder with a real WebSocket pump. The tests use a local mock server so no external network or real API key is required.

**Reference:** AssemblyAI Universal-Streaming v3 docs: `https://www.assemblyai.com/docs/speech-to-text/universal-streaming`. Confirm URL, message schemas, and auth headers against the live docs during implementation — the shapes below reflect the v3 protocol at plan-writing time.

- [ ] **Step 1: Write a failing integration test with a mock WebSocket server**

Add to `tests/assemblyai_engine.rs`:

```rust
use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::{accept_async, tungstenite::Message};

use voice_bird::transcription::{
    assemblyai_engine::AssemblyAiEngine, EngineConfig, EngineEvent, TranscriptionEngine,
};

/// Spawn a local ws:// server that accepts one client, immediately sends
/// the provided JSON messages, then closes. Returns the bound address.
async fn spawn_mock_server(messages: Vec<String>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();
        for m in messages {
            ws.send(Message::Text(m)).await.unwrap();
        }
        let _ = ws.close(None).await;
    });
    addr
}

#[tokio::test]
async fn emits_model_loaded_on_begin() {
    let addr = spawn_mock_server(vec![
        r#"{"type":"Begin","session_id":"s1","expires_at":0}"#.into(),
    ])
    .await;
    std::env::set_var(
        "ASSEMBLYAI_WS_URL_OVERRIDE",
        format!("ws://{}/v3/ws", addr),
    );

    let mut e = AssemblyAiEngine::new("sk-x".into());
    let handle = e
        .start(EngineConfig::Cloud {
            api_key: "sk-x".into(),
            language: None,
            sample_rate: 16_000,
        })
        .unwrap();

    let mut rx = handle.events_rx;
    let ev = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    match ev {
        EngineEvent::ModelLoaded { name } => {
            assert!(name.contains("assemblyai"), "got: {name}");
        }
        other => panic!("expected ModelLoaded, got {other:?}"),
    }
}
```

Add `futures-util` to `[dev-dependencies]` in `Cargo.toml`:

```toml
futures-util = "0.3"
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test assemblyai_engine emits_model_loaded_on_begin`
Expected: FAIL (engine still `bail!`s).

- [ ] **Step 3: Implement WebSocket connection + Begin parsing**

Rewrite `src/transcription/assemblyai_engine.rs`:

```rust
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{
        client::IntoClientRequest,
        handshake::client::Request,
        http::HeaderValue,
        Message,
    },
};

use super::{EngineConfig, EngineEvent, EngineHandle, TranscriptionEngine};

pub struct AssemblyAiEngine {
    api_key: String,
}

impl AssemblyAiEngine {
    pub fn new(api_key: String) -> Self {
        Self { api_key }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum AaiMessage {
    Begin {
        #[allow(dead_code)]
        session_id: String,
    },
    #[serde(other)]
    Unknown,
}

fn ws_url(sample_rate: u32, language: &Option<String>) -> String {
    if let Ok(override_url) = std::env::var("ASSEMBLYAI_WS_URL_OVERRIDE") {
        return format!(
            "{}?sample_rate={}&format_turns=true{}",
            override_url,
            sample_rate,
            language.as_deref().map(|l| format!("&language_code={l}")).unwrap_or_default(),
        );
    }
    format!(
        "wss://streaming.assemblyai.com/v3/ws?sample_rate={}&format_turns=true{}",
        sample_rate,
        language.as_deref().map(|l| format!("&language_code={l}")).unwrap_or_default(),
    )
}

impl TranscriptionEngine for AssemblyAiEngine {
    fn start(&mut self, cfg: EngineConfig) -> anyhow::Result<EngineHandle> {
        let (api_key, language, sample_rate) = match cfg {
            EngineConfig::Cloud { api_key, language, sample_rate } =>
                (api_key, language, sample_rate),
            EngineConfig::Local { .. } => {
                anyhow::bail!("AssemblyAiEngine requires EngineConfig::Cloud");
            }
        };
        if api_key.is_empty() {
            anyhow::bail!("AssemblyAiEngine: api_key is empty");
        }
        if sample_rate != 16_000 {
            anyhow::bail!("AssemblyAiEngine requires 16 kHz PCM; got {sample_rate}");
        }

        let (pcm_tx, mut pcm_rx) = mpsc::channel::<Vec<f32>>(32);
        let (events_tx, events_rx) = broadcast::channel::<EngineEvent>(256);
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();

        let url = ws_url(sample_rate, &language);

        tokio::spawn(async move {
            // Build request with Authorization header.
            let mut req: Request = match url.clone().into_client_request() {
                Ok(r) => r,
                Err(e) => {
                    let _ = events_tx.send(EngineEvent::Error(format!("bad url: {e}")));
                    return;
                }
            };
            let auth = match HeaderValue::from_str(&api_key) {
                Ok(h) => h,
                Err(_) => {
                    let _ = events_tx.send(EngineEvent::Error("invalid api key header".into()));
                    return;
                }
            };
            req.headers_mut().insert("Authorization", auth);

            let (mut ws, _resp) = match connect_async(req).await {
                Ok(pair) => pair,
                Err(e) => {
                    let _ = events_tx.send(EngineEvent::Error(format!("connect: {e}")));
                    return;
                }
            };

            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    _pcm = pcm_rx.recv() => {
                        // PCM forwarding is wired in Task 8.
                    }
                    maybe_msg = ws.next() => {
                        let Some(msg) = maybe_msg else { break; };
                        let msg = match msg {
                            Ok(m) => m,
                            Err(e) => {
                                let _ = events_tx.send(EngineEvent::Error(
                                    format!("ws recv: {e}"),
                                ));
                                break;
                            }
                        };
                        match msg {
                            Message::Text(txt) => {
                                match serde_json::from_str::<AaiMessage>(&txt) {
                                    Ok(AaiMessage::Begin { .. }) => {
                                        let _ = events_tx.send(EngineEvent::ModelLoaded {
                                            name: "assemblyai-universal-v3".into(),
                                        });
                                    }
                                    Ok(AaiMessage::Unknown) => {}
                                    Err(_) => {}
                                }
                            }
                            Message::Close(_) => break,
                            _ => {}
                        }
                    }
                }
            }

            let _ = ws.close(None).await;
        });

        Ok(EngineHandle { pcm_tx, events_rx, shutdown: shutdown_tx })
    }
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test --test assemblyai_engine`
Expected: all four tests pass (`rejects_local_variant`, `rejects_empty_api_key`, `rejects_non_16khz_sample_rate`, `emits_model_loaded_on_begin`).

- [ ] **Step 5: Commit**

```bash
git add src/transcription/assemblyai_engine.rs tests/assemblyai_engine.rs Cargo.toml Cargo.lock
git commit -m "engine(assemblyai): connect WebSocket, emit ModelLoaded on Begin"
```

---

## Task 8: AssemblyAI engine — forward PCM as i16 binary frames

**Files:**
- Modify: `src/transcription/assemblyai_engine.rs`
- Modify: `tests/assemblyai_engine.rs`

- [ ] **Step 1: Write the failing test**

Add to `tests/assemblyai_engine.rs`:

```rust
use std::sync::Arc;
use tokio::sync::Mutex;

/// Run a mock server that echoes a Begin then records all binary frames
/// it receives. Returns the frames after client disconnects.
async fn record_binary_frames() -> (SocketAddr, Arc<Mutex<Vec<Vec<u8>>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let frames = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let frames_clone = frames.clone();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();
        ws.send(Message::Text(
            r#"{"type":"Begin","session_id":"s1","expires_at":0}"#.into(),
        ))
        .await
        .unwrap();
        while let Some(Ok(m)) = ws.next().await {
            if let Message::Binary(b) = m {
                frames_clone.lock().await.push(b);
            }
        }
    });
    (addr, frames)
}

#[tokio::test]
async fn forwards_pcm_as_i16_binary_frames() {
    let (addr, frames) = record_binary_frames().await;
    std::env::set_var(
        "ASSEMBLYAI_WS_URL_OVERRIDE",
        format!("ws://{}/v3/ws", addr),
    );

    let mut e = AssemblyAiEngine::new("sk-x".into());
    let handle = e
        .start(EngineConfig::Cloud {
            api_key: "sk-x".into(),
            language: None,
            sample_rate: 16_000,
        })
        .unwrap();

    // Wait for ModelLoaded so we know the WS is up.
    let mut rx = handle.events_rx;
    let _ = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await;

    // Send 1 second of silence as f32, expect ~20 frames of 800 samples.
    let chunk = vec![0.0_f32; 16_000];
    handle.pcm_tx.send(chunk).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Drop pcm_tx to let the engine's select exit on next iteration.
    drop(handle.pcm_tx);
    let _ = handle.shutdown.send(());
    tokio::time::sleep(Duration::from_millis(200)).await;

    let frames = frames.lock().await;
    assert!(!frames.is_empty(), "no frames received");
    let total_bytes: usize = frames.iter().map(|f| f.len()).sum();
    // 16000 samples * 2 bytes = 32000 bytes of i16 PCM.
    assert_eq!(total_bytes, 32_000, "unexpected total bytes: {total_bytes}");
    for f in frames.iter() {
        // Each frame is ~50 ms = 800 samples = 1600 bytes. Final partial
        // frame may be smaller. All frames must have an even byte count.
        assert_eq!(f.len() % 2, 0, "frame length not a multiple of 2");
        assert!(f.len() <= 1600, "frame too large: {}", f.len());
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test assemblyai_engine forwards_pcm_as_i16_binary_frames`
Expected: FAIL (no binary frames sent).

- [ ] **Step 3: Implement PCM forwarding**

In `src/transcription/assemblyai_engine.rs`, inside the `tokio::spawn` loop, replace the `_pcm = pcm_rx.recv()` branch with:

```rust
                    maybe_pcm = pcm_rx.recv() => {
                        let Some(chunk) = maybe_pcm else { break; };
                        // ~50 ms frames at 16 kHz mono = 800 samples = 1600 bytes.
                        const FRAME_SAMPLES: usize = 800;
                        for win in chunk.chunks(FRAME_SAMPLES) {
                            let mut bytes = Vec::with_capacity(win.len() * 2);
                            for &s in win {
                                let clamped = s.clamp(-1.0, 1.0);
                                let i = (clamped * i16::MAX as f32) as i16;
                                bytes.extend_from_slice(&i.to_le_bytes());
                            }
                            if let Err(e) = ws.send(Message::Binary(bytes)).await {
                                let _ = events_tx.send(EngineEvent::Error(
                                    format!("ws send: {e}"),
                                ));
                                break;
                            }
                        }
                    }
```

Also, at the end of the spawn (after the loop exits), send a `Terminate` text message before closing, so the server gets a clean shutdown:

```rust
            let _ = ws
                .send(Message::Text(r#"{"type":"Terminate"}"#.into()))
                .await;
            let _ = ws.close(None).await;
```

- [ ] **Step 4: Run tests**

Run: `cargo test --test assemblyai_engine`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add src/transcription/assemblyai_engine.rs tests/assemblyai_engine.rs
git commit -m "engine(assemblyai): forward PCM as 50 ms i16 LE binary frames"
```

---

## Task 9: AssemblyAI engine — map Turn events to Tentative/Committed

**Files:**
- Modify: `src/transcription/assemblyai_engine.rs`
- Modify: `tests/assemblyai_engine.rs`

- [ ] **Step 1: Write the failing test**

Add to `tests/assemblyai_engine.rs`:

```rust
#[tokio::test]
async fn turn_partial_maps_to_tentative_and_final_maps_to_committed() {
    let addr = spawn_mock_server(vec![
        r#"{"type":"Begin","session_id":"s1","expires_at":0}"#.into(),
        r#"{"type":"Turn","transcript":"hel","end_of_turn":false,"turn_is_formatted":false,"audio_start_ms":0,"audio_end_ms":500}"#.into(),
        r#"{"type":"Turn","transcript":"hello world","end_of_turn":true,"turn_is_formatted":true,"audio_start_ms":0,"audio_end_ms":1200}"#.into(),
    ])
    .await;
    std::env::set_var(
        "ASSEMBLYAI_WS_URL_OVERRIDE",
        format!("ws://{}/v3/ws", addr),
    );

    let mut e = AssemblyAiEngine::new("sk-x".into());
    let handle = e
        .start(EngineConfig::Cloud {
            api_key: "sk-x".into(),
            language: None,
            sample_rate: 16_000,
        })
        .unwrap();
    let mut rx = handle.events_rx;

    // Drain in order with a timeout per event.
    let mut saw_model = false;
    let mut saw_tentative = false;
    let mut saw_committed = false;
    for _ in 0..6 {
        let Ok(Ok(ev)) =
            tokio::time::timeout(Duration::from_secs(2), rx.recv()).await
        else { break };
        match ev {
            EngineEvent::ModelLoaded { .. } => saw_model = true,
            EngineEvent::Tentative(t) if t == "hel" => saw_tentative = true,
            EngineEvent::Committed(seg) if seg.text == "hello world" => {
                saw_committed = true;
                assert_eq!(seg.t_start.as_millis(), 0);
                assert_eq!(seg.t_end.as_millis(), 1200);
            }
            _ => {}
        }
    }
    assert!(saw_model && saw_tentative && saw_committed,
        "events: model={saw_model} tentative={saw_tentative} committed={saw_committed}");
}

#[tokio::test]
async fn error_message_maps_to_engine_error() {
    let addr = spawn_mock_server(vec![
        r#"{"type":"Error","error":"auth failed"}"#.into(),
    ])
    .await;
    std::env::set_var(
        "ASSEMBLYAI_WS_URL_OVERRIDE",
        format!("ws://{}/v3/ws", addr),
    );

    let mut e = AssemblyAiEngine::new("sk-x".into());
    let handle = e
        .start(EngineConfig::Cloud {
            api_key: "sk-x".into(),
            language: None,
            sample_rate: 16_000,
        })
        .unwrap();
    let mut rx = handle.events_rx;
    let ev = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    match ev {
        EngineEvent::Error(msg) => assert!(msg.contains("auth failed"), "got: {msg}"),
        other => panic!("expected Error, got {other:?}"),
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test assemblyai_engine turn_partial_maps|error_message_maps`
Expected: FAIL.

- [ ] **Step 3: Implement Turn/Error parsing**

In `src/transcription/assemblyai_engine.rs`, extend the `AaiMessage` enum:

```rust
use std::time::Duration;

use super::{EngineConfig, EngineEvent, EngineHandle, Segment, TranscriptionEngine};

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum AaiMessage {
    Begin {
        #[allow(dead_code)]
        session_id: String,
    },
    Turn {
        transcript: String,
        end_of_turn: bool,
        #[serde(default)]
        audio_start_ms: u64,
        #[serde(default)]
        audio_end_ms: u64,
    },
    Termination {},
    Error {
        error: String,
    },
    #[serde(other)]
    Unknown,
}
```

Then, in the `Message::Text(txt)` branch, replace the current match:

```rust
                        match serde_json::from_str::<AaiMessage>(&txt) {
                            Ok(AaiMessage::Begin { .. }) => {
                                let _ = events_tx.send(EngineEvent::ModelLoaded {
                                    name: "assemblyai-universal-v3".into(),
                                });
                            }
                            Ok(AaiMessage::Turn {
                                transcript,
                                end_of_turn,
                                audio_start_ms,
                                audio_end_ms,
                            }) => {
                                if end_of_turn {
                                    let seg = Segment {
                                        t_start: Duration::from_millis(audio_start_ms),
                                        t_end: Duration::from_millis(audio_end_ms),
                                        text: transcript,
                                        tokens: Vec::new(),
                                    };
                                    let _ = events_tx.send(EngineEvent::Committed(seg));
                                } else {
                                    let _ = events_tx.send(
                                        EngineEvent::Tentative(transcript),
                                    );
                                }
                            }
                            Ok(AaiMessage::Termination {}) => break,
                            Ok(AaiMessage::Error { error }) => {
                                let _ = events_tx.send(EngineEvent::Error(error));
                                break;
                            }
                            Ok(AaiMessage::Unknown) => {}
                            Err(_) => {}
                        }
```

- [ ] **Step 4: Run tests**

Run: `cargo test --test assemblyai_engine`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add src/transcription/assemblyai_engine.rs tests/assemblyai_engine.rs
git commit -m "engine(assemblyai): map Turn partial→Tentative, final→Committed, Error→Error"
```

---

## Task 10: AssemblyAI engine — shutdown & Terminate

**Files:**
- Modify: `tests/assemblyai_engine.rs`

`Terminate` on shutdown and Close handling were already added in Task 8's final edit. This task adds test coverage.

- [ ] **Step 1: Write the failing test (record that Terminate is sent)**

Add to `tests/assemblyai_engine.rs`:

```rust
#[tokio::test]
async fn shutdown_sends_terminate_text_message() {
    // Mock server: send Begin, then record every text message from client.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let texts = Arc::new(Mutex::new(Vec::<String>::new()));
    let texts_clone = texts.clone();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();
        ws.send(Message::Text(
            r#"{"type":"Begin","session_id":"s","expires_at":0}"#.into(),
        ))
        .await
        .unwrap();
        while let Some(Ok(m)) = ws.next().await {
            if let Message::Text(t) = m {
                texts_clone.lock().await.push(t);
            }
        }
    });
    std::env::set_var(
        "ASSEMBLYAI_WS_URL_OVERRIDE",
        format!("ws://{}/v3/ws", addr),
    );

    let mut e = AssemblyAiEngine::new("sk-x".into());
    let handle = e
        .start(EngineConfig::Cloud {
            api_key: "sk-x".into(),
            language: None,
            sample_rate: 16_000,
        })
        .unwrap();
    let mut rx = handle.events_rx;
    let _ = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await;

    let _ = handle.shutdown.send(());
    tokio::time::sleep(Duration::from_millis(300)).await;

    let texts = texts.lock().await;
    assert!(
        texts.iter().any(|t| t.contains("Terminate")),
        "Terminate not sent; got: {:?}",
        *texts
    );
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test --test assemblyai_engine shutdown_sends_terminate`
Expected: passes (Task 8's Terminate-on-exit code covers this).

- [ ] **Step 3: Commit**

```bash
git add tests/assemblyai_engine.rs
git commit -m "test(assemblyai): cover Terminate-on-shutdown contract"
```

---

## Task 11: Wire `select_engine` for `"assemblyai"` preference

**Files:**
- Modify: `src/transcription/mod.rs`
- Modify: `src/app.rs`

`select_engine`'s current signature returns `Box<dyn TranscriptionEngine>`. To surface the "cloud preferred but key missing" condition cleanly, we return `Result<(EngineKind, Box<dyn TranscriptionEngine>), String>` via a new helper, keeping the old function as a thin wrapper.

- [ ] **Step 1: Write the failing test**

Add to `src/transcription/mod.rs` in a new `#[cfg(test)] mod select_tests` block at the bottom:

```rust
#[cfg(test)]
mod select_tests {
    use super::*;

    #[test]
    fn assemblyai_with_key_returns_cloud_engine() {
        let res = try_select_engine("assemblyai", "sk-fake", None);
        let (kind, _engine) = res.expect("expected Ok");
        assert_eq!(kind, EngineKind::AssemblyAi);
    }

    #[test]
    fn assemblyai_without_key_returns_err() {
        let err = try_select_engine("assemblyai", "", None).err().unwrap();
        assert!(err.to_lowercase().contains("api key"));
    }

    #[test]
    fn non_cloud_preference_ignores_key() {
        let (kind, _) = try_select_engine("whisper_rs", "", None).unwrap();
        assert_eq!(kind, EngineKind::WhisperRs);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib transcription::select_tests`
Expected: FAIL (function does not exist yet).

- [ ] **Step 3: Implement**

In `src/transcription/mod.rs`, add below the existing `select_engine` function:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineKind {
    WhisperRs,
    WhisperKit,
    AssemblyAi,
}

/// Typed variant of `select_engine`. Returns an error for cases that
/// should surface to the user (e.g. cloud engine selected but no key).
pub fn try_select_engine(
    prefer: &str,
    assemblyai_api_key: &str,
    sidecar_path: Option<&std::path::Path>,
) -> Result<(EngineKind, Box<dyn TranscriptionEngine>), String> {
    if prefer == "assemblyai" {
        if assemblyai_api_key.is_empty() {
            return Err(
                "AssemblyAI selected but no API key — open settings (press ',')".into(),
            );
        }
        return Ok((
            EngineKind::AssemblyAi,
            Box::new(assemblyai_engine::AssemblyAiEngine::new(
                assemblyai_api_key.to_string(),
            )),
        ));
    }

    #[cfg(target_os = "macos")]
    {
        if prefer == "whisperkit" || prefer == "auto" {
            if let Some(path) = sidecar_path {
                if path.exists() {
                    return Ok((
                        EngineKind::WhisperKit,
                        Box::new(whisper_kit_engine::WhisperKitEngine::new(
                            path.to_path_buf(),
                        )),
                    ));
                }
            }
        }
    }
    let _ = sidecar_path;
    Ok((
        EngineKind::WhisperRs,
        Box::<whisper_rs_engine::WhisperRsEngine>::default(),
    ))
}
```

- [ ] **Step 4: Update `app.rs` to use the typed helper**

In `src/app.rs`, replace the `select_engine` call + the string-based `engine_kind_used` match with:

```rust
        let sidecar = voice_bird::transcription::sidecar_path();
        let (engine_kind_used, mut engine) = match voice_bird::transcription::try_select_engine(
            &self.config.engine_prefer,
            &self.config.assemblyai_api_key,
            sidecar.as_deref(),
        ) {
            Ok(pair) => pair,
            Err(msg) => {
                self.banner = Some(msg);
                self.status = RecordingStatus::Error("engine selection failed".into());
                return;
            }
        };
        self.engine_kind = match engine_kind_used {
            voice_bird::transcription::EngineKind::WhisperRs => "whisper_rs".into(),
            voice_bird::transcription::EngineKind::WhisperKit => "whisperkit".into(),
            voice_bird::transcription::EngineKind::AssemblyAi => "assemblyai".into(),
        };
        self.is_cloud_engine = matches!(
            engine_kind_used,
            voice_bird::transcription::EngineKind::AssemblyAi,
        );
        self.banner = None;
```

(the `is_cloud_engine` field is added in Task 12.)

- [ ] **Step 5: Update the engine.start call to dispatch on kind**

Right below the block above, replace the existing `engine.start(EngineConfig::Local { ... })` with:

```rust
        let engine_cfg = if self.is_cloud_engine {
            EngineConfig::Cloud {
                api_key: self.config.assemblyai_api_key.clone(),
                language: Some(self.config.language.clone()).filter(|s| s != "auto"),
                sample_rate: 16_000,
            }
        } else {
            EngineConfig::Local {
                model_path,
                language: Some(self.config.language.clone()).filter(|s| s != "auto"),
                sample_rate: 16_000,
                hop_ms: self.config.hop_ms,
                min_window_ms: self.config.min_window_ms,
            }
        };
        let handle = match engine.start(engine_cfg) {
            Ok(h) => h,
            Err(e) => {
                self.status = RecordingStatus::Error(format!("engine: {e}"));
                return;
            }
        };
```

Note: `model_path` is only used in the `Local` branch. The `model_path = gguf_path(...)` statement earlier in `start_recording` will fail for cloud users who haven't downloaded a model. Gate it:

```rust
        let model_path = if matches!(
            voice_bird::transcription::try_select_engine(
                &self.config.engine_prefer,
                &self.config.assemblyai_api_key,
                sidecar.as_deref(),
            ),
            Ok((voice_bird::transcription::EngineKind::AssemblyAi, _))
        ) {
            std::path::PathBuf::new() // unused for cloud
        } else {
            match voice_bird::transcription::models::gguf_path(&self.config.default_model) {
                Ok(p) => p,
                Err(e) => {
                    self.status = RecordingStatus::Error(format!("model path: {e}"));
                    return;
                }
            }
        };
```

- [ ] **Step 6: Run all tests**

Run: `cargo test`
Expected: all pass. (The `is_cloud_engine` field is defined in Task 12; if compilation fails here, skip ahead to Task 12 first, then come back. In practice we implement Tasks 11 and 12 together in one commit.)

- [ ] **Step 7: Commit**

```bash
git add src/transcription/mod.rs src/app.rs
git commit -m "engine: try_select_engine + wire assemblyai preference in start_recording"
```

---

## Task 12: App state — `AppMode::Settings` + `is_cloud_engine` flag

**Files:**
- Modify: `src/app.rs`

- [ ] **Step 1: Add the variant and field**

In `src/app.rs`, update the `AppMode` enum:

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum AppMode {
    Normal,
    ModelPicker,
    Help,
    Settings,
}
```

Add to the `App` struct (after `engine_kind`):

```rust
    /// True while a cloud engine is actively transmitting audio. Drives
    /// the CLOUD badge in the header. Set in `start_recording`, cleared
    /// in `stop_recording`.
    pub is_cloud_engine: bool,

    /// Snapshot of config taken on entering Settings mode. Used to
    /// discard edits on Cancel.
    pub settings_snapshot: Option<crate::config::AppConfig>,

    /// Cursor index over the ordered list of editable settings fields.
    pub settings_cursor: usize,

    /// In-flight text buffer while editing a single settings field.
    /// `None` = not currently editing a field.
    pub settings_edit_buf: Option<String>,

    /// Error line displayed at the bottom of the settings view.
    pub settings_error: Option<String>,

    /// When recording started with a cloud engine, the wall-clock time
    /// at which the "Audio is being sent to AssemblyAI." reminder
    /// should be hidden (3 s after recording start).
    pub cloud_reminder_until: Option<std::time::Instant>,
```

Note: `crate::config::AppConfig` — this file uses `use voice_bird::config::AppConfig` at the top, so reference it as `AppConfig` (not via `crate::`).

Fix the field type:

```rust
    pub settings_snapshot: Option<AppConfig>,
```

Update `App::new()` to initialize the new fields (after `banner: None,`):

```rust
            is_cloud_engine: false,
            settings_snapshot: None,
            settings_cursor: 0,
            settings_edit_buf: None,
            settings_error: None,
            cloud_reminder_until: None,
```

- [ ] **Step 2: Clear `is_cloud_engine` in `stop_recording`**

In `src/app.rs`, find `fn stop_recording` and add near the top of the function body:

```rust
        self.is_cloud_engine = false;
        self.cloud_reminder_until = None;
```

- [ ] **Step 3: Set `cloud_reminder_until` in `start_recording`**

In `start_recording`, right after the `self.is_cloud_engine = matches!(...)` line added in Task 11, add:

```rust
        if self.is_cloud_engine {
            self.cloud_reminder_until = Some(
                std::time::Instant::now() + std::time::Duration::from_secs(3),
            );
        } else {
            self.cloud_reminder_until = None;
        }
```

- [ ] **Step 4: Build**

Run: `cargo build`
Expected: compiles cleanly.

- [ ] **Step 5: Commit**

```bash
git add src/app.rs
git commit -m "app: AppMode::Settings + is_cloud_engine + cloud reminder state"
```

---

## Task 13: Settings view — ordered field list + render skeleton

**Files:**
- Create: `src/settings_view.rs`
- Modify: `src/main.rs` (to declare the new module)
- Modify: `src/ui.rs` (dispatch rendering)

- [ ] **Step 1: Create `src/settings_view.rs`**

```rust
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use crate::app::App;

/// Editable settings fields, in render order. The settings view
/// iterates this slice; section headers are rendered between runs of
/// adjacent fields sharing the same `section` string.
#[derive(Debug, Clone, Copy)]
pub struct Field {
    pub section: &'static str,
    pub key: FieldKey,
    pub label: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKey {
    DefaultModel,
    Language,
    SessionDir,
    AudioDefaultSource,
    InputDevice,
    EnginePrefer,
    HopMs,
    MinWindowMs,
    RefinementModel,
    RefinementWindowMs,
    RefinementBeamSize,
    AssemblyAiApiKey,
}

pub const FIELDS: &[Field] = &[
    Field { section: "General", key: FieldKey::DefaultModel, label: "Default model" },
    Field { section: "General", key: FieldKey::Language, label: "Language" },
    Field { section: "General", key: FieldKey::SessionDir, label: "Session directory" },
    Field { section: "Audio", key: FieldKey::AudioDefaultSource, label: "Default source" },
    Field { section: "Audio", key: FieldKey::InputDevice, label: "Input device" },
    Field { section: "Engine", key: FieldKey::EnginePrefer, label: "Engine preference" },
    Field { section: "Engine", key: FieldKey::HopMs, label: "Hop (ms)" },
    Field { section: "Engine", key: FieldKey::MinWindowMs, label: "Min window (ms)" },
    Field { section: "Refinement (whisper only)", key: FieldKey::RefinementModel, label: "Refinement model" },
    Field { section: "Refinement (whisper only)", key: FieldKey::RefinementWindowMs, label: "Window (ms)" },
    Field { section: "Refinement (whisper only)", key: FieldKey::RefinementBeamSize, label: "Beam size" },
    Field { section: "Cloud", key: FieldKey::AssemblyAiApiKey, label: "AssemblyAI API key" },
];

fn mask_api_key(key: &str) -> String {
    if key.is_empty() {
        "(unset)".into()
    } else if key.len() <= 4 {
        "•".repeat(key.len())
    } else {
        let shown = &key[key.len() - 4..];
        let hidden = "•".repeat(key.len() - 4);
        format!("{hidden}{shown}")
    }
}

pub fn field_display(app: &App, key: FieldKey) -> String {
    let c = &app.config;
    match key {
        FieldKey::DefaultModel => c.default_model.clone(),
        FieldKey::Language => c.language.clone(),
        FieldKey::SessionDir => c.session_dir.clone(),
        FieldKey::AudioDefaultSource => c.audio_default_source.clone(),
        FieldKey::InputDevice => c
            .input_device
            .clone()
            .unwrap_or_else(|| "(OS default)".into()),
        FieldKey::EnginePrefer => c.engine_prefer.clone(),
        FieldKey::HopMs => c.hop_ms.to_string(),
        FieldKey::MinWindowMs => c.min_window_ms.to_string(),
        FieldKey::RefinementModel => c
            .refinement_model
            .clone()
            .unwrap_or_else(|| "(off)".into()),
        FieldKey::RefinementWindowMs => c.refinement_window_ms.to_string(),
        FieldKey::RefinementBeamSize => c.refinement_beam_size.to_string(),
        FieldKey::AssemblyAiApiKey => format!("{} [plaintext]", mask_api_key(&c.assemblyai_api_key)),
    }
}

pub fn render(f: &mut Frame, area: Rect, app: &App) {
    let outer = Block::default()
        .borders(Borders::ALL)
        .title(" Settings ");
    let inner = outer.inner(area);
    f.render_widget(outer, area);

    let mut lines: Vec<Line> = Vec::new();
    let mut prev_section: Option<&'static str> = None;
    let mut field_idx = 0usize;

    for fld in FIELDS {
        if prev_section != Some(fld.section) {
            if prev_section.is_some() {
                lines.push(Line::from(""));
            }
            lines.push(Line::from(Span::styled(
                format!("▸ {}", fld.section),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            )));
            prev_section = Some(fld.section);
        }

        let marker = if field_idx == app.settings_cursor { "▶ " } else { "  " };
        let value_text = if field_idx == app.settings_cursor && app.settings_edit_buf.is_some() {
            app.settings_edit_buf.clone().unwrap_or_default()
        } else {
            field_display(app, fld.key)
        };

        let value_style = if field_idx == app.settings_cursor && app.settings_edit_buf.is_some() {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default()
        };

        lines.push(Line::from(vec![
            Span::raw(format!("    {marker}")),
            Span::raw(format!("{:<22}", format!("{}:", fld.label))),
            Span::styled(value_text, value_style),
        ]));

        field_idx += 1;
    }

    lines.push(Line::from(""));
    let hint = if app.settings_edit_buf.is_some() {
        "Enter: save field   Esc: cancel edit"
    } else {
        "↑↓: move   Enter: edit   s: save   Esc: close"
    };
    lines.push(Line::from(Span::styled(
        hint,
        Style::default().fg(Color::DarkGray),
    )));
    if let Some(err) = &app.settings_error {
        lines.push(Line::from(Span::styled(
            err.clone(),
            Style::default().fg(Color::Red),
        )));
    }

    let p = Paragraph::new(lines);
    f.render_widget(p, inner);
}

// Key handling lives in Task 14.
```

- [ ] **Step 2: Register the module**

In `src/main.rs`, add below the existing `mod` lines:

```rust
mod settings_view;
```

- [ ] **Step 3: Dispatch settings rendering from ui.rs**

In `src/ui.rs`, update `pub fn render`:

```rust
pub fn render(f: &mut Frame, app: &App) {
    if app.mode == AppMode::ModelPicker {
        render_model_picker(f, f.area(), app);
        return;
    }
    if app.mode == AppMode::Settings {
        crate::settings_view::render(f, f.area(), app);
        return;
    }
    // …existing body unchanged…
}
```

- [ ] **Step 4: Build**

Run: `cargo build`
Expected: compiles (settings view renders statically; no keys wired yet).

- [ ] **Step 5: Commit**

```bash
git add src/settings_view.rs src/main.rs src/ui.rs
git commit -m "ui(settings): scaffold full-screen settings view and field list"
```

---

## Task 14: Settings view — open/close, navigation, editing

**Files:**
- Modify: `src/settings_view.rs`
- Modify: `src/main.rs`
- Modify: `src/app.rs`

- [ ] **Step 1: Add key handler + open/close methods on App**

Append to `src/settings_view.rs`:

```rust
use crossterm::event::KeyCode;

use voice_bird::config::AppConfig;

pub fn open(app: &mut App) {
    app.mode = crate::app::AppMode::Settings;
    app.settings_snapshot = Some(app.config.clone());
    app.settings_cursor = 0;
    app.settings_edit_buf = None;
    app.settings_error = None;
}

fn close(app: &mut App) {
    app.mode = crate::app::AppMode::Normal;
    app.settings_snapshot = None;
    app.settings_edit_buf = None;
    app.settings_error = None;
}

/// Revert config to the snapshot taken at open() time, then close.
fn cancel(app: &mut App) {
    if let Some(snap) = app.settings_snapshot.take() {
        app.config = snap;
    }
    close(app);
}

fn current_field(app: &App) -> Option<&'static Field> {
    FIELDS.get(app.settings_cursor)
}

fn apply_edit(app: &mut App) {
    let Some(buf) = app.settings_edit_buf.take() else { return };
    let Some(fld) = current_field(app).copied() else { return };
    let c = &mut app.config;
    match fld.key {
        FieldKey::DefaultModel => c.default_model = buf,
        FieldKey::Language => c.language = buf,
        FieldKey::SessionDir => c.session_dir = buf,
        FieldKey::AudioDefaultSource => c.audio_default_source = buf,
        FieldKey::InputDevice => {
            c.input_device = if buf.is_empty() { None } else { Some(buf) };
        }
        FieldKey::EnginePrefer => c.engine_prefer = buf,
        FieldKey::HopMs => match buf.parse::<u32>() {
            Ok(n) => c.hop_ms = n,
            Err(_) => {
                app.settings_error = Some(format!("hop_ms: not a number ({buf})"));
                return;
            }
        },
        FieldKey::MinWindowMs => match buf.parse::<u32>() {
            Ok(n) => c.min_window_ms = n,
            Err(_) => {
                app.settings_error = Some(format!("min_window_ms: not a number ({buf})"));
                return;
            }
        },
        FieldKey::RefinementModel => {
            c.refinement_model = if buf.is_empty() { None } else { Some(buf) };
        }
        FieldKey::RefinementWindowMs => match buf.parse::<u32>() {
            Ok(n) => c.refinement_window_ms = n,
            Err(_) => {
                app.settings_error = Some(format!("refinement_window_ms: not a number ({buf})"));
                return;
            }
        },
        FieldKey::RefinementBeamSize => match buf.parse::<u8>() {
            Ok(n) => c.refinement_beam_size = n,
            Err(_) => {
                app.settings_error = Some(format!("refinement_beam_size: not a number ({buf})"));
                return;
            }
        },
        FieldKey::AssemblyAiApiKey => c.assemblyai_api_key = buf,
    }
    app.settings_error = None;
}

fn try_save(app: &mut App) -> bool {
    if app.config.engine_prefer == "assemblyai"
        && app.config.assemblyai_api_key.is_empty()
    {
        app.settings_error =
            Some("engine_prefer=assemblyai requires a non-empty AssemblyAI API key".into());
        return false;
    }
    if let Err(e) = app.config.save() {
        app.settings_error = Some(format!("save: {e}"));
        return false;
    }
    true
}

pub fn handle_key(app: &mut App, key: KeyCode) {
    // Edit mode handling first.
    if let Some(buf) = app.settings_edit_buf.as_mut() {
        match key {
            KeyCode::Esc => {
                app.settings_edit_buf = None;
            }
            KeyCode::Enter => apply_edit(app),
            KeyCode::Backspace => {
                buf.pop();
            }
            KeyCode::Char(ch) => buf.push(ch),
            _ => {}
        }
        return;
    }

    match key {
        KeyCode::Esc | KeyCode::Char('q') => cancel(app),
        KeyCode::Up | KeyCode::Char('k') => {
            if app.settings_cursor > 0 {
                app.settings_cursor -= 1;
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if app.settings_cursor + 1 < FIELDS.len() {
                app.settings_cursor += 1;
            }
        }
        KeyCode::Enter => {
            if let Some(fld) = current_field(app).copied() {
                app.settings_edit_buf = Some(match fld.key {
                    FieldKey::AssemblyAiApiKey => app.config.assemblyai_api_key.clone(),
                    _ => field_display(app, fld.key),
                });
            }
        }
        KeyCode::Char('s') => {
            if try_save(app) {
                close(app);
            }
        }
        _ => {}
    }
    let _ = AppConfig::config_path(); // keep import referenced
}
```

- [ ] **Step 2: Route key events in `main.rs`**

In `src/main.rs`, update the key dispatch:

```rust
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        match app.mode {
                            AppMode::Normal => handle_normal_mode(app, key.code),
                            AppMode::ModelPicker => handle_picker_mode(app, key.code),
                            AppMode::Help => handle_help_mode(app, key.code),
                            AppMode::Settings => settings_view::handle_key(app, key.code),
                        }
                    }
```

- [ ] **Step 3: Open settings with `,` from Normal mode**

In `fn handle_normal_mode` in `src/main.rs`, add a case. Put it alongside the other single-key cases:

```rust
        KeyCode::Char(',') => {
            if !matches!(app.status, RecordingStatus::Recording) {
                settings_view::open(app);
            } else {
                app.banner = Some("stop recording before opening settings".into());
            }
        }
```

- [ ] **Step 4: Manual test**

Run: `cargo run` in a terminal. Press `,` to open settings, navigate with arrow keys, press Enter on a field, type a new value, Enter to apply. Press `s` to save (empty key + assemblyai should refuse). Press `Esc` to cancel without saving. Verify `~/.config/voice-bird/config.toml` reflects saved values.

- [ ] **Step 5: Build and test**

Run: `cargo test`
Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add src/settings_view.rs src/main.rs src/app.rs
git commit -m "ui(settings): open/close, navigation, field editing, save with validation"
```

---

## Task 15: CLOUD badge in header + 3-second recording reminder

**Files:**
- Modify: `src/ui.rs`

- [ ] **Step 1: Update `render_header`**

In `src/ui.rs`, find `fn render_header`. Add a right-aligned badge and a reminder line when applicable. Replace the body with (adjust to match the existing header layout):

```rust
pub fn render_header(f: &mut Frame, area: Rect, app: &App) {
    // Split the header row horizontally to reserve a right-aligned badge slot
    // whenever the active engine is cloud.
    let (title_area, badge_area) = if app.is_cloud_engine {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(10), Constraint::Length(9)])
            .split(area);
        (cols[0], Some(cols[1]))
    } else {
        (area, None)
    };

    let model = app.config.default_model.as_str();
    let engine = app.engine_kind.as_str();
    let title = format!("Voice Bird · {model} · {engine}");
    f.render_widget(
        Paragraph::new(title).block(Block::default().borders(Borders::BOTTOM)),
        title_area,
    );

    if let Some(ba) = badge_area {
        let badge = Paragraph::new(Span::styled(
            " CLOUD ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
        f.render_widget(badge, ba);
    }
}
```

If the existing `render_header` differs significantly in signature or internals, adapt the above to slot in alongside the existing layout — the goal is a right-aligned yellow `CLOUD` tag when `app.is_cloud_engine` is true.

- [ ] **Step 2: Add the 3-second reminder line**

In `fn render` in `src/ui.rs`, compute a `has_reminder` flag alongside `has_banner`:

```rust
    let has_reminder = app
        .cloud_reminder_until
        .map(|t| std::time::Instant::now() < t)
        .unwrap_or(false);
```

Add a constraint for it when true:

```rust
    let mut constraints: Vec<Constraint> = vec![
        Constraint::Length(3), // header
        Constraint::Length(devices_h),
        Constraint::Min(4),    // committed
        Constraint::Length(3), // tentative
    ];
    if has_reminder {
        constraints.push(Constraint::Length(1));
    }
    if has_banner {
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Length(1));
```

Then render the reminder in the correct slot:

```rust
    let mut idx = 4;
    if has_reminder {
        let r = Paragraph::new(Span::styled(
            "Audio is being sent to AssemblyAI.",
            Style::default().fg(Color::Yellow),
        ));
        f.render_widget(r, root[idx]);
        idx += 1;
    }
    if has_banner {
        render_banner(f, root[idx], app);
        idx += 1;
    }
    render_footer(f, root[idx], app);
```

(Replace the existing tail-end of `render` with the above index-driven dispatch; the current code hardcodes offsets for only `has_banner`.)

- [ ] **Step 3: Build + manual verify**

Run: `cargo build`
Expected: compiles. Set `engine_prefer="assemblyai"` and a valid key in config, launch `cargo run`, start recording, and observe the `CLOUD` badge and the 3-second reminder line.

- [ ] **Step 4: Commit**

```bash
git add src/ui.rs
git commit -m "ui: CLOUD header badge + 3 s recording reminder"
```

---

## Task 16: Persistent banner when cloud preference + no key

**Files:**
- Modify: `src/app.rs`

- [ ] **Step 1: Extend `App::new()` to surface the banner on launch**

In `src/app.rs`, after `AppConfig::load()` succeeds in `App::new()`, add:

```rust
        let banner_on_launch = if config.engine_prefer == "assemblyai"
            && config.assemblyai_api_key.is_empty()
        {
            Some("Cloud engine selected but no API key — open settings (press ',')".into())
        } else {
            None
        };
```

Then replace `banner: None,` in the struct literal with `banner: banner_on_launch,`.

- [ ] **Step 2: Block recording start when the banner condition is live**

In `src/app.rs`'s `fn start_recording`, add very early (before any audio setup):

```rust
        if self.config.engine_prefer == "assemblyai"
            && self.config.assemblyai_api_key.is_empty()
        {
            self.banner = Some(
                "Cloud engine selected but no API key — open settings (press ',')".into(),
            );
            self.status = RecordingStatus::Error("no api key".into());
            return;
        }
```

- [ ] **Step 3: Build + manual verify**

Run: `cargo run` with `engine_prefer = "assemblyai"` and no key in config.
Expected: banner visible at launch; pressing `r` to record does nothing except re-show the banner.

- [ ] **Step 4: Commit**

```bash
git add src/app.rs
git commit -m "app: persistent banner when engine=assemblyai and key missing"
```

---

## Task 17: Disable refinement when the active engine is cloud

**Files:**
- Modify: `src/app.rs`

The existing refinement setup lives inside `start_recording` (around the `refinement_model` lookup / spawn). Currently refinement only spawns when `config.refinement_model` resolves to an existing on-disk gguf file. We gate it additionally on `!self.is_cloud_engine`.

- [ ] **Step 1: Short-circuit refinement for cloud sessions**

In `src/app.rs`, in `start_recording`, at the top of the `self.config.refinement_model.as_ref().and_then(|id| { ... })` chain, add:

```rust
        let (refinement_pcm_tx, refinement_handle) = if self.is_cloud_engine {
            (None, None)
        } else {
            self
                .config
                .refinement_model
                .as_ref()
                .and_then(|id| {
                    /* …existing body unchanged… */
                })
                .unwrap_or((None, None))
        };
```

(Adapt the exact shape to match the current code structure. The intent: if `is_cloud_engine`, skip the refinement setup entirely.)

- [ ] **Step 2: Build + manual verify**

Run: `cargo run` with a cloud engine configured + `refinement_model` set.
Expected: recording starts without a refinement engine spawn (check logs — no "refinement load model" line).

- [ ] **Step 3: Commit**

```bash
git add src/app.rs
git commit -m "app: skip refinement engine when primary engine is cloud"
```

---

## Task 18: README.md — reframe "local by default"

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Update the intro**

In `README.md`, replace:

```
# Voice Bird

Terminal-based voice transcription. Runs fully locally — your audio never leaves your machine.
```

with:

```
# Voice Bird

Terminal-based voice transcription. Runs **locally by default** — your audio never leaves your machine. An optional cloud engine (AssemblyAI) is available for users without a GPU or ANE; it is off by default and requires both an API key and an explicit change to `engine_prefer`. When active, a `CLOUD` badge is shown in the header.
```

- [ ] **Step 2: Add a "Cloud engines (optional)" subsection**

After the "macOS bonus: ANE-accelerated inference" section in `README.md`, add:

```markdown
### Cloud engines (optional)

If your machine can't keep up with local Whisper, Voice Bird can stream audio to AssemblyAI's Universal-Streaming service instead. This is off by default; when on, a `CLOUD` badge is shown in the header and a reminder appears at the start of each recording.

1. Get an API key from https://www.assemblyai.com/.
2. Open Voice Bird and press `,` to open Settings.
3. Set `Engine preference` to `assemblyai`, paste your key into `AssemblyAI API key`, press `s` to save.

Your key lives in `~/.config/voice-bird/config.toml` in plaintext (chmod `0600` on Unix). Anyone with read access to that file can read your key.
```

- [ ] **Step 3: Update the Keys table**

Add a row to the Keys table:

```markdown
| `,` | open settings |
```

- [ ] **Step 4: Commit**

```bash
git add README.md
git commit -m "docs(readme): reframe as local-by-default; document cloud engine"
```

---

## Task 19: CLAUDE.md — update project description and architecture list

**Files:**
- Modify: `CLAUDE.md`

- [ ] **Step 1: Update the Project Overview paragraph**

In `CLAUDE.md`, replace the Project Overview paragraph with:

```
Voice Bird is a Rust TUI for voice transcription that is **local by default** (via `whisper-rs` / WhisperKit) with an **optional cloud engine** (AssemblyAI Universal-Streaming) for users without sufficient local compute. It records microphone audio via `cpal`, resamples to 16 kHz mono with `rubato`, and — depending on `engine_prefer` — runs Whisper locally or streams PCM to AssemblyAI over a WebSocket. Every session lives under `~/voice-bird/sessions/<timestamp>-<source>/` as an append-only `transcript.jsonl` plus finalized `transcript.{json,txt}`, `audio.wav`, and `meta.json`.

Cloud engines are opt-in, require an API key in `config.toml`, and are clearly indicated via a `CLOUD` badge in the TUI header plus a recording-start reminder.
```

- [ ] **Step 2: Add new files to the Architecture list**

In the Architecture bullet list, add:

```
- `src/transcription/assemblyai_engine.rs` — AssemblyAI Universal-Streaming v3 WebSocket client. Streams 16 kHz i16 PCM, maps Turn events onto `EngineEvent`.
- `src/settings_view.rs` — full-screen in-app settings view (opens on `,` from Normal mode); edits `AppConfig` including the AssemblyAI API key.
```

- [ ] **Step 3: Add a Cloud engines note to Key Conventions**

Append to the Key Conventions list:

```
- Cloud engines are opt-in. `engine_prefer = "assemblyai"` requires `assemblyai_api_key` to be set; otherwise start_recording refuses and a persistent banner prompts the user to open settings. The CLOUD badge is driven by `App::is_cloud_engine`, set at start_recording, cleared at stop_recording.
```

- [ ] **Step 4: Commit**

```bash
git add CLAUDE.md
git commit -m "docs(claude): document optional AssemblyAI engine and settings view"
```

---

## Task 20: Real-service smoke test (feature-gated)

**Files:**
- Modify: `tests/engine_smoke.rs`

- [ ] **Step 1: Add a gated smoke test**

Append to `tests/engine_smoke.rs` (after the existing `whisper_rs_produces_non_empty_transcript_for_fixture` test):

```rust
#[cfg(feature = "engine-smoke-assemblyai")]
#[test]
fn assemblyai_produces_committed_event_for_fixture() {
    use std::time::Duration;
    use voice_bird::transcription::{
        assemblyai_engine::AssemblyAiEngine, EngineConfig, EngineEvent, TranscriptionEngine,
    };

    let key = std::env::var("ASSEMBLYAI_API_KEY")
        .expect("ASSEMBLYAI_API_KEY must be set for this smoke test");

    let spec = hound::WavReader::open("tests/fixtures/hello_world_16k.wav").unwrap();
    let samples: Vec<f32> = spec
        .into_samples::<i16>()
        .map(|s| s.unwrap() as f32 / i16::MAX as f32)
        .collect();

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut engine = AssemblyAiEngine::new(key);
        let handle = engine
            .start(EngineConfig::Cloud {
                api_key: String::new(), // ignored; engine's own copy is used for header
                language: Some("en".into()),
                sample_rate: 16_000,
            })
            .unwrap();

        for chunk in samples.chunks(8_000) {
            handle.pcm_tx.send(chunk.to_vec()).await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        // Let the service finalize.
        tokio::time::sleep(Duration::from_secs(3)).await;
        let _ = handle.shutdown.send(());

        let mut rx = handle.events_rx;
        let mut saw_committed = false;
        while let Ok(Ok(ev)) =
            tokio::time::timeout(Duration::from_secs(5), rx.recv()).await
        {
            if matches!(ev, EngineEvent::Committed(_)) {
                saw_committed = true;
                break;
            }
        }
        assert!(saw_committed, "did not receive a Committed event");
    });
}
```

Note: the `AssemblyAiEngine::new(key)` constructor takes the real API key. Pass any placeholder for `EngineConfig::Cloud.api_key` — the engine checks it is non-empty but uses the constructor copy for the WebSocket header. To avoid confusion, pass the real key in both. Revise the snippet if needed.

Simplified body for clarity — pass the key in both places:

```rust
        let key_clone = std::env::var("ASSEMBLYAI_API_KEY").unwrap();
        let mut engine = AssemblyAiEngine::new(key_clone.clone());
        let handle = engine
            .start(EngineConfig::Cloud {
                api_key: key_clone,
                language: Some("en".into()),
                sample_rate: 16_000,
            })
            .unwrap();
```

- [ ] **Step 2: Run the gated test locally (one-time manual verify)**

```bash
ASSEMBLYAI_API_KEY=sk-... cargo test \
  --features engine-smoke-assemblyai \
  --test engine_smoke assemblyai_produces_committed_event_for_fixture \
  -- --nocapture
```

Expected: at least one `EngineEvent::Committed` received; test passes.

- [ ] **Step 3: Verify default `cargo test` still passes (feature off)**

Run: `cargo test`
Expected: feature-gated test is skipped by cfg; all other tests pass.

- [ ] **Step 4: Commit**

```bash
git add tests/engine_smoke.rs
git commit -m "test(assemblyai): feature-gated real-service smoke test"
```

---

## Task 21: Final build + lint pass

**Files:** _(none; verification only)_

- [ ] **Step 1: Build all targets**

Run: `cargo build --all-targets`
Expected: clean build, no warnings.

- [ ] **Step 2: Run all tests**

Run: `cargo test`
Expected: all pass.

- [ ] **Step 3: Run clippy**

Run: `cargo clippy --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 4: Run xtask check-catalog (CI gate)**

Run: `cargo run -p xtask -- check-catalog`
Expected: passes (no `<FILL>` placeholders introduced).

- [ ] **Step 5: Commit any trivial fixups (if needed)**

```bash
git add -u
git commit -m "chore: resolve clippy/build cleanups"
```

(If nothing to commit, skip.)

---

## Self-Review (completed inline by plan author)

**1. Spec coverage check**
- Cloud engine + Universal-Streaming v3 protocol → Tasks 6–10
- `EngineConfig` enum split → Tasks 4, 5
- `select_engine` new branch + refinement disable → Tasks 11, 17
- AssemblyAI `assemblyai_api_key` field + 0600 chmod + warning comment → Tasks 2, 3
- `AppMode::Settings` + `is_cloud_engine` + cloud-reminder lifecycle → Task 12
- Full-screen settings view (layout, navigation, edit, save/cancel, API key masking, validation) → Tasks 13, 14
- CLOUD header badge + 3-second recording reminder → Task 15
- Persistent banner when cloud preference + no key + recording block → Task 16
- README reframe + CLAUDE.md reframe → Tasks 18, 19
- Feature-gated real-service smoke test → Task 20
- Mock WebSocket unit tests for engine → Tasks 7–10
- `tokio-tungstenite` + rustls deps → Task 1
- Final verification → Task 21

**2. Placeholder scan** — no "TBD", "TODO", "implement later", or "similar to Task N" references. Every step shows concrete code or commands.

**3. Type consistency** — `EngineKind` enum (Task 11) is referenced consistently. `FieldKey` variants in `settings_view` are referenced with the same names in both the `FIELDS` table (Task 13) and the `apply_edit` / `field_display` match arms (Tasks 13, 14). `is_cloud_engine` field is introduced in Task 12 and first referenced there (with a forward-reference caveat in Task 11 Step 4 noted explicitly).

One known out-of-order reference: Task 11 Step 4 references `self.is_cloud_engine` which is added in Task 12. The task body flags this and recommends implementing 11 + 12 together or 12 before 11. Acceptable given the plan is read linearly.

No other ambiguities found.
