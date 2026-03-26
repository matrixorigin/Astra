use std::collections::BTreeSet;

use serde_json::{Map, Value};

pub fn latest_assistant_tool_call_ids(history: &[Map<String, Value>]) -> BTreeSet<String> {
    history
        .iter()
        .rev()
        .find_map(|message| {
            if message.get("role").and_then(Value::as_str) == Some("assistant") {
                message
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .map(|tool_calls| {
                        tool_calls
                            .iter()
                            .filter_map(|tool_call| {
                                tool_call
                                    .get("id")
                                    .and_then(Value::as_str)
                                    .map(ToString::to_string)
                            })
                            .collect::<BTreeSet<_>>()
                    })
            } else {
                None
            }
        })
        .unwrap_or_default()
}

pub fn build_unconsumed_tool_messages(
    tool_results: &[Value],
    consumed: &BTreeSet<String>,
    allowed_tool_call_ids: &BTreeSet<String>,
) -> Vec<Map<String, Value>> {
    tool_results
        .iter()
        .filter_map(|tool_result| {
            let object = tool_result.as_object()?;
            let tool_call_id = object
                .get("tool_call_id")
                .and_then(Value::as_str)
                .unwrap_or("");
            if tool_call_id.is_empty()
                || consumed.contains(tool_call_id)
                || !allowed_tool_call_ids.contains(tool_call_id)
            {
                return None;
            }
            let raw_content = object.get("result").map(|value| match value {
                Value::String(text) => text.clone(),
                other => json_stringify(other),
            })?;
            // Prompt injection guard: delimit tool output so the LLM treats
            // it as data, not instructions.  Any injected "system" directives
            // inside tool output are contained within these markers.
            let content = format!("[TOOL OUTPUT]\n{}\n[/TOOL OUTPUT]", raw_content);
            Some(Map::from_iter([
                ("role".to_string(), Value::from("tool")),
                ("tool_call_id".to_string(), Value::from(tool_call_id)),
                ("content".to_string(), Value::from(content)),
            ]))
        })
        .collect()
}

fn json_stringify(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}
