use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use crate::app::App;

/// Editable settings fields, in render order. The settings view
/// iterates this slice; section headers are rendered between runs of
/// adjacent fields sharing the same `section` string.
#[derive(Debug, Clone, Copy)]
pub struct Field {
    pub section: &'static str,
    pub key: FieldKey,
    pub label: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKey {
    DefaultModel,
    Language,
    SessionDir,
    AudioDefaultSource,
    InputDevice,
    EnginePrefer,
    HopMs,
    MinWindowMs,
    RefinementModel,
    RefinementWindowMs,
    RefinementBeamSize,
    VoiceBirdApiKey,
    VoiceBirdServerUrl,
    CloudBroadcastEnabled,
}

pub const FIELDS: &[Field] = &[
    Field { section: "General", key: FieldKey::DefaultModel, label: "Default model" },
    Field { section: "General", key: FieldKey::Language, label: "Language" },
    Field { section: "General", key: FieldKey::SessionDir, label: "Session directory" },
    Field { section: "Audio", key: FieldKey::AudioDefaultSource, label: "Default source" },
    Field { section: "Audio", key: FieldKey::InputDevice, label: "Input device" },
    Field { section: "Engine", key: FieldKey::EnginePrefer, label: "Engine preference" },
    Field { section: "Engine", key: FieldKey::HopMs, label: "Hop (ms)" },
    Field { section: "Engine", key: FieldKey::MinWindowMs, label: "Min window (ms)" },
    Field { section: "Refinement (whisper only)", key: FieldKey::RefinementModel, label: "Refinement model" },
    Field { section: "Refinement (whisper only)", key: FieldKey::RefinementWindowMs, label: "Window (ms)" },
    Field { section: "Refinement (whisper only)", key: FieldKey::RefinementBeamSize, label: "Beam size" },
    Field { section: "Cloud", key: FieldKey::VoiceBirdServerUrl, label: "Voice Bird server URL" },
    Field { section: "Cloud", key: FieldKey::VoiceBirdApiKey, label: "Voice Bird API key" },
    Field { section: "Cloud", key: FieldKey::CloudBroadcastEnabled, label: "Live broadcast" },
];

/// How a field is edited in the settings view.
pub enum FieldKind {
    /// Free-text input. Enter opens text edit, Backspace/Char/Enter as today.
    Text,
    /// Cycle through a hardcoded list of values. Left/Right (and h/l) advance
    /// backward/forward; wraps at ends.
    Cycle,
    /// Numeric cycle through a preset list. Same UX as Cycle but the list is
    /// integer values displayed with a unit suffix.
    Numeric,
}

pub fn field_kind(key: FieldKey) -> FieldKind {
    match key {
        FieldKey::SessionDir
        | FieldKey::VoiceBirdApiKey
        | FieldKey::VoiceBirdServerUrl => FieldKind::Text,
        FieldKey::HopMs
        | FieldKey::MinWindowMs
        | FieldKey::RefinementWindowMs
        | FieldKey::RefinementBeamSize => FieldKind::Numeric,
        FieldKey::CloudBroadcastEnabled => FieldKind::Cycle,
        _ => FieldKind::Cycle,
    }
}

/// Public re-export for the debug snapshot writer in main.rs.
/// Returns the same options used by the cycle UX, suffixed with the
/// no-op marker for fields whose semantics include "(none)" / "(off)".
pub fn cycle_options_for(key: FieldKey, app: &App) -> Vec<String> {
    cycle_options(key, app)
}

fn cycle_options(key: FieldKey, app: &App) -> Vec<String> {
    use voice_bird::transcription::models::Catalog;
    match key {
        FieldKey::DefaultModel => Catalog::builtin()
            .all()
            .iter()
            .map(|m| m.id.to_string())
            .collect(),
        FieldKey::Language => vec!["en", "auto", "es", "fr", "de", "ja", "zh"]
            .into_iter()
            .map(String::from)
            .collect(),
        FieldKey::AudioDefaultSource => vec!["microphone".into(), "system".into()],
        FieldKey::InputDevice => {
            // First option is the empty/None marker rendered as "(OS default)"
            // by field_display. Then enumerate currently-known devices from app.sessions.
            let mut v = vec![String::new()];
            for s in &app.sessions {
                v.push(s.device_name.clone());
            }
            v
        }
        FieldKey::EnginePrefer => vec![
            "auto".into(),
            "whisperkit".into(),
            "whisper_rs".into(),
        ],
        FieldKey::CloudBroadcastEnabled => vec!["off".into(), "on".into()],
        FieldKey::RefinementModel => {
            let mut v = vec![String::new()]; // empty = "(off)"
            for m in Catalog::builtin().all() {
                v.push(m.id.to_string());
            }
            v
        }
        // Fall back for unexpected non-cycle fields.
        _ => Vec::new(),
    }
}

fn numeric_options(key: FieldKey) -> Vec<u32> {
    match key {
        FieldKey::HopMs => vec![250, 500, 750, 1000, 1500, 2000],
        FieldKey::MinWindowMs => vec![500, 750, 1000, 1500, 2000],
        FieldKey::RefinementWindowMs => vec![10_000, 15_000, 20_000, 30_000, 60_000],
        FieldKey::RefinementBeamSize => vec![1, 3, 5, 8, 10],
        _ => Vec::new(),
    }
}

fn current_string_value(app: &App, key: FieldKey) -> String {
    let c = &app.config;
    match key {
        FieldKey::DefaultModel => c.default_model.clone(),
        FieldKey::Language => c.language.clone(),
        FieldKey::AudioDefaultSource => c.audio_default_source.clone(),
        FieldKey::InputDevice => c.input_device.clone().unwrap_or_default(),
        FieldKey::EnginePrefer => c.engine_prefer.clone(),
        FieldKey::RefinementModel => c.refinement_model.clone().unwrap_or_default(),
        FieldKey::CloudBroadcastEnabled => {
            if c.cloud_broadcast_enabled { "on".into() } else { "off".into() }
        }
        _ => String::new(),
    }
}

fn current_numeric_value(app: &App, key: FieldKey) -> u32 {
    let c = &app.config;
    match key {
        FieldKey::HopMs => c.hop_ms,
        FieldKey::MinWindowMs => c.min_window_ms,
        FieldKey::RefinementWindowMs => c.refinement_window_ms,
        FieldKey::RefinementBeamSize => c.refinement_beam_size as u32,
        _ => 0,
    }
}

/// Returns the index in `options` of `current`, or 0 if not found.
fn index_of<T: PartialEq>(options: &[T], current: &T) -> usize {
    options.iter().position(|x| x == current).unwrap_or(0)
}

fn cycle_string(app: &mut App, key: FieldKey, delta: i32) {
    let opts = cycle_options(key, app);
    if opts.is_empty() { return; }
    let cur = current_string_value(app, key);
    let i = index_of(&opts, &cur);
    let n = opts.len() as i32;
    let next = (((i as i32 + delta) % n) + n) % n;
    let v = opts[next as usize].clone();
    let c = &mut app.config;
    match key {
        FieldKey::DefaultModel => c.default_model = v,
        FieldKey::Language => c.language = v,
        FieldKey::AudioDefaultSource => c.audio_default_source = v,
        FieldKey::InputDevice => {
            c.input_device = if v.is_empty() { None } else { Some(v) };
        }
        FieldKey::EnginePrefer => c.engine_prefer = v,
        FieldKey::RefinementModel => {
            c.refinement_model = if v.is_empty() { None } else { Some(v) };
        }
        FieldKey::CloudBroadcastEnabled => {
            c.cloud_broadcast_enabled = v == "on";
        }
        _ => {}
    }
    app.settings_error = None;
}

fn cycle_numeric(app: &mut App, key: FieldKey, delta: i32) {
    let opts = numeric_options(key);
    if opts.is_empty() { return; }
    let cur = current_numeric_value(app, key);
    let i = index_of(&opts, &cur);
    let n = opts.len() as i32;
    let next = (((i as i32 + delta) % n) + n) % n;
    let v = opts[next as usize];
    let c = &mut app.config;
    match key {
        FieldKey::HopMs => c.hop_ms = v,
        FieldKey::MinWindowMs => c.min_window_ms = v,
        FieldKey::RefinementWindowMs => c.refinement_window_ms = v,
        FieldKey::RefinementBeamSize => c.refinement_beam_size = v as u8,
        _ => {}
    }
    app.settings_error = None;
}

fn mask_api_key(key: &str) -> String {
    if key.is_empty() {
        "(unset)".into()
    } else if key.len() <= 4 {
        "•".repeat(key.len())
    } else {
        let shown = &key[key.len() - 4..];
        let hidden = "•".repeat(key.len() - 4);
        format!("{hidden}{shown}")
    }
}

pub fn field_display(app: &App, key: FieldKey) -> String {
    let c = &app.config;
    match key {
        FieldKey::DefaultModel => c.default_model.clone(),
        FieldKey::Language => c.language.clone(),
        FieldKey::SessionDir => c.session_dir.clone(),
        FieldKey::AudioDefaultSource => c.audio_default_source.clone(),
        FieldKey::InputDevice => c
            .input_device
            .clone()
            .unwrap_or_else(|| "(OS default)".into()),
        FieldKey::EnginePrefer => c.engine_prefer.clone(),
        FieldKey::HopMs => c.hop_ms.to_string(),
        FieldKey::MinWindowMs => c.min_window_ms.to_string(),
        FieldKey::RefinementModel => c
            .refinement_model
            .clone()
            .unwrap_or_else(|| "(off)".into()),
        FieldKey::RefinementWindowMs => c.refinement_window_ms.to_string(),
        FieldKey::RefinementBeamSize => c.refinement_beam_size.to_string(),
        FieldKey::VoiceBirdApiKey => format!("{} [plaintext]", mask_api_key(&c.voicebird_api_key)),
        FieldKey::VoiceBirdServerUrl => c.voicebird_server_url.clone(),
        FieldKey::CloudBroadcastEnabled => {
            if c.cloud_broadcast_enabled { "on".into() } else { "off".into() }
        }
    }
}

pub fn render(f: &mut Frame, area: Rect, app: &App) {
    let outer = Block::default()
        .borders(Borders::ALL)
        .title(" Settings ");
    let inner = outer.inner(area);
    f.render_widget(outer, area);

    let mut lines: Vec<Line> = Vec::new();
    let mut prev_section: Option<&'static str> = None;

    for (field_idx, fld) in FIELDS.iter().enumerate() {
        if prev_section != Some(fld.section) {
            if prev_section.is_some() {
                lines.push(Line::from(""));
            }
            lines.push(Line::from(Span::styled(
                format!("▸ {}", fld.section),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            )));
            prev_section = Some(fld.section);
        }

        let marker = if field_idx == app.settings_cursor { "▶ " } else { "  " };
        let value_text = if field_idx == app.settings_cursor && app.settings_edit_buf.is_some() {
            app.settings_edit_buf.clone().unwrap_or_default()
        } else {
            field_display(app, fld.key)
        };

        let value_style = if field_idx == app.settings_cursor && app.settings_edit_buf.is_some() {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default()
        };

        lines.push(Line::from(vec![
            Span::raw(format!("    {marker}")),
            Span::raw(format!("{:<22}", format!("{}:", fld.label))),
            Span::styled(value_text, value_style),
        ]));

    }

    lines.push(Line::from(""));
    let hint = if app.settings_edit_buf.is_some() {
        "Enter: save field   Esc: cancel edit"
    } else {
        let kind = current_field(app)
            .map(|f| field_kind(f.key))
            .unwrap_or(FieldKind::Text);
        match kind {
            FieldKind::Text => "↑↓: move   Enter: edit   s: save   Esc: close",
            FieldKind::Cycle | FieldKind::Numeric => {
                "↑↓: move   ←→: cycle   s: save   Esc: close"
            }
        }
    };
    lines.push(Line::from(Span::styled(
        hint,
        Style::default().fg(Color::DarkGray),
    )));
    if let Some(err) = &app.settings_error {
        lines.push(Line::from(Span::styled(
            err.clone(),
            Style::default().fg(Color::Red),
        )));
    }

    let p = Paragraph::new(lines);
    f.render_widget(p, inner);
}

use crossterm::event::KeyCode;

pub fn open(app: &mut App) {
    app.mode = crate::app::AppMode::Settings;
    app.settings_snapshot = Some(app.config.clone());
    app.settings_cursor = 0;
    app.settings_edit_buf = None;
    app.settings_error = None;
}

fn close(app: &mut App) {
    app.mode = crate::app::AppMode::Normal;
    app.settings_snapshot = None;
    app.settings_edit_buf = None;
    app.settings_error = None;
}

/// Revert config to the snapshot taken at open() time, then close.
fn cancel(app: &mut App) {
    if let Some(snap) = app.settings_snapshot.take() {
        app.config = snap;
    }
    close(app);
}

fn current_field(app: &App) -> Option<&'static Field> {
    FIELDS.get(app.settings_cursor)
}

