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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_msg(role: &str, content: &str) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("role".into(), json!(role));
        m.insert("content".into(), json!(content));
        m
    }

    #[test]
    fn classify_empty_messages() {
        assert_eq!(classify_task(&[]), None);
    }

    #[test]
    fn classify_no_user_messages() {
        let msgs = vec![make_msg("assistant", "hello")];
        assert_eq!(classify_task(&msgs), None);
    }

    #[test]
    fn classify_empty_user_message() {
        let msgs = vec![make_msg("user", "")];
        assert_eq!(classify_task(&msgs), None);
    }

    #[test]
    fn classify_code_block_detected() {
        let msgs = vec![make_msg("user", "Fix this:\n```rust\nfn main() {}\n```")];
        assert_eq!(classify_task(&msgs), Some("code".into()));
    }

    #[test]
    fn classify_file_extension_detected() {
        let msgs = vec![make_msg("user", "Edit the .rs file")];
        assert_eq!(classify_task(&msgs), Some("code".into()));
    }

    #[test]
    fn classify_file_extension_various() {
        for ext in &[".py", ".go", ".ts", ".js", ".java", ".cpp", ".rb"] {
            let msgs = vec![make_msg("user", &format!("Check {}", ext))];
            assert_eq!(classify_task(&msgs), Some("code".into()), "failed for {}", ext);
        }
    }

    #[test]
    fn classify_reasoning_keywords() {
        for keyword in &["explain", "analyze", "reason", "compare"] {
            let msgs = vec![make_msg("user", &format!("Please {} this", keyword))];
            assert_eq!(classify_task(&msgs), Some("reasoning".into()), "failed for {}", keyword);
        }
    }

    #[test]
    fn classify_no_match_returns_none() {
        let msgs = vec![make_msg("user", "hello world")];
        assert_eq!(classify_task(&msgs), None);
    }

    #[test]
    fn classify_uses_last_user_message() {
        let msgs = vec![
            make_msg("user", "explain this"),
            make_msg("assistant", "ok"),
            make_msg("user", "hello world"),
        ];
        // Last user message is "hello world" — no match
        assert_eq!(classify_task(&msgs), None);
    }

    #[test]
    fn classify_code_takes_priority_over_reasoning() {
        // Code block check happens before reasoning keyword check
        let msgs = vec![make_msg("user", "explain this ```code```")];
        assert_eq!(classify_task(&msgs), Some("code".into()));
    }

    #[test]
    fn classify_missing_content_field() {
        let mut m = Map::new();
        m.insert("role".into(), json!("user"));
        // No "content" field
        assert_eq!(classify_task(&[m]), None);
    }

    #[test]
    fn classify_file_ext_not_in_word() {
        // ".rs" must be preceded by non-alphanumeric — "furs" should NOT match
        let msgs = vec![make_msg("user", "the word furs should not match")];
        assert_eq!(classify_task(&msgs), None);
    }
}
