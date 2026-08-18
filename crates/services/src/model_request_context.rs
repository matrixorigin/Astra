//! Typed, content-free observability for one physical model request.
//!
//! A request-context event is deliberately assembled from deterministic
//! admission, wire, budget, and provider-terminal facts. It never attempts to
//! infer intent from prompt text, and it never stores messages or tool output.

use serde::{Deserialize, Serialize};
use sqlx::{MySql, QueryBuilder, Row};

use astra_core::SharedPool;

use crate::{ServiceError, ServiceErrorKind, ServiceResult};

pub const MODEL_REQUEST_CONTEXT_SCHEMA: &str = "model_request_context_v1";
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

/// A durable owner scope for bounded request diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ModelRequestContextScope<'a> {
    Session(&'a str),
    HarnessRun(&'a str),
}

impl<'a> ModelRequestContextScope<'a> {
    fn column(self) -> &'static str {
        match self {
            Self::Session(_) => "session_id",
            Self::HarnessRun(_) => "harness_run_id",
        }
    }

    fn id(self) -> &'a str {
        match self {
            Self::Session(id) | Self::HarnessRun(id) => id,
        }
    }
}

fn event_beyond_scope_limit_sql(scope: ModelRequestContextScope<'_>) -> String {
    format!(
        "SELECT event_id FROM model_request_context_events
         WHERE user_id = ? AND {} = ?
         ORDER BY created_at DESC, event_id DESC
         LIMIT 1 OFFSET ?",
        scope.column()
    )
}

fn oldest_complete_attempts_in_scope_sql(scope: ModelRequestContextScope<'_>) -> String {
    format!(
        "SELECT attempt_id FROM model_request_context_events
         WHERE user_id = ? AND {} = ?
         GROUP BY attempt_id
         HAVING COUNT(*) = 2
         ORDER BY MIN(created_at) ASC, attempt_id ASC
         LIMIT ?",
        scope.column()
    )
}

const EXPIRED_ATTEMPTS_SQL: &str = "SELECT user_id, attempt_id
     FROM model_request_context_events
     GROUP BY user_id, attempt_id
     HAVING MAX(created_at) < DATE_SUB(NOW(6), INTERVAL ? DAY)
     ORDER BY MAX(created_at) ASC, user_id ASC, attempt_id ASC
     LIMIT ?";

fn delete_complete_attempts_query<'a>(
    user_id: &'a str,
    attempt_ids: &'a [String],
) -> QueryBuilder<'a, MySql> {
    let mut query =
        QueryBuilder::<MySql>::new("DELETE FROM model_request_context_events WHERE user_id = ");
    query.push_bind(user_id);
    query.push(" AND attempt_id IN (");
    {
        let mut attempts = query.separated(", ");
        for attempt_id in attempt_ids {
            attempts.push_bind(attempt_id);
        }
    }
    query.push(")");
    query
}

fn delete_attempt_keys_query<'a>(attempt_keys: &'a [(String, String)]) -> QueryBuilder<'a, MySql> {
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

/// Keep a request-diagnostic scope bounded after appending an event.
///
/// The event rows are diagnostic observability, not user-visible conversation
/// history. Complete attempts are pruned as accepted/terminal pairs. If the
/// scope is over the limit but contains only incomplete attempts, it remains
/// temporarily over the limit until a terminal fact arrives.
pub(crate) async fn compact_model_request_context_scope(
    connection: &mut sqlx::MySqlConnection,
    user_id: &str,
    scope: ModelRequestContextScope<'_>,
) -> ServiceResult<u64> {
    let probe_sql = event_beyond_scope_limit_sql(scope);
    let over_limit = sqlx::query_scalar::<_, String>(&probe_sql)
        .bind(user_id)
        .bind(scope.id())
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

    let completed_attempts_sql = oldest_complete_attempts_in_scope_sql(scope);
    let attempt_ids = sqlx::query_scalar::<_, String>(&completed_attempts_sql)
        .bind(user_id)
        .bind(scope.id())
        .bind(MODEL_REQUEST_CONTEXT_COMPACTION_ATTEMPT_BATCH_LIMIT)
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

    let mut delete = delete_complete_attempts_query(user_id, &attempt_ids);
    delete
        .build()
        .execute(&mut *connection)
        .await
        .map(|result| result.rows_affected())
        .map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "delete complete model request context attempts for compaction",
                error,
            )
        })
}

/// Delete diagnostic attempts after their explicit retention window.
/// This also covers harness runs, which have no session row to delete. Each
/// attempt is removed atomically only after its newest diagnostic fact has
/// exceeded the retention window, so complete pairs remain intact and stale
/// incomplete attempts cannot survive forever after a process failure.
pub(crate) async fn expire_model_request_context_events(
    pool: &SharedPool,
    batch_limit: u32,
) -> ServiceResult<u64> {
    let attempt_keys = sqlx::query_as::<_, (String, String)>(EXPIRED_ATTEMPTS_SQL)
        .bind(MODEL_REQUEST_CONTEXT_RETENTION_DAYS)
        .bind(batch_limit)
        .fetch_all(pool.get())
        .await
        .map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "select expired model request context attempts",
                error,
            )
        })?;

    if attempt_keys.is_empty() {
        return Ok(0);
    }

    let mut delete = delete_attempt_keys_query(&attempt_keys);
    delete
        .build()
        .execute(pool.get())
        .await
        .map(|result| result.rows_affected())
        .map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "expire model request context attempts",
                error,
            )
        })
}

