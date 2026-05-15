//! Phase-1 red tests for the new `ToolSurface` API.
//!
//! This file exists to drive TDD for the tool-surfacing rewrite. Every test
//! exercises the *target* shape of the API — not the current one — so these
//! tests are expected to fail (or not compile) until the matching impl
//! lands. See `docs/plans/` and `memory/project_tool_surface_rewrite.md`.
//!
//! Contracts under test:
//!   1. Default T1 (pinned) members are a configurable set — not the
//!      whole TOOL_CATALOG.
//!   2. User config can add, remove, or replace the default pinned set.
//!   3. Every non-T1 tool appears in the deferred list as `name + short_desc`
//!      (no schema, no parameters).
//!   4. `tools[]` bytes are stable across two successive builds with the
//!      same inputs — the Anthropic prompt-cache invariant.
//!   5. Registering a plugin does not perturb `tools[]` bytes; the new
//!      plugin appears only in the deferred manifest.
//!   6. `cache_control` lands on the last pinned tool schema.

#![cfg(test)]

use crate::tool_registry::surface::{DeferredEntry, ToolSurface};
use astra_config::ToolSurfaceConfig;
use astra_turn_core::tool_registry_meta::TOOL_CATALOG;
use serde_json::{Value, json};

// ── Fixtures ────────────────────────────────────────────────────────────────

/// Build mock schemas for every tool in the catalog plus the runtime-injected
/// schemas (`skill`, `tool_search`) that production always adds before
/// `ToolSurface::build` is called. This mirrors what ToolRegistry sees in a
/// live session.
fn catalog_schemas() -> Vec<Value> {
    let mut schemas: Vec<Value> = TOOL_CATALOG
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
    for (name, desc) in [
        ("skill", "Execute a named skill (SKILL.md workflow)."),
        (
            "tool_search",
            "Search and activate deferred tools. select:NAME returns full schema.",
        ),
        (
            "task",
            "Manage session todos: create / update / list / get / stop / archive.",
        ),
    ] {
        schemas.push(json!({
            "type": "function",
            "function": {
                "name": name,
                "description": desc,
                "parameters": {"type": "object", "properties": {}}
            }
        }));
    }
    schemas
}

fn plugin_schema(name: &str, description: &str) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": {"type": "object", "properties": {}}
        }
    })
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

// ── 1. Defaults ─────────────────────────────────────────────────────────────

/// The default pinned set is the 12-member core.
/// See `DEFAULT_PINNED` comment for rationale per-entry.
#[test]
fn pinned_default_members_are_the_core_set() {
    let cfg = ToolSurfaceConfig::default();
    let surface = ToolSurface::build(catalog_schemas(), &cfg, &[]);

    let pinned_names = names(&surface.pinned_schemas());
    let expected: std::collections::HashSet<&str> = [
        "bash",
        "read_file",
        "write_file",
        "str_replace", // astra's editor tool
        "grep",
        "glob",
        "list_dir",
        "memory",     // intrinsic per TOOL_CATALOG comment
        "introspect", // runtime diagnostics
        "tool_search",
        "skill",
        "task", // session_todos surface — TUI dashboard depends on it
    ]
    .into_iter()
    .collect();

    for must_have in &expected {
        assert!(
            pinned_names.iter().any(|n| n == must_have),
            "default pinned must contain {must_have}; got {pinned_names:?}"
        );
    }
    assert_eq!(
        pinned_names.len(),
        expected.len(),
        "exactly {} default pinned, got {}: {pinned_names:?}",
        expected.len(),
        pinned_names.len()
    );
}

#[test]
fn pinned_schemas_are_sorted_alphabetically_for_cache_stability() {
    let cfg = ToolSurfaceConfig::default();
    let surface = ToolSurface::build(catalog_schemas(), &cfg, &[]);
    let names = names(&surface.pinned_schemas());
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "pinned must be sorted alphabetically");
}

// ── 2. Config overrides ─────────────────────────────────────────────────────

