// Included in the existing server host test module to reuse its real provider,
// inference-ledger, route admission and loop fixtures.

#[test]
fn recovery_generation_control_and_budget_follow_admitted_capabilities() {
    use astra_core::model_wire::thinking::ThinkingProtocol;
    use astra_services::models::ThinkingCapability;
    use astra_turn_core::cache_placement::CacheProtocol;
    use astra_turn_core::thinking_config::ThinkingEffort;
    let requested = ThinkingConfig::Adaptive {
        effort: ThinkingEffort::Max,
    };
    for (capability, protocol, expected, fast) in [
        (
            Some(ThinkingCapability::Both),
            ThinkingProtocol::ThinkingObject,
            ThinkingConfig::Off,
            true,
        ),
        (
            Some(ThinkingCapability::Both),
            ThinkingProtocol::ReasoningEffort,
            ThinkingConfig::Adaptive {
                effort: ThinkingEffort::Low,
            },
            false,
        ),
        (
            Some(ThinkingCapability::EffortOnly),
            ThinkingProtocol::ReasoningEffort,
            ThinkingConfig::Adaptive {
                effort: ThinkingEffort::Low,
            },
            false,
        ),
        (
            Some(ThinkingCapability::NativeOnly),
            ThinkingProtocol::Unknown,
            requested.clone(),
            false,
        ),
        (None, ThinkingProtocol::Unknown, requested.clone(), false),
        (
            Some(ThinkingCapability::None),
            ThinkingProtocol::Unknown,
            ThinkingConfig::Off,
            true,
        ),
    ] {
        let mut config = summary_test_config(String::new());
        config.thinking_capability = capability;
        config.thinking_protocol = Some(protocol);
        let boundary = ProviderAttemptBoundary::new(true, false);
        let effective = primary_thinking_for_attempt(
            &requested,
            false,
            false,
            false,
            boundary,
            CacheProtocol::None,
            &config,
        );
        assert_eq!(effective, expected, "{capability:?}/{protocol:?}");
        let boundary = boundary.with_generation_control(&effective, &config);
        assert_eq!(boundary.budget(), fast.then_some(Duration::from_secs(30)));
        assert!(!boundary.allows_host_continuation());
        let remaining = Duration::from_secs(7);
        assert_eq!(
            boundary.provider_work_budget(Some(ProviderWorkBudget(remaining))),
            Some(remaining)
        );
        assert_eq!(
            boundary.provider_work_budget(Some(ProviderWorkBudget(Duration::from_secs(80)))),
            Some(Duration::from_secs(if fast { 30 } else { 80 }))
        );
        for exact in [
            ThinkingConfig::Enabled {
                budget_tokens: 4096,
            },
            requested.clone(),
        ] {
            let effective = primary_thinking_for_attempt(
                &exact,
                true,
                true,
                true,
                boundary,
                CacheProtocol::None,
                &config,
            );
            assert_eq!(effective, exact);
            assert_eq!(
                boundary
                    .with_generation_control(&effective, &config)
                    .budget(),
                None
            );
        }
    }
}

#[test]
fn exact_native_recovery_budget_is_not_silently_reduced() {
    use astra_services::models::ThinkingCapability;
    use astra_turn_core::cache_placement::CacheProtocol;
    let mut config = summary_test_config(String::new());
    config.provider = "anthropic".into();
    config.thinking_capability = Some(ThinkingCapability::Both);
    let exact = ThinkingConfig::Enabled {
        budget_tokens: 4096,
    };
    let boundary = ProviderAttemptBoundary::new(true, true);
    let effective = primary_thinking_for_attempt(
        &exact,
        false,
        true,
        true,
        boundary,
        CacheProtocol::None,
        &config,
    );
    assert_eq!(effective, exact);
    assert!(effective.is_supported_by(
        &config.provider,
        Some("both"),
        config.thinking_protocol.unwrap_or_default()
    ));
    assert_eq!(
        boundary
            .with_generation_control(&effective, &config)
            .budget(),
        None
    );
}

