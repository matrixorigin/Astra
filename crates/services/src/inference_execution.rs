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
    /// Non-secret identity of the server-managed credential owner. The route
    /// stores this Offering id for audit; provider credentials are never copied
    /// into invocation records.
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

/// Serialize the two mutually exclusive lifecycle decisions for one logical
/// invocation: opening another physical attempt, or publishing its final
/// settlement. The lock is deliberately on the immutable invocation identity,
/// not on a mutable status index.
async fn lock_admitted_inference_invocation(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    user_id: &str,
    invocation_id: &str,
    action: &'static str,
) -> ServiceResult<()> {
    let status = sqlx::query(
        "SELECT status FROM inference_invocations
         WHERE user_id = ? AND invocation_id = ?
         FOR UPDATE",
    )
    .bind(user_id)
    .bind(invocation_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|error| {
        ServiceError::with_source(
            ServiceErrorKind::Persistence,
            "lock inference invocation lifecycle",
            error,
        )
    })?
    .map(|row| row.try_get::<String, _>("status"))
    .transpose()
    .map_err(|error| {
        ServiceError::with_source(
            ServiceErrorKind::Persistence,
            "decode locked inference invocation status",
            error,
        )
    })?;
    match status.as_deref() {
        Some("admitted") => Ok(()),
        Some(status) => Err(ServiceError::conflict(format!(
            "inference invocation {invocation_id} is {status}; cannot {action}"
        ))),
        None => Err(ServiceError::conflict(format!(
            "inference invocation {invocation_id} is unavailable; cannot {action}"
        ))),
    }
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
    let mut tx = db.begin().await.map_err(|error| {
        ServiceError::with_source(
            ServiceErrorKind::Persistence,
            "begin inference provider attempt admission",
            error,
        )
    })?;
    lock_admitted_inference_invocation(
        &mut tx,
        &attempt.user_id,
        &attempt.invocation_id,
        "begin a provider attempt",
    )
    .await?;
    let settlement_pending = sqlx::query(
        "SELECT 1 FROM inference_invocation_settlement_debts
         WHERE user_id = ? AND invocation_id = ?
         LIMIT 1",
    )
    .bind(&attempt.user_id)
    .bind(&attempt.invocation_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|error| {
        ServiceError::with_source(
            ServiceErrorKind::Persistence,
            "check inference settlement before provider attempt",
            error,
        )
    })?
    .is_some();
    if settlement_pending {
        return Err(ServiceError::conflict(format!(
            "inference invocation {} has a durable settlement decision; provider delivery must not be repeated",
            attempt.invocation_id
        )));
    }
    let result = sqlx::query(
        "INSERT INTO inference_provider_attempts
         (attempt_id, invocation_id, user_id, session_id, run_id, harness_run_id, attempt_index,
          provider, status, started_at, terminal_at)
         SELECT ?, invocation_id, user_id, session_id, run_id, harness_run_id,
                ?, ?, 'started', NOW(6), NULL
         FROM inference_invocations
         WHERE user_id = ? AND invocation_id = ? AND status = 'admitted'
           AND NOT EXISTS (
                SELECT 1
                FROM inference_invocation_settlement_debts AS settlement_debt
                WHERE settlement_debt.user_id = inference_invocations.user_id
                  AND settlement_debt.invocation_id = inference_invocations.invocation_id
           )",
    )
    .bind(&attempt.attempt_id)
    .bind(i64::from(attempt.attempt_index))
    .bind(&attempt.provider)
    .bind(&attempt.user_id)
    .bind(&attempt.invocation_id)
    .execute(&mut *tx)
    .await;
    match result {
        Ok(result) if result.rows_affected() == 1 => tx.commit().await.map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "commit inference provider attempt admission",
                error,
            )
        }),
        Ok(_) => Err(ServiceError::conflict(format!(
            "inference invocation {} is not admitted for provider attempt {}",
            attempt.invocation_id, attempt.attempt_id
        ))),
        Err(error) => {
            rollback_inference_tx(tx, "begin_inference_provider_attempt").await;
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
    let terminal_state = DurableInferenceTerminal::from_terminal(terminal, fingerprint.clone())?;
    let db = pool.get();
    if let Some(existing) = existing_provider_attempt_fingerprint(db, attempt).await? {
        return if existing == fingerprint {
            if terminal.status == InferenceTerminalStatus::Succeeded {
                record_inference_settlement_debt(
                    db,
                    &attempt.user_id,
                    &attempt.invocation_id,
                    &terminal_state,
                    SettlementDebtMode::RequireQuiescent,
                )
                .await?;
            }
            Ok(())
        } else {
            Err(ServiceError::conflict(format!(
                "inference provider attempt {} terminal payload conflicts with its durable result",
                attempt.attempt_id
            )))
        };
    }
    let update = sqlx::query(
        "UPDATE inference_provider_attempts
         SET status = ?, terminal_fingerprint = ?, provider_response_id = ?,
             input_tokens = ?, output_tokens = ?, cache_read_tokens = ?,
             cache_creation_tokens = ?, error_kind = ?, error_message = ?, terminal_at = NOW(6)
         WHERE user_id = ? AND attempt_id = ? AND status = 'started'",
    )
    .bind(&terminal_state.status)
    .bind(&fingerprint)
    .bind(&terminal_state.provider_response_id)
    .bind(terminal_state.input_tokens)
    .bind(terminal_state.output_tokens)
    .bind(terminal_state.cache_read_tokens)
    .bind(terminal_state.cache_creation_tokens)
    .bind(&terminal_state.error_kind)
    .bind(&terminal_state.error_message)
    .bind(&attempt.user_id)
    .bind(&attempt.attempt_id);

    let result = if terminal.status == InferenceTerminalStatus::Succeeded {
        // A successful physical response is final. Persist its recovery debt in
        // the same transaction, so a crash cannot leave a successful response
        // invisible to the logical lifecycle without an explicit recovery fact.
        let mut tx = db.begin().await.map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "begin successful inference provider terminal",
                error,
            )
        })?;
        lock_admitted_inference_invocation(
            &mut tx,
            &attempt.user_id,
            &attempt.invocation_id,
            "record a successful provider terminal",
        )
        .await?;
        let result = update.execute(&mut *tx).await.map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "finish inference provider attempt",
                error,
            )
        })?;
        if result.rows_affected() == 1 {
            write_inference_settlement_debt(
                &mut tx,
                &attempt.user_id,
                &attempt.invocation_id,
                &terminal_state,
            )
            .await?;
        }
        tx.commit().await.map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "commit successful inference provider terminal",
                error,
            )
        })?;
        result
    } else {
        update.execute(db).await.map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "finish inference provider attempt",
                error,
            )
        })?
    };
    if result.rows_affected() == 1 {
        return Ok(());
    }
    if let Some(existing) = existing_provider_attempt_fingerprint(db, attempt).await? {
        return if existing == fingerprint {
            if terminal.status == InferenceTerminalStatus::Succeeded {
                record_inference_settlement_debt(
                    db,
                    &attempt.user_id,
                    &attempt.invocation_id,
                    &terminal_state,
                    SettlementDebtMode::RequireQuiescent,
                )
                .await?;
            }
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

#[derive(Clone, Debug)]
struct DurableInferenceTerminal {
    status: String,
    terminal_fingerprint: Option<String>,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_creation_tokens: i64,
    provider_response_id: Option<String>,
    error_kind: Option<String>,
    error_message: Option<String>,
}

impl DurableInferenceTerminal {
    fn from_terminal(
        terminal: &InferenceInvocationTerminal,
        terminal_fingerprint: String,
    ) -> ServiceResult<Self> {
        Ok(Self {
            status: terminal.status.as_str().to_string(),
            terminal_fingerprint: Some(terminal_fingerprint),
            input_tokens: checked_i64(terminal.usage.input_tokens, "terminal input_tokens")?,
            output_tokens: checked_i64(terminal.usage.output_tokens, "terminal output_tokens")?,
            cache_read_tokens: checked_i64(
                terminal.usage.cache_read_tokens,
                "terminal cache_read_tokens",
            )?,
            cache_creation_tokens: checked_i64(
                terminal.usage.cache_creation_tokens,
                "terminal cache_creation_tokens",
            )?,
            provider_response_id: terminal.provider_response_id.clone(),
            error_kind: terminal.error_kind.clone(),
            error_message: terminal.error_message.clone(),
        })
    }

    fn decode(row: &sqlx::mysql::MySqlRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            status: row.try_get("status")?,
            terminal_fingerprint: row.try_get("terminal_fingerprint")?,
            input_tokens: row.try_get("input_tokens")?,
            output_tokens: row.try_get("output_tokens")?,
            cache_read_tokens: row.try_get("cache_read_tokens")?,
            cache_creation_tokens: row.try_get("cache_creation_tokens")?,
            provider_response_id: row.try_get("provider_response_id")?,
            error_kind: row.try_get("error_kind")?,
            error_message: row.try_get("error_message")?,
        })
    }
}

