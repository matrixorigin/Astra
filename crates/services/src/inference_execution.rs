use astra_core::SharedPool;
use astra_turn_types::{InferenceInvocationScope, InferencePurpose};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::Row;

use crate::models::{ModelAccessKind, ModelExecutionPlacement, validate_model_offering_id};
use crate::service_error::{ServiceError, ServiceErrorKind, ServiceResult};

const INFERENCE_ID_HEX_LEN: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InferenceInvocationInput {
    pub user_id: String,
    pub scope: InferenceInvocationScope,
    pub offering_id: String,
    pub resolved_model_name: String,
    pub upstream_model_name: String,
    pub provider: String,
    pub purpose: InferencePurpose,
    pub execution_placement: ModelExecutionPlacement,
    pub access_kind: ModelAccessKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InferenceInvocationPlan {
    route_id: String,
    invocation_id: String,
    input: InferenceInvocationInput,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InferenceProviderAttemptPlan {
    attempt_id: String,
    invocation_id: String,
    user_id: String,
    attempt_index: u32,
    provider: String,
}

impl InferenceInvocationPlan {
    #[must_use]
    pub fn route_id(&self) -> &str {
        &self.route_id
    }

    #[must_use]
    pub fn invocation_id(&self) -> &str {
        &self.invocation_id
    }
}

impl InferenceProviderAttemptPlan {
    #[must_use]
    pub fn attempt_id(&self) -> &str {
        &self.attempt_id
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferenceTerminalStatus {
    Succeeded,
    Failed,
    Cancelled,
    DeliveryUnknown,
}

impl InferenceTerminalStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::DeliveryUnknown => "delivery_unknown",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InferenceUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InferenceInvocationTerminal {
    pub status: InferenceTerminalStatus,
    pub usage: InferenceUsage,
    pub provider_response_id: Option<String>,
    pub error_kind: Option<String>,
    pub error_message: Option<String>,
}

impl InferenceInvocationTerminal {
    #[must_use]
    pub fn succeeded(usage: InferenceUsage, provider_response_id: Option<String>) -> Self {
        Self {
            status: InferenceTerminalStatus::Succeeded,
            usage,
            provider_response_id,
            error_kind: None,
            error_message: None,
        }
    }
}

fn validate_identity(value: &str, label: &str, max_bytes: usize) -> ServiceResult<()> {
    if value.is_empty()
        || value.len() > max_bytes
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(ServiceError::invalid(format!(
            "{label} must be an exact non-empty identifier of at most {max_bytes} bytes"
        )));
    }
    Ok(())
}

fn hash_identity(namespace: &str, fields: &[&str]) -> String {
    let mut hasher = Sha256::new();
    hasher.update((namespace.len() as u64).to_be_bytes());
    hasher.update(namespace.as_bytes());
    for field in fields {
        hasher.update((field.len() as u64).to_be_bytes());
        hasher.update(field.as_bytes());
    }
    let digest = format!("{:x}", hasher.finalize());
    format!("{namespace}-{}", &digest[..INFERENCE_ID_HEX_LEN])
}

pub fn plan_inference_invocation(
    input: InferenceInvocationInput,
) -> ServiceResult<InferenceInvocationPlan> {
    validate_identity(&input.user_id, "user_id", 128)?;
    if let Some(session_id) = input.scope.session_id() {
        validate_identity(session_id, "session_id", 64)?;
    }
    if let Some(run_id) = input.scope.run_id() {
        validate_identity(run_id, "run_id", 64)?;
    }
    if let Some(harness_run_id) = input.scope.harness_run_id() {
        validate_identity(harness_run_id, "harness_run_id", 128)?;
    }
    validate_identity(input.scope.operation_id(), "operation_id", 64)?;
    validate_model_offering_id(&input.offering_id)
        .map_err(|error| ServiceError::invalid(error.to_string()))?;
    validate_identity(&input.resolved_model_name, "resolved_model_name", 255)?;
    validate_identity(&input.upstream_model_name, "upstream_model_name", 255)?;
    validate_identity(&input.provider, "provider", 64)?;

    let turn = input.scope.turn().map(|value| value.to_string());
    let round = input.scope.round().map(|value| value.to_string());
    let logical_attempt = input.scope.logical_attempt().to_string();
    let purpose = input.purpose.as_str();
    let placement = input.execution_placement.as_str();
    let access_kind = input.access_kind.as_str();
    let identity_fields = [
        input.user_id.as_str(),
        input.scope.kind(),
        input.scope.session_id().unwrap_or(""),
        input.scope.run_id().unwrap_or(""),
        input.scope.harness_run_id().unwrap_or(""),
        turn.as_deref().unwrap_or(""),
        round.as_deref().unwrap_or(""),
        input.scope.operation_id(),
        logical_attempt.as_str(),
        input.offering_id.as_str(),
        input.resolved_model_name.as_str(),
        input.upstream_model_name.as_str(),
        input.provider.as_str(),
        purpose,
        placement,
        access_kind,
    ];
    let invocation_id = hash_identity("inv", &identity_fields);
    let route_id = hash_identity("route", &[invocation_id.as_str()]);
    Ok(InferenceInvocationPlan {
        route_id,
        invocation_id,
        input,
    })
}

#[must_use]
pub fn plan_inference_provider_attempt(
    invocation: &InferenceInvocationPlan,
    attempt_index: u32,
) -> InferenceProviderAttemptPlan {
    let attempt_index_text = attempt_index.to_string();
    InferenceProviderAttemptPlan {
        attempt_id: hash_identity(
            "attempt",
            &[
                invocation.invocation_id.as_str(),
                attempt_index_text.as_str(),
            ],
        ),
        invocation_id: invocation.invocation_id.clone(),
        user_id: invocation.input.user_id.clone(),
        attempt_index,
        provider: invocation.input.provider.clone(),
    }
}

fn checked_i64(value: u64, label: &str) -> ServiceResult<i64> {
    i64::try_from(value)
        .map_err(|_| ServiceError::invalid(format!("{label} exceeds the durable BIGINT range")))
}

fn terminal_fingerprint(terminal: &InferenceInvocationTerminal) -> ServiceResult<String> {
    let payload = serde_json::to_vec(&serde_json::json!({
        "status": terminal.status,
        "usage": terminal.usage,
        "provider_response_id": terminal.provider_response_id,
        "error_kind": terminal.error_kind,
        "error_message": terminal.error_message,
    }))
    .map_err(|error| {
        ServiceError::with_source(
            ServiceErrorKind::Internal,
            "serialize inference terminal fingerprint",
            error,
        )
    })?;
    Ok(format!("{:x}", Sha256::digest(payload)))
}

async fn rollback_inference_tx(tx: sqlx::Transaction<'_, sqlx::MySql>, operation: &'static str) {
    if let Err(error) = tx.rollback().await {
        tracing::warn!(
            target: "astra_services::inference_execution",
            operation,
            %error,
            "inference transaction rollback failed"
        );
    }
}

async fn ensure_invocation_scope(
    connection: &mut sqlx::MySqlConnection,
    input: &InferenceInvocationInput,
) -> ServiceResult<()> {
    let query = match &input.scope {
        InferenceInvocationScope::Run {
            session_id, run_id, ..
        } => sqlx::query(
            "SELECT 1 FROM agent_runs
             WHERE user_id = ? AND session_id = ? AND run_id = ? LIMIT 1 FOR UPDATE",
        )
        .bind(&input.user_id)
        .bind(session_id)
        .bind(run_id),
        InferenceInvocationScope::Session { session_id, .. } => sqlx::query(
            "SELECT 1 FROM agent_sessions
             WHERE user_id = ? AND session_id = ? LIMIT 1 FOR UPDATE",
        )
        .bind(&input.user_id)
        .bind(session_id),
        InferenceInvocationScope::HarnessRun { harness_run_id, .. } => sqlx::query(
            "SELECT 1 FROM harness_runs
             WHERE user_id = ? AND harness_run_id = ? LIMIT 1 FOR UPDATE",
        )
        .bind(&input.user_id)
        .bind(harness_run_id),
    };
    let exists = query
        .fetch_optional(&mut *connection)
        .await
        .map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "verify inference owner scope",
                error,
            )
        })?
        .is_some();
    if !exists {
        return Err(ServiceError::not_found(format!(
            "inference {} scope does not exist for user_id={} owner_id={}",
            input.scope.kind(),
            input.user_id,
            input
                .scope
                .run_id()
                .or_else(|| input.scope.session_id())
                .or_else(|| input.scope.harness_run_id())
                .unwrap_or("none")
        )));
    }
    Ok(())
}

