//! Shared tool execution policy.
//!
//! Keep action-aware batching decisions here instead of embedding them in
//! stream consumers. The SSE, headless, and CLI paths must classify tools the
//! same way or multi-tool turns drift into transport-specific behavior.

/// Small coalescing window for adjacent concurrency-safe `tool_request` SSE
/// frames.
///
/// Providers often stream parallel tool calls as separate frames. Waiting this
/// long lets sibling calls (notably the agent spawn action) join one batch before the
/// first tool starts blocking the socket reader. The window is deliberately
/// tiny and only applies after all pending requests are already classified as
/// concurrency-safe; side-effectful tools still execute immediately.
pub const TOOL_BATCH_COALESCE_MS: u64 = 25;

/// Environment knob for operators debugging stream chunking behavior.
///
/// Production should leave this unset and use the [`TOOL_BATCH_COALESCE_MS`]
/// default. Two legitimate use cases for setting it:
/// 1. **Debugging a stuck batch** — set to `0` to disable coalescing
///    entirely so each tool dispatches immediately. Useful when the
///    coalescing loop is suspected of holding a chunk that should
///    have flushed.
/// 2. **Provider-specific tuning** — some providers chunk SSE more
///    aggressively than others; raising the window slightly may pull
///    in more siblings. Capped at 250ms so a misconfigured value
///    can't make N=1 single-tool turns feel visibly stuck.
///
/// Not exposed via TOML config / settings file — it is intentionally
/// debug-only and process-scoped. Permanent tuning belongs in the
/// const default if measurements support a change.
pub const TOOL_BATCH_COALESCE_MS_ENV: &str = "ASTRA_TOOL_BATCH_COALESCE_MS";

fn parse_tool_batch_coalesce_ms(raw: Option<&str>) -> u64 {
    raw.and_then(|raw| raw.parse::<u64>().ok())
        .map(|ms| ms.min(250))
        .unwrap_or(TOOL_BATCH_COALESCE_MS)
}

/// Return the configured coalescing window.
///
/// Invalid values fall back to [`TOOL_BATCH_COALESCE_MS`]. `0` is honoured
/// (disables coalescing — each tool dispatches immediately). Values above
/// 250ms are clamped so a misconfigured environment cannot make safe tools
/// feel visibly stuck before execution starts.
pub fn tool_batch_coalesce_duration() -> std::time::Duration {
    let configured = std::env::var(TOOL_BATCH_COALESCE_MS_ENV).ok();
    let ms = parse_tool_batch_coalesce_ms(configured.as_deref());
    std::time::Duration::from_millis(ms)
}

/// Returns `true` if the named tool is safe for concurrent execution.
///
/// Concurrent-safe tools are read-only operations without observable side
/// effects, or action-shaped tools whose selected action is order-independent.
/// This includes both sync tools (fast local I/O) and async tools (network I/O
/// that benefits most from parallel execution).
pub fn is_tool_concurrency_safe(tool: &str, args: Option<&serde_json::Value>) -> bool {
    // `memory` is action-aware: `recall` / `expand` / `profile` are pure
    // reads, safe to parallelize. All other actions (remember / forget /
    // update / focus / reflect / feedback) must be serialized.
    if tool == "memory" {
        return matches!(
            args.and_then(|a| a.get("action")).and_then(|v| v.as_str()),
            Some("recall") | Some("expand") | Some("profile")
        );
    }
    // `agent` is action-aware: `spawn` and `get_result` are safe to
    // parallelize (each spawn creates an isolated sub-process; each
    // get_result reads from the agent registry by unique agent_id).
    // `send_message` mutates a recipient mailbox — concurrent sends
    // to the same target could reorder, so it stays sequential.
    // `run_chain` orchestrates a fixed pipeline that may mutate
    // state — also sequential.
    if tool == "agent" {
        return matches!(
            args.and_then(|a| a.get("action")).and_then(|v| v.as_str()),
            Some("spawn") | Some("get_result")
        );
    }
    matches!(
        tool,
        // Local read-only (sync).
        "read_file"
            | "list_dir"
            | "grep"
            | "glob"
            | "git_status"
            | "git_diff"
            | "git_log"
            | "git_show"
            | "git_blame"
            | "git_file_history"
            | "git_contributors"
            | "git_log_search"
            | "find_definition"
            | "find_references"
            | "call_graph"
            | "extract_members"
            | "type_hierarchy"
            | "hover_info"
            | "symbol_search"
            | "dead_code"
            | "symbols"
            | "lsp"
            | "env"
            | "brief"
            | "tool_search"
            | "get_agent_info"
            | "reflect"
            | "web_fetch"
            | "web_search"
            | "share_context"
            | "query_context"
            // GitHub read-only (async - benefits from join_all).
            | "github_list_prs"
            | "github_get_pr"
            | "github_ci_status"
            | "github_list_issues"
            | "github_get_issue"
            | "github_repo_stats"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_tool_batch_coalesce_ms_honors_default_zero_and_clamp() {
        assert_eq!(
            parse_tool_batch_coalesce_ms(None),
            TOOL_BATCH_COALESCE_MS,
            "unset config must use the default window"
        );
        assert_eq!(
            parse_tool_batch_coalesce_ms(Some("0")),
            0,
            "operators must be able to disable coalescing for debugging"
        );
        assert_eq!(
            parse_tool_batch_coalesce_ms(Some("999")),
            250,
            "misconfigured high values must clamp instead of stalling tool dispatch"
        );
    }

    #[test]
    fn parse_tool_batch_coalesce_ms_falls_back_on_invalid_values() {
        assert_eq!(
            parse_tool_batch_coalesce_ms(Some("abc")),
            TOOL_BATCH_COALESCE_MS
        );
        assert_eq!(
            parse_tool_batch_coalesce_ms(Some("-1")),
            TOOL_BATCH_COALESCE_MS
        );
    }

    #[test]
    fn concurrency_safety_is_action_aware_for_memory_and_agent() {
        assert!(is_tool_concurrency_safe(
            "memory",
            Some(&json!({"action": "recall"}))
        ));
        assert!(is_tool_concurrency_safe(
            "memory",
            Some(&json!({"action": "expand"}))
        ));
        assert!(is_tool_concurrency_safe(
            "memory",
            Some(&json!({"action": "profile"}))
        ));
        assert!(!is_tool_concurrency_safe(
            "memory",
            Some(&json!({"action": "remember"}))
        ));
        assert!(!is_tool_concurrency_safe("memory", None));

        assert!(is_tool_concurrency_safe(
            "agent",
            Some(&json!({"action": "spawn"}))
        ));
        assert!(is_tool_concurrency_safe(
            "agent",
            Some(&json!({"action": "get_result"}))
        ));
        assert!(!is_tool_concurrency_safe(
            "agent",
            Some(&json!({"action": "send_message"}))
        ));
        assert!(!is_tool_concurrency_safe("agent", None));
    }
}
