//! Runner delivery extends the canonical inference ledger, not a second job
//! state machine. A grant is immutable before transport; terminal ACK means
//! payload custody plus a recoverable continuation obligation, not just usage.

use std::{collections::HashSet, num::NonZeroU64};

use astra_turn_types::runner_inference::{
    RunnerInferenceArtifactReference, RunnerInferenceAttemptIdentity, RunnerInferenceDigest,
    RunnerInferenceDispatchGrant, RunnerInferenceId, RunnerInferenceStartEvidence,
    RunnerInferenceTerminalAck,
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
    let mut tx = pool.get().begin().await.map_err(persistence)?;
    lock_connection(&mut tx, connection).await?;
    let row = sqlx::query("SELECT runner_grant_json FROM inference_provider_attempts WHERE user_id = ? AND attempt_id = ?")
        .bind(&identity.user_id).bind(identity.attempt_id.as_str()).fetch_optional(&mut *tx).await.map_err(persistence)?
        .ok_or_else(|| ServiceError::not_found("Runner terminal attempt absent"))?;
    if decode_grant(&row)?.attempt != *identity {
        return Err(ServiceError::conflict("Runner terminal identity mismatch"));
    }
    Ok(())
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
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
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
    let mut tx = pool.get().begin().await.map_err(persistence)?;
    lock_connection(&mut tx, connection).await?;
    let row = sqlx::query("SELECT runner_grant_json FROM inference_provider_attempts WHERE user_id = ? AND attempt_id = ?")
        .bind(&connection.user_id).bind(grant.attempt.attempt_id.as_str()).fetch_optional(&mut *tx).await.map_err(persistence)?
        .ok_or_else(|| ServiceError::not_found("Runner request attempt absent"))?;
    if decode_grant(&row)? != *grant {
        return Err(ServiceError::conflict("Runner request grant mismatch"));
    }
    load_exact_artifact_tx(&mut tx, &grant.attempt, &grant.attempt.request).await
}

pub async fn load_runner_response_custody(
    pool: &SharedPool,
    claim: &RunnerContinuationClaim,
) -> ServiceResult<RunnerCustodyBytes> {
    let mut tx = pool.get().begin().await.map_err(persistence)?;
    if !matches!(
        lock_invocation_scope_authority(&mut tx, &claim.invocation.input).await?,
        InvocationScopeAuthority::Live
    ) {
        return Err(ServiceError::conflict(
            "Runner response continuation authority changed",
        ));
    }
    lock_admitted_inference_invocation(
        &mut tx,
        &claim.identity.user_id,
        claim.identity.invocation_id.as_str(),
        &claim.invocation.owner_token,
        claim.invocation.owner_generation,
        "load Runner response custody",
    )
    .await?;
    load_exact_artifact_tx(&mut tx, &claim.identity, &claim.response).await
}