#[test]
fn config_pinned_tools_additive_appends_to_defaults() {
    let cfg = ToolSurfaceConfig {
        pinned_tools: vec!["github".into(), "memory".into()],
    };
    let surface = ToolSurface::build(catalog_schemas(), &cfg, &[]);

    let pinned = names(&surface.pinned_schemas());
    assert!(pinned.iter().any(|n| n == "github"));
    assert!(pinned.iter().any(|n| n == "memory"));
    // Defaults still there
    assert!(pinned.iter().any(|n| n == "bash"));
    assert!(pinned.iter().any(|n| n == "tool_search"));
}

#[test]
fn config_pinned_tools_prefix_dash_removes_default() {
    // Remove grep from the default set — user prefers `bash grep` instead.
    let cfg = ToolSurfaceConfig {
        pinned_tools: vec!["-grep".into()],
    };
    let surface = ToolSurface::build(catalog_schemas(), &cfg, &[]);

    let pinned = names(&surface.pinned_schemas());
    assert!(
        !pinned.iter().any(|n| n == "grep"),
        "-grep in config must remove grep from pinned; got {pinned:?}"
    );
    // grep now appears in deferred instead.
    assert!(
        surface.deferred().iter().any(|e| e.name == "grep"),
        "grep removed from pinned must land in deferred"
    );
}

#[test]
fn empty_and_malformed_config_entries_are_ignored_not_panic() {
    // Footguns the user might type by accident: bare "-", "--foo",
    // empty string, leading whitespace. All should be silently ignored
    // (or sanitised), not panic and not misinterpret.
    let cfg = ToolSurfaceConfig {
        pinned_tools: vec![
            "".into(),
            "-".into(),
            "--foo".into(),
            "  ".into(),
            " github".into(),
        ],
    };
    let surface = ToolSurface::build(catalog_schemas(), &cfg, &[]);
    let pinned = names(&surface.pinned_schemas());
    // `--foo` is NOT a valid "remove -foo" because -foo isn't a real tool
    // AND double-dash is not our syntax. Must be ignored, not applied.
    // ` github` (leading space) is NOT the same as `github` — must be ignored.
    assert!(!pinned.iter().any(|n| n == "foo"));
    assert!(
        !pinned.iter().any(|n| n == " github"),
        "whitespace-prefixed names must be rejected"
    );
    // Defaults survive all this malformed input.
    assert!(pinned.iter().any(|n| n == "bash"));
    assert!(pinned.iter().any(|n| n == "tool_search"));
}

#[test]
fn unknown_tool_name_in_config_is_ignored_not_panic() {
    let cfg = ToolSurfaceConfig {
        pinned_tools: vec!["not_a_real_tool".into(), "-also_not_real".into()],
    };
    // Should not panic; unknown names simply do nothing.
    let surface = ToolSurface::build(catalog_schemas(), &cfg, &[]);
    let pinned = names(&surface.pinned_schemas());
    assert!(!pinned.iter().any(|n| n == "not_a_real_tool"));
    // Defaults preserved.
    assert!(pinned.iter().any(|n| n == "bash"));
}

// ── 3. Deferred manifest ────────────────────────────────────────────────────

#[test]
fn deferred_list_contains_every_non_pinned_tool() {
    let cfg = ToolSurfaceConfig::default();
    let surface = ToolSurface::build(catalog_schemas(), &cfg, &[]);

    let pinned: std::collections::HashSet<String> =
        names(&surface.pinned_schemas()).into_iter().collect();
    let deferred: std::collections::HashSet<String> =
        surface.deferred().iter().map(|e| e.name.clone()).collect();

    // Partition: every catalog tool is in exactly one of the two.
    for tool in TOOL_CATALOG {
        let in_pinned = pinned.contains(tool.name);
        let in_deferred = deferred.contains(tool.name);
        assert!(
            in_pinned ^ in_deferred,
            "{} must be in exactly one of {{pinned, deferred}}; pinned={in_pinned} deferred={in_deferred}",
            tool.name
        );
    }
}

#[test]
fn deferred_entries_are_name_plus_short_desc_capped() {
    let cfg = ToolSurfaceConfig::default();
    let surface = ToolSurface::build(catalog_schemas(), &cfg, &[]);

    for entry in surface.deferred() {
        assert!(!entry.name.is_empty(), "deferred entry must have a name");
        assert!(
            entry.short_desc.chars().count() <= 120,
            "short_desc for {} exceeds 120 chars: {} chars",
            entry.name,
            entry.short_desc.chars().count()
        );
    }
}

