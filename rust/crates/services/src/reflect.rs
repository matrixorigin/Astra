use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use mo_agent_core::{
    ErrorResponse, MatrixOneSettings, SharedPool, connect_matrixone, error_response, internal_error,
};
use serde::{Deserialize, Serialize};
use sqlx::{Row, query};

// ── Types ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReflectReport {
    pub session_id: String,
    pub focus: String,
    pub overview: SessionOverview,
    pub insights: Vec<Insight>,
    pub recommendations: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionOverview {
    pub total_events: i64,
    pub total_decisions: i64,
    pub duration_minutes: Option<f64>,
    pub unique_skills_used: i64,
    pub error_count: i64,
    pub error_rate_pct: f64,
    pub top_event_types: Vec<(String, i64)>,
    pub top_skills: Vec<(String, i64)>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Insight {
    pub severity: String,
    pub category: String,
    pub message: String,
    pub evidence: String,
}

/// Intermediate type for error pattern aggregation.
#[derive(Debug, Clone)]
pub struct ErrorPattern {
    pub skill_name: String,
    pub event_type: String,
    pub fail_count: i64,
    pub sample_error: String,
}

/// Intermediate type for decision aggregation.
#[derive(Debug, Clone)]
pub struct DecisionAgg {
    pub decision_type: String,
    pub cnt: i64,
    pub models_used: i64,
}

pub type ServiceResult<T> = Result<T, (StatusCode, Json<ErrorResponse>)>;

// ── Trait ─────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait ReflectService: Send + Sync {
    async fn build_evidence(
        &self,
        user_id: &str,
        session_id: &str,
        focus: &str,
        last_n: i32,
        question: &str,
    ) -> ServiceResult<ReflectReport>;
}

// ── Insight generation (pure logic, no DB) ───────────────────────────────────

pub fn generate_insights(
    overview: &SessionOverview,
    error_patterns: &[ErrorPattern],
    decision_aggs: &[DecisionAgg],
) -> Vec<Insight> {
    let mut insights = Vec::new();

    // High error rate
    if overview.total_events > 0 && overview.error_rate_pct > 30.0 {
        insights.push(Insight {
            severity: "critical".into(),
            category: "error_pattern".into(),
            message: format!(
                "High error rate: {:.0}% of events are errors",
                overview.error_rate_pct
            ),
            evidence: format!(
                "{} errors out of {} events",
                overview.error_count, overview.total_events
            ),
        });
    } else if overview.total_events > 0 && overview.error_rate_pct > 15.0 {
        insights.push(Insight {
            severity: "warning".into(),
            category: "error_pattern".into(),
            message: format!(
                "Elevated error rate: {:.0}% of events are errors",
                overview.error_rate_pct
            ),
            evidence: format!(
                "{} errors out of {} events",
                overview.error_count, overview.total_events
            ),
        });
    }

    // Repeated tool failures
    for ep in error_patterns {
        if ep.fail_count >= 3 {
            insights.push(Insight {
                severity: "warning".into(),
                category: "tool_usage".into(),
                message: format!(
                    "{} failed {} times ({})",
                    ep.skill_name, ep.fail_count, ep.event_type
                ),
                evidence: ep.sample_error.clone(),
            });
        }
    }

    // Single skill dominance (over-reliance)
    if let Some((skill, count)) = overview.top_skills.first() {
        if overview.total_events > 0 {
            let pct = (*count as f64 / overview.total_events as f64) * 100.0;
            if pct > 60.0 {
                insights.push(Insight {
                    severity: "info".into(),
                    category: "tool_usage".into(),
                    message: format!("Over-reliance on {skill}: {pct:.0}% of all events"),
                    evidence: format!("{count} out of {} events", overview.total_events),
                });
            }
        }
    }

    // Multi-model usage
    for da in decision_aggs {
        if da.models_used > 2 && da.cnt >= 5 {
            insights.push(Insight {
                severity: "info".into(),
                category: "performance".into(),
                message: format!(
                    "{} decisions used {} different models",
                    da.decision_type, da.models_used
                ),
                evidence: format!("{} total decisions of this type", da.cnt),
            });
        }
    }

    // Very short session
    if overview.total_events < 5 && overview.total_events > 0 {
        insights.push(Insight {
            severity: "info".into(),
            category: "performance".into(),
            message: "Very short session — limited data for analysis".into(),
            evidence: format!("{} events total", overview.total_events),
        });
    }

    // Empty session
    if overview.total_events == 0 {
        insights.push(Insight {
            severity: "info".into(),
            category: "performance".into(),
            message: "Empty session — no events recorded".into(),
            evidence: "0 events".into(),
        });
    }

    // Long session without decisions (possible stall)
    if overview.total_events > 20 && overview.total_decisions == 0 {
        insights.push(Insight {
            severity: "warning".into(),
            category: "stall".into(),
            message: "Many events but no decision audits — possible routing issue".into(),
            evidence: format!(
                "{} events, 0 decisions",
                overview.total_events
            ),
        });
    }

    insights
}

