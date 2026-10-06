use std::collections::BTreeMap;

use crate::producer::sources::{AudioSourceSnapshot, FunnelStep, SourceSelection};
use crate::bus::{AppEvent, FocusMove};
use crate::language::LanguageProfile;
use crate::picker::{ListPicker, ModelEntry, PickerEvent, SessionMenu};

/// Render-side phase for one download. Lives in `consumer/ui_view.rs` (UI
/// only) because the SQLite `DownloadStatus` enum carries the
/// matching lifecycle for the table — the renderer doesn't need
/// the full enum, only the two phases it renders distinctly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadPhase {
    Fetching,
    Installing,
}

/// Hard cap on the number of blocks rendered side-by-side. Sessions
/// past this count stay alive (hidden); the user reaches them via
/// the session menu (`Tab`).
pub const MAX_VISIBLE_BLOCKS: usize = 4;

/// State of one block. The block is the unit of interaction: it picks a
/// language, waits for both of its models, then records with that language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockState {
    PickingDevice(ListPicker),
    PickingApp(ListPicker),
    /// Choosing a language. Carries its own picker — no shared overlay.
    PickingLanguage(ListPicker),
    /// Waiting until every model id in `pending` is present on disk.
    /// Progress stays in [`UiView::downloads`] so blocks share per-model state.
    Waiting {
        language: &'static LanguageProfile,
        pending: Vec<&'static str>,
    },
    /// Mocked in this PR — no audio device; the state only changes rendering.
    /// Recording stops when either required model goes missing.
    Recording { language: &'static LanguageProfile },
    /// A required model failed. Retry re-checks both models on disk.
    Failed {
        language: &'static LanguageProfile,
        error: String,
        /// Required models that are still running after the failure.
        pending: Vec<&'static str>,
    },
}

/// A single inner column. Pure data — no threads, no handles, no clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub id: u8,
    pub state: BlockState,
    pub source: Option<SourceSelection>,
    /// Whether the block occupies a column on screen. `false` once the
    /// cap has pushed the block off the visible strip; it stays alive
    /// and the renderer simply skips it.
    pub visible: bool,
    /// Monotonic stamp bumped every time this block takes focus.
    /// The eviction rule is "drop the visible block with the smallest
    /// stamp" — i.e. least-recently-focused — so a block you just
    /// revealed can never be evicted on the next reveal.
    pub last_focused: u64,
}

impl Block {
    /// Build a fresh, visible block. `show_block` is the single
    /// seam that flips `visible` to `true` and stamps `last_focused`;
    /// literals in tests that want a visible block can use this
    /// constructor too, since `Default` matches the production path.
    pub fn new(id: u8, state: BlockState) -> Self {
        Self {
            id,
            state,
            source: None,
            visible: true,
            last_focused: 0,
        }
    }

    /// Build a hidden block. Used by [`UiView::apply`] for
    /// `AddBlock`: the reducer pushes the block into `blocks` with
    /// `visible: false` and then calls `show_block`, which decides
    /// whether to flip it visible (and which peer to evict if not).
    /// Keeping the push hidden is what guarantees the cap math runs
    /// through `show_block` and not via a stray "always visible on
    /// push" path.
    fn new_hidden(id: u8, state: BlockState) -> Self {
        Self {
            id,
            state,
            source: None,
            visible: false,
            last_focused: 0,
        }
    }

    pub fn language(&self) -> Option<&'static LanguageProfile> {
        match &self.state {
            BlockState::PickingLanguage(_) | BlockState::PickingDevice(_) | BlockState::PickingApp(_) => {
                None
            }
            BlockState::Waiting { language, .. }
            | BlockState::Recording { language }
            | BlockState::Failed { language, .. } => Some(language),
        }
    }

    pub fn pending_models(&self) -> &[&'static str] {
        match &self.state {
            BlockState::Waiting { pending, .. } | BlockState::Failed { pending, .. } => pending,
            _ => &[],
        }
    }

    /// Models this active block counts as installed, excluding pending work.
    pub fn ready_models(&self) -> impl Iterator<Item = &'static ModelEntry> + '_ {
        let models = match &self.state {
            BlockState::Recording { language } | BlockState::Waiting { language, .. } => {
                Some(language.models())
            }
            BlockState::PickingLanguage(_)
            | BlockState::PickingDevice(_)
            | BlockState::PickingApp(_)
            | BlockState::Failed { .. } => None,
        };
        models
            .into_iter()
            .flatten()
            .filter(|model| !self.pending_models().contains(&model.id))
    }
}

impl Default for Block {
    /// Sentinel default for tests that build a `Block { id, state,
    /// ..Default::default() }`. Not used by production code; the
    /// reducer constructs blocks via [`Block::new`].
    fn default() -> Self {
        Self::new(0, BlockState::PickingLanguage(ListPicker::default()))
    }
}

/// Render-side projection of one download. The SQLite downloads table and
/// this map fold the same accepted bus events: decisions read SQLite while
/// rendering remains a pure function of `UiView`.
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
pub struct UiView {
    pub title: String,
    pub should_quit: bool,
    /// Every session, in creation order. `focus` is an index into this
    /// vector — the renderer's "is this the focused block" check is
    /// `block.id == focused_id`, not the index in the filtered visible
    /// list. Rendering order stays creation order so columns never
    /// jump when a session is revealed.
    pub blocks: Vec<Block>,
    pub focus: usize,
    pub downloads: BTreeMap<&'static str, DownloadState>,
    pub next_block_id: u8,
    /// Left-hand session menu. `None` when closed; `Some(_)`
    /// otherwise. The renderer draws the panel when this is `Some`
    /// and the column layout only sees [`Block::visible`] rows.
    pub menu: Option<SessionMenu>,
    /// Monotonic stamp source for [`Block::last_focused`]. Bumped on
    /// every focus change and every reveal-via-menu, so a recently
    /// focused block carries a strictly-greater stamp than any block
    /// not focused since the last bump.
    pub focus_clock: u64,
    /// Transient user-facing warning shown in the title bar. Set by
    /// the reducer when an action can't be completed (e.g. AddBlock
    /// when the 1..=255 id space is full); cleared automatically once
    /// the underlying condition is gone. `None` is the steady state.
    pub warning: Option<String>,
}

impl Default for UiView {
    fn default() -> Self {
        Self {
            title: "Voice Bird".to_string(),
            should_quit: false,
            blocks: Vec::new(),
            focus: 0,
            downloads: BTreeMap::new(),
            next_block_id: 1,
            menu: None,
            focus_clock: 0,
            warning: None,
        }
    }
}

impl UiView {
    pub fn focused(&self) -> Option<&Block> {
        self.blocks.get(self.focus)
    }

    pub fn focused_mut(&mut self) -> Option<&mut Block> {
        self.blocks.get_mut(self.focus)
    }

    /// Visible blocks in creation order. The renderer iterates this
    /// to lay out columns; the cap is enforced in the reducer, so a
    /// caller never has to clamp here.
    pub fn visible_blocks(&self) -> impl Iterator<Item = &Block> {
        self.blocks.iter().filter(|b| b.visible)
    }

