use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::cloud_approval_policy::{
    CloudGatedToolKind, bash_command_is_read_only, cloud_gated_tool_kind,
};
use crate::tool_sandbox::{CommandRisk, analyze_command_risks};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionCategory {
    Read,
    Write,
    Execute,
    Destructive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompensationKind {
    DeleteFile,
    RestoreFileContents,
    RestoreOrDeleteFile,
    GitRestoreIndex,
    GitRestoreWorktree,
    GitRevertCommit,
    RestoreDatabaseSnapshot,
    Manual,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionCompensationProfile {
    pub bounded: bool,
    pub category: ActionCategory,
    pub reversible: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub requires_pre_state: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compensation_kind: Option<CompensationKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compensation_summary: Option<String>,
}

impl ActionCompensationProfile {
    fn read(bounded: bool) -> Self {
        Self {
            bounded,
            category: ActionCategory::Read,
            reversible: true,
            requires_pre_state: false,
            compensation_kind: None,
            compensation_summary: None,
        }
    }

    fn compensated(
        bounded: bool,
        category: ActionCategory,
        requires_pre_state: bool,
        compensation_kind: CompensationKind,
        compensation_summary: String,
    ) -> Self {
        Self {
            bounded,
            category,
            reversible: true,
            requires_pre_state,
            compensation_kind: Some(compensation_kind),
            compensation_summary: Some(compensation_summary),
        }
    }

    fn manual(bounded: bool, category: ActionCategory, summary: &str) -> Self {
        Self {
            bounded,
            category,
            reversible: false,
            requires_pre_state: false,
            compensation_kind: Some(CompensationKind::Manual),
            compensation_summary: Some(summary.to_string()),
        }
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn normalize_args(args: &Value) -> Value {
    match args {
        Value::String(raw) => {
            serde_json::from_str(raw).unwrap_or_else(|_| Value::Object(Default::default()))
        }
        value => value.clone(),
    }
}

fn string_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str).map(str::trim)
}

fn first_sql_keyword(sql: &str) -> Option<String> {
    sql.split_whitespace()
        .next()
        .map(|keyword| keyword.trim_matches(|c: char| c == '(' || c == ';'))
        .filter(|keyword| !keyword.is_empty())
        .map(|keyword| keyword.to_ascii_uppercase())
}

fn file_target_summary(path: Option<&str>) -> String {
    path.filter(|path| !path.is_empty())
        .map(|path| format!("target `{path}`"))
        .unwrap_or_else(|| "the target file".to_string())
}

fn shell_action_profile(command: Option<&str>) -> ActionCompensationProfile {
    let Some(command) = command.filter(|command| !command.trim().is_empty()) else {
        return ActionCompensationProfile::manual(
            false,
            ActionCategory::Execute,
            "unbounded shell action with no automatic rollback registered",
        );
    };

    if bash_command_is_read_only(command) {
        return ActionCompensationProfile::read(false);
    }

    let lower = command.trim().to_ascii_lowercase();
    if lower == "git commit" || lower.starts_with("git commit ") {
        return ActionCompensationProfile::compensated(
            false,
            ActionCategory::Execute,
            false,
            CompensationKind::GitRevertCommit,
            "create a compensating revert commit with `git revert <commit>`".to_string(),
        );
    }
    if lower.starts_with("git add ") {
        return ActionCompensationProfile::compensated(
            false,
            ActionCategory::Execute,
            false,
            CompensationKind::GitRestoreIndex,
            "unstage the paths with `git restore --staged <paths>`".to_string(),
        );
    }
    if lower.starts_with("git rm ") || lower.starts_with("git mv ") {
        return ActionCompensationProfile::compensated(
            false,
            ActionCategory::Execute,
            false,
            CompensationKind::GitRestoreWorktree,
            "restore tracked paths with `git restore --source=HEAD --staged --worktree <paths>`"
                .to_string(),
        );
    }

    let risks = analyze_command_risks(command);
    let destructive = lower.starts_with("rm ")
        || lower.contains(" rm ")
        || risks.iter().any(|risk| {
            matches!(
                risk,
                CommandRisk::PathTraversal
                    | CommandRisk::ProcessControl
                    | CommandRisk::PrivilegeEscalation
                    | CommandRisk::RemoteCodeExecution
                    | CommandRisk::OutputRedirection
            )
        });

    if destructive {
        ActionCompensationProfile::manual(
            false,
            ActionCategory::Destructive,
            "destructive shell action has no automatic rollback registered",
        )
    } else {
        ActionCompensationProfile::manual(
            false,
            ActionCategory::Execute,
            "unbounded shell action has no automatic rollback registered",
        )
    }
}

fn sql_action_profile(args: &Value) -> ActionCompensationProfile {
    let keyword = string_arg(args, "sql").and_then(first_sql_keyword);
    match keyword.as_deref() {
        Some("SELECT" | "SHOW" | "DESCRIBE" | "EXPLAIN") => ActionCompensationProfile::read(true),
        Some("INSERT" | "UPDATE" | "REPLACE" | "CREATE") => ActionCompensationProfile::compensated(
            true,
            ActionCategory::Write,
            true,
            CompensationKind::RestoreDatabaseSnapshot,
            "restore affected data from a MatrixOne snapshot captured before execution".to_string(),
        ),
        Some("DROP" | "DELETE" | "TRUNCATE" | "ALTER" | "GRANT" | "REVOKE") => {
            ActionCompensationProfile::compensated(
                true,
                ActionCategory::Destructive,
                true,
                CompensationKind::RestoreDatabaseSnapshot,
                "restore affected objects from a MatrixOne snapshot captured before execution"
                    .to_string(),
            )
        }
        _ if args
            .get("allow_destructive")
            .and_then(Value::as_bool)
            .unwrap_or(false) =>
        {
            ActionCompensationProfile::compensated(
                true,
                ActionCategory::Destructive,
                true,
                CompensationKind::RestoreDatabaseSnapshot,
                "restore affected objects from a MatrixOne snapshot captured before execution"
                    .to_string(),
            )
        }
        _ => ActionCompensationProfile::read(true),
    }
}

pub fn tool_action_profile(tool_name: &str, args: &Value) -> ActionCompensationProfile {
    let normalized_args = normalize_args(args);
    match tool_name {
        "create_file" => ActionCompensationProfile::compensated(
            true,
            ActionCategory::Write,
            false,
            CompensationKind::DeleteFile,
            format!(
                "delete {}",
                file_target_summary(string_arg(&normalized_args, "path"))
            ),
        ),
        "write_file" => ActionCompensationProfile::compensated(
            true,
            ActionCategory::Write,
            true,
            CompensationKind::RestoreOrDeleteFile,
            format!(
                "restore prior contents for {} or delete it if this write created the file",
                file_target_summary(string_arg(&normalized_args, "path"))
            ),
        ),
        "edit_file" | "str_replace" => ActionCompensationProfile::compensated(
            true,
            ActionCategory::Write,
            true,
            CompensationKind::RestoreFileContents,
            format!(
                "restore prior contents for {}",
                file_target_summary(string_arg(&normalized_args, "path"))
            ),
        ),
        "bash" | "exec" | "run_command" | "shell" => {
            shell_action_profile(string_arg(&normalized_args, "command"))
        }
        "mo_query" => sql_action_profile(&normalized_args),
        _ if tool_name.starts_with("mcp_") => ActionCompensationProfile::manual(
            false,
            ActionCategory::Execute,
            "external MCP action has no automatic rollback registered",
        ),
        _ => match cloud_gated_tool_kind(tool_name) {
            Some(CloudGatedToolKind::Write) => ActionCompensationProfile::manual(
                true,
                ActionCategory::Write,
                "manual rollback required; no automatic compensation plan is registered",
            ),
            Some(CloudGatedToolKind::Execute) => ActionCompensationProfile::manual(
                true,
                ActionCategory::Execute,
                "manual rollback required; no automatic compensation plan is registered",
            ),
            None => ActionCompensationProfile::read(true),
        },
    }
}

pub fn tool_action_profile_value(tool_name: &str, args: &Value) -> Value {
    serde_json::to_value(tool_action_profile(tool_name, args)).unwrap_or(Value::Null)
}

pub fn compensation_prompt_note(tool_name: &str, args: &Value) -> Option<String> {
    let profile = tool_action_profile(tool_name, args);
    if profile.category == ActionCategory::Read {
        return None;
    }
    if let Some(summary) = profile.compensation_summary {
        return Some(format!("Compensation: {summary}"));
    }
    if profile.reversible {
        Some("Compensation: rollback is available for this action".to_string())
    } else if profile.bounded {
        Some("Compensation: manual rollback required".to_string())
    } else {
        Some("Compensation: unbounded action with no automatic rollback registered".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn write_file_requires_pre_state_capture() {
        let profile = tool_action_profile("write_file", &json!({"path": "src/lib.rs"}));
        assert!(profile.bounded);
        assert_eq!(profile.category, ActionCategory::Write);
        assert!(profile.reversible);
        assert!(profile.requires_pre_state);
        assert_eq!(
            profile.compensation_kind,
            Some(CompensationKind::RestoreOrDeleteFile)
        );
        assert!(
            profile
                .compensation_summary
                .as_deref()
                .unwrap_or_default()
                .contains("src/lib.rs")
        );
    }

    #[test]
    fn create_file_uses_delete_compensation() {
        let profile = tool_action_profile("create_file", &json!({"path": "tmp.txt"}));
        assert_eq!(profile.category, ActionCategory::Write);
        assert!(!profile.requires_pre_state);
        assert_eq!(
            profile.compensation_kind,
            Some(CompensationKind::DeleteFile)
        );
    }

    #[test]
    fn git_commit_has_compensation_summary() {
        let profile = tool_action_profile("bash", &json!({"command": "git commit -m 'x'"}));
        assert!(!profile.bounded);
        assert_eq!(profile.category, ActionCategory::Execute);
        assert!(profile.reversible);
        assert_eq!(
            profile.compensation_kind,
            Some(CompensationKind::GitRevertCommit)
        );
    }

    #[test]
    fn destructive_shell_is_marked_manual() {
        let profile = tool_action_profile("bash", &json!({"command": "rm -rf tmp"}));
        assert_eq!(profile.category, ActionCategory::Destructive);
        assert!(!profile.reversible);
        assert_eq!(profile.compensation_kind, Some(CompensationKind::Manual));
    }

    #[test]
    fn mo_query_write_uses_snapshot_compensation() {
        let profile = tool_action_profile("mo_query", &json!({"sql": "UPDATE t SET x = 1"}));
        assert_eq!(profile.category, ActionCategory::Write);
        assert!(profile.reversible);
        assert!(profile.requires_pre_state);
        assert_eq!(
            profile.compensation_kind,
            Some(CompensationKind::RestoreDatabaseSnapshot)
        );
    }

    #[test]
    fn read_only_tools_do_not_emit_prompt_note() {
        assert!(compensation_prompt_note("read_file", &json!({"path": "README.md"})).is_none());
    }
}
