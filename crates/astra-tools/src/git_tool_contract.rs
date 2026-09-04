use serde_json::Value;

pub const GIT_ACTIONS: &[&str] = &[
    "status",
    "diff",
    "log",
    "show",
    "blame",
    "file_history",
    "log_search",
    "contributors",
    "commit",
    "revert_commit",
    "stash",
    "checkout_file",
    "worktree",
    "push",
];

pub const GIT_ACTIONS_DISPLAY: &str = "status, diff, log, show, blame, file_history, log_search, contributors, commit, revert_commit, stash, checkout_file, worktree, push";

/// Canonical model-facing stash sub-actions. Legacy `save` remains accepted by
/// the executor, but `push` is the single advertised spelling.
pub const GIT_STASH_SUB_ACTIONS: &[&str] = &["push", "apply", "pop", "list", "drop"];
pub const GIT_STASH_SUB_ACTIONS_DISPLAY: &str = "push, apply, pop, list, drop";

/// Canonical model-facing worktree sub-actions. `enter` and `exit` are part of
/// the Edge session contract, while `add`, `list`, and `remove` wrap Git's
/// persistent worktree operations.
pub const GIT_WORKTREE_SUB_ACTIONS: &[&str] = &["enter", "exit", "add", "list", "remove"];
pub const GIT_WORKTREE_SUB_ACTIONS_DISPLAY: &str = "enter, exit, add, list, remove";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitAction {
    Status,
    Diff,
    Log,
    Show,
    Blame,
    FileHistory,
    LogSearch,
    Contributors,
    Commit,
    RevertCommit,
    Stash,
    CheckoutFile,
    Worktree,
    Push,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitStashSubAction {
    Push,
    Apply,
    Pop,
    List,
    Drop,
}

impl GitStashSubAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Push => "push",
            Self::Apply => "apply",
            Self::Pop => "pop",
            Self::List => "list",
            Self::Drop => "drop",
        }
    }

    pub fn mutates_workspace(self) -> bool {
        !matches!(self, Self::List)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitWorktreeSubAction {
    Enter,
    Exit,
    Add,
    List,
    Remove,
}

impl GitWorktreeSubAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Enter => "enter",
            Self::Exit => "exit",
            Self::Add => "add",
            Self::List => "list",
            Self::Remove => "remove",
        }
    }

    pub fn mutates_workspace(self) -> bool {
        !matches!(self, Self::List)
    }
}

impl GitAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Diff => "diff",
            Self::Log => "log",
            Self::Show => "show",
            Self::Blame => "blame",
            Self::FileHistory => "file_history",
            Self::LogSearch => "log_search",
            Self::Contributors => "contributors",
            Self::Commit => "commit",
            Self::RevertCommit => "revert_commit",
            Self::Stash => "stash",
            Self::CheckoutFile => "checkout_file",
            Self::Worktree => "worktree",
            Self::Push => "push",
        }
    }
}

pub fn git_missing_action_message() -> String {
    format!(
        "missing required parameter `action` for `git`. Retry the same `git` tool with action set to one of: {GIT_ACTIONS_DISPLAY}."
    )
}

pub fn git_action_type_message() -> &'static str {
    "field `action` for `git` must be a string"
}

pub fn git_unknown_action_message(action: &str) -> String {
    format!("unknown `git` action '{action}'. Use one of: {GIT_ACTIONS_DISPLAY}.")
}

pub fn git_action_from_args(args: &Value) -> Result<GitAction, String> {
    match args.get("action") {
        Some(Value::String(action)) if !action.trim().is_empty() => match action.as_str() {
            "status" => Ok(GitAction::Status),
            "diff" => Ok(GitAction::Diff),
            "log" => Ok(GitAction::Log),
            "show" => Ok(GitAction::Show),
            "blame" => Ok(GitAction::Blame),
            "file_history" => Ok(GitAction::FileHistory),
            "log_search" => Ok(GitAction::LogSearch),
            "contributors" => Ok(GitAction::Contributors),
            "commit" => Ok(GitAction::Commit),
            "revert_commit" => Ok(GitAction::RevertCommit),
            "stash" => Ok(GitAction::Stash),
            "checkout_file" => Ok(GitAction::CheckoutFile),
            "worktree" => Ok(GitAction::Worktree),
            "push" => Ok(GitAction::Push),
            other => Err(git_unknown_action_message(other)),
        },
        Some(Value::String(_)) | None => Err(git_missing_action_message()),
        Some(_) => Err(git_action_type_message().to_string()),
    }
}

fn nested_action_from_args<'a>(
    args: &'a Value,
    parent_action: &str,
    display: &str,
) -> Result<&'a str, String> {
    match args.get("sub_action") {
        Some(Value::String(action)) if !action.trim().is_empty() => Ok(action),
        Some(Value::String(_)) | None => {
            // Internal callers historically passed the nested action through
            // `action`. Preserve that compatibility without allowing a public
            // `action=stash|worktree` request to re-enter the old catch-22.
            if let Some(Value::String(action)) = args.get("action")
                && action != parent_action
                && !action.trim().is_empty()
            {
                return Ok(action);
            }
            Err(format!(
                "git(action={parent_action}) requires non-empty string field `sub_action`. Use one of: {display}."
            ))
        }
        Some(_) => Err(format!(
            "field `sub_action` for git(action={parent_action}) must be a string"
        )),
    }
}

