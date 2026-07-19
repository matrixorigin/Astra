use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::cloud::approval_policy::{
    CloudGatedToolKind, bash_command_is_read_only, cloud_gated_tool_kind_with_args,
};
use astra_sandbox::{CommandRisk, analyze_command_risks};

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
    RestoreSessionState,
    Manual,
}

// ── Execution outcome classification ──

/// High-level execution outcome for a tool invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionOutcome {
    Success,
    Failure,
    Timeout,
    Rejected,
    ResourceLimit,
}

/// Structured failure category derived from tool result content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCategory {
    CompileError,
    TestFailure,
    PermissionDenied,
    ResourceNotFound,
    NetworkError,
    SyntaxError,
    RuntimeError,
    Timeout,
    ResourceExhaustion,
    ValidationError,
    NonProgress,
    Unknown,
}

/// Typed execution outcome classification attached to an action profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionOutcomeClassification {
    pub outcome: ExecutionOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_category: Option<FailureCategory>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_snippet: Option<String>,
}

/// Structured execution-result facts. New production call sites should pass
/// typed fields here instead of asking this module to infer control flow from
/// human-readable tool output.
#[derive(Debug, Clone, Copy)]
pub struct ExecutionOutcomeInput<'a> {
    pub result_text: &'a str,
    pub is_error: bool,
    pub duration_ms: u64,
    pub was_rejected: bool,
    pub error_kind: Option<astra_core::ErrorKind>,
    pub result_class: Option<&'a str>,
    pub exit_semantics: Option<&'a str>,
}

/// Classify a tool result into a typed execution outcome.
///
/// Legacy convenience wrapper. Prefer [`classify_execution_outcome_from_input`]
/// so callers can provide typed `error_kind`, `result_class`, or
/// `exit_semantics` instead of relying on output prose.
pub fn classify_execution_outcome(
    result_text: &str,
    is_error: bool,
    duration_ms: u64,
    was_rejected: bool,
) -> ExecutionOutcomeClassification {
    classify_execution_outcome_from_input(ExecutionOutcomeInput {
        result_text,
        is_error,
        duration_ms,
        was_rejected,
        error_kind: None,
        result_class: None,
        exit_semantics: None,
    })
}

/// Classify a tool result from structured facts first, falling back to text only
/// for legacy callers that have not yet been migrated.
pub fn classify_execution_outcome_from_input(
    input: ExecutionOutcomeInput<'_>,
) -> ExecutionOutcomeClassification {
    if input.was_rejected {
        return ExecutionOutcomeClassification {
            outcome: ExecutionOutcome::Rejected,
            failure_category: None,
            error_snippet: None,
        };
    }

    if let Some(kind) = input.error_kind {
        if let Some(classification) = classify_error_kind_outcome(kind, input.result_text) {
            return classification;
        }
    }

    if let Some(classification) = input
        .result_class
        .and_then(|class| classify_result_class_outcome(class, input.result_text))
    {
        return classification;
    }

    if let Some(classification) = input
        .exit_semantics
        .and_then(|semantics| classify_exit_semantics_outcome(semantics, input.result_text))
    {
        return classification;
    }

    if !input.is_error {
        return ExecutionOutcomeClassification {
            outcome: ExecutionOutcome::Success,
            failure_category: None,
            error_snippet: None,
        };
    }
    if input.duration_ms > 120_000 {
        return ExecutionOutcomeClassification {
            outcome: ExecutionOutcome::Timeout,
            failure_category: Some(FailureCategory::Timeout),
            error_snippet: non_empty_snippet(input.result_text, 200),
        };
    }

    ExecutionOutcomeClassification {
        outcome: ExecutionOutcome::Failure,
        failure_category: Some(FailureCategory::Unknown),
        error_snippet: non_empty_snippet(input.result_text, 200),
    }
}

fn classify_error_kind_outcome(
    kind: astra_core::ErrorKind,
    result_text: &str,
) -> Option<ExecutionOutcomeClassification> {
    use astra_core::ErrorKind;

    let (outcome, failure_category) = match kind {
        ErrorKind::ToolTimeout => (ExecutionOutcome::Timeout, Some(FailureCategory::Timeout)),
        ErrorKind::ResourceLimit => (
            ExecutionOutcome::ResourceLimit,
            Some(FailureCategory::ResourceExhaustion),
        ),
        ErrorKind::Auth => (
            ExecutionOutcome::Failure,
            Some(FailureCategory::PermissionDenied),
        ),
        ErrorKind::ToolNotFound => (
            ExecutionOutcome::Failure,
            Some(FailureCategory::ResourceNotFound),
        ),
        ErrorKind::Network
        | ErrorKind::RateLimit
        | ErrorKind::ServerError
        | ErrorKind::StreamIdle
        | ErrorKind::StreamTransport
        | ErrorKind::ConnectionPoolExhausted => (
            ExecutionOutcome::Failure,
            Some(FailureCategory::NetworkError),
        ),
        ErrorKind::ContextWindow | ErrorKind::BudgetExhausted | ErrorKind::ToolRoundsExhausted => (
            ExecutionOutcome::ResourceLimit,
            Some(FailureCategory::ResourceExhaustion),
        ),
        ErrorKind::ToolInvalidArgs | ErrorKind::InvalidRequest => (
            ExecutionOutcome::Failure,
            Some(FailureCategory::ValidationError),
        ),
        ErrorKind::ContractViolation | ErrorKind::DatabaseError => (
            ExecutionOutcome::Failure,
            Some(FailureCategory::RuntimeError),
        ),
        ErrorKind::Stall => (
            ExecutionOutcome::Failure,
            Some(FailureCategory::NonProgress),
        ),
        ErrorKind::Cancelled => (ExecutionOutcome::Failure, Some(FailureCategory::Unknown)),
        ErrorKind::MissingModelSelection | ErrorKind::ToolUnavailable | ErrorKind::ToolBinding => {
            (ExecutionOutcome::Failure, Some(FailureCategory::Unknown))
        }
        ErrorKind::Unknown => return None,
    };

    Some(ExecutionOutcomeClassification {
        outcome,
        failure_category,
        error_snippet: failure_category.and_then(|_| non_empty_snippet(result_text, 200)),
    })
}

