//! Build OpenAI-shaped `messages` JSON from REPL (user, assistant) pairs + current user line.

use serde_json::{Value, json};

/// Convert REPL history plus the current user message into `messages` for `/chat` payloads.
/// Empty `user` in a pair means compacted context: only the assistant summary is kept.
pub fn openai_messages_from_repl_history(
    history: &[(String, String)],
    current_user_message: &str,
) -> Vec<Value> {
    let mut messages: Vec<Value> = history
        .iter()
        .flat_map(|(u, a)| {
            if u.is_empty() {
                vec![json!({"role": "assistant", "content": a})]
            } else {
                vec![
                    json!({"role": "user", "content": u}),
                    json!({"role": "assistant", "content": a}),
                ]
            }
        })
        .collect();
    messages.push(json!({"role": "user", "content": current_user_message}));
    messages
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_history_only_current_user() {
        let m = openai_messages_from_repl_history(&[], "hi");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0]["role"], "user");
        assert_eq!(m[0]["content"], "hi");
    }

    #[test]
    fn compacted_pair_skips_user_role() {
        let m = openai_messages_from_repl_history(&[(String::new(), "summary".into())], "next");
        assert_eq!(m.len(), 2);
        assert_eq!(m[0]["role"], "assistant");
        assert_eq!(m[0]["content"], "summary");
        assert_eq!(m[1]["content"], "next");
    }

    #[test]
    fn normal_turn_then_current() {
        let m = openai_messages_from_repl_history(
            &[("u1".into(), "a1".into()), ("u2".into(), "a2".into())],
            "u3",
        );
        assert_eq!(m.len(), 5);
        assert_eq!(m[0]["role"], "user");
        assert_eq!(m[0]["content"], "u1");
        assert_eq!(m[4]["content"], "u3");
    }
}
