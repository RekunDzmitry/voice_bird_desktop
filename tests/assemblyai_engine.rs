use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::{accept_async, tungstenite::Message};

use voice_bird::transcription::{
    assemblyai_engine::AssemblyAiEngine, EngineConfig, EngineEvent, TranscriptionEngine,
};

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