async fn existing_invocation_status(
    db: &sqlx::Pool<sqlx::MySql>,
    plan: &InferenceInvocationPlan,
) -> ServiceResult<Option<String>> {
    sqlx::query(
        "SELECT status FROM inference_invocations WHERE user_id = ? AND invocation_id = ? LIMIT 1",
    )
    .bind(&plan.input.user_id)
    .bind(&plan.invocation_id)
    .fetch_optional(db)
    .await
    .map_err(|error| {
        ServiceError::with_source(
            ServiceErrorKind::Persistence,
            "load existing inference invocation",
            error,
        )
    })?
    .map(|row| {
        row.try_get::<String, _>("status").map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "decode existing inference invocation status",
                error,
            )
        })
    })
    .transpose()
}

fn existing_invocation_error(plan: &InferenceInvocationPlan, status: &str) -> ServiceError {
    ServiceError::conflict(format!(
        "inference invocation {} already exists with status {status}; provider delivery must not be repeated",
        plan.invocation_id
    ))
}

/// Durably admit one logical inference.
///
/// Success is the caller's permission to contact the provider. Replaying the
/// same logical identity is rejected even when its status is still `admitted`:
/// after a process crash, provider delivery is unknown and blind retry would
/// violate the billing and transcript boundary.
pub async fn admit_inference_invocation(
    pool: &SharedPool,
    plan: &InferenceInvocationPlan,
) -> ServiceResult<()> {
    let db = pool.get();
    if let Some(status) = existing_invocation_status(db, plan).await? {
        return Err(existing_invocation_error(plan, &status));
    }

    let mut tx = db.begin().await.map_err(|error| {
        ServiceError::with_source(
            ServiceErrorKind::Persistence,
            "begin inference admission",
            error,
        )
    })?;
    let write_result: ServiceResult<()> = async {
        ensure_invocation_scope(&mut tx, &plan.input).await?;
        sqlx::query(
            "INSERT INTO inference_routes
             (route_id, user_id, session_id, scope_kind, run_id, harness_run_id,
              offering_id, resolved_model_name,
              upstream_model_name, provider, execution_placement, access_kind, purpose, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW(6))",
        )
        .bind(&plan.route_id)
        .bind(&plan.input.user_id)
        .bind(plan.input.scope.session_id())
        .bind(plan.input.scope.kind())
        .bind(plan.input.scope.run_id())
        .bind(plan.input.scope.harness_run_id())
        .bind(&plan.input.offering_id)
        .bind(&plan.input.resolved_model_name)
        .bind(&plan.input.upstream_model_name)
        .bind(&plan.input.provider)
        .bind(plan.input.execution_placement.as_str())
        .bind(plan.input.access_kind.as_str())
        .bind(plan.input.purpose.as_str())
        .execute(&mut *tx)
        .await
        .map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "insert inference route",
                error,
            )
        })?;

        sqlx::query(
            "INSERT INTO inference_invocations
             (invocation_id, route_id, user_id, session_id, scope_kind, run_id, harness_run_id,
              turn_index,
              round_index, operation_id, logical_attempt, purpose, status, created_at, terminal_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'admitted', NOW(6), NULL)",
        )
        .bind(&plan.invocation_id)
        .bind(&plan.route_id)
        .bind(&plan.input.user_id)
        .bind(plan.input.scope.session_id())
        .bind(plan.input.scope.kind())
        .bind(plan.input.scope.run_id())
        .bind(plan.input.scope.harness_run_id())
        .bind(plan.input.scope.turn().map(i64::from))
        .bind(plan.input.scope.round().map(i64::from))
        .bind(plan.input.scope.operation_id())
        .bind(i64::from(plan.input.scope.logical_attempt()))
        .bind(plan.input.purpose.as_str())
        .execute(&mut *tx)
        .await
        .map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "insert inference invocation",
                error,
            )
        })?;

        Ok(())
    }
    .await;

    if let Err(error) = write_result {
        rollback_inference_tx(tx, "admit_inference_invocation").await;
        match existing_invocation_status(db, plan).await {
            Ok(Some(status)) => return Err(existing_invocation_error(plan, &status)),
            Ok(None) => return Err(error),
            Err(status_err) => {
                tracing::error!(
                    %error,
                    %status_err,
                    "admission write failed and existing-status re-check also failed"
                );
                return Err(error);
            }
        }
    }
    tx.commit().await.map_err(|error| {
        ServiceError::with_source(
            ServiceErrorKind::Persistence,
            "commit inference admission",
            error,
        )
    })?;
    Ok(())
}

