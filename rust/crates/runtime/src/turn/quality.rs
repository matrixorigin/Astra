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
