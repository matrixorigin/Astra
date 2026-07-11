use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeScaffoldingKind {
    SystemReminderWrapper,
    AttentionManifest,
    WorkingSetManifest,
    SessionAnchor,
    SessionResumeHydration,
    AlreadyFetchedInventory,
    CrossSessionProjectContext,
    PreviousRoundSummary,
    SequentialToolCallsWarning,
    VerificationRequired,
    ErrorBudgetDirective,
    GenericRuntimeScaffolding,
}

pub const SYSTEM_REMINDER_WRAPPER_PREFIX: &str = "<system-reminder>";
pub const ATTENTION_MANIFEST_PREFIX: &str = "[attention:v1]";
pub const WORKING_SET_MANIFEST_PREFIX: &str = "[working-set:v1]";
pub const SESSION_ANCHOR_PREFIX: &str = "[session-anchor]";
pub const SESSION_RESUME_PREFIX: &str = crate::resume_hydration::SESSION_RESUME_PREFIX;
pub const ALREADY_FETCHED_PREFIX: &str = "## Already Fetched";
pub const CROSS_SESSION_PROJECT_CONTEXT_PREFIX: &str = "## Cross-Session Project Context";
pub const PREVIOUS_ROUND_PREFIX: &str = "✓ Previous round:";
pub const SEQUENTIAL_TOOL_CALLS_PREFIX: &str = "## ⚠ Sequential Tool Calls Detected";
pub const VERIFICATION_REQUIRED_PREFIX: &str = "⚠️ VERIFICATION REQUIRED";
pub const ERROR_BUDGET_PREFIX: &str = "🔄 ERROR BUDGET";
pub const INTERNAL_SKILL_AUTO_ROUTE_TOOL_CALL_ID_PREFIX: &str = "skill-auto-route";

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PromptFacingRuntimeNormalization {
    pub messages: Vec<Value>,
    pub required_runtime_texts: Vec<String>,
}

pub fn detect_runtime_scaffolding(content: &str) -> Option<RuntimeScaffoldingKind> {
    let trimmed = content.trim_start();
    if trimmed.starts_with(SYSTEM_REMINDER_WRAPPER_PREFIX) {
        Some(RuntimeScaffoldingKind::SystemReminderWrapper)
    } else if trimmed.starts_with(ATTENTION_MANIFEST_PREFIX) {
        Some(RuntimeScaffoldingKind::AttentionManifest)
    } else if trimmed.starts_with(WORKING_SET_MANIFEST_PREFIX) {
        Some(RuntimeScaffoldingKind::WorkingSetManifest)
    } else if trimmed.starts_with(SESSION_ANCHOR_PREFIX) {
        Some(RuntimeScaffoldingKind::SessionAnchor)
    } else if trimmed.starts_with(SESSION_RESUME_PREFIX) {
        Some(RuntimeScaffoldingKind::SessionResumeHydration)
    } else if trimmed.starts_with(ALREADY_FETCHED_PREFIX) {
        Some(RuntimeScaffoldingKind::AlreadyFetchedInventory)
    } else if trimmed.starts_with(CROSS_SESSION_PROJECT_CONTEXT_PREFIX) {
        Some(RuntimeScaffoldingKind::CrossSessionProjectContext)
    } else if trimmed.starts_with(PREVIOUS_ROUND_PREFIX) {
        Some(RuntimeScaffoldingKind::PreviousRoundSummary)
    } else if trimmed.starts_with(SEQUENTIAL_TOOL_CALLS_PREFIX) {
        Some(RuntimeScaffoldingKind::SequentialToolCallsWarning)
    } else if trimmed.starts_with(VERIFICATION_REQUIRED_PREFIX) {
        Some(RuntimeScaffoldingKind::VerificationRequired)
    } else if trimmed.starts_with(ERROR_BUDGET_PREFIX) {
        Some(RuntimeScaffoldingKind::ErrorBudgetDirective)
    } else if astra_turn_types::scaffolding_body_prefixes_for_filtering()
        .any(|prefix| trimmed.starts_with(prefix))
    {
        Some(RuntimeScaffoldingKind::GenericRuntimeScaffolding)
    } else {
        None
    }
}

pub fn is_continuation_scaffolding_for_role(role: &str, content: &str) -> bool {
    matches!(role, "user" | "assistant" | "system") && detect_runtime_scaffolding(content).is_some()
}