    /// Make `id` visible and focused. If the cap is already reached,
    /// hide the visible block with the oldest focus stamp first.
    /// Idempotent for a block already visible — it still takes focus,
    /// which restamps `last_focused` so it can't be evicted on the
    /// next reveal.
    ///
    /// `id` that doesn't match any block is a silent no-op: the
    /// resolver could be racing a `BlockClosed` event, and panicking
    /// would freeze the loop.
    pub fn show_block(&mut self, id: u8) {
        let Some(target_idx) = self.blocks.iter().position(|b| b.id == id) else {
            return;
        };
        // Bump the clock first so the just-revealed block carries a
        // strictly-greater stamp than every visible peer (including
        // its previous stamp, which is what guarantees "a block you
        // just revealed cannot be evicted on the next reveal").
        self.focus_clock = self.focus_clock.wrapping_add(1);
        let stamp = self.focus_clock;
        // If the target is already visible, still restamp + take
        // focus; otherwise evict and then mark visible.
        let already_visible = self.blocks[target_idx].visible;
        if !already_visible {
            // Evict the visible block with the smallest stamp — the
            // least-recently-focused one — before flipping the target.
            // Ties cannot happen: `focus_clock` is bumped per reveal,
            // and a block that just took focus also took the stamp.
            if self.visible_blocks().count() >= MAX_VISIBLE_BLOCKS {
                if let Some((evict_idx, _)) = self
                    .blocks
                    .iter()
                    .enumerate()
                    .filter(|(_, b)| b.visible)
                    .min_by_key(|(_, b)| b.last_focused)
                {
                    self.blocks[evict_idx].visible = false;
                }
            }
            self.blocks[target_idx].visible = true;
        }
        self.blocks[target_idx].last_focused = stamp;
        self.focus = target_idx;
    }

    /// Promote the most-recently-focused hidden block (if any) into
    /// the visible window. Called after a block has been removed:
    /// the user opened the menu expecting a full window, and leaving
    /// an empty column while a hidden block is still alive would be
    /// surprising.
    fn promote_hidden(&mut self) {
        let free = MAX_VISIBLE_BLOCKS.saturating_sub(self.visible_blocks().count());
        if free == 0 {
            return;
        }
        // Pick the hidden block with the largest stamp — the
        // most-recently-focused one — to honour the LRU-on-focus
        // rule. Ties (all hidden, same stamp = 0) fall to the first
        // such index; deterministic.
        for _ in 0..free {
            let Some(idx) = self
                .blocks
                .iter()
                .enumerate()
                .filter(|(_, b)| !b.visible)
                .max_by_key(|(_, b)| b.last_focused)
                .map(|(i, _)| i)
            else {
                break;
            };
            self.blocks[idx].visible = true;
        }
    }
    fn fail_waiters(&mut self, model: &str, error: &str) {
        self.downloads.remove(model);
        for block in &mut self.blocks {
            let failed = match &mut block.state {
                BlockState::Waiting { language, pending } if pending.contains(&model) => {
                    pending.retain(|pending_model| *pending_model != model);
                    Some((*language, pending.clone()))
                }
                BlockState::Failed { pending, .. } => {
                    pending.retain(|pending_model| *pending_model != model);
                    None
                }
                _ => None,
            };
            if let Some((language, pending)) = failed {
                block.state = BlockState::Failed {
                    language,
                    pending,
                    error: error.to_string(),
                };
            }
        }
    }

    fn mark_ready(&mut self, model: &str) {
        self.downloads.remove(model);
        for block in &mut self.blocks {
            let completed_language = match &mut block.state {
                BlockState::Waiting { language, pending } => {
                    pending.retain(|pending_model| *pending_model != model);
                    pending.is_empty().then_some(*language)
                }
                BlockState::Failed { pending, .. } => {
                    pending.retain(|pending_model| *pending_model != model);
                    None
                }
                _ => None,
            };
            if let Some(language) = completed_language {
                block.state = BlockState::Recording { language };
            }
        }
    }

    fn mark_missing(&mut self, model: &'static str) {
        for block in &mut self.blocks {
            match &mut block.state {
                BlockState::Recording { language }
                    if language.models().iter().any(|entry| entry.id == model) =>
                {
                    block.state = BlockState::Waiting {
                        language,
                        pending: vec![model],
                    };
                }
                BlockState::Waiting { language, pending }
                    if language.models().iter().any(|entry| entry.id == model)
                        && !pending.contains(&model) =>
                {
                    pending.push(model);
                }
                _ => {}
            }
        }
    }

    fn push_block(&mut self, snapshot: Option<AudioSourceSnapshot>) {
        // Scan the nonzero id space, including wraparound, without replacing a live session.
        let candidate = self.next_block_id;
        let id = (candidate..=u8::MAX)
            .chain(1..candidate)
            .find(|&id| !self.blocks.iter().any(|block| block.id == id));
        let Some(id) = id else {
            self.warning = Some("session limit reached; close a session to make room".to_string());
            return;
        };
        self.next_block_id = if id == u8::MAX { 1 } else { id + 1 };
        let state = if snapshot.is_some() {
            BlockState::PickingDevice(ListPicker::default())
        } else {
            BlockState::PickingLanguage(ListPicker::default())
        };
        let mut block = Block::new_hidden(id, state);
        block.source = snapshot.map(|snapshot| SourceSelection {
            snapshot,
            device: None,
            app: None,
            rev: 0,
        });
        self.blocks.push(block);
        self.show_block(id);
    }

