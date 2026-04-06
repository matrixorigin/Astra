use serde_json::Value;

use crate::{
    build_unconsumed_tool_messages, latest_assistant_tool_call_ids, merge_tool_results_into_history,
};

pub fn apply_turn_inputs_to_history(
    history: &[Value],
    messages: &[Value],
    tool_results: &[Value],
) -> Vec<Value> {
    let mut next_history = history.to_vec();
    let consumed = merge_tool_results_into_history(&mut next_history, Some(tool_results));
    for message in messages {
        if let Some(object) = message.as_object()
            && object.get("role").and_then(Value::as_str).is_some()
            && object.get("content").and_then(Value::as_str).is_some()
        {
            next_history.push(message.clone());
        }
    }
    let history_objects = next_history
        .iter()
        .filter_map(Value::as_object)
        .cloned()
        .collect::<Vec<_>>();
    let allowed = latest_assistant_tool_call_ids(&history_objects);
    next_history.extend(
        build_unconsumed_tool_messages(tool_results, &consumed, &allowed)
            .into_iter()
            .map(Value::Object),
    );
    next_history
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_all() {
        let result = apply_turn_inputs_to_history(&[], &[], &[]);
        assert!(result.is_empty());
    }

    #[test]
    fn passes_through_existing_history() {
        let history = vec![json!({"role": "user", "content": "hi"})];
        let result = apply_turn_inputs_to_history(&history, &[], &[]);
        assert!(!result.is_empty());
        assert_eq!(result[0]["content"], "hi");
    }

    #[test]
    fn filters_invalid_messages() {
        let messages = vec![
            json!({"role": "user", "content": "valid"}),
            json!({"no_role": true}),
            json!({"role": "user"}), // no content
            json!("not an object"),
        ];
        let result = apply_turn_inputs_to_history(&[], &messages, &[]);
        let user_msgs: Vec<_> = result.iter().filter(|m| m["role"] == "user").collect();
        assert_eq!(user_msgs.len(), 1);
    }

    #[test]
    fn tool_results_merged() {
        let history = vec![json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{"id": "tc1", "type": "function", "function": {"name": "bash", "arguments": "{}"}}]
        })];
        let tool_results = vec![json!({
            "role": "tool",
            "tool_call_id": "tc1",
            "content": "output"
        })];
        let result = apply_turn_inputs_to_history(&history, &[], &tool_results);
        let tool_msgs: Vec<_> = result.iter().filter(|m| m["role"] == "tool").collect();
        assert!(!tool_msgs.is_empty());
    }
}
