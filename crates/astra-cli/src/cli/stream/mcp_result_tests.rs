//! Real stdio MCP -> CLI callback -> server pipeline -> model/journal regressions.

use super::*;
use astra_runtime::turn::agentic::headless_round::{
    HeadlessToolRoundCtx, NoopHeadlessTerminal, run_agentic_headless_tool_round,
};
use astra_services::session_journal::{ToolCallDisposition, ToolCallRecord};
use astra_thin_client::ToolResultRequest;
use astra_turn_core::sse_stream_host::SseStreamHost;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::RwLock;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn callback_through_pipeline(
    fixture_tool: &str,
    fail_reconnect: bool,
) -> (ToolResultRequest, Value, ToolCallRecord) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/tools/result"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .expect(1)
        .mount(&server)
        .await;
    let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let counter = workspace.path().join("applied.txt");
    let tool = format!("mcp__result_path__{fixture_tool}");
    let lost_ack = fixture_tool == "apply_then_drop_ack";
    let args = if lost_ack {
        json!({"path": counter})
    } else {
        json!({})
    };
    let mut manager = crate::mcp_client::McpClientManager::new();
    manager
        .connect(crate::mcp_client::McpServerConfig {
            name: "result_path".into(),
            transport: crate::mcp_client::Transport::Stdio {
                command: vec![
                    crate::mcp_client::ensure_mock_mcp_server_binary()
                        .to_string_lossy()
                        .into_owned(),
                ],
                args: if fail_reconnect {
                    vec![
                        "--exit-if-file-exists".into(),
                        counter.to_string_lossy().into_owned(),
                    ]
                } else {
                    vec![]
                },
                env: Default::default(),
            },
            description: String::new(),
            enabled: true,
            retry: crate::mcp_client::RetryConfig {
                max_retries: 0,
                ..Default::default()
            },
        })
        .await
        .expect("connect real stdio fixture");
    let original_connection = manager.get("result_path").unwrap();
    let schemas = manager.all_tool_schemas();
    let manager = Arc::new(RwLock::new(manager));
    let mut executor = crate::edge_tools::ToolExecutor::new(workspace.path());
    executor.install_mcp_bundle(Arc::clone(&manager), schemas);
    let executor = Arc::new(executor);
    let mut tool_cache = EdgeToolCache::new(8);
    let mut host = CliSseStreamHost::from_edge_ctx(
        EdgeSseContext {
            api: &api,
            token: "tok",
            executor_id: "edge-test",
            executor: Arc::clone(&executor),
            render_policy: RenderPolicy::Silent,
            perm_manager: None,
            cancel_token: None,
            stream_event_tx: None,
            stream_event_sink: None,
            approval_request_tx: None,
            ask_user_request_tx: None,
            skill_resolver: None,
            skill_continuation: false,
            turn_rollback_on_failure: false,
            tool_cache: &mut tool_cache,
            observability_hub: None,
            incremental_state: None,
            request_session_execution_lease: None,
        },
        80,
        false,
    );
    host.on_server_tool_surface_admission(&tool).unwrap();
    let results = host
        .execute_tools_batch(vec![ToolBatchRequest {
            session_id: "mcp-session".into(),
            run_id: "mcp-run".into(),
            turn_chain_id: "mcp-chain".into(),
            request_id: format!("{fixture_tool}-{fail_reconnect}"),
            tool: tool.clone(),
            args: args.clone(),
            execution_timeout_ms: 30_000,
            execution_deadline_unix_ms: u64::MAX,
        }])
        .await;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].status, "failed", "{:?}", results[0]);
    if lost_ack {
        assert_eq!(std::fs::read_to_string(&counter).unwrap(), "applied\n");
    }
    let current_connection = manager.read().await.get("result_path");
    if fail_reconnect {
        assert!(
            current_connection.is_none(),
            "fixture must fail reconnection"
        );
        let binding = executor.runtime_environment_binding_for_tool(
            &tool,
            &astra_runtime_env::ToolRegistry::builtins(),
        );
        assert!(
            astra_runtime_env::CapabilityResolver
                .check_tool_call_for_surface(
                    &astra_runtime_env::ToolRegistry::builtins(),
                    &tool,
                    &args,
                    &binding.capabilities,
                    &binding.tool_surface,
                )
                .is_err(),
            "the removed route must not authorize future calls"
        );
    } else {
        assert_eq!(
            Arc::ptr_eq(&original_connection, &current_connection.unwrap()),
            !lost_ack,
            "only transport uncertainty should reconnect"
        );
    }
    let requests = server.received_requests().await.unwrap();
    let callback: ToolResultRequest = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(callback.session_id, "mcp-session");
    assert_eq!(callback.run_id, "mcp-run");
    assert_eq!(callback.turn_chain_id, "mcp-chain");
    assert_eq!(callback.edge_agent_id, "edge-test");
    assert_eq!(callback.request_id, results[0].request_id);
    assert_eq!(callback.tool_result_fields, results[0].tool_result_fields);
    assert_eq!(
        callback.tool_result_fields.as_ref().unwrap()["runtime_environment_advertisement"]["binding"]
            ["executor"]["kind"],
        "mcp"
    );

    // Use the actual serialized HTTP callback for the server-side result row.
    // The cloud ledger supplies tool/args from the original dispatch identity.
    let edge_results = vec![EdgeToolExecResult {
        execution_completion: None,
        request_id: callback.request_id.clone(),
        tool: tool.clone(),
        args: args.clone(),
        output: callback.output.clone(),
        status: callback.status.clone(),
        duration_ms: callback.duration_ms,
        tool_result_fields: callback.tool_result_fields.clone(),
    }];
    let calls = vec![
        json!({"id": callback.request_id, "type": "function", "function": {
            "name": tool, "arguments": serde_json::to_string(&args).unwrap(),
        }}),
    ];
    let mut messages = Vec::new();
    let mut tool_results = Vec::new();
    let mut records = Vec::new();
    let mut recorder =
        astra_pipeline::step_recorder::StepRecorder::new("test-user", "mcp-session", "test-task");
    recorder.begin_turn(1);
    run_agentic_headless_tool_round(HeadlessToolRoundCtx {
        task_resolution_authority: None,
        turn_index: 0,
        session_turn: 1,
        quiet: true,
        api: &api,
        token: "tok",
        current_user_id: None,
        current_session_id: None,
        current_run_id: Some(&callback.run_id),
        current_turn_chain_id: Some(&callback.turn_chain_id),
        durable_dispatch_admission: None,
        physical_tool_calls: &calls,
        logical_tool_calls: &calls,
        deferred_activations_by_call_id: &HashMap::new(),
        runtime_control_calls_by_id: &HashMap::new(),
        edge_tool_round: &edge_results,
        reasoning_content: "",
        reasoning_signature: "",
        messages: &mut messages,
        tool_results: &mut tool_results,
        valid_tool_names: &HashSet::from([tool]),
        deferred_tool_names: &HashSet::new(),
        restricted_tools: &mut HashSet::new(),
        turn_guard: &mut astra_turn_core::turn_guard::TurnGuard::new(),
        step_recorder: &mut recorder,
        idempotency_cache: &mut astra_pipeline::step_protocol::InMemoryIdempotencyCache::new(),
        semantic_dedup: &mut astra_text_utils::semantic_dedup::SemanticDedup::new(0.95),
        call_counts: &mut HashMap::new(),
        max_identical_calls: 2,
        max_tools_per_turn: 15,
        max_consecutive_empty_name: 3,
        tool_call_records: &mut records,
        tool_event_hooks: &astra_runtime::skills::hooks::ToolEventHookRegistry::default(),
        term: &mut NoopHeadlessTerminal,
        mailbox: None,
        permission_context: None,
        progress_emitter: None,
        pre_resolved_results: &[],
        runtime_tool_executor: None,
        external_effect_recovery_paths: None,
        turn_start: None,
        llm_round: 0,
        plan_mode_active: false,
    })
    .await;
    assert_eq!(records.len(), 1);
    let record = records.pop().unwrap();
    assert_eq!(
        record.effective_disposition(),
        ToolCallDisposition::Executed
    );
    assert!(!record.ok);
    assert_eq!(
        record.tool_call_id.as_deref(),
        Some(callback.request_id.as_str())
    );
    // Check the durable journal representation, not just runtime-only fields.
    let record: ToolCallRecord =
        serde_json::from_value(serde_json::to_value(record).unwrap()).unwrap();
    let mut message = messages.into_iter().find(|m| m["role"] == "tool").unwrap();
    astra_turn_core::tool::result::advisory::project_advisories(&mut message);
    (callback, message, record)
}

