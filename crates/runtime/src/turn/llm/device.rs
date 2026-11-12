//! Device custody is interpreted by the canonical provider collectors.
use super::*;
use std::sync::Arc;

/// Request-local transport under an existing canonical Run. The database
/// retains immutable grant/response evidence; this type owns no Run lease.
#[derive(Clone)]
pub(crate) struct RunDeviceInference {
    pool: astra_core::SharedPool,
    models: Arc<dyn astra_services::ModelService>,
    edges: astra_server_types::edge_connection_pool::EdgeConnectionPool,
    user_id: String,
    scope: astra_turn_types::InferenceInvocationScope,
    authority: super::super::durable::DurableInferenceRunAuthority,
}

impl RunDeviceInference {
    pub(crate) fn new(
        pool: astra_core::SharedPool,
        edges: astra_server_types::edge_connection_pool::EdgeConnectionPool,
        models: Arc<dyn astra_services::ModelService>,
        user_id: String,
        scope: astra_turn_types::InferenceInvocationScope,
        authority: super::super::durable::DurableInferenceRunAuthority,
    ) -> Result<Self, astra_core::ClassifiedError> {
        if scope.run_id().is_none() {
            return Err(responses::error(
                astra_core::ErrorKind::ContractViolation,
                "device_run_scope_missing",
            ));
        }
        Ok(Self {
            pool,
            models,
            edges,
            user_id,
            scope,
            authority,
        })
    }

