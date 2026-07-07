//! Prompt-facing conversation message normalization.
//!
//! Runtime state may contain provider tool-call frames, tool outputs, cache
//! markers, reasoning-only assistant frames, and compaction boundaries. Those
//! are execution trace, not stable cross-turn chat history. Use this module at
//! session restore and CSL prompt-materialization boundaries.

use crate::conversation_log::SessionStateCompact;
use serde_json::{Value, json};

const MAX_PROMPT_FACING_MESSAGES: usize = 40;
const RUNTIME_RECAP_PREFIX: &str = "[Session runtime recap]";

pub fn extract_text_content(msg: &Value) -> Option<String> {
    if let Some(s) = msg.get("content").and_then(|c| c.as_str()) {
        return Some(s.to_string());
    }
    if let Some(arr) = msg.get("content").and_then(|c| c.as_array()) {
        let texts: Vec<&str> = arr
            .iter()
            .filter_map(|block| {
                let kind = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match kind {
                    "text" | "output_text" => block
                        .get("text")
                        .or_else(|| block.get("content"))
                        .and_then(|t| t.as_str()),
                    _ => None,
                }
            })
            .collect();
        if !texts.is_empty() {
            return Some(texts.join("\n"));
        }
    }
    None
}

pub fn sanitize_prompt_facing_messages(messages: Vec<Value>) -> Vec<Value> {
    let mut out = Vec::new();
    let start = latest_compaction_boundary_start(&messages).unwrap_or(0);
    let mut has_user_context = false;

    for msg in messages.into_iter().skip(start) {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
        if role == "tool" {
            continue;
        }
        if !matches!(role, "user" | "assistant" | "system") {
            continue;
        }

        if role == "assistant" {
            if contains_tool_call_frame(&msg) {
                continue;
            }
        }

        let Some(raw_content) = extract_text_content(&msg) else {
            continue;
        };
        let Some(content) = prompt_facing_content_for_role(role, &raw_content) else {
            continue;
        };
        if content.trim().is_empty() {
            continue;
        }
        if role == "system" && content.trim_start().starts_with(RUNTIME_RECAP_PREFIX) {
            continue;
        }

        if role == "assistant" && !has_user_context {
            continue;
        }
        out.push(json!({
            "role": role,
            "content": content,
        }));
        if role == "user" {
            has_user_context = true;
        }
    }

    trim_to_recent_messages(out)
}

pub fn sanitize_prompt_facing_messages_with_state(
    messages: Vec<Value>,
    state: &SessionStateCompact,
) -> Vec<Value> {
    let mut out = sanitize_prompt_facing_messages(messages);
    if let Some(recap) = runtime_recap_message(state) {
        out.push(recap);
    }
    trim_to_recent_messages(out)
}

pub fn sanitize_user_visible_messages(messages: Vec<Value>) -> Vec<Value> {
    messages
        .into_iter()
        .filter_map(|msg| user_visible_message(&msg))
        .collect()
}

pub fn user_visible_message(msg: &Value) -> Option<Value> {
    let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
    if !matches!(role, "user" | "assistant" | "system") {
        return None;
    }
    let raw_content = extract_text_content(msg)?;
    let content = prompt_facing_content_for_role(role, &raw_content)?;
    if role == "system" && is_prompt_internal_system_text(&content) {
        return None;
    }
    let content = sanitize_user_visible_text(&content);
    if content.trim().is_empty() {
        return None;
    }
    Some(json!({
        "role": role,
        "content": content,
    }))
}

