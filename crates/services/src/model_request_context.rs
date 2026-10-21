//! Typed, content-free observability for one physical model request.
//!
//! A request-context event is deliberately assembled from deterministic
//! admission, wire, budget, and provider-terminal facts. It never attempts to
//! infer intent from prompt text, and it never stores messages or tool output.

use serde::{Deserialize, Serialize};
use sqlx::{MySql, QueryBuilder, Row};
use std::collections::BTreeSet;

use astra_core::SharedPool;

use crate::{CancellationSafePoolConnection, ServiceError, ServiceErrorKind, ServiceResult};

pub const MODEL_REQUEST_CONTEXT_SCHEMA: &str = "model_request_context_v2";
pub(crate) const MODEL_REQUEST_CONTEXT_RETENTION_DAYS: u32 = 30;
pub(crate) const MAX_MODEL_REQUEST_CONTEXT_EVENTS_PER_SCOPE: u32 = 2048;

/// The number of complete physical attempts removed from one scope at a time.
/// Each attempt has exactly one accepted and one terminal record, so this
/// bounds a compaction write without splitting a diagnostic pair.
const MODEL_REQUEST_CONTEXT_COMPACTION_ATTEMPT_BATCH_LIMIT: u32 = 256;

const _: () = assert!(MODEL_REQUEST_CONTEXT_COMPACTION_ATTEMPT_BATCH_LIMIT > 0);
const _: () = assert!(
    MODEL_REQUEST_CONTEXT_COMPACTION_ATTEMPT_BATCH_LIMIT * 2
        <= MAX_MODEL_REQUEST_CONTEXT_EVENTS_PER_SCOPE
);

