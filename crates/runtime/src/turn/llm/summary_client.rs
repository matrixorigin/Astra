use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use astra_turn_core::cloud_summary::{SummaryLlmClient, SummaryResponse};
use astra_turn_core::thinking_config::{ThinkingConfig, ThinkingEffort};
use astra_turn_types::InferencePurpose;

use super::client::{LlmCall, OwnedLlmExecutionRoute};
use super::durable::DurableInferenceLedger;

#[cfg(test)]
use super::client::{global_llm_client, llm_nonstream_timeout};

#[derive(Clone)]
struct DurableSummaryExecution {
    ledger: DurableInferenceLedger,
    base_scope: astra_turn_types::InferenceInvocationScope,
    attempt_allocator: DurableSummaryAttemptAllocator,
}

/// Host-scoped allocator for auxiliary inference identities.
///
/// Every call reserves two adjacent identities because durable admission may
/// consume `N` while resolving an ambiguous commit and retry provider delivery
/// exactly once as `N + 1`. The allocator is intentionally shareable across
/// independently constructed clients for the same host/run, not merely clones
/// of one client.
#[derive(Clone, Default)]
pub(crate) struct DurableSummaryAttemptAllocator {
    next_logical_attempt_by_scope: Arc<std::sync::Mutex<std::collections::HashMap<String, u64>>>,
    #[cfg(test)]
    initial_next_logical_attempt: u64,
}

impl DurableSummaryAttemptAllocator {
    fn reserve_pair_at_least(
        &self,
        scope_key: &str,
        durable_pair_base: u32,
    ) -> Result<u32, String> {
        if !durable_pair_base.is_multiple_of(2) {
            return Err("durable summary logical attempt cursor is not pair-aligned".to_string());
        }
        let mut cursors = self
            .next_logical_attempt_by_scope
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let next = cursors.entry(scope_key.to_string()).or_insert({
            #[cfg(test)]
            {
                self.initial_next_logical_attempt
            }
            #[cfg(not(test))]
            {
                0
            }
        });
        *next = (*next).max(u64::from(durable_pair_base));
        let requested = u32::try_from(*next)
            .map_err(|_| "durable summary logical attempt space is exhausted".to_string())?;
        requested
            .checked_add(1)
            .ok_or_else(|| "durable summary logical attempt space is exhausted".to_string())?;
        *next = next
            .checked_add(2)
            .ok_or_else(|| "durable summary logical attempt space is exhausted".to_string())?;
        Ok(requested)
    }

    #[cfg(test)]
    fn with_next_logical_attempt(next_logical_attempt: u64) -> Self {
        Self {
            next_logical_attempt_by_scope: Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            initial_next_logical_attempt: next_logical_attempt,
        }
    }
}

const MAX_DURABLE_SUMMARY_CURSOR_COLLISIONS: usize = 8;

#[derive(Clone)]
enum SummaryExecution {
    Durable(Box<DurableSummaryExecution>),
    #[cfg(test)]
    Direct,
}

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
    execution: SummaryExecution,
}

impl RuntimeSummaryClient {
    fn attempt_allocator_scope_key(
        base_scope: &astra_turn_types::InferenceInvocationScope,
        purpose: InferencePurpose,
        route: &OwnedLlmExecutionRoute,
    ) -> Result<String, String> {
        serde_json::to_string(&serde_json::json!({
            "scope": base_scope.clone().with_logical_attempt(0),
            "purpose": purpose.as_str(),
            "model": &route.model_name,
            "wire_model": route.wire_model_name.as_deref(),
            "provider": &route.provider,
        }))
        .map_err(|error| format!("serialize durable summary allocator scope: {error}"))
    }

