//! Pipeline session analysis — extracts context pipeline health from journal events.
//!
//! Reads PipelineFeedback/PipelineAlert/PipelineCompactionAudit events from a
//! SessionCapture and produces structured diagnostics: cache trend, compaction
//! frequency, pressure evolution, and alert timeline.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::session_capture::SessionCapture;

const RAW_CACHE_BREAK_MIN_RATIO: f64 = 0.25;

/// Aggregate pipeline health metrics for a session.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PipelineHealthReport {
    /// Per-turn cache hit ratio (0.0–1.0). Index = turn - 1.
    pub cache_hit_ratios: Vec<f64>,
    /// Average cache hit ratio across all turns.
    pub avg_cache_hit_ratio: f64,
    /// Number of compaction events recorded.
    pub compaction_count: u32,
    /// Total tokens freed by compaction.
    pub total_tokens_freed: u64,
    /// Alerts that fired (turn, rule, severity).
    pub alerts: Vec<PipelineAlertEntry>,
    /// Number of explicit prompt-cache break alerts.
    pub prompt_cache_breaks: u32,
    /// Whether a compaction cascade was detected.
    pub cascade_detected: bool,
    /// Number of turns with pipeline feedback.
    pub turns_with_feedback: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineAlertEntry {
    pub turn: u32,
    pub rule: String,
    pub severity: String,
}

/// Analyze a session capture for pipeline health.
pub fn analyze_pipeline_health(capture: &SessionCapture) -> PipelineHealthReport {
    let mut report = PipelineHealthReport::default();
    let mut feedback_ratios = Vec::new();
    let mut raw_usage_ratios = Vec::new();

    for event in &capture.events {
        let metadata = event.raw.get("metadata");

        match event.event_type.as_str() {
            "PipelineFeedback" => {
                if let Some(meta) = metadata
                    && let Some(ratio) = meta.get("cache_hit_ratio").and_then(|v| v.as_f64())
                {
                    feedback_ratios.push(ratio);
                }
            }
            "llm_response_full" => {
                if let Some(ratio) = raw_llm_response_cache_hit_ratio(event) {
                    raw_usage_ratios.push(ratio);
                }
            }
            "PipelineCompactionAudit" => {
                if let Some(meta) = metadata {
                    report.compaction_count += 1;
                    if let Some(freed) = meta.get("tokens_freed").and_then(|v| v.as_u64()) {
                        report.total_tokens_freed += freed;
                    }
                }
            }
            "PipelineAlert" => {
                if let Some(meta) = metadata {
                    let rule = meta
                        .get("alert_rule")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_string();
                    let severity = meta
                        .get("alert_severity")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_string();
                    let turn = event.raw.get("turn").and_then(|v| v.as_u64()).unwrap_or(0) as u32;

                    if rule == "compaction_cascade" {
                        report.cascade_detected = true;
                    }
                    if rule == "prompt_cache_break" {
                        report.prompt_cache_breaks += 1;
                    }

                    report.alerts.push(PipelineAlertEntry {
                        turn,
                        rule,
                        severity,
                    });
                }
            }
            _ => {}
        }
    }

    if report.prompt_cache_breaks == 0 {
        let raw_breaks = detect_raw_prompt_cache_breaks(capture);
        report.prompt_cache_breaks = raw_breaks.len() as u32;
        report.alerts.extend(raw_breaks);
    }

    report.cache_hit_ratios = if feedback_ratios.is_empty() {
        raw_usage_ratios
    } else {
        feedback_ratios
    };
    report.turns_with_feedback = report.cache_hit_ratios.len() as u32;
    if !report.cache_hit_ratios.is_empty() {
        report.avg_cache_hit_ratio =
            report.cache_hit_ratios.iter().sum::<f64>() / report.cache_hit_ratios.len() as f64;
    }

    report
}

