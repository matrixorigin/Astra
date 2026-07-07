//! Agentic `/chat` JSON payload steps after tool schemas are resolved.

use std::collections::HashSet;

use serde_json::Value;

use crate::chat_turn_payload::attach_filtered_edge_tools;

/// Attach restricted-filtered `edge_tools` to the payload.
pub fn attach_filtered_edge_tools_to_payload(
    payload: &mut Value,
    turn_schemas: Vec<Value>,
    restricted_tools: &HashSet<String>,
) {
    attach_filtered_edge_tools(payload, turn_schemas, restricted_tools);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn attaches_filtered_edge_tools() {
        let mut payload = json!({
            "edge_profile": { "cwd": "/tmp" }
        });
        let schemas = vec![json!({"function": {"name": "grep"}})];
        let restricted = HashSet::new();

        attach_filtered_edge_tools_to_payload(&mut payload, schemas, &restricted);

        assert_eq!(payload["edge_tools"][0]["function"]["name"], "grep");
    }

    #[test]
    fn restricted_tools_are_not_attached() {
        let mut payload = json!({
            "edge_profile": { "cwd": "/tmp" }
        });
        let schemas = vec![json!({"function": {"name": "grep"}})];
        let restricted = HashSet::from(["grep".to_string()]);

        attach_filtered_edge_tools_to_payload(&mut payload, schemas, &restricted);

        assert_eq!(payload["edge_tools"].as_array().unwrap().len(), 0);
    }
}
