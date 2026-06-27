use std::collections::BTreeSet;

use astra_core::{
    ErrorResponse, MatrixOneSettings, ObservationActionHint, ObservationBudgetOmitted,
    ObservationBudgetResult, ObservationConfidence, ObservationDataCoverage, ObservationEvidence,
    ObservationFailureCluster, ObservationGraphEdgeKind, ObservationGraphLayer,
    ObservationGraphNode, ObservationGraphNodeKind, ObservationGraphSlice, ObservationRecord,
    ObservationView, SharedPool, classify_event_kind, error_response, internal_error,
    push_graph_edge, push_graph_node, truncate_graph_summary,
};
use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use sqlx::{Row, query};

mod observation;
mod request;
use observation::{build_observation_envelope, graph_decision_ref, graph_event_ref};
pub use request::ReflectRequest;

// ── Types ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReflectReport {
    pub schema_version: u32,
    pub tool: String,
    pub session_id: String,
    pub analysis_view: String,
    pub topic: String,
    pub facet: String,
    pub depth: String,
    pub horizon: String,
    pub source_policy: String,
    pub include_context: bool,
    pub data_coverage: ObservationDataCoverage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<ObservationView>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observations: Vec<ObservationRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<ObservationEvidence>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub action_hints: Vec<ObservationActionHint>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failure_clusters: Vec<ObservationFailureCluster>,
    #[serde(default)]
    pub graph_slice: ObservationGraphSlice,
    #[serde(default)]
    pub budget_result: ObservationBudgetResult,
    pub overview: SessionOverview,
    /// Root-cause diagnoses from actual error content analysis
    pub diagnoses: Vec<Diagnosis>,
    /// Statistical insights (secondary)
    pub insights: Vec<Insight>,
    pub recommendations: Vec<String>,
}

// Type aliases removed — use Observation* types from astra_core directly

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

/// A root-cause diagnosis: what actually went wrong, from error content analysis.
///
/// `category` uses the canonical [`astra_core::ErrorKind`] taxonomy. Before
/// P0.1 we kept a parallel `ErrorClass` enum here; that duplication caused
/// ambiguity for any consumer that also looked at upstream `ErrorKind`
/// tags. Single source of truth now lives in `astra_core`.
///
/// **Wire format change**: `category` serializes as `snake_case`
/// (`"resource_limit"`) instead of the old `PascalCase` (`"ResourceLimit"`).
/// This is intentional — all enum tags in the codebase use snake_case.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Diagnosis {
    pub category: astra_core::ErrorKind,
    pub severity: String,
    pub summary: String,
    /// Actual error content snippets (evidence)
    pub samples: Vec<String>,
    pub occurrences: i64,
    pub affected_tool: String,
    pub fix_hint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Insight {
    pub severity: String,
    pub category: String,
    pub message: String,
    pub evidence: String,
}

/// Raw error record fetched from DB for content analysis.
#[derive(Debug, Clone)]
pub struct RawError {
    skill_name: String,
    event_type: String,
    content: String,
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

#[derive(Debug, Clone)]
struct EvidenceDecision {
    decision_id: String,
    event_id: String,
    decision_type: String,
    decision_output: serde_json::Value,
    created_at: String,
}

#[derive(Debug, Clone)]
struct EvidenceEvent {
    event_id: String,
    event_type: String,
    content: String,
    skill_name: Option<String>,
    parent_event_id: Option<String>,
    causal_chain_id: Option<String>,
    created_at: String,
}

fn build_evidence_graph(
    decisions: &[EvidenceDecision],
    events: &[EvidenceEvent],
    parent_id_map: &std::collections::HashMap<String, Vec<String>>,
) -> Option<ObservationGraphSlice> {
    if decisions.is_empty() && events.is_empty() {
        return None;
    }

    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let mut event_node_ids = std::collections::HashSet::new();
    let mut event_refs = std::collections::HashMap::new();
    let mut decision_refs = std::collections::HashMap::new();
    let mut edge_keys = BTreeSet::new();
    let event_ids: std::collections::HashSet<&str> =
        events.iter().map(|event| event.event_id.as_str()).collect();

    for decision in decisions {
        let ref_id = graph_decision_ref(&decision.decision_id);
        decision_refs.insert(decision.decision_id.clone(), ref_id.clone());
        nodes.push(ObservationGraphNode {
            ref_id,
            layer: ObservationGraphLayer::Runtime,
            kind: ObservationGraphNodeKind::Decision,
            label: decision.decision_type.clone(),
            summary: truncate_graph_summary(&decision.decision_output.to_string(), 140),
            metadata: Some(serde_json::json!({
                "decision_id": decision.decision_id,
                "event_id": decision.event_id,
                "decision_output": decision.decision_output,
                "created_at": decision.created_at,
            })),
        });
    }

    for event in events {
        let ref_id = graph_event_ref(&event.event_id);
        event_node_ids.insert(event.event_id.clone());
        event_refs.insert(event.event_id.clone(), ref_id.clone());
        nodes.push(ObservationGraphNode {
            ref_id,
            layer: ObservationGraphLayer::Runtime,
            kind: classify_event_kind(&event.event_type),
            label: event.event_type.clone(),
            summary: truncate_graph_summary(&event.content, 140),
            metadata: Some(serde_json::json!({
                "event_id": event.event_id,
                "skill_name": event.skill_name,
                "causal_chain_id": event.causal_chain_id,
                "created_at": event.created_at,
            })),
        });
    }

    for decision in decisions {
        if event_node_ids.contains(&decision.event_id)
            && let (Some(from), Some(to)) = (
                event_refs.get(decision.event_id.as_str()),
                decision_refs.get(decision.decision_id.as_str()),
            )
        {
            push_graph_edge(
                &mut edges,
                &mut edge_keys,
                from.clone(),
                to.clone(),
                ObservationGraphEdgeKind::Supports,
            );
        }
    }

    for event in events {
        let full_parent_ids = crate::storage::normalized_parent_event_ids(
            event.parent_event_id.as_deref(),
            parent_id_map.get(&event.event_id).map(Vec::as_slice),
        );

        for parent_event_id in full_parent_ids {
            if event_ids.contains(parent_event_id.as_str())
                && let (Some(from), Some(to)) = (
                    event_refs.get(parent_event_id.as_str()),
                    event_refs.get(event.event_id.as_str()),
                )
            {
                push_graph_edge(
                    &mut edges,
                    &mut edge_keys,
                    from.clone(),
                    to.clone(),
                    ObservationGraphEdgeKind::Causes,
                );
            }

            for decision in decisions {
                if decision.event_id == parent_event_id
                    && let (Some(from), Some(to)) = (
                        decision_refs.get(decision.decision_id.as_str()),
                        event_refs.get(event.event_id.as_str()),
                    )
                {
                    push_graph_edge(
                        &mut edges,
                        &mut edge_keys,
                        from.clone(),
                        to.clone(),
                        ObservationGraphEdgeKind::Causes,
                    );
                }
            }
        }
    }

    Some(ObservationGraphSlice {
        nodes,
        edges,
        budget_result: ObservationBudgetResult::default(),
    })
}
/// Request for LLM-powered single-turn analysis.
#[derive(Debug, Clone)]
pub struct TurnAnalysisRequest {
    pub session_id: String,
    pub turn: u32,
    pub question: String,
}

/// Result of LLM-powered turn analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnAnalysisReport {
    pub session_id: String,
    pub turn: u32,
    pub question: String,
    /// LLM-generated root cause analysis.
    pub diagnosis: String,
    /// Specific recommendations from the LLM.
    pub recommendations: Vec<String>,
    /// Tool surface quality assessment.
    pub tool_surface_quality: Option<String>,
    /// Data sources used for the analysis.
    pub data_sources: Vec<String>,
}

pub type ServiceResult<T> = Result<T, (StatusCode, Json<ErrorResponse>)>;

// ── Trait ─────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait ReflectService: Send + Sync {
    async fn build_evidence(
        &self,
        user_id: &str,
        session_id: &str,
        request: ReflectRequest,
    ) -> ServiceResult<ReflectReport>;
}

// ── Error classification (pure logic, no DB) ─────────────────────────────────

/// Sole entry point for turning a `(content, event_type)` pair into an
/// [`astra_core::ErrorKind`]. `event_type == "stall_detected"` short-circuits
/// to [`ErrorKind::Stall`]; everything else delegates to
/// [`astra_core::classify_tool_output`].
pub fn classify_error(content: &str, event_type: &str) -> astra_core::ErrorKind {
    if event_type == "stall_detected" {
        return astra_core::ErrorKind::Stall;
    }
    astra_core::classify_tool_output(content)
}

