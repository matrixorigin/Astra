//! Canonical Runner execution coordinator shared by turns and auxiliary/API calls.
//! Transport readiness is a hint; durable attempt/custody rows remain authority.

use super::client::{
    LlmCallResult, LlmCancel, LlmStreamCallback, LlmStreamUpdate, RunnerInferencePreviewDecoder,
    collect_runner_response,
};
use astra_core::SharedPool;
use astra_turn_types::runner_inference::{
    RunnerInferenceAttemptIdentity, RunnerInferenceProgressBatch,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn emit_runner_preview(
    batch: &RunnerInferenceProgressBatch,
    expected: &RunnerInferenceAttemptIdentity,
    decoder: &mut RunnerInferencePreviewDecoder,
    stream_callback: &mut Option<&mut LlmStreamCallback<'_>>,
) {
    if batch.attempt != *expected {
        // The pool is keyed by (user, attempt), and ingress validates the same
        // full identity. Treat a mismatch as disposable corruption rather
        // than allowing another user's preview into this turn.
        decoder.mark_gap();
        tracing::warn!(
            expected_attempt_id = %expected.attempt_id.as_str(),
            received_attempt_id = %batch.attempt.attempt_id.as_str(),
            "discarding Runner progress for a different immutable attempt"
        );
        return;
    }
    for update in decoder.push_batch(batch) {
        if let Some(callback) = stream_callback.as_deref_mut() {
            callback(update);
        }
    }
}

// A wake is consumed immediately, never queued. Keep the bounded batch inline
// instead of allocating a second heap object for every preview delivery.
#[allow(clippy::large_enum_variant)]
enum RunnerWake {
    Readiness(Result<(), tokio::sync::watch::error::RecvError>),
    Cancel,
    Deadline,
    Preview(Result<RunnerInferenceProgressBatch, tokio::sync::broadcast::error::RecvError>),
}

/// Transport observation only. All durable state transitions remain in the
/// coordinator below; provisional output has strictly lower priority.
async fn next_runner_wake(
    readiness: &mut tokio::sync::watch::Receiver<
        astra_services::inference_execution::runner_wait::RunnerReadiness,
    >,
    preview: &mut Option<tokio::sync::broadcast::Receiver<RunnerInferenceProgressBatch>>,
    preview_open: bool,
    cancel: Option<LlmCancel<'_>>,
    deadline: tokio::time::Instant,
) -> RunnerWake {
    tokio::select! {
        biased;
        changed = readiness.changed() => RunnerWake::Readiness(changed),
        _ = super::client::wait_llm_cancel(cancel.unwrap_or(LlmCancel::None)), if cancel.is_some() => RunnerWake::Cancel,
        _ = async {
            // A newly created Sleep may first wait for the timer driver even
            // at an elapsed deadline. An always-ready preview must not win
            // that poll and postpone the already-expired control boundary.
            if deadline > tokio::time::Instant::now() {
                tokio::time::sleep_until(deadline).await;
            }
        } => RunnerWake::Deadline,
        message = async {
            match preview.as_mut() {
                Some(receiver) => receiver.recv().await,
                None => std::future::pending().await,
            }
        }, if preview_open => RunnerWake::Preview(message),
    }
}

/// Auxiliary calls use the same admission, custody and continuation owner as
/// agent rounds. Dropping this future leaves the durable Runner grant with the
/// existing reconciliation owner; it never synthesizes a Server terminal.
pub(crate) async fn execute_nonstream(
    pool: &SharedPool,
    edge_pool: &astra_server_types::edge_connection_pool::EdgeConnectionPool,
    ledger: &super::durable::DurableInferenceLedger,
    admitted: &astra_services::AdmittedModelExecution,
    scope: astra_turn_types::InferenceInvocationScope,
    call: RunnerAuxiliaryCall<'_>,
) -> Result<LlmCallResult, astra_core::ClassifiedError> {
    let astra_services::ModelExecutionMaterial::Runner(binding) = &admitted.execution_material
    else {
        return Err(astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ContractViolation,
            "Runner coordinator requires an admitted Runner Offering",
        ));
    };
    let wire_model = admitted
        .wire_model_name
        .as_deref()
        .unwrap_or(&admitted.model_name);
    let prepared = super::client::prepare_runner_request(
        call.messages,
        &[],
        wire_model,
        super::client::RunnerRequestOptions {
            max_output_tokens: Some(call.max_output_tokens),
            temperature: Some(call.temperature),
            thinking: &astra_turn_core::thinking_config::ThinkingConfig::Off,
            cache_capability: None,
            no_tool_choice: true,
        },
    )?;
    let invocation = ledger
        .admit(
            scope,
            call.purpose,
            &admitted.model_name,
            wire_model,
            &admitted.provider,
        )
        .await
        .map_err(|failure| failure.error)?;
    let result = call_runner_and_collect(
        pool,
        edge_pool,
        &invocation,
        binding,
        prepared,
        &admitted.model_name,
        call.timeout,
        LlmCancel::None,
        None,
    )
    .await;
    match result {
        Ok((result, _receipt)) => {
            invocation.finish_result(&result).await?;
            Ok(result)
        }
        Err(error) => {
            invocation.finish_error(&error).await?;
            Err(error)
        }
    }
}

