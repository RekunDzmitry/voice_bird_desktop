use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};
use std::time::Instant;

use crate::app::{App, AppMode, PickerFocus, RecordingStatus, Section, Slot, SlotId, SlotKind};
use voice_bird_cli::session::layout::SessionSource;
use voice_bird_cli::session::target::Target;

pub fn render(f: &mut Frame, app: &App) {
    if app.mode == AppMode::ModelPicker {
        render_model_picker(f, f.area(), app);
        return;
    }

    // When a banner is set (engine error surfaced to the user), insert a
    // single-line red strip below the main workspace.
    // Plan deviation: the plan originally called for a silent
    // WhisperKit→whisper-rs restart on error; we surface errors via this
    // banner instead and let the user press `r` to retry.
    let has_banner = app.banner.is_some();
    let has_export = app.export_banner.is_some();
    let has_reminder = app
        .focused_cloud_reminder_until()
        .map(|t| Instant::now() < t)
        .unwrap_or(false);
    // Picker grows with the larger of devices/apps/targets, capped to
    // keep transcript space. The mode panel on the right shares this
    // row and needs at least 5 rows, so the picker is floored at 8 to
    // give a 3-column layout room for a few rows + scroll. Cap at 16
    // so the slot row still gets the bulk of the screen.
    let max_pane_len = app.devices.len().max(app.apps.len() + 1).max(3) as u16;
    let devices_h = (max_pane_len + 2).clamp(8, 16);
    let mut constraints: Vec<Constraint> = vec![
        Constraint::Length(3), // [0] header
        Constraint::Min(6),    // [1] main content + sidebar
    ];
    if has_reminder {
        constraints.push(Constraint::Length(1)); // reminder
    }
    if has_banner {
        constraints.push(Constraint::Length(1)); // error banner
    }
    if has_export {
        constraints.push(Constraint::Length(1)); // export banner
    }
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(f.area());

    render_header(f, root[0], app);

    // Main body splits horizontally: the recorder workspace on the left
    // and a fixed-width sidebar for mode + hotkeys on the right. Keeping
    // the key list out of a one-line bottom bar prevents truncation on normal
    // terminal widths.
    let main = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(72), Constraint::Length(36)])
        .split(root[1]);

    // Three rows stacked: a 3-pane picker (Devices / Apps / Targets)
    // and the slot row. The Targets pane replaces the per-slot chip
    // strip and makes picking a target as discoverable as picking a
    // device or app. Heights are weighted so the picker keeps its
    // room while the slot row keeps the bulk of the screen.
    let workspace = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(devices_h), Constraint::Min(6)])
        .split(main[0]);
    render_picker(f, workspace[0], app);
    render_sections(f, workspace[1], app);

    render_sidebar(f, main[1], app);

    // Running index for the optional rows after the sections row.
    let mut next_slot: usize = 2;

    if has_reminder {
        let r = Paragraph::new(Span::styled(
            "Audio is being sent to Voice Bird.",
            Style::default().fg(Color::Yellow),
        ));
        f.render_widget(r, root[next_slot]);
        next_slot += 1;
    }
    if has_banner {
        render_banner(f, root[next_slot], app);
        next_slot += 1;
    }
    if has_export {
        render_export_banner(f, root[next_slot], app);
    }

    // Modal overlay (rendered last so it sits on top of everything).
    if app.mode == AppMode::ApiKeyModal {
        render_api_key_modal(f, f.area(), app);
    }
    if app.mode == AppMode::PathModal {
        render_path_modal(f, f.area(), app);
    }
    if app.mode == AppMode::AgentFunnel {
        render_agent_funnel(f, f.area(), app);
    }
    if let AppMode::ConfirmDeleteAgentTarget { id } = &app.mode {
        render_confirm_delete(f, f.area(), app, id);
    }
}

/// Render the slot row. Each slot draws as its own block with a
/// per-slot Mode header in the title and the transcript + tentative
/// line stacked inside. Empty slots show a placeholder. The focused
/// slot's border is highlighted. Slot count grows with the Vec —
/// Phase A is hard-coded to three (the initial layout) but the
/// render path already iterates the Vec directly.
fn render_sections(f: &mut Frame, area: Rect, app: &App) {
    let n = app.slots.len();
    // Weight the layout so the focused slot gets 2 horizontal parts
    // and every other slot gets 1. With 1 slot this is moot; with 2
    // the focused pane claims ~65% of the row, with 3 it's ~50%;
    // all of those keep the dynamic "[N] <device> + <app> → <tgt>"
    // title from ellipsizing on the slot the user is editing.
    // Equal split is reserved for slot counts above the focused
    // weight so unfocused slots stay readable.
    let constraints: Vec<Constraint> = if n <= 1 {
        vec![Constraint::Ratio(1, 1)]
    } else {
        app.slots
            .iter()
            .map(|s| {
                if s.id == app.focused_slot {
                    Constraint::Ratio(2, 1)
                } else {
                    Constraint::Ratio(1, 1)
                }
            })
            .collect()
    };
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints)
        .split(area);
    for (i, slot) in app.slots.iter().enumerate() {
        render_section_column(f, cols[i], app, slot);
    }
}

fn render_section_column(f: &mut Frame, area: Rect, app: &App, slot: &Slot) {
    let slot_id = slot.id;
    let is_focused = slot_id == app.focused_slot;
    let section = slot.as_section();
    let saved = match &slot.kind {
        SlotKind::Saved { saved } => Some(saved),
        _ => None,
    };

    // Title: dynamic, picker-aware, wraps to a second line when the
    // composed string would exceed the column width. The two
    // components are "[N] <device> + <app> → <target>" line 1, and
    // a small status indicator (cloud ON/OFF, language, model) on
    // line 2 when the slot is recording. Otherwise just one line.
    let inner_w = area.width.saturating_sub(2) as usize; // borders
    let title_lines = build_slot_title(app, slot, section, saved, inner_w);
    let border_style = if is_focused {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else if section.is_some() {
        Style::default().fg(Color::Gray)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    // Each Line in the title vector becomes its own `block.title()`
    // call. ratatui stacks them top-to-bottom, giving us the
    // two-line wrap for free.
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style);
    for line in title_lines {
        block = block.title(line);
    }
    let inner = block.inner(area);
    f.render_widget(block, area);

    let Some(section) = section else {
        // Check for preserved (saved) transcript from a prior stop.
        if let Some(saved) = saved {
            let committed = saved.committed.lock();
            let refined = saved.refined.lock();
            let mut lines: Vec<Line> = Vec::with_capacity(refined.len() + committed.len());
            for c in refined.iter() {
                let ts = format!(
                    "{:02}:{:02}",
                    c.t_start_ms / 60_000,
                    (c.t_start_ms % 60_000) / 1000
                );
                lines.push(Line::from(vec![
                    Span::styled(ts, Style::default().fg(Color::DarkGray)),
                    Span::raw("  "),
                    Span::raw(c.text.clone()),
                ]));
            }
            let tail_style = if refined.is_empty() {
                Style::default()
            } else {
                Style::default().fg(Color::Gray)
            };
            for c in committed.iter() {
                let ts = format!(
                    "{:02}:{:02}",
                    c.t_start_ms / 60_000,
                    (c.t_start_ms % 60_000) / 1000
                );
                lines.push(Line::from(vec![
                    Span::styled(ts, Style::default().fg(Color::DarkGray)),
                    Span::raw("  "),
                    Span::styled(c.text.clone(), tail_style),
                ]));
            }
            drop(refined);
            drop(committed);

            // Show tentative bar as a paused hint.
            let cells = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(2), Constraint::Length(1)])
                .split(inner);
            let p = Paragraph::new(lines).wrap(Wrap { trim: false });
            f.render_widget(p, cells[0]);
            let hint = Paragraph::new(Line::from(Span::styled(
                "… (paused — press [x] to clear)",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            )));
            f.render_widget(hint, cells[1]);
            return;
        }

        // Empty slot placeholder. Keep this short so it fits inside a
        // narrow 1/3 column on smaller terminals.
        let p = Paragraph::new(vec![
            Line::from(Span::styled(
                "  (empty)",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(Span::styled(
                "  Pick a device, [Enter] to fill",
                Style::default().fg(Color::DarkGray),
            )),
        ])
        .wrap(Wrap { trim: false });
        f.render_widget(p, inner);
        return;
    };

    // Split inner area: transcript on top, tentative on the last line.
    let cells = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(2), Constraint::Length(1)])
        .split(inner);

    render_section_transcript(f, cells[0], section);
    render_section_tentative(f, cells[1], section);
}

