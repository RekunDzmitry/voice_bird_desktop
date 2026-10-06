//! Producers publish events; the consumer uses these handles to start work.

use std::sync::Arc;

use crate::transcription_models::ModelStore;

use self::download::Downloader;
use self::sources::{SourceCatalog, SourceManager};

pub mod download;
pub mod input;
pub mod model_watch;
pub mod sources;

/// Long-lived collaborators shared by command-driven producer work.
pub struct Producers {
    pub sources: SourceManager,
    pub downloads: Arc<dyn Downloader>,
    pub model_store: Arc<dyn ModelStore>,
}

impl Producers {
    pub fn new(
        downloads: Arc<dyn Downloader>,
        model_store: Arc<dyn ModelStore>,
        sources: Arc<dyn SourceCatalog>,
    ) -> Self {
        Self {
            sources: SourceManager::new(sources),
            downloads,
            model_store,
        }
    }
}
