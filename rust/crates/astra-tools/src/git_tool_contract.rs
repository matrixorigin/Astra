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
}
