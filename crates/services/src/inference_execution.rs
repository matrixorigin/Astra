use astra_core::{SharedPool, matrixone_statement_with_null_shape};
use astra_turn_types::{InferenceInvocationScope, InferencePurpose};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::Row;

use crate::model_request_context::{
    MODEL_REQUEST_CONTEXT_SCHEMA, ModelRequestContextEvent, ModelRequestContextScope,
    ModelRequestContextSeed, ModelRequestEventStage, ModelRequestIdentity, ModelRequestTopology,
    ModelRequestUsage, ModelRequestWireComposition, compact_model_request_context_scope,
};
use crate::models::{ModelAccessKind, ModelExecutionPlacement, validate_model_offering_id};
use crate::service_error::{ServiceError, ServiceErrorKind, ServiceResult};

const INFERENCE_ID_HEX_LEN: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InferenceInvocationInput {
    pub user_id: String,
    pub scope: InferenceInvocationScope,
    /// Exact durable execution capability for a run-scoped invocation.
    ///
    /// Session/harness work has its own owner boundary and must leave this
    /// empty. A run-scoped provider admission is executable only while all of
    /// these immutable facts still match in the same transaction that inserts
    /// the invocation row.
    pub run_authority: Option<InferenceRunAdmissionAuthority>,
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
pub struct InferenceRunAdmissionAuthority {
    pub expected_owner_generation: u64,
    pub expected_owner_pod_id: String,
    /// Greatest durable run-event index through which this executor has
    /// applied user control. `-1` means no event boundary was applied.
    pub expected_control_epoch: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InferenceInvocationPlan {
    route_id: String,
    invocation_id: String,
    admission_token: String,
    owner_token: String,
    owner_generation: u64,
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
    owner_token: String,
    owner_generation: u64,
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

    /// Opaque owner identity for the current durable lease generation.
    #[must_use]
    pub fn owner_token(&self) -> &str {
        &self.owner_token
    }

    /// Monotonic fencing generation. Recovery increments this before it can
    /// settle an abandoned invocation, so an old process can never publish a
    /// late provider or logical terminal.
    #[must_use]
    pub fn owner_generation(&self) -> u64 {
        self.owner_generation
    }

    /// Stable attempt identity within the caller-owned inference round.
    ///
    /// The runtime may advance this once after conclusively settling an
    /// ambiguous pre-provider admission. Exposing the authoritative value
    /// prevents an outer provider retry from reusing the recovered identity.
    #[must_use]
    pub fn logical_attempt(&self) -> u32 {
        self.input.scope.logical_attempt()
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
    /// Provider-normalized input buckets. Fresh input, cache reads, and cache
    /// creation are disjoint and have one shared representation everywhere.
    pub input: astra_turn_types::NormalizedPromptCacheUsage,
    pub output_tokens: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferenceUsageStatus {
    /// The provider supplied terminal usage for the complete response.
    ProviderExact,
    /// The provider supplied usage before an interrupted/uncertain terminal.
    ProviderPartial,
    /// No provider usage fact was available. Numeric zeroes are placeholders,
    /// not measured zero-token billing.
    #[default]
    Unavailable,
}

impl InferenceUsageStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProviderExact => "provider_exact",
            Self::ProviderPartial => "provider_partial",
            Self::Unavailable => "unavailable",
        }
    }
}

/// Provider-I/O authority at an exact-attempt settlement boundary.
///
/// `PreDelivery` proves the runtime never allowed the HTTP request to leave;
/// an absent durable attempt row is therefore a valid cancelled outcome.
/// `DeliveryAuthorized` means an absent row is a durable conflict and must be
/// quarantined. Legacy rows use an internal `unknown` state and never get
/// synthesized through this API.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InferenceProviderDeliveryState {
    PreDelivery,
    DeliveryAuthorized,
}

/// Authoritative outcome of a logical-invocation admission whose caller lost
/// the database acknowledgement.
///
/// Resolution is performed while holding the same durable scope locks as the
/// original admission transaction, so `Absent` cannot race a late commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InferenceInvocationAdmissionResolution {
    Settled,
    ExactTerminal,
    ConflictingIdentity,
    ScopeUnavailable,
    /// The exact ambiguous identity was conclusively closed, but the caller's
    /// run generation/owner/lease/control capability is no longer live. The
    /// caller must not create a replacement logical attempt.
    AuthorityLost,
}

/// Authoritative result of projecting one exact durable settlement decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InferenceSettlementReconcileOutcome {
    /// The canonical invocation carries the exact requested terminal.
    Settled,
    /// Durable debt remains retryable; the bounded runtime owner or sweeper
    /// may attempt the same indexed identity again.
    TransientPending,
    /// The debt was retained as an operator-visible incident and removed from
    /// the retry set. Runtime owners must release capacity instead of hammering
    /// a state that requires repair.
    PermanentlyQuarantined,
}

impl InferenceProviderDeliveryState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PreDelivery => "pre_delivery",
            Self::DeliveryAuthorized => "delivery_authorized",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InferenceInvocationTerminal {
    pub status: InferenceTerminalStatus,
    pub usage: InferenceUsage,
    pub usage_status: InferenceUsageStatus,
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
            usage_status: InferenceUsageStatus::ProviderExact,
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
    match (&input.scope, &input.run_authority) {
        (InferenceInvocationScope::Run { .. }, Some(authority)) => {
            validate_identity(
                &authority.expected_owner_pod_id,
                "expected_owner_pod_id",
                128,
            )?;
            if authority.expected_control_epoch < -1 {
                return Err(ServiceError::invalid(
                    "expected_control_epoch must be at least -1",
                ));
            }
            i64::try_from(authority.expected_owner_generation).map_err(|_| {
                ServiceError::invalid("expected_owner_generation exceeds the durable BIGINT range")
            })?;
        }
        (InferenceInvocationScope::Run { .. }, None) => {
            return Err(ServiceError::invalid(
                "run-scoped inference requires exact durable execution authority",
            ));
        }
        (_, Some(_)) => {
            return Err(ServiceError::invalid(
                "run execution authority cannot cross a session or harness scope",
            ));
        }
        (_, None) => {}
    }
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
        owner_token: new_admission_token(),
        owner_generation: 1,
        input,
    })
}

/// Return the first unused two-attempt allocation after durable history for
/// one exact logical operation. `N` is the requested identity and `N + 1` is
/// reserved for the single foreground ambiguity recovery. This is a cursor
/// read, not provider authority: concurrent callers are still serialized by
/// the unique invocation identity at admission and losers must re-read.
pub async fn next_inference_logical_attempt_pair_base(
    pool: &SharedPool,
    input: &InferenceInvocationInput,
) -> ServiceResult<u32> {
    // Reuse the complete public input validation contract before allowing a
    // cursor lookup to influence durable identity selection.
    let _ = plan_inference_invocation(input.clone())?;
    let row = sqlx::query(
        "SELECT MAX(logical_attempt) AS max_logical_attempt
         FROM inference_invocations
         WHERE user_id = ?
           AND scope_kind = ?
           AND session_id <=> ?
           AND run_id <=> ?
           AND harness_run_id <=> ?
           AND turn_index <=> ?
           AND round_index <=> ?
           AND operation_id = ?
           AND purpose = ?",
    )
    .bind(&input.user_id)
    .bind(input.scope.kind())
    .bind(input.scope.session_id())
    .bind(input.scope.run_id())
    .bind(input.scope.harness_run_id())
    .bind(input.scope.turn().map(i64::from))
    .bind(input.scope.round().map(i64::from))
    .bind(input.scope.operation_id())
    .bind(input.purpose.as_str())
    .fetch_one(pool.get())
    .await
    .map_err(|error| {
        ServiceError::with_source(
            ServiceErrorKind::Persistence,
            "read durable inference logical-attempt cursor",
            error,
        )
    })?;
    let Some(max_logical_attempt) = row
        .try_get::<Option<i64>, _>("max_logical_attempt")
        .map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "decode durable inference logical-attempt cursor",
                error,
            )
        })?
    else {
        return Ok(0);
    };
    let max_logical_attempt = u32::try_from(max_logical_attempt).map_err(|_| {
        ServiceError::conflict(
            "durable inference logical-attempt history is outside the supported u32 range",
        )
    })?;
    max_logical_attempt
        .checked_div(2)
        .and_then(|pair| pair.checked_add(1))
        .and_then(|pair| pair.checked_mul(2))
        .ok_or_else(|| {
            ServiceError::conflict("durable inference logical-attempt pair space is exhausted")
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
        owner_token: invocation.owner_token.clone(),
        owner_generation: invocation.owner_generation,
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
        "usage_status": terminal.usage_status,
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
        let measured = terminal.usage.input.total_input_tokens();
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
            input: terminal.usage.input,
            output_tokens: terminal.usage.output_tokens,
        }
    });
    let mut cache = attempt.request_context.cache.clone();
    if let Some(usage) = usage.as_ref() {
        let total_input_tokens = usage.total_input_tokens();
        cache.cache_read_share = (total_input_tokens > 0)
            .then_some(usage.input.cache_read_tokens as f64 / total_input_tokens as f64);
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
        usage_status: terminal.map(|terminal| terminal.usage_status.as_str().to_string()),
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
    let context_expired_at = sqlx::query(
        "SELECT context_expired_at
         FROM inference_provider_attempts
         WHERE user_id = ? AND attempt_id = ?
         FOR UPDATE",
    )
    .bind(&attempt.user_id)
    .bind(&attempt.attempt_id)
    .fetch_optional(&mut *connection)
    .await
    .map_err(|error| {
        ServiceError::with_source(
            ServiceErrorKind::Persistence,
            "lock inference provider attempt for model request context",
            error,
        )
    })?
    .ok_or_else(|| {
        ServiceError::conflict(format!(
            "inference provider attempt {} is unavailable for model request context",
            attempt.attempt_id
        ))
    })?
    .try_get::<Option<chrono::NaiveDateTime>, _>("context_expired_at")
    .map_err(|error| {
        ServiceError::with_source(
            ServiceErrorKind::Persistence,
            "decode inference provider attempt context expiry",
            error,
        )
    })?;
    insert_model_request_context_event_with_expiry(
        connection,
        attempt,
        stage,
        terminal,
        context_expired_at,
    )
    .await
}

