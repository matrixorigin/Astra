use regex::Regex;
use serde_json::{Map, Value};

pub fn classify_task(messages: &[Map<String, Value>]) -> Option<String> {
    let text = messages
        .iter()
        .filter(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        .filter_map(|message| message.get("content").and_then(Value::as_str))
        .next_back()
        .unwrap_or("");
    if text.is_empty() {
        return None;
    }

    let lower = text.to_lowercase();
    if lower.contains("```") {
        return Some("code".to_string());
    }

    let file_ext = Regex::new(r"(^|[^A-Za-z0-9_])\.(py|go|ts|js|rs|java|cpp|rb)\b")
        .expect("file extension regex should compile");
    if file_ext.is_match(&lower) {
        return Some("code".to_string());
    }

    let reasoning = Regex::new(r"\b(explain|analyze|reason|compare)\b")
        .expect("reasoning regex should compile");
    if reasoning.is_match(&lower) {
        return Some("reasoning".to_string());
    }

    None
}