/// Build diagnoses from raw error records by classifying and grouping.
pub fn build_diagnoses(raw_errors: &[RawError]) -> Vec<Diagnosis> {
    use astra_core::ErrorKind;
    use std::collections::HashMap;

    let mut groups: HashMap<(ErrorKind, String), Vec<&RawError>> = HashMap::new();
    for err in raw_errors {
        let kind = classify_error(&err.content, &err.event_type);
        let tool = if err.skill_name.is_empty() || err.skill_name == "unknown" {
            "system".to_string()
        } else {
            err.skill_name.clone()
        };
        groups.entry((kind, tool)).or_default().push(err);
    }

    let mut diagnoses: Vec<Diagnosis> = groups
        .into_iter()
        .map(|((kind, tool), errors)| {
            let count = errors.len() as i64;
            let mut seen = std::collections::HashSet::new();
            let samples: Vec<String> = errors
                .iter()
                .filter_map(|e| {
                    let snippet: String = e.content.chars().take(150).collect();
                    if seen.insert(snippet.clone()) {
                        Some(snippet)
                    } else {
                        None
                    }
                })
                .take(3)
                .collect();

            Diagnosis {
                category: kind,
                severity: severity_for(kind, count).to_string(),
                summary: summary_for(kind, &tool, count),
                samples,
                occurrences: count,
                affected_tool: tool,
                fix_hint: kind.diagnosis_hint().to_string(),
            }
        })
        .collect();

    diagnoses.sort_by(|a, b| {
        let sev_ord = |s: &str| match s {
            "critical" => 0,
            "warning" => 1,
            _ => 2,
        };
        sev_ord(&a.severity)
            .cmp(&sev_ord(&b.severity))
            .then(b.occurrences.cmp(&a.occurrences))
    });

    diagnoses
}

fn severity_for(kind: astra_core::ErrorKind, count: i64) -> &'static str {
    use astra_core::ErrorKind as K;
    match (kind, count) {
        // Always critical — system-level or data-integrity
        (K::ResourceLimit | K::DatabaseError, _) => "critical",
        // Stall ramps with repetition
        (K::Stall, n) if n >= 3 => "warning",
        (K::Stall, _) => "info",
        (K::MissingModelSelection, _) => "warning",
        // Generic count-based escalation
        (_, n) if n >= 5 => "critical",
        (_, n) if n >= 3 => "warning",
        _ => "info",
    }
}

fn summary_for(kind: astra_core::ErrorKind, tool: &str, count: i64) -> String {
    use astra_core::ErrorKind as K;
    match kind {
        K::ResourceLimit => format!(
            "System resource exhaustion ({tool}): OS cannot fork/allocate — {count} occurrences"
        ),
        K::Auth => format!(
            "Authentication failure ({tool}): credentials invalid or expired — {count} occurrences"
        ),
        K::Network | K::RateLimit | K::ServerError | K::StreamIdle | K::StreamTransport => {
            format!(
                "Network/provider issue ({tool}): connection or upstream failure — {count} occurrences"
            )
        }
        K::ToolTimeout => {
            format!("Timeout ({tool}): operation exceeded time limit — {count} occurrences")
        }
        K::ToolNotFound => format!(
            "Missing files/paths ({tool}): agent tried nonexistent paths — {count} occurrences"
        ),
        K::ToolInvalidArgs | K::InvalidRequest => {
            format!("Tool parameter errors ({tool}): wrong arguments passed — {count} occurrences")
        }
        K::ToolUnavailable => {
            format!("Tool not available in this environment ({tool}) — {count} occurrences")
        }
        K::ToolBinding => {
            format!(
                "Tool binding mismatch ({tool}): advertised without executor/transport — {count} occurrences"
            )
        }
        K::ContextWindow => format!(
            "Prompt exceeded context window ({tool}): compact history or switch model — {count} occurrences"
        ),
        K::BudgetExhausted => {
            format!("Turn/session budget exhausted ({tool}): {count} occurrences")
        }
        K::ToolRoundsExhausted => format!("Tool-round cap hit ({tool}): {count} occurrences"),
        K::DatabaseError => {
            format!("Database error ({tool}): SQL or pool failure — {count} occurrences")
        }
        K::Stall => {
            format!("Agent stall detected — {count} stall events, agent may be looping or stuck")
        }
        K::MissingModelSelection => format!(
            "Missing model selection ({tool}): choose a concrete model before starting a turn — {count} occurrences"
        ),
        K::Cancelled => format!("Cancelled operations ({tool}): {count} occurrences"),
        K::Unknown => format!("Unclassified errors ({tool}): {count} occurrences"),
    }
}

// ── Statistical insights (secondary) ─────────────────────────────────────────

pub fn generate_insights(
    overview: &SessionOverview,
    error_patterns: &[ErrorPattern],
    decision_aggs: &[DecisionAgg],
) -> Vec<Insight> {
    let mut insights = Vec::new();

    if overview.total_events > 0 && overview.error_rate_pct > 30.0 {
        insights.push(Insight {
            severity: "critical".into(),
            category: "error_pattern".into(),
            message: format!("High error rate: {:.0}%", overview.error_rate_pct),
            evidence: format!("{}/{} events", overview.error_count, overview.total_events),
        });
    } else if overview.total_events > 0 && overview.error_rate_pct > 15.0 {
        insights.push(Insight {
            severity: "warning".into(),
            category: "error_pattern".into(),
            message: format!("Elevated error rate: {:.0}%", overview.error_rate_pct),
            evidence: format!("{}/{} events", overview.error_count, overview.total_events),
        });
    }

    for ep in error_patterns {
        if ep.fail_count >= 3 {
            insights.push(Insight {
                severity: "warning".into(),
                category: "tool_usage".into(),
                message: format!("{} failed {} times", ep.skill_name, ep.fail_count),
                evidence: ep.sample_error.clone(),
            });
        }
    }

    if let Some((skill, count)) = overview.top_skills.first()
        && overview.total_events > 0
    {
        let pct = (*count as f64 / overview.total_events as f64) * 100.0;
        if pct > 60.0 {
            insights.push(Insight {
                severity: "info".into(),
                category: "tool_usage".into(),
                message: format!("Over-reliance on {skill}: {pct:.0}%"),
                evidence: format!("{count}/{}", overview.total_events),
            });
        }
    }

    for da in decision_aggs {
        if da.models_used > 2 && da.cnt >= 5 {
            insights.push(Insight {
                severity: "info".into(),
                category: "performance".into(),
                message: format!("{} used {} models", da.decision_type, da.models_used),
                evidence: format!("{} decisions", da.cnt),
            });
        }
    }

    if overview.total_events == 0 {
        insights.push(Insight {
            severity: "info".into(),
            category: "performance".into(),
            message: "Empty session — no events recorded".into(),
            evidence: "0 events".into(),
        });
    }

    if overview.total_events > 20 && overview.total_decisions == 0 {
        insights.push(Insight {
            severity: "warning".into(),
            category: "stall".into(),
            message: "Many events but no decisions — possible routing issue".into(),
            evidence: format!("{} events, 0 decisions", overview.total_events),
        });
    }

    insights
}

pub fn generate_recommendations(
    overview: &SessionOverview,
    diagnoses: &[Diagnosis],
    insights: &[Insight],
) -> Vec<String> {
    let mut recs = Vec::new();

    // Priority: diagnoses first (specific, actionable)
    for d in diagnoses {
        let fix_hint = d.fix_hint.trim();
        if (d.severity == "critical" || d.severity == "warning") && !fix_hint.is_empty() {
            recs.push(fix_hint.to_string());
        }
    }

    // Then generic insight recommendations
    for insight in insights {
        match (insight.severity.as_str(), insight.category.as_str()) {
            (_, "tool_usage") if insight.message.contains("Over-reliance") => {
                recs.push("Consider using more diverse tools for better coverage".into());
            }
            (_, "stall") => {
                recs.push(
                    "Review agent routing — events without decisions may be misconfigured".into(),
                );
            }
            _ => {}
        }
    }

    if let Some(dur) = overview.duration_minutes
        && dur > 30.0
        && overview.total_events > 100
    {
        recs.push("Long session — consider breaking into smaller tasks".into());
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
        crate::require_shared_pool(
            self.pool.as_ref(),
            "DatabaseReflectService",
            &self.matrixone,
        )
    }

    async fn build_recent_evidence_graph(
        &self,
        pool: &sqlx::Pool<sqlx::MySql>,
        user_id: &str,
        session_id: &str,
        analysis_view: &str,
        last_n: i32,
    ) -> ServiceResult<Option<ObservationGraphSlice>> {
        if !analysis_view_queries_recent_evidence_graph(analysis_view) {
            return Ok(None);
        }

        let decision_limit = i64::from(last_n.clamp(1, 50));
        let decision_rows = query(
            "SELECT d.decision_id, d.event_id, d.decision_type, \
               IFNULL(CAST(d.decision_output AS CHAR), '{}') AS decision_output_json, \
               DATE_FORMAT(d.created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
             FROM ctx_decision_audits d \
             JOIN agent_events e ON e.event_id = d.event_id AND e.session_id = d.session_id AND e.user_id = ? \
             WHERE d.user_id = ? AND d.session_id = ? \
             ORDER BY d.created_at DESC LIMIT ?",
        )
        .bind(user_id)
        .bind(user_id)
        .bind(session_id)
        .bind(decision_limit)
        .fetch_all(pool)
        .await
        .map_err(|e| internal_error(format!("evidence graph decisions query: {e}")))?;

        let decisions: Vec<EvidenceDecision> = decision_rows
            .iter()
            .map(|row| {
                let decision_output_json: String = row
                    .try_get("decision_output_json")
                    .unwrap_or_else(|_| "{}".to_string());
                EvidenceDecision {
                    decision_id: row.try_get("decision_id").unwrap_or_default(),
                    event_id: row.try_get("event_id").unwrap_or_default(),
                    decision_type: row.try_get("decision_type").unwrap_or_default(),
                    decision_output: serde_json::from_str(&decision_output_json)
                        .unwrap_or(serde_json::Value::Object(Default::default())),
                    created_at: row.try_get("created_at").unwrap_or_default(),
                }
            })
            .collect();

        let event_limit = std::cmp::max(decision_limit * 10, 50);
        let event_rows = query(
            "SELECT event_id, event_type, \
               SUBSTRING(COALESCE(CAST(content AS CHAR), ''), 1, 180) AS content, \
               skill_name, parent_event_id, causal_chain_id, \
               DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
             FROM agent_events \
             WHERE session_id = ? AND user_id = ? \
             ORDER BY created_at DESC LIMIT ?",
        )
        .bind(session_id)
        .bind(user_id)
        .bind(event_limit)
        .fetch_all(pool)
        .await
        .map_err(|e| internal_error(format!("evidence graph events query: {e}")))?;

        let recent_events: Vec<EvidenceEvent> = event_rows
            .iter()
            .map(|row| EvidenceEvent {
                event_id: row.try_get("event_id").unwrap_or_default(),
                event_type: row.try_get("event_type").unwrap_or_default(),
                content: row.try_get("content").unwrap_or_default(),
                skill_name: row.try_get("skill_name").ok(),
                parent_event_id: row.try_get("parent_event_id").ok(),
                causal_chain_id: row.try_get("causal_chain_id").ok(),
                created_at: row.try_get("created_at").unwrap_or_default(),
            })
            .collect();

        let parent_id_map = crate::storage::load_agent_event_parent_ids(
            pool,
            &recent_events
                .iter()
                .map(|event| event.event_id.clone())
                .collect::<Vec<_>>(),
        )
        .await
        .map_err(|e| internal_error(format!("evidence graph parent query: {e}")))?;

        let filtered_events =
            filter_evidence_events_for_graph(&decisions, recent_events, &parent_id_map);

        Ok(build_evidence_graph(
            &decisions,
            &filtered_events,
            &parent_id_map,
        ))
    }
}

