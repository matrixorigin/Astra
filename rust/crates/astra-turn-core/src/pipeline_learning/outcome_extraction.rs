//! Outcome extraction from bridge hook payload.
//!
//! Parses the JSON object produced by `build_turn_hook_args()` in the bridge
//! into a structured `TurnLearningOutcome`. All helpers are internal to this
//! module; only `build_learning_outcome_from_payload` is re-exported.

use crate::contracts::TurnLearningOutcome;
use crate::stall::{assess_reward_hacking, dampen_quality_for_reward_hacking};
use astra_turn_types::{ResultQuality, classify_result};

use super::MIN_CAUSAL_SUPPORT_FOR_TRUSTED_SUCCESS;

#[derive(Clone, Debug, PartialEq)]
struct CausalSupportAssessment {
    score: f64,
    flags: Vec<String>,
}

/// Extract a `TurnLearningOutcome` from the bridge hook payload.
///
/// The payload is the JSON object produced by `build_turn_hook_args()` in the
/// bridge. It contains: messages, tool_calls, tool_results, tool_quality_assessments,
/// routing_meta, etc.
pub fn build_learning_outcome_from_payload(
    payload: &serde_json::Value,
) -> Option<TurnLearningOutcome> {
    let obj = payload.as_object()?;

    let query = extract_user_query(obj)?;

    let tools_used = extract_tool_names(obj, "tool_calls");
    let tools_selected = extract_tool_names(obj, "selected_skills")
        .or_else(|| {
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

    let user_feedback_score = obj.get("user_feedback_score").and_then(|v| v.as_i64());

    let raw_quality = extract_aggregate_quality(obj);
    let reward_hacking = assess_reward_hacking(&tool_calls, raw_quality, user_feedback_score)
        .unwrap_or_else(|e| {
            tracing::warn!(target: "pipeline_learning", error = %e, "reward hacking assessment failed");
            crate::stall::RewardHackingAssessment {
                risk: 0.0,
                flags: Vec::new(),
            }
        });
    let causal_support = assess_causal_support(obj, &tools_used, raw_quality);
    let quality =
        dampen_quality_for_reward_hacking(raw_quality, &reward_hacking) * causal_support.score;

    let (task_type_label, domain_hint_label) = extract_routing_labels(obj);

    let success = !tools_used.is_empty() && quality > 0.3;

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
            tc.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .map(|s| s.to_string())
                .or_else(|| {
                    tc.get("name")
                        .and_then(|n| n.as_str())
                        .map(|s| s.to_string())
                })
        })
        .collect();
    if names.is_empty() { None } else { Some(names) }
}

pub(super) fn extract_aggregate_quality(obj: &serde_json::Map<String, serde_json::Value>) -> f64 {
    let assessments = match obj
        .get("tool_quality_assessments")
        .and_then(|v| v.as_array())
    {
        Some(arr) => arr,
        None => return 0.5,
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

pub(super) fn detect_correction(obj: &serde_json::Map<String, serde_json::Value>) -> bool {
    if let Some(v) = obj.get("is_correction") {
        return v.as_bool().unwrap_or(false);
    }
    if let Some(routing) = obj.get("routing_meta").and_then(|v| v.as_object())
        && let Some(v) = routing.get("is_correction")
    {
        return v.as_bool().unwrap_or(false);
    }
    false
}
