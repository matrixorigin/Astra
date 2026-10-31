use async_trait::async_trait;
use axum::http::StatusCode;
use serde_json::Value;
use sqlx::{Row, query};

use astra_core::{MatrixOneSettings, SharedPool, error_response, internal_error};

use crate::storage::agent_session_exists_for_user;

use super::{
    INTENT_DRIFT_ASSESSMENT_EVENT_TYPE, IntentDriftAssessmentV1, IntentDriftCheckResponseV2,
    IntrospectionService, ServiceResult, SkillInfo, SkillsIntrospectionResponse,
};

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

#[derive(Debug, Clone, PartialEq)]
struct IntentDriftAssessmentRow {
    event_id: String,
    created_at: String,
    assessment: IntentDriftAssessmentV1,
}

trait IntrospectionRow {
    fn string_column(&self, column: &str) -> Result<String, sqlx::Error>;
    fn i64_column(&self, column: &str) -> Result<i64, sqlx::Error>;
}

impl IntrospectionRow for sqlx::mysql::MySqlRow {
    fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
        self.try_get(column)
    }

    fn i64_column(&self, column: &str) -> Result<i64, sqlx::Error> {
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

fn introspection_row_i64(
    row: &impl IntrospectionRow,
    context: &str,
    column: &str,
) -> ServiceResult<i64> {
    row.i64_column(column)
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

fn intent_drift_assessment_from_row(
    row: &impl IntrospectionRow,
) -> ServiceResult<IntentDriftAssessmentRow> {
    let context = "intent_drift_assessment_row";
    let metadata_json = introspection_row_string(row, context, "metadata_json")?;
    let assessment: IntentDriftAssessmentV1 = serde_json::from_str(&metadata_json)
        .map_err(|error| introspection_decode_error(context, "metadata_json", error))?;
    assessment
        .validate()
        .map_err(|error| introspection_decode_error(context, "metadata_json", error))?;
    Ok(IntentDriftAssessmentRow {
        event_id: introspection_required_non_empty_string(row, context, "event_id")?,
        created_at: introspection_required_non_empty_string(row, context, "created_at")?,
        assessment,
    })
}

fn unavailable_intent_drift_response(
    user_id: &str,
    session_id: &str,
) -> ServiceResult<IntentDriftCheckResponseV2> {
    IntentDriftCheckResponseV2::unavailable(user_id, session_id).map_err(internal_error)
}

fn assessed_intent_drift_response(
    user_id: &str,
    session_id: &str,
    row: IntentDriftAssessmentRow,
) -> ServiceResult<IntentDriftCheckResponseV2> {
    IntentDriftCheckResponseV2::assessed(
        user_id,
        session_id,
        row.assessment,
        row.event_id,
        row.created_at,
    )
    .map_err(internal_error)
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
            matches!(
                row.event_type.as_str(),
                "tool_call_completed" | "tool_call_failed"
            )
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
               CAST(COALESCE(SUM(CASE WHEN event_type IN ('tool_call_completed', 'tool_call_failed') THEN 1 ELSE 0 END), 0) AS SIGNED) AS tool_total_calls, \
               CAST(COALESCE(SUM(CASE WHEN event_type = 'tool_call_failed' THEN 1 ELSE 0 END), 0) AS SIGNED) AS tool_fail_count, \
               CAST(COALESCE(SUM(CASE WHEN event_type IN ('ask_user_submitted', 'ask_user_cancelled', 'ask_user_timeout', 'ask_user_error') THEN 1 ELSE 0 END), 0) AS SIGNED) AS ask_user_total_calls, \
               CAST(COALESCE(SUM(CASE WHEN event_type IN ('ask_user_cancelled', 'ask_user_timeout', 'ask_user_error') THEN 1 ELSE 0 END), 0) AS SIGNED) AS ask_user_fail_count \
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
                   AND event_type = 'tool_call_failed' \
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
                       AND event_type IN ('tool_call_completed', 'tool_call_failed') \
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

    async fn get_drift_check(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> ServiceResult<IntentDriftCheckResponseV2> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        self.verify_session_owner(&pool, session_id, user_id)
            .await?;

        let latest = query(
            "SELECT event_id, IFNULL(CAST(metadata AS CHAR), '{}') AS metadata_json, \
                    CAST(created_at AS CHAR) AS created_at \
             FROM agent_events \
             WHERE session_id = ? AND user_id = ? AND event_type = ? \
             ORDER BY created_at DESC, event_id DESC LIMIT 1",
        )
        .bind(session_id)
        .bind(user_id)
        .bind(INTENT_DRIFT_ASSESSMENT_EVENT_TYPE)
        .fetch_optional(&pool)
        .await
        .map_err(internal_error)?;

        let Some(row) = latest.as_ref() else {
            return unavailable_intent_drift_response(user_id, session_id);
        };
        let assessment = intent_drift_assessment_from_row(row)?;
        assessed_intent_drift_response(user_id, session_id, assessment)
    }
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

        fn i64_column(&self, column: &str) -> Result<i64, sqlx::Error> {
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
    }

    struct FakeCountRow {
        failed_column: Option<&'static str>,
        cnt: i64,
    }

    impl FakeCountRow {
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

    impl IntrospectionRow for FakeCountRow {
        fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
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
    }

    struct FakeIntentDriftAssessmentRow {
        failed_column: Option<&'static str>,
        empty_column: Option<&'static str>,
        metadata_json: &'static str,
    }

    impl FakeIntentDriftAssessmentRow {
        fn complete() -> Self {
            Self {
                failed_column: None,
                empty_column: None,
                metadata_json: r#"{
                    "schema_version": 1,
                    "provenance": {
                        "kind": "llm_judge",
                        "invocation_id": "invocation-1",
                        "provider": "test-provider",
                        "model": "test-model",
                        "provider_response_id": "response-1"
                    },
                    "verdict": "drifting",
                    "score": 0.75,
                    "level": "high",
                    "evidence": ["tool activity no longer serves the judged objective"],
                    "turn": 3,
                    "round": 1
                }"#,
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

        fn with_metadata(metadata_json: &'static str) -> Self {
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
    }

    impl IntrospectionRow for FakeIntentDriftAssessmentRow {
        fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
            self.maybe_fail(column)?;
            let value: Result<&str, sqlx::Error> = match column {
                "event_id" => Ok("drift-assessment-1"),
                "created_at" => Ok("2026-07-30 12:00:00.000000"),
                "metadata_json" => Ok(self.metadata_json),
                _ => Err(sqlx::Error::ColumnNotFound(column.to_string())),
            };
            match value {
                Ok(_) if self.empty_column == Some(column) => Ok(String::new()),
                Ok(value) => Ok(value.to_string()),
                Err(error) => Err(error),
            }
        }

        fn i64_column(&self, column: &str) -> Result<i64, sqlx::Error> {
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
    fn aggregate_count_row_decode_preserves_value_and_fails_loudly() {
        let count = introspection_row_non_negative_i64(
            &FakeCountRow::with_count(3),
            "aggregate_count_row",
            "cnt",
        )
        .unwrap();
        assert_eq!(count, 3);

        assert_introspection_internal_error_mentions(
            introspection_row_non_negative_i64(
                &FakeCountRow::fail_on("cnt"),
                "aggregate_count_row",
                "cnt",
            ),
            "cnt",
        );
        assert_introspection_internal_error_mentions(
            introspection_row_non_negative_i64(
                &FakeCountRow::with_count(-1),
                "aggregate_count_row",
                "cnt",
            ),
            "non-negative integer",
        );
    }

    #[test]
    fn intent_drift_projection_requires_llm_provenance_and_fails_loudly_on_malformed_fact() {
        let row =
            intent_drift_assessment_from_row(&FakeIntentDriftAssessmentRow::complete()).unwrap();
        assert_eq!(row.event_id, "drift-assessment-1");
        assert_eq!(
            row.assessment.verdict,
            crate::introspection::IntentDriftVerdict::Drifting
        );
        assert_eq!(row.assessment.score, 0.75);
        assert_eq!(
            row.assessment.level,
            crate::introspection::IntentDriftLevel::High
        );
        assert_eq!(row.assessment.provenance.invocation_id, "invocation-1");

        let response =
            assessed_intent_drift_response("owner-1", "session-1", row).expect("valid projection");
        assert_eq!(response.schema_version, 2);
        assert_eq!(
            response.assessment_status,
            crate::introspection::IntentDriftAssessmentStatus::Assessed
        );
        assert_eq!(
            response.verdict,
            Some(crate::introspection::IntentDriftVerdict::Drifting)
        );
        assert_eq!(response.score, Some(0.75));
        assert_eq!(
            response.level,
            Some(crate::introspection::IntentDriftLevel::High)
        );
        assert_eq!(
            response.source_event_id.as_deref(),
            Some("drift-assessment-1")
        );

        for column in ["event_id", "created_at", "metadata_json"] {
            assert_introspection_internal_error_mentions(
                intent_drift_assessment_from_row(&FakeIntentDriftAssessmentRow::fail_on(column)),
                column,
            );
        }
        for column in ["event_id", "created_at"] {
            assert_introspection_internal_error_mentions(
                intent_drift_assessment_from_row(&FakeIntentDriftAssessmentRow::empty_on(column)),
                "expected non-empty string",
            );
        }

        for invalid in [
            r#"{"schema_version":1,"provenance":{"kind":"heuristic","invocation_id":"inv-1","provider":"p","model":"m"},"verdict":"drifting","score":0.8,"level":"high","evidence":[],"turn":1,"round":0}"#,
            r#"{"schema_version":2,"provenance":{"kind":"llm_judge","invocation_id":"inv-1","provider":"p","model":"m"},"verdict":"drifting","score":0.8,"level":"high","evidence":[],"turn":1,"round":0}"#,
            r#"{"schema_version":1,"provenance":{"kind":"llm_judge","invocation_id":"inv-1","provider":"p","model":"m"},"verdict":"aligned","score":0.2,"level":"high","evidence":[],"turn":1,"round":0}"#,
            r#"{"schema_version":1,"provenance":{"kind":"llm_judge","invocation_id":"inv-1","provider":"p","model":"m"},"verdict":"drifting","score":1.5,"level":"high","evidence":[],"turn":1,"round":0}"#,
        ] {
            assert_introspection_internal_error_mentions(
                intent_drift_assessment_from_row(&FakeIntentDriftAssessmentRow::with_metadata(
                    invalid,
                )),
                "metadata_json",
            );
        }
    }

    #[test]
    fn missing_intent_drift_assessment_is_unavailable_not_aligned() {
        let response = unavailable_intent_drift_response("owner-1", "session-1")
            .expect("valid unavailable projection");
        assert_eq!(response.schema_version, 2);
        assert_eq!(
            response.assessment_status,
            crate::introspection::IntentDriftAssessmentStatus::Unavailable
        );
        assert!(response.verdict.is_none());
        assert!(response.score.is_none());
        assert!(response.level.is_none());
        assert!(response.evidence.is_empty());
    }

    #[test]
    fn ask_user_history_summary_aggregates_recent_interactions() {
        let rows = vec![
            AskUserHistoryRow {
                session_id: "s1".into(),
                event_type: "tool_call_completed".into(),
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
                event_type: "tool_call_failed".into(),
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
                event_type: "tool_call_completed".into(),
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
    }
}