/// Verify that a checkpoint marker corresponds to one immutable Runner
/// custody record.  This verifies *consumption evidence* only and never
/// returns response bytes: a receipt carried by the restored checkpoint is
/// already part of the canonical conversation and must not be replayed.
pub async fn verify_runner_checkpoint_consumption(
    pool: &SharedPool,
    input: &InferenceInvocationInput,
    receipt: &astra_turn_types::runner_inference::RunnerInferenceContinuationReceipt,
) -> ServiceResult<()> {
    if input.user_id != receipt.attempt.user_id || input.scope != receipt.attempt.scope {
        return Err(ServiceError::conflict(
            "Runner checkpoint receipt scope mismatch",
        ));
    }
    let mut tx = pool.get().begin().await.map_err(persistence)?;
    if !matches!(
        lock_invocation_scope_authority(&mut tx, input).await?,
        InvocationScopeAuthority::Live
    ) {
        return Err(ServiceError::conflict(
            "Runner checkpoint receipt run authority unavailable",
        ));
    }
    let invocation = sqlx::query(
        "SELECT status FROM inference_invocations
         WHERE user_id = ? AND invocation_id = ? FOR UPDATE",
    )
    .bind(&receipt.attempt.user_id)
    .bind(receipt.attempt.invocation_id.as_str())
    .fetch_optional(&mut *tx)
    .await
    .map_err(persistence)?
    .ok_or_else(|| ServiceError::not_found("Runner checkpoint receipt invocation absent"))?;
    let invocation_status: String = invocation.try_get("status").map_err(persistence)?;
    if !matches!(
        invocation_status.as_str(),
        "succeeded" | "failed" | "cancelled" | "delivery_unknown"
    ) {
        return Err(ServiceError::conflict(
            "Runner checkpoint receipt invocation is not logically terminal",
        ));
    }
    let row = sqlx::query(
        "SELECT runner_grant_json, runner_terminal_hash, runner_response_artifact_id,
         runner_response_hash, runner_response_bytes, runner_continuation_pending,
         runner_terminal_conflict
         FROM inference_provider_attempts WHERE user_id = ? AND attempt_id = ? FOR UPDATE",
    )
    .bind(&receipt.attempt.user_id)
    .bind(receipt.attempt.attempt_id.as_str())
    .fetch_optional(&mut *tx)
    .await
    .map_err(persistence)?
    .ok_or_else(|| ServiceError::not_found("Runner checkpoint receipt attempt absent"))?;
    let response_bytes = i64::try_from(receipt.response.byte_len.get())
        .map_err(|_| ServiceError::invalid("Runner checkpoint receipt length overflow"))?;
    if decode_grant(&row)?.attempt != receipt.attempt
        || row
            .try_get::<String, _>("runner_terminal_hash")
            .map_err(persistence)?
            != receipt.terminal_sha256.as_str()
        || row
            .try_get::<String, _>("runner_response_artifact_id")
            .map_err(persistence)?
            != receipt.response.artifact_id.as_str()
        || row
            .try_get::<String, _>("runner_response_hash")
            .map_err(persistence)?
            != receipt.response.sha256.as_str()
        || row
            .try_get::<i64, _>("runner_response_bytes")
            .map_err(persistence)?
            != response_bytes
        || !row
            .try_get::<bool, _>("runner_continuation_pending")
            .map_err(persistence)?
        || row
            .try_get::<bool, _>("runner_terminal_conflict")
            .map_err(persistence)?
    {
        return Err(ServiceError::conflict(
            "Runner checkpoint receipt is stale or quarantined",
        ));
    }
    tx.commit().await.map_err(persistence)
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

/// Load the bounded, contiguous logical-attempt chain for exactly one next
/// Runner round.  Callers supply the post-checkpoint scope (normally logical
/// attempt zero) and every receipt already embedded in that checkpoint.  The
/// query is by the full durable scope; it never falls back to latest-by-run.
///
/// A receipt in `consumed` removes exactly the matching terminal from
/// consideration.  Its authenticity must have been checked with
/// [`verify_runner_checkpoint_consumption`] before this call.  At most two
/// attempts are returned because the Agent Loop's output-cap continuation is
/// bounded to one suffix; a third contiguous terminal is a contract failure,
/// not an invitation to replay an unbounded history.
pub async fn load_next_runner_continuation_chain(
    pool: &SharedPool,
    input: &InferenceInvocationInput,
    consumed: &[astra_turn_types::runner_inference::RunnerInferenceContinuationReceipt],
) -> ServiceResult<Vec<RunnerRecoveredContinuation>> {
    let mut tx = pool.get().begin().await.map_err(persistence)?;
    if !matches!(
        lock_invocation_scope_authority(&mut tx, input).await?,
        InvocationScopeAuthority::Live
    ) {
        return Err(ServiceError::conflict(
            "Runner continuation recovery run authority unavailable",
        ));
    }
    let scope = &input.scope;
    let consumed: HashSet<(String, String)> = consumed
        .iter()
        .map(|receipt| {
            (
                receipt.attempt.attempt_id.as_str().to_string(),
                receipt.terminal_sha256.as_str().to_string(),
            )
        })
        .collect();
    let rows = sqlx::query(
        "SELECT attempt.runner_grant_json, attempt.runner_terminal_hash,
         attempt.runner_response_artifact_id, attempt.runner_response_hash,
         attempt.runner_response_bytes, attempt.status, attempt.terminal_fingerprint,
         attempt.usage_status, attempt.input_tokens, attempt.output_tokens,
         attempt.cache_read_tokens, attempt.cache_creation_tokens,
         attempt.provider_response_id, attempt.error_kind, attempt.error_message,
         invocation.logical_attempt
         FROM inference_provider_attempts AS attempt
         INNER JOIN inference_invocations AS invocation
           ON invocation.user_id = attempt.user_id
          AND invocation.invocation_id = attempt.invocation_id
         WHERE attempt.user_id = ?
           AND invocation.scope_kind = ?
           AND invocation.session_id <=> ? AND invocation.run_id <=> ?
           AND invocation.harness_run_id <=> ? AND invocation.turn_index <=> ?
           AND invocation.round_index <=> ? AND invocation.operation_id = ?
           AND invocation.purpose = ?
           AND invocation.logical_attempt >= ?
           AND invocation.status IN ('succeeded', 'failed', 'cancelled', 'delivery_unknown')
           AND attempt.runner_continuation_pending = TRUE
           AND attempt.runner_terminal_conflict = FALSE
         ORDER BY invocation.logical_attempt ASC LIMIT 3 FOR UPDATE",
    )
    .bind(&input.user_id)
    .bind(scope.kind())
    .bind(scope.session_id())
    .bind(scope.run_id())
    .bind(scope.harness_run_id())
    .bind(scope.turn().map(i64::from))
    .bind(scope.round().map(i64::from))
    .bind(scope.operation_id())
    .bind(input.purpose.as_str())
    .bind(i64::from(scope.logical_attempt()))
    .fetch_all(&mut *tx)
    .await
    .map_err(persistence)?;

    let mut chain = Vec::new();
    let mut expected_attempt = i64::from(scope.logical_attempt());
    for row in rows {
        let identity = decode_grant(&row)?.attempt;
        let terminal_hash = RunnerInferenceDigest::new(
            row.try_get::<String, _>("runner_terminal_hash")
                .map_err(persistence)?,
        )
        .map_err(ServiceError::invalid)?;
        if consumed.contains(&(
            identity.attempt_id.as_str().to_string(),
            terminal_hash.as_str().to_string(),
        )) {
            continue;
        }
        let logical_attempt: i64 = row.try_get("logical_attempt").map_err(persistence)?;
        if logical_attempt != expected_attempt {
            return Err(ServiceError::conflict(
                "Runner continuation recovery has a logical-attempt gap",
            ));
        }
        let response = RunnerInferenceArtifactReference {
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
                .map_err(|_| ServiceError::invalid("Runner recovery response length invalid"))?,
            )
            .ok_or_else(|| ServiceError::invalid("Runner recovery response is empty"))?,
        };
        let receipt = astra_turn_types::runner_inference::RunnerInferenceContinuationReceipt {
            attempt: identity,
            terminal_sha256: terminal_hash,
            response,
        };
        let request =
            load_exact_artifact_tx(&mut tx, &receipt.attempt, &receipt.attempt.request).await?;
        let response = load_exact_artifact_tx(&mut tx, &receipt.attempt, &receipt.response).await?;
        let physical_terminal =
            public_terminal(&DurableInferenceTerminal::decode(&row).map_err(persistence)?)?;
        chain.push(RunnerRecoveredContinuation {
            receipt,
            physical_terminal,
            request,
            response,
        });
        expected_attempt = expected_attempt.saturating_add(1);
    }
    if chain.len() > 2 {
        return Err(ServiceError::conflict(
            "Runner continuation recovery exceeds output-cap chain bound",
        ));
    }
    tx.commit().await.map_err(persistence)?;
    Ok(chain)
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
    let mut tx = pool.get().begin().await.map_err(persistence)?;
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
    // Column is a closed internal enum projection, never input text.
    sqlx::query(&format!("UPDATE inference_provider_attempts SET {column} = COALESCE({column}, NOW(6)) WHERE user_id = ? AND attempt_id = ?"))
        .bind(&connection.user_id).bind(grant.attempt.attempt_id.as_str()).execute(&mut *tx).await.map_err(persistence)?;
    tx.commit().await.map_err(persistence)
}

