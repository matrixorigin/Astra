//! Prompt-visible resume hydration hints.
//!
//! Session resume must not depend on optional tools such as memory,
//! introspect, or task lookup. This module builds a bounded, deterministic
//! hint from prompt-facing history so the next LLM call knows it is continuing
//! an existing session even when those tools are unavailable.

use serde_json::Value;

use astra_turn_types::{
    ObjectiveRelation, UserFeedback, UserTurnSemantics, UserTurnSemanticsError,
};

pub const SESSION_RESUME_PREFIX: &str = "[session-resume:v1]";

const MAX_RECENT_MESSAGES: usize = 8;
const MAX_SUMMARY_ITEMS: usize = 6;
const MAX_USER_INPUT_CHARS: usize = 260;
const MAX_ASSISTANT_CHARS: usize = 360;
const MAX_MESSAGE_CHARS: usize = 220;
const MAX_HINT_CHARS: usize = 2_400;

pub fn build_resume_hydration_hint_from_messages(
    messages: &[Value],
) -> Result<Option<String>, UserTurnSemanticsError> {
    let objective_context = objective_context_from_messages(messages)?;
    astra_core::history_work::record_serialized_value(
        astra_core::history_work::HistoryWorkSite::ResumeHintHistoryClone,
        messages,
    );
    let prompt_messages = crate::prompt_facing::sanitize_prompt_facing_messages_with_turn_semantics(
        messages.to_vec(),
    );
    build_resume_hydration_hint(&prompt_messages, Some(objective_context))
}

pub fn build_resume_hydration_hint_from_prompt_messages(
    messages: &[Value],
) -> Result<Option<String>, UserTurnSemanticsError> {
    build_resume_hydration_hint(messages, None)
}

