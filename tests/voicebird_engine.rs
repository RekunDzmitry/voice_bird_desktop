use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serial_test::serial;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_tungstenite::{accept_async, tungstenite::Message};

use voice_bird::transcription::{
    voicebird_engine::VoiceBirdEngine, EngineConfig, EngineEvent, TranscriptionEngine,
};

const TEST_URL: &str = "wss://example.test/api/audio/stream";

/// Spawn a mock voice_bird_web server. The server consumes the client's
/// `init` JSON, optionally replies with a fixed sequence of JSON messages,
/// then records every binary frame the client sends. Returns the bound
/// address and a handle to the recorded frames.
async fn spawn_recording_server(
    after_init: Vec<String>,
) -> (SocketAddr, Arc<Mutex<Vec<Vec<u8>>>>, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let frames: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let texts: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let frames_clone = frames.clone();
    let texts_clone = texts.clone();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();

        // Send the connection ack the real server opens with.
        let _ = ws
            .send(Message::Text(
                r#"{"type":"connected","message":"hi"}"#.into(),
            ))
            .await;

        // Wait for the client's init message.
        if let Some(Ok(Message::Text(t))) = ws.next().await {
            texts_clone.lock().await.push(t);
        }

        for m in after_init {
            let _ = ws.send(Message::Text(m)).await.ok();
        }

        while let Some(Ok(m)) = ws.next().await {
            match m {
                Message::Binary(b) => frames_clone.lock().await.push(b),
                Message::Text(t) => texts_clone.lock().await.push(t),
                _ => {}
            }
        }
    });
    (addr, frames, texts)
}

/// Spawn a server that just feeds the client a sequence of JSON messages
/// after consuming `init`, then closes.
async fn spawn_mock_server(after_init: Vec<String>) -> SocketAddr {
    let (addr, _frames, _texts) = spawn_recording_server(after_init).await;
    addr
}

#[test]
fn rejects_local_variant() {
    let mut e = VoiceBirdEngine::new("vb-x".into(), TEST_URL.into());
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
    let mut e = VoiceBirdEngine::new(String::new(), TEST_URL.into());
    let err = e
        .start(EngineConfig::Cloud {
            api_key: String::new(),
            language: None,
            sample_rate: 16_000,
            server_url: TEST_URL.into(),
            device_name: "test-device".into(),
            app_name: String::new(),
        })
        .err()
        .expect("expected Err on empty key");
    assert!(err.to_string().contains("api_key"));
}

#[test]
fn rejects_non_16khz_sample_rate() {
    let mut e = VoiceBirdEngine::new("vb-x".into(), TEST_URL.into());
    let err = e
        .start(EngineConfig::Cloud {
            api_key: "vb-x".into(),
            language: None,
            sample_rate: 44_100,
            server_url: TEST_URL.into(),
            device_name: "test-device".into(),
            app_name: String::new(),
        })
        .err()
        .expect("expected Err on wrong sample rate");
    assert!(err.to_string().contains("16 kHz"));
}

#[tokio::test]
#[serial]
async fn emits_model_loaded_on_init_success() {
    let addr = spawn_mock_server(vec![
        r#"{"type":"init_success","session_id":"s1","message":"ok","transcription_available":true}"#.into(),
    ])
    .await;
    unsafe {
        std::env::set_var(
            "VOICEBIRD_WS_URL_OVERRIDE",
            format!("ws://{}/api/audio/stream", addr),
        );
    }

    let mut e = VoiceBirdEngine::new("vb-x".into(), TEST_URL.into());
    let handle = e
        .start(EngineConfig::Cloud {
            api_key: "vb-x".into(),
            language: None,
            sample_rate: 16_000,
            server_url: TEST_URL.into(),
            device_name: "test-device".into(),
            app_name: String::new(),
        })
        .unwrap();

    let mut rx = handle.events_rx;
    let ev: EngineEvent = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    match ev {
        EngineEvent::ModelLoaded { name } => {
            assert!(name.contains("voice-bird"), "got: {name}");
        }
        other => panic!("expected ModelLoaded, got {other:?}"),
    }
}