async fn existing_provider_attempt_status(
    db: &sqlx::Pool<sqlx::MySql>,
    attempt: &InferenceProviderAttemptPlan,
) -> ServiceResult<Option<String>> {
    sqlx::query(
        "SELECT status FROM inference_provider_attempts
         WHERE user_id = ? AND attempt_id = ? LIMIT 1",
    )
    .bind(&attempt.user_id)
    .bind(&attempt.attempt_id)
    .fetch_optional(db)
    .await
    .map_err(|error| {
        ServiceError::with_source(
            ServiceErrorKind::Persistence,
            "load existing inference provider attempt",
            error,
        )
    })?
    .map(|row| {
        row.try_get::<String, _>("status").map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "decode inference provider attempt status",
                error,
            )
        })
    })
    .transpose()
}

/// Persist one physical provider request before network I/O.
pub async fn begin_inference_provider_attempt(
    pool: &SharedPool,
    attempt: &InferenceProviderAttemptPlan,
) -> ServiceResult<()> {
    let db = pool.get();
    if let Some(status) = existing_provider_attempt_status(db, attempt).await? {
        return Err(ServiceError::conflict(format!(
            "inference provider attempt {} already exists with status {status}; provider delivery must not be repeated",
            attempt.attempt_id
        )));
    }
    let result = sqlx::query(
        "INSERT INTO inference_provider_attempts
         (attempt_id, invocation_id, user_id, session_id, run_id, harness_run_id, attempt_index,
          provider, status, started_at, terminal_at)
         SELECT ?, invocation_id, user_id, session_id, run_id, harness_run_id,
                ?, ?, 'started', NOW(6), NULL
         FROM inference_invocations
         WHERE user_id = ? AND invocation_id = ? AND status = 'admitted'",
    )
    .bind(&attempt.attempt_id)
    .bind(i64::from(attempt.attempt_index))
    .bind(&attempt.provider)
    .bind(&attempt.user_id)
    .bind(&attempt.invocation_id)
    .execute(db)
    .await;
    match result {
        Ok(result) if result.rows_affected() == 1 => Ok(()),
        Ok(_) => Err(ServiceError::conflict(format!(
            "inference invocation {} is not admitted for provider attempt {}",
            attempt.invocation_id, attempt.attempt_id
        ))),
        Err(error) => {
            if let Some(status) = existing_provider_attempt_status(db, attempt).await? {
                Err(ServiceError::conflict(format!(
                    "inference provider attempt {} already exists with status {status}; provider delivery must not be repeated",
                    attempt.attempt_id
                )))
            } else {
                Err(ServiceError::with_source(
                    ServiceErrorKind::Persistence,
                    "insert inference provider attempt",
                    error,
                ))
            }
        }
    }
}