fn persistence(error: sqlx::Error) -> ServiceError {
    ServiceError::with_source(
        ServiceErrorKind::Persistence,
        "Runner inference ledger",
        error,
    )
}

/// Holds private request content only until admission. Debug deliberately does
/// not traverse canonical request data or the provider body.
pub struct RunnerInferenceDispatchPlan {
    invocation: InferenceInvocationPlan,
    attempt: InferenceProviderAttemptPlan,
    grant: RunnerInferenceDispatchGrant,
    request: String,
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

impl std::fmt::Debug for RunnerInferenceDispatchPlan {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RunnerInferenceDispatchPlan")
            .field("grant", &self.grant)
            .field("request", &"[REDACTED]")
            .finish()
    }
}

impl RunnerInferenceDispatchPlan {
    pub fn invocation(&self) -> &InferenceInvocationPlan {
        &self.invocation
    }
    pub fn attempt(&self) -> &InferenceProviderAttemptPlan {
        &self.attempt
    }
    pub fn grant(&self) -> &RunnerInferenceDispatchGrant {
        &self.grant
    }

    pub fn with_canonical_transitions(
        mut self,
        transitions: &[astra_turn_types::ProviderCanonicalTransitionV2],
    ) -> ServiceResult<Self> {
        self.attempt = self.attempt.with_canonical_transitions(transitions)?;
        Ok(self)
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

/// A stable logical route pins binding/profile revisions, while its Offering
/// identity stays stable across local credential/profile rotation.
pub fn plan_runner_inference_dispatch(
    input: InferenceInvocationInput,
    binding: &ResolvedRunnerModelBinding,
    request: &[u8],
    deadline_unix_ms: u64,
) -> ServiceResult<RunnerInferenceDispatchPlan> {
    if input.user_id != binding.user_id
        || input.scope.session_id().is_none()
        || input.execution_placement != ModelExecutionPlacement::Edge
        || input.access_kind != ModelAccessKind::ThisDevice
        || input.upstream_model_name != binding.definition.model_name.as_str()
        || input.resolved_model_name != binding.definition.model_name.as_str()
        || input.provider != "openai"
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
    if !matches!(
        input.purpose,
        InferencePurpose::PrimaryAgent
            | InferencePurpose::SubAgent
            | InferencePurpose::RequiredCompaction
    ) {
        return Err(ServiceError::invalid(
            "Runner binding has no authority for optional background inference",
        ));
    }
    if deadline_unix_ms == 0 || deadline_unix_ms > i64::MAX as u64 {
        return Err(ServiceError::invalid(
            "Runner inference deadline is out of range",
        ));
    }
    let request_text = validate_json_body(request)?;
    let request_value: serde_json::Value = serde_json::from_str(&request_text)
        .map_err(|_| ServiceError::invalid("Runner request is not valid JSON"))?;
    if request_value
        .get("model")
        .and_then(serde_json::Value::as_str)
        != Some(binding.definition.model_name.as_str())
    {
        return Err(ServiceError::invalid(
            "Runner exact request model does not match its binding",
        ));
    }
    let mut invocation = plan_inference_invocation(input)?;
    let binding_json = serde_json::to_string(&binding.definition.identity)
        .map_err(|_| ServiceError::invalid("Runner binding encoding failed"))?;
    invocation.invocation_id = hash_identity("inv", &[&invocation.invocation_id, &binding_json]);
    invocation.route_id = hash_identity("route", &[&invocation.invocation_id]);
    let wire = InferenceProviderWireIdentity::new(
        "openai_compatible",
        digest(request).as_str(),
        request.len() as u64,
    )?;
    let attempt = plan_inference_provider_attempt(&invocation, 0, wire);
    let reference = artifact_reference(hash_identity("rreq", &[&attempt.attempt_id]), request)?;
    let grant = RunnerInferenceDispatchGrant {
        attempt: RunnerInferenceAttemptIdentity {
            user_id: invocation.input.user_id.clone(),
            scope: invocation.input.scope.clone(),
            invocation_id: RunnerInferenceId::new(invocation.invocation_id.clone())
                .map_err(ServiceError::invalid)?,
            attempt_id: RunnerInferenceId::new(attempt.attempt_id.clone())
                .map_err(ServiceError::invalid)?,
            binding: binding.definition.identity.clone(),
            request: reference,
        },
        grant_id: RunnerInferenceId::new(new_admission_token()).map_err(ServiceError::invalid)?,
        process_boot_nonce: binding.process_boot_nonce.clone(),
        // Admission replaces this provisional value from the database clock
        // before the grant becomes durable or leaves the Server.
        start_before_unix_ms: deadline_unix_ms,
        deadline_unix_ms,
    };
    Ok(RunnerInferenceDispatchPlan {
        invocation,
        attempt,
        grant,
        request: request_text,
    })
}

/// Exact immutable inputs for a single Runner provider-attempt dispatch.
/// Keeping them together prevents independent callers from mixing an admitted
/// invocation, request context, or wire identity from different attempts.
pub struct RunnerProviderAttemptDispatchInput<'a> {
    pub invocation: &'a InferenceInvocationPlan,
    pub attempt_index: u32,
    pub wire: InferenceProviderWireIdentity,
    pub request_context: ModelRequestContextSeed,
    pub canonical_transitions: &'a [astra_turn_types::ProviderCanonicalTransitionV2],
    pub binding: &'a ResolvedRunnerModelBinding,
    pub request: &'a [u8],
    pub deadline_unix_ms: u64,
}

pub fn plan_runner_provider_attempt_dispatch(
    input: RunnerProviderAttemptDispatchInput<'_>,
) -> ServiceResult<RunnerProviderAttemptDispatchPlan> {
    let invocation_input = &input.invocation.input;
    if invocation_input.user_id != input.binding.user_id
        || invocation_input.scope.session_id().is_none()
        || invocation_input.execution_placement != ModelExecutionPlacement::Edge
        || invocation_input.access_kind != ModelAccessKind::ThisDevice
        || invocation_input.upstream_model_name != input.binding.definition.model_name.as_str()
        || invocation_input.resolved_model_name != input.binding.definition.model_name.as_str()
        || invocation_input.provider != "openai"
        || invocation_input.offering_id
            != crate::runner_model_bindings::runner_offering_id(
                &invocation_input.user_id,
                &input.binding.definition.identity,
            )
        || !matches!(
            invocation_input.purpose,
            InferencePurpose::PrimaryAgent
                | InferencePurpose::SubAgent
                | InferencePurpose::RequiredCompaction
        )
    {
        return Err(ServiceError::invalid(
            "Runner attempt does not match its admitted logical invocation and binding",
        ));
    }
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
        || input.wire.protocol != "openai_compatible"
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
    )
    .with_canonical_transitions(input.canonical_transitions)?;
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

/// Atomic exact request custody, route/attempt admission, and start grant. A
/// failed/unknown commit must be retried with this same plan, never a new grant.
pub async fn admit_runner_inference_dispatch(
    pool: &SharedPool,
    plan: &RunnerInferenceDispatchPlan,
) -> ServiceResult<RunnerInferenceDispatchGrant> {
    let identity = &plan.grant.attempt;
    let mut tx = pool.get().begin().await.map_err(persistence)?;
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
    if resolved.process_boot_nonce != plan.grant.process_boot_nonce {
        return Err(ServiceError::conflict(
            "Runner process boot changed before admission",
        ));
    }
    // Use the database epoch as the admission clock. Comparing a Rust UTC
    // `NaiveDateTime` with `NOW()` makes correctness depend on the database
    // session timezone and caused valid grants to fail eight hours early on a
    // default local MatrixOne installation.
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
    insert_inference_invocation_admission(&mut tx, &plan.invocation).await?;
    insert_inference_provider_attempt_admission(
        &mut tx,
        &plan.attempt,
        checked_i64(plan.attempt.wire.provider_wire_bytes, "provider_wire_bytes")?,
    )
    .await?;
    let record = artifact_record(
        identity,
        &identity.request,
        "runner_inference_request",
        plan.request.clone(),
    )?;
    persist_referenced_json_artifact_tx(&mut tx, &record)
        .await
        .map_err(|_| {
            ServiceError::new(
                ServiceErrorKind::Persistence,
                "persist Runner request custody",
            )
        })?;
    sqlx::query(
        "UPDATE inference_routes SET runner_binding_json = ? WHERE user_id = ? AND route_id = ?",
    )
    .bind(
        serde_json::to_string(&identity.binding)
            .map_err(|_| ServiceError::invalid("Runner binding encoding failed"))?,
    )
    .bind(&identity.user_id)
    .bind(&plan.invocation.route_id)
    .execute(&mut *tx)
    .await
    .map_err(persistence)?;
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

pub async fn admit_runner_provider_attempt_dispatch(
    pool: &SharedPool,
    plan: &RunnerProviderAttemptDispatchPlan,
) -> ServiceResult<RunnerInferenceDispatchGrant> {
    let identity = &plan.grant.attempt;
    let mut tx = pool.get().begin().await.map_err(persistence)?;
    if !matches!(
        lock_invocation_scope_authority(&mut tx, &plan.invocation.input).await?,
        InvocationScopeAuthority::Live
    ) {
        return Err(ServiceError::conflict(
            "Runner inference scope authority unavailable",
        ));
    }
    lock_admitted_inference_invocation(
        &mut tx,
        &identity.user_id,
        identity.invocation_id.as_str(),
        &plan.invocation.owner_token,
        plan.invocation.owner_generation,
        "admit Runner provider attempt",
    )
    .await?;
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
    insert_inference_provider_attempt_admission(
        &mut tx,
        &plan.attempt,
        checked_i64(plan.attempt.wire.provider_wire_bytes, "provider_wire_bytes")?,
    )
    .await?;
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
        "UPDATE inference_routes SET runner_binding_json = ? WHERE user_id = ? AND route_id = ?",
    )
    .bind(
        serde_json::to_string(&identity.binding)
            .map_err(|_| ServiceError::invalid("Runner binding encoding failed"))?,
    )
    .bind(&identity.user_id)
    .bind(plan.invocation.route_id())
    .execute(&mut *tx)
    .await
    .map_err(persistence)?;
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
    let mut tx = pool.get().begin().await.map_err(persistence)?;
    // Cancelled runs may still acquire payload custody. Deleted/tombstoned
    // sessions may not; they must not be recreated by a late device result.
    crate::storage::admit_session_event_write(&mut tx, session_id, &identity.user_id, false)
        .await
        .map_err(persistence)?;
    sqlx::query("SELECT invocation_id FROM inference_invocations WHERE user_id = ? AND invocation_id = ? FOR UPDATE")
        .bind(&identity.user_id).bind(identity.invocation_id.as_str())
        .fetch_optional(&mut *tx).await.map_err(persistence)?
        .ok_or_else(|| ServiceError::not_found("Runner invocation absent"))?;
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
        runner_terminal_hash = ?, runner_continuation_pending = TRUE
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
    let mut tx = pool.get().begin().await.map_err(persistence)?;
    let session = identity
        .scope
        .session_id()
        .ok_or_else(|| ServiceError::invalid("Runner delivery requires session scope"))?;
    crate::storage::admit_session_event_write(&mut tx, session, &identity.user_id, false)
        .await
        .map_err(persistence)?;
    let run_cancelled = if let Some(run_id) = identity.scope.run_id() {
        let row = sqlx::query(
            "SELECT IF(status = 'running' AND cancellation_requested_at IS NULL, 0, 1) AS cancelled
            FROM agent_runs WHERE user_id = ? AND session_id = ? AND run_id = ? FOR UPDATE",
        )
        .bind(&identity.user_id)
        .bind(session)
        .bind(run_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(persistence)?;
        match row {
            Some(row) => row.try_get::<i64, _>("cancelled").map_err(persistence)? != 0,
            None => true,
        }
    } else {
        false
    };
    let row = sqlx::query(
        "SELECT runner_grant_json, runner_terminal_hash,
        IF(runner_cancel_requested_at IS NOT NULL, 1, 0) AS cancelled,
        IF(runner_grant_expires_at > NOW(6), 1, 0) AS start_valid,
        IF(runner_local_fence_at IS NOT NULL OR runner_no_start_evidence IS NOT NULL, 1, 0) AS start_known,
        IF(runner_dispatch_claim_expires_at > NOW(6), 1, 0) AS claimed
        FROM inference_provider_attempts WHERE user_id = ? AND attempt_id = ? FOR UPDATE",
    )
    .bind(&identity.user_id)
    .bind(identity.attempt_id.as_str())
    .fetch_optional(&mut *tx)
    .await
    .map_err(persistence)?
    .ok_or_else(|| ServiceError::not_found("Runner delivery attempt absent"))?;
    let grant = decode_grant(&row)?;
    if &grant.attempt != identity {
        return Err(ServiceError::conflict("Runner delivery identity mismatch"));
    }
    let registry = lock_connection(&mut tx, connection).await?;
    if row
        .try_get::<Option<String>, _>("runner_terminal_hash")
        .map_err(persistence)?
        .is_some()
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
    let mut tx = pool.get().begin().await.map_err(persistence)?;
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
    let mut tx = pool.get().begin().await.map_err(persistence)?;
    lock_connection(&mut tx, connection).await?;
    let rows = sqlx::query(
        "SELECT runner_grant_json FROM inference_provider_attempts
        WHERE user_id = ? AND runner_id = ? AND status = 'started'
          AND (runner_dispatch_claim_expires_at IS NULL OR runner_dispatch_claim_expires_at <= NOW(6))
        ORDER BY runner_grant_expires_at, attempt_id LIMIT ?",
    )
    .bind(&connection.user_id)
    .bind(connection.runner_id.as_str())
    .bind(i64::from(limit.clamp(1, 128)))
    .fetch_all(&mut *tx)
    .await
    .map_err(persistence)?;
    rows.iter().map(decode_grant).collect()
}

#[derive(Clone, Debug)]
pub struct RunnerContinuationClaim {
    invocation: InferenceInvocationPlan,
    identity: RunnerInferenceAttemptIdentity,
    terminal_hash: RunnerInferenceDigest,
    physical_terminal: InferenceInvocationTerminal,
    pub response: RunnerInferenceArtifactReference,
}

impl RunnerContinuationClaim {
    pub fn invocation(&self) -> &InferenceInvocationPlan {
        &self.invocation
    }
    pub fn identity(&self) -> &RunnerInferenceAttemptIdentity {
        &self.identity
    }

    pub fn physical_terminal(&self) -> &InferenceInvocationTerminal {
        &self.physical_terminal
    }

    /// Produce the non-secret receipt that a canonical Agent Loop checkpoint
    /// may later bind to its post-response state.  This is deliberately not
    /// an acknowledgement: retention remains pending until run settlement.
    pub fn checkpoint_receipt(
        &self,
    ) -> astra_turn_types::runner_inference::RunnerInferenceContinuationReceipt {
        astra_turn_types::runner_inference::RunnerInferenceContinuationReceipt {
            attempt: self.identity.clone(),
            terminal_sha256: self.terminal_hash.clone(),
            response: self.response.clone(),
        }
    }
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

/// Claim the continuation through the existing inference owner lease. A live
/// owner can claim using its exact token; a recovered run owner must wait for
/// expiry. Run generation/control and personal session ownership are rechecked.
pub async fn claim_runner_continuation(
    pool: &SharedPool,
    input: InferenceInvocationInput,
    identity: &RunnerInferenceAttemptIdentity,
    live_invocation_owner_token: Option<&str>,
) -> ServiceResult<RunnerContinuationClaim> {
    if input.user_id != identity.user_id || input.scope != identity.scope {
        return Err(ServiceError::conflict("Runner continuation scope mismatch"));
    }
    let mut tx = pool.get().begin().await.map_err(persistence)?;
    if !matches!(
        lock_invocation_scope_authority(&mut tx, &input).await?,
        InvocationScopeAuthority::Live
    ) {
        return Err(ServiceError::conflict(
            "Runner continuation run authority unavailable",
        ));
    }
    let invocation = sqlx::query(
        "SELECT route_id, admission_token, owner_token, owner_generation, status,
        IF(owner_lease_expires_at > NOW(6), 1, 0) AS owner_live
        FROM inference_invocations WHERE user_id = ? AND invocation_id = ? FOR UPDATE",
    )
    .bind(&identity.user_id)
    .bind(identity.invocation_id.as_str())
    .fetch_optional(&mut *tx)
    .await
    .map_err(persistence)?
    .ok_or_else(|| ServiceError::not_found("Runner continuation invocation absent"))?;
    let old_token: String = invocation.try_get("owner_token").map_err(persistence)?;
    let owner_live = invocation
        .try_get::<i64, _>("owner_live")
        .map_err(persistence)?
        == 1;
    if invocation
        .try_get::<String, _>("status")
        .map_err(persistence)?
        != "admitted"
        || (owner_live && live_invocation_owner_token != Some(old_token.as_str()))
    {
        return Err(ServiceError::conflict(
            "Runner continuation inference owner is not available",
        ));
    }
    let row = sqlx::query("SELECT runner_grant_json, runner_terminal_hash, runner_response_artifact_id,
        runner_response_hash, runner_response_bytes, runner_continuation_pending, runner_terminal_conflict,
        status, terminal_fingerprint, usage_status, input_tokens, output_tokens,
        cache_read_tokens, cache_creation_tokens, provider_response_id, error_kind, error_message
        FROM inference_provider_attempts WHERE user_id = ? AND attempt_id = ? FOR UPDATE")
        .bind(&identity.user_id).bind(identity.attempt_id.as_str()).fetch_optional(&mut *tx).await.map_err(persistence)?
        .ok_or_else(|| ServiceError::not_found("Runner continuation attempt absent"))?;
    if decode_grant(&row)?.attempt != *identity
        || !row
            .try_get::<bool, _>("runner_continuation_pending")
            .map_err(persistence)?
        || row
            .try_get::<bool, _>("runner_terminal_conflict")
            .map_err(persistence)?
    {
        return Err(ServiceError::conflict(
            "Runner continuation is absent, consumed or quarantined",
        ));
    }
    let old_generation: i64 = invocation
        .try_get("owner_generation")
        .map_err(persistence)?;
    // The foreground owner is already the canonical continuation owner. Keep
    // its exact token/generation so the in-memory lease and the durable row do
    // not diverge after a normal Runner round. Only recovery after lease expiry
    // fences the abandoned owner with a fresh generation.
    let (token, generation) = if owner_live {
        (old_token, old_generation)
    } else {
        let generation = old_generation
            .checked_add(1)
            .ok_or_else(|| ServiceError::conflict("Runner inference owner generation exhausted"))?;
        let token = new_admission_token();
        sqlx::query(
            "UPDATE inference_invocations SET owner_token = ?, owner_generation = ?,
            owner_lease_expires_at = DATE_ADD(NOW(6), INTERVAL 60 SECOND)
            WHERE user_id = ? AND invocation_id = ?",
        )
        .bind(&token)
        .bind(generation)
        .bind(&identity.user_id)
        .bind(identity.invocation_id.as_str())
        .execute(&mut *tx)
        .await
        .map_err(persistence)?;
        (token, generation)
    };
    let physical_terminal =
        public_terminal(&DurableInferenceTerminal::decode(&row).map_err(persistence)?)?;
    let claim = RunnerContinuationClaim {
        invocation: InferenceInvocationPlan {
            route_id: invocation.try_get("route_id").map_err(persistence)?,
            invocation_id: identity.invocation_id.as_str().into(),
            admission_token: invocation.try_get("admission_token").map_err(persistence)?,
            owner_token: token,
            owner_generation: generation as u64,
            input,
        },
        identity: identity.clone(),
        terminal_hash: RunnerInferenceDigest::new(
            row.try_get::<String, _>("runner_terminal_hash")
                .map_err(persistence)?,
        )
        .map_err(ServiceError::invalid)?,
        physical_terminal,
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
            .ok_or_else(|| ServiceError::invalid("empty Runner response custody"))?,
        },
    };
    tx.commit().await.map_err(persistence)?;
    Ok(claim)
}

/// Settle the logical inference invocation from a durably custodied Runner
/// response. This deliberately does not acknowledge Agent Backbone
/// absorption: `runner_continuation_pending` remains set until the canonical
/// run transaction calls `acknowledge_runner_continuations_for_terminal_run_tx`.
/// Exact run/inference owners are revalidated, including cancellation.
pub async fn settle_runner_continuation_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    claim: &RunnerContinuationClaim,
    logical_terminal: &InferenceInvocationTerminal,
) -> ServiceResult<()> {
    if !matches!(
        lock_invocation_scope_authority(tx, &claim.invocation.input).await?,
        InvocationScopeAuthority::Live
    ) {
        return Err(ServiceError::conflict(
            "Runner continuation run authority changed",
        ));
    }
    lock_admitted_inference_invocation(
        tx,
        &claim.identity.user_id,
        claim.identity.invocation_id.as_str(),
        &claim.invocation.owner_token,
        claim.invocation.owner_generation,
        "consume Runner continuation",
    )
    .await?;
    let row = sqlx::query(
        "SELECT status, terminal_fingerprint, usage_status, input_tokens, output_tokens,
        cache_read_tokens, cache_creation_tokens, provider_response_id, error_kind, error_message
        FROM inference_provider_attempts WHERE user_id = ? AND attempt_id = ?
          AND runner_terminal_hash = ? AND runner_continuation_pending = TRUE
          AND runner_terminal_conflict = FALSE FOR UPDATE",
    )
    .bind(&claim.identity.user_id)
    .bind(claim.identity.attempt_id.as_str())
    .bind(claim.terminal_hash.as_str())
    .fetch_optional(&mut **tx)
    .await
    .map_err(persistence)?
    .ok_or_else(|| ServiceError::conflict("Runner continuation consumed or quarantined"))?;
    let physical = DurableInferenceTerminal::decode(&row).map_err(persistence)?;
    let logical = DurableInferenceTerminal::from_terminal(
        logical_terminal,
        terminal_fingerprint(logical_terminal)?,
    )?;
    if logical.input_tokens != physical.input_tokens
        || logical.output_tokens != physical.output_tokens
        || logical.cache_read_tokens != physical.cache_read_tokens
        || logical.cache_creation_tokens != physical.cache_creation_tokens
        || logical.usage_status != physical.usage_status
        || logical.provider_response_id != physical.provider_response_id
        || (logical.status == "succeeded" && physical.status != "succeeded")
    {
        return Err(ServiceError::invalid(
            "logical Runner outcome cannot rewrite physical usage or promote incomplete transport",
        ));
    }
    settle_runner_invocation_tx(tx, &claim.identity, &logical).await
}

async fn settle_runner_invocation_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    identity: &RunnerInferenceAttemptIdentity,
    terminal: &DurableInferenceTerminal,
) -> ServiceResult<()> {
    sqlx::query("UPDATE inference_invocations SET status = ?, terminal_fingerprint = ?, usage_status = ?,
        input_tokens = ?, output_tokens = ?, cache_read_tokens = ?, cache_creation_tokens = ?,
        provider_response_id = ?, error_kind = ?, error_message = ?, provider_delivery_state = 'delivery_authorized', terminal_at = NOW(6)
        WHERE user_id = ? AND invocation_id = ?")
        .bind(&terminal.status).bind(&terminal.terminal_fingerprint).bind(&terminal.usage_status)
        .bind(terminal.input_tokens).bind(terminal.output_tokens).bind(terminal.cache_read_tokens).bind(terminal.cache_creation_tokens)
        .bind(&terminal.provider_response_id).bind(&terminal.error_kind).bind(&terminal.error_message)
        .bind(&identity.user_id).bind(identity.invocation_id.as_str())
        .execute(&mut **tx).await.map_err(persistence)?;
    Ok(())
}

/// A Runner response remains a continuation obligation after its logical
/// inference invocation is settled. Only the transaction that makes the
/// complete run result durable may acknowledge that the Agent Backbone has
/// absorbed every response for this run.
pub async fn acknowledge_runner_continuations_for_terminal_run_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    user_id: &str,
    session_id: &str,
    run_id: &str,
    expected_run_generation: u64,
) -> ServiceResult<u64> {
    let row = sqlx::query(
        "SELECT status, run_generation FROM agent_runs
         WHERE user_id = ? AND session_id = ? AND run_id = ? FOR UPDATE",
    )
    .bind(user_id)
    .bind(session_id)
    .bind(run_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(persistence)?
    .ok_or_else(|| ServiceError::not_found("Runner continuation run absent"))?;
    let status: String = row.try_get("status").map_err(persistence)?;
    let generation: i64 = row.try_get("run_generation").map_err(persistence)?;
    let expected_generation = i64::try_from(expected_run_generation)
        .map_err(|_| ServiceError::invalid("Runner continuation run generation overflow"))?;
    if generation != expected_generation
        || !matches!(
            status.as_str(),
            "completed" | "delegated" | "failed" | "cancelled"
        )
    {
        return Err(ServiceError::conflict(
            "Runner continuations require the exact terminal run generation",
        ));
    }

    let result = sqlx::query(
        "UPDATE inference_provider_attempts
         SET runner_continuation_pending = FALSE
         WHERE user_id = ? AND session_id = ? AND run_id = ?
           AND runner_continuation_pending = TRUE
           AND runner_terminal_conflict = FALSE
           AND EXISTS (
             SELECT 1 FROM inference_invocations AS invocation
             WHERE invocation.user_id = inference_provider_attempts.user_id
               AND invocation.invocation_id = inference_provider_attempts.invocation_id
               AND invocation.status IN ('succeeded', 'failed', 'cancelled', 'delivery_unknown')
           )",
    )
    .bind(user_id)
    .bind(session_id)
    .bind(run_id)
    .execute(&mut **tx)
    .await
    .map_err(persistence)?;
    Ok(result.rows_affected())
}

pub async fn list_pending_runner_continuations(
    pool: &SharedPool,
    limit: u32,
) -> ServiceResult<Vec<RunnerInferenceAttemptIdentity>> {
    let rows = sqlx::query("SELECT runner_grant_json FROM (
        SELECT runner_grant_json, user_id, attempt_id, terminal_at,
            ROW_NUMBER() OVER (PARTITION BY user_id ORDER BY terminal_at, attempt_id) AS owner_rank
        FROM inference_provider_attempts WHERE runner_continuation_pending = TRUE AND runner_terminal_conflict = FALSE
        ) AS pending ORDER BY owner_rank, terminal_at, user_id, attempt_id LIMIT ?")
        .bind(i64::from(limit.clamp(1, 128))).fetch_all(pool.get()).await.map_err(persistence)?;
    rows.iter()
        .map(|row| decode_grant(row).map(|grant| grant.attempt))
        .collect()
}

/// A cancelled/terminal run retains actual response and usage without being
/// resumed. Only the exact pending continuation is retired, never run status.
pub async fn discard_cancelled_runner_continuation(
    pool: &SharedPool,
    authenticated_user_id: &str,
    identity: &RunnerInferenceAttemptIdentity,
) -> ServiceResult<()> {
    if authenticated_user_id != identity.user_id {
        return Err(ServiceError::conflict("Runner discard owner mismatch"));
    }
    let session = identity
        .scope
        .session_id()
        .ok_or_else(|| ServiceError::invalid("Runner discard requires session scope"))?;
    let mut tx = pool.get().begin().await.map_err(persistence)?;
    crate::storage::admit_session_event_write(&mut tx, session, authenticated_user_id, false)
        .await
        .map_err(persistence)?;
    let run_allows_discard = if let Some(run_id) = identity.scope.run_id() {
        let row = sqlx::query(
            "SELECT status,
            IF(cancellation_requested_at IS NOT NULL, 1, 0) AS cancelled
            FROM agent_runs WHERE user_id = ? AND session_id = ? AND run_id = ? FOR UPDATE",
        )
        .bind(authenticated_user_id)
        .bind(session)
        .bind(run_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(persistence)?
        .ok_or_else(|| ServiceError::not_found("Runner continuation run absent"))?;
        let status: String = row.try_get("status").map_err(persistence)?;
        matches!(
            status.as_str(),
            "completed" | "delegated" | "failed" | "cancelled"
        ) || row.try_get::<i64, _>("cancelled").map_err(persistence)? == 1
    } else {
        false
    };
    let invocation = sqlx::query("SELECT status FROM inference_invocations WHERE user_id = ? AND invocation_id = ? FOR UPDATE")
        .bind(authenticated_user_id).bind(identity.invocation_id.as_str()).fetch_optional(&mut *tx).await.map_err(persistence)?
        .ok_or_else(|| ServiceError::not_found("Runner continuation invocation absent"))?;
    let invocation_status: String = invocation.try_get("status").map_err(persistence)?;
    let row = sqlx::query(
        "SELECT runner_grant_json, runner_continuation_pending, runner_terminal_conflict,
        IF(runner_cancel_requested_at IS NOT NULL, 1, 0) AS cancelled,
        status, terminal_fingerprint, usage_status, input_tokens, output_tokens, cache_read_tokens,
        cache_creation_tokens, provider_response_id, error_kind, error_message
        FROM inference_provider_attempts WHERE user_id = ? AND attempt_id = ? FOR UPDATE",
    )
    .bind(authenticated_user_id)
    .bind(identity.attempt_id.as_str())
    .fetch_optional(&mut *tx)
    .await
    .map_err(persistence)?
    .ok_or_else(|| ServiceError::not_found("Runner continuation attempt absent"))?;
    if decode_grant(&row)?.attempt != *identity
        || row
            .try_get::<bool, _>("runner_terminal_conflict")
            .map_err(persistence)?
    {
        return Err(ServiceError::conflict(
            "Runner discard identity mismatch or terminal quarantined",
        ));
    }
    if !run_allows_discard && row.try_get::<i64, _>("cancelled").map_err(persistence)? != 1 {
        return Err(ServiceError::conflict(
            "live Runner continuation cannot be discarded",
        ));
    }
    if !row
        .try_get::<bool, _>("runner_continuation_pending")
        .map_err(persistence)?
    {
        return Ok(());
    }
    let terminal = DurableInferenceTerminal::decode(&row).map_err(persistence)?;
    match invocation_status.as_str() {
        "admitted" => settle_runner_invocation_tx(&mut tx, identity, &terminal).await?,
        "succeeded" | "failed" | "cancelled" | "delivery_unknown" => {}
        _ => {
            return Err(ServiceError::conflict(
                "Runner discard found an invalid logical invocation state",
            ));
        }
    }
    sqlx::query(
        "UPDATE inference_provider_attempts SET runner_continuation_pending = FALSE
         WHERE user_id = ? AND attempt_id = ?",
    )
    .bind(authenticated_user_id)
    .bind(identity.attempt_id.as_str())
    .execute(&mut *tx)
    .await
    .map_err(persistence)?;
    tx.commit().await.map_err(persistence)
}
