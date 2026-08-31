//! Per-room activation funnel.
//!
//! When the user activates a non-free room for the first time
//! (or after they cleared stored bindings), the TUI walks
//! through a per-role setup wizard. The wizard's state machine
//! is here — the rendering lives in `ui::render_funnel_modal`
//! and the wiring into `App::activate_room` is one if-let in
//! `app.rs`.
//!
//! The wizard does not block the user: Esc cancels back to
//! the previous room, Enter advances to the next role (or
//! commits the prompt and exits on the last step).
//!
//! The prompt step launches `$EDITOR` (or a fallback) so the
//! user has a real editor — not a TUI single-line input — for
//! editing the agent's system prompt. The TUI suspends raw
//! mode while the editor runs (otherwise nano/vi see garbled
//! input) and re-acquires raw mode on return. The file is
//! read back, trimmed, stored in `RoomFunnelState::prompt_draft`.
//! Esc on the prompt step keeps the empty draft → falls back
//! to the room's built-in `prompt_template`.

use crate::room::{Room, SourceKind};

/// State of the in-progress funnel. Index 0 is the first role;
/// the last role is followed by the assistant-prompt step, which
/// is `current_step == room.role_constraints.len()`.
#[derive(Debug, Clone)]
pub struct RoomFunnelState {
    /// Index into `Room::role_constraints` AND into the
    /// one-step-past for the prompt step. Saturated.
    pub current_step: usize,
    /// Draft system prompt the user is editing in `$EDITOR`.
    /// Empty when the user hasn't opened the editor yet.
    pub prompt_draft: String,
    /// Per-role source bindings the user accepts. Index is
    /// `role_constraints` index (same as `current_step`).
    pub role_bindings: Vec<RoleBindingDraft>,
    /// Room that the funnel is activating. Cached so the
    /// dispatchers can render modal content without round-trips
    /// to `App::rooms`.
    pub room_slug: String,
}

impl RoomFunnelState {
    /// Start a funnel for the given room. The first role step is
    /// 0; if the room has no role constraints the only step is
    /// the prompt step (index 0 = the prompt slot).
    pub fn new(room: &Room) -> Self {
        let role_bindings: Vec<RoleBindingDraft> = room
            .role_constraints
            .iter()
            .map(|c| RoleBindingDraft {
                role_slug: c.role_slug.clone(),
                source_kind: c.source_kind,
                selected_index: None,
                selected_name: None,
            })
            .collect();
        Self {
            current_step: 0,
            prompt_draft: String::new(),
            role_bindings,
            room_slug: room.slug.clone(),
        }
    }

    pub fn at_prompt_step(&self) -> bool {
        self.current_step == self.role_bindings.len()
    }

    /// True when the funnel has advanced past the prompt step
    /// (i.e. the user pressed Enter from the prompt slot and
    /// the wizard is now ready to be closed).
    pub fn at_commit_step(&self) -> bool {
        self.current_step > self.role_bindings.len()
    }

    /// Advance by one step. Unlike a saturation guard, this
    /// allows `current_step` to land ON every role, ON the
    /// prompt step, and PAST the prompt step (the "commit"
    /// sentinel). Total legitimate values are
    /// `0..=role_bindings.len() + 1`, where `len()+1` is the
    /// commit sentinel — see `at_commit_step`.
    pub fn advance(&mut self) {
        self.current_step += 1;
    }

    pub fn total_steps(&self) -> usize {
        // Logical steps visible to the user: one per role +
        // the prompt step. The commit sentinel is NOT a step;
        // it's the "funnel is closed, write the prompt" exit.
        self.role_bindings.len() + 1
    }

    /// Escape outcome on the prompt step: discard the draft,
    /// fall back to the room's built-in `prompt_template` (or
    /// empty string if there isn't one).
    pub fn revert_prompt(&mut self) {
        self.prompt_draft.clear();
    }