async fn write_inference_settlement_debt(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    user_id: &str,
    invocation_id: &str,
    terminal: &DurableInferenceTerminal,
) -> ServiceResult<()> {
    let fingerprint = terminal.terminal_fingerprint.as_deref().ok_or_else(|| {
        ServiceError::invalid("inference settlement debt requires a terminal fingerprint")
    })?;
    sqlx::query(
        "INSERT IGNORE INTO inference_invocation_settlement_debts
         (user_id, invocation_id, session_id, harness_run_id,
          terminal_status, terminal_fingerprint,
          input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
          provider_response_id, error_kind, error_message)
         SELECT invocation.user_id, invocation.invocation_id,
                invocation.session_id, invocation.harness_run_id,
                ?, ?, ?, ?, ?, ?, ?, ?, ?
         FROM inference_invocations AS invocation
         WHERE invocation.user_id = ? AND invocation.invocation_id = ?",
    )
    .bind(&terminal.status)
    .bind(fingerprint)
    .bind(terminal.input_tokens)
    .bind(terminal.output_tokens)
    .bind(terminal.cache_read_tokens)
    .bind(terminal.cache_creation_tokens)
    .bind(&terminal.provider_response_id)
    .bind(&terminal.error_kind)
    .bind(&terminal.error_message)
    .bind(user_id)
    .bind(invocation_id)
    .execute(&mut **tx)
    .await
    .map_err(|error| {
        ServiceError::with_source(
            ServiceErrorKind::Persistence,
            "record inference settlement debt",
            error,
        )
    })?;

    let existing = sqlx::query(
        "SELECT terminal_status, terminal_fingerprint
         FROM inference_invocation_settlement_debts
         WHERE user_id = ? AND invocation_id = ?",
    )
    .bind(user_id)
    .bind(invocation_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|error| {
        ServiceError::with_source(
            ServiceErrorKind::Persistence,
            "verify inference settlement debt",
            error,
        )
    })?;
    let existing_status = existing
        .try_get::<String, _>("terminal_status")
        .map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "decode inference settlement debt status",
                error,
            )
        })?;
    let existing_fingerprint = existing
        .try_get::<String, _>("terminal_fingerprint")
        .map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "decode inference settlement debt fingerprint",
                error,
            )
        })?;
    if existing_status == terminal.status && existing_fingerprint == fingerprint {
        Ok(())
    } else {
        Err(ServiceError::conflict(format!(
            "inference invocation {invocation_id} already has a different durable settlement intent"
        )))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SettlementDebtMode {
    RequireQuiescent,
    FenceOpenAttempts,
}

async fn record_inference_settlement_debt(
    db: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    invocation_id: &str,
    terminal: &DurableInferenceTerminal,
    mode: SettlementDebtMode,
) -> ServiceResult<()> {
    let mut tx = db.begin().await.map_err(|error| {
        ServiceError::with_source(
            ServiceErrorKind::Persistence,
            "begin inference settlement debt",
            error,
        )
    })?;
    let invocation = sqlx::query(
        "SELECT status, terminal_fingerprint FROM inference_invocations
         WHERE user_id = ? AND invocation_id = ?
         FOR UPDATE",
    )
    .bind(user_id)
    .bind(invocation_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|error| {
        ServiceError::with_source(
            ServiceErrorKind::Persistence,
            "lock inference invocation settlement",
            error,
        )
    })?;
    let Some(invocation) = invocation else {
        return Err(ServiceError::conflict(format!(
            "inference invocation {invocation_id} is unavailable; cannot record a terminal settlement"
        )));
    };
    let status = invocation.try_get::<String, _>("status").map_err(|error| {
        ServiceError::with_source(
            ServiceErrorKind::Persistence,
            "decode locked inference invocation status",
            error,
        )
    })?;
    if status != "admitted" {
        let durable_fingerprint = invocation
            .try_get::<Option<String>, _>("terminal_fingerprint")
            .map_err(|error| {
                ServiceError::with_source(
                    ServiceErrorKind::Persistence,
                    "decode locked inference invocation fingerprint",
                    error,
                )
            })?;
        return if durable_fingerprint.as_deref() == terminal.terminal_fingerprint.as_deref() {
            Ok(())
        } else {
            Err(ServiceError::conflict(format!(
                "inference invocation {invocation_id} is {status} with a different terminal result"
            )))
        };
    }
    if mode == SettlementDebtMode::RequireQuiescent {
        let has_open_attempt = sqlx::query(
            "SELECT 1 FROM inference_provider_attempts
             WHERE user_id = ? AND invocation_id = ? AND status = 'started'
             LIMIT 1",
        )
        .bind(user_id)
        .bind(invocation_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "check open inference provider attempts before settlement",
                error,
            )
        })?
        .is_some();
        if has_open_attempt {
            return Err(ServiceError::conflict(format!(
                "inference invocation {invocation_id} still has an active provider attempt"
            )));
        }
    }
    write_inference_settlement_debt(&mut tx, user_id, invocation_id, terminal).await?;
    tx.commit().await.map_err(|error| {
        ServiceError::with_source(
            ServiceErrorKind::Persistence,
            "commit inference settlement debt",
            error,
        )
    })
}

