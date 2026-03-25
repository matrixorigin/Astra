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