/// line (when the title fits) or two lines (the picker prefix on
/// line 1, the routing target in colour on line 2). The caller
/// applies each line as a separate `block.title()` call. Each
/// component stays inside `inner_w` columns; if the picker prefix
/// alone exceeds that, the device name is ellipsized to make room.
fn build_slot_title(
    app: &App,
    slot: &Slot,
    section: Option<&Section>,
    saved: Option<&crate::app::SavedTranscript>,
    inner_w: usize,
) -> Vec<Line<'static>> {
    let n = slot.id.0;

    if section.is_none() && saved.is_none() && !slot_has_picker_pick(app) {
        return vec![Line::from(format!(" [{n}] (empty) "))];
    }

    let device = app
        .focused_device()
        .map(|d| d.name.clone())
        .or_else(|| app.config.input_device.clone());
    let app_pick = app.focused_app().map(|a| a.name.clone());
    let target = if let Some(s) = section {
        s.target.clone()
    } else {
        app.focused_pending_target()
    };
    let device_label = device.as_deref().unwrap_or("(no device)");
    let app_str = app_pick
        .as_deref()
        .map(|a| format!(" + {a}"))
        .unwrap_or_default();
    let prefix = format!(" [{n}] {device_label}{app_str} → {} ", target);
    let target_color = match target {
        Target::Stdout => Color::Green,
        Target::Cloud => Color::Magenta,
        Target::Agent { .. } => Color::Cyan,
    };
    if prefix.chars().count() <= inner_w {
        vec![Line::from(Span::styled(
            prefix,
            Style::default().add_modifier(Modifier::BOLD),
        ))]
    } else {
        let prefix_short = truncate_for_two_line(&prefix, inner_w);
        vec![
            Line::from(Span::styled(
                prefix_short,
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                format!(" → {target} "),
                Style::default()
                    .fg(target_color)
                    .add_modifier(Modifier::BOLD),
            )),
        ]
    }
}

/// A slot counts as "picker-picked" when the user has at least one
/// A slot counts as "picker-picked" when the user has at least one
/// picker selection that hasn't been turned into a recording yet —
/// i.e. a device name in config OR a non-None focused app OR a
/// pending target override for this slot.
fn slot_has_picker_pick(app: &App) -> bool {
    app.config.input_device.is_some()
        || app.focused_app().is_some()
        || app.pending_target_overrides.contains_key(&app.focused_slot)
}

/// slot they're on and which target was picked.
fn truncate_for_two_line(prefix: &str, inner_w: usize) -> String {
    // The target suffix is appended as a separate line in the
    // caller, so we only need to make the prefix (everything up to
    // " → ") fit. The arrow + target is always its own second line.
    if let Some(idx) = prefix.rfind(" → ") {
        let head = &prefix[..idx + 1];
        if head.chars().count() <= inner_w {
            return prefix.to_string();
        }
        if let Some(bracket_end) = head.find("] ") {
            let start = &head[..bracket_end + 2];
            let rest = &head[bracket_end + 2..head.len() - 1];
            let avail = inner_w.saturating_sub(start.chars().count() + 1);
            if avail >= 3 {
                let trimmed = truncate_with_ellipsis(rest, avail);
                return format!("{start}{trimmed}…");
            }
        }
    }
    prefix.to_string()
}

/// Truncate `s` to `max` columns, appending "…" if anything was
/// dropped. `max` must be ≥ 1.
fn truncate_with_ellipsis(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        s.to_string()
    } else {
        let keep = max.saturating_sub(1);
        let mut out: String = s.chars().take(keep).collect();
        out.push('…');
        out
    }
}
fn source_label(source: &SessionSource) -> String {
    match source {
        SessionSource::Microphone => "mic".into(),
        SessionSource::System => "system".into(),
        SessionSource::App {
            name, device_name, ..
        } => {
            if device_name.is_empty() {
                name.clone()
            } else {
                format!("{name} on {device_name}")
            }
        }
    }
}

fn render_section_transcript(f: &mut Frame, area: Rect, section: &Section) {
    let committed = section.committed.lock();
    let refined = section.refined.lock();

    // Refined leads (authoritative); streaming committed is the live tail.
    let mut lines: Vec<Line> = Vec::with_capacity(refined.len() + committed.len());

    for c in refined.iter() {
        let ts = format!(
            "{:02}:{:02}",
            c.t_start_ms / 60_000,
            (c.t_start_ms % 60_000) / 1000
        );
        lines.push(Line::from(vec![
            Span::styled(ts, Style::default().fg(Color::DarkGray)),
            Span::raw("  "),
            Span::raw(c.text.clone()),
        ]));
    }

    let tail_style = if refined.is_empty() {
        Style::default()
    } else {
        Style::default().fg(Color::Gray)
    };
    for c in committed.iter() {
        let ts = format!(
            "{:02}:{:02}",
            c.t_start_ms / 60_000,
            (c.t_start_ms % 60_000) / 1000
        );
        lines.push(Line::from(vec![
            Span::styled(ts, Style::default().fg(Color::DarkGray)),
            Span::raw("  "),
            Span::styled(c.text.clone(), tail_style),
        ]));
    }

    // Approximate row count for follow/scroll math (paragraph wraps).
    let inner_w = area.width.max(1) as usize;
    let approx_rows: u16 = lines
        .iter()
        .map(|l| {
            let w: usize = l.spans.iter().map(|s| s.content.chars().count()).sum();
            (w.div_ceil(inner_w)).max(1) as u16
        })
        .sum();
    let visible = area.height;
    let max_scroll = approx_rows.saturating_sub(visible);
    let scroll_y = if section.transcript_follow {
        max_scroll
    } else {
        section.transcript_scroll.min(max_scroll)
    };

    let p = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll_y, 0));
    f.render_widget(p, area);
}

fn render_section_tentative(f: &mut Frame, area: Rect, section: &Section) {
    let text = section.tentative.lock().clone();
    let p = Paragraph::new(Line::from(Span::styled(
        format!("… {}", text),
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC),
    )));
    f.render_widget(p, area);
}

pub fn render_model_picker(f: &mut Frame, area: Rect, app: &App) {
    let catalog = voice_bird_cli::transcription::models::Catalog::builtin();
    let selected = app.picker.as_ref().map(|p| p.index);
    let items: Vec<Line> = catalog
        .all()
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let marker = if Some(i) == selected { "▶ " } else { "  " };
            Line::from(vec![
                Span::raw(marker),
                Span::styled(m.id, Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(format!("  {} MB  {}  ", m.size_mb, m.language)),
                Span::raw(if m.is_default { "(default)" } else { "" }),
            ])
        })
        .collect();

    let title = if app.config_was_loaded_from_disk {
        " Pick a model (Esc to cancel) "
    } else {
        " Pick a model (first run — required) "
    };

    let p = Paragraph::new(items).block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(p, area);

    // Popup: download progress / error if a download is in flight.
    if let Some(progress_arc) = app.picker.as_ref().and_then(|p| p.downloading.as_ref()) {
        let g = progress_arc.lock();
        let msg = if let Some(err) = &g.error {
            format!("Download failed: {}: {}", g.model_id, err)
        } else {
            let pct = g.total.map(|t| (g.bytes * 100 / t.max(1))).unwrap_or(0);
            format!("Downloading {}: {pct}%", g.model_id)
        };
        let popup =
            Paragraph::new(msg).block(Block::default().borders(Borders::ALL).title(" Download "));
        let popup_area = centered(60, 3, area);
        f.render_widget(Clear, popup_area);
        f.render_widget(popup, popup_area);
    }
}