#[cfg(test)]
mod retention_tests {
    use super::*;

    #[test]
    fn diagnostic_retention_policy_is_explicit_and_bounded() {
        assert_eq!(MODEL_REQUEST_CONTEXT_RETENTION_DAYS, 30);
        assert_eq!(MAX_MODEL_REQUEST_CONTEXT_EVENTS_PER_SCOPE, 2048);
        assert_eq!(MODEL_REQUEST_CONTEXT_COMPACTION_ATTEMPT_BATCH_LIMIT, 256);
    }

    #[test]
    fn diagnostic_scope_selects_the_owner_specific_index_column() {
        assert_eq!(
            ModelRequestContextScope::Session("s-1").column(),
            "session_id"
        );
        assert_eq!(
            ModelRequestContextScope::HarnessRun("h-1").column(),
            "harness_run_id"
        );
    }

    #[test]
    fn scope_compaction_and_expiry_keep_attempts_atomic() {
        use sqlx::Execute;

        let scope = ModelRequestContextScope::Session("s-1");
        let probe = event_beyond_scope_limit_sql(scope);
        let completed_attempts = oldest_complete_attempts_in_scope_sql(scope);
        let attempts = vec!["attempt-1".to_string()];
        let mut delete_builder = delete_complete_attempts_query("user-1", &attempts);
        let delete = delete_builder.build();
        let expired_attempts = EXPIRED_ATTEMPTS_SQL;
        let attempt_keys = vec![("user-1".to_string(), "attempt-1".to_string())];
        let mut expiry_delete_builder = delete_attempt_keys_query(&attempt_keys);
        let expiry_delete = expiry_delete_builder.build();

        assert!(probe.contains("LIMIT 1 OFFSET ?"));
        assert!(completed_attempts.contains("GROUP BY attempt_id"));
        assert!(completed_attempts.contains("HAVING COUNT(*) = 2"));
        assert!(completed_attempts.contains("ORDER BY MIN(created_at) ASC, attempt_id ASC"));
        assert!(delete.sql().contains("AND attempt_id IN"));
        assert!(!delete.sql().contains("event_id IN"));
        assert!(expired_attempts.contains("GROUP BY user_id, attempt_id"));
        assert!(expired_attempts.contains("HAVING MAX(created_at) <"));
        assert!(expired_attempts.contains("ORDER BY MAX(created_at) ASC"));
        assert!(!expired_attempts.contains("HAVING COUNT(*) = 2"));
        assert!(
            expiry_delete
                .sql()
                .contains("user_id = ? AND attempt_id = ?")
        );
    }
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
    pub fresh_input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub request_input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRequestContextEvent {
    pub schema: String,
    pub stage: ModelRequestEventStage,
    pub identity: ModelRequestIdentity,
    pub lineage: ModelRequestLineage,
    pub budget: ModelRequestBudget,
    pub usage: Option<ModelRequestUsage>,
    pub composition: ModelRequestComposition,
    pub wire_composition: ModelRequestWireComposition,
    pub cache: ModelRequestCache,
    pub compaction: ModelRequestCompaction,
    pub terminal_status: Option<String>,
    pub error_kind: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRequestIdentity {
    pub request_id: String,
    pub provider_response_id: Option<String>,
    pub owner_scope: String,
    pub session_id: Option<String>,
    pub run_id: Option<String>,
    pub harness_run_id: Option<String>,
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
    let rows = sqlx::query(
        "SELECT event_id, event_stage, terminal_status, event_json, created_at
         FROM model_request_context_events
         WHERE user_id = ? AND session_id = ?
         ORDER BY created_at DESC, event_id DESC
         LIMIT ?",
    )
    .bind(user_id)
    .bind(session_id)
    .bind(bounded_limit(limit))
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
        let event = serde_json::from_str(&event_json).map_err(|error| {
            ServiceError::with_source(
                ServiceErrorKind::Persistence,
                "decode model request context event",
                error,
            )
        })?;
        records.push(ModelRequestContextRecord {
            event_id: row.try_get("event_id").map_err(|error| {
                ServiceError::with_source(
                    ServiceErrorKind::Persistence,
                    "decode model request context event_id",
                    error,
                )
            })?,
            stage,
            terminal_status: row.try_get("terminal_status").map_err(|error| {
                ServiceError::with_source(
                    ServiceErrorKind::Persistence,
                    "decode model request context terminal_status",
                    error,
                )
            })?,
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
                COALESCE(SUM(requests), 0) AS requests,
                COALESCE(SUM(input_tokens), 0) AS input_tokens,
                COALESCE(SUM(output_tokens), 0) AS output_tokens,
                COALESCE(SUM(cache_read_tokens), 0) AS cache_read_tokens,
                COALESCE(SUM(cache_creation_tokens), 0) AS cache_creation_tokens
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
            SUM(CASE WHEN accepted.event_stage = 'accepted' THEN 1 ELSE 0 END)
                AS accepted_requests,
            SUM(CASE WHEN terminal.event_stage = 'terminal' THEN 1 ELSE 0 END)
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
