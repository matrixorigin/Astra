//! End-to-end contract: the deferred activation flow composes correctly
//! across two model requests.
//!
//! Turn N : LLM sees `<deferred_tools>` listing `github`. Calls
//!          `tool_search(query="select:github")`. Runtime returns compact
//!          callable shape and records the selected name.
//! Turn N+1: Surface assembly consumes that one-shot selection and injects the
//!          full `github` schema into `tools[]`. The validator admits the tool
//!          because it is visible in the current request, not because deferred
//!          activation became a long-lived execution allowlist.
//!
//! This test simulates both turns at the public-API level. If either
//! primitive regresses — `tool_search(select:…)` stops returning callable
//! shape, or the selected schema is not what makes the next turn executable —
//! this test fails loudly.

use astra_runtime::turn::headless_tool_pipeline::admissible_tool_names_from_visible;
use astra_tools::schemas::all_tool_schemas;
use astra_tools::tool_search::tool_search;
use astra_turn_core::tool::deferred_activation::activated_tool_names_from_tool_search_output;
use serde_json::{Value, json};

fn pick_schema(schemas: &[Value], name: &str) -> Option<Value> {
    schemas
        .iter()
        .find(|s| {
            s.get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                == Some(name)
        })
        .cloned()
}

#[test]
fn turn_n_tool_search_select_returns_callable_shape_for_deferred_tool() {
    // Turn N: LLM asks for the github schema. Production passes the searchable
    // surface (visible tools plus currently activatable deferred tools), not a
    // validator allowlist.
    let schemas = all_tool_schemas();
    let result = tool_search(&schemas, &json!({"query": "select:github"}));

    let parsed: Value = serde_json::from_str(&result).expect("tool_search must return valid JSON");

    let matches = parsed["matches"]
        .as_array()
        .expect("result must contain `matches` array");
    assert_eq!(matches.len(), 1, "one match for select:github");

    let github = &matches[0];
    assert_eq!(github["name"].as_str(), Some("github"));

    // The result must be usable recovery guidance — enough shape for the model
    // to form the next call, without being the authority that admits execution.
    assert!(
        github.get("parameters").is_some(),
        "select: mode must include callable parameters so the next turn can invoke: {github}"
    );

    let desc = github["description"].as_str().unwrap_or("");
    assert!(
        !desc.ends_with('…'),
        "select: mode must keep a usable description; got: {desc}"
    );

    let missing = parsed["missing"].as_array().unwrap();
    assert!(missing.is_empty(), "github exists; `missing` must be empty");
}

#[test]
fn turn_n_plus_1_validator_admits_github_only_after_schema_is_injected() {
    // Turn N+1 before surface assembly consumes the selection: github is still
    // absent from tools[], so execution stays rejected.
    let pinned_visible = vec![
        json!({"type": "function", "function": {"name": "bash"}}),
        json!({"type": "function", "function": {"name": "read_file"}}),
        json!({"type": "function", "function": {"name": "tool_search"}}),
    ];
    let admitted = admissible_tool_names_from_visible(&pinned_visible);
    assert!(!admitted.contains("github"));

    let schemas = all_tool_schemas();
    let mut visible_after_activation = pinned_visible;
    visible_after_activation.push(pick_schema(&schemas, "github").unwrap());
    let admitted = admissible_tool_names_from_visible(&visible_after_activation);
    assert!(
        admitted.contains("github"),
        "validator must admit github only after its schema is visible; got {admitted:?}"
    );
    // And the visible tools remain admitted.
    assert!(admitted.contains("bash"));
    assert!(admitted.contains("tool_search"));
}

#[test]
fn two_turn_flow_composes_end_to_end() {
    // The combined assertion: turn N selects a deferred tool, turn N+1 injects
    // its schema, and the following turn without use does not retain it.
    let schemas = all_tool_schemas();

    // Turn N — ask for web_fetch schema.
    let t1 = tool_search(&schemas, &json!({"query": "select:web_fetch"}));
    let t1_parsed: Value = serde_json::from_str(&t1).unwrap();
    assert_eq!(
        t1_parsed["matches"][0]["name"].as_str(),
        Some("web_fetch"),
        "turn N: select must return web_fetch schema"
    );
    let activated = activated_tool_names_from_tool_search_output(&t1);
    assert_eq!(activated, vec!["web_fetch".to_string()]);

    // Turn N+1 before injection: `tools[]` has pinned only, NOT web_fetch.
    let pinned_visible = vec![
        pick_schema(&schemas, "bash").unwrap(),
        pick_schema(&schemas, "read_file").unwrap(),
    ];
    let admitted = admissible_tool_names_from_visible(&pinned_visible);
    assert!(!admitted.contains("web_fetch"));

    let mut injected_visible = pinned_visible.clone();
    for name in &activated {
        injected_visible.push(pick_schema(&schemas, name).unwrap());
    }
    let admitted = admissible_tool_names_from_visible(&injected_visible);
    assert!(
        admitted.contains("web_fetch"),
        "turn N+1: web_fetch must be admissible after its schema is injected"
    );

    let followup_without_invocation = admissible_tool_names_from_visible(&pinned_visible);
    assert!(
        !followup_without_invocation.contains("web_fetch"),
        "unused one-shot activation must not make web_fetch executable forever"
    );
}

#[test]
fn activation_flow_rejects_hallucinated_tool_name() {
    // Symmetric negative: if the LLM hallucinates a tool that doesn't exist
    // anywhere (not in catalog, not visible), validator must reject.
    // Otherwise the runtime would happily dispatch made-up names.
    let visible: Vec<Value> = vec![];
    let admitted = admissible_tool_names_from_visible(&visible);
    assert!(
        !admitted.contains("definitely_not_a_real_tool"),
        "hallucinated names must stay rejected"
    );
}