/// Durably declare that the lifecycle owner has stopped retrying this logical
/// invocation. This fences any later physical attempt before the caller closes
/// attempts that may already be in flight. Recovery can therefore converge a
/// partial terminal write without redelivering provider I/O.
pub async fn declare_inference_settlement(
    pool: &SharedPool,
    plan: &InferenceInvocationPlan,
    terminal: &InferenceInvocationTerminal,
) -> ServiceResult<()> {
    let fingerprint = terminal_fingerprint(terminal)?;
    let terminal = DurableInferenceTerminal::from_terminal(terminal, fingerprint)?;
    record_inference_settlement_debt(
        pool.get(),
        &plan.input.user_id,
        &plan.invocation_id,
        &terminal,
        SettlementDebtMode::FenceOpenAttempts,
    )
    .await
}

async fn clear_inference_settlement_debt(
    db: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    invocation_id: &str,
    terminal_fingerprint: &str,
) -> ServiceResult<()> {
    delete_inference_settlement_debt(db, user_id, invocation_id, terminal_fingerprint)
        .await
        .map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "clear inference settlement debt",
                error,
            )
        })
        .map(|_| ())
}

async fn apply_inference_terminal_if_quiescent(
    db: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    invocation_id: &str,
    terminal: DurableInferenceTerminal,
) -> Result<u64, sqlx::Error> {
    sqlx::query(
        "UPDATE inference_invocations
         SET status = ?,
             terminal_fingerprint = ?,
             input_tokens = ?,
             output_tokens = ?,
             cache_read_tokens = ?,
             cache_creation_tokens = ?,
             provider_response_id = ?,
             error_kind = ?,
             error_message = ?,
             terminal_at = NOW(6)
         WHERE user_id = ?
           AND invocation_id = ?
           AND status = 'admitted'
           AND NOT EXISTS (
                SELECT 1
                FROM inference_provider_attempts AS open_attempt
                WHERE open_attempt.user_id = inference_invocations.user_id
                  AND open_attempt.invocation_id = inference_invocations.invocation_id
                  AND open_attempt.status = 'started'
           )",
    )
    .bind(terminal.status)
    .bind(terminal.terminal_fingerprint)
    .bind(terminal.input_tokens)
    .bind(terminal.output_tokens)
    .bind(terminal.cache_read_tokens)
    .bind(terminal.cache_creation_tokens)
    .bind(terminal.provider_response_id)
    .bind(terminal.error_kind)
    .bind(terminal.error_message)
    .bind(user_id)
    .bind(invocation_id)
    .execute(db)
    .await
    .map(|result| result.rows_affected())
}

