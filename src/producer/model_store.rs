//! Installs verified model bytes and publishes worker outcomes.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::task::JoinHandle;

use crate::bus::{AppEvent, EventSender};
use crate::db::downloads::{CancelCheck, CancelProbe};
use crate::download::{truncate_error, DownloadError};
use crate::picker::ModelEntry;
use crate::transcription_models::{handler_for, ModelStore};

pub fn start(
    model_store: Arc<dyn ModelStore>,
    entry: &'static ModelEntry,
    staged: PathBuf,
    mut probe: CancelProbe,
    attempt: u32,
    tx: EventSender,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let model = entry.id;
        if handler_for(entry.format).install_is_slow() {
            tx.publish(AppEvent::DownloadInstalling { attempt, model });
        }
        let result = tokio::task::spawn_blocking(move || {
            // Use a fresh probe here: cancellation after fetch must be
            // observed even by a fast handler that only renames a file.
            if probe.is_cancelled() {
                return Err(DownloadError::Cancelled);
            }
            model_store.install(entry, &staged, &mut probe)
        })
        .await
        .unwrap_or_else(|error| Err(DownloadError::Install(error.to_string())));
        match result {
            Ok(()) => tx.publish(AppEvent::DownloadSucceeded { attempt, model }),
            Err(DownloadError::Cancelled) => {
                tx.publish(AppEvent::DownloadCancelled { attempt, model });
            }
            Err(error) => tx.publish(AppEvent::DownloadFailed {
                attempt,
                model,
                error: truncate_error(&error.to_string()),
            }),
        }
    })
}
