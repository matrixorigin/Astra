//! Prompt-visible resume hydration hints.
//!
//! Session resume must not depend on optional tools such as memory,
//! introspect, or task lookup. This module builds a bounded, deterministic
//! hint from prompt-facing history so the next LLM call knows it is continuing
//! an existing session even when those tools are unavailable.

use serde_json::Value;

pub const SESSION_RESUME_PREFIX: &str = "[session-resume:v1]";

const MAX_RECENT_MESSAGES: usize = 8;
const MAX_SUMMARY_ITEMS: usize = 6;
const MAX_USER_INPUT_CHARS: usize = 260;
const MAX_ASSISTANT_CHARS: usize = 360;
const MAX_MESSAGE_CHARS: usize = 220;
const MAX_HINT_CHARS: usize = 2_400;

pub fn build_resume_hydration_hint_from_messages(messages: &[Value]) -> Option<String> {
    let prompt_messages = crate::prompt_facing::sanitize_prompt_facing_messages(messages.to_vec());
    build_resume_hydration_hint_from_prompt_messages(&prompt_messages)
}

pub fn build_resume_hydration_hint_from_prompt_messages(messages: &[Value]) -> Option<String> {
    let entries = prompt_entries(messages);
    if entries.is_empty() {
        return None;
    }

    // Minimum viability: require at least one user message AND one assistant message.
    // A hint built from user-only or assistant-only history provides no meaningful
    // continuity context and risks the model hallucinating prior state.
    let has_user = entries.iter().any(|e| e.role == "user");
    let has_assistant = entries.iter().any(|e| e.role == "assistant");
    if !has_user || !has_assistant {
        return None;
    }

    let latest_user_input = latest_user_input(&entries);
    let last_assistant_state = latest_assistant(&entries);
    let mut lines = vec![
        SESSION_RESUME_PREFIX.to_string(),
        "Hydrated previous session context from stored prompt-facing history.".to_string(),
        "Treat this as the same session, not a new session.".to_string(),
        "Use separately supplied task and active-work evidence as the authority for what remains in progress.".to_string(),
        "Do not claim that no prior conversation exists when this block is present.".to_string(),
        String::new(),
        "Observed conversation tail:".to_string(),
    ];

    match latest_user_input {
        Some(input) => lines.push(format!(
            "- latest_user_input: {}",
            truncate_chars(input, MAX_USER_INPUT_CHARS)
        )),
        None => lines.push("- latest_user_input: unavailable from restored history".to_string()),
    }
    match last_assistant_state {
        Some(state) => lines.push(format!(
            "- last_assistant_state: {}",
            truncate_chars(state, MAX_ASSISTANT_CHARS)
        )),
        None => lines.push("- last_assistant_state: unavailable from restored history".to_string()),
    }

    lines.push(String::new());
    lines.push("Compact transcript summary:".to_string());
    for entry in entries
        .iter()
        .rev()
        .take(MAX_SUMMARY_ITEMS)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        lines.push(format!(
            "- {}: {}",
            entry.role,
            truncate_chars(&entry.text, MAX_MESSAGE_CHARS)
        ));
    }

    lines.push(String::new());
    lines.push("Recent messages:".to_string());
    for entry in entries
        .iter()
        .rev()
        .take(MAX_RECENT_MESSAGES)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        lines.push(format!(
            "- {}: {}",
            entry.role,
            truncate_chars(&entry.text, MAX_MESSAGE_CHARS)
        ));
    }

    Some(truncate_chars(&lines.join("\n"), MAX_HINT_CHARS))
}

pub fn build_resume_hydration_failure_hint(reason: &str) -> String {
    let reason = truncate_chars(reason.trim(), 360);
    format!(
        "{SESSION_RESUME_PREFIX}\n\
Resume context hydration was requested but no prior prompt-facing history could be restored.\n\
Reason: {reason}\n\
Treat this as a degraded resume, not proof of a new session. Do not claim this is a new session. \
If the user asks what happened, state that the stored session context was unavailable or incomplete in this runtime."
    )
}

pub fn build_resume_hydration_failure_hint_for_error(_error: &str) -> String {
    // This boundary receives an untyped storage error string. Do not infer a
    // failure category from wording; typed callers should supply one through
    // `build_resume_hydration_failure_hint` when it is available.
    build_resume_hydration_failure_hint("session restore infrastructure issue")
}