async fn matching_successful_provider_attempt(
    db: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    invocation_id: &str,
    fingerprint: &str,
) -> Result<Option<DurableInferenceTerminal>, sqlx::Error> {
    sqlx::query(
        "SELECT status, terminal_fingerprint, input_tokens, output_tokens, cache_read_tokens,
                cache_creation_tokens, provider_response_id, error_kind, error_message
         FROM inference_provider_attempts
         WHERE user_id = ?
           AND invocation_id = ?
           AND status = 'succeeded'
           AND terminal_fingerprint = ?
           AND NOT EXISTS (
                SELECT 1
                FROM inference_provider_attempts AS open_attempt
                WHERE open_attempt.user_id = inference_provider_attempts.user_id
                  AND open_attempt.invocation_id = inference_provider_attempts.invocation_id
                  AND open_attempt.status = 'started'
           )
           AND NOT EXISTS (
                SELECT 1
                FROM inference_provider_attempts AS later_attempt
                WHERE later_attempt.user_id = inference_provider_attempts.user_id
                  AND later_attempt.invocation_id = inference_provider_attempts.invocation_id
                  AND later_attempt.attempt_index > inference_provider_attempts.attempt_index
           )
         LIMIT 1",
    )
    .bind(user_id)
    .bind(invocation_id)
    .bind(fingerprint)
    .fetch_optional(db)
    .await?
    .map(|row| DurableInferenceTerminal::decode(&row))
    .transpose()
}

