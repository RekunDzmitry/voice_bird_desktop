//! Durable per-recording transcripts. PCM never enters this store.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{bail, Context};
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use crate::audio::CaptureSource;
use crate::audio_sources::AppTarget;
use crate::bus::TranscriptPass;

#[derive(Serialize, Deserialize)]
struct WrittenSegment {
    t_start_ms: u64,
    t_end_ms: u64,
    text: String,
}

#[derive(Serialize)]
struct SegmentRef<'a> {
    t_start_ms: u64,
    t_end_ms: u64,
    text: &'a str,
}

#[derive(Serialize)]
struct SourceMeta {
    kind: &'static str,
    device_name: String,
    app: Option<AppTarget>,
}

#[derive(Serialize)]
struct SessionMeta {
    version: &'static str,
    block: u8,
    recording: u64,
    language: String,
    live_model: String,
    refine_model: String,
    source: SourceMeta,
    started_at: String,
    ended_at: Option<String>,
    duration_ms: u64,
    status: &'static str,
}

struct SegmentWriter(BufWriter<File>);

impl SegmentWriter {
    fn create(path: &Path) -> anyhow::Result<Self> {
        let file = OpenOptions::new().create_new(true).write(true).open(path)?;
        file.sync_all()?;
        Ok(Self(BufWriter::new(file)))
    }

    fn append(&mut self, segment: &SegmentRef<'_>) -> anyhow::Result<()> {
        serde_json::to_writer(&mut self.0, segment)?;
        self.0.write_all(b"\n")?;
        self.0.flush()?;
        self.0.get_ref().sync_all()?;
        Ok(())
    }
}

/// Owns one unique recording directory and its fsynced committed transcripts.
/// Finalization reads the durable JSONL, rather than retaining unbounded text.
pub struct SessionWriter {
    path: PathBuf,
    live: SegmentWriter,
    refined: SegmentWriter,
    meta: SessionMeta,
    started: Instant,
    finalized: bool,
}