    #[must_use]
    pub(crate) fn new_with_attempt_allocator(
        route: OwnedLlmExecutionRoute,
        max_output_tokens: usize,
        ledger: DurableInferenceLedger,
        base_scope: astra_turn_types::InferenceInvocationScope,
        attempt_allocator: DurableSummaryAttemptAllocator,
    ) -> Self {
        Self {
            route,
            max_output_tokens,
            prompt_cache_tools: Vec::new(),
            cache_capability: None,
            execution: SummaryExecution::Durable(Box::new(DurableSummaryExecution {
                ledger,
                base_scope,
                attempt_allocator,
            })),
        }
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
    fn thinking_for(purpose: InferencePurpose, route: &OwnedLlmExecutionRoute) -> ThinkingConfig {
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
        match (purpose, route.thinking_capability) {
            // A persisted EffortOnly value may predate the provider's typed
            // suppression capability. The admitted endpoint protocol is the
            // stronger fact for bounded auxiliary work, so prefer Off when
            // the route can express it natively; the client will add the
            // exact suppression field to the wire request.
            (
                InferencePurpose::Introspection,
                Some(astra_services::models::ThinkingCapability::EffortOnly),
            ) if protocol.can_disable() => ThinkingConfig::Off,
            (
                InferencePurpose::Introspection,
                Some(astra_services::models::ThinkingCapability::EffortOnly),
            ) if protocol
                == astra_core::model_wire::thinking::ThinkingProtocol::ReasoningEffort =>
            {
                ThinkingConfig::Adaptive {
                    effort: ThinkingEffort::Low,
                }
            }
            _ => ThinkingConfig::Off,
        }
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
            } else if !thinking.is_off() {
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

    /// Low-level provider-adapter constructor for unit tests. Production
    /// summary paths must use [`Self::new`] so auxiliary calls cannot bypass
    /// durable admission and usage settlement.
    #[cfg(test)]
    #[must_use]
    pub fn new_direct_for_test(route: OwnedLlmExecutionRoute, max_output_tokens: usize) -> Self {
        Self {
            route,
            max_output_tokens,
            prompt_cache_tools: Vec::new(),
            cache_capability: None,
            execution: SummaryExecution::Direct,
        }
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
        let result = match &self.execution {
            SummaryExecution::Durable(execution) => {
                let DurableSummaryExecution {
                    ledger,
                    base_scope,
                    attempt_allocator,
                } = execution.as_ref();
                let allocator_scope_key =
                    Self::attempt_allocator_scope_key(base_scope, purpose, &self.route)
                        .map_err(contract_error)?;
                let mut collisions = 0;
                loop {
                    let durable_pair_base = ledger
                        .next_logical_attempt_pair_base(
                            base_scope.clone(),
                            purpose,
                            &self.route.model_name,
                            self.route
                                .wire_model_name
                                .as_deref()
                                .unwrap_or(&self.route.model_name),
                            &self.route.provider,
                        )
                        .await?;
                    // Reserve both identities in one short, non-async critical
                    // section. No allocator lock may span provider or database I/O.
                    let requested_logical_attempt = attempt_allocator
                        .reserve_pair_at_least(&allocator_scope_key, durable_pair_base)
                        .map_err(contract_error)?;
                    let outcome = ledger
                        .execute_stream_no_tool_choice(
                            base_scope.with_logical_attempt(requested_logical_attempt),
                            LlmCall {
                                purpose,
                                messages,
                                tools: &self.prompt_cache_tools,
                                cache_capability: self.cache_capability,
                                route: self.route.borrowed(),
                                max_output_tokens: Some(self.max_output_tokens),
                                temperature,
                                has_fallback: false,
                                thinking,
                            },
                        )
                        .await;
                    debug_assert!(
                        outcome.logical_attempt() <= requested_logical_attempt.saturating_add(1),
                        "durable summary recovery exceeded its reserved identity pair"
                    );
                    if outcome.admission_identity_is_occupied()
                        && collisions < MAX_DURABLE_SUMMARY_CURSOR_COLLISIONS
                    {
                        collisions += 1;
                        continue;
                    }
                    break outcome.into_result();
                }
            }
            #[cfg(test)]
            SummaryExecution::Direct => {
                crate::turn::llm::client::call_llm_nonstream_no_tool_choice(
                    global_llm_client(),
                    LlmCall {
                        purpose,
                        messages,
                        tools: &self.prompt_cache_tools,
                        cache_capability: self.cache_capability,
                        route: self.route.borrowed(),
                        max_output_tokens: Some(self.max_output_tokens),
                        temperature,
                        has_fallback: false,
                        thinking,
                    },
                    llm_nonstream_timeout(),
                )
                .await
            }
        };
        match result {
            // A no-tool provider response that nevertheless contains a native
            // or degraded call is a completed transport with invalid
            // structured output, not a network failure. Return an empty typed
            // payload so schema-owning callers can take their one bounded
            // repair path. No call from this private adapter is executable.
            Ok(result) if !result.tool_calls.is_empty() || result.full_text.trim().is_empty() => {
                Ok(SummaryResponse {
                    text: String::new(),
                    is_ptl_error: false,
                    finish_reason: result.effective_finish_reason.or(result.finish_reason),
                    usage: result.usage,
                })
            }
            Ok(result) => Ok(SummaryResponse {
                text: result.full_text,
                is_ptl_error: false,
                finish_reason: result.effective_finish_reason.or(result.finish_reason),
                usage: result.usage,
            }),
            Err(error) if error.kind == astra_core::ErrorKind::ContextWindow => {
                Ok(SummaryResponse {
                    text: String::new(),
                    is_ptl_error: true,
                    finish_reason: None,
                    usage: serde_json::Map::new(),
                })
            }
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU32, Ordering};

    use axum::{Router, body::Body, response::Response, routing::post};

    use super::super::durable::InferenceLedgerPersistence as _;
    use super::*;

    #[derive(Default)]
    struct RecoverFirstAdmissionPersistence {
        inner: super::super::durable::TestInferenceLedgerPersistence,
        admitted_logical_attempts: std::sync::Mutex<Vec<u32>>,
        recover_attempt_zero: std::sync::atomic::AtomicBool,
        admission_conflicts: AtomicU32,
        cursor_barrier: std::sync::Mutex<Option<Arc<tokio::sync::Barrier>>>,
        cursor_barrier_reads_remaining: AtomicU32,
        successful_commit_ack_delay: Option<std::time::Duration>,
    }

    #[async_trait]
    impl super::super::durable::InferenceLedgerPersistence for RecoverFirstAdmissionPersistence {
        async fn next_logical_attempt_pair_base(
            &self,
            _input: &astra_services::InferenceInvocationInput,
        ) -> astra_services::ServiceResult<u32> {
            let max = self
                .admitted_logical_attempts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .copied()
                .max();
            let next = match max {
                None => Ok(0),
                Some(max) => (max / 2)
                    .checked_add(1)
                    .and_then(|pair| pair.checked_mul(2))
                    .ok_or_else(|| {
                        astra_services::ServiceError::conflict(
                            "durable inference logical-attempt pair space is exhausted",
                        )
                    }),
            }?;
            let wait_at_cursor_barrier = self
                .cursor_barrier_reads_remaining
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok();
            let cursor_barrier = if wait_at_cursor_barrier {
                self.cursor_barrier
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone()
            } else {
                None
            };
            if let Some(barrier) = cursor_barrier {
                barrier.wait().await;
            }
            Ok(next)
        }

        async fn admit_invocation(
            &self,
            plan: &astra_services::InferenceInvocationPlan,
        ) -> astra_services::ServiceResult<()> {
            if let Err(error) = self.inner.admit_invocation(plan).await {
                if error.kind == astra_services::ServiceErrorKind::Conflict {
                    self.admission_conflicts.fetch_add(1, Ordering::SeqCst);
                }
                return Err(error);
            }
            self.admitted_logical_attempts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(plan.logical_attempt());
            if self.recover_attempt_zero.load(Ordering::Acquire) && plan.logical_attempt() == 0 {
                // Model a committed N admission whose acknowledgement never
                // reached the caller. Foreground recovery closes N and retries
                // exactly once as N+1.
                std::future::pending().await
            } else {
                Ok(())
            }
        }

        async fn settle_uncertain_admission(
            &self,
            plan: &astra_services::InferenceInvocationPlan,
            terminal: &astra_services::InferenceInvocationTerminal,
        ) -> astra_services::ServiceResult<astra_services::InferenceInvocationAdmissionResolution>
        {
            self.inner.settle_uncertain_admission(plan, terminal).await
        }

        async fn declare_settlement(
            &self,
            plan: &astra_services::InferenceInvocationPlan,
            terminal: &astra_services::InferenceInvocationTerminal,
        ) -> astra_services::ServiceResult<()> {
            self.inner.declare_settlement(plan, terminal).await
        }

        async fn declare_attempt_settlement(
            &self,
            plan: &astra_services::InferenceInvocationPlan,
            attempt: &astra_services::InferenceProviderAttemptPlan,
            terminal: &astra_services::InferenceInvocationTerminal,
            provider_delivery_state: astra_services::InferenceProviderDeliveryState,
        ) -> astra_services::ServiceResult<()> {
            self.inner
                .declare_attempt_settlement(plan, attempt, terminal, provider_delivery_state)
                .await
        }

        async fn finish_invocation(
            &self,
            plan: &astra_services::InferenceInvocationPlan,
            terminal: &astra_services::InferenceInvocationTerminal,
        ) -> astra_services::ServiceResult<()> {
            self.inner.finish_invocation(plan, terminal).await
        }

        async fn begin_provider_attempt(
            &self,
            attempt: &astra_services::InferenceProviderAttemptPlan,
        ) -> astra_services::ServiceResult<()> {
            self.inner.begin_provider_attempt(attempt).await
        }

        async fn finish_provider_attempt(
            &self,
            attempt: &astra_services::InferenceProviderAttemptPlan,
            terminal: &astra_services::InferenceInvocationTerminal,
        ) -> astra_services::ServiceResult<()> {
            self.inner.finish_provider_attempt(attempt, terminal).await
        }

        async fn finish_successful_provider_attempt_and_invocation(
            &self,
            plan: &astra_services::InferenceInvocationPlan,
            attempt: &astra_services::InferenceProviderAttemptPlan,
            terminal: &astra_services::InferenceInvocationTerminal,
        ) -> astra_services::ServiceResult<()> {
            self.inner
                .finish_successful_provider_attempt_and_invocation(plan, attempt, terminal)
                .await?;
            if let Some(delay) = self.successful_commit_ack_delay {
                tokio::time::sleep(delay).await;
            }
            Ok(())
        }
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
            offering_id: "summary-offering".to_string(),
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
            model_name: execution.model_name.clone(),
            wire_model_name: None,
            api_key: execution.api_key.clone(),
            base_url: execution.base_url.clone(),
            provider: execution.provider.clone(),
            thinking_capability: None,
            fixed_temperature: execution.fixed_temperature,
            thinking_protocol: execution.thinking_protocol,
            header_overrides: HashMap::new(),
            request_body_overrides: None,
            completions_url_override: None,
            request_timeout: None,
        }
    }

    fn summary_scope() -> astra_turn_types::InferenceInvocationScope {
        astra_turn_types::InferenceInvocationScope::Run {
            session_id: "summary-session".to_string(),
            run_id: "summary-run".to_string(),
            turn: 1,
            round: 1,
            operation_id: "summary_repair".to_string(),
            logical_attempt: 0,
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
            model_name: "test-model".to_string(),
            wire_model_name: None,
            api_key: String::new(),
            base_url: "https://example.invalid/v1".to_string(),
            provider: "openai".to_string(),
            thinking_capability,
            fixed_temperature: None,
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
            ThinkingConfig::Off,
            "legacy capability alone cannot prove an effort wire protocol"
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
            assert_eq!(
                RuntimeSummaryClient::thinking_for(InferencePurpose::Introspection, &route),
                ThinkingConfig::Off,
                "only an explicitly effort-only model may receive reasoning_effort"
            );
            assert_eq!(
                RuntimeSummaryClient::thinking_for(InferencePurpose::RequiredCompaction, &route),
                ThinkingConfig::Off,
                "the bounded auxiliary policy must not leak into unrelated inference"
            );
        }
    }

    #[test]
    fn bounded_introspection_honors_typed_deepseek_suppression_even_with_stale_capability() {
        assert_eq!(
            RuntimeSummaryClient::thinking_for(
                InferencePurpose::Introspection,
                &deepseek_effort_only_route(),
            ),
            ThinkingConfig::Off,
            "the endpoint protocol can disable DeepSeek V4 thinking even when an older DB probe says effort_only"
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
                    Response::builder()
                        .status(200)
                        .header("content-type", "application/json")
                        .body(Body::from(response.to_string()))
                        .expect("summary provider response")
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
        let client = RuntimeSummaryClient::new_direct_for_test(summary_route(&execution), 64)
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

    async fn summary_with_commit_ack_delay(
        delay: std::time::Duration,
    ) -> (
        Result<SummaryResponse, astra_core::ClassifiedError>,
        u32,
        Vec<u32>,
    ) {
        let requests = Arc::new(AtomicU32::new(0));
        let handler_requests = requests.clone();
        let app = Router::new().route(
            "/chat/completions",
            post(move || {
                let requests = handler_requests.clone();
                async move {
                    requests.fetch_add(1, Ordering::SeqCst);
                    let event = serde_json::json!({
                        "id": "slow-commit-summary",
                        "choices": [{"index":0,"delta":{"content":
                            "{\"work_lifecycle\":\"not_required\",\"execution_topology\":\"primary\"}"},
                            "finish_reason":"stop"}],
                        "usage":{"prompt_tokens":100,"completion_tokens":12,
                            "prompt_tokens_details":{"cached_tokens":64}}
                    });
                    Response::builder().status(200)
                        .header("content-type", "text/event-stream")
                        .body(Body::from(format!("data: {event}\n\ndata: [DONE]\n\n")))
                        .unwrap()
                }
            }),
        );
        let execution = summary_execution(spawn_summary_test_server(app).await);
        let persistence = Arc::new(RecoverFirstAdmissionPersistence {
            successful_commit_ack_delay: Some(delay),
            ..Default::default()
        });
        let ledger = DurableInferenceLedger::required_with_persistence(
            None,
            Some(&execution),
            "summary-user",
            Some(persistence.clone()),
        )
        .unwrap()
        .with_run_authority(summary_authority());
        let client = RuntimeSummaryClient::new_with_attempt_allocator(
            summary_route(&execution),
            1_024,
            ledger,
            summary_scope(),
            DurableSummaryAttemptAllocator::default(),
        );
        let summary = client
            .summarize(
                InferencePurpose::Introspection,
                &[serde_json::json!({"role":"user","content":"Classify this direct answer request"})],
            )
            .await;
        let attempts = persistence
            .admitted_logical_attempts
            .lock()
            .unwrap()
            .clone();
        (summary, requests.load(Ordering::SeqCst), attempts)
    }

    #[tokio::test]
    async fn durable_summary_receives_success_and_usage_after_slow_commit_ack() {
        let (summary, requests, attempts) =
            summary_with_commit_ack_delay(std::time::Duration::from_secs(1)).await;
        let summary =
            summary.expect("unused provider time remains available for commit acknowledgement");
        astra_services::parse_work_admission_response(&summary.text).unwrap();
        let usage = crate::turn::token_usage::TokenUsage::from_partial_json_map(&summary.usage);
        assert_eq!(usage.input_tokens, 36);
        assert_eq!(usage.cached_input_tokens, 64);
        assert_eq!(usage.output_tokens, 12);
        assert_eq!(requests, 1);
        assert_eq!(attempts, vec![0]);
    }

    #[tokio::test]
    async fn durable_summary_timeout_preserves_error_kind_and_provider_usage() {
        let (summary, requests, attempts) =
            summary_with_commit_ack_delay(std::time::Duration::from_secs(9)).await;
        let error =
            summary.expect_err("a late durable success cannot authorize foreground delivery");
        assert_eq!(error.kind, astra_core::ErrorKind::DatabaseError);
        let details: Value = serde_json::from_str(error.details_json.as_deref().unwrap()).unwrap();
        assert_eq!(
            details["deadline"]["phase"],
            "provider_attempt_terminalization"
        );
        let usage = crate::turn::token_usage::TokenUsage::from_partial_json_map(
            details["usage"].as_object().unwrap(),
        );
        assert_eq!(
            (
                usage.input_tokens,
                usage.cached_input_tokens,
                usage.output_tokens
            ),
            (36, 64, 12)
        );
        assert_eq!(requests, 1);
        assert_eq!(attempts, vec![0]);
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
                    Response::builder()
                        .status(200)
                        .header("content-type", "application/json")
                        .body(Body::from(response.to_string()))
                        .expect("strict provider response")
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
            for streaming in [false, true] {
                let client = if streaming {
                    let ledger = DurableInferenceLedger::required_with_persistence(
                        None,
                        Some(&execution),
                        "summary-user",
                        Some(Arc::new(RecoverFirstAdmissionPersistence::default())),
                    )
                    .unwrap()
                    .with_run_authority(summary_authority());
                    RuntimeSummaryClient::new_with_attempt_allocator(
                        summary_route(&execution),
                        1_024,
                        ledger,
                        summary_scope(),
                        DurableSummaryAttemptAllocator::default(),
                    )
                } else {
                    RuntimeSummaryClient::new_direct_for_test(summary_route(&execution), 1_024)
                };
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
            4,
            "one request per provider and transport, no retry/fallback"
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
            axum::Json(serde_json::json!({"choices":[{"message":{"content":"decision"},"finish_reason":"stop"}]}))
        }));
        let execution = summary_execution(spawn_summary_test_server(app).await);
        let mut route = summary_route(&execution);
        route.model_name = "arbitrary-local-alias".into();
        route.thinking_protocol =
            Some(astra_core::model_wire::thinking::ThinkingProtocol::Moonshot);
        let client = RuntimeSummaryClient::new_direct_for_test(route, 1024);
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
                    Response::builder()
                        .status(200)
                        .header("content-type", "application/json")
                        .body(Body::from(response.to_string()))
                        .expect("configured provider response")
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
        let client = RuntimeSummaryClient::new_direct_for_test(route, 128);

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
                Response::builder()
                    .status(200)
                    .header("content-type", "application/json")
                    .body(Body::from(response.to_string()))
                    .expect("summary provider response")
            }),
        );
        let execution = summary_execution(spawn_summary_test_server(app).await);
        let client = RuntimeSummaryClient::new_direct_for_test(summary_route(&execution), 64);
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