    pub fn apply(&mut self, event: &AppEvent) {
        match event {
            AppEvent::AddBlock => self.push_block(None),
            AppEvent::AddSourceBlock { snapshot } => self.push_block(Some(snapshot.clone())),
            AppEvent::SourceStepChanged {
                block,
                from,
                to,
                rev,
                device,
                app,
            } => {
                let Some(block) = self
                    .blocks
                    .iter_mut()
                    .find(|candidate| candidate.id == *block)
                else {
                    return;
                };
                let step = match block.state {
                    BlockState::PickingDevice(_) => FunnelStep::Device,
                    BlockState::PickingApp(_) => FunnelStep::App,
                    BlockState::PickingLanguage(_) => FunnelStep::Language,
                    _ => FunnelStep::Committed,
                };
                let Some(source) = block.source.as_mut() else {
                    return;
                };
                if step != *from || source.rev != *rev || *to == FunnelStep::Committed {
                    return;
                }
                source.device = device.clone();
                source.app = app.clone();
                source.rev += 1;
                block.state = match to {
                    FunnelStep::Device => BlockState::PickingDevice(ListPicker {
                        index: source
                            .snapshot
                            .devices
                            .iter()
                            .position(|row| Some(row) == source.device.as_ref())
                            .unwrap_or(0),
                    }),
                    FunnelStep::App => BlockState::PickingApp(ListPicker {
                        index: source
                            .snapshot
                            .apps
                            .iter()
                            .position(|row| Some(row) == source.app.as_ref())
                            .unwrap_or(0),
                    }),
                    FunnelStep::Language => BlockState::PickingLanguage(ListPicker::default()),
                    FunnelStep::Committed => unreachable!(),
                };
            }
            AppEvent::SourceStepRejected { .. } => {}
            AppEvent::FocusMoved { direction } => {
                if self.blocks.is_empty() {
                    return;
                }
                // Walk over the whole `blocks` slice (visible + hidden).
                // Skipping hidden indices would strand the user when
                // the focused block becomes hidden — the keyboard path
                // has no menu to fall back on. We *do* route focus
                // through `show_block` if the destination is hidden so
                // the cap stays enforced and the focused border lands
                // on a rendered column.
                let new_focus = match direction {
                    FocusMove::Prev => self.focus.saturating_sub(1),
                    FocusMove::Next => {
                        let last = self.blocks.len() - 1;
                        (self.focus + 1).min(last)
                    }
                };
                self.focus = new_focus;
                let id = self.blocks[new_focus].id;
                if !self.blocks[new_focus].visible {
                    // The destination was hidden. Make it visible,
                    // which evicts the LRU peer and restamps focus.
                    self.show_block(id);
                } else {
                    // Visible: bump the stamp so this block can't be
                    // evicted on the next reveal, but don't trigger
                    // an eviction we don't need.
                    self.focus_clock = self.focus_clock.wrapping_add(1);
                    self.blocks[new_focus].last_focused = self.focus_clock;
                }
            }
            AppEvent::PickerMoved { direction, .. } => {
                if let Some(block) = self.focused_mut() {
                    match &mut block.state {
                        BlockState::PickingLanguage(picker) => {
                            picker.apply(*direction, crate::language::LANGUAGES.len());
                        }
                        BlockState::PickingDevice(cursor) => {
                            if let Some(source) = &block.source {
                                cursor.apply(*direction, source.snapshot.devices.len());
                            }
                        }
                        BlockState::PickingApp(cursor) => {
                            if let Some(source) = &block.source {
                                cursor.apply(*direction, source.snapshot.apps.len());
                            }
                        }
                        _ => {}
                    }
                }
            }
            AppEvent::LanguageSelected {
                block,
                language,
                pending,
            } => {
                if let Some(block) = self
                    .blocks
                    .iter_mut()
                    .find(|candidate| candidate.id == *block)
                {
                    if matches!(
                        block.state,
                        BlockState::PickingLanguage(_) | BlockState::Failed { .. }
                    ) {
                        block.state = if pending.is_empty() {
                            BlockState::Recording { language }
                        } else {
                            BlockState::Waiting {
                                language,
                                pending: pending.clone(),
                            }
                        };
                    }
                }
            }
            AppEvent::ModelAlreadyCached(entry) => self.mark_ready(entry.id),
            AppEvent::ModelMissing(entry) => self.mark_missing(entry.id),
            AppEvent::DownloadRequested { model: entry, .. } => {
                self.downloads.entry(entry.id).or_insert(DownloadState {
                    phase: DownloadPhase::Fetching,
                    bytes: 0,
                    total: None,
                    bytes_per_sec: 0,
                });
            }
            AppEvent::DownloadProgress {
                attempt: _,
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
            AppEvent::DownloadInstalling { attempt: _, model } => {
                if let Some(row) = self.downloads.get_mut(*model) {
                    row.phase = DownloadPhase::Installing;
                }
            }
            AppEvent::DownloadSucceeded { attempt: _, model } => self.mark_ready(model),
            AppEvent::DownloadFailed {
                attempt: _,
                model,
                error,
            }
            | AppEvent::DownloadClaimFailed {
                attempt: _,
                model,
                error,
            } => self.fail_waiters(model, error),
            AppEvent::BlockClosed { block } => {
                let Some(index) = self.blocks.iter().position(|candidate| candidate.id == *block)
                else {
                    return;
                };
                let removed_pending = self.blocks[index].pending_models().to_vec();
                self.blocks.remove(index);
                if index < self.focus {
                    self.focus -= 1;
                } else if !self.blocks.is_empty() {
                    self.focus = self.focus.min(self.blocks.len() - 1);
                } else {
                    self.focus = 0;
                }
                self.promote_hidden();
                for model in removed_pending {
                    let still_pending = self
                        .blocks
                        .iter()
                        .any(|block| block.pending_models().contains(&model));
                    if !still_pending {
                        self.downloads.remove(model);
                    }
                }
                // If the warning was the id-exhaustion message and
                // we now have a free id slot, clear it. Any other
                // warning (future use) stays put — only the
                // exhaustion warning auto-clears on BlockClosed,
                // because only BlockClosed frees an id.
                if self.warning.as_deref()
                    == Some("session limit reached; close a session to make room")
                    && self.blocks.len() < u8::MAX as usize
                {
                    self.warning = None;
                }
            }
            AppEvent::DownloadCancelled { attempt: _, model } => {
                self.downloads.remove(*model);
            }
            AppEvent::MenuOpened => {
                // Open at the currently-focused row. With no blocks,
                // the menu has no rows; defer to the menu layer's
                // empty-list handling.
                let len = self.blocks.len();
                let idx = if len == 0 { 0 } else { self.focus.min(len - 1) };
                self.menu = Some(SessionMenu::open_at(idx));
            }
            AppEvent::MenuClosed => {
                self.menu = None;
            }
            AppEvent::MenuMoved { direction } => {
                if let Some(menu) = self.menu.as_mut() {
                    menu.apply(PickerEvent::Moved(*direction), self.blocks.len());
                }
            }
            AppEvent::SessionShown { id } => {
                self.show_block(*id);
                self.menu = None;
            }
            // The table publishes these after persisting a lifecycle
            // transition. Replaying terminal outcomes here reconciles a worker
            // event that was reduced while a new block was still PickingLanguage.
            AppEvent::DownloadStatusChanged {
                model, to, error, ..
            } => match to {
                crate::bus::DownloadStatus::Succeeded => self.mark_ready(model.as_ref()),
                crate::bus::DownloadStatus::Failed => self.fail_waiters(
                    model.as_ref(),
                    error.as_deref().unwrap_or("download failed"),
                ),
                crate::bus::DownloadStatus::Cancelled => {
                    self.fail_waiters(model.as_ref(), "download cancelled");
                }
                crate::bus::DownloadStatus::Interrupted => {
                    self.fail_waiters(model.as_ref(), "download interrupted");
                }
                _ => {}
            },
            AppEvent::DownloadEventRejected { .. } => {}
            AppEvent::Quit => self.should_quit = true,
            // Bus commands routed by the consumer after the UI projection.
            // The reducer only mirrors their UI effects.
            AppEvent::BeginLanguage {
                block,
                source_rev: Some(rev),
                ..
            } => {
                if let Some(source) = self
                    .blocks
                    .iter_mut()
                    .find(|candidate| candidate.id == *block)
                    .and_then(|block| block.source.as_mut())
                {
                    if source.rev == *rev {
                        source.rev += 1;
                    }
                }
            }
            AppEvent::RequestBlock
            | AppEvent::BeginLanguage { .. }
            | AppEvent::DiscardInflight { .. } => {}
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::{LanguageProfile, LANGUAGES};
    use crate::picker::PickerMove;

    fn english() -> &'static LanguageProfile {
        &LANGUAGES[0]
    }

    fn select(block: u8, pending: Vec<&'static str>) -> AppEvent {
        AppEvent::LanguageSelected {
            block,
            language: english(),
            pending,
        }
    }

    #[test]
    fn missing_model_stops_every_affected_recording_block_and_success_resumes() {
        let language = english();
        let mut s = UiView {
            blocks: vec![
                Block::new(1, BlockState::Recording { language }),
                Block::new(2, BlockState::Recording { language }),
            ],
            ..UiView::default()
        };
        s.blocks[1].visible = false;
        s.apply(&AppEvent::ModelMissing(language.live));
        for block in &s.blocks {
            assert_eq!(block.state, BlockState::Waiting {
                language,
                pending: vec![language.live.id],
            });
            assert_eq!(block.ready_models().collect::<Vec<_>>(), vec![language.refine]);
        }
        s.apply(&AppEvent::DownloadSucceeded { attempt: 2, model: language.live.id });
        for block in &s.blocks {
            assert_eq!(block.state, BlockState::Recording { language });
            assert_eq!(block.ready_models().collect::<Vec<_>>(), language.models());
        }
    }

    #[test]
    fn missing_model_extends_waiting_once_and_requires_both_models_to_resume() {
        let language = english();
        let mut s = UiView::default();
        s.apply(&AppEvent::AddBlock);
        s.apply(&select(1, vec![language.refine.id]));
        s.apply(&AppEvent::ModelMissing(language.live));
        s.apply(&AppEvent::ModelMissing(language.live));
        assert_eq!(s.blocks[0].state, BlockState::Waiting {
            language,
            pending: vec![language.refine.id, language.live.id],
        });
        assert!(s.blocks[0].ready_models().next().is_none());
        s.apply(&AppEvent::DownloadSucceeded { attempt: 1, model: language.refine.id });
        assert_eq!(s.blocks[0].pending_models(), &[language.live.id]);
        s.apply(&AppEvent::DownloadSucceeded { attempt: 2, model: language.live.id });
        assert_eq!(s.blocks[0].state, BlockState::Recording { language });
    }

    #[test]
    fn missing_model_leaves_other_languages_picking_and_failed_untouched() {
        static OTHER: LanguageProfile = LanguageProfile {
            code: "other",
            live: &crate::picker::BASE_EN,
            refine: &crate::picker::TINY_EN,
        };
        let language = english();
        assert!(OTHER.models().iter().all(|model| model.id != language.live.id));
        let mut s = UiView {
            blocks: vec![
                Block::default(),
                Block::new(1, BlockState::Failed {
                    language, error: "failure".to_string(), pending: vec![],
                }),
                Block::new(2, BlockState::Recording { language: &OTHER }),
                Block::new(3, BlockState::Waiting {
                    language: &OTHER, pending: vec![OTHER.refine.id],
                }),
            ],
            ..UiView::default()
        };
        let before = s.blocks.clone();
        assert!(s.blocks[0].ready_models().next().is_none());
        assert!(s.blocks[1].ready_models().next().is_none());
        s.apply(&AppEvent::ModelMissing(language.live));
        assert_eq!(s.blocks, before);
    }

    #[test]
    fn apply_quit_sets_should_quit() {
        let mut s = UiView::default();
        s.apply(&AppEvent::Quit);
        assert!(s.should_quit);
    }

    #[test]
    fn apply_add_block_pushes_picking_block_and_focuses_it() {
        let mut s = UiView::default();
        s.apply(&AppEvent::AddBlock);
        assert_eq!(s.blocks.len(), 1);
        assert_eq!(s.focus, 0);
        assert_eq!(s.blocks[0].id, 1);
        assert!(matches!(s.blocks[0].state, BlockState::PickingLanguage(_)));
        assert_eq!(s.next_block_id, 2);
        assert!(s.blocks[0].visible, "a fresh block starts visible");
    }

    #[test]
    fn apply_add_block_works_while_a_download_is_in_flight() {
        let mut s = UiView::default();
        s.apply(&AppEvent::AddBlock);
        s.apply(&select(1, Vec::new()));
        s.apply(&AppEvent::AddBlock);
        assert_eq!(s.blocks.len(), 2);
        assert_eq!(s.focus, 1);
        assert!(matches!(
            s.blocks[0].state,
            BlockState::Recording { language } if language == english()
        ));
        assert!(matches!(s.blocks[1].state, BlockState::PickingLanguage(_)));
    }

    /// Regression: after 256 `AddBlock` events the candidate id
    /// `next_block_id` rolls back to 1, colliding with the still
    /// alive block 1. Before the fix, `show_block(1)` matched
    /// the *older* session, leaving the new one hidden and
    /// focusing the wrong block.
    #[test]
    fn add_block_avoids_colliding_id_when_wrap_would_hit_a_live_block() {
        let mut s = UiView::default();
        // Add 256 blocks. The 256th would naively reuse id 1
        // (since next_block_id wraps from 255 -> 1), but block 1
        // is still alive.
        for _ in 0..256 {
            s.apply(&AppEvent::AddBlock);
        }
        // Block 1 is still alive at index 0 (the cap may have
        // hidden it, but `show_block` keeps it focused).
        let first = s.blocks.iter().find(|b| b.id == 1).expect("block 1");
        let last = s.blocks.last().expect("newest block");
        assert_eq!(first.id, 1);
        assert_ne!(
            last.id, 1,
            "newest block must NOT reuse id 1 while block 1 is still alive"
        );
        // Every live id is unique.
        let mut ids: Vec<u8> = s.blocks.iter().map(|b| b.id).collect();
        ids.sort();
        let original_len = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), original_len, "live blocks have duplicate ids");
        // The newest block is the focused one — its id is what
        // `show_block` last saw, so the focus must point at it,
        // not at the older block with id 1.
        let focused = s.focused().expect("some block is focused");
        assert_eq!(
            focused.id, last.id,
            "focus must be on the newest block, not a stale id-1 collision"
        );
    }

