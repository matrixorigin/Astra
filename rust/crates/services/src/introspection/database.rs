use async_trait::async_trait;
use axum::http::StatusCode;
use serde_json::Value;
use sqlx::{Row, query};

use astra_core::{
    MatrixOneSettings, SharedPool, connect_matrixone, error_response, internal_error,
};

use super::scoring::{
    DEGRADATION_DELTA, QUALITY_DEGRADED, QUALITY_GOOD, TOKEN_CHAR_RATIO, analyze_context_health,
    compaction_effectiveness, compaction_forecast, compute_trend, memory_recall_final_score,
    memory_recall_score, parse_relevance_scores, parse_token_usage, pollution_ratio,
    relevance_quality, zone_balance,
};
use super::{
    EpisodicStats, IntrospectionService, MemoryIntrospectionResponse, ProceduralStats,
    SemanticStats, ServiceResult, SkillInfo, SkillsIntrospectionResponse,
};

const MAX_INTROSPECTION_SNAPSHOTS: i32 = 128;
const MAX_INTROSPECTION_USAGE_ROWS: i32 = 128;
const MAX_MEMORY_RECALL_RESULTS: i32 = 50;

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
        let row = query("SELECT user_id FROM agent_sessions WHERE session_id = ?")
            .bind(session_id)
            .fetch_optional(pool)
            .await
            .map_err(internal_error)?;

        match row {
            Some(r) => {
                let owner: String = r.try_get("user_id").map_err(internal_error)?;
                if owner != user_id {
                    Err(error_response(StatusCode::NOT_FOUND, "Session not found"))
                } else {
                    Ok(())
                }
            }
            None => Err(error_response(StatusCode::NOT_FOUND, "Session not found")),
        }
    }
    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.pool = Some(pool);
        self
    }

    async fn get_pool(&self) -> Result<sqlx::Pool<sqlx::MySql>, sqlx::Error> {
        if let Some(ref p) = self.pool {
            return Ok(p.get().clone());
        }
        connect_matrixone(&self.matrixone).await
    }
}

