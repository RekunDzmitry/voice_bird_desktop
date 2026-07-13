//! Add / remove the voice-bird entry in `~/.omp/agent/mcp.json`.
//!
//! omp 16.3.11 reads this file at startup and spawns each `command`
//! under `mcpServers` as a stdio MCP server. We register one server
//! per voice-bird App under the name `voice-bird`; omp then surfaces
//! our tools as `mcp__voice-bird__push_segment` etc. in the agent's
//! tool list.
//!
//! On drop we remove the entry so a dead voice-bird doesn't leave a
//! stale registration that causes omp to fail to start the server.

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

fn read_file(path: &Path) -> anyhow::Result<McpFile> {
    if !path.exists() {
        return Ok(McpFile::default());
    }
    let raw = std::fs::read_to_string(path)?;
    // Tolerate a missing `mcpServers` field — newer omp configs may
    // carry other top-level keys.
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
}