    /// After a wrap, AddBlock still finds a free id even when
    /// the candidate collides. With `next_block_id = 1` and
    /// block 1 alive, the candidate would naively collide; the
    /// scan must skip id 1 and land on the first free slot.
    #[test]
    fn add_block_after_wrap_picks_the_next_free_id() {
        let mut s = UiView::default();
        s.apply(&AppEvent::AddBlock);
        // Block 1 is alive. Force the next candidate back to 1
        // (the post-wrap value).
        s.next_block_id = 1;
        s.apply(&AppEvent::AddBlock);
        // Scan starts at 1 (taken), then 2, 3, ... and picks
        // the first free slot: id 2.
        assert!(
            s.blocks.iter().any(|b| b.id == 2),
            "scan should have picked id 2; live ids: {:?}",
            s.blocks.iter().map(|b| b.id).collect::<Vec<_>>()
        );
        // The candidate id 1 was NOT issued - exactly one
        // block with id 1 (the original).
        let ids: Vec<u8> = s.blocks.iter().map(|b| b.id).collect();
        assert_eq!(
            ids.iter().filter(|&&i| i == 1).count(),
            1,
            "exactly one block with id 1 (the original); got {ids:?}"
        );
    }

    /// When the candidate `next_block_id` is itself free, the
    /// scan returns it unchanged. Specifically: after using id
    /// 255, `next_block_id` wraps to 1; if id 1 has been
    /// closed by then, the scan picks id 1 (the wrapped value).
    #[test]
    fn add_block_picks_wrapped_id_when_it_is_free() {
        let mut s = UiView::default();
        // Create block 1, then close it (frees id 1).
        s.apply(&AppEvent::AddBlock);
        s.apply(&AppEvent::BlockClosed { block: s.focused().map_or(1, |block| block.id) });
        assert_eq!(s.blocks.len(), 0, "block 1 closed");
        // next_block_id is now 2. Force it to the post-wrap
        // value 1; id 1 is free, so the scan picks 1.
        s.next_block_id = 1;
        s.apply(&AppEvent::AddBlock);
        assert_eq!(s.blocks[0].id, 1, "scan picked the wrapped id 1");
    }

