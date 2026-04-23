use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serial_test::serial;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_tungstenite::{accept_async, tungstenite::Message};

use voice_bird::transcription::{
    assemblyai_engine::AssemblyAiEngine, EngineConfig, EngineEvent, TranscriptionEngine,
};

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

/// Spawn a local ws:// server that accepts one client, immediately sends
/// the provided JSON messages, then closes. Returns the bound address.
async fn spawn_mock_server(messages: Vec<String>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();
        for m in messages {
            ws.send(Message::Text(m.into())).await.unwrap();
        }
        let _ = ws.close(None).await;
    });
    addr
}

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

#[tokio::test]
#[serial]
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

#[tokio::test]
#[serial]
async fn forwards_pcm_as_i16_binary_frames() {
    let (addr, frames) = record_binary_frames().await;
    unsafe {
        std::env::set_var(
            "ASSEMBLYAI_WS_URL_OVERRIDE",
            format!("ws://{}/v3/ws", addr),
        );
    }

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

#[tokio::test]
#[serial]
async fn turn_partial_maps_to_tentative_and_final_maps_to_committed() {
    let addr = spawn_mock_server(vec![
        r#"{"type":"Begin","session_id":"s1","expires_at":0}"#.into(),
        r#"{"type":"Turn","transcript":"hel","end_of_turn":false,"turn_is_formatted":false,"audio_start_ms":0,"audio_end_ms":500}"#.into(),
        r#"{"type":"Turn","transcript":"hello world","end_of_turn":true,"turn_is_formatted":true,"audio_start_ms":0,"audio_end_ms":1200}"#.into(),
    ])
    .await;
    unsafe {
        std::env::set_var(
            "ASSEMBLYAI_WS_URL_OVERRIDE",
            format!("ws://{}/v3/ws", addr),
        );
    }

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
#[serial]
async fn error_message_maps_to_engine_error() {
    let addr = spawn_mock_server(vec![
        r#"{"type":"Error","error":"auth failed"}"#.into(),
    ])
    .await;
    unsafe {
        std::env::set_var(
            "ASSEMBLYAI_WS_URL_OVERRIDE",
            format!("ws://{}/v3/ws", addr),
        );
    }

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
