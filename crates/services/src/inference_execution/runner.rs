//! Runner delivery extends the canonical inference ledger, not a second job
//! state machine. A grant is immutable before transport; terminal ACK means
//! payload custody plus a recoverable continuation obligation, not just usage.

use astra_turn_types::runner_inference::{RunnerInferenceBindingDefinition, RunnerInferenceSource};
use std::num::NonZeroU64;

use astra_turn_types::runner_inference::{
    RunnerInferenceArtifactReference, RunnerInferenceAttemptIdentity, RunnerInferenceDigest,
    RunnerInferenceDispatchGrant, RunnerInferenceId, RunnerInferenceStartEvidence,
    RunnerInferenceTerminalAck, RunnerInferenceTerminalRetirement,
};

use super::*;
use crate::runner_model_bindings::{
    AuthenticatedRunnerConnection, ResolvedRunnerModelBinding, lock_connection,
    lock_resolved_binding,
};
use crate::session_artifact_store::{
    SessionArtifactJsonRecord, SessionArtifactReference, SessionArtifactReferenceKind,
    persist_referenced_json_artifact_tx,
};

const MAX_CUSTODY_BYTES: usize = 16 * 1024 * 1024;
const MAX_START_WINDOW_MS: u64 = 15_000;
const MAX_RUNNER_PENDING_ATTEMPTS: i64 = 128;
const MAX_RUNNER_ACTIVE_ATTEMPTS: i64 =
    astra_turn_types::runner_inference::RUNNER_INFERENCE_MAX_ACTIVE_ATTEMPTS as i64;
const MAX_RUNNER_SESSION_ACTIVE_ATTEMPTS: i64 = 1;

/// Called with the registry row locked by binding admission. The existing
/// durable attempts are the waiting queue; no socket-owned lifecycle is added.
async fn check_runner_queue_capacity(
    tx: &mut impl crate::cancellation_safe_db::TransactionConnection,
    identity: &RunnerInferenceAttemptIdentity,
) -> ServiceResult<()> {
    let pending: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM inference_provider_attempts
         WHERE user_id = ? AND runner_id = ? AND status = 'started'
           AND runner_terminal_hash IS NULL AND runner_capacity_retired_at IS NULL",
    )
    .bind(&identity.user_id)
    .bind(identity.binding.runner_id.as_str())
    .fetch_one(&mut **tx)
    .await
    .map_err(persistence)?;
    if pending >= MAX_RUNNER_PENDING_ATTEMPTS {
        return Err(ServiceError::conflict(
            "Runner pending request capacity exhausted",
        ));
    }
    Ok(())
}

pub fn validate_runner_invocation(
    input: &InferenceInvocationInput,
    binding: &ResolvedRunnerModelBinding,
) -> ServiceResult<()> {
    if input.user_id != binding.user_id
        || input.scope.run_id().is_none()
        || input.execution_placement != ModelExecutionPlacement::Edge
        || input.access_kind != ModelAccessKind::ThisDevice
        || input.upstream_model_name != binding.definition.model_name.as_str()
        || input.resolved_model_name != binding.definition.model_name.as_str()
        || input.provider != binding.definition.provider()
        || input.offering_id
            != crate::runner_model_bindings::runner_offering_id(
                &input.user_id,
                &binding.definition.identity,
            )
    {
        return Err(ServiceError::invalid(
            "Runner admission must match the personal session binding",
        ));
    }
    require_runner_purpose(input.purpose)
}

/// Optional background inference needs an explicit publication-policy opt-in,
/// which this binding contract does not yet support. Keep one policy owner.
pub fn require_runner_purpose(purpose: InferencePurpose) -> ServiceResult<()> {
    if !matches!(
        purpose,
        InferencePurpose::PrimaryAgent
            | InferencePurpose::SubAgent
            | InferencePurpose::RequiredCompaction
    ) {
        return Err(ServiceError::invalid(
            "This device model does not support this inference purpose; explicitly choose another model before retrying",
        ));
    }
    Ok(())
}

/// Authorize a terminal-transfer header before reserving any body capacity.
/// Reconnect generation is checked by registry ownership, not stored as attempt
/// authority; historical matching attempts remain eligible for custody replay.
pub async fn validate_runner_terminal_attempt(
    pool: &SharedPool,
    connection: &AuthenticatedRunnerConnection,
    identity: &RunnerInferenceAttemptIdentity,
) -> ServiceResult<()> {
    if identity.user_id != connection.user_id || identity.binding.runner_id != connection.runner_id
    {
        return Err(ServiceError::conflict("Runner terminal owner mismatch"));
    }
    let mut tx = crate::cancellation_safe_db::CancellationSafeTransaction::begin(pool.get())
        .await
        .map_err(persistence)?;
    lock_connection(&mut tx, connection).await?;
    let row = sqlx::query("SELECT runner_grant_json FROM inference_provider_attempts WHERE user_id = ? AND attempt_id = ?")
        .bind(&identity.user_id).bind(identity.attempt_id.as_str()).fetch_optional(&mut *tx).await.map_err(persistence)?
        .ok_or_else(|| ServiceError::not_found("Runner terminal attempt absent"))?;
    if decode_grant(&row)?.attempt != *identity {
        return Err(ServiceError::conflict("Runner terminal identity mismatch"));
    }
    Ok(())
}

/// Deletion authority belongs to the existing session lifecycle tombstone.
/// An absent attempt alone is never an acknowledgement or a retirement proof.
pub async fn retired_runner_terminal(
    pool: &SharedPool,
    connection: &AuthenticatedRunnerConnection,
    identity: &RunnerInferenceAttemptIdentity,
    terminal_sha256: &RunnerInferenceDigest,
) -> ServiceResult<Option<RunnerInferenceTerminalRetirement>> {
    if identity.user_id != connection.user_id || identity.binding.runner_id != connection.runner_id
    {
        return Err(ServiceError::conflict("Runner terminal owner mismatch"));
    }
    let session = identity
        .scope
        .session_id()
        .ok_or_else(|| ServiceError::invalid("Runner terminal requires session scope"))?;
    let mut tx = crate::cancellation_safe_db::CancellationSafeTransaction::begin(pool.get())
        .await
        .map_err(persistence)?;
    let registry = lock_connection(&mut tx, connection).await?;
    let journal: Option<String> = registry
        .try_get("inference_journal_id")
        .map_err(persistence)?;
    let edge: Option<String> = registry.try_get("inference_edge_id").map_err(persistence)?;
    if journal.as_deref() != Some(identity.binding.journal_id.as_str())
        || edge.as_deref() != Some(connection.edge_id.as_str())
    {
        return Err(ServiceError::conflict("Runner terminal journal mismatch"));
    }
    let deleted = sqlx::query(
        "SELECT 1 FROM session_deletion_tombstones WHERE user_id = ? AND session_id = ?",
    )
    .bind(&identity.user_id)
    .bind(session)
    .fetch_optional(&mut *tx)
    .await
    .map_err(persistence)?
    .is_some();
    Ok(deleted.then(|| RunnerInferenceTerminalRetirement {
        attempt: identity.clone(),
        terminal_sha256: terminal_sha256.clone(),
    }))
}