// Physical completion is not logical settlement. These joins also reject
// orphaned or contradictory diagnostic owners rather than erasing evidence.
fn push_retention_owner_joins(query: &mut QueryBuilder<MySql>, owners: &[&str]) {
    // MatrixOne does not propagate the bounded tenant predicates across all
    // joins. Bind each side explicitly; otherwise even a tiny page scans the
    // global attempt, invocation and debt tables.
    for (join, alias) in [
    (" JOIN inference_provider_attempts physical
   ON physical.user_id = context.user_id AND physical.attempt_id = context.attempt_id
  AND physical.invocation_id = context.invocation_id
  AND physical.session_id = context.session_id AND physical.run_id <=> context.run_id", "physical"),
    (" JOIN inference_invocations invocation
   ON invocation.user_id = physical.user_id AND invocation.invocation_id = physical.invocation_id
  AND invocation.session_id = context.session_id AND invocation.run_id <=> context.run_id", "invocation"),
    (" LEFT JOIN inference_invocation_settlement_debts debt
   ON debt.user_id = invocation.user_id AND debt.invocation_id = invocation.invocation_id", "debt"),
    (" LEFT JOIN inference_provider_attempts open_attempt
   ON open_attempt.user_id = invocation.user_id AND open_attempt.invocation_id = invocation.invocation_id
  AND open_attempt.status = 'started'", "open_attempt"),
    ] {
        query.push(join).push(" AND ").push(alias).push(".user_id IN (");
        let mut list = query.separated(", ");
        for owner in owners { list.push_bind(*owner); }
        query.push(")");
    }
}

const RETENTION_SETTLED_PREDICATE: &str = "
 invocation.status IN ('succeeded','failed','cancelled','delivery_unknown')
 AND physical.status IN ('succeeded','failed','cancelled','delivery_unknown')
 AND invocation.terminal_fingerprint IS NOT NULL AND physical.terminal_fingerprint IS NOT NULL
 AND debt.invocation_id IS NULL AND open_attempt.attempt_id IS NULL";

/// Only a transient scan position; execution and deletion authority stay in SQL.
#[derive(Debug, Default)]
pub struct ModelRequestContextExpiryCursor {
    cutoff: Option<chrono::NaiveDateTime>,
    after: Option<(chrono::NaiveDateTime, String, String)>,
}

#[derive(Debug, Default)]
pub struct ModelRequestContextExpiryPage {
    pub examined: u64,
    pub deleted: u64,
    pub more: bool,
}

fn validate_expired_attempt_keys_query(
    attempt_keys: &[(String, String)],
    cutoff: Option<chrono::NaiveDateTime>,
) -> QueryBuilder<MySql> {
    let mut query = QueryBuilder::<MySql>::new(
        "SELECT context.user_id, context.attempt_id FROM model_request_context_events context",
    );
    let owners = attempt_keys
        .iter()
        .map(|(owner, _)| owner.as_str())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    push_retention_owner_joins(&mut query, &owners);
    query.push(format!(" WHERE {RETENTION_SETTLED_PREDICATE} AND ("));
    for (index, (user_id, attempt_id)) in attempt_keys.iter().enumerate() {
        if index > 0 {
            query.push(" OR ");
        }
        query
            .push("(context.user_id = ")
            .push_bind(user_id)
            .push(" AND context.attempt_id = ")
            .push_bind(attempt_id)
            .push(")");
    }
    query.push(") GROUP BY context.user_id, context.attempt_id HAVING COUNT(*) = 2");
    if let Some(cutoff) = cutoff {
        query
            .push(" AND MAX(context.created_at) < ")
            .push_bind(cutoff);
    }
    query
}

fn delete_attempt_keys_query(attempt_keys: &[(String, String)]) -> QueryBuilder<MySql> {
    let mut query = QueryBuilder::<MySql>::new("DELETE FROM model_request_context_events WHERE ");
    for (index, (user_id, attempt_id)) in attempt_keys.iter().enumerate() {
        if index > 0 {
            query.push(" OR ");
        }
        query
            .push("(user_id = ")
            .push_bind(user_id)
            .push(" AND attempt_id = ")
            .push_bind(attempt_id)
            .push(")");
    }
    query
}

/// Both retention paths run this inside their existing transaction. Marking and
/// pair deletion have one owner, and neither can erase an unsettled invocation.
async fn prune_context_attempts(
    connection: &mut sqlx::MySqlConnection,
    keys: &[(String, String)],
    cutoff: Option<chrono::NaiveDateTime>,
) -> Result<u64, sqlx::Error> {
    if keys.is_empty() {
        return Ok(0);
    }
    lock_provider_attempts_for_context_expiry_query(keys)
        .build()
        .fetch_all(&mut *connection)
        .await?;
    let eligible = eligible_context_attempts(connection, keys, cutoff).await?;
    if eligible.is_empty() {
        return Ok(0);
    }
    mark_provider_attempt_context_expired_query(&eligible)
        .build()
        .execute(&mut *connection)
        .await?;
    Ok(delete_attempt_keys_query(&eligible)
        .build()
        .execute(&mut *connection)
        .await?
        .rows_affected())
}

async fn eligible_context_attempts(
    connection: &mut sqlx::MySqlConnection,
    keys: &[(String, String)],
    cutoff: Option<chrono::NaiveDateTime>,
) -> Result<Vec<(String, String)>, sqlx::Error> {
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    validate_expired_attempt_keys_query(keys, cutoff)
        .build_query_as()
        .fetch_all(connection)
        .await
}

fn lock_provider_attempts_for_context_expiry_query(
    attempt_keys: &[(String, String)],
) -> QueryBuilder<MySql> {
    let mut query = QueryBuilder::<MySql>::new(
        "SELECT user_id, attempt_id FROM inference_provider_attempts WHERE ",
    );
    for (index, (user_id, attempt_id)) in attempt_keys.iter().enumerate() {
        if index > 0 {
            query.push(" OR ");
        }
        query
            .push("(user_id = ")
            .push_bind(user_id)
            .push(" AND attempt_id = ")
            .push_bind(attempt_id)
            .push(")");
    }
    query.push(" ORDER BY user_id ASC, attempt_id ASC FOR UPDATE");
    query
}

fn mark_provider_attempt_context_expired_query(
    attempt_keys: &[(String, String)],
) -> QueryBuilder<MySql> {
    let mut query = QueryBuilder::<MySql>::new(
        "UPDATE inference_provider_attempts
         SET context_expired_at = COALESCE(context_expired_at, NOW(6))
         WHERE ",
    );
    for (index, (user_id, attempt_id)) in attempt_keys.iter().enumerate() {
        if index > 0 {
            query.push(" OR ");
        }
        query
            .push("(user_id = ")
            .push_bind(user_id)
            .push(" AND attempt_id = ")
            .push_bind(attempt_id)
            .push(")");
    }
    query
}

/// Keep a request-diagnostic scope bounded after appending an event.
///
/// The event rows are diagnostic observability, not user-visible conversation
/// history. Complete attempts are pruned as accepted/terminal pairs. If the
/// scope has unresolved invocations, their accounting evidence stays pinned
/// until logical settlement; the diagnostic capacity is deliberately soft.
pub(crate) async fn compact_model_request_context_scope(
    connection: &mut sqlx::MySqlConnection,
    user_id: &str,
    session_id: &str,
) -> ServiceResult<u64> {
    let over_limit = sqlx::query_scalar::<_, String>(
        "SELECT event_id FROM model_request_context_events
         WHERE user_id = ? AND session_id = ? LIMIT 1 OFFSET ?",
    )
    .bind(user_id)
    .bind(session_id)
    .bind(MAX_MODEL_REQUEST_CONTEXT_EVENTS_PER_SCOPE)
    .fetch_optional(&mut *connection)
    .await
    .map_err(|error| {
        ServiceError::with_source(
            ServiceErrorKind::Persistence,
            "probe model request context scope retention",
            error,
        )
    })?;

    if over_limit.is_none() {
        return Ok(0);
    }

    let mut query = QueryBuilder::<MySql>::new(
        "SELECT context.attempt_id FROM model_request_context_events context",
    );
    push_retention_owner_joins(&mut query, &[user_id]);
    query
        .push(" WHERE context.user_id = ")
        .push_bind(user_id)
        .push(" AND context.session_id = ")
        .push_bind(session_id)
        .push(format!(
            " AND {RETENTION_SETTLED_PREDICATE}
          GROUP BY context.attempt_id HAVING COUNT(*) = 2
          ORDER BY MIN(context.created_at), context.attempt_id LIMIT "
        ))
        .push_bind(MODEL_REQUEST_CONTEXT_COMPACTION_ATTEMPT_BATCH_LIMIT);
    let attempt_ids = query
        .build_query_scalar::<String>()
        .fetch_all(&mut *connection)
        .await
        .map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "load complete model request context attempts for compaction",
                error,
            )
        })?;

    if attempt_ids.is_empty() {
        return Ok(0);
    }

    let keys = attempt_ids
        .into_iter()
        .map(|id| (user_id.to_owned(), id))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    prune_context_attempts(connection, &keys, None)
        .await
        .map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "delete complete model request context attempts for compaction",
                error,
            )
        })
}

