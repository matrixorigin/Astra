use serde::{Deserialize, Serialize};
use serde_json::Value;

use astra_services::{MutationActionCategory, MutationCompensationPolicy};

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
    GitApplyStash,
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

    pub fn mutation_compensation_policy(&self) -> MutationCompensationPolicy {
        MutationCompensationPolicy {
            bounded: self.bounded,
            reversible: self.reversible,
            requires_pre_state: self.requires_pre_state,
            action_category: mutation_action_category(self.category),
            compensation_kind: self.compensation_kind.map(compensation_kind_label),
            compensation_summary: self.compensation_summary.clone(),
        }
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn mutation_action_category(category: ActionCategory) -> MutationActionCategory {
    match category {
        ActionCategory::Read => MutationActionCategory::Read,
        ActionCategory::Write => MutationActionCategory::Write,
        ActionCategory::Execute => MutationActionCategory::Execute,
        ActionCategory::Destructive => MutationActionCategory::Destructive,
    }
}

fn compensation_kind_label(kind: CompensationKind) -> String {
    match kind {
        CompensationKind::DeleteFile => "delete_file",
        CompensationKind::RestoreFileContents => "restore_file_contents",
        CompensationKind::RestoreOrDeleteFile => "restore_or_delete_file",
        CompensationKind::GitApplyStash => "git_apply_stash",
        CompensationKind::GitRestoreIndex => "git_restore_index",
        CompensationKind::GitRestoreWorktree => "git_restore_worktree",
        CompensationKind::GitRevertCommit => "git_revert_commit",
        CompensationKind::RestoreDatabaseSnapshot => "restore_database_snapshot",
        CompensationKind::Manual => "manual",
    }
    .to_string()
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

fn rollback_file_tool_scope_hint(path: Option<&str>) -> String {
    path.filter(|path| !path.is_empty())
        .map(|path| format!("call `rollback_file_edits` with scope=`file` and path=`{path}`"))
        .unwrap_or_else(|| {
            "call `rollback_file_edits` with scope=`file` and the target path".to_string()
        })
}

fn rollback_turn_tool_scope_hint() -> &'static str {
    "use `rollback_turn_actions` with scope=`current_turn` to revert mixed file/database changes from the same turn"
}

fn restore_file_compensation_summary(path: Option<&str>, delete_if_created: bool) -> String {
    let target = file_target_summary(path);
    if delete_if_created {
        format!(
            "{} to restore prior contents for {} or delete it if this write created the file; alternatively, {}",
            rollback_file_tool_scope_hint(path),
            target,
            rollback_turn_tool_scope_hint()
        )
    } else {
        format!(
            "{} to restore prior contents for {}; alternatively, {}",
            rollback_file_tool_scope_hint(path),
            target,
            rollback_turn_tool_scope_hint()
        )
    }
}

fn delete_created_file_compensation_summary(path: Option<&str>) -> String {
    format!(
        "{} to delete {}",
        rollback_file_tool_scope_hint(path),
        file_target_summary(path)
    )
}

fn restore_deleted_file_compensation_summary(path: Option<&str>) -> String {
    format!(
        "{} to restore deleted contents for {}; alternatively, {}",
        rollback_file_tool_scope_hint(path),
        file_target_summary(path),
        rollback_turn_tool_scope_hint()
    )
}

fn adjust_config_compensation_summary(path: Option<&str>) -> String {
    let target = path
        .filter(|path| !path.is_empty())
        .map(|path| format!("config path `{path}`"))
        .unwrap_or_else(|| "the changed config path".to_string());
    format!(
        "prefer `rollback_session_state` with scope=`current_turn` (or {}) to restore {}; alternatively rerun `adjust_config` with the previous `old` value from the tool result",
        rollback_turn_tool_scope_hint(),
        target
    )
}

fn tool_priority_compensation_summary(tool: Option<&str>) -> String {
    let target = tool
        .filter(|tool| !tool.is_empty())
        .map(|tool| format!("tool `{tool}`"))
        .unwrap_or_else(|| "the affected tool".to_string());
    format!(
        "prefer `rollback_session_state` with scope=`current_turn` (or {}) to restore {}'s prior preference state; the `previous_pinned_tools` and `previous_deprioritized_tools` fields remain the manual fallback",
        rollback_turn_tool_scope_hint(),
        target
    )
}

fn set_goal_compensation_summary() -> &'static str {
    "prefer `rollback_session_state` with scope=`current_turn` (or `rollback_turn_actions`) to restore the previous_goal and goal-tracking snapshot; rerun `set_goal` with the `previous_goal` from the tool result only as the manual fallback"
}