#[tokio::test]
#[serial]
async fn forwards_pcm_as_i16_binary_frames() {
    let (addr, frames, texts) = spawn_recording_server(vec![
        r#"{"type":"init_success","session_id":"s1","transcription_available":true}"#.into(),
    ])
    .await;
    unsafe {
        std::env::set_var(
            "VOICEBIRD_WS_URL_OVERRIDE",
            format!("ws://{}/api/audio/stream", addr),
        );
    }

    let mut e = VoiceBirdEngine::new("vb-x".into(), TEST_URL.into());
    let handle = e
        .start(EngineConfig::Cloud {
            api_key: "vb-x".into(),
            language: None,
            sample_rate: 16_000,
            server_url: TEST_URL.into(),
            device_name: "test-device".into(),
            app_name: String::new(),
        })
        .unwrap();

    // Wait for ModelLoaded so we know the WS is up and init_success arrived.
    let mut rx = handle.events_rx;
    let _ev: Option<Result<EngineEvent, _>> =
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .ok();

    // Send 1 second of silence as f32, expect ~20 frames of 800 samples.
    let chunk = vec![0.0_f32; 16_000];
    handle.pcm_tx.send(chunk).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    drop(handle.pcm_tx);
    let _ = handle.shutdown.send(());
    tokio::time::sleep(Duration::from_millis(200)).await;

    let frames = frames.lock().await;
    let texts = texts.lock().await;
    assert!(!frames.is_empty(), "no audio frames received");
    let total_bytes: usize = frames.iter().map(|f| f.len()).sum();
    // 16000 samples * 2 bytes = 32000 bytes of i16 PCM.
    assert_eq!(total_bytes, 32_000, "unexpected total bytes: {total_bytes}");
    for f in frames.iter() {
        assert_eq!(f.len() % 2, 0, "frame length not a multiple of 2");
        assert!(f.len() <= 1600, "frame too large: {}", f.len());
    }

    // The first text message must be the init handshake with our api key.
    let init_msg = texts.first().expect("expected init text message");
    assert!(init_msg.contains("\"type\":\"init\""));
    assert!(init_msg.contains("\"api_key\":\"vb-x\""));
    assert!(init_msg.contains("\"sample_rate\":16000"));
}

#[tokio::test]
#[serial]
async fn transcript_partial_maps_to_tentative_and_final_to_committed() {
    let addr = spawn_mock_server(vec![
        r#"{"type":"init_success","session_id":"s1","transcription_available":true}"#.into(),
        r#"{"type":"transcript","text":"hel","is_final":false,"audio_start_ms":0,"audio_end_ms":500}"#.into(),
        r#"{"type":"transcript","text":"hello world","is_final":true,"audio_start_ms":0,"audio_end_ms":1200}"#.into(),
    ])
    .await;
    unsafe {
        std::env::set_var(
            "VOICEBIRD_WS_URL_OVERRIDE",
            format!("ws://{}/api/audio/stream", addr),
        );
    }

    let mut e = VoiceBirdEngine::new("vb-x".into(), TEST_URL.into());
    let handle = e
        .start(EngineConfig::Cloud {
            api_key: "vb-x".into(),
            language: None,
            sample_rate: 16_000,
            server_url: TEST_URL.into(),
            device_name: "test-device".into(),
            app_name: String::new(),
        })
        .unwrap();
    let mut rx = handle.events_rx;

    let mut saw_model = false;
    let mut saw_tentative = false;
    let mut saw_committed = false;
    for _ in 0..6 {
        let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await else {
            break;
        };
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
    assert!(
        saw_model && saw_tentative && saw_committed,
        "events: model={saw_model} tentative={saw_tentative} committed={saw_committed}"
    );
}

#[tokio::test]
#[serial]
async fn shutdown_sends_terminate_text_message() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let texts = Arc::new(Mutex::new(Vec::<String>::new()));
    let texts_clone = texts.clone();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();
        let _ = ws
            .send(Message::Text(r#"{"type":"connected"}"#.into()))
            .await;
        // Consume init.
        if let Some(Ok(Message::Text(t))) = ws.next().await {
            texts_clone.lock().await.push(t);
        }
        let _ = ws
            .send(Message::Text(
                r#"{"type":"init_success","session_id":"s","transcription_available":true}"#.into(),
            ))
            .await;
        while let Some(Ok(m)) = ws.next().await {
            if let Message::Text(t) = m {
                texts_clone.lock().await.push(t);
            }
        }
    });
    unsafe {
        std::env::set_var(
            "VOICEBIRD_WS_URL_OVERRIDE",
            format!("ws://{}/api/audio/stream", addr),
        );
    }

    let mut e = VoiceBirdEngine::new("vb-x".into(), TEST_URL.into());
    let handle = e
        .start(EngineConfig::Cloud {
            api_key: "vb-x".into(),
            language: None,
            sample_rate: 16_000,
            server_url: TEST_URL.into(),
            device_name: "test-device".into(),
            app_name: String::new(),
        })
        .unwrap();
    let mut rx = handle.events_rx;
    let _ev: Option<Result<EngineEvent, _>> =
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .ok();

    let _ = handle.shutdown.send(());
    tokio::time::sleep(Duration::from_millis(300)).await;

    let texts = texts.lock().await;
    assert!(
        texts.iter().any(|t| t.contains("\"type\":\"terminate\"")),
        "terminate not sent; got: {:?}",
        *texts
    );
}