/// Examine one indexed page of old diagnostics. Whole pairs are reclaimed only
/// after logical settlement; unresolved or inconsistent evidence stays pinned.
pub async fn expire_model_request_context_events(
    pool: &SharedPool,
    batch_limit: u32,
    cursor: &mut ModelRequestContextExpiryCursor,
) -> ServiceResult<ModelRequestContextExpiryPage> {
    if batch_limit == 0 {
        return Ok(ModelRequestContextExpiryPage::default());
    }
    let batch_limit = batch_limit.min(1000);
    let page = tokio::time::timeout(std::time::Duration::from_secs(1), async {
    let mut connection = CancellationSafePoolConnection::acquire(pool.get()).await?;
    let cutoff = match cursor.cutoff {
        Some(cutoff) => cutoff,
        None => sqlx::query_scalar::<_, chrono::NaiveDateTime>(
            "SELECT CAST(DATE_SUB(NOW(6), INTERVAL ? DAY) AS DATETIME(6))")
            .bind(MODEL_REQUEST_CONTEXT_RETENTION_DAYS)
            .fetch_one(connection.connection_mut()).await?,
    };
    let mut query = QueryBuilder::<MySql>::new(
        "SELECT created_at, event_id, user_id, attempt_id FROM model_request_context_events WHERE created_at < ");
    query.push_bind(cutoff);
    if let Some((created_at, event_id, user_id)) = &cursor.after {
        query.push(" AND (created_at > ").push_bind(*created_at)
            .push(" OR (created_at = ").push_bind(*created_at)
            .push(" AND event_id > ").push_bind(event_id)
            .push(") OR (created_at = ").push_bind(*created_at)
            .push(" AND event_id = ").push_bind(event_id)
            .push(" AND user_id > ").push_bind(user_id).push("))");
    }
    query.push(" ORDER BY created_at, event_id, user_id LIMIT ").push_bind(batch_limit);
    let rows = query.build()
        .fetch_all(connection.connection_mut())
        .await?;
    let after = rows.last().map(|row| Ok::<_,sqlx::Error>((row.try_get("created_at")?, row.try_get("event_id")?, row.try_get("user_id")?)))
        .transpose()?;
    let keys = rows.iter().map(|row| Ok((row.try_get::<String,_>("user_id")?, row.try_get::<String,_>("attempt_id")?)))
        .collect::<Result<BTreeSet<_>, sqlx::Error>>()?.into_iter().collect::<Vec<_>>();
    let mut tx = connection.begin().await?;
    // Filter without locks first: pinned executions may hold their attempt
    // locks for unrelated work. Revalidate again under the eligible locks.
    let eligible = eligible_context_attempts(&mut tx, &keys, Some(cutoff)).await?;
    let deleted = prune_context_attempts(&mut tx, &eligible, Some(cutoff)).await?;
    tx.commit().await?;
    connection.release();
    Ok::<_, sqlx::Error>((cutoff, after, ModelRequestContextExpiryPage {
        examined: rows.len() as u64,
        deleted,
        more: rows.len() == batch_limit as usize,
    }))
    }).await.map_err(|error| ServiceError::with_source(ServiceErrorKind::Persistence, "request diagnostic expiry page timed out", error))?
        .map_err(|error| ServiceError::with_source(ServiceErrorKind::Persistence, "expire request diagnostic page", error))?;
    let (cutoff, after, page) = page;
    if page.more {
        cursor.cutoff = Some(cutoff);
        cursor.after = after;
    } else {
        *cursor = ModelRequestContextExpiryCursor::default();
    }
    Ok(page)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelRequestTopology {
    CliServer,
    EdgeServer,
    ServerOnly,
}

impl ModelRequestTopology {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CliServer => "cli_server",
            Self::EdgeServer => "edge_server",
            Self::ServerOnly => "server_only",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelRequestRolloutStage {
    #[default]
    Shadow,
    OptIn,
    TopologyCanary,
    Default,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRequestLineage {
    pub branch_id: Option<String>,
    pub journal_event_seq: Option<u64>,
    pub conversation_seq: Option<u64>,
    pub attachment_epoch: Option<u64>,
    pub writer_epoch: Option<u64>,
    pub run_generation: Option<u64>,
    pub provider_binding_generation: Option<u64>,
    pub delivery_generation: Option<u64>,
    pub authorization_epoch: Option<u64>,
    pub device_trust_epoch: Option<u64>,
    pub permission_epoch: Option<u64>,
    pub conversation_root_hash: Option<String>,
    pub prompt_assembly_manifest_hash: Option<String>,
    pub projection_schema: Option<String>,
    pub compaction_generation: Option<u64>,
    pub resume_source: Option<String>,
    pub checkpoint_id: Option<String>,
    pub fork_parent_session_id: Option<String>,
    pub fork_cursor: Option<u64>,
    pub fork_root_hash: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRequestBudget {
    pub raw_context_window_tokens: Option<u64>,
    pub usable_input_limit_tokens: Option<u64>,
    pub reserved_output_tokens: Option<u64>,
    pub reserved_summary_tokens: Option<u64>,
    pub reserved_protocol_tokens: Option<u64>,
    pub compact_trigger_tokens: Option<u64>,
    pub hard_limit_tokens: Option<u64>,
    pub estimated_input_tokens: Option<u64>,
    pub measured_input_tokens: Option<u64>,
    pub usage_source: Option<String>,
    pub estimate_error_tokens: Option<i64>,
    pub estimate_error_ratio: Option<f64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRequestComposition {
    pub stable_system_tokens: Option<u64>,
    pub dynamic_system_tokens: Option<u64>,
    pub history_user_tokens: Option<u64>,
    pub history_assistant_tokens: Option<u64>,
    pub history_tool_use_tokens: Option<u64>,
    pub history_tool_result_tokens: Option<u64>,
    pub memory_tokens: Option<u64>,
    pub visible_tool_schema_tokens: Option<u64>,
    pub deferred_tool_manifest_tokens: Option<u64>,
    pub runtime_volatile_tokens: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRequestCache {
    pub capability: Option<String>,
    pub layout: Option<String>,
    pub current_identity: Option<String>,
    pub previous_identity: Option<String>,
    pub invalidation_reasons: Vec<String>,
    pub eligible_tokens: Option<u64>,
    pub cache_read_share: Option<f64>,
    pub delta_reuse_tokens: Option<u64>,
    pub delta_append_tokens: Option<u64>,
    pub delta_replace_tokens: Option<u64>,
    pub delta_drop_tokens: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRequestCompaction {
    pub compaction_id: Option<String>,
    pub trigger: Option<String>,
    pub tier: Option<String>,
    pub method: Option<String>,
    pub before_tokens: Option<u64>,
    pub after_tokens: Option<u64>,
    pub freed_tokens: Option<u64>,
    pub before_messages: Option<u64>,
    pub after_messages: Option<u64>,
    pub protected_tokens: Option<u64>,
    pub protected_messages: Option<u64>,
    pub summary_tokens: Option<u64>,
    pub latency_ms: Option<u64>,
    pub insufficient: bool,
    pub futile: bool,
    pub circuit_open: bool,
}

/// Surface-owned facts known before final provider serialization.
///
/// Missing values remain explicit `null` fields in the event. Producers must
/// not guess them from provider/model names or prompt prose.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRequestContextSeed {
    pub topology: ModelRequestTopology,
    pub rollout_stage: ModelRequestRolloutStage,
    /// Physical execution round used only for diagnostics. Durable inference
    /// identity may intentionally use a canonical round so a content-addressed
    /// auxiliary subject cannot be retried by moving to another provider round.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_round: Option<u32>,
    /// Catalog-provided family used for low-cardinality aggregation. It is
    /// never inferred by substring matching the provider model name.
    pub model_family: Option<String>,
    pub actor_id: Option<String>,
    pub execution_principal: Option<String>,
    pub billing_scope: Option<String>,
    pub auth_session_id: Option<String>,
    pub device_instance_id: Option<String>,
    pub agent_id: Option<String>,
    pub parent_run_id: Option<String>,
    pub interaction_owner: String,
    pub loop_owner: String,
    pub execution_binding: String,
    pub lineage: ModelRequestLineage,
    pub budget: ModelRequestBudget,
    pub composition: ModelRequestComposition,
    pub cache: ModelRequestCache,
    pub compaction: ModelRequestCompaction,
}

impl ModelRequestContextSeed {
    #[must_use]
    pub fn server_default() -> Self {
        Self {
            topology: ModelRequestTopology::ServerOnly,
            rollout_stage: ModelRequestRolloutStage::Shadow,
            execution_round: None,
            model_family: None,
            actor_id: None,
            execution_principal: None,
            billing_scope: None,
            auth_session_id: None,
            device_instance_id: None,
            agent_id: None,
            parent_run_id: None,
            interaction_owner: "server".to_string(),
            loop_owner: "server".to_string(),
            execution_binding: "server".to_string(),
            lineage: ModelRequestLineage::default(),
            budget: ModelRequestBudget::default(),
            composition: ModelRequestComposition::default(),
            cache: ModelRequestCache::default(),
            compaction: ModelRequestCompaction::default(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRequestWireComposition {
    pub system_bytes: u64,
    pub conversation_bytes: u64,
    pub tool_schema_bytes: u64,
    pub provider_envelope_bytes: u64,
    pub system_items: u32,
    pub conversation_items: u32,
    pub tool_schema_items: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelRequestEventStage {
    Accepted,
    Terminal,
}

impl ModelRequestEventStage {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Terminal => "terminal",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRequestUsage {
    pub input: astra_turn_types::NormalizedPromptCacheUsage,
    pub output_tokens: u64,
}

impl ModelRequestUsage {
    /// Provider-normalized request input. The three stored input fields are
    /// the only authority; total input is always derived from their disjoint
    /// buckets so cache reads can never be divided by fresh input alone.
    #[must_use]
    pub fn total_input_tokens(&self) -> u64 {
        self.input.total_input_tokens()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRequestContextEvent {
    pub schema: String,
    pub stage: ModelRequestEventStage,
    pub identity: ModelRequestIdentity,
    /// Admitted route attribution, taken from the inference ledger rather
    /// than caller-supplied diagnostic metadata.
    pub route: ModelRequestRoute,
    pub lineage: ModelRequestLineage,
    pub budget: ModelRequestBudget,
    pub usage: Option<ModelRequestUsage>,
    pub composition: ModelRequestComposition,
    pub wire_composition: ModelRequestWireComposition,
    pub cache: ModelRequestCache,
    pub compaction: ModelRequestCompaction,
    pub terminal_status: Option<String>,
    /// Whether terminal token usage is exact, partial, or unavailable.
    #[serde(default)]
    pub usage_status: Option<String>,
    pub error_kind: Option<String>,
}

impl ModelRequestContextEvent {
    pub(crate) fn validate_accepted(&self) -> ServiceResult<()> {
        validate_model_request_event_projection(
            ModelRequestEventStage::Accepted,
            None,
            &self.schema,
            self.stage,
            self.terminal_status.as_deref(),
        )?;
        if self.identity.provider_response_id.is_some()
            || self.usage.is_some()
            || self.usage_status.is_some()
            || self.error_kind.is_some()
            || self.budget.measured_input_tokens.is_some()
            || self.budget.usage_source.is_some()
            || self.budget.estimate_error_tokens.is_some()
            || self.budget.estimate_error_ratio.is_some()
            || self.cache.cache_read_share.is_some()
        {
            return Err(ServiceError::conflict(
                "accepted request context contains terminal facts",
            ));
        }
        Ok(())
    }

    pub(crate) fn validate_stored_identity(
        &self,
        user_id: &str,
        session_id: Option<&str>,
        run_id: Option<&str>,
        attempt_id: &str,
        invocation_id: &str,
    ) -> ServiceResult<()> {
        if self.identity.owner_scope != user_id
            || self.identity.session_id.as_deref() != session_id
            || self.identity.run_id.as_deref() != run_id
            || self.identity.request_id != attempt_id
            || self.route.invocation_id != invocation_id
        {
            return Err(ServiceError::conflict(
                "model request payload disagrees with its stored identity",
            ));
        }
        Ok(())
    }
}

/// Terminal-only durable facts. Immutable request context belongs to the
/// accepted row; public observations hydrate both through the shared reader.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelRequestTerminalFact {
    pub schema: String,
    pub terminal_status: String,
    pub provider_response_id: Option<String>,
    pub usage_status: String,
    pub usage: Option<ModelRequestUsage>,
    pub error_kind: Option<String>,
    pub usage_source: String,
}

pub(crate) const MODEL_REQUEST_TERMINAL_SCHEMA: &str = "model_request_terminal_v1";

impl ModelRequestTerminalFact {
    pub(crate) fn validate(&self) -> ServiceResult<()> {
        if self.schema != MODEL_REQUEST_TERMINAL_SCHEMA
            || !matches!(
                self.terminal_status.as_str(),
                "succeeded" | "failed" | "cancelled" | "delivery_unknown"
            )
            || !matches!(
                self.usage_source.as_str(),
                "provider_terminal" | "provider_terminal_recovery"
            )
        {
            return Err(ServiceError::conflict(
                "unsupported model request terminal fact",
            ));
        }
        match (self.usage_status.as_str(), self.usage.is_some()) {
            ("unavailable", false) | ("provider_exact" | "provider_partial", true) => Ok(()),
            _ => Err(ServiceError::conflict(
                "model request terminal usage disagrees with coverage",
            )),
        }
    }

    pub(crate) fn hydrate(
        &self,
        mut accepted: ModelRequestContextEvent,
    ) -> ServiceResult<ModelRequestContextEvent> {
        self.validate()?;
        accepted.validate_accepted()?;
        accepted.stage = ModelRequestEventStage::Terminal;
        accepted
            .identity
            .provider_response_id
            .clone_from(&self.provider_response_id);
        accepted.terminal_status = Some(self.terminal_status.clone());
        accepted.error_kind.clone_from(&self.error_kind);
        apply_model_request_terminal_usage(
            &mut accepted,
            self.usage.clone(),
            &self.usage_status,
            &self.usage_source,
        )?;
        Ok(accepted)
    }
}

pub(crate) fn clear_model_request_measured_usage(event: &mut ModelRequestContextEvent) {
    event.usage = None;
    event.budget.measured_input_tokens = None;
    event.budget.usage_source = None;
    event.budget.estimate_error_tokens = None;
    event.budget.estimate_error_ratio = None;
    event.cache.cache_read_share = None;
}

/// One projection for foreground observations, recovery and database reads.
pub(crate) fn apply_model_request_terminal_usage(
    event: &mut ModelRequestContextEvent,
    usage: Option<ModelRequestUsage>,
    usage_status: &str,
    source: &str,
) -> ServiceResult<()> {
    clear_model_request_measured_usage(event);
    event.usage_status = Some(usage_status.to_string());
    let (exact, usage) = match (usage_status, usage) {
        ("unavailable", None) => return Ok(()),
        ("provider_exact", Some(usage)) => (true, usage),
        ("provider_partial", Some(usage)) => (false, usage),
        _ => {
            return Err(ServiceError::conflict(
                "model request usage disagrees with coverage",
            ));
        }
    };
    if exact {
        let measured = usage.total_input_tokens();
        event.budget.measured_input_tokens = Some(measured);
        event.budget.usage_source = Some(source.to_string());
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
            projected_cache_read_share(&event.identity.provider_protocol, usage_status, &usage);
    }
    event.usage = Some(usage);
    Ok(())
}

pub(crate) fn projected_cache_read_share(
    protocol: &str,
    status: &str,
    usage: &ModelRequestUsage,
) -> Option<f64> {
    let total = usage.total_input_tokens();
    (protocol != "typesafe_systemone" && status == "provider_exact" && total > 0)
        .then(|| usage.input.cache_read_tokens as f64 / total as f64)
}

/// Non-secret linkage between a physical request and its admitted route.
///
/// Physical retries share an invocation and route. A new logical invocation
/// has its own route; this is not an adaptive router decision identifier.
/// These facts describe execution, not response quality or selection policy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRequestRoute {
    pub route_id: String,
    pub invocation_id: String,
    pub upstream_model: String,
    pub execution_placement: crate::models::ModelExecutionPlacement,
    pub access_kind: crate::models::ModelAccessKind,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRequestIdentity {
    pub request_id: String,
    pub provider_response_id: Option<String>,
    pub owner_scope: String,
    pub session_id: Option<String>,
    pub run_id: Option<String>,
    pub turn: Option<u32>,
    pub round: Option<u32>,
    pub logical_attempt: u32,
    pub physical_attempt: u32,
    pub actor_id: Option<String>,
    pub execution_principal: Option<String>,
    pub billing_scope: Option<String>,
    pub auth_session_id: Option<String>,
    pub device_instance_id: Option<String>,
    pub agent_id: Option<String>,
    pub parent_run_id: Option<String>,
    pub topology: ModelRequestTopology,
    pub interaction_owner: String,
    pub loop_owner: String,
    pub execution_binding: String,
    pub provider: String,
    pub model: String,
    pub offering_id: String,
    pub inference_purpose: String,
    /// Canonical business operation within the inference purpose.
    pub operation_id: String,
    pub provider_protocol: String,
    pub provider_wire_hash: String,
    pub provider_wire_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRequestContextRecord {
    pub event_id: String,
    pub stage: ModelRequestEventStage,
    pub terminal_status: Option<String>,
    pub event: ModelRequestContextEvent,
    pub created_at: chrono::NaiveDateTime,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRequestMetricsRow {
    pub topology: String,
    pub provider: String,
    pub model_family: String,
    pub purpose: String,
    pub terminal_status: String,
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRequestTraceCoverage {
    pub accepted_requests: u64,
    pub terminal_requests: u64,
    pub open_requests: u64,
}

fn bounded_limit(limit: u32) -> i64 {
    i64::from(limit.clamp(1, 500))
}

fn non_negative_u64(value: i64, column: &'static str) -> ServiceResult<u64> {
    u64::try_from(value).map_err(|_| {
        ServiceError::conflict(format!(
            "model request context column {column} is negative: {value}"
        ))
    })
}

fn row_non_negative_u64(row: &sqlx::mysql::MySqlRow, column: &'static str) -> ServiceResult<u64> {
    let value = row.try_get::<i64, _>(column).map_err(|error| {
        ServiceError::with_source(
            ServiceErrorKind::Persistence,
            format!("decode model request context column {column}"),
            error,
        )
    })?;
    non_negative_u64(value, column)
}

fn row_string(row: &sqlx::mysql::MySqlRow, column: &'static str) -> ServiceResult<String> {
    row.try_get(column).map_err(|error| {
        ServiceError::with_source(
            ServiceErrorKind::Persistence,
            format!("decode model request context column {column}"),
            error,
        )
    })
}

fn validate_model_request_event_projection(
    stored_stage: ModelRequestEventStage,
    stored_terminal_status: Option<&str>,
    event_schema: &str,
    event_stage: ModelRequestEventStage,
    event_terminal_status: Option<&str>,
) -> ServiceResult<()> {
    if event_schema != MODEL_REQUEST_CONTEXT_SCHEMA {
        return Err(ServiceError::conflict(format!(
            "stored model request context uses unsupported schema {}",
            event_schema
        )));
    }
    if event_stage != stored_stage {
        return Err(ServiceError::conflict(
            "stored model request context stage disagrees with its typed payload",
        ));
    }
    if event_terminal_status != stored_terminal_status {
        return Err(ServiceError::conflict(
            "stored model request context terminal status disagrees with its typed payload",
        ));
    }
    match (stored_stage, stored_terminal_status) {
        (ModelRequestEventStage::Accepted, None) | (ModelRequestEventStage::Terminal, Some(_)) => {
            Ok(())
        }
        _ => Err(ServiceError::conflict(
            "stored model request context stage has an invalid terminal status",
        )),
    }
}

/// Query the append-only per-request trace without scanning raw prompt data.
pub async fn list_model_request_context_events(
    pool: &SharedPool,
    user_id: &str,
    session_id: &str,
    limit: u32,
) -> ServiceResult<Vec<ModelRequestContextRecord>> {
    if user_id.is_empty() || session_id.is_empty() {
        return Err(ServiceError::invalid(
            "model request context query requires owner and session identity",
        ));
    }
    // MatrixOne does not infer tenant predicates through the limited page CTE.
    // Keep the accepted-side predicates explicit to avoid a global scan.
    let rows = sqlx::query(
        "WITH page AS (
             SELECT event_id, attempt_id, invocation_id, user_id, session_id, run_id,
                    event_stage, terminal_status, event_json, created_at
             FROM model_request_context_events
             WHERE user_id = ? AND session_id = ?
             ORDER BY created_at DESC, event_id DESC LIMIT ?
         )
         SELECT page.*, accepted.event_json AS accepted_json
         FROM page LEFT JOIN model_request_context_events AS accepted
           ON page.event_stage = 'terminal'
          AND accepted.user_id = page.user_id
          AND accepted.session_id = page.session_id
          AND accepted.attempt_id = page.attempt_id
          AND accepted.invocation_id = page.invocation_id
          AND accepted.run_id <=> page.run_id
          AND accepted.event_stage = 'accepted'
          AND accepted.user_id = ? AND accepted.session_id = ?
         ORDER BY page.created_at DESC, page.event_id DESC",
    )
    .bind(user_id)
    .bind(session_id)
    .bind(bounded_limit(limit))
    .bind(user_id)
    .bind(session_id)
    .fetch_all(pool.get())
    .await
    .map_err(|error| {
        ServiceError::with_source(
            ServiceErrorKind::Persistence,
            "list model request context events",
            error,
        )
    })?;
    let mut records = Vec::with_capacity(rows.len());
    for row in rows {
        let event_stage: String = row.try_get("event_stage").map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "decode model request context stage",
                error,
            )
        })?;
        let stage = match event_stage.as_str() {
            "accepted" => ModelRequestEventStage::Accepted,
            "terminal" => ModelRequestEventStage::Terminal,
            other => {
                return Err(ServiceError::conflict(format!(
                    "stored model request context has unknown stage {other}"
                )));
            }
        };
        let event_json: String = row.try_get("event_json").map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "decode model request context payload",
                error,
            )
        })?;
        let decode_context = |json: &str| -> ServiceResult<ModelRequestContextEvent> {
            serde_json::from_str(json).map_err(|error| {
                ServiceError::with_source(
                    ServiceErrorKind::Persistence,
                    "decode model request context event",
                    error,
                )
            })
        };
        let event = match stage {
            ModelRequestEventStage::Accepted => {
                let event = decode_context(&event_json)?;
                event.validate_accepted()?;
                event
            }
            ModelRequestEventStage::Terminal => {
                let fact: ModelRequestTerminalFact =
                    serde_json::from_str(&event_json).map_err(|error| {
                        ServiceError::with_source(
                            ServiceErrorKind::Persistence,
                            "decode model request terminal fact",
                            error,
                        )
                    })?;
                let accepted_json: Option<String> =
                    row.try_get("accepted_json").map_err(|error| {
                        ServiceError::with_source(
                            ServiceErrorKind::Persistence,
                            "decode model request accepted partner",
                            error,
                        )
                    })?;
                let accepted = accepted_json.ok_or_else(|| {
                    ServiceError::conflict("model request terminal has no exact accepted context")
                })?;
                fact.hydrate(decode_context(&accepted)?)?
            }
        };
        let attempt_id = row_string(&row, "attempt_id")?;
        let invocation_id = row_string(&row, "invocation_id")?;
        let run_id: Option<String> = row.try_get("run_id").map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "decode model request run identity",
                error,
            )
        })?;
        event.validate_stored_identity(
            user_id,
            Some(session_id),
            run_id.as_deref(),
            &attempt_id,
            &invocation_id,
        )?;
        let terminal_status: Option<String> = row.try_get("terminal_status").map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "decode model request context terminal_status",
                error,
            )
        })?;
        validate_model_request_event_projection(
            stage,
            terminal_status.as_deref(),
            &event.schema,
            event.stage,
            event.terminal_status.as_deref(),
        )?;
        records.push(ModelRequestContextRecord {
            event_id: row.try_get("event_id").map_err(|error| {
                ServiceError::with_source(
                    ServiceErrorKind::Persistence,
                    "decode model request context event_id",
                    error,
                )
            })?,
            stage,
            terminal_status,
            event,
            created_at: row.try_get("created_at").map_err(|error| {
                ServiceError::with_source(
                    ServiceErrorKind::Persistence,
                    "decode model request context created_at",
                    error,
                )
            })?,
        })
    }
    Ok(records)
}

/// Low-cardinality database projection suitable for metrics/dashboard
/// scrapers. Model family is an explicit catalog fact, never parsed from a
/// model name.
pub async fn aggregate_model_request_metrics(
    pool: &SharedPool,
) -> ServiceResult<Vec<ModelRequestMetricsRow>> {
    let rows = sqlx::query(
        "SELECT topology, provider, model_family, purpose, terminal_status,
                CAST(COALESCE(SUM(requests), 0) AS SIGNED) AS requests,
                CAST(COALESCE(SUM(input_tokens), 0) AS SIGNED) AS input_tokens,
                CAST(COALESCE(SUM(output_tokens), 0) AS SIGNED) AS output_tokens,
                CAST(COALESCE(SUM(cache_read_tokens), 0) AS SIGNED) AS cache_read_tokens,
                CAST(COALESCE(SUM(cache_creation_tokens), 0) AS SIGNED) AS cache_creation_tokens
         FROM model_request_metric_shards
         GROUP BY topology, provider, model_family, purpose, terminal_status
         ORDER BY topology, provider, model_family, purpose, terminal_status",
    )
    .fetch_all(pool.get())
    .await
    .map_err(|error| {
        ServiceError::with_source(
            ServiceErrorKind::Persistence,
            "aggregate model request context metrics",
            error,
        )
    })?;
    rows.into_iter()
        .map(|row| -> ServiceResult<ModelRequestMetricsRow> {
            Ok(ModelRequestMetricsRow {
                topology: row_string(&row, "topology")?,
                provider: row_string(&row, "provider")?,
                model_family: row_string(&row, "model_family")?,
                purpose: row_string(&row, "purpose")?,
                terminal_status: row_string(&row, "terminal_status")?,
                requests: row_non_negative_u64(&row, "requests")?,
                input_tokens: row_non_negative_u64(&row, "input_tokens")?,
                output_tokens: row_non_negative_u64(&row, "output_tokens")?,
                cache_read_tokens: row_non_negative_u64(&row, "cache_read_tokens")?,
                cache_creation_tokens: row_non_negative_u64(&row, "cache_creation_tokens")?,
            })
        })
        .collect()
}

pub async fn model_request_trace_coverage(
    pool: &SharedPool,
    user_id: &str,
    session_id: &str,
) -> ServiceResult<ModelRequestTraceCoverage> {
    let row = sqlx::query(
        "SELECT
            CAST(SUM(CASE WHEN accepted.event_stage = 'accepted' THEN 1 ELSE 0 END) AS SIGNED)
                AS accepted_requests,
            CAST(SUM(CASE WHEN terminal.event_stage = 'terminal' THEN 1 ELSE 0 END) AS SIGNED)
                AS terminal_requests
         FROM model_request_context_events AS accepted
         LEFT JOIN model_request_context_events AS terminal
           ON terminal.user_id = accepted.user_id
          AND terminal.attempt_id = accepted.attempt_id
          AND terminal.event_stage = 'terminal'
         WHERE accepted.user_id = ? AND accepted.session_id = ?
           AND accepted.event_stage = 'accepted'",
    )
    .bind(user_id)
    .bind(session_id)
    .fetch_one(pool.get())
    .await
    .map_err(|error| {
        ServiceError::with_source(
            ServiceErrorKind::Persistence,
            "measure model request trace coverage",
            error,
        )
    })?;
    let accepted_requests = non_negative_u64(
        row.try_get::<Option<i64>, _>("accepted_requests")
            .map_err(|error| {
                ServiceError::with_source(
                    ServiceErrorKind::Persistence,
                    "decode accepted model request trace count",
                    error,
                )
            })?
            .unwrap_or(0),
        "accepted_requests",
    )?;
    let terminal_requests = non_negative_u64(
        row.try_get::<Option<i64>, _>("terminal_requests")
            .map_err(|error| {
                ServiceError::with_source(
                    ServiceErrorKind::Persistence,
                    "decode terminal model request trace count",
                    error,
                )
            })?
            .unwrap_or(0),
        "terminal_requests",
    )?;
    Ok(ModelRequestTraceCoverage {
        accepted_requests,
        terminal_requests,
        open_requests: accepted_requests.saturating_sub(terminal_requests),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_projection_requires_the_exact_latest_typed_contract() {
        assert!(
            validate_model_request_event_projection(
                ModelRequestEventStage::Accepted,
                None,
                MODEL_REQUEST_CONTEXT_SCHEMA,
                ModelRequestEventStage::Accepted,
                None,
            )
            .is_ok()
        );
        assert!(
            validate_model_request_event_projection(
                ModelRequestEventStage::Terminal,
                Some("succeeded"),
                MODEL_REQUEST_CONTEXT_SCHEMA,
                ModelRequestEventStage::Terminal,
                Some("succeeded"),
            )
            .is_ok()
        );

        for invalid in [
            validate_model_request_event_projection(
                ModelRequestEventStage::Terminal,
                Some("succeeded"),
                "model_request_context_v1",
                ModelRequestEventStage::Terminal,
                Some("succeeded"),
            ),
            validate_model_request_event_projection(
                ModelRequestEventStage::Terminal,
                Some("succeeded"),
                MODEL_REQUEST_CONTEXT_SCHEMA,
                ModelRequestEventStage::Accepted,
                Some("succeeded"),
            ),
            validate_model_request_event_projection(
                ModelRequestEventStage::Terminal,
                Some("failed"),
                MODEL_REQUEST_CONTEXT_SCHEMA,
                ModelRequestEventStage::Terminal,
                Some("succeeded"),
            ),
            validate_model_request_event_projection(
                ModelRequestEventStage::Terminal,
                None,
                MODEL_REQUEST_CONTEXT_SCHEMA,
                ModelRequestEventStage::Terminal,
                None,
            ),
        ] {
            assert!(invalid.is_err());
        }
    }
}
