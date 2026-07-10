//! Pure plan-mode tool policy.
//!
//! Plan mode is a permission overlay, not a separate capability provider:
//! the model should keep a stable tool surface for exploration, while
//! implementation side effects are denied at execution/admission time.

use serde_json::Value;

/// Whether `tool_name` is a plan-control tool that must always remain callable.
pub fn is_plan_control_tool(tool_name: &str) -> bool {
    crate::tool::schema::prune::PLAN_MODE_REQUIRED_TOOLS.contains(&tool_name)
}

/// Whether this invocation is plan-authoring/session-local state, not external
/// implementation execution.
fn is_plan_internal_authoring_tool(tool_name: &str, args: &Value) -> bool {
    if is_plan_control_tool(tool_name) {
        return true;
    }

    match tool_name {
        "memory" => matches!(
            args.get("action").and_then(Value::as_str),
            Some("recall" | "expand" | "profile" | "remember" | "update")
        ),
        "task_board" => matches!(
            args.get("action").and_then(Value::as_str),
            Some("create" | "update" | "list" | "get" | "list_user" | "adopt")
        ),
        "task_output" | "task_list" => true,
        _ => false,
    }
}

/// Tools whose successful call creates runtime/process side effects even when
/// their command text appears read-only.
fn is_persistent_execution_surface(tool_name: &str) -> bool {
    matches!(tool_name, "background_shell")
}

fn is_plan_read_only_bash(args: &Value) -> bool {
    let Some(command) = args.get("command").and_then(Value::as_str).map(str::trim) else {
        return false;
    };
    let command = command.strip_prefix("cd ").map_or(command, |rest| {
        rest.split_once("&&")
            .map(|(_, tail)| tail.trim())
            .unwrap_or(rest)
    });
    matches!(
        command.split_whitespace().next(),
        Some("ls" | "pwd" | "grep" | "find" | "rg" | "cat" | "head" | "tail" | "wc")
    ) || command.starts_with("git status")
        || command.starts_with("git diff")
        || command.starts_with("git log")
        || command.starts_with("git show")
        || command.starts_with("git blame")
        || command.starts_with("cargo check")
        || command.starts_with("cargo test")
        || command.starts_with("make check")
        || command.starts_with("make test")
}

/// Returns true when a tool invocation must be blocked while plan authoring is
/// active.
///
/// This is args-aware. For example, `bash {"command":"git status"}` is an
/// observation and stays allowed; `bash {"command":"touch x"}` is execution
/// and is blocked.
pub fn is_plan_mode_blocked_tool(tool_name: &str, args: &Value) -> bool {
    if is_plan_internal_authoring_tool(tool_name, args) {
        return false;
    }
    if is_persistent_execution_surface(tool_name) {
        return true;
    }
    if tool_name == "bash" {
        return !is_plan_read_only_bash(args);
    }
    crate::tool::categories::classify(tool_name, Some(args))
        .category
        .is_mutating()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn plan_mode_uses_args_aware_shell_policy() {
        for command in [
            "git status --short",
            "ls crates",
            "cargo check 2>&1 | head -50",
        ] {
            assert!(
                !is_plan_mode_blocked_tool("bash", &json!({"command": command})),
                "read-only bash command must remain available in plan mode: {command}"
            );
        }

        for command in [
            "touch plan.txt",
            "git push origin main",
            "rm -rf /tmp/stale",
        ] {
            assert!(
                is_plan_mode_blocked_tool("bash", &json!({"command": command})),
                "mutating bash command must be blocked in plan mode: {command}"
            );
        }
    }

    #[test]
    fn plan_mode_blocks_external_mutations_but_allows_plan_internal_authoring() {
        for (tool, args) in [
            ("write_file", json!({"path": "plan.txt", "content": "x"})),
            (
                "str_replace",
                json!({"path": "plan.txt", "old": "a", "new": "b"}),
            ),
            ("multi_edit", json!({"path": "plan.txt", "edits": []})),
            ("delete_file", json!({"path": "plan.txt"})),
            ("rollback_file_edits", json!({"scope": "current_turn"})),
            ("rollback_session_state", json!({"scope": "last_turn"})),
            ("adjust_config", json!({"key": "model", "value": "fast"})),
            ("compress_context", json!({"target_tokens": 1000})),
            ("publish_artifact", json!({"path": "report.md"})),
            ("run_script", json!({"script": "touch plan.txt"})),
            ("rollback_database_snapshots", json!({})),
            ("background_shell", json!({"command": "ls rust"})),
        ] {
            assert!(
                is_plan_mode_blocked_tool(tool, &args),
                "{tool} must be blocked during plan authoring"
            );
        }

        for (tool, args) in [
            ("read_file", json!({"path": "src/lib.rs"})),
            ("grep", json!({"pattern": "needle", "path": "src"})),
            ("glob", json!({"pattern": "**/*.rs"})),
            ("list_dir", json!({"path": "src"})),
            ("enter_plan_mode", json!({})),
            ("exit_plan_mode", json!({"plan": "1. inspect"})),
            (
                "task_board",
                json!({"action": "create", "title": "draft plan item"}),
            ),
            ("task_board", json!({"action": "update", "task_id": "t1"})),
            ("task_board", json!({"action": "list"})),
            ("task_board", json!({"action": "get", "task_id": "t1"})),
            (
                "memory",
                json!({"action": "remember", "content": "plan context"}),
            ),
            (
                "memory",
                json!({"action": "recall", "query": "plan context"}),
            ),
        ] {
            assert!(
                !is_plan_mode_blocked_tool(tool, &args),
                "{tool} with args {args} should remain available during plan authoring"
            );
        }
    }

    #[test]
    fn plan_mode_is_action_aware_for_git_github_and_task_control() {
        for action in ["commit", "revert_commit", "push"] {
            assert!(is_plan_mode_blocked_tool("git", &json!({"action": action})));
        }
        assert!(is_plan_mode_blocked_tool(
            "git",
            &json!({"action": "stash", "sub_action": "pop"})
        ));
        assert!(!is_plan_mode_blocked_tool(
            "git",
            &json!({"action": "stash", "sub_action": "list"})
        ));
        for action in ["status", "diff", "log", "show", "blame"] {
            assert!(!is_plan_mode_blocked_tool(
                "git",
                &json!({"action": action})
            ));
        }

        assert!(is_plan_mode_blocked_tool(
            "github",
            &json!({"action": "create_issue"})
        ));
        assert!(!is_plan_mode_blocked_tool(
            "github",
            &json!({"action": "list_prs"})
        ));

        assert!(is_plan_mode_blocked_tool(
            "task_stop",
            &json!({"task_id": "bg-shell-1"})
        ));
        assert!(is_plan_mode_blocked_tool(
            "task_board",
            &json!({"action": "stop", "task_id": "bg-shell-1"})
        ));
    }
}
