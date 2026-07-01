use async_trait::async_trait;
use axum::http::StatusCode;
use serde_json::Value;
use sqlx::{MySql, QueryBuilder, Row, query};
use std::collections::HashMap;

use astra_core::{MatrixOneSettings, SharedPool, error_response, internal_error};

use crate::storage::agent_session_exists_for_user;

use super::scoring::{
    DEGRADATION_DELTA, QUALITY_DEGRADED, QUALITY_GOOD, TOKEN_CHAR_RATIO, analyze_context_health,
    billable_input_from_canonical, compaction_effectiveness, compaction_forecast, compute_drift,
    compute_trend, pollution_ratio, relevance_quality, zone_balance,
};
use super::{IntrospectionService, ServiceResult, SkillInfo, SkillsIntrospectionResponse};

const MAX_INTROSPECTION_USAGE_ROWS: i32 = 128;
const ASK_USER_HISTORY_EVENT_LIMIT: i32 = 50;

#[derive(Clone, Debug)]
pub struct DatabaseIntrospectionService {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
}

impl DatabaseIntrospectionService {
    pub fn new(matrixone: MatrixOneSettings) -> Self {
        Self {
            matrixone,
            pool: None,
        }
    }

    async fn verify_session_owner(
        &self,
        pool: &sqlx::Pool<sqlx::MySql>,
        session_id: &str,
        user_id: &str,
    ) -> ServiceResult<()> {
        if agent_session_exists_for_user(pool, session_id, user_id)
            .await
            .map_err(internal_error)?
        {
            Ok(())
        } else {
            Err(error_response(StatusCode::NOT_FOUND, "Session not found"))
        }
    }
    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.pool = Some(pool);
        self
    }

    async fn get_pool(&self) -> Result<sqlx::Pool<sqlx::MySql>, sqlx::Error> {
        crate::require_shared_pool(
            self.pool.as_ref(),
            "DatabaseIntrospectionService",
            &self.matrixone,
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
struct AskUserHistoryRow {
    session_id: String,
    event_type: String,
    created_at: String,
    metadata: Value,
    content_preview: String,
}

trait IntrospectionRow {
    fn string_column(&self, column: &str) -> Result<String, sqlx::Error>;
    fn optional_string_column(&self, column: &str) -> Result<Option<String>, sqlx::Error>;
    fn i64_column(&self, column: &str) -> Result<i64, sqlx::Error>;
    fn optional_i64_column(&self, column: &str) -> Result<Option<i64>, sqlx::Error>;
}

impl IntrospectionRow for sqlx::mysql::MySqlRow {
    fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
        self.try_get(column)
    }

    fn optional_string_column(&self, column: &str) -> Result<Option<String>, sqlx::Error> {
        self.try_get(column)
    }

    fn i64_column(&self, column: &str) -> Result<i64, sqlx::Error> {
        self.try_get(column)
    }

    fn optional_i64_column(&self, column: &str) -> Result<Option<i64>, sqlx::Error> {
        self.try_get(column)
    }
}

fn introspection_decode_error(
    context: &str,
    column: &str,
    error: impl std::fmt::Display,
) -> (StatusCode, axum::Json<astra_core::ErrorResponse>) {
    internal_error(format!(
        "introspection {context} decode column `{column}`: {error}"
    ))
}

fn introspection_row_string(
    row: &impl IntrospectionRow,
    context: &str,
    column: &str,
) -> ServiceResult<String> {
    row.string_column(column)
        .map_err(|error| introspection_decode_error(context, column, error))
}

fn introspection_row_optional_string(
    row: &impl IntrospectionRow,
    context: &str,
    column: &str,
) -> ServiceResult<Option<String>> {
    row.optional_string_column(column)
        .map_err(|error| introspection_decode_error(context, column, error))
}

fn introspection_row_i64(
    row: &impl IntrospectionRow,
    context: &str,
    column: &str,
) -> ServiceResult<i64> {
    row.i64_column(column)
        .map_err(|error| introspection_decode_error(context, column, error))
}

fn introspection_row_optional_i64(
    row: &impl IntrospectionRow,
    context: &str,
    column: &str,
) -> ServiceResult<Option<i64>> {
    row.optional_i64_column(column)
        .map_err(|error| introspection_decode_error(context, column, error))
}

fn introspection_row_non_negative_i64(
    row: &impl IntrospectionRow,
    context: &str,
    column: &str,
) -> ServiceResult<i64> {
    let value = introspection_row_i64(row, context, column)?;
    if value < 0 {
        return Err(introspection_decode_error(
            context,
            column,
            format!("expected non-negative integer, got {value}"),
        ));
    }
    Ok(value)
}

fn introspection_row_optional_non_negative_i64(
    row: &impl IntrospectionRow,
    context: &str,
    column: &str,
) -> ServiceResult<Option<i64>> {
    let Some(value) = introspection_row_optional_i64(row, context, column)? else {
        return Ok(None);
    };
    if value < 0 {
        return Err(introspection_decode_error(
            context,
            column,
            format!("expected non-negative integer, got {value}"),
        ));
    }
    Ok(Some(value))
}

fn introspection_required_non_empty_string(
    row: &impl IntrospectionRow,
    context: &str,
    column: &str,
) -> ServiceResult<String> {
    let value = introspection_row_string(row, context, column)?;
    if value.trim().is_empty() {
        return Err(introspection_decode_error(
            context,
            column,
            "expected non-empty string",
        ));
    }
    Ok(value)
}

fn installed_skill_info_from_row(row: &impl IntrospectionRow) -> ServiceResult<SkillInfo> {
    let context = "installed_skill_row";
    Ok(SkillInfo {
        name: introspection_required_non_empty_string(row, context, "skill_name")?,
        version: introspection_required_non_empty_string(row, context, "skill_version")?,
        description: introspection_row_string(row, context, "description")?,
        category: introspection_row_string(row, context, "category")?,
    })
}

fn cloud_skill_info_from_row(row: &impl IntrospectionRow) -> ServiceResult<SkillInfo> {
    let context = "cloud_skill_row";
    Ok(SkillInfo {
        name: introspection_required_non_empty_string(row, context, "skill_name")?,
        version: introspection_required_non_empty_string(row, context, "version")?,
        description: introspection_row_string(row, context, "description")?,
        category: introspection_row_string(row, context, "category")?,
    })
}

fn decision_trace_row_from_row(row: &impl IntrospectionRow) -> ServiceResult<Value> {
    let context = "decision_trace_row";
    let output_json = introspection_row_string(row, context, "output_json")?;
    let output: Value = serde_json::from_str(&output_json)
        .map_err(|error| introspection_decode_error(context, "output_json", error))?;

    Ok(serde_json::json!({
        "decision_id": introspection_required_non_empty_string(row, context, "decision_id")?,
        "event_id": introspection_required_non_empty_string(row, context, "event_id")?,
        "decision_type": introspection_required_non_empty_string(row, context, "decision_type")?,
        "model_used": introspection_row_optional_string(row, context, "model_used")?,
        "created_at": introspection_required_non_empty_string(row, context, "created_at")?,
        "output": output,
    }))
}

#[derive(Debug, Clone, PartialEq)]
struct ToolHistoryAgg {
    tool_total_calls: i64,
    tool_fail_count: i64,
    ask_user_total_calls: i64,
    ask_user_fail_count: i64,
}

fn tool_history_agg_from_row(row: &impl IntrospectionRow) -> ServiceResult<ToolHistoryAgg> {
    let context = "tool_history_agg_row";
    let agg = ToolHistoryAgg {
        tool_total_calls: introspection_row_non_negative_i64(row, context, "tool_total_calls")?,
        tool_fail_count: introspection_row_non_negative_i64(row, context, "tool_fail_count")?,
        ask_user_total_calls: introspection_row_non_negative_i64(
            row,
            context,
            "ask_user_total_calls",
        )?,
        ask_user_fail_count: introspection_row_non_negative_i64(
            row,
            context,
            "ask_user_fail_count",
        )?,
    };

    if agg.tool_fail_count > agg.tool_total_calls {
        return Err(introspection_decode_error(
            context,
            "tool_fail_count",
            format!(
                "expected <= tool_total_calls {}, got {}",
                agg.tool_total_calls, agg.tool_fail_count
            ),
        ));
    }
    if agg.ask_user_fail_count > agg.ask_user_total_calls {
        return Err(introspection_decode_error(
            context,
            "ask_user_fail_count",
            format!(
                "expected <= ask_user_total_calls {}, got {}",
                agg.ask_user_total_calls, agg.ask_user_fail_count
            ),
        ));
    }

    Ok(agg)
}

fn tool_history_failure_from_row(row: &impl IntrospectionRow) -> ServiceResult<Value> {
    let context = "tool_history_failure_row";
    Ok(serde_json::json!({
        "session_id": introspection_required_non_empty_string(row, context, "session_id")?,
        "error_preview": introspection_row_string(row, context, "error_preview")?,
        "created_at": introspection_required_non_empty_string(row, context, "created_at")?,
    }))
}

fn ask_user_history_row_from_row(row: &impl IntrospectionRow) -> ServiceResult<AskUserHistoryRow> {
    let context = "ask_user_history_row";
    let metadata_json = introspection_row_string(row, context, "metadata_json")?;
    let metadata: Value = serde_json::from_str(&metadata_json)
        .map_err(|error| introspection_decode_error(context, "metadata_json", error))?;
    Ok(AskUserHistoryRow {
        session_id: introspection_required_non_empty_string(row, context, "session_id")?,
        event_type: introspection_required_non_empty_string(row, context, "event_type")?,
        created_at: introspection_required_non_empty_string(row, context, "created_at")?,
        metadata,
        content_preview: introspection_row_string(row, context, "content_preview")?,
    })
}

fn parse_relevance_scores_strict(context: &str, raw: &str) -> ServiceResult<HashMap<String, f64>> {
    let value: Value = serde_json::from_str(raw)
        .map_err(|error| introspection_decode_error(context, "relevance_scores", error))?;
    let object = value.as_object().ok_or_else(|| {
        introspection_decode_error(context, "relevance_scores", "expected JSON object")
    })?;

    let mut scores = HashMap::with_capacity(object.len());
    for (key, value) in object {
        let score = value.as_f64().ok_or_else(|| {
            introspection_decode_error(
                context,
                "relevance_scores",
                format!("score `{key}` must be numeric"),
            )
        })?;
        if !(0.0..=1.0).contains(&score) {
            return Err(introspection_decode_error(
                context,
                "relevance_scores",
                format!("score `{key}` must be in 0..=1, got {score}"),
            ));
        }
        scores.insert(key.clone(), score);
    }
    Ok(scores)
}

fn parse_json_object_strict(context: &str, column: &str, raw: &str) -> ServiceResult<Value> {
    let value: Value = serde_json::from_str(raw)
        .map_err(|error| introspection_decode_error(context, column, error))?;
    if !value.is_object() {
        return Err(introspection_decode_error(
            context,
            column,
            "expected JSON object",
        ));
    }
    Ok(value)
}

fn parse_token_budget_value(context: &str, raw: &str) -> ServiceResult<Value> {
    let value: Value = serde_json::from_str(raw)
        .map_err(|error| introspection_decode_error(context, "token_budget", error))?;
    if value.is_object() {
        return Ok(value);
    }
    if let Some(total) = value.as_i64()
        && total >= 0
    {
        return Ok(serde_json::json!({ "total": total }));
    }
    Err(introspection_decode_error(
        context,
        "token_budget",
        "expected JSON object or non-negative integer",
    ))
}

fn retrieval_quality_scores_from_row(
    row: &impl IntrospectionRow,
) -> ServiceResult<HashMap<String, f64>> {
    let context = "retrieval_quality_row";
    let raw = introspection_row_string(row, context, "relevance_scores")?;
    parse_relevance_scores_strict(context, &raw)
}

#[derive(Debug, Clone, PartialEq)]
struct ContextSnapshotCoreRow {
    snapshot_id: String,
    task_type: Option<String>,
    ctx_managed_tokens: Option<i64>,
    assembly_ms: Option<i64>,
    budget: Value,
    relevance_scores: HashMap<String, f64>,
    llm_response_id: Option<String>,
}

fn context_snapshot_core_from_row(
    row: &impl IntrospectionRow,
) -> ServiceResult<ContextSnapshotCoreRow> {
    let context = "context_snapshot_row";
    let budget_raw = introspection_row_string(row, context, "token_budget")?;
    let relevance_raw = introspection_row_string(row, context, "relevance_scores")?;
    Ok(ContextSnapshotCoreRow {
        snapshot_id: introspection_required_non_empty_string(row, context, "context_capture_id")?,
        task_type: introspection_row_optional_string(row, context, "task_type")?,
        ctx_managed_tokens: introspection_row_optional_non_negative_i64(
            row,
            context,
            "total_tokens",
        )?,
        assembly_ms: introspection_row_optional_non_negative_i64(row, context, "assembly_time_ms")?,
        budget: parse_token_budget_value(context, &budget_raw)?,
        relevance_scores: parse_relevance_scores_strict(context, &relevance_raw)?,
        llm_response_id: introspection_row_optional_string(row, context, "llm_response_id")?,
    })
}

#[derive(Debug, Clone, PartialEq)]
struct ContextSnapshotContentRow {
    selected_events: Option<String>,
    code_context: Option<String>,
    skill_definitions: Option<String>,
    documentation: Option<String>,
}

fn context_snapshot_content_from_row(
    row: &impl IntrospectionRow,
) -> ServiceResult<ContextSnapshotContentRow> {
    let context = "context_snapshot_content_row";
    Ok(ContextSnapshotContentRow {
        selected_events: introspection_row_optional_string(row, context, "selected_events")?,
        code_context: introspection_row_optional_string(row, context, "code_context")?,
        skill_definitions: introspection_row_optional_string(row, context, "skill_definitions")?,
        documentation: introspection_row_optional_string(row, context, "documentation")?,
    })
}

#[derive(Debug, Clone, PartialEq)]
struct TokenUsageEventRow {
    event_id: String,
    token_usage: Value,
}

fn token_usage_event_from_row(row: &impl IntrospectionRow) -> ServiceResult<TokenUsageEventRow> {
    let context = "token_usage_event_row";
    let token_usage_raw = introspection_row_string(row, context, "token_usage")?;
    Ok(TokenUsageEventRow {
        event_id: introspection_required_non_empty_string(row, context, "event_id")?,
        token_usage: parse_json_object_strict(context, "token_usage", &token_usage_raw)?,
    })
}

fn context_trend_response_event_id_from_row(row: &impl IntrospectionRow) -> ServiceResult<String> {
    introspection_required_non_empty_string(
        row,
        "context_trend_response_event_row",
        "response_event_id",
    )
}

fn optional_drift_preview_from_row(
    row: Option<&impl IntrospectionRow>,
    context: &str,
) -> ServiceResult<String> {
    let Some(row) = row else {
        return Ok(String::new());
    };
    introspection_row_string(row, context, "preview")
}

fn extract_ask_user_audit(metadata: &Value) -> Option<&Value> {
    metadata.get("ask_user").or_else(|| {
        metadata
            .get("prompt")
            .map(|_| metadata)
            .filter(|meta| meta.get("response").is_some())
    })
}

fn build_ask_user_history_summary(rows: &[AskUserHistoryRow]) -> Value {
    let has_first_class_events = rows
        .iter()
        .any(|row| row.event_type.starts_with("ask_user_"));
    let mut submitted_count = 0usize;
    let mut cancelled_count = 0usize;
    let mut timeout_count = 0usize;
    let mut error_count = 0usize;
    let mut prompt_count = 0usize;
    let mut question_count_sum = 0usize;
    let mut recent_interactions = Vec::new();

    for row in rows {
        let Some(audit) = extract_ask_user_audit(&row.metadata) else {
            continue;
        };
        let prompt = audit.get("prompt");
        let counts_as_prompt = if has_first_class_events {
            row.event_type == "ask_user_prompted"
        } else {
            prompt.is_some()
        };
        if counts_as_prompt {
            let Some(prompt) = prompt else {
                continue;
            };
            prompt_count += 1;
            question_count_sum += prompt
                .get("question_count")
                .and_then(Value::as_u64)
                .unwrap_or_default() as usize;
        }

        let counts_as_interaction = if has_first_class_events {
            matches!(
                row.event_type.as_str(),
                "ask_user_submitted" | "ask_user_cancelled" | "ask_user_timeout" | "ask_user_error"
            )
        } else {
            matches!(row.event_type.as_str(), "tool_call" | "tool_error")
        };
        if !counts_as_interaction {
            continue;
        }

        let Some(response) = audit.get("response") else {
            continue;
        };
        let Some(outcome) = response.get("outcome").and_then(Value::as_str) else {
            continue;
        };
        match outcome {
            "submitted" => submitted_count += 1,
            "cancelled" => cancelled_count += 1,
            "timeout" => timeout_count += 1,
            _ => error_count += 1,
        }
        let first_question = prompt
            .and_then(|prompt| prompt.get("questions"))
            .and_then(Value::as_array)
            .and_then(|questions| questions.first())
            .and_then(|question| question.get("question"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        recent_interactions.push(serde_json::json!({
            "session_id": row.session_id,
            "created_at": row.created_at,
            "event_type": row.event_type,
            "outcome": outcome,
            "question_count": prompt
                .and_then(|prompt| prompt.get("question_count"))
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            "first_question": first_question,
            "answered_question_count": response
                .get("answered_question_count")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            "annotation_count": response
                .get("annotation_count")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            "freeform_answer_count": response
                .get("freeform_answer_count")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            "content_preview": row.content_preview,
        }));
    }

    let interactions_observed = submitted_count + cancelled_count + timeout_count + error_count;
    let avg_question_count = if prompt_count > 0 {
        question_count_sum as f64 / prompt_count as f64
    } else {
        0.0
    };

    serde_json::json!({
        "interactions_observed": interactions_observed,
        "submitted_count": submitted_count,
        "cancelled_count": cancelled_count,
        "timeout_count": timeout_count,
        "error_count": error_count,
        "prompt_count": prompt_count,
        "avg_question_count": avg_question_count,
        "recent_interactions": recent_interactions,
    })
}

#[async_trait]
impl IntrospectionService for DatabaseIntrospectionService {
    async fn get_skills_introspection(
        &self,
        user_id: &str,
    ) -> ServiceResult<SkillsIntrospectionResponse> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let installed_rows = query(
            "SELECT i.skill_name, i.skill_version, COALESCE(r.description, '') AS description, COALESCE(r.category, '') AS category \
             FROM skill_installations i \
             LEFT JOIN skills_registry r \
                 ON r.skill_name = i.skill_name AND r.version = i.skill_version AND r.is_active = 1 \
             WHERE i.user_id = ? AND i.status = 'installed' LIMIT 50",
        )
        .bind(user_id)
        .fetch_all(&pool)
        .await
        .map_err(internal_error)?;

        let cloud_rows = query(
            "SELECT skill_name, version, COALESCE(description, '') AS description, COALESCE(category, '') AS category \
             FROM skills_registry WHERE is_active = 1 \
             ORDER BY skill_name, version DESC LIMIT 200",
        )
        .fetch_all(&pool)
        .await
        .map_err(internal_error)?;

        let mut installed = Vec::with_capacity(installed_rows.len());
        let mut installed_names = std::collections::HashSet::new();
        for r in &installed_rows {
            let skill = installed_skill_info_from_row(r)?;
            installed_names.insert(skill.name.clone());
            installed.push(skill);
        }

        let mut cloud = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for r in &cloud_rows {
            let skill = cloud_skill_info_from_row(r)?;
            if seen.contains(&skill.name) || installed_names.contains(&skill.name) {
                continue;
            }
            seen.insert(skill.name.clone());
            cloud.push(skill);
        }

        Ok(SkillsIntrospectionResponse { installed, cloud })
    }

    async fn get_context_trend(
        &self,
        user_id: &str,
        session_id: &str,
        turns: i32,
        context_window: i64,
    ) -> ServiceResult<Value> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        self.verify_session_owner(&pool, session_id, user_id)
            .await?;
        let turns = turns.clamp(1, MAX_INTROSPECTION_USAGE_ROWS);

        let response_id_rows = query(
            "SELECT cs.llm_response_id AS response_event_id \
             FROM ctx_snapshots cs \
             JOIN agent_events e \
               ON e.event_id = cs.event_id \
              AND e.session_id = cs.session_id \
              AND e.user_id = ? \
             WHERE cs.user_id = ? \
               AND cs.session_id = ? \
               AND cs.llm_response_id IS NOT NULL \
             ORDER BY cs.created_at DESC, cs.context_capture_id DESC LIMIT ?",
        )
        .bind(user_id)
        .bind(user_id)
        .bind(session_id)
        .bind(turns)
        .fetch_all(&pool)
        .await
        .map_err(internal_error)?;

        let response_event_ids: Vec<String> = response_id_rows
            .iter()
            .map(context_trend_response_event_id_from_row)
            .collect::<ServiceResult<_>>()?;

        if response_event_ids.is_empty() {
            return Ok(serde_json::json!({"turns_sampled": 0, "trend": "no_data"}));
        }

        let mut usage_query = QueryBuilder::<MySql>::new(
            "SELECT event_id, IFNULL(CAST(token_usage AS CHAR), '{}') AS token_usage \
             FROM agent_events WHERE session_id = ",
        );
        usage_query.push_bind(session_id);
        usage_query.push(" AND user_id = ");
        usage_query.push_bind(user_id);
        usage_query.push(" AND token_usage IS NOT NULL AND event_id IN (");
        let mut separated = usage_query.separated(", ");
        for event_id in &response_event_ids {
            separated.push_bind(event_id);
        }
        separated.push_unseparated(")");

        let usage_rows = usage_query
            .build()
            .fetch_all(&pool)
            .await
            .map_err(internal_error)?;
        let usage_by_event_id = usage_rows
            .iter()
            .map(token_usage_event_from_row)
            .map(|result| result.map(|row| (row.event_id, row.token_usage)))
            .collect::<ServiceResult<HashMap<_, _>>>()?;

        let usages: Vec<Value> = response_event_ids
            .iter()
            .filter_map(|response_event_id| usage_by_event_id.get(response_event_id).cloned())
            .collect();

        if usages.is_empty() {
            return Ok(serde_json::json!({"turns_sampled": 0, "trend": "no_data"}));
        }

        let prompt_history: Vec<i64> = usages
            .iter()
            .filter_map(billable_input_from_canonical)
            .collect();

        let trend = compute_trend(&prompt_history);
        let current = usages
            .first()
            .cloned()
            .unwrap_or(Value::Object(Default::default()));

        let per_turn: Vec<Value> = usages
            .iter()
            .map(|u| {
                serde_json::json!({
                    "input_tokens": u.get("input_tokens"),
                    "cached_input_tokens": u.get("cached_input_tokens"),
                    "cache_creation_tokens": u.get("cache_creation_tokens"),
                    "output_tokens": u.get("output_tokens"),
                    "total_tokens": u.get("total_tokens"),
                })
            })
            .collect();

        let cw = context_window.max(1);
        let current_prompt = billable_input_from_canonical(&current).unwrap_or(0);

        Ok(serde_json::json!({
            "turns_sampled": usages.len(),
            "trend": trend,
            "current_tokens": {
                "input_tokens": current.get("input_tokens"),
                "cached_input_tokens": current.get("cached_input_tokens"),
                "cache_creation_tokens": current.get("cache_creation_tokens"),
                "output_tokens": current.get("output_tokens"),
                "total_tokens": current.get("total_tokens"),
            },
            "context_window_limit": context_window,
            "utilization": ((current_prompt as f64 / cw as f64) * 1000.0).round() / 1000.0,
            "forecast": serde_json::to_value(compaction_forecast(&prompt_history, context_window)).unwrap_or_default(),
            "compaction_history": serde_json::to_value(compaction_effectiveness(&prompt_history)).unwrap_or_default(),
            "per_turn": per_turn,
        }))
    }

    async fn get_context_snapshot(
        &self,
        user_id: &str,
        session_id: &str,
        turn_index: Option<i32>,
        detail: bool,
        raw: bool,
        raw_token_budget: i32,
    ) -> ServiceResult<Value> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        self.verify_session_owner(&pool, session_id, user_id)
            .await?;

        let total_turns_row = query(
            "SELECT COUNT(*) AS cnt \
             FROM ctx_snapshots cs \
             JOIN agent_events e \
               ON e.event_id = cs.event_id \
              AND e.session_id = cs.session_id \
              AND e.user_id = ? \
             WHERE cs.user_id = ? \
               AND cs.session_id = ?",
        )
        .bind(user_id)
        .bind(user_id)
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .map_err(internal_error)?;
        let total_turns = introspection_row_non_negative_i64(
            &total_turns_row,
            "context_snapshot_count_row",
            "cnt",
        )?;

        if total_turns == 0 {
            return Err(error_response(
                StatusCode::NOT_FOUND,
                "No snapshots for this session",
            ));
        }

        let actual_turn = if let Some(ti) = turn_index {
            if ti as i64 > total_turns {
                return Err(error_response(
                    StatusCode::NOT_FOUND,
                    format!("Turn {} not found (session has {} turns)", ti, total_turns),
                ));
            }
            ti as i64
        } else {
            total_turns
        };

        let content_cols = if detail || raw {
            ", cs.selected_events, cs.code_context, cs.skill_definitions, cs.documentation"
        } else {
            ""
        };

        let sql = format!(
            "SELECT cs.context_capture_id, \
                    IFNULL(CAST(cs.token_budget AS CHAR), '{{}}') AS token_budget, \
                    cs.total_tokens, cs.assembly_time_ms, \
                    IFNULL(CAST(cs.relevance_scores AS CHAR), '{{}}') AS relevance_scores, \
                    cs.task_type, cs.llm_response_id{content_cols} \
             FROM ctx_snapshots cs \
             JOIN agent_events e \
               ON e.event_id = cs.event_id \
              AND e.session_id = cs.session_id \
              AND e.user_id = ? \
             WHERE cs.user_id = ? \
             AND cs.session_id = ? \
             ORDER BY cs.created_at ASC, cs.context_capture_id ASC \
             LIMIT 1 OFFSET ?"
        );

        let row = query(&sql)
            .bind(user_id)
            .bind(user_id)
            .bind(session_id)
            .bind(actual_turn - 1)
            .fetch_one(&pool)
            .await
            .map_err(internal_error)?;

        let snapshot = context_snapshot_core_from_row(&row)?;
        let task_type = snapshot.task_type.clone();

        let mut result = serde_json::json!({
            "snapshot_id": snapshot.snapshot_id,
            "turn": actual_turn,
            "total_turns": total_turns,
            "task_type": task_type,
            "context_managed_tokens": snapshot.ctx_managed_tokens,
            "assembly_ms": snapshot.assembly_ms,
        });

        // Layer 1: health, zone_balance, token_breakdown
        {
            let trend_limit = std::cmp::min(actual_turn as i32, MAX_INTROSPECTION_USAGE_ROWS);
            let current_usage = if let Some(ref resp_id) = snapshot.llm_response_id {
                let current_row = query(
                    "SELECT event_id, IFNULL(CAST(token_usage AS CHAR), '{}') AS token_usage \
                     FROM agent_events \
                     WHERE session_id = ? AND user_id = ? AND event_id = ? \
                       AND event_type = 'llm_response' AND token_usage IS NOT NULL \
                     LIMIT 1",
                )
                .bind(session_id)
                .bind(user_id)
                .bind(resp_id)
                .fetch_optional(&pool)
                .await
                .map_err(internal_error)?;
                current_row
                    .as_ref()
                    .map(token_usage_event_from_row)
                    .transpose()?
                    .map(|row| row.token_usage)
            } else {
                None
            };

            let trend_rows = if let Some(ref resp_id) = snapshot.llm_response_id {
                query(
                    "SELECT e.event_id, IFNULL(CAST(e.token_usage AS CHAR), '{}') AS token_usage \
                     FROM agent_events e \
                     WHERE e.session_id = ? AND e.user_id = ? \
                       AND e.event_type = 'llm_response' AND e.token_usage IS NOT NULL \
                       AND e.created_at <= ( \
                           SELECT anchor.created_at FROM agent_events anchor \
                           WHERE anchor.session_id = ? AND anchor.user_id = ? AND anchor.event_id = ? \
                           LIMIT 1 \
                       ) \
                     ORDER BY e.created_at DESC, e.event_id DESC LIMIT ?",
                )
                .bind(session_id)
                .bind(user_id)
                .bind(session_id)
                .bind(user_id)
                .bind(resp_id)
                .bind(trend_limit)
                .fetch_all(&pool)
                .await
                .map_err(internal_error)?
            } else {
                query(
                    "SELECT event_id, IFNULL(CAST(token_usage AS CHAR), '{}') AS token_usage \
                     FROM agent_events \
                     WHERE session_id = ? AND user_id = ? \
                       AND event_type = 'llm_response' AND token_usage IS NOT NULL \
                     ORDER BY created_at DESC, event_id DESC LIMIT ?",
                )
                .bind(session_id)
                .bind(user_id)
                .bind(trend_limit)
                .fetch_all(&pool)
                .await
                .map_err(internal_error)?
            };
            let trend_usage_rows: Vec<TokenUsageEventRow> = trend_rows
                .iter()
                .map(token_usage_event_from_row)
                .collect::<ServiceResult<_>>()?;

            let mut current_usage = current_usage;
            if current_usage.is_none()
                && let Some(first) = trend_usage_rows.first()
            {
                current_usage = Some(first.token_usage.clone());
            }

            let trend_prompts: Vec<i64> = trend_usage_rows
                .iter()
                .filter_map(|usage_row| billable_input_from_canonical(&usage_row.token_usage))
                .collect();

            let current_prompt = current_usage
                .as_ref()
                .and_then(billable_input_from_canonical);

            if let Some(ref cu) = current_usage {
                result["llm_prompt_tokens"] = serde_json::json!(billable_input_from_canonical(cu));
                result["llm_completion_tokens"] =
                    serde_json::json!(cu.get("output_tokens").and_then(|v| v.as_i64()));
                result["llm_total_tokens"] =
                    serde_json::json!(cu.get("total_tokens").and_then(|v| v.as_i64()));
            }

            let health = analyze_context_health(
                &snapshot.budget,
                &trend_prompts,
                current_prompt,
                current_usage.as_ref(),
                128000,
            );
            result["health"] = serde_json::to_value(&health).unwrap_or_default();
            result["zone_balance"] =
                serde_json::to_value(zone_balance(&snapshot.budget, task_type.as_deref()))
                    .unwrap_or_default();

            // Token breakdown
            let tool_tokens = snapshot
                .budget
                .get("tool_schemas")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let non_tool_tokens: i64 = snapshot
                .budget
                .as_object()
                .map(|o| {
                    o.iter()
                        .filter(|(k, _)| k.as_str() != "tool_schemas")
                        .filter_map(|(_, v)| v.as_i64())
                        .sum()
                })
                .unwrap_or(0);
            let total_managed = tool_tokens + non_tool_tokens;
            let tool_ratio = if total_managed > 0 {
                ((tool_tokens as f64 / total_managed as f64) * 100.0).round() / 100.0
            } else {
                0.0
            };
            result["token_breakdown"] = serde_json::json!({
                "tool_tokens": tool_tokens,
                "non_tool_tokens": non_tool_tokens,
                "total_managed": total_managed,
                "tool_ratio": tool_ratio,
                "recommendation": if total_managed > 0 && tool_tokens as f64 / total_managed as f64 > 0.7 {
                    "tool_schemas dominating context — consider high-confidence or catalog selection"
                } else {
                    "balanced"
                },
            });
        }

        // Relevance + pollution
        if !snapshot.relevance_scores.is_empty() {
            result["relevance"] =
                serde_json::to_value(relevance_quality(&snapshot.relevance_scores))
                    .unwrap_or_default();
            result["pollution"] = serde_json::to_value(pollution_ratio(&snapshot.relevance_scores))
                .unwrap_or_default();
        }

        // Layer 2 & 3
        if detail || raw {
            let contents = context_snapshot_content_from_row(&row)?;

            result["contents"] = summarize_contents(
                contents.selected_events.as_deref(),
                contents.code_context.as_deref(),
                contents.skill_definitions.as_deref(),
                contents.documentation.as_deref(),
            );

            if raw {
                result["raw"] = raw_contents(
                    contents.selected_events.as_deref(),
                    contents.code_context.as_deref(),
                    contents.skill_definitions.as_deref(),
                    contents.documentation.as_deref(),
                    raw_token_budget,
                );
            }
        }

        Ok(result)
    }

    async fn get_retrieval_quality(
        &self,
        user_id: &str,
        session_id: &str,
        turns: i32,
    ) -> ServiceResult<Value> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        self.verify_session_owner(&pool, session_id, user_id)
            .await?;
        let turns = turns.clamp(1, MAX_INTROSPECTION_USAGE_ROWS);

        let rows = query(
            "SELECT IFNULL(CAST(cs.relevance_scores AS CHAR), '{}') AS relevance_scores \
             FROM ctx_snapshots cs \
             JOIN agent_events e \
               ON e.event_id = cs.event_id \
              AND e.session_id = cs.session_id \
              AND e.user_id = ? \
             WHERE cs.user_id = ? AND cs.session_id = ? ORDER BY cs.created_at DESC LIMIT ?",
        )
        .bind(user_id)
        .bind(user_id)
        .bind(session_id)
        .bind(turns)
        .fetch_all(&pool)
        .await
        .map_err(internal_error)?;

        if rows.is_empty() {
            return Ok(serde_json::json!({"turns_sampled": 0, "overall_quality": "no_data"}));
        }

        let mut means = Vec::new();
        for r in rows.iter().rev() {
            let scores = retrieval_quality_scores_from_row(r)?;
            let q = relevance_quality(&scores);
            if let Some(m) = q.mean {
                means.push(m);
            }
        }

        if means.is_empty() {
            return Ok(
                serde_json::json!({"turns_sampled": rows.len(), "overall_quality": "no_data"}),
            );
        }

        let overall_mean =
            (means.iter().sum::<f64>() / means.len() as f64 * 1000.0).round() / 1000.0;
        let degrading = means.len() >= 2 && (means[0] - means[means.len() - 1]) > DEGRADATION_DELTA;
        let overall_quality = if degrading {
            "degrading"
        } else if overall_mean >= QUALITY_GOOD {
            "good"
        } else if overall_mean >= QUALITY_DEGRADED {
            "degraded"
        } else {
            "poor"
        };

        let recommendation = if overall_quality == "degrading" || overall_quality == "poor" {
            "consider context reset or re-retrieval"
        } else {
            "retrieval healthy"
        };

        Ok(serde_json::json!({
            "turns_sampled": rows.len(),
            "overall_quality": overall_quality,
            "mean_relevance": overall_mean,
            "recommendation": recommendation,
        }))
    }

    async fn get_decision_trace(
        &self,
        user_id: &str,
        session_id: &str,
        last_n: i32,
    ) -> ServiceResult<Value> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        self.verify_session_owner(&pool, session_id, user_id)
            .await?;
        let last_n = last_n.clamp(1, 200);

        let rows = query(
            "SELECT d.decision_id, d.event_id, d.decision_type, d.model_used, \
                    IFNULL(CAST(d.decision_output AS CHAR), '{}') AS output_json, \
                    DATE_FORMAT(d.created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
             FROM ctx_decision_audits d \
             JOIN agent_events e ON e.event_id = d.event_id AND e.session_id = d.session_id AND e.user_id = ? \
             WHERE d.user_id = ? AND d.session_id = ? \
             ORDER BY d.created_at DESC LIMIT ?",
        )
        .bind(user_id)
        .bind(user_id)
        .bind(session_id)
        .bind(i64::from(last_n))
        .fetch_all(&pool)
        .await
        .map_err(internal_error)?;

        let decisions: Vec<Value> = rows
            .iter()
            .map(decision_trace_row_from_row)
            .collect::<ServiceResult<_>>()?;

        Ok(serde_json::json!({
            "schema_version": 1,
            "session_id": session_id,
            "user_id": user_id,
            "last_n": last_n,
            "decisions": decisions,
        }))
    }

    async fn get_tool_history(
        &self,
        user_id: &str,
        tool: &str,
        window_hours: i32,
    ) -> ServiceResult<Value> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let window_hours = window_hours.clamp(1, 24 * 30); // up to 30 days
        let is_ask_user = tool == "ask_user";

        let agg = query(
            "SELECT \
               COALESCE(SUM(CASE WHEN event_type IN ('tool_call', 'tool_error') THEN 1 ELSE 0 END), 0) AS tool_total_calls, \
               COALESCE(SUM(CASE WHEN event_type = 'tool_error' THEN 1 ELSE 0 END), 0) AS tool_fail_count, \
               COALESCE(SUM(CASE WHEN event_type IN ('ask_user_submitted', 'ask_user_cancelled', 'ask_user_timeout', 'ask_user_error') THEN 1 ELSE 0 END), 0) AS ask_user_total_calls, \
               COALESCE(SUM(CASE WHEN event_type IN ('ask_user_cancelled', 'ask_user_timeout', 'ask_user_error') THEN 1 ELSE 0 END), 0) AS ask_user_fail_count \
              FROM agent_events \
              WHERE user_id = ? \
                AND (skill_name = ? OR meta_tool_name = ?) \
                AND created_at >= DATE_SUB(CURRENT_TIMESTAMP(6), INTERVAL ? HOUR)",
        )
        .bind(user_id)
        .bind(tool)
        .bind(tool)
        .bind(i64::from(window_hours))
        .fetch_one(&pool)
        .await
        .map_err(internal_error)?;

        let agg = tool_history_agg_from_row(&agg)?;
        let (total_calls, fail_count) = if is_ask_user && agg.ask_user_total_calls > 0 {
            (agg.ask_user_total_calls, agg.ask_user_fail_count)
        } else {
            (agg.tool_total_calls, agg.tool_fail_count)
        };
        let ok_count = total_calls - fail_count;
        let success_rate = if total_calls > 0 {
            (ok_count as f64) / (total_calls as f64)
        } else {
            0.0
        };

        let failures = if is_ask_user && agg.ask_user_total_calls > 0 {
            query(
                "SELECT session_id, \
                        SUBSTRING(COALESCE(CAST(content AS CHAR), ''), 1, 200) AS error_preview, \
                        DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
                 FROM agent_events \
                 WHERE user_id = ? \
                   AND (skill_name = ? OR meta_tool_name = ?) \
                   AND event_type IN ('ask_user_cancelled', 'ask_user_timeout', 'ask_user_error') \
                   AND created_at >= DATE_SUB(CURRENT_TIMESTAMP(6), INTERVAL ? HOUR) \
                 ORDER BY created_at DESC LIMIT 10",
            )
            .bind(user_id)
            .bind(tool)
            .bind(tool)
            .bind(i64::from(window_hours))
            .fetch_all(&pool)
            .await
            .map_err(internal_error)?
        } else {
            query(
                "SELECT session_id, \
                        SUBSTRING(COALESCE(CAST(content AS CHAR), ''), 1, 200) AS error_preview, \
                        DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
                 FROM agent_events \
                 WHERE user_id = ? \
                   AND (skill_name = ? OR meta_tool_name = ?) \
                   AND event_type = 'tool_error' \
                   AND created_at >= DATE_SUB(CURRENT_TIMESTAMP(6), INTERVAL ? HOUR) \
                 ORDER BY created_at DESC LIMIT 10",
            )
            .bind(user_id)
            .bind(tool)
            .bind(tool)
            .bind(i64::from(window_hours))
            .fetch_all(&pool)
            .await
            .map_err(internal_error)?
        };

        let recent_failures: Vec<Value> = failures
            .iter()
            .map(tool_history_failure_from_row)
            .collect::<ServiceResult<_>>()?;

        let ask_user_summary = if is_ask_user {
            let rows = query(
                "SELECT session_id, event_type, \
                        SUBSTRING(COALESCE(CAST(content AS CHAR), ''), 1, 200) AS content_preview, \
                        IFNULL(CAST(metadata AS CHAR), '{}') AS metadata_json, \
                        DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
                 FROM agent_events \
                 WHERE user_id = ? \
                   AND (skill_name = ? OR meta_tool_name = ?) \
                   AND event_type IN ('ask_user_prompted', 'ask_user_submitted', 'ask_user_cancelled', 'ask_user_timeout', 'ask_user_error') \
                   AND created_at >= DATE_SUB(CURRENT_TIMESTAMP(6), INTERVAL ? HOUR) \
                 ORDER BY created_at DESC LIMIT ?",
            )
            .bind(user_id)
            .bind(tool)
            .bind(tool)
            .bind(i64::from(window_hours))
            .bind(i64::from(ASK_USER_HISTORY_EVENT_LIMIT))
            .fetch_all(&pool)
            .await
            .map_err(internal_error)?;
            let rows = if rows.is_empty() {
                query(
                    "SELECT session_id, event_type, \
                            SUBSTRING(COALESCE(CAST(content AS CHAR), ''), 1, 200) AS content_preview, \
                            IFNULL(CAST(metadata AS CHAR), '{}') AS metadata_json, \
                            DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
                     FROM agent_events \
                     WHERE user_id = ? \
                       AND (skill_name = ? OR meta_tool_name = ?) \
                       AND event_type IN ('tool_call', 'tool_error') \
                       AND created_at >= DATE_SUB(CURRENT_TIMESTAMP(6), INTERVAL ? HOUR) \
                     ORDER BY created_at DESC LIMIT ?",
                )
                .bind(user_id)
                .bind(tool)
                .bind(tool)
                .bind(i64::from(window_hours))
                .bind(i64::from(ASK_USER_HISTORY_EVENT_LIMIT))
                .fetch_all(&pool)
                .await
                .map_err(internal_error)?
            } else {
                rows
            };
            let history_rows = rows
                .iter()
                .map(ask_user_history_row_from_row)
                .collect::<ServiceResult<Vec<_>>>()?;
            Some(build_ask_user_history_summary(&history_rows))
        } else {
            None
        };

        let mut response = serde_json::json!({
            "schema_version": 1,
            "user_id": user_id,
            "tool": tool,
            "window_hours": window_hours,
            "total_calls": total_calls,
            "ok_count": ok_count,
            "fail_count": fail_count,
            "success_rate": success_rate,
            "recent_failures": recent_failures,
        });
        if let Some(ask_user) = ask_user_summary
            && let Some(obj) = response.as_object_mut()
        {
            obj.insert("ask_user".into(), ask_user);
        }
        Ok(response)
    }

    async fn get_drift_check(&self, user_id: &str, session_id: &str) -> ServiceResult<Value> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        self.verify_session_owner(&pool, session_id, user_id)
            .await?;

        // Original intent: earliest user-facing event.
        let first = query(
            "SELECT SUBSTRING(COALESCE(CAST(content AS CHAR), ''), 1, 240) AS preview \
             FROM agent_events \
             WHERE session_id = ? AND user_id = ? AND (event_type = 'user_query' OR event_type = 'user_message') \
             ORDER BY created_at ASC LIMIT 1",
        )
        .bind(session_id)
        .bind(user_id)
        .fetch_optional(&pool)
        .await
        .map_err(internal_error)?;

        let original_intent =
            optional_drift_preview_from_row(first.as_ref(), "drift_original_intent_row")?;

        // Current focus: last non-error event.
        let last = query(
            "SELECT SUBSTRING(COALESCE(CAST(content AS CHAR), ''), 1, 240) AS preview \
             FROM agent_events \
             WHERE session_id = ? AND user_id = ? \
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(session_id)
        .bind(user_id)
        .fetch_optional(&pool)
        .await
        .map_err(internal_error)?;

        let current_focus =
            optional_drift_preview_from_row(last.as_ref(), "drift_current_focus_row")?;

        let (drift_score, drift_level, signals) = compute_drift(&original_intent, &current_focus);

        Ok(serde_json::json!({
            "schema_version": 1,
            "user_id": user_id,
            "session_id": session_id,
            "original_intent_preview": original_intent,
            "current_focus_preview": current_focus,
            "drift_score": drift_score,
            "drift_level": drift_level,
            "signals": signals,
        }))
    }
}

