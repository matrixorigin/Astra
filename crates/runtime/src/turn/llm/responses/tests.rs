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
    snapshots: Mutex<Vec<super::UsageSnapshot>>,
}

#[tokio::test(start_paused = true)]
async fn raw_reasoning_is_private_but_keeps_the_stream_alive() {
    use futures_util::{StreamExt, stream};
    use std::collections::HashSet;

    struct Timeouts;
    impl Drop for Timeouts {
        fn drop(&mut self) {
            super::super::TEST_STREAM_IDLE_TIMEOUT.with(|value| *value.borrow_mut() = None);
            super::super::TEST_STREAM_IDLE_TIMEOUT_AFTER_PROGRESS
                .with(|value| *value.borrow_mut() = None);
        }
    }
    super::super::TEST_STREAM_IDLE_TIMEOUT
        .with(|value| *value.borrow_mut() = Some(Duration::from_millis(100)));
    super::super::TEST_STREAM_IDLE_TIMEOUT_AFTER_PROGRESS
        .with(|value| *value.borrow_mut() = Some(Duration::from_millis(100)));
    let _timeouts = Timeouts;
    let terminal = json!({"type":"response.completed", "response":{
        "status":"completed", "output":[{"type":"message", "role":"assistant", "content":[
            {"type":"output_text", "text":"done"}
        ]}]
    }});
    for (delta, budget, should_succeed) in [
        (json!("private reasoning"), None, true),
        (json!("  "), None, false),
        (json!(12), None, false),
        (
            json!("private reasoning"),
            Some(Duration::from_millis(250)),
            false,
        ),
    ] {
        let raw = json!({"type":"response.reasoning_text.delta", "delta":delta});
        let events = vec![raw.clone(), raw.clone(), raw, terminal.clone()];
        let stream = stream::iter(events).then(|event| async move {
            tokio::time::sleep(Duration::from_millis(80)).await;
            Ok::<_, std::io::Error>(format!("data: {event}\n\n"))
        });
        let response = reqwest::Response::from(
            axum::http::Response::builder()
                .status(200)
                .body(reqwest::Body::wrap_stream(stream))
                .unwrap(),
        );
        let route = LlmExecutionRoute {
            inference_protocol: Some(LlmProviderProtocol::OpenAiResponses),
            response_scope: Some("scope"),
            subscription: None,
            fixed_temperature: None,
            supported_reasoning_efforts: None,
            thinking_protocol: None,
            model_name: "model",
            wire_model_name: None,
            api_key: "key",
            base_url: "http://localhost",
            provider: "openai",
            header_overrides: None,
            request_body_overrides: None,
            completions_url_override: None,
            request_timeout: None,
        };
        let mut updates = Vec::new();
        let mut callback = |update| updates.push(update);
        let result = collect(
            response,
            route,
            &HashSet::new(),
            LlmCancel::None,
            Some(&mut callback),
            tokio::time::Instant::now().into_std(),
            budget,
            None,
            None,
        )
        .await;
        assert_eq!(result.is_ok(), should_succeed);
        if let Ok(result) = result {
            assert_eq!(result.full_text, "done");
            assert!(result.reasoning.is_empty());
        }
        assert!(
            updates.is_empty(),
            "raw reasoning must not reach user callbacks"
        );
    }
}

