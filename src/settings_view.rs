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
    AssemblyAiApiKey,
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
    Field { section: "Cloud", key: FieldKey::AssemblyAiApiKey, label: "AssemblyAI API key" },
];

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
        FieldKey::AssemblyAiApiKey => format!("{} [plaintext]", mask_api_key(&c.assemblyai_api_key)),
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
        "↑↓: move   Enter: edit   s: save   Esc: close"
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
        FieldKey::AssemblyAiApiKey => c.assemblyai_api_key = buf,
    }
    app.settings_error = None;
}

fn try_save(app: &mut App) -> bool {
    if app.config.engine_prefer == "assemblyai"
        && app.config.assemblyai_api_key.is_empty()
    {
        app.settings_error =
            Some("engine_prefer=assemblyai requires a non-empty AssemblyAI API key".into());
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
        KeyCode::Enter => {
            if let Some(fld) = current_field(app).copied() {
                let c = &app.config;
                app.settings_edit_buf = Some(match fld.key {
                    FieldKey::AssemblyAiApiKey => c.assemblyai_api_key.clone(),
                    FieldKey::InputDevice => c.input_device.clone().unwrap_or_default(),
                    FieldKey::RefinementModel => c.refinement_model.clone().unwrap_or_default(),
                    FieldKey::DefaultModel => c.default_model.clone(),
                    FieldKey::Language => c.language.clone(),
                    FieldKey::SessionDir => c.session_dir.clone(),
                    FieldKey::AudioDefaultSource => c.audio_default_source.clone(),
                    FieldKey::EnginePrefer => c.engine_prefer.clone(),
                    FieldKey::HopMs => c.hop_ms.to_string(),
                    FieldKey::MinWindowMs => c.min_window_ms.to_string(),
                    FieldKey::RefinementWindowMs => c.refinement_window_ms.to_string(),
                    FieldKey::RefinementBeamSize => c.refinement_beam_size.to_string(),
                });
            }
        }
        KeyCode::Char('s') => {
            if try_save(app) {
                close(app);
            }
        }
        _ => {}
    }
}