fn compress_context_compensation_summary() -> &'static str {
    "prefer `rollback_session_state` with scope=`current_turn` (or `rollback_turn_actions`) to restore session-local compression state; manual compression journal markers remain append-only if you inspect the persisted journal later"
}

fn task_create_compensation_summary() -> &'static str {
    "prefer `rollback_session_state` with scope=`current_turn` (or `rollback_turn_actions`) to restore the pre-task snapshot; `task_stop` with the returned `task_id` remains the manual fallback if you only want to cancel the created task"
}

fn task_update_compensation_summary() -> &'static str {
    "prefer `rollback_session_state` with scope=`current_turn` (or `rollback_turn_actions`) to restore the pre-update task snapshot; otherwise use `task_get` plus the `previous_status` from the tool result and rerun `task_update` manually"
}

fn task_stop_compensation_summary() -> &'static str {
    "prefer `rollback_session_state` with scope=`current_turn` (or `rollback_turn_actions`) to restore the pre-stop task snapshot; otherwise use `task_update` with the `previous_status` from the tool result to reopen the task manually"
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
            format!(
                "call `rollback_database_snapshots` with scope=`current_turn` during the turn, or scope=`snapshot` with the captured snapshot_id, to restore affected data; alternatively, {}",
                rollback_turn_tool_scope_hint()
            ),
        ),
        Some("DROP" | "DELETE" | "TRUNCATE" | "ALTER" | "GRANT" | "REVOKE") => {
            ActionCompensationProfile::compensated(
                true,
                ActionCategory::Destructive,
                true,
                CompensationKind::RestoreDatabaseSnapshot,
                format!(
                    "call `rollback_database_snapshots` with scope=`current_turn` during the turn, or scope=`snapshot` with the captured snapshot_id, to restore affected objects; alternatively, {}",
                    rollback_turn_tool_scope_hint()
                ),
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
                format!(
                    "call `rollback_database_snapshots` with scope=`current_turn` during the turn, or scope=`snapshot` with the captured snapshot_id, to restore affected objects; alternatively, {}",
                    rollback_turn_tool_scope_hint()
                ),
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
            delete_created_file_compensation_summary(string_arg(&normalized_args, "path")),
        ),
        "delete_file" => ActionCompensationProfile::compensated(
            true,
            ActionCategory::Destructive,
            true,
            CompensationKind::RestoreFileContents,
            restore_deleted_file_compensation_summary(string_arg(&normalized_args, "path")),
        ),
        "write_file" => ActionCompensationProfile::compensated(
            true,
            ActionCategory::Write,
            true,
            CompensationKind::RestoreOrDeleteFile,
            restore_file_compensation_summary(string_arg(&normalized_args, "path"), true),
        ),
        "adjust_config" => ActionCompensationProfile::manual(
            true,
            ActionCategory::Write,
            &adjust_config_compensation_summary(string_arg(&normalized_args, "path")),
        ),
        "prioritize_tool" | "deprioritize_tool" => ActionCompensationProfile::manual(
            true,
            ActionCategory::Write,
            &tool_priority_compensation_summary(string_arg(&normalized_args, "tool")),
        ),
        "set_goal" => ActionCompensationProfile::manual(
            true,
            ActionCategory::Destructive,
            set_goal_compensation_summary(),
        ),
        "compress_context" => ActionCompensationProfile::manual(
            true,
            ActionCategory::Write,
            compress_context_compensation_summary(),
        ),
        "task_create" => ActionCompensationProfile::manual(
            true,
            ActionCategory::Write,
            task_create_compensation_summary(),
        ),
        "task_update" => ActionCompensationProfile::manual(
            true,
            ActionCategory::Write,
            task_update_compensation_summary(),
        ),
        "task_stop" => ActionCompensationProfile::manual(
            true,
            ActionCategory::Destructive,
            task_stop_compensation_summary(),
        ),
        "git_commit" => ActionCompensationProfile::compensated(
            false,
            ActionCategory::Execute,
            false,
            CompensationKind::GitRevertCommit,
            "use `rollback_turn_actions` with scope=`current_turn` during the turn to revert the recorded commit when it is still the current HEAD tail, or call `git_revert_commit` with the returned commit_sha for an explicit compensating revert commit".to_string(),
        ),
        "git_revert_commit" => ActionCompensationProfile::manual(
            false,
            ActionCategory::Execute,
            "git_revert_commit creates a new compensating commit; undo it by reverting the new revert commit if needed",
        ),
        "git_worktree" => match string_arg(&normalized_args, "action")
            .map(|action| action.to_ascii_lowercase())
            .as_deref()
        {
            Some("list" | "ls") => ActionCompensationProfile::read(true),
            Some("enter") => ActionCompensationProfile::compensated(
                true,
                ActionCategory::Execute,
                false,
                CompensationKind::GitRestoreWorktree,
                "use `rollback_turn_actions` with scope=`current_turn` during the turn to remove the recorded worktree and restore the session root while it is still clean; otherwise leave with `git_worktree` action=`exit` or remove it manually".to_string(),
            ),
            Some("add" | "create") => ActionCompensationProfile::compensated(
                true,
                ActionCategory::Execute,
                false,
                CompensationKind::GitRestoreWorktree,
                "use `rollback_turn_actions` with scope=`current_turn` during the turn to remove the recorded clean worktree; if it has since changed, remove it manually with `git_worktree` action=`remove` and the recorded path".to_string(),
            ),
            Some("exit") => {
                let exit_action = string_arg(&normalized_args, "exit_action")
                    .map(|value| value.to_ascii_lowercase());
                if exit_action.as_deref() == Some("remove") {
                    ActionCompensationProfile::manual(
                        false,
                        ActionCategory::Destructive,
                        "git_worktree exit with exit_action=remove can delete the worktree and discard work; use action=`enter` or recreate the worktree manually if you need to return",
                    )
                } else {
                    ActionCompensationProfile::manual(
                        false,
                        ActionCategory::Execute,
                        "git_worktree exit restores the original session root; re-enter the worktree or recreate it manually if you need to return",
                    )
                }
            }
            Some("remove" | "rm" | "delete") => ActionCompensationProfile::manual(
                false,
                ActionCategory::Destructive,
                "git_worktree remove can delete the worktree and optionally its branch; restore it by recreating the worktree or branch manually if needed",
            ),
            _ => ActionCompensationProfile::manual(
                false,
                ActionCategory::Execute,
                "git_worktree action is unknown or not yet modeled for automatic rollback",
            ),
        },
        "git_checkout_file" => ActionCompensationProfile::compensated(
            true,
            ActionCategory::Destructive,
            true,
            CompensationKind::RestoreOrDeleteFile,
            restore_file_compensation_summary(string_arg(&normalized_args, "path"), true),
        ),
        "git_stash" => match string_arg(&normalized_args, "action")
            .map(|action| action.to_ascii_lowercase())
            .as_deref()
        {
            Some("list") => ActionCompensationProfile::read(true),
            Some("push" | "save") => ActionCompensationProfile::compensated(
                true,
                ActionCategory::Execute,
                false,
                CompensationKind::GitApplyStash,
                "use `rollback_turn_actions` with scope=`current_turn` to re-apply the recorded stash for the turn, or re-apply the captured stash with `git_stash` using action=`apply` and the returned stash_ref"
                    .to_string(),
            ),
            Some("apply") => ActionCompensationProfile::manual(
                false,
                ActionCategory::Destructive,
                "git stash apply mutates the working tree; capture a fresh stash or commit first if you may need to undo it",
            ),
            Some("pop" | "drop") => ActionCompensationProfile::manual(
                false,
                ActionCategory::Destructive,
                "git stash pop/drop mutates the stash stack and working tree; no automatic rollback is registered",
            ),
            _ => ActionCompensationProfile::manual(
                false,
                ActionCategory::Execute,
                "git stash action is unknown or not yet modeled for automatic rollback",
            ),
        },
        "notebook_edit" => ActionCompensationProfile::compensated(
            true,
            ActionCategory::Write,
            true,
            CompensationKind::RestoreOrDeleteFile,
            restore_file_compensation_summary(string_arg(&normalized_args, "notebook_path"), true),
        ),
        "edit_file" | "multi_edit" | "str_replace" => ActionCompensationProfile::compensated(
            true,
            ActionCategory::Write,
            true,
            CompensationKind::RestoreFileContents,
            restore_file_compensation_summary(string_arg(&normalized_args, "path"), false),
        ),
        "rename_symbol" => ActionCompensationProfile::compensated(
            true,
            ActionCategory::Write,
            true,
            CompensationKind::RestoreFileContents,
            format!(
                "{} to revert renamed files from the same turn",
                rollback_turn_tool_scope_hint()
            ),
        ),
        "rollback_database_snapshots" => ActionCompensationProfile::manual(
            true,
            ActionCategory::Destructive,
            "database snapshot restore mutates state; capture a fresh snapshot first if you may need to undo the rollback",
        ),
        "rollback_turn_actions" => ActionCompensationProfile::manual(
            true,
            ActionCategory::Destructive,
            "turn rollback can mutate both workspace files and database state; capture fresh recovery points first if you may need to undo the rollback",
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
                .contains("rollback_file_edits")
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
        assert!(
            profile
                .compensation_summary
                .as_deref()
                .unwrap_or_default()
                .contains("rollback_file_edits")
        );
    }

    #[test]
    fn multi_edit_uses_file_restore_compensation() {
        let profile = tool_action_profile("multi_edit", &json!({"path": "src/lib.rs"}));
        assert_eq!(profile.category, ActionCategory::Write);
        assert!(profile.requires_pre_state);
        assert_eq!(
            profile.compensation_kind,
            Some(CompensationKind::RestoreFileContents)
        );
        assert!(
            profile
                .compensation_summary
                .as_deref()
                .unwrap_or_default()
                .contains("rollback_file_edits")
        );
    }

    #[test]
    fn delete_file_uses_file_restore_compensation() {
        let profile = tool_action_profile("delete_file", &json!({"path": "src/lib.rs"}));
        assert!(profile.bounded);
        assert_eq!(profile.category, ActionCategory::Destructive);
        assert!(profile.reversible);
        assert!(profile.requires_pre_state);
        assert_eq!(
            profile.compensation_kind,
            Some(CompensationKind::RestoreFileContents)
        );
        assert!(
            profile
                .compensation_summary
                .as_deref()
                .unwrap_or_default()
                .contains("rollback_file_edits")
        );
    }

    #[test]
    fn rollback_database_snapshots_is_destructive_manual() {
        let profile = tool_action_profile("rollback_database_snapshots", &json!({}));
        assert_eq!(profile.category, ActionCategory::Destructive);
        assert!(!profile.requires_pre_state);
        assert!(!profile.reversible);
        assert_eq!(profile.compensation_kind, Some(CompensationKind::Manual));
    }

    #[test]
    fn rollback_turn_actions_is_destructive_manual() {
        let profile = tool_action_profile("rollback_turn_actions", &json!({}));
        assert_eq!(profile.category, ActionCategory::Destructive);
        assert!(!profile.requires_pre_state);
        assert!(!profile.reversible);
        assert_eq!(profile.compensation_kind, Some(CompensationKind::Manual));
    }

    #[test]
    fn session_state_tools_are_not_treated_as_reads() {
        let adjust = tool_action_profile(
            "adjust_config",
            &json!({"path": "memory.retrieval_top_k", "value": 6}),
        );
        assert!(adjust.bounded);
        assert_eq!(adjust.category, ActionCategory::Write);
        assert!(!adjust.reversible);
        assert_eq!(adjust.compensation_kind, Some(CompensationKind::Manual));
        assert!(
            adjust
                .compensation_summary
                .as_deref()
                .unwrap_or_default()
                .contains("adjust_config")
        );

        let set_goal = tool_action_profile("set_goal", &json!({"goal": "ship rollback shell"}));
        assert!(set_goal.bounded);
        assert_eq!(set_goal.category, ActionCategory::Destructive);
        assert!(!set_goal.reversible);
        assert_eq!(set_goal.compensation_kind, Some(CompensationKind::Manual));
        assert!(
            set_goal
                .compensation_summary
                .as_deref()
                .unwrap_or_default()
                .contains("previous_goal")
        );
    }

    #[test]
    fn task_mutators_are_bounded_manual_actions() {
        let create = tool_action_profile("task_create", &json!({"title": "demo"}));
        assert!(create.bounded);
        assert_eq!(create.category, ActionCategory::Write);
        assert!(!create.reversible);
        assert_eq!(create.compensation_kind, Some(CompensationKind::Manual));
        assert!(
            create
                .compensation_summary
                .as_deref()
                .unwrap_or_default()
                .contains("task_stop")
        );

        let stop = tool_action_profile("task_stop", &json!({"task_id": "task-1"}));
        assert!(stop.bounded);
        assert_eq!(stop.category, ActionCategory::Destructive);
        assert!(!stop.reversible);
        assert_eq!(stop.compensation_kind, Some(CompensationKind::Manual));
        assert!(
            stop.compensation_summary
                .as_deref()
                .unwrap_or_default()
                .contains("previous_status")
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
    fn git_commit_tool_has_compensation_summary() {
        let profile = tool_action_profile("git_commit", &json!({"message": "x"}));
        assert!(!profile.bounded);
        assert_eq!(profile.category, ActionCategory::Execute);
        assert!(profile.reversible);
        assert_eq!(
            profile.compensation_kind,
            Some(CompensationKind::GitRevertCommit)
        );
        assert!(
            profile
                .compensation_summary
                .as_deref()
                .unwrap_or_default()
                .contains("git_revert_commit")
        );
    }

    #[test]
    fn git_revert_commit_tool_is_manual() {
        let profile = tool_action_profile("git_revert_commit", &json!({"commit_sha": "abc123"}));
        assert!(!profile.bounded);
        assert_eq!(profile.category, ActionCategory::Execute);
        assert!(!profile.reversible);
        assert_eq!(profile.compensation_kind, Some(CompensationKind::Manual));
    }

    #[test]
    fn git_worktree_list_is_read_only() {
        let profile = tool_action_profile("git_worktree", &json!({"action": "list"}));
        assert!(profile.bounded);
        assert_eq!(profile.category, ActionCategory::Read);
        assert_eq!(profile.compensation_kind, None);
    }

    #[test]
    fn git_worktree_enter_is_compensated() {
        let profile = tool_action_profile(
            "git_worktree",
            &json!({"action": "enter", "branch": "demo"}),
        );
        assert!(profile.bounded);
        assert_eq!(profile.category, ActionCategory::Execute);
        assert!(profile.reversible);
        assert_eq!(
            profile.compensation_kind,
            Some(CompensationKind::GitRestoreWorktree)
        );
        assert!(
            profile
                .compensation_summary
                .as_deref()
                .unwrap_or_default()
                .contains("rollback_turn_actions")
        );
    }

    #[test]
    fn git_worktree_add_is_compensated() {
        let profile =
            tool_action_profile("git_worktree", &json!({"action": "add", "branch": "demo"}));
        assert!(profile.bounded);
        assert_eq!(profile.category, ActionCategory::Execute);
        assert!(profile.reversible);
        assert_eq!(
            profile.compensation_kind,
            Some(CompensationKind::GitRestoreWorktree)
        );
    }

    #[test]
    fn git_stash_push_has_compensation_summary() {
        let profile = tool_action_profile("git_stash", &json!({"action": "push"}));
        assert!(profile.bounded);
        assert_eq!(profile.category, ActionCategory::Execute);
        assert!(profile.reversible);
        assert_eq!(
            profile.compensation_kind,
            Some(CompensationKind::GitApplyStash)
        );
        assert!(
            profile
                .compensation_summary
                .as_deref()
                .unwrap_or_default()
                .contains("stash_ref")
        );
    }

    #[test]
    fn git_checkout_file_uses_bounded_file_rollback() {
        let profile = tool_action_profile("git_checkout_file", &json!({"path": "src/lib.rs"}));
        assert!(profile.bounded);
        assert_eq!(profile.category, ActionCategory::Destructive);
        assert!(profile.reversible);
        assert_eq!(
            profile.compensation_kind,
            Some(CompensationKind::RestoreOrDeleteFile)
        );
        assert!(
            profile
                .compensation_summary
                .as_deref()
                .unwrap_or_default()
                .contains("rollback_file_edits")
        );
    }

    #[test]
    fn rename_symbol_uses_turn_rollback_hint() {
        let profile = tool_action_profile(
            "rename_symbol",
            &json!({"symbol": "old_name", "new_name": "new_name"}),
        );
        assert!(profile.bounded);
        assert_eq!(profile.category, ActionCategory::Write);
        assert!(profile.reversible);
        assert_eq!(
            profile.compensation_kind,
            Some(CompensationKind::RestoreFileContents)
        );
        assert!(
            profile
                .compensation_summary
                .as_deref()
                .unwrap_or_default()
                .contains("rollback_turn_actions")
        );
    }

    #[test]
    fn notebook_edit_uses_file_rollback_hint() {
        let profile = tool_action_profile(
            "notebook_edit",
            &json!({"notebook_path": "analysis.ipynb", "edit_mode": "replace"}),
        );
        assert!(profile.bounded);
        assert_eq!(profile.category, ActionCategory::Write);
        assert!(profile.reversible);
        assert_eq!(
            profile.compensation_kind,
            Some(CompensationKind::RestoreOrDeleteFile)
        );
        assert!(
            profile
                .compensation_summary
                .as_deref()
                .unwrap_or_default()
                .contains("rollback_file_edits")
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

    #[test]
    fn profile_bridges_to_mutation_compensation_policy() {
        let policy = tool_action_profile("write_file", &json!({"path": "src/lib.rs"}))
            .mutation_compensation_policy();
        assert!(policy.bounded);
        assert!(policy.reversible);
        assert!(policy.requires_pre_state);
        assert_eq!(policy.action_category, MutationActionCategory::Write);
        assert_eq!(
            policy.compensation_kind.as_deref(),
            Some("restore_or_delete_file")
        );
    }
}
