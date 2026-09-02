//! Test helpers. Compiled unconditionally (not `#[cfg(test)]`) so the
//! `tests/` integration crate can use them too.

use ratatui::{backend::TestBackend, Terminal};

use crate::{state::UiState, ui};

/// Render `state` into a `w`×`h` in-memory terminal and return the cell
/// grid as text, one line per row. Port of the old `ui.rs` test helper.
pub fn render_to_string(state: &UiState, w: u16, h: u16) -> String {
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal.draw(|f| ui::render(f, state)).expect("draw");
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
