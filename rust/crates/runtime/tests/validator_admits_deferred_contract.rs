//! P0 contract: the runtime validator must accept tool calls whose names
//! are dispatchable even when they're not in the per-turn `tools[]` slice.
//!
//! This is the whole point of the deferred tool architecture. Before this
//! fix, `tool_search(query="select:web_fetch")` returned a schema, but
//! calling `web_fetch` on the next turn got rejected as "Unknown tool"
//! because only pinned tools sit in `tools[]`.
//!
//! Red first. The test targets a pure function we add to the validator
//! boundary. Production wires it at `sync_valid_tools_to_visible`.

use astra_runtime::turn::headless_tool_pipeline::admissible_tool_names_from_visible;
use serde_json::{Value, json};

fn schema(name: &str) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": "",
            "parameters": {"type": "object", "properties": {}}
        }
    })
}

#[test]
fn visible_tools_plus_catalog_are_all_admitted() {
    // Visible this turn: just the T1 pinned schemas.
    let visible = vec![schema("bash"), schema("read_file")];
    let admitted = admissible_tool_names_from_visible(&visible);

    // bash is visible → admitted directly.
    assert!(admitted.contains("bash"));
    // github is in TOOL_CATALOG (pinned=true) but not in `visible` this
    // turn. The validator must still admit it — the model just got a
    // schema for it via `tool_search(select:github)` and is calling it now.
    assert!(
        admitted.contains("github"),
        "deferred tools in the catalog must be admitted; got {admitted:?}"
    );
    assert!(admitted.contains("memory"));
    assert!(admitted.contains("introspect"));
}

#[test]
fn truly_unknown_name_stays_rejected() {
    let visible = vec![schema("bash")];
    let admitted = admissible_tool_names_from_visible(&visible);
    assert!(!admitted.contains("completely_made_up_tool"));
}

#[test]
fn visible_schema_not_in_catalog_is_still_admitted() {
    // A runtime-injected schema (skill / tool_search / spawn_agent etc.)
    // lives in `visible` but not in the static catalog. It must be
    // admitted.
    let visible = vec![schema("skill"), schema("bash")];
    let admitted = admissible_tool_names_from_visible(&visible);
    assert!(
        admitted.contains("skill"),
        "visible tools not in catalog must still be admitted; got {admitted:?}"
    );
}

#[test]
fn runtime_injected_tools_like_skill_and_spawn_agent_are_admitted_when_visible() {
    // Runtime-injected schemas (skill, spawn_agent, web_search, task, notify,
    // ask_user) aren't in TOOL_CATALOG but they ARE dispatchable. When
    // they're in the visible tools[] slice, admissible_tool_names must
    // include them (this was already trivially true via the union) —
    // and critically, must also admit them when NOT visible but supplied
    // via the injected extra list. Covered by the explicit helper.
    use astra_runtime::turn::headless_tool_pipeline::admissible_tool_names_from_visible_and_extras;
    let visible = vec![schema("bash")];
    let extras = vec!["skill".to_string(), "spawn_agent".to_string()];
    let admitted = admissible_tool_names_from_visible_and_extras(&visible, &extras);
    assert!(admitted.contains("bash"));
    assert!(
        admitted.contains("skill"),
        "runtime-injected 'skill' must be admitted via extras"
    );
    assert!(admitted.contains("spawn_agent"));
    // Catalog entries still admitted.
    assert!(admitted.contains("github"));
}

#[test]
fn plugin_names_admitted_via_extras() {
    // MCP/plugin tools aren't in TOOL_CATALOG. The extras list is how
    // callers surface them to the validator.
    use astra_runtime::turn::headless_tool_pipeline::admissible_tool_names_from_visible_and_extras;
    let visible = vec![schema("bash")];
    let extras = vec!["mcp__weather".to_string()];
    let admitted = admissible_tool_names_from_visible_and_extras(&visible, &extras);
    assert!(admitted.contains("mcp__weather"));
}

#[test]
fn empty_visible_still_admits_catalog() {
    // An edge case — an empty `visible` slice shouldn't strand the model.
    // It should still be able to reach catalog tools via tool_search.
    let admitted = admissible_tool_names_from_visible(&[]);
    assert!(admitted.contains("bash"));
    assert!(admitted.contains("github"));
}