    pub(super) async fn execute(
        &self,
        mut prepared: PreparedStreamingCall<'_>,
        cancel: LlmCancel<'_>,
        callback: Option<&mut LlmStreamCallback<'_>>,
        observer: &dyn ProviderAttemptObserver,
        budget: Option<Duration>,
    ) -> Result<LlmCallResult, astra_core::ClassifiedError> {
        use astra_services::inference_execution::runner as custody;
        let binding = prepared.route.device_binding.ok_or_else(|| {
            responses::error(
                astra_core::ErrorKind::ContractViolation,
                "device_binding_missing",
            )
        })?;
        if binding.user_id != self.user_id || prepared.route.subscription.is_some() {
            return Err(responses::error(
                astra_core::ErrorKind::ContractViolation,
                "device_binding_owner_mismatch",
            ));
        }
        custody::require_runner_purpose(prepared.purpose).map_err(service_error)?;
        let admission_started = Instant::now();
        let offering = astra_services::runner_model_bindings::runner_offering_id(
            &self.user_id,
            &binding.definition.identity,
        );
        let fresh = self
            .models
            .admit_model_offering(self.user_id.clone(), offering)
            .await
            .map_err(admission_error)?;
        if fresh.device_binding.as_ref() != Some(binding) {
            return Err(responses::error(
                astra_core::ErrorKind::InvalidRequest,
                "device_binding_changed",
            ));
        }
        prepared.preparation_elapsed = prepared
            .preparation_elapsed
            .saturating_add(admission_started.elapsed());

        if let Some(error) = self.authority.local_fence_error("device inference") {
            return Err(error);
        }
        let index = begin_observed_provider_attempt(Some(observer), prepared.request.identity())
            .await?
            .ok_or_else(|| {
                responses::error(
                    astra_core::ErrorKind::ContractViolation,
                    "device_attempt_missing",
                )
            })?;
        let phase = std::sync::atomic::AtomicU8::new(0);
        let result = self
            .execute_attempt(prepared, cancel, callback, observer, budget, index, &phase)
            .await;
        let mut terminal = match &result {
            Ok(result) => provider_attempt_terminal_from_result(result),
            Err(error) => provider_attempt_terminal_from_error(error),
        };
        if result.is_err() && phase.load(Ordering::Acquire) == 1 {
            terminal.status = astra_services::InferenceTerminalStatus::DeliveryUnknown;
        }
        observer.finish_attempt(index, &terminal).await?;
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_attempt(
        &self,
        prepared: PreparedStreamingCall<'_>,
        cancel: LlmCancel<'_>,
        mut callback: Option<&mut LlmStreamCallback<'_>>,
        observer: &dyn ProviderAttemptObserver,
        budget: Option<Duration>,
        index: u32,
        phase: &std::sync::atomic::AtomicU8,
    ) -> Result<LlmCallResult, astra_core::ClassifiedError> {
        use astra_services::inference_execution::runner as custody;
        use astra_services::inference_execution::runner_wait::RunnerReadiness;
        let started = Instant::now();
        let binding = prepared
            .route
            .device_binding
            .expect("device route validated before admission");
        let input = astra_services::InferenceInvocationInput {
            user_id: self.user_id.clone(),
            scope: self.scope.clone(),
            run_authority: Some(self.authority.observed_authority().clone()),
            offering_id: astra_services::runner_model_bindings::runner_offering_id(
                &self.user_id,
                &binding.definition.identity,
            ),
            resolved_model_name: binding.definition.model_name.as_str().into(),
            upstream_model_name: binding.definition.model_name.as_str().into(),
            provider: binding.definition.provider().into(),
            purpose: prepared.purpose,
            execution_placement: astra_services::ModelExecutionPlacement::Edge,
            access_kind: astra_services::ModelAccessKind::ThisDevice,
        };
        let invocation =
            astra_services::plan_inference_invocation(input.clone()).map_err(service_error)?;
        let wire = super::super::durable::service_wire_identity(prepared.request.identity())?;
        let budget = budget.unwrap_or_else(llm_total_budget);
        let remaining = budget.saturating_sub(prepared.preparation_elapsed);
        if remaining.is_zero() {
            return Err(responses::error(
                astra_core::ErrorKind::ProviderDeadline,
                "device_deadline",
            ));
        }
        let deadline = std::time::SystemTime::now()
            .checked_add(remaining)
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|time| u64::try_from(time.as_millis()).ok())
            .ok_or_else(|| {
                responses::error(
                    astra_core::ErrorKind::ContractViolation,
                    "device_deadline_invalid",
                )
            })?;
        let mut context = astra_services::ModelRequestContextSeed::server_default();
        context.topology = astra_services::ModelRequestTopology::EdgeServer;
        context.execution_binding = "edge".into();
        let plan = custody::plan_runner_provider_attempt_dispatch(
            custody::RunnerProviderAttemptDispatchInput {
                invocation: &invocation,
                attempt_index: index,
                wire,
                request_context: context,
                binding,
                request: &prepared.request.body(),
                deadline_unix_ms: deadline,
            },
        )
        .map_err(service_error)?;
        let reservation = self
            .edges
            .runner_continuation_waiters
            .reserve(&self.pool, &self.user_id)
            .map_err(service_error)?;
        let mut cancellation = DeviceCancellation {
            pool: self.pool.clone(),
            edges: self.edges.clone(),
            identity: plan.grant().attempt.clone(),
            armed: true,
        };
        let admission = async {
            // A cancelled/ambiguous commit may already be discoverable by the
            // socket owner. Record possible delivery before polling it.
            phase.store(1, Ordering::Release);
            observer.note_dispatch_started(index);
            let admitted = custody::admit_runner_provider_attempt_dispatch(&self.pool, &plan).await;
            if admitted.as_ref().is_err_and(|error| {
                matches!(
                    error.kind,
                    astra_services::ServiceErrorKind::Invalid
                        | astra_services::ServiceErrorKind::NotFound
                        | astra_services::ServiceErrorKind::Conflict
                        | astra_services::ServiceErrorKind::ConflictTransient
                        | astra_services::ServiceErrorKind::Verification
                )
            }) {
                phase.store(0, Ordering::Release);
                cancellation.armed = false;
            }
            admitted
        };
        let grant = tokio::select! {
            biased;
            _=wait_llm_cancel(cancel)=>return Err(responses::error(astra_core::ErrorKind::Cancelled,"device_cancelled")),
            result=tokio::time::timeout(remaining.min(Duration::from_secs(25)),admission)=>result
                .map_err(|_|responses::error(astra_core::ErrorKind::ProviderDeadline,"device_admission_timeout"))?
                .map_err(service_error)?,
        };
        observer.note_remote_request_id(index, grant.attempt.attempt_id.as_str());
        let mut readiness = reservation
            .subscribe(&grant.attempt)
            .map_err(service_error)?;
        let mut preview = (callback.is_some()
            && prepared.route.inference_protocol == Some(LlmProviderProtocol::OpenAiResponses))
        .then(DevicePreview::default);
        // Subscribe before waking the socket owner. Missing/cross-pod preview
        // does not alter durable custody or authorize a provider retry.
        let mut progress = preview.as_ref().and_then(|_| {
            self.edges
                .subscribe_runner_inference_progress(&grant.attempt)
        });
        self.edges
            .notify_runner_inference(&self.user_id, grant.attempt.binding.runner_id.as_str());
        // A grant can cross the socket before its acknowledgement. Never retry
        // the provider request on a timeout, disconnect or lost Server future.
        let wait = async {
            loop {
                match *readiness.borrow_and_update() {
                    RunnerReadiness::Ready => break,
                    RunnerReadiness::Unavailable => {
                        return Err(responses::error(
                            astra_core::ErrorKind::StreamTransport,
                            "device_response_unavailable",
                        ));
                    }
                    RunnerReadiness::Waiting => {}
                }
                tokio::select! {
                    biased;
                    changed = readiness.changed() => changed.map_err(|_| {
                        responses::error(astra_core::ErrorKind::StreamTransport, "device_response_unavailable")
                    })?,
                    batch = receive_preview(&mut progress) => {
                        match batch {
                            Ok(batch) if batch.attempt == grant.attempt
                                && batch.validate_wire_bounds().is_ok() => {
                                if let (Some(preview), Some(callback)) = (preview.as_mut(), callback.as_deref_mut()) {
                                    preview.observe(&batch.events, callback);
                                }
                            }
                            _ => {
                                // A gap/closed ring stops provisional display;
                                // the terminal replays the undisplayed suffix.
                                progress = None;
                                if let Some(preview) = preview.as_mut() { preview.stopped = true; }
                            }
                        }
                    }
                }
            }
            custody::load_run_owned_runner_response(&self.pool, &input, &grant.attempt)
                .await
                .map_err(service_error)
        };
        let evidence = tokio::select! {
            biased;
            _=wait_llm_cancel(cancel)=>return Err(responses::error(astra_core::ErrorKind::Cancelled,"device_cancelled")),
            result=tokio::time::timeout(remaining.saturating_sub(started.elapsed()),wait)=>result
                .map_err(|_|responses::error(astra_core::ErrorKind::ProviderDeadline,"device_deadline"))??,
        };
        cancellation.armed = false;
        phase.store(2, Ordering::Release);
        let response: astra_turn_types::runner_inference::RunnerInferenceResponse =
            serde_json::from_slice(evidence.response.as_bytes()).map_err(|_| {
                responses::error(
                    astra_core::ErrorKind::ContractViolation,
                    "device_response_invalid",
                )
            })?;
        let mut result = collect_custody_response(
            response,
            prepared.route,
            started,
            &prepared.authorized_tool_names,
            prepared.wire_output_limit,
            callback,
            preview,
        )
        .await;
        if let Ok(result) = &mut result {
            result.judgment_provenance = prepared.judgment_provenance;
        }
        result
    }
}