async fn existing_provider_attempt_fingerprint(
    db: &sqlx::Pool<sqlx::MySql>,
    attempt: &InferenceProviderAttemptPlan,
) -> ServiceResult<Option<String>> {
    sqlx::query(
        "SELECT terminal_fingerprint FROM inference_provider_attempts
         WHERE user_id = ? AND attempt_id = ? LIMIT 1",
    )
    .bind(&attempt.user_id)
    .bind(&attempt.attempt_id)
    .fetch_optional(db)
    .await
    .map_err(|error| {
        ServiceError::with_source(
            ServiceErrorKind::Persistence,
            "load inference provider attempt fingerprint",
            error,
        )
    })?
    .map(|row| {
        row.try_get::<Option<String>, _>("terminal_fingerprint")
            .map_err(|error| {
                ServiceError::with_source(
                    ServiceErrorKind::Persistence,
                    "decode inference provider attempt fingerprint",
                    error,
                )
            })
    })
    .transpose()
    .map(Option::flatten)
}

pub async fn finish_inference_provider_attempt(
    pool: &SharedPool,
    attempt: &InferenceProviderAttemptPlan,
    terminal: &InferenceInvocationTerminal,
) -> ServiceResult<()> {
    let fingerprint = terminal_fingerprint(terminal)?;
    let input_tokens = checked_i64(terminal.usage.input_tokens, "provider input_tokens")?;
    let output_tokens = checked_i64(terminal.usage.output_tokens, "provider output_tokens")?;
    let cache_read_tokens = checked_i64(
        terminal.usage.cache_read_tokens,
        "provider cache_read_tokens",
    )?;
    let cache_creation_tokens = checked_i64(
        terminal.usage.cache_creation_tokens,
        "provider cache_creation_tokens",
    )?;
    let db = pool.get();
    if let Some(existing) = existing_provider_attempt_fingerprint(db, attempt).await? {
        return if existing == fingerprint {
            Ok(())
        } else {
            Err(ServiceError::conflict(format!(
                "inference provider attempt {} terminal payload conflicts with its durable result",
                attempt.attempt_id
            )))
        };
    }
    let result = sqlx::query(
        "UPDATE inference_provider_attempts
         SET status = ?, terminal_fingerprint = ?, provider_response_id = ?,
             input_tokens = ?, output_tokens = ?, cache_read_tokens = ?,
             cache_creation_tokens = ?, error_kind = ?, error_message = ?, terminal_at = NOW(6)
         WHERE user_id = ? AND attempt_id = ? AND status = 'started'",
    )
    .bind(terminal.status.as_str())
    .bind(&fingerprint)
    .bind(&terminal.provider_response_id)
    .bind(input_tokens)
    .bind(output_tokens)
    .bind(cache_read_tokens)
    .bind(cache_creation_tokens)
    .bind(&terminal.error_kind)
    .bind(&terminal.error_message)
    .bind(&attempt.user_id)
    .bind(&attempt.attempt_id)
    .execute(db)
    .await
    .map_err(|error| {
        ServiceError::with_source(
            ServiceErrorKind::Persistence,
            "finish inference provider attempt",
            error,
        )
    })?;
    if result.rows_affected() == 1 {
        return Ok(());
    }
    if let Some(existing) = existing_provider_attempt_fingerprint(db, attempt).await? {
        return if existing == fingerprint {
            Ok(())
        } else {
            Err(ServiceError::conflict(format!(
                "inference provider attempt {} terminal payload conflicts with its durable result",
                attempt.attempt_id
            )))
        };
    }
    Err(ServiceError::conflict(format!(
        "inference provider attempt {} is not in started state",
        attempt.attempt_id
    )))
}

