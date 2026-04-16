use std::collections::BTreeSet;

use serde_json::{Map, Value, json};

pub fn collect_skill_version_names(
    tool_results: &[Map<String, Value>],
    tool_calls: &[Map<String, Value>],
) -> BTreeSet<String> {
    let mut all_names = BTreeSet::new();
    for tool_result in tool_results {
        if let Some(name) = tool_result.get("name").and_then(Value::as_str)
            && !name.is_empty()
        {
            all_names.insert(name.to_string());
        }
    }
    for tool_call in tool_calls {
        if let Some(name) = tool_call
            .get("function")
            .and_then(Value::as_object)
            .and_then(|function| function.get("name"))
            .and_then(Value::as_str)
            && !name.is_empty()
        {
            all_names.insert(name.to_string());
        }
    }
    all_names
}

pub fn build_routing_decision_event_payload(
    routing_meta: &Map<String, Value>,
) -> Map<String, Value> {
    Map::from_iter([
        ("content".to_string(), json!(routing_meta)),
        (
            "metadata".to_string(),
            json!({
                "intent": routing_meta.get("intent").cloned().unwrap_or(Value::Null),
                "tier": routing_meta.get("tier").cloned().unwrap_or(Value::Null),
            }),
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- collect_skill_version_names ---

    #[test]
    fn collect_empty_both() {
        assert!(collect_skill_version_names(&[], &[]).is_empty());
    }

    #[test]
    fn collect_from_results_only() {
        let results = vec![Map::from_iter([("name".to_string(), json!("bash"))])];
        let names = collect_skill_version_names(&results, &[]);
        assert!(names.contains("bash"));
    }

    #[test]
    fn collect_from_calls_only() {
        let calls = vec![Map::from_iter([(
            "function".to_string(),
            json!({"name": "read_file"}),
        )])];
        let names = collect_skill_version_names(&[], &calls);
        assert!(names.contains("read_file"));
    }

    #[test]
    fn collect_deduplicates() {
        let results = vec![Map::from_iter([("name".to_string(), json!("bash"))])];
        let calls = vec![Map::from_iter([(
            "function".to_string(),
            json!({"name": "bash"}),
        )])];
        let names = collect_skill_version_names(&results, &calls);
        assert_eq!(names.len(), 1);
    }

    #[test]
    fn collect_skips_empty_names() {
        let results = vec![Map::from_iter([("name".to_string(), json!(""))])];
        assert!(collect_skill_version_names(&results, &[]).is_empty());
    }

    #[test]
    fn collect_skips_missing_name() {
        let results = vec![Map::from_iter([("other".to_string(), json!("x"))])];
        assert!(collect_skill_version_names(&results, &[]).is_empty());
    }

    // --- build_routing_decision_event_payload ---

    #[test]
    fn routing_payload_with_fields() {
        let meta = Map::from_iter([
            ("intent".to_string(), json!("code_gen")),
            ("tier".to_string(), json!("premium")),
        ]);
        let payload = build_routing_decision_event_payload(&meta);
        assert_eq!(payload["metadata"]["intent"].as_str().unwrap(), "code_gen");
        assert_eq!(payload["metadata"]["tier"].as_str().unwrap(), "premium");
    }

    #[test]
    fn routing_payload_missing_fields() {
        let meta = Map::new();
        let payload = build_routing_decision_event_payload(&meta);
        assert!(payload["metadata"]["intent"].is_null());
        assert!(payload["metadata"]["tier"].is_null());
    }
}
