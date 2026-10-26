//! Misbehaving provider responses through real parsing, admission and recovery.
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

async fn recover_from_call(
    name: &str,
    arguments: &str,
    preliminary_text: &str,
) -> (Vec<Value>, Vec<Value>) {
    let workspace = tempfile::TempDir::new().unwrap();
    std::fs::write(
        workspace.path().join("evidence.txt"),
        "unchanged evidence\n",
    )
    .unwrap();
    let gateway = ProviderGateway::start(vec![ProviderScript::new("primary recovery", |r| r.path == "/v1/chat/completions" && r.body["model"] == "provider-fixture-model", vec![
        ProviderResponse::OpenAi(json!({"id":"misbehaving-response","model":"provider-fixture-model","choices":[{"index":0,"message":{"role":"assistant","content":preliminary_text,"tool_calls":[{"id":"call-unhappy","type":"function","function":{"name":name,"arguments":arguments}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}})),
        ProviderResponse::OpenAi(json!({"id":"recovery-response","model":"provider-fixture-model","choices":[{"index":0,"message":{"role":"assistant","content":"The request could not be executed; no changes were made."},"finish_reason":"stop"}],"usage":{"prompt_tokens":15,"completion_tokens":6,"total_tokens":21}})),
    ])]).await;
    let ledger = ProviderObservationsFixture::default();
    let session = format!("unhappy-fixture-{}", uuid::Uuid::new_v4());
    let mut host = server_host_builder(
        &gateway,
        &ledger,
        &session,
        "openai",
        "provider-fixture-model",
        None,
    )
    .with_static_tool_catalog_admissible(true)
    .with_execution_binding_snapshot(ExecutionBindingSnapshot::inferred(
        WorkspaceBinding::server_sandbox(workspace.path()),
        ExecutorBinding::server_local(),
    ))
    .build();
    let mut state = loop_state(
        &session,
        Vec::new(),
        "Explain the evidence without making any changes.",
    );
    bind_server_workspace(&mut state, workspace.path()).await;
    state.skills.request_constraints.allowed_tools =
        Some(["read_file".to_owned()].into_iter().collect());
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        run_agentic_loop_with_host(&mut host, &mut state),
    )
    .await
    .expect("bounded recovery")
    .unwrap();
    assert_eq!(
        state.final_text,
        "The request could not be executed; no changes were made."
    );
    assert_eq!(state.llm_rounds_completed, 2);
    assert_eq!((state.total_prompt, state.total_completion), (25, 11));
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("evidence.txt")).unwrap(),
        "unchanged evidence\n"
    );
    assert_eq!(ledger.attempt_count(), 2);
    ledger.assert_quiescent();
    gateway.assert_complete();
    let requests = gateway.requests.lock().await;
    assert_eq!(requests.len(), 2);
    let messages = requests[1].body["messages"].as_array().unwrap().clone();
    let records = state
        .stall
        .tool_call_records
        .iter()
        .map(|record| serde_json::to_value(record).unwrap())
        .collect();
    (messages, records)
}

fn paired_failure(messages: &[Value]) -> &str {
    let assistant = messages
        .iter()
        .find(|m| m.pointer("/tool_calls/0/id").and_then(Value::as_str) == Some("call-unhappy"))
        .expect("real assistant tool request");
    assert_eq!(assistant["tool_calls"].as_array().unwrap().len(), 1);
    let results: Vec<_> = messages
        .iter()
        .filter(|m| m["role"] == "tool" && m["tool_call_id"] == "call-unhappy")
        .collect();
    assert_eq!(results.len(), 1, "exactly one actual admission result");
    results[0]["content"].as_str().unwrap()
}

#[tokio::test]
async fn unhappy_tool_call_with_invalid_json_args() {
    let (messages, records) = recover_from_call("read_file", "{broken", "").await;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["disposition"], "rejected");
    assert_eq!(records[0]["ok"], false);
    let rejection: Value =
        serde_json::from_str(records[0]["result_full"].as_str().unwrap()).unwrap();
    assert_eq!(rejection["status"], "rejected");
    assert_eq!(rejection["error_kind"], "tool_call_arguments_invalid");
    let wire_rejection: Value = serde_json::from_str(paired_failure(&messages)).unwrap();
    assert_eq!(wire_rejection["error_kind"], "tool_call_arguments_invalid");
}

#[tokio::test]
async fn unhappy_tool_call_to_unknown_tool_name() {
    let (messages, records) = recover_from_call("nonexistent_synth_tool", "{}", "").await;
    // The native response parser rejects calls absent from the exact wire surface.
    assert!(records.is_empty(), "no tool invocation was admitted");
    assert!(
        messages
            .iter()
            .all(|message| message.get("tool_calls").is_none() && message["role"] != "tool")
    );
    assert!(
        messages.iter().any(|message| message["content"]
            .as_str()
            .is_some_and(|text| text
                .contains("provider_attempt_ended_without_executable_or_visible_delivery")))
    );
}

#[tokio::test]
async fn unhappy_assistant_final_then_extra_tool_calls() {
    let (messages, records) = recover_from_call(
        "read_file",
        "{\"path\":\"missing-file\"}",
        "Premature final answer.",
    )
    .await;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["disposition"], "executed");
    assert_eq!(records[0]["ok"], false);
    assert!(paired_failure(&messages).contains("PATH_RESOLUTION_FAILED"));
    // The public loop delivers only the later answer after real tool completion.
}
