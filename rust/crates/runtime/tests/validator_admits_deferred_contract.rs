//! Deferred execution contract: the validator admits visible tools plus
//! explicitly activated/injected names. The full catalog is discovery data,
//! not an execution allowlist.

use astra_runtime::turn::headless_tool_pipeline::{
    admissible_tool_names_from_visible, admissible_tool_names_from_visible_and_extras,
};
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
fn visible_tools_are_admitted_but_catalog_is_not_implicit() {
    let visible = vec![schema("bash"), schema("read_file")];
    let admitted = admissible_tool_names_from_visible(&visible);

    assert!(admitted.contains("bash"));
    assert!(
        !admitted.contains("github"),
        "catalog-only deferred tools must not be executable before activation; got {admitted:?}"
    );
}

#[test]
fn truly_unknown_name_stays_rejected() {
    let visible = vec![schema("bash")];
    let admitted = admissible_tool_names_from_visible(&visible);
    assert!(!admitted.contains("completely_made_up_tool"));
}

#[test]
fn visible_schema_not_in_catalog_is_still_admitted() {
    // A runtime-surface schema (skill / task / tool_search etc.)
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
fn runtime_surface_tools_like_skill_and_task_are_admitted_when_visible() {
    let visible = vec![schema("bash")];
    let extras = vec!["skill".to_string(), "task".to_string()];
    let admitted = admissible_tool_names_from_visible_and_extras(&visible, &extras);
    assert!(admitted.contains("bash"));
    assert!(
        admitted.contains("skill"),
        "runtime-injected 'skill' must be admitted via extras"
    );
    assert!(admitted.contains("task"));
    assert!(!admitted.contains("github"));
}

#[test]
fn plugin_names_admitted_via_extras() {
    let visible = vec![schema("bash")];
    let extras = vec!["mcp__weather".to_string()];
    let admitted = admissible_tool_names_from_visible_and_extras(&visible, &extras);
    assert!(admitted.contains("mcp__weather"));
}

#[test]
fn empty_visible_admits_only_explicit_extras() {
    let admitted = admissible_tool_names_from_visible(&[]);
    assert!(admitted.is_empty());

    let admitted = admissible_tool_names_from_visible_and_extras(&[], &["github".to_string()]);
    assert!(admitted.contains("github"));
}