fn raw_llm_response_cache_hit_ratio(event: &crate::session_capture::JournalEvent) -> Option<f64> {
    let usage = event
        .raw
        .get("metadata")
        .and_then(|meta| meta.get("response"))
        .and_then(|response| response.get("response"))
        .and_then(|response| response.get("usage"))?;

    let input_tokens = usage
        .get("input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cache_read_tokens = usage
        .get("cached_input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cache_creation_tokens = usage
        .get("cache_creation_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let total_input = input_tokens
        .saturating_add(cache_read_tokens)
        .saturating_add(cache_creation_tokens);
    if total_input == 0 {
        return None;
    }
    Some(cache_read_tokens as f64 / total_input as f64)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RawPromptFingerprint {
    provider: String,
    model: String,
    system_prompt: String,
    tools_json: String,
}

#[derive(Debug, Clone)]
struct RawPromptTurn {
    turn: u32,
    fingerprint: RawPromptFingerprint,
    cache_hit_ratio: f64,
}

fn detect_raw_prompt_cache_breaks(capture: &SessionCapture) -> Vec<PipelineAlertEntry> {
    let mut turns = Vec::new();
    let mut pending_request: Option<RawPromptFingerprint> = None;
    let mut response_turn = 0u32;

    for event in &capture.events {
        match event.event_type.as_str() {
            "llm_request_full" => {
                pending_request = raw_prompt_fingerprint(event);
            }
            "llm_response_full" => {
                response_turn = response_turn.saturating_add(1);
                let Some(fingerprint) = pending_request.take() else {
                    continue;
                };
                let Some(cache_hit_ratio) = raw_llm_response_cache_hit_ratio(event) else {
                    continue;
                };
                turns.push(RawPromptTurn {
                    turn: response_turn,
                    fingerprint,
                    cache_hit_ratio,
                });
            }
            _ => {}
        }
    }

    let mut alerts = Vec::new();
    for pair in turns.windows(2) {
        let previous = &pair[0];
        let current = &pair[1];
        let fingerprint_changed = previous.fingerprint != current.fingerprint;
        let crossed_below_floor = previous.cache_hit_ratio >= RAW_CACHE_BREAK_MIN_RATIO
            && current.cache_hit_ratio < RAW_CACHE_BREAK_MIN_RATIO;
        if (fingerprint_changed || crossed_below_floor)
            && current.cache_hit_ratio < RAW_CACHE_BREAK_MIN_RATIO
        {
            alerts.push(PipelineAlertEntry {
                turn: current.turn,
                rule: "prompt_cache_break".into(),
                severity: "warning".into(),
            });
        }
    }
    alerts
}

fn raw_prompt_fingerprint(
    event: &crate::session_capture::JournalEvent,
) -> Option<RawPromptFingerprint> {
    let metadata = event.raw.get("metadata")?;
    let request = metadata.get("request")?;
    let messages = request.get("messages")?.as_array()?;
    let system_prompt = messages
        .iter()
        .find(|message| message.get("role").and_then(Value::as_str) == Some("system"))
        .or_else(|| messages.first())
        .map(message_content_text)
        .unwrap_or_default();
    let tools_json =
        serde_json::to_string(request.get("tools").unwrap_or(&Value::Array(vec![]))).ok()?;
    Some(RawPromptFingerprint {
        provider: metadata
            .get("provider")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
        model: metadata
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
        system_prompt,
        tools_json,
    })
}

fn message_content_text(message: &Value) -> String {
    content_value_text(message.get("content").unwrap_or(&Value::Null))
}

fn content_value_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| {
                item.get("text")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| item.as_str().map(str::to_string))
            })
            .collect::<Vec<_>>()
            .join("\n\n"),
        Value::Object(map) => map
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| serde_json::to_string(value).unwrap_or_default()),
        _ => value.to_string(),
    }
}