    /// The scan must also skip a *block of holes* - if ids
    /// 1..=N are all taken, the scan walks forward until it
    /// finds the gap.
    #[test]
    fn add_block_skips_a_range_of_taken_ids() {
        let mut s = UiView::default();
        // Create blocks 1, 2, 3.
        for _ in 0..3 {
            s.apply(&AppEvent::AddBlock);
        }
        let live_ids: std::collections::HashSet<u8> = s.blocks.iter().map(|b| b.id).collect();
        assert_eq!(live_ids, [1u8, 2, 3].into_iter().collect());
        // Force next_block_id back to 1; the next AddBlock will
        // scan past 1, 2, 3 and pick 4.
        s.next_block_id = 1;
        s.apply(&AppEvent::AddBlock);
        let new_ids: std::collections::HashSet<u8> = s.blocks.iter().map(|b| b.id).collect();
        assert_eq!(
            new_ids,
            [1u8, 2, 3, 4].into_iter().collect(),
            "scan must skip taken ids 1..=3 and land on 4"
        );
    }

    /// Exhaustion: if every id in `1..=u8::MAX` is taken,
    /// AddBlock refuses and surfaces a `state.warning` for the
    /// title bar. (The block count is also unchanged — we don't
    /// drop a session on the user's behalf.) We can't actually
    /// create 255 live blocks in a unit test cheaply, so we set
    /// `next_block_id` to a value that, combined with a manually
    /// populated `blocks` vector covering the full id range,
    /// makes the scan find nothing.
    #[test]
    fn add_block_is_a_noop_when_all_ids_are_exhausted() {
        let mut s = UiView {
            title: "Voice Bird".to_string(),
            should_quit: false,
            blocks: (1..=u8::MAX)
                .map(|id| Block::new(id, BlockState::PickingLanguage(ListPicker::default())))
                .collect(),
            focus: 0,
            downloads: BTreeMap::new(),
            next_block_id: 1,
            menu: None,
            focus_clock: 0,
            warning: None,
        };
        let len_before = s.blocks.len();
        s.apply(&AppEvent::AddBlock);
        assert_eq!(
            s.blocks.len(),
            len_before,
            "AddBlock must not push when every id 1..=255 is taken"
        );
    }

    /// When every id 1..=255 is taken, AddBlock must set a
    /// user-visible warning so the user understands why the
    /// key press did nothing. The warning is shown in the title
    /// bar by the renderer.
    #[test]
    fn add_block_sets_a_warning_when_ids_are_exhausted() {
        let mut s = UiView {
            title: "Voice Bird".to_string(),
            should_quit: false,
            blocks: (1..=u8::MAX)
                .map(|id| Block::new(id, BlockState::PickingLanguage(ListPicker::default())))
                .collect(),
            focus: 0,
            downloads: BTreeMap::new(),
            next_block_id: 1,
            menu: None,
            focus_clock: 0,
            warning: None,
        };
        assert!(s.warning.is_none(), "no warning in steady state");
        s.apply(&AppEvent::AddBlock);
        assert!(
            s.warning.is_some(),
            "AddBlock must surface a warning when refused"
        );
        let msg = s.warning.as_deref().unwrap();
        assert!(
            msg.contains("session limit"),
            "warning should mention the limit; got {msg:?}"
        );
    }

    /// Closing a block after the warning was raised must clear
    /// the warning — the user freed an id and the next AddBlock
    /// will succeed.
    #[test]
    fn block_closed_clears_the_exhaustion_warning() {
        let mut s = UiView {
            title: "Voice Bird".to_string(),
            should_quit: false,
            blocks: (1..=u8::MAX)
                .map(|id| Block::new(id, BlockState::PickingLanguage(ListPicker::default())))
                .collect(),
            focus: 0,
            downloads: BTreeMap::new(),
            next_block_id: 1,
            menu: None,
            focus_clock: 0,
            warning: None,
        };
        s.apply(&AppEvent::AddBlock);
        assert!(s.warning.is_some(), "warning raised on refused AddBlock");
        // Closing the focused block (id 1) frees one id slot.
        s.apply(&AppEvent::BlockClosed { block: s.focused().map_or(1, |block| block.id) });
        assert!(
            s.warning.is_none(),
            "BlockClosed must clear the exhaustion warning once a slot is free"
        );
    }

    /// Closing a block before the warning was raised must NOT
    /// spuriously clear any warning that was set some other way
    /// (future use). Currently there's only the exhaustion
    /// warning; this test guards the equality check.
    #[test]
    fn block_closed_only_clears_the_exhaustion_warning() {
        let mut s = UiView {
            warning: Some("some other future warning".to_string()),
            ..UiView::default()
        };
        s.apply(&AppEvent::AddBlock);
        s.apply(&AppEvent::BlockClosed { block: s.focused().map_or(1, |block| block.id) });
        assert_eq!(
            s.warning.as_deref(),
            Some("some other future warning"),
            "BlockClosed must not clear unrelated warnings"
        );
    }

    #[test]
    fn focus_moves_and_saturates_at_both_ends() {
        let mut s = UiView::default();
        for _ in 0..3 {
            s.apply(&AppEvent::AddBlock);
        }
        assert_eq!(s.focus, 2);
        for _ in 0..5 {
            s.apply(&AppEvent::FocusMoved {
                direction: FocusMove::Prev,
            });
        }
        assert_eq!(s.focus, 0);
        for _ in 0..5 {
            s.apply(&AppEvent::FocusMoved {
                direction: FocusMove::Next,
            });
        }
        assert_eq!(s.focus, 2);
    }

    #[test]
    fn focus_moved_is_noop_when_no_blocks() {
        let mut s = UiView::default();
        s.apply(&AppEvent::FocusMoved {
            direction: FocusMove::Next,
        });
        assert_eq!(s.focus, 0);
    }

