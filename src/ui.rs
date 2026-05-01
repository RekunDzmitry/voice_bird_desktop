use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};
use std::time::Instant;

use crate::app::{App, AppMode, RecordingStatus};

pub fn render(f: &mut Frame, app: &App) {
    if app.mode == AppMode::ModelPicker {
        render_model_picker(f, f.area(), app);
        return;
    }
    if app.mode == AppMode::Settings {
        crate::settings_view::render(f, f.area(), app);
        return;
    }

    // When a banner is set (engine error surfaced to the user), insert a
    // single-line red strip between the tentative zone and the footer.
    // Plan deviation: the plan originally called for a silent
    // WhisperKit→whisper-rs restart on error; we surface errors via this
    // banner instead and let the user press `r` to retry.
    let has_banner = app.banner.is_some();
    let has_reminder = app
        .cloud_reminder_until
        .map(|t| Instant::now() < t)
        .unwrap_or(false);
    // Devices panel grows with session count, capped to keep transcript
    // space. Always show at least the header frame even when empty.
    let devices_h = (app.sessions.len() as u16 + 2).clamp(3, 8);
    let mut constraints: Vec<Constraint> = vec![
        Constraint::Length(3),         // [0] header
        Constraint::Length(devices_h), // [1] devices panel
        Constraint::Min(4),            // [2] committed zone
        Constraint::Length(3),         // [3] tentative line
    ];
    if has_reminder {
        constraints.push(Constraint::Length(1)); // reminder
    }
    if has_banner {
        constraints.push(Constraint::Length(1)); // banner
    }
    constraints.push(Constraint::Length(1)); // footer/keys

    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(f.area());

    render_header(f, root[0], app);
    render_devices(f, root[1], app);
    render_committed(f, root[2], app);
    render_tentative(f, root[3], app);

    // Running index for the optional rows after the tentative line.
    let mut next_slot: usize = 4;

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
    render_footer(f, root[next_slot], app);
}

pub fn render_model_picker(f: &mut Frame, area: Rect, app: &App) {
    let catalog = voice_bird::transcription::models::Catalog::builtin();
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

    let p = Paragraph::new(items)
        .block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(p, area);

    // Popup: download progress / error if a download is in flight.
    if let Some(progress_arc) = app
        .picker
        .as_ref()
        .and_then(|p| p.downloading.as_ref())
    {
        let g = progress_arc.lock();
        let msg = if let Some(err) = &g.error {
            format!("Download failed: {}: {}", g.model_id, err)
        } else {
            let pct = g
                .total
                .map(|t| (g.bytes * 100 / t.max(1)))
                .unwrap_or(0);
            format!("Downloading {}: {pct}%", g.model_id)
        };
        let popup = Paragraph::new(msg)
            .block(Block::default().borders(Borders::ALL).title(" Download "));
        let popup_area = centered(60, 3, area);
        f.render_widget(Clear, popup_area);
        f.render_widget(popup, popup_area);
    }
}

fn render_devices(f: &mut Frame, area: Rect, app: &App) {
    use crate::platform::AudioSessionKind;

    let selected = app.selected_index;
    let saved = app.config.input_device.as_deref();
    let is_recording = matches!(app.status, RecordingStatus::Recording);

    let items: Vec<Line> = if app.sessions.is_empty() {
        vec![Line::from(Span::styled(
            "  (no audio devices found — press [r] to refresh)",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        app.sessions
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let is_cursor = i == selected && !is_recording;
                let marker = if is_cursor { "▶ " } else { "  " };
                let is_saved = Some(s.device_name.as_str()) == saved;
                let kind_tag = match s.kind {
                    AudioSessionKind::Input => Span::styled(
                        " [input] ",
                        Style::default().fg(Color::Cyan),
                    ),
                    AudioSessionKind::Output => Span::styled(
                        " [output/loopback] ",
                        Style::default().fg(Color::Magenta),
                    ),
                };
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
                    Span::styled(s.device_name.clone(), name_style),
                    kind_tag,
                    Span::styled(saved_tag, Style::default().fg(Color::DarkGray)),
                ])
            })
            .collect()
    };

    let title = " Audio devices — [↑/↓] select  [Enter] start  [r] refresh ";
    let p = Paragraph::new(items).block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(p, area);
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
        Span::raw(app.config.default_model.as_str()),
        Span::raw("  │  "),
        Span::raw(format!("in: {}", device_label)),
        Span::raw("  │  "),
        Span::raw(format!(
            "engine: {}",
            // While recording / after a run, show the engine that was
            // actually selected (e.g. `whisper_rs` when the WhisperKit
            // sidecar was absent). Idle → fall back to the config
            // preference so the user can see what will be used next.
            if app.engine_kind.is_empty() {
                app.config.engine_prefer.as_str()
            } else {
                app.engine_kind.as_str()
            }
        )),
        Span::raw("  │  "),
        Span::styled(status, status_style(&app.status)),
        Span::raw("  │  "),
        Span::raw(timer),
    ]);

    if app.cloud_broadcast_active {
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

fn render_committed(f: &mut Frame, area: Rect, app: &App) {
    let committed = app.committed.lock();
    let refined = app.refined.lock();

    // Refined segments lead (authoritative). The streaming `committed`
    // vec is cleared every time a new refined segment lands, so what's
    // in it now is just the live tail since the last refinement — show
    // it below, in a dimmed style, to signal it may get replaced.
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

    // Scroll math. Paragraph with Wrap splits a single logical line into
    // multiple display rows at terminal width — we can't know the exact
    // count without re-running wrap logic here, so we approximate line
    // count by (logical lines) × (1 + avg_text_len / inner_width). Good
    // enough for follow/clamp behavior; scroll remains usable even when
    // the approximation is off by a few rows.
    let inner_w = area.width.saturating_sub(2).max(1) as usize;
    let approx_rows: u16 = lines
        .iter()
        .map(|l| {
            let w: usize = l.spans.iter().map(|s| s.content.chars().count()).sum();
            (w.div_ceil(inner_w)).max(1) as u16
        })
        .sum();
    let visible = area.height.saturating_sub(2); // minus top+bottom border
    let max_scroll = approx_rows.saturating_sub(visible);
    let scroll_y = if app.transcript_follow {
        max_scroll
    } else {
        app.transcript_scroll.min(max_scroll)
    };

    let mut title = if refined.is_empty() {
        " Transcript ".to_string()
    } else {
        format!(" Transcript (refined: {}) ", refined.len())
    };
    if !app.transcript_follow && max_scroll > 0 {
        title = format!("{}[scroll {}/{}] ", title, scroll_y, max_scroll);
    }
    let p = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll_y, 0))
        .block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(p, area);
}

