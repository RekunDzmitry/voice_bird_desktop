//! Reconcile desired recording blocks with the producer's worker registry.
use std::sync::Arc;

use crate::audio::CaptureSource;
use crate::bus::{AppEvent, EventSender};
use crate::consumer::ui_view::{BlockState, UiView};
use crate::producer::recording::{
    next_recording_id, RecordingProducer, RecordingRequest, RecordingServices,
};
use crate::transcription_models::ModelStore;

pub struct RecordingConsumer {
    pub producer: RecordingProducer,
    model_store: Arc<dyn ModelStore>,
}

impl RecordingConsumer {
    pub fn new(services: RecordingServices, model_store: Arc<dyn ModelStore>) -> Self {
        Self {
            producer: RecordingProducer::new(services),
            model_store,
        }
    }

    pub fn consume(&mut self, event: &AppEvent, view: &mut UiView, tx: &EventSender) {
        match event {
            AppEvent::TranscriptionRequested { request } => {
                self.producer.dispatch(request);
            }
            AppEvent::RecordingStopped { block, recording } => {
                self.producer.complete(*block, *recording, false);
            }
            AppEvent::RecordingFailed {
                block, recording, ..
            } => {
                self.producer.complete(*block, *recording, true);
            }
            _ => {}
        }
        self.reconcile(view, tx);
    }

    pub fn reconcile(&mut self, view: &mut UiView, tx: &EventSender) {
        for block in self.producer.blocks() {
            if view.should_quit
                || !view.blocks.iter().any(|candidate| {
                    candidate.id == block && matches!(candidate.state, BlockState::Recording { .. })
                })
            {
                self.producer.stop(block);
            }
        }
        self.producer.reap_stopped();
        if view.should_quit {
            return;
        }
        let requests: Vec<_> = view
            .blocks
            .iter()
            .filter_map(|block| {
                let BlockState::Recording {
                    language,
                    recording,
                    ..
                } = block.state
                else {
                    return None;
                };
                if self.producer.contains(block.id) || recording.is_some() {
                    return None;
                }
                Some((
                    block.id,
                    language,
                    CaptureSource::from_selection(block.source.as_ref()),
                ))
            })
            .collect();
        for (block, language, source) in requests {
            let recording = next_recording_id();
            view.begin_session(block, recording);
            match source {
                Ok(source) => self.producer.start(
                    RecordingRequest {
                        block,
                        recording,
                        source,
                        language,
                        live_model: self.model_store.installed_path(language.live),
                        refine_model: self.model_store.installed_path(language.refine),
                    },
                    tx,
                ),
                Err(error) => tx.publish(AppEvent::RecordingFailed {
                    block,
                    recording,
                    error: format!("{error:#}"),
                }),
            }
        }
    }
}