async fn insert_model_request_context_event_with_expiry(
    connection: &mut sqlx::MySqlConnection,
    attempt: &InferenceProviderAttemptPlan,
    stage: ModelRequestEventStage,
    terminal: Option<&InferenceInvocationTerminal>,
    context_expired_at: Option<chrono::NaiveDateTime>,
) -> ServiceResult<()> {
    let (event_id, event_json, event) = model_request_event(attempt, stage, terminal)?;
    let usage = event.usage.as_ref();
    let model_family = attempt
        .request_context
        .model_family
        .as_deref()
        .unwrap_or("unspecified");
    if context_expired_at.is_none() {
        let session_id = attempt.invocation_input.scope.session_id();
        let run_id = attempt.invocation_input.scope.run_id();
        let harness_run_id = attempt.invocation_input.scope.harness_run_id();
        let terminal_status = event.terminal_status.as_deref();
        let usage_present = usage.is_some();
        let insert_sql = matrixone_statement_with_null_shape(
            "INSERT INTO model_request_context_events
         (event_id, user_id, attempt_id, invocation_id, session_id, run_id, harness_run_id,
          event_stage, terminal_status, topology, provider, model_family, purpose,
          input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
          event_json, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW(6))",
            [
                session_id.is_some(),
                run_id.is_some(),
                harness_run_id.is_some(),
                terminal_status.is_some(),
                usage_present,
                usage_present,
                usage_present,
                usage_present,
            ],
        );
        sqlx::query(&insert_sql)
            .bind(event_id)
            .bind(&attempt.user_id)
            .bind(&attempt.attempt_id)
            .bind(&attempt.invocation_id)
            .bind(session_id)
            .bind(run_id)
            .bind(harness_run_id)
            .bind(stage.as_str())
            .bind(terminal_status)
            .bind(event.identity.topology.as_str())
            .bind(&event.identity.provider)
            .bind(model_family)
            .bind(&event.identity.inference_purpose)
            .bind(checked_optional_i64(
                usage.map(ModelRequestUsage::total_input_tokens),
                "model request input_tokens",
            )?)
            .bind(checked_optional_i64(
                usage.map(|usage| usage.output_tokens),
                "model request output_tokens",
            )?)
            .bind(checked_optional_i64(
                usage.map(|usage| usage.input.cache_read_tokens),
                "model request cache_read_tokens",
            )?)
            .bind(checked_optional_i64(
                usage.map(|usage| usage.input.cache_creation_tokens),
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
        let scope = match &attempt.invocation_input.scope {
            InferenceInvocationScope::Run { session_id, .. }
            | InferenceInvocationScope::Session { session_id, .. } => {
                ModelRequestContextScope::Session(session_id)
            }
            InferenceInvocationScope::HarnessRun { harness_run_id, .. } => {
                ModelRequestContextScope::HarnessRun(harness_run_id)
            }
        };
        compact_model_request_context_scope(connection, &attempt.user_id, scope).await?;
    }
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
            usage.total_input_tokens(),
            "model request metric input_tokens",
        )?)
        .bind(checked_i64(
            usage.output_tokens,
            "model request metric output_tokens",
        )?)
        .bind(checked_i64(
            usage.input.cache_read_tokens,
            "model request metric cache_read_tokens",
        )?)
        .bind(checked_i64(
            usage.input.cache_creation_tokens,
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
    Ok(())
}

async fn insert_recovered_model_request_terminal(
    db: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    invocation_id: &str,
    attempt_id: &str,
    terminal: &DurableInferenceTerminal,
) -> Result<(), sqlx::Error> {
    let mut tx = db.begin().await?;
    insert_recovered_model_request_terminal_tx(
        &mut tx,
        user_id,
        invocation_id,
        attempt_id,
        terminal,
    )
    .await?;
    tx.commit().await
}

async fn insert_recovered_model_request_terminal_tx(
    connection: &mut sqlx::MySqlConnection,
    user_id: &str,
    invocation_id: &str,
    attempt_id: &str,
    terminal: &DurableInferenceTerminal,
) -> Result<(), sqlx::Error> {
    let accepted = sqlx::query(
        "SELECT event_json, model_family
         FROM model_request_context_events
         WHERE user_id = ? AND invocation_id = ? AND attempt_id = ?
           AND event_stage = 'accepted'",
    )
    .bind(user_id)
    .bind(invocation_id)
    .bind(attempt_id)
    .fetch_optional(&mut *connection)
    .await?;
    let Some(accepted) = accepted else {
        return Err(sqlx::Error::Protocol(format!(
            "exact provider terminal recovery has no accepted request-context event for {user_id}/{invocation_id}/{attempt_id}"
        )));
    };
    let accepted_json = accepted.try_get::<String, _>("event_json")?;
    let model_family = accepted.try_get::<String, _>("model_family")?;
    let mut event = serde_json::from_str::<ModelRequestContextEvent>(&accepted_json)
        .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
    event.stage = ModelRequestEventStage::Terminal;
    event.identity.provider_response_id = terminal.provider_response_id.clone();
    let fresh_input_tokens = u64::try_from(terminal.input_tokens)
        .map_err(|_| sqlx::Error::Protocol("negative recovered fresh input tokens".to_string()))?;
    let cache_read_tokens = u64::try_from(terminal.cache_read_tokens)
        .map_err(|_| sqlx::Error::Protocol("negative recovered cache-read tokens".to_string()))?;
    let cache_creation_tokens = u64::try_from(terminal.cache_creation_tokens).map_err(|_| {
        sqlx::Error::Protocol("negative recovered cache-creation tokens".to_string())
    })?;
    let output_tokens = u64::try_from(terminal.output_tokens)
        .map_err(|_| sqlx::Error::Protocol("negative recovered output tokens".to_string()))?;
    let usage = ModelRequestUsage {
        input: astra_turn_types::NormalizedPromptCacheUsage::new(
            fresh_input_tokens,
            cache_read_tokens,
            cache_creation_tokens,
        ),
        output_tokens,
    };
    let measured = usage.total_input_tokens();
    event.budget.measured_input_tokens = Some(measured);
    event.budget.usage_source = Some("provider_terminal_recovery".to_string());
    if let Some(estimated) = event.budget.estimated_input_tokens {
        let error = i128::from(measured) - i128::from(estimated);
        event.budget.estimate_error_tokens =
            Some(i64::try_from(error).unwrap_or(if error.is_negative() {
                i64::MIN
            } else {
                i64::MAX
            }));
        event.budget.estimate_error_ratio =
            (estimated > 0).then_some(error as f64 / estimated as f64);
    }
    event.cache.cache_read_share =
        (measured > 0).then_some(usage.input.cache_read_tokens as f64 / measured as f64);
    event.usage = Some(usage.clone());
    event.terminal_status = Some(terminal.status.clone());
    event.usage_status = Some(terminal.usage_status.clone());
    event.error_kind = terminal.error_kind.clone();
    let event_json =
        serde_json::to_string(&event).map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
    let event_id = hash_identity(
        "mrctx",
        &[attempt_id, ModelRequestEventStage::Terminal.as_str()],
    );
    let total_input_tokens = terminal
        .input_tokens
        .checked_add(terminal.cache_read_tokens)
        .and_then(|total| total.checked_add(terminal.cache_creation_tokens))
        .ok_or_else(|| sqlx::Error::Protocol("recovered input token total overflow".to_string()))?;
    let existing_terminal = sqlx::query(
        "SELECT event_json FROM model_request_context_events
         WHERE event_id = ? FOR UPDATE",
    )
    .bind(&event_id)
    .fetch_optional(&mut *connection)
    .await?;
    if let Some(existing_terminal) = existing_terminal {
        let existing_json = existing_terminal.try_get::<String, _>("event_json")?;
        let existing_event = serde_json::from_str::<ModelRequestContextEvent>(&existing_json)
            .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
        let exact_usage = existing_event.usage.as_ref().is_some_and(|existing| {
            existing.input.fresh_input_tokens == fresh_input_tokens
                && existing.input.cache_read_tokens == cache_read_tokens
                && existing.input.cache_creation_tokens == cache_creation_tokens
                && existing.output_tokens == output_tokens
        });
        let exact_terminal = existing_event.stage == ModelRequestEventStage::Terminal
            && existing_event.terminal_status.as_deref() == Some(terminal.status.as_str())
            && existing_event.usage_status.as_deref() == Some(terminal.usage_status.as_str())
            && existing_event.identity.provider_response_id == terminal.provider_response_id
            && existing_event.error_kind == terminal.error_kind
            && exact_usage;
        return if exact_terminal {
            Ok(())
        } else {
            Err(sqlx::Error::Protocol(format!(
                "recovered model request terminal {event_id} conflicts with its append-only event"
            )))
        };
    }
    sqlx::query(
        "INSERT INTO model_request_context_events
         (event_id, user_id, attempt_id, invocation_id, session_id, run_id, harness_run_id,
          event_stage, terminal_status, topology, provider, model_family, purpose,
          input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
          event_json, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, 'terminal', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW(6))",
    )
    .bind(event_id)
    .bind(user_id)
    .bind(attempt_id)
    .bind(invocation_id)
    .bind(event.identity.session_id.as_deref())
    .bind(event.identity.run_id.as_deref())
    .bind(event.identity.harness_run_id.as_deref())
    .bind(&terminal.status)
    .bind(event.identity.topology.as_str())
    .bind(&event.identity.provider)
    .bind(&model_family)
    .bind(&event.identity.inference_purpose)
    .bind(total_input_tokens)
    .bind(terminal.output_tokens)
    .bind(terminal.cache_read_tokens)
    .bind(terminal.cache_creation_tokens)
    .bind(event_json)
    .execute(&mut *connection)
    .await?;
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
          cache_creation_tokens = cache_creation_tokens + VALUES(cache_creation_tokens),
          updated_at = NOW(6)",
    )
    .bind(model_request_metric_shard(attempt_id))
    .bind(event.identity.topology.as_str())
    .bind(&event.identity.provider)
    .bind(model_family)
    .bind(&event.identity.inference_purpose)
    .bind(&terminal.status)
    .bind(total_input_tokens)
    .bind(terminal.output_tokens)
    .bind(terminal.cache_read_tokens)
    .bind(terminal.cache_creation_tokens)
    .execute(&mut *connection)
    .await?;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InvocationScopeAuthority {
    Live,
    Unavailable,
}

async fn lock_invocation_scope_authority(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    input: &InferenceInvocationInput,
) -> ServiceResult<InvocationScopeAuthority> {
    // This is an ownership/lifecycle observation inside the admission
    // transaction. MatrixOne's supported row-lock primitive here is
    // `FOR UPDATE`; the shared storage primitive establishes the global
    // session -> tombstone -> execution-slot -> exact-run order before this
    // function observes run events. Keep those locks only through the durable
    // invocation insert. Sibling fanout admissions may briefly queue on this
    // scope lock, but provider I/O never occurs inside the transaction, so the
    // execution topology remains parallel. The durable logical-invocation row
    // remains the idempotency authority.
    let authority = match &input.scope {
        InferenceInvocationScope::Run {
            session_id, run_id, ..
        } => {
            let expected = input.run_authority.as_ref().ok_or_else(|| {
                ServiceError::invalid(
                    "run-scoped inference requires exact durable execution authority",
                )
            })?;
            let run_exists = match crate::storage::admit_session_scoped_run_write(
                tx,
                session_id,
                &input.user_id,
                run_id,
                false,
            )
            .await
            {
                Ok(run_exists) => run_exists,
                Err(sqlx::Error::RowNotFound) => false,
                Err(error) => {
                    return Err(ServiceError::with_source(
                        ServiceErrorKind::Persistence,
                        "lock canonical inference session/run scope",
                        error,
                    ));
                }
            };
            if !run_exists {
                InvocationScopeAuthority::Unavailable
            } else {
                let row = sqlx::query(
                    "SELECT status, run_generation, owner_pod_id, last_event_idx,
                            CAST(cancellation_requested_at IS NOT NULL AS SIGNED)
                                AS cancellation_requested,
                            CAST(CASE WHEN owner_lease_expires_at >= NOW(6) THEN 1 ELSE 0 END AS SIGNED)
                                AS owner_lease_active
                     FROM agent_runs
                     WHERE user_id = ? AND session_id = ? AND run_id = ?
                     LIMIT 1 FOR UPDATE",
                )
                .bind(&input.user_id)
                .bind(session_id)
                .bind(run_id)
                .fetch_optional(&mut **tx)
                .await
                .map_err(|error| {
                    ServiceError::with_source(
                        ServiceErrorKind::Persistence,
                        "verify inference run execution authority",
                        error,
                    )
                })?;
                let Some(row) = row else {
                    return Ok(InvocationScopeAuthority::Unavailable);
                };
                let status: String = row.try_get("status").map_err(|error| {
                    ServiceError::with_source(
                        ServiceErrorKind::Persistence,
                        "decode inference run status",
                        error,
                    )
                })?;
                let stored_generation: i64 = row.try_get("run_generation").map_err(|error| {
                    ServiceError::with_source(
                        ServiceErrorKind::Persistence,
                        "decode inference run generation",
                        error,
                    )
                })?;
                let owner_pod_id: Option<String> =
                    row.try_get("owner_pod_id").map_err(|error| {
                        ServiceError::with_source(
                            ServiceErrorKind::Persistence,
                            "decode inference run owner",
                            error,
                        )
                    })?;
                let last_event_idx: i64 = row.try_get("last_event_idx").map_err(|error| {
                    ServiceError::with_source(
                        ServiceErrorKind::Persistence,
                        "decode inference run control epoch",
                        error,
                    )
                })?;
                let lease_active: i64 = row.try_get("owner_lease_active").map_err(|error| {
                    ServiceError::with_source(
                        ServiceErrorKind::Persistence,
                        "decode inference run owner lease",
                        error,
                    )
                })?;
                let cancellation_requested: i64 =
                    row.try_get("cancellation_requested").map_err(|error| {
                        ServiceError::with_source(
                            ServiceErrorKind::Persistence,
                            "decode inference run cancellation fence",
                            error,
                        )
                    })?;
                let expected_generation = i64::try_from(expected.expected_owner_generation)
                    .map_err(|_| {
                        ServiceError::invalid(
                            "expected_owner_generation exceeds the durable BIGINT range",
                        )
                    })?;
                if expected.expected_control_epoch > last_event_idx {
                    return Err(ServiceError::invalid(format!(
                        "inference control epoch {} is ahead of durable run event index {}",
                        expected.expected_control_epoch, last_event_idx
                    )));
                }
                if status != "running"
                    || stored_generation != expected_generation
                    || owner_pod_id.as_deref() != Some(expected.expected_owner_pod_id.as_str())
                    || lease_active != 1
                    || cancellation_requested != 0
                {
                    InvocationScopeAuthority::Unavailable
                } else {
                    // The run row is held before its event range, preserving
                    // canonical session -> tombstone/slot -> run -> run-events
                    // lock order. Any
                    // later guidance is an execution fence. User cancellation
                    // is represented by the run row marker locked above;
                    // execution-owner cancellation atomically makes the row
                    // terminal, so neither requires an event-history probe.
                    let control_fence: Option<i64> = sqlx::query_scalar(
                        "SELECT event_idx FROM agent_run_events
                         WHERE user_id = ? AND run_id = ?
                           AND event_type = 'user_intent' AND event_idx > ?
                         ORDER BY event_idx ASC LIMIT 1",
                    )
                    .bind(&input.user_id)
                    .bind(run_id)
                    .bind(expected.expected_control_epoch)
                    .fetch_optional(&mut **tx)
                    .await
                    .map_err(|error| {
                        ServiceError::with_source(
                            ServiceErrorKind::Persistence,
                            "verify inference run control fence",
                            error,
                        )
                    })?;
                    if control_fence.is_some() {
                        InvocationScopeAuthority::Unavailable
                    } else {
                        InvocationScopeAuthority::Live
                    }
                }
            }
        }
        InferenceInvocationScope::Session { session_id, .. } => {
            match crate::storage::admit_session_event_write(tx, session_id, &input.user_id, false)
                .await
            {
                Ok(()) => InvocationScopeAuthority::Live,
                Err(sqlx::Error::RowNotFound) => InvocationScopeAuthority::Unavailable,
                Err(error) => {
                    return Err(ServiceError::with_source(
                        ServiceErrorKind::Persistence,
                        "lock canonical inference session scope",
                        error,
                    ));
                }
            }
        }
        InferenceInvocationScope::HarnessRun { harness_run_id, .. } => {
            let status = sqlx::query_scalar::<_, String>(
                "SELECT status FROM harness_runs
                 WHERE user_id = ? AND harness_run_id = ? LIMIT 1 FOR UPDATE",
            )
            .bind(&input.user_id)
            .bind(harness_run_id)
            .fetch_optional(&mut **tx)
            .await
            .map_err(|error| {
                ServiceError::with_source(
                    ServiceErrorKind::Persistence,
                    "verify inference harness scope",
                    error,
                )
            })?;
            match status.as_deref() {
                Some("running") => InvocationScopeAuthority::Live,
                Some(_) | None => InvocationScopeAuthority::Unavailable,
            }
        }
    };
    Ok(authority)
}

fn unavailable_scope_error(input: &InferenceInvocationInput) -> ServiceError {
    ServiceError::not_found(format!(
        "inference {} scope is unavailable or no longer owned by this execution",
        input.scope.kind()
    ))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PersistedInvocationAdmissionFact {
    route_id: String,
    admission_token: String,
    owner_token: String,
    owner_generation: i64,
    status: String,
    terminal_fingerprint: Option<String>,
}

async fn load_invocation_admission_fact(
    db: &sqlx::Pool<sqlx::MySql>,
    plan: &InferenceInvocationPlan,
) -> ServiceResult<Option<PersistedInvocationAdmissionFact>> {
    sqlx::query(
        "SELECT route_id, admission_token, owner_token, owner_generation,
                status, terminal_fingerprint
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
            owner_token: row.try_get("owner_token")?,
            owner_generation: row.try_get("owner_generation")?,
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
    if persisted.owner_token != plan.owner_token {
        mismatches.push("owner_token");
    }
    if persisted.owner_generation != i64::try_from(plan.owner_generation).unwrap_or(i64::MAX) {
        mismatches.push("owner_generation");
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
        if lock_invocation_scope_authority(&mut tx, &plan.input).await?
            != InvocationScopeAuthority::Live
        {
            return Err(unavailable_scope_error(&plan.input));
        }
        insert_inference_invocation_admission(&mut tx, plan).await
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

async fn insert_inference_invocation_admission(
    connection: &mut sqlx::MySqlConnection,
    plan: &InferenceInvocationPlan,
) -> ServiceResult<()> {
    // Keep the enum discriminant and its owner coordinates in one SQL shape.
    // A generic row with three independently nullable owner binds asks the
    // database/driver to reconstruct the Rust enum under concurrency and has
    // produced impossible mixed rows at the CHECK boundary. Variant-specific
    // statements make every submitted row valid by construction; the CHECKs
    // remain defense in depth for non-Rust writers.
    let route_query = match &plan.input.scope {
        InferenceInvocationScope::Run {
            session_id, run_id, ..
        } => sqlx::query(
            "INSERT INTO inference_routes
                 (route_id, user_id, session_id, scope_kind, run_id,
                  offering_id, resolved_model_name, upstream_model_name, provider,
                  execution_placement, access_kind, purpose, created_at)
                 VALUES (?, ?, ?, 'run', ?, ?, ?, ?, ?, ?, ?, ?, NOW(6))",
        )
        .bind(&plan.route_id)
        .bind(&plan.input.user_id)
        .bind(session_id)
        .bind(run_id),
        InferenceInvocationScope::Session { session_id, .. } => sqlx::query(
            "INSERT INTO inference_routes
                 (route_id, user_id, session_id, scope_kind,
                  offering_id, resolved_model_name, upstream_model_name, provider,
                  execution_placement, access_kind, purpose, created_at)
                 VALUES (?, ?, ?, 'session', ?, ?, ?, ?, ?, ?, ?, NOW(6))",
        )
        .bind(&plan.route_id)
        .bind(&plan.input.user_id)
        .bind(session_id),
        InferenceInvocationScope::HarnessRun { harness_run_id, .. } => sqlx::query(
            "INSERT INTO inference_routes
                 (route_id, user_id, scope_kind, harness_run_id,
                  offering_id, resolved_model_name, upstream_model_name, provider,
                  execution_placement, access_kind, purpose, created_at)
                 VALUES (?, ?, 'harness_run', ?, ?, ?, ?, ?, ?, ?, ?, NOW(6))",
        )
        .bind(&plan.route_id)
        .bind(&plan.input.user_id)
        .bind(harness_run_id),
    };
    route_query
        .bind(&plan.input.offering_id)
        .bind(&plan.input.resolved_model_name)
        .bind(&plan.input.upstream_model_name)
        .bind(&plan.input.provider)
        .bind(plan.input.execution_placement.as_str())
        .bind(plan.input.access_kind.as_str())
        .bind(plan.input.purpose.as_str())
        .execute(&mut *connection)
        .await
        .map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "insert inference route",
                error,
            )
        })?;

    let invocation_query = match &plan.input.scope {
        InferenceInvocationScope::Run {
            session_id, run_id, ..
        } => sqlx::query(
            "INSERT INTO inference_invocations
                 (invocation_id, route_id, user_id, session_id, scope_kind, run_id,
                  admission_token, owner_token, owner_generation, owner_lease_expires_at,
                  turn_index, round_index, operation_id, logical_attempt, purpose, status,
                  terminal_fingerprint, usage_status, provider_delivery_state, created_at, terminal_at)
                 VALUES (?, ?, ?, ?, 'run', ?, ?, ?, ?,
                         DATE_ADD(NOW(6), INTERVAL 60 SECOND), ?, ?, ?, ?, ?,
                         'admitted', NULL, 'unavailable', 'unknown', NOW(6), NULL)",
        )
        .bind(&plan.invocation_id)
        .bind(&plan.route_id)
        .bind(&plan.input.user_id)
        .bind(session_id)
        .bind(run_id),
        InferenceInvocationScope::Session { session_id, .. } => sqlx::query(
            "INSERT INTO inference_invocations
                 (invocation_id, route_id, user_id, session_id, scope_kind,
                  admission_token, owner_token, owner_generation, owner_lease_expires_at,
                  turn_index, round_index, operation_id, logical_attempt, purpose, status,
                  terminal_fingerprint, usage_status, provider_delivery_state, created_at, terminal_at)
                 VALUES (?, ?, ?, ?, 'session', ?, ?, ?,
                         DATE_ADD(NOW(6), INTERVAL 60 SECOND), ?, ?, ?, ?, ?,
                         'admitted', NULL, 'unavailable', 'unknown', NOW(6), NULL)",
        )
        .bind(&plan.invocation_id)
        .bind(&plan.route_id)
        .bind(&plan.input.user_id)
        .bind(session_id),
        InferenceInvocationScope::HarnessRun { harness_run_id, .. } => sqlx::query(
            "INSERT INTO inference_invocations
                 (invocation_id, route_id, user_id, scope_kind, harness_run_id,
                  admission_token, owner_token, owner_generation, owner_lease_expires_at,
                  operation_id, logical_attempt, purpose, status, terminal_fingerprint,
                  usage_status, provider_delivery_state, created_at, terminal_at)
                 VALUES (?, ?, ?, 'harness_run', ?, ?, ?, ?,
                         DATE_ADD(NOW(6), INTERVAL 60 SECOND), ?, ?, ?,
                         'admitted', NULL, 'unavailable', 'unknown', NOW(6), NULL)",
        )
        .bind(&plan.invocation_id)
        .bind(&plan.route_id)
        .bind(&plan.input.user_id)
        .bind(harness_run_id),
    };
    let invocation_query = invocation_query
        .bind(&plan.admission_token)
        .bind(&plan.owner_token)
        .bind(i64::try_from(plan.owner_generation).map_err(|_| {
            ServiceError::invalid("inference owner generation exceeds the durable BIGINT range")
        })?);
    let invocation_query = match &plan.input.scope {
        InferenceInvocationScope::Run { turn, round, .. }
        | InferenceInvocationScope::Session { turn, round, .. } => invocation_query
            .bind(i64::from(*turn))
            .bind(i64::from(*round)),
        InferenceInvocationScope::HarnessRun { .. } => invocation_query,
    };
    invocation_query
        .bind(plan.input.scope.operation_id())
        .bind(i64::from(plan.input.scope.logical_attempt()))
        .bind(plan.input.purpose.as_str())
        .execute(&mut *connection)
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

/// Recover an admission whose caller stopped waiting before the database
/// acknowledgement arrived, without inferring a rollback from a negative read.
///
/// If the original transaction rolled back, this transaction inserts the exact
/// same logical identity. If it committed, the durable invocation row and its
/// admission token decide the outcome. Scope validation uses the same short
/// `FOR UPDATE` lock as the original transaction, so deletion cannot race the
/// recovery insert.
pub async fn settle_uncertain_inference_admission(
    pool: &SharedPool,
    plan: &InferenceInvocationPlan,
    terminal: &InferenceInvocationTerminal,
) -> ServiceResult<InferenceInvocationAdmissionResolution> {
    if terminal.status != InferenceTerminalStatus::Cancelled
        || terminal.usage != InferenceUsage::default()
        || terminal.usage_status != InferenceUsageStatus::Unavailable
        || terminal.provider_response_id.is_some()
    {
        return Err(ServiceError::invalid(
            "an uncertain logical admission may settle only as a zero-usage pre-provider cancellation",
        ));
    }
    let fingerprint = terminal_fingerprint(terminal)?;
    let durable_terminal = DurableInferenceTerminal::from_terminal(terminal, fingerprint.clone())?;
    let db = pool.get();
    let mut tx = db.begin().await.map_err(|error| {
        ServiceError::with_source(
            ServiceErrorKind::Persistence,
            "begin inference admission recovery",
            error,
        )
    })?;
    let scope_authority = match lock_invocation_scope_authority(&mut tx, &plan.input).await {
        Ok(authority) => authority,
        Err(error) => {
            rollback_inference_tx(tx, "settle_uncertain_inference_admission").await;
            return Err(error);
        }
    };
    let row = sqlx::query(
        "SELECT route_id, admission_token, owner_token, owner_generation,
                status, terminal_fingerprint,
                IF(owner_lease_expires_at > NOW(6), 1, 0) AS lease_live
         FROM inference_invocations
         WHERE user_id = ? AND invocation_id = ? LIMIT 1 FOR UPDATE",
    )
    .bind(&plan.input.user_id)
    .bind(&plan.invocation_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|error| {
        ServiceError::with_source(
            ServiceErrorKind::Persistence,
            "read inference admission resolution",
            error,
        )
    })?;
    let mut resolution = match row {
        None => {
            if scope_authority == InvocationScopeAuthority::Live {
                insert_inference_invocation_admission(&mut tx, plan).await?;
                InferenceInvocationAdmissionResolution::Settled
            } else {
                InferenceInvocationAdmissionResolution::ScopeUnavailable
            }
        }
        Some(row) => {
            let lease_live = row.try_get::<i64, _>("lease_live").map_err(|error| {
                ServiceError::with_source(
                    ServiceErrorKind::Persistence,
                    "decode resolved inference owner lease",
                    error,
                )
            })? == 1;
            let persisted = PersistedInvocationAdmissionFact {
                route_id: row.try_get("route_id").map_err(|error| {
                    ServiceError::with_source(
                        ServiceErrorKind::Persistence,
                        "decode resolved inference route",
                        error,
                    )
                })?,
                admission_token: row.try_get("admission_token").map_err(|error| {
                    ServiceError::with_source(
                        ServiceErrorKind::Persistence,
                        "decode resolved inference admission token",
                        error,
                    )
                })?,
                owner_token: row.try_get("owner_token").map_err(|error| {
                    ServiceError::with_source(
                        ServiceErrorKind::Persistence,
                        "decode resolved inference owner token",
                        error,
                    )
                })?,
                owner_generation: row.try_get("owner_generation").map_err(|error| {
                    ServiceError::with_source(
                        ServiceErrorKind::Persistence,
                        "decode resolved inference owner generation",
                        error,
                    )
                })?,
                status: row.try_get("status").map_err(|error| {
                    ServiceError::with_source(
                        ServiceErrorKind::Persistence,
                        "decode resolved inference status",
                        error,
                    )
                })?,
                terminal_fingerprint: row.try_get("terminal_fingerprint").map_err(|error| {
                    ServiceError::with_source(
                        ServiceErrorKind::Persistence,
                        "decode resolved inference terminal fingerprint",
                        error,
                    )
                })?,
            };
            if persisted.route_id != plan.route_id
                || persisted.admission_token != plan.admission_token
                || persisted.owner_token != plan.owner_token
                || persisted.owner_generation
                    != i64::try_from(plan.owner_generation).unwrap_or(i64::MAX)
            {
                InferenceInvocationAdmissionResolution::ConflictingIdentity
            } else if persisted.status == "admitted" && persisted.terminal_fingerprint.is_none() {
                if lease_live {
                    InferenceInvocationAdmissionResolution::Settled
                } else {
                    InferenceInvocationAdmissionResolution::ConflictingIdentity
                }
            } else if persisted.status == terminal.status.as_str()
                && persisted.terminal_fingerprint.as_deref() == Some(fingerprint.as_str())
            {
                InferenceInvocationAdmissionResolution::ExactTerminal
            } else {
                InferenceInvocationAdmissionResolution::ConflictingIdentity
            }
        }
    };
    let wrote_settlement_debt = resolution == InferenceInvocationAdmissionResolution::Settled;
    if wrote_settlement_debt {
        write_inference_settlement_debt(
            &mut tx,
            &plan.input.user_id,
            &plan.invocation_id,
            &durable_terminal,
            None,
            ProviderDeliveryState::PreDelivery,
        )
        .await?;
    }
    if scope_authority == InvocationScopeAuthority::Unavailable
        && matches!(
            resolution,
            InferenceInvocationAdmissionResolution::Settled
                | InferenceInvocationAdmissionResolution::ExactTerminal
        )
    {
        resolution = InferenceInvocationAdmissionResolution::AuthorityLost;
    }
    let Err(error) = tx.commit().await else {
        return Ok(resolution);
    };
    let commit_error = ServiceError::with_source(
        ServiceErrorKind::Persistence,
        "commit uncertain inference admission settlement",
        error,
    );
    if !wrote_settlement_debt {
        return Err(commit_error);
    }
    match inference_settlement_debt_matches(
        db,
        &plan.input.user_id,
        &plan.invocation_id,
        &durable_terminal,
        None,
        ProviderDeliveryState::PreDelivery,
    )
    .await
    {
        Ok(true) => Ok(resolution),
        Ok(false) => Err(commit_error),
        Err(read_error) => {
            tracing::warn!(
                invocation_id = %plan.invocation_id,
                %read_error,
                "uncertain inference admission settlement commit could not be confirmed"
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

fn validate_first_provider_attempt_binding(
    invocation: &InferenceInvocationPlan,
    attempt: &InferenceProviderAttemptPlan,
) -> ServiceResult<()> {
    let mut mismatches = Vec::new();
    if attempt.invocation_id != invocation.invocation_id {
        mismatches.push("invocation_id");
    }
    if attempt.user_id != invocation.input.user_id {
        mismatches.push("user_id");
    }
    if attempt.provider != invocation.input.provider {
        mismatches.push("provider");
    }
    if attempt.invocation_input != invocation.input {
        mismatches.push("invocation_input");
    }
    if mismatches.is_empty() {
        return Ok(());
    }
    Err(ServiceError::invalid(format!(
        "first provider attempt does not belong to inference invocation {}: {}",
        invocation.invocation_id,
        mismatches.join(", ")
    )))
}

async fn insert_inference_provider_attempt_admission(
    connection: &mut sqlx::MySqlConnection,
    attempt: &InferenceProviderAttemptPlan,
    provider_wire_bytes: i64,
) -> ServiceResult<()> {
    let result = sqlx::query(
        "INSERT INTO inference_provider_attempts
         (attempt_id, invocation_id, user_id, session_id, run_id, harness_run_id, attempt_index,
          provider, admission_token, provider_protocol, provider_wire_hash, provider_wire_bytes,
          status, usage_status, started_at, terminal_at)
         SELECT ?, invocation_id, user_id, session_id, run_id, harness_run_id,
                ?, ?, ?, ?, ?, ?, 'started', 'unavailable', NOW(6), NULL
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
    .execute(&mut *connection)
    .await
    .map_err(|error| {
        ServiceError::with_source(
            ServiceErrorKind::Persistence,
            "insert inference provider attempt",
            error,
        )
    })?;
    if result.rows_affected() != 1 {
        return Err(ServiceError::conflict(format!(
            "inference invocation {} is not admitted for provider attempt {}",
            attempt.invocation_id, attempt.attempt_id
        )));
    }
    // This row was inserted immediately above in the same transaction, so its
    // context-expiry fact is exactly NULL. Re-reading and locking it would add
    // a database round trip without adding any concurrency protection.
    insert_model_request_context_event_with_expiry(
        connection,
        attempt,
        ModelRequestEventStage::Accepted,
        None,
        None,
    )
    .await
}

async fn validate_ambiguous_invocation_with_first_attempt_admission(
    db: &sqlx::Pool<sqlx::MySql>,
    invocation: &InferenceInvocationPlan,
    attempt: &InferenceProviderAttemptPlan,
    provider_wire_bytes: i64,
) -> ServiceResult<()> {
    let invocation_fact = load_invocation_admission_fact(db, invocation)
        .await?
        .ok_or_else(|| {
            ServiceError::conflict(format!(
                "inference invocation {} is missing after an ambiguous combined admission commit",
                invocation.invocation_id
            ))
        })?;
    validate_ambiguous_invocation_admission(&invocation_fact, invocation)?;
    let attempt_fact = load_provider_attempt_fact(db, attempt)
        .await?
        .ok_or_else(|| {
            ServiceError::conflict(format!(
                "inference provider attempt {} is missing after an ambiguous combined admission commit",
                attempt.attempt_id
            ))
        })?;
    validate_ambiguous_provider_attempt_admission(&attempt_fact, attempt, provider_wire_bytes)
}

/// Atomically admit a logical invocation and its first physical provider
/// request. Success is returned only after the route, invocation, attempt, and
/// accepted request-context event are all durable, so provider I/O never
/// observes a partial admission.
pub async fn admit_inference_invocation_with_first_provider_attempt(
    pool: &SharedPool,
    invocation: &InferenceInvocationPlan,
    attempt: &InferenceProviderAttemptPlan,
) -> ServiceResult<()> {
    validate_first_provider_attempt_binding(invocation, attempt)?;
    let provider_wire_bytes = checked_i64(attempt.wire.provider_wire_bytes, "provider_wire_bytes")?;
    let db = pool.get();
    let mut tx = db.begin().await.map_err(|error| {
        ServiceError::with_source(
            ServiceErrorKind::Persistence,
            "begin combined inference admission",
            error,
        )
    })?;
    let write_result: ServiceResult<()> = async {
        insert_inference_invocation_admission(&mut tx, invocation).await?;
        insert_inference_provider_attempt_admission(&mut tx, attempt, provider_wire_bytes).await
    }
    .await;
    if let Err(error) = write_result {
        rollback_inference_tx(tx, "admit_inference_invocation_with_first_provider_attempt").await;
        match load_invocation_admission_fact(db, invocation).await {
            Ok(Some(persisted)) => return Err(existing_invocation_error(invocation, &persisted)),
            Ok(None) => {}
            Err(read_error) => {
                tracing::warn!(
                    invocation_id = %invocation.invocation_id,
                    %error,
                    %read_error,
                    "combined inference admission failed and invocation re-read also failed"
                );
                return Err(error);
            }
        }
        match load_provider_attempt_fact(db, attempt).await {
            Ok(Some(persisted)) => {
                validate_persisted_provider_attempt_identity(
                    &persisted,
                    attempt,
                    provider_wire_bytes,
                )?;
                return Err(ServiceError::conflict(format!(
                    "inference provider attempt {} already exists with status {}; provider delivery must not be repeated",
                    attempt.attempt_id, persisted.status
                )));
            }
            Ok(None) => {}
            Err(read_error) => {
                tracing::warn!(
                    attempt_id = %attempt.attempt_id,
                    %error,
                    %read_error,
                    "combined inference admission failed and provider-attempt re-read also failed"
                );
            }
        }
        return Err(error);
    }
    let Err(error) = tx.commit().await else {
        return Ok(());
    };
    let commit_error = ServiceError::with_source(
        ServiceErrorKind::Persistence,
        "commit combined inference admission",
        error,
    );
    match validate_ambiguous_invocation_with_first_attempt_admission(
        db,
        invocation,
        attempt,
        provider_wire_bytes,
    )
    .await
    {
        Ok(()) => Ok(()),
        Err(read_error) => {
            tracing::warn!(
                invocation_id = %invocation.invocation_id,
                attempt_id = %attempt.attempt_id,
                %read_error,
                "combined inference admission commit is unresolved after authoritative re-read"
            );
            Err(commit_error)
        }
    }
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
    owner_token: &str,
    owner_generation: u64,
    action: &'static str,
) -> ServiceResult<()> {
    let owner_generation = i64::try_from(owner_generation).map_err(|_| {
        ServiceError::invalid("inference owner generation exceeds the durable BIGINT range")
    })?;
    let row = sqlx::query(
        "SELECT status, owner_token, owner_generation,
                IF(owner_lease_expires_at > NOW(6), 1, 0) AS lease_live
         FROM inference_invocations
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
    })?;
    let Some(row) = row else {
        return Err(ServiceError::conflict(format!(
            "inference invocation {invocation_id} is unavailable; cannot {action}"
        )));
    };
    let status = row.try_get::<String, _>("status").map_err(|error| {
        ServiceError::with_source(
            ServiceErrorKind::Persistence,
            "decode locked inference invocation status",
            error,
        )
    })?;
    let durable_owner_token = row.try_get::<String, _>("owner_token").map_err(|error| {
        ServiceError::with_source(
            ServiceErrorKind::Persistence,
            "decode locked inference owner token",
            error,
        )
    })?;
    let durable_owner_generation = row.try_get::<i64, _>("owner_generation").map_err(|error| {
        ServiceError::with_source(
            ServiceErrorKind::Persistence,
            "decode locked inference owner generation",
            error,
        )
    })?;
    let lease_live = row.try_get::<i64, _>("lease_live").map_err(|error| {
        ServiceError::with_source(
            ServiceErrorKind::Persistence,
            "decode locked inference owner lease",
            error,
        )
    })? == 1;
    if durable_owner_token != owner_token || durable_owner_generation != owner_generation {
        return Err(ServiceError::conflict(format!(
            "inference invocation {invocation_id} belongs to a different owner generation; cannot {action}"
        )));
    }
    match status.as_str() {
        "admitted" if lease_live => Ok(()),
        "admitted" => Err(ServiceError::conflict(format!(
            "inference invocation {invocation_id} owner lease expired; cannot {action}"
        ))),
        status => Err(ServiceError::conflict(format!(
            "inference invocation {invocation_id} is {status}; cannot {action}"
        ))),
    }
}

/// Extend the current owner's lease using the database clock. An expired lease
/// is never resurrected: recovery owns the next generation once the deadline
/// passes, even if the old process later regains connectivity.
pub async fn renew_inference_invocation_owner(
    pool: &SharedPool,
    plan: &InferenceInvocationPlan,
) -> ServiceResult<()> {
    let owner_generation = i64::try_from(plan.owner_generation).map_err(|_| {
        ServiceError::invalid("inference owner generation exceeds the durable BIGINT range")
    })?;
    let updated = sqlx::query(
        "UPDATE inference_invocations
         SET owner_lease_expires_at = DATE_ADD(NOW(6), INTERVAL 60 SECOND)
         WHERE user_id = ? AND invocation_id = ? AND status = 'admitted'
           AND owner_token = ? AND owner_generation = ?
           AND owner_lease_expires_at > NOW(6)",
    )
    .bind(&plan.input.user_id)
    .bind(&plan.invocation_id)
    .bind(&plan.owner_token)
    .bind(owner_generation)
    .execute(pool.get())
    .await
    .map_err(|error| {
        ServiceError::with_source(
            ServiceErrorKind::Persistence,
            "renew inference owner lease",
            error,
        )
    })?;
    if updated.rows_affected() == 1 {
        Ok(())
    } else {
        Err(ServiceError::conflict(format!(
            "inference invocation {} owner lease is no longer renewable",
            plan.invocation_id
        )))
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
    if lock_invocation_scope_authority(&mut tx, &attempt.invocation_input).await?
        != InvocationScopeAuthority::Live
    {
        rollback_inference_tx(tx, "begin_inference_provider_attempt_scope_authority").await;
        return Err(unavailable_scope_error(&attempt.invocation_input));
    }
    // Provider-attempt admission is the final durable fence before HTTP. Keep
    // the canonical scope -> invocation lock order so a session deletion, run
    // owner transfer, lease expiry, or newer control fact cannot slip between
    // logical admission and physical delivery authorization.
    lock_admitted_inference_invocation(
        &mut tx,
        &attempt.user_id,
        &attempt.invocation_id,
        &attempt.owner_token,
        attempt.owner_generation,
        "begin a provider attempt",
    )
    .await?;
    sqlx::query(
        "UPDATE inference_invocations
         SET owner_lease_expires_at = DATE_ADD(NOW(6), INTERVAL 60 SECOND)
         WHERE user_id = ? AND invocation_id = ? AND status = 'admitted'
           AND owner_token = ? AND owner_generation = ?
           AND owner_lease_expires_at > NOW(6)",
    )
    .bind(&attempt.user_id)
    .bind(&attempt.invocation_id)
    .bind(&attempt.owner_token)
    .bind(i64::try_from(attempt.owner_generation).map_err(|_| {
        ServiceError::invalid("inference owner generation exceeds the durable BIGINT range")
    })?)
    .execute(&mut *tx)
    .await
    .map_err(|error| {
        ServiceError::with_source(
            ServiceErrorKind::Persistence,
            "extend inference owner lease before provider delivery",
            error,
        )
    })?;
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
        rollback_inference_tx(tx, "begin_provider_attempt_settlement_pending").await;
        return Err(ServiceError::conflict(format!(
            "inference invocation {} has a durable settlement decision; provider delivery must not be repeated",
            attempt.invocation_id
        )));
    }
    let provider_attempt_open = sqlx::query(
        "SELECT 1 FROM inference_provider_attempts
         WHERE user_id = ? AND invocation_id = ? AND status = 'started'
         LIMIT 1",
    )
    .bind(&attempt.user_id)
    .bind(&attempt.invocation_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|error| {
        ServiceError::with_source(
            ServiceErrorKind::Persistence,
            "check open inference provider attempt",
            error,
        )
    })?
    .is_some();
    if provider_attempt_open {
        rollback_inference_tx(tx, "begin_provider_attempt_already_open").await;
        return Err(ServiceError::conflict(format!(
            "inference invocation {} already has an open provider attempt; concurrent provider delivery is forbidden",
            attempt.invocation_id
        )));
    }
    let result = sqlx::query(
        "INSERT INTO inference_provider_attempts
         (attempt_id, invocation_id, user_id, session_id, run_id, harness_run_id, attempt_index,
          provider, admission_token, provider_protocol, provider_wire_hash, provider_wire_bytes,
          status, usage_status, started_at, terminal_at)
         SELECT ?, invocation_id, user_id, session_id, run_id, harness_run_id,
                ?, ?, ?, ?, ?, ?, 'started', 'unavailable', NOW(6), NULL
         FROM inference_invocations
         WHERE user_id = ? AND invocation_id = ? AND status = 'admitted'
           AND owner_token = ? AND owner_generation = ?
           AND owner_lease_expires_at > NOW(6)
           AND NOT EXISTS (
                SELECT 1
                FROM inference_invocation_settlement_debts AS settlement_debt
                WHERE settlement_debt.user_id = inference_invocations.user_id
                  AND settlement_debt.invocation_id = inference_invocations.invocation_id
           )
           AND NOT EXISTS (
                SELECT 1
                FROM inference_provider_attempts AS open_attempt
                WHERE open_attempt.user_id = inference_invocations.user_id
                  AND open_attempt.invocation_id = inference_invocations.invocation_id
                  AND open_attempt.status = 'started'
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
    .bind(&attempt.owner_token)
    .bind(i64::try_from(attempt.owner_generation).map_err(|_| {
        ServiceError::invalid("inference owner generation exceeds the durable BIGINT range")
    })?)
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
            InferenceSettlementDebtRequest {
                user_id: &attempt.user_id,
                invocation_id: &attempt.invocation_id,
                owner_token: &attempt.owner_token,
                owner_generation: attempt.owner_generation,
                terminal: terminal_state,
                provider_attempt_id: Some(&attempt.attempt_id),
                provider_delivery_state: ProviderDeliveryState::DeliveryAuthorized,
                mode: SettlementDebtMode::RequireQuiescent,
            },
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
    if let Some(persisted) = load_provider_attempt_fact(db, attempt).await?
        && classify_persisted_provider_terminal(
            &persisted,
            attempt,
            provider_wire_bytes,
            terminal,
            &fingerprint,
        )? == PersistedProviderTerminalMatch::ExactTerminal
    {
        return Ok(());
    }
    let update = sqlx::query(
        "UPDATE inference_provider_attempts
         SET status = ?, terminal_fingerprint = ?, provider_response_id = ?,
             usage_status = ?,
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
    .bind(&terminal_state.usage_status)
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
            &attempt.owner_token,
            attempt.owner_generation,
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
                Some(&attempt.attempt_id),
                ProviderDeliveryState::DeliveryAuthorized,
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
        lock_admitted_inference_invocation(
            &mut tx,
            &attempt.user_id,
            &attempt.invocation_id,
            &attempt.owner_token,
            attempt.owner_generation,
            "record a provider terminal",
        )
        .await?;
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
        "SELECT terminal_fingerprint, owner_token, owner_generation
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
            "load inference terminal fingerprint",
            error,
        )
    })?
    .map(|row| {
        let owner_token = row.try_get::<String, _>("owner_token").map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "decode inference terminal owner token",
                error,
            )
        })?;
        let owner_generation = row.try_get::<i64, _>("owner_generation").map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "decode inference terminal owner generation",
                error,
            )
        })?;
        if owner_token != plan.owner_token
            || owner_generation != i64::try_from(plan.owner_generation).unwrap_or(i64::MAX)
        {
            return Err(ServiceError::conflict(format!(
                "inference invocation {} terminal belongs to a different owner generation",
                plan.invocation_id
            )));
        }
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
    usage_status: String,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_creation_tokens: i64,
    provider_response_id: Option<String>,
    error_kind: Option<String>,
    error_message: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProviderDeliveryState {
    Unknown,
    PreDelivery,
    DeliveryAuthorized,
}

impl From<InferenceProviderDeliveryState> for ProviderDeliveryState {
    fn from(value: InferenceProviderDeliveryState) -> Self {
        match value {
            InferenceProviderDeliveryState::PreDelivery => Self::PreDelivery,
            InferenceProviderDeliveryState::DeliveryAuthorized => Self::DeliveryAuthorized,
        }
    }
}

impl ProviderDeliveryState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::PreDelivery => "pre_delivery",
            Self::DeliveryAuthorized => "delivery_authorized",
        }
    }
}

impl DurableInferenceTerminal {
    fn from_terminal(
        terminal: &InferenceInvocationTerminal,
        terminal_fingerprint: String,
    ) -> ServiceResult<Self> {
        Ok(Self {
            status: terminal.status.as_str().to_string(),
            terminal_fingerprint: Some(terminal_fingerprint),
            usage_status: terminal.usage_status.as_str().to_string(),
            input_tokens: checked_i64(
                terminal.usage.input.fresh_input_tokens,
                "terminal fresh_input_tokens",
            )?,
            output_tokens: checked_i64(terminal.usage.output_tokens, "terminal output_tokens")?,
            cache_read_tokens: checked_i64(
                terminal.usage.input.cache_read_tokens,
                "terminal cache_read_tokens",
            )?,
            cache_creation_tokens: checked_i64(
                terminal.usage.input.cache_creation_tokens,
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
            usage_status: row.try_get("usage_status")?,
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
    provider_attempt_id: Option<&str>,
    provider_delivery_state: ProviderDeliveryState,
) -> ServiceResult<()> {
    let fingerprint = terminal.terminal_fingerprint.as_deref().ok_or_else(|| {
        ServiceError::invalid("inference settlement debt requires a terminal fingerprint")
    })?;
    sqlx::query(
        "INSERT IGNORE INTO inference_invocation_settlement_debts
         (user_id, invocation_id, session_id, harness_run_id,
          terminal_status, terminal_fingerprint, usage_status,
          input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
          provider_response_id, error_kind, error_message, provider_attempt_id,
          provider_delivery_state)
         SELECT invocation.user_id, invocation.invocation_id,
                invocation.session_id, invocation.harness_run_id,
                ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?
         FROM inference_invocations AS invocation
         WHERE invocation.user_id = ? AND invocation.invocation_id = ?",
    )
    .bind(&terminal.status)
    .bind(fingerprint)
    .bind(&terminal.usage_status)
    .bind(terminal.input_tokens)
    .bind(terminal.output_tokens)
    .bind(terminal.cache_read_tokens)
    .bind(terminal.cache_creation_tokens)
    .bind(&terminal.provider_response_id)
    .bind(&terminal.error_kind)
    .bind(&terminal.error_message)
    .bind(provider_attempt_id)
    .bind(provider_delivery_state.as_str())
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
        "SELECT terminal_status, terminal_fingerprint, provider_attempt_id,
                provider_delivery_state
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
    let existing_attempt_id = existing
        .try_get::<Option<String>, _>("provider_attempt_id")
        .map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "decode inference settlement debt provider attempt",
                error,
            )
        })?;
    let existing_delivery_state = existing
        .try_get::<String, _>("provider_delivery_state")
        .map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "decode inference settlement debt delivery state",
                error,
            )
        })?;
    if existing_status != terminal.status || existing_fingerprint != fingerprint {
        return Err(ServiceError::conflict(format!(
            "inference invocation {invocation_id} already has a different durable settlement intent"
        )));
    }
    match (existing_attempt_id.as_deref(), provider_attempt_id) {
        (Some(existing), Some(requested)) if existing != requested => {
            Err(ServiceError::conflict(format!(
                "inference invocation {invocation_id} settlement names conflicting provider attempts"
            )))
        }
        (Some(_), Some(_)) if existing_delivery_state != provider_delivery_state.as_str() => {
            Err(ServiceError::conflict(format!(
                "inference invocation {invocation_id} settlement has conflicting provider delivery authority"
            )))
        }
        (Some(existing), Some(requested)) if existing == requested => Ok(()),
        (None, Some(requested)) => {
            sqlx::query(
                "UPDATE inference_invocation_settlement_debts
                 SET provider_attempt_id = ?, provider_delivery_state = ?
                 WHERE user_id = ? AND invocation_id = ?
                   AND terminal_fingerprint = ? AND provider_attempt_id IS NULL",
            )
            .bind(requested)
            .bind(provider_delivery_state.as_str())
            .bind(user_id)
            .bind(invocation_id)
            .bind(fingerprint)
            .execute(&mut **tx)
            .await
            .map_err(|error| {
                ServiceError::with_source(
                    ServiceErrorKind::Persistence,
                    "attach provider attempt to inference settlement debt",
                    error,
                )
            })?;
            Ok(())
        }
        (None, None)
            if existing_delivery_state == ProviderDeliveryState::Unknown.as_str()
                && provider_delivery_state == ProviderDeliveryState::PreDelivery
                && terminal.status == InferenceTerminalStatus::Cancelled.as_str()
                && terminal.usage_status == InferenceUsageStatus::Unavailable.as_str()
                && terminal.input_tokens == 0
                && terminal.output_tokens == 0
                && terminal.cache_read_tokens == 0
                && terminal.cache_creation_tokens == 0
                && terminal.provider_response_id.is_none() =>
        {
            let updated = sqlx::query(
                "UPDATE inference_invocation_settlement_debts
                 SET provider_delivery_state = 'pre_delivery'
                 WHERE user_id = ? AND invocation_id = ?
                   AND terminal_fingerprint = ? AND provider_attempt_id IS NULL
                   AND provider_delivery_state = 'unknown'",
            )
            .bind(user_id)
            .bind(invocation_id)
            .bind(fingerprint)
            .execute(&mut **tx)
            .await
            .map_err(|error| {
                ServiceError::with_source(
                    ServiceErrorKind::Persistence,
                    "strengthen pre-provider inference settlement authority",
                    error,
                )
            })?;
            if updated.rows_affected() == 1 {
                Ok(())
            } else {
                Err(ServiceError::conflict(format!(
                    "inference invocation {invocation_id} settlement delivery authority changed concurrently"
                )))
            }
        }
        (None, None)
            if existing_delivery_state == provider_delivery_state.as_str()
                || (existing_delivery_state == ProviderDeliveryState::PreDelivery.as_str()
                    && provider_delivery_state == ProviderDeliveryState::Unknown) =>
        {
            // A generic logical settlement carries no delivery fact. It may be
            // replayed after the stronger pre-delivery fact is established, but
            // must never downgrade that fact back to `unknown`.
            Ok(())
        }
        (Some(_), None) if provider_delivery_state == ProviderDeliveryState::Unknown => {
            // Exact attempt settlement is stronger than a replay of the same
            // logical terminal, so retain the exact durable owner.
            Ok(())
        }
        _ => Err(ServiceError::conflict(format!(
            "inference invocation {invocation_id} settlement has incompatible provider delivery authority"
        ))),
    }
}

