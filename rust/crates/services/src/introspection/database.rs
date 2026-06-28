use async_trait::async_trait;
use axum::http::StatusCode;
use serde_json::Value;
use sqlx::{MySql, QueryBuilder, Row, query};

use astra_core::{MatrixOneSettings, SharedPool, error_response, internal_error};

use crate::storage::agent_session_exists_for_user;

use super::scoring::{
    DEGRADATION_DELTA, QUALITY_DEGRADED, QUALITY_GOOD, TOKEN_CHAR_RATIO, analyze_context_health,
    billable_input_from_canonical, compaction_effectiveness, compaction_forecast, compute_drift,
    compute_trend, parse_relevance_scores, parse_token_usage, pollution_ratio, relevance_quality,
    zone_balance,
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

fn extract_ask_user_audit(metadata: &Value) -> Option<&Value> {
    metadata.get("ask_user").or_else(|| {
        metadata
            .get("prompt")
            .map(|_| metadata)
            .filter(|meta| meta.get("response").is_some())
    })
}

fn is_first_class_ask_user_interaction_event(event_type: &str) -> bool {
    matches!(
        event_type,
        "ask_user_submitted"
            | "ask_user_cancelled"
            | "ask_user_timeout"
            | "ask_user_error"
            | "ask_user_auto_unanswered"
            | "ask_user_auto_duplicate"
    )
}

fn ask_user_outcome_from_audit(audit: &Value) -> Option<&str> {
    audit
        .get("response")
        .and_then(|response| response.get("outcome"))
        .and_then(Value::as_str)
        .or_else(|| audit.get("status").and_then(Value::as_str))
}

fn build_ask_user_history_summary(rows: &[AskUserHistoryRow]) -> Value {
    let has_first_class_events = rows
        .iter()
        .any(|row| row.event_type.starts_with("ask_user_"));
    let mut submitted_count = 0usize;
    let mut cancelled_count = 0usize;
    let mut timeout_count = 0usize;
    let mut auto_unanswered_count = 0usize;
    let mut auto_duplicate_count = 0usize;
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
            is_first_class_ask_user_interaction_event(row.event_type.as_str())
        } else {
            matches!(row.event_type.as_str(), "tool_call" | "tool_error")
        };
        if !counts_as_interaction {
            continue;
        }

        let response = audit.get("response");
        let Some(outcome) = ask_user_outcome_from_audit(audit) else {
            continue;
        };
        match outcome {
            "submitted" => submitted_count += 1,
            "cancelled" => cancelled_count += 1,
            "timeout" => timeout_count += 1,
            "auto_unanswered" => auto_unanswered_count += 1,
            "auto_unanswered_duplicate" => auto_duplicate_count += 1,
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
                .and_then(|response| response.get("answered_question_count"))
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            "annotation_count": response
                .and_then(|response| response.get("annotation_count"))
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            "freeform_answer_count": response
                .and_then(|response| response.get("freeform_answer_count"))
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            "content_preview": row.content_preview,
        }));
    }

    let no_answer_count =
        cancelled_count + timeout_count + auto_unanswered_count + auto_duplicate_count;
    let interactions_observed = submitted_count + no_answer_count + error_count;
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
        "auto_unanswered_count": auto_unanswered_count,
        "auto_duplicate_count": auto_duplicate_count,
        "no_answer_count": no_answer_count,
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
            "SELECT i.skill_name, i.skill_version, r.description, r.category \
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
            "SELECT skill_name, version, description, category \
             FROM skills_registry WHERE is_active = 1 \
             ORDER BY skill_name, version DESC LIMIT 200",
        )
        .fetch_all(&pool)
        .await
        .map_err(internal_error)?;

        let mut installed = Vec::with_capacity(installed_rows.len());
        let mut installed_names = std::collections::HashSet::new();
        for r in &installed_rows {
            let name: String = r.try_get("skill_name").unwrap_or_default();
            installed_names.insert(name.clone());
            installed.push(SkillInfo {
                name,
                version: r.try_get("skill_version").unwrap_or_default(),
                description: r.try_get("description").unwrap_or_default(),
                category: r.try_get("category").unwrap_or_default(),
            });
        }

        let mut cloud = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for r in &cloud_rows {
            let name: String = r.try_get("skill_name").unwrap_or_default();
            if seen.contains(&name) || installed_names.contains(&name) {
                continue;
            }
            seen.insert(name.clone());
            cloud.push(SkillInfo {
                name,
                version: r.try_get("version").unwrap_or_default(),
                description: r.try_get("description").unwrap_or_default(),
                category: r.try_get("category").unwrap_or_default(),
            });
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
            .filter_map(|row| row.try_get::<Option<String>, _>("response_event_id").ok()?)
            .collect();

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
            .filter_map(|row| {
                let event_id: String = row.try_get("event_id").ok()?;
                let token_usage: String = row.try_get("token_usage").ok()?;
                Some((event_id, token_usage))
            })
            .collect::<std::collections::HashMap<_, _>>();

        let usages: Vec<Value> = response_event_ids
            .iter()
            .filter_map(|response_event_id| {
                usage_by_event_id
                    .get(response_event_id)
                    .and_then(|raw| parse_token_usage(raw))
            })
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

        let total_turns: i64 = query(
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
        .map_err(internal_error)?
        .try_get("cnt")
        .unwrap_or(0);

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

        let snapshot_id: String = row.try_get("context_capture_id").unwrap_or_default();
        let task_type: Option<String> = row.try_get("task_type").ok();
        let ctx_managed_tokens: Option<i64> = row.try_get("total_tokens").ok();
        let assembly_ms: Option<i64> = row.try_get("assembly_time_ms").ok();
        let budget_raw: String = row.try_get("token_budget").unwrap_or_else(|_| "{}".into());
        let relevance_raw: String = row
            .try_get("relevance_scores")
            .unwrap_or_else(|_| "{}".into());
        let llm_response_id: Option<String> = row.try_get("llm_response_id").ok();

        let mut result = serde_json::json!({
            "snapshot_id": snapshot_id,
            "turn": actual_turn,
            "total_turns": total_turns,
            "task_type": task_type,
            "context_managed_tokens": ctx_managed_tokens,
            "assembly_ms": assembly_ms,
        });

        // Layer 1: health, zone_balance, token_breakdown
        if let Ok(budget) = serde_json::from_str::<Value>(&budget_raw) {
            let trend_limit = std::cmp::min(actual_turn as i32, MAX_INTROSPECTION_USAGE_ROWS);
            let trend_rows = query(
                "SELECT event_id, IFNULL(CAST(token_usage AS CHAR), '{}') AS token_usage \
                 FROM agent_events \
                 WHERE session_id = ? AND user_id = ? AND event_type = 'llm_response' AND token_usage IS NOT NULL \
                  ORDER BY created_at ASC LIMIT ?",
            )
            .bind(session_id)
            .bind(user_id)
            .bind(trend_limit)
            .fetch_all(&pool)
            .await
            .map_err(internal_error)?;

            let mut current_usage: Option<Value> = None;
            if let Some(ref resp_id) = llm_response_id {
                for tr in &trend_rows {
                    let eid: String = tr.try_get("event_id").unwrap_or_default();
                    if eid == *resp_id {
                        let raw: String = tr.try_get("token_usage").unwrap_or_default();
                        current_usage = parse_token_usage(&raw);
                        break;
                    }
                }
            }
            if current_usage.is_none()
                && let Some(last) = trend_rows.last()
            {
                let raw: String = last.try_get("token_usage").unwrap_or_default();
                current_usage = parse_token_usage(&raw);
            }

            let trend_prompts: Vec<i64> = trend_rows
                .iter()
                .filter_map(|r| {
                    let raw: String = r.try_get("token_usage").ok()?;
                    let u = parse_token_usage(&raw)?;
                    billable_input_from_canonical(&u)
                })
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

            let reversed_prompts: Vec<i64> = trend_prompts.iter().copied().rev().collect();
            let health = analyze_context_health(
                &budget,
                &reversed_prompts,
                current_prompt,
                current_usage.as_ref(),
                128000,
            );
            result["health"] = serde_json::to_value(&health).unwrap_or_default();
            result["zone_balance"] =
                serde_json::to_value(zone_balance(&budget, task_type.as_deref()))
                    .unwrap_or_default();

            // Token breakdown
            let tool_tokens = budget
                .get("tool_schemas")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let non_tool_tokens: i64 = budget
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
        let scores = parse_relevance_scores(&relevance_raw);
        if !scores.is_empty() {
            result["relevance"] =
                serde_json::to_value(relevance_quality(&scores)).unwrap_or_default();
            result["pollution"] =
                serde_json::to_value(pollution_ratio(&scores)).unwrap_or_default();
        }

        // Layer 2 & 3
        if detail || raw {
            let events: Option<String> = row.try_get("selected_events").ok();
            let code: Option<String> = row.try_get("code_context").ok();
            let skills: Option<String> = row.try_get("skill_definitions").ok();
            let docs: Option<String> = row.try_get("documentation").ok();

            result["contents"] = summarize_contents(
                events.as_deref(),
                code.as_deref(),
                skills.as_deref(),
                docs.as_deref(),
            );

            if raw {
                result["raw"] = raw_contents(
                    events.as_deref(),
                    code.as_deref(),
                    skills.as_deref(),
                    docs.as_deref(),
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
            let raw: String = r.try_get("relevance_scores").unwrap_or_default();
            let scores = parse_relevance_scores(&raw);
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
            .map(|r| {
                let output_json: String = r.try_get("output_json").unwrap_or_else(|_| "{}".into());
                let output: Value =
                    serde_json::from_str(&output_json).unwrap_or(Value::Object(Default::default()));
                serde_json::json!({
                    "decision_id": r.try_get::<String, _>("decision_id").unwrap_or_default(),
                    "event_id": r.try_get::<String, _>("event_id").unwrap_or_default(),
                    "decision_type": r.try_get::<String, _>("decision_type").unwrap_or_default(),
                    "model_used": r.try_get::<Option<String>, _>("model_used").unwrap_or(None),
                    "created_at": r.try_get::<String, _>("created_at").unwrap_or_default(),
                    "output": output,
                })
            })
            .collect();

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
               SUM(CASE WHEN event_type IN ('tool_call', 'tool_error') THEN 1 ELSE 0 END) AS tool_total_calls, \
               SUM(CASE WHEN event_type = 'tool_error' THEN 1 ELSE 0 END) AS tool_fail_count, \
               SUM(CASE WHEN event_type IN ('ask_user_submitted', 'ask_user_cancelled', 'ask_user_timeout', 'ask_user_error', 'ask_user_auto_unanswered', 'ask_user_auto_duplicate') THEN 1 ELSE 0 END) AS ask_user_total_calls, \
               SUM(CASE WHEN event_type = 'ask_user_error' THEN 1 ELSE 0 END) AS ask_user_fail_count \
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

        let tool_total_calls: i64 = agg.try_get("tool_total_calls").unwrap_or(0);
        let tool_fail_count: i64 = agg.try_get("tool_fail_count").unwrap_or(0);
        let ask_user_total_calls: i64 = agg.try_get("ask_user_total_calls").unwrap_or(0);
        let ask_user_fail_count: i64 = agg.try_get("ask_user_fail_count").unwrap_or(0);
        let (total_calls, fail_count) = if is_ask_user && ask_user_total_calls > 0 {
            (ask_user_total_calls, ask_user_fail_count)
        } else {
            (tool_total_calls, tool_fail_count)
        };
        let ok_count = (total_calls - fail_count).max(0);
        let success_rate = if total_calls > 0 {
            (ok_count as f64) / (total_calls as f64)
        } else {
            0.0
        };

        let failures = if is_ask_user && ask_user_total_calls > 0 {
            query(
                "SELECT session_id, \
                        SUBSTRING(COALESCE(CAST(content AS CHAR), ''), 1, 200) AS error_preview, \
                        DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
                 FROM agent_events \
                 WHERE user_id = ? \
                   AND (skill_name = ? OR meta_tool_name = ?) \
                   AND event_type = 'ask_user_error' \
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
            .map(|r| {
                serde_json::json!({
                    "session_id": r.try_get::<String, _>("session_id").unwrap_or_default(),
                    "error_preview": r.try_get::<String, _>("error_preview").unwrap_or_default(),
                    "created_at": r.try_get::<String, _>("created_at").unwrap_or_default(),
                })
            })
            .collect();

        let ask_user_summary = if is_ask_user {
            let rows = query(
                "SELECT session_id, event_type, \
                        SUBSTRING(COALESCE(CAST(content AS CHAR), ''), 1, 200) AS content_preview, \
                        IFNULL(CAST(metadata AS CHAR), '{}') AS metadata_json, \
                        DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
                 FROM agent_events \
                 WHERE user_id = ? \
                   AND (skill_name = ? OR meta_tool_name = ?) \
                   AND event_type IN ('ask_user_prompted', 'ask_user_submitted', 'ask_user_cancelled', 'ask_user_timeout', 'ask_user_error', 'ask_user_auto_unanswered', 'ask_user_auto_duplicate') \
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
                .map(|row| AskUserHistoryRow {
                    session_id: row.try_get::<String, _>("session_id").unwrap_or_default(),
                    event_type: row.try_get::<String, _>("event_type").unwrap_or_default(),
                    created_at: row.try_get::<String, _>("created_at").unwrap_or_default(),
                    metadata: serde_json::from_str(
                        &row.try_get::<String, _>("metadata_json")
                            .unwrap_or_else(|_| "{}".into()),
                    )
                    .unwrap_or(Value::Null),
                    content_preview: row
                        .try_get::<String, _>("content_preview")
                        .unwrap_or_default(),
                })
                .collect::<Vec<_>>();
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

        let original_intent: String = first
            .as_ref()
            .and_then(|r| r.try_get::<String, _>("preview").ok())
            .unwrap_or_default();

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

        let current_focus: String = last
            .as_ref()
            .and_then(|r| r.try_get::<String, _>("preview").ok())
            .unwrap_or_default();

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
        assert_eq!(summary["no_answer_count"], 1);
        assert_eq!(summary["error_count"], 0);
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

    #[test]
    fn ask_user_history_summary_tracks_auto_unanswered_without_marking_error() {
        let rows = vec![
            AskUserHistoryRow {
                session_id: "s-auto".into(),
                event_type: "ask_user_auto_unanswered".into(),
                created_at: "2026-01-01T00:00:00".into(),
                metadata: serde_json::json!({
                    "request_id": "req-auto",
                    "ask_user": {
                        "status": "auto_unanswered",
                        "source": "runtime_policy",
                        "prompt": {
                            "question_count": 1,
                            "headers": ["Decision"],
                            "questions": [{"question": "Which option?"}]
                        }
                    }
                }),
                content_preview: "Which option?".into(),
            },
            AskUserHistoryRow {
                session_id: "s-auto".into(),
                event_type: "ask_user_auto_duplicate".into(),
                created_at: "2026-01-01T00:00:01".into(),
                metadata: serde_json::json!({
                    "request_id": "req-auto-2",
                    "ask_user": {
                        "status": "auto_unanswered_duplicate",
                        "source": "runtime_policy",
                        "prompt": {
                            "question_count": 1,
                            "headers": ["Decision"],
                            "questions": [{"question": "Which option?"}]
                        },
                        "response": {
                            "outcome": "auto_unanswered_duplicate",
                            "answered_question_count": 0,
                            "annotation_count": 0,
                            "freeform_answer_count": 0
                        }
                    }
                }),
                content_preview: "Which option?".into(),
            },
            AskUserHistoryRow {
                session_id: "s-auto".into(),
                event_type: "ask_user_error".into(),
                created_at: "2026-01-01T00:00:02".into(),
                metadata: serde_json::json!({
                    "request_id": "req-error",
                    "ask_user": {
                        "prompt": {"question_count": 1},
                        "response": {
                            "outcome": "interaction_error",
                            "answered_question_count": 0,
                            "annotation_count": 0,
                            "freeform_answer_count": 0
                        }
                    }
                }),
                content_preview: "transport failed".into(),
            },
        ];

        let summary = build_ask_user_history_summary(&rows);
        assert_eq!(summary["interactions_observed"], 3);
        assert_eq!(summary["auto_unanswered_count"], 1);
        assert_eq!(summary["auto_duplicate_count"], 1);
        assert_eq!(summary["no_answer_count"], 2);
        assert_eq!(summary["error_count"], 1);
        assert_eq!(summary["prompt_count"], 0);
        assert_eq!(
            summary["recent_interactions"][0]["outcome"],
            "auto_unanswered"
        );
        assert_eq!(
            summary["recent_interactions"][1]["outcome"],
            "auto_unanswered_duplicate"
        );
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

    // ── Query type serde defaults ───────────────────────────────────────

    #[test]
    fn context_trend_query_defaults() {
        use super::super::ContextTrendQuery;
        let q: ContextTrendQuery = serde_json::from_str(r#"{"session_id":"s1"}"#).unwrap();
        assert_eq!(q.turns, 10);
        assert_eq!(q.context_window, 128000);
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
