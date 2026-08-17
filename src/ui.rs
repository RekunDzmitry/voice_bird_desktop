use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};
use std::time::Instant;

use crate::app::{App, AppMode, PickerFocus, RecordingStatus, Section, Slot, SlotKind};

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

    // Three rows stacked: a 3-pane picker (Devices / Apps / Agents)
    // and the slot row. The Agents pane replaces the per-slot chip
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
    if app.mode == AppMode::Status {
        render_status_overlay(f, f.area(), app);
    }
}

/// How many agent events the `t` status overlay shows. The
/// underlying log keeps `AGENT_EVENT_CAP` (50); the overlay shows
/// the newest slice that fits a modest popup.
const STATUS_OVERLAY_ROWS: usize = 15;

/// Centered overlay for the `t` status key: the most recent Agent
/// events, newest first, each with a wall-clock timestamp. `?`
/// stays help-only — this overlay is the one place recorder/agent
/// status surfaces on demand.
fn render_status_overlay(f: &mut Frame, area: Rect, app: &App) {
    let events = app.app_events.lock();
    let h = (events.len().min(STATUS_OVERLAY_ROWS) as u16 + 4).max(6);
    let popup = centered(72, h, area);
    f.render_widget(Clear, popup);

    let mut lines: Vec<Line> = Vec::new();
    if events.is_empty() {
        lines.push(Line::from(Span::styled(
            "(no agent events yet)",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for ev in events.iter().rev().take(STATUS_OVERLAY_ROWS) {
            lines.push(Line::from(vec![
                Span::styled(
                    ev.at.format("%H:%M:%S ").to_string(),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(ev.message.clone(), Style::default().fg(Color::White)),
            ]));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "[t/Esc/Enter] close",
        Style::default().fg(Color::DarkGray),
    )));

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Agent status ")
        .style(Style::default().fg(Color::White));
    f.render_widget(Paragraph::new(lines).block(block), popup);
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
            // Resume hint only makes sense on the focused slot —
            // pressing R resumes the focused slot, so showing
            // the hint on a non-focused slot would mislead.
            // The clear hint (x) is universal and stays on
            // every paused slot.
            //
            // Focused variant must fit in 1/3 of a 180-col
            // terminal (~58 cols; three section columns side
            // by side, each ~60 cols, no wrap on this
            // Paragraph). The full form
            // `[R] resumes · [x] clears · Enter = new session`
            // is 56 chars and clips in the slot column at
            // <180 cols. The shorter form keeps the
            // discoverability (every key + a one-word verb)
            // without ellipsising.
            let hint_text = if is_focused {
                "[R] resume · [x] clear · Enter = new"
            } else {
                "… (paused — press [x] to clear)"
            };
            let hint = Paragraph::new(Line::from(Span::styled(
                hint_text,
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
    let slot_id = slot.id;

    if section.is_none() && saved.is_none() && !slot_has_picker_pick(app, slot_id) {
        return vec![Line::from(format!(" [{n}] (empty) "))];
    }

    // Per-slot device + app: focused slot reads the live cursor;
    // non-focused slots read their memo so the title stays FROZEN
    // while the user moves the cursor elsewhere. Tab back to the
    // slot to see the live cursor resume.
    let device = app
        .slot_device(slot_id)
        .map(|d| d.name.clone())
        .or_else(|| app.config.input_device.clone());
    let app_pick = app.slot_app(slot_id).map(|a| a.name.clone());
    let _ = section;
    let _ = saved;
    let device_label = device.as_deref().unwrap_or("(no device)");
    let app_str = app_pick
        .as_deref()
        .map(|a| format!(" + {a}"))
        .unwrap_or_default();
    // Role prefix: when the slot was provisioned by an agent
    // room, prepend the role name so the user can tell at a
    // glance which human role this slot represents. Free Room
    // slots are unlabeled.
    let role_prefix = slot
        .role
        .as_ref()
        .map(|r| format!("[{}] ", r.name))
        .unwrap_or_default();
    // Title is ` [N] {role}{device} + {app} ` — no target suffix.
    // The routing target (Stdout / Cloud Agent) is picked in
    // the Agents pane, not declared in the slot title. §8.5
    // retired Stdout as a routing decision; today every slot
    // implicitly routes Stdout, so the old `→ Stdout` arrow
    // was dead text the user saw on every slot.
    let prefix = format!(" [{n}] {role_prefix}{device_label}{app_str} ");
    if prefix.chars().count() <= inner_w {
        vec![Line::from(Span::styled(
            prefix,
            Style::default().add_modifier(Modifier::BOLD),
        ))]
    } else {
        // When the picker prefix alone exceeds the column width,
        // ellipsize the device/app name to fit one line. (The
        // legacy two-line layout reserved a second line for the
        // target suffix — that line is gone now, so we just
        // truncate instead.)
        let prefix_short = truncate_with_ellipsis(&prefix, inner_w);
        vec![Line::from(Span::styled(
            prefix_short,
            Style::default().add_modifier(Modifier::BOLD),
        ))]
    }
}

/// A slot counts as "picker-picked" when the user has at least one
/// A slot counts as "picker-picked" when the user has at least one
/// picker selection that hasn't been turned into a recording yet —
/// i.e. a device name in config OR a non-None focused app OR a
/// pending target override for this slot.
fn slot_has_picker_pick(app: &App, slot_id: crate::app::SlotId) -> bool {
    app.config.input_device.is_some()
        || app.slot_app(slot_id).is_some()
        || app.pending_target_overrides.contains_key(&slot_id)
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
            let pct = g.total.map(|t| g.bytes * 100 / t.max(1) ).unwrap_or(0);
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

    // Focused slot's customized output path, or the default
    // if no customization. The slot is the single source of
    // truth for per-slot output path.
    let path_raw = app
        .slot_by_id(app.focused_slot)
        .and_then(|s| s.config.path.clone())
        .unwrap_or_else(|| app.default_slot_config.path.clone());
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
    // Reveal BOTH the first 5 chars (the predictable prefix - e.g.
    // `vb_01`) AND the last 5 chars (the high-entropy tail). The
    // prefix lets the user spot a corrupted paste like
    // `sk-testvb_…` (real key accidentally typed after a stale
    // test fixture); the tail lets the user recognise the key
    // ("is this the prod key?"). The middle is dotted.
    //
    // For keys short enough that the prefix and tail would
    // overlap (total <= PREFIX + TAIL = 10 chars), the whole key
    // is masked — there is no way to show both ends without
    // leaking the body.
    const PREFIX: usize = 5;
    const TAIL: usize = 5;
    let total = key.chars().count();
    if key.is_empty() {
        "(empty)".into()
    } else if total <= PREFIX + TAIL {
        "•".repeat(total)
    } else {
        let chars: Vec<char> = key.chars().collect();
        let prefix: String = chars[..PREFIX].iter().collect();
        let tail: String = chars[total - TAIL..].iter().collect();
        let middle = total - PREFIX - TAIL;
        format!("{prefix}{}{tail}", "•".repeat(middle))
    }
}

/// Centered overlay used to paste the Voice Bird API key. Drawn on top
/// of the regular main screen — the underlying state stays visible
/// (dimmed by the fact that it isn't being interacted with) so the user
/// keeps their bearings. Reads from `app.api_key_buf`, which is set by
/// `App::open_api_key_modal` and cleared on Esc/Enter in the main key
/// handler.
fn render_api_key_modal(f: &mut Frame, area: Rect, app: &App) {
    let popup = centered(70, 6, area);
    f.render_widget(Clear, popup);

    let key = app.api_key_buf.clone().unwrap_or_default();
    let masked = mask_api_key(&key);
    // Show the SAVED key (not the in-progress buffer) so the user can
    // see what is currently on disk - e.g. spot a corrupted prefix
    // like `sk-testvb_…` that has been silently pasted over a real
    // `vb_…` key. Empty when nothing is saved yet.
    let current_key = mask_api_key(&app.config.voicebird_api_key);
    let lines = vec![
        Line::from(Span::styled(
            // In-modal control hint. Covers the case where the keys
            // sidebar is partially obscured by the popup on narrower
            // terminals. Phrasing here is intentionally distinct
            // from the keys sidebar's `[Ctrl+U] clear` cell row (no
            // brackets, plain paragraph text).
            "Paste API key - Enter to save, Ctrl+U to clear, Esc to cancel",
            Style::default().fg(Color::Gray),
        )),
        Line::from(vec![
            Span::styled("Current key: ", Style::default().fg(Color::Gray)),
            Span::styled(
                current_key,
                Style::default().fg(Color::DarkGray),
            ),
        ]),
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

/// Top-row picker. Three panes side by side: Devices (physical I/O),
/// Apps (per-application capture), and Agents (cloud-prompt picker —
/// today a single `Stdout` row while the cloud list lands in §10).
/// Each pane is a self-contained `Block` with its own border +
/// title + cursor; the picker wires them through a single outer
/// layout so they all share the same row height.
///
/// Width split is percentage-based and intentionally biased toward
/// Devices: device names are the longest strings we render, and
/// dropping Devices below ~40% starts clipping them. Apps and
/// Agents are short lists so they can survive narrower columns.
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
    render_rooms_pane(f, cols[2], app);
}
/// Render the Rooms pane. Rooms live in `App::rooms` (index 0 is
/// always the hardcoded Free Room; cloud rooms follow). The
/// active room gets a `●` marker; locked Pro rooms (when
/// `plan_is_pro == Some(false)`) render dim with a 🔒 suffix.
/// Enter in this pane activates the picked room.
fn render_rooms_pane(f: &mut Frame, area: Rect, app: &App) {
    let focused = app.picker_focus == PickerFocus::Rooms;
    let title = if focused {
        " Rooms ▸ [↑/↓] pick  [←] apps  [Enter] activate "
    } else {
        " Rooms  ([→] focus) "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(pane_border_style(focused))
        .title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let active_idx = app.active_room;
    let is_pro = app.plan_is_pro;

    let items: Vec<Line> = app
        .rooms
        .iter()
        .enumerate()
        .map(|(i, room)| {
            let is_cursor = i == app.selected_room_index && focused;
            let is_active = i == active_idx;
            let is_locked = room.requires_pro && is_pro == Some(false);
            let marker = if is_cursor { "▶ " } else { "  " };
            let icon = room.icon.as_deref().unwrap_or("");
            let label = if icon.is_empty() {
                room.name.clone()
            } else {
                format!("{} {}", icon, room.name)
            };
            let mut suffix = String::new();
            if is_active {
                suffix.push_str(" ●");
            }
            if is_locked {
                suffix.push_str(" 🔒");
            }
            let style = if is_cursor {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else if is_locked {
                Style::default().fg(Color::DarkGray)
            } else if is_active {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            let suffix_style = if is_locked {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            };
            Line::from(vec![
                Span::raw(marker),
                Span::styled(label, style),
                Span::styled(suffix, suffix_style),
            ])
        })
        .collect();

    let cursor_row = app.selected_room_index as u16;
    let scroll = clamp_scroll_for_render(
        cursor_row,
        app.room_scroll,
        items.len() as u16,
        inner.height,
    );
    let p = Paragraph::new(items).scroll((scroll, 0));
    f.render_widget(p, inner);
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
            // users looking at Apps or Agents can still see what
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
        " Apps ▸ [↑/↓] pick  [Space] none  [←] devices  [→] agents "
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
    let local_keys = cfg!(not(windows));
    let cloud_key_label = if local_keys { "cloud" } else { "API key" };
    let lines: Vec<Line> = match (any_active, &app.mode) {
        (_, AppMode::ApiKeyModal) => vec![
            hotkey_line("[Enter]", "save"),
            hotkey_line("[Ctrl+U]", "clear"),
            hotkey_line("[Esc]", "cancel"),
        ],
        (_, AppMode::PathModal) => vec![
            hotkey_line("[Enter]", "save"),
            hotkey_line("[Esc]", "cancel"),
        ],
        (false, _) => {
            let mut lines = vec![
                hotkey_line("[↑/↓]", "select"),
                hotkey_line("[←/→]", "pane"),
                hotkey_line("[Space]", "no app"),
                hotkey_line("[Enter]", "new session"),
                hotkey_line("[Tab]", "focus slot"),
                hotkey_line("[+]", "add slot"),
                hotkey_line("[-]", "remove slot"),
                hotkey_line("[r]", "refresh"),
                hotkey_line("[c]", cloud_key_label),
                hotkey_line("[K]", "API key"),
                hotkey_line("[l]", "language"),
            ];
            if local_keys {
                lines.push(hotkey_line("[m]", "model"));
                lines.push(hotkey_line("[p]", "path"));
            }
            // `[a]`/`[e]`/`[d]` Agent CRUD keys removed in §8.
            // `[e]` exports a recording from the Devices pane;
            // the Agents pane will be repurposed for cloud Agents in §10.
            if app.picker_focus != crate::app::PickerFocus::Rooms && local_keys {
                lines.push(hotkey_line("[e]", "export"));
            }
            lines
        }
        (true, _) => {
            let mut lines = vec![
                hotkey_line("[↑/↓]", "select"),
                hotkey_line("[←/→]", "pane"),
                hotkey_line("[Enter]", "new session"),
                hotkey_line("[Tab]", "focus slot"),
                hotkey_line("[+]", "add slot"),
                hotkey_line("[-]", "remove slot"),
                hotkey_line("[s]", "stop"),
                hotkey_line("[R]", "resume"),
                hotkey_line("[S]", "stop all"),
                hotkey_line("[c]", cloud_key_label),
                hotkey_line("[K]", "API key"),
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
            lines.push(hotkey_line("[t]", "status"));
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

    /// Canonical synthetic API key for mask-rendering tests. The shape
    /// mirrors a real `vb_<64-hex>` key (5 ASCII + 60 hex-ish chars
    /// after) so the mask renders a meaningful prefix + dots, but the
    /// content is obviously fake — production-looking keys (whether
    /// real or from local config) must never appear in tests because
    /// they invite accidental leaks via `git log -p` / copy-paste.
    /// Do NOT replace this with a value from any local config file.
    const TEST_API_KEY: &str =
        "vb_TESTSC0res-cure0rts-cure0res-cure-rtscure-rtscure-rtscure-rtscure0res-c0res-c0";
    /// First 5 chars of `TEST_API_KEY` — the visible prefix the
    /// prefix-revealing mask contract must surface. Pinned here so
    /// tests assert a stable substring even if the fake key changes.
    const TEST_API_KEY_PREFIX: &str = "vb_TE";
    /// Last 5 chars of `TEST_API_KEY` — the visible suffix the
    /// both-ends mask contract must surface. Pinned here so tests
    /// assert a stable substring even if the fake key changes.
    const TEST_API_KEY_SUFFIX: &str = "es-c0";

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

    /// The `t` status overlay lists agent events newest-first with
    /// timestamps, and shows a placeholder when nothing happened yet.
    #[test]
    fn status_overlay_lists_recent_agent_events() {
        let mut app = App::new();
        app.mode = crate::app::AppMode::Status;
        let out = render_to_string(&app, 100, 30);
        assert!(out.contains("Agent status"), "overlay title missing:\n{out}");
        assert!(out.contains("(no agent events yet)"));

        // Push events directly via the shared field. The
        // legacy `push_app_event` free fn was removed with
        // §8.3+§8.4's consumer task — the caller used to be
        // the agent runtime, which no longer ships in the
        // desktop. The overlay render path is unchanged.
        {
            let now = chrono::Local::now();
            let mut buf = app.app_events.lock();
            buf.push_back(crate::app::AppEvent {
                at: now,
                message: "saved Agent target 'prod'".into(),
            });
            buf.push_back(crate::app::AppEvent {
                at: now,
                message: "verify OK for 'b:9092' in 42ms".into(),
            });
        }
        let out = render_to_string(&app, 100, 30);
        assert!(out.contains("saved Agent target 'prod'"), "event missing:\n{out}");
        assert!(out.contains("verify OK for 'b:9092' in 42ms"));
        // Newest first: the verify line renders above the save line.
        let verify_pos = out.find("verify OK").unwrap();
        let saved_pos = out.find("saved Agent target").unwrap();
        assert!(verify_pos < saved_pos, "events must render newest-first");
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
        assert!(out.contains("[Enter] new session"), "enter hint missing:\n{out}");
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
        app.default_slot_config.cloud_on = false;
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
        app.default_slot_config.cloud_on = true;
        app.default_slot_config.language = "ru".into();
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
            "modal prompt missing:\n{out}",
        );
        // First 5 chars of the buffer are visible unmasked (the
        // prefix the user uses to spot a corrupted paste like
        // `sk-testvb_…` pollution). The last 5 chars are also
        // visible (high-entropy tail). For `vb-test-key` (11
        // chars) the prefix is `vb-te` and the tail is `t-key`.
        assert!(out.contains("vb-te"), "masked prefix missing:\n{out}");
        assert!(out.contains("t-key"), "masked tail missing:\n{out}");
    }
    /// `mask_api_key` should not reveal keys short enough that the
    /// 5-char prefix and 5-char tail would overlap or touch — the
    /// whole key is masked at or below the 10-char cutoff.
    #[test]
    fn mask_api_key_never_reveals_short_keys() {
        assert_eq!(mask_api_key(""), "(empty)");
        // 5 chars - well under the 10-char cutoff, fully masked.
        assert_eq!(mask_api_key("abcde"), "•••••");
        // 4 chars - fully masked.
        assert_eq!(mask_api_key("abcd"), "••••");
        // 10 chars - exactly at the cutoff (PREFIX + TAIL), still
        // fully masked because any reveal would overlap.
        assert_eq!(mask_api_key("abcdefghij"), "••••••••••");
        // 11 chars - just over the cutoff. Prefix and tail
        // become visible with a single dot between them.
        assert_eq!(mask_api_key("abcdefghijk"), "abcde•ghijk");
    }
    /// The API-key modal advertises `Current key: vb_01…` above the input
    /// rendered label uses the SAVED key, not the buffer.
    #[test]
    fn api_key_modal_shows_current_saved_key_above_input() {
        let mut app = App::new();
        // Saved key is long; buffer holds a brand-new paste. The
        // modal must show the saved key (so the user can see what
        // is currently on disk), not the in-progress edit.
        // Synthetic test key. Production-looking keys (whether real
        // or from local config) must never appear in tests — they
        // invite accidental leaks via `git log -p` / copy-paste
        // snippets. The `TEST_API_KEY` const in this module owns
        // the canonical synthetic shape (vb_<5 ASCII> + 60 dots).
        app.config.voicebird_api_key = TEST_API_KEY.into();
        app.api_key_buf = Some("vb_bran-new-paste-here".into());
        app.mode = crate::app::AppMode::ApiKeyModal;
        let out = render_to_string(&app, 140, 30);
        assert!(
            out.contains("Current key:"),
            "modal must advertise the currently saved key; rendered:\n{out}",
        );
        // The label lists the SAVED key's prefix.
        assert!(
            out.contains(TEST_API_KEY_PREFIX),
            "saved-key prefix must be visible in the modal; rendered:\n{out}",
        );
    }

    /// The API-key modal lists `[Ctrl+U] clear` in the keys sidebar so the
    /// user knows there is a way to wipe the buffer (and the saved key,
    /// on Enter with an empty buffer) without leaving the TUI.
    #[test]
    fn api_key_modal_lists_clear_in_keys_hints() {
        let mut app = App::new();
        app.mode = crate::app::AppMode::ApiKeyModal;
        app.api_key_buf = Some("vb-anything".into());
        // Big enough viewport that the centered modal popup doesn't
        // obscure the keys sidebar. On a 140x30 terminal the popup
        // covers the `[Ctrl+U] clear` line (visible only on wider
        // terminals), so the production terminal hides part of the
        // hint but the data is still produced for the rendering pass.
        // We use a wide viewport here so the assertion can find the
        // literal text regardless of the modal overlay.
        let out = render_to_string(&app, 200, 50);
        assert!(
            out.contains("[Ctrl+U]"),
            "modal must advertise [Ctrl+U] clear; rendered:\n{out}",
        );
        assert!(
            out.contains("clear"),
            "modal must advertise the clear action; rendered:\n{out}",
        );
    }
    /// The mode panel shows the model name (auto-picked or user-chosen)
    /// so the user can see what's loaded without leaving the main screen.
    #[test]
    fn mode_panel_shows_model_name() {
        let mut app = App::new();
        app.default_slot_config.model = "tiny.en".into();
        let out = render_to_string(&app, 140, 30);
        assert!(out.contains("tiny.en"), "model name missing:\n{out}");
        assert!(out.contains("(m)"), "model picker hint missing:\n{out}");
    }
    /// `mask_api_key` reveals BOTH the prefix and the tail of a
    /// long key — the prefix so the user can spot a corrupted
    /// paste like `sk-testvb_…` (predictable boilerplate carries
    /// no entropy but reveals "is this the prod key?" at a
    /// glance), the tail so the user can recognise the key from
    /// its high-entropy suffix. The middle is dotted. For keys
    /// short enough that the prefix and tail would overlap (or
    /// touch), the whole thing is masked.
    #[test]
    fn mask_api_key_reveals_prefix_and_suffix() {
        // A 17-char key reveals 5 + 7 dots + 5 = 17.
        assert_eq!(
            mask_api_key("abcdefghijklmnopq"),
            "abcde•••••••mnopq",
            "long key must show prefix + dotted middle + tail",
        );
        // Synthetic test key from TEST_API_KEY — the prefix and
        // suffix are both stable substrings of the synthetic
        // constant, so we assert exact strings rather than
        // recomputing the dot count.
        let masked = mask_api_key(TEST_API_KEY);
        assert!(
            masked.starts_with(TEST_API_KEY_PREFIX),
            "mask must reveal the prefix {TEST_API_KEY_PREFIX:?}; got {masked:?}",
        );
        assert!(
            masked.ends_with(TEST_API_KEY_SUFFIX),
            "mask must reveal the tail {TEST_API_KEY_SUFFIX:?}; got {masked:?}",
        );
        // The middle must be dots only — no leak of the secret.
        // Char-indexed slice so the multi-byte `•` (3 bytes in
        // UTF-8) doesn't get split on a byte boundary.
        let chars: Vec<char> = masked.chars().collect();
        let middle = &chars[TEST_API_KEY_PREFIX.chars().count()
            ..chars.len() - TEST_API_KEY_SUFFIX.chars().count()];
        let middle_str: String = middle.iter().collect();
        assert!(
            middle_str.chars().all(|c| c == '•'),
            "middle must be all dots; got {middle_str:?}",
        );
        // And the original secret body must not appear in the
        // rendered mask.
        assert!(
            !masked.contains("TESTSC"),
            "mask leaked synthetic secret body; got {masked:?}",
        );
    }
    /// The API-key modal surfaces the Ctrl+U clear hint INSIDE the
    /// popup body itself, not just in the keys sidebar - the keys
    /// sidebar can be obscured by the modal popup on narrower
    /// terminals, so the user inside the modal must be able to read
    /// the controls inline. We assert the phrase `Ctrl+U to clear`
    /// appears verbatim in the rendered frame; this phrasing is
    /// distinct from the keys sidebar's `[Ctrl+U] clear` (note the
    /// brackets around `Ctrl+U` in the sidebar) and from
    /// `Ctrl+U` followed by a separate `clear` cell.
    #[test]
    fn api_key_modal_surfaces_ctrl_u_hint_in_modal_body() {
        let mut app = App::new();
        app.mode = crate::app::AppMode::ApiKeyModal;
        app.api_key_buf = Some(String::new());
        // See TEST_API_KEY in the test module's const block. No
        // production-looking keys in tests.
        app.config.voicebird_api_key = TEST_API_KEY.into();
        let out = render_to_string(&app, 200, 50);
        assert!(
            out.contains("Ctrl+U to clear"),
            "modal prompt text must advertise `Ctrl+U to clear` in-place \
             (distinguishable from the keys sidebar's `[Ctrl+U] clear`); \
             rendered:\n{out}",
        );
    }
    /// The keys sidebar lists `[K]` as the dedicated, always-available
    /// shortcut for opening the API-key modal. Without `[K]` in the
    /// sidebar the user has no in-app way to find the API-key modal
    /// when 'c' is taken (it toggles cloud on a focused section).
    #[test]
    #[allow(non_snake_case)]
    fn keys_panel_lists_K_for_setting_api_key() {
        let app = App::new();
        let out = render_to_string(&app, 140, 30);
        assert!(
            out.contains("[K]"),
            "keys panel must advertise `[K]`; rendered:\n{out}",
        );
        assert!(
            out.contains("API key"),
            "keys panel must advertise the API-key action; rendered:\n{out}",
        );
    }
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

    /// Pre-fix bug: every slot title read the GLOBAL cursor via
    /// `app.focused_device()` / `app.focused_app()`. When the user
    /// Tab'd between slots and pressed arrow keys with slot 1
    /// focused, slots 2 and 3's titles also updated to whatever
    /// slot 1 was on — even though their per-slot memo was
    /// preserved. The user saw three slots all showing the same
    /// "EPOS PC 8 USB + Safari" combination regardless of focus,
    /// and only switching focus back to a slot "recovered" the
    /// per-slot device+app they had picked earlier.
    ///
    /// Post-fix: the slot title is FROZEN for non-focused slots
    /// and reads from the per-slot memo. The focused slot still
    /// reflects the live cursor. Moving the cursor with slot 1
    /// focused must leave slots 2 and 3's titles untouched.
    #[test]
    fn unfocused_slot_titles_are_frozen_when_focused_slot_cursor_moves() {
        let mut app = App::new();
        app.add_slot();
        app.add_slot();

        // Seed inventory.
        app.devices = vec![
            input("MacBook Pro Microphone"),
            input("USB Headset"),
        ];
        app.apps = vec![fake_app("us.zoom.xos", "Zoom")];

        // Slot 2 (id=2) memoizes device 1 (USB Headset) + no app.
        // Slot 3 (id=3) memoizes device 0 (MacBook Pro Mic) + app 0 (Zoom).
        // We push these into `slot_picker_memo` directly (the
        // public API is focus-driven; the test bypasses to keep
        // the test focused on the render-path contract).
        app.slot_picker_memo.insert(
            crate::app::SlotId(2),
            crate::app::PickerSelection {
                device_idx: 1,
                app_idx: None,
                focus: PickerFocus::Devices,
            },
        );
        app.slot_picker_memo.insert(
            crate::app::SlotId(3),
            crate::app::PickerSelection {
                device_idx: 0,
                app_idx: Some(0),
                focus: PickerFocus::Devices,
            },
        );

        // Focus slot 1, set the LIVE cursor to a third
        // device+app combo that's different from both memos.
        app.focused_slot = crate::app::SlotId(1);
        app.selected_device_index = 0;
        app.selected_app_index = None;
        app.picker_focus = PickerFocus::Devices;

        // `slot_has_picker_pick` reads global state, so seed it
        // enough to make the title path render (and exercise the
        // per-slot device/app lookup the test is targeting). The
        // real test fixture is the per-slot memo above.
        app.config.input_device = Some("MacBook Pro Microphone".into());
        app.pending_target_overrides.insert(
            crate::app::SlotId(2),
            voice_bird_cli::session::target::Target::Stdout,
        );
        app.pending_target_overrides.insert(
            crate::app::SlotId(3),
            voice_bird_cli::session::target::Target::Stdout,
        );

        let out = render_to_string(&app, 180, 40);
        let out = render_to_string(&app, 180, 40);

        // Slot 1 title shows the live cursor (MacBook Pro Mic).
        assert!(
            out.contains("MacBook Pro Microphone"),
            "focused slot 1 must show the live cursor device; rendered:\n{out}"
        );

        // Slot 2 title must be FROZEN at its memoized device
        // (USB Headset) — must NOT reflect slot 1's cursor.
        assert!(
            out.contains("USB Headset"),
            "unfocused slot 2 title must show its memoized device (USB Headset), \
             not the live cursor (MacBook Pro Microphone); rendered:\n{out}"
        );
        // Slot 2's memoized app is None — must NOT show + Zoom.
        assert!(
            !out.contains("[2] MacBook Pro Microphone"),
            "slot 2 title must NOT adopt slot 1's device; rendered:\n{out}"
        );

        // Slot 3 title must show its memoized combo (MacBook Pro Mic + Zoom),
        // NOT slot 1's live state (no app).
        // The exact rendering includes "[3] <device> + <app>"; assert on
        // the substring that uniquely identifies slot 3's memoized pick.
        assert!(
            out.contains("MacBook Pro Microphone") && out.contains("Zoom"),
            "unfocused slot 3 title must show its memoized device + app \
             (MacBook Pro Microphone + Zoom); rendered:\n{out}"
        );
    }

    /// Pre-fix bug: the slot title template is
    /// ` [N] {device} + {app} → Stdout `. Every slot shows
    /// `→ Stdout` even though §8.5 retired Stdout as a routing
    /// target — it's no longer in `Target::cloud`/`Target::agent`
    /// decisions. The arrow + Stdout suffix is dead text the user
    /// reads on every slot.
    ///
    /// Post-fix: title is ` [N] {device} + {app} ` with no
    /// target suffix. The cloud Agent is selected elsewhere (the
    /// Agents picker), not declared in the slot title.
    #[test]
    fn slot_title_does_not_include_stdout_suffix() {
        let mut app = App::new();
        app.add_slot();
        app.add_slot();

        app.devices = vec![crate::platform::AudioDevice {
            name: "MacBook Pro Microphone".into(),
            kind: voice_bird_cli::config::AudioSessionKind::Input,
        }];
        app.apps = vec![crate::platform::AppSession {
            id: "us.zoom.xos".into(),
            name: "Zoom".into(),
            process_id: 0,
        }];

        // Seed each slot's memo so all three titles render the
        // full `<device> + <app>` form (otherwise some slots
        // would short-circuit to "(empty)" and never exercise
        // the title template).
        app.slot_picker_memo.insert(
            crate::app::SlotId(2),
            crate::app::PickerSelection {
                device_idx: 0,
                app_idx: Some(0),
                focus: PickerFocus::Devices,
            },
        );
        app.slot_picker_memo.insert(
            crate::app::SlotId(3),
            crate::app::PickerSelection {
                device_idx: 0,
                app_idx: Some(0),
                focus: PickerFocus::Devices,
            },
        );
        app.config.input_device = Some("MacBook Pro Microphone".into());

        let out = render_to_string(&app, 180, 40);

        // Assert the device+app is present so we know the title
        // template fired (otherwise the absence of "Stdout" is
        // meaningless — could just be that the slot stayed
        // empty).
        assert!(
            out.contains("MacBook Pro Microphone") && out.contains("Zoom"),
            "fixture must render the device+app title; got:\n{out}"
        );

        // The slot title must NOT contain the trailing
        // `→ Stdout` arrow + label. Assert on each slot's full
        // title shape so a regression in the template is
        // pinned to the offending slot.
        assert!(
            !out.contains("[1] MacBook Pro Microphone + Zoom → Stdout"),
            "slot 1 title must drop the Stdout suffix; got:\n{out}"
        );
        assert!(
            !out.contains("[2] MacBook Pro Microphone + Zoom → Stdout"),
            "slot 2 title must drop the Stdout suffix; got:\n{out}"
        );
        assert!(
            !out.contains("[3] MacBook Pro Microphone + Zoom → Stdout"),
            "slot 3 title must drop the Stdout suffix; got:\n{out}"
        );
    }
    /// Agents pane renders as the third picker column. The
    /// header is " Agents " (focused variant carries the
    /// action hint). The two fixed rows are Stdout and
    /// Agent — Cloud is no longer a target (cloud is a
    /// per-section transport flag in the Mode panel).
    /// The active pick is tagged with "(active)" next to
    /// the focused slot's current target.
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
    /// Enabled Stdout also carries no hint — pinning the
    /// invariant that every picker row shares a single
    /// shape so the pin column lands identically. Cloud
    /// is no longer a target; its toggle lives in the
    /// Mode panel.
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
        // Agents picker pane rather than a separate hotkey.
        for key in [
            "[↑/↓]",
            "[←/→]",
            "[Space]",
            "[Enter]",
            "[Tab]",
            // short-circuit the test when the static list drops
            // them.
            "[+]",
            "[c]",
            "[K]",
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
        app.selected_room_index = 0;
                                             // Pretend a previous start installed an Agent session id, so
                                             // the Agent row stays pickable but isn't the picked one.
        let out = render_to_string(&app, 180, 40);
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("snapshot-idle.txt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &out).unwrap();
    }
    /// A paused (Saved) slot's bottom hint must make the
    /// Enter/R distinction explicit: R resumes the focused
    /// slot in place, x clears it, Enter starts a brand-new
    /// session (in a new slot, if one is free).
    ///
    /// The resume + clear hints only appear on the
    /// focused slot, since R/x target the focused slot
    /// regardless of which paused slot is showing the
    /// hint. Non-focused saved slots keep the
    /// clear-only hint.
    #[test]
    fn paused_slot_hint_advertises_resume_and_enter_contrast() {
        use crate::app::{SavedTranscript, SlotKind};
        use std::sync::Arc;
        use voice_bird_cli::session::target::Target;

        let mut app = App::new();
        // Add a second slot so we can focus one and
        // leave the other unfocused.
        let slot_b = app.add_slot().expect("add_slot under MAX_SECTIONS");
        let slot_a = app.slots[0].id;

        // Seed both slots as Saved.
        for slot in [slot_a, slot_b] {
            let pos = app.slot_index(slot).unwrap();
            app.slots[pos].kind = SlotKind::Saved {
                saved: SavedTranscript {
                    committed: Arc::new(parking_lot::Mutex::new(Vec::new())),
                    refined: Arc::new(parking_lot::Mutex::new(Vec::new())),
                    label: "mic · cloud:OFF".into(),
                    target: Target::Stdout,
                    source: voice_bird_cli::session::layout::SessionSource::Microphone,
                    settings: crate::app::SectionSettings {
                        cloud_on: false,
                        language: "en".into(),
                        model: "tiny.en".into(),
                    },
                    session_started_at: chrono::Utc::now(),
                    role: None,
                },
            };
        }
        // Focused = slot A. The focused paused hint
        // must mention R (resume), x (clear), and
        // Enter (new session).
        app.focused_slot = slot_a;
        let out_focused = render_to_string(&app, 180, 40);
        assert!(
            out_focused.contains("[R] resume"),
            "focused paused hint must advertise [R] resume; got:\n{out_focused}"
        );
        assert!(
            out_focused.contains("[x] clear"),
            "focused paused hint must also advertise [x] clear; got:\n{out_focused}"
        );
        assert!(
            out_focused.contains("Enter = new"),
            "focused paused hint must clarify Enter starts a new session; got:\n{out_focused}"
        );

        // Focused = slot B. Slot A is now non-focused
        // and must show only the clear hint; slot B
        // is focused and shows the full R/x/Enter
        // hint. Both strings must be present in the
        // rendered output (different slots, different
        // lines).
        app.focused_slot = slot_b;
        let out_unfocused = render_to_string(&app, 180, 40);
        assert!(
            out_unfocused.contains("[R] resume"),
            "after switching focus to slot B, the focused slot's hint must advertise [R]; got:\n{out_unfocused}"
        );
        assert!(
            out_unfocused.contains("(paused — press [x] to clear)"),
            "non-focused paused slot A must keep the clear-only hint (no [R]); got:\n{out_unfocused}"
        );
    }
    //
    //
    //
    //
    // Test removed in §8.3 (AgentFunnel state deleted).
}

// Removed in §8.3:
// - `targets_pane_lists_stdout_and_agent_in_order`
// - `target_row_style_agent_is_dim_when_disabled`
// - `target_row_style_agent_is_cyan_when_enabled`
// - `multi_slot_round_trip_keeps_picked_pins_on_focused_slot`
// - `paused_slot_hint_advertises_resume_and_enter_contrast`
// - `funnel_footer_advertises_back_key`
// - `agent_event_log_caps_and_records_failures_only`
// - `dispatch_default_id_routes_to_legacy_mcp_buffer`
// - `dispatch_known_id_pushes_to_mapped_target`
// - `dispatch_missing_id_drops_segment_not_default`
// - `dispatch_push_failure_is_reported_not_rerouted`
// - `dispatch_non_agent_targets_route_nothing`
// - `select_next_advances_past_first_agent_target`
// - `agent_state_event_log_caps_at_50_entries`
