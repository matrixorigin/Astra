//! Pipeline Learning Writer — bridges TurnLearningWriter trait to pipeline modules.
//!
//! Receives turn outcomes from the bridge side-effects hook and updates:
//! - EntityGraph: entity → domain → tools associations
//! - PatternLibrary: tool chain success/failure patterns
//! - ProgressiveCalibrator: per-intent/domain/task correction rates
//!
//! Phase D enhancement: `record_implicit_feedback()` bridges ImplicitSignal
//! into the calibrator, converting signal type + confidence to correction events.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::pipeline::calibration::ProgressiveCalibrator;
use crate::pipeline::entity::{EntityGraph, extract_entities};
use crate::pipeline::pattern::PatternLibrary;
use crate::pipeline::routing::{DomainHint, TaskType};
use crate::turn::contracts::{TurnLearningOutcome, TurnLearningWriter};
use crate::turn::implicit_feedback::{ImplicitSignal, implicit_feedback_rating};
use crate::turn::result_quality::{ResultQuality, classify_result};
use crate::turn::stall::{assess_reward_hacking, dampen_quality_for_reward_hacking};

const MIN_CAUSAL_SUPPORT_FOR_TRUSTED_SUCCESS: f64 = 0.6;

#[derive(Clone, Debug, PartialEq)]
struct CausalSupportAssessment {
    score: f64,
    flags: Vec<String>,
}

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

/// Outcome from a failed turn, for learning from failures.
///
/// Distinct from [`TurnLearningOutcome`] because failure-specific fields
/// (error category, stall detection) don't apply to successes.
pub struct FailureLearningOutcome {
    pub query: String,
    pub tools_attempted: Vec<String>,
    pub error_category: String,
    pub stall_detected: bool,
    pub correction_was_active: bool,
    pub task_type: String,
    pub domain_hint: Option<String>,
}

impl PipelineLearningWriter {
    /// Record a turn failure across all pipeline modules.
    ///
    /// - **PatternLibrary**: increments failure_count for matching patterns
    /// - **EntityGraph**: dampens confidence for entities mentioned in the query
    ///
    /// Does not touch the ProgressiveCalibrator (corrections are tracked separately).
    pub fn record_failure(&self, outcome: &FailureLearningOutcome) {
        let task_type = parse_task_type(Some(outcome.task_type.as_str()));
        let domain = parse_domain_hint(outcome.domain_hint.as_deref());

        if let Some(pl) = &self.pattern_library {
            let mut lib = pl.lock().unwrap_or_else(|e| e.into_inner());
            lib.record_failure(&outcome.tools_attempted, task_type, domain);
        }

        if let Some(eg) = &self.entity_graph {
            let mut graph = eg.lock().unwrap_or_else(|e| e.into_inner());
            let entities = extract_entities(&outcome.query);
            for entity in &entities {
                graph.record_failure(entity, &outcome.tools_attempted);
            }
        }
    }

    /// Record implicit feedback signal into the learning pipeline.
    ///
    /// Bridges `ImplicitSignal` detected at turn start into the calibrator:
    /// - Correction/frustration → treated as was_corrected=true
    /// - Positive → treated as was_corrected=false (confirmation)
    /// - Neutral/rephrasing/clarification → no calibration update
    ///
    /// The signal's confidence modulates the feedback score (1-5 scale mapped
    /// to 0-100 for the calibrator).
    ///
    /// Returns a `StructuredFeedback` if heuristic extraction succeeds, or `None`
    /// if the correction is too complex (caller should use LLM extraction).
    pub fn record_implicit_feedback(
        &self,
        signal: &ImplicitSignal,
        intent: &str,
        domain: Option<DomainHint>,
        task_type: TaskType,
    ) -> Option<astra_turn_types::StructuredFeedback> {
        // Only calibrate on actionable signals
        let was_corrected = match signal.signal_type.as_str() {
            "correction" | "frustration" => true,
            "positive" => false,
            _ => return None, // neutral/rephrasing/clarification don't affect calibration
        };

        // Convert 1-5 rating to 0-100 feedback score
        let rating = implicit_feedback_rating(&signal.signal_type);
        let base_score = (rating - 1) * 25; // 1→0, 2→25, 3→50, 4→75, 5→100
        let confidence_factor = signal.confidence;
        let feedback_score = Some((base_score as f64 * confidence_factor).round() as i64);

        if let Some(pc) = &self.progressive_calibrator {
            let mut cal = pc.lock().unwrap_or_else(|e| e.into_inner());
            cal.record(intent, domain, task_type, was_corrected, feedback_score);
        }

        // Attempt heuristic structured feedback extraction (no LLM needed)
        if was_corrected {
            crate::pipeline::feedback_extraction::heuristic_extract(
                &signal.evidence,
                "",
                &signal.signal_type,
                signal.confidence,
            )
        } else {
            None
        }
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
        Some("web") => Some(DomainHint::Web),
        Some("system") => Some(DomainHint::System),
        Some("database") => Some(DomainHint::Database),
        _ => None,
    }
}