fn summarize_contents(
    selected_events: Option<&str>,
    code_context: Option<&str>,
    skill_definitions: Option<&str>,
    documentation: Option<&str>,
) -> Value {
    let mut summary = serde_json::Map::new();

    if let Some(raw) = selected_events
        && let Ok(events) = serde_json::from_str::<Vec<Value>>(raw)
    {
        let mut by_type = std::collections::HashMap::<String, usize>::new();
        for e in &events {
            let et = e
                .get("event_type")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            *by_type.entry(et.into()).or_default() += 1;
        }
        summary.insert(
            "events".into(),
            serde_json::json!({"total": events.len(), "by_type": by_type}),
        );
    }

    if let Some(raw) = code_context
        && let Ok(code) = serde_json::from_str::<Vec<Value>>(raw)
    {
        let paths: Vec<&str> = code
            .iter()
            .filter_map(|c| {
                c.get("file")
                    .or_else(|| c.get("path"))
                    .and_then(|v| v.as_str())
            })
            .take(10)
            .collect();
        summary.insert(
            "code".into(),
            serde_json::json!({"files": code.len(), "paths": paths}),
        );
    }

    if let Some(raw) = skill_definitions
        && let Ok(skills) = serde_json::from_str::<Vec<Value>>(raw)
    {
        let names: Vec<&str> = skills
            .iter()
            .filter_map(|s| s.get("skill_name").and_then(|v| v.as_str()))
            .collect();
        summary.insert("skills".into(), serde_json::json!(names));
    }

    if let Some(raw) = documentation
        && let Ok(docs) = serde_json::from_str::<Vec<Value>>(raw)
    {
        let titles: Vec<&str> = docs
            .iter()
            .filter_map(|d| {
                d.get("source")
                    .or_else(|| d.get("title"))
                    .and_then(|v| v.as_str())
            })
            .take(10)
            .collect();
        summary.insert("docs".into(), serde_json::json!(titles));
    }

    Value::Object(summary)
}