fn render_tentative(f: &mut Frame, area: Rect, app: &App) {
    let text = app.tentative.lock().clone();
    let p = Paragraph::new(Line::from(Span::styled(
        format!("… {}", text),
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC),
    )))
    .block(Block::default().borders(Borders::ALL));
    f.render_widget(p, area);
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

fn render_footer(f: &mut Frame, area: Rect, app: &App) {
    let keys = match &app.status {
        RecordingStatus::Idle => {
            "[↑/↓] device  [Enter] start  [r] refresh  [s] settings  [m] model  [PgUp/PgDn/Home/End] scroll  [q] quit  [?] help"
        }
        RecordingStatus::Recording => {
            "[s] stop  [↑/↓/PgUp/PgDn/Home/End] scroll  [q] quit"
        }
        RecordingStatus::Error(_) => "[Enter] retry  [r] refresh  [q] quit",
    };
    let p = Paragraph::new(keys).style(Style::default().fg(Color::Gray));
    f.render_widget(p, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::platform::AudioSession;
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

    fn fake_sessions(names: &[&str]) -> Vec<AudioSession> {
        use crate::platform::AudioSessionKind;
        names
            .iter()
            .map(|n| AudioSession {
                device_name: (*n).into(),
                app_name: (*n).into(),
                process_id: 0,
                kind: AudioSessionKind::Input,
            })
            .collect()
    }

    #[test]
    fn devices_panel_renders_with_title_and_names() {
        let mut app = App::new();
        app.mode = crate::app::AppMode::Normal;
        app.sessions = fake_sessions(&["MacBook Pro Microphone", "BlackHole 2ch"]);
        app.selected_index = 1;
        let out = render_to_string(&app, 120, 30);
        assert!(out.contains("Audio devices"), "title missing:\n{out}");
        assert!(out.contains("MacBook Pro Microphone"), "device 0 missing:\n{out}");
        assert!(out.contains("BlackHole 2ch"), "device 1 missing:\n{out}");
        assert!(out.contains("[input]"), "input kind tag missing:\n{out}");
    }

    #[test]
    fn footer_shows_enter_and_refresh_when_idle() {
        let app = App::new();
        let out = render_to_string(&app, 120, 30);
        assert!(out.contains("[Enter] start"), "enter hint missing:\n{out}");
        assert!(out.contains("[r] refresh"), "refresh hint missing:\n{out}");
    }

    #[test]
    fn header_shows_input_label() {
        let app = App::new();
        let out = render_to_string(&app, 120, 30);
        assert!(out.contains("in:"), "'in:' label missing:\n{out}");
    }

    #[test]
    fn output_devices_show_loopback_tag() {
        use crate::platform::AudioSessionKind;
        let mut app = App::new();
        app.sessions = vec![
            AudioSession {
                device_name: "MacBook Pro Microphone".into(),
                app_name: "MacBook Pro Microphone".into(),
                process_id: 0,
                kind: AudioSessionKind::Input,
            },
            AudioSession {
                device_name: "MacBook Pro Speakers".into(),
                app_name: "MacBook Pro Speakers".into(),
                process_id: 0,
                kind: AudioSessionKind::Output,
            },
        ];
        let out = render_to_string(&app, 120, 30);
        assert!(out.contains("MacBook Pro Speakers"), "output device missing:\n{out}");
        assert!(out.contains("[output/loopback]"), "output tag missing:\n{out}");
    }

    #[test]
    fn empty_device_list_prompts_refresh() {
        let mut app = App::new();
        app.sessions.clear();
        let out = render_to_string(&app, 120, 30);
        assert!(
            out.contains("no audio devices found"),
            "empty-list hint missing:\n{out}"
        );
    }
}
