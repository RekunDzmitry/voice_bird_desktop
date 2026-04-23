use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, mpsc, oneshot};

use super::{EngineConfig, EngineEvent, EngineHandle, Segment, Token, TranscriptionEngine};

/// Events emitted by the Swift sidecar over its stdout as one JSON object
/// per line. `type` is used as the tag (serde `tag = "type"`).
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum SidecarEvent {
    Ready {
        model: String,
    },
    Committed {
        t0: f64,
        t1: f64,
        text: String,
    },
    Tentative {
        text: String,
    },
    Error {
        message: String,
    },
}

/// Rust client for the `voice-bird-whisperkit` Swift sidecar. The engine
/// spawns the binary, writes a JSON handshake line, then pushes PCM in
/// length-prefixed binary frames and parses line-delimited JSON events from
/// stdout.
pub struct WhisperKitEngine {
    sidecar_path: PathBuf,
}

impl WhisperKitEngine {
    pub fn new(sidecar_path: PathBuf) -> Self {
        Self { sidecar_path }
    }
}

impl TranscriptionEngine for WhisperKitEngine {
    fn start(&mut self, cfg: EngineConfig) -> anyhow::Result<EngineHandle> {
        let (pcm_tx, mut pcm_rx) = mpsc::channel::<Vec<f32>>(32);
        let (events_tx, events_rx) = broadcast::channel::<EngineEvent>(256);
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();

        // Spawn the sidecar with piped stdio. `kill_on_drop(true)` ensures
        // we do not leak a zombie child if the handle owning `child` is
        // dropped unexpectedly (panic, unwind).
        let mut child: Child = Command::new(&self.sidecar_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| anyhow::anyhow!("spawn sidecar {:?}: {e}", self.sidecar_path))?;

        // Take stdin/stdout exactly once. `Child.stdin` is
        // `Option<ChildStdin>` in tokio 1.x; taking it moves ownership out
        // of the child so the I/O tasks below can `.await` on writes /
        // reads without borrowing the `Child` struct.
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("sidecar stdin not captured"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("sidecar stdout not captured"))?;

        let (model_path, language, _sample_rate) = match cfg {
            EngineConfig::Local {
                model_path,
                language,
                sample_rate,
                ..
            } => (model_path, language, sample_rate),
        };

        // --- Handshake line: JSON with model + language --------------------
        // The Swift side decodes `[String: String]`, so we serialize a flat
        // map (no nested structs). model_path is interpreted by WhisperKit
        // as a model id ("tiny.en", "distil-small.en", etc.) rather than a
        // gguf file path — engine-selection code is expected to translate
        // before calling `start`.
        let handshake = serde_json::json!({
            "model": model_path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| model_path.to_string_lossy().into_owned()),
            "language": language.clone().unwrap_or_else(|| "auto".into()),
        });
        let mut handshake_bytes = serde_json::to_vec(&handshake)
            .map_err(|e| anyhow::anyhow!("serialize handshake: {e}"))?;
        handshake_bytes.push(b'\n');

        // --- Producer task: handshake + PCM frames -------------------------
        // Plan deviation: the plan sketches two tasks that both `take()`
        // stdin; only one can succeed. We consolidate into a single task
        // that (a) writes the handshake line, (b) pulls PCM chunks and
        // writes them as 4-byte LE length + f32 little-endian samples,
        // matching the Swift `Protocol.swift` format.
        let events_tx_producer = events_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = stdin.write_all(&handshake_bytes).await {
                let _ = events_tx_producer
                    .send(EngineEvent::Error(format!("sidecar handshake write: {e}")));
                return;
            }
            if let Err(e) = stdin.flush().await {
                let _ = events_tx_producer
                    .send(EngineEvent::Error(format!("sidecar handshake flush: {e}")));
                return;
            }

