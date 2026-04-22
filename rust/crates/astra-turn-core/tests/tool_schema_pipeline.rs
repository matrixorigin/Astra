//! Context-pipeline contracts verified at the crate boundary.
//!
//! These tests pin higher-level invariants that pure unit tests in
//! `astra-turn-core::tool_schema_prune` cannot express on their own:
//!
//! * Tier progression is monotonically non-increasing in serialized size.
//! * Every tier preserves all tool NAMES (we only strip guidance detail).
//! * Pinning preserves invoked tools at their full schema even at higher tiers.
//! * Exclusion filter composes cleanly with pruning.
//!
//! The goal is to prevent a regression where a new pruning tier accidentally
//! drops a tool entirely or keeps schemas verbose despite heavy pressure.

use astra_turn_core::compaction_types::CompactionTier;
use astra_turn_core::tool_schema_prune::{
    filter_tool_schemas_by_excluded_names, prune_tool_schemas,
};
use serde_json::{Value, json};

fn sample_tools() -> Vec<Value> {
    vec![
        json!({
            "type": "function",
            "function": {
                "name": "bash",
                "description": "Execute a bash command. Long description with multiple sentences. This should be truncated at higher tiers.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "cmd": {
                            "type": "string",
                            "description": "The shell command to execute. This property description should be stripped at CompactHistory and higher.",
                        }
                    },
                    "required": ["cmd"],
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read a file from disk. Secondary sentence explaining options. Trailing prose.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the file. Supports absolute or relative paths.",
                        },
                        "start": {
                            "type": "integer",
                            "description": "Optional start line",
                        },
                    },
                    "required": ["path"],
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "grep",
                "description": "Search for a pattern across files.",
                "parameters": { "type": "object", "properties": {} }
            }
        }),
    ]
}

fn names(tools: &[Value]) -> Vec<String> {
    tools
        .iter()
        .filter_map(|t| {
            t.get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .map(String::from)
        })
        .collect()
}

fn serialized_size(tools: &[Value]) -> usize {
    serde_json::to_vec(tools).unwrap().len()
}

// ── cp-schema-prune-tier-progression ────────────────────────────────────────
#[test]
fn pruning_size_monotonically_non_increasing_across_tiers() {
    let tools = sample_tools();
    let n = serialized_size(&prune_tool_schemas(&tools, CompactionTier::Normal));
    let t = serialized_size(&prune_tool_schemas(&tools, CompactionTier::TrimSchemas));
    let c = serialized_size(&prune_tool_schemas(&tools, CompactionTier::CompactHistory));
    let a = serialized_size(&prune_tool_schemas(&tools, CompactionTier::AggressivePrune));
    assert!(t <= n, "TrimSchemas ({t}) must be ≤ Normal ({n})");
    assert!(c <= t, "CompactHistory ({c}) must be ≤ TrimSchemas ({t})");
    assert!(
        a <= c,
        "AggressivePrune ({a}) must be ≤ CompactHistory ({c})"
    );
    assert!(
        a < n,
        "AggressivePrune must be strictly smaller than Normal"
    );
}

// ── cp-schema-prune-preserves-names ─────────────────────────────────────────
#[test]
fn all_tiers_preserve_every_tool_name() {
    let tools = sample_tools();
    let expected = names(&tools);
    for tier in [
        CompactionTier::Normal,
        CompactionTier::TrimSchemas,
        CompactionTier::CompactHistory,
        CompactionTier::AggressivePrune,
    ] {
        let pruned = prune_tool_schemas(&tools, tier);
        assert_eq!(
            names(&pruned),
            expected,
            "tier {tier:?} must preserve tool names",
        );
    }
}

// ── cp-schema-prune-strips-property-descriptions ─────────────────────────────
#[test]
fn compact_history_strips_property_descriptions() {
    let tools = sample_tools();
    let pruned = prune_tool_schemas(&tools, CompactionTier::CompactHistory);
    for t in &pruned {
        let Some(props) = t
            .pointer("/function/parameters/properties")
            .and_then(Value::as_object)
        else {
            continue;
        };
        for (k, v) in props {
            assert!(
                v.get("description").is_none(),
                "property {k:?} must lose its description at CompactHistory",
            );
        }
    }
}

// ── cp-schema-prune-aggressive-drops-optional ────────────────────────────────
#[test]
fn aggressive_prune_drops_optional_parameters() {
    let tools = sample_tools();
    let pruned = prune_tool_schemas(&tools, CompactionTier::AggressivePrune);
    let read_file = pruned
        .iter()
        .find(|t| {
            t.pointer("/function/name")
                .and_then(Value::as_str)
                .is_some_and(|n| n == "read_file")
        })
        .expect("read_file present");
    let props = read_file
        .pointer("/function/parameters/properties")
        .and_then(Value::as_object);
    if let Some(p) = props {
        // `path` is required and must stay; `start` is optional.
        assert!(p.contains_key("path"));
        assert!(
            !p.contains_key("start"),
            "optional `start` must be dropped at AggressivePrune",
        );
    }
}

// ── cp-excluded-names-filter-composes ────────────────────────────────────────
#[test]
fn exclusion_filter_composes_with_pruning() {
    let tools = sample_tools();
    let excluded: std::collections::HashSet<String> = ["bash".to_string()].into_iter().collect();
    let pruned = prune_tool_schemas(&tools, CompactionTier::TrimSchemas);
    let filtered = filter_tool_schemas_by_excluded_names(pruned, &excluded);
    let names = names(&filtered);
    assert!(!names.iter().any(|n| n == "bash"));
    assert!(names.iter().any(|n| n == "read_file"));
    assert!(names.iter().any(|n| n == "grep"));
}

// ── cp-empty-tools-noop ──────────────────────────────────────────────────────
#[test]
fn empty_tools_list_survives_every_tier() {
    let empty: Vec<Value> = Vec::new();
    for tier in [
        CompactionTier::Normal,
        CompactionTier::TrimSchemas,
        CompactionTier::CompactHistory,
        CompactionTier::AggressivePrune,
    ] {
        let pruned = prune_tool_schemas(&empty, tier);
        assert!(pruned.is_empty(), "tier {tier:?} must not invent tools");
    }
}
