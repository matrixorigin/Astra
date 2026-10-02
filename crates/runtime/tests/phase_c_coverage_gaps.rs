//! Phase C — Track #2 online coverage gap-fills.
//!
//! These tests target genuine gaps surfaced by the systematic coverage audit.
//! They exercise public APIs end-to-end rather than re-covering ground already
//! handled by the focused unit tests in each module.
//!
//! Scope in this file:
//!
//! * **Skill composition validation** — input-schema enum rejection, output
//!   parse-and-validate, depth limit propagation, and child timeout inheritance
//!   from the parent budget.
//!
//! Any scenario already covered by a focused test in the source crate is
//! intentionally skipped; each test below was placed because the audit
//! identified a real behavior that no existing test asserts.

use astra_skills::composition::{
    CompositionContext, CompositionError, MAX_COMPOSITION_DEPTH, validate_input, validate_output,
};
use serde_json::json;

// ── Skill composition: schema validation ────────────────────────────────────

#[test]
fn input_schema_rejects_value_outside_enum() {
    let schema = json!({
        "type": "object",
        "required": ["mode"],
        "properties": {
            "mode": { "type": "string", "enum": ["fast", "slow"] }
        }
    });
    let args = json!({ "mode": "medium" });

    let errors = validate_input(&schema, &args);

    assert!(
        !errors.is_empty(),
        "enum-violating input must be rejected, got {errors:?}"
    );
    assert!(
        errors.iter().any(|e| e.contains("not in allowed set")),
        "enum rejection should mention the allowed set, got {errors:?}"
    );
}

#[test]
fn input_schema_rejects_wrong_type_on_declared_field() {
    let schema = json!({
        "type": "object",
        "properties": {
            "count": { "type": "integer" },
            "label": { "type": "string" }
        },
        "required": ["count", "label"]
    });
    // `count` is a string, `label` is a number — both must be flagged.
    let args = json!({ "count": "seven", "label": 42 });

    let errors = validate_input(&schema, &args);

    assert!(
        errors.iter().any(|e| e.contains("'count'")),
        "type mismatch on count must be flagged, got {errors:?}"
    );
    assert!(
        errors.iter().any(|e| e.contains("'label'")),
        "type mismatch on label must be flagged, got {errors:?}"
    );
}

#[test]
fn output_schema_flags_nonjson_as_invalid() {
    let schema = json!({ "type": "object", "properties": {} });
    let errors = validate_output(&schema, "hello, definitely not json");
    assert_eq!(errors.len(), 1, "non-JSON output yields one warning");
    assert!(
        errors[0].contains("not valid JSON"),
        "expected a 'not valid JSON' warning, got {errors:?}"
    );
}

#[test]
fn output_schema_accepts_valid_json_shape() {
    let schema = json!({
        "type": "object",
        "required": ["result"],
        "properties": { "result": { "type": "string" } }
    });
    let errors = validate_output(&schema, r#"{"result":"ok"}"#);
    assert!(
        errors.is_empty(),
        "well-formed output must pass, got {errors:?}"
    );
}

#[test]
fn output_schema_flags_json_missing_required_field() {
    let schema = json!({
        "type": "object",
        "required": ["result"],
        "properties": { "result": { "type": "string" } }
    });
    // Valid JSON, wrong shape (LLM-generated "close but wrong").
    let errors = validate_output(&schema, r#"{"data": 123}"#);
    assert!(
        errors.iter().any(|e| e.contains("result")),
        "missing required field must be surfaced, got {errors:?}"
    );
}

// ── Skill composition: depth + timeout propagation ──────────────────────────

#[test]
fn depth_limit_blocks_further_nesting_at_max() {
    // Drive a chain root → d1 → d2 → d3. d3 is beyond MAX_COMPOSITION_DEPTH.
    let root = CompositionContext::root();
    assert_eq!(root.depth, 0);
    root.check_depth().expect("root is always under max");

    let d1 = root.child("parent", None);
    d1.check_depth().expect("depth 1 allowed");

    let d2 = d1.child("grandparent", None);
    d2.check_depth().expect("depth 2 allowed");

    let d3 = d2.child("great-grandparent", None);
    match d3.check_depth() {
        Err(CompositionError::MaxDepthExceeded { depth, max }) => {
            assert_eq!(depth, MAX_COMPOSITION_DEPTH);
            assert_eq!(max, MAX_COMPOSITION_DEPTH);
        }
        other => panic!("expected MaxDepthExceeded at depth 3, got {other:?}"),
    }
}

#[test]
fn child_timeout_clamps_to_parent_remaining_budget() {
    // Parent has a 5s budget. Child asks for 60s — effective must be ≤ 5.
    let mut root = CompositionContext::root();
    root.timeout_secs = Some(5);

    let child = root.child("parent-skill", Some(60));

    assert_eq!(
        child.timeout_secs,
        Some(5),
        "child timeout must be clamped to parent remaining budget"
    );
}

#[test]
fn child_timeout_adopts_declared_limit_when_parent_has_none() {
    let root = CompositionContext::root(); // timeout_secs: None
    let child = root.child("parent-skill", Some(7));

    assert_eq!(
        child.timeout_secs,
        Some(7),
        "child adopts declared limit when parent is unbounded"
    );
}

#[test]
fn composition_side_effects_accumulate_in_child() {
    let mut root = CompositionContext::root();
    root.record_side_effects(&["wrote_file".into()]);

    let mut child = root.child("parent", None);
    assert_eq!(
        child.side_effects,
        vec!["wrote_file".to_string()],
        "child inherits parent side effects"
    );

    // New side effect from child doesn't duplicate the parent's.
    child.record_side_effects(&["wrote_file".into(), "ran_bash".into()]);
    assert_eq!(
        child.side_effects,
        vec!["wrote_file".to_string(), "ran_bash".to_string()]
    );
}
