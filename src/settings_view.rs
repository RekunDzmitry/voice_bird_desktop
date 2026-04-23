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
    let mut field_idx = 0usize;

    for fld in FIELDS {
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

        field_idx += 1;
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

// Key handling lives in Task 14.