async fn inference_settlement_debt_matches(
    db: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    invocation_id: &str,
    terminal: &DurableInferenceTerminal,
    provider_attempt_id: Option<&str>,
    provider_delivery_state: ProviderDeliveryState,
) -> ServiceResult<bool> {
    let row = sqlx::query(
        "SELECT terminal_status, terminal_fingerprint, provider_attempt_id,
                provider_delivery_state
         FROM inference_invocation_settlement_debts
         WHERE user_id = ? AND invocation_id = ?",
    )
    .bind(user_id)
    .bind(invocation_id)
    .fetch_optional(db)
    .await
    .map_err(|error| {
        ServiceError::with_source(
            ServiceErrorKind::Persistence,
            "re-read inference settlement debt",
            error,
        )
    })?;
    let Some(row) = row else {
        return Ok(false);
    };
    let status = row
        .try_get::<String, _>("terminal_status")
        .map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "decode re-read inference settlement status",
                error,
            )
        })?;
    let fingerprint = row
        .try_get::<String, _>("terminal_fingerprint")
        .map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "decode re-read inference settlement fingerprint",
                error,
            )
        })?;
    let attempt_id = row
        .try_get::<Option<String>, _>("provider_attempt_id")
        .map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "decode re-read inference settlement provider attempt",
                error,
            )
        })?;
    let delivery_state = row
        .try_get::<String, _>("provider_delivery_state")
        .map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "decode re-read inference settlement delivery state",
                error,
            )
        })?;
    let delivery_authority_matches = match (attempt_id.as_deref(), provider_attempt_id) {
        (Some(existing), Some(expected)) => {
            existing == expected && delivery_state == provider_delivery_state.as_str()
        }
        (None, Some(_)) => false,
        (None, None) if provider_delivery_state == ProviderDeliveryState::PreDelivery => {
            delivery_state == ProviderDeliveryState::PreDelivery.as_str()
        }
        (None, None) if provider_delivery_state == ProviderDeliveryState::Unknown => {
            delivery_state == ProviderDeliveryState::Unknown.as_str()
                || delivery_state == ProviderDeliveryState::PreDelivery.as_str()
        }
        (Some(_), None) => provider_delivery_state == ProviderDeliveryState::Unknown,
        (None, None) => false,
    };
    Ok(status == terminal.status
        && terminal.terminal_fingerprint.as_deref() == Some(fingerprint.as_str())
        && delivery_authority_matches)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SettlementDebtMode {
    RequireQuiescent,
    FenceOpenAttempts,
}

