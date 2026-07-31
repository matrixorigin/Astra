use astra_core::SharedPool;
use astra_turn_types::{InferenceInvocationScope, InferencePurpose};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::Row;

use crate::model_request_context::{
    MODEL_REQUEST_CONTEXT_SCHEMA, ModelRequestContextEvent, ModelRequestContextSeed,
    ModelRequestEventStage, ModelRequestIdentity, ModelRequestTopology, ModelRequestUsage,
    ModelRequestWireComposition,
};
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
    admission_token: String,
    input: InferenceInvocationInput,
}

#[derive(Clone, Debug, PartialEq)]
pub struct InferenceProviderAttemptPlan {
    attempt_id: String,
    invocation_id: String,
    user_id: String,
    attempt_index: u32,
    provider: String,
    admission_token: String,
    wire: InferenceProviderWireIdentity,
    invocation_input: InferenceInvocationInput,
    request_context: ModelRequestContextSeed,
}

/// Immutable identity of the exact serialized provider request body.
///
/// The runtime constructs this only after provider-specific request assembly.
/// The service validates and persists it with the physical-attempt admission,
/// before any network I/O is authorized.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InferenceProviderWireIdentity {
    protocol: String,
    provider_wire_hash: String,
    provider_wire_bytes: u64,
    composition: ModelRequestWireComposition,
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

    /// One provider request has exactly one physical-attempt identity.
    #[must_use]
    pub fn request_id(&self) -> &str {
        &self.attempt_id
    }

    #[must_use]
    pub fn invocation_id(&self) -> &str {
        &self.invocation_id
    }

    #[must_use]
    pub fn attempt_index(&self) -> u32 {
        self.attempt_index
    }

    #[must_use]
    pub fn wire(&self) -> &InferenceProviderWireIdentity {
        &self.wire
    }

    #[must_use]
    pub fn request_context(&self) -> &ModelRequestContextSeed {
        &self.request_context
    }
}

impl InferenceProviderWireIdentity {
    pub fn new(
        protocol: impl Into<String>,
        provider_wire_hash: impl Into<String>,
        provider_wire_bytes: u64,
    ) -> ServiceResult<Self> {
        let protocol = protocol.into();
        if !matches!(
            protocol.as_str(),
            "openai_compatible" | "anthropic_messages" | "bedrock_converse"
        ) {
            return Err(ServiceError::invalid(
                "provider_protocol must be one of the typed transport protocols",
            ));
        }
        let provider_wire_hash = provider_wire_hash.into();
        if provider_wire_hash.len() != 64
            || !provider_wire_hash
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(ServiceError::invalid(
                "provider_wire_hash must be an exact lowercase SHA-256 hex digest",
            ));
        }
        if provider_wire_bytes == 0 {
            return Err(ServiceError::invalid(
                "provider_wire_bytes must describe a non-empty serialized request",
            ));
        }
        Ok(Self {
            protocol,
            provider_wire_hash,
            provider_wire_bytes,
            composition: ModelRequestWireComposition::default(),
        })
    }

    #[must_use]
    pub fn with_composition(mut self, composition: ModelRequestWireComposition) -> Self {
        self.composition = composition;
        self
    }

    #[must_use]
    pub fn protocol(&self) -> &str {
        &self.protocol
    }

    #[must_use]
    pub fn provider_wire_hash(&self) -> &str {
        &self.provider_wire_hash
    }

    #[must_use]
    pub fn provider_wire_bytes(&self) -> u64 {
        self.provider_wire_bytes
    }

    #[must_use]
    pub fn composition(&self) -> &ModelRequestWireComposition {
        &self.composition
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

fn new_admission_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()
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
        admission_token: new_admission_token(),
        input,
    })
}

#[must_use]
pub fn plan_inference_provider_attempt(
    invocation: &InferenceInvocationPlan,
    attempt_index: u32,
    wire: InferenceProviderWireIdentity,
) -> InferenceProviderAttemptPlan {
    let mut request_context = ModelRequestContextSeed::server_default();
    if invocation.input.execution_placement == ModelExecutionPlacement::Edge {
        request_context.topology = ModelRequestTopology::EdgeServer;
        request_context.execution_binding = "edge".to_string();
    }
    plan_inference_provider_attempt_with_context(invocation, attempt_index, wire, request_context)
}

