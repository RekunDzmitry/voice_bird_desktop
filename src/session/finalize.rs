use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::session::writer::WrittenSegment;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionMeta {
    pub version: String,
    pub model: String,
    pub engine: String,
    pub source: String,
    pub device: String,
    pub started_at: String,
    pub ended_at: String,
    pub duration_ms: u64,
}

#[derive(Debug, Serialize)]
struct FinalTranscript<'a> {
    segments: &'a [WrittenSegment],
    meta: &'a SessionMeta,
}

/// Owned version of `FinalTranscript` — used to deserialize
/// transcript.json for export/recovery.
#[derive(Debug, Deserialize)]
pub struct FinalTranscriptValue {
    pub segments: Vec<WrittenSegment>,
    pub meta: SessionMeta,
}

pub fn finalize(
    jsonl: &Path,
    out_json: &Path,
    out_txt: &Path,
    out_meta: &Path,
    meta: &SessionMeta,
) -> anyhow::Result<()> {
    let segments = read_jsonl(jsonl)?;
    write_atomic(out_json, |f| {
        serde_json::to_writer_pretty(f, &FinalTranscript { segments: &segments, meta })?;
        Ok(())
    })?;
    write_atomic(out_txt, |f| {
        for s in &segments {
            writeln!(f, "{}", s.text)?;
        }
        Ok(())
    })?;
    write_atomic(out_meta, |f| {
        serde_json::to_writer_pretty(f, meta)?;
        Ok(())
    })?;
    Ok(())
}

fn read_jsonl(path: &Path) -> anyhow::Result<Vec<WrittenSegment>> {
    let s = std::fs::read_to_string(path)?;
    let mut out = Vec::new();
    for line in s.lines() {
        let line = line.trim();
        if line.is_empty() { continue; }
        out.push(serde_json::from_str(line)?);
    }
    Ok(out)
}

fn write_atomic<F>(path: &Path, write: F) -> anyhow::Result<()>
where F: FnOnce(&mut std::fs::File) -> anyhow::Result<()>
{
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        write(&mut f)?;
        f.sync_data()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::writer::{SegmentWriter, WrittenSegment};
    use tempfile::TempDir;

    #[test]
    fn writes_json_and_txt_from_jsonl() {
        let dir = TempDir::new().unwrap();
        let jsonl = dir.path().join("transcript.jsonl");
        {
            let mut w = SegmentWriter::open(&jsonl).unwrap();
            w.append(&WrittenSegment { t_start_ms: 0,    t_end_ms: 1500, text: "hello".into() }).unwrap();
            w.append(&WrittenSegment { t_start_ms: 1500, t_end_ms: 3000, text: "world".into() }).unwrap();
        }

        let meta = SessionMeta {
            version: "0.3.0".into(),
            model: "distil-small.en".into(),
            engine: "whisper_rs".into(),
            source: "mic".into(),
            device: "MacBook Pro Microphone".into(),
            started_at: "2026-04-16T14:32:07Z".into(),
            ended_at: "2026-04-16T14:32:10Z".into(),
            duration_ms: 3000,
        };

        let out_json = dir.path().join("transcript.json");
        let out_txt  = dir.path().join("transcript.txt");
        let out_meta = dir.path().join("meta.json");
        finalize(&jsonl, &out_json, &out_txt, &out_meta, &meta).unwrap();

        let j: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&out_json).unwrap()).unwrap();
        assert_eq!(j["segments"].as_array().unwrap().len(), 2);
        assert_eq!(j["meta"]["model"], "distil-small.en");

        let t = std::fs::read_to_string(&out_txt).unwrap();
        assert_eq!(t, "hello\nworld\n");

        let m: SessionMeta =
            serde_json::from_str(&std::fs::read_to_string(&out_meta).unwrap()).unwrap();
        assert_eq!(m.model, "distil-small.en");
    }

    #[test]
    fn empty_jsonl_produces_empty_transcript() {
        let dir = TempDir::new().unwrap();
        let jsonl = dir.path().join("transcript.jsonl");
        std::fs::write(&jsonl, "").unwrap();
        let meta = SessionMeta::default();

        finalize(
            &jsonl,
            &dir.path().join("transcript.json"),
            &dir.path().join("transcript.txt"),
            &dir.path().join("meta.json"),
            &meta,
        ).unwrap();

        assert_eq!(std::fs::read_to_string(dir.path().join("transcript.txt")).unwrap(), "");
    }
}