#[async_trait]
impl IntrospectionService for DatabaseIntrospectionService {
    async fn get_memory_introspection(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> ServiceResult<MemoryIntrospectionResponse> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        self.verify_session_owner(&pool, session_id, user_id)
            .await?;

        // Episodic stats
        let episodic = {
            let row = query(
                "SELECT COUNT(*) AS total, \
                 SUM(CASE WHEN event_type = 'user_query' THEN 1 ELSE 0 END) AS user_queries, \
                 SUM(CASE WHEN event_type IN ('tool_call','tool_result') THEN 1 ELSE 0 END) AS tool_calls \
                 FROM agent_events WHERE session_id = ?",
            )
            .bind(session_id)
            .fetch_one(&pool)
            .await
            .map_err(internal_error)?;

            let total: i64 = row.try_get("total").unwrap_or(0);
            let turns: i64 = row.try_get("user_queries").unwrap_or(0);
            let tool_calls: i64 = row.try_get("tool_calls").unwrap_or(0);

            let tool_ratio = tool_calls as f64 / total.max(1) as f64;
            let tool_intensity = if tool_ratio > 0.5 {
                "high"
            } else if tool_ratio > 0.2 {
                "medium"
            } else {
                "low"
            };
            let session_depth = if turns >= 10 {
                "deep"
            } else if turns >= 4 {
                "moderate"
            } else {
                "shallow"
            };

            EpisodicStats {
                turns,
                total_events: total,
                tool_intensity: tool_intensity.into(),
                session_depth: session_depth.into(),
            }
        };

        // Semantic stats
        let semantic = {
            let snap_rows = query(
                "SELECT token_budget, total_tokens, assembly_time_ms, created_at \
                 FROM ctx_snapshots WHERE session_id = ? ORDER BY created_at DESC LIMIT ?",
            )
            .bind(session_id)
            .bind(MAX_INTROSPECTION_SNAPSHOTS)
            .fetch_all(&pool)
            .await
            .map_err(internal_error)?;

            let count = snap_rows.len() as i64;
            let token_history: Vec<i64> = snap_rows
                .iter()
                .filter_map(|r| r.try_get::<i64, _>("total_tokens").ok())
                .collect();
            let peak = token_history.iter().copied().max().unwrap_or(0);

            let mut stats = SemanticStats {
                ctx_snapshots: count,
                peak_tokens: peak,
                context_managed_tokens: None,
                last_assembly_ms: None,
                llm_prompt_tokens: None,
                llm_completion_tokens: None,
                llm_total_tokens: None,
                health: None,
            };

            if let Some(latest) = snap_rows.first() {
                stats.context_managed_tokens = latest.try_get("total_tokens").ok();
                stats.last_assembly_ms = latest.try_get("assembly_time_ms").ok();

                let usage_rows = query(
                    "SELECT IFNULL(CAST(token_usage AS CHAR), '{}') AS token_usage \
                      FROM agent_events \
                      WHERE session_id = ? AND event_type = 'llm_response' \
                        AND token_usage IS NOT NULL \
                      ORDER BY created_at DESC LIMIT ?",
                )
                .bind(session_id)
                .bind(MAX_INTROSPECTION_USAGE_ROWS)
                .fetch_all(&pool)
                .await
                .map_err(internal_error)?;

                let mut llm_usage_val: Option<Value> = None;
                if let Some(first) = usage_rows.first() {
                    let raw: String = first.try_get("token_usage").unwrap_or_default();
                    llm_usage_val = parse_token_usage(&raw);
                }

                if let Some(ref usage) = llm_usage_val {
                    stats.llm_prompt_tokens = usage.get("prompt").and_then(|v| v.as_i64());
                    stats.llm_completion_tokens = usage.get("completion").and_then(|v| v.as_i64());
                    stats.llm_total_tokens = usage.get("total").and_then(|v| v.as_i64());
                }

                let budget_raw: Option<String> = latest.try_get("token_budget").ok();
                if let Some(ref budget_str) = budget_raw
                    && let Ok(budget) = serde_json::from_str::<Value>(budget_str)
                {
                    let prompt_history: Vec<i64> = usage_rows
                        .iter()
                        .filter_map(|r| {
                            let raw: String = r.try_get("token_usage").ok()?;
                            let u = parse_token_usage(&raw)?;
                            u.get("prompt")?.as_i64()
                        })
                        .collect();

                    let health = analyze_context_health(
                        &budget,
                        &prompt_history,
                        None,
                        llm_usage_val.as_ref(),
                        128000,
                    );
                    stats.health = serde_json::to_value(&health).ok();
                }
            }

            stats
        };

        // Procedural stats
        let procedural = {
            let row = query(
                "SELECT COUNT(*) AS total, \
                 SUM(CASE WHEN user_feedback_score > 0 THEN 1 ELSE 0 END) AS positive \
                 FROM skill_selection_events WHERE session_id = ?",
            )
            .bind(session_id)
            .fetch_one(&pool)
            .await
            .map_err(internal_error)?;

            let total: i64 = row.try_get("total").unwrap_or(0);
            let positive: i64 = row.try_get("positive").unwrap_or(0);
            let accuracy = if total >= 10 {
                Some(((positive as f64 / total as f64) * 100.0).round() / 100.0)
            } else {
                None
            };

            ProceduralStats {
                skill_selections: total,
                accuracy_rate: accuracy,
            }
        };

        // Profile memories
        let profile = {
            let rows = query(
                "SELECT SUBSTRING(CAST(content AS CHAR), 1, 8192) AS content FROM mem_memories \
                 WHERE user_id = ? AND is_active = 1 AND memory_type = 'profile' \
                 ORDER BY updated_at DESC LIMIT 20",
            )
            .bind(user_id)
            .fetch_all(&pool)
            .await
            .ok();

            rows.and_then(|rs| {
                if rs.is_empty() {
                    None
                } else {
                    Some(
                        rs.iter()
                            .filter_map(|r| r.try_get::<String, _>("content").ok())
                            .collect(),
                    )
                }
            })
        };

        Ok(MemoryIntrospectionResponse {
            episodic,
            semantic,
            procedural,
            profile,
        })
    }

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

        let rows = query(
            "SELECT IFNULL(CAST(e.token_usage AS CHAR), '{}') AS token_usage \
             FROM ctx_snapshots s \
             JOIN agent_events e ON e.event_id = s.llm_response_id \
             WHERE s.session_id = ? AND s.llm_response_id IS NOT NULL AND e.token_usage IS NOT NULL \
             ORDER BY s.created_at DESC, s.context_capture_id DESC LIMIT ?",
        )
        .bind(session_id)
        .bind(turns)
        .fetch_all(&pool)
        .await
        .map_err(internal_error)?;

        if rows.is_empty() {
            return Ok(serde_json::json!({"turns_sampled": 0, "trend": "no_data"}));
        }

        let usages: Vec<Value> = rows
            .iter()
            .filter_map(|r| {
                let raw: String = r.try_get("token_usage").ok()?;
                parse_token_usage(&raw)
            })
            .collect();

