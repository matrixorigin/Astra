//! Real-journal wire-shape regression tests.
//!
//! Four independent review rounds caught the harness making optimistic
//! assumptions about journal shape that didn't match what the runtime
//! actually writes to `~/.astra/sessions/<id>.jsonl`. This integration
//! test suite pins the wire contract via SYNTHETIC FIXTURES that are
//! structurally identical to real journals as captured on 2026-05-01.
//!
//! Each test corresponds to one criterion the harness depends on.
//! Ground-truth values were derived by hand from the original real
//! journal (see `tests/fixtures/README.md` for refresh steps).
//!
//! If the runtime ever changes the wire shape, these tests fail. That
//! is the entire point — silent drift is what R2/R3/R4 caught, and
//! only explicit fixture-vs-code matching would have caught it earlier.
//!
//! Tests live at the `tests/` integration level so they exercise
//! `astra_test_harness`'s public API the way a real embedder would.

use std::path::PathBuf;

use astra_test_harness::criteria::{
    Criterion, CriterionResult, evaluate_deterministic_with_session,
};
use astra_test_harness::runner::RunOutcome;
use astra_test_harness::session_capture::load_session_from_path;

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn outcome_placeholder() -> RunOutcome {
    // External integration test — `RunOutcome` is `#[non_exhaustive]`
    // so struct literals and struct-update are both blocked. The
    // intended downstream pattern is `new(model).with_*(...)` setters.
    RunOutcome::new("fixture-model").with_session_id("fixture-sess-legacy")
}

// ── Legacy fixture: llm_round with nested tool_calls[] ──
//
// This is the dominant real-journal shape. Ground truth derived by
// line-counting the fixture:
//
//   11 lines total; one each of session_start,
//   context_assembly_recorded, llm_request_full, llm_response_full,
//   turn, turn_evaluation, session_end; 4 llm_round events.
//
//   tool_calls (nested in 3 of the 4 llm_round events, first-seen
//   order, de-duplicated):  git_diff, git_log
//
// The 4th llm_round has NO tool_calls (the "final synthesis" round),
// which is normal in real sessions and exercises the shape-1 loop's
// handling of missing / empty nested arrays.

#[test]
fn legacy_fixture_loads_correct_event_count() {
    let cap = load_session_from_path(
        "fixture-sess-legacy",
        &fixture_path("fixture_realistic_legacy.jsonl"),
    )
    .expect("fixture should load");
    assert_eq!(
        cap.events.len(),
        11,
        "fixture has 11 events; loader must preserve every one"
    );
    assert_eq!(cap.skipped_lines, 0, "no malformed lines in fixture");
}

#[test]
fn legacy_fixture_counts_llm_round_correctly() {
    let cap = load_session_from_path(
        "fixture-sess-legacy",
        &fixture_path("fixture_realistic_legacy.jsonl"),
    )
    .expect("fixture should load");
    assert_eq!(
        cap.count_events("llm_round"),
        4,
        "fixture has 4 llm_round events (3 with nested tool_calls, 1 synthesis round)"
    );
    assert_eq!(cap.count_events("turn"), 1);
    assert_eq!(cap.count_events("session_start"), 1);
    assert_eq!(
        cap.count_events("tool_invocation"),
        0,
        "legacy journals do NOT emit top-level tool_invocation"
    );
}

#[test]
fn legacy_fixture_tools_invoked_walks_nested_tool_calls() {
    // This is R4's Blocker regression: `tools_invoked()` must walk
    // `llm_round.tool_calls[]` for the legacy layout, not just the
    // non-existent top-level `tool_invocation` events. Ground truth
    // from the fixture: git_diff appears twice in sequential rounds,
    // then git_log once. De-duped first-seen order: [git_diff, git_log].
    let cap = load_session_from_path(
        "fixture-sess-legacy",
        &fixture_path("fixture_realistic_legacy.jsonl"),
    )
    .expect("fixture should load");
    let tools = cap.tools_invoked();
    assert_eq!(
        tools,
        vec!["git_diff".to_string(), "git_log".to_string()],
        "tools_invoked must walk llm_round.tool_calls[] and return [git_diff, git_log] in first-seen order; got {tools:?}"
    );
}

