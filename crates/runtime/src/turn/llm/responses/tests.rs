use super::*;
use astra_services::chatgpt::{
    ChatgptConfig, CompleteAuthorization, ConnectionOperation, StartAuthorization,
};
use astra_services::{
    DatabaseModelService, FernetTokenEncryptor, ModelService, UserModelCreateRequestData,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU32, Ordering},
};

#[allow(dead_code)]
mod fixture {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../services/tests/common/chatgpt_fixture.rs"
    ));
}

#[derive(Default)]
struct Attempts {
    next: AtomicU32,
    finished: Mutex<Vec<astra_services::InferenceTerminalStatus>>,
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
        self.finished.lock().unwrap().push(terminal.status);
        Ok(())
    }
}
async fn infer(
    execution: &astra_services::AdmittedModelExecution,
    messages: &[Value],
    tools: &[Value],
    attempts: &Attempts,
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
        Some(Duration::from_secs(12)),
    )
    .await
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1 and an explicitly isolated MatrixOne database; no live provider"]
async fn responses_runtime_tool_checkpoint_resume_quota_and_disconnect() {
    let mut settings = astra_core::MatrixOneSettings::from_env();
    crate::isolated_database::require_isolated_database(&settings.database);
    settings.db_pool_max_connections = 8;
    settings.db_pool_min_connections = 0;
    astra_services::storage::ensure_core_schema(&settings, "mysql")
        .await
        .unwrap();
    let pool = astra_core::SharedPool::new(&settings).await.unwrap();
    let provider = fixture::ProviderFixture::start().await;
    let key = Arc::new(FernetTokenEncryptor::new("responses-runtime-test-key").unwrap());
    let config = ChatgptConfig::for_test(&provider.base);
    let service = DatabaseModelService::new(settings.clone(), key.clone())
        .with_pool(pool.clone())
        .with_chatgpt_config(config.clone());
    let replica = DatabaseModelService::new(settings, key)
        .with_pool(pool)
        .with_chatgpt_config(config);
    let owner = format!("runtime-{}", uuid::Uuid::new_v4());
    let authorization = service
        .model_connection_operation(
            owner.clone(),
            ConnectionOperation::Start(StartAuthorization {
                redirect_uri: "http://127.0.0.1:14555/auth/callback".into(),
                label: Some("Runtime fixture".into()),
                connection_id: None,
                retry_authorization_id: None,
            }),
        )
        .await
        .unwrap();
    let code = provider.issue_code(&authorization, "runtime-account", true);
    let linked = service
        .model_connection_operation(
            owner.clone(),
            ConnectionOperation::Complete(
                authorization["authorization_id"].as_str().unwrap().into(),
                CompleteAuthorization {
                    state: authorization["state"].as_str().unwrap().into(),
                    code: Some(code),
                    client_id: Some("oaiapp_fixture".into()),
                    error: None,
                },
            ),
        )
        .await
        .unwrap();
    let connection = linked["connection"]["connection_id"]
        .as_str()
        .unwrap()
        .to_owned();
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
    let mut recorder =
        astra_pipeline::step_recorder::StepRecorder::new(&owner, "subscription-session", "test");
    recorder.begin_turn(1);
    let checkpoint = astra_turn_core::turn_checkpoint::build_turn_continuity_checkpoint(
        astra_turn_core::turn_checkpoint::TurnCheckpointInput {
            step_recorder: &recorder,
            messages: &messages,
            max_turn_input_tokens: 128000,
            last_measured_prompt_tokens: Some(20),
            remaining_turns: 4,
            restricted_tools: &HashSet::new(),
            recent_tools: &[],
            interruption: None,
            approval_overrides: None,
            consecutive_context_window_errors: 0,
            deferred_tool_activations: &mut Vec::new(),
            pipeline_session: None,
            workspace_observation_quarantine: None,
        },
    )
    .unwrap();
    let saved = serde_json::to_vec(&checkpoint).unwrap();
    let resumed: astra_pipeline::step_protocol::HeavyCheckpoint =
        serde_json::from_slice(&saved).unwrap();
    let second = infer(&admitted, &resumed.messages, &tools, &attempts)
        .await
        .unwrap();
    assert_eq!(second.full_text, "Resumed successfully");
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
        attempts.finished.lock().unwrap().last(),
        Some(&astra_services::InferenceTerminalStatus::DeliveryUnknown)
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
    provider.state.hold_responses.store(true, Ordering::SeqCst);
    let before = provider.state.responses_calls.load(Ordering::SeqCst);
    let disconnect_started = Instant::now();
    let call = infer(&admitted, &messages, &tools, &attempts);
    let disconnect = async {
        tokio::time::timeout(Duration::from_secs(5), async {
            while provider.state.responses_calls.load(Ordering::SeqCst) == before {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        replica
            .model_connection_operation(owner, ConnectionOperation::Disconnect(connection))
            .await
            .unwrap();
    };
    let (failure, ()) = tokio::join!(call, disconnect);
    assert!(
        disconnect_started.elapsed() < Duration::from_secs(5),
        "a different replica must fence the request before its ordinary deadline"
    );
    assert!(failure.is_err());
    assert_eq!(
        attempts.finished.lock().unwrap().len(),
        5,
        "every admitted request must settle"
    );
    assert_eq!(
        attempts.finished.lock().unwrap().last(),
        Some(&astra_services::InferenceTerminalStatus::DeliveryUnknown)
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
