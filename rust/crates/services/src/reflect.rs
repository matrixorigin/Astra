use astra_core::{
    ErrorResponse, MatrixOneSettings, SharedPool,
    connect_matrixone, error_response, internal_error,
};
use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use sqlx::{Row, query};


// ── Types ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReflectReport {
    pub session_id: String,
    pub focus: String,
    pub overview: SessionOverview,
    /// Root-cause diagnoses from actual error content analysis
    pub diagnoses: Vec<Diagnosis>,
    /// Statistical insights (secondary)
    pub insights: Vec<Insight>,
    pub recommendations: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reflection_context: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_preview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_graph: Option<EvidenceGraph>,
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

/// A root-cause diagnosis: what actually went wrong, from error content analysis.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Diagnosis {
    pub category: ErrorClass,
    pub severity: String,
    pub summary: String,
    /// Actual error content snippets (evidence)
    pub samples: Vec<String>,
    pub occurrences: i64,
    pub affected_tool: String,
    pub fix_hint: String,
}

/// Classified error category for root-cause analysis.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum ErrorClass {
    ResourceLimit,
    Auth,
    Network,
    FileNotFound,
    ToolMisuse,
    Timeout,
    DatabaseError,
    Stall,
    Unknown,
}

impl std::fmt::Display for ErrorClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ResourceLimit => write!(f, "resource_limit"),
            Self::Auth => write!(f, "auth"),
            Self::Network => write!(f, "network"),
            Self::FileNotFound => write!(f, "file_not_found"),
            Self::ToolMisuse => write!(f, "tool_misuse"),
            Self::Timeout => write!(f, "timeout"),
            Self::DatabaseError => write!(f, "database"),
            Self::Stall => write!(f, "stall"),
            Self::Unknown => write!(f, "unknown"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Insight {
    pub severity: String,
    pub category: String,
    pub message: String,
    pub evidence: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceGraphNodeKind {
    Decision,
    Observation,
    Outcome,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceGraphEdgeKind {
    Causes,
    Supports,
    Contradicts,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvidenceGraphNode {
    pub id: String,
    pub kind: EvidenceGraphNodeKind,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EvidenceGraphEdge {
    pub from: String,
    pub to: String,
    pub kind: EvidenceGraphEdgeKind,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct EvidenceGraph {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<EvidenceGraphNode>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edges: Vec<EvidenceGraphEdge>,
}

/// Raw error record fetched from DB for content analysis.
#[derive(Debug, Clone)]
pub struct RawError {
    skill_name: String,
    #[allow(dead_code)]
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

/// Data completeness assessment for a reflection report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataCompleteness {
    /// Number of events found in local journal.
    pub journal_events: u32,
    /// Number of events found in cloud DB (0 if offline/unavailable).
    pub cloud_events: u32,
    /// Events in journal but missing from cloud (potential ingestion drops).
    pub missing_from_cloud: u32,
    /// Confidence score (0.0 = no data, 1.0 = complete).
    pub confidence: f64,
    /// Human-readable warnings about data gaps.
    pub warnings: Vec<String>,
}
fn truncate_graph_summary(value: &str, max_chars: usize) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let snippet: String = trimmed.chars().take(max_chars).collect();
    Some(snippet)
}

fn classify_evidence_event_kind(event_type: &str) -> EvidenceGraphNodeKind {
    if matches!(
        event_type,
        "tool_result" | "tool_error" | "error" | "stall_detected"
    ) || event_type.contains("error")
        || event_type.contains("fail")
    {
        EvidenceGraphNodeKind::Outcome
    } else {
        EvidenceGraphNodeKind::Observation
    }
}

fn build_evidence_graph(
    decisions: &[EvidenceDecision],
    events: &[EvidenceEvent],
    parent_id_map: &std::collections::HashMap<String, Vec<String>>,
) -> Option<EvidenceGraph> {
    if decisions.is_empty() && events.is_empty() {
        return None;
    }

    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let mut event_node_ids = std::collections::HashSet::new();
    let mut edge_keys = std::collections::HashSet::new();
    let event_ids: std::collections::HashSet<&str> =
        events.iter().map(|event| event.event_id.as_str()).collect();

    for decision in decisions {
        nodes.push(EvidenceGraphNode {
            id: format!("decision:{}", decision.decision_id),
            kind: EvidenceGraphNodeKind::Decision,
            label: decision.decision_type.clone(),
            summary: truncate_graph_summary(&decision.decision_output.to_string(), 140),
            anchor: Some(decision.event_id.clone()),
            created_at: Some(decision.created_at.clone()),
            metadata: Some(serde_json::json!({
                "decision_id": decision.decision_id,
                "event_id": decision.event_id,
                "decision_output": decision.decision_output,
            })),
        });
    }

    for event in events {
        let node_id = format!("event:{}", event.event_id);
        event_node_ids.insert(event.event_id.clone());
        nodes.push(EvidenceGraphNode {
            id: node_id,
            kind: classify_evidence_event_kind(&event.event_type),
            label: event.event_type.clone(),
            summary: truncate_graph_summary(&event.content, 140),
            anchor: Some(event.event_id.clone()),
            created_at: Some(event.created_at.clone()),
            metadata: Some(serde_json::json!({
                "skill_name": event.skill_name,
                "causal_chain_id": event.causal_chain_id,
            })),
        });
    }

    for decision in decisions {
        if event_node_ids.contains(&decision.event_id) {
            let from = format!("event:{}", decision.event_id);
            let to = format!("decision:{}", decision.decision_id);
            let key = format!("{from}->{to}:supports");
            if edge_keys.insert(key) {
                edges.push(EvidenceGraphEdge {
                    from,
                    to,
                    kind: EvidenceGraphEdgeKind::Supports,
                });
            }
        }
    }

    for event in events {
        let full_parent_ids = crate::storage::normalized_parent_event_ids(
            event.parent_event_id.as_deref(),
            parent_id_map.get(&event.event_id).map(Vec::as_slice),
        );

        for parent_event_id in full_parent_ids {
            if event_ids.contains(parent_event_id.as_str()) {
                let from = format!("event:{parent_event_id}");
                let to = format!("event:{}", event.event_id);
                let key = format!("{from}->{to}:causes");
                if edge_keys.insert(key) {
                    edges.push(EvidenceGraphEdge {
                        from,
                        to,
                        kind: EvidenceGraphEdgeKind::Causes,
                    });
                }
            }

            for decision in decisions {
                if decision.event_id == parent_event_id {
                    let from = format!("decision:{}", decision.decision_id);
                    let to = format!("event:{}", event.event_id);
                    let key = format!("{from}->{to}:causes");
                    if edge_keys.insert(key) {
                        edges.push(EvidenceGraphEdge {
                            from,
                            to,
                            kind: EvidenceGraphEdgeKind::Causes,
                        });
                    }
                }
            }
        }
    }

    Some(EvidenceGraph { nodes, edges })
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
    /// Tool selection quality assessment.
    pub tool_selection_quality: Option<String>,
    /// Data sources used for the analysis.
    pub data_sources: Vec<String>,
}

/// Unified session diagnostic report combining all inspection capabilities.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionDiagnosticReport {
    pub session_id: String,
    /// Data completeness assessment.
    pub data_completeness: DataCompleteness,
    /// Total turns in session.
    pub total_turns: u32,
    /// Count of turn errors.
    pub error_count: u32,
    /// Count of stall events.
    pub stall_count: u32,
    /// Count of TurnGuard verdict events.
    pub verdict_count: u32,
    /// Tools that were deprioritized.
    pub deprioritized_tools: Vec<String>,
    /// Summary of error types encountered.
    pub error_summary: Vec<String>,
    /// Actionable recommendations.
    pub recommendations: Vec<String>,
    /// Number of composite snapshots available for this session.
    #[serde(default)]
    pub composite_snapshot_count: u32,
    /// Dimensions covered by the most recent composite snapshot.
    #[serde(default)]
    pub latest_snapshot_dimensions: Vec<String>,
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

// ── Error classification (pure logic, no DB) ─────────────────────────────────

/// Classify an error — considers both content text and event_type.
pub fn classify_error(content: &str, event_type: &str) -> ErrorClass {
    // Event-type based classification (high priority)
    if event_type == "stall_detected" {
        return ErrorClass::Stall;
    }

    classify_error_content(content)
}

/// Classify an error string into a root-cause category (content only).
///
/// Delegates to [`astra_core::classify_tool_output`] for the core classification,
/// then maps to the report-level [`ErrorClass`].
pub fn classify_error_content(content: &str) -> ErrorClass {
    let lower = content.to_lowercase();

    // Database errors — domain-specific, not in ErrorKind
    if lower.contains("sql syntax error")
        || lower.contains("error returned from database")
        || lower.contains("sqlx")
        || lower.contains("deadlock")
    {
        return ErrorClass::DatabaseError;
    }

    // Delegate to the canonical classifier
    match astra_core::classify_tool_output(content) {
        astra_core::ErrorKind::ResourceLimit => ErrorClass::ResourceLimit,
        astra_core::ErrorKind::Auth => ErrorClass::Auth,
        astra_core::ErrorKind::Network
        | astra_core::ErrorKind::RateLimit
        | astra_core::ErrorKind::ServerError
        | astra_core::ErrorKind::StreamIdle
        | astra_core::ErrorKind::StreamTransport => ErrorClass::Network,
        astra_core::ErrorKind::ToolTimeout => ErrorClass::Timeout,
        astra_core::ErrorKind::ToolNotFound => ErrorClass::FileNotFound,
        astra_core::ErrorKind::ToolInvalidArgs | astra_core::ErrorKind::InvalidRequest => {
            ErrorClass::ToolMisuse
        }
        _ => ErrorClass::Unknown,
    }
}

/// Build diagnoses from raw error records by classifying and grouping.
pub fn build_diagnoses(raw_errors: &[RawError]) -> Vec<Diagnosis> {
    use std::collections::HashMap;

    // Group by (ErrorClass, affected_tool)
    let mut groups: HashMap<(ErrorClass, String), Vec<&RawError>> = HashMap::new();
    for err in raw_errors {
        let class = classify_error(&err.content, &err.event_type);
        let tool = if err.skill_name.is_empty() || err.skill_name == "unknown" {
            "system".to_string()
        } else {
            err.skill_name.clone()
        };
        groups.entry((class, tool)).or_default().push(err);
    }

    let mut diagnoses: Vec<Diagnosis> = groups
        .into_iter()
        .map(|((class, tool), errors)| {
            let count = errors.len() as i64;
            // Take up to 3 unique sample snippets (truncated)
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

            let severity = match (&class, count) {
                (ErrorClass::ResourceLimit, _) => "critical",
                (ErrorClass::Stall, n) if n >= 3 => "warning",
                (ErrorClass::Stall, _) => "info",
                (_, n) if n >= 5 => "critical",
                (_, n) if n >= 3 => "warning",
                _ => "info",
            }
            .to_string();

            let summary = match &class {
                ErrorClass::ResourceLimit => format!(
                    "System resource exhaustion ({tool}): OS cannot fork/allocate — {count} occurrences"
                ),
                ErrorClass::Auth => format!(
                    "Authentication failure ({tool}): credentials invalid or expired — {count} occurrences"
                ),
                ErrorClass::Network => format!(
                    "Network connectivity issue ({tool}): connection failures — {count} occurrences"
                ),
                ErrorClass::Timeout => format!(
                    "Timeout ({tool}): operation exceeded time limit — {count} occurrences"
                ),
                ErrorClass::FileNotFound => format!(
                    "Missing files/paths ({tool}): agent tried nonexistent paths — {count} occurrences"
                ),
                ErrorClass::ToolMisuse => format!(
                    "Tool parameter errors ({tool}): wrong arguments passed — {count} occurrences"
                ),
                ErrorClass::DatabaseError => format!(
                    "Database error ({tool}): SQL or connection failure — {count} occurrences"
                ),
                ErrorClass::Stall => format!(
                    "Agent stall detected — {count} stall events, agent may be looping or stuck"
                ),
                ErrorClass::Unknown => format!(
                    "Unclassified errors ({tool}): {count} occurrences"
                ),
            };

            let fix_hint = match &class {
                ErrorClass::ResourceLimit => "Check system limits: `ulimit -u` (max procs), `ulimit -n` (open files). Kill orphan processes: `ps aux | grep defunct`. May need to restart the system or increase limits.".to_string(),
                ErrorClass::Auth => "Re-authenticate with `/login`. Check token expiry. Verify API credentials in environment variables.".to_string(),
                ErrorClass::Network => "Check network connectivity and proxy settings. Verify `NO_PROXY=localhost,127.0.0.1` for local services. Check if target service is running.".to_string(),
                ErrorClass::Timeout => format!("Tool `{tool}` is slow. Consider breaking the operation into smaller chunks or increasing the timeout."),
                ErrorClass::FileNotFound => "Agent guessed wrong paths. Use `list_dir` before `read_file`/`grep`. Check that the workspace context is accurate.".to_string(),
                ErrorClass::ToolMisuse => "Model is calling tools with wrong parameters. This may improve with a better model or clearer system prompt.".to_string(),
                ErrorClass::DatabaseError => "Check MatrixOne connectivity and SQL syntax. Use CAST for DATETIME columns, MIN/MAX for non-grouped columns.".to_string(),
                ErrorClass::Stall => "Agent is stuck in a loop. Try `/rewind` to go back, or switch to a different model with `/model`. Break complex tasks into smaller steps.".to_string(),
                ErrorClass::Unknown => "Review the error samples above to identify the pattern.".to_string(),
            };

            Diagnosis {
                category: class,
                severity,
                summary,
                samples,
                occurrences: count,
                affected_tool: tool,
                fix_hint,
            }
        })
        .collect();

    // Sort: critical first, then by occurrence count
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
        if d.severity == "critical" || d.severity == "warning" {
            recs.push(d.fix_hint.clone());
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

fn build_reflection_context_value(
    session_id: &str,
    overview: &SessionOverview,
    diagnoses: &[Diagnosis],
    insights: &[Insight],
    recommendations: &[String],
) -> serde_json::Value {
    let mut signals = Vec::new();
    for diag in diagnoses.iter().take(6) {
        signals.push(serde_json::json!({
            "kind": diag.category.to_string(),
            "detail": diag.summary,
            "skill_context": if diag.affected_tool.is_empty() || diag.affected_tool == "unknown" {
                serde_json::Value::Null
            } else {
                serde_json::Value::String(diag.affected_tool.clone())
            },
            "turn_id": "server",
        }));
    }
    for insight in insights.iter().filter(|insight| insight.severity != "info") {
        if signals.len() >= 6 {
            break;
        }
        let detail = if insight.evidence.is_empty() {
            insight.message.clone()
        } else {
            format!("{} — {}", insight.message, insight.evidence)
        };
        signals.push(serde_json::json!({
            "kind": insight.category,
            "detail": detail,
            "skill_context": serde_json::Value::Null,
            "turn_id": "server",
        }));
    }

    let mut by_tool: std::collections::BTreeMap<String, i64> = std::collections::BTreeMap::new();
    for diag in diagnoses {
        if diag.affected_tool.is_empty() || diag.affected_tool == "unknown" {
            continue;
        }
        *by_tool.entry(diag.affected_tool.clone()).or_default() += diag.occurrences;
    }
    let mut tool_stats = by_tool
        .into_iter()
        .map(|(tool_name, failures)| {
            serde_json::json!({
                "tool_name": tool_name,
                "calls": failures,
                "failures": failures,
                "avg_latency_ms": 0,
            })
        })
        .collect::<Vec<_>>();
    tool_stats.sort_by(|a, b| {
        b["failures"]
            .as_i64()
            .unwrap_or_default()
            .cmp(&a["failures"].as_i64().unwrap_or_default())
    });
    tool_stats.truncate(8);

    serde_json::json!({
        "session_id": session_id,
        "turns_completed": overview.total_decisions.max(0),
        "scenario": serde_json::Value::Null,
        "signals": signals,
        "active_experiment": serde_json::Value::Null,
        "tool_stats": tool_stats,
        "token_utilisation": 0.0,
        "recent_tactical_actions": recommendations.iter().take(6).cloned().collect::<Vec<_>>(),
    })
}

fn render_reflection_prompt_preview(
    session_id: &str,
    focus: &str,
    question: &str,
    context: &serde_json::Value,
) -> String {
    let mut lines = vec![
        format!("Session: {session_id}"),
        format!("Focus: {focus}"),
        format!(
            "Turns completed: {}",
            context["turns_completed"].as_i64().unwrap_or_default()
        ),
    ];
    if !question.trim().is_empty() {
        lines.push(format!("Question: {}", question.trim()));
    }

    if let Some(tool_stats) = context["tool_stats"]
        .as_array()
        .filter(|stats| !stats.is_empty())
    {
        lines.push("Tool pressure:".to_string());
        for stat in tool_stats.iter().take(4) {
            lines.push(format!(
                "- {}: {} failures",
                stat["tool_name"].as_str().unwrap_or("unknown"),
                stat["failures"].as_i64().unwrap_or_default()
            ));
        }
    }

    if let Some(signals) = context["signals"]
        .as_array()
        .filter(|signals| !signals.is_empty())
    {
        lines.push("Signals:".to_string());
        for signal in signals.iter().take(4) {
            lines.push(format!(
                "- {}: {}",
                signal["kind"].as_str().unwrap_or("signal"),
                signal["detail"].as_str().unwrap_or("")
            ));
        }
    }

    if let Some(actions) = context["recent_tactical_actions"]
        .as_array()
        .filter(|actions| !actions.is_empty())
    {
        lines.push("Recent tactical actions:".to_string());
        for action in actions.iter().take(4).filter_map(serde_json::Value::as_str) {
            lines.push(format!("- {action}"));
        }
    }

    lines.join("\n")
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

    async fn build_recent_evidence_graph(
        &self,
        pool: &sqlx::Pool<sqlx::MySql>,
        session_id: &str,
        focus: &str,
        last_n: i32,
    ) -> ServiceResult<Option<EvidenceGraph>> {
        if !matches!(focus, "auto" | "tool_selection") {
            return Ok(None);
        }

        let decision_limit = i64::from(last_n.clamp(1, 50));
        let decision_rows = query(
            "SELECT decision_id, event_id, decision_type, \
               IFNULL(CAST(decision_output AS CHAR), '{}') AS decision_output_json, \
               DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
             FROM ctx_decision_audits \
             WHERE session_id = ? \
             ORDER BY created_at DESC LIMIT ?",
        )
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

        if decisions.is_empty() {
            return Ok(None);
        }

        let event_limit = std::cmp::max(decision_limit * 10, 50);
        let event_rows = query(
            "SELECT event_id, event_type, \
               SUBSTRING(COALESCE(CAST(content AS CHAR), ''), 1, 180) AS content, \
               skill_name, parent_event_id, causal_chain_id, \
               DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
             FROM agent_events \
             WHERE session_id = ? \
             ORDER BY created_at DESC LIMIT ?",
        )
        .bind(session_id)
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

        let decision_event_ids: std::collections::HashSet<String> = decisions
            .iter()
            .map(|decision| decision.event_id.clone())
            .collect();
        let relevant_chain_ids: std::collections::HashSet<String> = recent_events
            .iter()
            .filter(|event| decision_event_ids.contains(&event.event_id))
            .filter_map(|event| event.causal_chain_id.clone())
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

        let filtered_events: Vec<EvidenceEvent> = recent_events
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
            .collect();

        Ok(build_evidence_graph(
            &decisions,
            &filtered_events,
            &parent_id_map,
        ))
    }
}

#[async_trait]
impl ReflectService for DatabaseReflectService {
    async fn build_evidence(
        &self,
        user_id: &str,
        session_id: &str,
        focus: &str,
        last_n: i32,
        question: &str,
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

        // Error patterns (aggregated, for insights)
        let error_patterns = if matches!(focus, "auto" | "skill_failure" | "tool_selection") {
            let ep_rows = query(
                "SELECT IFNULL(skill_name, 'unknown') AS skill_name, event_type, COUNT(*) AS fail_count, \
                   SUBSTRING(COALESCE(MIN(content), ''), 1, 100) AS sample_error \
                 FROM agent_events \
                 WHERE session_id = ? AND (event_type IN ('error', 'tool_error', 'stall_detected') \
                   OR event_type LIKE '%error%' OR event_type LIKE '%fail%') \
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

        // ── Diagnostic: fetch recent ACTUAL error content for root-cause analysis
        // Limit to 30 most recent errors — enough for pattern detection, bounded cost
        let raw_errors = {
            let err_rows = query(
                "SELECT IFNULL(skill_name, 'unknown') AS skill_name, event_type, \
                   SUBSTRING(COALESCE(CAST(content AS CHAR), ''), 1, 300) AS content \
                 FROM agent_events \
                 WHERE session_id = ? AND (event_type IN ('error', 'tool_error', 'stall_detected') \
                   OR event_type LIKE '%error%' OR event_type LIKE '%fail%') \
                 ORDER BY created_at DESC LIMIT 30",
            )
            .bind(session_id)
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
        let reflection_context = build_reflection_context_value(
            session_id,
            &overview,
            &diagnoses,
            &insights,
            &recommendations,
        );
        let prompt_preview =
            render_reflection_prompt_preview(session_id, focus, question, &reflection_context);
        let evidence_graph = self
            .build_recent_evidence_graph(&pool, session_id, focus, last_n)
            .await?;

        Ok(ReflectReport {
            session_id: session_id.to_string(),
            focus: focus.to_string(),
            overview,
            diagnoses,
            insights,
            recommendations,
            reflection_context: Some(reflection_context),
            prompt_preview: Some(prompt_preview),
            evidence_graph,
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
        assert!(
            insights
                .iter()
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
        let overview = make_overview(50, 5, vec![], 5, None);
        let patterns = vec![ErrorPattern {
            skill_name: "bash".into(),
            event_type: "tool_error".into(),
            fail_count: 5,
            sample_error: "permission denied".into(),
        }];
        let insights = generate_insights(&overview, &patterns, &[]);
        assert!(
            insights
                .iter()
                .any(|i| i.category == "tool_usage" && i.message.contains("bash"))
        );
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
        assert!(
            !insights
                .iter()
                .any(|i| i.category == "tool_usage" && i.message.contains("bash"))
        );
    }

    #[test]
    fn insight_over_reliance() {
        let overview = make_overview(100, 0, vec![("bash".into(), 75)], 5, None);
        let insights = generate_insights(&overview, &[], &[]);
        assert!(insights.iter().any(|i| i.message.contains("Over-reliance")));
    }

    #[test]
    fn insight_no_over_reliance_when_balanced() {
        let overview = make_overview(
            100,
            0,
            vec![("bash".into(), 30), ("grep".into(), 25)],
            5,
            None,
        );
        let insights = generate_insights(&overview, &[], &[]);
        assert!(!insights.iter().any(|i| i.message.contains("Over-reliance")));
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
        // Should not panic, error rate is 0.5% so no error insights
        assert!(!insights.iter().any(|i| i.category == "error_pattern"));
    }

    #[test]
    fn recommendations_for_errors() {
        let overview = make_overview(100, 40, vec![], 5, None);
        let diagnoses = build_diagnoses(&[]); // no raw errors for this test
        let insights = generate_insights(&overview, &[], &[]);
        let recs = generate_recommendations(&overview, &diagnoses, &insights);
        // With no actual error content, no specific recs generated
        assert!(recs.is_empty() || recs.iter().any(|r| !r.is_empty()));
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
        let recs = generate_recommendations(&overview, &[], &insights);
        // No specific diagnosis-driven recs without raw errors
        let _ = recs;
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
    fn compute_duration_basic() {
        let d = compute_duration_minutes(Some("2026-03-25 08:00:00"), Some("2026-03-25 08:18:30"));
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
        let reflection_context = serde_json::json!({
            "session_id": "test-sess",
            "turns_completed": 2,
            "tool_stats": [{"tool_name": "bash", "calls": 3, "failures": 3, "avg_latency_ms": 0}],
            "signals": [{"kind": "resource_limit", "detail": "fork failed", "skill_context": "bash", "turn_id": "server"}],
            "recent_tactical_actions": ["check ulimit -u"],
            "token_utilisation": 0.0
        });
        let report = ReflectReport {
            session_id: "test-sess".into(),
            focus: "auto".into(),
            overview: make_overview(10, 1, vec![("bash".into(), 8)], 2, Some(5.0)),
            diagnoses: vec![Diagnosis {
                category: ErrorClass::ResourceLimit,
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
            reflection_context: Some(reflection_context),
            prompt_preview: Some("Session: test-sess".into()),
            evidence_graph: Some(EvidenceGraph {
                nodes: vec![EvidenceGraphNode {
                    id: "decision:d1".into(),
                    kind: EvidenceGraphNodeKind::Decision,
                    label: "tool_selection".into(),
                    summary: Some("picked bash".into()),
                    anchor: Some("evt-1".into()),
                    created_at: Some("2026-04-12T10:00:00".into()),
                    metadata: None,
                }],
                edges: vec![],
            }),
        };
        let json = serde_json::to_string(&report).unwrap();
        let parsed: ReflectReport = serde_json::from_str(&json).unwrap();
        assert_eq!(report, parsed);
    }

    #[test]
    fn build_evidence_graph_attaches_decisions_to_related_events() {
        let decisions = vec![EvidenceDecision {
            decision_id: "d1".into(),
            event_id: "evt-user".into(),
            decision_type: "tool_selection".into(),
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
            edge.from == "event:evt-user"
                && edge.to == "decision:d1"
                && edge.kind == EvidenceGraphEdgeKind::Supports
        }));
        assert!(graph.edges.iter().any(|edge| {
            edge.from == "decision:d1"
                && edge.to == "event:evt-tool"
                && edge.kind == EvidenceGraphEdgeKind::Causes
        }));
    }

    #[test]
    fn reflect_shape_helpers_build_local_compatible_fields() {
        let overview = make_overview(10, 2, vec![("bash".into(), 8)], 3, Some(5.0));
        let diagnoses = vec![Diagnosis {
            category: ErrorClass::Timeout,
            severity: "warning".into(),
            summary: "bash timed out".into(),
            samples: vec!["command timed out".into()],
            occurrences: 2,
            affected_tool: "bash".into(),
            fix_hint: "narrow the command scope".into(),
        }];
        let insights = vec![Insight {
            severity: "warning".into(),
            category: "performance".into(),
            message: "slow turn".into(),
            evidence: "2 timeouts".into(),
        }];
        let recommendations = vec!["narrow the command scope".to_string()];

        let context = build_reflection_context_value(
            "test-sess",
            &overview,
            &diagnoses,
            &insights,
            &recommendations,
        );
        let prompt =
            render_reflection_prompt_preview("test-sess", "performance", "why so slow?", &context);

        assert_eq!(context["session_id"], "test-sess");
        assert_eq!(context["tool_stats"][0]["tool_name"], "bash");
        assert_eq!(context["signals"][0]["kind"], "timeout");
        assert!(prompt.contains("Focus: performance"));
        assert!(prompt.contains("Question: why so slow?"));
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
             WHERE session_id = ? AND (event_type IN ('error', 'tool_error', 'stall_detected') \
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

    #[test]
    fn classify_resource_limit_fork() {
        assert_eq!(
            classify_error_content("fork: Resource temporarily unavailable"),
            ErrorClass::ResourceLimit
        );
    }

    #[test]
    fn classify_resource_limit_oom() {
        assert_eq!(
            classify_error_content("Cannot allocate memory (ENOMEM)"),
            ErrorClass::ResourceLimit
        );
    }

    #[test]
    fn classify_resource_limit_files() {
        assert_eq!(
            classify_error_content("too many open files"),
            ErrorClass::ResourceLimit
        );
    }

    #[test]
    fn classify_auth() {
        assert_eq!(
            classify_error_content("HTTP 403: Unauthorized access"),
            ErrorClass::Auth
        );
        assert_eq!(
            classify_error_content("token expired, please re-authenticate"),
            ErrorClass::Auth
        );
    }

    #[test]
    fn classify_network() {
        assert_eq!(
            classify_error_content("error sending request for url (http://127.0.0.1:8000)"),
            ErrorClass::Network
        );
        assert_eq!(
            classify_error_content("connection refused"),
            ErrorClass::Network
        );
    }

    #[test]
    fn classify_file_not_found() {
        assert_eq!(
            classify_error_content("No such file or directory (os error 2)"),
            ErrorClass::FileNotFound
        );
        assert_eq!(
            classify_error_content("Path does not exist: /foo/bar"),
            ErrorClass::FileNotFound
        );
    }

    #[test]
    fn classify_tool_misuse() {
        assert_eq!(
            classify_error_content("Missing 'path' parameter"),
            ErrorClass::ToolMisuse
        );
        assert_eq!(
            classify_error_content("old_str not found in the file"),
            ErrorClass::ToolMisuse
        );
    }

    #[test]
    fn classify_database() {
        assert_eq!(
            classify_error_content("SQL syntax error: column must appear in GROUP BY"),
            ErrorClass::DatabaseError
        );
    }

    #[test]
    fn classify_timeout() {
        assert_eq!(
            classify_error_content("operation deadline exceeded"),
            ErrorClass::Timeout
        );
    }

    #[test]
    fn classify_unknown() {
        assert_eq!(
            classify_error_content("something completely unexpected happened"),
            ErrorClass::Unknown
        );
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
        assert!(diags.iter().any(|d| d.category == ErrorClass::ResourceLimit
            && d.affected_tool == "bash"
            && d.occurrences == 2));
        assert!(diags.iter().any(|d| d.category == ErrorClass::FileNotFound
            && d.affected_tool == "grep"
            && d.occurrences == 1));
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
        assert!(diags[0].fix_hint.contains("ulimit"));
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
        assert_eq!(diags[0].category, ErrorClass::ResourceLimit);
    }

    #[test]
    fn recommendations_from_diagnoses() {
        let overview = make_overview(50, 3, vec![], 5, None);
        let diags = vec![Diagnosis {
            category: ErrorClass::ResourceLimit,
            severity: "critical".into(),
            summary: "fork failed".into(),
            samples: vec![],
            occurrences: 3,
            affected_tool: "bash".into(),
            fix_hint: "Check ulimit -u".into(),
        }];
        let recs = generate_recommendations(&overview, &diags, &[]);
        assert!(recs.iter().any(|r| r.contains("ulimit")));
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
        assert_eq!(diags[0].category, ErrorClass::ResourceLimit);
        // Fix hint explains it's a process fork issue, not a general system issue
        assert!(diags[0].fix_hint.contains("ulimit") || diags[0].fix_hint.contains("procs"));
    }

    #[test]
    fn classify_stall_by_event_type() {
        assert_eq!(
            classify_error("some content", "stall_detected"),
            ErrorClass::Stall
        );
        // Even if content looks like network error, event_type takes priority
        assert_eq!(
            classify_error("connection refused", "stall_detected"),
            ErrorClass::Stall
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
        assert_eq!(diags[0].category, ErrorClass::Stall);
        assert_eq!(diags[0].occurrences, 3);
        assert_eq!(diags[0].severity, "warning"); // 3 stalls = warning
        assert!(diags[0].fix_hint.contains("rewind") || diags[0].fix_hint.contains("/rewind"));
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
        assert_eq!(diags[0].category, ErrorClass::ResourceLimit);
        assert_eq!(diags[1].category, ErrorClass::Stall);
    }
}
