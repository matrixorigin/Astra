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
            let mut pair = Vec::with_capacity(2);
            if !u.is_empty() {
                pair.push(json!({"role": "user", "content": u}));
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

/// Remove empty `tool_calls: []` arrays from assistant messages in-place.
///
/// Some providers (e.g. MiniMax) reject messages with `tool_calls: []`.
/// This is the single source of truth — all compaction, history-building,
/// and LLM-request paths should call this rather than inlining the logic.
pub fn sanitize_empty_assistant_tool_calls_mut(messages: &mut [Value]) {
    for message in messages {
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(obj) = message.as_object_mut() else {
            continue;
        };
        if obj
            .get("tool_calls")
            .and_then(Value::as_array)
            .is_some_and(|arr| arr.is_empty())
        {
            obj.remove("tool_calls");
        }
    }
}

/// Clone-based variant for call sites that take `&Value` (e.g. grouping).
pub fn sanitize_empty_assistant_tool_calls_cloned(message: &Value) -> Value {
    let mut message = message.clone();
    if let Some(obj) = message.as_object_mut() {
        if obj.get("role").and_then(Value::as_str) == Some("assistant")
            && obj
                .get("tool_calls")
                .and_then(Value::as_array)
                .is_some_and(|arr| arr.is_empty())
        {
            obj.remove("tool_calls");
        }
    }
    message
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
    fn sanitize_mut_removes_empty_tool_calls() {
        let mut msgs = vec![
            serde_json::json!({"role": "assistant", "content": "done", "tool_calls": []}),
            serde_json::json!({"role": "assistant", "content": null, "tool_calls": [{"id":"c1"}]}),
            serde_json::json!({"role": "user", "content": "hi"}),
        ];
        sanitize_empty_assistant_tool_calls_mut(&mut msgs);
        assert!(msgs[0].get("tool_calls").is_none(), "{:?}", msgs[0]);
        assert!(msgs[1].get("tool_calls").is_some());
        assert!(msgs[2].get("tool_calls").is_none());
    }

    #[test]
    fn sanitize_cloned_removes_empty_tool_calls() {
        let msg = serde_json::json!({"role": "assistant", "content": "done", "tool_calls": []});
        let out = sanitize_empty_assistant_tool_calls_cloned(&msg);
        assert!(out.get("tool_calls").is_none(), "{out:?}");
        // Non-empty preserved
        let msg2 =
            serde_json::json!({"role": "assistant", "content": null, "tool_calls": [{"id":"c1"}]});
        let out2 = sanitize_empty_assistant_tool_calls_cloned(&msg2);
        assert!(out2.get("tool_calls").is_some());
    }
}
