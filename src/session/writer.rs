use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WrittenSegment {
    pub t_start_ms: u64,
    pub t_end_ms: u64,
    pub text: String,
}

pub struct SegmentWriter {
    file: BufWriter<File>,
}

impl SegmentWriter {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        Ok(Self { file: BufWriter::new(file) })
    }

    pub fn append(&mut self, seg: &WrittenSegment) -> anyhow::Result<()> {
        serde_json::to_writer(&mut self.file, seg)?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        self.file.get_ref().sync_data()?;  // fsync per segment
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::TempDir;

    fn seg(t0: f64, t1: f64, text: &str) -> WrittenSegment {
        WrittenSegment {
            t_start_ms: (t0 * 1000.0) as u64,
            t_end_ms:   (t1 * 1000.0) as u64,
            text: text.into(),
        }
    }

    #[test]
    fn appends_one_line_per_segment() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("transcript.jsonl");
        let mut w = SegmentWriter::open(&path).unwrap();
        w.append(&seg(0.0, 1.5, "hello")).unwrap();
        w.append(&seg(1.5, 3.0, "world")).unwrap();
        drop(w);

        let s = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<_> = s.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"text\":\"hello\""));
        assert!(lines[1].contains("\"text\":\"world\""));
    }

    #[test]
    fn survives_reopen_and_appends() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("transcript.jsonl");
        {
            let mut w = SegmentWriter::open(&path).unwrap();
            w.append(&seg(0.0, 1.0, "first")).unwrap();
        }
        {
            let mut w = SegmentWriter::open(&path).unwrap();
            w.append(&seg(1.0, 2.0, "second")).unwrap();
        }
        let s = std::fs::read_to_string(&path).unwrap();
        assert_eq!(s.lines().count(), 2);
    }
}