fn apply_edit(app: &mut App) {
    let Some(buf) = app.settings_edit_buf.take() else { return };
    let Some(fld) = current_field(app).copied() else { return };
    let c = &mut app.config;
    match fld.key {
        FieldKey::DefaultModel => c.default_model = buf,
        FieldKey::Language => c.language = buf,
        FieldKey::SessionDir => c.session_dir = buf,
        FieldKey::AudioDefaultSource => c.audio_default_source = buf,
        FieldKey::InputDevice => {
            c.input_device = if buf.is_empty() { None } else { Some(buf) };
        }
        FieldKey::EnginePrefer => c.engine_prefer = buf,
        FieldKey::HopMs => match buf.parse::<u32>() {
            Ok(n) => c.hop_ms = n,
            Err(_) => {
                app.settings_error = Some(format!("hop_ms: not a number ({buf})"));
                return;
            }
        },
        FieldKey::MinWindowMs => match buf.parse::<u32>() {
            Ok(n) => c.min_window_ms = n,
            Err(_) => {
                app.settings_error = Some(format!("min_window_ms: not a number ({buf})"));
                return;
            }
        },
        FieldKey::RefinementModel => {
            c.refinement_model = if buf.is_empty() { None } else { Some(buf) };
        }
        FieldKey::RefinementWindowMs => match buf.parse::<u32>() {
            Ok(n) => c.refinement_window_ms = n,
            Err(_) => {
                app.settings_error = Some(format!("refinement_window_ms: not a number ({buf})"));
                return;
            }
        },
        FieldKey::RefinementBeamSize => match buf.parse::<u8>() {
            Ok(n) => c.refinement_beam_size = n,
            Err(_) => {
                app.settings_error = Some(format!("refinement_beam_size: not a number ({buf})"));
                return;
            }
        },
        FieldKey::VoiceBirdApiKey => c.voicebird_api_key = buf,
        FieldKey::VoiceBirdServerUrl => c.voicebird_server_url = buf,
        FieldKey::CloudBroadcastEnabled => {
            // Toggle is edited via Cycle, not Text — apply_edit shouldn't
            // be reached for it, but tolerate by parsing common boolean
            // strings.
            c.cloud_broadcast_enabled = matches!(buf.as_str(), "on" | "true" | "1" | "yes");
        }
    }
    app.settings_error = None;
}