pub(crate) struct RunnerAuxiliaryCall<'a> {
    pub purpose: astra_turn_types::InferencePurpose,
    pub messages: &'a [serde_json::Value],
    pub max_output_tokens: usize,
    pub temperature: f64,
    pub timeout: Duration,
}

#[cfg(test)]
#[test]
fn runner_o_series_compilation_uses_declared_bounded_completion_limit() {
    use super::client::{RunnerRequestOptions, prepare_runner_request};
    let prepared = prepare_runner_request(
        &[serde_json::json!({"role":"user", "content":"Reply with OK."})],
        &[],
        "o3",
        RunnerRequestOptions {
            max_output_tokens: Some(4),
            temperature: None,
            thinking: &astra_turn_core::thinking_config::ThinkingConfig::Off,
            cache_capability: None,
            no_tool_choice: false,
        },
    )
    .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&prepared.exact_body()).unwrap();
    assert_eq!(body["model"], "o3");
    assert_eq!(body["max_completion_tokens"], 4);
    assert!(body.get("max_tokens").is_none());
    assert!(body.get("temperature").is_none());
    assert_eq!(prepared.wire_output_limit(), Some(4));
}

#[cfg(test)]
#[test]
fn runner_auxiliary_compilation_preserves_wire_model_budget_and_temperature() {
    use super::client::{RunnerRequestOptions, prepare_runner_request};
    let messages = [serde_json::json!({"role":"user", "content":"hello"})];
    let prepared = prepare_runner_request(
        &messages,
        &[],
        "private-wire-model",
        RunnerRequestOptions {
            max_output_tokens: Some(123),
            temperature: Some(0.37),
            thinking: &astra_turn_core::thinking_config::ThinkingConfig::Off,
            cache_capability: None,
            no_tool_choice: true,
        },
    )
    .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&prepared.exact_body()).unwrap();
    assert_eq!(body["model"], "private-wire-model");
    assert_eq!(body["temperature"], 0.37);
    assert_eq!(body["max_completion_tokens"], 123);
    assert_eq!(body["stream"], true);
    assert!(prepared.authorized_tool_names().is_empty());
    assert_eq!(prepared.wire_output_limit(), Some(123));
}

pub(crate) fn runner_service_error(
    stage: &'static str,
    error: astra_services::ServiceError,
) -> astra_core::ClassifiedError {
    let kind = match error.kind {
        astra_services::ServiceErrorKind::Persistence => astra_core::ErrorKind::DatabaseError,
        astra_services::ServiceErrorKind::Network => astra_core::ErrorKind::Network,
        astra_services::ServiceErrorKind::Invalid | astra_services::ServiceErrorKind::NotFound => {
            astra_core::ErrorKind::InvalidRequest
        }
        astra_services::ServiceErrorKind::Verification
        | astra_services::ServiceErrorKind::Conflict
        | astra_services::ServiceErrorKind::ConflictTransient
        | astra_services::ServiceErrorKind::Internal => astra_core::ErrorKind::ContractViolation,
    };
    astra_core::ClassifiedError::new(kind, format!("Runner {stage} failed: {error}"))
}

