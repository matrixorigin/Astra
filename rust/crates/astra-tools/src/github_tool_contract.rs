use serde_json::Value;

/// Public actions exposed by the model-facing `github` schema.
pub const GITHUB_ACTIONS: &[&str] = &[
    "list_prs",
    "get_pr",
    "ci_status",
    "repo_stats",
    "list_issues",
    "get_issue",
    "create_issue",
];

pub const GITHUB_ACTIONS_DISPLAY: &str =
    "list_prs, get_pr, ci_status, repo_stats, list_issues, get_issue, create_issue";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GithubAction {
    ListPrs,
    GetPr,
    CiStatus,
    RepoStats,
    ListIssues,
    GetIssue,
    CreateIssue,
}

impl GithubAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ListPrs => "list_prs",
            Self::GetPr => "get_pr",
            Self::CiStatus => "ci_status",
            Self::RepoStats => "repo_stats",
            Self::ListIssues => "list_issues",
            Self::GetIssue => "get_issue",
            Self::CreateIssue => "create_issue",
        }
    }
}

pub fn github_missing_action_message() -> String {
    format!(
        "missing required parameter `action` for `github`. Retry the same `github` tool with action set to one of: {GITHUB_ACTIONS_DISPLAY}."
    )
}

pub fn github_action_type_message() -> &'static str {
    "field `action` for `github` must be a string"
}

pub fn github_unknown_action_message(action: &str) -> String {
    format!("unknown `github` action '{action}'. Use one of: {GITHUB_ACTIONS_DISPLAY}.")
}

pub fn github_action_from_args(args: &Value) -> Result<GithubAction, String> {
    match args.get("action") {
        Some(Value::String(action)) if !action.trim().is_empty() => match action.as_str() {
            "list_prs" => Ok(GithubAction::ListPrs),
            "get_pr" => Ok(GithubAction::GetPr),
            "ci_status" => Ok(GithubAction::CiStatus),
            "repo_stats" => Ok(GithubAction::RepoStats),
            "list_issues" => Ok(GithubAction::ListIssues),
            "get_issue" => Ok(GithubAction::GetIssue),
            "create_issue" => Ok(GithubAction::CreateIssue),
            other => Err(github_unknown_action_message(other)),
        },
        Some(Value::String(_)) | None => Err(github_missing_action_message()),
        Some(_) => Err(github_action_type_message().to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn action_contract_matches_schema_order() {
        let parsed_actions = GITHUB_ACTIONS
            .iter()
            .copied()
            .map(|action| {
                github_action_from_args(&json!({"action": action}))
                    .expect("schema action must parse")
                    .as_str()
            })
            .collect::<Vec<_>>();
        assert_eq!(parsed_actions, GITHUB_ACTIONS);
    }

    #[test]
    fn action_parser_rejects_missing_blank_wrong_type_and_unknown() {
        let missing = github_action_from_args(&json!({})).expect_err("missing action must fail");
        assert!(missing.contains("missing required parameter `action`"));
        assert!(missing.contains(GITHUB_ACTIONS_DISPLAY));

        let blank =
            github_action_from_args(&json!({"action": ""})).expect_err("blank action must fail");
        assert_eq!(blank, missing);

        let wrong_type =
            github_action_from_args(&json!({"action": 7})).expect_err("wrong type must fail");
        assert_eq!(wrong_type, github_action_type_message());

        let unknown =
            github_action_from_args(&json!({"action": "merge_pr"})).expect_err("unknown must fail");
        assert!(unknown.contains("unknown `github` action 'merge_pr'"));
    }
}
