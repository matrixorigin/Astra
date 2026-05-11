//! Phase-2 contract tests for the `<deferred_tools>` system-prompt section.
//!
//! This block is what lets the LLM know every tool that exists outside the
//! pinned `tools[]` array — without paying their full schema cost per turn.
//!
//! Contracts:
//!   1. The section is emitted with `CacheScope::Session` (not `None`) so
//!      it joins the cached session prefix instead of invalidating it every
//!      turn.
//!   2. The rendered text is byte-stable for equal inputs across builds —
//!      no timestamps, no HashMap-order drift.
//!   3. The block is suppressed when there are no deferred entries (no
//!      point emitting an empty `<deferred_tools>` tag).
//!   4. Every `DeferredEntry` appears as a `<tool>` sub-element with
//!      `<name>` and `<description>` — mirrors the `<available_skills>`
//!      shape already used for skills so the LLM sees a consistent
//!      system-reminder style.
//!   5. Pinned tools never appear in the block — they're already in
//!      `tools[]`.
//!   6. A consistent nudge line ("If a tool in <deferred_tools> fits…")
//!      appears so weak models know to call `tool_search(select:…)`.
//!
//! All tests reference the *target* API (`build_deferred_tools_section`)
//! which doesn't exist yet — they must fail to compile, then fail at
//! runtime, then pass.

use astra_config::ToolSurfaceConfig;
use astra_runtime::prompts::build_deferred_tools_section;
use astra_runtime::tool_registry::surface::ToolSurface;
use astra_turn_core::section_types::CacheScope;
use serde_json::{Value, json};

fn catalog_schemas() -> Vec<Value> {
    use astra_turn_core::tool_registry_meta::TOOL_CATALOG;
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

fn default_surface() -> ToolSurface {
    ToolSurface::build(catalog_schemas(), &ToolSurfaceConfig::default(), &[])
}

// ── 1. Cache scope ──────────────────────────────────────────────────────────

#[test]
fn deferred_block_is_session_scope_not_none() {
    let section = build_deferred_tools_section(&default_surface())
        .expect("non-empty deferred list should produce a section");
    assert_eq!(
        section.scope,
        CacheScope::Session,
        "deferred tools block must be CacheScope::Session so it joins the cached prefix"
    );
}

// ── 2. Byte stability ───────────────────────────────────────────────────────

#[test]
fn deferred_block_content_stable_across_two_builds() {
    let a = build_deferred_tools_section(&default_surface()).unwrap();
    let b = build_deferred_tools_section(&default_surface()).unwrap();
    assert_eq!(a.text, b.text, "deferred block must be byte-stable");
}

#[test]
fn deferred_block_stable_when_unrelated_plugin_order_varies() {
    // Build two surfaces whose plugin schemas are the same set but added
    // in different order. The resulting deferred block must be identical
    // because the underlying ToolSurface sorts alphabetically.
    let p1 = json!({
        "type": "function",
        "function": {
            "name": "mcp__alpha",
            "description": "alpha tool",
            "parameters": {"type": "object", "properties": {}}
        }
    });
    let p2 = json!({
        "type": "function",
        "function": {
            "name": "mcp__beta",
            "description": "beta tool",
            "parameters": {"type": "object", "properties": {}}
        }
    });

    let cfg = ToolSurfaceConfig::default();
    let s_ab = ToolSurface::build(catalog_schemas(), &cfg, &[p1.clone(), p2.clone()]);
    let s_ba = ToolSurface::build(catalog_schemas(), &cfg, &[p2, p1]);

    let a = build_deferred_tools_section(&s_ab).unwrap();
    let b = build_deferred_tools_section(&s_ba).unwrap();
    assert_eq!(
        a.text, b.text,
        "plugin registration order must not affect deferred block bytes"
    );
}

// ── 3. Empty handling ───────────────────────────────────────────────────────

#[test]
fn deferred_block_returns_none_when_no_deferred_entries() {
    // Pin every tool; deferred becomes empty.
    let cfg = ToolSurfaceConfig {
        pinned_tools: catalog_schemas()
            .iter()
            .filter_map(|s| {
                s.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .map(String::from)
            })
            .collect(),
    };
    let surface = ToolSurface::build(catalog_schemas(), &cfg, &[]);
    assert!(
        surface.deferred().is_empty(),
        "test precondition: nothing should be deferred"
    );
    assert!(
        build_deferred_tools_section(&surface).is_none(),
        "an empty deferred list must not emit a section"
    );
}

// ── 4. Structure / format ───────────────────────────────────────────────────

#[test]
fn deferred_block_format_contains_open_and_close_tags() {
    let section = build_deferred_tools_section(&default_surface()).unwrap();
    assert!(
        section.text.contains("<deferred_tools>"),
        "missing open tag: {}",
        section.text
    );
    assert!(
        section.text.contains("</deferred_tools>"),
        "missing close tag: {}",
        section.text
    );
}

#[test]
fn deferred_block_lists_every_deferred_entry_by_name() {
    let surface = default_surface();
    let section = build_deferred_tools_section(&surface).unwrap();
    for entry in surface.deferred() {
        let needle = format!("<name>{}</name>", entry.name);
        assert!(
            section.text.contains(&needle),
            "deferred block missing entry {}: got:\n{}",
            entry.name,
            section.text
        );
    }
}

#[test]
fn deferred_block_includes_short_description_per_entry() {
    let surface = default_surface();
    let section = build_deferred_tools_section(&surface).unwrap();
    for entry in surface.deferred() {
        // Description is XML-escaped? For now we assert the raw text is
        // embedded; XML escaping can be added if a tool description ever
        // contains &, <, or >. None in the current catalog do.
        assert!(
            section.text.contains(&entry.short_desc),
            "deferred block missing description for {}: got:\n{}",
            entry.name,
            section.text
        );
    }
}

#[test]
fn deferred_block_does_not_mention_any_pinned_tool() {
    use astra_runtime::tool_registry::surface::DEFAULT_PINNED;
    let section = build_deferred_tools_section(&default_surface()).unwrap();
    for pinned in DEFAULT_PINNED {
        let tag = format!("<name>{pinned}</name>");
        assert!(
            !section.text.contains(&tag),
            "deferred block must not list pinned tool {pinned}: got:\n{}",
            section.text
        );
    }
}

// ── 5. Activation nudge ─────────────────────────────────────────────────────

#[test]
fn deferred_block_contains_tool_search_activation_nudge() {
    let section = build_deferred_tools_section(&default_surface()).unwrap();
    assert!(
        section.text.contains("tool_search"),
        "deferred block must reference tool_search so the model knows how to activate a deferred tool: got:\n{}",
        section.text
    );
    assert!(
        section.text.contains("select:"),
        "deferred block must show the `select:NAME` activation form"
    );
}
