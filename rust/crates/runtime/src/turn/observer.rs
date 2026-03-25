use serde_json::{Map, Value};

pub fn should_run_observer(full_text: &str, has_tool_calls: bool) -> bool {
    !full_text.is_empty() && !has_tool_calls
}

pub fn build_observer_messages(
    user_content: Option<&str>,
    full_text: &str,
) -> Vec<Map<String, Value>> {
    let mut messages = Vec::new();
    if let Some(user_content) = user_content.filter(|content| !content.is_empty()) {
        messages.push(Map::from_iter([
            ("role".to_string(), Value::from("user")),
            ("content".to_string(), Value::from(user_content)),
        ]));
    }
    messages.push(Map::from_iter([
        ("role".to_string(), Value::from("assistant")),
        ("content".to_string(), Value::from(full_text)),
    ]));
    messages
}
