//! Tool-surface contract tests: `tool_search` is the first-class always_load
//! activation primitive, while selected deferred tools are queued for the next
//! request's `tools[]` instead of becoming long-lived validator state.
//!
//! Pre-phase-4 state:
//!   - `tool_search` was hidden inside the `session` meta-tool's `action`
//!     enum, so the LLM had to call `session(action="tool_search", …)`.
//!     Hoisting it to a top-level always_load tool gives the deferred
//!     activation flow an unambiguous entry point.
//!   - Some paths treated an activated deferred name as an execution allowlist.
//!     The fixed contract makes activation pending schema-injection state
//!     retained until the selected tool is actually called or becomes stale;
//!     execution still depends on the current request's visible schema set or
//!     an explicit transport/plugin grant.
//!
//! `introspect` stays in the catalog — it exposes runtime diagnostics
//! (token pressure, cache hit rate, tool health, volatile injections,
//! stall state) that `session` does not duplicate. It is a genuinely
//! separate always-load capability, dispatched by the edge-tool executor.

use astra_tools::schemas::all_tool_schemas;
use astra_turn_core::tool_registry_meta::TOOL_CATALOG;
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

// ── 1. tool_search is a first-class, always_load tool ────────────────────────────

#[test]
fn tool_search_schema_is_emitted_as_a_top_level_tool() {
    let names = schema_names(&all_tool_schemas());
    assert!(
        names.contains(&"tool_search".to_string()),
        "tool_search must appear as its own schema (not hidden inside session): got {names:?}"
    );
}

#[test]
fn tool_search_is_in_catalog_and_always_load() {
    assert!(
        TOOL_CATALOG.iter().any(|t| t.name == "tool_search"),
        "tool_search must be present in TOOL_CATALOG"
    );
    assert!(
        astra_runtime::tool_registry::surface::default_always_load_names()
            .iter()
            .any(|name| name == "tool_search"),
        "tool_search must be always_load — it's the activation primitive for deferred tools"
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
// `introspect` and `reflect` are recovery/debug entrypoints. They should not
// require the model to discover that self-observation exists before it can use
// it to recover from drift, runtime errors, or confusing state.

#[test]
fn observation_tools_are_available_and_always_load_by_default() {
    let introspect = TOOL_CATALOG
        .iter()
        .find(|t| t.name == "introspect")
        .expect("introspect must remain in TOOL_CATALOG");
    let reflect = TOOL_CATALOG
        .iter()
        .find(|t| t.name == "reflect")
        .expect("reflect must remain in TOOL_CATALOG");
    assert_eq!(introspect.name, "introspect");
    assert_eq!(reflect.name, "reflect");

    let always_load = astra_runtime::tool_registry::surface::default_always_load_names();
    assert!(
        always_load.iter().any(|name| name == "introspect")
            && always_load.iter().any(|name| name == "reflect"),
        "observation tools must be in every local always_load tool prefix"
    );

    let names = schema_names(&all_tool_schemas());
    assert!(
        names.contains(&"introspect".to_string()),
        "introspect schema must still be emitted"
    );
    assert!(
        names.contains(&"reflect".to_string()),
        "reflect schema must still be emitted"
    );
    // They live side-by-side with tool_search; tool_search remains the
    // activation primitive for the rest of the deferred catalog.
    assert!(names.contains(&"tool_search".to_string()));
}

// ── 3. Validator extras are explicit grants, not deferred state ─────────────
//
// The validator logic lives in runtime/turn/headless_tool_pipeline/policy.rs
// and gates on `valid_tool_names`. The helper may include caller-supplied
// extras for runtime/plugin transports, but deferred-tool activation should be
// consumed earlier by surface assembly so the selected schema becomes visible.

#[test]
fn admissible_tool_names_includes_explicit_runtime_extras_beyond_visible_tools() {
    use astra_runtime::turn::headless_tool_pipeline::admissible_tool_names;

    let visible: std::collections::HashSet<String> =
        ["bash".into(), "read_file".into()].into_iter().collect();
    let extras: std::collections::HashSet<String> = ["mcp__weather".into()].into_iter().collect();

    let admitted = admissible_tool_names(&visible, &extras);

    // Visible names are admitted unconditionally.
    assert!(admitted.contains("bash"));
    assert!(admitted.contains("read_file"));
    // Caller-supplied extras are admitted only when a concrete transport path
    // has granted them.
    assert!(
        admitted.contains("mcp__weather"),
        "explicit runtime extras must be admitted: got {admitted:?}"
    );
}

#[test]
fn admissible_tool_names_does_not_admit_catalog_by_default() {
    use astra_runtime::turn::headless_tool_pipeline::admissible_tool_names;

    let visible: std::collections::HashSet<String> = ["bash".into()].into_iter().collect();
    let activated: std::collections::HashSet<String> = std::collections::HashSet::new();
    let admitted = admissible_tool_names(&visible, &activated);

    assert!(!admitted.contains("web_fetch"));
    assert!(
        !admitted.contains("completely_made_up_tool"),
        "unknown names must still be rejected"
    );
}
