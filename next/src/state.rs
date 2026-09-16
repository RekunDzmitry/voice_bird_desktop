use std::collections::BTreeMap;

use crate::bus::{AppEvent, FocusMove};
use crate::picker::{ModelPicker, PickerIntent};
use crate::store::DownloadPhase;

/// State of one block. The block is the unit of interaction: it picks a
/// model, waits for it, then records with it. Every stage renders inside
/// the block's own column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockState {
    /// Choosing a model. Carries its own picker — no shared overlay.
    Picking(ModelPicker),
    /// Waiting on `model`'s download. Progress is NOT stored here: it is
    /// read from [`UiState::downloads`], which is what lets two blocks on
    /// the same model render one shared bar.
    Waiting { model: &'static str },
    /// Mocked in this PR — no audio device; the state only changes
    /// rendering.
    Recording { model: &'static str },
    /// Last download for `model` failed. The resolver retries via `r`.
    Failed { model: &'static str, error: String },
}

/// A single inner column. Pure data — no threads, no handles, no clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub id: u32,
    pub state: BlockState,
}

impl Block {
    pub fn model(&self) -> Option<&'static str> {
        match &self.state {
            BlockState::Picking(_) => None,
            BlockState::Waiting { model }
            | BlockState::Recording { model }
            | BlockState::Failed { model, .. } => Some(model),
        }
    }
}

/// Render-side projection of one download. Mirrors
/// [`crate::store::DownloadRecord`] one-for-one; both are folded from
/// the same drained events so they cannot disagree. The renderer reads
/// from `UiState` (a pure data struct) and cannot reach the
/// `Mutex`-guarded repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadState {
    pub phase: DownloadPhase,
    pub bytes: u64,
    pub total: Option<u64>,
    /// Throughput observed between the last two throttle emits. Zero
    /// until the throttle has produced a second sample. The renderer
    /// uses it to label the gauge when `total` is `None` (otherwise
    /// the user sees a bar with no MB/s readout for the whole 1.6 GB
    /// pull).
    pub bytes_per_sec: u64,
}

impl DownloadState {
    /// Ratio in `0.0..=1.0`. Clamps so `ratatui::widgets::Gauge::ratio`
    /// never panics on a server under-reporting `Content-Length`.
    pub fn ratio(&self) -> Option<f64> {
        let total = self.total?;
        if total == 0 {
            return Some(1.0);
        }
        let r = self.bytes as f64 / total as f64;
        Some(r.clamp(0.0, 1.0))
    }
}

/// Everything the UI needs to draw one frame. Plain data only.
#[derive(Debug, Clone)]
pub struct UiState {
    pub title: String,
    pub should_quit: bool,
    pub blocks: Vec<Block>,
    pub focus: usize,
    pub downloads: BTreeMap<&'static str, DownloadState>,
    pub next_block_id: u32,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            title: "Voice Bird".to_string(),
            should_quit: false,
            blocks: Vec::new(),
            focus: 0,
            downloads: BTreeMap::new(),
            next_block_id: 1,
        }
    }
}

impl UiState {
    pub fn focused(&self) -> Option<&Block> {
        self.blocks.get(self.focus)
    }

    pub fn focused_mut(&mut self) -> Option<&mut Block> {
        self.blocks.get_mut(self.focus)
    }

