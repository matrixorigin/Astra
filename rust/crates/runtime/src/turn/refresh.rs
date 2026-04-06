use serde_json::Value;

pub fn extract_first_user_query(messages: &[Value]) -> String {
    messages
        .iter()
        .find_map(|message| {
            let object = message.as_object()?;
            if object.get("role").and_then(Value::as_str) == Some("user") {
                Some(
                    object
                        .get("content")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                )
            } else {
                None
            }
        })
        .unwrap_or_default()
}

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

pub fn plan_memory_refresh(
    messages: &[Value],
    tool_results: Option<&[Value]>,
    history: Option<&[Value]>,
) -> Option<String> {
    let user_query = extract_first_user_query(messages);
    if user_query.is_empty()
        && tool_results
            .map(|results| results.is_empty())
            .unwrap_or(true)
    {
        return None;
    }
    if !user_query.is_empty() {
        return Some(user_query);
    }
    history.and_then(|history| {
        history.iter().rev().find_map(|message| {
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- extract_first_user_query ---

    #[test]
    fn first_query_empty_messages() {
        assert_eq!(extract_first_user_query(&[]), "");
    }

    #[test]
    fn first_query_no_user_role() {
        let msgs = [json!({"role": "assistant", "content": "hi"})];
        assert_eq!(extract_first_user_query(&msgs), "");
    }

    #[test]
    fn first_query_found() {
        let msgs = [
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "hello"}),
            json!({"role": "user", "content": "world"}),
        ];
        assert_eq!(extract_first_user_query(&msgs), "hello");
    }

    #[test]
    fn first_query_missing_content() {
        let msgs = [json!({"role": "user"})];
        assert_eq!(extract_first_user_query(&msgs), "");
    }

    #[test]
    fn first_query_non_string_content() {
        let msgs = [json!({"role": "user", "content": 42})];
        assert_eq!(extract_first_user_query(&msgs), "");
    }

    #[test]
    fn first_query_non_object_message() {
        let msgs = [json!("just a string")];
        assert_eq!(extract_first_user_query(&msgs), "");
    }

    // --- extract_latest_user_query ---

    #[test]
    fn latest_query_empty() {
        assert_eq!(extract_latest_user_query(&[]), "");
    }

    #[test]
    fn latest_query_skips_empty_content() {
        let msgs = [
            json!({"role": "user", "content": "first"}),
            json!({"role": "user", "content": ""}),
        ];
        assert_eq!(extract_latest_user_query(&msgs), "first");
    }

    #[test]
    fn latest_query_returns_last() {
        let msgs = [
            json!({"role": "user", "content": "old"}),
            json!({"role": "assistant", "content": "resp"}),
            json!({"role": "user", "content": "new"}),
        ];
        assert_eq!(extract_latest_user_query(&msgs), "new");
    }

    #[test]
    fn latest_query_all_empty_content() {
        let msgs = [
            json!({"role": "user", "content": ""}),
            json!({"role": "user", "content": ""}),
        ];
        assert_eq!(extract_latest_user_query(&msgs), "");
    }

    // --- plan_memory_refresh ---

    #[test]
    fn plan_all_empty() {
        assert!(plan_memory_refresh(&[], None, None).is_none());
    }

    #[test]
    fn plan_user_query_present() {
        let msgs = [json!({"role": "user", "content": "search this"})];
        assert_eq!(
            plan_memory_refresh(&msgs, None, None),
            Some("search this".to_string())
        );
    }

    #[test]
    fn plan_no_user_query_empty_tool_results() {
        let msgs = [json!({"role": "system", "content": "sys"})];
        let empty: &[Value] = &[];
        assert!(plan_memory_refresh(&msgs, Some(empty), None).is_none());
    }

    #[test]
    fn plan_no_user_query_has_tool_results_falls_to_history() {
        let msgs = [json!({"role": "system", "content": "sys"})];
        let results = [json!({"tool_call_id": "tc1"})];
        let history = [
            json!({"role": "user", "content": "from history"}),
            json!({"role": "assistant", "content": "resp"}),
        ];
        assert_eq!(
            plan_memory_refresh(&msgs, Some(&results), Some(&history)),
            Some("from history".to_string())
        );
    }

    #[test]
    fn plan_no_user_query_has_tool_results_no_history() {
        let msgs = [json!({"role": "system", "content": "sys"})];
        let results = [json!({"tool_call_id": "tc1"})];
        assert!(plan_memory_refresh(&msgs, Some(&results), None).is_none());
    }

    #[test]
    fn plan_no_user_query_has_tool_results_empty_history() {
        let msgs = [json!({"role": "system", "content": "sys"})];
        let results = [json!({"tool_call_id": "tc1"})];
        let history: &[Value] = &[];
        assert!(plan_memory_refresh(&msgs, Some(&results), Some(history)).is_none());
    }
}
