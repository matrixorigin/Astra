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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_run_text_no_calls() {
        assert!(should_run_observer("hello", false));
    }

    #[test]
    fn should_run_empty_text() {
        assert!(!should_run_observer("", false));
    }

    #[test]
    fn should_run_has_tool_calls() {
        assert!(!should_run_observer("hello", true));
    }

    #[test]
    fn observer_messages_with_user() {
        let msgs = build_observer_messages(Some("question"), "answer");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"].as_str().unwrap(), "user");
        assert_eq!(msgs[1]["role"].as_str().unwrap(), "assistant");
    }

    #[test]
    fn observer_messages_no_user() {
        let msgs = build_observer_messages(None, "answer");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"].as_str().unwrap(), "assistant");
    }

    #[test]
    fn observer_messages_empty_user_content() {
        let msgs = build_observer_messages(Some(""), "answer");
        assert_eq!(msgs.len(), 1);
    }
}