fn filter_evidence_events_for_graph(
    decisions: &[EvidenceDecision],
    recent_events: Vec<EvidenceEvent>,
    parent_id_map: &std::collections::HashMap<String, Vec<String>>,
) -> Vec<EvidenceEvent> {
    if decisions.is_empty() {
        return recent_events;
    }

    let decision_event_ids: std::collections::HashSet<String> = decisions
        .iter()
        .map(|decision| decision.event_id.clone())
        .collect();
    let relevant_chain_ids: std::collections::HashSet<String> = recent_events
        .iter()
        .filter(|event| decision_event_ids.contains(&event.event_id))
        .filter_map(|event| event.causal_chain_id.clone())
        .collect();

    recent_events
        .into_iter()
        .filter(|event| {
            decision_event_ids.contains(&event.event_id)
                || event
                    .parent_event_id
                    .as_ref()
                    .map(|parent_event_id| decision_event_ids.contains(parent_event_id))
                    .unwrap_or(false)
                || event
                    .causal_chain_id
                    .as_ref()
                    .map(|causal_chain_id| relevant_chain_ids.contains(causal_chain_id))
                    .unwrap_or(false)
                || parent_id_map
                    .get(&event.event_id)
                    .map(|parent_ids| {
                        parent_ids
                            .iter()
                            .any(|parent_event_id| decision_event_ids.contains(parent_event_id))
                    })
                    .unwrap_or(false)
        })
        .collect()
}

#[async_trait]
impl ReflectService for DatabaseReflectService {
    async fn build_evidence(
        &self,
        user_id: &str,
        session_id: &str,
        request: ReflectRequest,
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
               SUM(CASE WHEN event_type IN ('error', 'tool_error', 'stall_detected') \
                    OR event_type LIKE '%error%' OR event_type LIKE '%fail%' THEN 1 ELSE 0 END) AS error_count, \
               CAST(MIN(created_at) AS CHAR) AS first_event, \
               CAST(MAX(created_at) AS CHAR) AS last_event \
             FROM agent_events WHERE session_id = ? AND user_id = ?",
        )
        .bind(session_id)
        .bind(user_id)
        .fetch_one(&pool)
        .await
        .map_err(|e| internal_error(format!("overview query: {e}")))?;

        let total_events: i64 = overview_row.try_get("total_events").unwrap_or(0);
        let unique_skills: i64 = overview_row.try_get("unique_skills").unwrap_or(0);
        let error_count: i64 = overview_row.try_get("error_count").unwrap_or(0);
        let first_event: Option<String> = overview_row.try_get("first_event").unwrap_or(None);
        let last_event: Option<String> = overview_row.try_get("last_event").unwrap_or(None);

        // Compute duration in Rust from timestamp strings
        let duration_minutes =
            compute_duration_minutes(first_event.as_deref(), last_event.as_deref());

        let error_rate_pct = if total_events > 0 {
            (error_count as f64 / total_events as f64) * 100.0
        } else {
            0.0
        };

        // Top event types
        let event_type_rows = query(
            "SELECT event_type, COUNT(*) AS cnt \
             FROM agent_events WHERE session_id = ? AND user_id = ? \
             GROUP BY event_type ORDER BY cnt DESC LIMIT 5",
        )
        .bind(session_id)
        .bind(user_id)
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
             FROM agent_events WHERE session_id = ? AND user_id = ? AND skill_name IS NOT NULL \
             GROUP BY skill_name ORDER BY cnt DESC LIMIT 5",
        )
        .bind(session_id)
        .bind(user_id)
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
            "SELECT d.decision_type, COUNT(*) AS cnt, \
               COUNT(DISTINCT d.model_used) AS models_used \
             FROM ctx_decision_audits d \
             JOIN agent_events e ON e.event_id = d.event_id AND e.session_id = d.session_id AND e.user_id = ? \
             WHERE d.user_id = ? AND d.session_id = ? \
             GROUP BY d.decision_type ORDER BY cnt DESC LIMIT 5",
        )
        .bind(user_id)
        .bind(user_id)
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

        // Error patterns (aggregated, for insights)
        let error_patterns = if analysis_view_queries_error_patterns(&request.analysis_view) {
            let ep_rows = query(
                "SELECT IFNULL(skill_name, 'unknown') AS skill_name, event_type, COUNT(*) AS fail_count, \
                   SUBSTRING(COALESCE(MIN(content), ''), 1, 100) AS sample_error \
                 FROM agent_events \
                 WHERE session_id = ? AND user_id = ? AND (event_type IN ('error', 'tool_error', 'stall_detected') \
                   OR event_type LIKE '%error%' OR event_type LIKE '%fail%') \
                 GROUP BY skill_name, event_type \
                 ORDER BY fail_count DESC LIMIT 10",
            )
            .bind(session_id)
            .bind(user_id)
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

        // ── Diagnostic: fetch recent ACTUAL error content for root-cause analysis
        // Limit to 30 most recent errors — enough for pattern detection, bounded cost
        let raw_errors = {
            let err_rows = query(
                "SELECT IFNULL(skill_name, 'unknown') AS skill_name, event_type, \
                   SUBSTRING(COALESCE(CAST(content AS CHAR), ''), 1, 300) AS content \
                 FROM agent_events \
                 WHERE session_id = ? AND user_id = ? AND (event_type IN ('error', 'tool_error', 'stall_detected') \
                   OR event_type LIKE '%error%' OR event_type LIKE '%fail%') \
                 ORDER BY created_at DESC LIMIT 30",
            )
            .bind(session_id)
            .bind(user_id)
            .fetch_all(&pool)
            .await
            .map_err(|e| internal_error(format!("raw errors query: {e}")))?;

            err_rows
                .iter()
                .map(|row| RawError {
                    skill_name: row.try_get("skill_name").unwrap_or_default(),
                    event_type: row.try_get("event_type").unwrap_or_default(),
                    content: row.try_get("content").unwrap_or_default(),
                })
                .collect::<Vec<_>>()
        };

        let diagnoses = build_diagnoses(&raw_errors);

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
        let recommendations = generate_recommendations(&overview, &diagnoses, &insights);
        let raw_evidence_graph = self
            .build_recent_evidence_graph(
                &pool,
                user_id,
                session_id,
                &request.analysis_view,
                request.last_n,
            )
            .await?;
        let (evidence_graph, budget_result) = budget_reflect_evidence_graph(raw_evidence_graph);
        let envelope = build_observation_envelope(
            session_id,
            &request,
            &overview,
            &diagnoses,
            &insights,
            &recommendations,
            evidence_graph.as_ref(),
        );
        let graph_slice = build_reflect_graph_slice(
            evidence_graph.as_ref(),
            &envelope.observations,
            &envelope.evidence,
            &envelope.failure_clusters,
            &budget_result,
        );
        let view = request.view(overview.total_events, overview.total_decisions);
        let data_coverage = view.data_coverage.clone();

        Ok(ReflectReport {
            schema_version: 1,
            tool: "reflect".to_string(),
            session_id: session_id.to_string(),
            analysis_view: request.analysis_view,
            topic: request.topic,
            facet: request.facet,
            depth: request.depth,
            horizon: request.horizon,
            source_policy: request.source_policy,
            include_context: request.include_context,
            data_coverage,
            view: Some(view),
            summary: envelope.summary,
            observations: envelope.observations,
            evidence: envelope.evidence,
            action_hints: envelope.action_hints,
            failure_clusters: envelope.failure_clusters,
            graph_slice,
            budget_result,
            overview,
            diagnoses,
            insights,
            recommendations,
        })
    }
}

const REFLECT_EVIDENCE_GRAPH_NODE_BUDGET: usize = 50;

