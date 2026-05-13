//! Timeline behaviour (RED).

#![cfg(test)]

use super::model::{StaticTurnSource, Timeline, TimelineTurn};

fn turn(t: u32, tin: u64, tout: u64, tools: u32, user: &str, err: Option<&str>) -> TimelineTurn {
    TimelineTurn {
        turn: t,
        started_at: format!("2024-01-15T10:{:02}:00Z", t),
        duration_ms: Some(1500 + (t as u64) * 100),
        model: Some("sonnet-4.6".into()),
        tokens_in: Some(tin),
        tokens_out: Some(tout),
        tool_count: Some(tools),
        user_preview: Some(user.into()),
        assistant_preview: Some(format!("reply to turn {t}")),
        error: err.map(String::from),
        cumulative_tokens_in: 0,
        cumulative_tokens_out: 0,
        ttft_ms: None,
        context_ms: None,
        memoria_ms: None,
        llm_rounds: None,
        selected_skills: None,
        total_tool_ms: None,
        total_llm_ms: None,
        tool_calls: Vec::new(),
        user_input: None,
        assistant_output: None,
    }
}

fn fixture_source() -> StaticTurnSource {
    StaticTurnSource::new(vec![
        turn(1, 500, 200, 0, "hi", None),
        turn(2, 800, 400, 2, "read the file", None),
        turn(3, 1200, 600, 1, "fix the bug", None),
    ])
}

fn new_timeline() -> Timeline {
    Timeline::new(fixture_source(), "sess_test")
}

// ─── Basics ────────────────────────────────────────────────────────

#[test]
fn new_loads_turns_from_source() {
    let tl = new_timeline();
    assert_eq!(tl.total(), 3);
    assert!(!tl.is_empty());
    assert_eq!(tl.turns().len(), 3);
}

#[test]
fn new_selects_first_turn_by_default() {
    let tl = new_timeline();
    assert_eq!(tl.selected(), Some(0));
    let t = tl.selected_turn().expect("selection");
    assert_eq!(t.turn, 1);
}

#[test]
fn empty_source_yields_no_selection() {
    let src = StaticTurnSource::new(vec![]);
    let tl = Timeline::new(src, "sess_empty");
    assert!(tl.is_empty());
    assert_eq!(tl.selected(), None);
    assert!(tl.selected_turn().is_none());
}

// ─── Cumulative roll-up ───────────────────────────────────────────

#[test]
fn cumulative_tokens_in_accumulate_left_to_right() {
    let tl = new_timeline();
    let ts = tl.turns();
    assert_eq!(ts[0].cumulative_tokens_in, 500);
    assert_eq!(ts[1].cumulative_tokens_in, 500 + 800);
    assert_eq!(ts[2].cumulative_tokens_in, 500 + 800 + 1200);
}

#[test]
fn cumulative_tokens_out_accumulate_left_to_right() {
    let tl = new_timeline();
    let ts = tl.turns();
    assert_eq!(ts[0].cumulative_tokens_out, 200);
    assert_eq!(ts[1].cumulative_tokens_out, 200 + 400);
    assert_eq!(ts[2].cumulative_tokens_out, 200 + 400 + 600);
}

#[test]
fn cumulative_ignores_none_values() {
    // Turn with tokens_in=None should not break accumulation.
    let src = StaticTurnSource::new(vec![
        turn(1, 100, 50, 0, "a", None),
        {
            let mut t = turn(2, 0, 0, 0, "b", None);
            t.tokens_in = None;
            t.tokens_out = None;
            t
        },
        turn(3, 200, 100, 0, "c", None),
    ]);
    let tl = Timeline::new(src, "sess_test");
    let ts = tl.turns();
    assert_eq!(ts[0].cumulative_tokens_in, 100);
    assert_eq!(ts[1].cumulative_tokens_in, 100, "None tokens_in = 0 delta");
    assert_eq!(ts[2].cumulative_tokens_in, 300);
}

#[test]
fn grand_totals_match_last_turn_cumulative() {
    let tl = new_timeline();
    assert_eq!(tl.grand_total_tokens_in(), 500 + 800 + 1200);
    assert_eq!(tl.grand_total_tokens_out(), 200 + 400 + 600);
}

// ─── Navigation ────────────────────────────────────────────────────

#[test]
fn navigation_wraps_around() {
    let mut tl = new_timeline();
    let n = tl.total();
    assert!(n > 1);
    tl.move_up();
    assert_eq!(tl.selected(), Some(n - 1));
    tl.move_down();
    assert_eq!(tl.selected(), Some(0));
}

#[test]
fn move_down_advances() {
    let mut tl = new_timeline();
    tl.move_down();
    assert_eq!(tl.selected(), Some(1));
    assert_eq!(tl.selected_turn().unwrap().turn, 2);
}

#[test]
fn move_on_empty_is_noop() {
    let mut tl = Timeline::new(StaticTurnSource::new(vec![]), "sess_empty");
    tl.move_down();
    tl.move_up();
    assert_eq!(tl.selected(), None);
}

// ─── Error flagging ────────────────────────────────────────────────

#[test]
fn error_turns_are_flagged() {
    let src = StaticTurnSource::new(vec![
        turn(1, 100, 50, 0, "hi", None),
        turn(2, 200, 100, 0, "boom", Some("rate limited")),
    ]);
    let tl = Timeline::new(src, "sess_test");
    assert!(!tl.turns()[0].is_error());
    assert!(tl.turns()[1].is_error());
}