#[tokio::test]
async fn api_key_responses_replays_tool_round_with_scoped_reasoning() {
    use axum::{
        Json, Router,
        extract::State,
        http::{HeaderMap, StatusCode},
        response::IntoResponse,
        routing::post,
    };
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let wire_requests = Arc::clone(&requests);
    let app = Router::new().route("/responses", post(
        |State(requests): State<Arc<Mutex<Vec<Value>>>>, uri: axum::http::Uri, headers: HeaderMap, Json(body): Json<Value>| async move {
            if body["input"].to_string().contains("look up") {
                assert_eq!(uri.path(), "/responses");
                assert_eq!(uri.query(), Some("workspace=one"));
                let authorization = headers.get_all("authorization").iter().collect::<Vec<_>>();
                assert_eq!(authorization, ["Basic dXNlcjpwYXNz", "Bearer custom"]);
                assert_eq!(headers.get("x-tenant").unwrap(), "tenant-a");
                assert_eq!(headers.get_all("x-tenant").iter().count(), 1);
            } else {
                assert_eq!(headers.get("authorization").unwrap(), "Bearer test-key");
            }
            if body.get("include").is_some()
                || body["tools"].as_array().is_some_and(|tools| tools.iter().any(|tool| tool["type"] != "function"))
            {
                return (StatusCode::BAD_REQUEST, Json(json!({"error":{"code":"unsupported_parameter"}}))).into_response();
            }
            if body["input"].to_string().contains("context failure") {
                return (StatusCode::BAD_REQUEST, Json(json!({"error":{"message":"maximum context length secret provider detail"}}))).into_response();
            }
            if body["input"].to_string().contains("billing failure") {
                return (StatusCode::PAYMENT_REQUIRED, Json(json!({"error":{"code":"insufficient_credit"}}))).into_response();
            }
            if body["input"].to_string().contains("reject this") {
                return (StatusCode::BAD_REQUEST, Json(json!({"error":{"code":"unsupported_parameter","param":"temperature","message":"secret provider detail"}}))).into_response();
            }
            if body["input"].to_string().contains("reject effort") {
                return (StatusCode::BAD_REQUEST, Json(json!({"error":{"code":"invalid_value","param":"reasoning.effort","message":"secret provider detail"}}))).into_response();
            }
            if body["input"].to_string().contains("low only") && body["reasoning"]["effort"] != "low" {
                return (StatusCode::BAD_REQUEST, Json(json!({"error":{"code":"invalid_value","param":"reasoning.effort"}}))).into_response();
            }
            let malformed = body["input"].to_string().contains("malformed terminal");
            let oversized = body["input"].to_string().contains("oversized terminal");
            let plaintext = body["input"].to_string().contains("plaintext case");
            let fresh_turn = body["input"].to_string().contains("start a new turn");
            let continued = body["input"].as_array().unwrap().iter().any(|item| item["type"] == "function_call_output");
            requests.lock().unwrap().push(body);
            let output = if malformed {
                json!([{"type":"message","content":[{"type":"output_text","text":"working"}]},
                    {"type":"function_call","name":"lookup","call_id":"call-bad","arguments":"{}"}])
            } else if oversized {
                let mut items = vec![json!({"type":"function_call","name":"lookup","call_id":"call-large","arguments":"{}"})];
                items.extend((0..4096).map(|_| json!({"type":"reasoning","encrypted_content":"ciphertext"})));
                json!(items)
            } else if continued || fresh_turn {
                json!([{"type":"message","id":"msg-1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"done","annotations":[]}]}])
            } else if plaintext {
                json!([{"type":"reasoning","id":"rs-plain","summary":[],"content":[{"type":"reasoning_text","text":"check the file"}]},
                    {"type":"function_call","id":"fn-1","name":"lookup","call_id":"call-1","arguments":"{}","status":"completed"}])
            } else {
                json!([{"type":"reasoning","id":"reason-1","encrypted_content":"ciphertext","summary":[]},
                    {"type":"function_call","id":"fn-1","name":"lookup","call_id":"call-1","arguments":"{}","status":"completed"}])
            };
            let mut events = String::new();
            if !malformed && !oversized {
                for (index, item) in output.as_array().unwrap().iter().enumerate() {
                    events.push_str(&format!("data: {}\n\n", json!({"type":"response.output_item.done","output_index":index,"item":item})));
                }
            }
            events.push_str(&format!("data: {}\n\n", json!({"type":"response.completed","response":{"id":"response-1","status":"completed","model":"model","output":if malformed || oversized { output } else { json!([]) },"usage":{"input_tokens":20,"output_tokens":5}}})));
            ([("content-type", "text/event-stream")], events).into_response()
        }
    )).route("/chat/completions", post(|| async {
        (StatusCode::BAD_REQUEST, Json(json!({"error":{"code":"unsupported_parameter","param":"reasoning_effort","message":"secret provider detail"}})))
    })).with_state(requests.clone());
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut offering = astra_services::ResolvedModelOffering {
        offering_id: "offering".into(),
        model: astra_services::ResolvedActiveLlmModel {
            inference_protocol: Some(LlmProviderProtocol::OpenAiResponses),
            response_scope_key: Some(astra_services::models::ResponseScopeKey::for_model(
                &FernetTokenEncryptor::new("test-scope-secret").unwrap(),
                "model",
            )),
            supported_reasoning_efforts: None,
            price_snapshot: None,
            model_name: "model".into(),
            wire_model_name: None,
            api_key: "test-key".into(),
            base_url: format!(
                "{}?workspace=one#first",
                base.replacen("http://", "http://user:pass@", 1)
            ),
            provider: "openai".into(),
            tags: Vec::new(),
            request_body_overrides: None,
            fixed_temperature: None,
            thinking_protocol: Some(
                astra_core::model_wire::thinking::ThinkingProtocol::ResponsesReasoning,
            ),
            prompt_cache_capability: None,
            thinking_capability: None,
            context_window: None,
            max_completion_tokens: None,
            request_headers: Some(
                json!({"X-Tenant":"tenant-a", "Authorization":"Bearer custom"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        },
    };
    let first_execution =
        astra_services::AdmittedModelExecution::from_offering(offering.clone()).unwrap();
    offering.model.request_headers = Some(
        json!({"x-tenant":"tenant-a", "authorization":"Bearer custom"})
            .as_object()
            .unwrap()
            .clone(),
    );
    offering.model.api_key = "unused-rotated-key".into();
    offering.model.base_url = format!(
        "{}\\?workspace=one",
        base.replacen("http://", "http://%75ser:%70ass@", 1)
    );
    let second_execution = astra_services::AdmittedModelExecution::from_offering(offering).unwrap();
    assert_eq!(
        first_execution.response_scope,
        second_execution.response_scope
    );
    let scope = first_execution.response_scope.clone().unwrap();
    fn cache_capability() -> CacheCapability {
        CacheCapability {
            protocol: astra_turn_core::cache_placement::CacheProtocol::StrictHistoryMatch,
            volatile_placement: VolatilePlacement::AppendOnlyUserTail,
            volatile_delivery: astra_turn_core::cache_placement::VolatileDeliveryPolicy::All,
            reuse_scope: None,
        }
    }
    fn call<'a>(
        messages: &'a [Value],
        tools: &'a [Value],
        scope: &'a str,
        base: &'a str,
        thinking: &'a ThinkingConfig,
    ) -> LlmCall<'a> {
        LlmCall {
            purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
            messages,
            tools,
            cache_capability: Some(cache_capability()),
            route: LlmExecutionRoute {
                inference_protocol: Some(LlmProviderProtocol::OpenAiResponses),
                response_scope: Some(scope),
                subscription: None,
                fixed_temperature: None,
                supported_reasoning_efforts: None,
                thinking_protocol: Some(
                    astra_core::model_wire::thinking::ThinkingProtocol::ResponsesReasoning,
                ),
                model_name: "model",
                wire_model_name: None,
                api_key: "test-key",
                base_url: base,
                provider: "openai",
                header_overrides: None,
                request_body_overrides: None,
                completions_url_override: None,
                request_timeout: None,
            },
            max_output_tokens: Some(1024),
            temperature: None,
            thinking,
        }
    }
    let tools = vec![
        json!({"type":"function","function":{"name":"lookup","description":"look up","parameters":{"type":"object","properties":{}}}}),
    ];
    let snapshot = |content: &str| {
        crate::turn::wire_assembly::required_append_only_runtime_authority_message(
            content,
            crate::turn::wire_assembly::RuntimeAuthorityKind::RuntimeContextSnapshot,
            astra_turn_types::RuntimeAuthorityLifetime::CurrentUserTurn,
        )
        .unwrap()
        .unwrap()
    };
    let mut first_messages = vec![json!({"role":"system","content":"stable policy"})];
    crate::turn::wire_assembly::ensure_append_only_runtime_authority_policy(&mut first_messages);
    first_messages.extend([
        json!({"role":"user","content":"look up"}),
        snapshot("first optional observation"),
    ]);
    let thinking = ThinkingConfig::Adaptive {
        effort: astra_turn_core::thinking_config::ThinkingEffort::Max,
    };
    let mut first_call = call(&first_messages, &tools, &scope, &base, &thinking);
    first_call.route = LlmExecutionRoute::from_admitted(&first_execution);
    let first = call_llm_and_collect_with_stream_callback(
        first_call,
        LlmCancel::None,
        None,
        None,
        RuntimeToolChoice::Auto,
        Some(Duration::from_secs(10)),
    )
    .await
    .unwrap();
    let mut messages = first_messages;
    let continuation = first.provider_continuation.unwrap();
    let continuation_text = continuation.to_string();
    for secret in [
        "test-key",
        "Bearer custom",
        "user:pass",
        "test-scope-secret",
    ] {
        assert!(!continuation_text.contains(secret));
    }
    messages.push(json!({"role":"assistant","content":"","tool_calls":first.tool_calls,astra_core::model_wire::responses::CONTINUATION_KEY:continuation}));
    let mut interrupted = messages.clone();
    interrupted.push(snapshot("runtime context is not a new human turn"));
    let mut interrupted_call = call(&interrupted, &tools, &scope, &base, &thinking);
    interrupted_call.route = LlmExecutionRoute::from_admitted(&second_execution);
    let failure = call_llm_and_collect_with_stream_callback(
        interrupted_call,
        LlmCancel::None,
        None,
        None,
        RuntimeToolChoice::Auto,
        Some(Duration::from_secs(10)),
    )
    .await
    .unwrap_err();
    assert!(
        failure
            .details_json
            .unwrap()
            .contains("responses_continuation_incomplete")
    );
    assert_eq!(
        requests.lock().unwrap().len(),
        1,
        "incomplete evidence must fail before provider dispatch"
    );
    // A result without a matching call in the same group can be the only
    // surviving evidence of an already executed tool.
    let mut extra_result = messages.clone();
    extra_result.push(json!({"role":"tool", "tool_call_id":"call-1", "content":"found"}));
    extra_result
        .push(json!({"role":"tool", "tool_call_id":"missing-call", "content":"effect occurred"}));
    let mut extra_result = astra_turn_core::prompt_facing::sanitize_canonical_continuation_messages_with_turn_semantics(extra_result).unwrap();
    extra_result = astra_turn_core::prompt_facing::sanitize_canonical_continuation_messages_with_turn_semantics(extra_result).unwrap();
    for protocol in [
        LlmProviderProtocol::OpenAiResponses,
        LlmProviderProtocol::OpenAiCompatible,
    ] {
        let mut attempt = call(&extra_result, &tools, &scope, &base, &thinking);
        attempt.route.inference_protocol = Some(protocol);
        let error = call_llm_and_collect_with_stream_callback(
            attempt,
            LlmCancel::None,
            None,
            None,
            RuntimeToolChoice::Auto,
            Some(Duration::from_secs(10)),
        )
        .await
        .unwrap_err();
        assert!(error.message.contains("Check their effects"));
        assert_eq!(
            requests.lock().unwrap().len(),
            1,
            "{protocol:?} dispatched an incomplete group"
        );
    }
    extra_result.push(json!({"role":"user", "content":"start a new turn"}));
    for protocol in [
        LlmProviderProtocol::OpenAiResponses,
        LlmProviderProtocol::OpenAiCompatible,
    ] {
        let mut attempt = call(&extra_result, &tools, &scope, &base, &thinking);
        attempt.route.inference_protocol = Some(protocol);
        assert!(prepare_streaming_call(attempt, RuntimeToolChoice::Auto).is_ok());
    }

    let mut lost_interrupted = interrupted;
    lost_interrupted
        .iter_mut()
        .find(|message| message["role"] == "assistant" && message["tool_calls"].is_array())
        .unwrap()
        .as_object_mut()
        .unwrap()
        .remove(astra_core::model_wire::responses::CONTINUATION_KEY);
    let mut recovered_histories = Vec::new();
    for tool_result in [
        json!({"role":"user", "tool_call_id":"call-1", "content":"tool result"}),
        json!({"role":"user", "content":[
            {"type":"text", "text":"tool output"},
            {"type":"tool_result", "tool_use_id":"call-1", "content":"tool result"}
        ]}),
    ] {
        let mut history = lost_interrupted.clone();
        history.push(tool_result);
        let mut restored = astra_turn_core::prompt_facing::sanitize_canonical_continuation_messages_with_turn_semantics(history).unwrap();
        for protocol in [
            LlmProviderProtocol::OpenAiResponses,
            LlmProviderProtocol::OpenAiCompatible,
        ] {
            let mut attempt = call(&restored, &tools, &scope, &base, &thinking);
            attempt.route.inference_protocol = Some(protocol);
            let error = call_llm_and_collect_with_stream_callback(
                attempt,
                LlmCancel::None,
                None,
                None,
                RuntimeToolChoice::Auto,
                Some(Duration::from_secs(10)),
            )
            .await
            .unwrap_err();
            assert!(error.message.contains("Check their effects"));
            assert_eq!(requests.lock().unwrap().len(), 1);
        }
        restored.push(json!({"role":"user", "content":"start a new turn"}));
        assert!(
            prepare_streaming_call(
                call(&restored, &tools, &scope, &base, &thinking),
                RuntimeToolChoice::Auto,
            )
            .is_ok()
        );
        recovered_histories.push(restored);
    }
    let mut lost_interrupted = astra_turn_core::prompt_facing::sanitize_canonical_continuation_messages_with_turn_semantics(lost_interrupted).unwrap();
    for protocol in [
        LlmProviderProtocol::OpenAiResponses,
        LlmProviderProtocol::OpenAiCompatible,
    ] {
        let mut attempt = call(&lost_interrupted, &tools, &scope, &base, &thinking);
        attempt.route.inference_protocol = Some(protocol);
        let error = call_llm_and_collect_with_stream_callback(
            attempt,
            LlmCancel::None,
            None,
            None,
            RuntimeToolChoice::Auto,
            Some(Duration::from_secs(10)),
        )
        .await
        .unwrap_err();
        assert!(error.message.contains("Check their effects"));
        assert_eq!(
            requests.lock().unwrap().len(),
            1,
            "{protocol:?} dispatched an incomplete turn"
        );
    }
    for (provider, protocol) in [
        ("anthropic", LlmProviderProtocol::AnthropicMessages),
        ("bedrock", LlmProviderProtocol::BedrockConverse),
    ] {
        let mut attempt = call(&lost_interrupted, &tools, &scope, &base, &thinking);
        attempt.route.provider = provider;
        attempt.route.inference_protocol = Some(protocol);
        let error = match prepare_streaming_call(attempt, RuntimeToolChoice::Auto) {
            Err(error) => error,
            Ok(_) => panic!("{protocol:?} accepted an incomplete turn"),
        };
        assert!(error.message.contains("Check their effects"));
    }
    let mut unanchored_call = messages.last().unwrap().clone();
    unanchored_call
        .as_object_mut()
        .unwrap()
        .remove(astra_core::model_wire::responses::CONTINUATION_KEY);
    let mut unanchored = astra_turn_core::prompt_facing::sanitize_canonical_continuation_messages_with_turn_semantics(vec![
        unanchored_call,
        json!({"role":"system", "content":"compacted", "_compact_boundary":true}),
        astra_turn_types::runtime_owned_message(
            "user", "runtime retry", astra_turn_types::RuntimeMessageDelivery::EphemeralControl,
        ),
    ]).unwrap();
    let failure = call_llm_and_collect_with_stream_callback(
        call(&unanchored, &tools, &scope, &base, &thinking),
        LlmCancel::None,
        None,
        None,
        RuntimeToolChoice::Auto,
        Some(Duration::from_secs(10)),
    )
    .await
    .unwrap_err();
    assert!(failure.message.contains("Check their effects"));
    assert_eq!(requests.lock().unwrap().len(), 1);
    let mut with_runtime_authority = unanchored.clone();
    with_runtime_authority.push(snapshot("runtime context is not a user turn"));
    let with_runtime_authority = astra_turn_core::prompt_facing::sanitize_canonical_continuation_messages_with_turn_semantics(with_runtime_authority).unwrap();
    assert!(with_runtime_authority.iter().any(|message| {
        message["role"] == "user"
            && astra_turn_types::runtime_message_delivery(message)
                == Some(astra_turn_types::RuntimeMessageDelivery::AppendOnlyRequiredContext)
            && !astra_turn_types::is_human_user_message(message)
    }));
    let failure = call_llm_and_collect_with_stream_callback(
        call(&with_runtime_authority, &tools, &scope, &base, &thinking),
        LlmCancel::None,
        None,
        None,
        RuntimeToolChoice::Auto,
        Some(Duration::from_secs(10)),
    )
    .await
    .unwrap_err();
    assert!(failure.message.contains("Check their effects"));
    assert_eq!(requests.lock().unwrap().len(), 1);
    unanchored.push(json!({"role":"user", "content":"start again"}));
    assert!(
        prepare_streaming_call(
            call(&unanchored, &tools, &scope, &base, &thinking),
            RuntimeToolChoice::Auto,
        )
        .is_ok()
    );
    lost_interrupted.push(json!({"role":"user", "content":"continue in a new turn"}));
    for protocol in [
        LlmProviderProtocol::OpenAiResponses,
        LlmProviderProtocol::OpenAiCompatible,
    ] {
        let mut attempt = call(&lost_interrupted, &tools, &scope, &base, &thinking);
        attempt.route.inference_protocol = Some(protocol);
        let prepared = prepare_streaming_call(attempt, RuntimeToolChoice::Auto).unwrap();
        let body: Value = serde_json::from_slice(&prepared.request.body).unwrap();
        let wire = body.to_string();
        assert!(!wire.contains(astra_turn_core::prompt_facing::INCOMPLETE_TOOL_GROUP_FIELD));
        assert!(
            !wire.contains("call-1"),
            "an incomplete call reached {protocol:?}"
        );
    }
    for (provider, protocol) in [
        ("anthropic", LlmProviderProtocol::AnthropicMessages),
        ("bedrock", LlmProviderProtocol::BedrockConverse),
    ] {
        let projected =
            consolidate_system_messages_for_protocol(&lost_interrupted, provider, None, protocol);
        let wire = json!(projected).to_string();
        assert!(!wire.contains(astra_turn_core::prompt_facing::INCOMPLETE_TOOL_GROUP_FIELD));
        assert!(!wire.contains("call-1"));
    }
    let mut ambiguous = astra_turn_core::prompt_facing::sanitize_canonical_continuation_messages_with_turn_semantics(vec![
        json!({"role":"user", "content":"inspect"}),
        json!({"role":"assistant", "content":[
            {"type":"text", "text":"checking"},
            {"type":"tool_use", "id":"hidden-call", "name":"hidden_operation", "input":{}}
        ], "tool_calls":[
            {"id":"duplicate-call", "type":"function", "function":{"name":"write_file", "arguments":"{}"}},
            {"id":"duplicate-call", "type":"function", "function":{"name":"delete_file", "arguments":"{}"}},
            {"id":"alias-call-1", "_astra_provider_tool_call_id":"duplicate-provider-id", "type":"function", "function":{"name":"write_file", "arguments":"{}"}},
            {"id":"alias-call-2", "_astra_provider_tool_call_id":"duplicate-provider-id", "type":"function", "function":{"name":"delete_file", "arguments":"{}"}},
            {"id":"safe-call", "type":"function", "function":{"name":"lookup", "arguments":"{}"}}
        ]}),
        json!({"role":"tool", "tool_call_id":"duplicate-call", "content":"ambiguous"}),
        json!({"role":"tool", "tool_call_id":"alias-call-1", "content":"first"}),
        json!({"role":"tool", "tool_call_id":"alias-call-2", "content":"second"}),
        json!({"role":"tool", "tool_call_id":"safe-call", "content":"found"}),
    ]).unwrap();
    assert_eq!(
        ambiguous[1][astra_turn_core::prompt_facing::INCOMPLETE_TOOL_GROUP_FIELD],
        true
    );
    ambiguous.push(json!({"role":"user", "content":"continue in a new turn"}));
    for protocol in [
        LlmProviderProtocol::OpenAiResponses,
        LlmProviderProtocol::OpenAiCompatible,
    ] {
        let mut attempt = call(&ambiguous, &tools, &scope, &base, &thinking);
        attempt.route.inference_protocol = Some(protocol);
        let prepared = prepare_streaming_call(attempt, RuntimeToolChoice::Auto).unwrap();
        let body: Value = serde_json::from_slice(&prepared.request.body).unwrap();
        let wire = body.to_string();
        assert!(wire.contains("safe-call"));
        for discarded in [
            "duplicate-call",
            "alias-call-1",
            "alias-call-2",
            "duplicate-provider-id",
            "write_file",
            "delete_file",
            "hidden-call",
            "hidden_operation",
        ] {
            assert!(
                !wire.contains(discarded),
                "{discarded} reached {protocol:?}"
            );
        }
    }
    messages.push(json!({"role":"tool","tool_call_id":"call-1","content":"found"}));
    messages.push(snapshot("updated optional observation"));
    let mut inconsistent = messages.clone();
    inconsistent
        .iter_mut()
        .find(|message| message["role"] == "assistant" && message["tool_calls"].is_array())
        .unwrap()[astra_core::model_wire::responses::CONTINUATION_KEY]["items"]
        .as_array_mut()
        .unwrap()
        .push(json!({"type":"function_call", "call_id":"call-2", "name":"lookup"}));
    let mut inconsistent = astra_turn_core::prompt_facing::sanitize_canonical_continuation_messages_with_turn_semantics(inconsistent).unwrap();
    for protocol in [
        LlmProviderProtocol::OpenAiResponses,
        LlmProviderProtocol::OpenAiCompatible,
    ] {
        let mut attempt = call(&inconsistent, &tools, &scope, &base, &thinking);
        attempt.route.inference_protocol = Some(protocol);
        let error = match prepare_streaming_call(attempt, RuntimeToolChoice::Auto) {
            Err(error) => error,
            Ok(_) => panic!("{protocol:?} accepted inconsistent saved calls"),
        };
        assert!(error.message.contains("Check their effects"));
    }
    inconsistent.push(json!({"role":"user", "content":"continue in a new turn"}));
    for protocol in [
        LlmProviderProtocol::OpenAiResponses,
        LlmProviderProtocol::OpenAiCompatible,
    ] {
        let mut attempt = call(&inconsistent, &tools, &scope, &base, &thinking);
        attempt.route.inference_protocol = Some(protocol);
        let prepared = prepare_streaming_call(attempt, RuntimeToolChoice::Auto).unwrap();
        let body: Value = serde_json::from_slice(&prepared.request.body).unwrap();
        let wire = body.to_string();
        assert!(wire.contains("call-1"));
        assert!(!wire.contains("call-2"));
    }
    let mut lost = messages.clone();
    lost.iter_mut()
        .find(|message| message["role"] == "assistant" && message["tool_calls"].is_array())
        .unwrap()
        .as_object_mut()
        .unwrap()
        .remove(astra_core::model_wire::responses::CONTINUATION_KEY);
    let mut lost = astra_turn_core::prompt_facing::sanitize_canonical_continuation_messages_with_turn_semantics(lost)
        .unwrap();
    let changed_scope = format!("sha256:{}", "b".repeat(64));
    for route_scope in [&scope, &changed_scope] {
        let failure = call_llm_and_collect_with_stream_callback(
            call(&lost, &tools, route_scope, &base, &thinking),
            LlmCancel::None,
            None,
            None,
            RuntimeToolChoice::Auto,
            Some(Duration::from_secs(10)),
        )
        .await
        .unwrap_err();
        assert!(
            failure
                .details_json
                .unwrap()
                .contains("responses_continuation_incomplete")
        );
        assert_eq!(
            requests.lock().unwrap().len(),
            1,
            "missing state must stop before dispatch"
        );
    }
    lost.push(json!({"role":"user", "content":"continue in a new turn"}));
    let mut historical_call = call(&lost, &tools, &changed_scope, &base, &thinking);
    historical_call.route = LlmExecutionRoute::from_admitted(&second_execution);
    historical_call.route.response_scope = Some(&changed_scope);
    let historical = call_llm_and_collect_with_stream_callback(
        historical_call,
        LlmCancel::None,
        None,
        None,
        RuntimeToolChoice::Auto,
        Some(Duration::from_secs(10)),
    )
    .await
    .unwrap();
    assert_eq!(historical.full_text, "done");
    assert_eq!(requests.lock().unwrap().len(), 2);
    let mut switched = call(&messages, &tools, &scope, &base, &thinking);
    switched.route = LlmExecutionRoute::from_admitted(&second_execution);
    switched.route.inference_protocol = Some(LlmProviderProtocol::OpenAiCompatible);
    let failure = match prepare_streaming_call(switched, RuntimeToolChoice::Auto) {
        Err(failure) => failure,
        Ok(_) => panic!("a different protocol cannot continue the active tool round"),
    };
    assert!(
        failure
            .details_json
            .unwrap()
            .contains("responses_continuation_scope_changed")
    );
    let mut second_call = call(&messages, &tools, &scope, &base, &thinking);
    second_call.route = LlmExecutionRoute::from_admitted(&second_execution);
    let second = call_llm_and_collect_with_stream_callback(
        second_call,
        LlmCancel::None,
        None,
        None,
        RuntimeToolChoice::Auto,
        Some(Duration::from_secs(10)),
    )
    .await
    .unwrap();
    assert_eq!(second.full_text, "done");
    let requests = requests.lock().unwrap();
    assert_eq!(requests[0]["reasoning"]["effort"], "max");
    assert!(requests[0].get("include").is_none());
    assert!(
        requests[2]["input"]
            .as_array()
            .unwrap()
            .starts_with(requests[0]["input"].as_array().unwrap()),
        "Responses must preserve the complete preceding request, including all runtime context"
    );
    assert_eq!(requests[0]["tools"], requests[2]["tools"]);
    assert!(
        requests[2]["input"]
            .to_string()
            .contains("updated optional observation")
    );
    assert!(
        requests[2]["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["type"] == "reasoning")
    );
    assert!(
        requests[2]["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["type"] == "function_call_output")
    );
    drop(requests);
    messages.push(
        crate::turn::wire_assembly::runtime_system_context_message("runtime state", true).unwrap(),
    );
    messages.push(
        crate::turn::wire_assembly::required_append_only_runtime_authority_message(
            "remaining_seconds=10",
            crate::turn::wire_assembly::RuntimeAuthorityKind::ExecutionTimeBudget,
            astra_turn_types::RuntimeAuthorityLifetime::NextAssistantDecision,
        )
        .unwrap()
        .unwrap(),
    );
    let other_scope = format!("sha256:{}", "b".repeat(64));
    let error = match prepare_streaming_call(
        call(&messages, &tools, &other_scope, &base, &thinking),
        RuntimeToolChoice::Auto,
    ) {
        Ok(_) => panic!("changed continuation scope must fail"),
        Err(error) => error,
    };
    assert!(error.message.contains("Start a new turn"));
    for tool_result in [
        json!({"role":"user", "tool_call_id":"call-1", "content":"tool result"}),
        json!({"role":"user", "content":[{"type":"tool_result", "tool_use_id":"call-1", "content":"tool result"}]}),
    ] {
        messages.push(tool_result);
        assert!(matches!(
            prepare_streaming_call(
                call(&messages, &tools, &other_scope, &base, &thinking),
                RuntimeToolChoice::Auto,
            ),
            Err(error) if error.message.contains("Start a new turn")
        ));
        messages.pop();
    }
    messages.push(json!({"role":"user", "content":"a new human turn"}));
    let prepared = prepare_streaming_call(
        call(&messages, &tools, &other_scope, &base, &thinking),
        RuntimeToolChoice::Auto,
    )
    .unwrap();
    let resumed_body: Value = serde_json::from_slice(&prepared.request.body).unwrap();
    let resumed_input = resumed_body["input"].as_array().unwrap();
    assert!(
        resumed_input
            .iter()
            .any(|item| item["type"] == "function_call" && item["call_id"] == "call-1")
    );
    assert!(
        resumed_input
            .iter()
            .any(|item| item["type"] == "function_call_output" && item["call_id"] == "call-1")
    );
    assert!(!resumed_body.to_string().contains("ciphertext"));
    let off = ThinkingConfig::Off;
    let mut explicit_temperature = call(&messages, &tools, &scope, &base, &thinking);
    explicit_temperature.temperature = Some(0.0);
    assert!(prepare_streaming_call(explicit_temperature, RuntimeToolChoice::Auto).is_err());
    assert!(
        prepare_streaming_call(
            call(&messages, &tools, &scope, &base, &off),
            RuntimeToolChoice::Auto
        )
        .is_err()
    );
    let efforts = vec!["none".to_owned()];
    let mut off_call = call(&messages, &tools, &scope, &base, &off);
    off_call.route.supported_reasoning_efforts = Some(&efforts);
    let prepared = prepare_streaming_call(off_call, RuntimeToolChoice::Auto).unwrap();
    let off_body: Value = serde_json::from_slice(&prepared.request.body).unwrap();
    assert_eq!(off_body["reasoning"]["effort"], "none");
    let rejected = vec![json!({"role":"user","content":"reject this"})];
    let error = call_llm_and_collect_with_stream_callback(
        call(&rejected, &tools, &scope, &base, &thinking),
        LlmCancel::None,
        None,
        None,
        RuntimeToolChoice::Auto,
        Some(Duration::from_secs(10)),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind, astra_core::ErrorKind::InvalidRequest);
    assert!(error.message.contains("temperature"));
    assert!(!format!("{error:?}").contains("secret provider detail"));
    let rejected_effort = vec![json!({"role":"user","content":"reject effort"})];
    let error = call_llm_and_collect_with_stream_callback(
        call(&rejected_effort, &tools, &scope, &base, &thinking),
        LlmCancel::None,
        None,
        None,
        RuntimeToolChoice::Auto,
        Some(Duration::from_secs(10)),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind, astra_core::ErrorKind::InvalidRequest);
    assert!(error.message.contains("max reasoning setting"));
    assert!(error.message.contains("Model default"));
    assert!(!format!("{error:?}").contains("secret provider detail"));
    let low_only = vec![json!({"role":"user","content":"low only"})];
    let low = ThinkingConfig::Adaptive {
        effort: astra_turn_core::thinking_config::ThinkingEffort::Low,
    };
    let result = call_llm_and_collect_with_stream_callback(
        call(&low_only, &tools, &scope, &base, &low),
        LlmCancel::None,
        None,
        None,
        RuntimeToolChoice::Auto,
        Some(Duration::from_secs(10)),
    )
    .await
    .unwrap();
    assert_eq!(
        result.tool_calls.len(),
        1,
        "the provider accepted the unverified low effort"
    );
    for input in ["malformed terminal", "oversized terminal"] {
        let messages = vec![json!({"role":"user", "content":input})];
        let before = wire_requests.lock().unwrap().len();
        let failure = call_llm_and_collect_with_stream_callback(
            call(&messages, &tools, &scope, &base, &thinking),
            LlmCancel::None,
            None,
            None,
            RuntimeToolChoice::Auto,
            Some(Duration::from_secs(10)),
        )
        .await
        .unwrap_err();
        assert_eq!(failure.kind, astra_core::ErrorKind::StreamTransport);
        assert_eq!(wire_requests.lock().unwrap().len(), before + 1);
    }
    for (text, kind) in [
        ("context failure", astra_core::ErrorKind::ContextWindow),
        ("billing failure", astra_core::ErrorKind::PaymentRequired),
    ] {
        let messages = vec![json!({"role":"user","content":text})];
        let error = call_llm_and_collect_with_stream_callback(
            call(&messages, &tools, &scope, &base, &thinking),
            LlmCancel::None,
            None,
            None,
            RuntimeToolChoice::Auto,
            Some(Duration::from_secs(10)),
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind, kind);
        assert!(!format!("{error:?}").contains("secret provider detail"));
    }
    let mut chat_call = call(&rejected, &tools, &scope, &base, &thinking);
    chat_call.route.inference_protocol = None;
    let error = call_llm_and_collect_with_stream_callback(
        chat_call,
        LlmCancel::None,
        None,
        None,
        RuntimeToolChoice::Auto,
        Some(Duration::from_secs(10)),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind, astra_core::ErrorKind::InvalidRequest);
    assert!(error.message.contains("reasoning_effort"));
    assert!(!format!("{error:?}").contains("secret provider detail"));
    let before = wire_requests.lock().unwrap().len();
    let mut resumed = call(&lost_interrupted, &tools, &scope, &base, &thinking);
    resumed.route = LlmExecutionRoute::from_admitted(&first_execution);
    call_llm_and_collect_with_stream_callback(
        resumed,
        LlmCancel::None,
        None,
        None,
        RuntimeToolChoice::Auto,
        Some(Duration::from_secs(10)),
    )
    .await
    .unwrap();
    let requests = wire_requests.lock().unwrap();
    assert_eq!(requests.len(), before + 1);
    assert!(
        !requests.last().unwrap()["input"]
            .to_string()
            .contains("call-1")
    );
    assert!(
        !requests
            .last()
            .unwrap()
            .to_string()
            .contains(astra_turn_core::prompt_facing::INCOMPLETE_TOOL_GROUP_FIELD)
    );
    drop(requests);
    for history in recovered_histories {
        let mut attempt = call(&history, &tools, &scope, &base, &thinking);
        attempt.route = LlmExecutionRoute::from_admitted(&first_execution);
        let result = call_llm_and_collect_with_stream_callback(
            attempt,
            LlmCancel::None,
            None,
            None,
            RuntimeToolChoice::Auto,
            Some(Duration::from_secs(10)),
        )
        .await
        .unwrap();
        assert_eq!(result.full_text, "done");
        assert!(result.tool_calls.is_empty());
        let requests = wire_requests.lock().unwrap();
        let input = &requests.last().unwrap()["input"];
        assert!(input.to_string().contains("start a new turn"));
        assert!(!input.to_string().contains("call-1"));
    }
    let mut attempt = call(&extra_result, &tools, &scope, &base, &thinking);
    attempt.route = LlmExecutionRoute::from_admitted(&first_execution);
    let result = call_llm_and_collect_with_stream_callback(
        attempt,
        LlmCancel::None,
        None,
        None,
        RuntimeToolChoice::Auto,
        Some(Duration::from_secs(10)),
    )
    .await
    .unwrap();
    assert_eq!(result.full_text, "done");
    assert!(
        !wire_requests
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .to_string()
            .contains("missing-call")
    );
    let mut plaintext_history = vec![json!({"role":"user", "content":"plaintext case"})];
    let default_thinking = ThinkingConfig::ModelDefault;
    let first = call_llm_and_collect_with_stream_callback(
        call(&plaintext_history, &tools, &scope, &base, &default_thinking),
        LlmCancel::None,
        None,
        None,
        RuntimeToolChoice::Auto,
        Some(Duration::from_secs(10)),
    )
    .await
    .unwrap();
    plaintext_history.push(
        json!({"role":"assistant", "content":"", "tool_calls":first.tool_calls,
        astra_core::model_wire::responses::CONTINUATION_KEY:first.provider_continuation.unwrap()}),
    );
    plaintext_history.push(json!({"role":"tool", "tool_call_id":"call-1", "content":"found"}));
    let committed =
        astra_turn_core::prompt_facing::sanitize_canonical_turn_delta_with_turn_semantics(
            plaintext_history,
            false,
        )
        .unwrap();
    let restored = astra_turn_core::prompt_facing::sanitize_canonical_continuation_messages_with_turn_semantics(committed).unwrap();
    let second = call_llm_and_collect_with_stream_callback(
        call(&restored, &tools, &scope, &base, &default_thinking),
        LlmCancel::None,
        None,
        None,
        RuntimeToolChoice::Auto,
        Some(Duration::from_secs(10)),
    )
    .await
    .unwrap();
    assert_eq!(second.full_text, "done");
    let requests = wire_requests.lock().unwrap();
    let input = &requests.last().unwrap()["input"];
    assert!(
        input
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["type"] == "reasoning"
                && item["content"][0]["text"] == "check the file")
    );
    assert!(
        input
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["type"] == "function_call_output" && item["call_id"] == "call-1")
    );
    server.abort();
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
    fn note_usage_snapshot(
        &self,
        _: u32,
        usage: crate::turn::token_usage::TokenUsage,
        presence: crate::turn::token_usage::TokenUsagePresence,
    ) {
        self.snapshots.lock().unwrap().push((usage, presence));
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
            temperature: execution.subscription.as_ref().map(|_| 0.7),
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
#[ignore = "requires an isolated MatrixOne database"]
async fn personal_byok_responses_runs_tools_and_fences_credential_rotation() {
    use astra_services::UserModelUpdateRequestData;
    use axum::{
        Json, Router,
        extract::State,
        http::{HeaderMap, StatusCode},
        response::IntoResponse,
        routing::post,
    };
    let settings = astra_core::MatrixOneSettings::from_env();
    crate::isolated_database::require_isolated_database(&settings.database);
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let app = Router::new().route("/responses", post(
        |State(requests): State<Arc<Mutex<Vec<Value>>>>, headers: HeaderMap, Json(body): Json<Value>| async move {
            if headers.get("authorization").is_none_or(|value| value != "Bearer test-key" && value != "Bearer new-key")
                || body["model"] != "fixture-model" || body["store"] != false || body["stream"] != true
                || body.get("include").is_some()
                || body["tools"].as_array().is_some_and(|tools| tools.iter().any(|tool| tool["type"] != "function")) {
                return (StatusCode::BAD_REQUEST, "invalid request").into_response();
            }
            let probe = body["input"].to_string().contains("Calculate 17 * 23");
            let continued = body["input"].as_array().unwrap().iter().any(|item| item["type"] == "function_call_output");
            let plain = body["input"].to_string().contains("plain lookup");
            let missing_reasoning = body["input"].to_string().contains("missing reasoning");
            let private_encoding = body["input"].to_string().contains("private encoding");
            let unexpected_namespace = body["input"].to_string().contains("unexpected namespace");
            requests.lock().unwrap().push(body);
            let output = if probe || continued {
                json!([{"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"done"}]}])
            } else if plain {
                json!([{"type":"function_call","id":"function","name":"lookup","call_id":"call-1","arguments":"{}","status":"completed"}])
            } else if missing_reasoning {
                json!([{"type":"reasoning","id":"reason","summary":[]},
                    {"type":"function_call","id":"function","name":"lookup","call_id":"call-1","arguments":"{}","status":"completed"}])
            } else if private_encoding {
                json!([{"type":"reasoning","_astra_encrypted_content_b64":"b3BhcXVl","summary":[]},
                    {"type":"function_call","id":"function","name":"lookup","call_id":"call-1","arguments":"{}","status":"completed"}])
            } else if unexpected_namespace {
                json!([{"type":"function_call","namespace":"other","name":"lookup","call_id":"call-1","arguments":"{}","status":"completed"}])
            } else {
                json!([{"type":"reasoning","id":"reason","summary":[],"encrypted_content":"opaque"},
                    {"type":"function_call","id":"function","name":"lookup","call_id":"call-1","arguments":"{}","status":"completed"}])
            };
            let event = json!({"type":"response.completed","response":{"id":"fixture","status":"completed","output":output}});
            ([("content-type", "text/event-stream")], format!("data: {event}\n\n")).into_response()
        }
    )).with_state(requests.clone());
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let endpoint = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    astra_services::storage::ensure_core_schema(&settings, "mysql")
        .await
        .unwrap();
    let pool = astra_core::SharedPool::new(&settings).await.unwrap();
    let key = Arc::new(FernetTokenEncryptor::new("byok-responses-fixture-key").unwrap());
    let service = DatabaseModelService::new(settings, key)
        .with_pool(pool)
        .with_deepseek_test_endpoint(endpoint)
        .unwrap();
    let owner = format!("byok-{}", uuid::Uuid::new_v4());
    let model = service
        .create_user_model(
            owner.clone(),
            UserModelCreateRequestData {
                connection_id: None,
                name: "fixture".into(),
                provider: "deepseek".into(),
                inference_protocol: Some(LlmProviderProtocol::OpenAiResponses),
                model: "fixture-model".into(),
                base_url: None,
                api_key: "test-key".into(),
                context_window: 128000,
                is_default: false,
            },
        )
        .await
        .unwrap();
    let admitted = service
        .admit_model_offering(owner.clone(), model.model_id.clone())
        .await
        .unwrap();
    let tools = vec![
        json!({"type":"function","function":{"name":"lookup","parameters":{"type":"object","properties":{}}}}),
    ];
    let attempts = Attempts::default();
    let mut history = vec![json!({"role":"user","content":"look up"})];
    let first = infer(&admitted, &history, &tools, &attempts).await.unwrap();
    history.push(
        json!({"role":"assistant","content":"","tool_calls":first.tool_calls,
        astra_core::model_wire::responses::CONTINUATION_KEY:first.provider_continuation.unwrap()}),
    );
    history.push(json!({"role":"tool","tool_call_id":"call-1","content":"found"}));
    assert_eq!(
        infer(&admitted, &history, &tools, &attempts)
            .await
            .unwrap()
            .full_text,
        "done"
    );
    let mut plain_history = vec![json!({"role":"user","content":"plain lookup"})];
    let plain = infer(&admitted, &plain_history, &tools, &attempts)
        .await
        .unwrap();
    plain_history.push(json!({"role":"assistant", "tool_calls":plain.tool_calls,
        astra_core::model_wire::responses::CONTINUATION_KEY:plain.provider_continuation.unwrap()}));
    plain_history.push(json!({"role":"tool", "tool_call_id":"call-1", "content":"found"}));
    assert_eq!(
        infer(&admitted, &plain_history, &tools, &attempts)
            .await
            .unwrap()
            .full_text,
        "done"
    );
    for content in [
        "missing reasoning",
        "private encoding",
        "unexpected namespace",
    ] {
        let before = requests.lock().unwrap().len();
        let invalid = [json!({"role":"user", "content":content})];
        let failure = infer(&admitted, &invalid, &tools, &attempts)
            .await
            .unwrap_err();
        assert!(failure.message.contains("incomplete response"));
        assert_eq!(requests.lock().unwrap().len(), before + 1);
    }
    service
        .update_user_model(
            owner.clone(),
            model.model_id.clone(),
            UserModelUpdateRequestData {
                api_key: Some("new-key".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let rotated = service
        .admit_model_offering(owner.clone(), model.model_id.clone())
        .await
        .unwrap();
    let before = requests.lock().unwrap().len();
    let failure = infer(&rotated, &history, &tools, &attempts)
        .await
        .unwrap_err();
    assert!(
        failure
            .details_json
            .unwrap()
            .contains("responses_continuation_scope_changed")
    );
    assert_eq!(requests.lock().unwrap().len(), before);
    history.push(json!({"role":"user","content":"continue with the new model"}));
    assert_eq!(
        infer(&rotated, &history, &tools, &attempts)
            .await
            .unwrap()
            .full_text,
        "done"
    );
    service
        .delete_user_model(owner, model.model_id)
        .await
        .unwrap();
    server.abort();
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
                inference_protocol: None,
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
    let build_host = |session: &str, ledger: ProviderObservationsFixture| {
        crate::server::server_loop_host::ServerAgenticLoopHostBuilder::new(
            settings.clone(),
            key.clone(),
            owner.clone(),
            session.to_owned(),
        )
        .with_provider_observations(ledger)
        .with_model_service(Some(Arc::new(service.clone())))
        .with_admitted_model_execution(Some(admitted.clone()))
        .with_turn_intent_policy(astra_services::runs::TurnIntentExecutionPolicy::FixedDefault)
        .with_skill_auto_route_policy(astra_services::runs::SkillAutoRouteExecutionPolicy::Disabled)
        .with_static_tool_catalog_admissible(true)
        .with_execution_binding_snapshot(ExecutionBindingSnapshot::inferred(
            WorkspaceBinding::server_sandbox(workspace.path()),
            ExecutorBinding::server_local(),
        ))
        .build()
    };
    let session = format!("capped-{}", uuid::Uuid::new_v4());
    let ledger = ProviderObservationsFixture::default();
    let mut host = build_host(&session, ledger.clone());
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

    provider.state.output_limit.store(false, Ordering::SeqCst);
    provider.state.requests.lock().unwrap().clear();
    *provider.state.terminal_output.lock().unwrap() = Some(json!([
        {"type":"message", "content":[{"type":"output_text", "text":"working"}]},
        {"type":"function_call", "name":"write_file", "call_id":"bad-call",
         "arguments":json!({"path":"malformed-effect.txt", "content":"executed"}).to_string()}
    ]));
    let invalid_session = format!("invalid-{}", uuid::Uuid::new_v4());
    let invalid_ledger = ProviderObservationsFixture::default();
    let mut invalid_host = build_host(&invalid_session, invalid_ledger.clone());
    let mut invalid_state = loop_state(&invalid_session, Vec::new(), "Write the requested file.");
    invalid_state.thinking = ThinkingConfig::ModelDefault;
    invalid_state.owner_user_id = Some(owner.clone());
    bind_server_workspace(&mut invalid_state, workspace.path()).await;
    invalid_state.skills.request_constraints.allowed_tools =
        Some(["write_file".to_owned()].into_iter().collect());
    let outcome = run_agentic_loop_with_host(&mut invalid_host, &mut invalid_state).await;
    assert!(outcome.is_err() || invalid_state.interruption.is_some());
    assert_eq!(invalid_state.total_tool_calls, 0);
    assert!(!workspace.path().join("malformed-effect.txt").exists());
    assert_eq!(
        provider.state.requests.lock().unwrap().len(),
        1,
        "outcome: {outcome:?}, interruption: {:?}",
        invalid_state.interruption
    );
    invalid_ledger.assert_quiescent();

    provider.state.scripted.store(false, Ordering::SeqCst);
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
            json!({"type":"function_call","name":"read_file","call_id":"provider-call"}),
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
        None,
    );
    assert_eq!(truncated.finish_reason.as_deref(), Some("length"));
    assert_eq!(truncated.full_text, "partial");
    // Without truncation evidence the stop/tool_calls distinction is kept.
    let stopped = super::result(
        ResponseOutput::default(),
        "conn",
        "model",
        "display-model",
        None,
    );
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
        None,
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
        None,
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
    let actual = result(output, "connection", "wire-model", "display-model", None);
    assert_eq!(actual.usage["input_tokens"], 30);
    assert_eq!(actual.usage["cached_input_tokens"], 40);
    assert_eq!(actual.usage["cache_creation_tokens"], 30);
    assert!(
        actual.usage_presence.fresh_input_tokens && actual.usage_presence.cache_creation_tokens
    );
}
#[test]
fn responses_usage_tracks_only_codec_accepted_bounded_fields() {
    let mut accumulator = ResponseAccumulator::default();
    let mut reported = serde_json::Map::new();
    let mut latest = None;
    let event = json!({"type":"response.created","response":{"usage":{"input_tokens":17,"extension":{"private":999}}}});
    super::observe_with_usage(&mut accumulator, &event, &mut reported, &mut latest, None).unwrap();
    let (_, presence) = latest.unwrap();
    assert_eq!(presence.measured_input_tokens, Some(17));
    assert!(!presence.output_tokens);
    let malformed = json!({"response":{"usage":{"input_tokens":999}}});
    assert!(
        super::observe_with_usage(
            &mut accumulator,
            &malformed,
            &mut reported,
            &mut latest,
            None
        )
        .is_err()
    );
    assert_eq!(latest.unwrap().1.measured_input_tokens, Some(17));
    let event = json!({"type":"response.failed","response":{"usage":{"input_tokens":-1,"output_tokens":5},"error":{"code":"failure"}}});
    assert!(
        super::observe_with_usage(&mut accumulator, &event, &mut reported, &mut latest, None)
            .is_err()
    );
    let (usage, presence) = latest.unwrap();
    assert!(presence.input_invalid);
    assert!(!presence.output_invalid);
    assert_eq!(usage.output_tokens, 5);
    assert!(presence.output_tokens);
    assert!(!reported.contains_key("extension"));
    assert!(reported.len() <= 2);
}

#[test]
fn responses_usage_stays_bounded_and_invalid_output_does_not_erase_input() {
    let mut accumulator = ResponseAccumulator::default();
    let mut reported = serde_json::Map::new();
    let mut latest = None;
    for index in 0..1000 {
        let event = json!({"type":"response.in_progress","response":{"usage":{
            "input_tokens":17, (format!("extra_{index}")): index,
            "input_tokens_details":{"cached_tokens":3,"ignored":index}
        }}});
        super::observe_with_usage(&mut accumulator, &event, &mut reported, &mut latest, None)
            .unwrap();
    }
    assert_eq!(reported.len(), 2);
    assert_eq!(
        reported["prompt_tokens_details"].as_object().unwrap().len(),
        1
    );
    let event = json!({"type":"response.in_progress","response":{"usage":{"output_tokens":-1}}});
    super::observe_with_usage(&mut accumulator, &event, &mut reported, &mut latest, None).unwrap();
    let (usage, presence) = latest.unwrap();
    assert_eq!(presence.measured_input_tokens, Some(17));
    assert!(presence.output_invalid);
    assert_eq!(usage.cached_input_tokens, 3);
}

#[test]
fn responses_usage_aliases_cannot_hide_invalid_or_conflicting_evidence() {
    for (raw, input_valid, output_valid) in [
        (
            json!({"input_tokens":17,"prompt_tokens":-1,"output_tokens":3}),
            false,
            true,
        ),
        (
            json!({"input_tokens":17,"prompt_tokens":18,"output_tokens":3}),
            false,
            true,
        ),
        (
            json!({"input_tokens":17,"output_tokens":3,"completion_tokens":-1}),
            true,
            false,
        ),
        (
            json!({"input_tokens":17,"output_tokens":3,"completion_tokens":4}),
            true,
            false,
        ),
        (
            json!({"input_tokens":17,"prompt_tokens":null,"output_tokens":3,"completion_tokens":3}),
            true,
            true,
        ),
        (
            json!({"input_tokens":17,"output_tokens":3,"input_tokens_details":{"cached_tokens":1},"prompt_tokens_details":{"cached_tokens":-1}}),
            false,
            true,
        ),
        (
            json!({"input_tokens":17,"output_tokens":3,"input_tokens_details":null,"prompt_tokens_details":{"cached_tokens":1}}),
            true,
            true,
        ),
    ] {
        let mut accumulator = ResponseAccumulator::default();
        let mut reported = serde_json::Map::new();
        let mut latest = None;
        let observer = Attempts::default();
        let event = json!({"type":"response.in_progress","response":{"usage":raw}});
        super::observe_with_usage(
            &mut accumulator,
            &event,
            &mut reported,
            &mut latest,
            Some((&observer, 0)),
        )
        .unwrap();
        let (_, presence) = observer.snapshots.lock().unwrap()[0];
        assert_eq!(!presence.input_invalid, input_valid, "{event}");
        assert_eq!(!presence.output_invalid, output_valid, "{event}");
        assert_eq!(
            presence.measured_input_tokens,
            input_valid.then_some(17),
            "{event}"
        );
        assert_eq!(presence.output_tokens, output_valid, "{event}");
    }
}
