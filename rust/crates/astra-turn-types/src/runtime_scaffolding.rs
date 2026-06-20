//! Detect runtime-injected scaffolding messages.
//!
//! The astra runtime synthesizes several kinds of *scaffolding* messages into
//! the LLM message stream: parallel-batching nudges, execution-escalation
//! corrections, verification directives, error-budget notices, self-check
//! injections, tool-call rollups. These are **ephemeral** — they steer a
//! single turn's behavior — and should never be treated as conversational
//! content.
//!
//! When scaffolding leaks into persistent stores (Memoria working-memory,
//! session journals, conversation compaction), it creates feedback loops:
//! the runtime injects text, the store indexes it, a later retrieval pulls
//! it back in, the runtime re-reads its own output. Observed in session
//! `6676c7b5` where one turn saw 78 such scaffolding echoes inflate the
//! volatile-lane `## User Memories` block to 6.4 KB.
//!
//! This module is the **single source of truth** for "is this message
//! scaffolding?" — callers in compaction, memory-writing, and prompt
//! assembly should route through [`is_runtime_scaffolding_message`] rather
//! than duplicating detection logic.

use serde_json::Value;

/// Prefixes that mark a message body as runtime-injected scaffolding.
///
/// Any message whose trimmed content starts with one of these strings is
/// scaffolding — no exceptions. Keep this list sorted by how frequently the
/// prefix appears so hot-path iteration exits early.
///
/// When adding a new runtime injection, add its prefix here so Memoria and
/// compaction automatically skip it without per-site plumbing.
pub const SCAFFOLDING_BODY_PREFIXES: &[&str] = &[
    // Rollups
    "Tools used:",
    "[Self-check",
    "[attention:v1]",
    "[session-anchor]",
    "[working-set:v1]",
    // Batching / parallel feedback
    "✓ Previous round:",
    "♻ Duplicate calls detected",
    // Runtime directives
    "⚠️ VERIFICATION REQUIRED",
    "🔄 ERROR BUDGET",
    "<system-reminder>",
    // Runtime correction / warning headers
    "## Already Fetched",
    "## Cross-Session Project Context",
    "## ⤴",
    "## ⚠",
    "Runtime correction:",
    "[compact session=",
];

/// Prefixes from removed scaffolding formats. These are not supported
/// interaction surfaces; they are filtered only so persisted runtime garbage
/// cannot re-enter memory or compaction.
pub const OBSOLETE_SCAFFOLDING_BODY_PREFIXES: &[&str] = &["[Active task attachment]"];

pub fn scaffolding_body_prefixes_for_filtering() -> impl Iterator<Item = &'static str> {
    SCAFFOLDING_BODY_PREFIXES
        .iter()
        .chain(OBSOLETE_SCAFFOLDING_BODY_PREFIXES.iter())
        .copied()
}