fn raw_contents(
    selected_events: Option<&str>,
    code_context: Option<&str>,
    skill_definitions: Option<&str>,
    documentation: Option<&str>,
    token_budget: i32,
) -> Value {
    let char_budget = token_budget as usize * TOKEN_CHAR_RATIO;
    let mut used = 0usize;
    let mut raw = serde_json::Map::new();

    let blobs: [(&str, Option<&str>); 4] = [
        ("events", selected_events),
        ("code", code_context),
        ("skills", skill_definitions),
        ("docs", documentation),
    ];

    for (key, blob) in blobs {
        let blob = match blob {
            Some(b) if !b.is_empty() && used < char_budget => b,
            _ => continue,
        };
        let data: Vec<Value> = match serde_json::from_str(blob) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let serialized = serde_json::to_string(&data).unwrap_or_default();
        if serialized.len() <= char_budget - used {
            raw.insert(key.into(), Value::Array(data));
            used += serialized.len();
        } else {
            let mut items = Vec::new();
            for item in &data {
                let item_str = serde_json::to_string(item).unwrap_or_default();
                if used + item_str.len() > char_budget {
                    break;
                }
                items.push(item.clone());
                used += item_str.len();
            }
            if !items.is_empty() {
                raw.insert(key.into(), Value::Array(items));
                raw.insert(format!("{key}_truncated"), Value::Bool(true));
            }
        }
    }

    Value::Object(raw)
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeIntrospectionSkillRow {
        failed_column: Option<&'static str>,
        empty_column: Option<&'static str>,
        description: &'static str,
        category: &'static str,
    }

    impl FakeIntrospectionSkillRow {
        fn complete() -> Self {
            Self {
                failed_column: None,
                empty_column: None,
                description: "Review Rust changes",
                category: "engineering",
            }
        }

        fn without_registry_metadata() -> Self {
            Self {
                description: "",
                category: "",
                ..Self::complete()
            }
        }

        fn fail_on(column: &'static str) -> Self {
            Self {
                failed_column: Some(column),
                ..Self::complete()
            }
        }

        fn empty_on(column: &'static str) -> Self {
            Self {
                empty_column: Some(column),
                ..Self::complete()
            }
        }

        fn maybe_fail(&self, column: &str) -> Result<(), sqlx::Error> {
            if self.failed_column == Some(column) {
                Err(sqlx::Error::ColumnNotFound(column.to_string()))
            } else {
                Ok(())
            }
        }

        fn text(&self, column: &str, value: &'static str) -> String {
            if self.empty_column == Some(column) {
                String::new()
            } else {
                value.to_string()
            }
        }
    }

    impl IntrospectionRow for FakeIntrospectionSkillRow {
        fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
            self.maybe_fail(column)?;
            Ok(match column {
                "skill_name" => self.text(column, "code-review"),
                "skill_version" => self.text(column, "1.2.3"),
                "version" => self.text(column, "2.0.0"),
                "description" => self.text(column, self.description),
                "category" => self.text(column, self.category),
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            })
        }

        fn optional_string_column(&self, column: &str) -> Result<Option<String>, sqlx::Error> {
            self.maybe_fail(column)?;
            Err(sqlx::Error::ColumnNotFound(column.to_string()))
        }

        fn i64_column(&self, column: &str) -> Result<i64, sqlx::Error> {
            self.maybe_fail(column)?;
            Err(sqlx::Error::ColumnNotFound(column.to_string()))
        }

        fn optional_i64_column(&self, column: &str) -> Result<Option<i64>, sqlx::Error> {
            self.maybe_fail(column)?;
            Err(sqlx::Error::ColumnNotFound(column.to_string()))
        }
    }

    struct FakeDecisionTraceRow {
        failed_column: Option<&'static str>,
        empty_column: Option<&'static str>,
        output_json: &'static str,
        model_used: Option<&'static str>,
    }

    impl FakeDecisionTraceRow {
        fn complete() -> Self {
            Self {
                failed_column: None,
                empty_column: None,
                output_json: r#"{"visible_tools":["bash"],"reason":"inspect"}"#,
                model_used: Some("glm-5.2"),
            }
        }

        fn without_model() -> Self {
            Self {
                model_used: None,
                ..Self::complete()
            }
        }

        fn fail_on(column: &'static str) -> Self {
            Self {
                failed_column: Some(column),
                ..Self::complete()
            }
        }

        fn empty_on(column: &'static str) -> Self {
            Self {
                empty_column: Some(column),
                ..Self::complete()
            }
        }

        fn with_output_json(output_json: &'static str) -> Self {
            Self {
                output_json,
                ..Self::complete()
            }
        }

        fn maybe_fail(&self, column: &str) -> Result<(), sqlx::Error> {
            if self.failed_column == Some(column) {
                Err(sqlx::Error::ColumnNotFound(column.to_string()))
            } else {
                Ok(())
            }
        }

        fn text(&self, column: &str, value: &'static str) -> String {
            if self.empty_column == Some(column) {
                String::new()
            } else {
                value.to_string()
            }
        }
    }

    impl IntrospectionRow for FakeDecisionTraceRow {
        fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
            self.maybe_fail(column)?;
            Ok(match column {
                "decision_id" => self.text(column, "decision-1"),
                "event_id" => self.text(column, "event-1"),
                "decision_type" => self.text(column, "tool_surface"),
                "output_json" => self.text(column, self.output_json),
                "created_at" => self.text(column, "2026-06-26T12:00:00"),
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            })
        }

        fn optional_string_column(&self, column: &str) -> Result<Option<String>, sqlx::Error> {
            self.maybe_fail(column)?;
            Ok(match column {
                "model_used" => self.model_used.map(str::to_string),
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            })
        }

        fn i64_column(&self, column: &str) -> Result<i64, sqlx::Error> {
            self.maybe_fail(column)?;
            Err(sqlx::Error::ColumnNotFound(column.to_string()))
        }

        fn optional_i64_column(&self, column: &str) -> Result<Option<i64>, sqlx::Error> {
            self.maybe_fail(column)?;
            Err(sqlx::Error::ColumnNotFound(column.to_string()))
        }
    }

    struct FakeToolHistoryRow {
        failed_column: Option<&'static str>,
        empty_column: Option<&'static str>,
        tool_total_calls: i64,
        tool_fail_count: i64,
        ask_user_total_calls: i64,
        ask_user_fail_count: i64,
        metadata_json: &'static str,
        error_preview: &'static str,
        content_preview: &'static str,
    }

    impl FakeToolHistoryRow {
        fn complete() -> Self {
            Self {
                failed_column: None,
                empty_column: None,
                tool_total_calls: 5,
                tool_fail_count: 2,
                ask_user_total_calls: 3,
                ask_user_fail_count: 1,
                metadata_json: r#"{"ask_user":{"prompt":{"question_count":1},"response":{"outcome":"submitted"}}}"#,
                error_preview: "permission denied",
                content_preview: "ask_user submitted",
            }
        }

        fn fail_on(column: &'static str) -> Self {
            Self {
                failed_column: Some(column),
                ..Self::complete()
            }
        }

        fn empty_on(column: &'static str) -> Self {
            Self {
                empty_column: Some(column),
                ..Self::complete()
            }
        }

        fn with_counts(
            tool_total_calls: i64,
            tool_fail_count: i64,
            ask_user_total_calls: i64,
            ask_user_fail_count: i64,
        ) -> Self {
            Self {
                tool_total_calls,
                tool_fail_count,
                ask_user_total_calls,
                ask_user_fail_count,
                ..Self::complete()
            }
        }

        fn with_metadata_json(metadata_json: &'static str) -> Self {
            Self {
                metadata_json,
                ..Self::complete()
            }
        }

        fn maybe_fail(&self, column: &str) -> Result<(), sqlx::Error> {
            if self.failed_column == Some(column) {
                Err(sqlx::Error::ColumnNotFound(column.to_string()))
            } else {
                Ok(())
            }
        }

        fn text(&self, column: &str, value: &'static str) -> String {
            if self.empty_column == Some(column) {
                String::new()
            } else {
                value.to_string()
            }
        }
    }

    impl IntrospectionRow for FakeToolHistoryRow {
        fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
            self.maybe_fail(column)?;
            Ok(match column {
                "session_id" => self.text(column, "session-1"),
                "error_preview" => self.text(column, self.error_preview),
                "created_at" => self.text(column, "2026-06-26T12:00:00"),
                "event_type" => self.text(column, "ask_user_submitted"),
                "metadata_json" => self.text(column, self.metadata_json),
                "content_preview" => self.text(column, self.content_preview),
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            })
        }

        fn optional_string_column(&self, column: &str) -> Result<Option<String>, sqlx::Error> {
            self.maybe_fail(column)?;
            Err(sqlx::Error::ColumnNotFound(column.to_string()))
        }

        fn i64_column(&self, column: &str) -> Result<i64, sqlx::Error> {
            self.maybe_fail(column)?;
            match column {
                "tool_total_calls" => Ok(self.tool_total_calls),
                "tool_fail_count" => Ok(self.tool_fail_count),
                "ask_user_total_calls" => Ok(self.ask_user_total_calls),
                "ask_user_fail_count" => Ok(self.ask_user_fail_count),
                _ => Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
        }

        fn optional_i64_column(&self, column: &str) -> Result<Option<i64>, sqlx::Error> {
            self.maybe_fail(column)?;
            Err(sqlx::Error::ColumnNotFound(column.to_string()))
        }
    }

    struct FakeRetrievalQualityRow {
        failed_column: Option<&'static str>,
        relevance_scores: &'static str,
    }

    impl FakeRetrievalQualityRow {
        fn with_scores(relevance_scores: &'static str) -> Self {
            Self {
                failed_column: None,
                relevance_scores,
            }
        }

        fn fail_on(column: &'static str) -> Self {
            Self {
                failed_column: Some(column),
                relevance_scores: r#"{"memory":0.8}"#,
            }
        }

        fn maybe_fail(&self, column: &str) -> Result<(), sqlx::Error> {
            if self.failed_column == Some(column) {
                Err(sqlx::Error::ColumnNotFound(column.to_string()))
            } else {
                Ok(())
            }
        }
    }

    impl IntrospectionRow for FakeRetrievalQualityRow {
        fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
            self.maybe_fail(column)?;
            match column {
                "relevance_scores" => Ok(self.relevance_scores.to_string()),
                _ => Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
        }

        fn optional_string_column(&self, column: &str) -> Result<Option<String>, sqlx::Error> {
            self.maybe_fail(column)?;
            Err(sqlx::Error::ColumnNotFound(column.to_string()))
        }

        fn i64_column(&self, column: &str) -> Result<i64, sqlx::Error> {
            self.maybe_fail(column)?;
            Err(sqlx::Error::ColumnNotFound(column.to_string()))
        }

        fn optional_i64_column(&self, column: &str) -> Result<Option<i64>, sqlx::Error> {
            self.maybe_fail(column)?;
            Err(sqlx::Error::ColumnNotFound(column.to_string()))
        }
    }

    struct FakeContextSnapshotCountRow {
        failed_column: Option<&'static str>,
        cnt: i64,
    }

    impl FakeContextSnapshotCountRow {
        fn with_count(cnt: i64) -> Self {
            Self {
                failed_column: None,
                cnt,
            }
        }

        fn fail_on(column: &'static str) -> Self {
            Self {
                failed_column: Some(column),
                cnt: 3,
            }
        }

        fn maybe_fail(&self, column: &str) -> Result<(), sqlx::Error> {
            if self.failed_column == Some(column) {
                Err(sqlx::Error::ColumnNotFound(column.to_string()))
            } else {
                Ok(())
            }
        }
    }

    impl IntrospectionRow for FakeContextSnapshotCountRow {
        fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
            self.maybe_fail(column)?;
            Err(sqlx::Error::ColumnNotFound(column.to_string()))
        }

        fn optional_string_column(&self, column: &str) -> Result<Option<String>, sqlx::Error> {
            self.maybe_fail(column)?;
            Err(sqlx::Error::ColumnNotFound(column.to_string()))
        }

        fn i64_column(&self, column: &str) -> Result<i64, sqlx::Error> {
            self.maybe_fail(column)?;
            match column {
                "cnt" => Ok(self.cnt),
                _ => Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
        }

        fn optional_i64_column(&self, column: &str) -> Result<Option<i64>, sqlx::Error> {
            self.maybe_fail(column)?;
            Err(sqlx::Error::ColumnNotFound(column.to_string()))
        }
    }

    struct FakeContextSnapshotRow {
        failed_column: Option<&'static str>,
        empty_column: Option<&'static str>,
        token_budget: &'static str,
        relevance_scores: &'static str,
        task_type: Option<&'static str>,
        llm_response_id: Option<&'static str>,
        total_tokens: Option<i64>,
        assembly_time_ms: Option<i64>,
        selected_events: Option<&'static str>,
        code_context: Option<&'static str>,
        skill_definitions: Option<&'static str>,
        documentation: Option<&'static str>,
    }

    impl FakeContextSnapshotRow {
        fn complete() -> Self {
            Self {
                failed_column: None,
                empty_column: None,
                token_budget: r#"{"tool_schemas":10,"history":20}"#,
                relevance_scores: r#"{"memory":0.8,"noise":0.2}"#,
                task_type: Some("debugging"),
                llm_response_id: Some("llm-1"),
                total_tokens: Some(30),
                assembly_time_ms: Some(12),
                selected_events: Some(r#"[{"event_type":"user_query"}]"#),
                code_context: Some(r#"[{"file":"src/main.rs"}]"#),
                skill_definitions: Some(r#"[{"skill_name":"code-review"}]"#),
                documentation: Some(r#"[{"source":"README.md"}]"#),
            }
        }

        fn without_optional_values() -> Self {
            Self {
                task_type: None,
                llm_response_id: None,
                total_tokens: None,
                assembly_time_ms: None,
                selected_events: None,
                code_context: None,
                skill_definitions: None,
                documentation: None,
                ..Self::complete()
            }
        }

        fn fail_on(column: &'static str) -> Self {
            Self {
                failed_column: Some(column),
                ..Self::complete()
            }
        }

        fn empty_on(column: &'static str) -> Self {
            Self {
                empty_column: Some(column),
                ..Self::complete()
            }
        }

        fn with_token_budget(token_budget: &'static str) -> Self {
            Self {
                token_budget,
                ..Self::complete()
            }
        }

        fn with_relevance_scores(relevance_scores: &'static str) -> Self {
            Self {
                relevance_scores,
                ..Self::complete()
            }
        }

        fn with_optional_i64(column: &'static str, value: Option<i64>) -> Self {
            let mut row = Self::complete();
            match column {
                "total_tokens" => row.total_tokens = value,
                "assembly_time_ms" => row.assembly_time_ms = value,
                _ => unreachable!("unexpected optional i64 column: {column}"),
            }
            row
        }

        fn maybe_fail(&self, column: &str) -> Result<(), sqlx::Error> {
            if self.failed_column == Some(column) {
                Err(sqlx::Error::ColumnNotFound(column.to_string()))
            } else {
                Ok(())
            }
        }

        fn text(&self, column: &str, value: &'static str) -> String {
            if self.empty_column == Some(column) {
                String::new()
            } else {
                value.to_string()
            }
        }
    }

    impl IntrospectionRow for FakeContextSnapshotRow {
        fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
            self.maybe_fail(column)?;
            Ok(match column {
                "context_capture_id" => self.text(column, "capture-1"),
                "token_budget" => self.text(column, self.token_budget),
                "relevance_scores" => self.text(column, self.relevance_scores),
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            })
        }

        fn optional_string_column(&self, column: &str) -> Result<Option<String>, sqlx::Error> {
            self.maybe_fail(column)?;
            Ok(match column {
                "task_type" => self.task_type,
                "llm_response_id" => self.llm_response_id,
                "selected_events" => self.selected_events,
                "code_context" => self.code_context,
                "skill_definitions" => self.skill_definitions,
                "documentation" => self.documentation,
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
            .map(|value| self.text(column, value)))
        }

        fn i64_column(&self, column: &str) -> Result<i64, sqlx::Error> {
            self.maybe_fail(column)?;
            Err(sqlx::Error::ColumnNotFound(column.to_string()))
        }

        fn optional_i64_column(&self, column: &str) -> Result<Option<i64>, sqlx::Error> {
            self.maybe_fail(column)?;
            match column {
                "total_tokens" => Ok(self.total_tokens),
                "assembly_time_ms" => Ok(self.assembly_time_ms),
                _ => Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
        }
    }

    struct FakeTokenUsageRow {
        failed_column: Option<&'static str>,
        empty_column: Option<&'static str>,
        token_usage: &'static str,
    }

    impl FakeTokenUsageRow {
        fn complete() -> Self {
            Self {
                failed_column: None,
                empty_column: None,
                token_usage: r#"{"input_tokens":100,"cached_input_tokens":0,"cache_creation_tokens":0,"output_tokens":20,"total_tokens":120}"#,
            }
        }

        fn fail_on(column: &'static str) -> Self {
            Self {
                failed_column: Some(column),
                ..Self::complete()
            }
        }

        fn empty_on(column: &'static str) -> Self {
            Self {
                empty_column: Some(column),
                ..Self::complete()
            }
        }

        fn with_token_usage(token_usage: &'static str) -> Self {
            Self {
                token_usage,
                ..Self::complete()
            }
        }

        fn maybe_fail(&self, column: &str) -> Result<(), sqlx::Error> {
            if self.failed_column == Some(column) {
                Err(sqlx::Error::ColumnNotFound(column.to_string()))
            } else {
                Ok(())
            }
        }

        fn text(&self, column: &str, value: &'static str) -> String {
            if self.empty_column == Some(column) {
                String::new()
            } else {
                value.to_string()
            }
        }
    }

    impl IntrospectionRow for FakeTokenUsageRow {
        fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
            self.maybe_fail(column)?;
            Ok(match column {
                "event_id" => self.text(column, "llm-1"),
                "token_usage" => self.text(column, self.token_usage),
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            })
        }

        fn optional_string_column(&self, column: &str) -> Result<Option<String>, sqlx::Error> {
            self.maybe_fail(column)?;
            Err(sqlx::Error::ColumnNotFound(column.to_string()))
        }

        fn i64_column(&self, column: &str) -> Result<i64, sqlx::Error> {
            self.maybe_fail(column)?;
            Err(sqlx::Error::ColumnNotFound(column.to_string()))
        }

        fn optional_i64_column(&self, column: &str) -> Result<Option<i64>, sqlx::Error> {
            self.maybe_fail(column)?;
            Err(sqlx::Error::ColumnNotFound(column.to_string()))
        }
    }

    struct FakeContextTrendResponseEventRow {
        failed_column: Option<&'static str>,
        empty_column: Option<&'static str>,
    }

    impl FakeContextTrendResponseEventRow {
        fn complete() -> Self {
            Self {
                failed_column: None,
                empty_column: None,
            }
        }

        fn fail_on(column: &'static str) -> Self {
            Self {
                failed_column: Some(column),
                ..Self::complete()
            }
        }

        fn empty_on(column: &'static str) -> Self {
            Self {
                empty_column: Some(column),
                ..Self::complete()
            }
        }

        fn maybe_fail(&self, column: &str) -> Result<(), sqlx::Error> {
            if self.failed_column == Some(column) {
                Err(sqlx::Error::ColumnNotFound(column.to_string()))
            } else {
                Ok(())
            }
        }
    }

    impl IntrospectionRow for FakeContextTrendResponseEventRow {
        fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
            self.maybe_fail(column)?;
            match column {
                "response_event_id" => {
                    if self.empty_column == Some(column) {
                        Ok(String::new())
                    } else {
                        Ok("llm-1".to_string())
                    }
                }
                _ => Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
        }

        fn optional_string_column(&self, column: &str) -> Result<Option<String>, sqlx::Error> {
            self.maybe_fail(column)?;
            Err(sqlx::Error::ColumnNotFound(column.to_string()))
        }

        fn i64_column(&self, column: &str) -> Result<i64, sqlx::Error> {
            self.maybe_fail(column)?;
            Err(sqlx::Error::ColumnNotFound(column.to_string()))
        }

        fn optional_i64_column(&self, column: &str) -> Result<Option<i64>, sqlx::Error> {
            self.maybe_fail(column)?;
            Err(sqlx::Error::ColumnNotFound(column.to_string()))
        }
    }

    struct FakeDriftPreviewRow {
        failed_column: Option<&'static str>,
        preview: &'static str,
    }

    impl FakeDriftPreviewRow {
        fn with_preview(preview: &'static str) -> Self {
            Self {
                failed_column: None,
                preview,
            }
        }

        fn fail_on(column: &'static str) -> Self {
            Self {
                failed_column: Some(column),
                preview: "initial task",
            }
        }

        fn maybe_fail(&self, column: &str) -> Result<(), sqlx::Error> {
            if self.failed_column == Some(column) {
                Err(sqlx::Error::ColumnNotFound(column.to_string()))
            } else {
                Ok(())
            }
        }
    }

    impl IntrospectionRow for FakeDriftPreviewRow {
        fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
            self.maybe_fail(column)?;
            match column {
                "preview" => Ok(self.preview.to_string()),
                _ => Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
        }

        fn optional_string_column(&self, column: &str) -> Result<Option<String>, sqlx::Error> {
            self.maybe_fail(column)?;
            Err(sqlx::Error::ColumnNotFound(column.to_string()))
        }

        fn i64_column(&self, column: &str) -> Result<i64, sqlx::Error> {
            self.maybe_fail(column)?;
            Err(sqlx::Error::ColumnNotFound(column.to_string()))
        }

        fn optional_i64_column(&self, column: &str) -> Result<Option<i64>, sqlx::Error> {
            self.maybe_fail(column)?;
            Err(sqlx::Error::ColumnNotFound(column.to_string()))
        }
    }

    fn assert_introspection_internal_error_mentions(
        result: ServiceResult<impl std::fmt::Debug>,
        needle: &str,
    ) {
        let (status, axum::Json(body)) = result.expect_err("decode should fail");
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            body.detail.contains(needle),
            "introspection decode error should identify `{needle}`: {:?}",
            body.detail
        );
    }

    #[test]
    fn installed_skill_row_decode_preserves_values_and_fails_loudly() {
        let skill = installed_skill_info_from_row(&FakeIntrospectionSkillRow::complete()).unwrap();
        assert_eq!(skill.name, "code-review");
        assert_eq!(skill.version, "1.2.3");
        assert_eq!(skill.description, "Review Rust changes");
        assert_eq!(skill.category, "engineering");

        let skill =
            installed_skill_info_from_row(&FakeIntrospectionSkillRow::without_registry_metadata())
                .unwrap();
        assert_eq!(skill.description, "");
        assert_eq!(skill.category, "");

        for column in ["skill_name", "skill_version", "description", "category"] {
            assert_introspection_internal_error_mentions(
                installed_skill_info_from_row(&FakeIntrospectionSkillRow::fail_on(column)),
                column,
            );
        }

        for column in ["skill_name", "skill_version"] {
            assert_introspection_internal_error_mentions(
                installed_skill_info_from_row(&FakeIntrospectionSkillRow::empty_on(column)),
                "expected non-empty string",
            );
        }
    }

    #[test]
    fn cloud_skill_row_decode_preserves_values_and_fails_loudly() {
        let skill = cloud_skill_info_from_row(&FakeIntrospectionSkillRow::complete()).unwrap();
        assert_eq!(skill.name, "code-review");
        assert_eq!(skill.version, "2.0.0");
        assert_eq!(skill.description, "Review Rust changes");
        assert_eq!(skill.category, "engineering");

        let skill =
            cloud_skill_info_from_row(&FakeIntrospectionSkillRow::without_registry_metadata())
                .unwrap();
        assert_eq!(skill.description, "");
        assert_eq!(skill.category, "");

        for column in ["skill_name", "version", "description", "category"] {
            assert_introspection_internal_error_mentions(
                cloud_skill_info_from_row(&FakeIntrospectionSkillRow::fail_on(column)),
                column,
            );
        }

        for column in ["skill_name", "version"] {
            assert_introspection_internal_error_mentions(
                cloud_skill_info_from_row(&FakeIntrospectionSkillRow::empty_on(column)),
                "expected non-empty string",
            );
        }
    }

    #[test]
    fn decision_trace_row_decode_preserves_values_and_fails_loudly() {
        let decision = decision_trace_row_from_row(&FakeDecisionTraceRow::complete()).unwrap();
        assert_eq!(decision["decision_id"], "decision-1");
        assert_eq!(decision["event_id"], "event-1");
        assert_eq!(decision["decision_type"], "tool_surface");
        assert_eq!(decision["model_used"], "glm-5.2");
        assert_eq!(decision["created_at"], "2026-06-26T12:00:00");
        assert_eq!(
            decision["output"],
            serde_json::json!({"visible_tools":["bash"],"reason":"inspect"})
        );

        let decision = decision_trace_row_from_row(&FakeDecisionTraceRow::without_model()).unwrap();
        assert!(decision["model_used"].is_null());

        for column in [
            "decision_id",
            "event_id",
            "decision_type",
            "model_used",
            "output_json",
            "created_at",
        ] {
            assert_introspection_internal_error_mentions(
                decision_trace_row_from_row(&FakeDecisionTraceRow::fail_on(column)),
                column,
            );
        }

        for column in ["decision_id", "event_id", "decision_type", "created_at"] {
            assert_introspection_internal_error_mentions(
                decision_trace_row_from_row(&FakeDecisionTraceRow::empty_on(column)),
                "expected non-empty string",
            );
        }

        assert_introspection_internal_error_mentions(
            decision_trace_row_from_row(&FakeDecisionTraceRow::with_output_json("{not-json")),
            "output_json",
        );
    }

    #[test]
    fn tool_history_agg_row_decode_preserves_values_and_fails_loudly() {
        let agg = tool_history_agg_from_row(&FakeToolHistoryRow::complete()).unwrap();
        assert_eq!(
            agg,
            ToolHistoryAgg {
                tool_total_calls: 5,
                tool_fail_count: 2,
                ask_user_total_calls: 3,
                ask_user_fail_count: 1,
            }
        );

        for column in [
            "tool_total_calls",
            "tool_fail_count",
            "ask_user_total_calls",
            "ask_user_fail_count",
        ] {
            assert_introspection_internal_error_mentions(
                tool_history_agg_from_row(&FakeToolHistoryRow::fail_on(column)),
                column,
            );
        }

        for (row, needle) in [
            (
                FakeToolHistoryRow::with_counts(-1, 0, 0, 0),
                "tool_total_calls",
            ),
            (
                FakeToolHistoryRow::with_counts(1, 2, 0, 0),
                "tool_fail_count",
            ),
            (
                FakeToolHistoryRow::with_counts(0, 0, -1, 0),
                "ask_user_total_calls",
            ),
            (
                FakeToolHistoryRow::with_counts(0, 0, 1, 2),
                "ask_user_fail_count",
            ),
        ] {
            assert_introspection_internal_error_mentions(tool_history_agg_from_row(&row), needle);
        }
    }

    #[test]
    fn tool_history_failure_row_decode_preserves_values_and_fails_loudly() {
        let failure = tool_history_failure_from_row(&FakeToolHistoryRow::complete()).unwrap();
        assert_eq!(failure["session_id"], "session-1");
        assert_eq!(failure["error_preview"], "permission denied");
        assert_eq!(failure["created_at"], "2026-06-26T12:00:00");

        for column in ["session_id", "error_preview", "created_at"] {
            assert_introspection_internal_error_mentions(
                tool_history_failure_from_row(&FakeToolHistoryRow::fail_on(column)),
                column,
            );
        }

        for column in ["session_id", "created_at"] {
            assert_introspection_internal_error_mentions(
                tool_history_failure_from_row(&FakeToolHistoryRow::empty_on(column)),
                "expected non-empty string",
            );
        }
    }

    #[test]
    fn ask_user_history_row_decode_preserves_values_and_fails_loudly() {
        let row = ask_user_history_row_from_row(&FakeToolHistoryRow::complete()).unwrap();
        assert_eq!(row.session_id, "session-1");
        assert_eq!(row.event_type, "ask_user_submitted");
        assert_eq!(row.created_at, "2026-06-26T12:00:00");
        assert_eq!(row.metadata["ask_user"]["response"]["outcome"], "submitted");
        assert_eq!(row.content_preview, "ask_user submitted");

        for column in [
            "session_id",
            "event_type",
            "created_at",
            "metadata_json",
            "content_preview",
        ] {
            assert_introspection_internal_error_mentions(
                ask_user_history_row_from_row(&FakeToolHistoryRow::fail_on(column)),
                column,
            );
        }

        for column in ["session_id", "event_type", "created_at"] {
            assert_introspection_internal_error_mentions(
                ask_user_history_row_from_row(&FakeToolHistoryRow::empty_on(column)),
                "expected non-empty string",
            );
        }

        assert_introspection_internal_error_mentions(
            ask_user_history_row_from_row(&FakeToolHistoryRow::with_metadata_json("{not-json")),
            "metadata_json",
        );
    }

    #[test]
    fn retrieval_quality_row_decode_preserves_scores_and_fails_loudly() {
        let scores = retrieval_quality_scores_from_row(&FakeRetrievalQualityRow::with_scores(
            r#"{"memory":0.8,"noise":0.2}"#,
        ))
        .unwrap();
        assert_eq!(scores.get("memory"), Some(&0.8));
        assert_eq!(scores.get("noise"), Some(&0.2));

        let empty =
            retrieval_quality_scores_from_row(&FakeRetrievalQualityRow::with_scores("{}")).unwrap();
        assert!(empty.is_empty());

        assert_introspection_internal_error_mentions(
            retrieval_quality_scores_from_row(&FakeRetrievalQualityRow::fail_on(
                "relevance_scores",
            )),
            "relevance_scores",
        );

        for raw in [
            "{not-json",
            "[]",
            r#"{"memory":"high"}"#,
            r#"{"memory":1.2}"#,
            r#"{"memory":-0.1}"#,
        ] {
            assert_introspection_internal_error_mentions(
                retrieval_quality_scores_from_row(&FakeRetrievalQualityRow::with_scores(raw)),
                "relevance_scores",
            );
        }
    }

    #[test]
    fn context_snapshot_count_row_decode_preserves_value_and_fails_loudly() {
        let count = introspection_row_non_negative_i64(
            &FakeContextSnapshotCountRow::with_count(3),
            "context_snapshot_count_row",
            "cnt",
        )
        .unwrap();
        assert_eq!(count, 3);

        assert_introspection_internal_error_mentions(
            introspection_row_non_negative_i64(
                &FakeContextSnapshotCountRow::fail_on("cnt"),
                "context_snapshot_count_row",
                "cnt",
            ),
            "cnt",
        );
        assert_introspection_internal_error_mentions(
            introspection_row_non_negative_i64(
                &FakeContextSnapshotCountRow::with_count(-1),
                "context_snapshot_count_row",
                "cnt",
            ),
            "non-negative integer",
        );
    }

    #[test]
    fn context_snapshot_core_row_decode_preserves_values_and_fails_loudly() {
        let row = context_snapshot_core_from_row(&FakeContextSnapshotRow::complete()).unwrap();
        assert_eq!(row.snapshot_id, "capture-1");
        assert_eq!(row.task_type.as_deref(), Some("debugging"));
        assert_eq!(row.ctx_managed_tokens, Some(30));
        assert_eq!(row.assembly_ms, Some(12));
        assert_eq!(row.budget["tool_schemas"], 10);
        assert_eq!(row.relevance_scores.get("memory"), Some(&0.8));
        assert_eq!(row.llm_response_id.as_deref(), Some("llm-1"));

        let row =
            context_snapshot_core_from_row(&FakeContextSnapshotRow::without_optional_values())
                .unwrap();
        assert_eq!(row.task_type, None);
        assert_eq!(row.ctx_managed_tokens, None);
        assert_eq!(row.assembly_ms, None);
        assert_eq!(row.llm_response_id, None);

        for column in [
            "context_capture_id",
            "task_type",
            "total_tokens",
            "assembly_time_ms",
            "token_budget",
            "relevance_scores",
            "llm_response_id",
        ] {
            assert_introspection_internal_error_mentions(
                context_snapshot_core_from_row(&FakeContextSnapshotRow::fail_on(column)),
                column,
            );
        }

        assert_introspection_internal_error_mentions(
            context_snapshot_core_from_row(&FakeContextSnapshotRow::empty_on("context_capture_id")),
            "expected non-empty string",
        );

        let row = context_snapshot_core_from_row(&FakeContextSnapshotRow::with_token_budget("42"))
            .expect("legacy integer token budget decodes");
        assert_eq!(row.budget["total"], 42);

        for raw_budget in ["{not-json", "[]"] {
            assert_introspection_internal_error_mentions(
                context_snapshot_core_from_row(&FakeContextSnapshotRow::with_token_budget(
                    raw_budget,
                )),
                "token_budget",
            );
        }

        for raw_scores in [
            "{not-json",
            "[]",
            r#"{"memory":"high"}"#,
            r#"{"memory":1.2}"#,
        ] {
            assert_introspection_internal_error_mentions(
                context_snapshot_core_from_row(&FakeContextSnapshotRow::with_relevance_scores(
                    raw_scores,
                )),
                "relevance_scores",
            );
        }

        for column in ["total_tokens", "assembly_time_ms"] {
            assert_introspection_internal_error_mentions(
                context_snapshot_core_from_row(&FakeContextSnapshotRow::with_optional_i64(
                    column,
                    Some(-1),
                )),
                column,
            );
        }
    }

    #[test]
    fn context_snapshot_content_row_decode_preserves_values_and_fails_loudly() {
        let content =
            context_snapshot_content_from_row(&FakeContextSnapshotRow::complete()).unwrap();
        assert_eq!(
            content.selected_events.as_deref(),
            Some(r#"[{"event_type":"user_query"}]"#)
        );
        assert_eq!(
            content.code_context.as_deref(),
            Some(r#"[{"file":"src/main.rs"}]"#)
        );
        assert_eq!(
            content.skill_definitions.as_deref(),
            Some(r#"[{"skill_name":"code-review"}]"#)
        );
        assert_eq!(
            content.documentation.as_deref(),
            Some(r#"[{"source":"README.md"}]"#)
        );

        let content =
            context_snapshot_content_from_row(&FakeContextSnapshotRow::without_optional_values())
                .unwrap();
        assert!(content.selected_events.is_none());
        assert!(content.code_context.is_none());
        assert!(content.skill_definitions.is_none());
        assert!(content.documentation.is_none());

        for column in [
            "selected_events",
            "code_context",
            "skill_definitions",
            "documentation",
        ] {
            assert_introspection_internal_error_mentions(
                context_snapshot_content_from_row(&FakeContextSnapshotRow::fail_on(column)),
                column,
            );
        }
    }

    #[test]
    fn token_usage_event_row_decode_preserves_values_and_fails_loudly() {
        let row = token_usage_event_from_row(&FakeTokenUsageRow::complete()).unwrap();
        assert_eq!(row.event_id, "llm-1");
        assert_eq!(row.token_usage["input_tokens"], 100);
        assert_eq!(row.token_usage["total_tokens"], 120);

        for column in ["event_id", "token_usage"] {
            assert_introspection_internal_error_mentions(
                token_usage_event_from_row(&FakeTokenUsageRow::fail_on(column)),
                column,
            );
        }

        assert_introspection_internal_error_mentions(
            token_usage_event_from_row(&FakeTokenUsageRow::empty_on("event_id")),
            "expected non-empty string",
        );

        for raw in ["{not-json", "[]"] {
            assert_introspection_internal_error_mentions(
                token_usage_event_from_row(&FakeTokenUsageRow::with_token_usage(raw)),
                "token_usage",
            );
        }
    }

    #[test]
    fn context_trend_response_event_row_decode_preserves_value_and_fails_loudly() {
        let event_id =
            context_trend_response_event_id_from_row(&FakeContextTrendResponseEventRow::complete())
                .unwrap();
        assert_eq!(event_id, "llm-1");

        assert_introspection_internal_error_mentions(
            context_trend_response_event_id_from_row(&FakeContextTrendResponseEventRow::fail_on(
                "response_event_id",
            )),
            "response_event_id",
        );
        assert_introspection_internal_error_mentions(
            context_trend_response_event_id_from_row(&FakeContextTrendResponseEventRow::empty_on(
                "response_event_id",
            )),
            "expected non-empty string",
        );
    }

    #[test]
    fn drift_preview_row_decode_preserves_optional_absence_and_fails_loudly() {
        let missing =
            optional_drift_preview_from_row(None::<&FakeDriftPreviewRow>, "drift_preview_row")
                .unwrap();
        assert_eq!(missing, "");

        let preview = optional_drift_preview_from_row(
            Some(&FakeDriftPreviewRow::with_preview("initial task")),
            "drift_preview_row",
        )
        .unwrap();
        assert_eq!(preview, "initial task");

        let empty = optional_drift_preview_from_row(
            Some(&FakeDriftPreviewRow::with_preview("")),
            "drift_preview_row",
        )
        .unwrap();
        assert_eq!(empty, "");

        assert_introspection_internal_error_mentions(
            optional_drift_preview_from_row(
                Some(&FakeDriftPreviewRow::fail_on("preview")),
                "drift_preview_row",
            ),
            "preview",
        );
    }

    // ── summarize_contents ──────────────────────────────────────────────

    #[test]
    fn summarize_contents_all_none() {
        let result = summarize_contents(None, None, None, None);
        assert!(result.as_object().unwrap().is_empty());
    }

    #[test]
    fn summarize_contents_valid_events() {
        let events = r#"[{"event_type":"user_query"},{"event_type":"tool_call"},{"event_type":"user_query"}]"#;
        let result = summarize_contents(Some(events), None, None, None);
        assert_eq!(result["events"]["total"], 3);
        let by_type = result["events"]["by_type"].as_object().unwrap();
        assert_eq!(by_type["user_query"], 2);
        assert_eq!(by_type["tool_call"], 1);
    }

    #[test]
    fn summarize_contents_malformed_events_json() {
        let result = summarize_contents(Some("not json"), None, None, None);
        // Malformed JSON → silently skipped
        assert!(result.as_object().unwrap().get("events").is_none());
    }

    #[test]
    fn summarize_contents_valid_code() {
        let code = r#"[{"file":"src/main.rs"},{"path":"src/lib.rs"}]"#;
        let result = summarize_contents(None, Some(code), None, None);
        assert_eq!(result["code"]["files"], 2);
        let paths = result["code"]["paths"].as_array().unwrap();
        assert!(paths.contains(&Value::String("src/main.rs".into())));
        assert!(paths.contains(&Value::String("src/lib.rs".into())));
    }

    #[test]
    fn summarize_contents_code_missing_path_field() {
        let code = r#"[{"content":"fn main(){}"}]"#;
        let result = summarize_contents(None, Some(code), None, None);
        assert_eq!(result["code"]["files"], 1);
        // No file/path field → filtered out
        assert!(result["code"]["paths"].as_array().unwrap().is_empty());
    }

    #[test]
    fn summarize_contents_valid_skills() {
        let skills = r#"[{"skill_name":"code_review"},{"skill_name":"test_gen"}]"#;
        let result = summarize_contents(None, None, Some(skills), None);
        let names = result["skills"].as_array().unwrap();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&Value::String("code_review".into())));
    }

    #[test]
    fn summarize_contents_valid_docs() {
        let docs = r#"[{"source":"README.md"},{"title":"API Guide"}]"#;
        let result = summarize_contents(None, None, None, Some(docs));
        let titles = result["docs"].as_array().unwrap();
        assert_eq!(titles.len(), 2);
    }

    #[test]
    fn summarize_contents_event_without_type() {
        let events = r#"[{"content":"hello"}]"#;
        let result = summarize_contents(Some(events), None, None, None);
        // Missing event_type → counted as "unknown"
        let by_type = result["events"]["by_type"].as_object().unwrap();
        assert_eq!(by_type["unknown"], 1);
    }

    // ── raw_contents ────────────────────────────────────────────────────

    #[test]
    fn raw_contents_all_none() {
        let result = raw_contents(None, None, None, None, 1000);
        assert!(result.as_object().unwrap().is_empty());
    }

    #[test]
    fn raw_contents_fits_budget() {
        let events = r#"[{"event_type":"user_query","content":"hi"}]"#;
        let result = raw_contents(Some(events), None, None, None, 1000);
        let arr = result["events"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert!(result.get("events_truncated").is_none());
    }

    #[test]
    fn raw_contents_zero_budget() {
        let events = r#"[{"event_type":"x"}]"#;
        // token_budget=0 → char_budget=0 → nothing fits
        let result = raw_contents(Some(events), None, None, None, 0);
        assert!(result.as_object().unwrap().is_empty());
    }

    #[test]
    fn raw_contents_truncates_large_data() {
        // Create events that exceed the budget
        let mut items = Vec::new();
        for i in 0..100 {
            items.push(serde_json::json!({"idx": i, "data": "x".repeat(50)}));
        }
        let events = serde_json::to_string(&items).unwrap();
        // Very small budget → only some items fit
        let result = raw_contents(Some(&events), None, None, None, 50);
        if let Some(arr) = result["events"].as_array() {
            assert!(arr.len() < 100);
            assert_eq!(result["events_truncated"], true);
        }
    }

    #[test]
    fn raw_contents_malformed_json_skipped() {
        let result = raw_contents(Some("not json"), None, None, None, 1000);
        assert!(result.as_object().unwrap().get("events").is_none());
    }

    #[test]
    fn raw_contents_empty_blob_skipped() {
        let result = raw_contents(Some(""), None, None, None, 1000);
        assert!(result.as_object().unwrap().is_empty());
    }

    #[test]
    fn raw_contents_multiple_sections() {
        let events = r#"[{"e":1}]"#;
        let code = r#"[{"file":"a.rs"}]"#;
        let result = raw_contents(Some(events), Some(code), None, None, 5000);
        assert!(result.get("events").is_some());
        assert!(result.get("code").is_some());
    }

    #[test]
    fn ask_user_history_summary_aggregates_recent_interactions() {
        let rows = vec![
            AskUserHistoryRow {
                session_id: "s1".into(),
                event_type: "tool_call".into(),
                created_at: "2026-01-01T00:00:00".into(),
                metadata: serde_json::json!({
                    "ask_user": {
                        "prompt": {
                            "question_count": 2,
                            "headers": ["Scope", "Notes"],
                            "questions": [{"question": "Which scope should we ship first?"}]
                        },
                        "response": {
                            "outcome": "submitted",
                            "answered_question_count": 2,
                            "annotation_count": 1,
                            "freeform_answer_count": 1
                        }
                    }
                }),
                content_preview: String::new(),
            },
            AskUserHistoryRow {
                session_id: "s2".into(),
                event_type: "tool_error".into(),
                created_at: "2026-01-02T00:00:00".into(),
                metadata: serde_json::json!({
                    "ask_user": {
                        "prompt": {
                            "question_count": 1,
                            "headers": ["Scope"],
                            "questions": [{"question": "Which scope should we ship first?"}]
                        },
                        "response": {
                            "outcome": "cancelled",
                            "answered_question_count": 0,
                            "annotation_count": 0,
                            "freeform_answer_count": 0
                        }
                    }
                }),
                content_preview: "ask_user failed".into(),
            },
        ];

        let summary = build_ask_user_history_summary(&rows);
        assert_eq!(summary["interactions_observed"], 2);
        assert_eq!(summary["submitted_count"], 1);
        assert_eq!(summary["cancelled_count"], 1);
        assert_eq!(summary["avg_question_count"], 1.5);
        assert_eq!(summary["recent_interactions"][0]["outcome"], "submitted");
    }

    #[test]
    fn ask_user_history_summary_prefers_first_class_events_without_double_counting() {
        let rows = vec![
            AskUserHistoryRow {
                session_id: "s1".into(),
                event_type: "ask_user_prompted".into(),
                created_at: "2026-01-01T00:00:00".into(),
                metadata: serde_json::json!({
                    "request_id": "req-1",
                    "ask_user": {
                        "prompt": {
                            "question_count": 2,
                            "headers": ["Scope", "Notes"],
                            "questions": [{"question": "Which scope should we ship first?"}]
                        }
                    }
                }),
                content_preview: String::new(),
            },
            AskUserHistoryRow {
                session_id: "s1".into(),
                event_type: "ask_user_submitted".into(),
                created_at: "2026-01-01T00:00:01".into(),
                metadata: serde_json::json!({
                    "request_id": "req-1",
                    "ask_user": {
                        "prompt": {
                            "question_count": 2,
                            "headers": ["Scope", "Notes"],
                            "questions": [{"question": "Which scope should we ship first?"}]
                        },
                        "response": {
                            "outcome": "submitted",
                            "answered_question_count": 2,
                            "annotation_count": 1,
                            "freeform_answer_count": 0
                        }
                    }
                }),
                content_preview: String::new(),
            },
            AskUserHistoryRow {
                session_id: "s1".into(),
                event_type: "tool_call".into(),
                created_at: "2026-01-01T00:00:02".into(),
                metadata: serde_json::json!({
                    "ask_user": {
                        "prompt": {
                            "question_count": 2,
                            "headers": ["Scope", "Notes"],
                            "questions": [{"question": "Which scope should we ship first?"}]
                        },
                        "response": {
                            "outcome": "submitted",
                            "answered_question_count": 2,
                            "annotation_count": 1,
                            "freeform_answer_count": 0
                        }
                    }
                }),
                content_preview: "legacy duplicate".into(),
            },
        ];

        let summary = build_ask_user_history_summary(&rows);
        assert_eq!(summary["prompt_count"], 1);
        assert_eq!(summary["avg_question_count"], 2.0);
        assert_eq!(summary["interactions_observed"], 1);
        assert_eq!(summary["recent_interactions"].as_array().unwrap().len(), 1);
    }

    // ── UnconfiguredIntrospectionService ─────────────────────────────────

    #[tokio::test]
    async fn unconfigured_service_returns_errors() {
        use super::super::{IntrospectionService, UnconfiguredIntrospectionService};
        let svc = UnconfiguredIntrospectionService;
        assert!(svc.get_skills_introspection("u1").await.is_err());
        assert!(svc.get_context_trend("u1", "s1", 10, 128000).await.is_err());
        assert!(
            svc.get_context_snapshot("u1", "s1", None, false, false, 2000)
                .await
                .is_err()
        );
        assert!(svc.get_retrieval_quality("u1", "s1", 5).await.is_err());
    }

    #[test]
    fn context_snapshot_usage_sampling_is_anchored_to_selected_response() {
        let source = include_str!("database.rs");
        let body = source
            .split("async fn get_context_snapshot")
            .nth(1)
            .and_then(|rest| rest.split("async fn get_retrieval_quality").next())
            .expect("get_context_snapshot body");

        assert!(
            body.contains("WHERE session_id = ? AND user_id = ? AND event_id = ?"),
            "current token usage must be loaded by the selected snapshot response id"
        );
        assert!(
            body.contains("anchor.event_id = ?") && body.contains("ORDER BY e.created_at DESC"),
            "trend sampling must use a recent window anchored to the selected response"
        );
        assert!(
            body.contains("trend_usage_rows.first()"),
            "fallback current usage should use the newest sampled row, not the oldest row"
        );
    }

    // ── Query type serde defaults ───────────────────────────────────────

    #[test]
    fn context_trend_query_defaults() {
        use super::super::ContextTrendQuery;
        let q: ContextTrendQuery = serde_json::from_str(r#"{"session_id":"s1"}"#).unwrap();
        assert_eq!(q.turns, 10);
        assert_eq!(q.context_window, 200000);
    }

    #[test]
    fn context_snapshot_query_defaults() {
        use super::super::ContextSnapshotQuery;
        let q: ContextSnapshotQuery = serde_json::from_str(r#"{"session_id":"s1"}"#).unwrap();
        assert!(!q.detail);
        assert!(!q.raw);
        assert_eq!(q.raw_token_budget, 2000);
        assert!(q.turn_index.is_none());
    }

    #[test]
    fn retrieval_quality_query_defaults() {
        use super::super::RetrievalQualityQuery;
        let q: RetrievalQualityQuery = serde_json::from_str(r#"{"session_id":"s1"}"#).unwrap();
        assert_eq!(q.turns, 5);
    }
}
