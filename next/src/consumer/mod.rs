//! Routes accepted events to event-specific consumers.
//!
//! Consumers may publish follow-up events. Those events return through the bus
//! and SQLite gate before another consumer handles them; no handler directly
//! invokes the next stage of a flow.

use std::sync::Arc;

use crate::bus::{AppEvent, EventSender};
use crate::db::Database;
use crate::producer::download::Downloader;
use crate::producer::sources::AudioSourcesCatalog;
use crate::transcription_models::ModelStore;

pub mod audio_sources;
pub mod downloads;
pub mod language;
pub mod ui_view;
pub use ui_view::UiView;

use audio_sources::AudioSourcesConsumer;
use downloads::DownloadsConsumer;
use language::LanguageConsumer;

/// Independent event consumers and their own collaborators.
pub struct Consumers {
    pub ui_view: UiView,
    pub audio_sources: AudioSourcesConsumer,
    pub language: LanguageConsumer,
    pub downloads: DownloadsConsumer,
}

impl Consumers {
    pub fn new(
        downloader: Arc<dyn Downloader>,
        model_store: Arc<dyn ModelStore>,
        audio_sources: Arc<dyn AudioSourcesCatalog>,
    ) -> Self {
        Self {
            ui_view: UiView::default(),
            audio_sources: AudioSourcesConsumer::new(audio_sources),
            language: LanguageConsumer,
            downloads: DownloadsConsumer::new(downloader, model_store),
        }
    }
}

/// Dispatches only events already accepted by the database gate.
pub struct Consumer {
    pub consumers: Consumers,
}

impl Consumer {
    pub fn new(consumers: Consumers) -> Self {
        Self { consumers }
    }

    pub fn consume(&mut self, events: &[AppEvent], db: &mut Database, tx: &EventSender) {
        for event in events {
            // A request can outlive its last waiter across bus passes. Do not
            // create a projection or claim for work nobody needs anymore.
            if let AppEvent::DownloadRequested { model: entry, .. } = event {
                if self.consumers.ui_view.should_quit
                    || !self.consumers.ui_view.blocks.iter().any(|block| {
                        block.pending_models().contains(&entry.id)
                    })
                {
                    continue;
                }
            }
            self.consumers.ui_view.apply(event);
            match event {
                AppEvent::RequestBlock if !self.consumers.ui_view.should_quit => {
                    self.consumers.audio_sources.request(tx);
                }
                AppEvent::BeginLanguage { block, language, .. }
                    if !self.consumers.ui_view.should_quit
                        && self.consumers.ui_view.blocks.iter().any(|candidate| candidate.id == *block) =>
                {
                    self.consumers.language.begin(*block, language, db, tx);
                }
                AppEvent::ModelMissing(entry) if !self.consumers.ui_view.should_quit => {
                    self.consumers.language.model_missing(entry, db, tx);
                }
                AppEvent::DownloadRequested { model, attempt } => {
                    self.consumers.downloads.request(model, *attempt, db, tx);
                }
                AppEvent::DiscardInflight { model } => {
                    self.consumers.downloads.discard_inflight(model);
                }
                _ => {}
            }
        }
    }
}
