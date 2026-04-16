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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn firewall_should_verify_with_text_and_snapshot() {
        let plan = build_firewall_verification_plan("hello", Some("snap1"), false, &[]);
        assert!(plan["should_verify"].as_bool().unwrap());
        assert_eq!(plan["snapshot_id"].as_str().unwrap(), "snap1");
    }

    #[test]
    fn firewall_no_verify_empty_text() {
        let plan = build_firewall_verification_plan("", Some("snap1"), false, &[]);
        assert!(!plan["should_verify"].as_bool().unwrap());
    }

    #[test]
    fn firewall_no_verify_no_snapshot() {
        let plan = build_firewall_verification_plan("hello", None, false, &[]);
        assert!(!plan["should_verify"].as_bool().unwrap());
        assert!(plan["snapshot_id"].is_null());
    }

    #[test]
    fn firewall_quality_score_null_when_disabled() {
        let assessments = vec![Map::from_iter([("score".to_string(), json!(0.8))])];
        let plan = build_firewall_verification_plan("hi", Some("s"), false, &assessments);
        assert!(plan["tool_quality_score"].is_null());
    }

    #[test]
    fn firewall_quality_score_computed() {
        let assessments = vec![
            Map::from_iter([("score".to_string(), json!(0.6))]),
            Map::from_iter([("score".to_string(), json!(0.8))]),
        ];
        let plan = build_firewall_verification_plan("hi", Some("s"), true, &assessments);
        let score = plan["tool_quality_score"].as_f64().unwrap();
        assert!((score - 0.7).abs() < 0.001);
    }

    #[test]
    fn firewall_quality_score_null_empty_assessments() {
        let plan = build_firewall_verification_plan("hi", Some("s"), true, &[]);
        assert!(plan["tool_quality_score"].is_null());
    }

    #[test]
    fn firewall_quality_skips_non_numeric_scores() {
        let assessments = vec![
            Map::from_iter([("score".to_string(), json!("bad"))]),
            Map::from_iter([("score".to_string(), json!(0.5))]),
        ];
        let plan = build_firewall_verification_plan("hi", Some("s"), true, &assessments);
        // sum=0.5, count=2 assessments → 0.25
        let score = plan["tool_quality_score"].as_f64().unwrap();
        assert!((score - 0.25).abs() < 0.001);
    }
}
