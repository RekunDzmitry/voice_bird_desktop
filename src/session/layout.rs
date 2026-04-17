use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub enum SessionSource {
    Microphone,
    System,
    App(String),
}

pub fn session_slug(ts: chrono::DateTime<chrono::Utc>, source: &SessionSource) -> String {
    let ts = ts.format("%Y-%m-%d_%H-%M-%S");
    let src = match source {
        SessionSource::Microphone => "mic".to_string(),
        SessionSource::System => "system".to_string(),
        SessionSource::App(name) => normalize_app_name(name),
    };
    format!("{}-{}", ts, src)
}

pub fn session_dir(
    base: &Path,
    ts: chrono::DateTime<chrono::Utc>,
    source: &SessionSource,
) -> PathBuf {
    base.join(session_slug(ts, source))
}

fn normalize_app_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn slug_uses_timestamp_and_source() {
        let ts = chrono::Utc.with_ymd_and_hms(2026, 4, 16, 14, 32, 7).unwrap();
        let s = session_slug(ts, &SessionSource::App("Zoom".into()));
        assert_eq!(s, "2026-04-16_14-32-07-zoom");
    }

    #[test]
    fn slug_normalizes_app_name() {
        let ts = chrono::Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
        let s = session_slug(ts, &SessionSource::App("Google Chrome Helper".into()));
        assert_eq!(s, "2026-01-02_03-04-05-google-chrome-helper");
    }

    #[test]
    fn slug_for_mic_and_system() {
        let ts = chrono::Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
        assert_eq!(session_slug(ts, &SessionSource::Microphone), "2026-01-02_03-04-05-mic");
        assert_eq!(session_slug(ts, &SessionSource::System),     "2026-01-02_03-04-05-system");
    }

    #[test]
    fn session_dir_joins_base_and_slug() {
        let ts = chrono::Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
        let root = std::path::PathBuf::from("/tmp/voice-bird/sessions");
        let dir = session_dir(&root, ts, &SessionSource::Microphone);
        assert_eq!(dir, std::path::PathBuf::from("/tmp/voice-bird/sessions/2026-01-02_03-04-05-mic"));
    }
}