#[tokio::test]
async fn mcp_lost_ack_reconnect_outcomes_preserve_uncertainty_through_callback_and_pipeline() {
    for fail_reconnect in [false, true] {
        let (callback, message, record) =
            callback_through_pipeline("apply_then_drop_ack", fail_reconnect).await;
        let fields = callback.tool_result_fields.unwrap();
        assert_eq!(fields["error_kind"], "tool_outcome_unknown");
        assert_eq!(fields["dispatch_certainty"], "unknown");
        assert_eq!(fields["side_effects_maybe"], true);
        assert_eq!(fields["retryable"], false);
        assert_eq!(
            record.error_kind,
            Some(astra_core::ErrorKind::ToolOutcomeUnknown)
        );
        assert!(
            record
                .result_full
                .as_deref()
                .unwrap()
                .contains("unknown outcome")
        );
        assert!(
            record
                .runtime_advisories
                .join("\n")
                .contains("First reconcile the provider's state")
        );
        let content = message["content"].as_str().unwrap();
        assert!(content.contains("unknown outcome"), "{content}");
        assert!(
            content.contains("First reconcile the provider's state"),
            "{content}"
        );
        assert!(content.contains("Do NOT retry this operation"), "{content}");
        assert!(!content.contains("capability denied"), "{content}");
        assert!(
            !content.contains("retry only with corrected arguments"),
            "{content}"
        );
        assert!(!content.contains("capability-equivalent tool"), "{content}");
    }
}