    #[test]
    fn summary_attempt_allocator_is_shared_only_within_the_exact_scope() {
        let allocator = DurableSummaryAttemptAllocator::default();
        assert_eq!(
            allocator
                .reserve_pair_at_least("scope-a", 0)
                .expect("first scope-a pair"),
            0
        );
        assert_eq!(
            allocator
                .reserve_pair_at_least("scope-a", 0)
                .expect("second scope-a pair"),
            2
        );
        assert_eq!(
            allocator
                .reserve_pair_at_least("scope-b", 0)
                .expect("independent scope-b pair"),
            0,
            "one noisy scope must not consume another scope's logical identity space"
        );
    }

    #[tokio::test]
    async fn recovered_malformed_summary_repairs_under_next_authoritative_attempt() {
        let provider_requests = Arc::new(AtomicU32::new(0));
        let provider_requests_for_handler = provider_requests.clone();
        let app = Router::new().route(
            "/chat/completions",
            post(move || {
                let provider_requests = provider_requests_for_handler.clone();
                async move {
                    let request_index = provider_requests.fetch_add(1, Ordering::SeqCst);
                    let content = if request_index == 0 {
                        "not-json"
                    } else {
                        r#"{"summary":"repaired"}"#
                    };
                    let event = serde_json::json!({
                        "choices": [{
                            "delta": {"content": content},
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
        let base_url = spawn_summary_test_server(app).await;
        let execution = astra_services::AdmittedModelExecution {
            offering_id: "summary-offering".to_string(),
            access_kind: astra_services::ModelAccessKind::SelfHosted,
            execution_placement: astra_services::ModelExecutionPlacement::Server,
            model_name: "summary-model".to_string(),
            wire_model_name: None,
            api_key: "summary-key".to_string(),
            base_url: base_url.clone(),
            provider: "openai".to_string(),
            cache_capability: None,
            thinking_capability: None,
            fixed_temperature: None,
            thinking_protocol: None,
            request_body_overrides: None,
            context_window: Some(8_192),
            max_completion_tokens: Some(1_024),
            header_overrides: HashMap::new(),
            completions_url_override: None,
            request_timeout_ms: None,
        };
        let persistence = Arc::new(RecoverFirstAdmissionPersistence::default());
        persistence
            .recover_attempt_zero
            .store(true, Ordering::Release);
        let ledger = DurableInferenceLedger::required_with_persistence(
            None,
            Some(&execution),
            "summary-user",
            Some(persistence.clone()),
        )
        .expect("durable summary ledger")
        .with_run_authority(super::super::durable::DurableInferenceRunAuthority::new(
            0,
            "summary-owner",
            0,
            None,
            None,
            None,
        ));
        let client = RuntimeSummaryClient::new_with_attempt_allocator(
            OwnedLlmExecutionRoute {
                model_name: execution.model_name.clone(),
                wire_model_name: None,
                api_key: execution.api_key.clone(),
                base_url,
                provider: execution.provider.clone(),
                thinking_capability: None,
                fixed_temperature: None,
                thinking_protocol: None,
                header_overrides: HashMap::new(),
                request_body_overrides: None,
                completions_url_override: None,
                request_timeout: None,
            },
            64,
            ledger,
            astra_turn_types::InferenceInvocationScope::Run {
                session_id: "summary-session".to_string(),
                run_id: "summary-run".to_string(),
                turn: 1,
                round: 1,
                operation_id: "summary_repair".to_string(),
                logical_attempt: 0,
            },
            DurableSummaryAttemptAllocator::default(),
        );
        let messages = vec![serde_json::json!({"role": "user", "content": "summarize"})];

        let malformed = client
            .summarize(InferencePurpose::Introspection, &messages)
            .await
            .expect("recovered provider response");
        assert_eq!(malformed.text, "not-json");
        let repaired = client
            .summarize(InferencePurpose::Introspection, &messages)
            .await
            .expect("repair response");

        assert_eq!(repaired.text, r#"{"summary":"repaired"}"#);
        assert_eq!(provider_requests.load(Ordering::SeqCst), 2);
        assert_eq!(
            *persistence
                .admitted_logical_attempts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            vec![0, 1, 2],
            "repair must advance after the authoritative recovered N+1 identity"
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
        let persistence = Arc::new(RecoverFirstAdmissionPersistence::default());
        let ledger = DurableInferenceLedger::required_with_persistence(
            None,
            Some(&execution),
            "summary-user",
            Some(persistence),
        )
        .expect("test ledger")
        .with_run_authority(summary_authority());
        let client = RuntimeSummaryClient::new_with_attempt_allocator(
            summary_route(&execution),
            1_024,
            ledger,
            summary_scope(),
            DurableSummaryAttemptAllocator::default(),
        );
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
    async fn independent_summary_clients_share_disjoint_pairs_without_serializing_provider_io() {
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
        let persistence = Arc::new(RecoverFirstAdmissionPersistence::default());
        let ledger = DurableInferenceLedger::required_with_persistence(
            None,
            Some(&execution),
            "summary-user",
            Some(persistence.clone()),
        )
        .expect("durable summary ledger")
        .with_run_authority(summary_authority());
        let allocator = DurableSummaryAttemptAllocator::default();
        let first = RuntimeSummaryClient::new_with_attempt_allocator(
            summary_route(&execution),
            64,
            ledger.clone(),
            summary_scope(),
            allocator.clone(),
        );
        let second = RuntimeSummaryClient::new_with_attempt_allocator(
            summary_route(&execution),
            64,
            ledger,
            summary_scope(),
            allocator,
        );
        let messages = vec![serde_json::json!({"role": "user", "content": "summarize"})];

        let (first_result, second_result) = tokio::join!(
            first.summarize(InferencePurpose::Introspection, &messages),
            second.summarize(InferencePurpose::Introspection, &messages),
        );
        assert_eq!(first_result.expect("first summary").text, "summary");
        assert_eq!(second_result.expect("second summary").text, "summary");
        let mut attempts = persistence
            .admitted_logical_attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        attempts.sort_unstable();
        assert_eq!(attempts, vec![0, 2]);
        assert_eq!(provider_requests.load(Ordering::SeqCst), 2);
        assert_eq!(
            max_active_requests.load(Ordering::SeqCst),
            2,
            "the identity allocator must not hold its mutex across provider I/O"
        );
    }

    #[tokio::test]
    async fn exhausted_summary_attempt_pair_fails_closed_before_admission_or_provider_io() {
        let execution = summary_execution("http://127.0.0.1:1".to_string());
        let persistence = Arc::new(RecoverFirstAdmissionPersistence::default());
        let ledger = DurableInferenceLedger::required_with_persistence(
            None,
            Some(&execution),
            "summary-user",
            Some(persistence.clone()),
        )
        .expect("durable summary ledger")
        .with_run_authority(summary_authority());
        let client = RuntimeSummaryClient::new_with_attempt_allocator(
            summary_route(&execution),
            64,
            ledger,
            summary_scope(),
            DurableSummaryAttemptAllocator::with_next_logical_attempt(u64::from(u32::MAX)),
        );
        let messages = vec![serde_json::json!({"role": "user", "content": "summarize"})];

        let error = client
            .summarize(InferencePurpose::Introspection, &messages)
            .await
            .expect_err("N without an available N+1 recovery identity must fail closed");
        assert_eq!(error.kind, astra_core::ErrorKind::ContractViolation);
        assert!(error.message.contains("logical attempt space is exhausted"));
        assert!(
            persistence
                .admitted_logical_attempts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "overflow must fail before durable admission and provider I/O"
        );
    }

    #[tokio::test]
    async fn reconstructed_host_allocator_starts_after_durable_prior_pair() {
        let provider_requests = Arc::new(AtomicU32::new(0));
        let execution = summary_execution(
            spawn_counted_summary_server(
                provider_requests.clone(),
                std::time::Duration::from_millis(1),
            )
            .await,
        );
        let persistence = Arc::new(RecoverFirstAdmissionPersistence::default());
        let ledger = DurableInferenceLedger::required_with_persistence(
            None,
            Some(&execution),
            "summary-user",
            Some(persistence.clone()),
        )
        .expect("durable summary ledger")
        .with_run_authority(summary_authority());
        let messages = vec![serde_json::json!({"role": "user", "content": "summarize"})];

        RuntimeSummaryClient::new_with_attempt_allocator(
            summary_route(&execution),
            64,
            ledger.clone(),
            summary_scope(),
            DurableSummaryAttemptAllocator::default(),
        )
        .summarize(InferencePurpose::Introspection, &messages)
        .await
        .expect("prior host summary");
        RuntimeSummaryClient::new_with_attempt_allocator(
            summary_route(&execution),
            64,
            ledger,
            summary_scope(),
            DurableSummaryAttemptAllocator::default(),
        )
        .summarize(InferencePurpose::Introspection, &messages)
        .await
        .expect("reconstructed host summary");

        assert_eq!(provider_requests.load(Ordering::SeqCst), 2);
        assert_eq!(
            *persistence
                .admitted_logical_attempts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            vec![0, 2],
            "a fresh in-memory allocator must derive its next pair from durable history"
        );
    }

    #[tokio::test]
    async fn concurrent_fresh_allocators_reread_after_typed_identity_collision() {
        let provider_requests = Arc::new(AtomicU32::new(0));
        let execution = summary_execution(
            spawn_counted_summary_server(
                provider_requests.clone(),
                std::time::Duration::from_millis(10),
            )
            .await,
        );
        let persistence = Arc::new(RecoverFirstAdmissionPersistence::default());
        *persistence
            .cursor_barrier
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(Arc::new(tokio::sync::Barrier::new(2)));
        persistence
            .cursor_barrier_reads_remaining
            .store(2, Ordering::Release);
        let ledger = DurableInferenceLedger::required_with_persistence(
            None,
            Some(&execution),
            "summary-user",
            Some(persistence.clone()),
        )
        .expect("durable summary ledger")
        .with_run_authority(summary_authority());
        let first = RuntimeSummaryClient::new_with_attempt_allocator(
            summary_route(&execution),
            64,
            ledger.clone(),
            summary_scope(),
            DurableSummaryAttemptAllocator::default(),
        );
        let second = RuntimeSummaryClient::new_with_attempt_allocator(
            summary_route(&execution),
            64,
            ledger,
            summary_scope(),
            DurableSummaryAttemptAllocator::default(),
        );
        let messages = vec![serde_json::json!({"role": "user", "content": "summarize"})];

        let (first_result, second_result) = tokio::join!(
            first.summarize(InferencePurpose::Introspection, &messages),
            second.summarize(InferencePurpose::Introspection, &messages),
        );
        first_result.expect("first fresh-host summary");
        second_result.expect("collision loser advances to the next durable pair");
        let mut attempts = persistence
            .admitted_logical_attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        attempts.sort_unstable();
        assert_eq!(attempts, vec![0, 2]);
        assert_eq!(
            provider_requests.load(Ordering::SeqCst),
            2,
            "the losing identity collision must not authorize duplicate provider I/O"
        );
        assert_eq!(
            persistence.admission_conflicts.load(Ordering::SeqCst),
            1,
            "the fresh-host loser must exercise typed occupied recovery"
        );
    }
}