/// Persist bounded execution-right retirement, never a provider outcome. The
/// reporter owns the same exclusively locked journal under a different boot,
/// has reconciled the exact grant as missing, and has observed its full deadline
/// elapse. Recheck that deadline and the authenticated incarnation here. A short
/// delivery claim, cancellation, or an absent journal record alone is insufficient.
pub async fn retire_runner_capacity(
    pool: &SharedPool,
    connection: &AuthenticatedRunnerConnection,
    grant: &RunnerInferenceDispatchGrant,
    process_boot_nonce: &RunnerInferenceId,
) -> ServiceResult<()> {
    let identity = &grant.attempt;
    if identity.user_id != connection.user_id || identity.binding.runner_id != connection.runner_id
    {
        return Err(ServiceError::conflict("Runner retirement owner mismatch"));
    }
    let mut tx = crate::cancellation_safe_db::CancellationSafeTransaction::begin(pool.get())
        .await
        .map_err(persistence)?;
    let session = identity
        .scope
        .session_id()
        .ok_or_else(|| ServiceError::invalid("Runner retirement requires session scope"))?;
    crate::storage::admit_session_event_write(&mut tx, session, &identity.user_id, false)
        .await
        .map_err(persistence)?;
    let row = sqlx::query(
        "SELECT runner_grant_json,
        IF(runner_deadline_at <= NOW(6), 1, 0) AS expired
        FROM inference_provider_attempts WHERE user_id = ? AND attempt_id = ? FOR UPDATE",
    )
    .bind(&identity.user_id)
    .bind(identity.attempt_id.as_str())
    .fetch_optional(&mut *tx)
    .await
    .map_err(persistence)?
    .ok_or_else(|| ServiceError::not_found("Runner retirement attempt absent"))?;
    if decode_grant(&row)? != *grant || row.try_get::<i64, _>("expired").map_err(persistence)? != 1
    {
        return Err(ServiceError::conflict(
            "Runner retirement requires the exact expired grant",
        ));
    }
    let registry = lock_connection(&mut tx, connection).await?;
    let boot: Option<String> = registry
        .try_get("inference_boot_nonce")
        .map_err(persistence)?;
    let journal: Option<String> = registry
        .try_get("inference_journal_id")
        .map_err(persistence)?;
    let edge: Option<String> = registry.try_get("inference_edge_id").map_err(persistence)?;
    if boot.as_deref() != Some(process_boot_nonce.as_str())
        || process_boot_nonce == &grant.process_boot_nonce
        || journal.as_deref() != Some(identity.binding.journal_id.as_str())
        || edge.as_deref() != Some(connection.edge_id.as_str())
    {
        return Err(ServiceError::conflict(
            "Runner retirement requires the current replacement incarnation",
        ));
    }
    sqlx::query(
        "UPDATE inference_provider_attempts SET runner_capacity_retired_at = NOW(6),
        runner_capacity_retired_by_boot = ? WHERE user_id = ? AND attempt_id = ?
        AND runner_capacity_retired_at IS NULL",
    )
    .bind(process_boot_nonce.as_str())
    .bind(&identity.user_id)
    .bind(identity.attempt_id.as_str())
    .execute(&mut *tx)
    .await
    .map_err(persistence)?;
    tx.commit().await.map_err(persistence)
}

/// Private request/response bytes. Debug reports size only.
pub struct RunnerCustodyBytes(Vec<u8>);

impl RunnerCustodyBytes {
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

impl std::fmt::Debug for RunnerCustodyBytes {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RunnerCustodyBytes")
            .field("byte_len", &self.0.len())
            .finish()
    }
}