async fn receive_preview(
    receiver: &mut Option<
        tokio::sync::broadcast::Receiver<
            astra_turn_types::runner_inference::RunnerInferenceProgressBatch,
        >,
    >,
) -> Result<
    astra_turn_types::runner_inference::RunnerInferenceProgressBatch,
    tokio::sync::broadcast::error::RecvError,
> {
    match receiver {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}

/// Disposable presentation only. The canonical collector still validates the
/// custodied response, tools, usage, and completion. A contiguous preview prefix
/// is hashed so final replay cannot silently substitute different provider facts.
#[derive(Default)]
struct DevicePreview {
    sequence: usize,
    bytes: usize,
    digest: sha2::Sha256,
    accumulator: astra_core::model_wire::responses::ResponseAccumulator,
    updates: std::collections::VecDeque<LlmStreamUpdate>,
    stopped: bool,
}

impl DevicePreview {
    fn observe(
        &mut self,
        events: &[astra_turn_types::runner_inference::RunnerInferenceProgressEvent],
        callback: &mut LlmStreamCallback<'_>,
    ) {
        use astra_turn_types::runner_inference::{
            RUNNER_INFERENCE_ARTIFACT_BYTES, RunnerInferenceProviderEvent,
        };
        use sha2::Digest;
        if self.stopped {
            return;
        }
        for item in events {
            if item.sequence < self.sequence as u64 {
                continue;
            }
            if item.sequence != self.sequence as u64 {
                self.stopped = true;
                return;
            }
            let Ok(encoded) = serde_json::to_vec(&item.event) else {
                self.stopped = true;
                return;
            };
            self.bytes = self.bytes.saturating_add(encoded.len());
            if self.bytes > RUNNER_INFERENCE_ARTIFACT_BYTES || self.sequence >= 65_536 {
                self.stopped = true;
                return;
            }
            self.digest.update((encoded.len() as u64).to_le_bytes());
            self.digest.update(encoded);
            self.sequence += 1;
            let RunnerInferenceProviderEvent::Json(value) = &item.event else {
                self.stopped = true;
                return;
            };
            match self.accumulator.observe(value) {
                Ok(Some((delta, reasoning))) => {
                    let update = if reasoning {
                        LlmStreamUpdate::Reasoning(delta)
                    } else {
                        LlmStreamUpdate::Text(delta)
                    };
                    callback(update.clone());
                    self.updates.push_back(update);
                }
                Ok(None) => {}
                Err(_) => {
                    self.stopped = true;
                    return;
                }
            }
        }
    }

    fn matches_custody(
        &self,
        events: &[astra_turn_types::runner_inference::RunnerInferenceProviderEvent],
    ) -> bool {
        use sha2::Digest;
        if events.len() < self.sequence {
            return false;
        }
        let mut digest = sha2::Sha256::new();
        for event in &events[..self.sequence] {
            let Ok(encoded) = serde_json::to_vec(event) else {
                return false;
            };
            digest.update((encoded.len() as u64).to_le_bytes());
            digest.update(encoded);
        }
        digest.finalize() == self.digest.clone().finalize()
    }
}

#[allow(clippy::too_many_arguments)]
async fn collect_custody_response(
    response: astra_turn_types::runner_inference::RunnerInferenceResponse,
    route: LlmExecutionRoute<'_>,
    started: Instant,
    authorized_tool_names: &HashSet<String>,
    wire_output_limit: Option<usize>,
    callback: Option<&mut LlmStreamCallback<'_>>,
    preview: Option<DevicePreview>,
) -> Result<LlmCallResult, astra_core::ClassifiedError> {
    let Some(preview) = preview else {
        return collect_response(
            response,
            route,
            started,
            authorized_tool_names,
            wire_output_limit,
            callback,
        )
        .await;
    };
    if !preview.matches_custody(&response.events) {
        return Err(responses::error(
            astra_core::ErrorKind::ContractViolation,
            "device_preview_custody_mismatch",
        ));
    }
    let mut pending = preview.updates;
    let mut mismatch = false;
    let mut callback = callback;
    let mut reconcile = |update| {
        if let Some(expected) = pending.pop_front() {
            mismatch |= expected != update;
        } else if !mismatch && let Some(callback) = callback.as_mut() {
            callback(update);
        }
    };
    let result = collect_response(
        response,
        route,
        started,
        authorized_tool_names,
        wire_output_limit,
        Some(&mut reconcile),
    )
    .await;
    if result.is_ok() && (mismatch || !pending.is_empty()) {
        return Err(responses::error(
            astra_core::ErrorKind::ContractViolation,
            "device_preview_decode_mismatch",
        ));
    }
    result
}

fn admission_error(
    (status, axum::Json(body)): (
        axum::http::StatusCode,
        axum::Json<astra_core::ErrorResponse>,
    ),
) -> astra_core::ClassifiedError {
    let kind = if status.is_server_error() {
        astra_core::ErrorKind::ServerError
    } else if status == axum::http::StatusCode::FORBIDDEN {
        astra_core::ErrorKind::PolicyDenied
    } else if status == axum::http::StatusCode::UNAUTHORIZED {
        astra_core::ErrorKind::Auth
    } else {
        astra_core::ErrorKind::InvalidRequest
    };
    // The model-service owner already provides safe, actionable diagnostics.
    // Preserve them instead of substituting a generic provider error message.
    astra_core::ClassifiedError::new(kind, body.detail).with_details_json(
        json!({"source":"device_inference", "code":body.error_code, "retry_safety":"none"})
            .to_string(),
    )
}

fn service_error(error: astra_services::ServiceError) -> astra_core::ClassifiedError {
    let kind = match error.kind {
        astra_services::ServiceErrorKind::NotFound => astra_core::ErrorKind::MissingModelSelection,
        astra_services::ServiceErrorKind::Invalid
        | astra_services::ServiceErrorKind::Conflict
        | astra_services::ServiceErrorKind::Verification => astra_core::ErrorKind::InvalidRequest,
        _ => astra_core::ErrorKind::ServerError,
    };
    astra_core::ClassifiedError::new(kind,"Device model execution could not be admitted; inspect /model and the device connection")
        .with_details_json(json!({"source":"device_inference","code":"device_inference_control_unavailable","reason":error.kind.as_str(),"retry_safety":"none"}).to_string())
}

struct DeviceCancellation {
    pool: astra_core::SharedPool,
    edges: astra_server_types::edge_connection_pool::EdgeConnectionPool,
    identity: astra_turn_types::runner_inference::RunnerInferenceAttemptIdentity,
    armed: bool,
}
impl Drop for DeviceCancellation {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let (pool, edges, identity) =
            (self.pool.clone(), self.edges.clone(), self.identity.clone());
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                // Admission may still be committing when the caller drops.
                // Retry only this idempotent cancellation intent, never the
                // provider operation, within a bounded settlement window.
                let _=tokio::time::timeout(Duration::from_secs(10),async {
                    for _ in 0..20 {
                        let result=astra_services::inference_execution::runner::request_runner_cancellation(&pool,&identity.user_id,&identity).await;
                        edges.notify_runner_inference(&identity.user_id,identity.binding.runner_id.as_str());
                        match result {
                            Ok(())=>break,
                            Err(error) if matches!(error.kind,astra_services::ServiceErrorKind::NotFound|astra_services::ServiceErrorKind::Persistence|astra_services::ServiceErrorKind::Network)=>tokio::time::sleep(Duration::from_millis(250)).await,
                            Err(_)=>break,
                        }
                    }
                }).await;
                // Failure does not prove no execution. The finite grant and
                // canonical Run fence remain authoritative after this task.
            });
        }
    }
}

