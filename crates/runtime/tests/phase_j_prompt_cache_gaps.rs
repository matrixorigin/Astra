//! Anthropic cache budgets and model changes observed on actual HTTP requests.
#![cfg(feature = "e2e-hooks")]

use astra_runtime::server::provider_test_support::{
    ProviderGateway, ProviderObservationsFixture, ProviderResponse, ProviderScript,
    bind_server_workspace, loop_state, server_host_builder,
};
use astra_runtime::server::tool_transport::{
    ExecutionBindingSnapshot, ExecutorBinding, WorkspaceBinding,
};
use astra_runtime::turn::agentic_loop::finalization::run_agentic_loop_with_host;
use serde_json::{Value, json};

fn count_cache_control(value: &Value) -> usize {
    match value {
        Value::Object(map) => {
            usize::from(map.contains_key("cache_control"))
                + map.values().map(count_cache_control).sum::<usize>()
        }
        Value::Array(values) => values.iter().map(count_cache_control).sum(),
        _ => 0,
    }
}

async fn request(model: &str, prefix: Vec<Value>) -> Value {
    let expected_model = model.to_owned();
    let gateway = ProviderGateway::start(vec![ProviderScript::new("Anthropic explanation", move |r| r.path == "/v1/messages" && r.body["model"] == expected_model, vec![ProviderResponse::Anthropic(json!({
        "id":"anthropic-explanation", "model":model, "content":[{"type":"text","text":"The explanation is complete."}], "stop_reason":"end_turn", "usage":{"input_tokens":10,"output_tokens":5}
    }))])]).await;
    let session = format!("cache-fixture-{}", uuid::Uuid::new_v4());
    let workspace = tempfile::TempDir::new().unwrap();
    let ledger = ProviderObservationsFixture::default();
    let mut host = server_host_builder(&gateway, &ledger, &session, "anthropic", model, None)
        .with_static_tool_catalog_admissible(true)
        .with_execution_binding_snapshot(ExecutionBindingSnapshot::inferred(
            WorkspaceBinding::server_sandbox(workspace.path()),
            ExecutorBinding::server_local(),
        ))
        .build();
    let mut state = loop_state(&session, prefix, "Explain this without making changes.");
    bind_server_workspace(&mut state, workspace.path()).await;
    state.policy_model_name = Some(model.into());
    state.skills.request_constraints.allowed_tools = Some(
        ["read_file", "glob", "grep"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
    );
    run_agentic_loop_with_host(&mut host, &mut state)
        .await
        .unwrap();
    assert_eq!(state.final_text, "The explanation is complete.");
    assert_eq!(ledger.attempt_count(), 1);
    ledger.assert_quiescent();
    gateway.assert_complete();
    let requests = gateway.requests.lock().await;
    assert_eq!(requests.len(), 1);
    requests[0].body.clone()
}

#[tokio::test]
async fn anthropic_total_cache_breakpoints_respect_four_budget() {
    let wire = request("claude-sonnet-4", Vec::new()).await;
    let system = count_cache_control(&wire["system"]);
    let tools = count_cache_control(&wire["tools"]);
    let messages = count_cache_control(&wire["messages"]);
    assert!(system > 0, "cache metadata must actually be enabled");
    assert!(system + tools + messages <= 4);
    assert_eq!(tools, 1);
    assert!(messages <= 1);
}

#[tokio::test]
async fn long_message_history_emits_single_message_breakpoint() {
    let prefix = (0..15)
        .flat_map(|i| {
            [
                json!({"role":"user","content":format!("u{i}")}),
                json!({"role":"assistant","content":format!("a{i}")}),
            ]
        })
        .collect();
    let wire = request("claude-sonnet-4", prefix).await;
    let messages = wire["messages"].as_array().unwrap();
    let seeded: Vec<_> = messages
        .iter()
        .filter_map(|message| {
            let text = match &message["content"] {
                Value::String(text) => text.clone(),
                Value::Array(blocks) => blocks
                    .iter()
                    .filter_map(|block| block["text"].as_str())
                    .collect::<Vec<_>>()
                    .join(""),
                other => panic!("invalid native Anthropic content: {other}"),
            };
            (text.starts_with('u') || text.starts_with('a'))
                .then_some((message["role"].clone(), text))
        })
        .collect();
    let expected: Vec<_> = (0..15)
        .flat_map(|i| {
            [
                (json!("user"), format!("u{i}")),
                (json!("assistant"), format!("a{i}")),
            ]
        })
        .collect();
    assert_eq!(
        seeded, expected,
        "all thirty seeded messages survive in order"
    );
    assert_eq!(count_cache_control(&wire["messages"]), 1);
    assert!(count_cache_control(&wire) <= 4);
}

#[tokio::test]
async fn anthropic_model_change_preserves_prefix_and_surfaces_model_id() {
    let a = request("claude-haiku-4.5", Vec::new()).await;
    let b = request("claude-sonnet-4.5", Vec::new()).await;
    assert_eq!(a["model"], "claude-haiku-4.5");
    assert_eq!(b["model"], "claude-sonnet-4.5");
    // Compare the actual cache-marked system blocks and tool declarations.
    // This proves prefix stability; it makes no claim about a provider cache hit.
    let cached = |wire: &Value| {
        let blocks = wire["system"].as_array().unwrap();
        let boundary = blocks
            .iter()
            .rposition(|block| block.get("cache_control").is_some())
            .expect("real cache boundary");
        blocks[..=boundary].to_vec()
    };
    assert!(!cached(&a).is_empty());
    assert!(!cached(&b).is_empty());
    assert!(!a["tools"].as_array().unwrap().is_empty());
    assert!(!b["tools"].as_array().unwrap().is_empty());
    assert_eq!(count_cache_control(&a["tools"]), 1);
    assert_eq!(count_cache_control(&b["tools"]), 1);
    assert_eq!(cached(&a), cached(&b));
    assert_eq!(a["tools"], b["tools"]);
    let visible_outside_cache = |wire: &Value| {
        let uncached_system: Vec<_> = wire["system"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|block| block.get("cache_control").is_none())
            .cloned()
            .collect();
        json!({"system":uncached_system,"messages":wire["messages"]}).to_string()
    };
    assert!(visible_outside_cache(&a).contains("Model: claude-haiku-4.5"));
    assert!(visible_outside_cache(&b).contains("Model: claude-sonnet-4.5"));
}