#[derive(Clone, Copy, Debug)]
struct InferenceSettlementDebtRequest<'a> {
    user_id: &'a str,
    invocation_id: &'a str,
    owner_token: &'a str,
    owner_generation: u64,
    terminal: &'a DurableInferenceTerminal,
    provider_attempt_id: Option<&'a str>,
    provider_delivery_state: ProviderDeliveryState,
    mode: SettlementDebtMode,
}

async fn record_inference_settlement_debt(
    db: &sqlx::Pool<sqlx::MySql>,
    request: InferenceSettlementDebtRequest<'_>,
) -> ServiceResult<()> {
    let InferenceSettlementDebtRequest {
        user_id,
        invocation_id,
        owner_token,
        owner_generation,
        terminal,
        provider_attempt_id,
        provider_delivery_state,
        mode,
    } = request;
    let mut tx = db.begin().await.map_err(|error| {
        ServiceError::with_source(
            ServiceErrorKind::Persistence,
            "begin inference settlement debt",
            error,
        )
    })?;
    let invocation = sqlx::query(
        "SELECT status, terminal_fingerprint, owner_token, owner_generation,
                IF(owner_lease_expires_at > NOW(6), 1, 0) AS lease_live
         FROM inference_invocations
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
    let durable_owner_token = invocation
        .try_get::<String, _>("owner_token")
        .map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "decode inference settlement owner token",
                error,
            )
        })?;
    let durable_owner_generation =
        invocation
            .try_get::<i64, _>("owner_generation")
            .map_err(|error| {
                ServiceError::with_source(
                    ServiceErrorKind::Persistence,
                    "decode inference settlement owner generation",
                    error,
                )
            })?;
    let expected_owner_generation = i64::try_from(owner_generation).map_err(|_| {
        ServiceError::invalid("inference owner generation exceeds the durable BIGINT range")
    })?;
    if durable_owner_token != owner_token || durable_owner_generation != expected_owner_generation {
        return Err(ServiceError::conflict(format!(
            "inference invocation {invocation_id} settlement belongs to a stale owner generation"
        )));
    }
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
    let lease_live = invocation
        .try_get::<i64, _>("lease_live")
        .map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "decode inference settlement owner lease",
                error,
            )
        })?
        == 1;
    if !lease_live {
        return Err(ServiceError::conflict(format!(
            "inference invocation {invocation_id} owner lease expired before settlement"
        )));
    }
    let successful_terminal = terminal.status == InferenceTerminalStatus::Succeeded.as_str();
    let terminal_fingerprint = if successful_terminal {
        Some(terminal.terminal_fingerprint.as_deref().ok_or_else(|| {
            ServiceError::invalid("successful inference settlement requires a terminal fingerprint")
        })?)
    } else {
        None
    };
    let attempt_state = if mode == SettlementDebtMode::RequireQuiescent || successful_terminal {
        Some(
            provider_attempt_settlement_state(
                &mut *tx,
                user_id,
                invocation_id,
                terminal_fingerprint,
            )
            .await
            .map_err(|error| {
                ServiceError::with_source(
                    ServiceErrorKind::Persistence,
                    "verify inference provider attempts under lifecycle lock",
                    error,
                )
            })?,
        )
    } else {
        None
    };
    if mode == SettlementDebtMode::RequireQuiescent
        && attempt_state.is_some_and(|state| state.has_open_attempt)
    {
        return Err(ServiceError::conflict(format!(
            "inference invocation {invocation_id} still has an active provider attempt"
        )));
    }
    if successful_terminal && !attempt_state.is_some_and(|state| state.successful_attempt_matches) {
        return Err(ServiceError::conflict(format!(
            "inference invocation {invocation_id} cannot succeed without a matching succeeded provider attempt"
        )));
    }
    write_inference_settlement_debt(
        &mut tx,
        user_id,
        invocation_id,
        terminal,
        provider_attempt_id,
        provider_delivery_state,
    )
    .await?;
    match tx.commit().await {
        Ok(()) => Ok(()),
        Err(error) => {
            let commit_error = ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "commit inference settlement debt",
                error,
            );
            match inference_settlement_debt_matches(
                db,
                user_id,
                invocation_id,
                terminal,
                provider_attempt_id,
                provider_delivery_state,
            )
            .await
            {
                Ok(true) => Ok(()),
                Ok(false) => Err(commit_error),
                Err(read_error) => {
                    tracing::warn!(
                        %user_id,
                        %invocation_id,
                        %read_error,
                        "inference settlement debt commit is unresolved after authoritative re-read failed"
                    );
                    Err(commit_error)
                }
            }
        }
    }
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
        InferenceSettlementDebtRequest {
            user_id: &plan.input.user_id,
            invocation_id: &plan.invocation_id,
            owner_token: &plan.owner_token,
            owner_generation: plan.owner_generation,
            terminal: &terminal,
            provider_attempt_id: None,
            provider_delivery_state: ProviderDeliveryState::Unknown,
            mode: SettlementDebtMode::FenceOpenAttempts,
        },
    )
    .await
}

