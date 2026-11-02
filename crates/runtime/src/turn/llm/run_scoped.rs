//! Request-local model observations under the existing Run authority.
//!
//! Model requests do not create another durable owner, lease or recovery debt.
//! A lost process ends the in-flight request; Run/Context and tool invocation
//! persistence retain their own contracts.

use super::client::{
    ProviderAttemptAdmission, ProviderAttemptObserver, ProviderWireRequestIdentity,
};
use super::client::{ProviderAttemptObservation, ProviderRequestObservation};
use super::durable::DurableInferenceRunAuthority;
use crate::turn::token_usage::TokenUsagePresence;
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::sync::Mutex;

pub(crate) struct RunScopedProviderObserver {
    authority: DurableInferenceRunAuthority,
    #[cfg(any(test, feature = "e2e-hooks"))]
    observations: Option<(
        crate::server::provider_test_support::ProviderObservationsFixture,
        astra_turn_types::InferenceInvocationScope,
        String,
    )>,
    attempts: Mutex<BTreeMap<u32, (ProviderAttemptObservation, TokenUsagePresence)>>,
}

impl RunScopedProviderObserver {
    pub(crate) fn new(authority: DurableInferenceRunAuthority) -> Self {
        Self {
            authority,
            #[cfg(any(test, feature = "e2e-hooks"))]
            observations: None,
            attempts: Mutex::new(BTreeMap::new()),
        }
    }

    #[cfg(any(test, feature = "e2e-hooks"))]
    pub(crate) fn with_observations(
        mut self,
        observations: crate::server::provider_test_support::ProviderObservationsFixture,
        scope: astra_turn_types::InferenceInvocationScope,
    ) -> Self {
        self.observations = Some((observations, scope, uuid::Uuid::new_v4().to_string()));
        self
    }

    #[cfg(any(test, feature = "e2e-hooks"))]
    fn publish_observations(&self, finished: bool) {
        if let Some((observations, scope, id)) = self.observations.as_ref() {
            observations.record(
                id,
                scope,
                self.authority.observed_authority(),
                self.facts(),
                finished,
            );
        }
    }

    pub(crate) fn facts(&self) -> Vec<ProviderAttemptObservation> {
        self.attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .map(|(fact, presence)| {
                let mut fact = fact.clone();
                if fact.dispatch_started {
                    fact.measured_input_tokens = (!presence.input_invalid)
                        .then_some(presence.measured_input_tokens)
                        .flatten();
                    let unknown =
                        astra_turn_types::CanonicalTokenUsage::new(None, None, None, None)
                            .expect("unknown usage is valid");
                    fact.qualified_usage = Some(match fact.terminal.as_ref() {
                        Some(terminal)
                            if terminal.usage_status
                                != astra_services::InferenceUsageStatus::Unavailable =>
                        {
                            let usage = &terminal.usage;
                            astra_turn_types::CanonicalTokenUsage::new(
                                (presence.fresh_input_tokens && !presence.input_invalid)
                                    .then_some(usage.input.fresh_input_tokens),
                                (presence.cache_read_tokens && !presence.input_invalid)
                                    .then_some(usage.input.cache_read_tokens),
                                (presence.cache_creation_tokens && !presence.input_invalid)
                                    .then_some(usage.input.cache_creation_tokens),
                                (presence.output_tokens && !presence.output_invalid)
                                    .then_some(usage.output_tokens),
                            )
                            .unwrap_or(unknown)
                        }
                        _ => unknown,
                    });
                }
                fact
            })
            .collect()
    }

    pub(crate) fn dispatched(&self) -> bool {
        self.attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .any(|(fact, _)| fact.dispatch_started)
    }
}

#[cfg(any(test, feature = "e2e-hooks"))]
impl Drop for RunScopedProviderObserver {
    fn drop(&mut self) {
        self.publish_observations(true);
    }
}