        let prompt_history: Vec<i64> = usages
            .iter()
            .filter_map(|u| u.get("prompt")?.as_i64())
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
                    "prompt": u.get("prompt"),
                    "completion": u.get("completion"),
                    "total": u.get("total"),
                })
            })
            .collect();

        let cw = context_window.max(1);
        let current_prompt = current.get("prompt").and_then(|v| v.as_i64()).unwrap_or(0);

        Ok(serde_json::json!({
            "turns_sampled": rows.len(),
            "trend": trend,
            "current_tokens": {
                "prompt": current.get("prompt"),
                "completion": current.get("completion"),
                "total": current.get("total"),
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

        let total_turns: i64 =
            query("SELECT COUNT(*) AS cnt FROM ctx_snapshots WHERE session_id = ?")
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
            ", selected_events, code_context, skill_definitions, documentation"
        } else {
            ""
        };

        let sql = format!(
            "SELECT context_capture_id, \
                    IFNULL(CAST(token_budget AS CHAR), '{{}}') AS token_budget, \
                    total_tokens, assembly_time_ms, \
                    IFNULL(CAST(relevance_scores AS CHAR), '{{}}') AS relevance_scores, \
                    task_type, llm_response_id{content_cols} \
             FROM ctx_snapshots \
             WHERE session_id = ? \
             ORDER BY created_at ASC, context_capture_id ASC \
             LIMIT 1 OFFSET ?"
        );

        let row = query(&sql)
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
                 WHERE session_id = ? AND event_type = 'llm_response' AND token_usage IS NOT NULL \
                  ORDER BY created_at ASC LIMIT ?",
            )
            .bind(session_id)
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
                    u.get("prompt")?.as_i64()
                })
                .collect();

            let current_prompt = current_usage
                .as_ref()
                .and_then(|u| u.get("prompt")?.as_i64());

            if let Some(ref cu) = current_usage {
                result["llm_prompt_tokens"] =
                    serde_json::json!(cu.get("prompt").and_then(|v| v.as_i64()));
                result["llm_completion_tokens"] =
                    serde_json::json!(cu.get("completion").and_then(|v| v.as_i64()));
                result["llm_total_tokens"] =
                    serde_json::json!(cu.get("total").and_then(|v| v.as_i64()));
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
            "SELECT IFNULL(CAST(relevance_scores AS CHAR), '{}') AS relevance_scores \
             FROM ctx_snapshots WHERE session_id = ? ORDER BY created_at DESC LIMIT ?",
        )
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

    async fn get_memory_recall(
        &self,
        user_id: &str,
        session_id: &str,
        query_str: &str,
        task_hint: &str,
        limit: i32,
    ) -> ServiceResult<Value> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        self.verify_session_owner(&pool, session_id, user_id)
            .await?;
        let limit = limit.clamp(1, MAX_MEMORY_RECALL_RESULTS);

        let terms: Vec<&str> = query_str
            .split_whitespace()
            .filter(|t| !t.is_empty())
            .collect();

        let rows = query(
            "SELECT memory_id, SUBSTRING(CAST(content AS CHAR), 1, 32768) AS content, \
                    initial_confidence, observed_at, created_at \
             FROM mem_memories \
             WHERE user_id = ? AND is_active = 1 \
             ORDER BY COALESCE(observed_at, created_at) DESC LIMIT 200",
        )
        .bind(user_id)
        .fetch_all(&pool)
        .await;

        let rows = match rows {
            Ok(r) => r,
            Err(_) => {
                return Ok(empty_memory_recall(query_str, task_hint));
            }
        };

        if rows.is_empty() {
            return Ok(empty_memory_recall(query_str, task_hint));
        }

        let now = chrono::Utc::now();
        let mut ranking: Vec<Value> = Vec::new();

        for row in &rows {
            let memory_id: String = row.try_get("memory_id").unwrap_or_default();
            let content: String = row.try_get("content").unwrap_or_default();
            let confidence: f64 = row.try_get("initial_confidence").unwrap_or(0.0);

            let observed_str: Option<String> = row
                .try_get("observed_at")
                .ok()
                .or_else(|| row.try_get("created_at").ok());

            let age_days = observed_str
                .and_then(|s| {
                    chrono::NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M:%S")
                        .ok()
                        .or_else(|| {
                            chrono::NaiveDateTime::parse_from_str(&s, "%Y-%m-%dT%H:%M:%S").ok()
                        })
                })
                .map(|dt| {
                    let aware = dt.and_utc();
                    (now - aware).num_seconds().max(0) as f64 / 86400.0
                })
                .unwrap_or(15.0);

            let breakdown = memory_recall_score(&content, &terms, confidence, age_days);
            let final_score = memory_recall_final_score(&content, &terms, confidence, age_days);

            ranking.push(serde_json::json!({
                "memory_id": memory_id,
                "final_score": final_score,
                "scores": breakdown,
            }));
        }

        ranking.sort_by(|a, b| {
            let sa = a.get("final_score").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let sb = b.get("final_score").and_then(|v| v.as_f64()).unwrap_or(0.0);
            sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
        });
        ranking.truncate(limit as usize);

        for (idx, item) in ranking.iter_mut().enumerate() {
            item.as_object_mut()
                .map(|o| o.insert("rank".into(), serde_json::json!(idx + 1)));
        }

        Ok(serde_json::json!({
            "query": query_str,
            "task_hint": task_hint,
            "retrieved_count": ranking.len(),
            "total_ms": 0,
            "phases": {
                "keyword": {"candidates": rows.len(), "ms": 0},
                "vector": {"candidates": rows.len(), "ms": 0},
                "merge": {"candidates": ranking.len(), "ms": 0},
            },
            "ranking": ranking,
        }))
    }
}