    #[test]
    fn picker_moved_targets_only_the_focused_block() {
        let mut s = UiView::default();
        s.apply(&AppEvent::AddBlock);
        s.apply(&select(1, Vec::new()));
        s.apply(&AppEvent::AddBlock);
        s.apply(&AppEvent::PickerMoved {
            direction: PickerMove::Down,
            from_language: None,
            to_language: None,
        });
        let picker_index = match &s.blocks[1].state {
            BlockState::PickingLanguage(picker) => picker.index,
            _ => panic!("block 2 should still be picking"),
        };
        assert_eq!(picker_index, 0);
        assert!(matches!(
            s.blocks[0].state,
            BlockState::Recording { language } if language == english()
        ));
    }

    #[test]
    fn language_selected_targets_block_id_and_records_when_ready() {
        let mut s = UiView::default();
        s.apply(&AppEvent::AddBlock);
        s.apply(&AppEvent::AddBlock);
        s.apply(&select(1, Vec::new()));
        assert!(matches!(
            s.blocks[0].state,
            BlockState::Recording { language } if language == english()
        ));
        assert!(matches!(s.blocks[1].state, BlockState::PickingLanguage(_)));
    }

    #[test]
    fn language_selected_is_noop_when_target_block_is_recording() {
        let mut s = UiView::default();
        s.apply(&AppEvent::AddBlock);
        s.apply(&select(1, Vec::new()));
        s.apply(&select(1, vec![english().live.id]));
        assert!(matches!(
            s.blocks[0].state,
            BlockState::Recording { language } if language == english()
        ));
    }

    #[test]
    fn block_closed_removes_and_clamps_focus() {
        let mut s = UiView::default();
        for _ in 0..3 {
            s.apply(&AppEvent::AddBlock);
        }
        s.apply(&AppEvent::BlockClosed { block: s.focused().map_or(1, |block| block.id) });
        assert_eq!(s.blocks.len(), 2);
        assert_eq!(s.focus, 1);
    }

    #[test]
    fn block_closed_on_first_clamps_focus_to_zero() {
        let mut s = UiView::default();
        for _ in 0..3 {
            s.apply(&AppEvent::AddBlock);
        }
        s.apply(&AppEvent::FocusMoved {
            direction: FocusMove::Prev,
        });
        s.apply(&AppEvent::FocusMoved {
            direction: FocusMove::Prev,
        });
        assert_eq!(s.focus, 0);
        s.apply(&AppEvent::BlockClosed { block: s.focused().map_or(1, |block| block.id) });
        assert_eq!(s.blocks.len(), 2);
        assert_eq!(s.focus, 0);
    }

    #[test]
    fn block_closed_with_no_blocks_is_a_noop() {
        let mut s = UiView::default();
        s.apply(&AppEvent::BlockClosed { block: s.focused().map_or(1, |block| block.id) });
        assert!(s.blocks.is_empty());
    }

    #[test]
    fn queued_close_targets_original_block_after_focus_changes() {
        let mut s = UiView::default();
        s.apply(&AppEvent::AddBlock);
        let close = AppEvent::BlockClosed { block: 1 };
        s.apply(&AppEvent::AddBlock);
        s.apply(&close);
        assert_eq!(s.blocks.iter().map(|block| block.id).collect::<Vec<_>>(), vec![2]);
        assert_eq!(s.focused().unwrap().id, 2);
        s.apply(&close);
        assert_eq!(s.focused().unwrap().id, 2);
    }

    #[test]
    fn next_block_id_monotonic_across_multiple_adds() {
        let mut s = UiView::default();
        for expected in 1u8..=3 {
            s.apply(&AppEvent::AddBlock);
            s.apply(&select(expected, Vec::new()));
            assert_eq!(s.blocks.last().unwrap().id, expected);
            assert_eq!(s.next_block_id, expected + 1);
        }
    }

    #[test]
    fn next_block_id_wraps_to_one_after_u8_max() {
        // Drive the reducer to the cap and confirm the next add
        // restarts from 1 — ids are unique among alive sessions,
        // not unique forever, and a closed block frees its id.
        let mut s = UiView {
            next_block_id: u8::MAX,
            ..Default::default()
        };
        s.apply(&AppEvent::AddBlock);
        assert_eq!(s.blocks.last().unwrap().id, u8::MAX);
        assert_eq!(s.next_block_id, 1, "wraps to 1 after u8::MAX");
    }

    #[test]
    fn apply_leaves_other_fields_untouched() {
        let mut s = UiView {
            title: "Hello".to_string(),
            should_quit: true,
            blocks: Vec::new(),
            focus: 0,
            downloads: BTreeMap::new(),
            next_block_id: 42,
            menu: None,
            focus_clock: 0,
            warning: None,
        };
        s.apply(&AppEvent::AddBlock);
        assert_eq!(s.title, "Hello");
        assert!(s.should_quit);
        assert_eq!(s.next_block_id, 43);
        assert!(matches!(s.blocks[0].state, BlockState::PickingLanguage(_)));
        assert!(s.menu.is_none());
        assert_eq!(s.focus_clock, 1, "AddBlock bumps the focus clock");
    }

    #[test]
    fn download_requested_creates_one_shared_record() {
        let mut s = UiView::default();
        s.apply(&AppEvent::DownloadRequested { model: english().live, attempt: 1 });
        s.apply(&AppEvent::DownloadProgress {
            attempt: 1,
            model: english().live.id,
            bytes: 100,
            total: Some(200),
            bytes_per_sec: 12,
        });
        s.apply(&AppEvent::DownloadRequested { model: english().live, attempt: 1 });
        let row = s.downloads.get(english().live.id).unwrap();
        assert_eq!(row.bytes, 100);
        assert_eq!(row.total, Some(200));
        assert_eq!(row.bytes_per_sec, 12);
    }

    #[test]
    fn download_progress_after_success_is_ignored() {
        let mut s = UiView::default();
        s.apply(&AppEvent::DownloadRequested { model: english().live, attempt: 1 });
        s.apply(&AppEvent::DownloadSucceeded {
            attempt: 1,
            model: english().live.id,
        });
        s.apply(&AppEvent::DownloadProgress {
            attempt: 1,
            model: english().live.id,
            bytes: 10,
            total: None,
            bytes_per_sec: 0,
        });
        assert!(!s.downloads.contains_key(english().live.id));
    }

    #[test]
    fn download_installing_sets_phase() {
        let mut s = UiView::default();
        s.apply(&AppEvent::DownloadRequested { model: english().refine, attempt: 1 });
        s.apply(&AppEvent::DownloadInstalling {
            attempt: 1,
            model: english().refine.id,
        });
        assert_eq!(
            s.downloads.get(english().refine.id).unwrap().phase,
            DownloadPhase::Installing
        );
    }