#[async_trait]
impl ProviderAttemptObserver for RunScopedProviderObserver {
    async fn begin_attempt(
        &self,
        wire: &ProviderWireRequestIdentity,
        retained_attempt: Option<u32>,
    ) -> Result<ProviderAttemptAdmission, astra_core::ClassifiedError> {
        if let Some(error) = self.authority.local_fence_error("provider dispatch") {
            return Err(error);
        }
        let mut attempts = self
            .attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let index = retained_attempt.unwrap_or(u32::try_from(attempts.len()).map_err(|_| {
            astra_core::ClassifiedError::new(
                astra_core::ErrorKind::ContractViolation,
                "model attempt count overflow",
            )
        })?);
        if retained_attempt.is_some() && !attempts.contains_key(&index) {
            return Err(astra_core::ClassifiedError::new(
                astra_core::ErrorKind::ContractViolation,
                "retained model attempt has no prepared request",
            ));
        }
        if let Some((fact, _)) = attempts.get(&index) {
            if fact.dispatch_started || !fact.request.matches_wire(wire) {
                return Err(astra_core::ClassifiedError::new(
                    astra_core::ErrorKind::ContractViolation,
                    "retained model attempt changed its request or was already dispatched",
                ));
            }
        } else {
            attempts.insert(
                index,
                (
                    ProviderAttemptObservation {
                        request: ProviderRequestObservation::from_wire(
                            uuid::Uuid::new_v4().to_string(),
                            index,
                            wire,
                        ),
                        dispatch_started: false,
                        terminal: None,
                        qualified_usage: None,
                        measured_input_tokens: None,
                    },
                    TokenUsagePresence::default(),
                ),
            );
        }
        drop(attempts);
        #[cfg(any(test, feature = "e2e-hooks"))]
        self.publish_observations(false);
        Ok(if self.authority.is_paused() {
            ProviderAttemptAdmission::Paused(index)
        } else {
            ProviderAttemptAdmission::Admitted(index)
        })
    }

    async fn wait_for_retained_pause(&self) -> Result<(), astra_core::ClassifiedError> {
        self.authority.wait_for_retained_pause().await
    }

    async fn finish_attempt(
        &self,
        index: u32,
        terminal: &astra_services::InferenceInvocationTerminal,
    ) -> Result<(), astra_core::ClassifiedError> {
        let mut attempts = self
            .attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (fact, _) = attempts.get_mut(&index).ok_or_else(|| {
            astra_core::ClassifiedError::new(
                astra_core::ErrorKind::ContractViolation,
                "model terminal has no observed request",
            )
        })?;
        if fact
            .terminal
            .as_ref()
            .is_some_and(|observed| observed != terminal)
        {
            return Err(astra_core::ClassifiedError::new(
                astra_core::ErrorKind::ContractViolation,
                "model attempt terminal changed after it was observed",
            ));
        }
        fact.terminal = Some(terminal.clone());
        Ok(())
    }

    fn note_usage_presence(&self, index: u32, presence: TokenUsagePresence) {
        if let Some((_, observed)) = self
            .attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(&index)
        {
            observed.merge(presence);
        }
    }

