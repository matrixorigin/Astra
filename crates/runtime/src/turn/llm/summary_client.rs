use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use astra_turn_core::cloud_summary::{SummaryLlmClient, SummaryResponse};
use astra_turn_core::thinking_config::{ThinkingConfig, ThinkingEffort};
use astra_turn_types::InferencePurpose;

use super::client::{LlmCall, OwnedLlmExecutionRoute};
use super::durable::DurableInferenceLedger;

#[cfg(test)]
use super::client::{call_llm_nonstream, global_llm_client, llm_nonstream_timeout};

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

/// Runtime-owned adapter from the provider execution contract to summary work.
/// Provider-specific request construction, authentication, timeouts, and
/// response parsing remain centralized in the canonical LLM client.
#[derive(Clone)]
pub(crate) struct RuntimeSummaryClient {
    route: OwnedLlmExecutionRoute,
    max_output_tokens: usize,
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
            execution: SummaryExecution::Durable(Box::new(DurableSummaryExecution {
                ledger,
                base_scope,
                attempt_allocator,
            })),
        }
    }

    /// Auxiliary semantic decisions need a short, predictable response. Some
    /// models cannot turn reasoning off but do offer an explicit low-effort
    /// control. Use only the probe-derived capability contract; generic
    /// OpenAI-compatible endpoints keep the established `Off` shape.
    fn thinking_for(purpose: InferencePurpose, route: &OwnedLlmExecutionRoute) -> ThinkingConfig {
        match (purpose, route.thinking_capability) {
            // A persisted EffortOnly value may predate the provider's typed
            // suppression capability. The admitted endpoint protocol is the
            // stronger fact for bounded auxiliary work, so prefer Off when
            // the route can express it natively; the client will add the
            // exact suppression field to the wire request.
            (
                InferencePurpose::Introspection,
                Some(astra_services::models::ThinkingCapability::EffortOnly),
            ) if astra_turn_core::thinking_config::openai_thinking_control(
                &route.provider,
                &route.base_url,
            ) != astra_turn_core::thinking_config::OpenAiThinkingControl::None =>
            {
                ThinkingConfig::Off
            }
            (
                InferencePurpose::Introspection,
                Some(astra_services::models::ThinkingCapability::EffortOnly),
            ) => ThinkingConfig::Adaptive {
                effort: ThinkingEffort::Low,
            },
            _ => ThinkingConfig::Off,
        }
    }

    /// Closed-schema runtime decisions must be reproducible for the same
    /// admitted context. Main-agent generation and compaction keep their own
    /// sampling policy; only the bounded introspection classifiers use zero
    /// temperature.
    fn temperature_for(purpose: InferencePurpose, thinking: &ThinkingConfig) -> Option<f64> {
        (matches!(purpose, InferencePurpose::Introspection) && thinking.is_off()).then_some(0.0)
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
    ) -> Result<SummaryResponse, String> {
        let thinking = Self::thinking_for(purpose, &self.route);
        let result = match &self.execution {
            SummaryExecution::Durable(execution) => {
                let DurableSummaryExecution {
                    ledger,
                    base_scope,
                    attempt_allocator,
                } = execution.as_ref();
                let allocator_scope_key =
                    Self::attempt_allocator_scope_key(base_scope, purpose, &self.route)?;
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
                        .await
                        .map_err(|error| error.to_string())?;
                    // Reserve both identities in one short, non-async critical
                    // section. No allocator lock may span provider or database I/O.
                    let requested_logical_attempt = attempt_allocator
                        .reserve_pair_at_least(&allocator_scope_key, durable_pair_base)?;
                    let outcome = ledger
                        .execute_stream(
                            base_scope.with_logical_attempt(requested_logical_attempt),
                            LlmCall {
                                purpose,
                                messages,
                                tools: &[],
                                cache_capability: None,
                                route: self.route.borrowed(),
                                max_output_tokens: Some(self.max_output_tokens),
                                temperature: Self::temperature_for(purpose, &thinking),
                                has_fallback: false,
                                thinking: &thinking,
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
                call_llm_nonstream(
                    global_llm_client(),
                    LlmCall {
                        purpose,
                        messages,
                        tools: &[],
                        cache_capability: None,
                        route: self.route.borrowed(),
                        max_output_tokens: Some(self.max_output_tokens),
                        temperature: Self::temperature_for(purpose, &thinking),
                        has_fallback: false,
                        thinking: &thinking,
                    },
                    llm_nonstream_timeout(),
                )
                .await
            }
        };
        match result {
            Ok(result) => Ok(SummaryResponse {
                text: result.full_text,
                is_ptl_error: false,
            }),
            Err(error) if error.kind == astra_core::ErrorKind::ContextWindow => {
                Ok(SummaryResponse {
                    text: String::new(),
                    is_ptl_error: true,
                })
            }
            Err(error) => Err(error.to_string()),
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
        let effort_only =
            route_with_capability(Some(astra_services::models::ThinkingCapability::EffortOnly));
        assert_eq!(
            RuntimeSummaryClient::thinking_for(InferencePurpose::Introspection, &effort_only),
            ThinkingConfig::Adaptive {
                effort: ThinkingEffort::Low,
            }
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
    fn closed_introspection_decisions_are_deterministic_without_changing_other_inference() {
        assert_eq!(
            RuntimeSummaryClient::temperature_for(
                InferencePurpose::Introspection,
                &ThinkingConfig::Off,
            ),
            Some(0.0)
        );
        assert_eq!(
            RuntimeSummaryClient::temperature_for(
                InferencePurpose::Introspection,
                &ThinkingConfig::Adaptive {
                    effort: ThinkingEffort::Low,
                },
            ),
            None,
            "thinking protocols own sampling and must not receive temperature"
        );
        for purpose in [
            InferencePurpose::PrimaryAgent,
            InferencePurpose::RequiredCompaction,
            InferencePurpose::MemoryExtraction,
        ] {
            assert_eq!(
                RuntimeSummaryClient::temperature_for(purpose, &ThinkingConfig::Off),
                None
            );
        }
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
        assert!(error.contains("logical attempt space is exhausted"));
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