async fn load_exact_artifact_tx(
    tx: &mut impl crate::cancellation_safe_db::TransactionConnection,
    identity: &RunnerInferenceAttemptIdentity,
    reference: &RunnerInferenceArtifactReference,
) -> ServiceResult<RunnerCustodyBytes> {
    if reference.byte_len.get() > MAX_CUSTODY_BYTES as u64 {
        return Err(ServiceError::invalid(
            "Runner artifact exceeds custody bound",
        ));
    }
    let row = sqlx::query("SELECT content_json FROM session_artifacts AS artifact
        WHERE user_id = ? AND session_id = ? AND artifact_id = ? AND status = 'active'
          AND EXISTS (SELECT 1 FROM session_artifact_references AS artifact_ref
            WHERE artifact_ref.user_id = artifact.user_id AND artifact_ref.session_id = artifact.session_id
              AND artifact_ref.artifact_id = artifact.artifact_id AND artifact_ref.reference_kind = 'invocation_ledger'
              AND artifact_ref.reference_id = ?)")
        .bind(&identity.user_id).bind(identity.scope.session_id()).bind(reference.artifact_id.as_str())
        .bind(identity.attempt_id.as_str()).fetch_optional(&mut **tx).await.map_err(persistence)?
        .ok_or_else(|| ServiceError::not_found("owned Runner artifact custody unavailable"))?;
    let encoded: String = row.try_get("content_json").map_err(persistence)?;
    let content: String = serde_json::from_str(&encoded)
        .map_err(|_| ServiceError::invalid("invalid durable Runner artifact encoding"))?;
    if content.len() as u64 != reference.byte_len.get()
        || digest(content.as_bytes()) != reference.sha256
    {
        return Err(ServiceError::conflict("Runner artifact integrity mismatch"));
    }
    Ok(RunnerCustodyBytes(content.into_bytes()))
}

pub async fn load_runner_request_custody(
    pool: &SharedPool,
    connection: &AuthenticatedRunnerConnection,
    grant: &RunnerInferenceDispatchGrant,
) -> ServiceResult<RunnerCustodyBytes> {
    if connection.user_id != grant.attempt.user_id
        || connection.runner_id != grant.attempt.binding.runner_id
    {
        return Err(ServiceError::conflict(
            "Runner request custody owner mismatch",
        ));
    }
    let mut tx = crate::cancellation_safe_db::CancellationSafeTransaction::begin(pool.get())
        .await
        .map_err(persistence)?;
    lock_connection(&mut tx, connection).await?;
    let row = sqlx::query("SELECT runner_grant_json FROM inference_provider_attempts WHERE user_id = ? AND attempt_id = ?")
        .bind(&connection.user_id).bind(grant.attempt.attempt_id.as_str()).fetch_optional(&mut *tx).await.map_err(persistence)?
        .ok_or_else(|| ServiceError::not_found("Runner request attempt absent"))?;
    if decode_grant(&row)? != *grant {
        return Err(ServiceError::conflict("Runner request grant mismatch"));
    }
    load_exact_artifact_tx(&mut tx, &grant.attempt, &grant.attempt.request).await
}

/// One exact Runner response that follows a restored canonical checkpoint.
/// The bytes are loaded only after the ledger row, identity, and artifact
/// reference have been locked and verified.
#[derive(Debug)]
pub struct RunnerRecoveredContinuation {
    pub receipt: astra_turn_types::runner_inference::RunnerInferenceContinuationReceipt,
    pub physical_terminal: InferenceInvocationTerminal,
    pub request: RunnerCustodyBytes,
    pub response: RunnerCustodyBytes,
}

/// Read physical response custody under the existing Run authority. This does
/// not claim a second invocation lease or decide whether the Run may resume.
pub async fn load_run_owned_runner_response(
    pool: &SharedPool,
    input: &InferenceInvocationInput,
    identity: &RunnerInferenceAttemptIdentity,
) -> ServiceResult<RunnerRecoveredContinuation> {
    if identity.user_id != input.user_id
        || identity.scope != input.scope
        || input.scope.run_id().is_none()
    {
        return Err(ServiceError::invalid(
            "Runner response requires its exact Run scope",
        ));
    }
    let mut tx = crate::cancellation_safe_db::CancellationSafeTransaction::begin(pool.get())
        .await
        .map_err(persistence)?;
    if !matches!(
        lock_invocation_scope_authority(&mut tx, input).await?,
        InvocationScopeAuthority::Live
    ) {
        return Err(ServiceError::conflict(
            "Runner response Run authority changed",
        ));
    }
    let row = sqlx::query(
        "SELECT runner_grant_json, runner_terminal_hash, runner_response_artifact_id,
        runner_response_hash, runner_response_bytes, runner_terminal_conflict,
        status, terminal_fingerprint, usage_status, input_tokens, output_tokens,
        cache_read_tokens, cache_creation_tokens, provider_response_id, error_kind, error_message
        FROM inference_provider_attempts WHERE user_id = ? AND attempt_id = ? FOR UPDATE",
    )
    .bind(&input.user_id)
    .bind(identity.attempt_id.as_str())
    .fetch_optional(&mut *tx)
    .await
    .map_err(persistence)?
    .ok_or_else(|| ServiceError::not_found("Runner response attempt absent"))?;
    if decode_grant(&row)?.attempt != *identity
        || row
            .try_get::<bool, _>("runner_terminal_conflict")
            .map_err(persistence)?
    {
        return Err(ServiceError::conflict(
            "Runner response identity changed or quarantined",
        ));
    }
    let receipt = astra_turn_types::runner_inference::RunnerInferenceContinuationReceipt {
        attempt: identity.clone(),
        terminal_sha256: RunnerInferenceDigest::new(
            row.try_get::<String, _>("runner_terminal_hash")
                .map_err(persistence)?,
        )
        .map_err(ServiceError::invalid)?,
        response: RunnerInferenceArtifactReference {
            artifact_id: RunnerInferenceId::new(
                row.try_get::<String, _>("runner_response_artifact_id")
                    .map_err(persistence)?,
            )
            .map_err(ServiceError::invalid)?,
            sha256: RunnerInferenceDigest::new(
                row.try_get::<String, _>("runner_response_hash")
                    .map_err(persistence)?,
            )
            .map_err(ServiceError::invalid)?,
            byte_len: NonZeroU64::new(
                u64::try_from(
                    row.try_get::<i64, _>("runner_response_bytes")
                        .map_err(persistence)?,
                )
                .map_err(|_| ServiceError::invalid("invalid Runner response length"))?,
            )
            .ok_or_else(|| ServiceError::invalid("empty Runner response"))?,
        },
    };
    let physical_terminal =
        public_terminal(&DurableInferenceTerminal::decode(&row).map_err(persistence)?)?;
    let request = load_exact_artifact_tx(&mut tx, identity, &identity.request).await?;
    let response = load_exact_artifact_tx(&mut tx, identity, &receipt.response).await?;
    tx.commit().await.map_err(persistence)?;
    Ok(RunnerRecoveredContinuation {
        receipt,
        physical_terminal,
        request,
        response,
    })
}

/// Positive and negative evidence stay on the original attempt. Absence of a
/// row/message is never inferred as proof of no provider execution.
pub async fn record_runner_start_evidence(
    pool: &SharedPool,
    connection: &AuthenticatedRunnerConnection,
    grant: &RunnerInferenceDispatchGrant,
    evidence: RunnerInferenceStartEvidence,
) -> ServiceResult<()> {
    if connection.user_id != grant.attempt.user_id
        || connection.runner_id != grant.attempt.binding.runner_id
    {
        return Err(ServiceError::conflict(
            "Runner start evidence owner mismatch",
        ));
    }
    let mut tx = crate::cancellation_safe_db::CancellationSafeTransaction::begin(pool.get())
        .await
        .map_err(persistence)?;
    let row = sqlx::query(
        "SELECT runner_grant_json, runner_no_start_evidence, status,
        IF(runner_local_fence_at IS NOT NULL, 1, 0) AS fenced,
        IF(runner_grant_expires_at <= NOW(6), 1, 0) AS expired,
        IF(runner_cancel_requested_at IS NOT NULL, 1, 0) AS cancelled
        FROM inference_provider_attempts WHERE user_id = ? AND attempt_id = ? FOR UPDATE",
    )
    .bind(&connection.user_id)
    .bind(grant.attempt.attempt_id.as_str())
    .fetch_optional(&mut *tx)
    .await
    .map_err(persistence)?
    .ok_or_else(|| ServiceError::not_found("Runner start evidence attempt absent"))?;
    if decode_grant(&row)? != *grant {
        return Err(ServiceError::conflict(
            "Runner start evidence grant mismatch",
        ));
    }
    lock_connection(&mut tx, connection).await?;
    let fenced = row.try_get::<i64, _>("fenced").map_err(persistence)? == 1;
    let no_start: Option<String> = row
        .try_get("runner_no_start_evidence")
        .map_err(persistence)?;
    let column = match evidence {
        RunnerInferenceStartEvidence::FenceCommitted => {
            if no_start.is_some() {
                return Err(ServiceError::conflict(
                    "Runner fence conflicts with no-start evidence",
                ));
            }
            "runner_local_fence_at"
        }
        RunnerInferenceStartEvidence::ProviderStarted => {
            if !fenced || no_start.is_some() {
                return Err(ServiceError::conflict(
                    "Runner provider start requires committed local fence",
                ));
            }
            "runner_provider_started_at"
        }
        RunnerInferenceStartEvidence::ExpiredWithoutFence
        | RunnerInferenceStartEvidence::CancelledWithoutFence
        | RunnerInferenceStartEvidence::RejectedWithoutFence => {
            if no_start.is_none()
                && row.try_get::<String, _>("status").map_err(persistence)? != "started"
            {
                return Err(ServiceError::conflict(
                    "Runner no-start evidence cannot replace terminal evidence",
                ));
            }
            let reason = if evidence == RunnerInferenceStartEvidence::ExpiredWithoutFence {
                if row.try_get::<i64, _>("expired").map_err(persistence)? != 1 {
                    return Err(ServiceError::conflict("Runner grant has not expired"));
                }
                "expired_without_fence"
            } else if evidence == RunnerInferenceStartEvidence::CancelledWithoutFence {
                if row.try_get::<i64, _>("cancelled").map_err(persistence)? != 1 {
                    return Err(ServiceError::conflict("Runner cancellation intent absent"));
                }
                "cancelled_without_fence"
            } else {
                "rejected_without_fence"
            };
            if fenced
                || no_start
                    .as_deref()
                    .is_some_and(|previous| previous != reason)
            {
                return Err(ServiceError::conflict(
                    "Runner no-start evidence conflicts with persisted evidence",
                ));
            }
            sqlx::query("UPDATE inference_provider_attempts SET runner_no_start_evidence = ? WHERE user_id = ? AND attempt_id = ?")
                .bind(reason).bind(&connection.user_id).bind(grant.attempt.attempt_id.as_str())
                .execute(&mut *tx).await.map_err(persistence)?;
            tx.commit().await.map_err(persistence)?;
            return Ok(());
        }
    };
    let statement = match column {
        "runner_local_fence_at" => {
            "UPDATE inference_provider_attempts SET runner_local_fence_at = COALESCE(runner_local_fence_at, NOW(6)) WHERE user_id = ? AND attempt_id = ?"
        }
        "runner_provider_started_at" => {
            "UPDATE inference_provider_attempts SET runner_provider_started_at = COALESCE(runner_provider_started_at, NOW(6)) WHERE user_id = ? AND attempt_id = ?"
        }
        _ => return Err(ServiceError::invalid("Invalid Runner evidence field")),
    };
    sqlx::query(statement)
        .bind(&connection.user_id)
        .bind(grant.attempt.attempt_id.as_str())
        .execute(&mut *tx)
        .await
        .map_err(persistence)?;
    tx.commit().await.map_err(persistence)
}

fn persistence(error: sqlx::Error) -> ServiceError {
    ServiceError::with_source(
        ServiceErrorKind::Persistence,
        "Runner inference ledger",
        error,
    )
}

/// Exact Runner provider attempt beneath an already admitted logical
/// invocation. This is the canonical agent-loop path: logical lifecycle is
/// shared with Server execution, while request custody, physical attempt and
/// finite Runner grant commit atomically before any device delivery.
pub struct RunnerProviderAttemptDispatchPlan {
    invocation: InferenceInvocationPlan,
    attempt: InferenceProviderAttemptPlan,
    grant: RunnerInferenceDispatchGrant,
    request: String,
}

impl std::fmt::Debug for RunnerProviderAttemptDispatchPlan {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RunnerProviderAttemptDispatchPlan")
            .field("grant", &self.grant)
            .field("request", &"[REDACTED]")
            .finish()
    }
}

