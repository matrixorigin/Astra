use serde_json::{Map, Value};

pub const RETRIEVAL_BUDGET_CHARS: usize = 8000;
const RETRIEVAL_HEADER: &str = "[Earlier relevant context from this session]\n";

pub fn format_retrieved_events(
    events: &[Map<String, Value>],
    recent_contents: &[String],
    budget_chars: usize,
) -> Option<String> {
    let mut parts = Vec::new();
    let mut used_chars = 0usize;

    for event in events {
        let Some(content) = event.get("content").and_then(Value::as_str) else {
            continue;
        };
        if content.is_empty() {
            continue;
        }
        if recent_contents
            .iter()
            .any(|recent| recent == &prefix_chars(content, 100))
        {
            continue;
        }
        let line = match event
            .get("event_type")
            .and_then(Value::as_str)
            .unwrap_or("")
        {
            "user_query" => format!("User: {}", prefix_chars(content, 200)),
            "llm_response" => format!("Assistant: {}", prefix_chars(content, 300)),
            "tool_result" => format!("Tool result: {}", prefix_chars(content, 300)),
            _ => continue,
        };
        if used_chars + line.len() > budget_chars {
            break;
        }
        used_chars += line.len();
        parts.push(line);
    }

    if parts.is_empty() {
        None
    } else {
        Some(format!("{RETRIEVAL_HEADER}{}", parts.join("\n")))
    }
}

pub fn rule_based_extraction(
    full_history: &[Map<String, Value>],
    recent_messages: &[Map<String, Value>],
    user_query: &str,
    budget_chars: usize,
) -> Option<String> {
    let query_words = split_words_lower(user_query);
    if query_words.is_empty() {
        return None;
    }

    let old_messages: Vec<&Map<String, Value>> = full_history
        .iter()
        .skip(1)
        .filter(|message| !recent_messages.iter().any(|recent| recent == *message))
        .collect();
    if old_messages.is_empty() {
        return None;
    }

    let mut scored: Vec<(usize, &Map<String, Value>)> = old_messages
        .into_iter()
        .filter_map(|message| {
            let content = message.get("content").and_then(Value::as_str).unwrap_or("");
            if content.is_empty() {
                return None;
            }
            let overlap = split_words_lower(content)
                .into_iter()
                .filter(|word| query_words.contains(word))
                .count();
            if overlap > 0 {
                Some((overlap, message))
            } else {
                None
            }
        })
        .collect();
    if scored.is_empty() {
        return None;
    }

    scored.sort_by(|a, b| b.0.cmp(&a.0));
    let mut parts = Vec::new();
    let mut used_chars = 0usize;
    for (_, message) in scored.into_iter().take(6) {
        let content = message.get("content").and_then(Value::as_str).unwrap_or("");
        let line = match message.get("role").and_then(Value::as_str).unwrap_or("?") {
            "user" => format!("User: {}", prefix_chars(content, 200)),
            "assistant" => format!("Assistant: {}", prefix_chars(content, 300)),
            "tool" => format!("Tool result: {}", prefix_chars(content, 300)),
            _ => continue,
        };
        if used_chars + line.len() > budget_chars {
            break;
        }
        used_chars += line.len();
        parts.push(line);
    }

    if parts.is_empty() {
        None
    } else {
        Some(format!("{RETRIEVAL_HEADER}{}", parts.join("\n")))
    }
}

fn prefix_chars(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

fn split_words_lower(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split_whitespace()
        .map(ToString::to_string)
        .collect()
}
