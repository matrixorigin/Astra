//! Pipeline Learning Writer — bridges TurnLearningWriter trait to pipeline modules.
//!
//! Receives turn outcomes from the bridge side-effects hook and updates:
//! - EntityGraph: entity → domain → tools associations
//! - PatternLibrary: tool chain success/failure patterns
//! - ProgressiveCalibrator: per-intent/domain/task correction rates

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::pipeline::calibration::ProgressiveCalibrator;
use crate::pipeline::entity::{EntityGraph, extract_entities};
use crate::pipeline::pattern::PatternLibrary;
use crate::pipeline::routing::{DomainHint, TaskType};
use crate::turn::contracts::{TurnLearningOutcome, TurnLearningWriter};

/// Concrete implementation of [`TurnLearningWriter`] that updates pipeline modules.
///
/// All modules are optional — if a module is not set, that learning axis is skipped.
/// Thread-safe via `Arc<Mutex<T>>` wrapping.
pub struct PipelineLearningWriter {
    entity_graph: Option<Arc<Mutex<EntityGraph>>>,
    pattern_library: Option<Arc<Mutex<PatternLibrary>>>,
    progressive_calibrator: Option<Arc<Mutex<ProgressiveCalibrator>>>,
}

impl PipelineLearningWriter {
    pub fn new() -> Self {
        Self {
            entity_graph: None,
            pattern_library: None,
            progressive_calibrator: None,
        }
    }

    pub fn with_entity_graph(mut self, graph: Arc<Mutex<EntityGraph>>) -> Self {
        self.entity_graph = Some(graph);
        self
    }

    pub fn with_pattern_library(mut self, library: Arc<Mutex<PatternLibrary>>) -> Self {
        self.pattern_library = Some(library);
        self
    }

    pub fn with_progressive_calibrator(
        mut self,
        calibrator: Arc<Mutex<ProgressiveCalibrator>>,
    ) -> Self {
        self.progressive_calibrator = Some(calibrator);
        self
    }
}

