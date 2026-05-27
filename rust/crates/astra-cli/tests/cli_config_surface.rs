//! Three surfaces exercised here, all driven by the same motivation:
//! operators and scripts must be able to override runtime config without
//! writing a TOML file on disk, and the `/config` slash view must reflect
//! the actual budget a turn will see.
//!
//! A. `--settings <JSON-or-path>` CLI flag:
//!    * inline JSON  →  partial overlay onto the resolved RuntimeConfig
//!    * path-to-file →  read + parse + same overlay semantics
//!    * malformed    →  surfaces a structured parse error, not a panic
//!
//! B. `/config` must show the *effective* `max_turn_input_tokens` that a
//!    given model will actually see, not only the raw config number.
//!    Matters because the budget refactor made the effective value model-
//!    dependent (Sonnet 4.6 gets 800k; 128k-window models get 102k).
//!    Users should not have to run a code audit to discover that.
//!
//! C. `/config edit` — interactive TUI edit flow. Follows Claude Code's
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
    SettingKind, apply_edit, apply_settings_json, build_settings_catalog,
    effective_budget_for_model, filter_settings, parse_settings_source,
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

    let json = r#"{"token_budget":{"max_turn_input_tokens":500000}}"#;
    let overlaid = apply_settings_json(base, json).expect("valid inline JSON");

    assert_eq!(
        overlaid.token_budget.max_turn_input_tokens, 500_000,
        "overlay must apply"
    );
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
    std::fs::write(
        tmp.path(),
        r#"{"context_window":{"adaptive_budget_reduction":true}}"#,
    )
    .unwrap();

    let raw = parse_settings_source(&tmp.path().to_string_lossy())
        .expect("path-form --settings must read the file");
    let base = RuntimeConfig::default();
    let overlaid = apply_settings_json(base, &raw).expect("file JSON must apply");
    assert!(
        overlaid.context_window.adaptive_budget_reduction,
        "file-sourced overlay must take effect"
    );
}

#[test]
fn settings_malformed_json_is_structured_error() {
    let base = RuntimeConfig::default();
    let err = apply_settings_json(base, "{not valid json").expect_err("must fail");
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
    let raw = parse_settings_source(r#"{"token_budget":{"max_turn_input_tokens":42}}"#)
        .expect("inline JSON accepted as-is");
    assert!(raw.starts_with('{'));
}

// ─── B. effective-budget display ─────────────────────────────────────────

#[test]
fn effective_budget_for_sonnet_4_6_respects_config_cap() {
    // A 1M-window provider can accept a very large prompt, but the agent's
    // per-turn working budget is intentionally capped by config by default.
    let config = RuntimeConfig::default();
    let shown = effective_budget_for_model(&config, Some("claude-sonnet-4-6"));
    assert_eq!(
        shown, config.token_budget.max_turn_input_tokens as u64,
        "Sonnet 4.6 effective budget should respect the configured cap"
    );
}

#[test]
fn effective_budget_for_unknown_model_falls_back_to_config_value() {
    let config = RuntimeConfig::default();
    let shown = effective_budget_for_model(&config, Some("no-such-model-42"));
    assert_eq!(
        shown, config.token_budget.max_turn_input_tokens as u64,
        "unknown model must show the configured fallback"
    );
}

#[test]
fn effective_budget_without_model_returns_configured_default() {
    let config = RuntimeConfig::default();
    let shown = effective_budget_for_model(&config, None);
    assert_eq!(shown, config.token_budget.max_turn_input_tokens as u64);
}

// ─── C. /config edit pure-model layer ────────────────────────────────────

#[test]
fn catalog_includes_knobs_that_motivated_this_refactor() {
    // The catalog is the source of truth for what `/config edit` can
    // reach. Anything a user might want to change to fix the
    // "conservative-stop under high pressure" symptom must be here.
    let config = RuntimeConfig::default();
    let items = build_settings_catalog(&config);
    let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();

    for required in [
        "token_budget.max_turn_input_tokens",
        "token_budget.system_prompt_reserve",
        "token_budget.tools_reserve",
        "context_window.adaptive",
        "context_window.adaptive_budget_reduction",
        "context_window.compression_threshold_min",
        "context_window.compression_threshold_max",
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
        .find(|i| i.id == "context_window.adaptive_budget_reduction")
        .expect("must be present");
    assert!(matches!(adaptive.kind, SettingKind::Bool));

    let budget = items
        .iter()
        .find(|i| i.id == "token_budget.max_turn_input_tokens")
        .expect("must be present");
    match &budget.kind {
        SettingKind::Number { min, .. } => assert!(
            *min >= 1000.0,
            "budget lower bound must not allow values so small the turn cannot run"
        ),
        other => panic!("budget knob should be Number, got {other:?}"),
    }
}

#[test]
fn fractional_threshold_knobs_accept_decimal_edits() {
    let config = RuntimeConfig::default();
    let updated = apply_edit(
        config,
        "context_window.compression_threshold_min",
        serde_json::json!(0.85),
    )
    .expect("fractional threshold edit must succeed");

    assert!((updated.context_window.compression_threshold_min - 0.85).abs() < f64::EPSILON);
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
fn apply_edit_rejects_compression_threshold_min_above_max() {
    let config = RuntimeConfig::default();
    let err = apply_edit(
        config,
        "context_window.compression_threshold_min",
        serde_json::json!(0.99),
    )
    .expect_err("min threshold must not exceed max threshold");

    assert!(
        err.to_string().to_lowercase().contains("min")
            && err.to_string().to_lowercase().contains("max"),
        "cross-field invariant should be clear: {err}"
    );
}

#[test]
fn filter_settings_matches_on_id_or_label() {
    let config = RuntimeConfig::default();
    let items = build_settings_catalog(&config);

    let hits = filter_settings(&items, "budget");
    assert!(
        hits.iter()
            .any(|i| i.id == "token_budget.max_turn_input_tokens"),
        "search for `budget` must surface the main budget knob"
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
        !config.context_window.adaptive_budget_reduction,
        "precondition: default off"
    );
    let updated = apply_edit(
        config,
        "context_window.adaptive_budget_reduction",
        serde_json::json!(true),
    )
    .expect("bool edit must succeed");
    assert!(updated.context_window.adaptive_budget_reduction);
}

#[test]
fn apply_edit_roundtrip_number_knob() {
    let config = RuntimeConfig::default();
    let updated = apply_edit(
        config,
        "token_budget.max_turn_input_tokens",
        serde_json::json!(750_000),
    )
    .expect("number edit must succeed");
    assert_eq!(updated.token_budget.max_turn_input_tokens, 750_000);
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
        "context_window.adaptive_budget_reduction",
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
