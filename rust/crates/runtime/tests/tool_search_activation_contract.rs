//! Phase-4 contract tests: `tool_search` as a first-class pinned tool,
//! and a validator primitive that admits deferred tool calls.
//!
//! Pre-phase-4 state:
//!   - `tool_search` was hidden inside the `session` meta-tool's `action`
//!     enum, so the LLM had to call `session(action="tool_search", …)`.
//!     Hoisting it to a top-level pinned tool gives the deferred
//!     activation flow an unambiguous entry point.
//!   - The headless pipeline rejected any tool call whose name wasn't in
//!     `valid_tool_names`, which is synced from the *visible* `tools[]`.
//!     That made the deferred activation flow impossible: once the LLM
//!     ran `tool_search(select:web_fetch)` and learned the schema, it
//!     still couldn't call `web_fetch` because it wasn't in `tools[]`.
//!
//! `introspect` stays in the catalog — it exposes runtime diagnostics
//! (token pressure, cache hit rate, tool health, volatile injections,
//! stall state) that `session` does not duplicate. It's a genuinely
//! separate capability, dispatched by the edge-tool executor.

use astra_tools::schemas::all_tool_schemas;
use astra_turn_core::tool_registry_meta::{TOOL_CATALOG, is_pinned_tool};
use serde_json::Value;

fn schema_names(schemas: &[Value]) -> Vec<String> {
    schemas
        .iter()
        .filter_map(|s| {
            s.get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .map(String::from)
        })
        .collect()
}

// ── 1. tool_search is a first-class, pinned tool ────────────────────────────

#[test]
fn tool_search_schema_is_emitted_as_a_top_level_tool() {
    let names = schema_names(&all_tool_schemas());
    assert!(
        names.contains(&"tool_search".to_string()),
        "tool_search must appear as its own schema (not hidden inside session): got {names:?}"
    );
}

#[test]
fn tool_search_is_in_catalog_and_pinned() {
    assert!(
        TOOL_CATALOG.iter().any(|t| t.name == "tool_search"),
        "tool_search must be present in TOOL_CATALOG"
    );
    assert!(
        is_pinned_tool("tool_search"),
        "tool_search must be pinned — it's the activation primitive for deferred tools"
    );
}

#[test]
fn tool_search_schema_advertises_select_mode() {
    let schema = all_tool_schemas()
        .into_iter()
        .find(|s| {
            s.get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                == Some("tool_search")
        })
        .expect("tool_search schema must exist");

    let desc = schema["function"]["description"]
        .as_str()
        .unwrap_or_default();
    assert!(
        desc.contains("select:"),
        "description must explain the select:NAME activation form; got: {desc}"
    );

    let query_prop = &schema["function"]["parameters"]["properties"]["query"];
    assert_eq!(
        query_prop["type"].as_str(),
        Some("string"),
        "query must be a string"
    );
}

// ── 2. introspect coexists with tool_search ─────────────────────────────────
//
// `introspect` surfaces runtime diagnostics (pressure, cache, health,
// volatile injections, stall state). `session` does not cover those, so
// `introspect` is kept as an independent capability.

#[test]
fn introspect_still_available_alongside_tool_search() {
    assert!(
        TOOL_CATALOG.iter().any(|t| t.name == "introspect"),
        "introspect must remain in TOOL_CATALOG — it exposes diagnostics session does not"
    );
    let names = schema_names(&all_tool_schemas());
    assert!(
        names.contains(&"introspect".to_string()),
        "introspect schema must still be emitted"
    );
    // Both live side-by-side.
    assert!(names.contains(&"tool_search".to_string()));
}

// ── 3. Validator admits deferred (executor-dispatchable) names ──────────────
//
// The validator logic lives in runtime/turn/headless_tool_pipeline/policy.rs
// and gates on `valid_tool_names`. We assert the helper that computes the
// "admissible set" includes names that the executor can dispatch even when
// they're not in the current tools[] slice.

#[test]
fn admissible_tool_names_includes_dispatchable_beyond_visible_tools() {
    use astra_runtime::turn::headless_tool_pipeline::admissible_tool_names;

    let visible: std::collections::HashSet<String> =
        ["bash".into(), "read_file".into()].into_iter().collect();
    let full_catalog: std::collections::HashSet<String> =
        ["bash".into(), "web_fetch".into()].into_iter().collect();

    let admitted = admissible_tool_names(&visible, &full_catalog);

    // Visible names are admitted unconditionally.
    assert!(admitted.contains("bash"));
    assert!(admitted.contains("read_file"));
    // A tool in the full catalog but NOT in visible tools[] is still
    // admitted — this is the deferred-activation flow.
    assert!(
        admitted.contains("web_fetch"),
        "deferred tools (in full catalog but not in tools[]) must be admitted: got {admitted:?}"
    );
}

#[test]
fn admissible_tool_names_rejects_truly_unknown_names() {
    use astra_runtime::turn::headless_tool_pipeline::admissible_tool_names;

    let visible: std::collections::HashSet<String> = ["bash".into()].into_iter().collect();
    let full_catalog: std::collections::HashSet<String> =
        ["bash".into(), "web_fetch".into()].into_iter().collect();
    let admitted = admissible_tool_names(&visible, &full_catalog);

    // A name in neither set stays rejected — this is the hallucination guard.
    assert!(
        !admitted.contains("completely_made_up_tool"),
        "unknown names must still be rejected"
    );
}