    #[test]
    fn block_records_only_after_both_models_succeed() {
        let mut s = UiView::default();
        s.apply(&AppEvent::AddBlock);
        s.apply(&select(1, vec![english().live.id, english().refine.id]));
        s.apply(&AppEvent::DownloadSucceeded {
            attempt: 1,
            model: english().live.id,
        });
        assert!(matches!(
            &s.blocks[0].state,
            BlockState::Waiting { pending, .. }
                if pending.as_slice() == [english().refine.id]
        ));
        s.apply(&AppEvent::DownloadSucceeded {
            attempt: 1,
            model: english().refine.id,
        });
        assert!(matches!(
            s.blocks[0].state,
            BlockState::Recording { language } if language == english()
        ));
    }

    #[test]
    fn table_status_reconciles_success_that_preceded_selection() {
        let mut s = UiView::default();
        s.apply(&AppEvent::AddBlock);
        s.apply(&AppEvent::DownloadSucceeded {
            attempt: 1,
            model: english().live.id,
        });
        s.apply(&select(1, vec![english().live.id]));
        assert!(matches!(s.blocks[0].state, BlockState::Waiting { .. }));

        s.apply(&AppEvent::DownloadStatusChanged {
            model: english().live.id.into(),
            attempt: 1,
            from: Some(crate::bus::DownloadStatus::Downloading),
            to: crate::bus::DownloadStatus::Succeeded,
            error: None,
        });

        assert!(matches!(
            s.blocks[0].state,
            BlockState::Recording { language } if language == english()
        ));
    }

    #[test]
    fn table_status_reconciles_failure_that_preceded_selection() {
        let mut s = UiView::default();
        s.apply(&AppEvent::AddBlock);
        s.apply(&AppEvent::DownloadFailed {
            attempt: 1,
            model: english().live.id,
            error: "HTTP 404".to_string(),
        });
        s.apply(&select(1, vec![english().live.id]));
        assert!(matches!(s.blocks[0].state, BlockState::Waiting { .. }));

        s.apply(&AppEvent::DownloadStatusChanged {
            model: english().live.id.into(),
            attempt: 1,
            from: Some(crate::bus::DownloadStatus::Downloading),
            to: crate::bus::DownloadStatus::Failed,
            error: Some("HTTP 404".to_string()),
        });

        assert!(matches!(
            &s.blocks[0].state,
            BlockState::Failed { language, error, .. }
                if *language == english() && error == "HTTP 404"
        ));
    }

    #[test]
    fn model_failure_fails_every_waiter_with_the_message() {
        let mut s = UiView::default();
        for id in 1..=2 {
            s.apply(&AppEvent::AddBlock);
            s.apply(&select(id, vec![english().refine.id]));
        }
        s.apply(&AppEvent::DownloadFailed {
            attempt: 1,
            model: english().refine.id,
            error: "HTTP 404".to_string(),
        });
        for block in &s.blocks {
            match &block.state {
                BlockState::Failed {
                    language, error, ..
                } => {
                    assert_eq!(*language, english());
                    assert_eq!(error, "HTTP 404");
                }
                other => panic!("expected Failed, got {other:?}"),
            }
        }
    }

    #[test]
    fn failed_block_retains_only_other_running_models() {
        let mut s = UiView::default();
        s.apply(&AppEvent::AddBlock);
        s.apply(&select(1, vec![english().live.id, english().refine.id]));
        s.apply(&AppEvent::DownloadFailed {
            attempt: 1,
            model: english().live.id,
            error: "HTTP 404".to_string(),
        });

        assert!(matches!(
            &s.blocks[0].state,
            BlockState::Failed { pending, .. }
                if pending.as_slice() == [english().refine.id]
        ));
        assert_eq!(s.blocks[0].pending_models(), &[english().refine.id]);
    }

    #[test]
    fn claim_failure_has_the_same_ui_semantics() {
        let mut s = UiView::default();
        s.apply(&AppEvent::AddBlock);
        s.apply(&select(1, vec![english().live.id]));
        s.apply(&AppEvent::DownloadClaimFailed {
            attempt: 1,
            model: english().live.id,
            error: "database locked".to_string(),
        });
        assert!(matches!(
            &s.blocks[0].state,
            BlockState::Failed {
                language, error, ..
            } if *language == english() && error == "database locked"
        ));
    }

    #[test]
    fn retry_selection_replaces_failed_state_with_current_pending_models() {
        let mut s = UiView::default();
        s.apply(&AppEvent::AddBlock);
        s.apply(&select(1, vec![english().live.id]));
        s.apply(&AppEvent::DownloadFailed {
            attempt: 1,
            model: english().live.id,
            error: "boom".into(),
        });
        s.apply(&select(1, vec![english().refine.id]));
        assert!(matches!(
            &s.blocks[0].state,
            BlockState::Waiting { pending, .. }
                if pending.as_slice() == [english().refine.id]
        ));
    }

    #[test]
    fn block_closed_drops_only_unshared_pending_downloads() {
        let mut s = UiView::default();
        for model in english().models() {
            s.apply(&AppEvent::DownloadRequested { model, attempt: 1 });
        }
        s.apply(&AppEvent::AddBlock);
        s.apply(&select(1, vec![english().live.id, english().refine.id]));
        s.apply(&AppEvent::AddBlock);
        s.apply(&select(2, vec![english().live.id]));
        s.apply(&AppEvent::FocusMoved {
            direction: FocusMove::Prev,
        });
        s.apply(&AppEvent::BlockClosed { block: s.focused().map_or(1, |block| block.id) });
        assert!(s.downloads.contains_key(english().live.id));
        assert!(!s.downloads.contains_key(english().refine.id));
    }

    #[test]
    fn block_closed_keeps_download_needed_by_a_failed_block() {
        let mut s = UiView::default();
        s.apply(&AppEvent::DownloadRequested { model: english().refine, attempt: 1 });
        s.apply(&AppEvent::AddBlock);
        s.apply(&select(1, vec![english().live.id, english().refine.id]));
        s.apply(&AppEvent::DownloadFailed {
            attempt: 1,
            model: english().live.id,
            error: "live failed".to_string(),
        });
        s.apply(&AppEvent::AddBlock);
        s.apply(&select(2, vec![english().refine.id]));

        s.apply(&AppEvent::BlockClosed { block: s.focused().map_or(1, |block| block.id) });

        assert!(matches!(s.blocks[0].state, BlockState::Failed { .. }));
        assert!(s.downloads.contains_key(english().refine.id));
    }

    #[test]
    fn ratio_clamps_when_bytes_exceed_total() {
        let state = DownloadState {
            phase: DownloadPhase::Fetching,
            bytes: 200,
            total: Some(100),
            bytes_per_sec: 0,
        };
        assert_eq!(state.ratio(), Some(1.0));
    }

    #[test]
    fn ratio_is_none_without_total() {
        let state = DownloadState {
            phase: DownloadPhase::Fetching,
            bytes: 100,
            total: None,
            bytes_per_sec: 0,
        };
        assert!(state.ratio().is_none());
    }

    #[test]
    fn ratio_with_zero_total_is_one() {
        let state = DownloadState {
            phase: DownloadPhase::Fetching,
            bytes: 0,
            total: Some(0),
            bytes_per_sec: 0,
        };
        assert_eq!(state.ratio(), Some(1.0));
    }

    // ----- visible-cap and menu reducer arms -----