#[async_trait]
impl TurnLearningWriter for PipelineLearningWriter {
    async fn record_outcome(&self, outcome: TurnLearningOutcome) -> Result<(), String> {
        // Quality gate: filter out trivial, derivable, or ambiguous outcomes
        if let Err(_rejection) = crate::pipeline::learning_quality_gate::evaluate(&outcome) {
            return Ok(());
        }

        let task_type = parse_task_type(outcome.task_type_label.as_deref());
        let domain = parse_domain_hint(outcome.domain_hint_label.as_deref());
        let feedback = outcome.user_feedback_score;
        let trusted_success = outcome.success
            && outcome.reward_hacking_risk < 0.5
            && outcome.causal_support_score >= MIN_CAUSAL_SUPPORT_FOR_TRUSTED_SUCCESS;

        // 1. Entity graph: learn entity → domain → tools (only on success)
        // Pass feedback to modulate confidence growth
        if trusted_success && let Some(eg) = &self.entity_graph {
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
                trusted_success,
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
    let tool_calls = obj
        .get("tool_calls")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();

    // User feedback score: extracted from payload if available (will be populated
    // later when feedback is submitted via /api/v1/learning/feedback)
    let user_feedback_score = obj.get("user_feedback_score").and_then(|v| v.as_i64());

    // Extract quality from tool_quality_assessments
    let raw_quality = extract_aggregate_quality(obj);
    let reward_hacking = assess_reward_hacking(&tool_calls, raw_quality, user_feedback_score);
    let causal_support = assess_causal_support(obj, &tools_used, raw_quality);
    let quality =
        dampen_quality_for_reward_hacking(raw_quality, &reward_hacking) * causal_support.score;

    // Extract routing metadata
    let (task_type_label, domain_hint_label) = extract_routing_labels(obj);

    // Determine success: tools were used and no error indicators
    let success = !tools_used.is_empty() && quality > 0.3;

    // Correction detection: check if this payload indicates a correction turn
    let was_corrected = detect_correction(obj);

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
        reward_hacking_risk: reward_hacking.risk,
        reward_hacking_flags: reward_hacking.flags,
        causal_support_score: causal_support.score,
        causal_support_flags: causal_support.flags,
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
        if let Some(score) = assessment_score(assessment) {
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

fn assess_causal_support(
    obj: &serde_json::Map<String, serde_json::Value>,
    tools_used: &[String],
    raw_quality: f64,
) -> CausalSupportAssessment {
    if tools_used.is_empty() {
        return CausalSupportAssessment {
            score: 1.0,
            flags: Vec::new(),
        };
    }

    let assessments = obj
        .get("tool_quality_assessments")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let tool_results = obj
        .get("tool_results")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut score = 1.0_f64;
    let mut flags = Vec::new();

    if assessments.is_empty() {
        score -= 0.25;
        flags.push("missing_quality_evidence".into());
    } else {
        let assessment_scores: Vec<f64> = assessments.iter().filter_map(assessment_score).collect();
        let positive_assessments = assessment_scores.iter().filter(|&&s| s >= 0.75).count();
        let negative_assessments = assessment_scores.iter().filter(|&&s| s <= 0.35).count();

        if positive_assessments == 0 {
            score -= 0.15;
            flags.push("weak_quality_evidence".into());
        }
        if negative_assessments > 0 {
            score -= 0.25;
            flags.push("negative_quality_signals".into());
        }
        if tools_used.len() > 1 && assessments.len() < tools_used.len() {
            score -= 0.10;
            flags.push("sparse_quality_coverage".into());
        }
        if negative_assessments >= positive_assessments && negative_assessments > 0 {
            score -= 0.15;
            flags.push("contradictory_quality_assessments".into());
        }
    }

    if tool_results.is_empty() {
        score -= 0.15;
        flags.push("missing_tool_results".into());
    } else {
        let mut success_results = 0usize;
        let mut error_results = 0usize;
        let mut empty_results = 0usize;
        let mut truncated_results = 0usize;
        let mut seen_results = 0usize;

        for result in &tool_results {
            let Some(quality) = tool_result_quality(result) else {
                continue;
            };
            seen_results += 1;
            match quality {
                ResultQuality::Success => success_results += 1,
                ResultQuality::Error => error_results += 1,
                ResultQuality::Empty => empty_results += 1,
                ResultQuality::Truncated => truncated_results += 1,
            }
        }

        if seen_results == 0 {
            score -= 0.15;
            flags.push("missing_tool_result_content".into());
        } else {
            if error_results > 0 {
                score -= 0.35;
                flags.push("error_tool_results".into());
            }
            if success_results == 0 && empty_results > 0 {
                score -= 0.20;
                flags.push("empty_tool_results".into());
            }
            if success_results == 0 && truncated_results > 0 {
                score -= 0.10;
                flags.push("truncated_tool_results".into());
            }
            if tools_used.len() > 1 && seen_results < tools_used.len() {
                score -= 0.10;
                flags.push("sparse_tool_result_coverage".into());
            }
        }
    }

    if raw_quality >= 0.8 && score < MIN_CAUSAL_SUPPORT_FOR_TRUSTED_SUCCESS {
        score -= 0.10;
        flags.push("high_quality_without_causal_support".into());
    }

    flags.sort();
    flags.dedup();
    CausalSupportAssessment {
        score: score.clamp(0.0, 1.0),
        flags,
    }
}

fn assessment_score(assessment: &serde_json::Value) -> Option<f64> {
    assessment
        .get("quality_score")
        .and_then(|v| v.as_f64())
        .or_else(|| assessment.get("score").and_then(|v| v.as_f64()))
        .or_else(|| {
            assessment
                .get("grade")
                .and_then(|v| v.as_str())
                .and_then(grade_to_quality_score)
        })
}

fn grade_to_quality_score(grade: &str) -> Option<f64> {
    match grade.to_ascii_lowercase().as_str() {
        "a" | "excellent" => Some(1.0),
        "b" | "good" | "complete" | "ok" => Some(0.8),
        "c" | "partial" => Some(0.4),
        "warning" => Some(0.3),
        "d" => Some(0.2),
        "f" | "failed" | "error" => Some(0.1),
        _ => None,
    }
}

fn tool_result_quality(result: &serde_json::Value) -> Option<ResultQuality> {
    let content = result
        .get("content")
        .or_else(|| result.get("result"))
        .cloned()?;
    let rendered = match content {
        serde_json::Value::String(text) => text,
        other => other.to_string(),
    };
    Some(classify_result(&rendered))
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
                {"tool_name": "github_list_prs", "grade": "good", "quality_score": 0.85},
                {"tool_name": "github_search_repos", "grade": "good", "quality_score": 0.85}
            ],
            "tool_results": [
                {"content": "{\"status\":\"ok\"}"},
                {"content": "{\"status\":\"ok\"}"}
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
        assert_eq!(outcome.reward_hacking_risk, 0.0);
        assert!(outcome.reward_hacking_flags.is_empty());
        assert_eq!(outcome.causal_support_score, 1.0);
        assert!(outcome.causal_support_flags.is_empty());
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
    fn extract_quality_accepts_score_and_letter_grades() {
        let obj: serde_json::Map<String, serde_json::Value> = serde_json::from_value(json!({
            "tool_quality_assessments": [
                {"score": 0.9},
                {"grade": "A"},
                {"grade": "warning"}
            ]
        }))
        .unwrap();
        let quality = extract_aggregate_quality(&obj);
        assert!((quality - ((0.9 + 1.0 + 0.3) / 3.0)).abs() < 0.01);
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

    #[test]
    fn extract_outcome_dampens_repetitive_exploration_score() {
        let payload = json!({
            "messages": [
                {"role": "user", "content": "find the struct"}
            ],
            "tool_calls": [
                {"function": {"name": "read_file", "arguments": {"path": "src/lib.rs"}}},
                {"function": {"name": "read_file", "arguments": {"path": "src/lib.rs"}}},
                {"function": {"name": "read_file", "arguments": {"path": "src/lib.rs"}}}
            ],
            "tool_quality_assessments": [
                {"quality_score": 0.9}
            ],
            "tool_results": [
                {"content": "struct Foo {}"}
            ]
        });

        let outcome = build_learning_outcome_from_payload(&payload).unwrap();
        assert!(outcome.reward_hacking_risk >= 0.8, "{outcome:?}");
        assert!(outcome.quality < 0.3, "{outcome:?}");
        assert!(!outcome.success, "{outcome:?}");
        assert!(
            outcome
                .reward_hacking_flags
                .iter()
                .any(|flag| flag.contains("repeated identical tool call"))
        );
    }

    #[test]
    fn extract_outcome_flags_weak_causal_support() {
        let payload = json!({
            "messages": [
                {"role": "user", "content": "check the repo status"}
            ],
            "tool_calls": [
                {"function": {"name": "read_file"}},
                {"function": {"name": "bash"}}
            ],
            "tool_quality_assessments": [
                {"tool_name": "read_file", "score": 0.9}
            ],
            "tool_results": [
                {"content": "Error: command failed"}
            ]
        });

        let outcome = build_learning_outcome_from_payload(&payload).unwrap();
        assert!(outcome.causal_support_score < MIN_CAUSAL_SUPPORT_FOR_TRUSTED_SUCCESS);
        assert!(
            outcome
                .causal_support_flags
                .iter()
                .any(|flag| flag == "error_tool_results")
        );
        assert!(
            outcome
                .causal_support_flags
                .iter()
                .any(|flag| flag == "high_quality_without_causal_support")
        );
        assert!(outcome.quality < 0.4, "{outcome:?}");
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
            reward_hacking_risk: 0.0,
            reward_hacking_flags: Vec::new(),
            causal_support_score: 1.0,
            causal_support_flags: Vec::new(),
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
            reward_hacking_risk: 0.0,
            reward_hacking_flags: Vec::new(),
            causal_support_score: 1.0,
            causal_support_flags: Vec::new(),
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
                reward_hacking_risk: 0.0,
                reward_hacking_flags: Vec::new(),
                causal_support_score: 1.0,
                causal_support_flags: Vec::new(),
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
                reward_hacking_risk: 0.0,
                reward_hacking_flags: Vec::new(),
                causal_support_score: 1.0,
                causal_support_flags: Vec::new(),
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
            reward_hacking_risk: 0.0,
            reward_hacking_flags: Vec::new(),
            causal_support_score: 1.0,
            causal_support_flags: Vec::new(),
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
            reward_hacking_risk: 0.0,
            reward_hacking_flags: Vec::new(),
            causal_support_score: 1.0,
            causal_support_flags: Vec::new(),
        };
        // Should not panic
        let result = writer.record_outcome(outcome).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn pipeline_writer_skips_entity_learning_on_reward_hacking_risk() {
        let graph = Arc::new(Mutex::new(EntityGraph::new()));
        let writer = PipelineLearningWriter::new().with_entity_graph(graph.clone());

        let outcome = TurnLearningOutcome {
            query: "inspect matrixorigin code".into(),
            tools_selected: vec!["read_file".into()],
            tools_used: vec!["read_file".into(), "read_file".into(), "read_file".into()],
            success: true,
            quality: 0.35,
            was_corrected: false,
            task_type_label: Some("code".into()),
            domain_hint_label: Some("code".into()),
            user_feedback_score: None,
            reward_hacking_risk: 0.8,
            reward_hacking_flags: vec!["repeated identical tool call x3".into()],
            causal_support_score: 1.0,
            causal_support_flags: Vec::new(),
        };

        writer.record_outcome(outcome).await.unwrap();

        let g = graph.lock().unwrap();
        assert!(g.boost_for("matrixorigin").is_empty());
    }

    #[tokio::test]
    async fn pipeline_writer_skips_entity_learning_on_low_causal_support() {
        let graph = Arc::new(Mutex::new(EntityGraph::new()));
        let writer = PipelineLearningWriter::new().with_entity_graph(graph.clone());

        let outcome = TurnLearningOutcome {
            query: "inspect matrixorigin code".into(),
            tools_selected: vec!["read_file".into(), "bash".into()],
            tools_used: vec!["read_file".into(), "bash".into()],
            success: true,
            quality: 0.35,
            was_corrected: false,
            task_type_label: Some("code".into()),
            domain_hint_label: Some("code".into()),
            user_feedback_score: None,
            reward_hacking_risk: 0.0,
            reward_hacking_flags: Vec::new(),
            causal_support_score: 0.4,
            causal_support_flags: vec!["high_quality_without_causal_support".into()],
        };

        writer.record_outcome(outcome).await.unwrap();

        let g = graph.lock().unwrap();
        assert!(g.boost_for("matrixorigin").is_empty());
    }

    // ─── Phase D: Implicit feedback → learning pipeline ──────────────

    #[test]
    fn implicit_feedback_correction_triggers_calibrator() {
        let cal = Arc::new(Mutex::new(ProgressiveCalibrator::new(0.15)));
        let writer = PipelineLearningWriter::new().with_progressive_calibrator(cal.clone());

        let signal = ImplicitSignal {
            signal_type: "correction".to_string(),
            confidence: 0.9,
            evidence: "不对".to_string(),
        };

        writer.record_implicit_feedback(&signal, "code", Some(DomainHint::Code), TaskType::Code);

        let c = cal.lock().unwrap();
        // Check that calibrator recorded the intent
        let stats = c.intent_stats("code");
        assert!(
            stats.is_some(),
            "calibrator should have recorded intent 'code'"
        );
        assert!(
            stats.unwrap().correction_rate() > 0.0,
            "correction signal should increase correction rate"
        );
    }

    #[test]
    fn implicit_feedback_frustration_triggers_calibrator() {
        let cal = Arc::new(Mutex::new(ProgressiveCalibrator::new(0.15)));
        let writer = PipelineLearningWriter::new().with_progressive_calibrator(cal.clone());

        let signal = ImplicitSignal {
            signal_type: "frustration".to_string(),
            confidence: 0.7,
            evidence: "terrible".to_string(),
        };

        writer.record_implicit_feedback(
            &signal,
            "fetch",
            Some(DomainHint::GitHub),
            TaskType::Fetch,
        );

        let c = cal.lock().unwrap();
        let stats = c.intent_stats("fetch");
        assert!(
            stats.is_some(),
            "calibrator should have recorded intent 'fetch'"
        );
        assert!(
            stats.unwrap().correction_rate() > 0.0,
            "frustration signal should increase correction rate"
        );
    }

    #[test]
    fn implicit_feedback_neutral_does_not_affect_calibrator() {
        let cal = Arc::new(Mutex::new(ProgressiveCalibrator::new(0.15)));
        let writer = PipelineLearningWriter::new().with_progressive_calibrator(cal.clone());

        let signal = ImplicitSignal {
            signal_type: "neutral".to_string(),
            confidence: 0.5,
            evidence: String::new(),
        };

        writer.record_implicit_feedback(&signal, "code", None, TaskType::Code);

        let c = cal.lock().unwrap();
        // Should not have recorded anything since neutral is ignored
        assert!(
            c.intent_stats("code").is_none(),
            "neutral signal should not affect calibrator"
        );
    }

    #[test]
    fn implicit_feedback_positive_records_success() {
        let cal = Arc::new(Mutex::new(ProgressiveCalibrator::new(0.15)));
        let writer = PipelineLearningWriter::new().with_progressive_calibrator(cal.clone());

        let signal = ImplicitSignal {
            signal_type: "positive".to_string(),
            confidence: 0.8,
            evidence: "thanks".to_string(),
        };

        writer.record_implicit_feedback(&signal, "conversational", None, TaskType::Conversational);

        let c = cal.lock().unwrap();
        let stats = c.intent_stats("conversational");
        assert!(stats.is_some(), "calibrator should have recorded intent");
        // Positive records was_corrected=false, so correction_rate should be 0
        assert!(
            stats.unwrap().correction_rate() == 0.0,
            "positive signal should record success (no correction)"
        );
    }
}
