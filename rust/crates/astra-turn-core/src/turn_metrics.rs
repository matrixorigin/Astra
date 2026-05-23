//! Turn-level metrics and quality utilities.
//!
//! This module consolidates small, related helpers for counting turn events
//! and building quality payloads that were previously in standalone files.

use serde_json::{Map, Value, json};

/// Count how many events should be persisted for a turn given the
/// constituent parts.
pub fn count_persisted_turn_events(
    has_user_content: bool,
    tool_results_len: usize,
    tool_calls_len: usize,
    cloud_tool_results_len: usize,
    has_full_text: bool,
) -> usize {
    let mut n_events = 0usize;
    if has_user_content {
        n_events += 1;
    }
    n_events += tool_results_len;
    n_events += tool_calls_len;
    n_events += cloud_tool_results_len;
    if has_full_text || tool_calls_len > 0 {
        n_events += 1;
    }
    n_events.max(1)
}

/// Build the JSON payload for a tool-result quality event.
pub fn build_tool_result_quality_event_payload(
    assessment: &Map<String, Value>,
) -> Map<String, Value> {
    Map::from_iter([
        ("content".to_string(), json!(assessment)),
        (
            "metadata".to_string(),
            json!({
                "tool_name": assessment.get("tool_name").cloned().unwrap_or(Value::Null),
                "quality_score": assessment.get("score").cloned().unwrap_or(Value::Null),
                "quality_grade": assessment.get("grade").cloned().unwrap_or(Value::Null),
                "signals": assessment.get("signals").cloned().unwrap_or(Value::Null),
                "stale": assessment.get("stale").cloned().unwrap_or(Value::Null),
            }),
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_all_false_returns_1() {
        assert_eq!(count_persisted_turn_events(false, 0, 0, 0, false), 1);
    }

    #[test]
    fn count_user_content_only() {
        assert_eq!(count_persisted_turn_events(true, 0, 0, 0, false), 1);
    }

    #[test]
    fn count_with_tool_calls_adds_response() {
        assert_eq!(count_persisted_turn_events(false, 0, 3, 0, false), 4);
    }

    #[test]
    fn count_with_full_text() {
        assert_eq!(count_persisted_turn_events(false, 0, 0, 0, true), 1);
    }

    #[test]
    fn count_all_populated() {
        assert_eq!(count_persisted_turn_events(true, 2, 3, 1, true), 8);
    }

    #[test]
    fn quality_payload_full() {
        let assessment = Map::from_iter([
            ("tool_name".to_string(), json!("bash")),
            ("score".to_string(), json!(0.9)),
            ("grade".to_string(), json!("A")),
            ("signals".to_string(), json!(["fast"])),
            ("stale".to_string(), json!(false)),
        ]);
        let payload = build_tool_result_quality_event_payload(&assessment);
        assert_eq!(payload["metadata"]["tool_name"].as_str().unwrap(), "bash");
        assert!((payload["metadata"]["quality_score"].as_f64().unwrap() - 0.9).abs() < 0.001);
    }

    #[test]
    fn quality_payload_empty() {
        let payload = build_tool_result_quality_event_payload(&Map::new());
        assert!(payload["metadata"]["tool_name"].is_null());
        assert!(payload["metadata"]["quality_score"].is_null());
    }
}