#[must_use]
pub fn plan_inference_provider_attempt_with_context(
    invocation: &InferenceInvocationPlan,
    attempt_index: u32,
    wire: InferenceProviderWireIdentity,
    request_context: ModelRequestContextSeed,
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
        admission_token: new_admission_token(),
        wire,
        invocation_input: invocation.input.clone(),
        request_context,
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

fn checked_optional_i64(value: Option<u64>, label: &str) -> ServiceResult<Option<i64>> {
    value.map(|value| checked_i64(value, label)).transpose()
}

fn model_request_metric_shard(attempt_id: &str) -> i16 {
    i16::from(Sha256::digest(attempt_id.as_bytes())[0] % 64)
}

fn model_request_event(
    attempt: &InferenceProviderAttemptPlan,
    stage: ModelRequestEventStage,
    terminal: Option<&InferenceInvocationTerminal>,
) -> ServiceResult<(String, String, ModelRequestContextEvent)> {
    if matches!(
        (stage, terminal),
        (ModelRequestEventStage::Accepted, Some(_)) | (ModelRequestEventStage::Terminal, None)
    ) {
        return Err(ServiceError::internal(
            "model request accepted events cannot carry terminals and terminal events require one",
        ));
    }
    let mut budget = attempt.request_context.budget.clone();
    let usage = terminal.map(|terminal| {
        let measured = terminal.usage.input_tokens;
        budget.measured_input_tokens = Some(measured);
        budget.usage_source = Some("provider_terminal".to_string());
        if let Some(estimated) = budget.estimated_input_tokens {
            let error = i128::from(measured) - i128::from(estimated);
            budget.estimate_error_tokens =
                Some(i64::try_from(error).unwrap_or(if error.is_negative() {
                    i64::MIN
                } else {
                    i64::MAX
                }));
            budget.estimate_error_ratio =
                (estimated > 0).then_some(error as f64 / estimated as f64);
        }
        ModelRequestUsage {
            fresh_input_tokens: measured
                .saturating_sub(terminal.usage.cache_read_tokens)
                .saturating_sub(terminal.usage.cache_creation_tokens),
            cache_read_tokens: terminal.usage.cache_read_tokens,
            cache_creation_tokens: terminal.usage.cache_creation_tokens,
            request_input_tokens: measured,
            output_tokens: terminal.usage.output_tokens,
        }
    });
    let mut cache = attempt.request_context.cache.clone();
    if let Some(usage) = usage.as_ref() {
        cache.cache_read_share = (usage.request_input_tokens > 0)
            .then_some(usage.cache_read_tokens as f64 / usage.request_input_tokens as f64);
    }
    let input = &attempt.invocation_input;
    let event = ModelRequestContextEvent {
        schema: MODEL_REQUEST_CONTEXT_SCHEMA.to_string(),
        stage,
        identity: ModelRequestIdentity {
            request_id: attempt.request_id().to_string(),
            provider_response_id: terminal
                .and_then(|terminal| terminal.provider_response_id.clone()),
            owner_scope: input.user_id.clone(),
            session_id: input.scope.session_id().map(str::to_string),
            run_id: input.scope.run_id().map(str::to_string),
            harness_run_id: input.scope.harness_run_id().map(str::to_string),
            turn: input.scope.turn(),
            round: input.scope.round(),
            logical_attempt: input.scope.logical_attempt(),
            physical_attempt: attempt.attempt_index,
            actor_id: attempt.request_context.actor_id.clone(),
            execution_principal: attempt.request_context.execution_principal.clone(),
            billing_scope: attempt.request_context.billing_scope.clone(),
            auth_session_id: attempt.request_context.auth_session_id.clone(),
            device_instance_id: attempt.request_context.device_instance_id.clone(),
            agent_id: attempt.request_context.agent_id.clone(),
            parent_run_id: attempt.request_context.parent_run_id.clone(),
            topology: attempt.request_context.topology,
            interaction_owner: attempt.request_context.interaction_owner.clone(),
            loop_owner: attempt.request_context.loop_owner.clone(),
            execution_binding: attempt.request_context.execution_binding.clone(),
            provider: input.provider.clone(),
            model: input.resolved_model_name.clone(),
            offering_id: input.offering_id.clone(),
            inference_purpose: input.purpose.as_str().to_string(),
            provider_protocol: attempt.wire.protocol.clone(),
            provider_wire_hash: attempt.wire.provider_wire_hash.clone(),
            provider_wire_bytes: attempt.wire.provider_wire_bytes,
        },
        lineage: attempt.request_context.lineage.clone(),
        budget,
        usage,
        composition: attempt.request_context.composition.clone(),
        wire_composition: attempt.wire.composition.clone(),
        cache,
        compaction: attempt.request_context.compaction.clone(),
        terminal_status: terminal.map(|terminal| terminal.status.as_str().to_string()),
        error_kind: terminal.and_then(|terminal| terminal.error_kind.clone()),
    };
    let event_json = serde_json::to_string(&event).map_err(|error| {
        ServiceError::with_source(
            ServiceErrorKind::Internal,
            "serialize model request context event",
            error,
        )
    })?;
    let event_id = hash_identity("mrctx", &[attempt.attempt_id.as_str(), stage.as_str()]);
    Ok((event_id, event_json, event))
}

async fn insert_model_request_context_event(
    connection: &mut sqlx::MySqlConnection,
    attempt: &InferenceProviderAttemptPlan,
    stage: ModelRequestEventStage,
    terminal: Option<&InferenceInvocationTerminal>,
) -> ServiceResult<()> {
    let (event_id, event_json, event) = model_request_event(attempt, stage, terminal)?;
    let usage = event.usage.as_ref();
    let model_family = attempt
        .request_context
        .model_family
        .as_deref()
        .unwrap_or("unspecified");
    sqlx::query(
        "INSERT INTO model_request_context_events
         (event_id, user_id, attempt_id, invocation_id, session_id, run_id, harness_run_id,
          event_stage, terminal_status, topology, provider, model_family, purpose,
          input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
          event_json, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW(6))",
    )
    .bind(event_id)
    .bind(&attempt.user_id)
    .bind(&attempt.attempt_id)
    .bind(&attempt.invocation_id)
    .bind(attempt.invocation_input.scope.session_id())
    .bind(attempt.invocation_input.scope.run_id())
    .bind(attempt.invocation_input.scope.harness_run_id())
    .bind(stage.as_str())
    .bind(event.terminal_status.as_deref())
    .bind(event.identity.topology.as_str())
    .bind(&event.identity.provider)
    .bind(model_family)
    .bind(&event.identity.inference_purpose)
    .bind(checked_optional_i64(
        usage.map(|usage| usage.request_input_tokens),
        "model request input_tokens",
    )?)
    .bind(checked_optional_i64(
        usage.map(|usage| usage.output_tokens),
        "model request output_tokens",
    )?)
    .bind(checked_optional_i64(
        usage.map(|usage| usage.cache_read_tokens),
        "model request cache_read_tokens",
    )?)
    .bind(checked_optional_i64(
        usage.map(|usage| usage.cache_creation_tokens),
        "model request cache_creation_tokens",
    )?)
    .bind(event_json)
    .execute(&mut *connection)
    .await
    .map_err(|error| {
        ServiceError::with_source(
            ServiceErrorKind::Persistence,
            "insert model request context event",
            error,
        )
    })?;
    if let (Some(status), Some(usage)) = (event.terminal_status.as_deref(), usage) {
        sqlx::query(
            "INSERT INTO model_request_metric_shards
             (metric_shard, topology, provider, model_family, purpose, terminal_status,
              requests, input_tokens, output_tokens, cache_read_tokens,
              cache_creation_tokens, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, 1, ?, ?, ?, ?, NOW(6))
             ON DUPLICATE KEY UPDATE
              requests = requests + 1,
              input_tokens = input_tokens + VALUES(input_tokens),
              output_tokens = output_tokens + VALUES(output_tokens),
              cache_read_tokens = cache_read_tokens + VALUES(cache_read_tokens),
              cache_creation_tokens =
                  cache_creation_tokens + VALUES(cache_creation_tokens),
              updated_at = NOW(6)",
        )
        .bind(model_request_metric_shard(&attempt.attempt_id))
        .bind(event.identity.topology.as_str())
        .bind(&event.identity.provider)
        .bind(model_family)
        .bind(&event.identity.inference_purpose)
        .bind(status)
        .bind(checked_i64(
            usage.request_input_tokens,
            "model request metric input_tokens",
        )?)
        .bind(checked_i64(
            usage.output_tokens,
            "model request metric output_tokens",
        )?)
        .bind(checked_i64(
            usage.cache_read_tokens,
            "model request metric cache_read_tokens",
        )?)
        .bind(checked_i64(
            usage.cache_creation_tokens,
            "model request metric cache_creation_tokens",
        )?)
        .execute(&mut *connection)
        .await
        .map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "accumulate model request metrics",
                error,
            )
        })?;
    }
    prune_model_request_context_scope(connection, attempt).await?;
    Ok(())
}