fn classify_result_class_outcome(
    result_class: &str,
    result_text: &str,
) -> Option<ExecutionOutcomeClassification> {
    let (outcome, failure_category) = match result_class {
        "success" => (ExecutionOutcome::Success, None),
        "timeout" | "timed_out" => (ExecutionOutcome::Timeout, Some(FailureCategory::Timeout)),
        "resource_limit" => (
            ExecutionOutcome::ResourceLimit,
            Some(FailureCategory::ResourceExhaustion),
        ),
        "compile_error" => (
            ExecutionOutcome::Failure,
            Some(FailureCategory::CompileError),
        ),
        "test_failure" => (
            ExecutionOutcome::Failure,
            Some(FailureCategory::TestFailure),
        ),
        "permission_denied" => (
            ExecutionOutcome::Failure,
            Some(FailureCategory::PermissionDenied),
        ),
        "resource_not_found" => (
            ExecutionOutcome::Failure,
            Some(FailureCategory::ResourceNotFound),
        ),
        "network_error" => (
            ExecutionOutcome::Failure,
            Some(FailureCategory::NetworkError),
        ),
        "syntax_error" => (
            ExecutionOutcome::Failure,
            Some(FailureCategory::SyntaxError),
        ),
        "runtime_error" | "execution_error" => (
            ExecutionOutcome::Failure,
            Some(FailureCategory::RuntimeError),
        ),
        "validation_error" => (
            ExecutionOutcome::Failure,
            Some(FailureCategory::ValidationError),
        ),
        "non_progress" => (
            ExecutionOutcome::Failure,
            Some(FailureCategory::NonProgress),
        ),
        _ => return None,
    };

    Some(ExecutionOutcomeClassification {
        outcome,
        failure_category,
        error_snippet: failure_category.and_then(|_| non_empty_snippet(result_text, 200)),
    })
}

fn classify_exit_semantics_outcome(
    exit_semantics: &str,
    result_text: &str,
) -> Option<ExecutionOutcomeClassification> {
    let (outcome, failure_category) = match exit_semantics {
        "success" => (ExecutionOutcome::Success, None),
        "timeout" | "timed_out" => (ExecutionOutcome::Timeout, Some(FailureCategory::Timeout)),
        "resource_limit" => (
            ExecutionOutcome::ResourceLimit,
            Some(FailureCategory::ResourceExhaustion),
        ),
        "rejected" => (ExecutionOutcome::Rejected, None),
        "failure" => (ExecutionOutcome::Failure, Some(FailureCategory::Unknown)),
        _ => return None,
    };

    Some(ExecutionOutcomeClassification {
        outcome,
        failure_category,
        error_snippet: failure_category.and_then(|_| non_empty_snippet(result_text, 200)),
    })
}

fn non_empty_snippet(text: &str, max: usize) -> Option<String> {
    if text.is_empty() {
        None
    } else {
        Some(truncate_snippet(text, max))
    }
}

fn truncate_snippet(text: &str, max: usize) -> String {
    if text.len() <= max {
        text.to_string()
    } else {
        let truncated = &text[..text.floor_char_boundary(max)];
        format!("{truncated}…")
    }
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

fn rollback_file_tool_scope_hint(path: Option<&str>) -> String {
    path.filter(|path| !path.is_empty())
        .map(|path| format!("call `rollback_file_edits` with scope=`file` and path=`{path}`"))
        .unwrap_or_else(|| {
            "call `rollback_file_edits` with scope=`file` and the target path".to_string()
        })
}

fn rollback_file_current_turn_scope_hint() -> &'static str {
    "call `rollback_file_edits` with scope=`current_turn` to restore recorded file edits from this turn"
}

fn restore_file_compensation_summary(path: Option<&str>, delete_if_created: bool) -> String {
    let target = file_target_summary(path);
    if delete_if_created {
        format!(
            "{} to restore prior contents for {} or delete it if this write created the file; alternatively, {}",
            rollback_file_tool_scope_hint(path),
            target,
            rollback_file_current_turn_scope_hint()
        )
    } else {
        format!(
            "{} to restore prior contents for {}; alternatively, {}",
            rollback_file_tool_scope_hint(path),
            target,
            rollback_file_current_turn_scope_hint()
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
        rollback_file_current_turn_scope_hint()
    )
}

fn adjust_config_compensation_summary(path: Option<&str>) -> String {
    let target = path
        .filter(|path| !path.is_empty())
        .map(|path| format!("config path `{path}`"))
        .unwrap_or_else(|| "the changed config path".to_string());
    format!(
        "prefer `rollback_session_state` with scope=`current_turn` to restore {}; alternatively rerun `adjust_config` with the previous `old` value from the tool result",
        target
    )
}

fn compress_context_compensation_summary() -> &'static str {
    "prefer `rollback_session_state` with scope=`current_turn` to restore session-local compression state; manual compression journal markers remain append-only if you inspect the persisted journal later"
}

fn task_action_create_compensation_summary() -> &'static str {
    "prefer `rollback_session_state` with scope=`current_turn` to restore the pre-task snapshot; `task_board(action='stop')` with the returned `task_id` remains the manual fallback if you only want to cancel the created task"
}

