use std::collections::HashSet;

use super::ToolExecutor;
use serde_json::{Value, json};

fn executor() -> ToolExecutor {
    let dir = tempfile::tempdir().unwrap();
    ToolExecutor::new(dir.path())
}

fn schema(name: &str) -> Value {
    json!({"type": "function", "function": {"name": name}})
}

fn set_visible(executor: &ToolExecutor, names: &[&str]) {
    let schemas: Vec<Value> = names.iter().map(|name| schema(name)).collect();
    executor.set_current_visible_tool_schemas(&schemas);
}

async fn run_search(executor: &ToolExecutor, args: Value) -> Value {
    let output = executor.execute("tool_search", &args).await;
    serde_json::from_str(&output)
        .unwrap_or_else(|error| panic!("tool_search must return JSON, got {error}: {output}"))
}

fn field_strings(value: &Value, field: &str) -> Vec<String> {
    value[field]
        .as_array()
        .unwrap_or_else(|| panic!("{field} must be an array in {value}"))
        .iter()
        .map(|value| {
            value
                .as_str()
                .unwrap_or_else(|| panic!("{field} entries must be strings in {value}"))
                .to_string()
        })
        .collect()
}

fn match_names(value: &Value) -> Vec<String> {
    value["matches"]
        .as_array()
        .unwrap_or_else(|| panic!("matches must be an array in {value}"))
        .iter()
        .map(|entry| {
            entry["name"]
                .as_str()
                .unwrap_or_else(|| panic!("match entry must have string name in {entry}"))
                .to_string()
        })
        .collect()
}

fn strings(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_string()).collect()
}

#[tokio::test]
async fn tool_search_missing_or_blank_query_returns_exact_error() {
    let executor = executor();
    set_visible(&executor, &["tool_search"]);

    let missing = executor.execute("tool_search", &json!({})).await;
    assert_eq!(missing, "Error: 'query' is required");

    let blank = executor
        .execute("tool_search", &json!({"query": "   "}))
        .await;
    assert_eq!(blank, "Error: 'query' is required");
}

#[tokio::test]
async fn direct_tool_search_fails_closed_without_installed_surface() {
    let executor = executor();
    executor.clear_current_tool_surface_for_tests();

    let parsed: Value =
        serde_json::from_str(&executor.tool_search(&json!({"query": "select:bash"}))).unwrap();

    assert_eq!(parsed["mode"].as_str(), Some("select"));
    assert_eq!(parsed["status"].as_str(), Some("empty_surface"));
    assert_eq!(parsed["total_tools"].as_u64(), Some(0));
    assert!(match_names(&parsed).is_empty());
    assert_eq!(field_strings(&parsed, "missing"), strings(&["bash"]));
    assert!(
        parsed.get("recovery").is_none(),
        "tool_search must return data, not prompt instructions: {parsed}"
    );
}

#[tokio::test]
async fn select_resolves_only_current_visible_tools_with_full_schema() {
    let executor = executor();
    set_visible(&executor, &["bash", "tool_search"]);

    let parsed = run_search(&executor, json!({"query": "select:bash"})).await;

    assert_eq!(parsed["mode"].as_str(), Some("select"));
    assert_eq!(parsed["query"].as_str(), Some("select:bash"));
    assert_eq!(field_strings(&parsed, "requested"), strings(&["bash"]));
    assert!(field_strings(&parsed, "missing").is_empty());
    assert_eq!(match_names(&parsed), strings(&["bash"]));
    assert_eq!(parsed["total_tools"].as_u64(), Some(2));
    assert!(
        parsed["matches"][0].get("parameters").is_some(),
        "select mode must return the full callable schema: {parsed}"
    );
}

#[tokio::test]
async fn select_is_case_insensitive_and_deduplicates_requested_names() {
    let executor = executor();
    set_visible(&executor, &["bash", "read_file", "tool_search"]);

    let parsed = run_search(
        &executor,
        json!({"query": "select:BASH,bash,READ_FILE,missing_tool"}),
    )
    .await;

    assert_eq!(
        field_strings(&parsed, "requested"),
        strings(&["BASH", "READ_FILE", "missing_tool"])
    );
    assert_eq!(match_names(&parsed), strings(&["bash", "read_file"]));
    assert_eq!(
        field_strings(&parsed, "missing"),
        strings(&["missing_tool"])
    );
    assert_eq!(parsed["total_tools"].as_u64(), Some(3));
}

#[tokio::test]
async fn select_pool_is_visible_union_activatable_not_full_catalog() {
    let executor = executor();
    set_visible(&executor, &["bash", "tool_search"]);
    executor.set_current_activatable_tool_names(HashSet::from(["web_fetch".to_string()]));

    let parsed = run_search(
        &executor,
        json!({"query": "select:bash,web_fetch,read_file"}),
    )
    .await;

    assert_eq!(match_names(&parsed), strings(&["bash", "web_fetch"]));
    assert_eq!(field_strings(&parsed, "missing"), strings(&["read_file"]));
    assert_eq!(parsed["total_tools"].as_u64(), Some(3));
    assert_eq!(
        executor.activated_deferred_tool_names(),
        vec!["web_fetch".to_string()],
        "only activatable selected tools should be recorded as activated"
    );
}