impl SessionWriter {
    pub fn create(
        root: &Path,
        block: u8,
        recording: u64,
        source: &CaptureSource,
        language: &str,
        live_model: &str,
        refine_model: &str,
    ) -> anyhow::Result<Self> {
        let now = Utc::now();
        let started = Instant::now();
        let (slug, source) = match source {
            CaptureSource::Mic { device_name } => (
                "mic".to_owned(),
                SourceMeta {
                    kind: "mic",
                    device_name: device_name.clone(),
                    app: None,
                },
            ),
            CaptureSource::App { device_name, app } => (
                format!("app-{}", normalize_slug(&app.name)),
                SourceMeta {
                    kind: "app",
                    device_name: device_name.clone(),
                    app: Some(app.clone()),
                },
            ),
        };
        std::fs::create_dir_all(root).with_context(|| format!("create sessions root {root:?}"))?;
        let path = root.join(format!(
            "{}-b{block}-{slug}-r{recording}",
            now.format("%Y-%m-%d_%H-%M-%S")
        ));
        // Never append to an existing session, even if the caller reuses an id.
        std::fs::create_dir(&path).with_context(|| format!("create session {path:?}"))?;
        File::open(root)?.sync_all()?;
        let live = SegmentWriter::create(&path.join("live.jsonl"))?;
        let refined = SegmentWriter::create(&path.join("transcript.jsonl"))?;
        let meta = SessionMeta {
            version: env!("CARGO_PKG_VERSION"),
            block,
            recording,
            language: language.into(),
            live_model: live_model.into(),
            refine_model: refine_model.into(),
            source,
            started_at: now.to_rfc3339_opts(SecondsFormat::Secs, true),
            ended_at: None,
            duration_ms: 0,
            status: "recording",
        };
        write_atomic(&path.join("meta.json"), |file| {
            serde_json::to_writer_pretty(file, &meta)?;
            Ok(())
        })?;
        Ok(Self {
            path,
            live,
            refined,
            meta,
            started,
            finalized: false,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn started_at(&self) -> &str {
        &self.meta.started_at
    }

    pub fn append(
        &mut self,
        pass: TranscriptPass,
        t_start_ms: u64,
        t_end_ms: u64,
        text: &str,
    ) -> anyhow::Result<()> {
        if self.finalized {
            bail!("cannot append to a finalized session");
        }
        let segment = SegmentRef {
            t_start_ms,
            t_end_ms,
            text,
        };
        match pass {
            TranscriptPass::Live => self.live.append(&segment),
            TranscriptPass::Refined => self.refined.append(&segment),
        }
    }

    pub fn finalize(&mut self, status: &str) -> anyhow::Result<()> {
        let status = match status {
            "stopped" => "stopped",
            "failed" => "failed",
            _ => bail!("invalid final session status: {status}"),
        };
        if self.finalized {
            bail!("session already finalized");
        }
        let refined = self.path.join("transcript.jsonl");
        let transcript = if refined.metadata()?.len() == 0 {
            self.path.join("live.jsonl")
        } else {
            refined
        };
        write_atomic(&self.path.join("transcript.txt"), |file| {
            for line in BufReader::new(File::open(&transcript)?).lines() {
                let line = line?;
                if !line.trim().is_empty() {
                    let segment: WrittenSegment = serde_json::from_str(&line)
                        .with_context(|| format!("read committed transcript {transcript:?}"))?;
                    writeln!(file, "{}", segment.text)?;
                }
            }
            Ok(())
        })?;
        self.meta.ended_at = Some(Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true));
        self.meta.duration_ms = self.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        self.meta.status = status;
        write_atomic(&self.path.join("meta.json"), |file| {
            serde_json::to_writer_pretty(file, &self.meta)?;
            Ok(())
        })?;
        self.finalized = true;
        Ok(())
    }
}

fn normalize_slug(name: &str) -> String {
    let mut slug = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    if slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        slug.push_str("unknown");
    }
    slug
}

fn write_atomic(
    path: &Path,
    write: impl FnOnce(&mut File) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let temporary = path.with_extension(format!(
        "{}.tmp",
        path.extension()
            .and_then(|value| value.to_str())
            .unwrap_or("")
    ));
    let mut created = false;
    let result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        created = true;
        write(&mut file)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        File::open(path.parent().context("atomic output has no parent")?)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() && created {
        // Preserve the original I/O/serialization error; cleanup is best effort.
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn writer(root: &Path, id: u64) -> SessionWriter {
        SessionWriter::create(
            root,
            2,
            id,
            &CaptureSource::Mic {
                device_name: "Built-in microphone".into(),
            },
            "en",
            "distil-small.en",
            "large-v3-turbo",
        )
        .unwrap()
    }

    #[test]
    fn durable_lines_and_refined_final_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = writer(dir.path(), 11);
        writer
            .append(TranscriptPass::Live, 0, 1100, "live\ntext")
            .unwrap();
        writer
            .append(TranscriptPass::Refined, 0, 1000, "corrected")
            .unwrap();
        let live: WrittenSegment = serde_json::from_str(
            std::fs::read_to_string(writer.path().join("live.jsonl"))
                .unwrap()
                .trim(),
        )
        .unwrap();
        assert_eq!(
            (live.t_start_ms, live.t_end_ms, live.text.as_str()),
            (0, 1100, "live\ntext")
        );
        let refined: WrittenSegment = serde_json::from_str(
            std::fs::read_to_string(writer.path().join("transcript.jsonl"))
                .unwrap()
                .trim(),
        )
        .unwrap();
        assert_eq!(
            (refined.t_start_ms, refined.t_end_ms, refined.text.as_str()),
            (0, 1000, "corrected")
        );
        // Force a deterministic elapsed duration without sleeping.
        writer.started = Instant::now() - std::time::Duration::from_millis(1500);
        writer.finalize("stopped").unwrap();
        assert_eq!(
            std::fs::read_to_string(writer.path().join("transcript.txt")).unwrap(),
            "corrected\n"
        );
        let meta: serde_json::Value =
            serde_json::from_slice(&std::fs::read(writer.path().join("meta.json")).unwrap())
                .unwrap();
        assert_eq!(meta["status"], "stopped");
        assert_eq!(meta["language"], "en");
        assert_eq!(meta["live_model"], "distil-small.en");
        assert_eq!(meta["refine_model"], "large-v3-turbo");
        assert_eq!(meta["source"]["kind"], "mic");
        assert_eq!(meta["source"]["device_name"], "Built-in microphone");
        assert_eq!(meta["started_at"], writer.started_at());
        chrono::DateTime::parse_from_rfc3339(meta["ended_at"].as_str().unwrap()).unwrap();
        assert!(meta["duration_ms"].as_u64().unwrap() >= 1500);
        assert!(writer
            .append(TranscriptPass::Live, 1100, 1200, "late")
            .is_err());
        assert!(writer.finalize("failed").is_err());
    }

    #[test]
    fn failed_session_falls_back_to_live_and_resumes_unique_directory() {
        let dir = tempfile::tempdir().unwrap();
        let mut first = writer(dir.path(), 20);
        let second = writer(dir.path(), 21);
        assert_ne!(first.path(), second.path());
        first
            .append(TranscriptPass::Live, 0, 1000, "fallback")
            .unwrap();
        first.finalize("failed").unwrap();
        assert_eq!(
            std::fs::read_to_string(first.path().join("transcript.txt")).unwrap(),
            "fallback\n"
        );
        let meta: serde_json::Value =
            serde_json::from_slice(&std::fs::read(first.path().join("meta.json")).unwrap())
                .unwrap();
        assert_eq!(meta["status"], "failed");
        assert!(!second.path().join("transcript.txt").exists());
    }

    #[test]
    fn app_source_metadata_and_safe_directory_name() {
        let dir = tempfile::tempdir().unwrap();
        let source = CaptureSource::App {
            device_name: "Speakers".into(),
            app: AppTarget {
                id: "com.test.app".into(),
                name: "../../Video / Call".into(),
                pid: 42,
            },
        };
        let writer =
            SessionWriter::create(dir.path(), 1, 30, &source, "uk", "live", "refine").unwrap();
        assert_eq!(writer.path().parent(), Some(dir.path()));
        assert!(writer
            .path()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .ends_with("-b1-app-video-call-r30"));
        let meta: serde_json::Value =
            serde_json::from_slice(&std::fs::read(writer.path().join("meta.json")).unwrap())
                .unwrap();
        assert_eq!(meta["source"]["app"]["id"], "com.test.app");
        assert_eq!(meta["source"]["app"]["name"], "../../Video / Call");
        assert_eq!(meta["source"]["app"]["pid"], 42);
        assert_eq!(meta["source"]["device_name"], "Speakers");
        assert_eq!(meta["status"], "recording");
        assert!(meta["ended_at"].is_null());
    }

    #[test]
    fn append_write_errors_are_propagated() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = writer(dir.path(), 35);
        // A read-only descriptor deterministically exercises the I/O failure.
        writer.live = SegmentWriter(BufWriter::new(
            File::open(writer.path().join("live.jsonl")).unwrap(),
        ));
        assert!(writer
            .append(TranscriptPass::Live, 0, 1000, "must not be acknowledged")
            .is_err());
        assert_eq!(
            std::fs::read_to_string(writer.path().join("live.jsonl")).unwrap(),
            ""
        );
    }