pub(super) async fn collect_response(
    response: astra_turn_types::runner_inference::RunnerInferenceResponse,
    route: LlmExecutionRoute<'_>,
    _started: Instant,
    authorized_tool_names: &HashSet<String>,
    wire_output_limit: Option<usize>,
    stream_callback: Option<&mut LlmStreamCallback<'_>>,
) -> Result<LlmCallResult, astra_core::ClassifiedError> {
    use astra_turn_types::runner_inference::{
        RunnerInferenceProviderEvent, RunnerInferenceTransportStatus,
    };

    let model_name = route.model_name;
    let mut saw_eof = false;
    let mut chunks = Vec::with_capacity(response.events.len());
    for event in &response.events {
        if saw_eof {
            return Err(astra_core::ClassifiedError::new(
                astra_core::ErrorKind::ContractViolation,
                "Runner response contains provider events after EOF",
            ));
        }
        match event {
            RunnerInferenceProviderEvent::Json(value) => {
                let mut chunk = Vec::from("data: ".as_bytes());
                serde_json::to_writer(&mut chunk, value).map_err(|_| {
                    astra_core::ClassifiedError::new(
                        astra_core::ErrorKind::ContractViolation,
                        "Runner response event serialization failed",
                    )
                })?;
                chunk.extend_from_slice(b"\n\n");
                chunks.push(Ok(Bytes::from(chunk)));
            }
            RunnerInferenceProviderEvent::Done => {
                chunks.push(Ok(Bytes::from_static(b"data: [DONE]\n\n")));
            }
            RunnerInferenceProviderEvent::Eof => saw_eof = true,
        }
    }
    if response.transport.status == RunnerInferenceTransportStatus::Complete && !saw_eof {
        return Err(astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ContractViolation,
            "successful Runner response is missing its provider EOF evidence",
        ));
    }
    let collected = if chunks.is_empty()
        && response.transport.status != RunnerInferenceTransportStatus::Complete
    {
        // HTTP rejection and local no-start outcomes legitimately contain no
        // provider stream. Preserve their typed terminal instead of asking the
        // success-stream decoder to manufacture a protocol diagnosis.
        LlmCallResult::default()
    } else {
        // These bytes are already durably custodied: provider execution and
        // its cancellation/deadline have ended. Decode them under a fresh,
        // bounded local budget so an expired invocation clock or late user
        // cancellation cannot erase provider facts that already exist.
        let decode_started = Instant::now();
        let decoded = if route.inference_protocol == Some(LlmProviderProtocol::OpenAiResponses) {
            responses::collect_stream(
                futures_util::stream::iter(chunks),
                route,
                authorized_tool_names,
                LlmCancel::None,
                stream_callback,
                decode_started,
                Some(std::time::Duration::from_secs(30)),
                None,
                None,
            )
            .await
        } else {
            collect_llm_stream_for_wire(
                futures_util::stream::iter(chunks),
                model_name,
                decode_started,
                Some(std::time::Duration::from_secs(30)),
                LlmCancel::None,
                std::time::Duration::from_secs(30),
                std::time::Duration::from_secs(30),
                authorized_tool_names,
                stream_callback,
                None,
            )
            .await
        };
        match decoded {
            Ok(collected) => collected,
            Err(error) if response.transport.status != RunnerInferenceTransportStatus::Complete => {
                // The Runner's durable transport terminal owns failure
                // classification. The OpenAI-compatible decoder may reject a
                // truncated event sequence, but its partial result remains
                // useful evidence and must survive into the classified error.
                partial(error)
            }
            Err(error) => {
                return Err(match error {
                    StreamCollectError::Provider { error, partial, .. } => {
                        attach_llm_result_details(error, &partial)
                    }
                    StreamCollectError::Cancelled { partial } => attach_llm_result_details(
                        astra_core::ClassifiedError::new(
                            astra_core::ErrorKind::Cancelled,
                            "Runner response collection cancelled",
                        ),
                        &partial,
                    ),
                    StreamCollectError::ProviderWorkDeadline { partial, .. }
                    | StreamCollectError::IdleTimeout { partial, .. }
                    | StreamCollectError::SemanticProgressTimeout { partial, .. } => {
                        attach_llm_result_details(
                            astra_core::ClassifiedError::new(
                                astra_core::ErrorKind::ProviderDeadline,
                                "Runner response collection exceeded its bounded deadline",
                            ),
                            &partial,
                        )
                    }
                    StreamCollectError::Transport { partial, .. } => attach_llm_result_details(
                        astra_core::ClassifiedError::new(
                            astra_core::ErrorKind::ContractViolation,
                            "Runner response contains invalid OpenAI-compatible events",
                        ),
                        &partial,
                    ),
                });
            }
        }
    };

    let mut collected = collected;

    if response.transport.status != RunnerInferenceTransportStatus::Complete {
        let kind = match response.transport.status {
            RunnerInferenceTransportStatus::Cancelled => astra_core::ErrorKind::Cancelled,
            RunnerInferenceTransportStatus::Deadline => astra_core::ErrorKind::ProviderDeadline,
            RunnerInferenceTransportStatus::HttpStatus(401 | 403) => astra_core::ErrorKind::Auth,
            RunnerInferenceTransportStatus::HttpStatus(429) => astra_core::ErrorKind::RateLimit,
            RunnerInferenceTransportStatus::HttpStatus(code) if (300..500).contains(&code) => {
                astra_core::ErrorKind::InvalidRequest
            }
            RunnerInferenceTransportStatus::CredentialUnavailable => astra_core::ErrorKind::Auth,
            RunnerInferenceTransportStatus::BindingUnavailable => {
                astra_core::ErrorKind::MissingModelSelection
            }
            RunnerInferenceTransportStatus::CapacityUnavailable => {
                astra_core::ErrorKind::ResourceLimit
            }
            RunnerInferenceTransportStatus::HttpStatus(code) if code >= 500 => {
                astra_core::ErrorKind::ServerError
            }
            RunnerInferenceTransportStatus::Protocol | RunnerInferenceTransportStatus::Limit => {
                astra_core::ErrorKind::ContractViolation
            }
            _ => astra_core::ErrorKind::StreamTransport,
        };
        return Err(attach_llm_result_details(
            astra_core::ClassifiedError::new(
                kind,
                format!(
                    "Runner provider attempt ended with {:?}",
                    response.transport.status
                ),
            ),
            &collected,
        ));
    }
    reconcile_missing_output_cap_finish_reason(&mut collected, wire_output_limit);
    Ok(collected)
}

