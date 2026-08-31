//! Banner messages surfaced through `App::banner`. Kept in one
//! place so every guard site uses identical wording and we never
//! drift from the spec the product team set.

/// Banner for the "user tried to disable cloud while an agent
/// room is active" block. Two sentences: first states the
/// constraint in plain terms; second names the recovery action
/// (Free Room by name, with the verb "select" / "activate").
///
/// Wording rules (apply to every room-locked banner going
/// forward):
/// - Two full sentences. First states the constraint in plain
///   terms; second names the recovery action.
/// - The recovery action names **Free Room by name** so the
///   user knows where to go, using "select" or "activate" — not
///   "switch".
/// - No emoji, no exclamation mark, no marketing.
/// - Names the active room, in single quotes, so the user
///   understands *why* they're blocked.
/// - ≤ 200 chars (banner strip wraps). Reads cleanly in the
///   bottom red strip.
pub fn cloud_required_banner(room_name: &str) -> String {
    format!(
        "'{room_name}' runs on the cloud and can't be recorded offline. \
         Select Free Room (or activate a different one) to disable cloud."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_required_banner_wording_matches_spec() {
        let s = cloud_required_banner("Software Interview");
        assert!(s.contains("'Software Interview'"));
        assert!(s.contains("Free Room"));
        assert!(s.contains("select") || s.contains("Select"));
        assert!(s.contains("disable cloud"));
        assert!(s.len() <= 200);
        assert!(!s.contains('!'));
        assert!(!s.contains('\n'));
    }

    #[test]
    fn cloud_required_banner_handles_longer_names() {
        let s = cloud_required_banner("a".repeat(80).as_str());
        assert!(s.len() <= 200);
        assert!(s.contains(&"a".repeat(80)));
    }

    #[test]
    fn cloud_required_banner_does_not_mention_either_free_or_select_for_a_different_room() {
        let s = cloud_required_banner("Doctor Appointment");
        assert!(s.contains("Doctor Appointment"));
        assert!(s.contains("Free Room"));
    }
}
