use serde_json::{Map, Value, json};

pub fn build_firewall_verification_plan(
    full_text: &str,
    snapshot_id: Option<&str>,
    tool_quality_enabled: bool,
    tool_quality_assessments: &[Map<String, Value>],
) -> Map<String, Value> {
    let tool_quality_score = if tool_quality_enabled && !tool_quality_assessments.is_empty() {
        let total: f64 = tool_quality_assessments
            .iter()
            .filter_map(|assessment| assessment.get("score").and_then(Value::as_f64))
            .sum();
        Value::from(total / tool_quality_assessments.len() as f64)
    } else {
        Value::Null
    };
    Map::from_iter([
        (
            "should_verify".to_string(),
            Value::Bool(!full_text.is_empty() && snapshot_id.is_some()),
        ),
        (
            "full_text".to_string(),
            Value::String(full_text.to_string()),
        ),
        (
            "snapshot_id".to_string(),
            snapshot_id.map(|v| json!(v)).unwrap_or(Value::Null),
        ),
        ("tool_quality_score".to_string(), tool_quality_score),
    ])
}
