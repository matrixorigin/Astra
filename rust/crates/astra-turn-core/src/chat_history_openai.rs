//! Build OpenAI-shaped `messages` JSON from REPL (user, assistant) pairs + current user line.

use serde_json::{Value, json};

/// Convert REPL history plus the current user message into `messages` for `/chat` payloads.
/// Empty `user` in a pair means compacted context: only the assistant summary is kept.
///
/// Prefetched context (`<prefetched_context>…</prefetched_context>`) injected in
/// previous turns is stripped from history — it was only relevant for that turn's
/// LLM call and would waste tokens if carried forward.
pub fn openai_messages_from_repl_history(
    history: &[(String, String)],
    current_user_message: &str,
) -> Vec<Value> {
    let mut messages: Vec<Value> = history
        .iter()
        .flat_map(|(u, a)| {
            let mut pair = Vec::with_capacity(2);
            if !u.is_empty() {
                pair.push(json!({"role": "user", "content": strip_prefetched_context(u)}));
            }
            if !a.is_empty() {
                pair.push(json!({"role": "assistant", "content": a}));
            }
            pair
        })
        .collect();
    messages.push(json!({"role": "user", "content": current_user_message}));
    messages
}

/// Remove `<prefetched_context>…</prefetched_context>` blocks from a string.
/// The block is ephemeral — only useful for the turn it was injected into.
fn strip_prefetched_context(s: &str) -> String {
    const OPEN: &str = "\n\n<prefetched_context>";
    const CLOSE: &str = "</prefetched_context>";
    if let Some(start) = s.find(OPEN) {
        if let Some(end) = s[start..].find(CLOSE) {
            let mut out = String::with_capacity(s.len());
            out.push_str(&s[..start]);
            out.push_str(&s[start + end + CLOSE.len()..]);
            return out;
        }
    }
    s.to_string()
}

/// Injected `role: user` row (guard nudges, intent-drift correction, etc.).
#[must_use]
pub fn openai_user_content_message(content: &str) -> Value {
    json!({ "role": "user", "content": content })
}

/// Append TurnGuard / stall injection strings as consecutive `user` rows.
pub fn append_openai_user_content_messages(messages: &mut Vec<Value>, contents: &[String]) {
    for c in contents {
        messages.push(openai_user_content_message(c));
    }
}

/// Deduplicated append of skill names selected this round (cross-turn telemetry).
pub fn merge_skill_names_track(all_selected: &mut Vec<String>, round_skills: &[String]) {
    for skill_name in round_skills {
        if !all_selected.contains(skill_name) {
            all_selected.push(skill_name.clone());
        }
    }
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

    #[test]
    fn openai_user_content_message_shape() {
        let v = openai_user_content_message("fix drift");
        assert_eq!(v["role"], "user");
        assert_eq!(v["content"], "fix drift");
    }

    #[test]
    fn merge_skill_names_track_dedupes() {
        let mut v = vec!["a".into()];
        merge_skill_names_track(&mut v, &["b".into(), "a".into(), "c".into()]);
        assert_eq!(v, vec!["a", "b", "c"]);
    }

    #[test]
    fn append_openai_user_content_messages_extends_vec() {
        let mut m = vec![json!({"role":"assistant","content":"a"})];
        append_openai_user_content_messages(&mut m, &["n1".into(), "n2".into()]);
        assert_eq!(m.len(), 3);
        assert_eq!(m[1]["content"], "n1");
        assert_eq!(m[2]["content"], "n2");
    }

    #[test]
    fn empty_assistant_is_filtered_out() {
        // Interrupted turn: user sent message but assistant response is empty.
        // Must not produce {"role":"assistant","content":""} — LLM API rejects it.
        let m = openai_messages_from_repl_history(&[("question".into(), String::new())], "retry");
        assert_eq!(m.len(), 2);
        assert_eq!(m[0]["role"], "user");
        assert_eq!(m[0]["content"], "question");
        assert_eq!(m[1]["role"], "user");
        assert_eq!(m[1]["content"], "retry");
    }

    #[test]
    fn both_empty_pair_is_skipped() {
        let m = openai_messages_from_repl_history(&[(String::new(), String::new())], "hi");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0]["content"], "hi");
    }

    #[test]
    fn strip_prefetched_context_from_history() {
        let user_with_ctx = "review commit\n\n<prefetched_context>\nguidance\n\ndiff here\n</prefetched_context>";
        let m = openai_messages_from_repl_history(
            &[(user_with_ctx.into(), "looks good".into())],
            "next question",
        );
        let hist_content = m[0]["content"].as_str().unwrap();
        assert!(!hist_content.contains("<prefetched_context>"), "history should not contain prefetched context");
        assert_eq!(hist_content, "review commit");
    }

    #[test]
    fn strip_prefetched_context_preserves_current_message() {
        // Current user message is NOT stripped — it's the active turn.
        let current = "review\n\n<prefetched_context>\nfresh diff\n</prefetched_context>";
        let m = openai_messages_from_repl_history(&[], current);
        assert!(m[0]["content"].as_str().unwrap().contains("<prefetched_context>"));
    }

    #[test]
    fn strip_prefetched_context_no_match() {
        let plain = "just a normal message";
        assert_eq!(strip_prefetched_context(plain), plain);
    }
}