pub fn runtime_recap_message(state: &SessionStateCompact) -> Option<Value> {
    let mut lines = Vec::new();
    if !state.recent_tools.is_empty() {
        lines.push(format!("Recent tools: {}", state.recent_tools.join(", ")));
    }
    if !state.activated_deferred_tool_names.is_empty() {
        lines.push(format!(
            "Activated deferred tools awaiting schema injection: {}",
            state.activated_deferred_tool_names.join(", ")
        ));
    }
    if state.budget_remaining_tokens > 0 || state.budget_remaining_rounds > 0 {
        lines.push(format!(
            "Last checkpoint budget: tokens={}, rounds={}",
            state.budget_remaining_tokens, state.budget_remaining_rounds
        ));
    }
    if state.consecutive_ctx_errors > 0 {
        lines.push(format!(
            "Context-window recovery attempts: {}",
            state.consecutive_ctx_errors
        ));
    }
    if let Some(delegation) = &state.delegation {
        lines.push(format!(
            "Delegation: id={}, pattern={}, completed_sub_runs={}",
            delegation.id,
            delegation.pattern,
            delegation.completed_sub_runs.len()
        ));
    }
    if lines.is_empty() {
        return None;
    }
    Some(json!({
        "role": "system",
        "content": format!("{RUNTIME_RECAP_PREFIX}\n{}", lines.join("\n")),
    }))
}

fn latest_compaction_boundary_start(messages: &[Value]) -> Option<usize> {
    messages.iter().rposition(|message| {
        extract_text_content(message)
            .map(|content| is_compaction_boundary_text(&content))
            .unwrap_or(false)
    })
}

fn is_compaction_boundary_text(content: &str) -> bool {
    let trimmed = content.trim_start();
    trimmed.starts_with("[Context compacted:")
        || trimmed.starts_with("[Conversation summary")
        || trimmed.starts_with("Context was compacted before this point.")
        || trimmed.starts_with("前文上下文已压缩")
}

fn is_prompt_internal_system_text(content: &str) -> bool {
    let trimmed = content.trim_start();
    trimmed.starts_with("[Runtime tool result]") || trimmed.starts_with(RUNTIME_RECAP_PREFIX)
}

fn prompt_facing_content_for_role(role: &str, content: &str) -> Option<String> {
    let content = if role == "user" {
        crate::runtime_scaffolding::strip_user_runtime_scaffolding_affixes(content)
    } else {
        content.trim().to_string()
    };
    if content.trim().is_empty() {
        return None;
    }
    if crate::runtime_scaffolding::is_continuation_scaffolding_for_role(role, &content) {
        return None;
    }
    Some(content)
}

fn contains_tool_call_frame(msg: &Value) -> bool {
    if let Some(tool_calls) = msg.get("tool_calls").and_then(|v| v.as_array()) {
        return !tool_calls.is_empty();
    }
    if let Some(content) = msg.get("content").and_then(|c| c.as_array()) {
        return content
            .iter()
            .any(|block| block.get("type").and_then(|t| t.as_str()) == Some("tool_use"));
    }
    false
}

fn sanitize_user_visible_text(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut chars = content.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            strip_escape_sequence(&mut chars);
            continue;
        }
        if ch.is_control() && !matches!(ch, '\n' | '\r' | '\t') {
            continue;
        }
        out.push(ch);
    }
    out.trim().to_string()
}

fn strip_escape_sequence<I>(chars: &mut std::iter::Peekable<I>)
where
    I: Iterator<Item = char>,
{
    match chars.peek().copied() {
        Some('[') => {
            chars.next();
            for ch in chars.by_ref() {
                if ('@'..='~').contains(&ch) {
                    break;
                }
            }
        }
        Some(']') => {
            chars.next();
            while let Some(ch) = chars.next() {
                if ch == '\u{7}' {
                    break;
                }
                if ch == '\u{1b}' && chars.peek().copied() == Some('\\') {
                    chars.next();
                    break;
                }
            }
        }
        Some(_) => {
            chars.next();
        }
        None => {}
    }
}

fn trim_to_recent_messages(mut messages: Vec<Value>) -> Vec<Value> {
    if messages.len() <= MAX_PROMPT_FACING_MESSAGES {
        return messages;
    }
    messages.drain(0..messages.len() - MAX_PROMPT_FACING_MESSAGES);
    messages
}