fn partial(error: StreamCollectError) -> LlmCallResult {
    match error {
        StreamCollectError::Provider { partial, .. }
        | StreamCollectError::IdleTimeout { partial, .. }
        | StreamCollectError::SemanticProgressTimeout { partial, .. }
        | StreamCollectError::ProviderWorkDeadline { partial, .. }
        | StreamCollectError::Transport { partial, .. }
        | StreamCollectError::Cancelled { partial } => partial,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn admission_failure_preserves_owner_message_code_and_failure_class() {
        for (status, expected) in [
            (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                astra_core::ErrorKind::ServerError,
            ),
            (
                axum::http::StatusCode::FORBIDDEN,
                astra_core::ErrorKind::PolicyDenied,
            ),
            (
                axum::http::StatusCode::UNAUTHORIZED,
                astra_core::ErrorKind::Auth,
            ),
            (
                axum::http::StatusCode::CONFLICT,
                astra_core::ErrorKind::InvalidRequest,
            ),
        ] {
            let error = admission_error((
                status,
                axum::Json(
                    astra_core::ErrorResponse::new("Select this workspace model again from /model")
                        .with_error_code("device_model_source_denied"),
                ),
            ));
            assert_eq!(error.kind, expected);
            assert!(error.message.contains("/model"));
            assert!(
                error
                    .details_json
                    .as_deref()
                    .unwrap()
                    .contains("device_model_source_denied")
            );
        }
    }

    use astra_turn_types::runner_inference::*;
    fn route() -> LlmExecutionRoute<'static> {
        LlmExecutionRoute {
            device_binding: None,
            inference_protocol: Some(LlmProviderProtocol::OpenAiResponses),
            response_scope: Some("00000000-0000-4000-8000-000000000001"),
            subscription: None,
            fixed_temperature: None,
            supported_reasoning_efforts: None,
            thinking_protocol: None,
            model_name: "fixture-model",
            wire_model_name: None,
            api_key: "",
            base_url: "https://api.openai.com/v1",
            provider: "chatgpt",
            header_overrides: None,
            request_body_overrides: None,
            completions_url_override: None,
            request_timeout: None,
        }
    }
    fn response(
        events: Vec<RunnerInferenceProviderEvent>,
        status: RunnerInferenceTransportStatus,
    ) -> RunnerInferenceResponse {
        RunnerInferenceResponse {
            events,
            transport: RunnerInferenceTransportTerminal {
                status,
                delivery: RunnerInferenceDeliveryEvidence::ResponseHeaders,
                provider_bytes: 32,
                events_delivered: 1,
            },
        }
    }
    fn terminal(output: Value) -> RunnerInferenceProviderEvent {
        RunnerInferenceProviderEvent::Json(
            json!({"type":"response.completed","response":{"id":"response-fixture","status":"completed","output":output,"usage":{"input_tokens":12,"output_tokens":3,"input_tokens_details":{"cached_tokens":2}}}}),
        )
    }
    fn text_delta(text: &str) -> RunnerInferenceProviderEvent {
        RunnerInferenceProviderEvent::Json(json!({
            "type":"response.output_text.delta", "output_index":0,
            "content_index":0, "delta":text,
        }))
    }

    #[tokio::test]
    async fn preview_is_visible_before_custody_and_final_replay_does_not_duplicate_it() {
        let mut preview = DevicePreview::default();
        let first = text_delta("hel");
        let mut updates = Vec::new();
        preview.observe(
            &[RunnerInferenceProgressEvent {
                sequence: 0,
                event: first.clone(),
            }],
            &mut |update| updates.push(update),
        );
        // No terminal exists yet: this proves presentation is incremental.
        assert_eq!(updates, vec![LlmStreamUpdate::Text("hel".into())]);
        let input = response(
            vec![
                first,
                text_delta("lo"),
                terminal(
                    json!([{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]}]),
                ),
                RunnerInferenceProviderEvent::Eof,
            ],
            RunnerInferenceTransportStatus::Complete,
        );
        let output = collect_custody_response(
            input,
            route(),
            Instant::now(),
            &HashSet::new(),
            None,
            Some(&mut |update| updates.push(update)),
            Some(preview),
        )
        .await
        .unwrap();
        assert_eq!(output.full_text, "hello");
        assert_eq!(
            updates,
            vec![
                LlmStreamUpdate::Text("hel".into()),
                LlmStreamUpdate::Text("lo".into())
            ]
        );
    }

    #[tokio::test]
    async fn preview_gap_stops_live_display_and_custody_supplies_the_missing_suffix() {
        let mut preview = DevicePreview::default();
        let first = text_delta("a");
        let mut updates = Vec::new();
        preview.observe(
            &[
                RunnerInferenceProgressEvent {
                    sequence: 0,
                    event: first.clone(),
                },
                RunnerInferenceProgressEvent {
                    sequence: 0,
                    event: first.clone(),
                },
                RunnerInferenceProgressEvent {
                    sequence: 2,
                    event: text_delta("c"),
                },
            ],
            &mut |update| updates.push(update),
        );
        assert!(preview.stopped);
        assert_eq!(updates, vec![LlmStreamUpdate::Text("a".into())]);
        let input = response(
            vec![
                first,
                text_delta("b"),
                text_delta("c"),
                terminal(
                    json!([{"type":"message","role":"assistant","content":[{"type":"output_text","text":"abc"}]}]),
                ),
                RunnerInferenceProviderEvent::Eof,
            ],
            RunnerInferenceTransportStatus::Complete,
        );
        let output = collect_custody_response(
            input,
            route(),
            Instant::now(),
            &HashSet::new(),
            None,
            Some(&mut |update| updates.push(update)),
            Some(preview),
        )
        .await
        .unwrap();
        assert_eq!(output.full_text, "abc");
        assert_eq!(
            updates,
            ["a", "b", "c"].map(|text| LlmStreamUpdate::Text(text.into()))
        );
    }

    #[tokio::test]
    async fn preview_cannot_substitute_durable_facts_or_authorize_tools() {
        let mut preview = DevicePreview::default();
        preview.observe(
            &[RunnerInferenceProgressEvent {
                sequence: 0,
                event: text_delta("provisional"),
            }],
            &mut |_| {},
        );
        let input = response(
            vec![text_delta("different"), RunnerInferenceProviderEvent::Eof],
            RunnerInferenceTransportStatus::Complete,
        );
        let error = collect_custody_response(
            input,
            route(),
            Instant::now(),
            &HashSet::new(),
            None,
            None,
            Some(preview),
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind, astra_core::ErrorKind::ContractViolation);

        let mut preview = DevicePreview::default();
        let tool = terminal(
            json!([{"type":"function_call","id":"item","call_id":"call","name":"not_admitted","arguments":"{}"}]),
        );
        preview.observe(
            &[RunnerInferenceProgressEvent {
                sequence: 0,
                event: tool.clone(),
            }],
            &mut |_| panic!("tool preview is not an executable callback"),
        );
        let input = response(
            vec![tool, RunnerInferenceProviderEvent::Eof],
            RunnerInferenceTransportStatus::Complete,
        );
        let error = collect_custody_response(
            input,
            route(),
            Instant::now(),
            &HashSet::new(),
            None,
            None,
            Some(preview),
        )
        .await
        .unwrap_err();
        assert!(error.details_json.unwrap().contains("tool_not_admitted"));
    }

    #[tokio::test]
    async fn custody_uses_responses_codec_and_stable_connection_scope() {
        let input = response(
            vec![
                terminal(
                    json!([{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]}]),
                ),
                RunnerInferenceProviderEvent::Eof,
            ],
            RunnerInferenceTransportStatus::Complete,
        );
        let output = collect_response(
            input,
            route(),
            Instant::now(),
            &HashSet::new(),
            Some(100),
            None,
        )
        .await
        .unwrap();
        assert_eq!(output.full_text, "hello");
        assert_eq!(output.response_id.as_deref(), Some("response-fixture"));
        assert_eq!(
            output.provider_continuation.unwrap()["connection_id"],
            "00000000-0000-4000-8000-000000000001"
        );
        let usage = crate::turn::token_usage::TokenUsage::from_partial_json_map(&output.usage);
        assert_eq!(usage.cached_input_tokens, 2);
        // The provider omitted cache creation. Preserve the measured inclusive
        // input total without inventing a zero creation bucket or fresh count.
        assert!(!output.usage.contains_key("input_tokens"));
        assert!(!output.usage_presence.fresh_input_tokens);
        assert_eq!(output.usage_presence.measured_input_tokens, Some(12));
        assert_eq!(usage.output_tokens, 3);
    }
    #[tokio::test]
    async fn framing_completion_never_authorizes_an_unadvertised_tool() {
        let input = response(
            vec![
                terminal(
                    json!([{"type":"function_call","id":"item","call_id":"call","name":"not_admitted","arguments":"{}"}]),
                ),
                RunnerInferenceProviderEvent::Eof,
            ],
            RunnerInferenceTransportStatus::Complete,
        );
        let failure = collect_response(
            input,
            route(),
            Instant::now(),
            &HashSet::new(),
            Some(100),
            None,
        )
        .await
        .unwrap_err();
        assert!(failure.details_json.unwrap().contains("tool_not_admitted"));
    }
    #[tokio::test]
    async fn provider_eof_without_response_completed_is_not_success() {
        let input = response(
            vec![RunnerInferenceProviderEvent::Eof],
            RunnerInferenceTransportStatus::Complete,
        );
        assert!(
            collect_response(input, route(), Instant::now(), &HashSet::new(), None, None)
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn completed_response_after_an_eof_is_a_contract_error() {
        let input = response(
            vec![RunnerInferenceProviderEvent::Eof, terminal(json!([]))],
            RunnerInferenceTransportStatus::Complete,
        );
        let failure = collect_response(input, route(), Instant::now(), &HashSet::new(), None, None)
            .await
            .unwrap_err();
        assert_eq!(failure.kind, astra_core::ErrorKind::ContractViolation);
    }
    #[tokio::test]
    async fn device_route_cannot_fall_through_to_server_http() {
        use std::num::{NonZeroU32, NonZeroU64};
        let id = |value: &str| RunnerInferenceId::new(value.to_owned()).unwrap();
        let binding = astra_services::runner_model_bindings::ResolvedRunnerModelBinding {
            user_id: "owner".into(),
            publication_revision: 1,
            process_boot_nonce: id("boot"),
            definition: RunnerInferenceBindingDefinition {
                identity: RunnerInferenceBindingIdentity {
                    runner_id: id("runner"),
                    journal_id: id("journal"),
                    binding_id: id("binding"),
                    binding_revision: NonZeroU64::new(1).unwrap(),
                    profile_revision: NonZeroU64::new(1).unwrap(),
                },
                display_name: RunnerInferenceModelName::new("fixture-model").unwrap(),
                model_name: RunnerInferenceModelName::new("fixture-model").unwrap(),
                protocol: RunnerInferenceProtocol::OpenAiResponses,
                source: RunnerInferenceSource::Chatgpt {
                    connection_ref: id("00000000-0000-4000-8000-000000000001"),
                    authorization_revision: NonZeroU64::new(1).unwrap(),
                    native_context: astra_turn_types::runner_inference::RunnerNativeContext {
                        session_id: RunnerInferenceId::new("platform-session").unwrap(),
                        workspace_id: RunnerInferenceId::new("workspace").unwrap(),
                        role_candidate: None,
                    },
                },
                context_window: NonZeroU32::new(4096).unwrap(),
                max_output_tokens: NonZeroU32::new(128).unwrap(),
                supported_reasoning_efforts: Vec::new(),
            },
        };
        let mut route = route();
        route.device_binding = Some(&binding);
        let messages = [json!({"role":"user","content":"hello"})];
        let prepared = prepare_streaming_call(
            LlmCall {
                purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
                messages: &messages,
                tools: &[],
                cache_capability: None,
                route,
                max_output_tokens: Some(128),
                temperature: None,
                thinking: &ThinkingConfig::ModelDefault,
            },
            RuntimeToolChoice::None,
        )
        .unwrap();
        let failure = call_prepared_llm_attempts(
            prepared,
            LlmCancel::None,
            None,
            None,
            Some(Duration::from_secs(1)),
        )
        .await
        .unwrap_err();
        assert!(
            failure
                .details_json
                .unwrap()
                .contains("device_inference_owner_missing")
        );
    }
}
