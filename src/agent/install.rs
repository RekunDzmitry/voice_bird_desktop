//! Locate the user's agent runtime (today: `oh-my-pi` / omp) and
//! report its version.
//!
//! Search order (first hit wins):
//!   1. `OMP_BIN` environment variable (CI / scripted overrides).
//!   2. The first executable named `omp` on `PATH`.
//!   3. `~/.bun/bin/omp` (the most common install location for the
//!      official oh-my-pi bootstrap).
//!
//! Version is parsed from `omp --version` output. The reference
//! install reports `"omp/<x.y.z>"` so we strip the leading `omp/`
//! before storing.
use std::path::{Path, PathBuf};

use super::AgentStatus;

/// How the agent runtime was found. Surfaced in the status bar
/// so the user can sanity-check the detection source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentDetectionSource {
    Env,
    Path,
    Bun,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDetection {
    pub path: PathBuf,
    pub version: String,
    pub source: AgentDetectionSource,
}

/// Locate the agent runtime and parse its version. Pure function
/// — the caller decides what to do with the result (storing it
/// in `App::agent`, surfacing it in the status bar, etc.).
pub fn detect() -> Result<AgentDetection, AgentStatus> {
    // 1. Explicit override.
    if let Some(p) = std::env::var_os("OMP_BIN") {
        let path = PathBuf::from(p);
        if let Ok(det) = check_executable(&path) {
            return Ok(AgentDetection {
                path,
                version: det,
                source: AgentDetectionSource::Env,
            });
        }
    }

    // 2. `PATH` lookup. We deliberately do not shell out to `which`
    //    because the plan said so and because walking PATH is a few
    //    lines of std-only code that avoids the new dependency on
    //    Phase C.
    if let Some(path) = find_on_path("omp") {
        if let Ok(version) = check_executable(&path) {
            return Ok(AgentDetection {
                path,
                version,
                source: AgentDetectionSource::Path,
            });
        }
    }

    // 3. `~/.bun/bin/omp` — bun is the install path the official
    //    oh-my-pi bootstrap recommends.
    if let Some(home) = dirs::home_dir() {
        let candidate = home.join(".bun").join("bin").join("omp");
        if let Ok(version) = check_executable(&candidate) {
            return Ok(AgentDetection {
                path: candidate,
                version,
                source: AgentDetectionSource::Bun,
            });
        }
    }

    Err(AgentStatus::NotFound)
}

fn find_on_path(binary: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for entry in std::env::split_paths(&path) {
        let candidate = entry.join(binary);
        if is_executable(&candidate) {
            return Some(candidate);
        }
        // Windows: also check `omp.exe`.
        #[cfg(windows)]
        {
            let exe = entry.join(format!("{binary}.exe"));
            if is_executable(&exe) {
                return Some(exe);
            }
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(path) {
            Ok(md) => md.permissions().mode() & 0o111 != 0,
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn check_executable(path: &Path) -> Result<String, ()> {
    if !is_executable(path) {
        return Err(());
    }
    let output = std::process::Command::new(path)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .map_err(|_| ())?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_version(&stdout).ok_or(())
}

/// Pull `x.y.z` out of the omp version banner. Reference output:
/// `omp/16.3.11`. Tolerates a leading `omp` (no slash) and any
/// surrounding whitespace.
fn parse_version(stdout: &str) -> Option<String> {
    for raw in stdout.split_whitespace() {
        let stripped = raw
            .strip_prefix("omp/")
            .or_else(|| raw.strip_prefix("omp"))
            .unwrap_or(raw);
        if stripped.is_empty() || !stripped.chars().next()?.is_ascii_digit() {
            continue;
        }
        // Stop at the first non-version character (commit hash, tag,
        // newline, etc.).
        let end = stripped
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(stripped.len());
        let candidate = &stripped[..end];
        if candidate.split('.').count() >= 2 {
            return Some(candidate.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_version_strips_omp_prefix() {
        assert_eq!(parse_version("omp/16.3.11\n").as_deref(), Some("16.3.11"));
        assert_eq!(parse_version("omp 16.3.11").as_deref(), Some("16.3.11"));
        assert_eq!(parse_version("omp/16.3.11 (abc123)").as_deref(), Some("16.3.11"));
    }

    #[test]
    fn parse_version_rejects_garbage() {
        assert_eq!(parse_version("hello world").as_deref(), None);
        assert_eq!(parse_version("").as_deref(), None);
    }

    #[test]
    fn is_executable_rejects_directories() {
        // /tmp is always a directory on Unix; this must return false
        // so we don't accidentally "find" `omp` in the search root.
        assert!(!is_executable(Path::new("/tmp")));
    }
    /// When `OMP_BIN` points at a tiny shim that prints `omp/<ver>`,
    /// detection must use it and tag the result as `Env`.
    #[test]
    fn detect_prefers_env_var_over_path() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("fake-omp");
        std::fs::write(
            &bin,
            "#!/bin/sh\necho 'omp/9.9.9'\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // SAFETY: tests are single-threaded for env mutations.
        unsafe { std::env::set_var("OMP_BIN", &bin); }
        let det = detect().expect("OMP_BIN should be honored");
        unsafe { std::env::remove_var("OMP_BIN"); }
        assert_eq!(det.path, bin);
        assert_eq!(det.version, "9.9.9");
        assert_eq!(det.source, AgentDetectionSource::Env);
    }

    /// With no env override and an empty PATH, detection returns
    /// `AgentStatus::NotFound` instead of panicking.
    #[test]
    fn detect_returns_not_found_when_no_candidates_exist() {
        let empty = tempfile::tempdir().unwrap();
        // SAFETY: tests are single-threaded for env mutations.
        unsafe {
            std::env::remove_var("OMP_BIN");
            std::env::set_var("PATH", empty.path());
            std::env::set_var("HOME", empty.path());
        }
        let result = detect();
        assert!(matches!(result, Err(AgentStatus::NotFound)));
    }
}
