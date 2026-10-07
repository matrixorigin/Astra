//! Real Server tool execution and subsequent provider wire history.
#![cfg(feature = "e2e-hooks")]

use astra_runtime::server::provider_test_support::{
    InferenceLedgerFixture, ProviderGateway, ProviderResponse, ProviderScript,
    bind_server_workspace, loop_state, server_host_builder,
};
use astra_runtime::server::tool_transport::{
    ExecutionBindingSnapshot, ExecutorBinding, WorkspaceBinding,
};
use astra_runtime::turn::agentic_loop::finalization::run_agentic_loop_with_host;
use serde_json::{Value, json};

#[tokio::test]
async fn server_lifecycle_parallel_reads_preserve_actual_results_and_pairing() {
    let workspace = tempfile::TempDir::new().unwrap();
    std::fs::write(
        workspace.path().join("evidence.txt"),
        "fixture evidence from the executor\n",
    )
    .unwrap();
    std::fs::write(
        workspace.path().join("second.txt"),
        "independent second evidence\n",
    )
    .unwrap();
    let gateway = ProviderGateway::start(vec![ProviderScript::new(
        "primary read then explain", |r| r.path == "/v1/chat/completions" && r.body["model"] == "provider-fixture-model",
        vec![
            ProviderResponse::OpenAi(json!({"id":"read-request","model":"provider-fixture-model",
                "choices":[{"index":0,"message":{"role":"assistant","tool_calls":[{"id":"call-read-evidence","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"evidence.txt\"}"}},{"id":"call-read-second","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"second.txt\"}"}}]},"finish_reason":"tool_calls"}],
                "usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),
            ProviderResponse::OpenAi(json!({"id":"read-answer","model":"provider-fixture-model",
                "choices":[{"index":0,"message":{"role":"assistant","content":"The file contains fixture evidence from the executor."},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":52,"completion_tokens":11,"total_tokens":63}})),
        ]
    )]).await;
    let ledger = InferenceLedgerFixture::default();
    let session = format!("tool-fixture-{}", uuid::Uuid::new_v4());
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
        "Read evidence.txt and explain its contents without making changes.",
    );
    bind_server_workspace(&mut state, workspace.path()).await;
    state.skills.request_constraints.allowed_tools =
        Some(["read_file".to_owned()].into_iter().collect());
    run_agentic_loop_with_host(&mut host, &mut state)
        .await
        .unwrap();
    assert_eq!(
        state.final_text,
        "The file contains fixture evidence from the executor."
    );
    assert_eq!(state.llm_rounds_completed, 2);
    assert_eq!(
        state
            .turn_event_buffer
            .as_ref()
            .expect("turn observation buffer")
            .current_round(),
        2
    );
    assert_eq!(state.total_tool_calls, 2);
    let records: Vec<_> = state
        .stall
        .tool_call_records
        .iter()
        .filter(|r| r.was_executed())
        .collect();
    assert_eq!(records.len(), 2);
    let batch = records[0].batch_id.as_ref().expect("actual batch identity");
    for record in records {
        assert_eq!(record.round, Some(0));
        assert_eq!(record.parallel, Some(true));
        assert_eq!(record.batch_id.as_ref(), Some(batch));
        assert!(record.start_offset_ms.is_some());
    }
    assert_eq!((state.total_prompt, state.total_completion), (94, 18));
    assert_eq!(ledger.attempt_count(), 2);
    ledger.assert_quiescent();
    gateway.assert_complete();
    let emitted: Vec<_> = host
        .take_emitted_events()
        .into_iter()
        .filter(|event| event["type"] == "tool_call")
        .collect();
    assert_eq!(
        emitted.len(),
        2,
        "each actual invocation emits one canonical call"
    );
    let call = &emitted[0]["tool_call"];
    assert_eq!(call["id"], "call-read-evidence");
    assert_eq!(call["function"]["name"], "read_file");
    assert_eq!(call["function"]["arguments"], "{\"path\":\"evidence.txt\"}");
    assert!(call.get("name").is_none());
    assert!(call.get("arguments").is_none());
    let requests = gateway.requests.lock().await;
    assert_eq!(requests.len(), 2);
    let messages = requests[1].body["messages"].as_array().unwrap();
    let prompt_content: String = messages
        .iter()
        .filter_map(|message| message["content"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        prompt_content
            .matches("\"kind\":\"tool_batch_coaching\"")
            .count(),
        1,
        "actual parallel execution produces one typed feedback envelope",
    );
    assert_eq!(
        prompt_content
            .matches("2 tools executed in parallel")
            .count(),
        1
    );
    assert!(
        !prompt_content.contains("Previous round:"),
        "history-derived duplicate feedback is retired",
    );
    let ids: Vec<_> = messages
        .iter()
        .filter_map(|m| m["tool_calls"].as_array())
        .flatten()
        .map(|call| call["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["call-read-evidence", "call-read-second"]);
    let assistant = messages
        .iter()
        .find(|m| {
            m.pointer("/tool_calls/0/id").and_then(Value::as_str) == Some("call-read-evidence")
        })
        .unwrap();
    let results: Vec<_> = messages
        .iter()
        .filter(|m| m["role"] == "tool" && m["tool_call_id"] == "call-read-evidence")
        .collect();
    assert_eq!(results.len(), 1);
    assert_eq!(assistant["tool_calls"].as_array().unwrap().len(), 2);
    let output = results[0]["content"].as_str().unwrap();
    assert!(
        output.contains("fixture evidence from the executor"),
        "actual executor output: {output}"
    );
    let second_results: Vec<_> = messages
        .iter()
        .filter(|m| m["role"] == "tool" && m["tool_call_id"] == "call-read-second")
        .collect();
    assert_eq!(second_results.len(), 1);
    assert!(
        second_results[0]["content"]
            .as_str()
            .unwrap()
            .contains("independent second evidence")
    );
    assert!(
        messages
            .iter()
            .all(|m| m.get("reasoning_content").is_none())
    );
}

#[tokio::test]
async fn server_lifecycle_cancellation_settles_a_live_provider_stream() {
    use astra_services::session_journal::JournalDirGuard;
    use std::sync::Arc;
    use std::time::Duration;

    let journal = tempfile::tempdir().unwrap();
    let _journal_guard = JournalDirGuard::new(journal.path());
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("evidence.txt"),
        "retained tool evidence\n",
    )
    .unwrap();
    std::fs::write(
        workspace.path().join("second.txt"),
        "independent retained evidence\n",
    )
    .unwrap();
    let read_round = |id: &str, path: &str| {
        ProviderResponse::OpenAi(json!({
            "model":"provider-fixture-model",
            "choices":[{"index":0,"message":{"role":"assistant","tool_calls":[{"id":id,"type":"function","function":{"name":"read_file","arguments":json!({"path":path}).to_string()}}]},"finish_reason":"tool_calls"}],
            "usage":{"prompt_tokens":20,"completion_tokens":3,"total_tokens":23}
        }))
    };
    let release = Arc::new(tokio::sync::Notify::new());
    let gateway = ProviderGateway::start(vec![ProviderScript::new(
        "cancel after actual partial output",
        |request| request.path == "/v1/chat/completions" && request.body["stream"] == true,
        vec![read_round("read-first-round", "evidence.txt"), read_round("read-second-round", "second.txt"), ProviderResponse::Stream {
            content_type: "text/event-stream",
            chunks: vec![
                format!("data: {}\n\n", json!({"choices":[{"index":0,"delta":{"content":"partial evidence"}}]})).into_bytes(),
                format!("data: {}\n\n", json!({"choices":[{"index":0,"delta":{"content":"must not be delivered"},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7}})).into_bytes(),
                b"data: [DONE]\n\n".to_vec(),
            ],
            release_before_chunk: Some((1, release.clone())),
        }],
    )]).await;
    let ledger = InferenceLedgerFixture::default();
    let session = format!("cancel-fixture-{}", uuid::Uuid::new_v4());
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
    let token = Arc::new(tokio_util::sync::CancellationToken::new());
    host.set_client_cancel(token.clone());
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    host.set_event_tx(tx);
    let mut state = loop_state(&session, Vec::new(), "Explain the available evidence.");
    bind_server_workspace(&mut state, workspace.path()).await;
    state.skills.request_constraints.allowed_tools =
        Some(["read_file".to_owned()].into_iter().collect());
    state.cancellation.token = Some(token.clone());
    let cancel_after_output = async {
        let mut observed = Vec::new();
        loop {
            let event = rx
                .recv()
                .await
                .expect("stream remains live until cancellation");
            if event["type"] == "text_delta" {
                assert_eq!(event["content"], "partial evidence");
                observed.push(event);
                token.cancel();
                break observed;
            }
        }
    };
    let (outcome, mut observed) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(
            run_agentic_loop_with_host(&mut host, &mut state),
            cancel_after_output
        )
    })
    .await
    .expect("cancellation must settle without releasing provider completion");
    let failure = outcome.unwrap_err();
    assert_eq!(failure.kind, astra_core::ErrorKind::Cancelled);
    let partial: Value = serde_json::from_str(
        failure
            .details_json
            .as_deref()
            .expect("delivered partial facts"),
    )
    .unwrap();
    assert_eq!(partial["partial_full_text"], "partial evidence");
    while let Ok(event) = rx.try_recv() {
        observed.push(event);
    }
    let visible: String = observed
        .iter()
        .filter(|event| event["type"] == "text_delta")
        .map(|event| event["content"].as_str().unwrap())
        .collect();
    assert_eq!(visible, "partial evidence");
    assert_eq!(
        state.cancellation.resolved_origin,
        Some(astra_turn_core::orchestration_types::CancellationOrigin::Runtime)
    );
    assert_eq!(state.total_tool_calls, 2);
    let records: Vec<_> = state
        .stall
        .tool_call_records
        .iter()
        .filter(|record| record.was_executed())
        .collect();
    assert_eq!(records.len(), 2);
    for (round, record) in records.iter().enumerate() {
        assert_eq!(record.round, Some(round as u32));
        assert!(record.start_offset_ms.is_some());
    }
    assert!(records[1].start_offset_ms.unwrap() >= records[0].start_offset_ms.unwrap());
    assert_eq!((state.total_prompt, state.total_completion), (40, 6));
    assert_eq!(ledger.attempt_count(), 3);
    assert!(
        ledger.wait_for_settlements(Duration::from_secs(5)).await,
        "detached settlement owner must finish"
    );
    ledger.assert_quiescent();
    gateway.assert_complete();
    let requests = gateway.requests.lock().await;
    assert_eq!(requests.len(), 3);
    let messages = requests[2].body["messages"].as_array().unwrap();
    for (id, expected) in [
        ("read-first-round", "retained tool evidence"),
        ("read-second-round", "independent retained evidence"),
    ] {
        let results: Vec<_> = messages
            .iter()
            .filter(|message| message["role"] == "tool" && message["tool_call_id"] == id)
            .collect();
        assert_eq!(results.len(), 1);
        assert!(results[0]["content"].as_str().unwrap().contains(expected));
    }
    let events =
        astra_services::session_journal::read_journal_for_user("provider-fixture-user", &session)
            .unwrap();
    let rounds: Vec<_> = events
        .iter()
        .filter(|event| {
            event.event_type == astra_services::session_journal::JournalEventType::LlmRound
        })
        .collect();
    assert_eq!(
        rounds.iter().map(|event| event.round).collect::<Vec<_>>(),
        vec![Some(0), Some(1)]
    );
    for round in rounds {
        assert_eq!(
            round.metadata.as_ref().expect("partial round metadata")["partial"],
            true
        );
    }
    release.notify_one();
}