#[test]
fn recovery_empty_result_reports_only_its_resolved_budget() {
    for (config, expected) in [
        (nonthinking_test_config(), Some(30_000)),
        (summary_test_config(String::new()), None),
    ] {
        let boundary = ProviderAttemptBoundary::new(true, false)
            .with_generation_control(&ThinkingConfig::Off, &config);
        let error = validate_provider_convergence_result(
            boundary,
            false,
            &LlmCallResult::default(),
            &HashSet::new(),
        )
        .unwrap_err();
        let details: Value = serde_json::from_str(error.details_json.as_deref().unwrap()).unwrap();
        assert_eq!(details["deadline"]["limit_ms"].as_u64(), expected);
        assert_eq!(details["deadline"]["retry_safety"], "none");
    }
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(auxiliary_llm_capacity_policy_env)]
async fn recovery_effort_only_wire_can_deliver_after_the_suppressed_cap() {
    use astra_core::model_wire::thinking::ThinkingProtocol;
    use astra_services::models::ThinkingCapability;
    use astra_turn_core::thinking_config::ThinkingEffort;
    let _provider_admission = EnvVarGuard::remove("ASTRA_LLM_PROVIDER_ADMISSION_MODE");
    let release = Arc::new(tokio::sync::Notify::new());
    let stream = ProviderResponse::Stream {
        content_type: "text/event-stream",
        chunks: vec![
            b"data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"fixture reasoning\"},\"finish_reason\":null}]}\n\n".to_vec(),
            b"data: {\"choices\":[{\"delta\":{\"content\":\"recovered result\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".to_vec(),
        ],
        release_before_chunk: Some((1, release.clone())),
    };
    let provider = ProviderGateway::start(vec![ProviderScript::new(
        "mandatory-thinking recovery",
        |request| request.path == "/v1/chat/completions",
        vec![stream],
    )])
    .await;
    let ledger = crate::server::provider_test_support::ProviderObservationsFixture::default();
    let mut execution = test_gateway_execution(
        format!("{}/v1/chat/completions", provider.base_url),
        Some(2000),
    );
    execution.thinking_capability = Some(ThinkingCapability::EffortOnly);
    execution.thinking_protocol = Some(ThinkingProtocol::ReasoningEffort);
    let mut host = test_host_builder("recovery-user", "recovery-session")
        .with_provider_observations(ledger.clone())
        .with_admitted_model_execution(Some(execution))
        .build();
    let (tx, mut rx) = tokio::sync::mpsc::channel(128);
    host.set_event_tx(tx);
    let mut state = create_durable_execution_test_state("recovery-session");
    state.thinking = ThinkingConfig::Adaptive {
        effort: ThinkingEffort::Max,
    };
    state.provider_adaptation.safe_recovery_attempted = true;
    state.provider_adaptation.safe_recovery_pending = true;
    state
        .messages
        .push(json!({"role":"user","content":"retained request"}));
    state.message = "retained request".into();
    state.user_intent = state.message.clone();
    state.step_recorder.begin_turn(0);
    let result = {
        let turn = host.execute_turn(&mut state);
        tokio::pin!(turn);
        loop {
            tokio::select! {
                result = &mut turn => panic!("provider finished before fixture release: {}", result.err().map(|e| e.to_string()).unwrap_or_default()),
                event = rx.recv() => {
                    let event = event.expect("live provider event");
                    if event["type"] == "reasoning_delta" { break; }
                }
            }
        }
        // Warp only after the admitted provider is demonstrably streaming.
        // The old 30-second optimization would expire before this release.
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(35)).await;
        release.notify_one();
        let result = turn.await;
        tokio::time::resume();
        result.expect("mandatory-thinking recovery keeps its ordinary bounded request slice")
    };
    assert_eq!(result.accum.full_text, "recovered result");
    assert!(!state.provider_adaptation.safe_recovery_pending);
    assert!(state.provider_adaptation.safe_recovery_attempted);
    let requests = provider.requests.lock().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].body["reasoning_effort"], "low");
    assert!(
        requests[0].body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["content"] == "retained request")
    );
    drop(requests);
    assert_eq!(
        state
            .step_recorder
            .events()
            .iter()
            .filter(|event| event.event_type
                == astra_pipeline::step_protocol::StepEventType::LlmRoundCompleted)
            .count(),
        1
    );
    assert!(
        !state
            .step_recorder
            .events()
            .iter()
            .any(|event| event.event_type
                == astra_pipeline::step_protocol::StepEventType::LlmRoundFailed)
    );
    ledger.assert_quiescent();
    provider.assert_complete();
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(auxiliary_llm_capacity_policy_env)]
async fn exhausted_provider_recovery_is_visible_and_closes_each_trace() {
    for measured in [false, true] {
        assert_exhausted_provider_recovery_trace(measured).await;
    }
}

