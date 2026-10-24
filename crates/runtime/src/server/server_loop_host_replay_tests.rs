// Reuse the admitted Server host and strict loopback provider fixtures.
#[tokio::test]
#[serial_test::serial(auxiliary_llm_capacity_policy_env)]
async fn glm_captured_reasoning_is_exact_on_second_and_third_provider_requests() {
    use astra_core::model_wire::thinking::ThinkingProtocol;
    use astra_services::models::ThinkingCapability;
    use astra_turn_core::edge_ledger::assistant_message_with_tool_calls_and_reasoning;
    let _provider_admission = EnvVarGuard::remove("ASTRA_LLM_PROVIDER_ADMISSION_MODE");
    const FIRST: &str = " first original\n\t";
    const SECOND: &str = "second original ";
    let first_call = json!({"id":"read-one", "type":"function", "function":{"name":"read_file", "arguments":"{\"path\":\"one.txt\"}"}});
    let second_call = json!({"id":"read-two", "type":"function", "function":{"name":"read_file", "arguments":"{\"path\":\"two.txt\"}"}});
    let expected_first = assistant_message_with_tool_calls_and_reasoning(
        std::slice::from_ref(&first_call),
        FIRST,
        false,
    );
    let expected_second = assistant_message_with_tool_calls_and_reasoning(
        std::slice::from_ref(&second_call),
        SECOND,
        false,
    );
    let expected_result_one =
        json!({"role":"tool", "tool_call_id":"read-one", "content":"one fixture result"});
    let expected_result_two =
        json!({"role":"tool", "tool_call_id":"read-two", "content":"two fixture result"});
    let first_for_match = expected_first.clone();
    let first_result_for_match = expected_result_one.clone();
    let second_for_match = expected_second.clone();
    let second_result_for_match = expected_result_two.clone();
    let response = |reasoning: &str, call: Value| {
        ProviderResponse::OpenAi(json!({
            "model":"glm-5.3", "choices":[{"index":0,"message":{"role":"assistant", "content":null,"reasoning_content":reasoning,"tool_calls":[call]},"finish_reason":"tool_calls"}],
            "usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}
        }))
    };
    let provider = ProviderGateway::start(vec![
        ProviderScript::new("first tool round", |request| request.body["model"] == "glm-5.3" && !request.body["messages"].as_array().unwrap().iter().any(|m|m["role"] == "tool"), vec![response(FIRST, first_call)]),
        ProviderScript::new("exact captured first block", move |request| {
            let messages = request.body["messages"].as_array().unwrap();
            request.body["model"] == "glm-5.3" && messages.contains(&first_for_match) && messages.contains(&first_result_for_match)
                && !messages.contains(&second_result_for_match)
        }, vec![response(SECOND, second_call)]),
        ProviderScript::new("exact captured both blocks", {
            let one = expected_first.clone(); let two = second_for_match;
            let result_one = expected_result_one.clone(); let result_two = expected_result_two.clone();
            move |request| {
                let messages = request.body["messages"].as_array().unwrap();
                let rows: Vec<_> = messages.iter().filter(|m|m["role"] == "assistant" || m["role"] == "tool").cloned().collect();
                request.body["model"] == "glm-5.3" && rows == vec![one.clone(),result_one.clone(),two.clone(),result_two.clone()]
            }
        }, vec![ProviderResponse::OpenAi(json!({"model":"glm-5.3", "choices":[{"index":0,"message":{"role":"assistant","content":"finished","reasoning_content":"final"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}))]),
    ]).await;
    let ledger = crate::server::provider_test_support::ProviderObservationsFixture::default();
    let mut execution = test_gateway_execution(
        format!("{}/v1/chat/completions", provider.base_url),
        Some(2000),
    );
    execution.model_name = "selected-display-alias".into();
    execution.wire_model_name = Some("glm-5.3".into());
    execution.thinking_capability = Some(ThinkingCapability::EffortOnly);
    execution.thinking_protocol = Some(ThinkingProtocol::ReasoningEffort);
    let mut host = test_host_builder("replay-user", "replay-session")
        .with_edge_tools(sample_edge_tools())
        .with_execution_binding_snapshot(edge_ledger_runtime_snapshot())
        .with_provider_observations(ledger.clone())
        .with_admitted_model_execution(Some(execution))
        .build();
    let mut state = create_durable_execution_test_state("replay-session");
    state.message = "Read the two supplied fixture results.".into();
    state.user_intent = state.message.clone();
    state.messages = vec![json!({"role":"user","content":state.message})];
    state.thinking = ThinkingConfig::Adaptive {
        effort: astra_turn_core::thinking_config::ThinkingEffort::Low,
    };
    for (round, expected, tool_result) in [
        (0, expected_first, expected_result_one),
        (1, expected_second, expected_result_two),
    ] {
        let canonical_before = state.messages.clone();
        state.step_recorder.begin_turn(round);
        state.current_round_index = round;
        let result = host.execute_turn(&mut state).await.unwrap();
        assert_eq!(
            state.messages, canonical_before,
            "wire projection cannot rewrite canonical transcript"
        );
        let assistant = assistant_message_with_tool_calls_and_reasoning(
            &result.accum.tool_calls,
            &result.accum.reasoning_content,
            false,
        );
        assert_eq!(assistant["reasoning_content"], expected["reasoning_content"]);
        assert_ne!(assistant["tool_calls"][0]["id"], expected["tool_calls"][0]["id"]);
        assert!(result.accum.error_message.is_none());
        // This projection fixture supplies tool results manually. Adopt the
        // actual provider decision through the same lease owner used by normal
        // execute_turn_and_ingest_phase before starting its next request.
        state.commit_volatile_attempt_lease();
        let mut canonical_result = tool_result;
        canonical_result["tool_call_id"] = assistant["tool_calls"][0]["id"].clone();
        state.messages.push(assistant);
        state.messages.push(canonical_result);
        state.llm_rounds_completed += 1;
    }
    let canonical_before = state.messages.clone();
    state.step_recorder.begin_turn(2);
    let result = host.execute_turn(&mut state).await.unwrap();
    assert_eq!(result.accum.full_text, "finished");
    assert!(result.accum.error_message.is_none());
    state.commit_volatile_attempt_lease();
    assert_eq!(state.messages, canonical_before);
    assert_eq!(provider.requests.lock().await.len(), 3);
    ledger.assert_quiescent();
    provider.assert_complete();
}

#[test]
fn glm_actual_server_capture_uses_admitted_upstream_without_changing_cache_identity() {
    use astra_turn_core::fork_prefix_store::{InMemoryPrefixStore, PrefixCaptureSink};
    let store = Arc::new(InMemoryPrefixStore::new());
    let mut execution =
        test_gateway_execution("http://127.0.0.1:1/v1/chat/completions", Some(2000));
    execution.model_name = "selected-display-alias".into();
    execution.wire_model_name = Some("glm-5.3".into());
    let mut host = test_host_builder("replay-user", "replay-capture")
        .with_model(Some("selected-display-alias".into()))
        .with_prefix_store(Some(store.clone()))
        .with_admitted_model_execution(Some(execution))
        .build();
    let mut state = create_durable_execution_test_state("replay-capture");
    state.thinking = ThinkingConfig::Off;
    state.messages =
        vec![json!({"role":"assistant","content":"result","reasoning_content":"original\n\t"})];
    let original = serde_json::to_vec(&state.messages).unwrap();
    host.on_turn_completed(&state);
    let prefix = store
        .get_prefix(state.current_run_id.as_deref().unwrap())
        .unwrap();
    assert_eq!(prefix.model_id, "selected-display-alias");
    assert_eq!(
        prefix.provider,
        astra_turn_core::fork_prefix::ProviderKind::from_provider_hint("selected-display-alias")
    );
    assert_eq!(
        prefix.canonical_prefix_bytes().as_slice(),
        original.as_slice()
    );
    let thinking = prefix.thinking.as_ref().unwrap();
    assert!(thinking.enabled);
    assert_eq!(thinking.kind, "native");
    assert_eq!(thinking.budget_tokens, 0);

    let unknown_store = Arc::new(InMemoryPrefixStore::new());
    let mut unknown = test_host_builder("replay-user", "unknown-capture")
        .with_model(Some("glm-5.3".into()))
        .with_prefix_store(Some(unknown_store.clone()))
        .build();
    unknown.on_turn_completed(&state);
    let prefix = unknown_store
        .get_prefix(state.current_run_id.as_deref().unwrap())
        .unwrap();
    assert!(
        prefix.thinking.is_none(),
        "display alias alone does not admit replay capability"
    );
}