async fn delete_inference_settlement_debt(
    db: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    invocation_id: &str,
    fingerprint: &str,
) -> Result<u64, sqlx::Error> {
    sqlx::query(
        "DELETE FROM inference_invocation_settlement_debts
         WHERE user_id = ? AND invocation_id = ? AND terminal_fingerprint = ?",
    )
    .bind(user_id)
    .bind(invocation_id)
    .bind(fingerprint)
    .execute(db)
    .await
    .map(|result| result.rows_affected())
}

const INFERENCE_SETTLEMENT_RECOVERY_BATCH: i64 = 256;

async fn close_open_attempts_owned_by_settlement_debt(
    db: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    invocation_id: &str,
) -> Result<u64, sqlx::Error> {
    let terminal = InferenceInvocationTerminal {
        status: InferenceTerminalStatus::DeliveryUnknown,
        usage: InferenceUsage::default(),
        provider_response_id: None,
        error_kind: Some("settlement_recovery".to_string()),
        error_message: Some(
            "provider attempt terminal write was not observed before logical settlement"
                .to_string(),
        ),
    };
    let fingerprint = terminal_fingerprint(&terminal)
        .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
    sqlx::query(
        "UPDATE inference_provider_attempts
         SET status = 'delivery_unknown', terminal_fingerprint = ?,
             input_tokens = 0, output_tokens = 0, cache_read_tokens = 0,
             cache_creation_tokens = 0, provider_response_id = NULL,
             error_kind = ?, error_message = ?, terminal_at = NOW(6)
         WHERE user_id = ? AND invocation_id = ? AND status = 'started'",
    )
    .bind(fingerprint)
    .bind(terminal.error_kind)
    .bind(terminal.error_message)
    .bind(user_id)
    .bind(invocation_id)
    .execute(db)
    .await
    .map(|result| result.rows_affected())
}

async fn reconcile_inference_settlement_debt(
    db: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    invocation_id: &str,
) -> Result<u64, sqlx::Error> {
    let Some(row) = sqlx::query(
        "SELECT debt.user_id, debt.invocation_id, debt.terminal_status AS status,
                debt.terminal_fingerprint, debt.input_tokens, debt.output_tokens,
                debt.cache_read_tokens, debt.cache_creation_tokens,
                debt.provider_response_id, debt.error_kind, debt.error_message,
                invocation.status AS invocation_status,
                invocation.terminal_fingerprint AS invocation_terminal_fingerprint
         FROM inference_invocation_settlement_debts AS debt
         JOIN inference_invocations AS invocation
           ON invocation.user_id = debt.user_id
          AND invocation.invocation_id = debt.invocation_id
         WHERE debt.user_id = ? AND debt.invocation_id = ?",
    )
    .bind(user_id)
    .bind(invocation_id)
    .fetch_optional(db)
    .await?
    else {
        return Ok(0);
    };

    let terminal = DurableInferenceTerminal::decode(&row)?;
    let fingerprint = terminal
        .terminal_fingerprint
        .clone()
        .ok_or_else(|| sqlx::Error::Protocol("settlement debt has no fingerprint".to_string()))?;
    let invocation_status = row.try_get::<String, _>("invocation_status")?;
    if invocation_status == "admitted" {
        let recovered_attempts =
            close_open_attempts_owned_by_settlement_debt(db, user_id, invocation_id).await?;
        if recovered_attempts > 0 {
            tracing::warn!(
                %user_id,
                %invocation_id,
                recovered_attempts,
                "closed provider attempts left open behind an authoritative settlement debt"
            );
        }
        if terminal.status == InferenceTerminalStatus::Succeeded.as_str()
            && matching_successful_provider_attempt(db, user_id, invocation_id, &fingerprint)
                .await?
                .is_none()
        {
            tracing::error!(
                %user_id,
                %invocation_id,
                debt_fingerprint = %fingerprint,
                "discarding inference success debt without a matching provider terminal"
            );
            delete_inference_settlement_debt(db, user_id, invocation_id, &fingerprint).await?;
            return Ok(0);
        }
        let updated =
            apply_inference_terminal_if_quiescent(db, user_id, invocation_id, terminal).await?;
        if updated == 1 {
            delete_inference_settlement_debt(db, user_id, invocation_id, &fingerprint).await?;
        } else {
            tracing::warn!(
                %user_id,
                %invocation_id,
                debt_fingerprint = %fingerprint,
                "inference settlement debt remains pending after a non-terminal reconciliation"
            );
        }
        return Ok(updated);
    }

    if row
        .try_get::<Option<String>, _>("invocation_terminal_fingerprint")?
        .as_deref()
        == Some(fingerprint.as_str())
    {
        delete_inference_settlement_debt(db, user_id, invocation_id, &fingerprint).await?;
    } else {
        tracing::error!(
            %user_id,
            %invocation_id,
            debt_fingerprint = %fingerprint,
            invocation_status,
            "discarding inference settlement debt that conflicts with terminal invocation state"
        );
        delete_inference_settlement_debt(db, user_id, invocation_id, &fingerprint).await?;
    }
    Ok(0)
}

