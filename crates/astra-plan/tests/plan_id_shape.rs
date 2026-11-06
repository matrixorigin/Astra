//! Shape of generated plan ids: a lowercase slug of the first three words of
//! the goal plus a hex suffix, or a `plan-` id when the goal has no usable
//! characters. Uniqueness within one second is tracked in #1173 and is not
//! asserted here.

use astra_plan::PlanModeState;

fn split_suffix(id: &str) -> (&str, &str) {
    id.rsplit_once('-').expect("id has a suffix")
}

fn is_lower_hex(s: &str) -> bool {
    s.chars()
        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

#[test]
fn slug_uses_first_three_words_lowercased() {
    let id = PlanModeState::generate_plan_id("Refactor The Parser module today");
    let (slug, suffix) = split_suffix(&id);
    assert_eq!(slug, "refactor-the-parser");
    assert_eq!(suffix.len(), 4, "{id}");
    assert!(is_lower_hex(suffix), "{id}");
}

#[test]
fn slug_drops_punctuation_and_is_capped_at_30_chars() {
    let id = PlanModeState::generate_plan_id("fix: auth/login, now!");
    assert!(id.starts_with("fix-authlogin-now-"), "{id}");
    let id = PlanModeState::generate_plan_id(&format!("{} b c", "a".repeat(50)));
    let (slug, _) = split_suffix(&id);
    assert_eq!(slug, "a".repeat(30));
}

#[test]
fn unicode_words_are_kept() {
    let id = PlanModeState::generate_plan_id("修复 登录 问题 now");
    let (slug, suffix) = split_suffix(&id);
    assert_eq!(slug, "修复-登录-问题");
    assert!(is_lower_hex(suffix));
    assert!(id.chars().all(|c| c.is_alphanumeric() || c == '-'), "{id}");
}

#[test]
fn empty_goal_uses_plan_prefix_with_eight_hex_digits() {
    for goal in ["", "   ", "\n\t"] {
        let id = PlanModeState::generate_plan_id(goal);
        let suffix = id.strip_prefix("plan-").unwrap_or_else(|| panic!("{id}"));
        assert_eq!(suffix.len(), 8, "{id}");
        assert!(is_lower_hex(suffix), "{id}");
    }
}

#[test]
fn new_state_starts_in_planning_with_the_goal() {
    let state = PlanModeState::new("ship the release".into());
    assert!(state.plan.subtasks.is_empty());
    assert_eq!(state.infer_phase(), astra_plan::PlanPhase::Planning);
}