async fn prune_model_request_context_scope(
    connection: &mut sqlx::MySqlConnection,
    attempt: &InferenceProviderAttemptPlan,
) -> ServiceResult<()> {
    const MAX_CONTEXT_EVENTS_PER_SCOPE: i64 = 2_048;
    const PRUNE_ATTEMPTS_PER_BATCH: i64 = 256;

    let (probe_sql, oldest_sql, scope_value) =
        if let Some(session_id) = attempt.invocation_input.scope.session_id() {
            (
                "SELECT event_id FROM model_request_context_events
                 WHERE user_id = ? AND session_id = ?
                 ORDER BY created_at DESC, event_id DESC
                 LIMIT 1 OFFSET ?",
                "SELECT attempt_id FROM model_request_context_events
                 WHERE user_id = ? AND session_id = ?
                 GROUP BY attempt_id
                 HAVING COUNT(*) = 2
                 ORDER BY MIN(created_at) ASC, attempt_id ASC
                 LIMIT ?",
                session_id,
            )
        } else if let Some(harness_run_id) = attempt.invocation_input.scope.harness_run_id() {
            (
                "SELECT event_id FROM model_request_context_events
                 WHERE user_id = ? AND harness_run_id = ?
                 ORDER BY created_at DESC, event_id DESC
                 LIMIT 1 OFFSET ?",
                "SELECT attempt_id FROM model_request_context_events
                 WHERE user_id = ? AND harness_run_id = ?
                 GROUP BY attempt_id
                 HAVING COUNT(*) = 2
                 ORDER BY MIN(created_at) ASC, attempt_id ASC
                 LIMIT ?",
                harness_run_id,
            )
        } else {
            return Err(ServiceError::new(
                ServiceErrorKind::Internal,
                "model request context has no durable session or harness scope",
            ));
        };
    let over_limit = sqlx::query(probe_sql)
        .bind(&attempt.user_id)
        .bind(scope_value)
        .bind(MAX_CONTEXT_EVENTS_PER_SCOPE)
        .fetch_optional(&mut *connection)
        .await
        .map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "probe model request context retention",
                error,
            )
        })?
        .is_some();
    if !over_limit {
        return Ok(());
    }

    let oldest_attempts = sqlx::query(oldest_sql)
        .bind(&attempt.user_id)
        .bind(scope_value)
        .bind(PRUNE_ATTEMPTS_PER_BATCH)
        .fetch_all(&mut *connection)
        .await
        .map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "load model request context retention batch",
                error,
            )
        })?;
    let attempt_ids = oldest_attempts
        .into_iter()
        .map(|row| {
            row.try_get::<String, _>("attempt_id").map_err(|error| {
                ServiceError::with_source(
                    ServiceErrorKind::Persistence,
                    "decode model request context retention batch",
                    error,
                )
            })
        })
        .collect::<ServiceResult<Vec<_>>>()?;
    if attempt_ids.is_empty() {
        // Concurrent accepted attempts are not disposable until their
        // terminal fact arrives. Provider-slot admission bounds this temporary
        // excess; a later terminal insert will prune complete pairs.
        return Ok(());
    }
    let mut delete = sqlx::QueryBuilder::<sqlx::MySql>::new(
        "DELETE FROM model_request_context_events WHERE user_id = ",
    );
    delete
        .push_bind(&attempt.user_id)
        .push(" AND attempt_id IN (");
    {
        let mut separated = delete.separated(", ");
        for attempt_id in &attempt_ids {
            separated.push_bind(attempt_id);
        }
    }
    delete.push(")");
    delete
        .build()
        .execute(&mut *connection)
        .await
        .map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "prune model request context retention batch",
                error,
            )
        })?;
    Ok(())
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
    let exists = match &input.scope {
        InferenceInvocationScope::Run {
            session_id, run_id, ..
        } => {
            let session_exists = sqlx::query(
                "SELECT 1 FROM agent_sessions
                 WHERE user_id = ? AND session_id = ? AND status <> 'deleting'
                 LIMIT 1 FOR UPDATE",
            )
            .bind(&input.user_id)
            .bind(session_id)
            .fetch_optional(&mut *connection)
            .await
            .map_err(|error| {
                ServiceError::with_source(
                    ServiceErrorKind::Persistence,
                    "verify inference session scope",
                    error,
                )
            })?
            .is_some();
            session_exists
                && sqlx::query(
                    "SELECT 1 FROM agent_runs
                     WHERE user_id = ? AND session_id = ? AND run_id = ?
                     LIMIT 1 FOR UPDATE",
                )
                .bind(&input.user_id)
                .bind(session_id)
                .bind(run_id)
                .fetch_optional(&mut *connection)
                .await
                .map_err(|error| {
                    ServiceError::with_source(
                        ServiceErrorKind::Persistence,
                        "verify inference run scope",
                        error,
                    )
                })?
                .is_some()
        }
        InferenceInvocationScope::Session { session_id, .. } => sqlx::query(
            "SELECT 1 FROM agent_sessions
             WHERE user_id = ? AND session_id = ? AND status <> 'deleting'
             LIMIT 1 FOR UPDATE",
        )
        .bind(&input.user_id)
        .bind(session_id)
        .fetch_optional(&mut *connection)
        .await
        .map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "verify inference session scope",
                error,
            )
        })?
        .is_some(),
        InferenceInvocationScope::HarnessRun { harness_run_id, .. } => sqlx::query(
            "SELECT 1 FROM harness_runs
             WHERE user_id = ? AND harness_run_id = ? LIMIT 1 FOR UPDATE",
        )
        .bind(&input.user_id)
        .bind(harness_run_id)
        .fetch_optional(&mut *connection)
        .await
        .map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "verify inference harness scope",
                error,
            )
        })?
        .is_some(),
    };
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