#[tracing::instrument(name = "runner_inference", skip_all, fields(
    user_id = %binding.user_id,
    runner_id = %binding.definition.identity.runner_id.as_str(),
    session_id = tracing::field::Empty,
    run_id = tracing::field::Empty,
    invocation_id = tracing::field::Empty,
    attempt_id = tracing::field::Empty,
))]
pub(crate) async fn call_runner_and_collect(
    pool: &SharedPool,
    edge_pool: &astra_server_types::edge_connection_pool::EdgeConnectionPool,
    durable_invocation: &crate::turn::llm::durable::DurableInferenceInvocation,
    binding: &astra_services::runner_model_bindings::ResolvedRunnerModelBinding,
    prepared: crate::turn::llm::client::PreparedRunnerRequest,
    model_name: &str,
    provider_budget: Duration,
    cancel: LlmCancel<'_>,
    mut stream_callback: Option<&mut LlmStreamCallback<'_>>,
) -> Result<
    (
        LlmCallResult,
        astra_turn_types::runner_inference::RunnerInferenceContinuationReceipt,
    ),
    astra_core::ClassifiedError,
> {
    let started = Instant::now();
    let input = durable_invocation.runner_continuation_input();
    tracing::Span::current().record("session_id", input.scope.session_id().unwrap_or_default());
    tracing::Span::current().record("run_id", input.scope.run_id().unwrap_or_default());
    let reservation = edge_pool.runner_continuation_waiters.reserve(pool, &binding.user_id)
        .map_err(|error| {
            if error.kind == astra_services::ServiceErrorKind::ConflictTransient {
                astra_core::ClassifiedError::new(astra_core::ErrorKind::RateLimit,
                    "Runner request capacity is full. No provider request was authorized; try again shortly.")
            } else { runner_service_error("readiness reservation", error) }
        })?;
    let deadline_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
        .saturating_add(provider_budget.as_millis().try_into().unwrap_or(u64::MAX));
    let (attempt_index, grant) = durable_invocation
        .admit_runner_attempt(&prepared, binding, deadline_unix_ms)
        .await?;
    tracing::Span::current().record("invocation_id", grant.attempt.invocation_id.as_str());
    tracing::Span::current().record("attempt_id", grant.attempt.attempt_id.as_str());
    tracing::debug!(stage = "grant_committed", "Runner request authorized");
    let runner_id = binding.definition.identity.runner_id.as_str();
    // Subscribe before waking the Runner so a first preview cannot race past
    // the owner. This hub is same-pod and disposable; terminal custody below
    // remains the only authoritative response/usage/tool path.
    let mut preview_rx = stream_callback
        .is_some()
        .then(|| edge_pool.subscribe_runner_inference_progress(&grant.attempt))
        .flatten();
    let mut preview_open = preview_rx.is_some();
    let mut preview_decoder = RunnerInferencePreviewDecoder::default();
    edge_pool.notify_runner_inference(&binding.user_id, runner_id);

    use astra_services::inference_execution::runner_wait::RunnerReadiness;
    let wait_deadline =
        tokio::time::Instant::now() + provider_budget.saturating_sub(started.elapsed());
    let mut readiness = reservation
        .subscribe(&grant.attempt)
        .map_err(|error| runner_service_error("readiness subscription", error))?;
    let mut cancellation_recorded = false;
    loop {
        let ready = *readiness.borrow_and_update();
        match ready {
            RunnerReadiness::Ready => break,
            RunnerReadiness::Unavailable => {
                return Err(astra_core::ClassifiedError::new(
                    astra_core::ErrorKind::ContractViolation,
                    "Runner continuation is absent or quarantined",
                ));
            }
            RunnerReadiness::Waiting => {}
        }
        match next_runner_wake(
            &mut readiness,
            &mut preview_rx,
            preview_open,
            (!cancellation_recorded).then_some(cancel),
            wait_deadline,
        )
        .await
        {
            RunnerWake::Readiness(changed) => {
                if changed.is_err() {
                    return Err(astra_core::ClassifiedError::new(
                        astra_core::ErrorKind::ContractViolation,
                        "Runner readiness observer stopped",
                    ));
                }
            }
            // Disposable output must never starve cancellation or the
            // original deadline, even when a peer floods valid progress.
            RunnerWake::Cancel => {
                astra_services::inference_execution::runner::request_runner_cancellation(
                    pool,
                    &binding.user_id,
                    &grant.attempt,
                )
                .await
                .map_err(|error| runner_service_error("cancellation", error))?;
                edge_pool.notify_runner_inference(&binding.user_id, runner_id);
                cancellation_recorded = true;
                tracing::debug!(
                    stage = "cancellation_requested",
                    "Runner cancellation recorded"
                );
            }
            RunnerWake::Deadline => {
                return Err(astra_core::ClassifiedError::new(
                    astra_core::ErrorKind::ProviderDeadline,
                    "Runner did not return a durable provider terminal before the inference deadline",
                ));
            }
            RunnerWake::Preview(preview) => {
                match preview {
                    Ok(batch) => emit_runner_preview(
                        &batch,
                        &grant.attempt,
                        &mut preview_decoder,
                        &mut stream_callback,
                    ),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // A gap is intentionally terminal for provisional
                        // rendering. The canonical response will replay from
                        // custody and the final-update filter will avoid
                        // duplicating the prefix already shown.
                        preview_decoder.mark_gap();
                        preview_open = false;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        preview_open = false;
                    }
                }
            }
        }
    }
    // Readiness and the final local journal commit can become visible in
    // either order. Drain already queued previews once before custody claim;
    // anything arriving later is safely converged by the terminal collector.
    // A fixed work budget prevents a concurrent/malicious progress producer
    // from postponing the authoritative custody claim indefinitely. Any
    // undisplayed bytes are supplied by the terminal collector below.
    for _ in 0..8 {
        if !preview_open {
            break;
        }
        let next = match preview_rx.as_mut() {
            Some(receiver) => receiver.try_recv(),
            None => break,
        };
        match next {
            Ok(batch) => emit_runner_preview(
                &batch,
                &grant.attempt,
                &mut preview_decoder,
                &mut stream_callback,
            ),
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {
                preview_decoder.mark_gap();
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
            Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                preview_open = false;
            }
        }
    }
    // Only readiness observation is shared/batched; this exact claim remains
    // the sole authorization boundary and revalidates all scope/owner facts.
    let claim = astra_services::inference_execution::runner::claim_runner_continuation(
        pool,
        durable_invocation.runner_continuation_input(),
        &grant.attempt,
        Some(durable_invocation.owner_token()),
    )
    .await
    .map_err(|error| runner_service_error("continuation claim", error))?;

    let response_bytes =
        astra_services::inference_execution::runner::load_runner_response_custody(pool, &claim)
            .await
            .map_err(|error| runner_service_error("response custody load", error))?;
    let response: astra_turn_types::runner_inference::RunnerInferenceResponse =
        serde_json::from_slice(response_bytes.as_bytes()).map_err(|_| {
            astra_core::ClassifiedError::new(
                astra_core::ErrorKind::ContractViolation,
                "Runner response custody contains an invalid response envelope",
            )
        })?;
    let result = if stream_callback.is_some() {
        let mut terminal_callback = |update: LlmStreamUpdate| {
            if let Some(update) = preview_decoder.forward_terminal_update(update)
                && let Some(callback) = stream_callback.as_deref_mut()
            {
                callback(update);
            }
        };
        collect_runner_response(
            response,
            model_name,
            started,
            prepared.authorized_tool_names(),
            prepared.wire_output_limit(),
            Some(&mut terminal_callback),
        )
        .await
    } else {
        collect_runner_response(
            response,
            model_name,
            started,
            prepared.authorized_tool_names(),
            prepared.wire_output_limit(),
            None,
        )
        .await
    };

    // Parsing may downgrade a physically complete stream to a logical failure
    // (for example an unauthorized tool call), but it may never rewrite the
    // Runner-observed usage or provider response identity.
    let physical = claim.physical_terminal().clone();
    let mut logical = runner_logical_terminal(&physical, &result);
    logical.usage = physical.usage.clone();
    logical.usage_status = physical.usage_status;
    logical.provider_response_id = physical.provider_response_id.clone();
    let mut tx = pool.get().begin().await.map_err(|error| {
        astra_core::ClassifiedError::new(
            astra_core::ErrorKind::DatabaseError,
            format!("Runner continuation transaction failed: {error}"),
        )
    })?;
    astra_services::inference_execution::runner::settle_runner_continuation_tx(
        &mut tx, &claim, &logical,
    )
    .await
    .map_err(|error| runner_service_error("continuation commit", error))?;
    tx.commit().await.map_err(|error| {
        astra_core::ClassifiedError::new(
            astra_core::ErrorKind::DatabaseError,
            format!("Runner continuation commit failed: {error}"),
        )
    })?;
    tracing::debug!(stage = "terminal_committed", elapsed_ms = started.elapsed().as_millis() as u64,
        physical_status = ?physical.status, logical_status = ?logical.status,
        "Runner inference settled from durable custody");
    durable_invocation
        .observe_runner_terminal(attempt_index, physical, logical)
        .await;
    result.map(|result| (result, claim.checkpoint_receipt()))
}