/// Right-hand sibling of the devices panel: shows the three knobs the
/// user can flip from the main screen — cloud on/off, transcription
/// language (locked to English when cloud is off), and the auto-picked
/// local model. Keys: `c` toggles cloud, `l` cycles language (cloud
/// only), `m` opens the manual model picker.
fn render_mode_panel(f: &mut Frame, area: Rect, app: &App) {
    let cloud_on = app.display_cloud_on();
    let cloud_label = if cloud_on { "[ON] " } else { "[OFF]" };
    let cloud_style = if cloud_on {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let lang_line: Line = if cloud_on {
        Line::from(vec![
            Span::raw("Language: "),
            Span::styled(
                app.display_language(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled("  (l)", Style::default().fg(Color::DarkGray)),
        ])
    } else {
        Line::from(vec![
            Span::raw("Language: "),
            Span::styled("en (locked)", Style::default().fg(Color::DarkGray)),
        ])
    };

    // Title hints when settings are scoped to a focused section vs global.
    let title = if app.focused().is_some() {
        " Mode (focused section) "
    } else {
        " Mode "
    };

    let path_raw = app.config.session_dir_expanded();
    let path_display = if path_raw.len() > 24 {
        format!("…{}", &path_raw[path_raw.len().saturating_sub(23)..])
    } else {
        path_raw.to_string()
    };

    // Windows is cloud-only: the cloud line is informational (no toggle),
    // 'c' manages the API key, and the local-only Model/Path lines are
    // omitted entirely. cfg!(windows) is a compile-time constant (the
    // branch folds away); it's used instead of #[cfg] so both layouts
    // stay type-checked from any host.
    let cloud_line = if cfg!(windows) {
        Line::from(vec![
            Span::raw("Cloud:    "),
            Span::styled("[ON] (cloud-only)", cloud_style),
            Span::styled("  (c: API key)", Style::default().fg(Color::DarkGray)),
        ])
    } else {
        Line::from(vec![
            Span::raw("Cloud:    "),
            Span::styled(cloud_label, cloud_style),
            Span::styled("  (c)", Style::default().fg(Color::DarkGray)),
        ])
    };
    let mut lines = vec![cloud_line, lang_line];
    if cfg!(not(windows)) {
        lines.push(Line::from(vec![
            Span::raw("Model:    "),
            Span::styled(app.display_model(), Style::default().fg(Color::Gray)),
            Span::styled("  (m)", Style::default().fg(Color::DarkGray)),
        ]));
        lines.push(Line::from(vec![
            Span::raw("Path:     "),
            Span::styled(&path_display, Style::default().fg(Color::Gray)),
            Span::styled("  (p)", Style::default().fg(Color::DarkGray)),
        ]));
    }
    let p = Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(p, area);
}

fn mask_api_key(key: &str) -> String {
    if key.is_empty() {
        "(empty)".into()
    } else if key.len() <= 4 {
        "•".repeat(key.len())
    } else {
        let shown = &key[key.len() - 4..];
        let hidden = "•".repeat(key.len() - 4);
        format!("{hidden}{shown}")
    }
}

/// Centered overlay used to paste the Voice Bird API key. Drawn on top
/// of the regular main screen — the underlying state stays visible
/// (dimmed by the fact that it isn't being interacted with) so the user
/// keeps their bearings. Reads from `app.api_key_buf`, which is set by
/// `App::open_api_key_modal` and cleared on Esc/Enter in the main key
/// handler.
fn render_api_key_modal(f: &mut Frame, area: Rect, app: &App) {
    let popup = centered(70, 5, area);
    f.render_widget(Clear, popup);

    let key = app.api_key_buf.clone().unwrap_or_default();
    let masked = mask_api_key(&key);
    let lines = vec![
        Line::from(Span::styled(
            "Paste API key — Enter to save, Esc to cancel",
            Style::default().fg(Color::Gray),
        )),
        Line::from(""),
        Line::from(Span::styled(
            masked,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Voice Bird API key ")
        .style(Style::default().fg(Color::White));
    let p = Paragraph::new(lines).block(block);
    f.render_widget(p, popup);
}

/// Centered overlay for editing the session output directory. Same
/// pattern as render_api_key_modal — the underlying UI stays visible
/// behind the popup. Shows the raw input plus an expanded preview
/// (resolving `~` to the real home directory).
fn render_path_modal(f: &mut Frame, area: Rect, app: &App) {
    let popup = centered(70, 6, area);
    f.render_widget(Clear, popup);

    let raw = app.path_buf.clone().unwrap_or_default();
    let expanded = if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            home.join(rest).to_string_lossy().into_owned()
        } else {
            raw.clone()
        }
    } else {
        raw.clone()
    };
    let lines = vec![
        Line::from(Span::styled(
            "Edit output path — Enter to save, Esc to cancel",
            Style::default().fg(Color::Gray),
        )),
        Line::from(""),
        Line::from(Span::styled(
            &raw,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!("→ {}", expanded),
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Output Path ")
        .style(Style::default().fg(Color::White));
    let p = Paragraph::new(lines).block(block);
    f.render_widget(p, popup);
}

/// Centered overlay for the multi-step "Add / Edit Agent target"
/// funnel. Renders the current step's question + the live
/// form values + a footer with the keys for the step.
/// Verify and Save show their respective status lines
/// instead of a question.
fn render_agent_funnel(f: &mut Frame, area: Rect, app: &App) {
    use voice_bird_cli::agent_funnel::{AgentFunnelStep, VerifyOutcome};

    let Some(funnel) = app.funnel.as_ref() else {
        return;
    };
    let popup = centered(72, 14, area);
    f.render_widget(Clear, popup);

    let mut lines: Vec<Line> = Vec::new();
    let step_label = match funnel.step {
        AgentFunnelStep::PickConnectionKind => "1/7 — Connection",
        AgentFunnelStep::Name => "2/7 — Name",
        AgentFunnelStep::Endpoint => "3/7 — Broker endpoint",
        AgentFunnelStep::Topic => "4/7 — Topic",
        AgentFunnelStep::Acks => "5/7 — Acknowledgement level",
        AgentFunnelStep::Verify => "6/7 — Verify",
        AgentFunnelStep::Save => "7/7 — Save",
    };
    lines.push(Line::from(Span::styled(
        format!(" {step_label} "),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));

    match funnel.step {
        AgentFunnelStep::PickConnectionKind => {
            lines.push(Line::from("Which connection?"));
            for (i, k) in voice_bird_cli::config::AgentConnectionKind::ALL
                .iter()
                .enumerate()
            {
                lines.push(Line::from(Span::styled(
                    format!("  [{}] {}", i + 1, k.label()),
                    Style::default().fg(Color::White),
                )));
            }
        }
        AgentFunnelStep::Name => {
            lines.push(Line::from("Target name (e.g. prod-events):"));
            lines.push(input_line(&funnel.name));
        }
        AgentFunnelStep::Endpoint => {
            lines.push(Line::from("Broker endpoint (host:port, comma-separated):"));
            lines.push(input_line(&funnel.endpoint));
        }
        AgentFunnelStep::Topic => {
            lines.push(Line::from("Topic name:"));
            lines.push(input_line(&funnel.topic));
        }
        AgentFunnelStep::Acks => {
            lines.push(Line::from("Acknowledgement level:"));
            let opts = [
                (voice_bird_cli::config::KafkaAcks::All, "[1] All (safe)"),
                (voice_bird_cli::config::KafkaAcks::One, "[2] One (fast)"),
                (
                    voice_bird_cli::config::KafkaAcks::Zero,
                    "[3] Zero (fire-and-forget)",
                ),
            ];
            for (val, label) in opts {
                let marker = if funnel.acks == val { "● " } else { "  " };
                let color = if funnel.acks == val {
                    Color::Yellow
                } else {
                    Color::Gray
                };
                lines.push(Line::from(Span::styled(
                    format!("{marker}{label}"),
                    Style::default().fg(color),
                )));
            }
        }
        AgentFunnelStep::Verify => {
            lines.push(Line::from(Span::styled(
                "Press [Enter] to verify the connection",
                Style::default().fg(Color::Gray),
            )));
            lines.push(Line::from(""));
            match &funnel.verify {
                VerifyOutcome::Pending => {
                    lines.push(Line::from(Span::styled(
                        "(not yet verified)",
                        Style::default().fg(Color::DarkGray),
                    )));
                }
                VerifyOutcome::InProgress => {
                    lines.push(Line::from(Span::styled(
                        "Verifying…",
                        Style::default().fg(Color::Yellow),
                    )));
                }
                VerifyOutcome::Ok { elapsed } => {
                    lines.push(Line::from(Span::styled(
                        format!("OK — round trip in {elapsed:?}"),
                        Style::default().fg(Color::Green),
                    )));
                }
                VerifyOutcome::Err { message } => {
                    lines.push(Line::from(Span::styled(
                        format!("FAILED: {message}"),
                        Style::default().fg(Color::Red),
                    )));
                }
            }
        }
        AgentFunnelStep::Save => {
            lines.push(Line::from(Span::styled(
                "Save this Agent target?",
                Style::default().fg(Color::Gray),
            )));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!("name     : {}", funnel.name),
                Style::default().fg(Color::White),
            )));
            lines.push(Line::from(Span::styled(
                format!("endpoint : {}", funnel.endpoint),
                Style::default().fg(Color::White),
            )));
            lines.push(Line::from(Span::styled(
                format!("topic    : {}", funnel.topic),
                Style::default().fg(Color::White),
            )));
            lines.push(Line::from(Span::styled(
                format!("acks     : {}", funnel.acks.as_str()),
                Style::default().fg(Color::White),
            )));
        }
    }

    lines.push(Line::from(""));
    let footer = match funnel.step {
        AgentFunnelStep::Verify => "[Enter] run verify  [Esc] cancel",
        AgentFunnelStep::Save => "[Enter] save  [Esc] cancel",
        _ => "[Enter] next  [Esc] cancel",
    };
    let back_hint = "  [←] back";
    lines.push(Line::from(Span::styled(
        format!("{footer}{back_hint}"),
        Style::default().fg(Color::DarkGray),
    )));

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Agent target ")
        .style(Style::default().fg(Color::White));
    let p = Paragraph::new(lines).block(block);
    f.render_widget(p, popup);
}

/// Centered overlay for the "delete this Agent target?" confirm
/// prompt. `y` confirms; any other key cancels. Implemented as
/// a single-line modal so it doesn't pull focus away from the
/// Targets pane for long.
fn render_confirm_delete(f: &mut Frame, area: Rect, app: &App, id: &str) {
    let popup = centered(60, 3, area);
    f.render_widget(Clear, popup);
    // Show the target's *name* (not its raw UUID) so the user
    // can tell which row they're about to delete. Fall back
    // to the id only if the config was edited out from under
    // us between prompt-open and prompt-render.
    let name = app
        .config
        .agent_target_by_id(id)
        .map(|t| t.name.clone())
        .unwrap_or_else(|| id.to_string());
    let lines = vec![Line::from(Span::styled(
        format!("Delete Agent target '{name}'? [y/N]"),
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
    ))];
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Confirm delete ")
        .style(Style::default().fg(Color::White));
    let p = Paragraph::new(lines).block(block);
    f.render_widget(p, popup);
}

/// Render a single text-input line with a yellow caret at the
/// end. Used by the funnel's Name / Endpoint / Topic steps.
fn input_line(value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            value.to_string(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("▏", Style::default().fg(Color::Yellow)),
    ])
}

/// Top-row picker. Three panes side by side: Devices (physical I/O),
/// Apps (per-application capture), and Targets (routing — Stdout /
/// Cloud / Agent). Each pane is a self-contained `Block` with its own
/// border + title + cursor; the picker wires them through a single
/// outer layout so they all share the same row height.
///
/// Width split is percentage-based and intentionally biased toward
/// Devices: device names are the longest strings we render, and
/// dropping Devices below ~40% starts clipping them. Apps and
/// Targets are short lists so they can survive narrower columns.
fn render_picker(f: &mut Frame, area: Rect, app: &App) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(45),
            Constraint::Percentage(33),
            Constraint::Percentage(22),
        ])
        .split(area);
    render_devices_pane(f, cols[0], app);
    render_apps_pane(f, cols[1], app);
    render_targets_list_pane(f, cols[2], app);
}
/// Third picker column. Lists the three routing options (Stdout /
/// Cloud / Agent) and lets the user pick one with the same arrow
/// navigation + Enter pattern as Devices and Apps. Picking here
/// writes to `pending_target_overrides` on the focused slot; the
/// next start_section consumes it.
///
/// The Agent row is rendered dim and the cursor refuses to land on it
/// when the agent runtime binary is not on disk (see
/// `App::focused_target_kind`). This keeps the visible state and
/// the pickable state in agreement.
fn render_targets_list_pane(f: &mut Frame, area: Rect, app: &App) {
    use crate::app::TargetKind;

    let focused = app.picker_focus == PickerFocus::Targets;
    let rows = app.targets();
    // The row that's actually wired to the focused slot's
    // pending-or-last target. Highlights survive the pane's focus
    // state — even when Targets isn't focused, the user can see
    // which row is the live choice.
    let picked = app.picked_target_kind();

    let title = if focused {
        " Targets ▸ [↑/↓] pick  [←] apps  [Enter] start "
    } else {
        " Targets  ([→] focus) "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(pane_border_style(focused))
        .title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let items: Vec<Line> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let is_cursor = i == app.selected_target_index.unwrap_or(0) && focused;
            // The Targets pane is now dynamic — `Agent` rows
            // can also be picked. Clone the kind so the borrow
            // on `row` ends before the next iteration's call.
            let is_picked = picked.as_ref() == Some(&row.kind) && !row.disabled;
            let marker = if is_cursor { "▶ " } else { "  " };
            // Look up the agent target's name so the row label
            // reads as "Agent: <name>" instead of a bare "Agent".
            let name = match &row.kind {
                crate::app::TargetKind::Agent { id } => {
                    app.config.agent_target_by_id(id).map(|t| t.name.clone())
                }
                _ => None,
            };
            let (label, style, hint) =
                target_row_style(row.kind.clone(), row.disabled, name.as_deref());
            // The picked row gets its base color bumped to Yellow
            // (the rest of the picker uses Green / Magenta / Cyan
            // per target kind) plus the BOLD modifier. Cursor row
            // wins on the invert to stay readable.
            let label_style = if is_cursor {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else if is_picked {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                style
            };
            let picked_tag = if is_picked {
                Span::styled(
                    " ●",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::raw("")
            };
            Line::from(vec![
                Span::raw(marker),
                Span::styled(label, label_style),
                Span::styled(hint, Style::default().fg(Color::DarkGray)),
                picked_tag,
            ])
        })
        .collect();

    let cursor_row = app.selected_target_index.unwrap_or(0) as u16;
    let scroll = clamp_scroll_for_render(
        cursor_row,
        app.target_scroll,
        items.len() as u16,
        inner.height,
    );
    let p = Paragraph::new(items).scroll((scroll, 0));
    f.render_widget(p, inner);
}
/// (label, style) for a single target row. `disabled=true` dims the
/// label and appends a hint so the user knows the row exists but
/// can't be picked.
/// (label, style, hint) for a single target row. `disabled=true`
/// dims the label and appends a hint so the user knows the row
/// exists but can't be picked.
fn target_row_style(
    kind: crate::app::TargetKind,
    disabled: bool,
    name: Option<&str>,
) -> (String, Style, String) {
    use crate::app::TargetKind;
    let base_color = match &kind {
        TargetKind::Stdout => Color::Green,
        TargetKind::Cloud => Color::Magenta,
        TargetKind::Agent { .. } => Color::Cyan,
    };
    let label = match &kind {
        TargetKind::Stdout => "Stdout".to_string(),
        TargetKind::Cloud => "Cloud".to_string(),
        TargetKind::Agent { .. } => match name {
            Some(n) => format!("Agent: {n}"),
            None => "Agent".to_string(),
        },
    };
    if disabled {
        (
            label,
            Style::default().fg(Color::DarkGray),
            "  (not installed)".into(),
        )
    } else {
        // Agent is rendered the same as Stdout / Cloud — no
        // trailing hint, same row geometry. The disabled
        // case (above) carries the "(not installed)" hint
        // since the row exists but can't be picked.
        let s = Style::default().fg(base_color).add_modifier(Modifier::BOLD);
        (label, s, String::new())
    }
}

fn pane_border_style(focused: bool) -> Style {
    if focused {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    }
}
fn render_devices_pane(f: &mut Frame, area: Rect, app: &App) {
    use crate::platform::AudioSessionKind;

    let focused = app.picker_focus == PickerFocus::Devices;
    let picked = app.picked_device_idx();

    let title = if focused {
        " Devices ▸ [↑/↓] select  [→] apps  [Enter] start "
    } else {
        " Devices  ([←] apps  [Tab] slot) "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(pane_border_style(focused))
        .title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    if app.devices.is_empty() {
        let p = Paragraph::new(Line::from(Span::styled(
            "  (no audio devices found — press [r] to refresh)",
            Style::default().fg(Color::DarkGray),
        )));
        f.render_widget(p, inner);
        return;
    }

    let items: Vec<Line> = app
        .devices
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let is_cursor = i == app.selected_device_index && focused;
            let is_picked = picked == Some(i);
            let marker = if is_cursor { "▶ " } else { "  " };
            let kind_tag = match d.kind {
                AudioSessionKind::Input => {
                    Span::styled(" [input] ", Style::default().fg(Color::Cyan))
                }
                AudioSessionKind::Output => {
                    Span::styled(" [output/loopback] ", Style::default().fg(Color::Magenta))
                }
                AudioSessionKind::App => Span::styled(" [app] ", Style::default().fg(Color::Green)),
            };
            // Picked rows glow yellow regardless of focus state —
            // they're the rows the focused slot is actually routed
            // to. The cursor row uses a white-on-black invert for
            // contrast; non-cursor picked rows just get the color
            // bump so it doesn't fight the focus invert.
            let name_style = if is_cursor {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else if is_picked {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().add_modifier(Modifier::BOLD)
            };
            // "●" pin appears only on the row that's pinned to the
            // slot. It survives the focus state of this pane —
            // users looking at Apps or Targets can still see what
            // the device choice is.
            let picked_tag = if is_picked {
                Span::styled(
                    " ●",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::raw("")
            };
            Line::from(vec![
                Span::raw(marker),
                Span::styled(d.name.clone(), name_style),
                kind_tag,
                picked_tag,
            ])
        })
        .collect();

    let scroll = clamp_scroll_for_render(
        app.selected_device_index as u16,
        app.device_scroll,
        items.len() as u16,
        inner.height,
    );

    let p = Paragraph::new(items).scroll((scroll, 0));
    f.render_widget(p, inner);
}

fn render_apps_pane(f: &mut Frame, area: Rect, app: &App) {
    let focused = app.picker_focus == PickerFocus::Apps;
    let picked = app.picked_app_idx();

    let title = if focused {
        " Apps ▸ [↑/↓] pick  [Space] none  [←] devices  [→] targets "
    } else {
        " Apps  ([→] focus) "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(pane_border_style(focused))
        .title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    if app.apps.is_empty() {
        let p = Paragraph::new(vec![
            Line::from(Span::styled(
                "  (none)",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(Span::styled(
                "  press [r] to refresh",
                Style::default().fg(Color::DarkGray),
            )),
        ]);
        f.render_widget(p, inner);
        return;
    }

    // First row is the synthetic "(no app — device only)" entry at
    // visible row 0. The apps Vec itself starts at 0 but the
    // rendered pane front-loads this row, so cursor/picked indices
    // shift by +1 throughout.
    let mut items: Vec<Line> = Vec::with_capacity(app.apps.len() + 1);
    let none_picked = picked == Some(0);
    let none_active = app.selected_app_index.is_none() && focused;
    let none_marker = if none_active { "▶ " } else { "  " };
    let none_picked_tag = if none_picked {
        Span::styled(
            " ●",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::raw("")
    };
    items.push(Line::from(vec![
        Span::raw(none_marker),
        Span::styled(
            "(no app — device only)",
            if none_picked {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::ITALIC | Modifier::BOLD)
            } else {
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC)
            },
        ),
        none_picked_tag,
    ]));

    for (i, a) in app.apps.iter().enumerate() {
        let is_cursor = Some(i) == app.selected_app_index && focused;
        let is_picked = picked == Some(i + 1);
        let marker = if is_cursor { "▶ " } else { "  " };
        let name_style = if is_cursor {
            Style::default()
                .fg(Color::Black)
                .bg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else if is_picked {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        };
        let picked_tag = if is_picked {
            Span::styled(
                " ●",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::raw("")
        };
        items.push(Line::from(vec![
            Span::raw(marker),
            Span::styled(a.name.clone(), name_style),
            picked_tag,
        ]));
    }

    // Cursor row in the rendered list: 0 means the synthetic "(no app)"
    // entry, otherwise selected_app_index + 1.
    let cursor_row = match app.selected_app_index {
        None => 0u16,
        Some(i) => (i + 1) as u16,
    };
    let scroll =
        clamp_scroll_for_render(cursor_row, app.app_scroll, items.len() as u16, inner.height);

    let p = Paragraph::new(items).scroll((scroll, 0));
    f.render_widget(p, inner);
}

/// Compute a render-time scroll offset that keeps `cursor_row` inside
/// the visible window of `visible` rows. Falls back to `pre_scroll`
/// when no clamping is needed. Avoids underflow / past-the-end.
fn clamp_scroll_for_render(cursor_row: u16, pre_scroll: u16, total: u16, visible: u16) -> u16 {
    if visible == 0 || total == 0 {
        return 0;
    }
    let max_scroll = total.saturating_sub(visible);
    let mut s = pre_scroll.min(max_scroll);
    if cursor_row < s {
        s = cursor_row;
    } else if cursor_row >= s.saturating_add(visible) {
        s = cursor_row + 1 - visible;
    }
    s.min(max_scroll)
}

fn centered(pct_w: u16, h: u16, parent: Rect) -> Rect {
    let w = parent.width * pct_w / 100;
    Rect {
        x: parent.x + (parent.width - w) / 2,
        y: parent.y + parent.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    }
}

fn render_header(f: &mut Frame, area: Rect, app: &App) {
    let timer = app.format_duration();
    let status = match &app.status {
        RecordingStatus::Idle => "idle",
        RecordingStatus::Recording => "● REC",
        RecordingStatus::Error(_) => "ERROR",
    };
    let device_label = app
        .config
        .input_device
        .clone()
        .unwrap_or_else(|| "default input".to_string());
    let line = Line::from(vec![
        Span::styled("Voice Bird", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("  │  "),
        Span::raw(app.display_model()),
        Span::raw("  │  "),
        Span::raw(format!("in: {}", device_label)),
        Span::raw("  │  "),
        Span::raw(format!(
            "engine: {}",
            // While recording / after a run, show the engine that was
            // actually selected (e.g. `whisper_rs` when the WhisperKit
            // sidecar was absent). Idle → fall back to the config
            // preference so the user can see what will be used next.
            {
                let kind = app.focused_engine_kind();
                if kind.is_empty() {
                    app.config.engine_prefer.as_str()
                } else {
                    kind
                }
            }
        )),
        Span::raw("  │  "),
        Span::styled(status, status_style(&app.status)),
        Span::raw("  │  "),
        Span::raw(timer),
    ]);

    if app.focused_cloud_active() {
        // Reserve 8 columns on the right for " LIVE " (with border padding).
        const BADGE_WIDTH: u16 = 8;
        let inner_w = area.width.saturating_sub(2); // subtract left+right borders
        let (title_w, badge_w) = if inner_w > BADGE_WIDTH {
            (inner_w - BADGE_WIDTH, BADGE_WIDTH)
        } else {
            (inner_w, 0)
        };

        // The outer block draws the border for the full area.
        let block = Block::default().borders(Borders::ALL);
        let inner = block.inner(area);
        f.render_widget(block, area);

        // Split inner area: left for title content, right for CLOUD badge.
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(title_w), Constraint::Length(badge_w)])
            .split(inner);

        let title_p = Paragraph::new(line);
        f.render_widget(title_p, cols[0]);

        if badge_w > 0 {
            let badge = Paragraph::new(Span::styled(
                " LIVE ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ));
            f.render_widget(badge, cols[1]);
        }
    } else {
        let p = Paragraph::new(line).block(Block::default().borders(Borders::ALL));
        f.render_widget(p, area);
    }
}

fn status_style(status: &RecordingStatus) -> Style {
    match status {
        RecordingStatus::Recording => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        RecordingStatus::Error(_) => Style::default().fg(Color::Red),
        _ => Style::default().fg(Color::Gray),
    }
}

fn render_banner(f: &mut Frame, area: Rect, app: &App) {
    let msg = app.banner.clone().unwrap_or_default();
    let p = Paragraph::new(Line::from(Span::styled(
        format!(" ! {msg}"),
        Style::default()
            .fg(Color::White)
            .bg(Color::Red)
            .add_modifier(Modifier::BOLD),
    )));
    f.render_widget(p, area);
}

fn render_export_banner(f: &mut Frame, area: Rect, app: &App) {
    let msg = app.export_banner.clone().unwrap_or_default();
    let style = if msg.starts_with("Export failed") || msg.starts_with("Failed") {
        Style::default()
            .fg(Color::White)
            .bg(Color::Red)
            .add_modifier(Modifier::BOLD)
    } else if msg == "Exporting…" {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        // Success / already exported
        Style::default()
            .fg(Color::Black)
            .bg(Color::Green)
            .add_modifier(Modifier::BOLD)
    };
    let p = Paragraph::new(Line::from(Span::styled(format!(" {msg}"), style)));
    f.render_widget(p, area);
}

fn render_sidebar(f: &mut Frame, area: Rect, app: &App) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(7), Constraint::Min(6)])
        .split(area);
    render_mode_panel(f, rows[0], app);
    render_hotkeys_panel(f, rows[1], app);
}

fn render_hotkeys_panel(f: &mut Frame, area: Rect, app: &App) {
    let any_active = app.active_section_count() > 0;
    // Windows is cloud-only: 'c' manages the API key instead of toggling
    // modes, and the local-only model/export/path keys don't exist.
    // cfg!(...) here is a compile-time constant, not a runtime check.
    let local_keys = cfg!(not(windows));
    let cloud_key_label = if local_keys { "cloud" } else { "API key" };
    let lines: Vec<Line> = match (any_active, &app.mode) {
        (_, AppMode::ApiKeyModal) | (_, AppMode::PathModal) => vec![
            hotkey_line("[Enter]", "save"),
            hotkey_line("[Esc]", "cancel"),
        ],
        (false, _) => {
            let mut lines = vec![
                hotkey_line("[↑/↓]", "select"),
                hotkey_line("[←/→]", "pane"),
                hotkey_line("[Space]", "no app"),
                hotkey_line("[Enter]", "start"),
                hotkey_line("[Tab]", "focus slot"),
                hotkey_line("[+]", "add slot"),
                hotkey_line("[-]", "remove slot"),
                hotkey_line("[r]", "refresh"),
                hotkey_line("[c]", cloud_key_label),
                hotkey_line("[l]", "language"),
            ];
            if local_keys {
                lines.push(hotkey_line("[m]", "model"));
                lines.push(hotkey_line("[p]", "path"));
            }
            // Agent CRUD keys live on the Targets pane only —
            // they don't collide with the Devices/Apps `l`,
            // `m`, `e` shortcuts. `e` is overloaded: in the
            // Targets pane it opens the edit funnel; in any
            // other pane (today: Devices) it exports.
            if app.picker_focus == crate::app::PickerFocus::Targets {
                lines.push(hotkey_line("[a]", "add agent"));
                lines.push(hotkey_line("[e]", "edit agent"));
                lines.push(hotkey_line("[d]", "delete agent"));
            } else if local_keys {
                lines.push(hotkey_line("[e]", "export"));
            }
            lines
        }
        (true, _) => {
            let mut lines = vec![
                hotkey_line("[↑/↓]", "select"),
                hotkey_line("[←/→]", "pane"),
                hotkey_line("[Enter]", "add"),
                hotkey_line("[Tab]", "focus slot"),
                hotkey_line("[+]", "add slot"),
                hotkey_line("[-]", "remove slot"),
                hotkey_line("[s]", "stop"),
                hotkey_line("[S]", "stop all"),
                hotkey_line("[c]", cloud_key_label),
                hotkey_line("[l]", "language"),
            ];
            if local_keys {
                lines.push(hotkey_line("[m]", "model"));
            }
            lines.push(hotkey_line("[PgUp]", "scroll up"));
            lines.push(hotkey_line("[PgDn]", "scroll down"));
            lines.push(hotkey_line("[Home]", "top"));
            lines.push(hotkey_line("[End]", "bottom"));
            lines.push(hotkey_line("[x]", "clear"));
            lines.push(hotkey_line("[q]", "quit"));
            lines.push(hotkey_line("[?]", "help"));
            lines
        }
    };

    let visible = area.height.saturating_sub(2);
    let hidden = (lines.len() as u16).saturating_sub(visible);
    let title = if hidden > 0 {
        format!(" Keys ({} more) ", hidden)
    } else {
        " Keys ".into()
    };
    let p = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(title))
        .scroll((0, 0))
        .wrap(Wrap { trim: false });
    f.render_widget(p, area);
}

fn hotkey_line(key: &'static str, action: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            key,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(action, Style::default().fg(Color::Gray)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, PickerFocus};
    use crate::platform::{AppSession, AudioDevice, AudioSessionKind};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn render_to_string(app: &App, w: u16, h: u16) -> String {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..h {
            for x in 0..w {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn input(name: &str) -> AudioDevice {
        AudioDevice {
            name: name.into(),
            kind: AudioSessionKind::Input,
        }
    }

    fn output(name: &str) -> AudioDevice {
        AudioDevice {
            name: name.into(),
            kind: AudioSessionKind::Output,
        }
    }

    fn fake_app(id: &str, name: &str) -> AppSession {
        AppSession {
            id: id.into(),
            name: name.into(),
            process_id: 0,
        }
    }

    #[test]
    fn devices_panel_renders_with_title_and_names() {
        let mut app = App::new();
        app.mode = crate::app::AppMode::Normal;
        app.devices = vec![input("MacBook Pro Microphone"), input("BlackHole 2ch")];
        app.selected_device_index = 1;
        let out = render_to_string(&app, 140, 30);
        assert!(out.contains("Devices"), "title missing:\n{out}");
        assert!(
            out.contains("MacBook Pro Microphone"),
            "device 0 missing:\n{out}"
        );
        assert!(out.contains("BlackHole 2ch"), "device 1 missing:\n{out}");
        assert!(out.contains("[input]"), "input kind tag missing:\n{out}");
    }

    #[test]
    fn key_sidebar_shows_enter_and_refresh_when_idle() {
        let app = App::new();
        let out = render_to_string(&app, 140, 30);
        assert!(out.contains("[Enter] start"), "enter hint missing:\n{out}");
        assert!(out.contains("[r] refresh"), "refresh hint missing:\n{out}");
    }

    #[test]
    fn header_shows_input_label() {
        let app = App::new();
        let out = render_to_string(&app, 140, 30);
        assert!(out.contains("in:"), "'in:' label missing:\n{out}");
    }

    #[test]
    fn output_devices_show_loopback_tag() {
        let mut app = App::new();
        app.devices = vec![
            input("MacBook Pro Microphone"),
            output("MacBook Pro Speakers"),
        ];
        let out = render_to_string(&app, 140, 30);
        assert!(
            out.contains("MacBook Pro Speakers"),
            "output device missing:\n{out}"
        );
        assert!(
            out.contains("[output/loopback]"),
            "output tag missing:\n{out}"
        );
    }

    #[test]
    fn empty_device_list_prompts_refresh() {
        let mut app = App::new();
        app.devices.clear();
        let out = render_to_string(&app, 140, 30);
        assert!(
            out.contains("no audio devices found"),
            "empty-list hint missing:\n{out}"
        );
    }

    #[test]
    fn apps_pane_renders_alongside_devices() {
        let mut app = App::new();
        app.devices = vec![output("MacBook Pro Speakers")];
        app.apps = vec![
            fake_app("us.zoom.xos", "Zoom"),
            fake_app("com.google.Chrome", "Chrome"),
        ];
        let out = render_to_string(&app, 160, 30);
        assert!(out.contains("Devices"), "devices title missing:\n{out}");
        assert!(out.contains("Apps"), "apps title missing:\n{out}");
        assert!(out.contains("Zoom"), "app 0 missing:\n{out}");
        assert!(out.contains("Chrome"), "app 1 missing:\n{out}");
    }

    #[test]
    fn apps_pane_focus_indicator_marks_apps_when_focused() {
        let mut app = App::new();
        app.devices = vec![output("MacBook Pro Speakers")];
        app.apps = vec![fake_app("us.zoom.xos", "Zoom")];
        app.picker_focus = PickerFocus::Apps;
        app.selected_app_index = Some(0);
        let out = render_to_string(&app, 160, 30);
        // Apps pane is focused: its title carries the action hint.
        assert!(
            out.contains("[Space] none"),
            "apps focus hint missing:\n{out}"
        );
        // Devices pane shows the unfocused hint.
        assert!(
            out.contains("[←] apps"),
            "devices unfocused hint missing:\n{out}"
        );
    }

    #[test]
    fn devices_pane_scrolls_to_keep_cursor_visible() {
        let mut app = App::new();
        let names: Vec<String> = (0..30).map(|i| format!("Dev {i:02}")).collect();
        app.devices = names.iter().map(|n| input(n)).collect();
        app.selected_device_index = 25;
        // Force a scroll-relevant render. The devices_h clamp caps the
        // panel at 14 rows total (12 inner). With cursor at row 25, the
        // visible window should anchor near the cursor — Dev 25 must be
        // visible, Dev 00 must not.
        let out = render_to_string(&app, 160, 30);
        assert!(out.contains("Dev 25"), "cursor row missing:\n{out}");
        assert!(!out.contains("Dev 00"), "scrolled-off row visible:\n{out}");
    }

    #[test]
    fn apps_pane_shows_synthetic_no_app_entry_when_apps_present() {
        let mut app = App::new();
        app.devices = vec![output("Speakers")];
        app.apps = vec![fake_app("us.zoom.xos", "Zoom")];
        // selected_app_index = None puts the cursor on the (no app) row
        // when the Apps pane is focused.
        app.picker_focus = PickerFocus::Apps;
        app.selected_app_index = None;
        let out = render_to_string(&app, 160, 30);
        assert!(
            out.contains("(no app — device only)"),
            "(no app) row missing:\n{out}"
        );
    }

    /// With cloud off, the mode panel locks language to English and
    /// hides the cycle hint. The "Cloud" label shows OFF.
    #[test]
    fn mode_panel_off_shows_language_locked() {
        let mut app = App::new();
        app.config.cloud_broadcast_enabled = false;
        let out = render_to_string(&app, 140, 30);
        assert!(out.contains("Mode"), "mode panel title missing:\n{out}");
        assert!(out.contains("[OFF]"), "cloud-off label missing:\n{out}");
        assert!(out.contains("locked"), "locked hint missing:\n{out}");
    }

    /// With cloud on, the mode panel offers the (l) language cycle hint
    /// and shows the saved language code.
    #[test]
    fn mode_panel_on_shows_language_cycle_hint() {
        let mut app = App::new();
        app.config.cloud_broadcast_enabled = true;
        app.config.language = "ru".into();
        let out = render_to_string(&app, 140, 30);
        assert!(out.contains("[ON]"), "cloud-on label missing:\n{out}");
        // `ru` appears in the language line; the (l) hint accompanies it.
        assert!(out.contains("ru"), "language code missing:\n{out}");
        assert!(out.contains("(l)"), "language cycle hint missing:\n{out}");
    }

    /// The key sidebar flips to the modal-specific hint while ApiKeyModal
    /// is active. The modal itself renders the prompt text.
    #[test]
    fn api_key_modal_renders_when_active() {
        let mut app = App::new();
        app.mode = crate::app::AppMode::ApiKeyModal;
        app.api_key_buf = Some("vb-test-key".into());
        let out = render_to_string(&app, 140, 30);
        assert!(
            out.contains("Voice Bird API key"),
            "modal title missing:\n{out}"
        );
        assert!(
            out.contains("Paste API key"),
            "modal prompt missing:\n{out}"
        );
        assert!(
            out.contains("[Enter] save"),
            "modal key hint missing:\n{out}"
        );
        // Last 4 chars of the buffer are shown unmasked.
        assert!(out.contains("-key"), "masked tail missing:\n{out}");
    }

    /// The mode panel shows the model name (auto-picked or user-chosen)
    /// so the user can see what's loaded without leaving the main screen.
    #[test]
    fn mode_panel_shows_model_name() {
        let mut app = App::new();
        app.config.default_model = "tiny.en".into();
        let out = render_to_string(&app, 140, 30);
        assert!(out.contains("tiny.en"), "model name missing:\n{out}");
        assert!(out.contains("(m)"), "model picker hint missing:\n{out}");
    }

    /// The workspace starts at 1 slot. Expanding with `+` adds
    /// additional slots; each column carries its slot number in
    /// the title. The empty-slot placeholder still shows when
    /// nothing is recording.
    #[test]
    fn three_section_columns_show_empty_placeholders() {
        let mut app = App::new();
        // App::new() ships one slot; the user opts in to more
        // with `+`. Verifying that *three* columns render means
        // expanding twice first.
        app.add_slot();
        app.add_slot();
        let out = render_to_string(&app, 180, 40);
        assert!(out.contains("[1]"), "slot 1 title missing:\n{out}");
        assert!(out.contains("[2]"), "slot 2 title missing:\n{out}");
        assert!(out.contains("[3]"), "slot 3 title missing:\n{out}");
        assert!(out.contains("(empty"), "empty placeholder missing:\n{out}");
    }
    /// Targets pane renders as the third picker column. The header is
    /// " Targets " (focused variant carries the action hint), the
    /// three rows are Stdout / Cloud / Agent, and the active pick is
    /// tagged with "(active)" next to the focused slot's current
    /// target.
    #[test]
    fn targets_pane_lists_stdout_cloud_agent_in_order() {
        // The default Agent row is gone — the user has to add
        // their own via the funnel. Plant one in the config so
        // the row renders in the expected order.
        let mut app = App::new();
        app.upsert_agent_target_in_memory(voice_bird_cli::config::AgentTargetConfig {
            id: "test-uuid".into(),
            name: "prod".into(),
            connection: voice_bird_cli::config::AgentConnection::Kafka(
                voice_bird_cli::config::KafkaAgentConnection {
                    endpoint: "localhost:9092".into(),
                    topic: "voice-bird".into(),
                    client_id: None,
                    acks: Default::default(),
                },
            ),
        });
        let out = render_to_string(&app, 180, 40);
        // Pane header (focused variant — Devices is the default focus,
        // so the targets pane shows the unfocused title).
        assert!(out.contains("Targets"), "targets pane missing:\n{out}");
        // Three rows in the fixed order: Stdout first, Agent last.
        let stdout_pos = out.find("Stdout").expect("Stdout row missing");
        let cloud_pos = out.find("Cloud").expect("Cloud row missing");
        let agent_pos = out.find("Agent:").expect("Agent row missing");
        assert!(stdout_pos < cloud_pos, "Stdout must come before Cloud");
        assert!(cloud_pos < agent_pos, "Cloud must come before Agent");
    }

    /// When the focused slot has been used before, the row matching
    /// the current target gets a "(active)" tag — without it the user
    /// has no way to tell at a glance which row is the active pick
    /// across multiple slots.
    #[test]
    fn targets_pane_marks_active_target_with_active_tag() {
        let app = App::new();
        let out = render_to_string(&app, 180, 40);
        // No sections have started yet — so the active tag is absent
        // for every row. This is the lazy invariant; the helper that
        // writes the tag only fires when `focused_target()` returns
        // `Some(_)` (i.e. a section has run on the focused slot).
        // The assertion is the absence of the tag in the idle case.
        assert!(
            !out.contains("(active)"),
            "no section has started yet — active tag should be absent:\n{out}"
        );
    }

    /// `target_row_style` (the helper behind the Targets pane rows)
    /// returns the Agent row in dark gray and tagged as
    /// "not installed" when the runtime binary is missing. The
    /// base helper is pure — no real agent-runtime detection
    /// required.
    #[test]
    fn target_row_style_agent_is_dim_when_disabled() {
        use crate::app::TargetKind;
        let (label, style, hint) =
            target_row_style(TargetKind::Agent { id: "any".into() }, true, None);
        assert_eq!(label, "Agent");
        assert_eq!(style.fg, Some(Color::DarkGray));
        assert!(hint.contains("not installed"));
    }
    /// When Agent is enabled, the row is rendered with the same
    /// shape as Stdout / Cloud — no trailing hint text, so the
    /// three rows share identical geometry and the pin glyph
    #[test]
    fn target_row_style_agent_is_cyan_when_enabled() {
        use crate::app::TargetKind;
        let (label, style, hint) = target_row_style(
            TargetKind::Agent { id: "any".into() },
            false,
            Some("prod-events"),
        );
        assert_eq!(label, "Agent: prod-events");
        assert_eq!(style.fg, Some(Color::Cyan));
        assert!(style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(hint, "", "enabled Agent must not carry a trailing hint");
    }
    /// Enabled Stdout / Cloud also carry no hint — pinning
    /// the invariant that all three picker rows share a
    /// single shape so the pin column lands identically.
    #[test]
    fn target_row_style_stdout_cloud_have_no_hint() {
        use crate::app::TargetKind;
        for k in [TargetKind::Stdout, TargetKind::Cloud] {
            let (_label, _style, hint) = target_row_style(k.clone(), false, None);
            assert_eq!(hint, "", "enabled {:?} must not carry a trailing hint", k);
        }
    }
    /// Idle key sidebar shows the new Tab/cfg keys.
    #[test]
    fn key_sidebar_shows_tab_and_cfg_hints_when_idle() {
        let app = App::new();
        let out = render_to_string(&app, 180, 40);
        assert!(out.contains("[Tab]"), "Tab hint missing:\n{out}");
        assert!(
            out.contains("[c]/[l]/[m]") || out.contains("[c]") && out.contains("[l]"),
            "cfg hint missing:\n{out}"
        );
    }

    #[test]
    fn key_sidebar_shows_all_idle_hotkeys_at_normal_height() {
        // Wide-enough to render the full idle sidebar: the static
        // list now includes the +/- workspace controls plus the
        // local-only m/e/p lines plus the optional /mcp hint. We
        // give the pane 26 rows of inner space.
        let app = App::new();
        let out = render_to_string(&app, 160, 50);
        // The hotkey sidebar guarantees the keys the user reaches
        // for are present. The set is wide enough to catch
        // regressions in the static list (e.g. a dropped `[Tab]`
        // The idle branch must list every key the user can press
        // when no section is recording. Recording-only keys
        // ([s]top, [x]clear, …) live on the `(true, _)` branch.
        // No `/mcp` line — the Agent target lives in the
        // Targets picker pane rather than a separate hotkey.
        for key in [
            "[↑/↓]",
            "[←/→]",
            "[Space]",
            "[Enter]",
            "[Tab]",
            // The +/- workspace controls are always rendered —
            // short-circuit the test when the static list drops
            // them.
            "[+]",
            "[-]",
            "[r]",
            "[c]",
            "[l]",
        ] {
            assert!(
                out.contains(key),
                "idle key {key} missing from sidebar:\n{out}"
            );
        }
    }
    /// Expanding the workspace shows the new slot number in the
    /// title once the column renders. Useful to prevent regressions
    /// in `fresh_slots` / `next_slot_id` — the title `[2]` only
    /// appears if the second slot's id is 2.
    #[test]
    fn add_slot_creates_distinct_numbered_columns() {
        let mut app = App::new();
        let id_a = app.add_slot();
        let id_b = app.add_slot();
        assert!(matches!(id_a, Some(crate::app::SlotId(2))));
        assert!(matches!(id_b, Some(crate::app::SlotId(3))));
        let out = render_to_string(&app, 180, 40);
        assert!(out.contains("[1]"), "slot 1 missing:\n{out}");
        assert!(out.contains("[2]"), "slot 2 missing:\n{out}");
        assert!(out.contains("[3]"), "slot 3 missing:\n{out}");
    }
    /// End-to-end visual smoke: drive a realistic state, write the
    /// rendered layout to `target/snapshot-idle.txt`. Powers the
    /// `snapshot_idle_layout_for_visual_review` workflow — useful
    /// to catch layout regressions in the picker shape, the slot
    /// row, the sidebar, and the picked-pin highlights at a
    /// glance.
    #[test]
    fn snapshot_idle_layout_for_visual_review() {
        let mut app = App::new();
        app.devices = vec![
            input("MacBook Pro Microphone"),
            output("Mac mini Speakers"),
            output("EPOS PC 8 USB"),
            input("HD Pro Webcam C920"),
        ];
        app.apps = vec![
            fake_app("chrome", "Google Chrome"),
            fake_app("zoom", "Zoom"),
            fake_app("terminal", "Terminal"),
        ];
        // Cursor lands on a non-trivial row; cursor + picked are
        // intentionally different so the test exercises both
        // branches.
        app.selected_device_index = 2;
        app.selected_app_index = Some(1);
        app.selected_target_index = Some(1); // Cloud
                                             // Pretend a previous start installed an Agent session id, so
                                             // the Agent row stays pickable but isn't the picked one.
        let out = render_to_string(&app, 180, 40);
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("snapshot-idle.txt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &out).unwrap();
    }
    /// Multi-slot round-trip — drove directly from a reported
    /// scenario: user starts a recording on slot 1, adds a second
    /// slot with `+`, navigates back to slot 1 with Shift-Tab,
    /// navigates forward to slot 2 with Tab. Each focus step must
    /// (a) succeed without surprising state, (b) keep the device,
    /// app, and target picked-row pins in the picker pointing at
    /// the *focused* slot's pick.
    #[test]
    fn multi_slot_round_trip_keeps_picked_pins_on_focused_slot() {
        use crate::app::{RecordingStatus, SavedTranscript, SlotKind};
        use crate::platform::{AudioDevice, AudioSessionKind};
        use std::sync::Arc;
        use voice_bird_cli::session::target::Target;

        let mut app = App::new();
        // Cloud is always pickable (doesn't depend on the user
        // having an agent runtime installed). Pick that as
        // the per-slot target so the test stays portable
        // across machines.
        // Seed the picker state so each pane is non-trivial —
        // otherwise the picked pin would land on a single
        // vacant row and the test wouldn't catch ordering bugs.
        app.devices = vec![
            AudioDevice {
                name: "MacBook Pro Microphone".into(),
                kind: AudioSessionKind::Input,
            },
            AudioDevice {
                name: "Mac mini Speakers".into(),
                kind: AudioSessionKind::Output,
            },
            AudioDevice {
                name: "EPOS PC 8 USB".into(),
                kind: AudioSessionKind::Output,
            },
        ];
        app.apps = vec![
            fake_app("chrome", "Google Chrome"),
            fake_app("zoom", "Zoom"),
            fake_app("terminal", "Terminal"),
        ];
        app.selected_device_index = 2; // EPOS PC 8 USB
        app.selected_app_index = Some(0); // Chrome
        app.selected_target_index = Some(1); // Cloud (cursor)
                                             // slot 1 starts as the only slot (id 1). Mark it as a
                                             // saved recording on Cloud so the device + target pick
                                             // are non-default. Bypassing `start_section` keeps the
                                             // test free of audio / cpal / tokio runtime needs.
        let saved_target = Target::Cloud;
        app.slots[0] = crate::app::Slot {
            id: crate::app::SlotId(1),
            kind: SlotKind::Saved {
                saved: SavedTranscript {
                    committed: Arc::new(parking_lot::Mutex::new(Vec::new())),
                    refined: Arc::new(parking_lot::Mutex::new(Vec::new())),
                    label: "EPOS PC 8 USB + Chrome -> Cloud".into(),
                    target: saved_target.clone(),
                },
            },
        };
        // Move focus explicitly to slot 1 (already is, by
        // construction).
        app.focused_slot = crate::app::SlotId(1);
        app.status = RecordingStatus::Idle;

        // ---- Step 0: focus on slot 1 ----
        // Confirm we start on slot 1 and the picks resolve to
        // the right rows in every pane.
        assert_eq!(app.focused_slot, crate::app::SlotId(1));
        assert_eq!(app.picked_device_idx(), Some(2));
        assert_eq!(app.picked_app_idx(), Some(1)); // Chrome at apps[0] + synthetic offset 1
        assert_eq!(
            app.picked_target_kind(),
            Some(crate::app::TargetKind::Cloud)
        );
        let out0 = render_to_string(&app, 180, 40);
        // The picked Device row ('EPOS PC 8 USB' at index 2)
        // must glow yellow + carry the '●' pin. Each
        // non-cursor device row appears once in the rendered
        // buffer — counting '●' against the picker tells us
        // if more than one row picked up the pin.
        assert!(
            out0.lines()
                .any(|l| l.contains("EPOS PC 8 USB") && l.contains('●')),
            "slot-1 step 0: device row missing '●' pin:\n{out0}"
        );
        assert!(
            out0.lines()
                .any(|l| l.contains("Chrome") && l.contains('●')),
            "slot-1 step 0: app row missing '●' pin:\n{out0}"
        );
        // Helper: locate a Targets-pane row by its label and
        // report whether the row carries the trailing '●' pin.
        // Anchors on the row's coordinate markers
        // ('││  ' header, '│' footer, no '→' arrow) so the
        // top border / slot title / side-panel cells don't
        // sneak in.
        // Helper: locate a Targets-pane row by its label and
        // report whether the row carries the trailing '●' pin.
        // The TestBackend pads every line to the column width
        // with spaces, so a row filter should anchor on the
        // column-start marker '││  ' and the row's label, not
        // on '│' (the right border, which is followed by
        // trailing spaces). Trim trailing whitespace after the
        // Each rendered row in the Targets pane reads as
        // "Label [hint] [●]" with one trailing pin. The
        // TestBackend flattens every pane into one 180-wide
        // line, so a pin search is the most reliable
        // verification — substring "Label ●" matches the
        // exact row we care about without dragging in
        // neighbour panes.
        let target_pin_present = |out: &str, label: &str| -> bool {
            let pinned_text = format!("{label} \u{25CF}");
            out.contains(&pinned_text)
        };
        // ---- Step 1: + adds a second slot ----
        // focus_next / focus_prev cycle through every slot
        // including Empty — the user can press Tab or
        // Shift-Tab between slots regardless of whether a
        // recording has started there yet.
        let added = app.add_slot();
        assert_eq!(added, Some(crate::app::SlotId(2)));
        assert_eq!(app.slots.len(), 2);
        // `add_slot` advances focused_slot to the new slot.
        assert_eq!(app.focused_slot, crate::app::SlotId(2));
        // Empty slot 2 picks fall back to the global
        // cursor positions (Device, App) and the
        // per-slot Stdout default (Target). Device +
        // App keep their previous state because the
        // user hasn't interacted with those panes;
        // Target falls back to Stdout because slot 2
        // has no recording and no pending override.
        assert_eq!(app.picked_device_idx(), Some(2));
        assert_eq!(app.picked_app_idx(), Some(1));
        assert_eq!(
            app.picked_target_kind(),
            Some(crate::app::TargetKind::Stdout)
        );
        let out1 = render_to_string(&app, 180, 40);
        assert!(
            target_pin_present(&out1, "Stdout"),
            "step 1: Stdout must be pinned on slot 2:\n{out1}"
        );
        assert!(
            !target_pin_present(&out1, "Cloud"),
            "step 1: Cloud must NOT leak from slot 1 to slot 2:\n{out1}"
        );
        assert!(out1.contains("[1]"), "slot 1 title missing:\n{out1}");
        assert!(out1.contains("[2]"), "slot 2 title missing:\n{out1}");

        // ---- Step 2: focus_prev takes us back to slot 1 ----
        app.focus_prev();
        assert_eq!(app.focused_slot, crate::app::SlotId(1));
        assert_eq!(
            app.picked_target_kind(),
            Some(crate::app::TargetKind::Cloud),
            "back on slot 1: picked target should be Cloud again"
        );
        assert_eq!(app.picked_device_idx(), Some(2));
        assert_eq!(app.picked_app_idx(), Some(1));
        let out2 = render_to_string(&app, 180, 40);
        // Returning to slot 1 must re-pin Cloud on Targets
        // — and must NOT leave Stdout pinned.
        assert!(
            target_pin_present(&out2, "Cloud"),
            "back on slot 1: Cloud must be pinned:\n{out2}"
        );
        assert!(
            !target_pin_present(&out2, "Stdout"),
            "back on slot 1: Stdout must not be pinned:\n{out2}"
        );

        // ---- Step 3: focus_next takes us back to slot 2 ----
        app.focus_next();
        assert_eq!(app.focused_slot, crate::app::SlotId(2));
        // Empty slot 2 falls back to defaults again — the
        // pending override from Step 0 hasn't been touched yet.
        assert_eq!(
            app.picked_target_kind(),
            Some(crate::app::TargetKind::Stdout),
            "forward on slot 2: picked target should be Stdout fallback"
        );
        let out3 = render_to_string(&app, 180, 40);
        assert!(
            target_pin_present(&out3, "Stdout"),
            "back on slot 2: Stdout must be pinned:\n{out3}"
        );
        assert!(
            !target_pin_present(&out3, "Cloud"),
            "back on slot 2: Cloud must not be pinned:\n{out3}"
        );

        // ---- Bonus: queue a pending Cloud pick on slot 2
        // and verify the override is per-slot. ----
        app.pending_target_overrides
            .insert(crate::app::SlotId(2), Target::Cloud);
        assert_eq!(
            app.picked_target_kind(),
            Some(crate::app::TargetKind::Cloud),
            "after queueing Cloud on slot 2, target picks Cloud"
        );
        app.focus_prev();
        assert_eq!(app.focused_slot, crate::app::SlotId(1));
        // Slot 1's pick is independent of slot 2's pending
        // override: still Cloud (last saved).
        assert_eq!(
            app.picked_target_kind(),
            Some(crate::app::TargetKind::Cloud),
            "slot 1 must keep its own target pick across slot 2's override"
        );
        app.focus_next();
        assert_eq!(app.focused_slot, crate::app::SlotId(2));
        assert_eq!(
            app.picked_target_kind(),
            Some(crate::app::TargetKind::Cloud),
            "slot 2 picks Cloud back after forward navigation"
        );
    }

    /// R7 (PR #31 round-3 review): the funnel footer must
    /// advertise the [←] back key. main.rs binds KeyCode::Left
    /// to AgentFunnel::back(), but every step footer still reads
    /// "[Enter] …  [Esc] cancel" — the escape hatch from a failed
    /// Verify is invisible exactly where it's needed.
    #[test]
    fn funnel_footer_advertises_back_key() {
        let mut app = App::new();
        app.funnel = Some(voice_bird_cli::agent_funnel::AgentFunnel::new_add());
        app.mode = crate::app::AppMode::AgentFunnel;
        let out = render_to_string(&app, 120, 40);
        let footers: Vec<&str> = out
            .lines()
            .filter(|l| l.contains("[Enter]"))
            .collect();
        assert!(
            out.contains("[←] back"),
            "funnel footer must advertise the [←] back key (R7); rendered footer(s): {footers:?}"
        );
    }
}