#[cfg(test)]
mod tests {
    use super::{
        runtime_recap_message, sanitize_prompt_facing_messages,
        sanitize_prompt_facing_messages_with_state, sanitize_user_visible_messages,
    };
    use crate::conversation_log::{DelegationCompact, SessionStateCompact};
    use serde_json::json;

    #[test]
    fn compresses_completed_tool_pair_and_drops_reasoning_only_messages() {
        let messages = vec![
            json!({"role": "user", "content": "fix it"}),
            json!({"role": "assistant", "reasoning_content": "I should inspect files"}),
            json!({"role": "assistant", "tool_calls": [{"id": "c1", "function": {"name": "read_file"}}]}),
            json!({"role": "tool", "tool_call_id": "c1", "content": "file"}),
            json!({"role": "assistant", "content": "done"}),
        ];

        let got = sanitize_prompt_facing_messages(messages);

        assert_eq!(
            got,
            vec![
                json!({"role": "user", "content": "fix it"}),
                json!({"role": "assistant", "content": "done"}),
            ]
        );
    }

    #[test]
    fn tool_results_do_not_become_prompt_facing_system_messages() {
        let messages = vec![
            json!({"role": "user", "content": "inspect"}),
            json!({"role": "assistant", "tool_calls": [
                {"id": "c1", "function": {"name": " read_file "}},
                {"id": "c2", "function": {"name": "  "}}
            ]}),
            json!({"role": "tool", "tool_call_id": "c1", "content": "file"}),
            json!({"role": "tool", "tool_call_id": "c2", "content": "blank-name result"}),
        ];

        let got = sanitize_prompt_facing_messages(messages);

        assert_eq!(got, vec![json!({"role": "user", "content": "inspect"})]);
    }

    #[test]
    fn drops_assistant_only_visible_text_without_user_context() {
        let messages = vec![
            json!({"role": "assistant", "content": "Earlier context compacted."}),
            json!({"role": "system", "content": "status note"}),
            json!({"role": "assistant", "content": "orphan answer"}),
            json!({"role": "user", "content": "continue"}),
            json!({"role": "assistant", "content": "ok"}),
        ];

        let got = sanitize_prompt_facing_messages(messages);

        assert_eq!(
            got,
            vec![
                json!({"role": "system", "content": "status note"}),
                json!({"role": "user", "content": "continue"}),
                json!({"role": "assistant", "content": "ok"}),
            ]
        );
    }

    #[test]
    fn compaction_boundary_replaces_older_goal_stack() {
        let messages = vec![
            json!({"role": "user", "content": "3 agents review everything"}),
            json!({"role": "assistant", "content": "review summary"}),
            json!({"role": "system", "content": "[Context compacted: older messages were removed to reduce token pressure. The conversation continues below.]"}),
            json!({"role": "user", "content": "不要review啊！"}),
            json!({"role": "assistant", "reasoning_content": "Maybe review anyway"}),
            json!({"role": "assistant", "content": "明白，不做 review。"}),
        ];

        let got = sanitize_prompt_facing_messages(messages);

        assert_eq!(got.len(), 3);
        assert_eq!(got[0]["role"], "system");
        assert!(
            got[0]["content"]
                .as_str()
                .unwrap()
                .contains("Context compacted")
        );
        assert_eq!(got[1]["content"], "不要review啊！");
        assert_eq!(got[2]["content"], "明白，不做 review。");
        assert!(
            got.iter()
                .all(|msg| !msg["content"].as_str().unwrap_or("").contains("3 agents"))
        );
    }

    #[test]
    fn orphan_tool_result_and_unresolved_tool_call_do_not_reach_prompt() {
        let messages = vec![
            json!({"role": "user", "content": "continue"}),
            json!({"role": "tool", "tool_call_id": "missing", "content": "stale"}),
            json!({"role": "assistant", "content": "I will run bash.", "tool_calls": [{"id": "dangling", "function": {"name": "bash"}}]}),
            json!({"role": "assistant", "content": "visible"}),
        ];

        let got = sanitize_prompt_facing_messages(messages);

        assert_eq!(
            got,
            vec![
                json!({"role": "user", "content": "continue"}),
                json!({"role": "assistant", "content": "visible"}),
            ]
        );
    }

