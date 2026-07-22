//! Add / remove the voice-bird entry in `~/.omp/agent/mcp.json`.
//!
//! Today's runtime is `oh-my-pi` (omp) 16.3.11, which reads this
//! file at startup and spawns each `command` under `mcpServers`
//! as a stdio MCP server. We register one server per voice-bird
//! App under the name `voice-bird`; omp then surfaces our tools
//! as `mcp__voice-bird__push_segment` etc. in the agent's tool
//! list.
//!
//! On drop we remove the entry so a dead voice-bird doesn't leave a
//! stale registration that causes the agent runtime to fail to
//! start the server.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const MCP_FILE: &str = "agent/mcp.json";
const SERVER_NAME: &str = "voice-bird";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct McpFile {
    #[serde(default, rename = "mcpServers")]
    mcp_servers: BTreeMap<String, McpServer>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct McpServer {
    #[serde(default = "default_type")]
    r#type: String,
    command: String,
    #[serde(default)]
    args: Vec<String>,
}

fn default_type() -> String {
    "stdio".to_string()
}

/// Resolve the absolute path of `~/.omp/agent/mcp.json`. Centralised
/// so tests can stub it.
fn mcp_path(home_dir: &Path) -> PathBuf {
    home_dir.join(MCP_FILE)
}

/// Resolve `~/.omp/` (the directory the agent runtime reads
/// `agent/mcp.json` from). We honour `$OMP_HOME` for tests /
/// sandboxing; default is `$HOME/.omp`.
pub fn register_home() -> PathBuf {
    if let Ok(p) = std::env::var("OMP_HOME") {
        return PathBuf::from(p);
    }
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".omp")
}

fn read_file(path: &Path) -> anyhow::Result<McpFile> {
    if !path.exists() {
        return Ok(McpFile::default());
    }
    // Tolerate a missing `mcpServers` field — newer agent
    // configs may carry other top-level keys.
    let raw = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&raw).unwrap_or_default())
}

fn write_file(path: &Path, body: &McpFile) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let raw = serde_json::to_string_pretty(body)?;
    std::fs::write(path, raw)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o644);
        let _ = std::fs::set_permissions(path, perms);
    }
    Ok(())
}

/// Register the voice-bird MCP server in `mcp.json`. Idempotent —
/// calling twice with the same binary path leaves the file in the
/// same state. Returns `Ok(())` even when the file already has other
/// entries; we only touch the `voice-bird` key.
pub fn register(binary_path: &Path, home_dir: &Path) -> anyhow::Result<()> {
    let path = mcp_path(home_dir);
    let mut body = read_file(&path)?;
    body.mcp_servers.insert(
        SERVER_NAME.into(),
        McpServer {
            r#type: "stdio".into(),
            command: binary_path.to_string_lossy().into_owned(),
            args: vec!["--mcp-server".into()],
        },
    );
    write_file(&path, &body)?;
    Ok(())
}

/// Remove the voice-bird MCP server entry from `mcp.json`. Idempotent
/// — does nothing if the key isn't present.
pub fn unregister(home_dir: &Path) -> anyhow::Result<()> {
    let path = mcp_path(home_dir);
    if !path.exists() {
        return Ok(());
    }
    let mut body = read_file(&path)?;
    body.mcp_servers.remove(SERVER_NAME);
    write_file(&path, &body)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn register_then_unregister_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let fake_bin = dir.path().join("voice-bird-cli");
        std::fs::write(&fake_bin, "#!/bin/sh\n").unwrap();

        register(&fake_bin, dir.path()).unwrap();
        let body = read_file(&mcp_path(dir.path())).unwrap();
        let entry = body.mcp_servers.get(SERVER_NAME).unwrap();
        assert_eq!(entry.command, fake_bin.to_string_lossy());
        assert_eq!(entry.r#type, "stdio");
        assert_eq!(entry.args, vec!["--mcp-server".to_string()]);

        unregister(dir.path()).unwrap();
        let body = read_file(&mcp_path(dir.path())).unwrap();
        assert!(body.mcp_servers.get(SERVER_NAME).is_none());
    }

    #[test]
    fn register_preserves_other_entries() {
        let dir = tempfile::tempdir().unwrap();
        let fake_bin = dir.path().join("voice-bird-cli");
        std::fs::write(&fake_bin, "#!/bin/sh\n").unwrap();
        // Pre-populate with an unrelated server. mcp_path expects the
        // `agent/` subdirectory to exist before write_file runs.
        std::fs::create_dir_all(mcp_path(dir.path()).parent().unwrap()).unwrap();
        std::fs::write(
            mcp_path(dir.path()),
            r#"{"mcpServers":{"codebase-memory":{"command":"cbm"}}}
            "#,
        )
        .unwrap();
        register(&fake_bin, dir.path()).unwrap();
        let body = read_file(&mcp_path(dir.path())).unwrap();
        assert!(body.mcp_servers.contains_key("codebase-memory"));
        assert!(body.mcp_servers.contains_key(SERVER_NAME));
    }

    #[test]
    fn unregister_is_idempotent_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        unregister(dir.path()).unwrap();
        // No panic, no error.
    }
    #[test]
    fn register_overwrites_with_later_path() {
        // Regression: App::new() used to register the agent
        // runtime's binary path (passed via AgentDetection.path)
        // instead of voice-bird's own current_exe(). The fix is
        // owns the resulting `command` field — re-registering with a
        // different path must overwrite.
        let dir = tempfile::tempdir().unwrap();
        let omp_path = dir.path().join("omp");
        let voice_bird_path = dir.path().join("voice-bird-cli");
        std::fs::write(&omp_path, "#!/bin/sh\n").unwrap();
        std::fs::write(&voice_bird_path, "#!/bin/sh\n").unwrap();

        register(&omp_path, dir.path()).unwrap();
        register(&voice_bird_path, dir.path()).unwrap();

        let body = read_file(&mcp_path(dir.path())).unwrap();
        let entry = body.mcp_servers.get(SERVER_NAME).unwrap();
        assert_eq!(
            entry.command,
            voice_bird_path.to_string_lossy(),
            "the later registration (voice-bird binary) must win",
        );
        assert_eq!(entry.args, vec!["--mcp-server".to_string()]);
    }

    #[test]
    #[serial]
    fn register_home_default_uses_home_env() {
        let prev_home = std::env::var("HOME").ok();
        let prev_omp = std::env::var("OMP_HOME").ok();
        std::env::set_var("HOME", "/tmp/vb-test-home");
        std::env::remove_var("OMP_HOME");
        let got = register_home();
        restore("HOME", prev_home);
        restore("OMP_HOME", prev_omp);
        assert_eq!(got, std::path::PathBuf::from("/tmp/vb-test-home/.omp"));
    }

    #[test]
    #[serial]
    fn register_home_honours_omp_home_override() {
        let prev = std::env::var("OMP_HOME").ok();
        std::env::set_var("OMP_HOME", "/tmp/vb-omp-override");
        let got = register_home();
        restore("OMP_HOME", prev);
        assert_eq!(got, std::path::PathBuf::from("/tmp/vb-omp-override"));
    }

    fn restore(key: &str, prev: Option<String>) {
        match prev {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }
}
