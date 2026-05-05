//! Pipeline session analysis — extracts context pipeline health from journal events.
//!
//! Reads PipelineFeedback/PipelineAlert/PipelineCompactionAudit events from a
//! SessionCapture and produces structured diagnostics: cache trend, compaction
//! frequency, pressure evolution, and alert timeline.

use serde::{Deserialize, Serialize};

use crate::session_capture::SessionCapture;

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
    let mut ratios = Vec::new();

    for event in &capture.events {
        let metadata = event.raw.get("metadata");

        match event.event_type.as_str() {
            "PipelineFeedback" => {
                if let Some(meta) = metadata {
                    if let Some(ratio) = meta.get("cache_hit_ratio").and_then(|v| v.as_f64()) {
                        ratios.push(ratio);
                        report.turns_with_feedback += 1;
                    }
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
                    let turn = event
                        .raw
                        .get("turn")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as u32;

                    if rule == "compaction_cascade" {
                        report.cascade_detected = true;
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

    report.cache_hit_ratios = ratios.clone();
    if !ratios.is_empty() {
        report.avg_cache_hit_ratio = ratios.iter().sum::<f64>() / ratios.len() as f64;
    }

    report
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
        let trend = if last > first { "↑" } else if last < first { "↓" } else { "→" };
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
        let capture = make_capture(vec![make_alert_event(
            7,
            "compaction_cascade",
            "Warning",
        )]);
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