    #[test]
    fn creation_and_finalization_errors_are_reported_without_replacing_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let blocked = dir.path().join("not-a-directory");
        std::fs::write(&blocked, "occupied").unwrap();
        assert!(SessionWriter::create(
            &blocked,
            1,
            1,
            &CaptureSource::Mic {
                device_name: String::new()
            },
            "en",
            "live",
            "refine"
        )
        .is_err());
        let mut writer = writer(dir.path(), 40);
        assert!(writer.finalize("recording").is_err());
        std::fs::write(writer.path().join("transcript.txt"), "previous export\n").unwrap();
        std::fs::write(writer.path().join("transcript.jsonl"), "invalid json\n").unwrap();
        assert!(writer.finalize("stopped").is_err());
        assert_eq!(
            std::fs::read_to_string(writer.path().join("transcript.txt")).unwrap(),
            "previous export\n"
        );
        let meta: serde_json::Value =
            serde_json::from_slice(&std::fs::read(writer.path().join("meta.json")).unwrap())
                .unwrap();
        assert_eq!(meta["status"], "recording");
        std::fs::write(writer.path().join("transcript.jsonl"), "").unwrap();
        writer.finalize("failed").unwrap();
        assert_eq!(
            std::fs::read_to_string(writer.path().join("transcript.txt")).unwrap(),
            ""
        );
    }
}