pub fn merge_resume_hints(first: Option<String>, second: Option<String>) -> Option<String> {
    match (
        first
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        second
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
    ) {
        (Some(a), Some(b)) if a == b => Some(a),
        (Some(a), Some(b)) => Some(format!("{a}\n\n{b}")),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

#[derive(Debug)]
struct PromptEntry {
    role: String,
    text: String,
}

fn prompt_entries(messages: &[Value]) -> Vec<PromptEntry> {
    messages
        .iter()
        .filter_map(|message| {
            let role = message.get("role").and_then(Value::as_str)?;
            if !matches!(role, "user" | "assistant" | "system") {
                return None;
            }
            let text = crate::prompt_facing::extract_text_content(message)?;
            let text = normalize_ws(&text);
            if text.is_empty() {
                return None;
            }
            Some(PromptEntry {
                role: role.to_string(),
                text,
            })
        })
        .collect()
}

fn latest_user_input(entries: &[PromptEntry]) -> Option<&str> {
    entries
        .iter()
        .rev()
        .find(|entry| entry.role == "user")
        .map(|entry| entry.text.as_str())
}

fn latest_assistant(entries: &[PromptEntry]) -> Option<&str> {
    entries
        .iter()
        .rev()
        .find(|entry| entry.role == "assistant")
        .map(|entry| entry.text.as_str())
}

fn normalize_ws(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars.saturating_sub(3)).collect();
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn resume_hydration_builds_active_worklog_from_prior_task() {
        let messages = vec![
            json!({"role": "user", "content": "3agents review current branch changes"}),
            json!({"role": "assistant", "content": "I will inspect diff, stat, and run parallel reviews."}),
            json!({"role": "assistant", "tool_calls": [{"id": "c1", "function": {"name": "git"}}]}),
            json!({"role": "tool", "tool_call_id": "c1", "content": "202 files changed"}),
            json!({"role": "user", "content": "继续"}),
            json!({"role": "assistant", "content": "Continuing the branch review from the diff."}),
        ];

        let hint = build_resume_hydration_hint_from_messages(&messages).expect("hint");

        assert!(hint.starts_with(SESSION_RESUME_PREFIX));
        assert!(hint.contains("Treat this as the same session"));
        assert!(hint.contains("latest_user_input: 继续"));
        assert!(hint.contains("last_assistant_state: Continuing the branch review"));
        assert!(!hint.contains("[Runtime tool result]"));
        assert!(!hint.contains("202 files changed"));
    }

    #[test]
    fn resume_hydration_refuses_user_only_or_assistant_only_history() {
        let user_only = vec![json!({"role": "user", "content": "继续"})];
        let assistant_only = vec![json!({"role": "assistant", "content": "好的，继续进行"})];

        assert!(
            build_resume_hydration_hint_from_prompt_messages(&user_only).is_none(),
            "user-only messages should not create resume hints"
        );
        assert!(
            build_resume_hydration_hint_from_prompt_messages(&assistant_only).is_none(),
            "assistant-only messages should not create resume hints"
        );
    }

    #[test]
    fn resume_hydration_failure_forbids_new_session_claim() {
        let hint = build_resume_hydration_failure_hint("checkpoint unavailable");

        assert!(hint.contains("degraded resume"));
        assert!(hint.contains("Do not claim this is a new session"));
        assert!(hint.contains("checkpoint unavailable"));
    }

    #[test]
    fn resume_hydration_failure_hint_does_not_echo_diagnostic_payload() {
        let hint = build_resume_hydration_failure_hint_for_error(
            "engine restore: sqlx error: table agent_events does not exist",
        );
        assert!(hint.contains("hydration was requested"));
        assert!(!hint.contains("sqlx"));
        assert!(!hint.contains("does not exist"));
    }

    #[test]
    fn merge_resume_hints_preserves_both_sources() {
        let merged = merge_resume_hints(Some("session".into()), Some("plan".into())).unwrap();
        assert_eq!(merged, "session\n\nplan");
        assert_eq!(
            merge_resume_hints(Some("same".into()), Some("same".into())).unwrap(),
            "same"
        );
    }
}
