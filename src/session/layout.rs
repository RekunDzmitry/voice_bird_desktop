use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionSource {
    Microphone,
    System,
    App {
        id: String,
        name: String,
        device_name: String,
    },
}

pub fn session_slug(ts: chrono::DateTime<chrono::Utc>, source: &SessionSource) -> String {
    let ts = ts.format("%Y-%m-%d_%H-%M-%S");
    let src = match source {
        SessionSource::Microphone => "mic".to_string(),
        SessionSource::System => "system".to_string(),
        SessionSource::App {
            name, device_name, ..
        } => {
            let app = normalize_slug(name);
            let dev = normalize_slug(device_name);
            if dev.is_empty() {
                app
            } else {
                format!("{app}-on-{dev}")
            }
        }
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

/// Build the per-role subdir inside a room session. The room
/// session layout is `<ts>-room-<slug>/<role_slug>/<ts2>-<source>`
/// — each role's section lives under its role's directory so
/// the standard per-role finalize path (SessionWriter,
/// meta.json, transcript.jsonl) works unchanged.
pub fn room_role_dir(
    base: &Path,
    ts: chrono::DateTime<chrono::Utc>,
    room_slug: &str,
    role_slug: &str,
) -> PathBuf {
    let parent = room_session_dir(base, ts, room_slug);
    parent.join(normalize_slug(role_slug))
}

/// Build a room session directory. The slug encodes the start
/// time + room slug so the per-role children can inherit the
/// timestamp. The dir hosts `room.json`, `room-transcript.jsonl`,
/// `context.md`, and one subdir per role.
pub fn room_session_dir(
    base: &Path,
    ts: chrono::DateTime<chrono::Utc>,
    room_slug: &str,
) -> PathBuf {
    let ts = ts.format("%Y-%m-%d_%H-%M-%S");
    base.join(format!("{ts}-room-{}", normalize_slug(room_slug)))
}

/// Test helper: the per-role session dir produced from a
/// `room_session_dir` (used by `room_role_dir` above). Exposed
/// so tests can assert the full path layout without rebuilding
/// the format string.
pub fn room_role_session_dir(
    room_base: &Path,
    role_slug: &str,
    ts: chrono::DateTime<chrono::Utc>,
    source: &SessionSource,
) -> PathBuf {
    room_base
        .join(normalize_slug(role_slug))
        .join(session_slug(ts, source))
}
fn normalize_slug(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
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

    fn app_zoom_on(device: &str) -> SessionSource {
        SessionSource::App {
            id: "us.zoom.xos".into(),
            name: "Zoom".into(),
            device_name: device.into(),
        }
    }

    #[test]
    fn slug_uses_timestamp_and_source() {
        let ts = chrono::Utc
            .with_ymd_and_hms(2026, 4, 16, 14, 32, 7)
            .unwrap();
        let s = session_slug(ts, &app_zoom_on("MacBook Pro Speakers"));
        assert_eq!(s, "2026-04-16_14-32-07-zoom-on-macbook-pro-speakers");
    }

    #[test]
    fn slug_normalizes_app_and_device_name() {
        let ts = chrono::Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
        let s = session_slug(
            ts,
            &SessionSource::App {
                id: "com.google.Chrome.helper".into(),
                name: "Google Chrome Helper".into(),
                device_name: "AirPods Pro (3rd gen)".into(),
            },
        );
        assert_eq!(
            s,
            "2026-01-02_03-04-05-google-chrome-helper-on-airpods-pro-3rd-gen"
        );
    }

    #[test]
    fn slug_for_app_without_device_name() {
        let ts = chrono::Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
        let s = session_slug(
            ts,
            &SessionSource::App {
                id: "us.zoom.xos".into(),
                name: "Zoom".into(),
                device_name: String::new(),
            },
        );
        assert_eq!(s, "2026-01-02_03-04-05-zoom");
    }

    #[test]
    fn slug_for_mic_and_system() {
        let ts = chrono::Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
        assert_eq!(
            session_slug(ts, &SessionSource::Microphone),
            "2026-01-02_03-04-05-mic"
        );
        assert_eq!(
            session_slug(ts, &SessionSource::System),
            "2026-01-02_03-04-05-system"
        );
    }

    #[test]
    fn session_dir_joins_base_and_slug() {
        let ts = chrono::Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
        let root = std::path::PathBuf::from("/tmp/voice-bird/sessions");
        let dir = session_dir(&root, ts, &SessionSource::Microphone);
        assert_eq!(
            dir,
            std::path::PathBuf::from("/tmp/voice-bird/sessions/2026-01-02_03-04-05-mic")
        );
    }

    /// Room session dir layout: `<ts>-room-<slug>/<role_slug>/<ts2>-<source>`.
    /// The per-role child dir inherits the timestamp prefix so
    /// per-role SessionWriter output lands inside it unchanged.
    #[test]
    fn room_session_dir_and_role_dir_layout() {
        let ts = chrono::Utc.with_ymd_and_hms(2026, 5, 13, 10, 0, 0).unwrap();
        let root = std::path::PathBuf::from("/tmp/voice-bird/sessions");
        let base = room_session_dir(&root, ts, "doctor-appointment");
        assert_eq!(
            base,
            std::path::PathBuf::from(
                "/tmp/voice-bird/sessions/2026-05-13_10-00-00-room-doctor-appointment"
            )
        );
        let role = room_role_dir(&root, ts, "doctor-appointment", "doctor");
        assert_eq!(
            role,
            std::path::PathBuf::from(
                "/tmp/voice-bird/sessions/2026-05-13_10-00-00-room-doctor-appointment/doctor"
            )
        );
        let role_session = room_role_session_dir(
            &base,
            "doctor",
            ts,
            &SessionSource::Microphone,
        );
        assert_eq!(
            role_session,
            std::path::PathBuf::from(
                "/tmp/voice-bird/sessions/2026-05-13_10-00-00-room-doctor-appointment/doctor/2026-05-13_10-00-00-mic"
            )
        );
    }

    /// Slug normalization strips characters that aren't safe
    /// for filesystem paths. The room slug flows through
    /// `normalize_slug` so a slug like "Doctor Appointment!"
    /// becomes "doctor-appointment".
    #[test]
    fn room_session_dir_normalizes_slug() {
        let ts = chrono::Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let root = std::path::PathBuf::from("/tmp/voice-bird/sessions");
        let base = room_session_dir(&root, ts, "Doctor Appointment!");
        assert_eq!(
            base,
            std::path::PathBuf::from(
                "/tmp/voice-bird/sessions/2026-01-01_00-00-00-room-doctor-appointment"
            )
        );
    }
}