#[derive(Clone, Debug, PartialEq, Eq)]
struct PersistedInvocationAdmissionFact {
    route_id: String,
    admission_token: String,
    status: String,
    terminal_fingerprint: Option<String>,
}

async fn load_invocation_admission_fact(
    db: &sqlx::Pool<sqlx::MySql>,
    plan: &InferenceInvocationPlan,
) -> ServiceResult<Option<PersistedInvocationAdmissionFact>> {
    sqlx::query(
        "SELECT route_id, admission_token, status, terminal_fingerprint
         FROM inference_invocations
         WHERE user_id = ? AND invocation_id = ? LIMIT 1",
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
        Ok::<_, sqlx::Error>(PersistedInvocationAdmissionFact {
            route_id: row.try_get("route_id")?,
            admission_token: row.try_get("admission_token")?,
            status: row.try_get("status")?,
            terminal_fingerprint: row.try_get("terminal_fingerprint")?,
        })
    })
    .transpose()
    .map_err(|error| {
        ServiceError::with_source(
            ServiceErrorKind::Persistence,
            "decode inference invocation admission fact",
            error,
        )
    })
}

fn existing_invocation_error(
    plan: &InferenceInvocationPlan,
    persisted: &PersistedInvocationAdmissionFact,
) -> ServiceError {
    ServiceError::conflict(format!(
        "inference invocation {} already exists with status {status}; provider delivery must not be repeated",
        plan.invocation_id,
        status = persisted.status,
    ))
}