impl RunnerProviderAttemptDispatchPlan {
    pub fn attempt(&self) -> &InferenceProviderAttemptPlan {
        &self.attempt
    }

    pub fn grant(&self) -> &RunnerInferenceDispatchGrant {
        &self.grant
    }
}

fn digest(bytes: &[u8]) -> RunnerInferenceDigest {
    RunnerInferenceDigest::new(format!("{:x}", Sha256::digest(bytes)))
        .expect("SHA-256 is lowercase hexadecimal")
}

fn artifact_reference(id: String, bytes: &[u8]) -> ServiceResult<RunnerInferenceArtifactReference> {
    if bytes.is_empty() || bytes.len() > MAX_CUSTODY_BYTES {
        return Err(ServiceError::invalid(
            "Runner artifact exceeds the admitted 16 MiB bound or is empty",
        ));
    }
    Ok(RunnerInferenceArtifactReference {
        artifact_id: RunnerInferenceId::new(id).map_err(ServiceError::invalid)?,
        sha256: digest(bytes),
        byte_len: NonZeroU64::new(bytes.len() as u64).expect("nonempty checked"),
    })
}

fn validate_json_body(bytes: &[u8]) -> ServiceResult<String> {
    if bytes.is_empty() || bytes.len() > MAX_CUSTODY_BYTES {
        return Err(ServiceError::invalid(
            "Runner payload exceeds admitted byte bound",
        ));
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|_| ServiceError::invalid("Runner payload is not UTF-8"))?;
    serde_json::from_str::<serde_json::Value>(text)
        .map_err(|_| ServiceError::invalid("Runner payload is not valid JSON"))?;
    Ok(text.to_owned())
}

/// Exact immutable inputs for a single Runner provider-attempt dispatch.
/// Keeping them together prevents independent callers from mixing an admitted
/// invocation, request context, or wire identity from different attempts.
pub struct RunnerProviderAttemptDispatchInput<'a> {
    pub invocation: &'a InferenceInvocationPlan,
    pub attempt_index: u32,
    pub wire: InferenceProviderWireIdentity,
    pub request_context: ModelRequestContextSeed,
    pub binding: &'a ResolvedRunnerModelBinding,
    pub request: &'a [u8],
    pub deadline_unix_ms: u64,
}

pub fn plan_runner_provider_attempt_dispatch(
    input: RunnerProviderAttemptDispatchInput<'_>,
) -> ServiceResult<RunnerProviderAttemptDispatchPlan> {
    let invocation_input = &input.invocation.input;
    validate_runner_invocation(invocation_input, input.binding)?;
    if input.deadline_unix_ms == 0 || input.deadline_unix_ms > i64::MAX as u64 {
        return Err(ServiceError::invalid(
            "Runner inference deadline is out of range",
        ));
    }
    let request = validate_json_body(input.request)?;
    let request_value: serde_json::Value = serde_json::from_str(&request)
        .map_err(|_| ServiceError::invalid("Runner request is not valid JSON"))?;
    if request_value
        .get("model")
        .and_then(serde_json::Value::as_str)
        != Some(input.binding.definition.model_name.as_str())
        || input.wire.provider_wire_hash != digest(request.as_bytes()).as_str()
        || input.wire.provider_wire_bytes != request.len() as u64
        || input.wire.exact_protocol != match input.binding.definition.protocol {
            astra_turn_types::runner_inference::RunnerInferenceProtocol::OpenAiChatCompletions => {
                "openai_compatible"
            }
            astra_turn_types::runner_inference::RunnerInferenceProtocol::OpenAiResponses => {
                "openai_responses"
            }
        }
    {
        return Err(ServiceError::invalid(
            "Runner exact request identity or model does not match its admission",
        ));
    }
    let attempt = plan_inference_provider_attempt_with_context(
        input.invocation,
        input.attempt_index,
        input.wire,
        input.request_context,
    );
    let request_ref = artifact_reference(
        hash_identity("rreq", &[attempt.attempt_id()]),
        request.as_bytes(),
    )?;
    let grant = RunnerInferenceDispatchGrant {
        attempt: RunnerInferenceAttemptIdentity {
            user_id: invocation_input.user_id.clone(),
            scope: invocation_input.scope.clone(),
            invocation_id: RunnerInferenceId::new(input.invocation.invocation_id().to_string())
                .map_err(ServiceError::invalid)?,
            attempt_id: RunnerInferenceId::new(attempt.attempt_id().to_string())
                .map_err(ServiceError::invalid)?,
            binding: input.binding.definition.identity.clone(),
            request: request_ref,
        },
        grant_id: RunnerInferenceId::new(new_admission_token()).map_err(ServiceError::invalid)?,
        process_boot_nonce: input.binding.process_boot_nonce.clone(),
        run_authority: invocation_input.run_authority.as_ref().map(|authority| {
            astra_turn_types::runner_inference::RunnerInferenceRunAuthority {
                owner_generation: authority.expected_owner_generation,
                owner_pod_id: authority.expected_owner_pod_id.clone(),
                control_epoch: authority.expected_control_epoch,
            }
        }),
        start_before_unix_ms: input.deadline_unix_ms,
        deadline_unix_ms: input.deadline_unix_ms,
    };
    Ok(RunnerProviderAttemptDispatchPlan {
        invocation: input.invocation.clone(),
        attempt,
        grant,
        request,
    })
}