#[test]
fn legacy_fixture_journal_tool_called_criterion_matches() {
    // End-to-end contract: a case using `journal_tool_called { name: "git_diff" }`
    // must PASS against a real-shape fixture. Before the R4 fix this
    // criterion returned the `no-session` FAIL path on EVERY real
    // session because tools_invoked() never saw the nested shape.
    let cap = load_session_from_path(
        "fixture-sess-legacy",
        &fixture_path("fixture_realistic_legacy.jsonl"),
    )
    .expect("fixture should load");
    let outcome = outcome_placeholder();

    for name in ["git_diff", "git_log"] {
        let r: Vec<CriterionResult> = evaluate_deterministic_with_session(
            &[Criterion::JournalToolCalled {
                name: name.into(),
                optional: false,
            }],
            &outcome,
            Some(&cap),
        );
        assert!(
            r[0].passed,
            "journal_tool_called {{name: {name:?}}} must PASS on a real-shape fixture; detail = {}",
            r[0].detail,
        );
    }

    // Negative control: a tool that wasn't called must FAIL.
    let r = evaluate_deterministic_with_session(
        &[Criterion::JournalToolCalled {
            name: "never_called_tool".into(),
            optional: false,
        }],
        &outcome,
        Some(&cap),
    );
    assert!(
        !r[0].passed,
        "journal_tool_called with an absent tool must FAIL to prove the positive case wasn't a tautology"
    );
}

#[test]
fn legacy_fixture_session_event_count_llm_round_matches() {
    // `session_event_count { event_type: llm_round, min: 1 }` should
    // PASS with ground truth 4 ≥ 1. Also tests the strict-fail path by
    // asserting `min: 5` (above ground truth) fails.
    let cap = load_session_from_path(
        "fixture-sess-legacy",
        &fixture_path("fixture_realistic_legacy.jsonl"),
    )
    .expect("fixture should load");
    let outcome = outcome_placeholder();

    let pass = evaluate_deterministic_with_session(
        &[Criterion::SessionEventCount {
            event_type: "llm_round".into(),
            min: 1,
            optional: false,
        }],
        &outcome,
        Some(&cap),
    );
    assert!(
        pass[0].passed,
        "4 llm_rounds >= 1 must PASS: {}",
        pass[0].detail
    );

    let fail = evaluate_deterministic_with_session(
        &[Criterion::SessionEventCount {
            event_type: "llm_round".into(),
            min: 5,
            optional: false,
        }],
        &outcome,
        Some(&cap),
    );
    assert!(
        !fail[0].passed,
        "4 llm_rounds >= 5 must FAIL: {}",
        fail[0].detail
    );
}

// ── Step-events fixture: <id>/step_events.jsonl ──
//
// Ground truth: 4 events (StepCreated, StepStarted, ToolCallCompleted,
// StepCompleted). Tool name `list_dir` appears once via
// ToolCallCompleted.payload.tool_name.

#[test]
fn step_events_fixture_counts_and_tool_match() {
    let cap = load_session_from_path(
        "fixture-sess-step",
        &fixture_path("fixture_realistic_step_events.jsonl"),
    )
    .expect("fixture should load");
    assert_eq!(cap.events.len(), 4);
    assert_eq!(cap.count_events("ToolCallCompleted"), 1);
    assert_eq!(cap.count_events("StepCreated"), 1);
    assert_eq!(
        cap.tools_invoked(),
        vec!["list_dir".to_string()],
        "step-events ToolCallCompleted must expose tool_name"
    );
}

// ── Cross-layout merge: both legacy + step_events for the same session ──

#[test]
fn merged_session_exposes_union_of_tool_names() {
    // When a session has BOTH layouts (common for runs that opted into
    // step-event logging mid-run), the loader MUST return the union.
    // Pre-R4 the loader returned early on legacy, silently hiding any
    // step-events-only tool.
    //
    // This test exercises the merge path by loading each fixture
    // separately then asserting that appending them (which is what
    // `load_session` does internally) produces the expected union.
    let legacy = load_session_from_path(
        "fixture-sess-legacy",
        &fixture_path("fixture_realistic_legacy.jsonl"),
    )
    .expect("legacy fixture");
    let step = load_session_from_path(
        "fixture-sess-step",
        &fixture_path("fixture_realistic_step_events.jsonl"),
    )
    .expect("step fixture");

    // Reconstruct the merged view manually to mirror what load_session
    // does when both files exist.
    let mut merged = legacy.clone();
    merged.events.extend(step.events);

    // Build a new SessionCapture-like wrapper — since SessionCapture is
    // #[non_exhaustive] we can't literal-construct from outside. Reuse
    // `merged` as-is; it's still valid.
    let tools = merged.tools_invoked();
    for expected in ["git_diff", "git_log", "list_dir"] {
        assert!(
            tools.contains(&expected.to_string()),
            "merged layout must expose {expected:?}; got {tools:?}"
        );
    }
}