pub fn git_stash_sub_action_from_args(args: &Value) -> Result<GitStashSubAction, String> {
    let action = nested_action_from_args(args, "stash", GIT_STASH_SUB_ACTIONS_DISPLAY)?;
    match action {
        "push" | "save" => Ok(GitStashSubAction::Push),
        "apply" => Ok(GitStashSubAction::Apply),
        "pop" => Ok(GitStashSubAction::Pop),
        "list" => Ok(GitStashSubAction::List),
        "drop" => Ok(GitStashSubAction::Drop),
        other => Err(format!(
            "unknown git stash sub_action '{other}'. Use one of: {GIT_STASH_SUB_ACTIONS_DISPLAY}."
        )),
    }
}

pub fn git_worktree_sub_action_from_args(args: &Value) -> Result<GitWorktreeSubAction, String> {
    let action = nested_action_from_args(args, "worktree", GIT_WORKTREE_SUB_ACTIONS_DISPLAY)?;
    match action {
        "enter" => Ok(GitWorktreeSubAction::Enter),
        "exit" => Ok(GitWorktreeSubAction::Exit),
        "add" | "create" => Ok(GitWorktreeSubAction::Add),
        "list" | "ls" => Ok(GitWorktreeSubAction::List),
        "remove" | "rm" | "delete" => Ok(GitWorktreeSubAction::Remove),
        other => Err(format!(
            "unknown git worktree sub_action '{other}'. Use one of: {GIT_WORKTREE_SUB_ACTIONS_DISPLAY}."
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn action_contract_matches_schema_order() {
        let parsed_actions = GIT_ACTIONS
            .iter()
            .copied()
            .map(|action| {
                git_action_from_args(&json!({"action": action}))
                    .expect("schema action must parse")
                    .as_str()
            })
            .collect::<Vec<_>>();
        assert_eq!(parsed_actions, GIT_ACTIONS);
    }

    #[test]
    fn action_parser_rejects_missing_blank_wrong_type_and_unknown() {
        let missing = git_action_from_args(&json!({})).expect_err("missing action must fail");
        assert!(missing.contains("missing required parameter `action`"));
        assert!(missing.contains(GIT_ACTIONS_DISPLAY));

        let blank = git_action_from_args(&json!({"action": ""})).expect_err("blank action fails");
        assert_eq!(blank, missing);

        let wrong_type =
            git_action_from_args(&json!({"action": 7})).expect_err("wrong type must fail");
        assert_eq!(wrong_type, git_action_type_message());

        let unknown =
            git_action_from_args(&json!({"action": "merge"})).expect_err("unknown must fail");
        assert!(unknown.contains("unknown `git` action 'merge'"));
    }

    #[test]
    fn nested_action_parsers_use_sub_action_and_keep_legacy_inner_calls() {
        assert_eq!(
            git_stash_sub_action_from_args(&json!({"action": "stash", "sub_action": "list"})),
            Ok(GitStashSubAction::List)
        );
        assert_eq!(
            git_worktree_sub_action_from_args(&json!({"action": "worktree", "sub_action": "list"})),
            Ok(GitWorktreeSubAction::List)
        );
        assert_eq!(
            git_stash_sub_action_from_args(&json!({"action": "save"})),
            Ok(GitStashSubAction::Push)
        );
        assert_eq!(
            git_worktree_sub_action_from_args(&json!({"action": "ls"})),
            Ok(GitWorktreeSubAction::List)
        );
    }

    #[test]
    fn nested_action_parsers_reject_missing_wrong_type_and_cross_parent_values() {
        for args in [json!({}), json!({"action": "worktree"})] {
            let error = git_worktree_sub_action_from_args(&args).unwrap_err();
            assert!(error.contains("requires non-empty string field `sub_action`"));
            assert!(error.contains(GIT_WORKTREE_SUB_ACTIONS_DISPLAY));
        }

        let wrong_type =
            git_worktree_sub_action_from_args(&json!({"action": "worktree", "sub_action": 7}))
                .unwrap_err();
        assert!(wrong_type.contains("must be a string"));

        let wrong_parent =
            git_stash_sub_action_from_args(&json!({"action": "stash", "sub_action": "enter"}))
                .unwrap_err();
        assert!(wrong_parent.contains("unknown git stash sub_action 'enter'"));
    }

    #[test]
    fn nested_action_mutation_contract_marks_only_lists_read_only() {
        assert!(!GitStashSubAction::List.mutates_workspace());
        assert!(!GitWorktreeSubAction::List.mutates_workspace());
        assert!(GitStashSubAction::Push.mutates_workspace());
        assert!(GitWorktreeSubAction::Enter.mutates_workspace());
    }
}