fn empty_memory_recall(query_str: &str, task_hint: &str) -> Value {
    serde_json::json!({
        "query": query_str,
        "task_hint": task_hint,
        "retrieved_count": 0,
        "total_ms": 0,
        "phases": {
            "keyword": {"candidates": 0, "ms": 0},
            "vector": {"candidates": 0, "ms": 0},
            "merge": {"candidates": 0, "ms": 0},
        },
        "ranking": [],
    })
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

    // ── empty_memory_recall ──────────────────────────────────────────────

    #[test]
    fn empty_memory_recall_structure() {
        let result = empty_memory_recall("test query", "code_gen");
        assert_eq!(result["query"], "test query");
        assert_eq!(result["task_hint"], "code_gen");
        assert_eq!(result["retrieved_count"], 0);
        assert!(result["ranking"].as_array().unwrap().is_empty());
        assert_eq!(result["phases"]["keyword"]["candidates"], 0);
    }

    #[test]
    fn empty_memory_recall_empty_strings() {
        let result = empty_memory_recall("", "");
        assert_eq!(result["query"], "");
        assert_eq!(result["task_hint"], "");
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

    // ── UnconfiguredIntrospectionService ─────────────────────────────────

    #[tokio::test]
    async fn unconfigured_service_returns_errors() {
        use super::super::{IntrospectionService, UnconfiguredIntrospectionService};
        let svc = UnconfiguredIntrospectionService;
        assert!(svc.get_memory_introspection("u1", "s1").await.is_err());
        assert!(svc.get_skills_introspection("u1").await.is_err());
        assert!(svc.get_context_trend("u1", "s1", 10, 128000).await.is_err());
        assert!(
            svc.get_context_snapshot("u1", "s1", None, false, false, 2000)
                .await
                .is_err()
        );
        assert!(svc.get_retrieval_quality("u1", "s1", 5).await.is_err());
        assert!(
            svc.get_memory_recall("u1", "s1", "q", "hint", 10)
                .await
                .is_err()
        );
    }

    // ── Query type serde defaults ───────────────────────────────────────

    #[test]
    fn context_trend_query_defaults() {
        use super::super::ContextTrendQuery;
        let q: ContextTrendQuery =
            serde_json::from_str(r#"{"session_id":"s1"}"#).unwrap();
        assert_eq!(q.turns, 10);
        assert_eq!(q.context_window, 128000);
    }

    #[test]
    fn context_snapshot_query_defaults() {
        use super::super::ContextSnapshotQuery;
        let q: ContextSnapshotQuery =
            serde_json::from_str(r#"{"session_id":"s1"}"#).unwrap();
        assert!(!q.detail);
        assert!(!q.raw);
        assert_eq!(q.raw_token_budget, 2000);
        assert!(q.turn_index.is_none());
    }

    #[test]
    fn retrieval_quality_query_defaults() {
        use super::super::RetrievalQualityQuery;
        let q: RetrievalQualityQuery =
            serde_json::from_str(r#"{"session_id":"s1"}"#).unwrap();
        assert_eq!(q.turns, 5);
    }

    #[test]
    fn memory_recall_query_defaults() {
        use super::super::MemoryRecallQuery;
        let q: MemoryRecallQuery =
            serde_json::from_str(r#"{"session_id":"s1","query":"test"}"#).unwrap();
        assert_eq!(q.task_hint, "default");
        assert_eq!(q.limit, 10);
    }
}
