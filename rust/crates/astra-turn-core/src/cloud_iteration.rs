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
    plan_cloud_loop_iteration_ext(
        loop_tool_calls,
        cloud_skill_names,
        loop_text,
        loop_reasoning,
        false,
    )
}

pub fn plan_cloud_loop_iteration_ext(
    loop_tool_calls: &[Value],
    cloud_skill_names: &BTreeSet<String>,
    loop_text: &str,
    loop_reasoning: Option<&str>,
    force_reasoning_field: bool,
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
        } else if force_reasoning_field {
            assistant_message["reasoning_content"] = Value::String(String::new());
        }

        let mut history_message = json!({
            "role": "assistant",
            "content": Value::Null,
            "tool_calls": tool_entries.clone(),
        });
        if let Some(reasoning) = loop_reasoning.filter(|value| !value.is_empty()) {
            history_message["reasoning_content"] = Value::String(reasoning.to_string());
        } else if force_reasoning_field {
            history_message["reasoning_content"] = Value::String(String::new());
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool_call(name: &str, id: &str) -> Value {
        json!({"id": id, "function": {"name": name, "arguments": "{}"}})
    }

    fn cloud_names(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn empty_tool_calls() {
        let plan = plan_cloud_loop_iteration(&[], &cloud_names(&["web"]), "hello", None);
        assert!(plan.cloud_tool_calls.is_empty());
        assert!(plan.edge_tool_calls.is_empty());
        assert!(plan.assistant_message.is_none());
        assert!(plan.history_message.is_none());
    }

    #[test]
    fn all_edge_tools() {
        let tools = vec![tool_call("bash", "1"), tool_call("read", "2")];
        let plan = plan_cloud_loop_iteration(&tools, &cloud_names(&["web"]), "", None);
        assert!(plan.cloud_tool_calls.is_empty());
        assert_eq!(plan.edge_tool_calls.len(), 2);
        assert!(plan.assistant_message.is_none());
    }

    #[test]
    fn all_cloud_tools() {
        let tools = vec![tool_call("web", "1")];
        let plan = plan_cloud_loop_iteration(&tools, &cloud_names(&["web"]), "", None);
        assert_eq!(plan.cloud_tool_calls.len(), 1);
        assert!(plan.edge_tool_calls.is_empty());
        assert!(plan.assistant_message.is_some());
        assert!(plan.history_message.is_some());
    }

    #[test]
    fn mixed_tools_split_correctly() {
        let tools = vec![
            tool_call("web", "1"),
            tool_call("bash", "2"),
            tool_call("web", "3"),
        ];
        let plan = plan_cloud_loop_iteration(&tools, &cloud_names(&["web"]), "", None);
        assert_eq!(plan.cloud_tool_calls.len(), 2);
        assert_eq!(plan.edge_tool_calls.len(), 1);
    }

    #[test]
    fn text_included_in_assistant_message() {
        let tools = vec![tool_call("web", "1")];
        let plan =
            plan_cloud_loop_iteration(&tools, &cloud_names(&["web"]), "thinking aloud", None);
        let msg = plan.assistant_message.unwrap();
        assert_eq!(msg["content"].as_str().unwrap(), "thinking aloud");
    }

    #[test]
    fn empty_text_not_included() {
        let tools = vec![tool_call("web", "1")];
        let plan = plan_cloud_loop_iteration(&tools, &cloud_names(&["web"]), "", None);
        let msg = plan.assistant_message.unwrap();
        assert!(msg.get("content").is_none());
    }

    #[test]
    fn reasoning_included_when_present() {
        let tools = vec![tool_call("web", "1")];
        let plan = plan_cloud_loop_iteration(&tools, &cloud_names(&["web"]), "", Some("because"));
        let msg = plan.assistant_message.unwrap();
        assert_eq!(msg["reasoning_content"].as_str().unwrap(), "because");
    }

    #[test]
    fn reasoning_empty_string_not_included() {
        let tools = vec![tool_call("web", "1")];
        let plan = plan_cloud_loop_iteration(&tools, &cloud_names(&["web"]), "", Some(""));
        let msg = plan.assistant_message.unwrap();
        assert!(msg.get("reasoning_content").is_none());
    }

    #[test]
    fn force_reasoning_adds_empty_string() {
        let tools = vec![tool_call("web", "1")];
        let plan = plan_cloud_loop_iteration_ext(&tools, &cloud_names(&["web"]), "", None, true);
        let msg = plan.assistant_message.unwrap();
        assert_eq!(msg["reasoning_content"].as_str().unwrap(), "");
        let hist = plan.history_message.unwrap();
        assert_eq!(hist["reasoning_content"].as_str().unwrap(), "");
    }

    #[test]
    fn history_message_has_null_content() {
        let tools = vec![tool_call("web", "1")];
        let plan = plan_cloud_loop_iteration(&tools, &cloud_names(&["web"]), "", None);
        let hist = plan.history_message.unwrap();
        assert!(hist["content"].is_null());
    }
}
