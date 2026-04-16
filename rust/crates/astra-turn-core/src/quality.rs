use serde_json::{Map, Value, json};

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
