use super::*;
use crate::turn::llm::client::{
    LlmCall, LlmCancel, LlmExecutionRoute, call_llm_and_collect, call_llm_nonstream,
    global_llm_client,
};
use astra_turn_core::thinking_config::ThinkingConfig;
use axum::{Router, body::Body, response::Response, routing::post};

// The incident exposed only upstream_forbidden. The diagnostic text supplied
// the billing explanation, so it must not become a machine-readable quota fact.
// All messages here are synthetic; no incident/account details are retained.
#[tokio::test]
async fn provider_rejection_http_to_run_terminal_contract() {
    for (status, body, expected, contract) in [
        (
            403,
            json!({"error":{"code":"upstream_forbidden","message":"预扣费额度失败 private-diagnostic"}}),
            "provider_forbidden",
            false,
        ),
        (
            403,
            json!({"error":{"code":"unknown","message":"insufficient quota private-diagnostic"}}),
            "provider_forbidden",
            false,
        ),
        (
            403,
            json!({"error":{"code":"insufficient_quota","message":"private-diagnostic"}}),
            "provider_quota_exhausted",
            false,
        ),
        (
            429,
            json!({"error":{"code":"insufficient_quota","message":"private-diagnostic"}}),
            "provider_quota_exhausted",
            false,
        ),
        (
            403,
            json!({"error":{"code":"invalid_api_key","message":"private-diagnostic"}}),
            "auth",
            false,
        ),
        (
            401,
            json!({"error":{"message":"private-diagnostic"}}),
            "auth",
            false,
        ),
        (
            403,
            json!({"error":{"code":"provider_forbidden","message":"private-diagnostic","retryable":false}}),
            "provider_forbidden",
            true,
        ),
        (
            403,
            json!({"error":{"code":"provider_quota_exhausted","message":"private-diagnostic","retryable":false}}),
            "provider_quota_exhausted",
            true,
        ),
        (
            401,
            json!({"error":{"code":"provider_authentication_failed","message":"private-diagnostic","retryable":false}}),
            "auth",
            true,
        ),
        (
            402,
            json!({"error":{"code":"insufficient_credit","message":"private-diagnostic","retryable":false}}),
            "unknown",
            true,
        ),
    ] {
        let expected_code = if contract {
            body["error"]["code"].as_str().unwrap().to_owned()
        } else {
            expected.to_owned()
        };
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let captured = hits.clone();
        let app = Router::new().route(
            "/chat/completions",
            post(move || {
                let body = body.to_string();
                let hits = captured.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    let mut response = Response::builder().status(status);
                    if contract {
                        response = response.header(
                            astra_services::models::MOI_MODEL_GATEWAY_ERROR_CONTRACT_HEADER,
                            "v1",
                        );
                    }
                    response.body(Body::from(body)).unwrap()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        for streaming in [true, false] {
            struct ProviderHost {
                base: String,
                streaming: bool,
                contract: bool,
                tools: std::collections::HashSet<String>,
            }
            #[async_trait]
            impl AgenticLoopHost for ProviderHost {
                async fn execute_turn(
                    &mut self,
                    _: &mut AgenticLoopState,
                ) -> Result<
                    crate::turn::agentic_loop::host::HostTurnResult,
                    astra_core::ClassifiedError,
                > {
                    let messages = [json!({"role":"user","content":"fixture"})];
                    let headers = std::collections::HashMap::from([(
                        astra_services::models::MOI_MODEL_GATEWAY_ERROR_CONTRACT_HEADER.to_owned(),
                        "v1".to_owned(),
                    )]);
                    let call = LlmCall {
                        purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
                        messages: &messages,
                        tools: &[],
                        cache_capability: None,
                        route: LlmExecutionRoute {
                            usage_attestation: None,
                            inference_protocol: None,
                            response_scope: None,
                            subscription: None,
                            fixed_temperature: None,
                            supported_reasoning_efforts: None,
                            thinking_protocol: None,
                            model_name: "fixture",
                            wire_model_name: None,
                            api_key: "fixture",
                            base_url: &self.base,
                            provider: "openai",
                            header_overrides: self.contract.then_some(&headers),
                            request_body_overrides: None,
                            completions_url_override: None,
                            request_timeout: None,
                        },
                        max_output_tokens: None,
                        temperature: None,
                        thinking: &ThinkingConfig::Off,
                    };
                    if self.streaming {
                        call_llm_and_collect(call, LlmCancel::None).await?;
                    } else {
                        call_llm_nonstream(global_llm_client(), call, Duration::from_secs(5))
                            .await?;
                    }
                    unreachable!("fixture must reject inference")
                }
                fn emit_headless_line(
                    &mut self,
                    _: crate::turn::agentic::headless_round::HeadlessStderrStyle,
                    _: String,
                ) {
                }
                fn is_quiet(&self) -> bool {
                    true
                }
                fn valid_tool_names(&self) -> &std::collections::HashSet<String> {
                    &self.tools
                }
            }
            let mut host = ProviderHost {
                base: base.clone(),
                streaming,
                contract,
                tools: Default::default(),
            };
            let mut state = crate::turn::agentic_loop::host::make_test_loop_state();
            let result = run_agentic_loop_with_host_panic_safe(&mut host, &mut state).await;
            let error = result
                .as_ref()
                .expect_err("provider rejection must fail the loop");
            assert_eq!(
                error.kind.as_str(),
                expected,
                "HTTP {status}, streaming={streaming}"
            );
            assert!(!format!("{error:?}").contains("private-diagnostic"));
            let (events, status, _) =
                AgenticRunLifecycleService::finalize_run_events(result, vec![], &state);
            assert_eq!(status, RunStatus::Failed);
            for event_type in ["run_error", "run_finished"] {
                let event = events
                    .iter()
                    .find(|event| event["event_type"] == event_type)
                    .unwrap();
                assert_eq!(event["data"]["error_code"], expected_code);
                assert_eq!(event["data"]["error_kind"], expected);
            }
            assert!(
                !events
                    .iter()
                    .any(|event| event["event_type"] == "run_interrupted")
            );
        }
        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "a terminal rejection must not be retried"
        );
        server.abort();
    }
}