#[tokio::test]
#[serial]
async fn error_message_maps_to_engine_error() {
    let addr = spawn_mock_server(vec![
        r#"{"type":"error","message":"auth failed","code":"INVALID_API_KEY"}"#.into(),
    ])
    .await;
    unsafe {
        std::env::set_var(
            "VOICEBIRD_WS_URL_OVERRIDE",
            format!("ws://{}/api/audio/stream", addr),
        );
    }

    let mut e = VoiceBirdEngine::new("vb-x".into(), TEST_URL.into());
    let handle = e
        .start(EngineConfig::Cloud {
            api_key: "vb-x".into(),
            language: None,
            sample_rate: 16_000,
            server_url: TEST_URL.into(),
            device_name: "test-device".into(),
            app_name: String::new(),
        })
        .unwrap();
    let mut rx = handle.events_rx;
    let ev: EngineEvent = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    match ev {
        EngineEvent::Error(msg) => assert!(msg.contains("auth failed"), "got: {msg}"),
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
#[serial]
async fn init_success_with_transcription_unavailable_raises_engine_error() {
    let addr = spawn_mock_server(vec![
        r#"{"type":"init_success","session_id":"s","transcription_available":false,"transcription_error":"upstream down"}"#.into(),
    ])
    .await;
    unsafe {
        std::env::set_var(
            "VOICEBIRD_WS_URL_OVERRIDE",
            format!("ws://{}/api/audio/stream", addr),
        );
    }

    let mut e = VoiceBirdEngine::new("vb-x".into(), TEST_URL.into());
    let handle = e
        .start(EngineConfig::Cloud {
            api_key: "vb-x".into(),
            language: None,
            sample_rate: 16_000,
            server_url: TEST_URL.into(),
            device_name: "test-device".into(),
            app_name: String::new(),
        })
        .unwrap();
    let mut rx = handle.events_rx;
    let ev: EngineEvent = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    match ev {
        EngineEvent::Error(msg) => assert!(msg.contains("upstream down"), "got: {msg}"),
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
#[serial]
async fn init_message_carries_device_and_app_names() {
    // Regression: the init handshake must surface both `device_name` and
    // `app_name` from EngineConfig::Cloud verbatim so voicebird.app can
    // group sessions by (device, app) — pre-fix, app_name was hardcoded
    // to the crate name.
    let (addr, _frames, texts) = spawn_recording_server(vec![
        r#"{"type":"init_success","session_id":"s1","transcription_available":true}"#.into(),
    ])
    .await;
    unsafe {
        std::env::set_var(
            "VOICEBIRD_WS_URL_OVERRIDE",
            format!("ws://{}/api/audio/stream", addr),
        );
    }

    let mut e = VoiceBirdEngine::new("vb-x".into(), TEST_URL.into());
    let handle = e
        .start(EngineConfig::Cloud {
            api_key: "vb-x".into(),
            language: None,
            sample_rate: 16_000,
            server_url: TEST_URL.into(),
            device_name: "EPOS PC 8 USB".into(),
            app_name: "Chrome".into(),
        })
        .unwrap();

    // Wait for the handshake to land before snapshotting captured texts.
    let mut rx = handle.events_rx;
    let _ = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await;

    let texts = texts.lock().await;
    let init = texts
        .first()
        .expect("expected init JSON to be captured by mock server");
    let parsed: serde_json::Value =
        serde_json::from_str(init).expect("init must be valid JSON");
    assert_eq!(parsed["type"], "init", "got: {init}");
    assert_eq!(parsed["device_name"], "EPOS PC 8 USB", "got: {init}");
    assert_eq!(parsed["app_name"], "Chrome", "got: {init}");
}
