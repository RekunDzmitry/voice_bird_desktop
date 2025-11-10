// Test script to verify WebSocket compression is disabled
// Run with: cargo run --bin test_compression

use anyhow::Result;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::connect_async_with_config;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use http::header::HeaderValue;

#[tokio::main]
async fn main() -> Result<()> {
    println!("🔬 WebSocket Compression Test");
    println!("═══════════════════════════════════════════════════════════");

    // Your server URL
    let server_url = std::env::var("SERVER_URL")
        .unwrap_or_else(|_| "ws://localhost:3000".to_string());
    let api_key = std::env::var("API_KEY")
        .unwrap_or_else(|_| "test-key".to_string());

    let ws_url = format!("{}/api/audio/stream", server_url);

    println!("📡 Connecting to: {}", ws_url);
    println!();

    // Create request
    let mut request = ws_url.into_client_request()?;

    // Add auth header
    request.headers_mut().insert(
        "Authorization",
        HeaderValue::from_str(&api_key)?
    );

    // Try to remove compression header (may or may not be present yet)
    request.headers_mut().remove("Sec-WebSocket-Extensions");
    println!("🔧 Attempted to remove Sec-WebSocket-Extensions header");
    println!();

    // Check what headers are being sent
    println!("📤 Request Headers:");
    for (name, value) in request.headers() {
        println!("   {}: {:?}", name, value);
    }
    println!();

    // Configure WebSocket
    let ws_config = WebSocketConfig {
        max_message_size: Some(10 * 1024 * 1024),
        max_frame_size: Some(10 * 1024 * 1024),
        max_write_buffer_size: 10 * 1024 * 1024,
        accept_unmasked_frames: false,
        ..Default::default()
    };

    println!("⚙️  WebSocket Config:");
    println!("   Max message size: {} bytes", ws_config.max_message_size.unwrap_or(0));
    println!("   Max frame size: {} bytes", ws_config.max_frame_size.unwrap_or(0));
    println!("   Write buffer: {} bytes", ws_config.max_write_buffer_size);
    println!();

    // Try to connect
    match connect_async_with_config(request, Some(ws_config), false).await {
        Ok((ws_stream, response)) => {
            println!("✅ Connection successful!");
            println!();
            println!("📥 Response Headers:");

            for (name, value) in response.headers() {
                println!("   {}: {:?}", name, value);
            }
            println!();

            // Check for compression extension
            if let Some(extensions) = response.headers().get("Sec-WebSocket-Extensions") {
                println!("⚠️  COMPRESSION DETECTED!");
                println!("   Extensions header present: {:?}", extensions);
                println!();
                println!("❌ TEST FAILED: Compression was negotiated");
                println!("   This means default-features = false didn't disable compression");
            } else {
                println!("✅ TEST PASSED: No compression extensions negotiated");
                println!("   The client and server are both compression-free");
            }

            // Check protocol
            if let Some(protocol) = response.headers().get("Sec-WebSocket-Protocol") {
                println!();
                println!("📋 Protocol: {:?}", protocol);
            }

            println!();
            println!("🔍 HTTP Status: {}", response.status());

            drop(ws_stream);
        }
        Err(e) => {
            println!("❌ Connection failed: {}", e);
            println!();
            println!("Error details: {:?}", e);
        }
    }

    Ok(())
}