/// Return the prompt-facing user text after removing runtime-owned wrappers.
///
/// This is the boundary between user intent and runtime telemetry. It must be
/// used before intent classification, task-profile inference, answer-relevance
/// checks, or any other decision that should be driven only by the latest real
/// user request. The markers here are protocol-owned runtime prefixes, not
/// natural-language intent heuristics.
pub fn strip_user_runtime_scaffolding_affixes(content: &str) -> String {
    split_user_runtime_scaffolding_affixes(content).prompt_facing_text
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UserRuntimeScaffoldingSplit {
    prompt_facing_text: String,
    required_runtime_texts: Vec<String>,
}

fn split_user_runtime_scaffolding_affixes(content: &str) -> UserRuntimeScaffoldingSplit {
    let mut required_runtime_texts = Vec::new();
    let mut text = content.trim().to_string();
    loop {
        let trimmed = text.trim_start();
        if !trimmed.starts_with(SYSTEM_REMINDER_WRAPPER_PREFIX) {
            break;
        }
        let Some(end) = trimmed.find("</system-reminder>") else {
            break;
        };
        push_required_runtime_text(
            &mut required_runtime_texts,
            &trimmed[..end + "</system-reminder>".len()],
        );
        text = trimmed[end + "</system-reminder>".len()..]
            .trim_start_matches(|ch: char| ch.is_whitespace())
            .to_string();
    }

    loop {
        let trimmed = text.trim_end();
        let Some(start) = trimmed.rfind(SYSTEM_REMINDER_WRAPPER_PREFIX) else {
            break;
        };
        let suffix = &trimmed[start..];
        if !suffix
            .trim_start()
            .starts_with(SYSTEM_REMINDER_WRAPPER_PREFIX)
            || !suffix.trim_end().ends_with("</system-reminder>")
        {
            break;
        }
        push_required_runtime_text(&mut required_runtime_texts, suffix);
        text = trimmed[..start]
            .trim_end_matches(|ch: char| ch.is_whitespace())
            .to_string();
    }

    let trimmed = text.trim_start();
    if trimmed.starts_with(SESSION_RESUME_PREFIX) {
        if let Some((head, suffix)) = trimmed.rsplit_once("\n\n") {
            let suffix = suffix.trim();
            if !suffix.is_empty()
                && !suffix.starts_with('-')
                && detect_runtime_scaffolding(suffix).is_none()
            {
                push_required_runtime_text(&mut required_runtime_texts, head);
                return UserRuntimeScaffoldingSplit {
                    prompt_facing_text: suffix.to_string(),
                    required_runtime_texts,
                };
            }
        }
    }

    UserRuntimeScaffoldingSplit {
        prompt_facing_text: text.trim().to_string(),
        required_runtime_texts,
    }
}

fn push_required_runtime_text(required_runtime_texts: &mut Vec<String>, text: &str) {
    let normalized = unwrap_system_reminder_text(text).trim().to_string();
    if !normalized.is_empty() {
        required_runtime_texts.push(normalized);
    }
}

fn unwrap_system_reminder_text(text: &str) -> &str {
    let trimmed = text.trim();
    let Some(inner) = trimmed.strip_prefix(SYSTEM_REMINDER_WRAPPER_PREFIX) else {
        return trimmed;
    };
    inner
        .trim()
        .strip_suffix("</system-reminder>")
        .unwrap_or(inner)
        .trim()
}

pub fn is_trailing_user_runtime_scaffolding(content: &str) -> bool {
    matches!(
        detect_runtime_scaffolding(content),
        Some(
            RuntimeScaffoldingKind::SystemReminderWrapper
                | RuntimeScaffoldingKind::AttentionManifest
        )
    )
}

/// Normalize prompt-facing history at ingress.
///
/// This is intentionally structural: it recognizes runtime-owned protocol
/// wrappers and internal auto-route tool IDs, not free-form natural language.
/// Returned `required_runtime_texts` must be routed through a control/system
/// lane by the caller instead of being written back into user content.
pub fn normalize_prompt_facing_runtime_messages(
    messages: Vec<Value>,
) -> PromptFacingRuntimeNormalization {
    let mut normalized = PromptFacingRuntimeNormalization::default();
    for mut message in messages {
        if is_internal_skill_auto_route_message(&message) {
            continue;
        }

        if message.get("role").and_then(|role| role.as_str()) == Some("user")
            && let Some(content) = message.get("content").and_then(|content| content.as_str())
        {
            let split = split_user_runtime_scaffolding_affixes(content);
            normalized
                .required_runtime_texts
                .extend(split.required_runtime_texts);
            if split.prompt_facing_text.trim().is_empty() {
                continue;
            }
            if split.prompt_facing_text != content {
                message["content"] = Value::String(split.prompt_facing_text);
            }
        }

        normalized.messages.push(message);
    }
    normalized
}

/// Return runtime-state messages that are safe to persist for recovery.
///
/// Unlike prompt-facing sanitization, this preserves ordinary provider tool
/// frames so crash recovery can replay in-flight work. It only removes
/// runtime-owned scaffolding that should never become resumable user intent:
/// user-message affixes and internally injected skill auto-route roundtrips.
pub fn sanitize_recoverable_runtime_messages(messages: Vec<Value>) -> Vec<Value> {
    messages
        .into_iter()
        .filter_map(sanitize_recoverable_runtime_message)
        .collect()
}

fn sanitize_recoverable_runtime_message(mut message: Value) -> Option<Value> {
    if is_internal_skill_auto_route_message(&message) {
        return None;
    }

    if message.get("role").and_then(|role| role.as_str()) == Some("user")
        && let Some(content) = message.get("content").and_then(|content| content.as_str())
    {
        let stripped = split_user_runtime_scaffolding_affixes(content).prompt_facing_text;
        if stripped.trim().is_empty() {
            return None;
        }
        if stripped != content {
            message["content"] = Value::String(stripped);
        }
    }

    Some(message)
}

fn is_internal_skill_auto_route_message(message: &Value) -> bool {
    if message.get("role").and_then(|role| role.as_str()) == Some("tool") {
        return message
            .get("tool_call_id")
            .and_then(|id| id.as_str())
            .is_some_and(is_internal_skill_auto_route_tool_call_id);
    }

    if message.get("role").and_then(|role| role.as_str()) != Some("assistant") {
        return false;
    }
    let Some(tool_calls) = message.get("tool_calls").and_then(|calls| calls.as_array()) else {
        return false;
    };
    !tool_calls.is_empty()
        && tool_calls.iter().all(|tool_call| {
            tool_call
                .get("id")
                .and_then(|id| id.as_str())
                .is_some_and(is_internal_skill_auto_route_tool_call_id)
        })
}

fn is_internal_skill_auto_route_tool_call_id(id: &str) -> bool {
    id == INTERNAL_SKILL_AUTO_ROUTE_TOOL_CALL_ID_PREFIX
        || id
            .strip_prefix(INTERNAL_SKILL_AUTO_ROUTE_TOOL_CALL_ID_PREFIX)
            .is_some_and(|suffix| suffix.starts_with('-'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn detects_known_runtime_scaffolding_markers() {
        assert_eq!(
            detect_runtime_scaffolding("<system-reminder>\nBackground task updates"),
            Some(RuntimeScaffoldingKind::SystemReminderWrapper)
        );
        assert_eq!(
            detect_runtime_scaffolding("[attention:v1]\ngoal: ship auth"),
            Some(RuntimeScaffoldingKind::AttentionManifest)
        );
        assert_eq!(
            detect_runtime_scaffolding("[working-set:v1]\ngoal: ship auth"),
            Some(RuntimeScaffoldingKind::WorkingSetManifest)
        );
        assert_eq!(
            detect_runtime_scaffolding("[session-resume:v1]\nHydrated previous session"),
            Some(RuntimeScaffoldingKind::SessionResumeHydration)
        );
        assert_eq!(
            detect_runtime_scaffolding("## Already Fetched (do NOT re-read)\nfoo.rs"),
            Some(RuntimeScaffoldingKind::AlreadyFetchedInventory)
        );
        assert_eq!(detect_runtime_scaffolding("plain user message"), None);
    }

    #[test]
    fn detects_stop_hook_and_error_budget_directives() {
        assert_eq!(
            detect_runtime_scaffolding(
                "⚠️ VERIFICATION REQUIRED: Before you finish, run missing checks"
            ),
            Some(RuntimeScaffoldingKind::VerificationRequired)
        );
        assert_eq!(
            detect_runtime_scaffolding("🔄 ERROR BUDGET EXHAUSTED: You've hit Unknown errors"),
            Some(RuntimeScaffoldingKind::ErrorBudgetDirective)
        );
    }

    #[test]
    fn falls_back_to_turn_types_scaffolding_prefixes() {
        assert_eq!(
            detect_runtime_scaffolding("Tools used: bash, grep, read_file"),
            Some(RuntimeScaffoldingKind::GenericRuntimeScaffolding)
        );
    }

    #[test]
    fn continuation_scaffolding_is_filtered_for_prompt_facing_roles() {
        for role in ["user", "assistant", "system"] {
            assert!(is_continuation_scaffolding_for_role(
                role,
                "⚠️ VERIFICATION REQUIRED: Before you finish"
            ));
            assert!(is_continuation_scaffolding_for_role(
                role,
                "Tools used: bash"
            ));
        }
        assert!(!is_continuation_scaffolding_for_role(
            "tool",
            "Tools used: bash"
        ));
    }

    #[test]
    fn plain_user_message_is_not_scaffolding() {
        assert_eq!(detect_runtime_scaffolding("plain user message"), None);
    }

    #[test]
    fn non_versioned_attention_prefix_is_not_scaffolding() {
        // "[attention:" alone without "v1]" could appear in user content
        // about attention mechanisms — require the version marker.
        assert_eq!(
            detect_runtime_scaffolding("[attention:span] user query about ML"),
            None
        );
        assert_eq!(detect_runtime_scaffolding("[working-set:foo]"), None);
    }

    #[test]
    fn trailing_user_scaffolding_only_matches_runtime_user_wrappers() {
        assert!(is_trailing_user_runtime_scaffolding(
            "<system-reminder>\nBackground task updates"
        ));
        assert!(is_trailing_user_runtime_scaffolding(
            "[attention:v1]\ngoal: ship auth"
        ));
        assert!(!is_trailing_user_runtime_scaffolding(
            "## ⚠ Sequential Tool Calls Detected"
        ));
    }

    #[test]
    fn strips_trailing_system_reminder_from_user_intent() {
        let input = "review uncommitted changes\n\n<system-reminder>\nlast assistant: fix/apply/edit\n</system-reminder>";
        assert_eq!(
            strip_user_runtime_scaffolding_affixes(input),
            "review uncommitted changes"
        );
    }

    #[test]
    fn strips_leading_system_reminder_from_user_intent() {
        let input = "<system-reminder>\nbackground task update\n</system-reminder>\n\n修复这个问题";
        assert_eq!(
            strip_user_runtime_scaffolding_affixes(input),
            "修复这个问题"
        );
    }

    #[test]
    fn extracts_real_suffix_from_session_resume_block() {
        let input = "[session-resume:v1]\nHydrated previous session.\n\n我是让你review啊！";
        assert_eq!(
            strip_user_runtime_scaffolding_affixes(input),
            "我是让你review啊！"
        );
    }

    #[test]
    fn recoverable_runtime_messages_strip_user_affixes_and_internal_auto_route_roundtrip() {
        let messages = vec![
            json!({"role": "user", "content": "我说过的所有话\n\n<system-reminder>\n[session-resume:v1]\nHydrated previous session context\n</system-reminder>"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "skill-auto-route-analyze-session",
                    "type": "function",
                    "function": {"name": "skill", "arguments": "{}"},
                }],
            }),
            json!({"role": "tool", "tool_call_id": "skill-auto-route-analyze-session", "content": "<skill-loaded name=\"analyze-session\"/>"}),
            json!({"role": "assistant", "content": "你问过我所有话。"}),
        ];

        let got = sanitize_recoverable_runtime_messages(messages);

        assert_eq!(
            got,
            vec![
                json!({"role": "user", "content": "我说过的所有话"}),
                json!({"role": "assistant", "content": "你问过我所有话。"}),
            ]
        );
    }

    #[test]
    fn prompt_facing_normalization_extracts_runtime_affixes_to_required_lane() {
        let messages = vec![
            json!({"role": "user", "content": "我说过的所有话\n\n<system-reminder>\n[session-resume:v1]\nHydrated previous session context\n</system-reminder>"}),
            json!({"role": "assistant", "content": "ok"}),
        ];

        let got = normalize_prompt_facing_runtime_messages(messages);

        assert_eq!(
            got.messages,
            vec![
                json!({"role": "user", "content": "我说过的所有话"}),
                json!({"role": "assistant", "content": "ok"}),
            ]
        );
        assert_eq!(
            got.required_runtime_texts,
            vec!["[session-resume:v1]\nHydrated previous session context"]
        );
    }

    #[test]
    fn prompt_facing_normalization_drops_internal_auto_route_roundtrip() {
        let messages = vec![
            json!({"role": "user", "content": "review changes"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "skill-auto-route-code-review",
                    "type": "function",
                    "function": {"name": "skill", "arguments": "{}"},
                }],
            }),
            json!({"role": "tool", "tool_call_id": "skill-auto-route-code-review", "content": "<skill-loaded name=\"code-review\"/>"}),
            json!({"role": "assistant", "content": "done"}),
        ];

        let got = normalize_prompt_facing_runtime_messages(messages);

        assert_eq!(
            got.messages,
            vec![
                json!({"role": "user", "content": "review changes"}),
                json!({"role": "assistant", "content": "done"}),
            ]
        );
        assert!(got.required_runtime_texts.is_empty());
    }

    #[test]
    fn recoverable_runtime_messages_preserve_ordinary_tool_roundtrip() {
        let messages = vec![
            json!({"role": "user", "content": "run tests"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "bash", "arguments": "{\"cmd\":\"cargo test\"}"},
                }],
            }),
            json!({"role": "tool", "tool_call_id": "call_1", "content": "ok"}),
        ];

        let got = sanitize_recoverable_runtime_messages(messages.clone());

        assert_eq!(got, messages);
    }
}