#[cfg(test)]
#[tokio::test]
async fn runner_control_and_deadline_take_priority_over_ready_preview() {
    use astra_services::inference_execution::runner_wait::RunnerReadiness;
    let (_state, mut readiness) = tokio::sync::watch::channel(RunnerReadiness::Waiting);
    let (sender, receiver) = tokio::sync::broadcast::channel(1);
    drop(sender); // recv is immediately ready, like a continuously busy peer.
    let mut preview = Some(receiver);
    let cancellation = tokio_util::sync::CancellationToken::new();
    cancellation.cancel();
    assert!(matches!(
        next_runner_wake(
            &mut readiness,
            &mut preview,
            true,
            Some(LlmCancel::Token(&cancellation)),
            tokio::time::Instant::now() + Duration::from_secs(60)
        )
        .await,
        RunnerWake::Cancel
    ));
    assert!(matches!(
        next_runner_wake(
            &mut readiness,
            &mut preview,
            true,
            None,
            tokio::time::Instant::now()
        )
        .await,
        RunnerWake::Deadline
    ));
}

fn runner_logical_terminal(
    physical: &astra_services::InferenceInvocationTerminal,
    result: &Result<LlmCallResult, astra_core::ClassifiedError>,
) -> astra_services::InferenceInvocationTerminal {
    match result {
        Ok(_) => physical.clone(),
        Err(error) => {
            let mut logical = crate::turn::llm::durable::terminal_from_error(error);
            // The Runner's physical terminal owns delivery certainty. Server-side
            // decoding may turn a complete provider response into a logical
            // failure, but it cannot discard positive no-dispatch evidence or
            // invent certainty for an ambiguous physical attempt.
            logical.status = match physical.status {
                astra_services::InferenceTerminalStatus::DeliveryUnknown => {
                    astra_services::InferenceTerminalStatus::DeliveryUnknown
                }
                astra_services::InferenceTerminalStatus::Cancelled => {
                    astra_services::InferenceTerminalStatus::Cancelled
                }
                astra_services::InferenceTerminalStatus::Succeeded
                | astra_services::InferenceTerminalStatus::Failed => {
                    astra_services::InferenceTerminalStatus::Failed
                }
            };
            logical
        }
    }
}