pub fn generate_recommendations(
    overview: &SessionOverview,
    insights: &[Insight],
) -> Vec<String> {
    let mut recs = Vec::new();

    for insight in insights {
        match (insight.severity.as_str(), insight.category.as_str()) {
            ("critical", "error_pattern") | ("warning", "error_pattern") => {
                recs.push("Investigate recurring errors — check logs for root cause".into());
            }
            (_, "tool_usage") if insight.message.contains("failed") => {
                recs.push(format!(
                    "Check preconditions before tool calls to reduce failures"
                ));
            }
            (_, "tool_usage") if insight.message.contains("Over-reliance") => {
                recs.push("Consider using a wider variety of tools for better coverage".into());
            }
            (_, "stall") => {
                recs.push(
                    "Review agent routing configuration — events without decisions may indicate misconfiguration".into(),
                );
            }
            _ => {}
        }
    }

    // Duration-based recommendation
    if let Some(dur) = overview.duration_minutes {
        if dur > 30.0 && overview.total_events > 100 {
            recs.push("Long session with many events — consider breaking into smaller tasks".into());
        }
    }

    recs.dedup();
    recs
}

// ── Database implementation ──────────────────────────────────────────────────

pub struct DatabaseReflectService {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
}

impl DatabaseReflectService {
    pub fn new(matrixone: MatrixOneSettings) -> Self {
        Self {
            matrixone,
            pool: None,
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
impl ReflectService for DatabaseReflectService {
    async fn build_evidence(
        &self,
        user_id: &str,
        session_id: &str,
        focus: &str,
        _last_n: i32,
        _question: &str,
    ) -> ServiceResult<ReflectReport> {
        let pool = self
            .get_pool()
            .await
            .map_err(|e| internal_error(format!("DB connect: {e}")))?;

        // Verify session ownership
        let owner_check =
            query("SELECT 1 FROM agent_sessions WHERE session_id = ? AND user_id = ? LIMIT 1")
                .bind(session_id)
                .bind(user_id)
                .fetch_optional(&pool)
                .await
                .map_err(|e| internal_error(format!("session check: {e}")))?;

        if owner_check.is_none() {
            return Err(error_response(
                StatusCode::NOT_FOUND,
                "Session not found or not owned by user",
            ));
        }

        // ── Aggregate queries (no raw row fetches) ───────────────────────

        // Overview counts
        let overview_row = query(
            "SELECT \
               COUNT(*) AS total_events, \
               COUNT(DISTINCT skill_name) AS unique_skills, \
               SUM(CASE WHEN event_type = 'error' OR event_type = 'tool_error' THEN 1 ELSE 0 END) AS error_count, \
               CAST(MIN(created_at) AS CHAR) AS first_event, \
               CAST(MAX(created_at) AS CHAR) AS last_event \
             FROM agent_events WHERE session_id = ?",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .map_err(|e| internal_error(format!("overview query: {e}")))?;

        let total_events: i64 = overview_row.try_get("total_events").unwrap_or(0);
        let unique_skills: i64 = overview_row.try_get("unique_skills").unwrap_or(0);
        let error_count: i64 = overview_row.try_get("error_count").unwrap_or(0);
        let first_event: Option<String> = overview_row.try_get("first_event").unwrap_or(None);
        let last_event: Option<String> = overview_row.try_get("last_event").unwrap_or(None);

        // Compute duration in Rust from timestamp strings
        let duration_minutes = compute_duration_minutes(first_event.as_deref(), last_event.as_deref());

        let error_rate_pct = if total_events > 0 {
            (error_count as f64 / total_events as f64) * 100.0
        } else {
            0.0
        };

        // Top event types
        let event_type_rows = query(
            "SELECT event_type, COUNT(*) AS cnt \
             FROM agent_events WHERE session_id = ? \
             GROUP BY event_type ORDER BY cnt DESC LIMIT 5",
        )
        .bind(session_id)
        .fetch_all(&pool)
        .await
        .map_err(|e| internal_error(format!("event types query: {e}")))?;

        let top_event_types: Vec<(String, i64)> = event_type_rows
            .iter()
            .map(|row| {
                let et: String = row.try_get("event_type").unwrap_or_default();
                let cnt: i64 = row.try_get("cnt").unwrap_or(0);
                (et, cnt)
            })
            .collect();

        // Top skills
        let skill_rows = query(
            "SELECT skill_name, COUNT(*) AS cnt \
             FROM agent_events WHERE session_id = ? AND skill_name IS NOT NULL \
             GROUP BY skill_name ORDER BY cnt DESC LIMIT 5",
        )
        .bind(session_id)
        .fetch_all(&pool)
        .await
        .map_err(|e| internal_error(format!("skills query: {e}")))?;

        let top_skills: Vec<(String, i64)> = skill_rows
            .iter()
            .map(|row| {
                let sn: String = row.try_get("skill_name").unwrap_or_default();
                let cnt: i64 = row.try_get("cnt").unwrap_or(0);
                (sn, cnt)
            })
            .collect();

        // Decision aggregation
        let decision_rows = query(
            "SELECT decision_type, COUNT(*) AS cnt, \
               COUNT(DISTINCT model_used) AS models_used \
             FROM ctx_decision_audits WHERE session_id = ? \
             GROUP BY decision_type ORDER BY cnt DESC LIMIT 5",
        )
        .bind(session_id)
        .fetch_all(&pool)
        .await
        .map_err(|e| internal_error(format!("decisions query: {e}")))?;

        let total_decisions: i64 = decision_rows
            .iter()
            .map(|row| row.try_get::<i64, _>("cnt").unwrap_or(0))
            .sum();

        let decision_aggs: Vec<DecisionAgg> = decision_rows
            .iter()
            .map(|row| DecisionAgg {
                decision_type: row.try_get("decision_type").unwrap_or_default(),
                cnt: row.try_get("cnt").unwrap_or(0),
                models_used: row.try_get("models_used").unwrap_or(0),
            })
            .collect();

        // Error patterns (focus-aware: always fetch for auto/skill_failure)
        let error_patterns = if matches!(focus, "auto" | "skill_failure" | "tool_selection") {
            let ep_rows = query(
                "SELECT IFNULL(skill_name, 'unknown') AS skill_name, event_type, COUNT(*) AS fail_count, \
                   SUBSTRING(COALESCE(MIN(content), ''), 1, 100) AS sample_error \
                 FROM agent_events \
                 WHERE session_id = ? AND (event_type LIKE '%error%' OR event_type LIKE '%fail%') \
                 GROUP BY skill_name, event_type \
                 ORDER BY fail_count DESC LIMIT 10",
            )
            .bind(session_id)
            .fetch_all(&pool)
            .await
            .map_err(|e| internal_error(format!("error patterns query: {e}")))?;

            ep_rows
                .iter()
                .map(|row| ErrorPattern {
                    skill_name: row.try_get("skill_name").unwrap_or_default(),
                    event_type: row.try_get("event_type").unwrap_or_default(),
                    fail_count: row.try_get("fail_count").unwrap_or(0),
                    sample_error: row.try_get("sample_error").unwrap_or_default(),
                })
                .collect()
        } else {
            Vec::new()
        };

        // ── Build report ─────────────────────────────────────────────────

        let overview = SessionOverview {
            total_events,
            total_decisions,
            duration_minutes,
            unique_skills_used: unique_skills,
            error_count,
            error_rate_pct,
            top_event_types,
            top_skills,
        };

        let insights = generate_insights(&overview, &error_patterns, &decision_aggs);
        let recommendations = generate_recommendations(&overview, &insights);

        Ok(ReflectReport {
            session_id: session_id.to_string(),
            focus: focus.to_string(),
            overview,
            insights,
            recommendations,
        })
    }
}

/// Parse two datetime strings (e.g. "2026-03-25 08:00:00") and compute
/// the difference in minutes. Returns `None` if either is missing or unparseable.
fn compute_duration_minutes(first: Option<&str>, last: Option<&str>) -> Option<f64> {
    let f = first?.trim();
    let l = last?.trim();
    if f.is_empty() || l.is_empty() {
        return None;
    }
    // Try common datetime formats
    let parse = |s: &str| -> Option<chrono::NaiveDateTime> {
        chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f")
            .or_else(|_| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S"))
            .or_else(|_| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M"))
            .ok()
    };
    let first_dt = parse(f)?;
    let last_dt = parse(l)?;
    let diff = last_dt.signed_duration_since(first_dt);
    Some(diff.num_seconds() as f64 / 60.0)
}

// ── Unconfigured ─────────────────────────────────────────────────────────────

pub struct UnconfiguredReflectService;

#[async_trait]
impl ReflectService for UnconfiguredReflectService {
    async fn build_evidence(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: i32,
        _: &str,
    ) -> ServiceResult<ReflectReport> {
        Err(internal_error("reflect service not configured"))
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_overview(
        total_events: i64,
        error_count: i64,
        top_skills: Vec<(String, i64)>,
        total_decisions: i64,
        duration_minutes: Option<f64>,
    ) -> SessionOverview {
        let error_rate_pct = if total_events > 0 {
            (error_count as f64 / total_events as f64) * 100.0
        } else {
            0.0
        };
        SessionOverview {
            total_events,
            total_decisions,
            duration_minutes,
            unique_skills_used: top_skills.len() as i64,
            error_count,
            error_rate_pct,
            top_event_types: vec![],
            top_skills,
        }
    }

    #[test]
    fn insight_high_error_rate() {
        let overview = make_overview(100, 40, vec![], 5, None);
        let insights = generate_insights(&overview, &[], &[]);
        assert!(insights.iter().any(|i| i.severity == "critical" && i.category == "error_pattern"));
    }

    #[test]
    fn insight_elevated_error_rate() {
        let overview = make_overview(100, 20, vec![], 5, None);
        let insights = generate_insights(&overview, &[], &[]);
        assert!(insights.iter().any(|i| i.severity == "warning" && i.category == "error_pattern"));
    }

    #[test]
    fn insight_no_error_rate_warning_when_low() {
        let overview = make_overview(100, 5, vec![], 5, None);
        let insights = generate_insights(&overview, &[], &[]);
        assert!(!insights.iter().any(|i| i.category == "error_pattern"));
    }

    #[test]
    fn insight_repeated_tool_failure() {
        let overview = make_overview(50, 5, vec![], 5, None);
        let patterns = vec![ErrorPattern {
            skill_name: "bash".into(),
            event_type: "tool_error".into(),
            fail_count: 5,
            sample_error: "permission denied".into(),
        }];
        let insights = generate_insights(&overview, &patterns, &[]);
        assert!(insights.iter().any(|i| i.category == "tool_usage" && i.message.contains("bash")));
    }

    #[test]
    fn insight_no_failure_warning_for_low_count() {
        let overview = make_overview(50, 2, vec![], 5, None);
        let patterns = vec![ErrorPattern {
            skill_name: "bash".into(),
            event_type: "tool_error".into(),
            fail_count: 2,
            sample_error: "not found".into(),
        }];
        let insights = generate_insights(&overview, &patterns, &[]);
        assert!(!insights.iter().any(|i| i.category == "tool_usage" && i.message.contains("bash")));
    }

    #[test]
    fn insight_over_reliance() {
        let overview = make_overview(100, 0, vec![("bash".into(), 75)], 5, None);
        let insights = generate_insights(&overview, &[], &[]);
        assert!(insights.iter().any(|i| i.message.contains("Over-reliance")));
    }

    #[test]
    fn insight_no_over_reliance_when_balanced() {
        let overview = make_overview(100, 0, vec![("bash".into(), 30), ("grep".into(), 25)], 5, None);
        let insights = generate_insights(&overview, &[], &[]);
        assert!(!insights.iter().any(|i| i.message.contains("Over-reliance")));
    }

    #[test]
    fn insight_short_session() {
        let overview = make_overview(3, 0, vec![], 1, None);
        let insights = generate_insights(&overview, &[], &[]);
        assert!(insights.iter().any(|i| i.message.contains("short session")));
    }

    #[test]
    fn insight_empty_session() {
        let overview = make_overview(0, 0, vec![], 0, None);
        let insights = generate_insights(&overview, &[], &[]);
        assert!(insights.iter().any(|i| i.message.contains("Empty session")));
    }

    #[test]
    fn insight_stall_many_events_no_decisions() {
        let overview = make_overview(50, 0, vec![], 0, None);
        let insights = generate_insights(&overview, &[], &[]);
        assert!(insights.iter().any(|i| i.category == "stall"));
    }

    #[test]
    fn insight_100_pct_error_rate() {
        let overview = make_overview(10, 10, vec![], 0, None);
        let insights = generate_insights(&overview, &[], &[]);
        assert!(insights.iter().any(|i| i.severity == "critical" && i.category == "error_pattern"));
    }

    #[test]
    fn insight_huge_session_numbers() {
        let overview = make_overview(100_000, 500, vec![("bash".into(), 40_000)], 5000, Some(120.0));
        let insights = generate_insights(&overview, &[], &[]);
        // Should not panic, error rate is 0.5% so no error insights
        assert!(!insights.iter().any(|i| i.category == "error_pattern"));
    }

    #[test]
    fn recommendations_for_errors() {
        let overview = make_overview(100, 40, vec![], 5, None);
        let insights = generate_insights(&overview, &[], &[]);
        let recs = generate_recommendations(&overview, &insights);
        assert!(recs.iter().any(|r| r.contains("error")));
    }

    #[test]
    fn recommendations_for_tool_failure() {
        let overview = make_overview(50, 5, vec![], 5, None);
        let patterns = vec![ErrorPattern {
            skill_name: "bash".into(),
            event_type: "tool_error".into(),
            fail_count: 5,
            sample_error: "permission denied".into(),
        }];
        let insights = generate_insights(&overview, &patterns, &[]);
        let recs = generate_recommendations(&overview, &insights);
        assert!(recs.iter().any(|r| r.contains("preconditions")));
    }

    #[test]
    fn recommendations_long_session() {
        let overview = make_overview(200, 0, vec![], 10, Some(45.0));
        let recs = generate_recommendations(&overview, &[]);
        assert!(recs.iter().any(|r| r.contains("breaking")));
    }

    #[test]
    fn recommendations_empty_for_clean_session() {
        let overview = make_overview(50, 0, vec![("bash".into(), 20)], 10, Some(5.0));
        let recs = generate_recommendations(&overview, &[]);
        assert!(recs.is_empty());
    }

    #[test]
    fn compute_duration_basic() {
        let d = compute_duration_minutes(
            Some("2026-03-25 08:00:00"),
            Some("2026-03-25 08:18:30"),
        );
        assert!((d.unwrap() - 18.5).abs() < 0.01);
    }

    #[test]
    fn compute_duration_none_on_missing() {
        assert!(compute_duration_minutes(None, Some("2026-03-25 08:00:00")).is_none());
        assert!(compute_duration_minutes(Some("2026-03-25 08:00:00"), None).is_none());
        assert!(compute_duration_minutes(None, None).is_none());
    }

    #[test]
    fn compute_duration_none_on_empty() {
        assert!(compute_duration_minutes(Some(""), Some("")).is_none());
    }

    #[test]
    fn report_serialization_roundtrip() {
        let report = ReflectReport {
            session_id: "test-sess".into(),
            focus: "auto".into(),
            overview: make_overview(10, 1, vec![("bash".into(), 8)], 2, Some(5.0)),
            insights: vec![Insight {
                severity: "info".into(),
                category: "performance".into(),
                message: "test".into(),
                evidence: "test evidence".into(),
            }],
            recommendations: vec!["do something".into()],
        };
        let json = serde_json::to_string(&report).unwrap();
        let parsed: ReflectReport = serde_json::from_str(&json).unwrap();
        assert_eq!(report, parsed);
    }

    /// Validate that all GROUP BY queries only SELECT grouped columns or aggregate functions.
    /// This prevents MatrixOne strict SQL standard errors (MySQL non-strict mode hides these).
    #[test]
    fn sql_group_by_compliance() {
        // All the SQL queries used in build_evidence, extracted as constants for testing.
        let queries = [
            // event types
            "SELECT event_type, COUNT(*) AS cnt \
             FROM agent_events WHERE session_id = ? \
             GROUP BY event_type ORDER BY cnt DESC LIMIT 5",
            // skills
            "SELECT skill_name, COUNT(*) AS cnt \
             FROM agent_events WHERE session_id = ? AND skill_name IS NOT NULL \
             GROUP BY skill_name ORDER BY cnt DESC LIMIT 5",
            // decisions
            "SELECT decision_type, COUNT(*) AS cnt, \
               COUNT(DISTINCT model_used) AS models_used \
             FROM ctx_decision_audits WHERE session_id = ? \
             GROUP BY decision_type ORDER BY cnt DESC LIMIT 5",
            // error patterns
            "SELECT IFNULL(skill_name, 'unknown') AS skill_name, event_type, COUNT(*) AS fail_count, \
               SUBSTRING(COALESCE(MIN(content), ''), 1, 100) AS sample_error \
             FROM agent_events \
             WHERE session_id = ? AND (event_type LIKE '%error%' OR event_type LIKE '%fail%') \
             GROUP BY skill_name, event_type \
             ORDER BY fail_count DESC LIMIT 10",
        ];

        for sql in &queries {
            let upper = sql.to_uppercase();
            if !upper.contains("GROUP BY") {
                continue;
            }
            // Extract GROUP BY columns
            let group_start = upper.find("GROUP BY").unwrap() + 8;
            let group_end = upper[group_start..]
                .find("ORDER BY")
                .or_else(|| upper[group_start..].find("LIMIT"))
                .or_else(|| upper[group_start..].find("HAVING"))
                .map(|i| group_start + i)
                .unwrap_or(upper.len());
            let group_cols: Vec<&str> = upper[group_start..group_end]
                .split(',')
                .map(|s| s.trim())
                .collect();

            // Extract SELECT columns (between SELECT and FROM)
            let sel_start = upper.find("SELECT").unwrap() + 6;
            let sel_end = upper.find("FROM").unwrap();
            // Extract SELECT columns — split by top-level commas only (respect parens)
            let select_part = &upper[sel_start..sel_end];
            let mut select_cols = Vec::new();
            let mut depth = 0;
            let mut start = 0;
            for (i, ch) in select_part.char_indices() {
                match ch {
                    '(' => depth += 1,
                    ')' => depth -= 1,
                    ',' if depth == 0 => {
                        select_cols.push(select_part[start..i].trim());
                        start = i + 1;
                    }
                    _ => {}
                }
            }
            select_cols.push(select_part[start..].trim());

            // Each non-aggregate SELECT column must appear in GROUP BY
            let agg_fns = ["COUNT(", "SUM(", "AVG(", "MIN(", "MAX(", "GROUP_CONCAT("];
            for col in &select_cols {
                let is_agg = agg_fns.iter().any(|f| col.contains(f));
                // Also handle wrapped: SUBSTRING(COALESCE(MIN(...)))
                let has_nested_agg = agg_fns.iter().any(|f| col.contains(f));
                if is_agg || has_nested_agg {
                    continue;
                }
                // Strip AS alias
                let base = if let Some(pos) = col.find(" AS ") {
                    col[..pos].trim()
                } else {
                    col.trim()
                };
                // Handle IFNULL(col, 'x') — extract the column name
                let check_col = if base.starts_with("IFNULL(") {
                    base.trim_start_matches("IFNULL(")
                        .split(',')
                        .next()
                        .unwrap_or(base)
                        .trim()
                } else {
                    base
                };
                assert!(
                    group_cols.iter().any(|g| g.contains(check_col)),
                    "SELECT column '{col}' not in GROUP BY {group_cols:?}\nQuery: {sql}"
                );
            }
        }
    }
}