/// True when `message` is a runtime-synthesized scaffolding message.
///
/// Detection rules (applied in order):
///
/// 1. `role == "system"` messages with no content → runtime-injected nudge
///    (the runtime never emits user-typed system turns mid-conversation).
/// 2. Any message whose trimmed `content` starts with one of
///    [`SCAFFOLDING_BODY_PREFIXES`] (applies across all roles — assistant
///    messages can carry runtime-stamped directives too).
/// 3. Removed legacy scaffolding prefixes are also filtered to prevent old
///    stored runtime text from being reintroduced.
///
/// Returns `false` for genuine user/assistant conversational turns.
///
/// This is deliberately **shape-based** (role + prefix match) rather than
/// attribute-based (e.g. looking for an `"astra_scaffolding": true` field)
/// because the runtime currently synthesizes these messages through several
/// paths without a unified metadata flag. The prefix list is the
/// narrowest-waist detector that works for all paths today.
pub fn is_runtime_scaffolding_message(message: &Value) -> bool {
    let role = message.get("role").and_then(Value::as_str);
    let content = message
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let trimmed = content.trim_start();

    if role == Some("system") && trimmed.is_empty() {
        return true;
    }

    for prefix in scaffolding_body_prefixes_for_filtering() {
        if trimmed.starts_with(prefix) {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn msg(role: &str, content: &str) -> Value {
        json!({"role": role, "content": content})
    }

    // ── Positive cases: scaffolding is detected ─────────────────────────

    #[test]
    fn system_role_with_regular_content_is_not_scaffolding() {
        assert!(!is_runtime_scaffolding_message(&msg(
            "system",
            "You are a helpful assistant for this workspace."
        )));
    }

    #[test]
    fn system_role_empty_content_is_scaffolding() {
        assert!(is_runtime_scaffolding_message(&json!({"role": "system"})));
    }

    #[test]
    fn parallel_feedback_nudge_is_scaffolding() {
        assert!(is_runtime_scaffolding_message(&msg(
            "assistant",
            "✓ Previous round: 3 tools executed in parallel — excellent."
        )));
    }

    #[test]
    fn duplicate_calls_nudge_is_scaffolding() {
        assert!(is_runtime_scaffolding_message(&msg(
            "user",
            "♻ Duplicate calls detected: [read_file (3x)]."
        )));
    }

    #[test]
    fn verification_directive_is_scaffolding() {
        assert!(is_runtime_scaffolding_message(&msg(
            "user",
            "⚠️ VERIFICATION REQUIRED: Before you finish, run these checks"
        )));
    }

    #[test]
    fn error_budget_directive_is_scaffolding() {
        assert!(is_runtime_scaffolding_message(&msg(
            "user",
            "🔄 ERROR BUDGET EXHAUSTED: hit Unknown errors 3 turns in a row"
        )));
    }

    #[test]
    fn runtime_correction_headers_are_scaffolding() {
        assert!(is_runtime_scaffolding_message(&msg(
            "user",
            "## ⤴ Execution Escalation Runtime correction:"
        )));
        assert!(is_runtime_scaffolding_message(&msg(
            "user",
            "## ⤴ Parallel Batching Force"
        )));
        assert!(is_runtime_scaffolding_message(&msg(
            "user",
            "## ⚠ Sequential Tool Calls Detected"
        )));
    }

    #[test]
    fn self_check_nudge_is_scaffolding() {
        assert!(is_runtime_scaffolding_message(&msg(
            "user",
            "[Self-check — round 12] You have been reading/exploring"
        )));
    }

    #[test]
    fn obsolete_active_task_attachment_is_filtered_as_garbage() {
        assert!(is_runtime_scaffolding_message(&msg(
            "user",
            "[Active task attachment] Resume the active task below"
        )));
    }

    #[test]
    fn context_manifests_are_scaffolding() {
        assert!(is_runtime_scaffolding_message(&msg(
            "user",
            "[attention:v1]\ngoal: ship auth"
        )));
        assert!(is_runtime_scaffolding_message(&msg(
            "user",
            "[working-set:v1]\ngoal: ship auth"
        )));
        assert!(is_runtime_scaffolding_message(&msg(
            "user",
            "[session-anchor]\nResume previous task state"
        )));
    }

    #[test]
    fn inventory_and_cross_session_context_are_scaffolding() {
        assert!(is_runtime_scaffolding_message(&msg(
            "user",
            "## Already Fetched (do NOT re-read)\nfoo.rs"
        )));
        assert!(is_runtime_scaffolding_message(&msg(
            "user",
            "## Cross-Session Project Context\n- stale retrieved memory"
        )));
    }

    #[test]
    fn system_reminder_wrapper_is_scaffolding_across_roles() {
        assert!(is_runtime_scaffolding_message(&msg(
            "user",
            "<system-reminder>\nBackground task updates"
        )));
    }

    #[test]
    fn tools_used_rollup_is_scaffolding() {
        assert!(is_runtime_scaffolding_message(&msg(
            "assistant",
            "Tools used: bash, read_file, grep"
        )));
    }

    #[test]
    fn runtime_correction_inline_prefix_is_scaffolding() {
        assert!(is_runtime_scaffolding_message(&msg(
            "user",
            "Runtime correction: your previous response answered without tools"
        )));
    }

    #[test]
    fn compact_session_marker_is_scaffolding() {
        assert!(is_runtime_scaffolding_message(&msg(
            "assistant",
            "[compact session=sess-123 turn=4 source=auto tier=normal]\nWorking memory summary"
        )));
    }

    #[test]
    fn leading_whitespace_tolerated() {
        // Some injection paths prepend a blank line — still scaffolding.
        assert!(is_runtime_scaffolding_message(&msg(
            "user",
            "\n\n✓ Previous round: 2 tools executed in parallel"
        )));
    }

    // ── Negative cases: genuine turns are NOT scaffolding ────────────────

    #[test]
    fn regular_user_message_is_not_scaffolding() {
        assert!(!is_runtime_scaffolding_message(&msg(
            "user",
            "Can you review the latest commits on this branch?"
        )));
    }

    #[test]
    fn regular_assistant_message_is_not_scaffolding() {
        assert!(!is_runtime_scaffolding_message(&msg(
            "assistant",
            "I found three commits that touch the volatile block."
        )));
    }

    #[test]
    fn tool_role_is_not_scaffolding() {
        // Tool results are their own category — compaction handles them
        // separately and they must not be misclassified as scaffolding.
        assert!(!is_runtime_scaffolding_message(&msg(
            "tool",
            "file contents..."
        )));
    }

    #[test]
    fn assistant_message_mentioning_tools_used_mid_body_is_not_scaffolding() {
        // Only *starts with* triggers detection — a conversational answer
        // discussing "Tools used" shouldn't be dropped.
        assert!(!is_runtime_scaffolding_message(&msg(
            "assistant",
            "In that session, Tools used: by the agent included bash."
        )));
    }

    #[test]
    fn empty_content_non_system_is_not_scaffolding() {
        assert!(!is_runtime_scaffolding_message(&msg("user", "")));
        assert!(!is_runtime_scaffolding_message(&msg("assistant", "")));
    }
}