fn build_resume_hydration_hint(
    messages: &[Value],
    objective_context: Option<Vec<ObjectiveContextItem>>,
) -> Result<Option<String>, UserTurnSemanticsError> {
    let entries = prompt_entries(messages)?;
    if entries.is_empty() {
        return Ok(None);
    }

    // Minimum viability: require at least one user message AND one assistant message.
    // A hint built from user-only or assistant-only history provides no meaningful
    // continuity context and risks the model hallucinating prior state.
    let has_user = entries.iter().any(|e| e.role == "user");
    let has_assistant = entries.iter().any(|e| e.role == "assistant");
    if !has_user || !has_assistant {
        return Ok(None);
    }

    let latest_user_input = latest_user_input(&entries);
    let last_assistant_state = latest_assistant(&entries);
    let objective_context =
        objective_context.unwrap_or_else(|| objective_context_from_entries(&entries));
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
    lines.push("Current objective context (typed turn semantics):".to_string());
    if objective_context.is_empty() {
        lines.push("- unavailable; use task and active-work evidence".to_string());
    } else {
        for item in objective_context {
            lines.push(format!(
                "- {}: {}",
                item.relation.objective_context_label(),
                truncate_chars(&item.text, MAX_USER_INPUT_CHARS)
            ));
        }
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

    Ok(Some(truncate_chars(&lines.join("\n"), MAX_HINT_CHARS)))
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
    semantics: Option<UserTurnSemantics>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectiveContextItem {
    pub relation: ObjectiveRelation,
    pub feedback: Option<UserFeedback>,
    pub text: String,
}

/// Project the current objective from producer-owned turn semantics. Messages
/// without current-schema metadata are deliberately ignored; this function is
/// not a natural-language classifier or a legacy migration layer.
pub fn objective_context_from_messages(
    messages: &[Value],
) -> Result<Vec<ObjectiveContextItem>, UserTurnSemanticsError> {
    Ok(objective_context_from_entries(&canonical_semantic_entries(
        messages,
    )?))
}

fn canonical_semantic_entries(
    messages: &[Value],
) -> Result<Vec<PromptEntry>, UserTurnSemanticsError> {
    let mut entries = Vec::new();
    for message in messages
        .iter()
        .filter(|message| !astra_turn_types::is_runtime_owned_message(message))
    {
        let Some(role) = message.get("role").and_then(Value::as_str) else {
            continue;
        };
        if !matches!(role, "user" | "assistant") {
            continue;
        }
        let Some(text) = crate::prompt_facing::extract_text_content(message) else {
            continue;
        };
        let text = normalize_ws(&text);
        if text.is_empty() {
            continue;
        }
        entries.push(PromptEntry {
            role: role.to_string(),
            text,
            semantics: astra_turn_types::user_turn_semantics(message)?,
        });
    }
    Ok(entries)
}

fn objective_context_from_entries(entries: &[PromptEntry]) -> Vec<ObjectiveContextItem> {
    const MAX_OBJECTIVE_ITEMS: usize = 4;

    let mut items = Vec::new();
    let mut has_prior_assistant = false;
    for entry in entries {
        if entry.role == "assistant" {
            has_prior_assistant = true;
            continue;
        }
        if entry.role != "user" {
            continue;
        }
        let Some(semantics) = entry.semantics else {
            continue;
        };

        let should_append = match semantics.objective_relation {
            ObjectiveRelation::Replace => {
                items.clear();
                true
            }
            ObjectiveRelation::Refine | ObjectiveRelation::Correct => true,
            ObjectiveRelation::Unknown => items.is_empty() && !has_prior_assistant,
            ObjectiveRelation::Acknowledge | ObjectiveRelation::Continue => false,
        };
        if should_append {
            items.push(ObjectiveContextItem {
                relation: semantics.objective_relation,
                feedback: semantics.feedback,
                text: entry.text.clone(),
            });
        }
    }

    while items.len() > MAX_OBJECTIVE_ITEMS {
        // Preserve the objective base at index 0 and evict the oldest
        // refinement/correction first.
        items.remove(1);
    }
    items
}

fn prompt_entries(messages: &[Value]) -> Result<Vec<PromptEntry>, UserTurnSemanticsError> {
    let mut entries = Vec::new();
    for message in messages {
        let Some(role) = message.get("role").and_then(Value::as_str) else {
            continue;
        };
        if !matches!(role, "user" | "assistant" | "system") {
            continue;
        }
        let Some(text) = crate::prompt_facing::extract_text_content(message) else {
            continue;
        };
        let text = normalize_ws(&text);
        if text.is_empty() {
            continue;
        }
        entries.push(PromptEntry {
            role: role.to_string(),
            text,
            semantics: astra_turn_types::user_turn_semantics(message)?,
        });
    }
    Ok(entries)
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
        let mut messages = vec![
            json!({"role": "user", "content": "3agents review current branch changes"}),
            json!({"role": "assistant", "content": "I will inspect diff, stat, and run parallel reviews."}),
            json!({"role": "assistant", "tool_calls": [{"id": "c1", "function": {"name": "git"}}]}),
            json!({"role": "tool", "tool_call_id": "c1", "content": "202 files changed"}),
            json!({"role": "user", "content": "继续"}),
            json!({"role": "assistant", "content": "Continuing the branch review from the diff."}),
        ];
        astra_turn_types::mark_user_turn_semantics(
            &mut messages[0],
            astra_turn_types::UserTurnSemantics::new(ObjectiveRelation::Replace, None),
        );
        astra_turn_types::mark_user_turn_semantics(
            &mut messages[4],
            astra_turn_types::UserTurnSemantics::new(ObjectiveRelation::Continue, None),
        );

        let hint = build_resume_hydration_hint_from_messages(&messages)
            .expect("valid semantics")
            .expect("hint");

        assert!(hint.starts_with(SESSION_RESUME_PREFIX));
        assert!(hint.contains("Treat this as the same session"));
        assert!(hint.contains("latest_user_input: 继续"));
        assert!(hint.contains("objective: 3agents review current branch changes"));
        assert!(!hint.contains("unchanged objective: 继续"));
        assert!(hint.contains("last_assistant_state: Continuing the branch review"));
        assert!(!hint.contains("[Runtime tool result]"));
        assert!(!hint.contains("202 files changed"));
    }

    #[test]
    fn typed_objective_survives_a_compaction_boundary_without_replaying_old_chat() {
        let mut messages = vec![
            json!({"role": "user", "content": "repair the session lifecycle"}),
            json!({"role": "assistant", "content": "I will inspect it."}),
            json!({"role": "system", "content": "arbitrary compaction boundary", "_compact_boundary": true}),
            json!({"role": "user", "content": "continue"}),
            json!({"role": "assistant", "content": "Continuing."}),
        ];
        astra_turn_types::mark_user_turn_semantics(
            &mut messages[0],
            astra_turn_types::UserTurnSemantics::new(ObjectiveRelation::Replace, None),
        );
        astra_turn_types::mark_user_turn_semantics(
            &mut messages[3],
            astra_turn_types::UserTurnSemantics::new(ObjectiveRelation::Continue, None),
        );

        let prompt_tail = crate::prompt_facing::sanitize_prompt_facing_messages_with_turn_semantics(
            messages.clone(),
        );
        assert!(
            prompt_tail
                .iter()
                .all(|message| message["content"] != "repair the session lifecycle"),
            "normal prompt projection must still honor the compaction boundary"
        );

        let hint = build_resume_hydration_hint_from_messages(&messages)
            .expect("valid semantics")
            .expect("resume hint");
        assert!(hint.contains("objective: repair the session lifecycle"));
        assert!(!hint.contains("unchanged objective: continue"));
    }

    #[test]
    fn objective_context_preserves_distinct_typed_refinements() {
        let mut messages = vec![
            json!({"role": "user", "content": "repair the session lifecycle"}),
            json!({"role": "assistant", "content": "I will inspect it."}),
            json!({"role": "user", "content": "use a single canonical registry"}),
            json!({"role": "assistant", "content": "Understood."}),
            json!({"role": "user", "content": "also verify the server-only path"}),
        ];
        for (index, relation) in [
            ObjectiveRelation::Replace,
            ObjectiveRelation::Refine,
            ObjectiveRelation::Refine,
        ]
        .into_iter()
        .enumerate()
        {
            let message_index = index * 2;
            astra_turn_types::mark_user_turn_semantics(
                &mut messages[message_index],
                astra_turn_types::UserTurnSemantics::new(relation, None),
            );
        }

        let context = objective_context_from_messages(&messages).expect("valid semantics");

        assert_eq!(
            context
                .iter()
                .map(|item| item.text.as_str())
                .collect::<Vec<_>>(),
            vec![
                "repair the session lifecycle",
                "use a single canonical registry",
                "also verify the server-only path",
            ]
        );
    }

    #[test]
    fn objective_context_does_not_classify_unmarked_text() {
        let messages = vec![
            json!({"role": "user", "content": "continue"}),
            json!({"role": "assistant", "content": "ok"}),
        ];

        assert!(
            objective_context_from_messages(&messages)
                .expect("unmarked messages are valid")
                .is_empty()
        );
    }

    #[test]
    fn resume_hydration_refuses_user_only_or_assistant_only_history() {
        let user_only = vec![json!({"role": "user", "content": "继续"})];
        let assistant_only = vec![json!({"role": "assistant", "content": "好的，继续进行"})];

        assert!(
            build_resume_hydration_hint_from_prompt_messages(&user_only)
                .expect("unmarked messages are valid")
                .is_none(),
            "user-only messages should not create resume hints"
        );
        assert!(
            build_resume_hydration_hint_from_prompt_messages(&assistant_only)
                .expect("unmarked messages are valid")
                .is_none(),
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
    fn corrupt_typed_semantics_degrades_explicitly_instead_of_becoming_absent() {
        let messages = vec![
            json!({
                "role": "user",
                "content": "repair lifecycle",
                (astra_turn_types::USER_TURN_SEMANTICS_FIELD): {
                    "schema_version": "invalid",
                    "objective_relation": "replace"
                }
            }),
            json!({"role": "assistant", "content": "working"}),
        ];

        assert!(matches!(
            build_resume_hydration_hint_from_messages(&messages),
            Err(UserTurnSemanticsError::Malformed(_))
        ));
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