            while let Some(chunk) = pcm_rx.recv().await {
                let n = chunk.len() as u32;
                let header = n.to_le_bytes();
                // bytemuck::cast_slice reinterprets &[f32] as &[u8] without
                // copying — on little-endian hosts this matches the Swift
                // side's `Array(ptr.bindMemory(to: Float.self))`.
                let body: &[u8] = bytemuck::cast_slice(&chunk);
                if let Err(e) = stdin.write_all(&header).await {
                    let _ = events_tx_producer
                        .send(EngineEvent::Error(format!("sidecar stdin write len: {e}")));
                    return;
                }
                if let Err(e) = stdin.write_all(body).await {
                    let _ = events_tx_producer
                        .send(EngineEvent::Error(format!("sidecar stdin write pcm: {e}")));
                    return;
                }
                if let Err(e) = stdin.flush().await {
                    let _ = events_tx_producer
                        .send(EngineEvent::Error(format!("sidecar stdin flush: {e}")));
                    return;
                }
            }
            // pcm_tx dropped — close stdin so the Swift side exits its
            // read loop and terminates cleanly. Dropping `stdin` here does
            // exactly that.
            drop(stdin);
        });

        // --- Consumer task: stdout JSONL → EngineEvent --------------------
        let events_tx_consumer = events_tx.clone();
        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    line = lines.next_line() => {
                        match line {
                            Ok(Some(line)) => {
                                if line.trim().is_empty() { continue; }
                                match serde_json::from_str::<SidecarEvent>(&line) {
                                    Ok(SidecarEvent::Ready { model }) => {
                                        let _ = events_tx_consumer
                                            .send(EngineEvent::ModelLoaded { name: model });
                                    }
                                    Ok(SidecarEvent::Committed { t0, t1, text }) => {
                                        let t_start = Duration::from_secs_f64(t0.max(0.0));
                                        let t_end = Duration::from_secs_f64(t1.max(t0).max(0.0));
                                        let seg = Segment {
                                            t_start,
                                            t_end,
                                            // WhisperKit does not emit
                                            // per-token timing in the
                                            // current protocol; we build
                                            // a single synthetic token
                                            // covering the whole segment
                                            // so downstream consumers that
                                            // inspect `tokens` still work.
                                            tokens: vec![Token {
                                                text: text.clone(),
                                                t_start_ms: t_start.as_millis() as u64,
                                                t_end_ms: t_end.as_millis() as u64,
                                            }],
                                            text,
                                        };
                                        let _ = events_tx_consumer
                                            .send(EngineEvent::Committed(seg));
                                    }
                                    Ok(SidecarEvent::Tentative { text }) => {
                                        let _ = events_tx_consumer
                                            .send(EngineEvent::Tentative(text));
                                    }
                                    Ok(SidecarEvent::Error { message }) => {
                                        let _ = events_tx_consumer
                                            .send(EngineEvent::Error(message));
                                    }
                                    Err(e) => {
                                        let _ = events_tx_consumer.send(EngineEvent::Error(
                                            format!("sidecar jsonl parse: {e}: {line}"),
                                        ));
                                    }
                                }
                            }
                            Ok(None) => break,
                            Err(e) => {
                                let _ = events_tx_consumer
                                    .send(EngineEvent::Error(format!("sidecar stdout read: {e}")));
                                break;
                            }
                        }
                    }
                }
            }
        });

        // --- Child lifetime --------------------------------------------------
        // Keep the `Child` alive for as long as the engine runs. Dropping
        // it would kill the subprocess (and `kill_on_drop(true)` makes that
        // guarantee explicit). We spawn a supervisor task that owns the
        // child and waits for it to exit, emitting an error if the sidecar
        // dies unexpectedly. This is simpler than extending `EngineHandle`
        // to carry the child — the handle's `shutdown` oneshot already
        // stops the consumer, and dropping `pcm_tx` closes stdin and lets
        // the Swift process exit cleanly.
        let events_tx_child = events_tx.clone();
        tokio::spawn(async move {
            match child.wait().await {
                Ok(status) if status.success() => {}
                Ok(status) => {
                    let _ = events_tx_child.send(EngineEvent::Error(format!(
                        "sidecar exited with status {status}"
                    )));
                }
                Err(e) => {
                    let _ = events_tx_child
                        .send(EngineEvent::Error(format!("sidecar wait: {e}")));
                }
            }
        });

        // events_tx is held by the three spawned tasks above; the handle
        // subscribes via the receiver we already created with `channel()`.
        let _ = events_tx; // (documentation: explicitly kept by the tasks)

        Ok(EngineHandle {
            pcm_tx,
            events_rx,
            shutdown: shutdown_tx,
        })
    }
}