async fn existing_terminal_fingerprint(
    db: &sqlx::Pool<sqlx::MySql>,
    plan: &InferenceInvocationPlan,
) -> ServiceResult<Option<String>> {
    sqlx::query(
        "SELECT terminal_fingerprint FROM inference_invocations
         WHERE user_id = ? AND invocation_id = ? LIMIT 1",
    )
    .bind(&plan.input.user_id)
    .bind(&plan.invocation_id)
    .fetch_optional(db)
    .await
    .map_err(|error| {
        ServiceError::with_source(
            ServiceErrorKind::Persistence,
            "load inference terminal fingerprint",
            error,
        )
    })?
    .map(|row| {
        row.try_get::<Option<String>, _>("terminal_fingerprint")
            .map_err(|error| {
                ServiceError::with_source(
                    ServiceErrorKind::Persistence,
                    "decode inference terminal fingerprint",
                    error,
                )
            })
    })
    .transpose()
    .map(Option::flatten)
}

/// Commit the logical invocation terminal state after its physical attempts.
/// Repeating the exact terminal payload is idempotent; a different payload for
/// the same invocation is a contract conflict. A successful logical result must
/// exactly match a durably successful physical attempt.
pub async fn finish_inference_invocation(
    pool: &SharedPool,
    plan: &InferenceInvocationPlan,
    terminal: &InferenceInvocationTerminal,
) -> ServiceResult<()> {
    let fingerprint = terminal_fingerprint(terminal)?;
    let input_tokens = checked_i64(terminal.usage.input_tokens, "input_tokens")?;
    let output_tokens = checked_i64(terminal.usage.output_tokens, "output_tokens")?;
    let cache_read_tokens = checked_i64(terminal.usage.cache_read_tokens, "cache_read_tokens")?;
    let cache_creation_tokens = checked_i64(
        terminal.usage.cache_creation_tokens,
        "cache_creation_tokens",
    )?;
    let db = pool.get();
    if let Some(existing) = existing_terminal_fingerprint(db, plan).await? {
        return if existing == fingerprint {
            Ok(())
        } else {
            Err(ServiceError::conflict(format!(
                "inference invocation {} terminal payload conflicts with its durable result",
                plan.invocation_id
            )))
        };
    }
    if terminal.status == InferenceTerminalStatus::Succeeded {
        let has_succeeded_attempt = sqlx::query(
            "SELECT 1 FROM inference_provider_attempts
             WHERE user_id = ? AND invocation_id = ? AND status = 'succeeded'
               AND terminal_fingerprint = ?
             LIMIT 1",
        )
        .bind(&plan.input.user_id)
        .bind(&plan.invocation_id)
        .bind(&fingerprint)
        .fetch_optional(db)
        .await
        .map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "verify successful inference provider attempt",
                error,
            )
        })?
        .is_some();
        if !has_succeeded_attempt {
            return Err(ServiceError::conflict(format!(
                "inference invocation {} cannot succeed without a matching succeeded provider attempt",
                plan.invocation_id
            )));
        }
    }

    let mut tx = db.begin().await.map_err(|error| {
        ServiceError::with_source(
            ServiceErrorKind::Persistence,
            "begin inference terminal commit",
            error,
        )
    })?;
    let write_result: ServiceResult<()> = async {
        let invocation = sqlx::query(
            "UPDATE inference_invocations
             SET status = ?, terminal_fingerprint = ?, input_tokens = ?, output_tokens = ?,
                 cache_read_tokens = ?, cache_creation_tokens = ?, provider_response_id = ?,
                 error_kind = ?, error_message = ?, terminal_at = NOW(6)
             WHERE user_id = ? AND invocation_id = ? AND status = 'admitted'",
        )
        .bind(terminal.status.as_str())
        .bind(&fingerprint)
        .bind(input_tokens)
        .bind(output_tokens)
        .bind(cache_read_tokens)
        .bind(cache_creation_tokens)
        .bind(&terminal.provider_response_id)
        .bind(&terminal.error_kind)
        .bind(&terminal.error_message)
        .bind(&plan.input.user_id)
        .bind(&plan.invocation_id)
        .execute(&mut *tx)
        .await
        .map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "finish inference invocation",
                error,
            )
        })?;
        if invocation.rows_affected() != 1 {
            return Err(ServiceError::conflict(format!(
                "inference invocation {} is not in admitted state",
                plan.invocation_id
            )));
        }
        Ok(())
    }
    .await;

    if let Err(error) = write_result {
        rollback_inference_tx(tx, "finish_inference_invocation").await;
        if let Some(existing) = existing_terminal_fingerprint(db, plan).await? {
            return if existing == fingerprint {
                Ok(())
            } else {
                Err(ServiceError::conflict(format!(
                    "inference invocation {} terminal payload conflicts with its durable result",
                    plan.invocation_id
                )))
            };
        }
        return Err(error);
    }
    tx.commit().await.map_err(|error| {
        ServiceError::with_source(
            ServiceErrorKind::Persistence,
            "commit inference terminal state",
            error,
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> InferenceInvocationInput {
        InferenceInvocationInput {
            user_id: "user-1".to_string(),
            scope: InferenceInvocationScope::Run {
                session_id: "session-1".to_string(),
                run_id: "run-1".to_string(),
                turn: 3,
                round: 2,
                operation_id: "agent_turn".to_string(),
                logical_attempt: 0,
            },
            offering_id: "offer-1".to_string(),
            resolved_model_name: "model-1".to_string(),
            upstream_model_name: "provider-model-1".to_string(),
            provider: "openai".to_string(),
            purpose: InferencePurpose::PrimaryAgent,
            execution_placement: ModelExecutionPlacement::Server,
            access_kind: ModelAccessKind::SelfHosted,
        }
    }

    #[test]
    fn invocation_identity_is_stable_and_every_execution_fact_is_bound() {
        let first = plan_inference_invocation(input()).expect("first plan");
        let second = plan_inference_invocation(input()).expect("second plan");
        assert_eq!(first, second);

        let mut changed = input();
        changed.purpose = InferencePurpose::SubAgent;
        assert_ne!(
            first.invocation_id,
            plan_inference_invocation(changed)
                .expect("changed plan")
                .invocation_id
        );
    }

    #[test]
    fn invocation_identity_rejects_ambiguous_runtime_scope() {
        let mut empty_session = input();
        empty_session.scope = InferenceInvocationScope::Session {
            session_id: String::new(),
            turn: 3,
            round: 2,
            operation_id: "memory_extraction".to_string(),
            logical_attempt: 0,
        };
        assert!(plan_inference_invocation(empty_session).is_err());

        let mut ambiguous_run = input();
        ambiguous_run.scope = InferenceInvocationScope::Run {
            session_id: "session-1".to_string(),
            run_id: " run-1".to_string(),
            turn: 3,
            round: 2,
            operation_id: "agent_turn".to_string(),
            logical_attempt: 0,
        };
        assert!(plan_inference_invocation(ambiguous_run).is_err());

        let mut invalid_provider = input();
        invalid_provider.provider = "open\nAI".to_string();
        assert!(plan_inference_invocation(invalid_provider).is_err());
    }

    #[test]
    fn invocation_identity_distinguishes_every_owner_kind() {
        let run = plan_inference_invocation(input()).expect("run plan");
        let mut session_input = input();
        session_input.scope = InferenceInvocationScope::Session {
            session_id: "session-1".to_string(),
            turn: 3,
            round: 2,
            operation_id: "agent_turn".to_string(),
            logical_attempt: 0,
        };
        let session = plan_inference_invocation(session_input).expect("session plan");
        let mut harness_input = input();
        harness_input.scope = InferenceInvocationScope::HarnessRun {
            harness_run_id: "harness-run-1".to_string(),
            operation_id: "skillify_extract".to_string(),
            logical_attempt: 0,
        };
        harness_input.purpose = InferencePurpose::SkillSynthesis;
        let harness = plan_inference_invocation(harness_input).expect("harness plan");

        assert_ne!(run.invocation_id(), session.invocation_id());
        assert_ne!(run.invocation_id(), harness.invocation_id());
        assert_ne!(session.invocation_id(), harness.invocation_id());
    }

    #[test]
    fn terminal_fingerprint_is_idempotent_but_usage_sensitive() {
        let terminal = InferenceInvocationTerminal::succeeded(
            InferenceUsage {
                input_tokens: 10,
                output_tokens: 3,
                cache_read_tokens: 4,
                cache_creation_tokens: 0,
            },
            Some("provider-response".to_string()),
        );
        assert_eq!(
            terminal_fingerprint(&terminal).expect("first fingerprint"),
            terminal_fingerprint(&terminal).expect("second fingerprint")
        );
        let mut changed = terminal;
        changed.usage.output_tokens = 4;
        assert_ne!(
            terminal_fingerprint(&changed).expect("changed fingerprint"),
            terminal_fingerprint(&InferenceInvocationTerminal::succeeded(
                InferenceUsage {
                    input_tokens: 10,
                    output_tokens: 3,
                    cache_read_tokens: 4,
                    cache_creation_tokens: 0,
                },
                Some("provider-response".to_string()),
            ))
            .expect("original fingerprint")
        );
    }
}