/// Render a human-readable pipeline health summary.
pub fn render_pipeline_health(report: &PipelineHealthReport) -> String {
    let mut out = String::new();
    out.push_str("── Pipeline Health ──\n");

    if report.turns_with_feedback == 0 {
        out.push_str("  No pipeline feedback events found.\n");
        return out;
    }

    out.push_str(&format!(
        "  Turns with feedback: {}\n",
        report.turns_with_feedback
    ));
    out.push_str(&format!(
        "  Avg cache hit ratio: {:.1}%\n",
        report.avg_cache_hit_ratio * 100.0
    ));

    if !report.cache_hit_ratios.is_empty() {
        let first = report.cache_hit_ratios.first().unwrap_or(&0.0);
        let last = report.cache_hit_ratios.last().unwrap_or(&0.0);
        let trend = if last > first {
            "↑"
        } else if last < first {
            "↓"
        } else {
            "→"
        };
        out.push_str(&format!(
            "  Cache trend: {:.0}% → {:.0}% {}\n",
            first * 100.0,
            last * 100.0,
            trend
        ));
    }

    if report.compaction_count > 0 {
        out.push_str(&format!(
            "  Compactions: {} ({} tokens freed)\n",
            report.compaction_count, report.total_tokens_freed
        ));
    }

    if report.cascade_detected {
        out.push_str("  ⚠ Compaction cascade detected\n");
    }
    if report.prompt_cache_breaks > 0 {
        out.push_str(&format!(
            "  ⚠ Prompt cache breaks: {}\n",
            report.prompt_cache_breaks
        ));
    }

    if !report.alerts.is_empty() {
        out.push_str(&format!("  Alerts: {}\n", report.alerts.len()));
        for alert in &report.alerts {
            out.push_str(&format!(
                "    T{}: [{}] {}\n",
                alert.turn, alert.severity, alert.rule
            ));
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_capture::JournalEvent;

    fn make_feedback_event(turn: u32, cache_hit_ratio: f64) -> JournalEvent {
        JournalEvent {
            event_type: "PipelineFeedback".into(),
            raw: serde_json::json!({
                "type": "PipelineFeedback",
                "turn": turn,
                "metadata": {
                    "kind": "Feedback",
                    "turn": turn,
                    "cache_hit_ratio": cache_hit_ratio,
                    "completion_tokens": 300,
                }
            }),
        }
    }

    fn make_compaction_event(turn: u32, tokens_freed: u64) -> JournalEvent {
        JournalEvent {
            event_type: "PipelineCompactionAudit".into(),
            raw: serde_json::json!({
                "type": "PipelineCompactionAudit",
                "turn": turn,
                "metadata": {
                    "kind": "CompactionAudit",
                    "turn": turn,
                    "compaction_strategy": "tool_result_clearing",
                    "tokens_freed": tokens_freed,
                }
            }),
        }
    }

    fn make_alert_event(turn: u32, rule: &str, severity: &str) -> JournalEvent {
        JournalEvent {
            event_type: "PipelineAlert".into(),
            raw: serde_json::json!({
                "type": "PipelineAlert",
                "turn": turn,
                "metadata": {
                    "kind": "Alert",
                    "turn": turn,
                    "alert_rule": rule,
                    "alert_severity": severity,
                }
            }),
        }
    }

    fn make_capture(events: Vec<JournalEvent>) -> SessionCapture {
        SessionCapture {
            session_id: "test-session".into(),
            journal_path: std::path::PathBuf::from("/tmp/test.jsonl"),
            events,
            skipped_lines: 0,
            dropped_lines: 0,
        }
    }

    fn make_llm_request_event(
        model: &str,
        provider: &str,
        system_prompt: Value,
        tools: Value,
    ) -> JournalEvent {
        JournalEvent {
            event_type: "llm_request_full".into(),
            raw: serde_json::json!({
                "type": "llm_request_full",
                "metadata": {
                    "model": model,
                    "provider": provider,
                    "request": {
                        "messages": [
                            {
                                "role": "system",
                                "content": system_prompt,
                            },
                            {
                                "role": "user",
                                "content": "Reply ACK.",
                            }
                        ],
                        "tools": tools,
                    }
                }
            }),
        }
    }

    fn make_llm_response_event(
        input_tokens: u64,
        cached_input_tokens: u64,
        cache_creation_tokens: u64,
    ) -> JournalEvent {
        JournalEvent {
            event_type: "llm_response_full".into(),
            raw: serde_json::json!({
                "type": "llm_response_full",
                "metadata": {
                    "response": {
                        "response": {
                            "usage": {
                                "input_tokens": input_tokens,
                                "cached_input_tokens": cached_input_tokens,
                                "cache_creation_tokens": cache_creation_tokens,
                            }
                        }
                    }
                }
            }),
        }
    }

    #[test]
    fn empty_session_produces_empty_report() {
        let capture = make_capture(vec![]);
        let report = analyze_pipeline_health(&capture);
        assert_eq!(report.turns_with_feedback, 0);
        assert_eq!(report.avg_cache_hit_ratio, 0.0);
    }

    #[test]
    fn feedback_events_produce_cache_trend() {
        let capture = make_capture(vec![
            make_feedback_event(1, 0.0),
            make_feedback_event(2, 0.7),
            make_feedback_event(3, 0.85),
            make_feedback_event(4, 0.9),
        ]);
        let report = analyze_pipeline_health(&capture);
        assert_eq!(report.turns_with_feedback, 4);
        assert_eq!(report.cache_hit_ratios.len(), 4);
        assert!(report.avg_cache_hit_ratio > 0.5);
    }

    #[test]
    fn llm_response_usage_fallback_produces_cache_trend() {
        let capture = make_capture(vec![
            make_llm_response_event(9_984, 5, 0),
            make_llm_response_event(162, 10_112, 0),
            make_llm_response_event(172, 10_112, 0),
        ]);
        let report = analyze_pipeline_health(&capture);
        assert_eq!(report.turns_with_feedback, 3);
        assert_eq!(report.cache_hit_ratios.len(), 3);
        assert!(report.cache_hit_ratios[0] < 0.01);
        assert!(report.cache_hit_ratios[1] > 0.9);
        assert!(report.avg_cache_hit_ratio > 0.6);
    }

    #[test]
    fn pipeline_feedback_takes_precedence_over_raw_usage_fallback() {
        let capture = make_capture(vec![
            make_llm_response_event(100, 9_900, 0),
            make_feedback_event(1, 0.2),
        ]);
        let report = analyze_pipeline_health(&capture);
        assert_eq!(report.turns_with_feedback, 1);
        assert_eq!(report.cache_hit_ratios, vec![0.2]);
    }

    #[test]
    fn raw_prompt_break_detection_flags_cache_drop_without_pipeline_alerts() {
        let capture = make_capture(vec![
            make_llm_request_event(
                "m",
                "openai",
                Value::String("system".into()),
                serde_json::json!([]),
            ),
            make_llm_response_event(100, 900, 0),
            make_llm_request_event(
                "m",
                "openai",
                Value::String("system".into()),
                serde_json::json!([]),
            ),
            make_llm_response_event(100, 900, 0),
            make_llm_request_event(
                "m",
                "openai",
                Value::String("system".into()),
                serde_json::json!([]),
            ),
            make_llm_response_event(1_000, 0, 0),
        ]);
        let report = analyze_pipeline_health(&capture);
        assert_eq!(report.prompt_cache_breaks, 1);
        assert_eq!(
            report
                .alerts
                .iter()
                .filter(|alert| alert.rule == "prompt_cache_break")
                .count(),
            1
        );
    }

    #[test]
    fn explicit_prompt_cache_break_alerts_suppress_raw_duplicates() {
        let capture = make_capture(vec![
            make_alert_event(3, "prompt_cache_break", "warning"),
            make_llm_request_event(
                "m",
                "openai",
                Value::String("system".into()),
                serde_json::json!([]),
            ),
            make_llm_response_event(100, 900, 0),
            make_llm_request_event(
                "m",
                "openai",
                Value::String("system".into()),
                serde_json::json!([]),
            ),
            make_llm_response_event(1_000, 0, 0),
        ]);
        let report = analyze_pipeline_health(&capture);
        assert_eq!(report.prompt_cache_breaks, 1);
        assert_eq!(
            report
                .alerts
                .iter()
                .filter(|alert| alert.rule == "prompt_cache_break")
                .count(),
            1
        );
    }

    #[test]
    fn compaction_events_accumulate() {
        let capture = make_capture(vec![
            make_compaction_event(3, 2000),
            make_compaction_event(5, 3000),
        ]);
        let report = analyze_pipeline_health(&capture);
        assert_eq!(report.compaction_count, 2);
        assert_eq!(report.total_tokens_freed, 5000);
    }

    #[test]
    fn cascade_alert_detected() {
        let capture = make_capture(vec![make_alert_event(7, "compaction_cascade", "Warning")]);
        let report = analyze_pipeline_health(&capture);
        assert!(report.cascade_detected);
        assert_eq!(report.alerts.len(), 1);
    }

    #[test]
    fn render_produces_readable_output() {
        let capture = make_capture(vec![
            make_feedback_event(1, 0.0),
            make_feedback_event(2, 0.8),
            make_feedback_event(3, 0.9),
            make_compaction_event(2, 1500),
        ]);
        let report = analyze_pipeline_health(&capture);
        let rendered = render_pipeline_health(&report);
        assert!(rendered.contains("Avg cache hit ratio"));
        assert!(rendered.contains("Compactions: 1"));
        assert!(rendered.contains("Cache trend"));
    }
}
