use std::path::Path;

use crate::session::finalize::{finalize, SessionMeta};

pub fn recover(session_dir: &Path) -> anyhow::Result<()> {
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
