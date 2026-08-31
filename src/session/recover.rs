use std::path::Path;

use crate::session::finalize::{finalize, SessionMeta};

pub fn recover(session_dir: &Path) -> anyhow::Result<()> {
    // Room-nested layout: `<ts>-room-<slug>/<role_slug>/<ts2>-<source>`
    // We recover each role's session dir, not the room dir itself.
    if session_dir
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.contains("-room-"))
        .unwrap_or(false)
    {
        for entry in std::fs::read_dir(session_dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                // Each role's dir contains per-source session subdirs.
                for session in std::fs::read_dir(entry.path())? {
                    let session = session?;
                    if session.file_type()?.is_dir() {
                        recover(&session.path())?;
                    }
                }
            }
        }
        return Ok(());
    }
    // Flat layout: a single session dir with transcript.jsonl
    // at the top. The legacy behavior: re-finalize the existing
    // transcript into transcript.json/txt using whatever meta
    // (or the default) is at the top of the dir.
     let jsonl = session_dir.join("transcript.jsonl");
     let out_json = session_dir.join("transcript.json");
     let out_txt = session_dir.join("transcript.txt");
     let out_meta = session_dir.join("meta.json");

     let meta = if out_meta.exists() {
         serde_json::from_str::<SessionMeta>(&std::fs::read_to_string(&out_meta)?)?
     } else {
         SessionMeta::default()
     };

    finalize(&jsonl, &out_json, &out_txt, &out_meta, &meta)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Recovering a room-nested dir walks each role's per-source
    /// session subdir, re-finalizing its transcript.jsonl into
    /// transcript.json + transcript.txt. The room dir itself
    /// doesn't have transcript.jsonl at the top — that's why
    /// we recurse into the per-role subdirs.
    #[test]
    fn recover_room_nested_walks_per_role_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let room_dir = dir.path().join("2026-05-13_10-00-00-room-doctor-appointment");
        // Two roles, each with one per-source session dir.
        for role in &["patient", "doctor"] {
            let role_dir = room_dir.join(role);
            std::fs::create_dir_all(&role_dir).unwrap();
            let session_dir = role_dir.join("2026-05-13_10-00-00-mic");
            std::fs::create_dir_all(&session_dir).unwrap();
            std::fs::write(
                session_dir.join("transcript.jsonl"),
                r#"{"t_start_ms":0,"t_end_ms":1000,"text":"hello"}"#,
            )
            .unwrap();
        }
        recover(&room_dir).unwrap();
        // Each per-role session dir now has transcript.json +
        // transcript.txt (the finalize step). The room dir
        // itself does NOT have these — only the leaf dirs do.
        for role in &["patient", "doctor"] {
            let session = room_dir.join(role).join("2026-05-13_10-00-00-mic");
            assert!(
                session.join("transcript.json").exists(),
                "transcript.json missing for {role}"
            );
            assert!(
                session.join("transcript.txt").exists(),
                "transcript.txt missing for {role}"
            );
        }
        // The room dir itself shouldn't have a transcript.json
        // (it never had transcript.jsonl to start with).
        assert!(!room_dir.join("transcript.json").exists());
    }

    /// Flat layout (the pre-room-nesting default) still works.
    /// The legacy single transcript.jsonl at the top of the
    /// dir gets re-finalized as before.
    #[test]
    fn recover_flat_layout_still_works() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("2026-05-13_10-00-00-mic");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(
            session_dir.join("transcript.jsonl"),
            r#"{"t_start_ms":0,"t_end_ms":1000,"text":"hello flat"}"#,
        )
        .unwrap();
        recover(&session_dir).unwrap();
        assert!(session_dir.join("transcript.json").exists());
        assert!(session_dir.join("transcript.txt").exists());
    }
}
