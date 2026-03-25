use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudLoopIterationPlan {
    pub cloud_tool_calls: Vec<Value>,
    pub edge_tool_calls: Vec<Value>,
    pub assistant_message: Option<Value>,
    pub history_message: Option<Value>,
}

pub fn plan_cloud_loop_iteration(
    loop_tool_calls: &[Value],
    cloud_skill_names: &BTreeSet<String>,
    loop_text: &str,
    loop_reasoning: Option<&str>,
) -> CloudLoopIterationPlan {
    let mut cloud_tool_calls = Vec::new();
    let mut edge_tool_calls = Vec::new();
    for tool_call in loop_tool_calls {
        let tool_name = tool_call
            .get("function")
            .and_then(|value| value.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if cloud_skill_names.contains(tool_name) {
            cloud_tool_calls.push(tool_call.clone());
        } else {
            edge_tool_calls.push(tool_call.clone());
        }
    }

    let (assistant_message, history_message) = if cloud_tool_calls.is_empty() {
        (None, None)
    } else {
        let tool_entries = cloud_tool_calls
            .iter()
            .map(|tool_call| {
                json!({
                    "id": tool_call.get("id").and_then(Value::as_str).unwrap_or_default(),
                    "type": "function",
                    "function": tool_call.get("function").cloned().unwrap_or_else(|| json!({})),
                })
            })
            .collect::<Vec<_>>();
        let mut assistant_message = json!({
            "role": "assistant",
            "tool_calls": tool_entries,
        });
        if !loop_text.is_empty() {
            assistant_message["content"] = Value::String(loop_text.to_string());
        }
        if let Some(reasoning) = loop_reasoning.filter(|value| !value.is_empty()) {
            assistant_message["reasoning_content"] = Value::String(reasoning.to_string());
        }

        let mut history_message = json!({
            "role": "assistant",
            "content": Value::Null,
            "tool_calls": assistant_message.get("tool_calls").cloned().unwrap_or_else(|| json!([])),
        });
        if let Some(reasoning) = loop_reasoning.filter(|value| !value.is_empty()) {
            history_message["reasoning_content"] = Value::String(reasoning.to_string());
        }
        (Some(assistant_message), Some(history_message))
    };

    CloudLoopIterationPlan {
        cloud_tool_calls,
        edge_tool_calls,
        assistant_message,
        history_message,
    }
}
