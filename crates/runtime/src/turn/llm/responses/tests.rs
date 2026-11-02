use super::*;
use astra_services::chatgpt::{ChatgptConfig, ConnectionOperation};
use astra_services::{
    DatabaseModelService, FernetTokenEncryptor, ModelService, UserModelCreateRequestData,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU32, Ordering},
};

#[allow(dead_code)]
mod fixture {
    use astra_services as services;
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../services/tests/common/chatgpt_fixture.rs"
    ));
}

#[derive(Default)]
struct Attempts {
    next: AtomicU32,
    finished: Mutex<Vec<astra_services::InferenceInvocationTerminal>>,
    measured_inputs: Mutex<Vec<u64>>,
}
#[async_trait]
impl ProviderAttemptObserver for Attempts {
    async fn begin_attempt(
        &self,
        _: &ProviderWireRequestIdentity,
        _: Option<u32>,
    ) -> Result<ProviderAttemptAdmission, astra_core::ClassifiedError> {
        Ok(ProviderAttemptAdmission::Admitted(
            self.next.fetch_add(1, Ordering::SeqCst),
        ))
    }
    fn note_usage_presence(&self, _: u32, presence: crate::turn::token_usage::TokenUsagePresence) {
        if let Some(total) = presence.measured_input_tokens {
            self.measured_inputs.lock().unwrap().push(total);
        }
    }
    async fn finish_attempt(
        &self,
        _: u32,
        terminal: &astra_services::InferenceInvocationTerminal,
    ) -> Result<(), astra_core::ClassifiedError> {
        self.finished.lock().unwrap().push(terminal.clone());
        Ok(())
    }
}
async fn infer(
    execution: &astra_services::AdmittedModelExecution,
    messages: &[Value],
    tools: &[Value],
    attempts: &Attempts,
) -> Result<LlmCallResult, astra_core::ClassifiedError> {
    infer_with_budget(
        execution,
        messages,
        tools,
        attempts,
        Duration::from_secs(12),
        &ThinkingConfig::ModelDefault,
    )
    .await
}
async fn infer_with_budget(
    execution: &astra_services::AdmittedModelExecution,
    messages: &[Value],
    tools: &[Value],
    attempts: &Attempts,
    budget: Duration,
    thinking: &ThinkingConfig,
) -> Result<LlmCallResult, astra_core::ClassifiedError> {
    call_llm_and_collect_with_stream_callback(
        LlmCall {
            purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
            messages,
            tools,
            cache_capability: None,
            route: LlmExecutionRoute::from_admitted(execution),
            max_output_tokens: Some(1024),
            temperature: Some(0.7),
            thinking,
        },
        LlmCancel::None,
        None,
        Some(attempts),
        RuntimeToolChoice::Auto,
        Some(budget),
    )
    .await
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1 and an explicitly isolated MatrixOne database; no live provider"]
async fn responses_transport_recovery_and_credential_fencing() {
    let mut settings = astra_core::MatrixOneSettings::from_env();
    crate::isolated_database::require_isolated_database(&settings.database);
    settings.db_pool_max_connections = 8;
    settings.db_pool_min_connections = 0;
    astra_services::storage::ensure_core_schema(&settings, "mysql")
        .await
        .unwrap();
    let pool = astra_core::SharedPool::new(&settings).await.unwrap();
    let mut provider = fixture::ProviderFixture::start().await;
    let key = Arc::new(FernetTokenEncryptor::new("responses-runtime-test-key").unwrap());
    let config = ChatgptConfig::for_test(&provider.base);
    let service = DatabaseModelService::new(settings.clone(), key.clone())
        .with_pool(pool.clone())
        .with_chatgpt_config(config.clone());
    let replica = DatabaseModelService::new(settings.clone(), key.clone())
        .with_pool(pool.clone())
        .with_chatgpt_config(config);
    let owner = format!("runtime-{}", uuid::Uuid::new_v4());
    fixture::ensure_owner(pool.get(), &owner).await;
    let connection = provider.connect(&service, &owner).await;
    let model = service
        .create_user_model(
            owner.clone(),
            UserModelCreateRequestData {
                name: "coding".into(),
                provider: "chatgpt".into(),
                model: "fixture-model".into(),
                base_url: None,
                api_key: String::new(),
                connection_id: Some(connection.clone()),
                context_window: 128000,
                is_default: true,
            },
        )
        .await
        .unwrap();
    let admitted = service
        .admit_model_offering(owner.clone(), model.model_id)
        .await
        .unwrap();
    let tools = vec![
        json!({"type":"function","function":{"name":"read_file","parameters":{"type":"object","properties":{}}}}),
    ];
    let mut messages = vec![
        json!({"role":"system","content":"Use the admitted tool."}),
        json!({"role":"user","content":"Read the fixture"}),
    ];
    // A terminal budget hit may contain a function item with valid JSON, but
    // neither that item nor its private protocol state can authorize execution.
    provider.state.output_limit.store(true, Ordering::SeqCst);
    let limited_attempts = Attempts::default();
    let partial = infer(&admitted, &messages, &tools, &limited_attempts)
        .await
        .unwrap();
    assert_eq!(partial.finish_reason.as_deref(), Some("length"));
    assert!(partial.tool_calls.is_empty());
    assert!(
        astra_turn_core::xml_tool_call_fallback::parse_degraded_tool_calls(&partial.full_text)
            .is_some()
    );
    assert!(partial.provider_continuation.is_none());
    assert_eq!(partial.usage["output_tokens"], 5);
    assert_eq!(limited_attempts.finished.lock().unwrap().len(), 1);
    // Run the same typed terminal through the real Server host and tool
    // executor. The provider's one continuation also exhausts its output cap.
    use crate::server::provider_test_support::{
        ProviderObservationsFixture, bind_server_workspace, loop_state,
    };
    use crate::server::tool_transport::{
        ExecutionBindingSnapshot, ExecutorBinding, WorkspaceBinding,
    };
    use crate::turn::agentic_loop::finalization::run_agentic_loop_with_host;
    let workspace = tempfile::TempDir::new().unwrap();
    let session = format!("capped-{}", uuid::Uuid::new_v4());
    let ledger = ProviderObservationsFixture::default();
    let mut host = crate::server::server_loop_host::ServerAgenticLoopHostBuilder::new(
        settings,
        key,
        owner.clone(),
        session.clone(),
    )
    .with_provider_observations(ledger.clone())
    .with_model_service(Some(Arc::new(service.clone())))
    .with_admitted_model_execution(Some(admitted.clone()))
    .with_turn_intent_policy(astra_services::runs::TurnIntentExecutionPolicy::FixedDefault)
    .with_skill_auto_route_policy(astra_services::runs::SkillAutoRouteExecutionPolicy::Disabled)
    .with_static_tool_catalog_admissible(true)
    .with_execution_binding_snapshot(ExecutionBindingSnapshot::inferred(
        WorkspaceBinding::server_sandbox(workspace.path()),
        ExecutorBinding::server_local(),
    ))
    .build();
    let mut state = loop_state(&session, Vec::new(), "Write the requested file.");
    state.thinking = ThinkingConfig::ModelDefault;
    state.owner_user_id = Some(owner.clone());
    bind_server_workspace(&mut state, workspace.path()).await;
    state.skills.request_constraints.allowed_tools =
        Some(["write_file".to_owned()].into_iter().collect());
    provider.state.scripted.store(true, Ordering::SeqCst);
    *provider.state.script.lock().unwrap() = vec![
        (
            "write_file".into(),
            json!({"path":"side-effects.txt","content":"executed"})
        );
        2
    ];
    provider.state.requests.lock().unwrap().clear();
    run_agentic_loop_with_host(&mut host, &mut state)
        .await
        .unwrap();
    let interruption = state.interruption.as_ref().expect("exhausted output cap");
    assert_eq!(
        interruption.kind,
        astra_turn_core::interruption::InterruptionKind::ExecutionIncomplete
    );
    assert_eq!(
        interruption.resume_action,
        astra_turn_core::interruption::ResumeAction::ContinueImmediately
    );
    assert_eq!(state.total_tool_calls, 0);
    assert!(state.stall.tool_call_records.is_empty());
    assert!(!workspace.path().join("side-effects.txt").exists());
    assert!(state.pending_provider_continuation.is_none());
    assert!(state.messages.iter().all(|message| {
        message.get("tool_calls").is_none()
            && message
                .get(astra_core::model_wire::responses::CONTINUATION_KEY)
                .is_none()
    }));
    let requests = provider.state.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    assert!(requests[0]["tools"].to_string().contains("write_file"));
    assert!(requests[1]["input"].as_array().unwrap().iter().all(|item| {
        !matches!(
            item["type"].as_str(),
            Some("function_call" | "function_call_output" | "reasoning")
        )
    }));
    assert!(provider.state.script.lock().unwrap().is_empty());
    assert_eq!(ledger.attempt_count(), 2);
    ledger.assert_quiescent();
    provider.state.scripted.store(false, Ordering::SeqCst);
    provider.state.output_limit.store(false, Ordering::SeqCst);
    provider.state.requests.lock().unwrap().clear();
    provider.state.unterminated.store(true, Ordering::SeqCst);
    let attempts = Attempts::default();
    let first = infer(&admitted, &messages, &tools, &attempts)
        .await
        .unwrap();
    assert_eq!(first.tool_calls[0]["id"], "call-1");
    assert_eq!(first.usage_presence.measured_input_tokens, Some(20));
    let mut calls = first.tool_calls;
    calls[0]["id"] = json!("internal-call");
    calls[0][astra_turn_core::tool::args::shape::PROVIDER_TOOL_CALL_ID_FIELD] = json!("call-1");
    messages.push(json!({"role":"assistant","content":"","tool_calls":calls,astra_core::model_wire::responses::CONTINUATION_KEY:first.provider_continuation.unwrap()}));
    messages
        .push(json!({"role":"tool","tool_call_id":"internal-call","content":"fixture contents"}));
    messages.push(
        crate::turn::wire_assembly::runtime_system_context_message(
            "FRESH_CHILD_FACT_transport",
            true,
        )
        .unwrap(),
    );
    let second = infer(&admitted, &messages, &tools, &attempts)
        .await
        .unwrap();
    assert_eq!(second.full_text, "Continued successfully");
    let requests = provider.state.requests.lock().unwrap().clone();
    let replay = requests.last().unwrap()["input"].as_array().unwrap();
    let result_index = replay
        .iter()
        .position(|item| item["type"] == "function_call_output")
        .unwrap();
    assert_eq!(replay[result_index + 1]["role"], "user");
    assert!(
        replay[result_index + 1]
            .to_string()
            .contains("FRESH_CHILD_FACT_transport")
    );
    assert!(!replay[0].to_string().contains("FRESH_CHILD_FACT_transport"));
    assert!(
        replay
            .iter()
            .any(|item| item["encrypted_content"] == "sk-fixtureciphertextciphertextciphertext")
    );
    assert!(
        replay
            .iter()
            .any(|item| item["type"] == "function_call" && item["call_id"] == "call-1")
    );
    assert!(
        replay
            .iter()
            .any(|item| item["type"] == "function_call_output" && item["call_id"] == "call-1")
    );
    assert_eq!(attempts.next.load(Ordering::SeqCst), 2);
    assert_eq!(attempts.finished.lock().unwrap().len(), 2);

    // Both requests are sent with one token. Delay the second 401 until the
    // first request has rotated and successfully used the replacement token.
    let stale = admitted
        .subscription
        .as_ref()
        .unwrap()
        .acquire(None)
        .await
        .unwrap();
    provider.reject_access_token(stale.bearer(), true);
    drop(stale);
    let refreshes = provider.state.refresh_calls.load(Ordering::SeqCst);
    let (left, right) = tokio::join!(
        infer(&admitted, &messages, &[], &attempts),
        infer(&admitted, &messages, &[], &attempts)
    );
    assert!(left.is_ok() && right.is_ok(), "{left:?} {right:?}");
    assert_eq!(provider.state.stale_requests.load(Ordering::SeqCst), 2);
    assert_eq!(
        provider.state.refresh_calls.load(Ordering::SeqCst),
        refreshes + 1,
        "late old-token 401 must reuse the winning credential version"
    );
    *provider.state.reject_token.lock().unwrap() = None;

    // Both recovery and retry exhaustion preserve their physical-attempt counts.
    {
        let _backoff = super::super::set_test_retry_backoff_ms(1);
        for failures in [1, LLM_MAX_RETRIES as usize + 1] {
            let before = provider.state.responses_calls.load(Ordering::SeqCst);
            provider
                .state
                .transient_responses
                .store(failures, Ordering::SeqCst);
            let result = infer(&admitted, &messages, &[], &attempts).await;
            assert_eq!(
                provider.state.responses_calls.load(Ordering::SeqCst),
                before + failures + usize::from(failures == 1)
            );
            if failures == 1 {
                assert!(result.is_ok());
            } else {
                let failure = result.unwrap_err();
                assert_eq!(failure.kind, astra_core::ErrorKind::ServerError);
                assert!(
                    failure
                        .message
                        .contains("subscription_sharing_usage_unavailable")
                );
                assert!(!failure.message.contains("private upstream prose"));
            }
        }
    }

    for (flag, kind, code, has_usage) in [
        (
            &provider.state.context_failure,
            astra_core::ErrorKind::ContextWindow,
            "context_length_exceeded",
            false,
        ),
        (
            &provider.state.quota,
            astra_core::ErrorKind::RateLimit,
            "subscription_sharing_usage_limit_exceeded",
            false,
        ),
        (
            &provider.state.stream_quota,
            astra_core::ErrorKind::RateLimit,
            "subscription_sharing_usage_limit_exceeded",
            true,
        ),
    ] {
        flag.store(true, Ordering::SeqCst);
        let before = provider.state.responses_calls.load(Ordering::SeqCst);
        let failure = infer(&admitted, &messages, &[], &attempts)
            .await
            .unwrap_err();
        assert_eq!(failure.kind, kind);
        assert!(failure.message.contains(code));
        assert!(!failure.message.contains("private upstream prose"));
        assert_eq!(
            provider.state.responses_calls.load(Ordering::SeqCst),
            before + 1,
            "terminal provider errors must not retry"
        );
        assert_eq!(
            provider.state.refresh_calls.load(Ordering::SeqCst),
            refreshes + 1
        );
        if has_usage {
            let terminals = attempts.finished.lock().unwrap();
            let terminal = terminals.last().unwrap();
            assert_eq!(
                attempts.measured_inputs.lock().unwrap().last().copied(),
                Some(17)
            );
            assert_eq!(terminal.usage.output_tokens, 23);
            assert_eq!(
                terminal.provider_response_id.as_deref(),
                Some("failed-response")
            );
        }
        flag.store(false, Ordering::SeqCst);
    }

    for effort in ["low", "medium", "high", "xhigh", "max"] {
        let thinking: ThinkingConfig =
            serde_json::from_value(json!({"mode":"adaptive","effort":effort})).unwrap();
        let result = infer_with_budget(
            &admitted,
            &messages,
            &[],
            &attempts,
            Duration::from_secs(12),
            &thinking,
        )
        .await
        .unwrap();
        assert_eq!(result.full_text, "Continued successfully");
        let requests = provider.state.requests.lock().unwrap();
        assert_eq!(
            requests.last().unwrap()["reasoning"],
            json!({"effort":effort})
        );
        assert!(requests.last().unwrap().get("reasoning_effort").is_none());
    }
    let mut restricted = admitted.clone();
    restricted.supported_reasoning_efforts = Some(vec!["high".into()]);
    let before = provider.state.responses_calls.load(Ordering::SeqCst);
    let low: ThinkingConfig =
        serde_json::from_value(json!({"mode":"adaptive","effort":"low"})).unwrap();
    assert!(
        infer_with_budget(
            &restricted,
            &messages,
            &[],
            &attempts,
            Duration::from_secs(12),
            &low
        )
        .await
        .is_err()
    );
    assert_eq!(
        provider.state.responses_calls.load(Ordering::SeqCst),
        before,
        "unsupported control must fail before HTTP dispatch"
    );
    assert!(
        provider.state.requests.lock().unwrap()[0]
            .get("reasoning")
            .is_none()
    );

    provider.state.truncate.store(true, Ordering::SeqCst);
    let before = provider.state.responses_calls.load(Ordering::SeqCst);
    assert!(
        infer(&admitted, &messages, &tools, &attempts)
            .await
            .is_err()
    );
    assert_eq!(
        provider.state.responses_calls.load(Ordering::SeqCst),
        before + 1,
        "partial output must not be retried"
    );
    assert_eq!(
        attempts
            .finished
            .lock()
            .unwrap()
            .last()
            .map(|terminal| terminal.status),
        Some(astra_services::InferenceTerminalStatus::DeliveryUnknown)
    );
    provider.state.truncate.store(false, Ordering::SeqCst);
    // A refused connection schedules retry backoff. The original invocation
    // deadline must expire during that sleep, before another attempt begins.
    provider.stop().await;
    let before_attempts = attempts.next.load(Ordering::SeqCst);
    let started = Instant::now();
    {
        let _backoff = super::super::set_test_retry_backoff_ms(1000);
        let failure = infer_with_budget(
            &admitted,
            &messages,
            &tools,
            &attempts,
            Duration::from_millis(100),
            &ThinkingConfig::ModelDefault,
        )
        .await
        .unwrap_err();
        assert_eq!(failure.kind, astra_core::ErrorKind::ProviderDeadline);
    }
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "retry sleep escaped the invocation budget"
    );
    assert_eq!(attempts.next.load(Ordering::SeqCst), before_attempts + 1);
    provider.restart().await;
    provider.state.hold_responses.store(true, Ordering::SeqCst);
    let before = provider.state.responses_calls.load(Ordering::SeqCst);
    let disconnect_started = Instant::now();
    let calls = futures_util::future::join_all(
        (0..16).map(|_| infer(&admitted, &messages, &tools, &attempts)),
    );
    let disconnect = async {
        tokio::time::timeout(Duration::from_secs(5), async {
            while provider.state.responses_calls.load(Ordering::SeqCst) < before + 16 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let binding = admitted.subscription.as_ref().unwrap();
        let reads = binding.connection_read_queries_for_test();
        tokio::time::sleep(Duration::from_millis(2200)).await;
        assert!(
            (1..=2).contains(&(binding.connection_read_queries_for_test() - reads)),
            "active requests must share one connection watcher"
        );
        replica
            .model_connection_operation(owner, ConnectionOperation::Disconnect(connection))
            .await
            .unwrap();
    };
    let (failures, ()) = tokio::join!(calls, disconnect);
    assert!(
        disconnect_started.elapsed() < Duration::from_secs(8),
        "a different replica must fence the request before its ordinary deadline"
    );
    assert!(failures.iter().all(Result::is_err));
    assert_eq!(
        attempts.finished.lock().unwrap().len(),
        attempts.next.load(Ordering::SeqCst) as usize,
        "every admitted request must settle"
    );
    assert_eq!(
        attempts
            .finished
            .lock()
            .unwrap()
            .last()
            .map(|terminal| terminal.status),
        Some(astra_services::InferenceTerminalStatus::DeliveryUnknown)
    );
}

#[test]
fn responses_and_openai_share_runtime_role_projection() {
    use crate::turn::wire_assembly::{
        RuntimeAuthorityKind, required_runtime_preamble_message, runtime_system_context_message,
    };
    use astra_core::model_wire::responses::{CONTINUATION_KEY, ResponseOutput, request};
    use astra_turn_core::tool::args::shape::PROVIDER_TOOL_CALL_ID_FIELD;
    let connection = "00000000-0000-0000-0000-000000000001";
    let continuation = ResponseOutput {
        items: vec![
            json!({"type":"reasoning","encrypted_content":"opaque","summary":[]}),
            json!({"type":"message","role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"Working"}]}),
            json!({"type":"function_call","name":"read_file","namespace":"astra","call_id":"provider-call"}),
        ],
        ..Default::default()
    }.continuation(connection, "fixture-model");
    let mut prefix = None;
    for fact in ["FRESH_CHILD_FACT_first", "FRESH_CHILD_FACT_changed"] {
        let messages = vec![
            json!({"role":"system","content":[{"type":"text","text":"stable policy"}]}),
            json!({"role":"user","content":"Finish the current task"}),
            json!({"role":"assistant","content":"Working","phase":"commentary","tool_calls":[{
                "id":"internal-call",PROVIDER_TOOL_CALL_ID_FIELD:"provider-call","type":"function",
                "function":{"name":"read_file","arguments":"{}"}
            }],CONTINUATION_KEY:continuation}),
            json!({"role":"tool","tool_call_id":"internal-call","content":"still_running"}),
            runtime_system_context_message(fact, true).unwrap(),
            required_runtime_preamble_message(
                &json!({"instruction":"Continue the assigned item","work_item_id":fact})
                    .to_string(),
                RuntimeAuthorityKind::ActiveWorkAttemptStart,
                astra_turn_types::RuntimeAuthorityLifetime::CurrentUserTurn,
            )
            .unwrap(),
        ];
        let openai = consolidate_system_messages_for_provider(&messages, "openai", None);
        let responses = consolidate_system_messages_for_provider(&messages, "chatgpt", None);
        let mut comparable = responses.clone();
        comparable[2]
            .as_object_mut()
            .unwrap()
            .remove(CONTINUATION_KEY);
        assert_eq!(comparable, openai, "runtime authority and order must match");
        assert_eq!(responses[2][CONTINUATION_KEY], continuation);
        assert_eq!(responses[2]["tool_calls"][0]["id"], "provider-call");
        assert_eq!(responses[3]["tool_call_id"], "provider-call");
        assert_eq!(responses[4]["role"], "user");
        assert_eq!(responses[5]["role"], "user");
        assert!(responses[5].to_string().contains("boundary_instruction"));
        assert!(
            responses[5]
                .to_string()
                .contains("Continue the assigned item")
        );
        assert!(!responses[0].to_string().contains(fact));
        if let Some(prefix) = &prefix {
            assert_eq!(&responses[0], prefix);
        }
        prefix = Some(responses[0].clone());
        assert_eq!(
            consolidate_system_messages_for_provider(&responses, "chatgpt", None),
            responses
        );

        let body = request("fixture-model", &responses, None, connection, false, None).unwrap();
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[0]["role"], "developer");
        assert!(!input[0].to_string().contains(fact));
        assert_eq!(input[2]["encrypted_content"], "opaque");
        assert_eq!(input[3]["phase"], "commentary");
        assert_eq!(input[4]["type"], "function_call");
        assert_eq!(input[4]["call_id"], "provider-call");
        assert_eq!(input[4]["arguments"], "{}");
        assert_eq!(input[5]["type"], "function_call_output");
        assert_eq!(input[5]["call_id"], "provider-call");
        assert_eq!(input[5]["output"], "still_running");
        assert_eq!(input[6]["role"], "user");
        assert!(input[6].to_string().contains(fact));
        assert_eq!(input[7]["role"], "user");
        assert!(input[7].to_string().contains("boundary_instruction"));
        assert!(!body.to_string().contains(CONTINUATION_KEY));
    }
}

#[test]
fn responses_continuation_is_private_to_its_protocol() {
    let mut messages = vec![
        json!({"role":"assistant","content":"visible","_astra_responses":{"items":[{"encrypted_content":"private"}]}}),
    ];
    strip_internal_runtime_markers(&mut messages);
    assert!(
        messages[0]
            .get(astra_core::model_wire::responses::CONTINUATION_KEY)
            .is_none()
    );
    assert_eq!(messages[0]["content"], "visible");
}

#[test]
fn responses_wire_output_limit_reads_the_subscription_budget_key() {
    // The Responses protocol cap is `max_output_tokens`; Chat Completions and
    // Bedrock keys keep their existing precedence.
    assert_eq!(
        super::super::provider_request_output_limit(&json!({"max_output_tokens": 512})),
        Some(512)
    );
    assert_eq!(
        super::super::provider_request_output_limit(
            &json!({"max_completion_tokens": 128, "max_output_tokens": 512})
        ),
        Some(128)
    );
    assert_eq!(
        super::super::provider_request_output_limit(&json!({})),
        None
    );
}

#[test]
fn responses_finish_reason_comes_from_provider_truncation_evidence() {
    use astra_core::model_wire::responses::ResponseOutput;
    // A provider-declared max_output_tokens truncation is terminal `length`.
    let truncated = super::result(
        ResponseOutput {
            text: "partial".into(),
            finish_reason: Some("length".into()),
            ..Default::default()
        },
        "conn",
        "model",
        "display-model",
    );
    assert_eq!(truncated.finish_reason.as_deref(), Some("length"));
    assert_eq!(truncated.full_text, "partial");
    // Without truncation evidence the stop/tool_calls distinction is kept.
    let stopped = super::result(ResponseOutput::default(), "conn", "model", "display-model");
    assert_eq!(stopped.finish_reason.as_deref(), Some("stop"));
    let calls = super::result(
        ResponseOutput {
            tool_calls: vec![
                json!({"id":"c","type":"function","function":{"name":"read_file","arguments":"{}"}}),
            ],
            ..Default::default()
        },
        "conn",
        "model",
        "display-model",
    );
    assert_eq!(calls.finish_reason.as_deref(), Some("tool_calls"));
}

#[test]
fn truncated_multi_item_text_reaches_runtime_without_provider_replay() {
    let mut accumulator = ResponseAccumulator::default();
    for event in [
        json!({"type":"response.output_item.done","output_index":0,"item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"prefix "}]}}),
        json!({"type":"response.output_text.delta","output_index":1,"content_index":0,"delta":"partial tail"}),
        json!({"type":"response.incomplete","response":{"id":"partial","status":"incomplete","output":[],"incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":12,"output_tokens":34}}}),
    ] {
        accumulator.observe(&event).unwrap();
    }
    let value = result(
        accumulator.finish().unwrap(),
        "connection",
        "wire",
        "display",
    );
    assert_eq!(value.full_text, "prefix partial tail");
    assert_eq!(value.finish_reason.as_deref(), Some("length"));
    assert_eq!(value.usage["output_tokens"], 34);
    assert!(value.tool_calls.is_empty());
    assert!(value.provider_continuation.is_none());
}

#[test]
fn response_result_couples_normalized_values_and_presence() {
    let output = ResponseOutput {
        usage: json!({"prompt_tokens":100,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":40,"cache_write_tokens":30}}).as_object().unwrap().clone(),
        ..Default::default()
    };
    let actual = result(output, "connection", "wire-model", "display-model");
    assert_eq!(actual.usage["input_tokens"], 30);
    assert_eq!(actual.usage["cached_input_tokens"], 40);
    assert_eq!(actual.usage["cache_creation_tokens"], 30);
    assert!(
        actual.usage_presence.fresh_input_tokens && actual.usage_presence.cache_creation_tokens
    );
}