fn artifact_record(
    identity: &RunnerInferenceAttemptIdentity,
    reference: &RunnerInferenceArtifactReference,
    kind: &str,
    content: String,
) -> ServiceResult<SessionArtifactJsonRecord> {
    Ok(SessionArtifactJsonRecord {
        artifact_id: reference.artifact_id.as_str().into(),
        session_id: identity
            .scope
            .session_id()
            .ok_or_else(|| {
                ServiceError::invalid("Runner custody requires a session artifact owner")
            })?
            .into(),
        user_id: identity.user_id.clone(),
        artifact_kind: kind.into(),
        source: Some("runner_inference".into()),
        turn: identity.scope.turn(),
        round: identity.scope.round(),
        // A JSON string preserves exact provider bytes, including whitespace
        // and field order. Artifact readers must not reserialize a parsed body.
        content: serde_json::Value::String(content),
        metadata: Some(
            serde_json::json!({"sha256": reference.sha256, "byte_len": reference.byte_len}),
        ),
        references: vec![SessionArtifactReference {
            kind: SessionArtifactReferenceKind::InvocationLedger,
            reference_id: identity.attempt_id.as_str().into(),
        }],
    })
}

fn decode_grant(row: &sqlx::mysql::MySqlRow) -> ServiceResult<RunnerInferenceDispatchGrant> {
    let encoded: String = row.try_get("runner_grant_json").map_err(persistence)?;
    serde_json::from_str(&encoded)
        .map_err(|_| ServiceError::invalid("invalid durable Runner grant"))
}

pub async fn admit_runner_provider_attempt_dispatch(
    pool: &SharedPool,
    plan: &RunnerProviderAttemptDispatchPlan,
) -> ServiceResult<RunnerInferenceDispatchGrant> {
    let identity = &plan.grant.attempt;
    let mut tx = crate::cancellation_safe_db::CancellationSafeTransaction::begin(pool.get())
        .await
        .map_err(persistence)?;
    if !matches!(
        lock_invocation_scope_authority(&mut tx, &plan.invocation.input).await?,
        InvocationScopeAuthority::Live
    ) {
        return Err(ServiceError::conflict(
            "Runner inference scope authority unavailable",
        ));
    }
    if let Some(row) = sqlx::query(
        "SELECT runner_grant_json FROM inference_provider_attempts
         WHERE user_id = ? AND attempt_id = ? FOR UPDATE",
    )
    .bind(&identity.user_id)
    .bind(identity.attempt_id.as_str())
    .fetch_optional(&mut *tx)
    .await
    .map_err(persistence)?
    {
        let persisted = decode_grant(&row)?;
        if persisted.attempt != plan.grant.attempt
            || persisted.process_boot_nonce != plan.grant.process_boot_nonce
            || persisted.deadline_unix_ms != plan.grant.deadline_unix_ms
        {
            return Err(ServiceError::conflict(
                "Runner grant already exists with another exact identity",
            ));
        }
        return Ok(persisted);
    }
    let resolved = lock_resolved_binding(&mut tx, &identity.user_id, &identity.binding).await?;
    verify_run_native_context(&mut tx, &plan.invocation.input, &resolved.definition).await?;
    check_runner_queue_capacity(&mut tx, identity).await?;
    if resolved.process_boot_nonce != plan.grant.process_boot_nonce {
        return Err(ServiceError::conflict(
            "Runner process boot changed before attempt admission",
        ));
    }
    let database_now_ms: i64 = sqlx::query(
        "SELECT CAST(ROUND(UNIX_TIMESTAMP(CURRENT_TIMESTAMP(6)) * 1000) AS SIGNED) AS now_ms",
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(persistence)?
    .try_get("now_ms")
    .map_err(persistence)?;
    let deadline_ms = i64::try_from(plan.grant.deadline_unix_ms)
        .map_err(|_| ServiceError::invalid("Runner deadline out of range"))?;
    if deadline_ms <= database_now_ms {
        return Err(ServiceError::conflict("Runner inference deadline expired"));
    }
    let start_ms = database_now_ms
        .checked_add(MAX_START_WINDOW_MS as i64)
        .ok_or_else(|| ServiceError::internal("Runner admission clock overflow"))?
        .min(deadline_ms);
    let mut grant = plan.grant.clone();
    grant.start_before_unix_ms = start_ms as u64;
    insert_run_owned_runner_attempt(&mut tx, &plan.attempt).await?;
    persist_referenced_json_artifact_tx(
        &mut tx,
        &artifact_record(
            identity,
            &identity.request,
            "runner_inference_request",
            plan.request.clone(),
        )?,
    )
    .await
    .map_err(|_| {
        ServiceError::new(
            ServiceErrorKind::Persistence,
            "persist Runner request custody",
        )
    })?;
    sqlx::query(
        "UPDATE inference_provider_attempts SET runner_id = ?, runner_journal_id = ?,
         runner_grant_json = ?, runner_grant_expires_at = FROM_UNIXTIME(? / 1000.0),
         runner_deadline_at = FROM_UNIXTIME(? / 1000.0)
         WHERE user_id = ? AND attempt_id = ?",
    )
    .bind(identity.binding.runner_id.as_str())
    .bind(identity.binding.journal_id.as_str())
    .bind(
        serde_json::to_string(&grant)
            .map_err(|_| ServiceError::invalid("Runner grant encoding failed"))?,
    )
    .bind(start_ms)
    .bind(deadline_ms)
    .bind(&identity.user_id)
    .bind(identity.attempt_id.as_str())
    .execute(&mut *tx)
    .await
    .map_err(persistence)?;
    tx.commit().await.map_err(persistence)?;
    Ok(grant)
}

async fn insert_run_owned_runner_attempt(
    tx: &mut impl crate::cancellation_safe_db::TransactionConnection,
    attempt: &InferenceProviderAttemptPlan,
) -> ServiceResult<()> {
    let scope = &attempt.invocation_input.scope;
    let run_id = scope
        .run_id()
        .ok_or_else(|| ServiceError::invalid("Runner attempt requires its canonical Run"))?;
    let session_id = scope
        .session_id()
        .ok_or_else(|| ServiceError::invalid("Runner attempt requires its canonical Session"))?;
    sqlx::query(
        "INSERT INTO inference_provider_attempts
        (attempt_id, invocation_id, user_id, session_id, run_id, attempt_index,
         provider, admission_token, provider_protocol, provider_wire_hash, provider_wire_bytes,
         status, usage_status, started_at)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'started', 'unavailable', NOW(6))",
    )
    .bind(&attempt.attempt_id)
    .bind(&attempt.invocation_id)
    .bind(&attempt.user_id)
    .bind(session_id)
    .bind(run_id)
    .bind(i64::from(attempt.attempt_index))
    .bind(&attempt.provider)
    .bind(&attempt.admission_token)
    .bind(&attempt.wire.protocol)
    .bind(&attempt.wire.provider_wire_hash)
    .bind(checked_i64(
        attempt.wire.provider_wire_bytes,
        "provider_wire_bytes",
    )?)
    .execute(&mut **tx)
    .await
    .map_err(persistence)?;
    insert_model_request_context_event_with_expiry(
        tx,
        attempt,
        ModelRequestEventStage::Accepted,
        None,
        None,
    )
    .await
}

/// Hash covers terminal meaning and full/partial response bytes. It excludes
/// arrival pod, socket generation and mutable Server ownership.
pub fn runner_terminal_digest(
    terminal: &InferenceInvocationTerminal,
    response: &[u8],
) -> ServiceResult<RunnerInferenceDigest> {
    astra_turn_types::runner_inference::runner_terminal_digest(terminal, response)
        .map_err(|_| ServiceError::invalid("Runner terminal encoding failed"))
}

