//! HTTP helpers for talking to voicebird.app from the desktop.
//!
//! The desktop learns the voicebird.app endpoint from
//! `AppConfig::voicebird_server_url` — a WebSocket URL the user
//! (or default config) configured for cloud recording. From that
//! single value the HTTP client here derives the REST base URL
//! used by the character-run path (`POST /api/character-runs`).

/// Derive the HTTP(S) REST base URL from a `ws://` or `wss://`
/// stream URL.
///
/// Examples (from the §3 unit-test contract):
/// - `wss://voicebird.app/api/audio/stream` → `https://voicebird.app`
/// - `ws://127.0.0.1:3000` → `http://127.0.0.1:3000`
/// - `ws://localhost:9999/api/audio/stream` → `http://localhost:9999`
/// - `voicebird.app` (no scheme) → `https://voicebird.app`
/// - `wss://voicebird.app:8080/path` → `https://voicebird.app:8080`
/// - `wss://voicebird.app` → `https://voicebird.app`
///
/// The path component of the input is dropped — callers compose
/// their own REST path on top of the returned base (e.g.
/// `format!("{base}/api/character-runs")`).
pub fn rest_base_url(ws_url: &str) -> String {
    let (scheme, rest) = if let Some(r) = ws_url.strip_prefix("wss://") {
        ("https", r)
    } else if let Some(r) = ws_url.strip_prefix("ws://") {
        ("http", r)
    } else {
        // No scheme: default to https (production voicebird.app is TLS).
        ("https", ws_url)
    };

    // The path component of the WS URL is dropped; only the
    // origin (scheme + host + port) survives.
    let host_port = rest.split('/').next().unwrap_or(rest);
    format!("{scheme}://{host_port}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wss_with_path() {
        assert_eq!(
            rest_base_url("wss://voicebird.app/api/audio/stream"),
            "https://voicebird.app"
        );
    }

    #[test]
    fn ws_with_port_produces_http() {
        assert_eq!(rest_base_url("ws://127.0.0.1:3000"), "http://127.0.0.1:3000");
    }

    #[test]
    fn ws_with_path_produces_http() {
        assert_eq!(
            rest_base_url("ws://localhost:9999/api/audio/stream"),
            "http://localhost:9999"
        );
    }

    #[test]
    fn no_scheme_falls_through_to_https() {
        assert_eq!(rest_base_url("voicebird.app"), "https://voicebird.app");
    }

    #[test]
    fn preserves_port() {
        assert_eq!(
            rest_base_url("wss://voicebird.app:8080/path"),
            "https://voicebird.app:8080"
        );
    }

    #[test]
    fn bare_host() {
        assert_eq!(rest_base_url("wss://voicebird.app"), "https://voicebird.app");
    }
}
