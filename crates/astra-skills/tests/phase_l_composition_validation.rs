//! Phase L (JSON Schema half) — validate_input / validate_output guardrails
//! for composition.rs. Complements `phase_l_xml_fallback_guardrails.rs` in
//! `astra-turn-core`.

use astra_skills::composition::{validate_input, validate_output};
use serde_json::json;

#[test]
fn phase_l_null_value_for_required_field_is_error() {
    let schema = json!({
        "properties": { "path": { "type": "string" } },
        "required": ["path"],
    });
    let args = json!({ "path": null });
    let errs = validate_input(&schema, &args);
    assert!(
        errs.iter().any(|e| e.contains("missing required field")),
        "null must count as missing for a required field, got {errs:?}"
    );
}

#[test]
fn phase_l_empty_string_for_required_field_is_accepted_by_contract() {
    let schema = json!({
        "properties": { "path": { "type": "string" } },
        "required": ["path"],
    });
    let args = json!({ "path": "" });
    assert!(validate_input(&schema, &args).is_empty());
}

#[test]
fn phase_l_array_type_mismatch_detected() {
    let schema = json!({
        "properties": { "paths": { "type": "array" } },
        "required": ["paths"],
    });
    let args = json!({ "paths": "not-an-array" });
    let errs = validate_input(&schema, &args);
    assert!(
        errs.iter().any(|e| e.contains("expected type 'array'")),
        "array mismatch must be flagged, got {errs:?}"
    );
}

#[test]
fn phase_l_object_type_mismatch_detected() {
    let schema = json!({
        "properties": { "config": { "type": "object" } },
        "required": [],
    });
    let args = json!({ "config": [1, 2, 3] });
    let errs = validate_input(&schema, &args);
    assert!(errs.iter().any(|e| e.contains("expected type 'object'")));
}

#[test]
fn phase_l_integer_vs_number_distinction() {
    let schema = json!({
        "properties": { "count": { "type": "integer" } },
    });
    let errs = validate_input(&schema, &json!({ "count": 3.15 }));
    assert!(
        errs.iter().any(|e| e.contains("'count'")),
        "3.15 must not satisfy integer, got {errs:?}"
    );
    let schema_num = json!({
        "properties": { "count": { "type": "number" } },
    });
    assert!(validate_input(&schema_num, &json!({ "count": 3.15 })).is_empty());
    assert!(validate_input(&schema_num, &json!({ "count": 42 })).is_empty());
}

#[test]
fn phase_l_enum_with_integer_values_validated() {
    let schema = json!({
        "properties": { "level": { "type": "integer", "enum": [1, 2, 3] } },
    });
    let errs = validate_input(&schema, &json!({ "level": 5 }));
    assert!(errs.iter().any(|e| e.contains("not in allowed set")));
    assert!(validate_input(&schema, &json!({ "level": 2 })).is_empty());
}

#[test]
fn phase_l_unknown_type_name_passes_through() {
    let schema = json!({
        "properties": { "x": { "type": "anyOfCustom" } },
    });
    assert!(validate_input(&schema, &json!({ "x": 42 })).is_empty());
}

#[test]
fn phase_l_validate_output_non_json_returns_single_warning() {
    let schema = json!({
        "properties": { "result": { "type": "string" } },
        "required": ["result"],
    });
    let warnings = validate_output(&schema, "not valid json at all");
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("not valid JSON"));
}