#[cfg(test)]
#[test]
fn runner_logical_failure_preserves_physical_delivery_certainty() {
    use astra_services::{
        InferenceInvocationTerminal, InferenceTerminalStatus, InferenceUsage, InferenceUsageStatus,
    };

    let physical = |status| InferenceInvocationTerminal {
        status,
        usage: InferenceUsage::default(),
        usage_status: InferenceUsageStatus::Unavailable,
        provider_response_id: None,
        error_kind: Some("runner_provider_transport".to_string()),
        error_message: None,
    };
    let failure: Result<LlmCallResult, astra_core::ClassifiedError> =
        Err(astra_core::ClassifiedError::new(
            astra_core::ErrorKind::StreamTransport,
            "Runner transport failed",
        ));
    let locally_definitive_failure: Result<LlmCallResult, astra_core::ClassifiedError> =
        Err(astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ProviderDeadline,
            "Runner response decoding exceeded its local bound",
        ));

    assert_eq!(
        runner_logical_terminal(&physical(InferenceTerminalStatus::Failed), &failure).status,
        InferenceTerminalStatus::Failed,
        "positive no-dispatch evidence must remain safely terminal"
    );
    assert_eq!(
        runner_logical_terminal(
            &physical(InferenceTerminalStatus::DeliveryUnknown),
            &locally_definitive_failure,
        )
        .status,
        InferenceTerminalStatus::DeliveryUnknown,
        "ambiguous physical delivery must never become retry-safe"
    );
}