fn task_action_update_compensation_summary() -> &'static str {
    "prefer `rollback_session_state` with scope=`current_turn` to restore the pre-update task snapshot; otherwise use `task_board(action='get')` plus the `previous_status` from the tool result and rerun `task_board(action='update', task_id='...', new_status='<previous_status>')` manually"
}

fn task_action_stop_compensation_summary() -> &'static str {
    "prefer `rollback_session_state` with scope=`current_turn` to restore the pre-stop task snapshot; otherwise use `task_board(action='update', task_id='...', new_status='<previous_status>')` with the `previous_status` from the tool result to reopen the task manually"
}

fn task_action_profile(args: &Value) -> ActionCompensationProfile {
    match string_arg(args, "action")
        .unwrap_or("list")
        .to_ascii_lowercase()
        .as_str()
    {
        "create" => session_state_action_profile(
            ActionCategory::Write,
            task_action_create_compensation_summary(),
        ),
        "update" => {
            let category = match string_arg(args, "new_status") {
                Some("deleted") => ActionCategory::Destructive,
                _ => ActionCategory::Write,
            };
            session_state_action_profile(category, task_action_update_compensation_summary())
        }
        "stop" => session_state_action_profile(
            ActionCategory::Destructive,
            task_action_stop_compensation_summary(),
        ),
        "archive" => session_state_action_profile(
            ActionCategory::Write,
            "prefer `rollback_session_state` with scope=`current_turn` to restore the pre-archive task snapshot",
        ),
        "list" | "get" | "list_user" => ActionCompensationProfile::read(true),
        _ => ActionCompensationProfile::read(true),
    }
}