fn try_save(app: &mut App) -> bool {
    if app.config.cloud_broadcast_enabled && app.config.voicebird_api_key.is_empty() {
        app.settings_error =
            Some("Live broadcast requires a non-empty Voice Bird API key".into());
        return false;
    }
    if app.config.cloud_broadcast_enabled && app.config.voicebird_server_url.is_empty() {
        app.settings_error =
            Some("Live broadcast requires a Voice Bird server URL".into());
        return false;
    }
    if let Err(e) = app.config.save() {
        app.settings_error = Some(format!("save: {e}"));
        return false;
    }
    true
}

pub fn handle_key(app: &mut App, key: KeyCode) {
    // Edit mode handling first.
    if let Some(buf) = app.settings_edit_buf.as_mut() {
        match key {
            KeyCode::Esc => {
                app.settings_edit_buf = None;
            }
            KeyCode::Enter => apply_edit(app),
            KeyCode::Backspace => {
                buf.pop();
            }
            KeyCode::Char(ch) => buf.push(ch),
            _ => {}
        }
        return;
    }

    // Navigation mode: determine field kind for the currently focused field.
    let kind = current_field(app)
        .map(|f| field_kind(f.key))
        .unwrap_or(FieldKind::Text);

    match key {
        KeyCode::Esc | KeyCode::Char('q') => cancel(app),
        KeyCode::Up | KeyCode::Char('k') => {
            if app.settings_cursor > 0 {
                app.settings_cursor -= 1;
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if app.settings_cursor + 1 < FIELDS.len() {
                app.settings_cursor += 1;
            }
        }
        KeyCode::Left | KeyCode::Char('h') => match kind {
            FieldKind::Cycle => {
                if let Some(f) = current_field(app).copied() {
                    cycle_string(app, f.key, -1);
                }
            }
            FieldKind::Numeric => {
                if let Some(f) = current_field(app).copied() {
                    cycle_numeric(app, f.key, -1);
                }
            }
            FieldKind::Text => {}
        },
        KeyCode::Right | KeyCode::Char('l') => match kind {
            FieldKind::Cycle => {
                if let Some(f) = current_field(app).copied() {
                    cycle_string(app, f.key, 1);
                }
            }
            FieldKind::Numeric => {
                if let Some(f) = current_field(app).copied() {
                    cycle_numeric(app, f.key, 1);
                }
            }
            FieldKind::Text => {}
        },
        KeyCode::Enter => match kind {
            FieldKind::Text => {
                // Open text edit — preload current value into buffer.
                if let Some(fld) = current_field(app).copied() {
                    let c = &app.config;
                    app.settings_edit_buf = Some(match fld.key {
                        FieldKey::VoiceBirdApiKey => c.voicebird_api_key.clone(),
                        FieldKey::VoiceBirdServerUrl => c.voicebird_server_url.clone(),
                        FieldKey::SessionDir => c.session_dir.clone(),
                        _ => String::new(),
                    });
                }
            }
            FieldKind::Cycle => {
                if let Some(f) = current_field(app).copied() {
                    cycle_string(app, f.key, 1);
                }
            }
            FieldKind::Numeric => {
                if let Some(f) = current_field(app).copied() {
                    cycle_numeric(app, f.key, 1);
                }
            }
        },
        KeyCode::Char('s') => {
            if try_save(app) {
                close(app);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;

    /// Advance `engine_prefer` forward through the full cycle: auto →
    /// whisperkit → whisper_rs → auto (wraps). Cloud broadcast lives in
    /// its own toggle now and is not part of this cycle.
    #[test]
    fn cycle_string_engine_prefer_forward() {
        let mut app = App::new();
        app.config.engine_prefer = "auto".into();

        cycle_string(&mut app, FieldKey::EnginePrefer, 1);
        assert_eq!(app.config.engine_prefer, "whisperkit");

        cycle_string(&mut app, FieldKey::EnginePrefer, 1);
        assert_eq!(app.config.engine_prefer, "whisper_rs");

        // Wrap back to the start — voicebird is no longer in the cycle.
        cycle_string(&mut app, FieldKey::EnginePrefer, 1);
        assert_eq!(app.config.engine_prefer, "auto");
    }

    /// Cloud broadcast toggle cycles off ↔ on.
    #[test]
    fn cycle_string_cloud_broadcast_toggle() {
        let mut app = App::new();
        app.config.cloud_broadcast_enabled = false;

        cycle_string(&mut app, FieldKey::CloudBroadcastEnabled, 1);
        assert!(app.config.cloud_broadcast_enabled);

        cycle_string(&mut app, FieldKey::CloudBroadcastEnabled, 1);
        assert!(!app.config.cloud_broadcast_enabled);
    }

    /// Saving with broadcast on but no API key surfaces an error.
    #[test]
    fn save_blocks_when_broadcast_on_without_key() {
        let mut app = App::new();
        app.config.cloud_broadcast_enabled = true;
        app.config.voicebird_api_key.clear();
        let ok = try_save(&mut app);
        assert!(!ok);
        assert!(app.settings_error.as_deref().unwrap_or("").to_lowercase().contains("api key"));
    }

    /// hop_ms forward: 750 → 1000 → 1500; backward: 750 → 500.
    #[test]
    fn cycle_numeric_hop_ms() {
        let mut app = App::new();
        app.config.hop_ms = 750;

        cycle_numeric(&mut app, FieldKey::HopMs, 1);
        assert_eq!(app.config.hop_ms, 1000);

        cycle_numeric(&mut app, FieldKey::HopMs, 1);
        assert_eq!(app.config.hop_ms, 1500);

        // Reset and test backward.
        app.config.hop_ms = 750;
        cycle_numeric(&mut app, FieldKey::HopMs, -1);
        assert_eq!(app.config.hop_ms, 500);
    }
}