    /// Move the current role's device/app cursor up by one row.
    /// Saturates at the top. A no-op on the prompt step AND on
    /// the commit sentinel — both are out of the role grid.
    pub fn cursor_up(&mut self) {
        if self.at_prompt_step() || self.at_commit_step() {
            return;
        }
        let entry = &mut self.role_bindings[self.current_step];
        entry.selected_index = Some(match entry.selected_index {
            None => 0,
            Some(0) => 0,
            Some(i) => i - 1,
        });
    }

    /// Move the current role's device/app cursor down by one row.
    /// Caller passes the visible row count (after the funnel has
    /// filtered devices or apps for the current source_kind).
    /// Saturates at the last row. A no-op on the prompt step
    /// AND on the commit sentinel.
    pub fn cursor_down(&mut self, max: usize) {
        if self.at_prompt_step() || self.at_commit_step() {
            return;
        }
        if max == 0 {
            return;
        }
        let entry = &mut self.role_bindings[self.current_step];
        let next = match entry.selected_index {
            None => 0,
            Some(i) if i + 1 < max => i + 1,
            Some(i) => i,
        };
        entry.selected_index = Some(next);
    }

    /// Resolve the current step's `selected_index` to a name
    /// supplied by the caller. The caller passes the visible
    /// rows (already filtered by `source_kind`); we just
    /// bounds-check the index and store the matching name.
    ///
    /// Why the caller passes rows: the funnel state lives in
    /// the lib crate (`voice_bird_cli::funnel`) which doesn't
    /// see `crate::platform::AudioDevice` (the platform module
    /// is bin-only). The lib stores the resolved name as a
    /// plain `String` so the commit path can look it up by
    /// name (stable across inventory refreshes) instead of by
    /// index (unstable).
    ///
    /// `visible_names` is the row-at-index list — exactly the
    /// same order the picker rendered and the cursor bounds
    /// were computed against. Out-of-range cursor clears
    /// `selected_name` so a stale pick can't survive an
    /// inventory refresh.
    pub fn record_selected_name<S: AsRef<str>>(&mut self, visible_names: &[S]) {
        if self.at_prompt_step() || self.at_commit_step() {
            return;
        }
        let Some(entry) = self.role_bindings.get_mut(self.current_step) else {
            return;
        };
        entry.selected_name = entry
            .selected_index
            .and_then(|i| visible_names.get(i))
            .map(|s| s.as_ref().to_string());
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleBindingDraft {
    pub role_slug: String,
    pub source_kind: SourceKind,
    /// Cursor over the filtered Devices or Apps list. `None`
    /// means "no pick yet, use the room/agent default".
    /// Persists across `current_step` changes (the cursor
    /// lives on the role, not on the wizard).
    pub selected_index: Option<usize>,
    /// Resolved name of the picked row (device name for
    /// `DeviceInput`/`DeviceOutput`, app display name for
    /// `AppLoopback`). Captured at cursor-move time so the
    /// commit path can resolve to the actual `AudioDevice` /
    /// `AppSession` by name — the index is unstable across
    /// inventory refreshes, but the name is what the user
    /// saw in the picker. `None` when the user hasn't moved
    /// the cursor onto a real row yet.
    pub selected_name: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::room::{RoleConstraint, RoleDef};

    fn two_role_room() -> Room {
        Room {
            slug: "x".into(),
            name: "X".into(),
            icon: None,
            roles: vec![
                RoleDef { slug: "a".into(), name: "A".into() },
                RoleDef { slug: "b".into(), name: "B".into() },
            ],
            agent: None,
            requires_pro: false,
            requires_cloud: true,
            prompt_template: "default".into(),
            role_constraints: vec![
                RoleConstraint {
                    role_slug: "a".into(),
                    source_kind: SourceKind::DeviceInput,
                    required_app_slug: None,
                    device_required: true,
                },
                RoleConstraint {
                    role_slug: "b".into(),
                    source_kind: SourceKind::AppLoopback,
                    required_app_slug: None,
                    device_required: false,
                },
            ],
        }
    }

    #[test]
    fn fresh_state_at_first_role() {
        let f = RoomFunnelState::new(&two_role_room());
        assert_eq!(f.current_step, 0);
        assert_eq!(f.total_steps(), 3);
        assert!(!f.at_prompt_step());
        assert!(!f.at_commit_step());
        assert_eq!(f.role_bindings.len(), 2);
        assert!(f.role_bindings[0].selected_index.is_none());
        assert!(f.role_bindings[0].selected_name.is_none());
    }

    #[test]
    fn advance_walks_through_roles_then_prompt() {
        let mut f = RoomFunnelState::new(&two_role_room());
        f.advance();
        assert_eq!(f.current_step, 1);
        assert!(!f.at_prompt_step());
        f.advance();
        assert_eq!(f.current_step, 2);
        assert!(f.at_prompt_step());
        f.advance();
        assert_eq!(f.current_step, 3);
        assert!(f.at_commit_step());
    }

    #[test]
    fn advance_is_not_saturated_so_user_can_walk_every_role() {
        let mut f = RoomFunnelState::new(&two_role_room());
        assert_eq!(f.current_step, 0, "start at role 0");
        f.advance();
        assert_eq!(f.current_step, 1, "role 1 after first Enter");
        assert!(!f.at_prompt_step());
        assert!(!f.at_commit_step());
        f.advance();
        assert_eq!(f.current_step, 2, "prompt step after second Enter");
        assert!(f.at_prompt_step());
        assert!(!f.at_commit_step());
        f.advance();
        assert_eq!(f.current_step, 3, "commit sentinel after third Enter");
        assert!(f.at_commit_step());
    }

    #[test]
    fn revert_prompt_clears_draft() {
        let mut f = RoomFunnelState::new(&two_role_room());
        f.prompt_draft = "user typed this".into();
        f.revert_prompt();
        assert!(f.prompt_draft.is_empty());
    }

    #[test]
    fn free_room_has_only_prompt_step() {
        let r = Room::free_room();
        let f = RoomFunnelState::new(&r);
        assert_eq!(f.total_steps(), 1);
        assert!(f.at_prompt_step());
    }

    #[test]
    fn cursor_down_walks_through_visible_rows() {
        let mut f = RoomFunnelState::new(&two_role_room());
        f.cursor_down(3);
        assert_eq!(f.role_bindings[0].selected_index, Some(0));
        f.cursor_down(3);
        assert_eq!(f.role_bindings[0].selected_index, Some(1));
        f.cursor_down(3);
        assert_eq!(f.role_bindings[0].selected_index, Some(2));
        f.cursor_down(3);
        assert_eq!(f.role_bindings[0].selected_index, Some(2));
    }

    #[test]
    fn cursor_up_saturates_at_top() {
        let mut f = RoomFunnelState::new(&two_role_room());
        f.cursor_down(5);
        f.cursor_down(5);
        assert_eq!(f.role_bindings[0].selected_index, Some(1));
        f.cursor_up();
        assert_eq!(f.role_bindings[0].selected_index, Some(0));
        f.cursor_up();
        f.cursor_up();
        assert_eq!(f.role_bindings[0].selected_index, Some(0));
    }

    #[test]
    fn cursor_is_noop_on_prompt_step() {
        let mut f = RoomFunnelState::new(&two_role_room());
        f.advance();
        f.advance();
        assert!(f.at_prompt_step());
        f.cursor_up();
        f.cursor_down(99);
        assert!(f.at_prompt_step());
    }

    #[test]
    fn cursor_is_noop_on_commit_sentinel() {
        let mut f = RoomFunnelState::new(&two_role_room());
        f.advance();
        f.advance();
        f.advance();
        assert!(f.at_commit_step());
        f.cursor_up();
        f.cursor_down(99);
        assert!(f.at_commit_step());
    }

    #[test]
    fn cursor_down_with_max_zero_stays_at_none() {
        let mut f = RoomFunnelState::new(&two_role_room());
        f.cursor_down(0);
        assert_eq!(f.role_bindings[0].selected_index, None);
    }

    #[test]
    fn record_selected_name_resolves_at_cursor() {
        let mut f = RoomFunnelState::new(&two_role_room());
        f.cursor_down(2); // row 0
        f.record_selected_name(&["EPOS PC 8 USB", "HD Pro Webcam C920"]);
        assert_eq!(
            f.role_bindings[0].selected_name.as_deref(),
            Some("EPOS PC 8 USB")
        );
        f.cursor_down(2); // row 1
        f.record_selected_name(&["EPOS PC 8 USB", "HD Pro Webcam C920"]);
        assert_eq!(
            f.role_bindings[0].selected_name.as_deref(),
            Some("HD Pro Webcam C920")
        );
    }

    #[test]
    fn record_selected_name_clears_when_index_out_of_range() {
        let mut f = RoomFunnelState::new(&two_role_room());
        // Visible list has 2 rows (filtered from the
        // caller's perspective — the dispatcher would have
        // recomputed it after an inventory refresh).
        f.cursor_down(2);
        f.record_selected_name(&["EPOS PC 8 USB", "HD Pro Webcam C920"]);
        assert_eq!(f.role_bindings[0].selected_name.as_deref(), Some("EPOS PC 8 USB"));
        // Now simulate an inventory refresh that DROPPED
        // the visible list to 1 row. The cursor is still
        // within bounds (0..2) of the OLD max but past the
        // end of the NEW list. record_selected_name must
        // clear selected_name because the row no longer
        // exists.
        f.cursor_down(2); // 0 → 1
        f.cursor_down(2); // 1 → saturates at 1 (max-1)
        f.cursor_down(2); // still saturated at 1
        assert_eq!(f.role_bindings[0].selected_index, Some(1));
        // Caller re-resolves after the refresh; cursor is
        // still Some(1) but the new list has only 1 row.
        f.record_selected_name(&["EPOS PC 8 USB"]);
        assert!(
            f.role_bindings[0].selected_name.is_none(),
            "stale name must be cleared when index is past the visible end"
        );
    }

    #[test]
    fn record_selected_name_noop_on_prompt_step() {
        let mut f = RoomFunnelState::new(&two_role_room());
        f.advance();
        f.advance();
        assert!(f.at_prompt_step());
        f.record_selected_name(&["anything"]);
        // Step 2 is the prompt step; no role_bindings[2] exists.
        // Make sure the function didn't panic and didn't
        // accidentally write somewhere it shouldn't.
        assert!(f.role_bindings.len() == 2);
        assert!(f.role_bindings[0].selected_name.is_none());
        assert!(f.role_bindings[1].selected_name.is_none());
    }

    #[test]
    fn record_selected_name_preserves_other_steps() {
        // Role 0's name must survive role 1's cursor moves —
        // `record_selected_name` only mutates the CURRENT step.
        let mut f = RoomFunnelState::new(&two_role_room());
        f.cursor_down(2);
        f.record_selected_name(&["EPOS PC 8 USB", "HD Pro Webcam C920"]);
        let role0 = f.role_bindings[0].selected_name.clone();
        assert_eq!(role0.as_deref(), Some("EPOS PC 8 USB"));
        f.advance();
        f.cursor_down(2);
        f.record_selected_name(&["Chrome", "Zoom"]);
        assert_eq!(
            f.role_bindings[0].selected_name, role0,
            "role 0's name must be untouched when role 1 cursor moves"
        );
        assert_eq!(f.role_bindings[1].selected_name.as_deref(), Some("Chrome"));
    }
}