    #[test]
    fn five_add_blocks_leave_four_visible_and_one_hidden() {
        let mut s = UiView::default();
        for _ in 0..5 {
            s.apply(&AppEvent::AddBlock);
        }
        assert_eq!(s.blocks.len(), 5);
        let visible: Vec<u8> = s
            .blocks
            .iter()
            .filter(|b| b.visible)
            .map(|b| b.id)
            .collect();
        let hidden: Vec<u8> = s
            .blocks
            .iter()
            .filter(|b| !b.visible)
            .map(|b| b.id)
            .collect();
        // The cap (4) evicts the oldest-focused visible peer when a
        // 5th block arrives. Block 1 was the first to be focused
        // (and never re-focused) so it carries the smallest stamp
        // and gets evicted; blocks 2..5 stay on screen.
        assert_eq!(visible, vec![2, 3, 4, 5]);
        assert_eq!(hidden, vec![1]);
    }

    #[test]
    fn eviction_picks_the_oldest_focused_visible_block_not_the_leftmost() {
        // Six blocks; the cap is 4. Two rounds of eviction: the 5th
        // block evicts block 1 (smallest stamp among visible peers);
        // the 6th block evicts block 2 (now the smallest stamp
        // visible). The result is independent of column order — the
        // leftmost-column block is id=1, and it IS evicted, but the
        // *rule* under test is "smallest stamp", not "leftmost".
        let mut s = UiView::default();
        for _ in 0..6 {
            s.apply(&AppEvent::AddBlock);
        }
        let visible: Vec<u8> = s
            .blocks
            .iter()
            .filter(|b| b.visible)
            .map(|b| b.id)
            .collect();
        let hidden: Vec<u8> = s
            .blocks
            .iter()
            .filter(|b| !b.visible)
            .map(|b| b.id)
            .collect();
        assert_eq!(visible, vec![3, 4, 5, 6]);
        assert_eq!(hidden, vec![1, 2]);
        assert!(!s.blocks[0].visible, "block 1 evicted first");
        assert!(!s.blocks[1].visible, "block 2 evicted second");
    }

    #[test]
    fn session_shown_brings_a_hidden_block_back_and_evicts_oldest_focused() {
        let mut s = UiView::default();
        for _ in 0..5 {
            s.apply(&AppEvent::AddBlock);
        }
        // Block 1 is hidden (evicted when block 5 arrived).
        assert!(!s.blocks[0].visible);
        s.apply(&AppEvent::SessionShown { id: 1 });
        // Block 1 returns on screen; the oldest-focused visible
        // peer is now block 2 (stamp 2), which is evicted. Block 5
        // keeps the largest stamp and stays visible.
        assert!(s.blocks[0].visible, "block 1 back on screen");
        assert!(!s.blocks[1].visible, "block 2 evicted to make room");
        assert!(s.blocks[2].visible, "block 3 stays");
        assert!(s.blocks[3].visible, "block 4 stays");
        assert!(s.blocks[4].visible, "block 5 stays");
        assert!(s.menu.is_none(), "SessionShown closes the menu");
    }

    #[test]
    fn session_shown_on_an_already_visible_id_does_not_evict() {
        let mut s = UiView::default();
        for _ in 0..3 {
            s.apply(&AppEvent::AddBlock);
        }
        let before: Vec<u8> = s
            .blocks
            .iter()
            .filter(|b| b.visible)
            .map(|b| b.id)
            .collect();
        s.apply(&AppEvent::SessionShown { id: 1 });
        let after: Vec<u8> = s
            .blocks
            .iter()
            .filter(|b| b.visible)
            .map(|b| b.id)
            .collect();
        assert_eq!(before, vec![1, 2, 3]);
        assert_eq!(after, vec![1, 2, 3]);
    }

    #[test]
    fn focus_moved_saturates_at_both_ends_across_hidden_blocks() {
        // focus is an index into `blocks`, not the visible subset,
        // so the user can navigate to a hidden block by walking
        // ←/→ past the visible ones.
        let mut s = UiView::default();
        for _ in 0..5 {
            s.apply(&AppEvent::AddBlock);
        }
        assert_eq!(s.focus, 4);
        s.apply(&AppEvent::FocusMoved {
            direction: FocusMove::Next,
        });
        assert_eq!(s.focus, 4, "Next saturates at the last block");
        for _ in 0..10 {
            s.apply(&AppEvent::FocusMoved {
                direction: FocusMove::Prev,
            });
        }
        assert_eq!(s.focus, 0, "Prev saturates at zero");
    }

    #[test]
    fn block_closed_promotes_the_most_recently_focused_hidden_block() {
        let mut s = UiView::default();
        for _ in 0..5 {
            s.apply(&AppEvent::AddBlock);
        }
        // After 5 AddBlocks: blocks=[1..5], block 1 is hidden.
        assert!(!s.blocks[0].visible);
        // Close the focused block (focus=4, block 5). The slot
        // opens; `promote_hidden` brings block 1 back on screen.
        s.apply(&AppEvent::BlockClosed { block: s.focused().map_or(1, |block| block.id) });
        assert_eq!(s.blocks.len(), 4);
        let visible_count = s.blocks.iter().filter(|b| b.visible).count();
        assert_eq!(visible_count, 4, "the window stays at the cap");
        assert!(
            s.blocks[0].visible,
            "block 1 (the only hidden one) was promoted"
        );
        assert!(s.menu.is_none());
    }

    #[test]
    fn menu_opened_sets_index_to_focused_block() {
        let mut s = UiView::default();
        for _ in 0..3 {
            s.apply(&AppEvent::AddBlock);
        }
        s.apply(&AppEvent::FocusMoved {
            direction: FocusMove::Prev,
        });
        s.apply(&AppEvent::FocusMoved {
            direction: FocusMove::Prev,
        });
        assert_eq!(s.focus, 0);
        s.apply(&AppEvent::MenuOpened);
        assert_eq!(s.menu.as_ref().unwrap().index, 0);
    }

    #[test]
    fn menu_moved_clamps_at_row_zero_and_at_last_row() {
        let mut s = UiView::default();
        for _ in 0..5 {
            s.apply(&AppEvent::AddBlock);
        }
        s.apply(&AppEvent::MenuOpened);
        // Five moves up: index stays at 0.
        for _ in 0..5 {
            s.apply(&AppEvent::MenuMoved {
                direction: PickerMove::Up,
            });
        }
        assert_eq!(s.menu.as_ref().unwrap().index, 0);
        // And at the bottom: six Down events land on row 4 (last).
        for _ in 0..6 {
            s.apply(&AppEvent::MenuMoved {
                direction: PickerMove::Down,
            });
        }
        assert_eq!(s.menu.as_ref().unwrap().index, 4);
    }

    #[test]
    fn menu_closed_clears_the_menu() {
        let mut s = UiView::default();
        s.apply(&AppEvent::AddBlock);
        s.apply(&AppEvent::MenuOpened);
        assert!(s.menu.is_some());
        s.apply(&AppEvent::MenuClosed);
        assert!(s.menu.is_none());
    }
}