/// Durably bind a logical settlement to the exact physical attempt whose
/// terminal acknowledgement may have been lost. Recovery applies that exact
/// terminal before mirroring the logical result; it never guesses success from
/// an open attempt or degrades known provider output to a generic timeout.
pub async fn declare_inference_attempt_settlement(
    pool: &SharedPool,
    plan: &InferenceInvocationPlan,
    attempt: &InferenceProviderAttemptPlan,
    terminal: &InferenceInvocationTerminal,
    provider_delivery_state: InferenceProviderDeliveryState,
) -> ServiceResult<()> {
    if attempt.user_id != plan.input.user_id || attempt.invocation_id != plan.invocation_id {
        return Err(ServiceError::invalid(
            "provider attempt does not belong to the declared logical invocation",
        ));
    }
    if provider_delivery_state == InferenceProviderDeliveryState::PreDelivery
        && (terminal.status != InferenceTerminalStatus::Cancelled
            || terminal.usage != InferenceUsage::default()
            || terminal.usage_status != InferenceUsageStatus::Unavailable
            || terminal.provider_response_id.is_some())
    {
        return Err(ServiceError::invalid(
            "an unauthorized provider attempt may settle only as a zero-usage pre-delivery cancellation",
        ));
    }
    let fingerprint = terminal_fingerprint(terminal)?;
    let terminal = DurableInferenceTerminal::from_terminal(terminal, fingerprint)?;
    record_inference_settlement_debt(
        pool.get(),
        InferenceSettlementDebtRequest {
            user_id: &plan.input.user_id,
            invocation_id: &plan.invocation_id,
            owner_token: &plan.owner_token,
            owner_generation: plan.owner_generation,
            terminal: &terminal,
            provider_attempt_id: Some(&attempt.attempt_id),
            provider_delivery_state: provider_delivery_state.into(),
            mode: SettlementDebtMode::FenceOpenAttempts,
        },
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
    provider_delivery_state: &str,
) -> Result<u64, sqlx::Error> {
    sqlx::query(
        "UPDATE inference_invocations
         SET status = ?,
             terminal_fingerprint = ?,
             usage_status = ?,
             provider_delivery_state = ?,
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
    .bind(terminal.usage_status)
    .bind(provider_delivery_state)
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
    provider_attempt_id: Option<&str>,
) -> Result<bool, sqlx::Error> {
    sqlx::query(
        "SELECT 1
         FROM inference_provider_attempts
         WHERE user_id = ?
           AND invocation_id = ?
           AND status = 'succeeded'
           AND terminal_fingerprint = ?
           AND (? IS NULL OR attempt_id = ?)
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
    .bind(provider_attempt_id)
    .bind(provider_attempt_id)
    .fetch_optional(db)
    .await
    .map(|row| row.is_some())
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ProviderAttemptSettlementState {
    has_open_attempt: bool,
    successful_attempt_matches: bool,
}

async fn provider_attempt_settlement_state<'e, E>(
    executor: E,
    user_id: &str,
    invocation_id: &str,
    successful_fingerprint: Option<&str>,
) -> Result<ProviderAttemptSettlementState, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::MySql>,
{
    let latest_attempt = sqlx::query(
        "SELECT attempt.status, attempt.terminal_fingerprint,
                (SELECT COUNT(*)
                 FROM inference_provider_attempts AS open_attempt
                 WHERE open_attempt.user_id = attempt.user_id
                   AND open_attempt.invocation_id = attempt.invocation_id
                   AND open_attempt.status = 'started') AS open_attempt_count
         FROM inference_provider_attempts AS attempt
         WHERE attempt.user_id = ? AND attempt.invocation_id = ?
         ORDER BY attempt.attempt_index DESC
         LIMIT 1",
    )
    .bind(user_id)
    .bind(invocation_id)
    .fetch_optional(executor)
    .await?;
    let Some(latest_attempt) = latest_attempt else {
        return Ok(ProviderAttemptSettlementState::default());
    };
    let has_open_attempt = latest_attempt.try_get::<i64, _>("open_attempt_count")? > 0;
    let latest_status = latest_attempt.try_get::<String, _>("status")?;
    let latest_fingerprint = latest_attempt.try_get::<Option<String>, _>("terminal_fingerprint")?;
    let successful_attempt_matches = successful_fingerprint.is_some()
        && !has_open_attempt
        && latest_status == "succeeded"
        && latest_fingerprint.as_deref() == successful_fingerprint;
    Ok(ProviderAttemptSettlementState {
        has_open_attempt,
        successful_attempt_matches,
    })
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

async fn quarantine_inference_settlement_debt(
    db: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    invocation_id: &str,
    fingerprint: &str,
    reason: &str,
) -> Result<(), sqlx::Error> {
    let reason = reason.chars().take(255).collect::<String>();
    sqlx::query(
        "UPDATE inference_invocation_settlement_debts
         SET reconciliation_status = 'quarantined', quarantine_reason = ?
         WHERE user_id = ? AND invocation_id = ? AND terminal_fingerprint = ?",
    )
    .bind(reason)
    .bind(user_id)
    .bind(invocation_id)
    .bind(fingerprint)
    .execute(db)
    .await
    .map(|_| ())
}

async fn defer_inference_settlement_debt(
    db: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    invocation_id: &str,
) -> Result<(), sqlx::Error> {
    // A transient or row-local persistence error must not leave one owner at
    // the head of every bounded batch. The durable debt remains pending, but
    // its next eligibility is delayed so other users receive a fair recovery
    // turn. Permanent semantic conflicts are quarantined at their detection
    // sites instead of using this retry lane.
    sqlx::query(
        "UPDATE inference_invocation_settlement_debts
         SET next_retry_at = DATE_ADD(NOW(6), INTERVAL 30 SECOND)
         WHERE user_id = ? AND invocation_id = ?
           AND reconciliation_status = 'pending'",
    )
    .bind(user_id)
    .bind(invocation_id)
    .execute(db)
    .await
    .map(|_| ())
}

const INFERENCE_SETTLEMENT_RECOVERY_BATCH: i64 = 256;

async fn close_open_attempts_owned_by_settlement_debt(
    db: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    invocation_id: &str,
    terminal: &DurableInferenceTerminal,
    provider_attempt_id: Option<&str>,
) -> Result<u64, sqlx::Error> {
    if let Some(attempt_id) = provider_attempt_id {
        return sqlx::query(
            "UPDATE inference_provider_attempts
             SET status = ?, terminal_fingerprint = ?,
                 usage_status = ?,
                 input_tokens = ?, output_tokens = ?, cache_read_tokens = ?,
                 cache_creation_tokens = ?, provider_response_id = ?,
                 error_kind = ?, error_message = ?, terminal_at = NOW(6)
             WHERE user_id = ? AND invocation_id = ? AND attempt_id = ?
               AND status = 'started'",
        )
        .bind(&terminal.status)
        .bind(&terminal.terminal_fingerprint)
        .bind(&terminal.usage_status)
        .bind(terminal.input_tokens)
        .bind(terminal.output_tokens)
        .bind(terminal.cache_read_tokens)
        .bind(terminal.cache_creation_tokens)
        .bind(&terminal.provider_response_id)
        .bind(&terminal.error_kind)
        .bind(&terminal.error_message)
        .bind(user_id)
        .bind(invocation_id)
        .bind(attempt_id)
        .execute(db)
        .await
        .map(|result| result.rows_affected());
    }

    let fallback = InferenceInvocationTerminal {
        status: InferenceTerminalStatus::DeliveryUnknown,
        usage: InferenceUsage::default(),
        usage_status: InferenceUsageStatus::Unavailable,
        provider_response_id: None,
        error_kind: Some("settlement_recovery".to_string()),
        error_message: Some(
            "provider attempt terminal write was not observed before logical settlement"
                .to_string(),
        ),
    };
    let fingerprint = terminal_fingerprint(&fallback)
        .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
    sqlx::query(
        "UPDATE inference_provider_attempts
         SET status = 'delivery_unknown', terminal_fingerprint = ?,
             usage_status = 'unavailable',
             input_tokens = 0, output_tokens = 0, cache_read_tokens = 0,
             cache_creation_tokens = 0, provider_response_id = NULL,
             error_kind = ?, error_message = ?, terminal_at = NOW(6)
         WHERE user_id = ? AND invocation_id = ? AND status = 'started'",
    )
    .bind(fingerprint)
    .bind(fallback.error_kind)
    .bind(fallback.error_message)
    .bind(user_id)
    .bind(invocation_id)
    .execute(db)
    .await
    .map(|result| result.rows_affected())
}

/// Resolve the race where another reconciler terminalizes the invocation
/// after this worker reads an admitted row but before its guarded UPDATE.
/// A zero-row UPDATE is not evidence that debt remains pending; re-read the
/// authoritative invocation before emitting an operational warning.
async fn clear_debt_after_concurrent_invocation_terminal(
    db: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    invocation_id: &str,
    debt_fingerprint: &str,
) -> Result<bool, sqlx::Error> {
    let Some(row) = sqlx::query(
        "SELECT status, terminal_fingerprint
         FROM inference_invocations
         WHERE user_id = ? AND invocation_id = ?",
    )
    .bind(user_id)
    .bind(invocation_id)
    .fetch_optional(db)
    .await?
    else {
        return Ok(false);
    };
    let status = row.try_get::<String, _>("status")?;
    if status == "admitted" {
        return Ok(false);
    }
    let terminal_fingerprint = row.try_get::<Option<String>, _>("terminal_fingerprint")?;
    if terminal_fingerprint.as_deref() == Some(debt_fingerprint) {
        delete_inference_settlement_debt(db, user_id, invocation_id, debt_fingerprint).await?;
    } else {
        tracing::error!(
            %user_id,
            %invocation_id,
            %status,
            debt_fingerprint,
            invocation_terminal_fingerprint = terminal_fingerprint.as_deref().unwrap_or("missing"),
            "inference settlement debt conflicts with a concurrently terminalized invocation; retaining durable incident authority"
        );
        quarantine_inference_settlement_debt(
            db,
            user_id,
            invocation_id,
            debt_fingerprint,
            "settlement debt conflicts with a concurrently terminalized invocation",
        )
        .await?;
        return Ok(false);
    }
    Ok(true)
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
                debt.usage_status,
                debt.provider_attempt_id,
                debt.provider_delivery_state,
                invocation.status AS invocation_status,
                invocation.terminal_fingerprint AS invocation_terminal_fingerprint
         FROM inference_invocation_settlement_debts AS debt
         LEFT JOIN inference_invocations AS invocation
           ON invocation.user_id = debt.user_id
          AND invocation.invocation_id = debt.invocation_id
         WHERE debt.user_id = ? AND debt.invocation_id = ?
           AND debt.reconciliation_status = 'pending'",
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
    let invocation_status = row.try_get::<Option<String>, _>("invocation_status")?;
    let provider_attempt_id = row.try_get::<Option<String>, _>("provider_attempt_id")?;
    let provider_delivery_state = row.try_get::<String, _>("provider_delivery_state")?;
    let Some(invocation_status) = invocation_status else {
        let reason = "settlement debt has no durable logical invocation owner";
        quarantine_inference_settlement_debt(db, user_id, invocation_id, &fingerprint, reason)
            .await?;
        tracing::error!(%user_id, %invocation_id, %reason, "quarantined orphaned inference settlement debt");
        return Ok(0);
    };
    if invocation_status == "admitted" {
        if provider_attempt_id.is_none()
            && provider_delivery_state == ProviderDeliveryState::DeliveryAuthorized.as_str()
        {
            let reason = "provider delivery state requires an exact provider attempt identity";
            quarantine_inference_settlement_debt(db, user_id, invocation_id, &fingerprint, reason)
                .await?;
            tracing::error!(%user_id, %invocation_id, %reason, "quarantined malformed inference settlement debt");
            return Ok(0);
        }
        if provider_attempt_id.is_some()
            && provider_delivery_state == ProviderDeliveryState::PreDelivery.as_str()
            && terminal.status != InferenceTerminalStatus::Cancelled.as_str()
        {
            let reason = "pre-delivery exact debt must be a cancelled terminal";
            quarantine_inference_settlement_debt(db, user_id, invocation_id, &fingerprint, reason)
                .await?;
            tracing::error!(%user_id, %invocation_id, %reason, "quarantined malformed inference settlement debt");
            return Ok(0);
        }
        if provider_attempt_id.is_none()
            && provider_delivery_state == ProviderDeliveryState::PreDelivery.as_str()
            && (terminal.status != InferenceTerminalStatus::Cancelled.as_str()
                || terminal.usage_status != InferenceUsageStatus::Unavailable.as_str()
                || terminal.input_tokens != 0
                || terminal.output_tokens != 0
                || terminal.cache_read_tokens != 0
                || terminal.cache_creation_tokens != 0
                || terminal.provider_response_id.is_some())
        {
            let reason = "pre-provider logical admission debt must be a zero-usage cancellation";
            quarantine_inference_settlement_debt(db, user_id, invocation_id, &fingerprint, reason)
                .await?;
            tracing::error!(%user_id, %invocation_id, %reason, "quarantined malformed inference settlement debt");
            return Ok(0);
        }
        let recovered_attempts = close_open_attempts_owned_by_settlement_debt(
            db,
            user_id,
            invocation_id,
            &terminal,
            provider_attempt_id.as_deref(),
        )
        .await?;
        if recovered_attempts > 0 {
            tracing::warn!(
                %user_id,
                %invocation_id,
                recovered_attempts,
                "closed provider attempts left open behind an authoritative settlement debt"
            );
        }
        let exact_attempt = if let Some(attempt_id) = provider_attempt_id.as_deref() {
            sqlx::query(
                "SELECT terminal_fingerprint FROM inference_provider_attempts
                 WHERE user_id = ? AND invocation_id = ? AND attempt_id = ?",
            )
            .bind(user_id)
            .bind(invocation_id)
            .bind(attempt_id)
            .fetch_optional(db)
            .await?
        } else {
            None
        };
        if let (Some(attempt_id), Some(attempt)) =
            (provider_attempt_id.as_deref(), exact_attempt.as_ref())
            && attempt
                .try_get::<Option<String>, _>("terminal_fingerprint")?
                .as_deref()
                != Some(fingerprint.as_str())
        {
            tracing::error!(
                %user_id,
                %invocation_id,
                %attempt_id,
                debt_fingerprint = %fingerprint,
                "exact-attempt settlement debt conflicts with physical terminal state; retaining durable incident authority"
            );
            quarantine_inference_settlement_debt(
                db,
                user_id,
                invocation_id,
                &fingerprint,
                "exact provider terminal conflicts with the persisted physical terminal",
            )
            .await?;
            return Ok(0);
        }
        if let Some(attempt_id) = provider_attempt_id.as_deref()
            && exact_attempt.is_none()
            && provider_delivery_state != ProviderDeliveryState::PreDelivery.as_str()
        {
            tracing::error!(
                %user_id,
                %invocation_id,
                %attempt_id,
                debt_fingerprint = %fingerprint,
                "delivery-authorized exact-attempt debt has no physical admission row; retaining durable incident authority"
            );
            quarantine_inference_settlement_debt(
                db,
                user_id,
                invocation_id,
                &fingerprint,
                "delivery-authorized or legacy exact debt has no physical attempt row",
            )
            .await?;
            return Ok(0);
        }
        if let Some(attempt_id) = provider_attempt_id.as_deref()
            && exact_attempt.is_some()
            && let Err(error) = insert_recovered_model_request_terminal(
                db,
                user_id,
                invocation_id,
                attempt_id,
                &terminal,
            )
            .await
        {
            let quarantine_reason = match &error {
                sqlx::Error::Protocol(message)
                    if message.contains("has no accepted request-context event") =>
                {
                    Some("exact provider attempt is missing accepted request-context evidence")
                }
                sqlx::Error::Protocol(_) => {
                    Some("exact provider attempt has malformed request-context evidence")
                }
                _ => None,
            };
            let Some(quarantine_reason) = quarantine_reason else {
                return Err(error);
            };
            quarantine_inference_settlement_debt(
                db,
                user_id,
                invocation_id,
                &fingerprint,
                quarantine_reason,
            )
            .await?;
            tracing::error!(
                %user_id,
                %invocation_id,
                %attempt_id,
                %error,
                "quarantined exact settlement debt with incomplete accounting evidence"
            );
            return Ok(0);
        }
        if terminal.status == InferenceTerminalStatus::Succeeded.as_str()
            && !matching_successful_provider_attempt(
                db,
                user_id,
                invocation_id,
                &fingerprint,
                provider_attempt_id.as_deref(),
            )
            .await?
        {
            tracing::error!(
                %user_id,
                %invocation_id,
                debt_fingerprint = %fingerprint,
                exact_attempt = provider_attempt_id.is_some(),
                "inference success debt has no matching provider terminal"
            );
            if provider_attempt_id.is_none() {
                // Legacy logical-only success debts could never establish
                // physical authority; remove those invalid rows. A modern
                // exact-attempt debt is an incident record and must survive
                // until the referenced attempt can be reconciled or repaired.
                delete_inference_settlement_debt(db, user_id, invocation_id, &fingerprint).await?;
            } else {
                quarantine_inference_settlement_debt(
                    db,
                    user_id,
                    invocation_id,
                    &fingerprint,
                    "exact success debt has no matching successful physical attempt",
                )
                .await?;
            }
            return Ok(0);
        }
        let updated = apply_inference_terminal_if_quiescent(
            db,
            user_id,
            invocation_id,
            terminal,
            &provider_delivery_state,
        )
        .await?;
        if updated == 1 {
            delete_inference_settlement_debt(db, user_id, invocation_id, &fingerprint).await?;
        } else if !clear_debt_after_concurrent_invocation_terminal(
            db,
            user_id,
            invocation_id,
            &fingerprint,
        )
        .await?
        {
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
            "inference settlement debt conflicts with terminal invocation state; retaining durable incident authority"
        );
        quarantine_inference_settlement_debt(
            db,
            user_id,
            invocation_id,
            &fingerprint,
            "settlement debt conflicts with the terminal logical invocation",
        )
        .await?;
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
         WHERE reconciliation_status = 'pending'
           AND next_retry_at <= NOW(6)
         ORDER BY next_retry_at ASC, user_id ASC, invocation_id ASC
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
        match reconcile_inference_settlement_debt(db, &user_id, &invocation_id).await {
            Ok(count) => Ok(count),
            Err(error) => {
                if let Err(defer_error) =
                    defer_inference_settlement_debt(db, &user_id, &invocation_id).await
                {
                    tracing::warn!(
                        %user_id,
                        %invocation_id,
                        %defer_error,
                        "failed to defer an inference settlement debt after reconciliation error"
                    );
                }
                Err(error)
            }
        }
    })
    .await
}

async fn recover_expired_inference_invocation(
    db: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    invocation_id: &str,
) -> Result<u64, sqlx::Error> {
    let mut tx = db.begin().await?;
    let Some(invocation) = sqlx::query(
        "SELECT status, owner_token, owner_generation,
                IF(owner_lease_expires_at <= NOW(6), 1, 0) AS lease_expired
         FROM inference_invocations
         WHERE user_id = ? AND invocation_id = ?
         FOR UPDATE",
    )
    .bind(user_id)
    .bind(invocation_id)
    .fetch_optional(&mut *tx)
    .await?
    else {
        tx.rollback().await?;
        return Ok(0);
    };
    let status = invocation.try_get::<String, _>("status")?;
    let lease_expired = invocation.try_get::<i64, _>("lease_expired")? == 1;
    if status != "admitted" || !lease_expired {
        tx.rollback().await?;
        return Ok(0);
    }
    let old_owner_token = invocation.try_get::<String, _>("owner_token")?;
    let old_owner_generation = invocation.try_get::<i64, _>("owner_generation")?;
    let new_owner_generation = old_owner_generation.checked_add(1).ok_or_else(|| {
        sqlx::Error::Protocol(format!(
            "inference owner generation exhausted for {user_id}/{invocation_id}"
        ))
    })?;
    let settlement_exists = sqlx::query(
        "SELECT 1 FROM inference_invocation_settlement_debts
         WHERE user_id = ? AND invocation_id = ? LIMIT 1",
    )
    .bind(user_id)
    .bind(invocation_id)
    .fetch_optional(&mut *tx)
    .await?
    .is_some();
    if settlement_exists {
        tx.rollback().await?;
        return Ok(0);
    }

    let attempts = sqlx::query(
        "SELECT attempt_id, status, terminal_fingerprint, usage_status,
                input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
                provider_response_id, error_kind, error_message
         FROM inference_provider_attempts
         WHERE user_id = ? AND invocation_id = ?
         ORDER BY attempt_index ASC, attempt_id ASC
         FOR UPDATE",
    )
    .bind(user_id)
    .bind(invocation_id)
    .fetch_all(&mut *tx)
    .await?;
    let started_attempt_ids = attempts
        .iter()
        .filter_map(|attempt| {
            attempt
                .try_get::<String, _>("status")
                .ok()
                .filter(|status| status == "started")
                .and_then(|_| attempt.try_get::<String, _>("attempt_id").ok())
        })
        .collect::<Vec<_>>();

    let (terminal, delivery_state) = if attempts.is_empty() {
        let terminal = InferenceInvocationTerminal {
            status: InferenceTerminalStatus::Cancelled,
            usage: InferenceUsage::default(),
            usage_status: InferenceUsageStatus::Unavailable,
            provider_response_id: None,
            error_kind: Some("owner_lease_expired".to_string()),
            error_message: Some(
                "inference owner stopped before provider delivery was authorized".to_string(),
            ),
        };
        let fingerprint = terminal_fingerprint(&terminal)
            .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
        (
            DurableInferenceTerminal::from_terminal(&terminal, fingerprint)
                .map_err(|error| sqlx::Error::Protocol(error.to_string()))?,
            ProviderDeliveryState::PreDelivery,
        )
    } else if !started_attempt_ids.is_empty() {
        let terminal = InferenceInvocationTerminal {
            status: InferenceTerminalStatus::DeliveryUnknown,
            usage: InferenceUsage::default(),
            usage_status: InferenceUsageStatus::Unavailable,
            provider_response_id: None,
            error_kind: Some("owner_lease_expired".to_string()),
            error_message: Some(
                "inference owner lease expired after provider delivery was authorized".to_string(),
            ),
        };
        let fingerprint = terminal_fingerprint(&terminal)
            .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
        (
            DurableInferenceTerminal::from_terminal(&terminal, fingerprint)
                .map_err(|error| sqlx::Error::Protocol(error.to_string()))?,
            ProviderDeliveryState::DeliveryAuthorized,
        )
    } else {
        let last = attempts.last().expect("non-empty terminal attempt set");
        let terminal_fingerprint = last
            .try_get::<Option<String>, _>("terminal_fingerprint")?
            .ok_or_else(|| {
                sqlx::Error::Protocol(format!(
                    "terminal provider attempt has no fingerprint for {user_id}/{invocation_id}"
                ))
            })?;
        (
            DurableInferenceTerminal {
                status: last.try_get("status")?,
                terminal_fingerprint: Some(terminal_fingerprint),
                usage_status: last.try_get("usage_status")?,
                input_tokens: last.try_get("input_tokens")?,
                output_tokens: last.try_get("output_tokens")?,
                cache_read_tokens: last.try_get("cache_read_tokens")?,
                cache_creation_tokens: last.try_get("cache_creation_tokens")?,
                provider_response_id: last.try_get("provider_response_id")?,
                error_kind: last.try_get("error_kind")?,
                error_message: last.try_get("error_message")?,
            },
            ProviderDeliveryState::DeliveryAuthorized,
        )
    };

    for attempt_id in &started_attempt_ids {
        let updated = sqlx::query(
            "UPDATE inference_provider_attempts
             SET status = 'delivery_unknown', terminal_fingerprint = ?,
                 usage_status = 'unavailable', input_tokens = 0, output_tokens = 0,
                 cache_read_tokens = 0, cache_creation_tokens = 0,
                 provider_response_id = NULL, error_kind = ?, error_message = ?,
                 terminal_at = NOW(6)
             WHERE user_id = ? AND invocation_id = ? AND attempt_id = ?
               AND status = 'started'",
        )
        .bind(&terminal.terminal_fingerprint)
        .bind(&terminal.error_kind)
        .bind(&terminal.error_message)
        .bind(user_id)
        .bind(invocation_id)
        .bind(attempt_id)
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() != 1 {
            return Err(sqlx::Error::Protocol(format!(
                "expired inference attempt {user_id}/{attempt_id} changed during recovery"
            )));
        }
        insert_recovered_model_request_terminal_tx(
            &mut tx,
            user_id,
            invocation_id,
            attempt_id,
            &terminal,
        )
        .await?;
    }

    let new_owner_token = new_admission_token();
    let updated = sqlx::query(
        "UPDATE inference_invocations
         SET owner_token = ?, owner_generation = ?, owner_lease_expires_at = NOW(6),
             status = ?, terminal_fingerprint = ?, usage_status = ?,
             provider_delivery_state = ?, input_tokens = ?, output_tokens = ?,
             cache_read_tokens = ?, cache_creation_tokens = ?, provider_response_id = ?,
             error_kind = ?, error_message = ?, terminal_at = NOW(6)
         WHERE user_id = ? AND invocation_id = ? AND status = 'admitted'
           AND owner_token = ? AND owner_generation = ?
           AND owner_lease_expires_at <= NOW(6)
           AND NOT EXISTS (
               SELECT 1 FROM inference_invocation_settlement_debts AS debt
               WHERE debt.user_id = inference_invocations.user_id
                 AND debt.invocation_id = inference_invocations.invocation_id
           )",
    )
    .bind(new_owner_token)
    .bind(new_owner_generation)
    .bind(&terminal.status)
    .bind(&terminal.terminal_fingerprint)
    .bind(&terminal.usage_status)
    .bind(delivery_state.as_str())
    .bind(terminal.input_tokens)
    .bind(terminal.output_tokens)
    .bind(terminal.cache_read_tokens)
    .bind(terminal.cache_creation_tokens)
    .bind(&terminal.provider_response_id)
    .bind(&terminal.error_kind)
    .bind(&terminal.error_message)
    .bind(user_id)
    .bind(invocation_id)
    .bind(old_owner_token)
    .bind(old_owner_generation)
    .execute(&mut *tx)
    .await?;
    if updated.rows_affected() != 1 {
        tx.rollback().await?;
        return Ok(0);
    }
    tx.commit().await?;
    Ok(1)
}

async fn recover_expired_inference_invocations_batch(
    db: &sqlx::Pool<sqlx::MySql>,
    limit: i64,
) -> Result<u64, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT user_id, invocation_id
         FROM (
             SELECT invocation.user_id, invocation.invocation_id,
                    invocation.owner_lease_expires_at,
                    ROW_NUMBER() OVER (
                        PARTITION BY invocation.user_id
                        ORDER BY invocation.owner_lease_expires_at ASC,
                                 invocation.invocation_id ASC
                    ) AS owner_rank
             FROM inference_invocations AS invocation
             WHERE invocation.status = 'admitted'
               AND invocation.owner_lease_expires_at <= NOW(6)
               AND NOT EXISTS (
                   SELECT 1 FROM inference_invocation_settlement_debts AS debt
                   WHERE debt.user_id = invocation.user_id
                     AND debt.invocation_id = invocation.invocation_id
               )
         ) AS expired
         ORDER BY owner_rank ASC, owner_lease_expires_at ASC,
                  user_id ASC, invocation_id ASC
         LIMIT ?",
    )
    .bind(limit.clamp(1, INFERENCE_SETTLEMENT_RECOVERY_BATCH))
    .fetch_all(db)
    .await?;
    let identities = rows
        .into_iter()
        .map(|row| Ok((row.try_get("user_id")?, row.try_get("invocation_id")?)))
        .collect::<Result<Vec<(String, String)>, sqlx::Error>>()?;
    reconcile_settlement_identities(identities, |user_id, invocation_id| async move {
        recover_expired_inference_invocation(db, &user_id, &invocation_id).await
    })
    .await
}

/// Reconcile at most one bounded batch of explicit settlement decisions.
/// Runtime workers call this repeatedly; schema readiness never waits for the
/// operational backlog to drain.
pub async fn reconcile_inference_settlements(pool: &SharedPool, limit: u32) -> ServiceResult<u64> {
    let limit = i64::from(limit.max(2));
    let debt_limit = (limit + 1) / 2;
    let orphan_limit = limit / 2;
    let reconciled_debts = reconcile_inference_settlement_debts_batch(pool.get(), debt_limit)
        .await
        .map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "reconcile inference settlement batch",
                error,
            )
        })?;
    let recovered_orphans = recover_expired_inference_invocations_batch(pool.get(), orphan_limit)
        .await
        .map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "recover expired inference owner batch",
                error,
            )
        })?;
    Ok(reconciled_debts + recovered_orphans)
}

