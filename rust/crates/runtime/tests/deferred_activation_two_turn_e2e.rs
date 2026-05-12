//! P0 end-to-end contract: the deferred activation flow composes correctly
//! across two turns.
//!
//! Turn N : LLM sees `<deferred_tools>` listing `github`. Calls
//!          `tool_search(query="select:github")`. Runtime returns the
//!          full `github` schema in the tool_result.
//! Turn N+1: LLM calls `github(action="list_prs", ...)`. `github` is NOT
//!          in `tools[]` (it's deferred). Validator must accept.
//!
//! This test simulates both turns at the public-API level. If either
//! primitive regresses — `tool_search(select:…)` stops returning a usable
//! schema, or the validator stops admitting deferred names — this test
//! fails loudly.

use astra_runtime::turn::headless_tool_pipeline::admissible_tool_names_from_visible;
use astra_tools::schemas::all_tool_schemas;
use astra_tools::tool_search::tool_search;
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
fn turn_n_tool_search_select_returns_usable_schema_for_deferred_tool() {
    // Turn N: LLM asks for the github schema. The full catalog is passed
    // to `tool_search` (this is what production should do — it's NOT the
    // per-turn `tools[]`, it's the dispatchable catalog).
    let schemas = all_tool_schemas();
    let result = tool_search(&schemas, &json!({"query": "select:github"}));

    let parsed: Value = serde_json::from_str(&result).expect("tool_search must return valid JSON");

    let matches = parsed["matches"]
        .as_array()
        .expect("result must contain `matches` array");
    assert_eq!(matches.len(), 1, "one match for select:github");

    let github = &matches[0];
    assert_eq!(github["name"].as_str(), Some("github"));

    // The schema must be usable — parameters must be present so the LLM
    // can actually invoke it next turn.
    assert!(
        github.get("parameters").is_some(),
        "select: mode must include full parameters so the next turn can invoke: {github}"
    );

    // Description should be the full description, not truncated — the
    // model explicitly asked for this schema, we owe it the real thing.
    let desc = github["description"].as_str().unwrap_or("");
    assert!(
        !desc.ends_with('…'),
        "select: mode must not truncate description; got: {desc}"
    );

    let missing = parsed["missing"].as_array().unwrap();
    assert!(missing.is_empty(), "github exists; `missing` must be empty");
}

#[test]
fn turn_n_plus_1_validator_admits_github_even_when_not_in_tools_array() {
    // Turn N+1: model now calls github. `tools[]` this turn contains only
    // the pinned set — github is deferred. The validator's admitted set
    // must include github because it's dispatchable via the catalog.
    let pinned_visible = vec![
        json!({"type": "function", "function": {"name": "bash"}}),
        json!({"type": "function", "function": {"name": "read_file"}}),
        json!({"type": "function", "function": {"name": "tool_search"}}),
    ];
    let admitted = admissible_tool_names_from_visible(&pinned_visible);
    assert!(
        admitted.contains("github"),
        "validator must admit github post-deferred-activation; got {admitted:?}"
    );
    // And the visible tools remain admitted.
    assert!(admitted.contains("bash"));
    assert!(admitted.contains("tool_search"));
}

#[test]
fn two_turn_flow_composes_end_to_end() {
    // The combined assertion: turn N produces a schema the model can
    // invoke, and turn N+1's validator accepts the invocation.
    let schemas = all_tool_schemas();

    // Turn N — ask for web_fetch schema.
    let t1 = tool_search(&schemas, &json!({"query": "select:web_fetch"}));
    let t1_parsed: Value = serde_json::from_str(&t1).unwrap();
    assert_eq!(
        t1_parsed["matches"][0]["name"].as_str(),
        Some("web_fetch"),
        "turn N: select must return web_fetch schema"
    );

    // Turn N+1 — validator check. `tools[]` has pinned only, NOT web_fetch.
    let pinned_visible = vec![
        pick_schema(&schemas, "bash").unwrap(),
        pick_schema(&schemas, "read_file").unwrap(),
    ];
    let admitted = admissible_tool_names_from_visible(&pinned_visible);
    assert!(
        admitted.contains("web_fetch"),
        "turn N+1: web_fetch must be admissible"
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
