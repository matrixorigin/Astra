//! Phase-6 contract: the production `resolve_schemas_with_pressure` path
//! returns byte-stable `tools[]` regardless of what the selector wanted.
//!
//! This is the end-to-end invariant the whole rewrite aimed at:
//!   - Turn 1 selector picks `[github, web_fetch]` → tools[] = T1 defaults.
//!   - Turn 2 selector picks `[lsp, task]` → tools[] = same bytes.
//!   - Anthropic/Bedrock prompt cache hits the whole tools[] + system prefix.
//!
//! Before this phase, `tools[]` changed whenever the selector's ranking
//! changed — the cache marker was "stable" but everything after it was
//! resent every turn.

use astra_runtime::tool_registry::ToolRegistry;
use astra_runtime::tool_selector::{resolve_schemas, resolve_schemas_with_pressure};
use astra_turn_core::tool_registry_meta::TOOL_CATALOG;
use serde_json::{Value, json};

fn catalog_schemas() -> Vec<Value> {
    let mut out: Vec<Value> = TOOL_CATALOG
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": {"type": "object", "properties": {}}
                }
            })
        })
        .collect();
    // Runtime-injected companions (skill is injected at turn start in prod).
    // tool_search is already in TOOL_CATALOG post-Phase-4; skill is not.
    out.push(json!({
        "type": "function",
        "function": {
            "name": "skill",
            "description": "Execute a named skill (SKILL.md workflow).",
            "parameters": {"type": "object", "properties": {}}
        }
    }));
    out
}

// ── 1. Byte-stable across varying selector output ──────────────────────────

#[test]
fn tools_array_bytes_identical_for_any_selector_output() {
    let registry = ToolRegistry::new(catalog_schemas());

    let selector_turn1 = vec!["github".into(), "web_fetch".into()];
    let selector_turn2 = vec!["lsp".into(), "task".into()];
    let selector_turn3: Vec<String> = vec![];
    let selector_turn4 = vec!["agent".into(), "mo".into(), "session".into()];

    let (schemas_1, _) = resolve_schemas(&registry, &selector_turn1);
    let (schemas_2, _) = resolve_schemas(&registry, &selector_turn2);
    let (schemas_3, _) = resolve_schemas(&registry, &selector_turn3);
    let (schemas_4, _) = resolve_schemas(&registry, &selector_turn4);

    let bytes_1 = serde_json::to_vec(&schemas_1).unwrap();
    let bytes_2 = serde_json::to_vec(&schemas_2).unwrap();
    let bytes_3 = serde_json::to_vec(&schemas_3).unwrap();
    let bytes_4 = serde_json::to_vec(&schemas_4).unwrap();

    assert_eq!(bytes_1, bytes_2);
    assert_eq!(bytes_1, bytes_3);
    assert_eq!(bytes_1, bytes_4);
}

// ── 2. Byte-stable across pressure levels (schemas do prune, but the
//    set of tools[] stays the same — same N schemas at the same names,
//    just with trimmed descriptions). Names must match. ──────────────────

#[test]
fn tools_array_names_identical_across_pressure_levels() {
    let registry = ToolRegistry::new(catalog_schemas());

    let (s_none, _) = resolve_schemas_with_pressure(&registry, &[], 0.0);
    let (s_light, _) = resolve_schemas_with_pressure(&registry, &[], 0.3);
    let (s_med, _) = resolve_schemas_with_pressure(&registry, &[], 0.6);

    fn names(v: &[Value]) -> Vec<String> {
        v.iter()
            .filter_map(|s| {
                s.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .map(String::from)
            })
            .collect()
    }
    assert_eq!(names(&s_none), names(&s_light));
    assert_eq!(names(&s_none), names(&s_med));
}

// ── 3. The default set lines up with T1 — ensures the pinned
//    contract stays intact end-to-end. ──────────────────────────────────

#[test]
fn production_tools_array_matches_default_pinned_set() {
    use astra_runtime::tool_registry::surface::DEFAULT_PINNED;
    let registry = ToolRegistry::new(catalog_schemas());
    let (schemas, _) = resolve_schemas(&registry, &[]);

    let got: std::collections::HashSet<String> = schemas
        .iter()
        .filter_map(|s| {
            s.get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .map(String::from)
        })
        .collect();
    let expected: std::collections::HashSet<String> =
        DEFAULT_PINNED.iter().map(|s| (*s).to_string()).collect();
    assert_eq!(
        got, expected,
        "production tools[] must equal DEFAULT_PINNED\n got: {got:?}\n want: {expected:?}"
    );
}