#[tokio::test]
async fn mcp_acknowledged_rpc_error_preserves_correction_through_callback_and_pipeline() {
    let (callback, message, record) = callback_through_pipeline("reject_parameters", false).await;
    let fields = callback.tool_result_fields.unwrap();
    assert_eq!(fields["error_kind"], "tool_invalid_args");
    assert_eq!(fields["dispatch_certainty"], "dispatched");
    assert_eq!(fields["execution_fact"], "failed");
    assert_eq!(fields["side_effects_maybe"], false);
    assert_eq!(fields["mcp_rpc_error"]["code"], -32602);
    assert_eq!(
        fields["mcp_rpc_error"]["message"],
        "fixture parameter rejection"
    );
    assert_eq!(fields["mcp_rpc_error"]["data"], json!({"field": "message"}));
    assert_eq!(
        record.error_kind,
        Some(astra_core::ErrorKind::ToolInvalidArgs)
    );
    let content = message["content"].as_str().unwrap();
    assert!(content.contains("fixture parameter rejection"), "{content}");
    assert!(!content.contains("unknown outcome"), "{content}");
    assert!(!content.contains("First reconcile"), "{content}");
    assert!(
        content.contains("Correct the named fields and make one new call"),
        "{content}"
    );
}

#[tokio::test]
async fn mcp_is_error_response_remains_tool_failure_through_callback_and_pipeline() {
    let (callback, message, record) = callback_through_pipeline("tool_failure", false).await;
    assert_eq!(callback.status, "failed");
    assert!(callback.output.contains("fixture tool failure"));
    assert_ne!(
        record.error_kind,
        Some(astra_core::ErrorKind::ToolOutcomeUnknown)
    );
    let content = message["content"].as_str().unwrap();
    assert!(content.contains("fixture tool failure"), "{content}");
    assert!(!content.contains("unknown outcome"), "{content}");
    assert!(!content.contains("First reconcile"), "{content}");
}
