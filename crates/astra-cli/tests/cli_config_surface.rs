//! Two surfaces exercised here, all driven by the same motivation:
//! operators and scripts must be able to override runtime config without
//! writing a TOML file on disk.
//!
//! A. `--settings <JSON-or-path>` CLI flag:
//!    * inline JSON  →  partial overlay onto the resolved RuntimeConfig
//!    * path-to-file →  read + parse + same overlay semantics
//!    * malformed    →  surfaces a structured parse error, not a panic
//!
//! B. `/config edit` — interactive TUI edit flow. Follows the reference agent's
//!    Config.tsx model: flat list of { id, label, type, value, onChange }
//!    items, filtered by a search query, dispatched to per-type editors
//!    (bool toggle / enum select / number input). Per-source snapshot
//!    enables a clean revert on cancel.
//!
//!    The full TUI is hard to drive from a unit test, so the contract
//!    tested here is the **pure model layer**:
//!      - build_settings_catalog(config) → Vec<SettingItem>
//!      - filter_settings(catalog, query) → Vec<SettingItem>
//!      - apply_edit(config, id, new_value) → Result<RuntimeConfig>
//!    The rendering / keystroke handling is thin glue over these.

use astra_config::config_overlay::{
    RuntimeConfigLayer, SettingKind, apply_edit, build_settings_catalog, filter_settings,
    parse_settings_source,
};
use astra_config::runtime_config::RuntimeConfig;

// ─── A. --settings flag ──────────────────────────────────────────────────

#[test]
fn settings_inline_json_partial_overlay() {
    // The JSON overlay is intentionally partial — it mentions only the
    // one knob the operator cares about. Everything else must keep its
    // resolved-from-disk value.
    let base = RuntimeConfig::default();
    let original_compression_threshold = base.compression.compression_threshold;

    let json = r#"{"memory":{"retrieval_top_k":12}}"#;
    let overlaid = RuntimeConfigLayer::from_json(json)
        .unwrap()
        .apply_to(&base)
        .expect("valid inline JSON");

    assert_eq!(overlaid.memory.retrieval_top_k, 12, "overlay must apply");
    assert_eq!(
        overlaid.compression.compression_threshold, original_compression_threshold,
        "untouched fields must retain their pre-overlay value"
    );
}

#[test]
fn settings_file_path_reads_and_applies() {
    // Write a tiny JSON file to a temp path; parse_settings_source must
    // recognise it as a file (not as a JSON literal) and return the
    // parsed content.
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), r#"{"memory":{"retrieval_top_k":9}}"#).unwrap();

    let raw = parse_settings_source(&tmp.path().to_string_lossy())
        .expect("path-form --settings must read the file");
    let base = RuntimeConfig::default();
    let overlaid = RuntimeConfigLayer::from_json(&raw)
        .unwrap()
        .apply_to(&base)
        .expect("file JSON must apply");
    assert_eq!(overlaid.memory.retrieval_top_k, 9);
}

#[test]
fn settings_malformed_json_is_structured_error() {
    let err = RuntimeConfigLayer::from_json("{not valid json").expect_err("must fail");
    let msg = err.to_string();
    assert!(
        msg.contains("JSON") || msg.contains("parse") || msg.contains("expected"),
        "error must be diagnostic, not opaque: {msg}"
    );
}

