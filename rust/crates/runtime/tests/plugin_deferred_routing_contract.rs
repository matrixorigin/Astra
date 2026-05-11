//! Phase-5 contract: registering a plugin must not pollute `tools[]`.
//!
//! Pre-phase-5 state:
//!   - `ToolRegistry::register_plugins` pushed plugin schemas into both
//!     `all_schemas` (makes the schema lookup-able) AND `plugin_tool_names`
//!     (makes the selector's `budget_select_measured` include it in
//!     `tools[]` whenever budget permits).
//!   - Result: the moment a plugin registers, the Anthropic prompt-cache
//!     prefix breaks; every subsequent `tools[]` also includes the plugin.
//!   - Intended contract: MCP tools default to a deferred listing;
//!     user must explicitly pin them.
//!
//! Contract:
//!   1. Plugin schemas are still **looked up** by name (so the executor
//!      can dispatch, and `tool_search(select:NAME)` can return the
//!      schema).
//!   2. Plugin names are **NOT** in the selector's dynamic-candidate
//!      list — they stay out of `tools[]` unless the user pins them via
//!      `runtime.tool_surface.pinned_tools`.
//!   3. Registering a plugin leaves the pinned `tools[]` bytes stable.

use astra_config::ToolSurfaceConfig;
use astra_runtime::tool_registry::surface::ToolSurface;
use astra_runtime::tool_registry::{PluginRegistry, ToolRegistry};
use astra_turn_core::tool_registry_meta::{IntentType, Scope, TOOL_CATALOG};
use astra_turn_core::tool_registry_plugin::PluginToolEntry;
use serde_json::{Value, json};

fn catalog_schemas() -> Vec<Value> {
    TOOL_CATALOG
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
        .collect()
}

fn weather_plugin() -> PluginToolEntry {
    PluginToolEntry {
        name: "mcp__weather".into(),
        description: "Get weather for a city".into(),
        triggers: vec!["weather".into()],
        pinned: false,
        intents: vec![IntentType::CodeRead],
        scope: Scope::External,
        schema: json!({
            "type": "function",
            "function": {
                "name": "mcp__weather",
                "description": "Get weather for a city",
                "parameters": {"type": "object", "properties": {}}
            }
        }),
        schema_tokens: 20,
        source: "test".into(),
        enabled: true,
    }
}

fn make_plugins() -> PluginRegistry {
    let mut reg = PluginRegistry::new();
    reg.register(weather_plugin()).expect("register weather");
    reg
}

fn names(schemas: &[Value]) -> Vec<String> {
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

// ── 1. Plugin schemas remain lookup-able post-register ──────────────────────

#[test]
fn registered_plugin_is_lookupable_by_name() {
    let mut registry = ToolRegistry::new(catalog_schemas());
    let plugins = make_plugins();
    registry.register_plugins(&plugins);
    assert!(
        registry.schema_by_name("mcp__weather").is_some(),
        "plugin schema must be discoverable by name so tool_search(select:…) can return it"
    );
}

// ── 2. Plugin names stay out of the dynamic-candidate list ──────────────────
//
// The selector's `budget_select_measured` iterates over
// `plugin_tool_names` to decide what non-catalog schemas to stuff into
// `tools[]`. Post-phase-5 that path stays empty — plugins are deferred.

// `dynamic_candidate_names` was a zombie API that always returned `Vec::new()`;
// deleted in P2 cleanup. The real invariant — "plugin does not reach tools[]" —
// is covered by `selector_does_not_include_plugin_in_production_path` below
// and `tools_array_byte_stable_when_plugin_registers`.

/// Direct invariant against the concrete path the old selector used to
/// promote plugins into `tools[]` — `select_routed_with_pressure` must
/// not emit the plugin name in its schemas output at any budget.
#[test]
fn selector_does_not_include_plugin_in_production_path() {
    use astra_runtime::pipeline::routing::RoutingEngine;

    let mut registry = ToolRegistry::new(catalog_schemas());
    let plugins = make_plugins();
    registry.register_plugins(&plugins);

    let query = "please check the weather in Beijing and tell me";
    let routing = RoutingEngine::analyze(query, 1, &[], &[], vec![]);
    let (schemas, _report) = registry.select_routed_with_pressure(
        query,
        &routing,
        4000, // plenty of budget
        &[],
        None,
        None,
        &[],
        0.0,
        &std::collections::HashMap::new(),
        &[],
        &std::collections::HashMap::new(),
    );
    let names = names(&schemas);
    assert!(
        !names.contains(&"mcp__weather".to_string()),
        "production selector must NOT include plugins in tools[] at any budget; got {names:?}"
    );
}

// ── 3. Register doesn't perturb the pinned tools[] bytes ────────────────────

#[test]
fn tools_array_byte_stable_when_plugin_registers() {
    let cfg = ToolSurfaceConfig::default();

    // tools[] from an unplugged registry
    let pristine_surface = ToolSurface::build(catalog_schemas(), &cfg, &[]);
    let baseline = serde_json::to_vec(&pristine_surface.pinned_schemas()).unwrap();

    // Now simulate registering a plugin and re-building the surface
    let mut registry = ToolRegistry::new(catalog_schemas());
    let plugins = make_plugins();
    registry.register_plugins(&plugins);

    // Gather schemas that WOULD be sent as tools[] — pinned only.
    // `ToolSurface::build` is the new source of truth; plugin schemas are
    // passed in separately. The bytes must match the pristine baseline.
    let plugin_schemas: Vec<Value> = plugins.schemas();
    let surface_after = ToolSurface::build(catalog_schemas(), &cfg, &plugin_schemas);
    let after = serde_json::to_vec(&surface_after.pinned_schemas()).unwrap();

    assert_eq!(
        baseline, after,
        "registering a plugin must NOT change the pinned tools[] bytes"
    );

    // And the plugin shows up only in deferred.
    let deferred_names: Vec<&str> = surface_after
        .deferred()
        .iter()
        .map(|e| e.name.as_str())
        .collect();
    assert!(
        deferred_names.contains(&"mcp__weather"),
        "plugin must land in the deferred list; got {deferred_names:?}"
    );
    assert!(
        !names(&surface_after.pinned_schemas())
            .iter()
            .any(|n| n == "mcp__weather"),
        "plugin must NOT appear in pinned"
    );
}

// ── 4. User can still opt-in via config ─────────────────────────────────────

#[test]
fn user_can_pin_a_plugin_via_config() {
    let cfg = ToolSurfaceConfig {
        pinned_tools: vec!["mcp__weather".into()],
    };

    let plugin_schemas = vec![weather_plugin().schema.clone()];
    let surface = ToolSurface::build(catalog_schemas(), &cfg, &plugin_schemas);

    assert!(
        names(&surface.pinned_schemas())
            .iter()
            .any(|n| n == "mcp__weather"),
        "user config must be able to pin a plugin into tools[]"
    );
    assert!(
        !surface.deferred().iter().any(|e| e.name == "mcp__weather"),
        "pinned plugin must NOT also show up in deferred"
    );
}