impl Default for PipelineLearningWriter {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse a task_type label string back to the TaskType enum.
fn parse_task_type(label: Option<&str>) -> TaskType {
    match label {
        Some("code") => TaskType::Code,
        Some("reasoning") => TaskType::Reasoning,
        Some("fetch") => TaskType::Fetch,
        Some("mutate") => TaskType::Mutate,
        Some("memory") => TaskType::Memory,
        Some("conversational") => TaskType::Conversational,
        Some("compound") => TaskType::Compound,
        _ => TaskType::Unknown,
    }
}

/// Parse a domain_hint label string back to the DomainHint enum.
fn parse_domain_hint(label: Option<&str>) -> Option<DomainHint> {
    match label {
        Some("github") => Some(DomainHint::GitHub),
        Some("code") => Some(DomainHint::Code),
        Some("memory") => Some(DomainHint::Memory),
        Some("git") => Some(DomainHint::Git),
        Some("system") => Some(DomainHint::System),
        Some("database") => Some(DomainHint::Database),
        _ => None,
    }
}

#[async_trait]
impl TurnLearningWriter for PipelineLearningWriter {
    async fn record_outcome(&self, outcome: TurnLearningOutcome) -> Result<(), String> {
        let task_type = parse_task_type(outcome.task_type_label.as_deref());
        let domain = parse_domain_hint(outcome.domain_hint_label.as_deref());
        let feedback = outcome.user_feedback_score;

        // 1. Entity graph: learn entity → domain → tools (only on success)
        // Pass feedback to modulate confidence growth
        if outcome.success
            && let Some(eg) = &self.entity_graph
        {
            let mut graph = eg.lock().unwrap_or_else(|e| e.into_inner());
            let entities = extract_entities(&outcome.query);
            if let Some(d) = domain {
                for entity in &entities {
                    graph.learn(entity, d, &outcome.tools_used, feedback);
                }
            }
        }

        // 2. Pattern library: record tool chain outcome (both success and failure)
        // Pass feedback to adjust success/quality judgments
        if let Some(pl) = &self.pattern_library {
            let mut lib = pl.lock().unwrap_or_else(|e| e.into_inner());
            lib.record_outcome(
                &outcome.tools_used,
                task_type,
                domain,
                outcome.success,
                outcome.quality,
                feedback,
            );
        }

        // 3. Progressive calibrator: record correction data
        // Pass feedback to convert low satisfaction into correction signal
        if let Some(pc) = &self.progressive_calibrator {
            let mut cal = pc.lock().unwrap_or_else(|e| e.into_inner());
            let intent = format!("{task_type:?}").to_lowercase();
            cal.record(&intent, domain, task_type, outcome.was_corrected, feedback);
        }

        Ok(())
    }
}

// ─── Extraction from Bridge Payload ──────────────────────────────────────────

/// Extract a `TurnLearningOutcome` from the bridge hook payload.
///
/// The payload is the JSON object produced by `build_turn_hook_args()` in the
/// bridge. It contains: messages, tool_calls, tool_results, tool_quality_assessments,
/// routing_meta, etc.
pub fn build_learning_outcome_from_payload(
    payload: &serde_json::Value,
) -> Option<TurnLearningOutcome> {
    let obj = payload.as_object()?;

    // Extract user query from messages
    let query = extract_user_query(obj)?;

    // Extract tool names from tool_calls
    let tools_used = extract_tool_names(obj, "tool_calls");
    let tools_selected = extract_tool_names(obj, "selected_skills")
        .or_else(|| {
            // Fall back to tool_calls if selected_skills not available
            if tools_used.is_some() {
                tools_used.clone()
            } else {
                None
            }
        })
        .unwrap_or_default();
    let tools_used = tools_used.unwrap_or_default();

    // Extract quality from tool_quality_assessments
    let quality = extract_aggregate_quality(obj);

    // Extract routing metadata
    let (task_type_label, domain_hint_label) = extract_routing_labels(obj);

    // Determine success: tools were used and no error indicators
    let success = !tools_used.is_empty() && quality > 0.3;

    // Correction detection: check if this payload indicates a correction turn
    let was_corrected = detect_correction(obj);

    // User feedback score: extracted from payload if available (will be populated
    // later when feedback is submitted via /api/v1/learning/feedback)
    let user_feedback_score = obj.get("user_feedback_score").and_then(|v| v.as_i64());

    Some(TurnLearningOutcome {
        query,
        tools_selected,
        tools_used,
        success,
        quality,
        was_corrected,
        task_type_label,
        domain_hint_label,
        user_feedback_score,
    })
}

fn extract_user_query(obj: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    // Look in messages for the last user message
    let messages = obj.get("messages")?.as_array()?;
    messages
        .iter()
        .rev()
        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
        .and_then(|m| m.get("content").and_then(|c| c.as_str()))
        .map(|s| s.to_string())
}

fn extract_tool_names(
    obj: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Option<Vec<String>> {
    let arr = obj.get(key)?.as_array()?;
    let names: Vec<String> = arr
        .iter()
        .filter_map(|tc| {
            // tool_calls format: {"function": {"name": "..."}}
            tc.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .map(|s| s.to_string())
                // Also handle flat format: {"name": "..."}
                .or_else(|| {
                    tc.get("name")
                        .and_then(|n| n.as_str())
                        .map(|s| s.to_string())
                })
        })
        .collect();
    if names.is_empty() { None } else { Some(names) }
}

fn extract_aggregate_quality(obj: &serde_json::Map<String, serde_json::Value>) -> f64 {
    let assessments = match obj
        .get("tool_quality_assessments")
        .and_then(|v| v.as_array())
    {
        Some(arr) => arr,
        None => return 0.5, // Default quality when no assessments
    };

    if assessments.is_empty() {
        return 0.5;
    }

    let mut total = 0.0_f64;
    let mut count = 0_usize;
    for assessment in assessments {
        if let Some(score) = assessment.get("quality_score").and_then(|v| v.as_f64()) {
            total += score;
            count += 1;
        } else if let Some(grade) = assessment.get("grade").and_then(|v| v.as_str()) {
            // Map grade labels to numeric scores
            let score = match grade {
                "excellent" => 1.0,
                "good" => 0.8,
                "complete" => 0.7,
                "partial" => 0.4,
                "failed" | "error" => 0.1,
                _ => 0.5,
            };
            total += score;
            count += 1;
        }
    }

    if count == 0 {
        0.5
    } else {
        total / count as f64
    }
}

fn extract_routing_labels(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> (Option<String>, Option<String>) {
    let routing = match obj.get("routing_meta").and_then(|v| v.as_object()) {
        Some(r) => r,
        None => return (None, None),
    };
    let task_type = routing
        .get("task_type")
        .and_then(|v| v.as_str())
        .map(|s| s.to_lowercase());
    let domain = routing
        .get("domain_hint")
        .and_then(|v| v.as_str())
        .map(|s| s.to_lowercase());
    (task_type, domain)
}

fn detect_correction(obj: &serde_json::Map<String, serde_json::Value>) -> bool {
    // Heuristic: if the payload includes "is_correction" or "correction" flag
    if let Some(v) = obj.get("is_correction") {
        return v.as_bool().unwrap_or(false);
    }
    // Check routing_meta for correction signals
    if let Some(routing) = obj.get("routing_meta").and_then(|v| v.as_object())
        && let Some(v) = routing.get("is_correction")
    {
        return v.as_bool().unwrap_or(false);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_task_type_known() {
        assert!(matches!(parse_task_type(Some("code")), TaskType::Code));
        assert!(matches!(parse_task_type(Some("fetch")), TaskType::Fetch));
        assert!(matches!(parse_task_type(Some("memory")), TaskType::Memory));
        assert!(matches!(
            parse_task_type(Some("conversational")),
            TaskType::Conversational
        ));
    }

    #[test]
    fn parse_task_type_unknown() {
        assert!(matches!(parse_task_type(None), TaskType::Unknown));
        assert!(matches!(parse_task_type(Some("alien")), TaskType::Unknown));
    }

    #[test]
    fn parse_domain_hint_known() {
        assert_eq!(parse_domain_hint(Some("github")), Some(DomainHint::GitHub));
        assert_eq!(parse_domain_hint(Some("code")), Some(DomainHint::Code));
        assert_eq!(parse_domain_hint(Some("memory")), Some(DomainHint::Memory));
    }

    #[test]
    fn parse_domain_hint_unknown() {
        assert_eq!(parse_domain_hint(None), None);
        assert_eq!(parse_domain_hint(Some("alien")), None);
    }

    #[test]
    fn extract_outcome_from_full_payload() {
        let payload = json!({
            "messages": [
                {"role": "user", "content": "check matrixorigin PRs"},
                {"role": "assistant", "content": "Here are the PRs..."}
            ],
            "tool_calls": [
                {"function": {"name": "github_list_prs"}},
                {"function": {"name": "github_search_repos"}}
            ],
            "tool_quality_assessments": [
                {"grade": "good", "quality_score": 0.85}
            ],
            "routing_meta": {
                "task_type": "fetch",
                "domain_hint": "github"
            }
        });

        let outcome = build_learning_outcome_from_payload(&payload).unwrap();
        assert_eq!(outcome.query, "check matrixorigin PRs");
        assert_eq!(
            outcome.tools_used,
            vec!["github_list_prs", "github_search_repos"]
        );
        assert!((outcome.quality - 0.85).abs() < 0.01);
        assert!(outcome.success);
        assert!(!outcome.was_corrected);
        assert_eq!(outcome.task_type_label.as_deref(), Some("fetch"));
        assert_eq!(outcome.domain_hint_label.as_deref(), Some("github"));
    }

    #[test]
    fn extract_outcome_no_tools_is_low_success() {
        let payload = json!({
            "messages": [
                {"role": "user", "content": "hello"}
            ]
        });

        let outcome = build_learning_outcome_from_payload(&payload).unwrap();
        assert_eq!(outcome.query, "hello");
        assert!(outcome.tools_used.is_empty());
        assert!(!outcome.success); // No tools used → not successful
    }

    #[test]
    fn extract_outcome_no_messages_returns_none() {
        let payload = json!({
            "tool_calls": [{"function": {"name": "bash"}}]
        });
        assert!(build_learning_outcome_from_payload(&payload).is_none());
    }

    #[test]
    fn extract_quality_from_grades() {
        let obj: serde_json::Map<String, serde_json::Value> = serde_json::from_value(json!({
            "tool_quality_assessments": [
                {"grade": "excellent"},
                {"grade": "good"},
                {"grade": "partial"}
            ]
        }))
        .unwrap();
        let quality = extract_aggregate_quality(&obj);
        // (1.0 + 0.8 + 0.4) / 3 = 0.733
        assert!((quality - 0.733).abs() < 0.01);
    }

    #[test]
    fn extract_quality_no_assessments_defaults() {
        let obj: serde_json::Map<String, serde_json::Value> =
            serde_json::from_value(json!({})).unwrap();
        assert!((extract_aggregate_quality(&obj) - 0.5).abs() < 0.01);
    }

    #[test]
    fn correction_detection_from_flag() {
        let obj: serde_json::Map<String, serde_json::Value> =
            serde_json::from_value(json!({"is_correction": true})).unwrap();
        assert!(detect_correction(&obj));
    }

    #[test]
    fn correction_detection_from_routing_meta() {
        let obj: serde_json::Map<String, serde_json::Value> = serde_json::from_value(json!({
            "routing_meta": {"is_correction": true}
        }))
        .unwrap();
        assert!(detect_correction(&obj));
    }

    #[test]
    fn correction_detection_absent() {
        let obj: serde_json::Map<String, serde_json::Value> =
            serde_json::from_value(json!({})).unwrap();
        assert!(!detect_correction(&obj));
    }

    // ── PipelineLearningWriter tests ──

    #[tokio::test]
    async fn pipeline_writer_updates_entity_graph_on_success() {
        let graph = Arc::new(Mutex::new(EntityGraph::new()));
        let writer = PipelineLearningWriter::new().with_entity_graph(graph.clone());

        let outcome = TurnLearningOutcome {
            query: "check matrixorigin PRs".into(),
            tools_selected: vec!["github_list_prs".into()],
            tools_used: vec!["github_list_prs".into()],
            success: true,
            quality: 0.9,
            was_corrected: false,
            task_type_label: Some("fetch".into()),
            domain_hint_label: Some("github".into()),
            user_feedback_score: None,
        };

        writer.record_outcome(outcome).await.unwrap();

        let g = graph.lock().unwrap();
        let boost = g.boost_for("matrixorigin");
        assert!(
            !boost.is_empty(),
            "entity graph should learn matrixorigin → GitHub"
        );
    }

    #[tokio::test]
    async fn pipeline_writer_skips_entity_on_failure() {
        let graph = Arc::new(Mutex::new(EntityGraph::new()));
        let writer = PipelineLearningWriter::new().with_entity_graph(graph.clone());

        let outcome = TurnLearningOutcome {
            query: "check kubernetes pods".into(),
            tools_selected: vec!["bash".into()],
            tools_used: vec!["bash".into()],
            success: false,
            quality: 0.2,
            was_corrected: false,
            task_type_label: Some("code".into()),
            domain_hint_label: Some("system".into()),
            user_feedback_score: None,
        };

        writer.record_outcome(outcome).await.unwrap();

        let g = graph.lock().unwrap();
        let boost = g.boost_for("kubernetes");
        assert!(
            boost.is_empty(),
            "failed outcomes should not train entity graph"
        );
    }

    #[tokio::test]
    async fn pipeline_writer_updates_pattern_library() {
        let lib = Arc::new(Mutex::new(PatternLibrary::new()));
        let writer = PipelineLearningWriter::new().with_pattern_library(lib.clone());

        // Record 2 outcomes (minimum for suggest())
        for _ in 0..2 {
            let outcome = TurnLearningOutcome {
                query: "list pull requests".into(),
                tools_selected: vec!["github_list_prs".into()],
                tools_used: vec!["github_list_prs".into(), "github_get_pr".into()],
                success: true,
                quality: 0.85,
                was_corrected: false,
                task_type_label: Some("fetch".into()),
                domain_hint_label: Some("github".into()),
                user_feedback_score: None,
            };
            writer.record_outcome(outcome).await.unwrap();
        }

        let l = lib.lock().unwrap();
        let suggestions = l.suggest(TaskType::Fetch, Some(DomainHint::GitHub), 5);
        assert!(
            !suggestions.is_empty(),
            "pattern library should have learned the tool chain"
        );
    }

    #[tokio::test]
    async fn pipeline_writer_updates_calibrator() {
        let cal = Arc::new(Mutex::new(ProgressiveCalibrator::new(0.15)));
        let writer = PipelineLearningWriter::new().with_progressive_calibrator(cal.clone());

        // Record several outcomes including corrections
        for i in 0..6 {
            let outcome = TurnLearningOutcome {
                query: format!("task {i}"),
                tools_selected: vec!["bash".into()],
                tools_used: vec!["bash".into()],
                success: true,
                quality: 0.8,
                was_corrected: i % 3 == 0, // every 3rd turn is corrected
                task_type_label: Some("code".into()),
                domain_hint_label: Some("code".into()),
                user_feedback_score: None,
            };
            writer.record_outcome(outcome).await.unwrap();
        }

        let c = cal.lock().unwrap();
        let threshold = c.calibrated_threshold("code", Some(DomainHint::Code), TaskType::Code);
        // With corrections, threshold is clamped to min_threshold (0.25) from base 0.15
        // The calibrator adjusts down from base, but clamp prevents going below min
        assert!(
            threshold <= 0.25,
            "calibrator should be at min_threshold with corrections, got: {}",
            threshold
        );
    }

    #[tokio::test]
    async fn pipeline_writer_all_modules_composed() {
        let graph = Arc::new(Mutex::new(EntityGraph::new()));
        let lib = Arc::new(Mutex::new(PatternLibrary::new()));
        let cal = Arc::new(Mutex::new(ProgressiveCalibrator::new(0.15)));

        let writer = PipelineLearningWriter::new()
            .with_entity_graph(graph.clone())
            .with_pattern_library(lib.clone())
            .with_progressive_calibrator(cal.clone());

        let outcome = TurnLearningOutcome {
            query: "show matrixorigin CI status".into(),
            tools_selected: vec!["github_list_prs".into()],
            tools_used: vec!["github_list_prs".into(), "github_list_workflows".into()],
            success: true,
            quality: 0.9,
            was_corrected: false,
            task_type_label: Some("fetch".into()),
            domain_hint_label: Some("github".into()),
            user_feedback_score: None,
        };

        let result = writer.record_outcome(outcome).await;
        assert!(result.is_ok(), "all modules should update without error");

        // Verify entity graph learned
        let g = graph.lock().unwrap();
        assert!(!g.boost_for("matrixorigin").is_empty());
    }

    #[tokio::test]
    async fn pipeline_writer_no_modules_is_noop() {
        let writer = PipelineLearningWriter::new();
        let outcome = TurnLearningOutcome {
            query: "test".into(),
            tools_selected: vec![],
            tools_used: vec![],
            success: false,
            quality: 0.0,
            was_corrected: false,
            task_type_label: None,
            domain_hint_label: None,
            user_feedback_score: None,
        };
        // Should not panic
        let result = writer.record_outcome(outcome).await;
        assert!(result.is_ok());
    }
}