#[test]
fn deferred_entries_have_no_schema_or_parameters() {
    // Structural test: DeferredEntry is name + short_desc only. Serializing
    // it must not include a `parameters` field. This guards against drift
    // where someone adds schema to the entry and bloats the prompt.
    let entry = DeferredEntry {
        name: "x".into(),
        short_desc: "y".into(),
    };
    let as_json = serde_json::to_value(&entry).expect("serializable");
    assert!(
        as_json.get("parameters").is_none(),
        "DeferredEntry must not carry parameters; got {as_json}"
    );
}

#[test]
fn deferred_list_is_sorted_alphabetically() {
    let cfg = ToolSurfaceConfig::default();
    let surface = ToolSurface::build(catalog_schemas(), &cfg, &[]);
    let names: Vec<&str> = surface.deferred().iter().map(|e| e.name.as_str()).collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(
        names, sorted,
        "deferred list must be sorted for cache stability"
    );
}

// ── 4. Byte stability (cache invariant) ─────────────────────────────────────

#[test]
fn tools_array_is_byte_stable_across_two_builds() {
    let cfg = ToolSurfaceConfig::default();
    let a = ToolSurface::build(catalog_schemas(), &cfg, &[]);
    let b = ToolSurface::build(catalog_schemas(), &cfg, &[]);
    let bytes_a = serde_json::to_vec(&a.pinned_schemas()).expect("json");
    let bytes_b = serde_json::to_vec(&b.pinned_schemas()).expect("json");
    assert_eq!(bytes_a, bytes_b, "pinned tools[] must be byte-stable");
}

#[test]
fn tools_array_byte_stable_when_plugin_registered_as_deferred() {
    let cfg = ToolSurfaceConfig::default();
    let baseline = ToolSurface::build(catalog_schemas(), &cfg, &[]);

    let plugin = vec![plugin_schema("mcp__weather", "Get weather for a city")];
    let with_plugin = ToolSurface::build(catalog_schemas(), &cfg, &plugin);

    let bytes_baseline = serde_json::to_vec(&baseline.pinned_schemas()).expect("json");
    let bytes_with = serde_json::to_vec(&with_plugin.pinned_schemas()).expect("json");
    assert_eq!(
        bytes_baseline, bytes_with,
        "registering a plugin must NOT perturb pinned tools[] bytes — plugin goes to deferred"
    );
    assert!(
        with_plugin
            .deferred()
            .iter()
            .any(|e| e.name == "mcp__weather"),
        "plugin must appear in deferred list"
    );
}

#[test]
fn plugin_is_not_auto_pinned() {
    // Corollary of the previous test: plugins default to deferred even if
    // they'd fit in the pinned budget. User must opt-in via config.
    let cfg = ToolSurfaceConfig::default();
    let plugin = vec![plugin_schema("mcp__db", "Query the internal DB")];
    let surface = ToolSurface::build(catalog_schemas(), &cfg, &plugin);
    assert!(
        !names(&surface.pinned_schemas())
            .iter()
            .any(|n| n == "mcp__db"),
        "plugin must NOT be auto-pinned"
    );
}

// ── 5. cache_control placement ──────────────────────────────────────────────

#[test]
fn cache_control_sits_on_last_pinned_tool_schema() {
    use crate::turn::prompt_cache::{PromptCacheConfig, annotate_tool_schemas_for_caching};
    let cfg = ToolSurfaceConfig::default();
    let surface = ToolSurface::build(catalog_schemas(), &cfg, &[]);
    let mut tools = surface.pinned_schemas();
    let cache_cfg = PromptCacheConfig {
        cache_enabled: true,
        is_anthropic: true,
    };
    annotate_tool_schemas_for_caching(&mut tools, &cache_cfg);

    let last = tools.last().expect("non-empty");
    assert!(
        last.get("cache_control").is_some(),
        "last pinned tool must carry cache_control; got {}",
        serde_json::to_string(last).unwrap()
    );
    // And no other tool should.
    for t in &tools[..tools.len() - 1] {
        assert!(
            t.get("cache_control").is_none(),
            "only the last pinned tool should carry cache_control"
        );
    }
}