fn budget_reflect_evidence_graph(
    evidence_graph: Option<ObservationGraphSlice>,
) -> (Option<ObservationGraphSlice>, ObservationBudgetResult) {
    let Some(mut graph) = evidence_graph else {
        return (None, ObservationBudgetResult::default());
    };

    let omitted_nodes = graph
        .nodes
        .len()
        .saturating_sub(REFLECT_EVIDENCE_GRAPH_NODE_BUDGET) as i64;
    if omitted_nodes > 0 {
        graph.nodes.truncate(REFLECT_EVIDENCE_GRAPH_NODE_BUDGET);
        let retained = graph
            .nodes
            .iter()
            .map(|node| node.ref_id.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        graph.edges.retain(|edge| {
            retained.contains(edge.from.as_str()) && retained.contains(edge.to.as_str())
        });
    }

    (
        Some(graph),
        ObservationBudgetResult {
            truncated: omitted_nodes > 0,
            next_cursor: None,
            omitted: ObservationBudgetOmitted {
                nodes: omitted_nodes,
                evidence_previews: omitted_nodes,
                ..Default::default()
            },
        },
    )
}

fn build_reflect_graph_slice(
    evidence_graph: Option<&ObservationGraphSlice>,
    observations: &[ObservationRecord],
    evidence: &[ObservationEvidence],
    failure_clusters: &[ObservationFailureCluster],
    budget_result: &ObservationBudgetResult,
) -> ObservationGraphSlice {
    let mut nodes = Vec::new();
    let mut node_refs = BTreeSet::new();
    let mut edges = Vec::new();
    let mut edge_keys = BTreeSet::new();

    if let Some(graph) = evidence_graph {
        for node in &graph.nodes {
            push_graph_node(
                &mut nodes,
                &mut node_refs,
                ObservationGraphNode {
                    ref_id: node.ref_id.clone(),
                    layer: node.layer,
                    kind: node.kind,
                    label: node.label.clone(),
                    summary: node.summary.clone(),
                    metadata: node.metadata.clone(),
                },
            );
        }
        for edge in &graph.edges {
            push_graph_edge(
                &mut edges,
                &mut edge_keys,
                edge.from.clone(),
                edge.to.clone(),
                edge.kind,
            );
        }
    }

    for item in evidence {
        push_graph_node(
            &mut nodes,
            &mut node_refs,
            ObservationGraphNode {
                ref_id: item.ref_id.clone(),
                layer: ObservationGraphLayer::Runtime,
                kind: ObservationGraphNodeKind::Evidence,
                label: item.evidence_class.clone(),
                summary: Some(item.summary.clone()),
                metadata: None,
            },
        );
    }

    for observation in observations {
        push_graph_node(
            &mut nodes,
            &mut node_refs,
            ObservationGraphNode {
                ref_id: observation.ref_id.clone(),
                layer: ObservationGraphLayer::Observation,
                kind: ObservationGraphNodeKind::Observation,
                label: observation.kind.clone(),
                summary: Some(observation.summary.clone()),
                metadata: None,
            },
        );
        for evidence_ref in &observation.evidence_refs {
            push_graph_edge(
                &mut edges,
                &mut edge_keys,
                observation.ref_id.clone(),
                evidence_ref.clone(),
                ObservationGraphEdgeKind::DerivedFrom,
            );
        }
    }

    for cluster in failure_clusters {
        push_graph_node(
            &mut nodes,
            &mut node_refs,
            ObservationGraphNode {
                ref_id: cluster.cluster_ref.clone(),
                layer: ObservationGraphLayer::Observation,
                kind: ObservationGraphNodeKind::FailureCluster,
                label: cluster.label.clone(),
                summary: Some(cluster.summary.clone()),
                metadata: None,
            },
        );
        for observation_ref in &cluster.observation_refs {
            push_graph_edge(
                &mut edges,
                &mut edge_keys,
                cluster.cluster_ref.clone(),
                observation_ref.clone(),
                ObservationGraphEdgeKind::DerivedFrom,
            );
        }
    }

    ObservationGraphSlice {
        nodes,
        edges,
        budget_result: budget_result.clone(),
    }
}

fn analysis_view_queries_error_patterns(analysis_view: &str) -> bool {
    matches!(
        analysis_view,
        "overview" | "execution_errors" | "execution_tools" | "execution_trace"
    )
}

fn analysis_view_queries_recent_evidence_graph(analysis_view: &str) -> bool {
    matches!(
        analysis_view,
        "overview" | "execution_tools" | "execution_trace"
    )
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
        _: ReflectRequest,
    ) -> ServiceResult<ReflectReport> {
        Err(internal_error("reflect service not configured"))
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
use astra_core::ObservationGraphEdge;

#[cfg(test)]
mod tests {
    use super::*;
    use astra_core::EvidenceRef;

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
    fn insight_error_rate_thresholds() {
        // high (critical, >=40%)
        let ov = make_overview(100, 40, vec![], 5, None);
        let ins = generate_insights(&ov, &[], &[]);
        assert!(
            ins.iter()
                .any(|i| i.severity == "critical" && i.category == "error_pattern")
        );

        // elevated (warning, 20%)
        let ov = make_overview(100, 20, vec![], 5, None);
        let ins = generate_insights(&ov, &[], &[]);
        assert!(
            ins.iter()
                .any(|i| i.severity == "warning" && i.category == "error_pattern")
        );

        // low (no warning)
        let ov = make_overview(100, 5, vec![], 5, None);
        let ins = generate_insights(&ov, &[], &[]);
        assert!(!ins.iter().any(|i| i.category == "error_pattern"));

        // 100% (critical)
        let ov = make_overview(10, 10, vec![], 0, None);
        let ins = generate_insights(&ov, &[], &[]);
        assert!(
            ins.iter()
                .any(|i| i.severity == "critical" && i.category == "error_pattern")
        );
    }

    #[test]
    fn insight_elevated_error_rate() {
        let overview = make_overview(100, 20, vec![], 5, None);
        let insights = generate_insights(&overview, &[], &[]);
        assert!(
            insights
                .iter()
                .any(|i| i.severity == "warning" && i.category == "error_pattern")
        );
    }

    #[test]
    fn insight_no_error_rate_warning_when_low() {
        let overview = make_overview(100, 5, vec![], 5, None);
        let insights = generate_insights(&overview, &[], &[]);
        assert!(!insights.iter().any(|i| i.category == "error_pattern"));
    }

    #[test]
    fn insight_repeated_tool_failure() {
        // high count triggers insight
        let ov = make_overview(50, 5, vec![], 5, None);
        let patterns = vec![ErrorPattern {
            skill_name: "bash".into(),
            event_type: "tool_error".into(),
            fail_count: 5,
            sample_error: "permission denied".into(),
        }];
        let ins = generate_insights(&ov, &patterns, &[]);
        assert!(
            ins.iter()
                .any(|i| i.category == "tool_usage" && i.message.contains("bash"))
        );

        // low count does not trigger
        let patterns = vec![ErrorPattern {
            skill_name: "bash".into(),
            event_type: "tool_error".into(),
            fail_count: 2,
            sample_error: "not found".into(),
        }];
        let ins = generate_insights(&ov, &patterns, &[]);
        assert!(
            !ins.iter()
                .any(|i| i.category == "tool_usage" && i.message.contains("bash"))
        );
    }

    #[test]
    fn insight_over_reliance() {
        // over-reliance detected
        let ov = make_overview(100, 0, vec![("bash".into(), 75)], 5, None);
        let ins = generate_insights(&ov, &[], &[]);
        assert!(ins.iter().any(|i| i.message.contains("Over-reliance")));

        // balanced usage — no warning
        let ov = make_overview(
            100,
            0,
            vec![("bash".into(), 30), ("grep".into(), 25)],
            5,
            None,
        );
        let ins = generate_insights(&ov, &[], &[]);
        assert!(!ins.iter().any(|i| i.message.contains("Over-reliance")));
    }

    #[test]
    fn insight_short_session() {
        let overview = make_overview(3, 0, vec![], 1, None);
        let insights = generate_insights(&overview, &[], &[]);
        // Small sessions with 1-4 events no longer get a "short session" insight
        // (removed trivial short-session noise)
        assert!(!insights.iter().any(|i| i.severity == "critical"));
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
        assert!(
            insights
                .iter()
                .any(|i| i.severity == "critical" && i.category == "error_pattern")
        );
    }

    #[test]
    fn insight_huge_session_numbers() {
        let overview = make_overview(
            100_000,
            500,
            vec![("bash".into(), 40_000)],
            5000,
            Some(120.0),
        );
        let insights = generate_insights(&overview, &[], &[]);
        assert!(
            insights.is_empty(),
            "large but healthy sessions should not produce low-signal insights: {insights:?}"
        );
    }

    #[test]
    fn recommendations_do_not_infer_actions_from_error_count_without_evidence() {
        let overview = make_overview(100, 40, vec![], 5, None);
        let diagnoses = build_diagnoses(&[]); // no raw errors for this test
        let insights = generate_insights(&overview, &[], &[]);
        let recs = generate_recommendations(&overview, &diagnoses, &insights);
        assert!(
            recs.is_empty(),
            "error counts without diagnoses or actionable insights must not create unsupported recommendations: {recs:?}"
        );
    }

    #[test]
    fn recommendations_skip_empty_diagnosis_fix_hints() {
        let overview = make_overview(50, 0, vec![], 0, None);
        let diagnoses = vec![Diagnosis {
            category: astra_core::ErrorKind::Unknown,
            severity: "warning".into(),
            summary: "diagnosis without actionable fix".into(),
            samples: vec!["ambiguous failure".into()],
            occurrences: 1,
            affected_tool: "bash".into(),
            fix_hint: "   ".into(),
        }];
        let insights = vec![Insight {
            severity: "warning".into(),
            category: "stall".into(),
            message: "Many events but no decisions — possible routing issue".into(),
            evidence: "50 events, 0 decisions".into(),
        }];

        let recs = generate_recommendations(&overview, &diagnoses, &insights);

        assert_eq!(
            recs,
            vec!["Review agent routing — events without decisions may be misconfigured"]
        );
        assert!(recs.iter().all(|rec| !rec.trim().is_empty()));
    }

    #[test]
    fn recommendations_long_session() {
        let overview = make_overview(200, 0, vec![], 10, Some(45.0));
        let recs = generate_recommendations(&overview, &[], &[]);
        assert!(recs.iter().any(|r| r.contains("breaking")));
    }

    #[test]
    fn recommendations_empty_for_clean_session() {
        let overview = make_overview(50, 0, vec![("bash".into(), 20)], 10, Some(5.0));
        let recs = generate_recommendations(&overview, &[], &[]);
        assert!(recs.is_empty());
    }

    #[test]
    fn observation_envelope_maps_diagnoses_to_refs_and_hints() {
        let request = ReflectRequest::from_observation_params(
            Some("execution"),
            Some("errors"),
            None,
            None,
            20,
            "why did bash fail?",
        );
        let overview = make_overview(10, 3, vec![("bash".into(), 5)], 2, Some(3.0));
        let diagnoses = vec![Diagnosis {
            category: astra_core::ErrorKind::ToolTimeout,
            severity: "warning".into(),
            summary: "Timeout (bash): operation exceeded time limit - 3 occurrences".into(),
            samples: vec!["command timed out after 30s".into()],
            occurrences: 3,
            affected_tool: "bash".into(),
            fix_hint: "Narrow Command Scope".into(),
        }];
        let recommendations = vec![" narrow   command scope ".to_string()];

        let envelope = build_observation_envelope(
            "sess/with spaces",
            &request,
            &overview,
            &diagnoses,
            &[],
            &recommendations,
            None,
        );

        let summary = envelope.summary;
        let observations = envelope.observations;
        let evidence = envelope.evidence;
        let action_hints = envelope.action_hints;
        let failure_clusters = envelope.failure_clusters;

        assert!(summary.contains("Timeout (bash)"));
        assert_eq!(observations.len(), 1);
        assert!(
            observations[0]
                .ref_id
                .starts_with("urn:astra:observation:graph:reflect:")
        );
        assert_eq!(observations[0].topic, "execution");
        assert_eq!(observations[0].facet, "errors");
        assert_eq!(observations[0].kind, "diagnosis:tool_timeout");
        assert_eq!(observations[0].confidence.evidence, Some(0.90));
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].evidence_class, "observed_evidence");
        assert_eq!(evidence[0].confidence.evidence, Some(0.90));
        assert_eq!(evidence[0].confidence.classification, None);
        assert_eq!(evidence[0].confidence.causal, None);
        assert!(
            evidence[0]
                .ref_id
                .starts_with("urn:astra:artifact:cloud:reflect:")
        );
        assert_eq!(action_hints.len(), 1);
        assert_eq!(action_hints[0].target_type, "user_guidance");
        assert_eq!(
            action_hints[0].observation_refs,
            vec![observations[0].ref_id.clone()]
        );
        assert_eq!(failure_clusters.len(), 1);
        assert_eq!(failure_clusters[0].label, "tool_timeout_bash");
        assert_eq!(
            failure_clusters[0].observation_refs,
            vec![observations[0].ref_id.clone()]
        );
        assert_refs_are_valid(&observations, &evidence, &action_hints, &failure_clusters);
    }

    #[test]
    fn observation_envelope_classifies_diagnoses_independent_of_overview_request() {
        let request =
            ReflectRequest::from_observation_params(None, None, None, None, 20, "what went wrong?");
        let overview = make_overview(12, 2, vec![("bash".into(), 5)], 2, Some(3.0));
        let diagnoses = vec![Diagnosis {
            category: astra_core::ErrorKind::ResourceLimit,
            severity: "critical".into(),
            summary: "Resource limit (bash): fork failed - 2 occurrences".into(),
            samples: vec!["fork: Resource temporarily unavailable".into()],
            occurrences: 2,
            affected_tool: "bash".into(),
            fix_hint: "check ulimit".into(),
        }];

        let envelope = build_observation_envelope(
            "sess-overview",
            &request,
            &overview,
            &diagnoses,
            &[],
            &[],
            None,
        );
        let observations = envelope.observations;

        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].topic, "execution");
        assert_eq!(observations[0].facet, "errors");
        assert_eq!(observations[0].kind, "diagnosis:resource_limit");
    }

    #[test]
    fn observation_envelope_maps_insight_topics_and_action_refs_precisely() {
        let request = ReflectRequest::from_observation_params(
            None,
            None,
            None,
            None,
            20,
            "how is the session going?",
        );
        let overview = make_overview(80, 0, vec![("bash".into(), 64)], 0, Some(8.0));
        let insights = vec![
            Insight {
                severity: "warning".into(),
                category: "stall".into(),
                message: "Many events but no decisions - possible routing issue".into(),
                evidence: "80 events, 0 decisions".into(),
            },
            Insight {
                severity: "info".into(),
                category: "tool_usage".into(),
                message: "Over-reliance on bash: 80%".into(),
                evidence: "64/80".into(),
            },
        ];
        let recommendations = vec![
            "Review agent routing — EVENTS WITHOUT DECISIONS may be misconfigured".to_string(),
            "Consider using more DIVERSE TOOLS for better coverage".to_string(),
        ];

        let envelope = build_observation_envelope(
            "sess-insights",
            &request,
            &overview,
            &[],
            &insights,
            &recommendations,
            None,
        );
        let observations = envelope.observations;
        let action_hints = envelope.action_hints;
        let failure_clusters = envelope.failure_clusters;

        assert_eq!(observations[0].topic, "execution");
        assert_eq!(observations[0].facet, "stall");
        assert_eq!(observations[1].topic, "execution");
        assert_eq!(observations[1].facet, "tools");
        assert_eq!(action_hints.len(), 2);
        assert_eq!(
            action_hints[0].observation_refs,
            vec![observations[0].ref_id.clone()]
        );
        assert_eq!(
            action_hints[1].observation_refs,
            vec![observations[1].ref_id.clone()]
        );
        assert!(
            failure_clusters.is_empty(),
            "statistical insights should not be promoted into failure clusters without diagnosis evidence"
        );
    }

    #[test]
    fn execution_trace_requests_enable_recent_evidence_graph() {
        let request = ReflectRequest::from_observation_params(
            Some("execution"),
            Some("trace"),
            Some("forensic"),
            Some("session"),
            20,
            "show the causal trace",
        );

        assert_eq!(request.analysis_view, "execution_trace");
        assert!(analysis_view_queries_error_patterns(&request.analysis_view));
        assert!(
            analysis_view_queries_recent_evidence_graph(&request.analysis_view),
            "execution/trace must not be blocked by internal analysis-view gating"
        );
    }

    #[test]
    fn non_trace_analysis_views_do_not_enable_recent_evidence_graph() {
        let knowledge = ReflectRequest::from_observation_params(
            Some("knowledge"),
            Some("context"),
            None,
            None,
            20,
            "",
        );
        let performance = ReflectRequest::from_observation_params(
            Some("runtime"),
            Some("performance"),
            None,
            None,
            20,
            "",
        );

        assert_eq!(knowledge.analysis_view, "knowledge_context");
        assert_eq!(performance.analysis_view, "runtime_performance");
        assert!(!analysis_view_queries_recent_evidence_graph(
            &knowledge.analysis_view
        ));
        assert!(!analysis_view_queries_recent_evidence_graph(
            &performance.analysis_view
        ));
    }

    #[test]
    fn observation_envelope_emits_health_observation_when_no_findings() {
        let request = ReflectRequest::from_observation_params(None, None, None, None, 20, "");
        let overview = make_overview(8, 0, vec![("bash".into(), 3)], 2, Some(1.0));

        let envelope =
            build_observation_envelope("sess-healthy", &request, &overview, &[], &[], &[], None);
        let summary = envelope.summary;
        let observations = envelope.observations;
        let evidence = envelope.evidence;
        let action_hints = envelope.action_hints;
        let failure_clusters = envelope.failure_clusters;

        assert!(summary.contains("Session healthy"));
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].kind, "session_health");
        assert_eq!(observations[0].severity, "info");
        assert!(evidence.is_empty());
        assert!(action_hints.is_empty());
        assert!(failure_clusters.is_empty());
        assert_refs_are_valid(&observations, &evidence, &action_hints, &failure_clusters);
    }

    #[test]
    fn observation_envelope_adds_standard_refs_for_evidence_graph_nodes() {
        let request = ReflectRequest::from_observation_params(None, None, None, None, 20, "");
        let overview = make_overview(2, 0, vec![], 1, None);
        let graph = ObservationGraphSlice {
            nodes: vec![
                ObservationGraphNode {
                    ref_id: graph_event_ref("evt-1"),
                    layer: ObservationGraphLayer::Runtime,
                    kind: ObservationGraphNodeKind::Event,
                    label: "user_query".into(),
                    summary: Some("asked a question".into()),
                    metadata: None,
                },
                ObservationGraphNode {
                    ref_id: graph_decision_ref("dec-1"),
                    layer: ObservationGraphLayer::Runtime,
                    kind: ObservationGraphNodeKind::Decision,
                    label: "tool_surface".into(),
                    summary: None,
                    metadata: None,
                },
            ],
            edges: vec![],
            budget_result: ObservationBudgetResult::default(),
        };

        let envelope = build_observation_envelope(
            "sess-graph",
            &request,
            &overview,
            &[],
            &[],
            &[],
            Some(&graph),
        );
        let evidence = envelope.evidence;

        assert!(
            evidence
                .iter()
                .any(|item| item.ref_id == "urn:astra:event:cloud:evt-1")
        );
        assert!(
            evidence
                .iter()
                .any(|item| item.ref_id == "urn:astra:decision:cloud:dec-1")
        );
        assert_refs_are_valid(&[], &evidence, &[], &[]);
    }

    #[test]
    fn reflect_graph_slice_projects_shared_layers_without_dangling_edges() {
        let request = ReflectRequest::from_observation_params(
            Some("execution"),
            Some("trace"),
            Some("forensic"),
            Some("session"),
            20,
            "show trace",
        );
        let overview = make_overview(10, 3, vec![("bash".into(), 5)], 2, Some(3.0));
        let diagnoses = vec![Diagnosis {
            category: astra_core::ErrorKind::ToolTimeout,
            severity: "warning".into(),
            summary: "Timeout (bash): operation exceeded time limit - 3 occurrences".into(),
            samples: vec!["command timed out after 30s".into()],
            occurrences: 3,
            affected_tool: "bash".into(),
            fix_hint: "Narrow Command Scope".into(),
        }];
        let graph = ObservationGraphSlice {
            nodes: vec![
                ObservationGraphNode {
                    ref_id: graph_event_ref("evt-1"),
                    layer: ObservationGraphLayer::Runtime,
                    kind: ObservationGraphNodeKind::Event,
                    label: "tool_call".into(),
                    summary: Some("ran bash".into()),
                    metadata: None,
                },
                ObservationGraphNode {
                    ref_id: graph_event_ref("evt-2"),
                    layer: ObservationGraphLayer::Runtime,
                    kind: ObservationGraphNodeKind::Outcome,
                    label: "tool_error".into(),
                    summary: Some("timeout".into()),
                    metadata: None,
                },
            ],
            edges: vec![ObservationGraphEdge {
                from: graph_event_ref("evt-1"),
                to: graph_event_ref("evt-2"),
                kind: ObservationGraphEdgeKind::Causes,
            }],
            budget_result: ObservationBudgetResult::default(),
        };
        let envelope = build_observation_envelope(
            "sess-graph-slice",
            &request,
            &overview,
            &diagnoses,
            &[],
            &["narrow command scope".to_string()],
            Some(&graph),
        );
        let graph_slice = build_reflect_graph_slice(
            Some(&graph),
            &envelope.observations,
            &envelope.evidence,
            &envelope.failure_clusters,
            &ObservationBudgetResult::default(),
        );

        assert!(graph_slice.nodes.iter().any(|node| {
            node.ref_id == "urn:astra:event:cloud:evt-1"
                && node.layer == ObservationGraphLayer::Runtime
                && node.kind == ObservationGraphNodeKind::Event
        }));
        assert!(graph_slice.nodes.iter().any(|node| {
            node.ref_id == envelope.observations[0].ref_id
                && node.layer == ObservationGraphLayer::Observation
                && node.kind == ObservationGraphNodeKind::Observation
        }));
        assert!(graph_slice.nodes.iter().any(|node| {
            node.ref_id == envelope.failure_clusters[0].cluster_ref
                && node.kind == ObservationGraphNodeKind::FailureCluster
        }));
        assert!(
            graph_slice
                .nodes
                .iter()
                .any(|node| node.ref_id == envelope.evidence[0].ref_id
                    && node.kind == ObservationGraphNodeKind::Evidence),
            "materialized evidence previews must be nodes so observation edges are not dangling"
        );

        let node_refs = graph_slice
            .nodes
            .iter()
            .map(|node| node.ref_id.as_str())
            .collect::<BTreeSet<_>>();
        for edge in &graph_slice.edges {
            assert!(
                node_refs.contains(edge.from.as_str()),
                "dangling from edge: {edge:?}"
            );
            assert!(
                node_refs.contains(edge.to.as_str()),
                "dangling to edge: {edge:?}"
            );
        }
    }

    #[test]
    fn reflect_graph_budget_truncates_nodes_and_prunes_dangling_edges() {
        let graph = ObservationGraphSlice {
            nodes: (0..55)
                .map(|idx| ObservationGraphNode {
                    ref_id: graph_event_ref(&format!("evt-{idx}")),
                    layer: ObservationGraphLayer::Runtime,
                    kind: ObservationGraphNodeKind::Event,
                    label: format!("event {idx}"),
                    summary: None,
                    metadata: None,
                })
                .collect(),
            edges: vec![
                ObservationGraphEdge {
                    from: graph_event_ref("evt-0"),
                    to: graph_event_ref("evt-49"),
                    kind: ObservationGraphEdgeKind::Supports,
                },
                ObservationGraphEdge {
                    from: graph_event_ref("evt-49"),
                    to: graph_event_ref("evt-54"),
                    kind: ObservationGraphEdgeKind::Causes,
                },
            ],
            budget_result: ObservationBudgetResult::default(),
        };

        let (budgeted_graph, budget) = budget_reflect_evidence_graph(Some(graph));
        let budgeted_graph = budgeted_graph.expect("budgeted graph");
        assert!(budget.truncated);
        assert_eq!(budget.omitted.nodes, 5);
        assert_eq!(budget.omitted.evidence_previews, 5);
        assert_eq!(budgeted_graph.nodes.len(), 50);
        assert_eq!(budgeted_graph.edges.len(), 1);
        assert_eq!(budgeted_graph.edges[0].to, graph_event_ref("evt-49"));
    }

    #[test]
    fn reflect_graph_budget_empty_graph_is_not_truncated() {
        let (graph, budget) = budget_reflect_evidence_graph(None);
        assert!(graph.is_none());
        assert!(!budget.truncated);
        assert!(budget.omitted.is_empty());
    }

    fn assert_refs_are_valid(
        observations: &[ObservationRecord],
        evidence: &[ObservationEvidence],
        action_hints: &[ObservationActionHint],
        failure_clusters: &[ObservationFailureCluster],
    ) {
        for observation in observations {
            EvidenceRef::parse(&observation.ref_id).unwrap_or_else(|err| {
                panic!("invalid observation ref {}: {err}", observation.ref_id)
            });
            for evidence_ref in &observation.evidence_refs {
                EvidenceRef::parse(evidence_ref).unwrap_or_else(|err| {
                    panic!("invalid observation evidence ref {evidence_ref}: {err}")
                });
            }
        }
        for evidence in evidence {
            EvidenceRef::parse(&evidence.ref_id)
                .unwrap_or_else(|err| panic!("invalid evidence ref {}: {err}", evidence.ref_id));
        }
        for hint in action_hints {
            for observation_ref in &hint.observation_refs {
                EvidenceRef::parse(observation_ref).unwrap_or_else(|err| {
                    panic!("invalid action hint ref {observation_ref}: {err}")
                });
            }
        }
        for cluster in failure_clusters {
            EvidenceRef::parse(&cluster.cluster_ref).unwrap_or_else(|err| {
                panic!("invalid failure cluster ref {}: {err}", cluster.cluster_ref)
            });
            for observation_ref in &cluster.observation_refs {
                EvidenceRef::parse(observation_ref).unwrap_or_else(|err| {
                    panic!("invalid failure cluster observation ref {observation_ref}: {err}")
                });
            }
        }
    }

    #[test]
    fn test_compute_duration_minutes() {
        // basic
        let d = compute_duration_minutes(Some("2026-03-25 08:00:00"), Some("2026-03-25 08:18:30"));
        assert!((d.unwrap() - 18.5).abs() < 0.01);

        // missing / empty
        assert!(compute_duration_minutes(None, Some("2026-03-25 08:00:00")).is_none());
        assert!(compute_duration_minutes(Some("2026-03-25 08:00:00"), None).is_none());
        assert!(compute_duration_minutes(None, None).is_none());
        assert!(compute_duration_minutes(Some(""), Some("")).is_none());
    }

    #[test]
    fn report_serialization_roundtrip() {
        let data_coverage = ObservationDataCoverage {
            overall: "fresh".into(),
            source: "server_db".into(),
            events: 10,
            decisions: 2,
            providers: Default::default(),
            warnings: vec![],
        };
        let report = ReflectReport {
            schema_version: 1,
            tool: "reflect".into(),
            session_id: "test-sess".into(),
            analysis_view: "overview".into(),
            topic: "overview".into(),
            facet: "overview".into(),
            depth: "diagnostic".into(),
            horizon: "session".into(),
            source_policy: "auto".into(),
            include_context: false,
            data_coverage: data_coverage.clone(),
            view: Some(ObservationView {
                topic: "overview".into(),
                facet: "overview".into(),
                depth: "diagnostic".into(),
                horizon: "session".into(),
                data_coverage,
            }),
            summary: "fork failed".into(),
            observations: vec![ObservationRecord {
                ref_id: "urn:astra:observation:graph:reflect:test-sess:diagnosis:0".into(),
                topic: "overview".into(),
                facet: "overview".into(),
                kind: "diagnosis:resource_limit".into(),
                severity: "critical".into(),
                summary: "fork failed".into(),
                confidence: ObservationConfidence::complete(0.90, 0.90, 0.82),
                evidence_refs: vec![
                    "urn:astra:artifact:cloud:reflect:test-sess:diagnosis:0:sample:0".into(),
                ],
            }],
            evidence: vec![ObservationEvidence {
                ref_id: "urn:astra:artifact:cloud:reflect:test-sess:diagnosis:0:sample:0".into(),
                evidence_class: "observed_evidence".into(),
                source: "agent_events.error_sample".into(),
                summary: "fork: Resource temporarily unavailable".into(),
                confidence: ObservationConfidence::evidence(0.90),
            }],
            action_hints: vec![ObservationActionHint {
                target_type: "user_guidance".into(),
                summary: "check ulimit -u".into(),
                confidence: ObservationConfidence::classification_evidence(0.75, 0.70),
                observation_refs: vec![
                    "urn:astra:observation:graph:reflect:test-sess:diagnosis:0".into(),
                ],
            }],
            failure_clusters: vec![ObservationFailureCluster {
                cluster_ref:
                    "urn:astra:failure_cluster:graph:reflect:test-sess:resource_limit:bash".into(),
                label: "resource_limit_bash".into(),
                summary: "resource_limit affected bash 3 times".into(),
                observation_refs: vec![
                    "urn:astra:observation:graph:reflect:test-sess:diagnosis:0".into(),
                ],
                evidence_class: "inferred_evidence".into(),
                confidence: ObservationConfidence::classification_evidence(0.84, 0.90),
            }],
            graph_slice: ObservationGraphSlice::default(),
            budget_result: ObservationBudgetResult::default(),
            overview: make_overview(10, 1, vec![("bash".into(), 8)], 2, Some(5.0)),
            diagnoses: vec![Diagnosis {
                category: astra_core::ErrorKind::ResourceLimit,
                severity: "critical".into(),
                summary: "fork failed".into(),
                samples: vec!["fork: Resource temporarily unavailable".into()],
                occurrences: 3,
                affected_tool: "bash".into(),
                fix_hint: "check ulimit -u".into(),
            }],
            insights: vec![Insight {
                severity: "info".into(),
                category: "performance".into(),
                message: "test".into(),
                evidence: "test evidence".into(),
            }],
            recommendations: vec!["do something".into()],
        };
        assert_refs_are_valid(
            &report.observations,
            &report.evidence,
            &report.action_hints,
            &report.failure_clusters,
        );
        let json = serde_json::to_string(&report).unwrap();
        let json_value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(json_value["schema_version"], 1);
        assert_eq!(json_value["tool"], "reflect");
        assert_eq!(json_value["topic"], json_value["view"]["topic"]);
        assert_eq!(json_value["facet"], json_value["view"]["facet"]);
        assert_eq!(json_value["depth"], json_value["view"]["depth"]);
        assert_eq!(json_value["horizon"], json_value["view"]["horizon"]);
        assert_eq!(
            json_value["data_coverage"],
            json_value["view"]["data_coverage"]
        );
        assert!(
            json_value.get("adaptation_signals").is_none(),
            "reflect reports must not embed tuning/adaptation signals in the observation report"
        );
        assert!(
            json_value.get("causal_chains").is_none(),
            "reflect reports must not expose unused causal-chain placeholders"
        );
        assert!(
            json_value.get("graph_slice").is_some(),
            "reflect reports must expose the shared graph_slice envelope"
        );
        assert!(
            json_value.get("evidence_graph").is_none(),
            "legacy evidence_graph must not be part of the public reflect report"
        );
        assert!(
            json_value.get("reflection_context").is_none(),
            "legacy reflection_context must not be part of the public reflect report"
        );
        assert!(
            json_value.get("prompt_preview").is_none(),
            "legacy prompt_preview must not be part of the public reflect report"
        );
        let parsed: ReflectReport = serde_json::from_str(&json).unwrap();
        assert_eq!(report, parsed);
    }

    #[test]
    fn build_evidence_graph_attaches_decisions_to_related_events() {
        let decisions = vec![EvidenceDecision {
            decision_id: "d1".into(),
            event_id: "evt-user".into(),
            decision_type: "tool_surface".into(),
            decision_output: serde_json::json!({"tool_calls":["bash"]}),
            created_at: "2026-04-12T10:00:01".into(),
        }];
        let events = vec![
            EvidenceEvent {
                event_id: "evt-user".into(),
                event_type: "user_query".into(),
                content: "list files".into(),
                skill_name: None,
                parent_event_id: None,
                causal_chain_id: Some("chain-1".into()),
                created_at: "2026-04-12T10:00:00".into(),
            },
            EvidenceEvent {
                event_id: "evt-tool".into(),
                event_type: "tool_result".into(),
                content: "listed files".into(),
                skill_name: Some("bash".into()),
                parent_event_id: Some("evt-user".into()),
                causal_chain_id: Some("chain-1".into()),
                created_at: "2026-04-12T10:00:02".into(),
            },
        ];
        let parent_id_map = std::collections::HashMap::from([(
            "evt-tool".to_string(),
            vec!["evt-user".to_string()],
        )]);

        let graph = build_evidence_graph(&decisions, &events, &parent_id_map).expect("graph");
        assert_eq!(graph.nodes.len(), 3);
        assert!(graph.edges.iter().any(|edge| {
            edge.from == graph_event_ref("evt-user")
                && edge.to == graph_decision_ref("d1")
                && edge.kind == ObservationGraphEdgeKind::Supports
        }));
        assert!(graph.edges.iter().any(|edge| {
            edge.from == graph_decision_ref("d1")
                && edge.to == graph_event_ref("evt-tool")
                && edge.kind == ObservationGraphEdgeKind::Causes
        }));
    }

    #[test]
    fn evidence_graph_keeps_event_only_failure_chains() {
        let events = vec![
            EvidenceEvent {
                event_id: "evt-call".into(),
                event_type: "tool_call".into(),
                content: "run bash".into(),
                skill_name: Some("bash".into()),
                parent_event_id: None,
                causal_chain_id: Some("chain-1".into()),
                created_at: "2026-04-12T10:00:00".into(),
            },
            EvidenceEvent {
                event_id: "evt-error".into(),
                event_type: "tool_error".into(),
                content: "permission denied".into(),
                skill_name: Some("bash".into()),
                parent_event_id: Some("evt-call".into()),
                causal_chain_id: Some("chain-1".into()),
                created_at: "2026-04-12T10:00:01".into(),
            },
        ];
        let parent_id_map = std::collections::HashMap::from([(
            "evt-error".to_string(),
            vec!["evt-call".to_string()],
        )]);
        let filtered = filter_evidence_events_for_graph(&[], events, &parent_id_map);

        let graph = build_evidence_graph(&[], &filtered, &parent_id_map)
            .expect("event-only failures still produce graph evidence");
        assert_eq!(graph.nodes.len(), 2);
        assert!(graph.nodes.iter().any(|node| {
            node.ref_id == graph_event_ref("evt-error")
                && node.kind == ObservationGraphNodeKind::Outcome
        }));
        assert!(graph.edges.iter().any(|edge| {
            edge.from == graph_event_ref("evt-call")
                && edge.to == graph_event_ref("evt-error")
                && edge.kind == ObservationGraphEdgeKind::Causes
        }));
    }

    /// Validate that all GROUP BY queries only SELECT grouped columns or aggregate functions.
    /// This prevents MatrixOne strict SQL standard errors (MySQL non-strict mode hides these).
    #[test]
    fn sql_group_by_compliance() {
        // All the SQL queries used in build_evidence, extracted as constants for testing.
        let queries = [
            // event types
            "SELECT event_type, COUNT(*) AS cnt \
             FROM agent_events WHERE session_id = ? AND user_id = ? \
             GROUP BY event_type ORDER BY cnt DESC LIMIT 5",
            // skills
            "SELECT skill_name, COUNT(*) AS cnt \
             FROM agent_events WHERE session_id = ? AND user_id = ? AND skill_name IS NOT NULL \
             GROUP BY skill_name ORDER BY cnt DESC LIMIT 5",
            // decisions
            "SELECT d.decision_type, COUNT(*) AS cnt, \
               COUNT(DISTINCT d.model_used) AS models_used \
             FROM ctx_decision_audits d \
             JOIN agent_events e ON e.event_id = d.event_id AND e.session_id = d.session_id AND e.user_id = ? \
             WHERE d.user_id = ? AND d.session_id = ? \
             GROUP BY d.decision_type ORDER BY cnt DESC LIMIT 5",
            // error patterns
            "SELECT IFNULL(skill_name, 'unknown') AS skill_name, event_type, COUNT(*) AS fail_count, \
               SUBSTRING(COALESCE(MIN(content), ''), 1, 100) AS sample_error \
             FROM agent_events \
             WHERE session_id = ? AND user_id = ? AND (event_type IN ('error', 'tool_error', 'stall_detected') \
               OR event_type LIKE '%error%' OR event_type LIKE '%fail%') \
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

    // ── Error classification tests ──────────────────────────────────────
    //
    // `classify_error` is the single entry point for `(content, event_type)`
    // pairs. It returns `astra_core::ErrorKind`. Exhaustive per-variant
    // coverage lives in astra-core's error_kind tests; here we only verify
    // the service-level behaviour (stall short-circuit, DB detection, and
    // delegation to core).

    #[test]
    fn classify_error_cases() {
        use astra_core::ErrorKind as K;
        let cases: &[(&str, &str, K)] = &[
            (
                "fork: Resource temporarily unavailable",
                "tool_error",
                K::ResourceLimit,
            ),
            (
                "Cannot allocate memory (ENOMEM)",
                "tool_error",
                K::ResourceLimit,
            ),
            ("HTTP 403: Unauthorized access", "tool_error", K::Auth),
            (
                "token expired, please re-authenticate",
                "tool_error",
                K::Auth,
            ),
            ("connection refused", "tool_error", K::Network),
            (
                "error sending request for url (x)",
                "tool_error",
                K::Network,
            ),
            (
                "No such file or directory (os error 2)",
                "tool_error",
                K::ToolNotFound,
            ),
            (
                "Path does not exist: /foo/bar",
                "tool_error",
                K::ToolNotFound,
            ),
            ("Missing 'path' parameter", "tool_error", K::ToolInvalidArgs),
            (
                "old_str not found in the file",
                "tool_error",
                K::ToolInvalidArgs,
            ),
            (
                "SQL syntax error: column must appear in GROUP BY",
                "tool_error",
                K::DatabaseError,
            ),
            (
                "sqlx: connection pool timed out",
                "tool_error",
                K::DatabaseError,
            ),
            ("operation deadline exceeded", "tool_error", K::ToolTimeout),
            ("something completely unexpected", "tool_error", K::Unknown),
        ];
        for &(content, event_type, expected) in cases {
            assert_eq!(
                classify_error(content, event_type),
                expected,
                "classify_error({content:?}, {event_type:?})"
            );
        }
    }

    // ── Diagnosis builder tests ─────────────────────────────────────────

    #[test]
    fn diagnoses_group_by_category_and_tool() {
        let errors = vec![
            RawError {
                skill_name: "bash".into(),
                event_type: "tool_error".into(),
                content: "fork: Resource temporarily unavailable".into(),
            },
            RawError {
                skill_name: "bash".into(),
                event_type: "tool_error".into(),
                content: "fork: Resource temporarily unavailable".into(),
            },
            RawError {
                skill_name: "grep".into(),
                event_type: "tool_error".into(),
                content: "No such file or directory".into(),
            },
        ];
        let diags = build_diagnoses(&errors);
        // Two groups: ResourceLimit/bash, FileNotFound/grep
        assert_eq!(diags.len(), 2);
        assert!(
            diags
                .iter()
                .any(|d| d.category == astra_core::ErrorKind::ResourceLimit
                    && d.affected_tool == "bash"
                    && d.occurrences == 2)
        );
        assert!(
            diags
                .iter()
                .any(|d| d.category == astra_core::ErrorKind::ToolNotFound
                    && d.affected_tool == "grep"
                    && d.occurrences == 1)
        );
    }

    #[test]
    fn diagnoses_critical_for_resource_limit() {
        let errors = vec![RawError {
            skill_name: "bash".into(),
            event_type: "tool_error".into(),
            content: "fork: Resource temporarily unavailable".into(),
        }];
        let diags = build_diagnoses(&errors);
        assert_eq!(diags[0].severity, "critical");
    }

    #[test]
    fn diagnoses_fix_hint_contains_ulimit() {
        let errors = vec![RawError {
            skill_name: "bash".into(),
            event_type: "tool_error".into(),
            content: "fork: Resource temporarily unavailable".into(),
        }];
        let diags = build_diagnoses(&errors);
        assert_eq!(
            diags[0].fix_hint,
            astra_core::ErrorKind::ResourceLimit.diagnosis_hint()
        );
    }

    #[test]
    fn diagnoses_samples_deduped_and_limited() {
        let errors: Vec<RawError> = (0..10)
            .map(|_| RawError {
                skill_name: "bash".into(),
                event_type: "tool_error".into(),
                content: "fork: Resource temporarily unavailable".into(),
            })
            .collect();
        let diags = build_diagnoses(&errors);
        // Same content → only 1 unique sample
        assert_eq!(diags[0].samples.len(), 1);
        assert_eq!(diags[0].occurrences, 10);
    }

    #[test]
    fn diagnoses_empty_on_no_errors() {
        assert!(build_diagnoses(&[]).is_empty());
    }

    #[test]
    fn diagnoses_sorted_critical_first() {
        let errors = vec![
            RawError {
                skill_name: "grep".into(),
                event_type: "tool_error".into(),
                content: "No such file or directory".into(),
            },
            RawError {
                skill_name: "bash".into(),
                event_type: "tool_error".into(),
                content: "fork: Resource temporarily unavailable".into(),
            },
        ];
        let diags = build_diagnoses(&errors);
        assert_eq!(diags[0].category, astra_core::ErrorKind::ResourceLimit);
    }

    #[test]
    fn recommendations_from_diagnoses() {
        let overview = make_overview(50, 3, vec![], 5, None);
        let diags = vec![Diagnosis {
            category: astra_core::ErrorKind::ResourceLimit,
            severity: "critical".into(),
            summary: "fork failed".into(),
            samples: vec![],
            occurrences: 3,
            affected_tool: "bash".into(),
            fix_hint: "Check ulimit -u".into(),
        }];
        let recs = generate_recommendations(&overview, &diags, &[]);
        assert_eq!(recs, vec!["Check ulimit -u"]);
    }

    #[test]
    fn contradiction_bash_fails_but_http_works() {
        // The user's real scenario: bash "fork" fails but memory_store (HTTP) works
        let errors = vec![
            RawError {
                skill_name: "bash".into(),
                event_type: "tool_error".into(),
                content: "fork: Resource temporarily unavailable".into(),
            },
            // memory_store succeeds — no error, not in the error list
        ];
        let diags = build_diagnoses(&errors);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].category, astra_core::ErrorKind::ResourceLimit);
        assert_eq!(
            diags[0].fix_hint,
            astra_core::ErrorKind::ResourceLimit.diagnosis_hint()
        );
    }

    #[test]
    fn classify_stall_by_event_type() {
        assert_eq!(
            classify_error("some content", "stall_detected"),
            astra_core::ErrorKind::Stall
        );
        // Even if content looks like network error, event_type takes priority
        assert_eq!(
            classify_error("connection refused", "stall_detected"),
            astra_core::ErrorKind::Stall
        );
    }

    #[test]
    fn diagnoses_stall_detected() {
        let errors = vec![
            RawError {
                skill_name: "system".into(),
                event_type: "stall_detected".into(),
                content: "Agent repeated same tool call 3 times".into(),
            },
            RawError {
                skill_name: "system".into(),
                event_type: "stall_detected".into(),
                content: "Agent repeated same tool call 3 times".into(),
            },
            RawError {
                skill_name: "system".into(),
                event_type: "stall_detected".into(),
                content: "Agent stuck in loop".into(),
            },
        ];
        let diags = build_diagnoses(&errors);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].category, astra_core::ErrorKind::Stall);
        assert_eq!(diags[0].occurrences, 3);
        assert_eq!(diags[0].severity, "warning"); // 3 stalls = warning
        assert_eq!(
            diags[0].fix_hint,
            astra_core::ErrorKind::Stall.diagnosis_hint()
        );
    }

    #[test]
    fn diagnoses_mixed_stall_and_errors() {
        let errors = vec![
            RawError {
                skill_name: "system".into(),
                event_type: "stall_detected".into(),
                content: "Agent looping".into(),
            },
            RawError {
                skill_name: "bash".into(),
                event_type: "tool_error".into(),
                content: "fork: Resource temporarily unavailable".into(),
            },
        ];
        let diags = build_diagnoses(&errors);
        assert_eq!(diags.len(), 2);
        // ResourceLimit is critical → sorted first
        assert_eq!(diags[0].category, astra_core::ErrorKind::ResourceLimit);
        assert_eq!(diags[1].category, astra_core::ErrorKind::Stall);
    }

    // ── P0.1 contract: taxonomy is a single ErrorKind source ─────────────

    #[test]
    fn diagnosis_fix_hint_comes_from_error_kind() {
        // The fix_hint on every diagnosis must match the canonical
        // `ErrorKind::diagnosis_hint()` output. This is the contract that
        // makes the taxonomy the single source of truth for operator advice.
        let errors = vec![
            RawError {
                skill_name: "bash".into(),
                event_type: "tool_error".into(),
                content: "fork: Resource temporarily unavailable".into(),
            },
            RawError {
                skill_name: "matrixone".into(),
                event_type: "tool_error".into(),
                content: "SQL syntax error: column must appear in GROUP BY".into(),
            },
            RawError {
                skill_name: "system".into(),
                event_type: "stall_detected".into(),
                content: "Agent looping".into(),
            },
        ];
        let diags = build_diagnoses(&errors);
        for d in &diags {
            assert_eq!(
                d.fix_hint,
                d.category.diagnosis_hint(),
                "fix_hint diverged from ErrorKind::diagnosis_hint for {:?}",
                d.category
            );
        }
    }

    #[test]
    fn database_errors_always_critical_regardless_of_count() {
        // DatabaseError signals data-integrity risk; a single occurrence
        // must still be critical (not info). Same rule as ResourceLimit.
        let errors = vec![RawError {
            skill_name: "matrixone".into(),
            event_type: "tool_error".into(),
            content: "SQL syntax error: column must appear in GROUP BY".into(),
        }];
        let diags = build_diagnoses(&errors);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].category, astra_core::ErrorKind::DatabaseError);
        assert_eq!(diags[0].severity, "critical");
    }
}
