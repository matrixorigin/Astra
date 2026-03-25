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