#[test]
fn parse_settings_source_treats_leading_brace_as_inline() {
    // An operator passing `--settings '{"k":1}'` must NOT have the string
    // misinterpreted as a file path. Heuristic: leading `{` = inline.
    let raw = parse_settings_source(r#"{"memory":{"retrieval_top_k":42}}"#)
        .expect("inline JSON accepted as-is");
    assert!(raw.starts_with('{'));
}

// ─── B. /config edit pure-model layer ────────────────────────────────────

#[test]
fn catalog_includes_knobs_that_motivated_this_refactor() {
    // The catalog is the source of truth for what `/config edit` can
    // reach. Anything a user might want to change to fix the
    // "conservative-stop under high pressure" symptom must be here.
    let config = RuntimeConfig::default();
    let items = build_settings_catalog(&config);
    let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();

    assert!(
        !ids.iter().any(|id| id.starts_with("runtime_limits.")),
        "local editing must not promise control of Server execution rounds"
    );

    for required in [
        "trace.llm_exchanges",
        "compression.compression_threshold",
        "compression.preserve_recent_turns",
        "memory.retrieval_top_k",
    ] {
        assert!(
            ids.contains(&required),
            "catalog must expose `{required}`, found: {ids:?}"
        );
    }
}

#[test]
fn catalog_items_carry_kind_matching_their_concrete_type() {
    // A bool knob exposes SettingKind::Bool, a number knob exposes
    // SettingKind::Number with a sensible range. The edit UI dispatches
    // on kind, so a wrong kind would mean the wrong editor fires.
    let config = RuntimeConfig::default();
    let items = build_settings_catalog(&config);

    let adaptive = items
        .iter()
        .find(|i| i.id == "trace.llm_exchanges")
        .expect("must be present");
    assert!(matches!(adaptive.kind, SettingKind::Bool));

    let budget = items
        .iter()
        .find(|i| i.id == "memory.retrieval_top_k")
        .expect("must be present");
    match &budget.kind {
        SettingKind::Number { min, .. } => {
            assert!(*min >= 1.0, "retrieval must select at least one result")
        }
        other => panic!("budget knob should be Number, got {other:?}"),
    }
}

#[test]
fn fractional_threshold_knobs_accept_decimal_edits() {
    let config = RuntimeConfig::default();
    let updated = apply_edit(
        config,
        "compression.compression_threshold",
        serde_json::json!(0.85),
    )
    .expect("fractional threshold edit must succeed");

    assert!((updated.compression.compression_threshold - 0.85).abs() < f64::EPSILON);
}

#[test]
fn apply_edit_rejects_fractional_threshold_outside_range() {
    let config = RuntimeConfig::default();
    let err = apply_edit(
        config,
        "compression.compression_threshold",
        serde_json::json!(1.25),
    )
    .expect_err("thresholds are fractions and must stay within [0.0, 1.0]");

    assert!(
        err.to_string().to_lowercase().contains("range"),
        "range violation should be explicit: {err}"
    );
}

#[test]
fn filter_settings_matches_on_id_or_label() {
    let config = RuntimeConfig::default();
    let items = build_settings_catalog(&config);

    let hits = filter_settings(&items, "memory");
    assert!(
        hits.iter().any(|i| i.id == "memory.retrieval_top_k"),
        "search for `memory` must surface the memory budget knob"
    );
    let none = filter_settings(&items, "surely-not-in-any-key-or-label-at-all");
    assert!(
        none.is_empty(),
        "no matches must return empty, got {none:?}"
    );
}

#[test]
fn filter_settings_empty_query_returns_all() {
    let config = RuntimeConfig::default();
    let items = build_settings_catalog(&config);
    let hits = filter_settings(&items, "");
    assert_eq!(hits.len(), items.len());
}

#[test]
fn apply_edit_roundtrip_bool_knob() {
    let config = RuntimeConfig::default();
    assert!(
        !config
            .trace
            .category_enabled(astra_config::runtime_config::TraceCategory::LlmExchanges),
        "precondition: default off"
    );
    let updated = apply_edit(config, "trace.llm_exchanges", serde_json::json!(true))
        .expect("bool edit must succeed");
    assert!(
        updated
            .trace
            .category_enabled(astra_config::runtime_config::TraceCategory::LlmExchanges)
    );
    assert_eq!(
        updated.trace.profile,
        astra_config::runtime_config::TraceProfile::Custom
    );
}

#[test]
fn apply_edit_roundtrip_number_knob() {
    let config = RuntimeConfig::default();
    let updated = apply_edit(config, "memory.retrieval_top_k", serde_json::json!(12))
        .expect("number edit must succeed");
    assert_eq!(updated.memory.retrieval_top_k, 12);
}

#[test]
fn apply_edit_rejects_unknown_path() {
    let config = RuntimeConfig::default();
    let err = apply_edit(config, "nope.does.not.exist", serde_json::json!(1)).unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("unknown"),
        "error for unknown path must mention that: {err}"
    );
}

#[test]
fn apply_edit_rejects_type_mismatch() {
    let config = RuntimeConfig::default();
    let err = apply_edit(
        config,
        "trace.llm_exchanges",
        serde_json::json!("not a bool"),
    )
    .unwrap_err();
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("bool") || msg.contains("type") || msg.contains("invalid"),
        "type mismatch must surface a diagnostic: {err}"
    );
}

// ─── Catalog ↔ apply_edit closure property ──────────────────────────────

#[test]
fn every_catalog_item_is_editable_via_apply_edit() {
    // Regression guard: if someone adds a knob to the catalog and forgets
    // the apply_edit branch, it silently becomes read-only. Close the loop
    // by exercising every listed item's current value through apply_edit.
    let config = RuntimeConfig::default();
    let items = build_settings_catalog(&config);
    for item in &items {
        let value_json = match &item.kind {
            SettingKind::Bool => serde_json::json!(item.value_as_bool().unwrap_or(false)),
            SettingKind::Number { .. } => {
                serde_json::json!(item.value_as_number().unwrap_or(0.0))
            }
            SettingKind::Enum { options } => {
                // Pick the current value or the first option — either must round-trip.
                let picked = item
                    .value_as_string()
                    .or_else(|| options.first().cloned())
                    .unwrap_or_default();
                serde_json::json!(picked)
            }
        };
        apply_edit(config.clone(), &item.id, value_json.clone()).unwrap_or_else(|e| {
            panic!(
                "catalog item {id:?} (value={value_json}) must be editable via apply_edit: {e}",
                id = item.id
            )
        });
    }
}

#[test]
fn retired_controls_are_absent_and_rejected_at_configuration_entrypoints() {
    let config = RuntimeConfig::default();
    let serialized = serde_json::to_value(&config).unwrap();
    let catalog = build_settings_catalog(&config);
    for section in [
        "verification",
        "memory_pressure",
        "context_window",
        "token_budget",
        "tool_selection",
    ] {
        assert!(serialized.get(section).is_none());
        assert!(
            !catalog
                .iter()
                .any(|item| item.id.starts_with(&format!("{section}.")))
        );
        let overlay = serde_json::json!({section: {}}).to_string();
        assert!(
            RuntimeConfigLayer::from_json(&overlay)
                .unwrap_err()
                .to_string()
                .contains("unknown field")
        );
    }
    for path in [
        "verification.strictness",
        "memory_pressure.adaptive",
        "context_window.compression_threshold_min",
        "token_budget.max_turn_input_tokens",
        "token_budget.tools_reserve",
        "tool_selection.max_tools_per_turn",
    ] {
        assert!(apply_edit(config.clone(), path, serde_json::json!(0.8)).is_err());
        let mut candidate = config.clone();
        let before = serde_json::to_value(&candidate).unwrap();
        let error = astra_config::apply_governed_config_mutation(
            &mut candidate,
            path,
            &serde_json::json!(0.8),
            true,
            0.3,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            astra_config::GovernedConfigMutationError::UnsupportedPath { .. }
        ));
        assert_eq!(serde_json::to_value(&candidate).unwrap(), before);
    }
}
