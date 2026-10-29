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
    )
    .await
}
async fn infer_with_budget(
    execution: &astra_services::AdmittedModelExecution,
    messages: &[Value],
    tools: &[Value],
    attempts: &Attempts,
    budget: Duration,
) -> Result<LlmCallResult, astra_core::ClassifiedError> {
    let projected = consolidate_system_messages_for_provider(messages, "chatgpt", None);
    call_llm_and_collect_with_stream_callback(
        LlmCall {
            purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
            messages: &projected,
            tools,
            cache_capability: None,
            route: LlmExecutionRoute::from_admitted(execution),
            max_output_tokens: Some(1024),
            temperature: Some(0.7),
            thinking: &ThinkingConfig::ModelDefault,
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
    let replica = DatabaseModelService::new(settings, key)
        .with_pool(pool)
        .with_chatgpt_config(config);
    let owner = format!("runtime-{}", uuid::Uuid::new_v4());
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
    provider.state.unterminated.store(true, Ordering::SeqCst);
    let attempts = Attempts::default();
    let first = infer(&admitted, &messages, &tools, &attempts)
        .await
        .unwrap();
    assert_eq!(first.tool_calls[0]["id"], "call-1");
    assert_eq!(first.usage["prompt_tokens"], 20);
    let mut calls = first.tool_calls;
    calls[0]["id"] = json!("internal-call");
    calls[0][astra_turn_core::tool::args::shape::PROVIDER_TOOL_CALL_ID_FIELD] = json!("call-1");
    messages.push(json!({"role":"assistant","content":"","tool_calls":calls,astra_core::model_wire::responses::CONTINUATION_KEY:first.provider_continuation.unwrap()}));
    messages
        .push(json!({"role":"tool","tool_call_id":"internal-call","content":"fixture contents"}));
    let second = infer(&admitted, &messages, &tools, &attempts)
        .await
        .unwrap();
    assert_eq!(second.full_text, "Continued successfully");
    let requests = provider.state.requests.lock().unwrap().clone();
    let replay = requests.last().unwrap()["input"].as_array().unwrap();
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

    provider.state.context_failure.store(true, Ordering::SeqCst);
    let failure = infer(&admitted, &messages, &[], &attempts)
        .await
        .unwrap_err();
    assert_eq!(failure.kind, astra_core::ErrorKind::ContextWindow);
    assert!(!failure.to_string().contains("private upstream prose"));
    assert_eq!(
        provider.state.refresh_calls.load(Ordering::SeqCst),
        refreshes + 1
    );
    provider
        .state
        .context_failure
        .store(false, Ordering::SeqCst);
    provider.state.stream_quota.store(true, Ordering::SeqCst);
    let before = provider.state.responses_calls.load(Ordering::SeqCst);
    let failure = infer(&admitted, &messages, &[], &attempts)
        .await
        .unwrap_err();
    assert_eq!(failure.kind, astra_core::ErrorKind::RateLimit);
    {
        let terminals = attempts.finished.lock().unwrap();
        let terminal = terminals.last().unwrap();
        assert_eq!(terminal.usage.input.total_input_tokens(), 17);
        assert_eq!(terminal.usage.output_tokens, 23);
        assert_eq!(
            terminal.provider_response_id.as_deref(),
            Some("failed-response")
        );
    }
    assert_eq!(
        provider.state.responses_calls.load(Ordering::SeqCst),
        before + 1
    );
    provider.state.stream_quota.store(false, Ordering::SeqCst);

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
    provider.state.quota.store(true, Ordering::SeqCst);
    let before = provider.state.responses_calls.load(Ordering::SeqCst);
    let failure = infer(&admitted, &messages, &tools, &attempts)
        .await
        .unwrap_err();
    assert!(
        failure
            .to_string()
            .contains("subscription_sharing_usage_limit_exceeded")
    );
    assert_eq!(
        provider.state.responses_calls.load(Ordering::SeqCst),
        before + 1,
        "quota must not trigger a retry"
    );
    provider.state.quota.store(false, Ordering::SeqCst);
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
        let reads = binding.fence_reads_for_test();
        tokio::time::sleep(Duration::from_millis(2200)).await;
        assert!(
            binding.fence_reads_for_test() - reads <= 2,
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
