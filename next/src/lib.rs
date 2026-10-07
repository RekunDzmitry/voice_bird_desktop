//! `voice-bird-next`: the incremental rewrite of the Voice Bird TUI.
//!
//! Ground rules (see README.md):
//! - [`consumer::UiView`] is plain data: no runtime handles or channels.
//! - [`ui::render`] is a pure function of the UI view.
//! - Consumers make accepted-event decisions and hand background work to producers.
//! - Every event is logged before SQLite gates domain transitions.
//! - Producers schedule workers and publish results; blocking work uses `spawn_blocking`.
pub mod audio_sources;
pub mod bus;
pub mod consumer;
pub mod db;
pub mod download;
pub mod event_log;
pub mod input;
pub mod language;
pub mod picker;
pub mod producer;
pub mod testing;
pub mod transcription_models;
pub mod ui;