#[tokio::test]
async fn selecting_visible_tool_does_not_record_deferred_activation() {
    let executor = executor();
    set_visible(&executor, &["bash", "tool_search"]);

    let parsed = run_search(&executor, json!({"query": "select:bash"})).await;

    assert_eq!(match_names(&parsed), strings(&["bash"]));
    assert_eq!(
        executor.activated_deferred_tool_names(),
        Vec::<String>::new(),
        "visible tools are already callable; selecting them must not create deferred activation state"
    );
}

#[tokio::test]
async fn activated_deferred_tool_stays_active_when_next_surface_makes_it_visible() {
    let executor = executor();
    set_visible(&executor, &["bash", "tool_search"]);
    executor.set_current_activatable_tool_names(HashSet::from(["web_fetch".to_string()]));

    let parsed = run_search(&executor, json!({"query": "select:web_fetch"})).await;
    assert_eq!(match_names(&parsed), strings(&["web_fetch"]));
    assert_eq!(
        executor.activated_deferred_tool_names(),
        strings(&["web_fetch"])
    );

    set_visible(&executor, &["bash", "tool_search", "web_fetch"]);
    executor.set_current_activatable_tool_names(HashSet::new());

    assert_eq!(
        executor.activated_deferred_tool_names(),
        strings(&["web_fetch"]),
        "activation must survive the visible/deferred partition flip after the selected tool is injected"
    );
}

#[tokio::test]
async fn activated_deferred_tool_is_pruned_when_no_longer_visible_or_activatable() {
    let executor = executor();
    set_visible(&executor, &["bash", "tool_search"]);
    executor.set_current_activatable_tool_names(HashSet::from(["web_fetch".to_string()]));

    let parsed = run_search(&executor, json!({"query": "select:web_fetch"})).await;
    assert_eq!(match_names(&parsed), strings(&["web_fetch"]));
    assert_eq!(
        executor.activated_deferred_tool_names(),
        strings(&["web_fetch"])
    );

    set_visible(&executor, &["bash", "tool_search"]);
    executor.set_current_activatable_tool_names(HashSet::new());

    assert_eq!(
        executor.activated_deferred_tool_names(),
        Vec::<String>::new(),
        "activation must be scoped to the current surface and disappear when the tool is neither visible nor activatable"
    );
}

#[tokio::test]
async fn keyword_search_uses_the_same_visible_union_activatable_pool() {
    let executor = executor();
    set_visible(&executor, &["bash", "tool_search"]);
    executor.set_current_activatable_tool_names(HashSet::from(["web_fetch".to_string()]));

    let parsed = run_search(
        &executor,
        json!({"query": "fetch web url", "max_results": 20}),
    )
    .await;
    let names = match_names(&parsed);

    assert_eq!(parsed["mode"].as_str(), Some("keyword"));
    assert_eq!(parsed["total_tools"].as_u64(), Some(3));
    assert!(
        names.iter().any(|name| name == "web_fetch"),
        "keyword search should find the activatable deferred tool: {parsed}"
    );
    assert!(
        names
            .iter()
            .all(|name| matches!(name.as_str(), "bash" | "tool_search" | "web_fetch")),
        "keyword search must not leak tools outside visible ∪ activatable: {parsed}"
    );
}

#[tokio::test]
async fn mcp_plugin_schema_requires_runtime_binding_to_resolve() {
    let executor = executor();
    set_visible(&executor, &["bash", "tool_search"]);
    let plugin = json!({
        "type": "function",
        "function": {
            "name": "mcp__weather",
            "description": "Get weather for a city.",
            "parameters": {
                "type": "object",
                "properties": {"city": {"type": "string"}},
                "required": ["city"]
            }
        }
    });
    executor.set_plugin_schemas(vec![plugin]);

    let hidden = run_search(&executor, json!({"query": "select:mcp__weather"})).await;
    assert!(match_names(&hidden).is_empty());
    assert_eq!(
        field_strings(&hidden, "missing"),
        strings(&["mcp__weather"])
    );
    assert_eq!(hidden["total_tools"].as_u64(), Some(2));

    executor.set_current_activatable_tool_names(HashSet::from(["mcp__weather".to_string()]));
    let activatable = run_search(&executor, json!({"query": "select:mcp__weather"})).await;
    assert!(
        match_names(&activatable).is_empty(),
        "cached MCP schemas must not resolve without a manager-owned runtime tool: {activatable}"
    );
    assert_eq!(
        field_strings(&activatable, "missing"),
        strings(&["mcp__weather"])
    );
    assert_eq!(activatable["total_tools"].as_u64(), Some(2));
}