async fn reconcile_settlement_identities<F, Fut>(
    identities: Vec<(String, String)>,
    mut reconcile: F,
) -> Result<u64, sqlx::Error>
where
    F: FnMut(String, String) -> Fut,
    Fut: std::future::Future<Output = Result<u64, sqlx::Error>>,
{
    let mut reconciled = 0;
    let mut first_error = None;
    for (user_id, invocation_id) in identities {
        match reconcile(user_id.clone(), invocation_id.clone()).await {
            Ok(count) => reconciled += count,
            Err(error) => {
                tracing::warn!(
                    %user_id,
                    %invocation_id,
                    %error,
                    "inference settlement debt reconciliation failed; later debts remain eligible"
                );
                first_error.get_or_insert(error);
            }
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(reconciled),
    }
}

async fn reconcile_inference_settlement_debts_batch(
    db: &sqlx::Pool<sqlx::MySql>,
    limit: i64,
) -> Result<u64, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT user_id, invocation_id
         FROM inference_invocation_settlement_debts
         ORDER BY user_id ASC, invocation_id ASC
         LIMIT ?",
    )
    .bind(limit.clamp(1, INFERENCE_SETTLEMENT_RECOVERY_BATCH))
    .fetch_all(db)
    .await?;
    let mut identities = Vec::with_capacity(rows.len());
    for row in rows {
        let user_id = match row.try_get::<String, _>("user_id") {
            Ok(user_id) => user_id,
            Err(error) => {
                tracing::warn!(%error, "skip malformed inference settlement debt owner");
                continue;
            }
        };
        let invocation_id = match row.try_get::<String, _>("invocation_id") {
            Ok(invocation_id) => invocation_id,
            Err(error) => {
                tracing::warn!(%user_id, %error, "skip malformed inference settlement debt identity");
                continue;
            }
        };
        identities.push((user_id, invocation_id));
    }
    reconcile_settlement_identities(identities, |user_id, invocation_id| async move {
        reconcile_inference_settlement_debt(db, &user_id, &invocation_id).await
    })
    .await
}

/// Reconcile at most one bounded batch of explicit settlement decisions.
/// Runtime workers call this repeatedly; schema readiness never waits for the
/// operational backlog to drain.
pub async fn reconcile_inference_settlements(pool: &SharedPool, limit: u32) -> ServiceResult<u64> {
    reconcile_inference_settlement_debts_batch(pool.get(), i64::from(limit.max(1)))
        .await
        .map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "reconcile inference settlement batch",
                error,
            )
        })
}

