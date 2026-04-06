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

#[cfg(test)]
mod tests {
    use super::*;

    fn user(content: &str) -> Value {
        json!({"role": "user", "content": content})
    }

    fn assistant(content: &str) -> Value {
        json!({"role": "assistant", "content": content})
    }

    fn system(content: &str) -> Value {
        json!({"role": "system", "content": content})
    }

    // ──────────────────────────────────────────────────────────
    // extract_latest_user_query
    // ──────────────────────────────────────────────────────────

    #[test]
    fn extract_user_query_finds_last_user() {
        let msgs = vec![user("first"), assistant("reply"), user("second")];
        assert_eq!(extract_latest_user_query(&msgs), "second");
    }

    #[test]
    fn extract_user_query_empty_messages() {
        assert_eq!(extract_latest_user_query(&[]), "");
    }

    #[test]
    fn extract_user_query_no_user_message() {
        let msgs = vec![assistant("hello"), system("sys")];
        assert_eq!(extract_latest_user_query(&msgs), "");
    }

    #[test]
    fn extract_user_query_skips_empty_content() {
        let msgs = vec![user("real"), user("")];
        assert_eq!(extract_latest_user_query(&msgs), "real");
    }

    // ──────────────────────────────────────────────────────────
    // compose_retrieval_view
    // ──────────────────────────────────────────────────────────

    #[test]
    fn compose_with_all_parts() {
        let sys_map = system("sys prompt")
            .as_object()
            .unwrap()
            .clone();
        let recent = vec![user("q"), assistant("a")];
        let result = compose_retrieval_view(Some(&sys_map), Some("retrieved"), &recent);
        assert_eq!(result.len(), 4); // system + retrieved + 2 recent
        assert_eq!(result[0]["role"], "system");
        assert_eq!(result[1]["content"], "retrieved");
    }

    #[test]
    fn compose_without_system_or_retrieved() {
        let recent = vec![user("q")];
        let result = compose_retrieval_view(None, None, &recent);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn compose_empty_retrieved_block_skipped() {
        let recent = vec![user("q")];
        let result = compose_retrieval_view(None, Some(""), &recent);
        assert_eq!(result.len(), 1); // empty block skipped
    }

    // ──────────────────────────────────────────────────────────
    // plan_retrieval_inputs
    // ──────────────────────────────────────────────────────────

    #[test]
    fn plan_retrieval_empty_history_returns_none() {
        assert!(plan_retrieval_inputs(&[], &[user("q")], 2, 4).is_none());
    }

    #[test]
    fn plan_retrieval_below_min_history_returns_none() {
        let h = vec![system("s"), user("u")];
        assert!(plan_retrieval_inputs(&h, &[user("q")], 5, 4).is_none());
    }

    #[test]
    fn plan_retrieval_no_user_query_returns_none() {
        let h = vec![system("s"), user("u"), assistant("a"), user("u2"), assistant("a2")];
        let current = vec![assistant("no user here")];
        assert!(plan_retrieval_inputs(&h, &current, 2, 4).is_none());
    }

    #[test]
    fn plan_retrieval_valid_returns_plan() {
        let h = vec![system("s"), user("u1"), assistant("a1"), user("u2"), assistant("a2")];
        let current = vec![user("my question")];
        let plan = plan_retrieval_inputs(&h, &current, 2, 4).unwrap();
        assert_eq!(plan.user_query, "my question");
        assert!(plan.system_message.is_some());
    }

    #[test]
    fn plan_retrieval_extracts_system_from_history() {
        let h = vec![system("sys"), user("u1"), assistant("a1")];
        let current = vec![user("q")];
        let plan = plan_retrieval_inputs(&h, &current, 1, 4).unwrap();
        let sys = plan.system_message.unwrap();
        assert_eq!(sys["content"], "sys");
    }

    #[test]
    fn plan_retrieval_no_system_in_history() {
        let h = vec![user("u1"), assistant("a1"), user("u2")];
        let current = vec![user("q")];
        let plan = plan_retrieval_inputs(&h, &current, 1, 4).unwrap();
        assert!(plan.system_message.is_none());
    }
}
