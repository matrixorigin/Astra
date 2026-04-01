//! Stop hooks: verification phase when the agent thinks it's done.
//!
//! Instead of executing shell commands directly (which bypasses the tool
//! permission/audit system), stop hooks inject a user message that instructs
//! the LLM to run verification commands via the normal `bash` tool.
//! This ensures all execution goes through PermissionManager, tool event
//! auditing, and TurnGuard error tracking.

/// A verification command to run before the loop is allowed to complete.
#[derive(Debug, Clone)]
pub struct StopHook {
    /// Human-readable label (e.g. "type-check", "lint").
    pub label: String,
    /// Shell command to execute (e.g. "cargo check").
    pub command: String,
    /// Working directory (informational, included in the prompt).
    pub working_dir: Option<String>,
}

/// Build a user message that instructs the LLM to run verification commands.
///
/// Returns `None` if there are no hooks (caller should complete normally).
pub fn build_stop_hook_prompt(hooks: &[StopHook]) -> Option<serde_json::Value> {
    if hooks.is_empty() {
        return None;
    }
    let commands: Vec<String> = hooks
        .iter()
        .map(|h| {
            if let Some(dir) = &h.working_dir {
                format!("- `{}` (in `{dir}`) — {}", h.command, h.label)
            } else {
                format!("- `{}` — {}", h.command, h.label)
            }
        })
        .collect();
    Some(serde_json::json!({
        "role": "user",
        "content": format!(
            "⚠️ VERIFICATION REQUIRED: Before you finish, run these checks using the bash tool:\n\
             {}\n\n\
             If any check fails, fix the issues and re-run the failing check. \
             Repeat until all checks pass. Only then may you complete.",
            commands.join("\n")
        )
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_hooks_returns_none() {
        assert!(build_stop_hook_prompt(&[]).is_none());
    }

    #[test]
    fn single_hook_generates_prompt() {
        let hooks = vec![StopHook {
            label: "type-check".into(),
            command: "cargo check".into(),
            working_dir: Some("/project".into()),
        }];
        let msg = build_stop_hook_prompt(&hooks).unwrap();
        let content = msg["content"].as_str().unwrap();
        assert!(content.contains("cargo check"));
        assert!(content.contains("/project"));
        assert!(content.contains("type-check"));
        assert!(content.contains("bash tool"));
    }

    #[test]
    fn multiple_hooks_listed() {
        let hooks = vec![
            StopHook {
                label: "check".into(),
                command: "cargo check".into(),
                working_dir: None,
            },
            StopHook {
                label: "lint".into(),
                command: "cargo clippy".into(),
                working_dir: None,
            },
        ];
        let msg = build_stop_hook_prompt(&hooks).unwrap();
        let content = msg["content"].as_str().unwrap();
        assert!(content.contains("cargo check"));
        assert!(content.contains("cargo clippy"));
    }
}
