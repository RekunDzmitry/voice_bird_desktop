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
    // Picker grows with the larger of devices/apps, capped to keep
    // transcript space. The mode panel on the right shares this row and
    // needs at least 5 rows (top border + Cloud + Language + Model +
    // bottom border), so row 1 is floored at 8 to give the new two-pane
    // picker room for a few rows + scroll. Cap at 14 so transcripts
    // still get the bulk of the screen.
    let max_pane_len = app.devices.len().max(app.apps.len()) as u16;
    let devices_h = (max_pane_len + 2).clamp(8, 14);
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

    // Three rows stacked: devices/apps picker, the Targets chip strip
    // (one cell per slot), and the slot row. Heights are weighted so
    // the picker keeps its room while the chip strip stays compact.
    let workspace = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(devices_h),
            Constraint::Length(3),
            Constraint::Min(6),
        ])
        .split(main[0]);
    render_devices(f, workspace[0], app);
    render_targets_pane(f, workspace[1], app);
    render_sections(f, workspace[2], app);

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
}

/// Render the slot row. Each slot draws as its own block with a
/// per-slot Mode header in the title and the transcript + tentative
/// line stacked inside. Empty slots show a placeholder. The focused
/// slot's border is highlighted. Slot count grows with the Vec —
/// Phase A is hard-coded to three (the initial layout) but the
/// render path already iterates the Vec directly.
fn render_sections(f: &mut Frame, area: Rect, app: &App) {
    let n = app.slots.len();
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(vec![Constraint::Ratio(1, n as u32); n])
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

    // Title: per-slot Mode header (compact: cloud/lang/model summary).
    // Falls back to the saved label when a section was stopped but its
    // transcript is still preserved.
    let title_text = if let Some(s) = section {
        section_column_title(slot_id, Some(s))
    } else if let Some(saved) = saved {
        saved.label.clone()
    } else {
        section_column_title(slot_id, None)
    };
    let border_style = if is_focused {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else if section.is_some() {
        Style::default().fg(Color::Gray)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(title_text);

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

/// Title shown in the column's border. Includes the slot number, the
/// source label (or "(empty)" / "(paused)"), and a compact `Cloud:[ON] pl base.en`
/// summary so the user can read settings at a glance per column.
fn section_column_title(slot: SlotId, section: Option<&Section>) -> String {
    let n = slot.0;
    match section {
        None => format!(" [{n}] (empty) "),
        Some(s) => {
            let label = source_label(&s.source);
            let cloud = if s.settings.cloud_on { "ON" } else { "OFF" };
            let lang = if s.settings.cloud_on {
                s.settings.language.as_str()
            } else {
                "en"
            };
            // Cloud-only Windows has no local model to report
            // (compile-time constant branch).
            if cfg!(windows) {
                format!(" [{n}] {label} · cloud:{cloud} · {lang} ")
            } else {
                format!(
                    " [{n}] {label} · cloud:{cloud} · {lang} · {model} ",
                    model = s.settings.model
                )
            }
        }
    }
}

/// Short user-readable label for a section's source. Devices use the
/// saved input_device name from config when available; SessionSource
/// alone doesn't carry the device name post-Enter.
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

fn render_devices(f: &mut Frame, area: Rect, app: &App) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(area);
    render_devices_pane(f, cols[0], app);
    render_apps_pane(f, cols[1], app);
}
/// Read-only strip showing each slot's current `Target` (Stdout / Cloud).
/// Sits between the devices/apps picker and the slot row so the user
/// can see at a glance where every pane's transcript is going. Empty
/// slots render as a dimmed "—" placeholder. The strip mirrors the
/// slot row's N-column layout, so as the slot count grows the chips
/// grow with it.
fn render_targets_pane(f: &mut Frame, area: Rect, app: &App) {
    let n = app.slots.len();
    if n == 0 {
        return;
    }
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(vec![Constraint::Ratio(1, n as u32); n])
        .split(area);
    for (i, slot) in app.slots.iter().enumerate() {
        render_target_chip(f, cols[i], app, slot);
    }
}

fn render_target_chip(f: &mut Frame, area: Rect, app: &App, slot: &Slot) {
    let is_focused = slot.id == app.focused_slot;
    let is_omp_available = matches!(app.omp, voice_bird_cli::omp::OmpStatus::Ready { .. });
    let (label, style) = chip_label_style(slot.target(), is_omp_available);
    let border_style = if is_focused {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(format!(" [{}] target ", slot.id.0));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let p = Paragraph::new(Line::from(Span::styled(label, style)))
        .alignment(Alignment::Center);
    f.render_widget(p, inner);
}

/// Pure helper: chip text + style for a given `Target`. Extracted so
/// unit tests can verify the `Omp` arm without spinning up a full
/// `Section` (which would need a real audio engine + CaptureKeepAlive).
fn chip_label_style(
    target: Option<voice_bird_cli::session::target::Target>,
    is_omp_available: bool,
) -> (String, Style) {
    use voice_bird_cli::session::target::Target;
    match target {
        Some(Target::Stdout) => (
            "▸ Stdout".into(),
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
        ),
        Some(Target::Cloud) => (
            "▸ Cloud".into(),
            Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        ),
        Some(Target::Omp { .. }) => {
            // Cyan when the omp binary is on disk; dim gray when it
            // isn't (the chip is still informative as a tag for the
            // user's routing choice, but obviously inert).
            let style = if is_omp_available {
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            ("▸ Omp".into(), style)
        }
        None => ("—".into(), Style::default().fg(Color::DarkGray)),
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
    let saved = app.config.input_device.as_deref();
    let saved_kind = app.config.input_device_kind;

    let title = if focused {
        " Devices ▸ [↑/↓] select  [→] apps  [Enter] start "
    } else {
        " Devices  ([←] focus) "
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
            let is_saved = Some(d.name.as_str()) == saved && saved_kind == Some(d.kind);
            let saved_tag = if is_saved { "  (saved)" } else { "" };
            let name_style = if is_cursor {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().add_modifier(Modifier::BOLD)
            };
            Line::from(vec![
                Span::raw(marker),
                Span::styled(d.name.clone(), name_style),
                kind_tag,
                Span::styled(saved_tag, Style::default().fg(Color::DarkGray)),
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

    let title = if focused {
        " Apps ▸ [↑/↓] pick  [Space] none  [←] devices "
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

    // First row is the synthetic "(no app — device only)" entry. It's
    // rendered visually but isn't selectable via the apps cursor — Space
    // is the way to land on it.
    let mut items: Vec<Line> = Vec::with_capacity(app.apps.len() + 1);
    let none_active = app.selected_app_index.is_none() && focused;
    let none_marker = if none_active { "▶ " } else { "  " };
    items.push(Line::from(vec![
        Span::raw(none_marker),
        Span::styled(
            "(no app — device only)",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        ),
    ]));

    for (i, a) in app.apps.iter().enumerate() {
        let is_cursor = Some(i) == app.selected_app_index && focused;
        let marker = if is_cursor { "▶ " } else { "  " };
        let name_style = if is_cursor {
            Style::default()
                .fg(Color::Black)
                .bg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        };
        items.push(Line::from(vec![
            Span::raw(marker),
            Span::styled(a.name.clone(), name_style),
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
                hotkey_line("[r]", "refresh"),
                hotkey_line("[c]", cloud_key_label),
                hotkey_line("[l]", "language"),
            ];
            if local_keys {
                lines.push(hotkey_line("[m]", "model"));
                lines.push(hotkey_line("[e]", "export"));
                lines.push(hotkey_line("[p]", "path"));
            }
            lines.push(hotkey_line("[x]", "clear"));
            lines.push(hotkey_line("[q]", "quit"));
            lines.push(hotkey_line("[?]", "help"));
            lines
        }
        (true, _) => {
            let mut lines = vec![
                hotkey_line("[↑/↓]", "select"),
                hotkey_line("[←/→]", "pane"),
                hotkey_line("[Enter]", "add"),
                hotkey_line("[Tab]", "focus slot"),
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
            out.contains("[←] focus"),
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

    /// Three section columns render side-by-side with empty placeholders
    /// when no sections are running. Each column shows its slot number
    /// in the title.
    #[test]
    fn three_section_columns_show_empty_placeholders() {
        let app = App::new();
        let out = render_to_string(&app, 180, 40);
        assert!(out.contains("[1]"), "slot 1 title missing:\n{out}");
        assert!(out.contains("[2]"), "slot 2 title missing:\n{out}");
        assert!(out.contains("[3]"), "slot 3 title missing:\n{out}");
        assert!(out.contains("(empty"), "empty placeholder missing:\n{out}");
    }
    /// Targets strip sits between the picker and the slot row and shows
    /// one column per slot with the current routing target (Stdout /
    /// Cloud / —). With no active sections every chip renders an em-dash
    /// placeholder under its `[N] target` block title.
    #[test]
    fn targets_pane_renders_one_column_per_slot_with_dashes_when_idle() {
        let app = App::new();
        let out = render_to_string(&app, 180, 40);
        assert!(out.contains("[1] target"), "slot 1 chip title missing:\n{out}");
        assert!(out.contains("[2] target"), "slot 2 chip title missing:\n{out}");
        assert!(out.contains("[3] target"), "slot 3 chip title missing:\n{out}");
        assert!(
            out.contains("—"),
            "em-dash placeholder missing from chips:\n{out}"
        );
    }

    /// The chip helper returns cyan-bold `▸ Omp` when the user has
    /// picked `Target::Omp` and the omp binary is on disk; the same
    /// chip is dim gray when omp is missing (still informative as a
    /// tag for the user's routing choice).
    #[test]
    fn chip_label_style_omp_is_cyan_when_omp_available() {
        use voice_bird_cli::session::target::Target;
        let t = Target::Omp { session_id: "x".into() };
        let (label, style) = chip_label_style(Some(t), true);
        assert_eq!(label, "▸ Omp");
        assert_eq!(style.fg, Some(Color::Cyan));
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn chip_label_style_omp_is_dim_when_omp_missing() {
        use voice_bird_cli::session::target::Target;
        let t = Target::Omp { session_id: "x".into() };
        let (label, style) = chip_label_style(Some(t), false);
        assert_eq!(label, "▸ Omp");
        assert_eq!(style.fg, Some(Color::DarkGray));
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
        let app = App::new();
        let out = render_to_string(&app, 160, 30);
        for key in [
            "[↑/↓]",
            "[←/→]",
            "[Space]",
            "[Enter]",
            "[Tab]",
            "[r]",
            "[c]",
            "[l]",
            "[m]",
            "[e]",
            "[p]",
            "[x]",
            "[q]",
            "[?]",
        ] {
            assert!(out.contains(key), "hotkey {key} missing:\n{out}");
        }
    }
    /// Visual reference snapshot of the idle TUI at a common terminal
    /// size. Writes the rendered buffer to `target/snapshot-idle.txt` so
    /// a human can eyeball layout regressions. Asserts nothing — the
    /// targeted assertions above already cover behaviour; this is a
    /// plain `cargo test` snapshot to keep manual review cheap.
    #[test]
    fn snapshot_idle_layout_for_visual_review() {
        let mut app = App::new();
        app.devices = vec![
            input("MacBook Pro Microphone"),
            input("BlackHole 2ch"),
        ];
        let out = render_to_string(&app, 180, 40);
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("snapshot-idle.txt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &out).unwrap();
    }
}