fn validate_ambiguous_invocation_admission(
    persisted: &PersistedInvocationAdmissionFact,
    plan: &InferenceInvocationPlan,
) -> ServiceResult<()> {
    let mut mismatches = Vec::new();
    if persisted.route_id != plan.route_id {
        mismatches.push("route_id");
    }
    if persisted.admission_token != plan.admission_token {
        mismatches.push("admission_token");
    }
    if !mismatches.is_empty() {
        return Err(ServiceError::conflict(format!(
            "inference invocation {} commit resolved to a different admission owner: {}",
            plan.invocation_id,
            mismatches.join(", ")
        )));
    }
    if persisted.status == "admitted" && persisted.terminal_fingerprint.is_none() {
        return Ok(());
    }
    Err(ServiceError::conflict(format!(
        "inference invocation {} commit resolved to status {} with terminal fingerprint {}; provider delivery is not authorized",
        plan.invocation_id,
        persisted.status,
        if persisted.terminal_fingerprint.is_some() {
            "present"
        } else {
            "missing"
        }
    )))
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
    if let Some(persisted) = load_invocation_admission_fact(db, plan).await? {
        return Err(existing_invocation_error(plan, &persisted));
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
              admission_token, turn_index,
              round_index, operation_id, logical_attempt, purpose, status, created_at, terminal_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'admitted', NOW(6), NULL)",
        )
        .bind(&plan.invocation_id)
        .bind(&plan.route_id)
        .bind(&plan.input.user_id)
        .bind(plan.input.scope.session_id())
        .bind(plan.input.scope.kind())
        .bind(plan.input.scope.run_id())
        .bind(plan.input.scope.harness_run_id())
        .bind(&plan.admission_token)
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
        match load_invocation_admission_fact(db, plan).await {
            Ok(Some(persisted)) => return Err(existing_invocation_error(plan, &persisted)),
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
    let Err(error) = tx.commit().await else {
        return Ok(());
    };
    let commit_error = ServiceError::with_source(
        ServiceErrorKind::Persistence,
        "commit inference admission",
        error,
    );
    match load_invocation_admission_fact(db, plan).await {
        Ok(Some(persisted)) => validate_ambiguous_invocation_admission(&persisted, plan),
        Ok(None) => Err(commit_error),
        Err(read_error) => {
            tracing::warn!(
                invocation_id = %plan.invocation_id,
                %read_error,
                "inference admission commit is unresolved after authoritative re-read failed"
            );
            Err(commit_error)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PersistedProviderAttemptFact {
    invocation_id: String,
    attempt_index: i64,
    provider: String,
    admission_token: String,
    provider_protocol: String,
    provider_wire_hash: String,
    provider_wire_bytes: i64,
    status: String,
    terminal_fingerprint: Option<String>,
}

async fn load_provider_attempt_fact(
    db: &sqlx::Pool<sqlx::MySql>,
    attempt: &InferenceProviderAttemptPlan,
) -> ServiceResult<Option<PersistedProviderAttemptFact>> {
    sqlx::query(
        "SELECT invocation_id, attempt_index, provider, admission_token, provider_protocol,
                provider_wire_hash, provider_wire_bytes, status, terminal_fingerprint
         FROM inference_provider_attempts
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
        Ok::<_, sqlx::Error>(PersistedProviderAttemptFact {
            invocation_id: row.try_get("invocation_id")?,
            attempt_index: row.try_get("attempt_index")?,
            provider: row.try_get("provider")?,
            admission_token: row.try_get("admission_token")?,
            provider_protocol: row.try_get("provider_protocol")?,
            provider_wire_hash: row.try_get("provider_wire_hash")?,
            provider_wire_bytes: row.try_get("provider_wire_bytes")?,
            status: row.try_get("status")?,
            terminal_fingerprint: row.try_get("terminal_fingerprint")?,
        })
    })
    .transpose()
    .map_err(|error| {
        ServiceError::with_source(
            ServiceErrorKind::Persistence,
            "decode inference provider attempt fact",
            error,
        )
    })
}

fn validate_persisted_provider_attempt_identity(
    persisted: &PersistedProviderAttemptFact,
    attempt: &InferenceProviderAttemptPlan,
    provider_wire_bytes: i64,
) -> ServiceResult<()> {
    let mut mismatches = Vec::new();
    if persisted.invocation_id != attempt.invocation_id {
        mismatches.push("invocation_id");
    }
    if persisted.attempt_index != i64::from(attempt.attempt_index) {
        mismatches.push("attempt_index");
    }
    if persisted.provider != attempt.provider {
        mismatches.push("provider");
    }
    if persisted.admission_token != attempt.admission_token {
        mismatches.push("admission_token");
    }
    if persisted.provider_protocol != attempt.wire.protocol {
        mismatches.push("provider_protocol");
    }
    if persisted.provider_wire_hash != attempt.wire.provider_wire_hash {
        mismatches.push("provider_wire_hash");
    }
    if persisted.provider_wire_bytes != provider_wire_bytes {
        mismatches.push("provider_wire_bytes");
    }
    if mismatches.is_empty() {
        return Ok(());
    }
    Err(ServiceError::conflict(format!(
        "inference provider attempt {} has conflicting immutable fields: {}",
        attempt.attempt_id,
        mismatches.join(", ")
    )))
}

fn validate_ambiguous_provider_attempt_admission(
    persisted: &PersistedProviderAttemptFact,
    attempt: &InferenceProviderAttemptPlan,
    provider_wire_bytes: i64,
) -> ServiceResult<()> {
    validate_persisted_provider_attempt_identity(persisted, attempt, provider_wire_bytes)?;
    if persisted.status == "started" && persisted.terminal_fingerprint.is_none() {
        return Ok(());
    }
    Err(ServiceError::conflict(format!(
        "inference provider attempt {} commit resolved to status {} with terminal fingerprint {}; provider delivery is not authorized",
        attempt.attempt_id,
        persisted.status,
        if persisted.terminal_fingerprint.is_some() {
            "present"
        } else {
            "missing"
        }
    )))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PersistedProviderTerminalMatch {
    Started,
    ExactTerminal,
}

fn classify_persisted_provider_terminal(
    persisted: &PersistedProviderAttemptFact,
    attempt: &InferenceProviderAttemptPlan,
    provider_wire_bytes: i64,
    terminal: &InferenceInvocationTerminal,
    fingerprint: &str,
) -> ServiceResult<PersistedProviderTerminalMatch> {
    validate_persisted_provider_attempt_identity(persisted, attempt, provider_wire_bytes)?;
    if persisted.status == "started" && persisted.terminal_fingerprint.is_none() {
        return Ok(PersistedProviderTerminalMatch::Started);
    }
    if persisted.status == terminal.status.as_str()
        && persisted.terminal_fingerprint.as_deref() == Some(fingerprint)
    {
        return Ok(PersistedProviderTerminalMatch::ExactTerminal);
    }
    Err(ServiceError::conflict(format!(
        "inference provider attempt {} terminal fact conflicts with its durable status/fingerprint",
        attempt.attempt_id
    )))
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
    let provider_wire_bytes = checked_i64(attempt.wire.provider_wire_bytes, "provider_wire_bytes")?;
    let db = pool.get();
    if let Some(persisted) = load_provider_attempt_fact(db, attempt).await? {
        validate_persisted_provider_attempt_identity(&persisted, attempt, provider_wire_bytes)?;
        return Err(ServiceError::conflict(format!(
            "inference provider attempt {} already exists with status {}; provider delivery must not be repeated",
            attempt.attempt_id, persisted.status
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
          provider, admission_token, provider_protocol, provider_wire_hash, provider_wire_bytes,
          status, started_at, terminal_at)
         SELECT ?, invocation_id, user_id, session_id, run_id, harness_run_id,
                ?, ?, ?, ?, ?, ?, 'started', NOW(6), NULL
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
    .bind(&attempt.admission_token)
    .bind(&attempt.wire.protocol)
    .bind(&attempt.wire.provider_wire_hash)
    .bind(provider_wire_bytes)
    .bind(&attempt.user_id)
    .bind(&attempt.invocation_id)
    .execute(&mut *tx)
    .await;
    match result {
        Ok(result) if result.rows_affected() == 1 => {
            if let Err(error) = insert_model_request_context_event(
                &mut tx,
                attempt,
                ModelRequestEventStage::Accepted,
                None,
            )
            .await
            {
                rollback_inference_tx(tx, "record accepted model request context").await;
                return Err(error);
            }
            let Err(error) = tx.commit().await else {
                return Ok(());
            };
            let commit_error = ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "commit inference provider attempt admission",
                error,
            );
            match load_provider_attempt_fact(db, attempt).await {
                Ok(Some(persisted)) => validate_ambiguous_provider_attempt_admission(
                    &persisted,
                    attempt,
                    provider_wire_bytes,
                ),
                Ok(None) => Err(commit_error),
                Err(read_error) => {
                    tracing::warn!(
                        attempt_id = %attempt.attempt_id,
                        %read_error,
                        "provider attempt admission commit is unresolved after authoritative re-read failed"
                    );
                    Err(commit_error)
                }
            }
        }
        Ok(_) => Err(ServiceError::conflict(format!(
            "inference invocation {} is not admitted for provider attempt {}",
            attempt.invocation_id, attempt.attempt_id
        ))),
        Err(error) => {
            rollback_inference_tx(tx, "begin_inference_provider_attempt").await;
            if let Some(persisted) = load_provider_attempt_fact(db, attempt).await? {
                validate_persisted_provider_attempt_identity(
                    &persisted,
                    attempt,
                    provider_wire_bytes,
                )?;
                Err(ServiceError::conflict(format!(
                    "inference provider attempt {} already exists with status {}; provider delivery must not be repeated",
                    attempt.attempt_id, persisted.status
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

async fn record_successful_attempt_debt_if_needed(
    db: &sqlx::Pool<sqlx::MySql>,
    attempt: &InferenceProviderAttemptPlan,
    terminal: &InferenceInvocationTerminal,
    terminal_state: &DurableInferenceTerminal,
) -> ServiceResult<()> {
    if terminal.status == InferenceTerminalStatus::Succeeded {
        record_inference_settlement_debt(
            db,
            &attempt.user_id,
            &attempt.invocation_id,
            terminal_state,
            SettlementDebtMode::RequireQuiescent,
        )
        .await?;
    }
    Ok(())
}

async fn recover_provider_terminal_after_unknown_write(
    db: &sqlx::Pool<sqlx::MySql>,
    attempt: &InferenceProviderAttemptPlan,
    provider_wire_bytes: i64,
    terminal: &InferenceInvocationTerminal,
    terminal_state: &DurableInferenceTerminal,
    fingerprint: &str,
    original_error: ServiceError,
) -> ServiceResult<()> {
    match load_provider_attempt_fact(db, attempt).await {
        Ok(Some(persisted)) => match classify_persisted_provider_terminal(
            &persisted,
            attempt,
            provider_wire_bytes,
            terminal,
            fingerprint,
        )? {
            PersistedProviderTerminalMatch::ExactTerminal => {
                record_successful_attempt_debt_if_needed(db, attempt, terminal, terminal_state)
                    .await
            }
            PersistedProviderTerminalMatch::Started => Err(original_error),
        },
        Ok(None) => Err(original_error),
        Err(read_error) => {
            tracing::warn!(
                attempt_id = %attempt.attempt_id,
                %read_error,
                "provider attempt terminal write is unresolved after authoritative re-read failed"
            );
            Err(original_error)
        }
    }
}

pub async fn finish_inference_provider_attempt(
    pool: &SharedPool,
    attempt: &InferenceProviderAttemptPlan,
    terminal: &InferenceInvocationTerminal,
) -> ServiceResult<()> {
    let fingerprint = terminal_fingerprint(terminal)?;
    let terminal_state = DurableInferenceTerminal::from_terminal(terminal, fingerprint.clone())?;
    let provider_wire_bytes = checked_i64(attempt.wire.provider_wire_bytes, "provider_wire_bytes")?;
    let db = pool.get();
    if let Some(persisted) = load_provider_attempt_fact(db, attempt).await? {
        match classify_persisted_provider_terminal(
            &persisted,
            attempt,
            provider_wire_bytes,
            terminal,
            &fingerprint,
        )? {
            PersistedProviderTerminalMatch::ExactTerminal => {
                record_successful_attempt_debt_if_needed(db, attempt, terminal, &terminal_state)
                    .await?;
                return Ok(());
            }
            PersistedProviderTerminalMatch::Started => {}
        }
    }
    let update = sqlx::query(
        "UPDATE inference_provider_attempts
         SET status = ?, terminal_fingerprint = ?, provider_response_id = ?,
             input_tokens = ?, output_tokens = ?, cache_read_tokens = ?,
             cache_creation_tokens = ?, error_kind = ?, error_message = ?, terminal_at = NOW(6)
         WHERE user_id = ? AND attempt_id = ?
           AND invocation_id = ? AND attempt_index = ? AND provider = ?
           AND admission_token = ? AND provider_protocol = ?
           AND provider_wire_hash = ? AND provider_wire_bytes = ?
           AND status = 'started'",
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
    .bind(&attempt.attempt_id)
    .bind(&attempt.invocation_id)
    .bind(i64::from(attempt.attempt_index))
    .bind(&attempt.provider)
    .bind(&attempt.admission_token)
    .bind(&attempt.wire.protocol)
    .bind(&attempt.wire.provider_wire_hash)
    .bind(provider_wire_bytes);

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
        let result = match update.execute(&mut *tx).await {
            Ok(result) => result,
            Err(error) => {
                let write_error = ServiceError::with_source(
                    ServiceErrorKind::Persistence,
                    "finish inference provider attempt",
                    error,
                );
                rollback_inference_tx(tx, "finish successful inference provider attempt").await;
                return recover_provider_terminal_after_unknown_write(
                    db,
                    attempt,
                    provider_wire_bytes,
                    terminal,
                    &terminal_state,
                    &fingerprint,
                    write_error,
                )
                .await;
            }
        };
        if result.rows_affected() == 1
            && let Err(error) = write_inference_settlement_debt(
                &mut tx,
                &attempt.user_id,
                &attempt.invocation_id,
                &terminal_state,
            )
            .await
        {
            rollback_inference_tx(tx, "record successful inference settlement debt").await;
            return Err(error);
        }
        if result.rows_affected() == 1
            && let Err(error) = insert_model_request_context_event(
                &mut tx,
                attempt,
                ModelRequestEventStage::Terminal,
                Some(terminal),
            )
            .await
        {
            rollback_inference_tx(tx, "record successful model request context").await;
            return Err(error);
        }
        if let Err(error) = tx.commit().await {
            let commit_error = ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "commit successful inference provider terminal",
                error,
            );
            return recover_provider_terminal_after_unknown_write(
                db,
                attempt,
                provider_wire_bytes,
                terminal,
                &terminal_state,
                &fingerprint,
                commit_error,
            )
            .await;
        }
        result
    } else {
        let mut tx = db.begin().await.map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "begin inference provider terminal",
                error,
            )
        })?;
        match update.execute(&mut *tx).await {
            Ok(result) => {
                if result.rows_affected() == 1
                    && let Err(error) = insert_model_request_context_event(
                        &mut tx,
                        attempt,
                        ModelRequestEventStage::Terminal,
                        Some(terminal),
                    )
                    .await
                {
                    rollback_inference_tx(tx, "record terminal model request context").await;
                    return Err(error);
                }
                if let Err(error) = tx.commit().await {
                    let commit_error = ServiceError::with_source(
                        ServiceErrorKind::Persistence,
                        "commit inference provider terminal",
                        error,
                    );
                    return recover_provider_terminal_after_unknown_write(
                        db,
                        attempt,
                        provider_wire_bytes,
                        terminal,
                        &terminal_state,
                        &fingerprint,
                        commit_error,
                    )
                    .await;
                }
                result
            }
            Err(error) => {
                let write_error = ServiceError::with_source(
                    ServiceErrorKind::Persistence,
                    "finish inference provider attempt",
                    error,
                );
                rollback_inference_tx(tx, "finish inference provider attempt").await;
                return recover_provider_terminal_after_unknown_write(
                    db,
                    attempt,
                    provider_wire_bytes,
                    terminal,
                    &terminal_state,
                    &fingerprint,
                    write_error,
                )
                .await;
            }
        }
    };
    if result.rows_affected() == 1 {
        return Ok(());
    }
    if let Some(persisted) = load_provider_attempt_fact(db, attempt).await? {
        return match classify_persisted_provider_terminal(
            &persisted,
            attempt,
            provider_wire_bytes,
            terminal,
            &fingerprint,
        )? {
            PersistedProviderTerminalMatch::ExactTerminal => {
                record_successful_attempt_debt_if_needed(db, attempt, terminal, &terminal_state)
                    .await
            }
            PersistedProviderTerminalMatch::Started => Err(ServiceError::conflict(format!(
                "inference provider attempt {} terminal update changed no started row",
                attempt.attempt_id
            ))),
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
    fn invocation_identity_changes_with_inference_purpose() {
        let primary = plan_inference_invocation(input()).expect("primary plan");
        let mut changed = input();
        changed.purpose = InferencePurpose::SubAgent;
        assert_ne!(
            primary.invocation_id,
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
    fn physical_request_identity_is_attempt_scoped_and_wire_exact() {
        let invocation = plan_inference_invocation(input()).expect("invocation plan");
        let wire = InferenceProviderWireIdentity::new(
            "openai_compatible",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            4_096,
        )
        .expect("exact wire identity");
        let first = plan_inference_provider_attempt(&invocation, 0, wire.clone());
        let retry = plan_inference_provider_attempt(&invocation, 1, wire.clone());

        assert_ne!(first.request_id(), retry.request_id());
        assert_eq!(first.wire(), &wire);
        assert_eq!(retry.wire(), &wire);
        assert_eq!(wire.protocol(), "openai_compatible");
        assert_eq!(
            wire.provider_wire_hash(),
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        );
        assert_eq!(wire.provider_wire_bytes(), 4_096);
    }

    #[test]
    fn independent_plans_have_distinct_admission_fences_for_the_same_logical_identity() {
        let first_invocation = plan_inference_invocation(input()).expect("first invocation");
        let second_invocation = plan_inference_invocation(input()).expect("second invocation");
        assert_eq!(
            first_invocation.invocation_id,
            second_invocation.invocation_id
        );
        assert_eq!(first_invocation.route_id, second_invocation.route_id);
        assert_ne!(
            first_invocation.admission_token,
            second_invocation.admission_token
        );

        let wire = InferenceProviderWireIdentity::new(
            "openai_compatible",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            4_096,
        )
        .expect("wire identity");
        let first_attempt = plan_inference_provider_attempt(&first_invocation, 0, wire.clone());
        let second_attempt = plan_inference_provider_attempt(&second_invocation, 0, wire);
        assert_eq!(first_attempt.attempt_id, second_attempt.attempt_id);
        assert_ne!(
            first_attempt.admission_token,
            second_attempt.admission_token
        );
    }

    #[test]
    fn ambiguous_invocation_admission_requires_the_plans_fencing_token() {
        let plan = plan_inference_invocation(input()).expect("invocation");
        let exact = PersistedInvocationAdmissionFact {
            route_id: plan.route_id.clone(),
            admission_token: plan.admission_token.clone(),
            status: "admitted".to_string(),
            terminal_fingerprint: None,
        };
        validate_ambiguous_invocation_admission(&exact, &plan)
            .expect("the commit belongs to this admission plan");

        let mut different_owner = exact.clone();
        different_owner.admission_token = new_admission_token();
        assert_eq!(
            validate_ambiguous_invocation_admission(&different_owner, &plan)
                .expect_err("another admission owner must not authorize provider delivery")
                .kind,
            ServiceErrorKind::Conflict
        );

        let mut terminal = exact;
        terminal.status = "failed".to_string();
        terminal.terminal_fingerprint = Some("a".repeat(64));
        assert_eq!(
            validate_ambiguous_invocation_admission(&terminal, &plan)
                .expect_err("a terminal invocation cannot authorize provider delivery")
                .kind,
            ServiceErrorKind::Conflict
        );
    }

    fn provider_attempt_fact(
        attempt: &InferenceProviderAttemptPlan,
        status: &str,
        terminal_fingerprint: Option<&str>,
    ) -> PersistedProviderAttemptFact {
        PersistedProviderAttemptFact {
            invocation_id: attempt.invocation_id.clone(),
            attempt_index: i64::from(attempt.attempt_index),
            provider: attempt.provider.clone(),
            admission_token: attempt.admission_token.clone(),
            provider_protocol: attempt.wire.protocol.clone(),
            provider_wire_hash: attempt.wire.provider_wire_hash.clone(),
            provider_wire_bytes: i64::try_from(attempt.wire.provider_wire_bytes).unwrap(),
            status: status.to_string(),
            terminal_fingerprint: terminal_fingerprint.map(str::to_string),
        }
    }

    fn exact_provider_attempt() -> InferenceProviderAttemptPlan {
        let invocation = plan_inference_invocation(input()).expect("invocation plan");
        plan_inference_provider_attempt(
            &invocation,
            0,
            InferenceProviderWireIdentity::new(
                "openai_compatible",
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                4_096,
            )
            .expect("exact wire identity"),
        )
    }

    #[test]
    fn ambiguous_attempt_admission_accepts_only_the_exact_started_wire_fact() {
        let attempt = exact_provider_attempt();
        let exact = provider_attempt_fact(&attempt, "started", None);
        validate_ambiguous_provider_attempt_admission(&exact, &attempt, 4_096)
            .expect("the row committed by the ambiguous admission is authoritative");

        let mut wrong_invocation = exact.clone();
        wrong_invocation.invocation_id.push_str("-other");
        let mut wrong_index = exact.clone();
        wrong_index.attempt_index += 1;
        let mut wrong_provider = exact.clone();
        wrong_provider.provider.push_str("-other");
        let mut wrong_owner = exact.clone();
        wrong_owner.admission_token = new_admission_token();
        let mut wrong_protocol = exact.clone();
        wrong_protocol.provider_protocol = "anthropic_messages".to_string();
        let mut wrong_wire = exact.clone();
        wrong_wire.provider_wire_hash = "f".repeat(64);
        let mut wrong_wire_bytes = exact.clone();
        wrong_wire_bytes.provider_wire_bytes += 1;
        for drifted in [
            wrong_invocation,
            wrong_index,
            wrong_provider,
            wrong_owner,
            wrong_protocol,
            wrong_wire,
            wrong_wire_bytes,
        ] {
            assert_eq!(
                validate_ambiguous_provider_attempt_admission(&drifted, &attempt, 4_096)
                    .expect_err("any immutable identity drift must fail closed")
                    .kind,
                ServiceErrorKind::Conflict
            );
        }

        let terminal = provider_attempt_fact(&attempt, "failed", Some(&"a".repeat(64)));
        assert_eq!(
            validate_ambiguous_provider_attempt_admission(&terminal, &attempt, 4_096)
                .expect_err("a terminal row cannot authorize provider redelivery")
                .kind,
            ServiceErrorKind::Conflict
        );
    }

    #[test]
    fn ambiguous_attempt_terminal_requires_exact_wire_status_and_fingerprint() {
        let attempt = exact_provider_attempt();
        let terminal = InferenceInvocationTerminal {
            status: InferenceTerminalStatus::DeliveryUnknown,
            usage: InferenceUsage {
                input_tokens: 200,
                output_tokens: 50,
                cache_read_tokens: 800,
                cache_creation_tokens: 100,
            },
            provider_response_id: Some("provider-partial".to_string()),
            error_kind: Some("stream_transport".to_string()),
            error_message: Some("partial delivery".to_string()),
        };
        let fingerprint = terminal_fingerprint(&terminal).expect("terminal fingerprint");
        let exact = provider_attempt_fact(&attempt, terminal.status.as_str(), Some(&fingerprint));
        assert_eq!(
            classify_persisted_provider_terminal(&exact, &attempt, 4_096, &terminal, &fingerprint,)
                .expect("the exact terminal replay is authoritative"),
            PersistedProviderTerminalMatch::ExactTerminal
        );
        assert_eq!(
            classify_persisted_provider_terminal(
                &provider_attempt_fact(&attempt, "started", None),
                &attempt,
                4_096,
                &terminal,
                &fingerprint,
            )
            .expect("a still-open row is distinguishable from an exact terminal"),
            PersistedProviderTerminalMatch::Started
        );

        let wrong_fingerprint =
            provider_attempt_fact(&attempt, terminal.status.as_str(), Some(&"b".repeat(64)));
        assert_eq!(
            classify_persisted_provider_terminal(
                &wrong_fingerprint,
                &attempt,
                4_096,
                &terminal,
                &fingerprint,
            )
            .expect_err("a different terminal fact must fail closed")
            .kind,
            ServiceErrorKind::Conflict
        );
    }

    #[test]
    fn provider_wire_identity_rejects_ambiguous_or_fabricated_values() {
        let hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert!(InferenceProviderWireIdentity::new("", hash, 1).is_err());
        assert!(InferenceProviderWireIdentity::new("openai compatible", hash, 1).is_err());
        assert!(
            InferenceProviderWireIdentity::new(
                "openai_compatible",
                "0123456789ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef",
                1,
            )
            .is_err()
        );
        assert!(InferenceProviderWireIdentity::new("openai_compatible", "abcd", 1).is_err());
        assert!(InferenceProviderWireIdentity::new("openai_compatible", hash, 0).is_err());
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

    #[test]
    fn model_request_event_uses_exact_causal_facts_and_partitions_usage() {
        let invocation = plan_inference_invocation(input()).expect("invocation");
        let mut seed = ModelRequestContextSeed::server_default();
        seed.topology = ModelRequestTopology::CliServer;
        seed.interaction_owner = "cli".to_string();
        seed.budget.estimated_input_tokens = Some(900);
        seed.cache.current_identity = Some("sha256:stable-prefix".to_string());
        let wire = InferenceProviderWireIdentity::new(
            "openai_compatible",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            4_096,
        )
        .expect("wire")
        .with_composition(ModelRequestWireComposition {
            system_bytes: 100,
            conversation_bytes: 3_000,
            tool_schema_bytes: 500,
            provider_envelope_bytes: 496,
            system_items: 1,
            conversation_items: 40,
            tool_schema_items: 8,
        });
        let attempt = plan_inference_provider_attempt_with_context(&invocation, 2, wire, seed);
        let terminal = InferenceInvocationTerminal::succeeded(
            InferenceUsage {
                input_tokens: 1_000,
                output_tokens: 80,
                cache_read_tokens: 600,
                cache_creation_tokens: 100,
            },
            Some("provider-response".to_string()),
        );

        let (accepted_id, accepted_json, accepted) =
            model_request_event(&attempt, ModelRequestEventStage::Accepted, None)
                .expect("accepted event");
        let (terminal_id, terminal_json, terminal_event) =
            model_request_event(&attempt, ModelRequestEventStage::Terminal, Some(&terminal))
                .expect("terminal event");

        assert_ne!(accepted_id, terminal_id);
        assert!(accepted.usage.is_none());
        assert_eq!(terminal_event.identity.physical_attempt, 2);
        assert_eq!(
            terminal_event.identity.provider_wire_hash,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        assert_eq!(
            terminal_event.usage,
            Some(ModelRequestUsage {
                fresh_input_tokens: 300,
                cache_read_tokens: 600,
                cache_creation_tokens: 100,
                request_input_tokens: 1_000,
                output_tokens: 80,
            })
        );
        assert_eq!(terminal_event.budget.estimate_error_tokens, Some(100));
        assert_eq!(terminal_event.cache.cache_read_share, Some(0.6));
        assert_eq!(terminal_event.wire_composition.system_bytes, 100);
        assert!(!accepted_json.contains("provider-response"));
        assert!(terminal_json.contains(MODEL_REQUEST_CONTEXT_SCHEMA));

        for status in [
            InferenceTerminalStatus::Succeeded,
            InferenceTerminalStatus::Failed,
            InferenceTerminalStatus::Cancelled,
            InferenceTerminalStatus::DeliveryUnknown,
        ] {
            let mut outcome = terminal.clone();
            outcome.status = status;
            let (_, _, event) =
                model_request_event(&attempt, ModelRequestEventStage::Terminal, Some(&outcome))
                    .expect("every durable provider terminal has one context event");
            assert_eq!(event.terminal_status.as_deref(), Some(status.as_str()));
            assert_eq!(
                event.usage.as_ref().map(|usage| usage.request_input_tokens),
                Some(1_000)
            );
        }
    }
}
