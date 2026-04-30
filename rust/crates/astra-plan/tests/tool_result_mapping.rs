//! Contract: how `astra_tools::ToolResult` translates to
//! `ObservedOutcome::ToolCall`.
//!
//! This is the ONLY bridge between the external tool world and the typed
//! plan world. If any of these invariants drift, the whole observation/diff
//! loop becomes a lie — a tool could fail and the executor would report
//! "satisfied" (or vice versa). Every test here encodes one of those
//! single-point failure modes.

use astra_plan::action_plan::{Action, ObservedOutcome, observation_from_tool_result};
use astra_tools::ToolResult;
use serde_json::json;

fn action(idx: u32, tool: &str) -> Action {
    Action::new(idx, tool, json!({"arg": "v"}))
}

// ─── Invariant 1: is_error=false ⇒ success=true, index/tool echoed ──────────
#[test]
fn successful_tool_result_becomes_successful_observation() {
    let a = action(3, "read_file");
    let tr = ToolResult {
        output: "contents of file".into(),
        metadata: None,
        is_error: false,
    };

    let obs = observation_from_tool_result(&a, tr);

    let ObservedOutcome::ToolCall {
        action_index,
        tool,
        success,
        result,
    } = obs;
    assert_eq!(action_index, 3);
    assert_eq!(tool, "read_file");
    assert!(success);
    assert_eq!(result["output"], "contents of file");
}

// ─── Invariant 2: is_error=true ⇒ success=false ─────────────────────────────
//
// `is_error` is the SINGLE source of truth for success. Even if the output
// looks pristine ("OK"), a tool that flagged itself as error stays errored.
#[test]
fn error_flagged_tool_result_becomes_failing_observation_even_with_clean_output() {
    let a = action(0, "bash");
    let tr = ToolResult {
        output: "OK, everything fine".into(),
        metadata: None,
        is_error: true,
    };

    let ObservedOutcome::ToolCall {
        success, result, ..
    } = observation_from_tool_result(&a, tr);

    assert!(!success, "is_error=true must map to success=false");
    // Output is still preserved for audit.
    assert_eq!(result["output"], "OK, everything fine");
}

// ─── Invariant 3: success=true WITH "error"-like output stays success ──────
//
// Mirror of invariant 2. The legacy `ToolResult::from_string` sniffs for
// "Error" prefixes, but this bridge must NOT re-do that. Once a tool
// reports `is_error=false`, we trust it. Drifting here would have the
// observation layer silently override tool semantics.
#[test]
fn success_with_error_like_output_stays_success() {
    let a = action(0, "bash");
    let tr = ToolResult {
        output: "Error: this looks bad but the tool said is_error=false".into(),
        metadata: None,
        is_error: false,
    };

    let ObservedOutcome::ToolCall { success, .. } = observation_from_tool_result(&a, tr);
    assert!(
        success,
        "observation layer must not second-guess is_error via string scanning",
    );
}

// ─── Invariant 4: metadata is included as an object when present ────────────
#[test]
fn metadata_is_attached_under_metadata_key_when_present() {
    let a = action(1, "bash");
    let mut meta = serde_json::Map::new();
    meta.insert("exit_code".into(), json!(0));
    meta.insert("stderr_bytes".into(), json!(42));
    let tr = ToolResult {
        output: "done".into(),
        metadata: Some(meta),
        is_error: false,
    };

    let ObservedOutcome::ToolCall { result, .. } = observation_from_tool_result(&a, tr);
    assert_eq!(result["metadata"]["exit_code"], 0);
    assert_eq!(result["metadata"]["stderr_bytes"], 42);
}

// ─── Invariant 5: metadata=None ⇒ the `metadata` key is ABSENT, not null ───
//
// Downstream audit hashes and prompt renderers treat "missing" differently
// from "null". A null metadata would make the result payload wider and
// could change `result_hash`. Keep the shape sparse: omit when absent.
#[test]
fn absent_metadata_is_omitted_not_nulled() {
    let a = action(0, "read_file");
    let tr = ToolResult {
        output: "hi".into(),
        metadata: None,
        is_error: false,
    };

    let ObservedOutcome::ToolCall { result, .. } = observation_from_tool_result(&a, tr);
    let obj = result.as_object().expect("result is an object");
    assert!(
        !obj.contains_key("metadata"),
        "metadata key must be absent when ToolResult.metadata is None; got {obj:?}",
    );
    // But `output` is always present.
    assert!(obj.contains_key("output"));
}

// ─── Invariant 6: action_index propagates exactly from &Action, not args ────
//
// Regression guard: the bridge reads the Action's `index()`, not any value
// stashed inside args. If someone "cleverly" pulled action_index from args,
// a plan that reused an arg key named "action_index" would be silently
// corrupted.
#[test]
fn action_index_comes_from_action_not_from_args_payload() {
    // Args contain a misleading action_index value.
    let misleading = Action::new(5, "bash", json!({"action_index": 999, "cmd": "ls"}));
    let tr = ToolResult {
        output: "listed".into(),
        metadata: None,
        is_error: false,
    };
    let ObservedOutcome::ToolCall { action_index, .. } =
        observation_from_tool_result(&misleading, tr);
    assert_eq!(
        action_index, 5,
        "action_index must reflect Action::index(), not args",
    );
}

// ─── Invariant 7: mapping is a pure function — stable under repetition ─────
//
// Same inputs twice must produce identical observations. Guards against
// hidden state / RNG / timestamping sneaking into this layer.
#[test]
fn mapping_is_pure_identical_inputs_yield_identical_outputs() {
    let a = action(0, "bash");
    let make = || ToolResult {
        output: "x".into(),
        metadata: None,
        is_error: false,
    };
    let o1 = observation_from_tool_result(&a, make());
    let o2 = observation_from_tool_result(&a, make());
    assert_eq!(o1, o2);
}