    fn note_dispatch_started(&self, index: u32) {
        if let Some((fact, _)) = self
            .attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(&index)
        {
            fact.dispatch_started = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn authority(
        cancelled: Arc<AtomicBool>,
        lost: Arc<AtomicBool>,
        paused: Arc<AtomicBool>,
    ) -> DurableInferenceRunAuthority {
        DurableInferenceRunAuthority::new(7, "run-owner", 0, Some(cancelled), None, Some(lost))
            .with_retained_pause_control(Some(paused), None)
    }

    fn wire() -> ProviderWireRequestIdentity {
        // Construct through the transport owner, not a parallel wire hash.
        super::super::client::prepare_streaming_call(
            super::super::client::LlmCall {
                purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
                messages: &[],
                tools: &[],
                cache_capability: None,
                route: super::super::client::LlmExecutionRoute {
                    subscription: None,
                    model_name: "fixture-model",
                    api_key: "fixture-key",
                    base_url: "http://127.0.0.1:1",
                    provider: "openai",
                    fixed_temperature: None,
                    supported_reasoning_efforts: None,
                    thinking_protocol: None,
                    wire_model_name: None,
                    header_overrides: None,
                    request_body_overrides: None,
                    completions_url_override: None,
                    request_timeout: None,
                },
                max_output_tokens: Some(16),
                temperature: None,
                thinking: &astra_turn_core::thinking_config::ThinkingConfig::Off,
            },
            super::super::client::RuntimeToolChoice::None,
        )
        .unwrap()
        .identity()
        .clone()
    }

    #[tokio::test]
    async fn pause_retains_one_request_and_cancel_prevents_dispatch() {
        let cancel = Arc::new(AtomicBool::new(false));
        let pause = Arc::new(AtomicBool::new(true));
        let observer = RunScopedProviderObserver::new(authority(
            cancel.clone(),
            Arc::new(AtomicBool::new(false)),
            pause.clone(),
        ));
        let wire = wire();
        assert!(matches!(
            observer.begin_attempt(&wire, None).await.unwrap(),
            ProviderAttemptAdmission::Paused(0)
        ));
        assert!(!observer.dispatched());
        pause.store(false, Ordering::Release);
        assert!(matches!(
            observer.begin_attempt(&wire, Some(0)).await.unwrap(),
            ProviderAttemptAdmission::Admitted(0)
        ));
        assert_eq!(observer.facts().len(), 1);
        cancel.store(true, Ordering::Release);
        assert_eq!(
            observer.begin_attempt(&wire, None).await.unwrap_err().kind,
            astra_core::ErrorKind::Cancelled
        );
        assert_eq!(observer.facts().len(), 1);
    }

    #[tokio::test]
    async fn lost_run_authority_never_creates_an_attempt() {
        let observer = RunScopedProviderObserver::new(authority(
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(false)),
        ));
        assert!(observer.begin_attempt(&wire(), None).await.is_err());
        assert!(observer.facts().is_empty());
    }

    #[tokio::test]
    async fn admission_is_not_dispatch_and_dispatched_unknown_usage_stays_unknown() {
        let observer = RunScopedProviderObserver::new(authority(
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        ));
        observer.begin_attempt(&wire(), None).await.unwrap();
        assert_eq!(observer.facts()[0].qualified_usage, None);
        observer.note_dispatch_started(0);
        assert_eq!(
            observer.facts()[0].qualified_usage,
            Some(astra_turn_types::CanonicalTokenUsage::new(None, None, None, None).unwrap())
        );
        assert!(observer.begin_attempt(&wire(), Some(0)).await.is_err());
    }
    #[tokio::test]
    async fn physical_input_survives_partial_failed_and_missing_attempts_without_double_counting() {
        let observer = RunScopedProviderObserver::new(authority(
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        ));
        // A measured prompt-only failed attempt, a partitioned successful one,
        // and a dispatched attempt with no usage must retain separate evidence.
        for index in 0..3 {
            observer.begin_attempt(&wire(), None).await.unwrap();
            observer.note_dispatch_started(index);
        }
        observer.note_usage_presence(
            0,
            TokenUsagePresence {
                measured_input_tokens: Some(17),
                ..Default::default()
            },
        );
        observer
            .finish_attempt(
                0,
                &crate::turn::llm::durable::terminal_from_error(&astra_core::ClassifiedError::new(
                    astra_core::ErrorKind::Cancelled,
                    "cancelled",
                )),
            )
            .await
            .unwrap();
        observer.note_usage_presence(
            1,
            TokenUsagePresence {
                fresh_input_tokens: true,
                cache_read_tokens: true,
                cache_creation_tokens: true,
                measured_input_tokens: Some(30),
                ..Default::default()
            },
        );
        observer
            .finish_attempt(
                1,
                &astra_services::InferenceInvocationTerminal {
                    status: astra_services::InferenceTerminalStatus::Succeeded,
                    usage: astra_services::InferenceUsage {
                        input: astra_turn_types::NormalizedPromptCacheUsage::new(10, 13, 7),
                        output_tokens: 0,
                    },
                    usage_status: astra_services::InferenceUsageStatus::ProviderPartial,
                    provider_response_id: None,
                    error_kind: None,
                    error_message: None,
                },
            )
            .await
            .unwrap();
        let facts = observer.facts();
        assert_eq!(facts[0].measured_input_tokens, Some(17));
        assert_eq!(facts[0].qualified_usage.unwrap().input_tokens(), None);
        assert_eq!(facts[1].qualified_usage.unwrap().input_tokens(), Some(10));
        assert_eq!(facts[2].measured_input_tokens, None);
        assert_eq!(
            facts
                .iter()
                .map(|fact| fact.observed_input_tokens())
                .sum::<u64>(),
            47
        );
        assert_eq!(
            observer.facts(),
            facts,
            "reading facts never consumes or adds usage"
        );
        observer.note_usage_presence(
            0,
            TokenUsagePresence {
                input_invalid: true,
                ..Default::default()
            },
        );
        assert_eq!(observer.facts()[0].measured_input_tokens, None);
        assert_eq!(observer.facts()[0].observed_input_tokens(), 0);
    }
}