/// Accept terminal evidence from the authenticated matching Runner even after
/// the old Server owner lease expired. Duplicate hashes replay ACK; conflicting
/// hashes quarantine continuation and never overwrite the first payload.
pub async fn take_runner_terminal_custody(
    pool: &SharedPool,
    connection: &AuthenticatedRunnerConnection,
    identity: &RunnerInferenceAttemptIdentity,
    terminal: &InferenceInvocationTerminal,
    response: &[u8],
    terminal_hash: &RunnerInferenceDigest,
) -> ServiceResult<RunnerInferenceTerminalAck> {
    if connection.user_id != identity.user_id || connection.runner_id != identity.binding.runner_id
    {
        return Err(ServiceError::conflict("Runner terminal owner mismatch"));
    }
    let response = validate_json_body(response)?;
    if &runner_terminal_digest(terminal, response.as_bytes())? != terminal_hash {
        return Err(ServiceError::invalid(
            "Runner terminal content hash mismatch",
        ));
    }
    let session_id = identity
        .scope
        .session_id()
        .ok_or_else(|| ServiceError::invalid("Runner terminal requires session scope"))?;
    let fingerprint = terminal_fingerprint(terminal)?;
    let durable = DurableInferenceTerminal::from_terminal(terminal, fingerprint.clone())?;
    let mut tx = crate::cancellation_safe_db::CancellationSafeTransaction::begin(pool.get())
        .await
        .map_err(persistence)?;
    // Cancelled runs may still acquire payload custody. Deleted/tombstoned
    // sessions may not; they must not be recreated by a late device result.
    crate::storage::admit_session_event_write(&mut tx, session_id, &identity.user_id, false)
        .await
        .map_err(persistence)?;
    if let Some(run_id) = identity.scope.run_id() {
        sqlx::query("SELECT run_id FROM agent_runs WHERE user_id = ? AND session_id = ? AND run_id = ? FOR UPDATE")
            .bind(&identity.user_id).bind(session_id).bind(run_id).fetch_optional(&mut *tx).await.map_err(persistence)?
            .ok_or_else(|| ServiceError::not_found("Runner canonical Run is absent"))?;
    } else {
        sqlx::query("SELECT invocation_id FROM inference_invocations WHERE user_id = ? AND invocation_id = ? FOR UPDATE")
        .bind(&identity.user_id).bind(identity.invocation_id.as_str())
        .fetch_optional(&mut *tx).await.map_err(persistence)?
        .ok_or_else(|| ServiceError::not_found("Runner invocation absent"))?;
    }
    let row = sqlx::query(
        "SELECT runner_grant_json, runner_terminal_hash, runner_terminal_conflict, runner_no_start_evidence
        FROM inference_provider_attempts WHERE user_id = ? AND attempt_id = ? FOR UPDATE",
    )
    .bind(&identity.user_id)
    .bind(identity.attempt_id.as_str())
    .fetch_optional(&mut *tx)
    .await
    .map_err(persistence)?
    .ok_or_else(|| ServiceError::not_found("Runner attempt absent"))?;
    let grant = decode_grant(&row)?;
    if &grant.attempt != identity {
        return Err(ServiceError::conflict(
            "Runner terminal attempt identity mismatch",
        ));
    }
    if row
        .try_get::<Option<String>, _>("runner_no_start_evidence")
        .map_err(persistence)?
        .is_some()
        && (terminal.status == InferenceTerminalStatus::Succeeded
            || terminal.usage != InferenceUsage::default()
            || terminal.usage_status != InferenceUsageStatus::Unavailable)
    {
        return Err(ServiceError::conflict(
            "Runner success conflicts with no-start evidence",
        ));
    }
    lock_connection(&mut tx, connection).await?;
    let previous: Option<String> = row.try_get("runner_terminal_hash").map_err(persistence)?;
    if let Some(previous) = previous {
        if previous != terminal_hash.as_str() {
            sqlx::query(
                "UPDATE inference_provider_attempts SET runner_terminal_conflict = TRUE
                WHERE user_id = ? AND attempt_id = ?",
            )
            .bind(&identity.user_id)
            .bind(identity.attempt_id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(persistence)?;
            tx.commit().await.map_err(persistence)?;
            return Err(ServiceError::conflict(
                "Runner terminal conflict quarantined",
            ));
        }
        if row
            .try_get::<bool, _>("runner_terminal_conflict")
            .map_err(persistence)?
        {
            return Err(ServiceError::conflict("Runner terminal is quarantined"));
        }
        return Ok(RunnerInferenceTerminalAck {
            attempt: identity.clone(),
            terminal_sha256: terminal_hash.clone(),
        });
    }
    let reference = artifact_reference(
        hash_identity(
            "rresp",
            &[identity.attempt_id.as_str(), terminal_hash.as_str()],
        ),
        response.as_bytes(),
    )?;
    let record = artifact_record(identity, &reference, "runner_inference_response", response)?;
    persist_referenced_json_artifact_tx(&mut tx, &record)
        .await
        .map_err(|_| {
            ServiceError::new(
                ServiceErrorKind::Persistence,
                "persist Runner response custody",
            )
        })?;
    sqlx::query("UPDATE inference_provider_attempts SET status = ?, terminal_fingerprint = ?,
        usage_status = ?, input_tokens = ?, output_tokens = ?, cache_read_tokens = ?, cache_creation_tokens = ?,
        provider_response_id = ?, error_kind = ?, error_message = ?, terminal_at = NOW(6),
        runner_response_artifact_id = ?, runner_response_hash = ?, runner_response_bytes = ?,
        runner_terminal_hash = ?
        WHERE user_id = ? AND attempt_id = ?")
        .bind(&durable.status).bind(fingerprint).bind(&durable.usage_status)
        .bind(durable.input_tokens).bind(durable.output_tokens).bind(durable.cache_read_tokens).bind(durable.cache_creation_tokens)
        .bind(&durable.provider_response_id).bind(&durable.error_kind).bind(&durable.error_message)
        .bind(reference.artifact_id.as_str()).bind(reference.sha256.as_str()).bind(reference.byte_len.get() as i64)
        .bind(terminal_hash.as_str()).bind(&identity.user_id).bind(identity.attempt_id.as_str())
        .execute(&mut *tx).await.map_err(persistence)?;
    insert_recovered_model_request_terminal_tx(
        &mut tx,
        &identity.user_id,
        identity.invocation_id.as_str(),
        identity.attempt_id.as_str(),
        &durable,
    )
    .await
    .map_err(persistence)?;
    tx.commit().await.map_err(persistence)?;
    Ok(RunnerInferenceTerminalAck {
        attempt: identity.clone(),
        terminal_sha256: terminal_hash.clone(),
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunnerDeliveryAction {
    Dispatch(RunnerInferenceDispatchGrant),
    Cancel(RunnerInferenceDispatchGrant),
    Reconcile(RunnerInferenceDispatchGrant),
}

#[derive(Clone, Debug)]
pub struct RunnerDeliveryClaim {
    pub token: RunnerInferenceId,
    pub action: RunnerDeliveryAction,
}

/// A short claim coordinates socket-owner pods. It never mints or extends a
/// start grant. Boot replacement, expired starts and cancellation reconcile the
/// existing attempt instead of sending a fresh provider request.
pub async fn claim_runner_delivery(
    pool: &SharedPool,
    connection: &AuthenticatedRunnerConnection,
    identity: &RunnerInferenceAttemptIdentity,
) -> ServiceResult<Option<RunnerDeliveryClaim>> {
    if connection.user_id != identity.user_id || connection.runner_id != identity.binding.runner_id
    {
        return Err(ServiceError::conflict("Runner delivery owner mismatch"));
    }
    let mut tx = crate::cancellation_safe_db::CancellationSafeTransaction::begin(pool.get())
        .await
        .map_err(persistence)?;
    let session = identity
        .scope
        .session_id()
        .ok_or_else(|| ServiceError::invalid("Runner delivery requires session scope"))?;
    crate::storage::admit_session_event_write(&mut tx, session, &identity.user_id, false)
        .await
        .map_err(persistence)?;
    // Read the immutable grant before taking the Run lock, then lock/recheck
    // its attempt below. All writers retain Session -> Run -> attempt order.
    let frozen = sqlx::query(
        "SELECT runner_grant_json FROM inference_provider_attempts
         WHERE user_id = ? AND attempt_id = ?",
    )
    .bind(&identity.user_id)
    .bind(identity.attempt_id.as_str())
    .fetch_optional(&mut *tx)
    .await
    .map_err(persistence)?
    .ok_or_else(|| ServiceError::not_found("Runner delivery attempt absent"))?;
    let frozen = decode_grant(&frozen)?;
    if frozen.attempt != *identity {
        return Err(ServiceError::conflict("Runner delivery identity mismatch"));
    }
    let authority = frozen
        .run_authority
        .as_ref()
        .map(|authority| InferenceRunAdmissionAuthority {
            expected_owner_generation: authority.owner_generation,
            expected_owner_pod_id: authority.owner_pod_id.clone(),
            expected_control_epoch: authority.control_epoch,
        });
    let run_cancelled = !matches!(
        lock_inference_scope_authority(
            &mut tx,
            &identity.user_id,
            &identity.scope,
            authority.as_ref()
        )
        .await?,
        InvocationScopeAuthority::Live
    );
    let row = sqlx::query(
        "SELECT runner_grant_json, runner_terminal_hash,
        IF(runner_cancel_requested_at IS NOT NULL, 1, 0) AS cancelled,
        IF(runner_grant_expires_at > NOW(6), 1, 0) AS start_valid,
        IF(runner_local_fence_at IS NOT NULL OR runner_no_start_evidence IS NOT NULL, 1, 0) AS start_known,
        IF(runner_dispatch_claim_expires_at > NOW(6), 1, 0) AS claimed,
        IF(runner_dispatch_claim_token IS NOT NULL, 1, 0) AS previously_claimed,
        IF(runner_capacity_retired_at IS NOT NULL, 1, 0) AS capacity_retired
        FROM inference_provider_attempts WHERE user_id = ? AND attempt_id = ? FOR UPDATE",
    )
    .bind(&identity.user_id)
    .bind(identity.attempt_id.as_str())
    .fetch_optional(&mut *tx)
    .await
    .map_err(persistence)?
    .ok_or_else(|| ServiceError::not_found("Runner delivery attempt absent"))?;
    let grant = decode_grant(&row)?;
    if grant != frozen {
        return Err(ServiceError::conflict("Runner delivery grant changed"));
    }
    let registry = lock_connection(&mut tx, connection).await?;
    if row
        .try_get::<Option<String>, _>("runner_terminal_hash")
        .map_err(persistence)?
        .is_some()
        || row
            .try_get::<i64, _>("capacity_retired")
            .map_err(persistence)?
            == 1
        || row.try_get::<i64, _>("claimed").map_err(persistence)? == 1
    {
        return Ok(None);
    }
    let cancelled = run_cancelled || row.try_get::<i64, _>("cancelled").map_err(persistence)? == 1;
    let current_boot: Option<String> = registry
        .try_get("inference_boot_nonce")
        .map_err(persistence)?;
    let current_journal: Option<String> = registry
        .try_get("inference_journal_id")
        .map_err(persistence)?;
    let current_edge: Option<String> =
        registry.try_get("inference_edge_id").map_err(persistence)?;
    let can_start = row.try_get::<i64, _>("start_valid").map_err(persistence)? == 1
        && row.try_get::<i64, _>("start_known").map_err(persistence)? == 0
        && current_boot.as_deref() == Some(grant.process_boot_nonce.as_str())
        && current_journal.as_deref() == Some(identity.binding.journal_id.as_str())
        && current_edge.as_deref() == Some(connection.edge_id.as_str());
    if can_start
        && !cancelled
        && row
            .try_get::<i64, _>("previously_claimed")
            .map_err(persistence)?
            == 0
    {
        // A delivery claim reserves local capacity until definitive terminal,
        // no-start evidence, or authenticated bounded incarnation retirement.
        // Claim timeout only permits replay of that same
        // grant; it must not free a possibly executing provider slot. Cancel
        // intent is likewise conservative until the Runner proves no start.
        let capacity = sqlx::query(
            "SELECT COUNT(*) AS active_count,
                CAST(COALESCE(SUM(IF(attempt.session_id = ?, 1, 0)), 0) AS SIGNED) AS session_count
             FROM inference_provider_attempts attempt
             WHERE attempt.user_id = ? AND attempt.runner_id = ?
               AND attempt.runner_journal_id = ? AND attempt.status = 'started'
               AND attempt.runner_terminal_hash IS NULL AND attempt.runner_no_start_evidence IS NULL
               AND attempt.runner_capacity_retired_at IS NULL
               AND (attempt.runner_dispatch_claim_token IS NOT NULL
                    OR attempt.runner_local_fence_at IS NOT NULL
                    OR attempt.runner_cancel_requested_at IS NOT NULL)",
        )
        .bind(session)
        .bind(&identity.user_id)
        .bind(identity.binding.runner_id.as_str())
        .bind(identity.binding.journal_id.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(persistence)?;
        let active: i64 = capacity.try_get("active_count").map_err(persistence)?;
        let session_active: i64 = capacity.try_get("session_count").map_err(persistence)?;
        if active >= MAX_RUNNER_ACTIVE_ATTEMPTS
            || session_active >= MAX_RUNNER_SESSION_ACTIVE_ATTEMPTS
        {
            return Ok(None);
        }
    }
    let action = if cancelled {
        RunnerDeliveryAction::Cancel(grant)
    } else if can_start {
        RunnerDeliveryAction::Dispatch(grant)
    } else {
        RunnerDeliveryAction::Reconcile(grant)
    };
    let token = RunnerInferenceId::new(new_admission_token()).map_err(ServiceError::invalid)?;
    sqlx::query("UPDATE inference_provider_attempts SET runner_dispatch_claim_token = ?,
        runner_dispatch_claim_expires_at = DATE_ADD(NOW(6), INTERVAL 10 SECOND),
        runner_cancel_requested_at = IF(?, COALESCE(runner_cancel_requested_at, NOW(6)), runner_cancel_requested_at)
        WHERE user_id = ? AND attempt_id = ?")
        .bind(token.as_str()).bind(cancelled).bind(&identity.user_id).bind(identity.attempt_id.as_str())
        .execute(&mut *tx).await.map_err(persistence)?;
    tx.commit().await.map_err(persistence)?;
    Ok(Some(RunnerDeliveryClaim { token, action }))
}

/// Persist intent only. This never reports no execution or zero usage: an
/// already escaped finite grant needs matching Runner reconciliation evidence.
pub async fn request_runner_cancellation(
    pool: &SharedPool,
    authenticated_user_id: &str,
    identity: &RunnerInferenceAttemptIdentity,
) -> ServiceResult<()> {
    if authenticated_user_id != identity.user_id {
        return Err(ServiceError::conflict("Runner cancellation owner mismatch"));
    }
    let mut tx = crate::cancellation_safe_db::CancellationSafeTransaction::begin(pool.get())
        .await
        .map_err(persistence)?;
    let session = identity
        .scope
        .session_id()
        .ok_or_else(|| ServiceError::invalid("Runner cancellation requires session scope"))?;
    crate::storage::admit_session_event_write(&mut tx, session, authenticated_user_id, false)
        .await
        .map_err(persistence)?;
    let row = sqlx::query(
        "SELECT runner_grant_json FROM inference_provider_attempts
        WHERE user_id = ? AND attempt_id = ? FOR UPDATE",
    )
    .bind(authenticated_user_id)
    .bind(identity.attempt_id.as_str())
    .fetch_optional(&mut *tx)
    .await
    .map_err(persistence)?
    .ok_or_else(|| ServiceError::not_found("Runner cancellation attempt absent"))?;
    if decode_grant(&row)?.attempt != *identity {
        return Err(ServiceError::conflict(
            "Runner cancellation identity mismatch",
        ));
    }
    sqlx::query("UPDATE inference_provider_attempts SET runner_cancel_requested_at = COALESCE(runner_cancel_requested_at, NOW(6)),
        runner_dispatch_claim_token = NULL, runner_dispatch_claim_expires_at = NULL
        WHERE user_id = ? AND attempt_id = ?")
        .bind(authenticated_user_id).bind(identity.attempt_id.as_str()).execute(&mut *tx).await.map_err(persistence)?;
    tx.commit().await.map_err(persistence)
}

/// One bounded batch per authenticated Runner, suitable for reconnect and the
/// existing shared sweeper. No per-invocation polling loop or socket-owned state.
pub async fn list_runner_reconciliation(
    pool: &SharedPool,
    connection: &AuthenticatedRunnerConnection,
    limit: u32,
) -> ServiceResult<Vec<RunnerInferenceDispatchGrant>> {
    let mut tx = crate::cancellation_safe_db::CancellationSafeTransaction::begin(pool.get())
        .await
        .map_err(persistence)?;
    lock_connection(&mut tx, connection).await?;
    let rows = sqlx::query(
        "SELECT runner_grant_json FROM (
            SELECT attempt.runner_grant_json, attempt.runner_grant_expires_at, attempt.attempt_id,
                ROW_NUMBER() OVER (PARTITION BY attempt.session_id
                    ORDER BY attempt.runner_grant_expires_at, attempt.attempt_id) AS session_position
            FROM inference_provider_attempts attempt
            WHERE attempt.user_id = ? AND attempt.runner_id = ? AND attempt.status = 'started'
              AND attempt.runner_capacity_retired_at IS NULL
              AND (attempt.runner_dispatch_claim_expires_at IS NULL OR attempt.runner_dispatch_claim_expires_at <= NOW(6))
        ) pending
        ORDER BY session_position, runner_grant_expires_at, attempt_id LIMIT ?",
    )
    .bind(&connection.user_id)
    .bind(connection.runner_id.as_str())
    .bind(i64::from(limit.clamp(1, 128)))
    .fetch_all(&mut *tx)
    .await
    .map_err(persistence)?;
    rows.iter().map(decode_grant).collect()
}

fn public_terminal(
    terminal: &DurableInferenceTerminal,
) -> ServiceResult<InferenceInvocationTerminal> {
    let status = match terminal.status.as_str() {
        "succeeded" => InferenceTerminalStatus::Succeeded,
        "failed" => InferenceTerminalStatus::Failed,
        "cancelled" => InferenceTerminalStatus::Cancelled,
        "delivery_unknown" => InferenceTerminalStatus::DeliveryUnknown,
        _ => {
            return Err(ServiceError::invalid(
                "invalid durable Runner terminal status",
            ));
        }
    };
    let usage_status = match terminal.usage_status.as_str() {
        "provider_exact" => InferenceUsageStatus::ProviderExact,
        "provider_partial" => InferenceUsageStatus::ProviderPartial,
        "unavailable" => InferenceUsageStatus::Unavailable,
        _ => return Err(ServiceError::invalid("invalid durable Runner usage status")),
    };
    let nonnegative = |value: i64| {
        u64::try_from(value).map_err(|_| ServiceError::invalid("negative durable Runner usage"))
    };
    Ok(InferenceInvocationTerminal {
        status,
        usage: InferenceUsage {
            input: astra_turn_types::NormalizedPromptCacheUsage {
                fresh_input_tokens: nonnegative(terminal.input_tokens)?,
                cache_read_tokens: nonnegative(terminal.cache_read_tokens)?,
                cache_creation_tokens: nonnegative(terminal.cache_creation_tokens)?,
            },
            output_tokens: nonnegative(terminal.output_tokens)?,
        },
        usage_status,
        provider_response_id: terminal.provider_response_id.clone(),
        error_kind: terminal.error_kind.clone(),
        error_message: terminal.error_message.clone(),
    })
}

async fn verify_run_native_context(
    tx: &mut impl crate::cancellation_safe_db::TransactionConnection,
    input: &InferenceInvocationInput,
    definition: &RunnerInferenceBindingDefinition,
) -> ServiceResult<()> {
    let RunnerInferenceSource::Chatgpt { native_context, .. } = &definition.source else {
        return Ok(());
    };
    let run_id = input
        .scope
        .run_id()
        .ok_or_else(|| ServiceError::invalid("device ChatGPT requires a canonical Run"))?;
    // The Run row/lifecycle fence is already held. Immutable start facts are
    // the existing execution owner, not a client-asserted session identity.
    let events: Vec<String> = sqlx::query_scalar(
        "SELECT payload_json FROM agent_run_events WHERE user_id = ? AND run_id = ? AND event_type = 'run_started' ORDER BY event_idx ASC LIMIT 2"
    ).bind(&input.user_id).bind(run_id).fetch_all(&mut **tx).await.map_err(persistence)?;
    if events.len() != 1 {
        return Err(ServiceError::invalid(
            "device Run authentication is unavailable",
        ));
    }
    let event: serde_json::Value = serde_json::from_str(&events[0])
        .map_err(|_| ServiceError::invalid("device Run authentication is malformed"))?;
    let data = event
        .get("data")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| ServiceError::invalid("device Run admission is malformed"))?;
    let provenance = crate::runs::decode_admission_authentication(data)
        .map_err(|_| ServiceError::invalid("device Run authentication is malformed"))?;
    match provenance {
        Some(crate::auth::ExecutionAuthenticationProvenance::VerifiedProviderSession {
            user_id,
            session_id,
            provider_id,
            ..
        }) if user_id == input.user_id
            && session_id == native_context.session_id.as_str()
            && provider_id.starts_with("uc:") =>
        {
            Ok(())
        }
        _ => Err(ServiceError::conflict(
            "device model belongs to a different MOI login; reconnect and select the model again",
        )),
    }
}