async fn assert_exhausted_provider_recovery_trace(measured: bool) {
    let _provider_admission = EnvVarGuard::remove("ASTRA_LLM_PROVIDER_ADMISSION_MODE");
    let response = || {
        let mut chunk =
            json!({"choices":[{"delta":{"reasoning_content":"fixture"},"finish_reason":null}]});
        if measured {
            chunk["usage"] = json!({"prompt_tokens":8,"completion_tokens":3});
        }
        ProviderResponse::Stream {
            content_type: "text/event-stream",
            chunks: vec![format!("data: {chunk}\n\n").into_bytes()],
            release_before_chunk: None,
        }
    };
    let provider = ProviderGateway::start(vec![ProviderScript::new(
        "failed inference and one recovery",
        |request| request.path == "/v1/chat/completions",
        vec![response(), response()],
    )])
    .await;
    let ledger = crate::server::provider_test_support::ProviderObservationsFixture::default();
    let mut host = test_host_builder("recovery-user", "exhausted-recovery")
        .with_provider_observations(ledger.clone())
        .with_admitted_model_execution(Some(test_gateway_execution(
            format!("{}/v1/chat/completions", provider.base_url),
            Some(2000),
        )))
        .build();
    let mut state = create_durable_execution_test_state("exhausted-recovery");
    state
        .messages
        .push(json!({"role":"user","content":"implement the supplied change"}));
    state.message = "implement the supplied change".into();
    state.user_intent = state.message.clone();
    let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
    assert!(
        outcome.is_err(),
        "an incomplete stream must retain its error"
    );
    let interruption = state.interruption.as_ref().expect("typed provider blocker");
    assert!(matches!(
        interruption.resume_action,
        astra_turn_core::interruption::ResumeAction::RequiresIntervention { .. }
    ));
    assert!(interruption.user_message.contains("after one recovery"));
    assert!(interruption.user_message.contains("unfinished"));
    assert!(!interruption.user_message.contains("Continue this session"));
    assert_eq!(interruption.tool_calls_completed, 0);
    assert!(!interruption.has_checkpoint);
    let usage = state.qualified_usage.expect("two dispatched attempts");
    assert_eq!(usage.input_tokens(), measured.then_some(16));
    assert_eq!(usage.output_tokens(), measured.then_some(6));
    let events = host.take_emitted_events();
    let recovery = events
        .iter()
        .position(|event| event["code"] == "provider_recovery")
        .expect("recovery is visible before its wait");
    let starts: Vec<_> = events
        .iter()
        .enumerate()
        .filter(|(_, event)| event["status"] == "llm_call_started")
        .map(|(index, _)| index)
        .collect();
    assert_eq!(starts.len(), 2);
    assert!(starts[0] < recovery && recovery < starts[1]);
    use astra_pipeline::step_protocol::StepEventType;
    let traces = state.step_recorder.events();
    let starts: Vec<_> = traces
        .iter()
        .filter(|event| event.event_type == StepEventType::LlmRoundStarted)
        .collect();
    let failures: Vec<_> = traces
        .iter()
        .filter(|event| event.event_type == StepEventType::LlmRoundFailed)
        .collect();
    assert_eq!(starts.len(), 2);
    assert_eq!(failures.len(), 2);
    for (start, failure) in starts.iter().zip(&failures) {
        assert_eq!(start.step_id, failure.step_id);
        assert!(failure.caused_by.contains(&start.event_id));
        assert!(start.payload.as_ref().unwrap()["invocation_id"].is_string());
        let usage = &failure.payload.as_ref().unwrap()["qualified_usage"];
        if measured {
            assert_eq!(usage["input_tokens"], 8);
            assert_eq!(usage["output_tokens"], 3);
            assert!(usage["cached_input_tokens"].is_null());
            assert!(usage["cache_creation_tokens"].is_null());
        } else {
            assert_eq!(
                usage,
                &json!({}),
                "dispatched usage stays explicitly unknown"
            );
        }
    }
    assert!(
        traces
            .iter()
            .filter(|event| event.event_type == StepEventType::StepCreated)
            .all(
                |created| traces.iter().any(|event| event.step_id == created.step_id
                    && matches!(
                        event.event_type,
                        StepEventType::StepIncomplete | StepEventType::StepCompleted
                    ))
            )
    );
    assert_eq!(provider.requests.lock().await.len(), 2);
    ledger.assert_quiescent();
    provider.assert_complete();
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(auxiliary_llm_capacity_policy_env)]
async fn clean_empty_recovery_reaches_run_finalizer_with_intervention() {
    let _provider_admission = EnvVarGuard::remove("ASTRA_LLM_PROVIDER_ADMISSION_MODE");
    let responses = [(8, 3), (5, 2)].map(|(input, output)| ProviderResponse::Stream {
        content_type: "text/event-stream",
        chunks: vec![format!(
            "data: {{\"choices\":[{{\"delta\":{{\"reasoning_content\":\"fixture\"}},\"finish_reason\":\"stop\"}}],\"usage\":{{\"prompt_tokens\":{input},\"completion_tokens\":{output}}}}}\n\ndata: [DONE]\n\n"
        ).into_bytes()],
        release_before_chunk: None,
    });
    let provider = ProviderGateway::start(vec![ProviderScript::new(
        "two clean empty completions",
        |request| request.path == "/v1/chat/completions",
        responses.into(),
    )])
    .await;
    let ledger = crate::server::provider_test_support::ProviderObservationsFixture::default();
    let mut host = test_host_builder("empty-user", "empty-recovery")
        .with_provider_observations(ledger.clone())
        .with_admitted_model_execution(Some(test_gateway_execution(
            format!("{}/v1/chat/completions", provider.base_url),
            Some(2000),
        )))
        .build();
    let mut state = create_durable_execution_test_state("empty-recovery");
    let hub = Arc::new(crate::observability::ObservabilityHub::new());
    state.telemetry.observability_session = Some(hub.start_session("empty-user", "empty-recovery"));
    state.telemetry.observability_hub = Some(hub.clone());
    state
        .messages
        .push(json!({"role":"user","content":"finish the supplied request"}));
    state.message = "finish the supplied request".into();
    state.user_intent = state.message.clone();
    let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
    crate::server::run::lifecycle::tests::assert_empty_recovery_run_finalization(outcome, &state);
    assert!(state.final_text.contains("unfinished"));
    assert_eq!(provider.requests.lock().await.len(), 2);
    assert_eq!(state.qualified_usage.unwrap().input_tokens(), Some(13));
    assert_eq!(state.qualified_usage.unwrap().output_tokens(), Some(5));
    let completed: Vec<_> = state
        .step_recorder
        .events()
        .iter()
        .filter(|event| {
            event.event_type == astra_pipeline::step_protocol::StepEventType::LlmRoundCompleted
        })
        .collect();
    assert_eq!(completed.len(), 2);
    for (event, (input, output)) in completed.iter().zip([(8, 3), (5, 2)]) {
        let payload = event.payload.as_ref().unwrap();
        assert_eq!(payload["input_tokens"], input);
        assert_eq!(payload["output_tokens"], output);
    }
    ledger.assert_quiescent();
    provider.assert_complete();
    let signals: Vec<_> = hub
        .recent_feedback_signals()
        .into_iter()
        .filter(|signal| signal.turn_id == state.current_run_id)
        .collect();
    assert_eq!(signals.len(), 1, "{signals:?}");
    assert_eq!(
        signals[0].signal_type,
        astra_core::feedback::SignalType::Interruption
    );
    assert_eq!(signals[0].turn_id, state.current_run_id);
    assert_eq!(signals[0].context["session_id"], "empty-recovery");
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(auxiliary_llm_capacity_policy_env)]
async fn provider_completion_trace_needs_no_inference_receipt() {
    let _provider_admission = EnvVarGuard::remove("ASTRA_LLM_PROVIDER_ADMISSION_MODE");
    let _budget = EnvVarGuard::set("ASTRA_LLM_TOTAL_BUDGET_S", "1");
    let provider = ProviderGateway::start(vec![ProviderScript::new(
        "completed provider, delayed receipt",
        |request| request.path == "/v1/chat/completions",
        vec![ProviderResponse::OpenAi(json!({
            "choices":[{"message":{"content":"completed provider result"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":8,"completion_tokens":3}
        }))],
    )])
    .await;
    let ledger = crate::server::provider_test_support::ProviderObservationsFixture::default();
    let mut host = test_host_builder("trace-user", "ledger-trace")
        .with_provider_observations(ledger.clone())
        .with_admitted_model_execution(Some(test_gateway_execution(
            format!("{}/v1/chat/completions", provider.base_url),
            Some(2000),
        )))
        .build();
    let mut state = create_durable_execution_test_state("ledger-trace");
    state
        .messages
        .push(json!({"role":"user","content":"return the supplied result"}));
    state.message = "return the supplied result".into();
    state.user_intent = state.message.clone();
    state.step_recorder.begin_turn(0);
    let result = host.execute_turn(&mut state).await.unwrap();
    assert!(result.error_kind.is_none());
    use astra_pipeline::step_protocol::StepEventType;
    let traces = state.step_recorder.events();
    let completed: Vec<_> = traces
        .iter()
        .filter(|event| event.event_type == StepEventType::LlmRoundCompleted)
        .collect();
    assert_eq!(completed.len(), 1);
    assert!(
        !traces
            .iter()
            .any(|event| event.event_type == StepEventType::LlmRoundFailed)
    );
    let payload = completed[0].payload.as_ref().unwrap();
    assert_eq!(payload["input_tokens"], 8);
    assert_eq!(payload["output_tokens"], 3);
    let usage = result.accum.qualified_usage.unwrap().to_json();
    assert_eq!(usage["input_tokens"], 8);
    assert_eq!(usage["output_tokens"], 3);
    assert!(usage["cached_input_tokens"].is_null());
    assert!(usage["cache_creation_tokens"].is_null());
    assert_eq!(provider.requests.lock().await.len(), 1);
    ledger.assert_quiescent();
    provider.assert_complete();
}