/// Resolve an ambiguous terminal transaction from its durable settlement debt.
/// If the database is still unavailable, preserve the original error and leave
/// the debt for the normal startup recovery instead of reissuing provider I/O.
async fn recover_terminal_after_commit_error(
    db: &sqlx::Pool<sqlx::MySql>,
    plan: &InferenceInvocationPlan,
    fingerprint: &str,
) -> ServiceResult<bool> {
    if let Err(error) =
        reconcile_inference_settlement_debt(db, &plan.input.user_id, &plan.invocation_id).await
    {
        tracing::warn!(
            invocation_id = %plan.invocation_id,
            %error,
            "inference terminal commit is unresolved and its settlement debt remains pending"
        );
        return Ok(false);
    }
    let Some(existing) = existing_terminal_fingerprint(db, plan).await? else {
        return Ok(false);
    };
    if existing == fingerprint {
        Ok(true)
    } else {
        Err(ServiceError::conflict(format!(
            "inference invocation {} terminal payload conflicts with its durable result",
            plan.invocation_id
        )))
    }
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
    let terminal_state = DurableInferenceTerminal::from_terminal(terminal, fingerprint.clone())?;
    let db = pool.get();
    if let Some(existing) = existing_terminal_fingerprint(db, plan).await? {
        return if existing == fingerprint {
            if let Err(error) = clear_inference_settlement_debt(
                db,
                &plan.input.user_id,
                &plan.invocation_id,
                &fingerprint,
            )
            .await
            {
                tracing::warn!(
                    invocation_id = %plan.invocation_id,
                    %error,
                    "logical invocation is terminal but its settlement debt cleanup will be retried"
                );
            }
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

    // The finalization owner explicitly records the logical outcome before it
    // tries to mirror it to `inference_invocations`. Unlike a provider attempt
    // failure, this is a durable declaration that retry policy has finished.
    record_inference_settlement_debt(
        db,
        &plan.input.user_id,
        &plan.invocation_id,
        &terminal_state,
        SettlementDebtMode::RequireQuiescent,
    )
    .await?;

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
             WHERE user_id = ? AND invocation_id = ? AND status = 'admitted'
               AND NOT EXISTS (
                    SELECT 1
                    FROM inference_provider_attempts AS open_attempt
                    WHERE open_attempt.user_id = inference_invocations.user_id
                      AND open_attempt.invocation_id = inference_invocations.invocation_id
                      AND open_attempt.status = 'started'
               )",
        )
        .bind(&terminal_state.status)
        .bind(&fingerprint)
        .bind(terminal_state.input_tokens)
        .bind(terminal_state.output_tokens)
        .bind(terminal_state.cache_read_tokens)
        .bind(terminal_state.cache_creation_tokens)
        .bind(&terminal_state.provider_response_id)
        .bind(&terminal_state.error_kind)
        .bind(&terminal_state.error_message)
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
        sqlx::query(
            "DELETE FROM inference_invocation_settlement_debts
             WHERE user_id = ? AND invocation_id = ? AND terminal_fingerprint = ?",
        )
        .bind(&plan.input.user_id)
        .bind(&plan.invocation_id)
        .bind(&fingerprint)
        .execute(&mut *tx)
        .await
        .map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "clear inference settlement debt with terminal invocation",
                error,
            )
        })?;
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
        if recover_terminal_after_commit_error(db, plan, &fingerprint).await? {
            tracing::warn!(
                invocation_id = %plan.invocation_id,
                "reconciled inference after logical terminal commit failed"
            );
            return Ok(());
        }
        return Err(error);
    }
    if let Err(error) = tx.commit().await {
        let commit_error = ServiceError::with_source(
            ServiceErrorKind::Persistence,
            "commit inference terminal state",
            error,
        );
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
        if recover_terminal_after_commit_error(db, plan, &fingerprint).await? {
            tracing::warn!(
                invocation_id = %plan.invocation_id,
                "reconciled inference after terminal commit result was unknown"
            );
            return Ok(());
        }
        return Err(commit_error);
    }
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

    #[tokio::test]
    async fn settlement_batch_continues_after_a_persistent_record_failure() {
        let visited = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let observed = visited.clone();
        let result = reconcile_settlement_identities(
            vec![
                ("user".to_string(), "first".to_string()),
                ("user".to_string(), "second".to_string()),
            ],
            move |_user_id, invocation_id| {
                let observed = observed.clone();
                async move {
                    observed.lock().await.push(invocation_id.clone());
                    if invocation_id == "first" {
                        Err(sqlx::Error::PoolTimedOut)
                    } else {
                        Ok(1)
                    }
                }
            },
        )
        .await;

        assert!(matches!(result, Err(sqlx::Error::PoolTimedOut)));
        assert_eq!(*visited.lock().await, vec!["first", "second"]);
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