fn session_state_action_profile(
    category: ActionCategory,
    compensation_summary: impl Into<String>,
) -> ActionCompensationProfile {
    ActionCompensationProfile::compensated(
        true,
        category,
        false,
        CompensationKind::RestoreSessionState,
        compensation_summary.into(),
    )
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
    let destructive = shell_invokes_destructive_command(command)
        || risks.iter().any(|risk| {
            matches!(
                risk,
                CommandRisk::PathTraversal
                    | CommandRisk::ProcessControl
                    | CommandRisk::PrivilegeEscalation
                    | CommandRisk::RemoteCodeExecution
                    | CommandRisk::OutputRedirection
                    | CommandRisk::DestructiveCommand(_)
                    | CommandRisk::CredentialAccess(_)
                    | CommandRisk::WorkspaceOutWrite(_)
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

fn shell_invokes_destructive_command(command: &str) -> bool {
    let mut token = String::new();
    let mut at_command_start = true;

    for ch in command.chars().chain(std::iter::once(' ')) {
        if ch.is_ascii_whitespace() || matches!(ch, ';' | '&' | '|' | '\n' | '(' | ')') {
            if shell_token_is_destructive_command(&token, &mut at_command_start) {
                return true;
            }
            token.clear();
            if matches!(ch, ';' | '&' | '|' | '\n' | '(' | ')') {
                at_command_start = true;
            }
        } else {
            token.push(ch);
        }
    }

    false
}

fn shell_token_is_destructive_command(token: &str, at_command_start: &mut bool) -> bool {
    if token.is_empty() || !*at_command_start {
        return false;
    }

    let normalized = token.trim_matches(|ch: char| matches!(ch, '"' | '\''));
    if normalized.is_empty() {
        return false;
    }

    if matches!(
        normalized,
        "sudo" | "doas" | "command" | "builtin" | "time" | "env"
    ) || normalized.split_once('=').is_some()
    {
        return false;
    }

    *at_command_start = false;
    matches!(normalized, "rm" | "rmdir" | "unlink" | "shred")
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
            "call `rollback_database_snapshots` with scope=`current_turn` during the turn, or scope=`snapshot` with the captured snapshot_id, to restore affected data"
                .to_string(),
        ),
        Some("DROP" | "DELETE" | "TRUNCATE" | "ALTER" | "GRANT" | "REVOKE") => {
            ActionCompensationProfile::compensated(
                true,
                ActionCategory::Destructive,
                true,
                CompensationKind::RestoreDatabaseSnapshot,
                "call `rollback_database_snapshots` with scope=`current_turn` during the turn, or scope=`snapshot` with the captured snapshot_id, to restore affected objects"
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
                "call `rollback_database_snapshots` with scope=`current_turn` during the turn, or scope=`snapshot` with the captured snapshot_id, to restore affected objects"
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
        "adjust_config" => session_state_action_profile(
            ActionCategory::Write,
            adjust_config_compensation_summary(string_arg(&normalized_args, "path")),
        ),
        "compress_context" => session_state_action_profile(
            ActionCategory::Write,
            compress_context_compensation_summary(),
        ),
        "task_board" => task_action_profile(&normalized_args),
        "git" => match string_arg(&normalized_args, "action")
            .map(|action| action.to_ascii_lowercase())
            .as_deref()
        {
            Some(
                "status" | "diff" | "log" | "show" | "blame" | "file_history" | "log_search"
                | "contributors",
            ) => ActionCompensationProfile::read(true),
            Some("commit") => ActionCompensationProfile::compensated(
                false,
                ActionCategory::Execute,
                false,
                CompensationKind::GitRevertCommit,
                "call `git` with action=`revert_commit` and the returned commit_sha to create an explicit compensating revert commit".to_string(),
            ),
            Some("revert_commit") => ActionCompensationProfile::manual(
                false,
                ActionCategory::Execute,
                "git revert_commit creates a new compensating commit; undo it by reverting the new revert commit if needed",
            ),
            Some("checkout_file") => ActionCompensationProfile::compensated(
                true,
                ActionCategory::Destructive,
                true,
                CompensationKind::RestoreOrDeleteFile,
                restore_file_compensation_summary(string_arg(&normalized_args, "path"), true),
            ),
            Some("stash") => match string_arg(&normalized_args, "sub_action")
                .map(|action| action.to_ascii_lowercase())
                .as_deref()
            {
                Some("list") => ActionCompensationProfile::read(true),
                Some("push" | "save") => ActionCompensationProfile::compensated(
                    true,
                    ActionCategory::Execute,
                    false,
                    CompensationKind::GitApplyStash,
                    "re-apply the captured stash with `git` using action=`stash`, sub_action=`apply`, and the returned stash_ref"
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
            Some("worktree") => match string_arg(&normalized_args, "sub_action")
                .map(|action| action.to_ascii_lowercase())
                .as_deref()
            {
                Some("list" | "ls") => ActionCompensationProfile::read(true),
                Some("enter") => ActionCompensationProfile::compensated(
                    true,
                    ActionCategory::Execute,
                    false,
                    CompensationKind::GitRestoreWorktree,
                    "leave the worktree with `git` action=`worktree`, sub_action=`exit`; remove the recorded worktree path manually only after confirming it is clean".to_string(),
                ),
                Some("add" | "create") => ActionCompensationProfile::compensated(
                    true,
                    ActionCategory::Execute,
                    false,
                    CompensationKind::GitRestoreWorktree,
                    "remove the recorded clean worktree with `git` action=`worktree`, sub_action=`remove` and the recorded path; if it has changed, inspect it before removal".to_string(),
                ),
                Some("exit") => ActionCompensationProfile::manual(
                    false,
                    ActionCategory::Execute,
                    "git worktree exit restores the original session root; re-enter the worktree or recreate it manually if you need to return",
                ),
                Some("remove" | "rm" | "delete") => ActionCompensationProfile::manual(
                    false,
                    ActionCategory::Destructive,
                    "git worktree remove can delete the worktree and optionally its branch; restore it by recreating the worktree or branch manually if needed",
                ),
                _ => ActionCompensationProfile::manual(
                    false,
                    ActionCategory::Execute,
                    "git worktree action is unknown or not yet modeled for automatic rollback",
                ),
            },
            Some("push") => ActionCompensationProfile::manual(
                false,
                ActionCategory::Execute,
                "git push mutates remote refs; coordinate with the remote branch owner or push a corrective commit/ref update if it must be undone",
            ),
            _ => ActionCompensationProfile::manual(
                false,
                ActionCategory::Execute,
                "git action is unknown or not yet modeled for automatic rollback",
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
                rollback_file_current_turn_scope_hint()
            ),
        ),
        "rollback_database_snapshots" => ActionCompensationProfile::manual(
            true,
            ActionCategory::Destructive,
            "database snapshot restore mutates state; capture a fresh snapshot first if you may need to undo the rollback",
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
        _ => match cloud_gated_tool_kind_with_args(tool_name, Some(&normalized_args)) {
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

fn profile_requires_explicit_approval(
    tool_name: &str,
    args: Option<&Value>,
    profile: &ActionCompensationProfile,
) -> bool {
    if profile.category == ActionCategory::Read {
        return false;
    }
    if !profile.bounded {
        return true;
    }
    !profile.reversible && cloud_gated_tool_kind_with_args(tool_name, args).is_some()
}

pub fn tool_requires_explicit_approval(tool_name: &str, args: &Value) -> bool {
    let profile = tool_action_profile(tool_name, args);
    profile_requires_explicit_approval(tool_name, Some(args), &profile)
}

pub fn explicit_approval_reason(tool_name: &str, args: &Value) -> Option<String> {
    let profile = tool_action_profile(tool_name, args);
    if !profile_requires_explicit_approval(tool_name, Some(args), &profile) {
        return None;
    }
    let reason = match (profile.bounded, profile.reversible) {
        (false, false) => {
            "Explicit approval required: action scope is unbounded and rollback is not automatic."
        }
        (false, true) => "Explicit approval required: action scope is unbounded.",
        (true, false) => {
            "Explicit approval required: no automatic rollback is registered for this write/execute action."
        }
        (true, true) => return None,
    };
    Some(reason.to_string())
}

pub fn primary_approval_reason(tool_name: &str, args: &Value) -> Option<String> {
    let profile = tool_action_profile(tool_name, args);
    if !profile_requires_explicit_approval(tool_name, Some(args), &profile) {
        return None;
    }

    let reason = match cloud_gated_tool_kind_with_args(tool_name, Some(args)) {
        Some(CloudGatedToolKind::Execute) if profile.category == ActionCategory::Destructive => {
            "This command can make destructive changes, so it needs your approval."
        }
        Some(CloudGatedToolKind::Execute) => {
            "This command runs in your shell and can change files or system state."
        }
        Some(CloudGatedToolKind::Write) => {
            "This action can change files or project state, so it needs your approval."
        }
        None => "This action can change project state, so it needs your approval.",
    };
    Some(reason.to_string())
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
    fn file_write_and_delete_compensation() {
        // write_file: requires pre-state capture, RestoreOrDeleteFile
        let p = tool_action_profile("write_file", &json!({"path": "src/lib.rs"}));
        assert!(p.bounded);
        assert_eq!(p.category, ActionCategory::Write);
        assert!(p.reversible);
        assert!(p.requires_pre_state);
        assert_eq!(
            p.compensation_kind,
            Some(CompensationKind::RestoreOrDeleteFile)
        );
        assert!(
            p.compensation_summary
                .as_deref()
                .unwrap_or_default()
                .contains("rollback_file_edits")
        );

        // create_file: DeleteFile (no pre-state needed)
        let p = tool_action_profile("create_file", &json!({"path": "tmp.txt"}));
        assert_eq!(p.category, ActionCategory::Write);
        assert!(!p.requires_pre_state);
        assert_eq!(p.compensation_kind, Some(CompensationKind::DeleteFile));
        assert!(
            p.compensation_summary
                .as_deref()
                .unwrap_or_default()
                .contains("rollback_file_edits")
        );

        // multi_edit: RestoreFileContents
        let p = tool_action_profile("multi_edit", &json!({"path": "src/lib.rs"}));
        assert_eq!(p.category, ActionCategory::Write);
        assert!(p.requires_pre_state);
        assert_eq!(
            p.compensation_kind,
            Some(CompensationKind::RestoreFileContents)
        );
        assert!(
            p.compensation_summary
                .as_deref()
                .unwrap_or_default()
                .contains("rollback_file_edits")
        );

        // delete_file: destructive, RestoreFileContents
        let p = tool_action_profile("delete_file", &json!({"path": "src/lib.rs"}));
        assert!(p.bounded);
        assert_eq!(p.category, ActionCategory::Destructive);
        assert!(p.reversible);
        assert!(p.requires_pre_state);
        assert_eq!(
            p.compensation_kind,
            Some(CompensationKind::RestoreFileContents)
        );
        assert!(
            p.compensation_summary
                .as_deref()
                .unwrap_or_default()
                .contains("rollback_file_edits")
        );
    }

    #[test]
    fn destructive_manual_and_shell_compensation() {
        // rollback_database_snapshots
        let p = tool_action_profile("rollback_database_snapshots", &json!({}));
        assert_eq!(p.category, ActionCategory::Destructive);
        assert!(!p.requires_pre_state);
        assert!(!p.reversible);
        assert_eq!(p.compensation_kind, Some(CompensationKind::Manual));

        // destructive shell (rm -rf, force push, etc.)
        let p = tool_action_profile("bash", &json!({"command": "rm -rf /tmp/test"}));
        assert_eq!(p.category, ActionCategory::Destructive);
        assert!(!p.reversible);
        assert_eq!(p.compensation_kind, Some(CompensationKind::Manual));
    }

    #[test]
    fn shell_destructive_detection_uses_command_tokens_not_substrings() {
        let echo = tool_action_profile("bash", &json!({"command": "echo rm -rf /tmp/test"}));
        assert_ne!(echo.category, ActionCategory::Destructive);

        let chained = tool_action_profile("bash", &json!({"command": "cd /tmp && rm -rf test"}));
        assert_eq!(chained.category, ActionCategory::Destructive);
    }

    #[test]
    fn session_state_tools_use_session_rollback_compensation() {
        let adjust = tool_action_profile(
            "adjust_config",
            &json!({"path": "memory.retrieval_top_k", "value": 6}),
        );
        assert!(adjust.bounded);
        assert_eq!(adjust.category, ActionCategory::Write);
        assert!(adjust.reversible);
        assert_eq!(
            adjust.compensation_kind,
            Some(CompensationKind::RestoreSessionState)
        );
        assert!(
            adjust
                .compensation_summary
                .as_deref()
                .unwrap_or_default()
                .contains("rollback_session_state")
        );

        let compress = tool_action_profile("compress_context", &json!({"turns": 4}));
        assert!(compress.bounded);
        assert_eq!(compress.category, ActionCategory::Write);
        assert!(compress.reversible);
        assert_eq!(
            compress.compensation_kind,
            Some(CompensationKind::RestoreSessionState)
        );
        assert!(
            compress
                .compensation_summary
                .as_deref()
                .unwrap_or_default()
                .contains("session-local compression state")
        );
    }

    #[test]
    fn task_mutators_use_session_rollback_compensation() {
        let create =
            tool_action_profile("task_board", &json!({"action": "create", "title": "demo"}));
        assert!(create.bounded);
        assert_eq!(create.category, ActionCategory::Write);
        assert!(create.reversible);
        assert_eq!(
            create.compensation_kind,
            Some(CompensationKind::RestoreSessionState)
        );
        assert!(
            create
                .compensation_summary
                .as_deref()
                .unwrap_or_default()
                .contains("rollback_session_state")
        );

        let update = tool_action_profile(
            "task_board",
            &json!({"action": "update", "task_id": "task-1", "new_status": "completed"}),
        );
        assert!(update.bounded);
        assert_eq!(update.category, ActionCategory::Write);
        assert!(update.reversible);
        assert_eq!(
            update.compensation_kind,
            Some(CompensationKind::RestoreSessionState)
        );
        assert!(
            update
                .compensation_summary
                .as_deref()
                .unwrap_or_default()
                .contains("pre-update task snapshot")
        );
        assert!(
            update
                .compensation_summary
                .as_deref()
                .unwrap_or_default()
                .contains("new_status='<previous_status>'")
        );

        let stop = tool_action_profile(
            "task_board",
            &json!({"action": "stop", "task_id": "task-1"}),
        );
        assert!(stop.bounded);
        assert_eq!(stop.category, ActionCategory::Destructive);
        assert!(stop.reversible);
        assert_eq!(
            stop.compensation_kind,
            Some(CompensationKind::RestoreSessionState)
        );
        assert!(
            stop.compensation_summary
                .as_deref()
                .unwrap_or_default()
                .contains("previous_status")
        );
        assert!(
            stop.compensation_summary
                .as_deref()
                .unwrap_or_default()
                .contains("new_status='<previous_status>'")
        );
    }

    #[test]
    fn task_read_actions_do_not_require_session_rollback_compensation() {
        for args in [
            json!({"action": "list"}),
            json!({"action": "get", "task_id": "task-1"}),
        ] {
            let profile = tool_action_profile("task_board", &args);
            assert_eq!(profile.category, ActionCategory::Read);
            assert!(profile.compensation_kind.is_none());
        }
    }

    #[test]
    fn unknown_tool_names_are_not_special_compensation_surfaces() {
        let profile = tool_action_profile("unknown_task_surface", &json!({"title": "demo"}));
        assert_eq!(profile.category, ActionCategory::Read);
        assert!(profile.compensation_kind.is_none());
    }

    // ── git compensation ──

    #[test]
    fn git_action_commit_compensation() {
        // bash git commit
        let p = tool_action_profile("bash", &json!({"command": "git commit -m 'x'"}));
        assert!(!p.bounded);
        assert_eq!(p.category, ActionCategory::Execute);
        assert!(p.reversible);
        assert_eq!(p.compensation_kind, Some(CompensationKind::GitRevertCommit));

        // consolidated git commit action
        let p = tool_action_profile("git", &json!({"action": "commit", "message": "x"}));
        assert!(!p.bounded);
        assert_eq!(p.category, ActionCategory::Execute);
        assert!(p.reversible);
        assert_eq!(p.compensation_kind, Some(CompensationKind::GitRevertCommit));
        assert!(
            p.compensation_summary
                .as_deref()
                .unwrap_or_default()
                .contains("action=`revert_commit`")
        );
    }

    #[test]
    fn git_action_worktree_compensation() {
        // list: read-only
        let p = tool_action_profile("git", &json!({"action": "worktree", "sub_action": "list"}));
        assert!(p.bounded);
        assert_eq!(p.category, ActionCategory::Read);
        assert_eq!(p.compensation_kind, None);

        // enter: reversible via GitRestoreWorktree
        let p = tool_action_profile(
            "git",
            &json!({"action": "worktree", "sub_action": "enter", "branch": "demo"}),
        );
        assert!(p.bounded);
        assert_eq!(p.category, ActionCategory::Execute);
        assert!(p.reversible);
        assert_eq!(
            p.compensation_kind,
            Some(CompensationKind::GitRestoreWorktree)
        );
        assert!(
            p.compensation_summary
                .as_deref()
                .unwrap_or_default()
                .contains("action=`worktree`")
        );

        // add: same compensation
        let p = tool_action_profile(
            "git",
            &json!({"action": "worktree", "sub_action": "add", "branch": "demo"}),
        );
        assert!(p.bounded);
        assert_eq!(p.category, ActionCategory::Execute);
        assert!(p.reversible);
        assert_eq!(
            p.compensation_kind,
            Some(CompensationKind::GitRestoreWorktree)
        );
    }

    #[test]
    fn git_irreversible_and_file_compensation() {
        // revert commit: manual (irreversible)
        let p = tool_action_profile(
            "git",
            &json!({"action": "revert_commit", "commit_sha": "abc123"}),
        );
        assert!(!p.bounded);
        assert_eq!(p.category, ActionCategory::Execute);
        assert!(!p.reversible);
        assert_eq!(p.compensation_kind, Some(CompensationKind::Manual));

        // stash push: reversible via GitApplyStash
        let p = tool_action_profile("git", &json!({"action": "stash", "sub_action": "push"}));
        assert!(p.bounded);
        assert_eq!(p.category, ActionCategory::Execute);
        assert!(p.reversible);
        assert_eq!(p.compensation_kind, Some(CompensationKind::GitApplyStash));
        assert!(
            p.compensation_summary
                .as_deref()
                .unwrap_or_default()
                .contains("stash_ref")
        );

        // checkout file: destructive but bounded + reversible
        let p = tool_action_profile(
            "git",
            &json!({"action": "checkout_file", "path": "src/lib.rs"}),
        );
        assert!(p.bounded);
        assert_eq!(p.category, ActionCategory::Destructive);
        assert!(p.reversible);
        assert_eq!(
            p.compensation_kind,
            Some(CompensationKind::RestoreOrDeleteFile)
        );
    }

    #[test]
    fn git_action_commit_has_compensation_summary() {
        let profile = tool_action_profile("git", &json!({"action": "commit", "message": "x"}));
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
                .contains("action=`revert_commit`")
        );
    }

    #[test]
    fn git_action_revert_commit_is_manual() {
        let profile = tool_action_profile(
            "git",
            &json!({"action": "revert_commit", "commit_sha": "abc123"}),
        );
        assert!(!profile.bounded);
        assert_eq!(profile.category, ActionCategory::Execute);
        assert!(!profile.reversible);
        assert_eq!(profile.compensation_kind, Some(CompensationKind::Manual));
    }

    #[test]
    fn git_action_worktree_list_is_read_only() {
        let profile =
            tool_action_profile("git", &json!({"action": "worktree", "sub_action": "list"}));
        assert!(profile.bounded);
        assert_eq!(profile.category, ActionCategory::Read);
        assert_eq!(profile.compensation_kind, None);
    }

    #[test]
    fn git_action_worktree_enter_is_compensated() {
        let profile = tool_action_profile(
            "git",
            &json!({"action": "worktree", "sub_action": "enter", "branch": "demo"}),
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
                .contains("action=`worktree`")
        );
    }

    #[test]
    fn rename_symbol_uses_file_rollback_hint() {
        let profile = tool_action_profile(
            "rename_symbol",
            &json!({"path": "src/lib.rs", "old_name": "foo", "new_name": "bar"}),
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
    fn mo_query_and_read_only_tools() {
        // mo_query write: snapshot compensation
        let p = tool_action_profile("mo_query", &json!({"sql": "INSERT INTO t VALUES(1)"}));
        assert_eq!(p.category, ActionCategory::Write);
        assert_eq!(
            p.compensation_kind,
            Some(CompensationKind::RestoreDatabaseSnapshot)
        );
        assert!(
            p.compensation_summary
                .as_deref()
                .unwrap_or_default()
                .contains("snapshot")
        );

        // read-only tools: no compensation prompt
        for tool in ["read_file", "list_dir", "glob", "grep", "lsp"] {
            let p = tool_action_profile(tool, &json!({}));
            assert_eq!(p.category, ActionCategory::Read);
            assert!(p.compensation_kind.is_none());
        }
    }

    // ── remaining tool-specific tests ──

    #[test]
    fn notebook_and_rename_symbol_compensation() {
        // notebook_edit: uses file rollback hint
        let p = tool_action_profile(
            "notebook_edit",
            &json!({"cell_id": "cell-1", "content": "x"}),
        );
        assert!(
            p.compensation_summary
                .as_deref()
                .unwrap_or_default()
                .contains("rollback_file_edits")
        );

        // rename_symbol: uses file rollback hint
        let p = tool_action_profile(
            "rename_symbol",
            &json!({"path": "src/lib.rs", "old_name": "foo", "new_name": "bar"}),
        );
        assert!(
            p.compensation_summary
                .as_deref()
                .unwrap_or_default()
                .contains("rollback_file_edits")
        );
    }

    #[test]
    fn read_only_tools_do_not_emit_prompt_note() {
        assert!(compensation_prompt_note("read_file", &json!({"path": "README.md"})).is_none());
    }

    #[test]
    fn explicit_approval_only_targets_irreversible_or_unbounded_boundary_actions() {
        assert!(!tool_requires_explicit_approval(
            "write_file",
            &json!({"path": "src/lib.rs"})
        ));
        assert!(!tool_requires_explicit_approval(
            "adjust_config",
            &json!({"path": "memory.retrieval_top_k", "value": 6})
        ));
        assert!(tool_requires_explicit_approval(
            "git",
            &json!({"action": "commit", "message": "ship it"})
        ));
        assert!(tool_requires_explicit_approval(
            "github",
            &json!({"action": "create_issue", "owner": "o", "repo": "r", "title": "t"})
        ));
        assert!(tool_requires_explicit_approval(
            "bash",
            &json!({"command": "rm -rf tmp"})
        ));
    }

    #[test]
    fn explicit_approval_reason_describes_boundary_gap() {
        let git_action_commit_reason =
            explicit_approval_reason("git", &json!({"action": "commit", "message": "x"}))
                .expect("git commit should require explicit approval");
        assert!(git_action_commit_reason.contains("unbounded"));

        let bash_reason = explicit_approval_reason("bash", &json!({"command": "rm -rf tmp"}))
            .expect("destructive bash should require explicit approval");
        assert!(bash_reason.contains("rollback"));

        assert!(explicit_approval_reason("write_file", &json!({"path": "x"})).is_none());
    }

    #[test]
    fn cloud_approval_required_tools_never_fall_back_to_read_profiles() {
        fn sample_args(tool_name: &str) -> Value {
            match tool_name {
                "bash" | "exec" | "run_command" | "shell" | "powershell" => {
                    json!({"command": "touch tmp.txt"})
                }
                "create_file" | "write_file" => json!({"path": "tmp.txt", "content": "ok"}),
                "delete_file" => json!({"path": "tmp.txt"}),
                "edit_file" | "str_replace" => {
                    json!({"path": "tmp.txt", "old_str": "a", "new_str": "b"})
                }
                "git" => json!({"action": "push", "remote": "origin", "branch": "main"}),
                "github" => {
                    json!({"action": "create_issue", "owner": "o", "repo": "r", "title": "t"})
                }
                "multi_edit" => {
                    json!({"path": "tmp.txt", "edits": [{"old_str": "a", "new_str": "b"}]})
                }
                "apply_patch" => {
                    json!({"path": "tmp.txt", "patch": "--- a\n+++ b\n@@ -1 +1 @@\n-a\n+b"})
                }
                "publish_artifact" => json!({"path": "tmp.md"}),
                "rollback_database_snapshots" | "rollback_file_edits" => json!({}),
                other => panic!("add sample args for {other}"),
            }
        }

        for &tool_name in crate::cloud::approval_policy::CLOUD_APPROVAL_REQUIRED_TOOLS.iter() {
            let profile = tool_action_profile(tool_name, &sample_args(tool_name));
            assert_ne!(
                profile.category,
                ActionCategory::Read,
                "{tool_name} should not resolve to a read-only profile"
            );
        }
    }

    // ── Execution outcome classification tests ──

    // ── classify outcomes ──

    #[test]
    fn classify_structured_failure_outcomes() {
        let cases: &[(
            Option<astra_core::ErrorKind>,
            Option<&str>,
            Option<&str>,
            ExecutionOutcome,
            Option<FailureCategory>,
        )] = &[
            (
                Some(astra_core::ErrorKind::ToolTimeout),
                None,
                None,
                ExecutionOutcome::Timeout,
                Some(FailureCategory::Timeout),
            ),
            (
                Some(astra_core::ErrorKind::ResourceLimit),
                None,
                None,
                ExecutionOutcome::ResourceLimit,
                Some(FailureCategory::ResourceExhaustion),
            ),
            (
                Some(astra_core::ErrorKind::Auth),
                None,
                None,
                ExecutionOutcome::Failure,
                Some(FailureCategory::PermissionDenied),
            ),
            (
                Some(astra_core::ErrorKind::ToolInvalidArgs),
                None,
                None,
                ExecutionOutcome::Failure,
                Some(FailureCategory::ValidationError),
            ),
            (
                Some(astra_core::ErrorKind::ContractViolation),
                None,
                None,
                ExecutionOutcome::Failure,
                Some(FailureCategory::RuntimeError),
            ),
            (
                None,
                Some("compile_error"),
                None,
                ExecutionOutcome::Failure,
                Some(FailureCategory::CompileError),
            ),
            (
                None,
                Some("test_failure"),
                None,
                ExecutionOutcome::Failure,
                Some(FailureCategory::TestFailure),
            ),
            (
                None,
                Some("network_error"),
                None,
                ExecutionOutcome::Failure,
                Some(FailureCategory::NetworkError),
            ),
            (
                None,
                None,
                Some("resource_limit"),
                ExecutionOutcome::ResourceLimit,
                Some(FailureCategory::ResourceExhaustion),
            ),
        ];

        for (error_kind, result_class, exit_semantics, outcome, failure_category) in cases {
            let c = classify_execution_outcome_from_input(ExecutionOutcomeInput {
                result_text: "",
                is_error: true,
                duration_ms: 2000,
                was_rejected: false,
                error_kind: *error_kind,
                result_class: *result_class,
                exit_semantics: *exit_semantics,
            });
            assert_eq!(c.outcome, *outcome);
            assert_eq!(c.failure_category, *failure_category);
        }
    }

    #[test]
    fn legacy_wrapper_does_not_infer_category_from_output_text() {
        let c = classify_execution_outcome("error[E0433]: compile error", true, 2000, false);
        assert_eq!(c.outcome, ExecutionOutcome::Failure);
        assert_eq!(c.failure_category, Some(FailureCategory::Unknown));
    }

    #[test]
    fn classify_success_and_edge_outcomes() {
        // success
        let c = classify_execution_outcome(r#"{"ok":true,"result":"done"}"#, false, 500, false);
        assert_eq!(c.outcome, ExecutionOutcome::Success);
        assert!(c.failure_category.is_none());
        assert!(c.error_snippet.is_none());
        // rejected
        let c = classify_execution_outcome("rejected by policy", true, 0, true);
        assert_eq!(c.outcome, ExecutionOutcome::Rejected);
        assert!(c.failure_category.is_none());
        // rejected takes priority over content
        let c = classify_execution_outcome("error[E0433]: some compile error", true, 100, true);
        assert_eq!(c.outcome, ExecutionOutcome::Rejected);
        assert!(c.failure_category.is_none());
        // structured resource limit
        let c = classify_execution_outcome_from_input(ExecutionOutcomeInput {
            result_text: "",
            is_error: true,
            duration_ms: 100,
            was_rejected: false,
            error_kind: Some(astra_core::ErrorKind::ResourceLimit),
            result_class: None,
            exit_semantics: None,
        });
        assert_eq!(c.outcome, ExecutionOutcome::ResourceLimit);
        assert_eq!(
            c.failure_category,
            Some(FailureCategory::ResourceExhaustion)
        );
        // timeout from duration, not output prose
        let c = classify_execution_outcome("long-running failure", true, 130_000, false);
        assert_eq!(c.outcome, ExecutionOutcome::Timeout);
        assert_eq!(c.failure_category, Some(FailureCategory::Timeout));
        // success with resource-limit text stays success (didn't fail)
        let c = classify_execution_outcome("Killed", false, 100, false);
        assert_eq!(c.outcome, ExecutionOutcome::Success);
    }

    #[test]
    fn classify_outcome_serde() {
        let c = ExecutionOutcomeClassification {
            outcome: ExecutionOutcome::Failure,
            failure_category: Some(FailureCategory::CompileError),
            error_snippet: Some("error[E0433]".to_string()),
        };
        let json = serde_json::to_string(&c).unwrap();
        let roundtrip: ExecutionOutcomeClassification = serde_json::from_str(&json).unwrap();
        assert_eq!(c, roundtrip);
        // success with None fields → skip in JSON
        let c = ExecutionOutcomeClassification {
            outcome: ExecutionOutcome::Success,
            failure_category: None,
            error_snippet: None,
        };
        let json = serde_json::to_string(&c).unwrap();
        assert!(!json.contains("failure_category"));
        assert!(!json.contains("error_snippet"));
    }

    #[test]
    fn classify_snippet_truncation() {
        let long_error = format!("error[E0433]: {}", "x".repeat(500));
        let c = classify_execution_outcome(&long_error, true, 2000, false);
        assert!(c.error_snippet.as_ref().unwrap().len() <= 210);
    }
}

#[cfg(test)]
mod structured_execution_outcome_tests {
    use super::*;

    #[test]
    fn structured_error_kind_classifies_without_text_matching() {
        let classification = classify_execution_outcome_from_input(ExecutionOutcomeInput {
            result_text: "",
            is_error: true,
            duration_ms: 1,
            was_rejected: false,
            error_kind: Some(astra_core::ErrorKind::ToolTimeout),
            result_class: None,
            exit_semantics: None,
        });

        assert_eq!(classification.outcome, ExecutionOutcome::Timeout);
        assert_eq!(
            classification.failure_category,
            Some(FailureCategory::Timeout)
        );
    }

    #[test]
    fn structured_result_class_classifies_without_output_prose() {
        let classification = classify_execution_outcome_from_input(ExecutionOutcomeInput {
            result_text: "",
            is_error: true,
            duration_ms: 1,
            was_rejected: false,
            error_kind: None,
            result_class: Some("test_failure"),
            exit_semantics: None,
        });

        assert_eq!(classification.outcome, ExecutionOutcome::Failure);
        assert_eq!(
            classification.failure_category,
            Some(FailureCategory::TestFailure)
        );
    }

    #[test]
    fn structured_exit_semantics_classifies_without_output_prose() {
        let classification = classify_execution_outcome_from_input(ExecutionOutcomeInput {
            result_text: "",
            is_error: true,
            duration_ms: 1,
            was_rejected: false,
            error_kind: None,
            result_class: None,
            exit_semantics: Some("resource_limit"),
        });

        assert_eq!(classification.outcome, ExecutionOutcome::ResourceLimit);
        assert_eq!(
            classification.failure_category,
            Some(FailureCategory::ResourceExhaustion)
        );
    }
}