    #[test]
    fn drops_persisted_runtime_scaffolding_directives() {
        let messages = vec![
            json!({"role": "user", "content": "continue"}),
            json!({"role": "assistant", "content": "working"}),
            json!({"role": "user", "content": "⚠️ VERIFICATION REQUIRED: Before you finish, run any missing checks"}),
            json!({"role": "user", "content": "🔄 ERROR BUDGET EXHAUSTED: You've hit Unknown errors 3 turns in a row"}),
            json!({"role": "user", "content": "## ⚡ Self-Status\nTurn 9/299 | Token pressure: 5% | Cache: 86%"}),
            json!({"role": "assistant", "content": "Tools used: bash, grep, read_file"}),
            json!({"role": "assistant", "content": "done"}),
        ];

        let got = sanitize_prompt_facing_messages(messages);
        let joined = got
            .iter()
            .filter_map(|msg| msg["content"].as_str())
            .collect::<Vec<_>>()
            .join("\n");

        assert_eq!(
            got,
            vec![
                json!({"role": "user", "content": "continue"}),
                json!({"role": "assistant", "content": "working"}),
                json!({"role": "assistant", "content": "done"}),
            ]
        );
        assert!(!joined.contains("VERIFICATION REQUIRED"));
        assert!(!joined.contains("ERROR BUDGET"));
        assert!(!joined.contains("Self-Status"));
        assert!(!joined.contains("Tools used:"));
    }

    #[test]
    fn preserves_real_user_text_after_leading_system_reminder() {
        let messages = vec![
            json!({"role": "user", "content": "<system-reminder>\n## Session Memory Advisory\nstale memory\n</system-reminder>\n\n你知道我们之前做什么？"}),
            json!({"role": "assistant", "content": "We reviewed the branch."}),
        ];

        let got = sanitize_prompt_facing_messages(messages);

        assert_eq!(
            got,
            vec![
                json!({"role": "user", "content": "你知道我们之前做什么？"}),
                json!({"role": "assistant", "content": "We reviewed the branch."}),
            ]
        );
    }

    #[test]
    fn strips_trailing_system_reminder_without_mutating_user_text() {
        let messages = vec![
            json!({"role": "user", "content": "继续修复\n\n<system-reminder>\n[session-resume:v1]\nHydrated previous session context\n</system-reminder>"}),
            json!({"role": "assistant", "content": "Continuing."}),
        ];

        let got = sanitize_prompt_facing_messages(messages);

        assert_eq!(
            got,
            vec![
                json!({"role": "user", "content": "继续修复"}),
                json!({"role": "assistant", "content": "Continuing."}),
            ]
        );
    }

    #[test]
    fn preserves_user_suffix_after_legacy_leading_resume_hint() {
        let messages = vec![
            json!({"role": "user", "content": "[session-resume:v1]\nResume context hydration was requested but no prior prompt-facing history could be restored.\nReason: degraded\nTreat this as a degraded resume.\n\n之前我说的话？"}),
            json!({"role": "assistant", "content": "I should recover the journal."}),
        ];

        let got = sanitize_prompt_facing_messages(messages);

        assert_eq!(
            got,
            vec![
                json!({"role": "user", "content": "之前我说的话？"}),
                json!({"role": "assistant", "content": "I should recover the journal."}),
            ]
        );
    }

