use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::find_tool_call_safe_split;

pub fn extract_latest_user_query(messages: &[Value]) -> String {
    messages
        .iter()
        .rev()
        .find_map(|message| {
            let object = message.as_object()?;
            if object.get("role").and_then(Value::as_str) == Some("user") {
                object
                    .get("content")
                    .and_then(Value::as_str)
                    .filter(|content| !content.is_empty())
                    .map(ToString::to_string)
            } else {
                None
            }
        })
        .unwrap_or_default()
}

pub fn build_recent_retrieval_tail(history: &[Value], recent_messages_keep: usize) -> Vec<Value> {
    let split_idx = find_tool_call_safe_split(history, recent_messages_keep);
    let recent = history[split_idx..].to_vec();
    if recent
        .first()
        .and_then(Value::as_object)
        .and_then(|message| message.get("role"))
        .and_then(Value::as_str)
        == Some("system")
    {
        recent[1..].to_vec()
    } else {
        recent
    }
}

pub fn compose_retrieval_view(
    system_message: Option<&Map<String, Value>>,
    retrieved_block: Option<&str>,
    recent_messages: &[Value],
) -> Vec<Value> {
    let mut result = Vec::new();
    if let Some(system_message) = system_message {
        result.push(Value::Object(system_message.clone()));
    }
    if let Some(retrieved_block) = retrieved_block.filter(|block| !block.is_empty()) {
        result.push(json!({"role": "system", "content": retrieved_block}));
    }
    result.extend(recent_messages.iter().cloned());
    result
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RetrievalPlan {
    pub system_message: Option<Map<String, Value>>,
    pub recent_messages: Vec<Value>,
    pub user_query: String,
}

pub fn plan_retrieval_inputs(
    history: &[Value],
    current_messages: &[Value],
    min_history: usize,
    recent_messages_keep: usize,
) -> Option<RetrievalPlan> {
    if history.is_empty() || history.len() < min_history {
        return None;
    }
    let user_query = extract_latest_user_query(current_messages);
    if user_query.is_empty() {
        return None;
    }
    Some(RetrievalPlan {
        system_message: history
            .first()
            .and_then(Value::as_object)
            .and_then(|message| {
                if message.get("role").and_then(Value::as_str) == Some("system") {
                    Some(message.clone())
                } else {
                    None
                }
            }),
        recent_messages: build_recent_retrieval_tail(history, recent_messages_keep),
        user_query,
    })
}