/// Project one already-declared settlement decision to its exact invocation.
///
/// Detached request owners use this point lookup after durably recording debt;
/// the batch sweeper remains a crash-recovery fallback rather than the normal
/// completion path. Repeating the exact terminal is idempotent, while a
/// conflicting terminal remains an authoritative contract failure.
pub async fn reconcile_inference_settlement(
    pool: &SharedPool,
    plan: &InferenceInvocationPlan,
    terminal: &InferenceInvocationTerminal,
) -> ServiceResult<InferenceSettlementReconcileOutcome> {
    let fingerprint = terminal_fingerprint(terminal)?;
    let db = pool.get();
    reconcile_inference_settlement_debt(db, &plan.input.user_id, &plan.invocation_id)
        .await
        .map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "reconcile exact inference settlement",
                error,
            )
        })?;
    match existing_terminal_fingerprint(db, plan).await? {
        Some(existing) if existing == fingerprint => {
            Ok(InferenceSettlementReconcileOutcome::Settled)
        }
        Some(_) => Err(ServiceError::conflict(format!(
            "inference invocation {} terminal payload conflicts with its durable result",
            plan.invocation_id
        ))),
        None => {
            let debt = sqlx::query(
                "SELECT terminal_fingerprint, reconciliation_status
                 FROM inference_invocation_settlement_debts
                 WHERE user_id = ? AND invocation_id = ? LIMIT 1",
            )
            .bind(&plan.input.user_id)
            .bind(&plan.invocation_id)
            .fetch_optional(db)
            .await
            .map_err(|error| {
                ServiceError::with_source(
                    ServiceErrorKind::Persistence,
                    "load exact inference settlement disposition",
                    error,
                )
            })?;
            let Some(debt) = debt else {
                return Ok(InferenceSettlementReconcileOutcome::TransientPending);
            };
            let debt_fingerprint =
                debt.try_get::<String, _>("terminal_fingerprint")
                    .map_err(|error| {
                        ServiceError::with_source(
                            ServiceErrorKind::Persistence,
                            "decode exact inference settlement fingerprint",
                            error,
                        )
                    })?;
            if debt_fingerprint != fingerprint {
                return Err(ServiceError::conflict(format!(
                    "inference invocation {} settlement debt conflicts with the requested terminal",
                    plan.invocation_id
                )));
            }
            match debt
                .try_get::<String, _>("reconciliation_status")
                .map_err(|error| {
                    ServiceError::with_source(
                        ServiceErrorKind::Persistence,
                        "decode exact inference settlement disposition",
                        error,
                    )
                })?
                .as_str()
            {
                "quarantined" => Ok(InferenceSettlementReconcileOutcome::PermanentlyQuarantined),
                "pending" => Ok(InferenceSettlementReconcileOutcome::TransientPending),
                status => Err(ServiceError::new(
                    ServiceErrorKind::Verification,
                    format!(
                        "inference invocation {} has unknown settlement reconciliation status {status}",
                        plan.invocation_id
                    ),
                )),
            }
        }
    }
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
    // The finalization owner explicitly records the logical outcome before it
    // tries to mirror it to `inference_invocations`. Unlike a provider attempt
    // failure, this is a durable declaration that retry policy has finished.
    record_inference_settlement_debt(
        db,
        InferenceSettlementDebtRequest {
            user_id: &plan.input.user_id,
            invocation_id: &plan.invocation_id,
            owner_token: &plan.owner_token,
            owner_generation: plan.owner_generation,
            terminal: &terminal_state,
            provider_attempt_id: None,
            provider_delivery_state: ProviderDeliveryState::Unknown,
            mode: SettlementDebtMode::RequireQuiescent,
        },
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
             SET status = ?, terminal_fingerprint = ?, usage_status = ?,
                 provider_delivery_state = IF(
                     EXISTS (
                         SELECT 1 FROM inference_provider_attempts AS delivered_attempt
                         WHERE delivered_attempt.user_id = inference_invocations.user_id
                           AND delivered_attempt.invocation_id = inference_invocations.invocation_id
                     ),
                     'delivery_authorized', 'pre_delivery'),
                 input_tokens = ?, output_tokens = ?,
                 cache_read_tokens = ?, cache_creation_tokens = ?, provider_response_id = ?,
                 error_kind = ?, error_message = ?, terminal_at = NOW(6)
             WHERE user_id = ? AND invocation_id = ? AND status = 'admitted'
               AND owner_token = ? AND owner_generation = ?
               AND owner_lease_expires_at > NOW(6)
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
        .bind(&terminal_state.usage_status)
        .bind(terminal_state.input_tokens)
        .bind(terminal_state.output_tokens)
        .bind(terminal_state.cache_read_tokens)
        .bind(terminal_state.cache_creation_tokens)
        .bind(&terminal_state.provider_response_id)
        .bind(&terminal_state.error_kind)
        .bind(&terminal_state.error_message)
        .bind(&plan.input.user_id)
        .bind(&plan.invocation_id)
        .bind(&plan.owner_token)
        .bind(i64::try_from(plan.owner_generation).map_err(|_| {
            ServiceError::invalid("inference owner generation exceeds the durable BIGINT range")
        })?)
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
            run_authority: Some(InferenceRunAdmissionAuthority {
                expected_owner_generation: 0,
                expected_owner_pod_id: "test-inference-owner".to_string(),
                expected_control_epoch: 0,
            }),
        }
    }

    #[test]
    fn invocation_identity_changes_with_inference_purpose() {
        let primary = plan_inference_invocation(input()).expect("primary plan");
        assert_eq!(primary.logical_attempt(), 0);
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
    fn run_scope_requires_exact_execution_authority_and_other_scopes_reject_it() {
        let mut missing = input();
        missing.run_authority = None;
        assert_eq!(
            plan_inference_invocation(missing)
                .expect_err("run inference without execution authority must fail")
                .kind,
            ServiceErrorKind::Invalid
        );

        let mut session = input();
        session.scope = InferenceInvocationScope::Session {
            session_id: "session-1".to_string(),
            turn: 3,
            round: 2,
            operation_id: "memory_extraction".to_string(),
            logical_attempt: 0,
        };
        assert_eq!(
            plan_inference_invocation(session)
                .expect_err("run authority must not cross into a session scope")
                .kind,
            ServiceErrorKind::Invalid
        );

        let mut invalid_epoch = input();
        invalid_epoch
            .run_authority
            .as_mut()
            .expect("run authority")
            .expected_control_epoch = -2;
        assert_eq!(
            plan_inference_invocation(invalid_epoch)
                .expect_err("control authority below the initial epoch must fail")
                .kind,
            ServiceErrorKind::Invalid
        );
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
        session_input.run_authority = None;
        let session = plan_inference_invocation(session_input).expect("session plan");
        let mut harness_input = input();
        harness_input.scope = InferenceInvocationScope::HarnessRun {
            harness_run_id: "harness-run-1".to_string(),
            operation_id: "skillify_extract".to_string(),
            logical_attempt: 0,
        };
        harness_input.run_authority = None;
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

        let mut retry_input = input();
        retry_input.scope = retry_input.scope.with_logical_attempt(1);
        let retry_invocation = plan_inference_invocation(retry_input).expect("retry invocation");
        assert_eq!(retry_invocation.logical_attempt(), 1);
        assert_ne!(
            first_invocation.invocation_id,
            retry_invocation.invocation_id
        );
        assert_ne!(first_invocation.route_id, retry_invocation.route_id);
    }

    #[test]
    fn ambiguous_invocation_admission_requires_the_plans_fencing_token() {
        let plan = plan_inference_invocation(input()).expect("invocation");
        let exact = PersistedInvocationAdmissionFact {
            route_id: plan.route_id.clone(),
            admission_token: plan.admission_token.clone(),
            owner_token: plan.owner_token.clone(),
            owner_generation: i64::try_from(plan.owner_generation).expect("owner generation"),
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
                input: astra_turn_types::NormalizedPromptCacheUsage::new(200, 800, 100),
                output_tokens: 50,
            },
            usage_status: InferenceUsageStatus::ProviderPartial,
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
                input: astra_turn_types::NormalizedPromptCacheUsage::new(10, 4, 0),
                output_tokens: 3,
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
                    input: astra_turn_types::NormalizedPromptCacheUsage::new(10, 4, 0),
                    output_tokens: 3,
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
                input: astra_turn_types::NormalizedPromptCacheUsage::new(300, 600, 100),
                output_tokens: 80,
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
        assert!(accepted.usage_status.is_none());
        assert_eq!(terminal_event.identity.physical_attempt, 2);
        assert_eq!(
            terminal_event.identity.provider_wire_hash,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        assert_eq!(
            terminal_event.usage,
            Some(ModelRequestUsage {
                input: astra_turn_types::NormalizedPromptCacheUsage::new(300, 600, 100),
                output_tokens: 80,
            })
        );
        assert_eq!(terminal_event.budget.estimate_error_tokens, Some(100));
        assert_eq!(terminal_event.cache.cache_read_share, Some(0.6));
        assert_eq!(
            terminal_event.usage_status.as_deref(),
            Some("provider_exact")
        );
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
                event
                    .usage
                    .as_ref()
                    .map(ModelRequestUsage::total_input_tokens),
                Some(1_000)
            );
        }
    }
}
