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
