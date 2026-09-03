//! `voice-bird-next`: the incremental rewrite of the Voice Bird TUI.
//!
//! Ground rules (see README.md):
//! - [`state::UiState`] is plain data — no `Instant`, runtime handles or
//!   channels — so every render is deterministic and testable.
//! - [`ui::render`] is a pure function of `&UiState`.
//! - Side effects (terminal, audio, engines, cloud) live in `main.rs` or
//!   behind traits, never inside the state struct.
//! - Input maps keys to [`bus::AppEvent`]; the bus transports them; a pure
//!   reducer on `UiState::apply` folds them in. Input never mutates state.
pub mod bus;
pub mod input;
pub mod state;
pub mod testing;
pub mod ui;