    #[test]
    fn anthropic_style_tool_blocks_are_compacted_without_provider_frames() {
        let messages = vec![
            json!({"role": "user", "content": "inspect"}),
            json!({
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "I will inspect."},
                    {"type": "tool_use", "id": "toolu_1", "name": "read_file", "input": {"path": "a.rs"}},
                ],
            }),
            json!({
                "role": "user",
                "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": [{"type": "text", "text": "line 1"}]},
                ],
            }),
            json!({"role": "assistant", "content": "done"}),
        ];

        let got = sanitize_prompt_facing_messages(messages);

        assert_eq!(
            got,
            vec![
                json!({"role": "user", "content": "inspect"}),
                json!({"role": "assistant", "content": "done"}),
            ]
        );
    }

    #[test]
    fn tool_result_rounds_are_dropped_instead_of_recap_injected() {
        let mut messages = vec![json!({"role": "user", "content": "inspect"})];
        for i in 0..12 {
            messages.push(json!({
                "role": "assistant",
                "tool_calls": [{"id": format!("c{i}"), "function": {"name": "grep"}}],
            }));
            messages.push(json!({
                "role": "tool",
                "tool_call_id": format!("c{i}"),
                "content": format!("match {i} {}", "x".repeat(900)),
            }));
        }

        let got = sanitize_prompt_facing_messages(messages);

        assert_eq!(got, vec![json!({"role": "user", "content": "inspect"})]);
    }

    #[test]
    fn runtime_recap_surfaces_structured_state_without_legacy_controls() {
        let state = SessionStateCompact {
            blocked_tools: vec!["stale_block".into()],
            recent_tools: vec!["read_file".into(), "grep".into()],
            activated_deferred_tool_names: vec!["write_file".into()],
            budget_remaining_tokens: 1234,
            budget_remaining_rounds: 7,
            consecutive_ctx_errors: 2,
            delegation: Some(DelegationCompact {
                id: "del-1".into(),
                pattern: "fanout".into(),
                completed_sub_runs: vec![],
            }),
            ..Default::default()
        };

        let recap = runtime_recap_message(&state).expect("runtime recap");
        let content = recap["content"].as_str().unwrap();

        assert!(content.starts_with("[Session runtime recap]"));
        assert!(content.contains("Recent tools: read_file, grep"));
        assert!(content.contains("Activated deferred tools awaiting schema injection: write_file"));
        assert!(content.contains("Last checkpoint budget: tokens=1234, rounds=7"));
        assert!(content.contains("Context-window recovery attempts: 2"));
        assert!(content.contains("Delegation: id=del-1, pattern=fanout, completed_sub_runs=0"));
        assert!(!content.contains("stale_block"));
    }

    #[test]
    fn sanitize_with_state_replaces_stale_runtime_recap_for_resume_prompt() {
        let messages = vec![
            json!({"role": "user", "content": "continue"}),
            json!({"role": "system", "content": "[Session runtime recap]\nRecent tools: stale"}),
        ];
        let state = SessionStateCompact {
            recent_tools: vec!["bash".into()],
            ..Default::default()
        };

        let got = sanitize_prompt_facing_messages_with_state(messages, &state);

        assert_eq!(got.len(), 2);
        assert_eq!(got[0]["content"], "continue");
        let recap = got[1]["content"].as_str().unwrap();
        assert!(recap.contains("Recent tools: bash"));
        assert!(!recap.contains("stale"));
    }

    #[test]
    fn user_visible_messages_drop_prompt_internal_recaps_and_control_bytes() {
        let messages = vec![
            json!({"role": "user", "content": "hello\u{0}"}),
            json!({"role": "system", "content": "[Runtime tool result]\nread_file: secret trace"}),
            json!({"role": "system", "content": "[Session runtime recap]\nRecent tools: bash"}),
            json!({"role": "tool", "content": "raw tool output"}),
            json!({"role": "assistant", "content": ""}),
            json!({"role": "assistant", "content": "\u{1b}[31mdone\u{1b}[0m"}),
            json!({"role": "system", "content": "visible status"}),
        ];

        let got = sanitize_user_visible_messages(messages);

        assert_eq!(
            got,
            vec![
                json!({"role": "user", "content": "hello"}),
                json!({"role": "assistant", "content": "done"}),
                json!({"role": "system", "content": "visible status"}),
            ]
        );
    }
}
