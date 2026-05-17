//! Suggesting safe fallbacks when a tool is blocked at runtime.
//!
//! When the runtime denies a tool call (typically because the tool
//! is in `restricted_tools` after repeated failures or because plan
//! mode hides mutating tools), the model needs an actionable error
//! message — not a generic "this is restricted" line, and definitely
//! not "use bash" when bash is itself the tool that was just denied.
//!
//! [`restricted_tool_workaround_message`] composes that message
//! from three inputs: the blocked tool, the set of currently
//! available tools (i.e. visible-to-model and not restricted), and
//! the category registry. It picks a same-category fallback when
//! one exists, falls back to a read-only inspector when nothing
//! else is callable, and finally asks the user when there is no
//! workable fallback.
//!
//! Pure: zero state, zero IO. Testable in isolation.

use std::collections::HashSet;

use crate::tool_categories::{ToolCategory, ToolRegistry};

/// Compose the user-visible (and model-visible) message returned
/// when `tool_name` is denied because it is in the session's
/// `restricted_tools` set.
///
/// `available_tools` is the set of tool names the model could still
/// call this turn (visible in the schema and not restricted).
/// `registry` provides category lookups.
///
/// Invariants the message holds:
///   1. Never recommend the blocked tool itself ("use `bash`" when
///      bash is the blocked tool).
///   2. Never recommend a tool not in `available_tools` (i.e. never
///      recommend another restricted tool).
///   3. Always explain *why* the block happened (so the model can
///      reason about whether re-trying the same call would help).
///   4. Always include a recoverable next action — even when no
///      fallback tool exists, instruct the model to ask the user.
pub fn restricted_tool_workaround_message(
    tool_name: &str,
    available_tools: &HashSet<String>,
    registry: &ToolRegistry,
) -> String {
    let blocked_category = registry.category(tool_name);
    let fallback = pick_fallback(tool_name, blocked_category, available_tools, registry);

    let why = format!(
        "Tool '{tool_name}' is currently restricted in this session. \
         The tool itself is not broken — repeated previous calls failed \
         (often due to argument errors or plan-mode gating)."
    );

    let next = match fallback {
        FallbackChoice::SameCategory(name) => format!(
            "Try `{name}` for the same goal — it is available and shares \
             the same effect class. If that still does not fit, describe \
             the change and ask the user to run it directly."
        ),
        FallbackChoice::ReadOnlyOnly(name) => format!(
            "No equivalent tool is available — `{name}` is the closest \
             read-only inspector. Use it to re-confirm the state, then \
             describe the change and ask the user to run it directly."
        ),
        FallbackChoice::None => {
            "No equivalent tool is available either. Describe what you need \
             in plain text and ask the user to run the command on your behalf."
                .to_string()
        }
    };

    format!("{why}\n\n{next}")
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FallbackChoice {
    /// Found a tool in the same category as the blocked one.
    SameCategory(String),
    /// Only read-only inspectors are available — degraded but
    /// still useful (the model can at least re-read state).
    ReadOnlyOnly(String),
    /// Nothing meaningful is available; the model must ask the user.
    None,
}

fn pick_fallback(
    blocked_tool: &str,
    blocked_category: ToolCategory,
    available_tools: &HashSet<String>,
    registry: &ToolRegistry,
) -> FallbackChoice {
    // Stable iteration order so identical inputs produce identical
    // messages (useful for tests, journal diffs, and snapshot
    // approval flows). HashSet is unordered; sort once.
    let mut sorted: Vec<&str> = available_tools.iter().map(String::as_str).collect();
    sorted.sort_unstable();

    let mut same_category: Option<&str> = None;
    let mut read_only_pick: Option<&str> = None;

    for candidate in sorted {
        // Invariant 1: never recommend the blocked tool itself.
        if candidate == blocked_tool {
            continue;
        }
        let cat = registry.category(candidate);
        if cat == blocked_category && same_category.is_none() {
            same_category = Some(candidate);
        }
        if cat == ToolCategory::ReadOnly && read_only_pick.is_none() {
            read_only_pick = Some(candidate);
        }
    }

    if let Some(name) = same_category {
        return FallbackChoice::SameCategory(name.to_string());
    }
    if let Some(name) = read_only_pick {
        return FallbackChoice::ReadOnlyOnly(name.to_string());
    }
    FallbackChoice::None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn registry() -> &'static ToolRegistry {
        // Use the process-wide singleton — the tests below exercise
        // tool names registered in the production catalog (read_file,
        // grep, write_file, str_replace, bash, …).
        crate::tool_categories::registry()
    }

    #[test]
    fn never_recommends_the_blocked_tool_as_its_own_workaround() {
        // Regression for session 19298aea: when bash was restricted,
        // the old hard-coded template said "use `bash` to accomplish
        // the same task directly" — feeding the model a self-referential
        // suggestion that just made it call bash again.
        let available = set(&["bash", "read_file", "grep"]);
        let msg = restricted_tool_workaround_message("bash", &available, &registry());
        assert!(
            !msg.contains("`bash`"),
            "must not recommend `bash` when bash is the blocked tool. Got: {msg}"
        );
    }

    #[test]
    fn picks_same_category_fallback_when_one_exists() {
        // bash and shell are both Shell category. If bash is blocked
        // but shell is available, suggest shell — same effect class,
        // not a degraded read-only stand-in.
        let available = set(&["bash", "shell", "read_file"]);
        let msg = restricted_tool_workaround_message("bash", &available, &registry());
        assert!(
            msg.contains("`shell`"),
            "should suggest `shell` (same Shell category). Got: {msg}"
        );
    }

    #[test]
    fn never_recommends_a_tool_outside_the_available_set() {
        // bash blocked, write_file blocked too — only read tools
        // remain. Must not suggest write_file even though
        // historically the template might have.
        let available = set(&["read_file", "grep"]);
        let msg = restricted_tool_workaround_message("bash", &available, &registry());
        assert!(
            !msg.contains("`write_file`"),
            "must not recommend write_file when it isn't in the available set. Got: {msg}"
        );
        assert!(
            !msg.contains("`bash`"),
            "must not recommend `bash` either. Got: {msg}"
        );
    }

    #[test]
    fn falls_back_to_read_only_when_no_same_category_tool_exists() {
        // write_file (Mutating) is blocked, no other Mutating or
        // Shell tools available, but read_file (ReadOnly) is. The
        // message degrades to "use the read tool to re-confirm
        // state, then ask the user".
        let available = set(&["read_file", "grep"]);
        let msg = restricted_tool_workaround_message("write_file", &available, &registry());
        assert!(
            msg.contains("`read_file`") || msg.contains("`grep`"),
            "must reference the available read-only tool. Got: {msg}"
        );
        assert!(
            msg.contains("ask the user"),
            "must instruct the model to ask the user since the read tool can't write. Got: {msg}"
        );
    }

    #[test]
    fn falls_back_to_ask_user_when_nothing_meaningful_remains() {
        // Empty available set — model has nothing left.
        let available = set(&[]);
        let msg = restricted_tool_workaround_message("write_file", &available, &registry());
        assert!(
            msg.contains("ask the user"),
            "no available tools means the only recourse is asking the user. Got: {msg}"
        );
        assert!(
            !msg.contains("Try `"),
            "must not produce a phantom 'Try `…`' recommendation when no fallback exists. Got: {msg}"
        );
    }

    #[test]
    fn message_explains_why_the_tool_is_blocked() {
        // The model needs the *reason* to decide whether to retry
        // the same call (won't help if it's a session restriction)
        // versus rephrasing arguments.
        let available = set(&["read_file"]);
        let msg = restricted_tool_workaround_message("bash", &available, &registry());
        assert!(
            msg.contains("restricted") && (msg.contains("repeated") || msg.contains("plan-mode")),
            "must explain that the block is a session-level restriction, not a transient bug. Got: {msg}"
        );
    }

    #[test]
    fn output_is_deterministic_across_runs() {
        // Sorting the available set means identical inputs produce
        // identical messages, which keeps journal diffs and
        // snapshot tests stable.
        let available = set(&["bash", "run_script", "read_file"]);
        let msg1 = restricted_tool_workaround_message("write_file", &available, &registry());
        let msg2 = restricted_tool_workaround_message("write_file", &available, &registry());
        assert_eq!(msg1, msg2);
    }
}
