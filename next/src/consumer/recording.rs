//! Translate desired recording blocks into identity-scoped bus commands.
use std::sync::Arc;

use crate::audio::CaptureSource;
use crate::bus::{next_monotonic_id, AppEvent, EventSender, RecordingRequest};
use crate::consumer::ui_view::{BlockState, UiView};
use crate::db::{recordings, Database};
use crate::transcription_models::ModelStore;

pub struct RecordingConsumer {
    model_store: Arc<dyn ModelStore>,
}

impl RecordingConsumer {
    pub fn new(model_store: Arc<dyn ModelStore>) -> Self {
        Self { model_store }
    }

    pub fn consume(
        &self,
        view: &mut UiView,
        db: &mut Database,
        tx: &EventSender,
    ) -> rusqlite::Result<()> {
        // Snapshot before mutating request rows; SQLite owns the lifecycle,
        // including commands queued for the next bus pass.
        let requests = recordings::requests(db)?;
        for request in &requests {
            let wanted = !view.should_quit
                && view.blocks.iter().any(|candidate| {
                    candidate.id == request.block
                        && matches!(candidate.state, BlockState::Recording { .. })
                });
            if request.ended {
                if !wanted || request.stopping || request.failed {
                    recordings::release_ended(db, request.block, request.recording)?;
                }
            } else if !wanted
                && !request.stopping
                && recordings::reserve_stop(db, request.block, request.recording)?
                && !request.stop_admitted
            {
                tx.publish(AppEvent::RecordingStopRequested {
                    block: request.block,
                    recording: request.recording,
                });
            }
        }
        if view.should_quit {
            return Ok(());
        }
        for index in 0..view.blocks.len() {
            let block = &view.blocks[index];
            let BlockState::Recording {
                language,
                recording,
                ..
            } = block.state
            else {
                continue;
            };
            if recording.is_some()
                || requests.iter().any(|request| {
                    request.block == block.id
                        && (!request.ended || (!request.stopping && !request.failed))
                })
            {
                continue;
            }
            let id = block.id;
            let recording = next_monotonic_id();
            if !recordings::reserve_start(db, id, recording)? {
                continue;
            }
            view.begin_session(id, recording);
            let source = CaptureSource::from_selection(view.blocks[index].source.as_ref());
            match source {
                Ok(source) => tx.publish(AppEvent::RecordingRequested {
                    request: Arc::new(RecordingRequest {
                        block: id,
                        recording,
                        source,
                        language,
                        live_model: self.model_store.installed_path(language.live),
                        refine_model: self.model_store.installed_path(language.refine),
                    }),
                }),
                Err(error) => tx.publish(AppEvent::RecordingFailed {
                    block: id,
                    recording,
                    error: format!("{error:#}"),
                }),
            }
        }
        Ok(())
    }
}