    pub fn apply(&mut self, event: &AppEvent) {
        match event {
            AppEvent::AddBlock => {
                let id = self.next_block_id;
                self.blocks.push(Block {
                    id,
                    state: BlockState::Picking(ModelPicker::open(PickerIntent::AddBlock)),
                });
                self.next_block_id = id + 1;
                self.focus = self.blocks.len() - 1;
            }
            AppEvent::FocusMoved { direction } => match direction {
                FocusMove::Prev => {
                    if !self.blocks.is_empty() {
                        self.focus = self.focus.saturating_sub(1);
                    }
                }
                FocusMove::Next => {
                    if !self.blocks.is_empty() {
                        let last = self.blocks.len() - 1;
                        self.focus = (self.focus + 1).min(last);
                    }
                }
            },
            AppEvent::PickerMoved { direction, .. } => {
                if let Some(block) = self.focused_mut() {
                    if let BlockState::Picking(picker) = &mut block.state {
                        picker.apply(crate::picker::PickerEvent::Moved(*direction));
                    }
                }
            }
            AppEvent::ModelSelected(entry) => {
                if let Some(block) = self.focused_mut() {
                    if matches!(block.state, BlockState::Picking(_)) {
                        block.state = BlockState::Recording { model: entry.id };
                    }
                }
            }
            AppEvent::RecordingStarted(entry) => {
                if let Some(block) = self.focused_mut() {
                    if matches!(
                        block.state,
                        BlockState::Picking(_) | BlockState::Failed { .. }
                    ) {
                        block.state = BlockState::Recording { model: entry.id };
                    }
                }
            }
            AppEvent::ModelAlreadyCached(entry) => {
                // Alias for RecordingStarted: the resolver already
                // published the cache-hit diagnostic immediately
                // before this event; the reducer's only job is to
                // flip the focused block to Recording.
                if let Some(block) = self.focused_mut() {
                    if matches!(
                        block.state,
                        BlockState::Picking(_) | BlockState::Failed { .. }
                    ) {
                        block.state = BlockState::Recording { model: entry.id };
                    }
                }
            }
            AppEvent::DownloadRequested(entry) => {
                if let Some(block) = self.focused_mut() {
                    if matches!(
                        block.state,
                        BlockState::Picking(_) | BlockState::Failed { .. }
                    ) {
                        block.state = BlockState::Waiting { model: entry.id };
                    }
                }
                self.downloads.entry(entry.id).or_insert(DownloadState {
                    phase: DownloadPhase::Fetching,
                    bytes: 0,
                    total: None,
                    bytes_per_sec: 0,
                });
            }
            AppEvent::DownloadProgress {
                model,
                bytes,
                total,
                bytes_per_sec,
            } => {
                if let Some(row) = self.downloads.get_mut(*model) {
                    row.bytes = *bytes;
                    row.total = *total;
                    row.bytes_per_sec = *bytes_per_sec;
                }
            }
            AppEvent::DownloadInstalling { model } => {
                if let Some(row) = self.downloads.get_mut(*model) {
                    row.phase = DownloadPhase::Installing;
                }
            }
            AppEvent::DownloadSucceeded { model } => {
                self.downloads.remove(*model);
                for block in &mut self.blocks {
                    if matches!(&block.state, BlockState::Waiting { model: m } if **m == **model)
                    {
                        block.state = BlockState::Recording { model };
                    }
                }
            }
            AppEvent::DownloadFailed { model, error } => {
                self.downloads.remove(*model);
                for block in &mut self.blocks {
                    if matches!(&block.state, BlockState::Waiting { model: m } if **m == **model)
                    {
                        block.state = BlockState::Failed {
                            model,
                            error: error.clone(),
                        };
                    }
                }
            }
            AppEvent::BlockClosed => {
                if !self.blocks.is_empty() {
                    self.blocks.remove(self.focus);
                    if !self.blocks.is_empty() {
                        self.focus = self.focus.min(self.blocks.len() - 1);
                    }
                }
            }
            AppEvent::DownloadCancelled { model } => {
                self.downloads.remove(*model);
            }
            AppEvent::Quit => self.should_quit = true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::picker::{CATALOG, PickerMove};

    #[test]
    fn apply_quit_sets_should_quit() {
        let mut s = UiState::default();
        s.apply(&AppEvent::Quit);
        assert!(s.should_quit);
    }

    #[test]
    fn apply_add_block_pushes_picking_block_and_focuses_it() {
        let mut s = UiState::default();
        s.apply(&AppEvent::AddBlock);
        assert_eq!(s.blocks.len(), 1);
        assert_eq!(s.focus, 0);
        assert_eq!(s.blocks[0].id, 1);
        assert!(matches!(s.blocks[0].state, BlockState::Picking(_)));
        assert_eq!(s.next_block_id, 2);
    }

    #[test]
    fn apply_add_block_works_while_a_download_is_in_flight() {
        let mut s = UiState::default();
        s.apply(&AppEvent::AddBlock);
        s.apply(&AppEvent::ModelSelected(&CATALOG[0]));
        s.apply(&AppEvent::AddBlock);
        assert_eq!(s.blocks.len(), 2);
        assert_eq!(s.focus, 1);
        assert!(matches!(s.blocks[0].state, BlockState::Recording { model: "distil-small.en" }));
        assert!(matches!(s.blocks[1].state, BlockState::Picking(_)));
    }

    #[test]
    fn focus_moves_and_saturates_at_both_ends() {
        let mut s = UiState::default();
        for _ in 0..3 {
            s.apply(&AppEvent::AddBlock);
        }
        assert_eq!(s.focus, 2);
        for _ in 0..5 {
            s.apply(&AppEvent::FocusMoved { direction: FocusMove::Prev });
        }
        assert_eq!(s.focus, 0);
        for _ in 0..5 {
            s.apply(&AppEvent::FocusMoved { direction: FocusMove::Next });
        }
        assert_eq!(s.focus, 2);
    }

    #[test]
    fn focus_moved_is_noop_when_no_blocks() {
        let mut s = UiState::default();
        s.apply(&AppEvent::FocusMoved { direction: FocusMove::Next });
        assert_eq!(s.focus, 0);
    }

    #[test]
    fn picker_moved_targets_only_the_focused_block() {
        let mut s = UiState::default();
        s.apply(&AppEvent::AddBlock);
        s.apply(&AppEvent::ModelSelected(&CATALOG[0]));
        s.apply(&AppEvent::AddBlock);
        s.apply(&AppEvent::PickerMoved { direction: PickerMove::Down, from_model: None, to_model: None });
        s.apply(&AppEvent::PickerMoved { direction: PickerMove::Down, from_model: None, to_model: None });
        let picker_index = match &s.blocks[1].state {
            BlockState::Picking(p) => p.index,
            _ => panic!("block 2 should still be picking"),
        };
        assert_eq!(picker_index, 2);
        assert!(matches!(s.blocks[0].state, BlockState::Recording { model: "distil-small.en" }));
    }

    #[test]
    fn model_selected_flips_focused_block() {
        let mut s = UiState::default();
        s.apply(&AppEvent::AddBlock);
        s.apply(&AppEvent::PickerMoved { direction: PickerMove::Down, from_model: None, to_model: None });
        s.apply(&AppEvent::ModelSelected(&CATALOG[2]));
        assert!(matches!(s.blocks[0].state, BlockState::Recording { model: "large-v3-turbo" }));
    }

    #[test]
    fn model_selected_is_noop_when_focused_block_is_recording() {
        let mut s = UiState::default();
        s.apply(&AppEvent::AddBlock);
        s.apply(&AppEvent::ModelSelected(&CATALOG[0]));
        s.apply(&AppEvent::ModelSelected(&CATALOG[1]));
        assert!(matches!(s.blocks[0].state, BlockState::Recording { model: "distil-small.en" }));
    }

    #[test]
    fn block_closed_removes_and_clamps_focus() {
        let mut s = UiState::default();
        for _ in 0..3 {
            s.apply(&AppEvent::AddBlock);
        }
        s.apply(&AppEvent::BlockClosed);
        assert_eq!(s.blocks.len(), 2);
        assert_eq!(s.focus, 1);
    }

    #[test]
    fn block_closed_on_first_clamps_focus_to_zero() {
        let mut s = UiState::default();
        for _ in 0..3 {
            s.apply(&AppEvent::AddBlock);
        }
        s.apply(&AppEvent::FocusMoved { direction: FocusMove::Prev });
        s.apply(&AppEvent::FocusMoved { direction: FocusMove::Prev });
        assert_eq!(s.focus, 0);
        s.apply(&AppEvent::BlockClosed);
        assert_eq!(s.blocks.len(), 2);
        assert_eq!(s.focus, 0);
    }

    #[test]
    fn block_closed_with_no_blocks_is_a_noop() {
        let mut s = UiState::default();
        s.apply(&AppEvent::BlockClosed);
        assert!(s.blocks.is_empty());
    }

    #[test]
    fn next_block_id_monotonic_across_multiple_adds() {
        let mut s = UiState::default();
        for expected in 1u32..=3 {
            s.apply(&AppEvent::AddBlock);
            s.apply(&AppEvent::ModelSelected(&CATALOG[0]));
            assert_eq!(s.blocks.last().unwrap().id, expected);
            assert_eq!(s.next_block_id, expected + 1);
        }
    }

    #[test]
    fn apply_leaves_other_fields_untouched() {
        let mut s = UiState {
            title: "Hello".to_string(),
            should_quit: true,
            blocks: Vec::new(),
            focus: 0,
            downloads: BTreeMap::new(),
            next_block_id: 42,
        };
        s.apply(&AppEvent::AddBlock);
        assert_eq!(s.title, "Hello");
        assert!(s.should_quit);
        assert_eq!(s.next_block_id, 43);
        assert!(matches!(s.blocks[0].state, BlockState::Picking(_)));
    }

    #[test]
    fn download_requested_creates_one_record_for_two_blocks() {
        let mut s = UiState::default();
        s.apply(&AppEvent::AddBlock);
        s.apply(&AppEvent::DownloadRequested(&CATALOG[5]));
        s.apply(&AppEvent::DownloadProgress {
            model: "tiny.en",
            bytes: 100,
            total: Some(200),
            bytes_per_sec: 0,
        });
        s.apply(&AppEvent::AddBlock);
        s.apply(&AppEvent::DownloadRequested(&CATALOG[5]));
        let row = s.downloads.get("tiny.en").unwrap();
        assert_eq!(row.bytes, 100);
        assert_eq!(row.total, Some(200));
        assert_eq!(s.blocks.len(), 2);
        assert!(matches!(s.blocks[0].state, BlockState::Waiting { model: "tiny.en" }));
        assert!(matches!(s.blocks[1].state, BlockState::Waiting { model: "tiny.en" }));
    }

    #[test]
    fn download_progress_updates_the_shared_record() {
        let mut s = UiState::default();
        s.apply(&AppEvent::AddBlock);
        s.apply(&AppEvent::DownloadRequested(&CATALOG[5]));
        s.apply(&AppEvent::DownloadProgress {
            model: "tiny.en",
            bytes: 50,
            total: Some(100),
            bytes_per_sec: 0,
        });
        let row = s.downloads.get("tiny.en").unwrap();
        assert_eq!(row.bytes, 50);
        assert_eq!(row.total, Some(100));
    }

    #[test]
    fn download_progress_after_success_is_ignored() {
        let mut s = UiState::default();
        s.apply(&AppEvent::AddBlock);
        s.apply(&AppEvent::DownloadRequested(&CATALOG[5]));
        s.apply(&AppEvent::DownloadSucceeded { model: "tiny.en" });
        assert!(!s.downloads.contains_key("tiny.en"));
        s.apply(&AppEvent::DownloadProgress {
            model: "tiny.en",
            bytes: 10,
            total: None,
            bytes_per_sec: 0,
        });
        assert!(!s.downloads.contains_key("tiny.en"));
    }

    #[test]
    fn download_installing_sets_phase() {
        let mut s = UiState::default();
        s.apply(&AppEvent::AddBlock);
        s.apply(&AppEvent::DownloadRequested(&CATALOG[5]));
        s.apply(&AppEvent::DownloadInstalling { model: "tiny.en" });
        assert_eq!(
            s.downloads.get("tiny.en").unwrap().phase,
            DownloadPhase::Installing
        );
    }

    #[test]
    fn download_succeeded_flips_every_waiting_block() {
        let mut s = UiState::default();
        s.apply(&AppEvent::AddBlock);
        s.apply(&AppEvent::DownloadRequested(&CATALOG[5]));
        s.apply(&AppEvent::AddBlock);
        s.apply(&AppEvent::DownloadRequested(&CATALOG[5]));
        s.apply(&AppEvent::DownloadSucceeded { model: "tiny.en" });
        assert!(matches!(s.blocks[0].state, BlockState::Recording { model: "tiny.en" }));
        assert!(matches!(s.blocks[1].state, BlockState::Recording { model: "tiny.en" }));
        assert!(!s.downloads.contains_key("tiny.en"));
    }

    #[test]
    fn download_failed_flips_every_waiting_block_with_the_message() {
        let mut s = UiState::default();
        s.apply(&AppEvent::AddBlock);
        s.apply(&AppEvent::DownloadRequested(&CATALOG[5]));
        s.apply(&AppEvent::AddBlock);
        s.apply(&AppEvent::DownloadRequested(&CATALOG[5]));
        s.apply(&AppEvent::DownloadFailed {
            model: "tiny.en",
            error: "HTTP 404".to_string(),
        });
        for block in &s.blocks {
            match &block.state {
                BlockState::Failed { model, error } => {
                    assert_eq!(*model, "tiny.en");
                    assert_eq!(error, "HTTP 404");
                }
                other => panic!("expected Failed, got {other:?}"),
            }
        }
        assert!(!s.downloads.contains_key("tiny.en"));
    }

    #[test]
    fn retry_from_failed_transitions_to_waiting() {
        let mut s = UiState::default();
        s.apply(&AppEvent::AddBlock);
        s.apply(&AppEvent::DownloadRequested(&CATALOG[5]));
        s.apply(&AppEvent::DownloadFailed {
            model: "tiny.en",
            error: "boom".into(),
        });
        s.apply(&AppEvent::DownloadRequested(&CATALOG[5]));
        assert!(matches!(s.blocks[0].state, BlockState::Waiting { model: "tiny.en" }));
    }

    #[test]
    fn block_closed_drops_the_record_when_no_waiters_remain() {
        let mut s = UiState::default();
        s.apply(&AppEvent::AddBlock);
        s.apply(&AppEvent::DownloadRequested(&CATALOG[5]));
        assert!(s.downloads.contains_key("tiny.en"));
        // The resolver publishes BlockClosed + DownloadCancelled when
        // the last waiter is dropped — split the test across both.
        s.apply(&AppEvent::BlockClosed);
        s.apply(&AppEvent::DownloadCancelled { model: "tiny.en" });
        assert!(!s.downloads.contains_key("tiny.en"));
    }

    #[test]
    fn block_closed_keeps_the_record_while_another_block_waits() {
        let mut s = UiState::default();
        s.apply(&AppEvent::AddBlock);
        s.apply(&AppEvent::DownloadRequested(&CATALOG[5]));
        s.apply(&AppEvent::AddBlock);
        s.apply(&AppEvent::DownloadRequested(&CATALOG[5]));
        s.apply(&AppEvent::BlockClosed);
        assert!(s.downloads.contains_key("tiny.en"));
        assert!(matches!(s.blocks[0].state, BlockState::Waiting { model: "tiny.en" }));
    }

    #[test]
    fn ratio_clamps_when_bytes_exceed_total() {
        let r = DownloadState {
            phase: DownloadPhase::Fetching,
            bytes: 200,
            total: Some(100),
            bytes_per_sec: 0,
        };
        let ratio = r.ratio().unwrap();
        assert!((0.0..=1.0).contains(&ratio));
        assert!((ratio - 1.0).abs() < 1e-9);
    }

    #[test]
    fn ratio_is_none_without_total() {
        let r = DownloadState {
            phase: DownloadPhase::Fetching,
            bytes: 100,
            total: None,
            bytes_per_sec: 0,
        };
        assert!(r.ratio().is_none());
    }

    #[test]
    fn ratio_with_zero_total_is_one() {
        let r = DownloadState {
            phase: DownloadPhase::Fetching,
            bytes: 0,
            total: Some(0),
            bytes_per_sec: 0,
        };
        assert_eq!(r.ratio(), Some(1.0));
    }

    #[test]
    fn ratio_zero_bytes_zero_total_is_one() {
        let r = DownloadState {
            phase: DownloadPhase::Fetching,
            bytes: 0,
            total: Some(0),
            bytes_per_sec: 0,
        };
        assert_eq!(r.ratio(), Some(1.0));
    }

    #[test]
    fn recording_started_flips_focused_block() {
        let mut s = UiState::default();
        s.apply(&AppEvent::AddBlock);
        s.apply(&AppEvent::RecordingStarted(&CATALOG[5]));
        assert!(matches!(s.blocks[0].state, BlockState::Recording { model: "tiny.en" }));
    }

    #[test]
    fn recording_started_flips_focused_failed_block() {
        let mut s = UiState::default();
        s.apply(&AppEvent::AddBlock);
        s.apply(&AppEvent::DownloadRequested(&CATALOG[5]));
        s.apply(&AppEvent::DownloadFailed {
            model: "tiny.en",
            error: "boom".into(),
        });
        s.apply(&AppEvent::RecordingStarted(&CATALOG[5]));
        assert!(matches!(s.blocks[0].state, BlockState::Recording { model: "tiny.en" }));
    }
}