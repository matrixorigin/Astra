#[cfg(test)]
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use astra_turn_core::cloud_summary::{SummaryLlmClient, SummaryResponse};
use astra_turn_core::thinking_config::ThinkingConfig;
#[cfg(test)]
use astra_turn_core::thinking_config::ThinkingEffort;
use astra_turn_types::InferencePurpose;

use super::client::{LlmCall, OwnedLlmExecutionRoute};
use super::durable::DurableInferenceRunAuthority;
use super::run_scoped::RunScopedProviderObserver;

use super::client::{auxiliary_execution_budget, global_llm_client, llm_nonstream_timeout};

/// Final temperature decision for one auxiliary inference call.
///
/// This is deliberately distinct from `Option<f64>`: both an inherited route
/// default and an asserted provider default are represented by `None` at the
/// lower-level client boundary, but only the former may contain a configured
/// body override. Resolution below validates that distinction before provider
/// I/O.
#[derive(Clone, Copy, Debug, PartialEq)]
enum SummaryTemperatureEmission {
    InheritRouteDefault,
    ProviderDefault,
    Forbidden,
    Explicit(f64),
}

impl SummaryTemperatureEmission {
    fn call_temperature(self) -> Option<f64> {
        match self {
            Self::Explicit(value) => Some(value),
            Self::InheritRouteDefault | Self::ProviderDefault | Self::Forbidden => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::InheritRouteDefault => "inherit_route_default",
            Self::ProviderDefault => "provider_default",
            Self::Forbidden => "forbidden",
            Self::Explicit(_) => "explicit",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SummaryGenerationPolicyProvenance {
    ExistingPurposePolicy,
    OfferingCapability,
    CanonicalProviderContract,
    ConservativeProtocolDefault,
}

impl SummaryGenerationPolicyProvenance {
    fn as_str(self) -> &'static str {
        match self {
            Self::ExistingPurposePolicy => "existing_purpose_policy",
            Self::OfferingCapability => "offering_capability",
            Self::CanonicalProviderContract => "canonical_provider_contract",
            Self::ConservativeProtocolDefault => "conservative_protocol_default",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct ResolvedSummaryGenerationPolicy {
    thinking: ThinkingConfig,
    temperature: SummaryTemperatureEmission,
    temperature_provenance: SummaryGenerationPolicyProvenance,
}

/// Runtime-owned adapter from the provider execution contract to summary work.
/// Provider-specific request construction, authentication, timeouts, and
/// response parsing remain centralized in the canonical LLM client.
#[derive(Clone)]
pub(crate) struct RuntimeSummaryClient {
    route: OwnedLlmExecutionRoute,
    max_output_tokens: usize,
    prompt_cache_tools: Vec<Value>,
    cache_capability: Option<astra_turn_core::cache_placement::CacheCapability>,
    authority: DurableInferenceRunAuthority,
    #[cfg(any(test, feature = "e2e-hooks"))]
    observations: Option<(
        crate::server::provider_test_support::ProviderObservationsFixture,
        astra_turn_types::InferenceInvocationScope,
    )>,
}

impl RuntimeSummaryClient {
    #[must_use]
    pub(crate) fn new(
        route: OwnedLlmExecutionRoute,
        max_output_tokens: usize,
        authority: DurableInferenceRunAuthority,
    ) -> Self {
        Self {
            route,
            max_output_tokens,
            prompt_cache_tools: Vec::new(),
            cache_capability: None,
            authority,
            #[cfg(any(test, feature = "e2e-hooks"))]
            observations: None,
        }
    }

    #[cfg(any(test, feature = "e2e-hooks"))]
    pub(crate) fn with_observations(
        mut self,
        observations: crate::server::provider_test_support::ProviderObservationsFixture,
        scope: astra_turn_types::InferenceInvocationScope,
    ) -> Self {
        self.observations = Some((observations, scope));
        self
    }

    /// Reuse the main inference request's stable tool projection and exact
    /// deployment cache capability for an inline compaction call. Auxiliary
    /// callers that do not share a main-request prefix keep the empty default.
    #[must_use]
    pub(crate) fn with_prompt_cache_context(
        mut self,
        tools: Vec<Value>,
        cache_capability: astra_turn_core::cache_placement::CacheCapability,
    ) -> Self {
        self.prompt_cache_tools = tools;
        self.cache_capability = Some(cache_capability);
        self
    }

    /// Auxiliary semantic decisions need a short, predictable response. Some
    /// models cannot turn reasoning off but do offer an explicit low-effort
    /// control. Use only admitted capability facts or Astra's maintained
    /// canonical-provider transition contract; never infer from a model name.
    fn thinking_for(_purpose: InferencePurpose, route: &OwnedLlmExecutionRoute) -> ThinkingConfig {
        let protocol = route.thinking_protocol.unwrap_or_else(|| {
            astra_core::model_wire::thinking::canonical_thinking_protocol(
                &route.provider,
                &route.base_url,
                route
                    .wire_model_name
                    .as_deref()
                    .unwrap_or(&route.model_name),
            )
        });
        ThinkingConfig::bounded_auxiliary(
            &route.provider,
            route
                .thinking_capability
                .map(|capability| capability.as_str()),
            protocol,
            route.supported_reasoning_efforts.as_deref(),
        )
    }

    fn configured_temperature(route: &OwnedLlmExecutionRoute) -> Result<Option<f64>, String> {
        astra_core::model_wire::thinking::configured_temperature(
            route.request_body_overrides.as_ref(),
            route.fixed_temperature,
        )
    }

    /// Resolve the complete bounded-summary generation policy once from the
    /// admitted route. Generic OpenAI-compatible transport is not proof that
    /// zero temperature is supported, so an unclassified route uses the
    /// endpoint default. Exact built-in providers retain their established
    /// deterministic classifier behavior.
    fn resolve_generation_policy(
        purpose: InferencePurpose,
        route: &OwnedLlmExecutionRoute,
    ) -> Result<ResolvedSummaryGenerationPolicy, String> {
        let thinking = Self::thinking_for(purpose, route);
        let configured_temperature = Self::configured_temperature(route)?;
        let protocol = route.thinking_protocol.unwrap_or_else(|| {
            astra_core::model_wire::thinking::canonical_thinking_protocol(
                &route.provider,
                &route.base_url,
                route
                    .wire_model_name
                    .as_deref()
                    .unwrap_or(&route.model_name),
            )
        });
        let (temperature, temperature_provenance) =
            if !matches!(purpose, InferencePurpose::Introspection) {
                (
                    SummaryTemperatureEmission::InheritRouteDefault,
                    SummaryGenerationPolicyProvenance::ExistingPurposePolicy,
                )
            } else if matches!(
                thinking,
                ThinkingConfig::Enabled { .. } | ThinkingConfig::Adaptive { .. }
            ) && (!protocol.can_disable()
                || matches!(route.provider.as_str(), "anthropic" | "bedrock"))
            {
                (
                    SummaryTemperatureEmission::Forbidden,
                    SummaryGenerationPolicyProvenance::OfferingCapability,
                )
            } else if let Some(value) = configured_temperature {
                (
                    SummaryTemperatureEmission::Explicit(value),
                    SummaryGenerationPolicyProvenance::OfferingCapability,
                )
            } else if protocol == astra_core::model_wire::thinking::ThinkingProtocol::Moonshot {
                (
                    SummaryTemperatureEmission::ProviderDefault,
                    SummaryGenerationPolicyProvenance::CanonicalProviderContract,
                )
            } else if route.completions_url_override.is_none()
                && astra_core::model_wire::thinking::canonical_zero_temperature(
                    &route.provider,
                    &route.base_url,
                )
            {
                (
                    SummaryTemperatureEmission::Explicit(0.0),
                    SummaryGenerationPolicyProvenance::CanonicalProviderContract,
                )
            } else {
                (
                    SummaryTemperatureEmission::ProviderDefault,
                    SummaryGenerationPolicyProvenance::ConservativeProtocolDefault,
                )
            };
        Ok(ResolvedSummaryGenerationPolicy {
            thinking,
            temperature,
            temperature_provenance,
        })
    }
}

#[async_trait]
impl SummaryLlmClient for RuntimeSummaryClient {
    async fn summarize(
        &self,
        purpose: InferencePurpose,
        messages: &[Value],
    ) -> Result<SummaryResponse, astra_core::ClassifiedError> {
        let contract_error = |message: String| {
            astra_core::ClassifiedError::new(astra_core::ErrorKind::ContractViolation, message)
        };
        let policy =
            Self::resolve_generation_policy(purpose, &self.route).map_err(contract_error)?;
        let thinking = &policy.thinking;
        let temperature = policy.temperature.call_temperature();
        tracing::debug!(
            target: "astra::inference_policy",
            purpose = purpose.as_str(),
            temperature_emission = policy.temperature.as_str(),
            temperature_value = ?temperature,
            temperature_provenance = policy.temperature_provenance.as_str(),
            "resolved auxiliary generation policy"
        );
        let observer = RunScopedProviderObserver::new(self.authority.clone());
        #[cfg(any(test, feature = "e2e-hooks"))]
        let observer = match self.observations.clone() {
            Some((observations, scope)) => observer.with_observations(observations, scope),
            None => observer,
        };
        let call = LlmCall {
            purpose,
            messages,
            tools: &self.prompt_cache_tools,
            cache_capability: self.cache_capability,
            route: self.route.borrowed(),
            max_output_tokens: Some(self.max_output_tokens),
            temperature,
            thinking,
        };
        let request_id = uuid::Uuid::new_v4().to_string();
        let execute = async {
            if self.route.provider == "typesafe" {
                super::client::call_llm_nonstream_with_attempt_observer_and_tool_choice(
                    global_llm_client(),
                    call,
                    auxiliary_execution_budget(purpose, llm_nonstream_timeout()),
                    Some(&observer),
                    super::client::RuntimeToolChoice::None,
                )
                .await
            } else {
                let prepared = super::client::prepare_streaming_call(
                    call,
                    super::client::RuntimeToolChoice::None,
                )?;
                super::client::call_prepared_llm_and_collect_with_stream_callback(
                    prepared,
                    super::client::LlmCancel::None,
                    None,
                    Some(&observer),
                    None,
                )
                .await
            }
        };
        // The Run already owns cancellation, lease loss and deadline. Model
        // calls borrow that authority instead of creating another DB lease.
        let result = tokio::select! {
            biased;
            error = self.authority.wait_for_local_fence("auxiliary model request") => Err(error),
            result = execute => result,
        };
        let execution_provenance =
            Some(astra_turn_core::cloud_summary::SummaryExecutionProvenance {
                invocation_id: request_id,
                model_name: self.route.model_name.clone(),
                provider: self.route.provider.clone(),
            });
        let qualified_usage = observer
            .facts()
            .into_iter()
            .filter_map(|fact| fact.qualified_usage)
            .reduce(|left, right| {
                left.checked_add(right).unwrap_or_else(|_| {
                    astra_turn_types::CanonicalTokenUsage::new(None, None, None, None)
                        .expect("unknown usage is valid")
                })
            });
        match result {
            // A no-tool provider response that nevertheless contains a native
            // or degraded call is a completed transport with invalid
            // structured output, not a network failure. Return an empty typed
            // payload so schema-owning callers can take their one bounded
            // repair path. No call from this private adapter is executable.
            Ok(result) if !result.tool_calls.is_empty() || result.full_text.trim().is_empty() => {
                Ok(SummaryResponse {
                    judgment_provenance: result.judgment_provenance,
                    model_used: result.model_used,
                    text: String::new(),
                    is_ptl_error: false,
                    finish_reason: result.effective_finish_reason.or(result.finish_reason),
                    usage: result.usage,
                    qualified_usage,
                    execution: execution_provenance,
                })
            }
            Ok(result) => Ok(SummaryResponse {
                judgment_provenance: result.judgment_provenance,
                model_used: result.model_used,
                text: result.full_text,
                is_ptl_error: false,
                finish_reason: result.effective_finish_reason.or(result.finish_reason),
                usage: result.usage,
                qualified_usage,
                execution: execution_provenance,
            }),
            Err(error) if error.kind == astra_core::ErrorKind::ContextWindow => {
                Ok(SummaryResponse {
                    judgment_provenance: None,
                    model_used: String::new(),
                    text: String::new(),
                    is_ptl_error: true,
                    finish_reason: None,
                    usage: serde_json::Map::new(),
                    qualified_usage,
                    // The local request id can identify the invocation even
                    // when the provider rejected the request for context
                    // length. Keep that identity for semantic diagnostics;
                    // it does not turn the failed response into a decision.
                    execution: execution_provenance,
                })
            }
            Err(error) => {
                let Some(usage) = qualified_usage else {
                    return Err(error);
                };
                let mut details = error
                    .details_json
                    .as_deref()
                    .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                    .filter(Value::is_object)
                    .unwrap_or_else(|| serde_json::json!({}));
                details["qualified_usage"] = usage.to_json();
                Err(error.with_details_json(details.to_string()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU32, Ordering};

    use axum::{Router, body::Body, response::Response, routing::post};

    use super::*;

    fn test_client(route: OwnedLlmExecutionRoute, max: usize) -> RuntimeSummaryClient {
        RuntimeSummaryClient::new(route, max, summary_authority())
    }

    fn summary_provider_response(mut response: Value) -> Response {
        for choice in response["choices"].as_array_mut().unwrap() {
            let mut delta = choice.as_object_mut().unwrap().remove("message").unwrap();
            if let Some(calls) = delta.get_mut("tool_calls").and_then(Value::as_array_mut) {
                for (index, call) in calls.iter_mut().enumerate() {
                    call["index"] = serde_json::json!(index);
                }
            }
            choice["delta"] = delta;
            choice["index"] = serde_json::json!(0);
        }
        Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from(format!("data: {response}\n\ndata: [DONE]\n\n")))
            .unwrap()
    }

    async fn spawn_summary_test_server(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind summary test server");
        let address = listener.local_addr().expect("summary test address");
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve summary test app");
        });
        format!("http://{address}")
    }

    async fn spawn_counted_summary_server(
        provider_requests: Arc<AtomicU32>,
        delay: std::time::Duration,
    ) -> String {
        let app = Router::new().route(
            "/chat/completions",
            post(move || {
                let provider_requests = provider_requests.clone();
                async move {
                    provider_requests.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(delay).await;
                    let event = serde_json::json!({
                        "choices": [{
                            "delta": {"content": "summary"},
                            "finish_reason": "stop"
                        }]
                    });
                    Response::builder()
                        .status(200)
                        .header("content-type", "text/event-stream")
                        .body(Body::from(format!("data: {event}\n\ndata: [DONE]\n\n")))
                        .expect("summary provider response")
                }
            }),
        );
        spawn_summary_test_server(app).await
    }

    fn summary_execution(base_url: String) -> astra_services::AdmittedModelExecution {
        astra_services::AdmittedModelExecution {
            subscription: None,
            admitted_at: std::time::Instant::now(),
            price_snapshot: None,
            offering_id: "summary-offering".to_string(),
            source_identity: None,
            access_kind: astra_services::ModelAccessKind::SelfHosted,
            execution_placement: astra_services::ModelExecutionPlacement::Server,
            model_name: "summary-model".to_string(),
            wire_model_name: None,
            api_key: "summary-key".to_string(),
            base_url,
            provider: "openai".to_string(),
            cache_capability: None,
            thinking_capability: None,
            fixed_temperature: None,
            supported_reasoning_efforts: None,
            thinking_protocol: None,
            request_body_overrides: None,
            context_window: Some(8_192),
            max_completion_tokens: Some(1_024),
            header_overrides: HashMap::new(),
            completions_url_override: None,
            request_timeout_ms: None,
        }
    }

    fn summary_route(execution: &astra_services::AdmittedModelExecution) -> OwnedLlmExecutionRoute {
        OwnedLlmExecutionRoute {
            subscription: execution.subscription.clone(),
            model_name: execution.model_name.clone(),
            wire_model_name: None,
            api_key: execution.api_key.clone(),
            base_url: execution.base_url.clone(),
            provider: execution.provider.clone(),
            thinking_capability: None,
            fixed_temperature: execution.fixed_temperature,
            supported_reasoning_efforts: execution.supported_reasoning_efforts.clone(),
            thinking_protocol: execution.thinking_protocol,
            header_overrides: HashMap::new(),
            request_body_overrides: None,
            completions_url_override: None,
            request_timeout: None,
        }
    }

    #[tokio::test]
    async fn typesafe_request_judgment_uses_run_authority_and_reports_usage() {
        let app = Router::new().route("/v1/systemone", post(|axum::Json(body): axum::Json<Value>| async move {
            assert_eq!(body["model"], "configured-jev");
            assert!(body.get("messages").is_none());
            let answers: serde_json::Map<String, Value> = body["questions"].as_object().unwrap().keys().map(|key| {
                let yes = matches!(key.as_str(), "mutation.read_only" | "scope.unknown" | "domain.none");
                (key.clone(), serde_json::json!({"type":"noul", "noul": if yes { 1.0 } else { 0.0 }}))
            }).collect();
            axum::Json(serde_json::json!({"model":"configured-jev", "answers":answers,"assessment":{"difficulty":"easy","difficulty_confidence":"high"},"usage":{"input_tokens":123,"output_tokens":19}}))
        }));
        let mut execution = summary_execution(spawn_summary_test_server(app).await);
        execution.provider = "typesafe".into();
        execution.model_name = "configured-jev".into();
        execution.offering_id = "configured-judgment-offering".into();
        let client =
            RuntimeSummaryClient::new(summary_route(&execution), 1024, summary_authority());
        let request = astra_services::work_admission_classification_request(&Default::default());
        let response = client
            .summarize(
                InferencePurpose::Introspection,
                &astra_services::work_admission_classification_messages(&request),
            )
            .await
            .unwrap();
        let classification = astra_services::parse_work_admission_classification(
            &request,
            &response.text,
            &response.model_used,
            response.judgment_provenance,
        )
        .unwrap();
        let decision = classification.into_not_required().unwrap();
        assert_eq!(
            decision.assessment(),
            Some(astra_turn_types::TurnAssessment {
                difficulty: astra_turn_types::TaskDifficulty::Easy,
                difficulty_confidence: astra_turn_types::AssessmentConfidence::High,
                ..Default::default()
            })
        );
        assert_eq!(response.usage["input_tokens"], 123);
        assert_eq!(response.usage["output_tokens"], 19);
        let provenance = response.execution.expect("execution provenance");
        assert_eq!(provenance.model_name, "configured-jev");
        assert_eq!(provenance.provider, "typesafe");
        assert!(uuid::Uuid::parse_str(&provenance.invocation_id).is_ok());
    }

    #[tokio::test]
    async fn streaming_judgment_source_is_execution_owned_not_answer_owned() {
        let request = astra_services::work_admission_classification_request(&Default::default());
        let native_answers = request
            .questions
            .keys()
            .map(|id| (id.clone(), serde_json::json!({"type":"noul", "noul":0.99})))
            .collect::<serde_json::Map<_, _>>();
        let discrete_answers = request
            .questions
            .keys()
            .map(|id| {
                (
                    id.clone(),
                    serde_json::json!(if id == "mutation.read_only" {
                        "yes"
                    } else {
                        "no"
                    }),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        for (raw, valid) in [
            (serde_json::json!({"answers":discrete_answers}).to_string(), true),
            (serde_json::json!({"schema_version":astra_turn_types::JUDGMENT_SCHEMA_VERSION,"model":"forged-jev","answers":native_answers}).to_string(), false),
        ] {
          for provider in ["openai", "bedrock"] {
            let calls = Arc::new(AtomicU32::new(0));
            let observed_calls = calls.clone();
            let raw = raw.clone();
            let app = Router::new().fallback(move || {
                let raw = raw.clone();
                let calls = observed_calls.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    if provider == "bedrock" {
                        use crate::server::provider_test_support::eventstream_frame;
                        let mut frames = eventstream_frame("messageStart", br#"{"role":"assistant"}"#);
                        frames.extend(eventstream_frame("contentBlockDelta", serde_json::json!({"contentBlockIndex":0,"delta":{"text":raw}}).to_string().as_bytes()));
                        frames.extend(eventstream_frame("messageStop", br#"{"stopReason":"end_turn"}"#));
                        frames.extend(eventstream_frame("metadata", br#"{"usage":{"inputTokens":42,"outputTokens":7,"totalTokens":49}}"#));
                        return Response::builder().header("content-type", "application/vnd.amazon.eventstream").body(Body::from(frames)).unwrap();
                    }
                    let event = serde_json::json!({"choices":[{"index":0,"delta":{"content":raw},"finish_reason":"stop"}]});
                    Response::builder().header("content-type", "text/event-stream")
                        .body(Body::from(format!("data: {event}\n\ndata: [DONE]\n\n"))).unwrap()
                }
            });
            let mut execution = summary_execution(spawn_summary_test_server(app).await);
            execution.provider = provider.into();
            let client = RuntimeSummaryClient::new(
                summary_route(&execution), 1024, summary_authority(),
            );
            let response = client.summarize(InferencePurpose::Introspection,
                &astra_services::work_admission_classification_messages(&request),
            ).await.unwrap();
            assert_eq!(response.judgment_provenance, Some(astra_turn_types::JudgmentResponseProvenance::DiscreteDecision));
            assert_eq!(response.model_used, "summary-model");
            let decoded = astra_services::parse_work_admission_classification(
                &request, &response.text, &response.model_used, response.judgment_provenance,
            );
            assert_eq!(decoded.is_ok(), valid);
            assert_eq!(calls.load(Ordering::SeqCst), 1, "invalid provenance must not trigger another inference");
          }
        }
    }

    fn summary_authority() -> super::super::durable::DurableInferenceRunAuthority {
        super::super::durable::DurableInferenceRunAuthority::new(
            0,
            "summary-owner",
            0,
            None,
            None,
            None,
        )
    }

    fn route_with_capability(
        thinking_capability: Option<astra_services::models::ThinkingCapability>,
    ) -> OwnedLlmExecutionRoute {
        OwnedLlmExecutionRoute {
            subscription: None,
            model_name: "test-model".to_string(),
            wire_model_name: None,
            api_key: String::new(),
            base_url: "https://example.invalid/v1".to_string(),
            provider: "openai".to_string(),
            thinking_capability,
            fixed_temperature: None,
            supported_reasoning_efforts: None,
            thinking_protocol: None,
            header_overrides: HashMap::new(),
            request_body_overrides: None,
            completions_url_override: None,
            request_timeout: None,
        }
    }

    fn deepseek_effort_only_route() -> OwnedLlmExecutionRoute {
        let mut route =
            route_with_capability(Some(astra_services::models::ThinkingCapability::EffortOnly));
        route.base_url = "https://api.deepseek.com".to_string();
        route
    }

    #[test]
    fn bounded_introspection_uses_low_effort_only_when_model_admission_proves_support() {
        let mut effort_only =
            route_with_capability(Some(astra_services::models::ThinkingCapability::EffortOnly));
        assert_eq!(
            RuntimeSummaryClient::thinking_for(InferencePurpose::Introspection, &effort_only),
            ThinkingConfig::ModelDefault,
            "unknown protocol cannot prove an explicit control"
        );
        effort_only.thinking_protocol =
            Some(astra_core::model_wire::thinking::ThinkingProtocol::ReasoningEffort);
        assert_eq!(
            RuntimeSummaryClient::thinking_for(InferencePurpose::Introspection, &effort_only),
            ThinkingConfig::Adaptive {
                effort: ThinkingEffort::Low,
            }
        );
        effort_only.thinking_protocol = None;
        effort_only.base_url = "https://api.openai.com/v1".into();
        assert_eq!(
            RuntimeSummaryClient::thinking_for(InferencePurpose::Introspection, &effort_only),
            ThinkingConfig::Adaptive {
                effort: ThinkingEffort::Low
            },
            "canonical fallback must be shared by both EffortOnly branches"
        );

        for capability in [
            None,
            Some(astra_services::models::ThinkingCapability::Both),
            Some(astra_services::models::ThinkingCapability::NativeOnly),
            Some(astra_services::models::ThinkingCapability::None),
        ] {
            let route = route_with_capability(capability);
            let expected = if capability == Some(astra_services::models::ThinkingCapability::None) {
                ThinkingConfig::Off
            } else {
                ThinkingConfig::ModelDefault
            };
            assert_eq!(
                RuntimeSummaryClient::thinking_for(InferencePurpose::Introspection, &route),
                expected,
                "unknown protocol must not receive an invented control"
            );
            assert_eq!(
                RuntimeSummaryClient::thinking_for(InferencePurpose::RequiredCompaction, &route),
                expected,
                "all auxiliary purposes share the admitted control contract"
            );
        }
    }

    #[test]
    fn bounded_introspection_preserves_effort_only_without_inventing_suppression() {
        assert_eq!(
            RuntimeSummaryClient::thinking_for(
                InferencePurpose::Introspection,
                &deepseek_effort_only_route(),
            ),
            ThinkingConfig::Adaptive {
                effort: ThinkingEffort::Low
            },
            "an effort-only capability never authorizes Off"
        );
    }

    #[test]
    fn bounded_introspection_resolves_temperature_from_admitted_capabilities() {
        let canonical = route_with_capability(None);
        for (provider, base_url) in [
            ("openai", "https://api.openai.com/v1"),
            ("anthropic", "https://api.anthropic.com/v1"),
            ("deepseek", "https://api.deepseek.com"),
        ] {
            let mut route = canonical.clone();
            route.provider = provider.to_string();
            route.base_url = base_url.into();
            let policy = RuntimeSummaryClient::resolve_generation_policy(
                InferencePurpose::Introspection,
                &route,
            )
            .expect("canonical policy");
            assert_eq!(
                policy.temperature,
                SummaryTemperatureEmission::Explicit(0.0),
                "provider={provider}"
            );
            assert_eq!(
                policy.temperature_provenance,
                SummaryGenerationPolicyProvenance::CanonicalProviderContract,
                "provider={provider}"
            );
        }

        let mut compatible = route_with_capability(None);
        compatible.provider = astra_services::byok_endpoint::COMPATIBLE_PROVIDER.to_string();
        let compatible_policy = RuntimeSummaryClient::resolve_generation_policy(
            InferencePurpose::Introspection,
            &compatible,
        )
        .expect("compatible policy");
        assert_eq!(
            compatible_policy.temperature,
            SummaryTemperatureEmission::ProviderDefault,
            "transport compatibility alone must not imply zero-temperature support"
        );
        assert_eq!(
            compatible_policy.temperature_provenance,
            SummaryGenerationPolicyProvenance::ConservativeProtocolDefault
        );

        compatible.fixed_temperature = Some(0.6);
        assert_eq!(
            RuntimeSummaryClient::resolve_generation_policy(
                InferencePurpose::Introspection,
                &compatible,
            )
            .expect("fixed-temperature policy")
            .temperature,
            SummaryTemperatureEmission::Explicit(0.6)
        );

        compatible.fixed_temperature = None;
        compatible.request_body_overrides = Some(serde_json::Map::from_iter([(
            "temperature".to_string(),
            serde_json::json!(0.7),
        )]));
        assert_eq!(
            RuntimeSummaryClient::resolve_generation_policy(
                InferencePurpose::Introspection,
                &compatible,
            )
            .expect("configured-temperature policy")
            .temperature,
            SummaryTemperatureEmission::Explicit(0.7),
            "the admitted override must not be silently replaced with zero"
        );

        let mut effort_only =
            route_with_capability(Some(astra_services::models::ThinkingCapability::EffortOnly));
        effort_only.thinking_protocol =
            Some(astra_core::model_wire::thinking::ThinkingProtocol::ReasoningEffort);
        assert_eq!(
            RuntimeSummaryClient::resolve_generation_policy(
                InferencePurpose::Introspection,
                &effort_only,
            )
            .expect("effort-only policy")
            .temperature,
            SummaryTemperatureEmission::Forbidden,
            "thinking protocols own sampling and must not receive temperature"
        );

        for purpose in [
            InferencePurpose::PrimaryAgent,
            InferencePurpose::RequiredCompaction,
            InferencePurpose::MemoryExtraction,
        ] {
            assert_eq!(
                RuntimeSummaryClient::resolve_generation_policy(purpose, &canonical)
                    .expect("non-introspection policy")
                    .temperature,
                SummaryTemperatureEmission::InheritRouteDefault
            );
        }
    }

    #[test]
    fn provider_labels_and_completion_overrides_do_not_prove_zero_temperature() {
        for provider in [
            "openai",
            "anthropic",
            "deepseek",
            "bedrock",
            "dashscope",
            "moonshot",
            "openrouter",
        ] {
            for url in [
                "https://api.moonshot.cn/v1",
                "https://dashscope.aliyuncs.com/compatible-mode/v1",
                "https://gateway.example/v1",
            ] {
                let mut route = route_with_capability(None);
                route.provider = provider.into();
                route.base_url = url.into();
                assert_eq!(
                    RuntimeSummaryClient::resolve_generation_policy(
                        InferencePurpose::Introspection,
                        &route
                    )
                    .unwrap()
                    .temperature,
                    SummaryTemperatureEmission::ProviderDefault,
                    "{provider} {url}"
                );
            }
        }
        let mut route = route_with_capability(None);
        route.base_url = "https://api.openai.com/v1".into();
        route.completions_url_override = Some("https://gateway.example/chat/completions".into());
        assert_eq!(
            RuntimeSummaryClient::resolve_generation_policy(
                InferencePurpose::Introspection,
                &route
            )
            .unwrap()
            .temperature,
            SummaryTemperatureEmission::ProviderDefault
        );
    }

    #[test]
    fn contradictory_or_invalid_temperature_capabilities_fail_before_provider_io() {
        let mut route = route_with_capability(None);
        route.provider = astra_services::byok_endpoint::COMPATIBLE_PROVIDER.to_string();
        route.fixed_temperature = Some(0.6);
        route.request_body_overrides = Some(serde_json::Map::from_iter([(
            "temperature".to_string(),
            serde_json::json!(1.0),
        )]));
        assert!(
            RuntimeSummaryClient::resolve_generation_policy(
                InferencePurpose::Introspection,
                &route,
            )
            .expect_err("conflicting capabilities")
            .contains("conflicts with fixed_temperature")
        );

        route.fixed_temperature = None;
        route.request_body_overrides = Some(serde_json::Map::from_iter([(
            "temperature".to_string(),
            serde_json::json!("cold"),
        )]));
        assert!(
            RuntimeSummaryClient::resolve_generation_policy(
                InferencePurpose::Introspection,
                &route,
            )
            .expect_err("invalid capability")
            .contains("finite non-negative number")
        );
    }

    #[tokio::test]
    async fn summary_transport_preserves_cache_tools_but_forbids_tool_selection() {
        let captured_body = Arc::new(std::sync::Mutex::new(None::<Value>));
        let captured_body_for_handler = captured_body.clone();
        let app = Router::new().route(
            "/chat/completions",
            post(move |axum::Json(body): axum::Json<Value>| {
                let captured_body = captured_body_for_handler.clone();
                async move {
                    *captured_body
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(body);
                    let response = serde_json::json!({
                        "id": "summary-response",
                        "choices": [{
                            "message": {"role": "assistant", "content": "structured summary"},
                            "finish_reason": "stop"
                        }],
                        "usage": {"prompt_tokens": 10, "completion_tokens": 2}
                    });
                    summary_provider_response(response)
                }
            }),
        );
        let execution = summary_execution(spawn_summary_test_server(app).await);
        let tools = vec![serde_json::json!({
            "type": "function",
            "function": {
                "name": "bash",
                "description": "Run a command",
                "parameters": {"type": "object", "properties": {}}
            }
        })];
        let cache_capability = astra_turn_core::cache_placement::CacheCapability {
            protocol: astra_turn_core::cache_placement::CacheProtocol::OpenAiAutoPrefix,
            volatile_placement:
                astra_turn_core::cache_placement::VolatilePlacement::AppendOnlyUserTail,
            volatile_delivery:
                astra_turn_core::cache_placement::VolatileDeliveryPolicy::RequiredOnly,
            reuse_scope: Some(astra_turn_core::cache_placement::CacheReuseScope::ConversationTurns),
        };
        let client = test_client(summary_route(&execution), 64)
            .with_prompt_cache_context(tools.clone(), cache_capability);
        let messages = vec![
            serde_json::json!({"role": "system", "content": "stable prefix"}),
            serde_json::json!({"role": "user", "content": "summarize"}),
        ];

        let summary = client
            .summarize(InferencePurpose::RequiredCompaction, &messages)
            .await
            .expect("the no-tool transport must return summary text");
        assert_eq!(summary.text, "structured summary");
        assert_eq!(summary.judgment_provenance, None);
        assert_eq!(summary.finish_reason.as_deref(), Some("stop"));
        assert_eq!(
            summary
                .usage
                .get("input_tokens")
                .and_then(serde_json::Value::as_u64),
            Some(10)
        );

        let body = captured_body
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .expect("captured request body");
        assert_eq!(body.get("messages"), Some(&Value::Array(messages)));
        assert_eq!(body.get("tools"), Some(&Value::Array(tools)));
        assert_eq!(body.get("tool_choice"), Some(&Value::String("none".into())));
    }

    #[tokio::test]
    async fn generic_openai_compatible_work_admission_uses_provider_defaults_once() {
        let provider_requests = Arc::new(AtomicU32::new(0));
        let captured_body = Arc::new(std::sync::Mutex::new(None::<Value>));
        let provider_requests_for_handler = provider_requests.clone();
        let captured_body_for_handler = captured_body.clone();
        let app = Router::new().route(
            "/chat/completions",
            post(move |axum::Json(body): axum::Json<Value>| {
                let provider_requests = provider_requests_for_handler.clone();
                let captured_body = captured_body_for_handler.clone();
                async move {
                    provider_requests.fetch_add(1, Ordering::SeqCst);
                    *captured_body
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(body.clone());
                    if body.get("temperature").is_some() {
                        return Response::builder()
                            .status(400)
                            .header("content-type", "application/json")
                            .body(Body::from(
                                r#"{"error":{"message":"thinking mode fixes temperature"}}"#,
                            ))
                            .expect("strict provider rejection");
                    }
                    let decision = r#"{"work_lifecycle":"not_required","execution_topology":"primary"}"#;
                    if body["stream"] == true {
                        let event = serde_json::json!({"choices":[{"index":0,"delta":{"content":decision},"finish_reason":null}]});
                        return Response::builder().status(200).header("content-type", "text/event-stream")
                            .body(Body::from(format!("data: {event}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n"))).unwrap();
                    }
                    let response = serde_json::json!({
                        "id": "kimi-like-summary",
                        "choices": [{
                            "message": {
                                "role": "assistant",
                                "reasoning_content": "The prompt requests one direct answer.",
                                "content": decision
                            },
                            "finish_reason": "stop"
                        }],
                        "usage": {"prompt_tokens": 10, "completion_tokens": 30}
                    });
                    summary_provider_response(response)
                }
            }),
        );
        let mut execution = summary_execution(spawn_summary_test_server(app).await);
        // Use an unclassified OpenAI-compatible protocol identifier while
        // keeping the test server on loopback. The production
        // `openai-compatible` identifier intentionally activates public-HTTPS
        // SSRF enforcement and cannot target this local fixture.
        for provider in ["mock-openai-compatible", "openai"] {
            execution.provider = provider.to_string();
            {
                let client = test_client(summary_route(&execution), 1_024);
                let messages = vec![serde_json::json!({
                    "role": "user",
                    "content": "Decide whether this turn needs durable Work"
                })];

                let summary = client
                    .summarize(InferencePurpose::Introspection, &messages)
                    .await
                    .expect("provider-default request must succeed");
                astra_services::parse_work_admission_response(&summary.text)
                    .expect("the separate final content must remain a valid Work decision");

                let body = captured_body
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone()
                    .expect("captured strict-provider request");
                assert!(
                    body.get("temperature").is_none(),
                    "generic compatibility must not invent a sampling capability"
                );
                assert!(
                    body.get("thinking").is_none(),
                    "stage 1 leaves an unclassified endpoint's thinking mode at its provider default"
                );
            }
        }
        assert_eq!(
            provider_requests.load(Ordering::SeqCst),
            2,
            "one request per provider, no retry/fallback"
        );
    }

    #[tokio::test]
    async fn admitted_moonshot_protocol_suppresses_reasoning_without_guessing_model_name() {
        let app = Router::new().route("/chat/completions", post(|axum::Json(body): axum::Json<Value>| async move {
            assert_eq!(body["thinking"], serde_json::json!({"type":"disabled"}));
            assert!(body.get("enable_thinking").is_none());
            assert!(body.get("reasoning_effort").is_none());
            assert!(body.get("temperature").is_none());
            assert_eq!(body["max_completion_tokens"], 1024);
            summary_provider_response(serde_json::json!({"choices":[{"message":{"content":"decision"},"finish_reason":"stop"}]}))
        }));
        let execution = summary_execution(spawn_summary_test_server(app).await);
        let mut route = summary_route(&execution);
        route.model_name = "arbitrary-local-alias".into();
        route.thinking_capability = Some(astra_services::models::ThinkingCapability::Both);
        route.thinking_protocol =
            Some(astra_core::model_wire::thinking::ThinkingProtocol::Moonshot);
        let client = test_client(route, 1024);
        assert_eq!(
            client
                .summarize(
                    InferencePurpose::Introspection,
                    &[serde_json::json!({"role":"user","content":"hello"})]
                )
                .await
                .unwrap()
                .text,
            "decision"
        );
    }

    #[tokio::test]
    async fn admitted_temperature_override_remains_authoritative_on_the_wire() {
        let captured_body = Arc::new(std::sync::Mutex::new(None::<Value>));
        let captured_body_for_handler = captured_body.clone();
        let app = Router::new().route(
            "/chat/completions",
            post(move |axum::Json(body): axum::Json<Value>| {
                let captured_body = captured_body_for_handler.clone();
                async move {
                    *captured_body
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(body);
                    let response = serde_json::json!({
                        "id": "configured-summary",
                        "choices": [{
                            "message": {"role": "assistant", "content": "configured"},
                            "finish_reason": "stop"
                        }]
                    });
                    summary_provider_response(response)
                }
            }),
        );
        let execution = summary_execution(spawn_summary_test_server(app).await);
        let mut route = summary_route(&execution);
        route.provider = "mock-openai-compatible".to_string();
        route.request_body_overrides = Some(serde_json::Map::from_iter([(
            "temperature".to_string(),
            serde_json::json!(0.7),
        )]));
        let client = test_client(route, 128);

        client
            .summarize(
                InferencePurpose::Introspection,
                &[serde_json::json!({"role": "user", "content": "classify"})],
            )
            .await
            .expect("configured request");

        let body = captured_body
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .expect("captured configured request");
        assert_eq!(body.get("temperature"), Some(&serde_json::json!(0.7)));
    }

    #[tokio::test]
    async fn provider_tool_call_on_no_tool_summary_is_repairable_invalid_output() {
        let app = Router::new().route(
            "/chat/completions",
            post(|| async {
                let response = serde_json::json!({
                    "id": "invalid-summary-response",
                    "choices": [{
                        "message": {
                            "role": "assistant",
                            "content": "{\"work_lifecycle\":\"not_required\"}",
                            "tool_calls": [{
                                "id": "forbidden-call",
                                "type": "function",
                                "function": {"name": "bash", "arguments": "{\"command\":\"true\"}"}
                            }]
                        },
                        "finish_reason": "tool_calls"
                    }],
                    "usage": {"prompt_tokens": 7, "completion_tokens": 3}
                });
                summary_provider_response(response)
            }),
        );
        let execution = summary_execution(spawn_summary_test_server(app).await);
        let client = test_client(summary_route(&execution), 64);
        let summary = client
            .summarize(
                InferencePurpose::Introspection,
                &[serde_json::json!({"role": "user", "content": "classify"})],
            )
            .await
            .expect("completed transport must reach the structured repair owner");

        assert!(summary.text.is_empty());
        assert_eq!(summary.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(
            summary.usage.get("input_tokens").and_then(Value::as_u64),
            Some(7)
        );
    }

    /// Explicit paid-provider check. The file contains URL, key and model as
    /// its last three nonempty lines; none of its contents are logged.
    #[tokio::test]
    #[cfg(feature = "live-provider-tests")]
    #[ignore = "requires ASTRA_TEST_SUMMARY_CONFIG_FILE and a real provider credential"]
    async fn live_work_admission_provider_contract() {
        let path = std::env::var("ASTRA_TEST_SUMMARY_CONFIG_FILE")
            .expect("set ASTRA_TEST_SUMMARY_CONFIG_FILE");
        let config = std::fs::read_to_string(path).expect("read provider configuration");
        let lines: Vec<_> = config
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        assert!(lines.len() >= 3, "expected URL, key and model");
        let fields = &lines[lines.len() - 3..];
        let mut execution = summary_execution(fields[0].to_string());
        execution.api_key = fields[1].to_string();
        execution.model_name =
            std::env::var("ASTRA_TEST_SUMMARY_MODEL").unwrap_or_else(|_| fields[2].to_string());
        execution.provider = astra_services::byok_endpoint::COMPATIBLE_PROVIDER.to_string();
        execution.access_kind = astra_services::ModelAccessKind::CloudByok;
        execution.thinking_protocol = Some(match std::env::var("ASTRA_TEST_THINKING_PROTOCOL") {
            Ok(value) => serde_json::from_value(serde_json::Value::String(value))
                .expect("known thinking protocol"),
            Err(_) => astra_core::model_wire::thinking::canonical_thinking_protocol(
                &execution.provider,
                &execution.base_url,
                &execution.model_name,
            ),
        });
        let client =
            RuntimeSummaryClient::new(summary_route(&execution), 1_024, summary_authority());
        let messages = astra_services::work_admission_judge_messages(
            &astra_services::TurnIntentJudgeContext {
                message: "今天星期几".to_string(),
                turn_count: 1,
                ..Default::default()
            },
        );
        let started = std::time::Instant::now();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            client.summarize(InferencePurpose::Introspection, &messages),
        )
        .await;
        eprintln!(
            "live_work_admission elapsed_ms={}",
            started.elapsed().as_millis()
        );
        let response = result
            .expect("provider deadline exceeded")
            .unwrap_or_else(|error| {
                eprintln!("live_failure_kind={}", error.kind.as_str());
                panic!("live summary request failed; credential and response suppressed")
            });
        let parsed = astra_services::parse_work_admission_response(&response.text);
        eprintln!(
            "live_work_admission structured_decision_valid={} response_bytes={}",
            parsed.is_ok(),
            response.text.len()
        );
        assert!(parsed.is_ok(), "provider returned no valid Work decision");
    }

    #[tokio::test]
    async fn independent_summary_clients_do_not_serialize_provider_io() {
        let active_requests = Arc::new(AtomicU32::new(0));
        let max_active_requests = Arc::new(AtomicU32::new(0));
        let provider_requests = Arc::new(AtomicU32::new(0));
        let app = Router::new().route(
            "/chat/completions",
            post({
                let active_requests = active_requests.clone();
                let max_active_requests = max_active_requests.clone();
                let provider_requests = provider_requests.clone();
                move || {
                    let active_requests = active_requests.clone();
                    let max_active_requests = max_active_requests.clone();
                    let provider_requests = provider_requests.clone();
                    async move {
                        provider_requests.fetch_add(1, Ordering::SeqCst);
                        let active = active_requests.fetch_add(1, Ordering::SeqCst) + 1;
                        max_active_requests.fetch_max(active, Ordering::SeqCst);
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        active_requests.fetch_sub(1, Ordering::SeqCst);
                        let event = serde_json::json!({
                            "choices": [{
                                "delta": {"content": "summary"},
                                "finish_reason": "stop"
                            }]
                        });
                        Response::builder()
                            .status(200)
                            .header("content-type", "text/event-stream")
                            .body(Body::from(format!("data: {event}\n\ndata: [DONE]\n\n")))
                            .expect("summary provider response")
                    }
                }
            }),
        );
        let execution = summary_execution(spawn_summary_test_server(app).await);
        let first = RuntimeSummaryClient::new(summary_route(&execution), 64, summary_authority());
        let second = RuntimeSummaryClient::new(summary_route(&execution), 64, summary_authority());
        let messages = vec![serde_json::json!({"role": "user", "content": "summarize"})];

        let (first_result, second_result) = tokio::join!(
            first.summarize(InferencePurpose::Introspection, &messages),
            second.summarize(InferencePurpose::Introspection, &messages),
        );
        let first = first_result.expect("first summary");
        let second = second_result.expect("second summary");
        assert_eq!(first.text, "summary");
        assert_eq!(second.text, "summary");
        assert_ne!(
            first.execution.unwrap().invocation_id,
            second.execution.unwrap().invocation_id
        );
        assert_eq!(provider_requests.load(Ordering::SeqCst), 2);
        assert_eq!(
            max_active_requests.load(Ordering::SeqCst),
            2,
            "independent calls must overlap provider I/O"
        );
    }

    #[tokio::test]
    async fn cancelled_run_never_dispatches_auxiliary_model_request() {
        let requests = Arc::new(AtomicU32::new(0));
        let route = summary_route(&summary_execution(
            spawn_counted_summary_server(requests.clone(), std::time::Duration::ZERO).await,
        ));
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let authority =
            DurableInferenceRunAuthority::new(1, "owner", 0, Some(cancelled), None, None);
        let client = RuntimeSummaryClient::new(route, 64, authority);
        let error = client
            .summarize(
                InferencePurpose::Introspection,
                &[serde_json::json!({"role":"user","content":"summarize"})],
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind, astra_core::ErrorKind::Cancelled);
        assert_eq!(requests.load(Ordering::SeqCst), 0);
    }
    #[tokio::test]
    async fn lost_run_owner_interrupts_inflight_auxiliary_request() {
        let requests = Arc::new(AtomicU32::new(0));
        let route = summary_route(&summary_execution(
            spawn_counted_summary_server(requests.clone(), std::time::Duration::from_secs(10))
                .await,
        ));
        let lost = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let authority =
            DurableInferenceRunAuthority::new(1, "owner", 0, None, None, Some(lost.clone()));
        let client = RuntimeSummaryClient::new(route, 64, authority);
        let input = [serde_json::json!({"role":"user","content":"summarize"})];
        let request = client.summarize(InferencePurpose::Introspection, &input);
        let lose_owner = async {
            while requests.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
            lost.store(true, Ordering::Release);
        };
        let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            tokio::join!(request, lose_owner)
        })
        .await
        .expect("owner loss must stop a request that is already in flight");
        assert_eq!(
            result.unwrap_err().kind,
            astra_core::ErrorKind::ContractViolation
        );
        assert_eq!(requests.load(Ordering::SeqCst), 1);
    }
}
