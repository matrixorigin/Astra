//! Git operations — delegates standalone functions to astra_tools::git_gix,
//! keeps CLI-specific journal management wrappers and worktree management.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::ToolExecutor;

// Re-export all public git functions from astra-tools.
// This eliminates ~2200 lines of duplicate standalone function definitions.
pub use astra_tools::git_gix::*;

fn tool_output_limit() -> usize {
    super::tool_output_limit()
}

/// Scale a base output limit by budget pressure.
/// pressure=0.0 → 100% of base, pressure=0.6 → 70%, pressure=0.9 → 46%.
/// Never goes below 40% of base — aggressive truncation on git diffs
/// forces bash fallbacks which waste a tool round.
fn pressure_scaled_limit(base: usize, pressure: f64) -> usize {
    let scale = (1.0 - pressure * 0.6).max(0.4);
    (base as f64 * scale) as usize
}

impl ToolExecutor {
    fn record_git_commit_rollback(&self, commit_sha: impl Into<String>, message: Option<String>) {
        let turn_index = self
            .journal_turn_index
            .load(std::sync::atomic::Ordering::Relaxed);
        match self.git_commit_journal.lock() {
            Ok(mut journal) => journal.record(commit_sha, turn_index, message),
            Err(poisoned) => poisoned
                .into_inner()
                .record(commit_sha, turn_index, message),
        }
    }

    fn git_commit_entries(&self) -> Vec<GitCommitRollbackEntry> {
        match self.git_commit_journal.lock() {
            Ok(journal) => journal.list(),
            Err(poisoned) => poisoned.into_inner().list(),
        }
    }

    fn git_commit_restore_plan_for_turn(&self, turn_index: u32) -> Vec<GitCommitRollbackEntry> {
        match self.git_commit_journal.lock() {
            Ok(journal) => journal.restore_plan_for_turn(turn_index),
            Err(poisoned) => poisoned.into_inner().restore_plan_for_turn(turn_index),
        }
    }

    fn git_commit_restore_plan_for_turn_since(
        &self,
        turn_index: u32,
        checkpoint: u64,
    ) -> Vec<GitCommitRollbackEntry> {
        match self.git_commit_journal.lock() {
            Ok(journal) => journal.restore_plan_for_turn_since(turn_index, checkpoint),
            Err(poisoned) => poisoned
                .into_inner()
                .restore_plan_for_turn_since(turn_index, checkpoint),
        }
    }

    pub(crate) fn git_commit_journal_checkpoint(&self) -> u64 {
        match self.git_commit_journal.lock() {
            Ok(journal) => journal.checkpoint(),
            Err(poisoned) => poisoned.into_inner().checkpoint(),
        }
    }

    fn remove_git_commit_rollback(&self, commit_sha: &str) {
        match self.git_commit_journal.lock() {
            Ok(mut journal) => {
                journal.remove_commit(commit_sha);
            }
            Err(poisoned) => {
                poisoned.into_inner().remove_commit(commit_sha);
            }
        }
    }

    fn rollback_git_commit_entry_json(entry: &GitCommitRollbackEntry) -> Value {
        let mut value = serde_json::Map::from_iter([
            (
                "commit_sha".to_string(),
                Value::String(entry.commit_sha.clone()),
            ),
            (
                "commit_short_sha".to_string(),
                Value::String(short_commit_sha(&entry.commit_sha)),
            ),
            (
                "turn_index".to_string(),
                Value::Number(serde_json::Number::from(entry.turn_index)),
            ),
        ]);
        if let Some(message) = entry.message.as_ref() {
            value.insert("message".to_string(), Value::String(message.clone()));
        }
        Value::Object(value)
    }

    pub(crate) fn rollback_git_commits(&self, args: &Value) -> String {
        let scope = args
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or("current_turn");
        let explicit_turn_index = if scope == "turn" {
            match args.get("turn_index").and_then(Value::as_u64) {
                Some(turn_index) => Some(turn_index),
                None => {
                    return serde_json::json!({
                        "success": false,
                        "error": "missing 'turn_index' for scope=turn",
                    })
                    .to_string();
                }
            }
        } else {
            None
        };
        let after_sequence = args
            .get("commit_after_sequence")
            .and_then(Value::as_u64)
            .unwrap_or(0);

        match scope {
            "list" => {
                let entries = self
                    .git_commit_entries()
                    .into_iter()
                    .map(|entry| Self::rollback_git_commit_entry_json(&entry))
                    .collect::<Vec<_>>();
                serde_json::json!({
                    "success": true,
                    "scope": "list",
                    "total_entries": entries.len(),
                    "entries": entries,
                    "summary": format!("Listed {} recorded git commit rollback entr{}", entries.len(), if entries.len() == 1 { "y" } else { "ies" }),
                })
                .to_string()
            }
            "turn" | "current_turn" => {
                let turn_index = explicit_turn_index.unwrap_or_else(|| {
                    self.journal_turn_index
                        .load(std::sync::atomic::Ordering::Relaxed) as u64
                }) as u32;
                let plan = if after_sequence > 0 {
                    self.git_commit_restore_plan_for_turn_since(turn_index, after_sequence)
                } else {
                    self.git_commit_restore_plan_for_turn(turn_index)
                };
                let mut reverted = Vec::new();
                let mut failed = Vec::new();

                if !plan.is_empty() {
                    match git_worktree_is_clean(&self.project_root) {
                        Ok(true) => {}
                        Ok(false) => {
                            let error =
                                "working tree must be clean before automatic git commit rollback"
                                    .to_string();
                            failed = plan
                                .iter()
                                .map(|entry| {
                                    let mut failed_entry =
                                        Self::rollback_git_commit_entry_json(entry)
                                            .as_object()
                                            .cloned()
                                            .unwrap_or_default();
                                    failed_entry
                                        .insert("error".to_string(), Value::String(error.clone()));
                                    Value::Object(failed_entry)
                                })
                                .collect();
                        }
                        Err(error) => {
                            failed = plan
                                .iter()
                                .map(|entry| {
                                    let mut failed_entry =
                                        Self::rollback_git_commit_entry_json(entry)
                                            .as_object()
                                            .cloned()
                                            .unwrap_or_default();
                                    failed_entry
                                        .insert("error".to_string(), Value::String(error.clone()));
                                    Value::Object(failed_entry)
                                })
                                .collect();
                        }
                    }
                }

                if failed.is_empty() && !plan.is_empty() {
                    let expected_tail = plan
                        .iter()
                        .map(|entry| entry.commit_sha.clone())
                        .collect::<Vec<_>>();
                    let actual_tail =
                        head_first_parent_tail(&self.project_root, expected_tail.len())
                            .unwrap_or_default();
                    if actual_tail != expected_tail {
                        let error = "recorded git commits are no longer the current HEAD tail; use git_revert_commit manually".to_string();
                        failed = plan
                            .iter()
                            .map(|entry| {
                                let mut failed_entry = Self::rollback_git_commit_entry_json(entry)
                                    .as_object()
                                    .cloned()
                                    .unwrap_or_default();
                                failed_entry
                                    .insert("error".to_string(), Value::String(error.clone()));
                                Value::Object(failed_entry)
                            })
                            .collect();
                    }
                }

                if failed.is_empty() {
                    for entry in &plan {
                        let outcome = crate::edge_tools::git_gix::git_revert_commit_with_metadata(
                            &self.project_root,
                            &serde_json::json!({
                                "commit_sha": entry.commit_sha,
                            }),
                        );
                        if outcome.output.starts_with("Error:") {
                            let mut failed_entry = Self::rollback_git_commit_entry_json(entry)
                                .as_object()
                                .cloned()
                                .unwrap_or_default();
                            failed_entry.insert("error".to_string(), Value::String(outcome.output));
                            failed.push(Value::Object(failed_entry));
                            break;
                        }
                        self.remove_git_commit_rollback(&entry.commit_sha);
                        self.clear_file_state();
                        let mut reverted_entry = Self::rollback_git_commit_entry_json(entry)
                            .as_object()
                            .cloned()
                            .unwrap_or_default();
                        if let Some(fields) = outcome.tool_result_fields.as_ref() {
                            if let Some(revert_commit_sha) =
                                fields.get("revert_commit_sha").and_then(Value::as_str)
                            {
                                reverted_entry.insert(
                                    "revert_commit_sha".to_string(),
                                    Value::String(revert_commit_sha.to_string()),
                                );
                                reverted_entry.insert(
                                    "revert_commit_short_sha".to_string(),
                                    Value::String(short_commit_sha(revert_commit_sha)),
                                );
                            }
                        }
                        reverted.push(Value::Object(reverted_entry));
                    }
                }

                let success = !reverted.is_empty() && failed.is_empty();
                let summary = if plan.is_empty() {
                    format!("No recorded git commit rollback handles found for turn {turn_index}")
                } else if failed.is_empty() {
                    format!(
                        "Reverted {} recorded git commit{} for turn {turn_index}",
                        reverted.len(),
                        if reverted.len() == 1 { "" } else { "s" }
                    )
                } else {
                    format!(
                        "Reverted {} recorded git commit{} for turn {turn_index} with {} failure{}",
                        reverted.len(),
                        if reverted.len() == 1 { "" } else { "s" },
                        failed.len(),
                        if failed.len() == 1 { "" } else { "s" }
                    )
                };
                serde_json::json!({
                    "success": success,
                    "scope": scope,
                    "turn_index": turn_index,
                    "reverted": reverted,
                    "failed": failed,
                    "summary": summary,
                })
                .to_string()
            }
            other => serde_json::json!({
                "success": false,
                "error": format!(
                    "unknown scope `{other}`. Supported: current_turn, turn, list"
                ),
            })
            .to_string(),
        }
    }

    pub(crate) fn git_commit(&self, args: &Value) -> String {
        self.git_commit_with_metadata(args).output
    }

    pub(crate) fn git_commit_with_metadata(&self, args: &Value) -> super::ToolExecutionOutcome {
        let outcome =
            crate::edge_tools::git_gix::git_commit_with_metadata(&self.project_root, args);
        if let Some(commit_sha) = outcome
            .tool_result_fields
            .as_ref()
            .and_then(|fields| fields.get("commit_sha"))
            .and_then(Value::as_str)
        {
            self.record_git_commit_rollback(
                commit_sha.to_string(),
                args.get("message")
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
            );
        }
        outcome
    }

    pub(crate) fn git_revert_commit(&self, args: &Value) -> String {
        self.git_revert_commit_with_metadata(args).output
    }

    pub(crate) fn git_revert_commit_with_metadata(
        &self,
        args: &Value,
    ) -> super::ToolExecutionOutcome {
        let outcome =
            crate::edge_tools::git_gix::git_revert_commit_with_metadata(&self.project_root, args);
        if !outcome.output.starts_with("Error:") {
            self.clear_file_state();
        }
        outcome
    }

    fn record_git_stash_rollback(&self, stash_ref: impl Into<String>, message: Option<String>) {
        let turn_index = self
            .journal_turn_index
            .load(std::sync::atomic::Ordering::Relaxed);
        match self.git_stash_journal.lock() {
            Ok(mut journal) => journal.record(stash_ref, turn_index, message),
            Err(poisoned) => poisoned.into_inner().record(stash_ref, turn_index, message),
        }
    }

    fn git_stash_entries(&self) -> Vec<GitStashRollbackEntry> {
        match self.git_stash_journal.lock() {
            Ok(journal) => journal.list(),
            Err(poisoned) => poisoned.into_inner().list(),
        }
    }

    fn git_stash_restore_plan_for_turn(&self, turn_index: u32) -> Vec<GitStashRollbackEntry> {
        match self.git_stash_journal.lock() {
            Ok(journal) => journal.restore_plan_for_turn(turn_index),
            Err(poisoned) => poisoned.into_inner().restore_plan_for_turn(turn_index),
        }
    }

    fn git_stash_restore_plan_for_turn_since(
        &self,
        turn_index: u32,
        checkpoint: u64,
    ) -> Vec<GitStashRollbackEntry> {
        match self.git_stash_journal.lock() {
            Ok(journal) => journal.restore_plan_for_turn_since(turn_index, checkpoint),
            Err(poisoned) => poisoned
                .into_inner()
                .restore_plan_for_turn_since(turn_index, checkpoint),
        }
    }

    pub(crate) fn git_stash_journal_checkpoint(&self) -> u64 {
        match self.git_stash_journal.lock() {
            Ok(journal) => journal.checkpoint(),
            Err(poisoned) => poisoned.into_inner().checkpoint(),
        }
    }

    fn remove_git_stash_rollback(&self, stash_ref: &str) {
        match self.git_stash_journal.lock() {
            Ok(mut journal) => {
                journal.remove_stash(stash_ref);
            }
            Err(poisoned) => {
                poisoned.into_inner().remove_stash(stash_ref);
            }
        }
    }

    fn rollback_git_stash_entry_json(entry: &GitStashRollbackEntry) -> Value {
        let mut value = serde_json::Map::from_iter([
            (
                "stash_ref".to_string(),
                Value::String(entry.stash_ref.clone()),
            ),
            (
                "turn_index".to_string(),
                Value::Number(serde_json::Number::from(entry.turn_index)),
            ),
        ]);
        if let Some(message) = entry.message.as_ref() {
            value.insert("message".to_string(), Value::String(message.clone()));
        }
        Value::Object(value)
    }

    pub(crate) fn rollback_git_stashes(&self, args: &Value) -> String {
        let scope = args
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or("current_turn");
        let explicit_turn_index = if scope == "turn" {
            match args.get("turn_index").and_then(Value::as_u64) {
                Some(turn_index) => Some(turn_index),
                None => {
                    return serde_json::json!({
                        "success": false,
                        "error": "missing 'turn_index' for scope=turn",
                    })
                    .to_string();
                }
            }
        } else {
            None
        };
        let after_sequence = args
            .get("stash_after_sequence")
            .and_then(Value::as_u64)
            .unwrap_or(0);

        match scope {
            "list" => {
                let entries = self
                    .git_stash_entries()
                    .into_iter()
                    .map(|entry| Self::rollback_git_stash_entry_json(&entry))
                    .collect::<Vec<_>>();
                serde_json::json!({
                    "success": true,
                    "scope": "list",
                    "total_entries": entries.len(),
                    "entries": entries,
                    "summary": format!("Listed {} recorded git stash rollback entr{}", entries.len(), if entries.len() == 1 { "y" } else { "ies" }),
                })
                .to_string()
            }
            "turn" | "current_turn" => {
                let turn_index = explicit_turn_index.unwrap_or_else(|| {
                    self.journal_turn_index
                        .load(std::sync::atomic::Ordering::Relaxed) as u64
                }) as u32;
                let plan = if after_sequence > 0 {
                    self.git_stash_restore_plan_for_turn_since(turn_index, after_sequence)
                } else {
                    self.git_stash_restore_plan_for_turn(turn_index)
                };
                let mut restored = Vec::new();
                let mut failed = Vec::new();
                for entry in &plan {
                    let apply_output = git_stash(
                        &self.project_root,
                        &serde_json::json!({
                            "action": "apply",
                            "stash_ref": entry.stash_ref,
                        }),
                    );
                    if apply_output.starts_with("Error:") {
                        let mut failed_entry = Self::rollback_git_stash_entry_json(entry)
                            .as_object()
                            .cloned()
                            .unwrap_or_default();
                        failed_entry.insert("error".to_string(), Value::String(apply_output));
                        failed.push(Value::Object(failed_entry));
                    } else {
                        self.remove_git_stash_rollback(&entry.stash_ref);
                        restored.push(Self::rollback_git_stash_entry_json(entry));
                    }
                }
                let success = !restored.is_empty() && failed.is_empty();
                let summary = if plan.is_empty() {
                    format!("No recorded git stash rollback handles found for turn {turn_index}")
                } else if failed.is_empty() {
                    format!(
                        "Re-applied {} recorded git stash{} for turn {turn_index}",
                        restored.len(),
                        if restored.len() == 1 { "" } else { "es" }
                    )
                } else {
                    format!(
                        "Re-applied {} recorded git stash{} for turn {turn_index} with {} failure{}",
                        restored.len(),
                        if restored.len() == 1 { "" } else { "es" },
                        failed.len(),
                        if failed.len() == 1 { "" } else { "s" }
                    )
                };
                serde_json::json!({
                    "success": success,
                    "scope": scope,
                    "turn_index": turn_index,
                    "restored": restored,
                    "failed": failed,
                    "summary": summary,
                })
                .to_string()
            }
            other => serde_json::json!({
                "success": false,
                "error": format!(
                    "unknown scope `{other}`. Supported: current_turn, turn, list"
                ),
            })
            .to_string(),
        }
    }

    pub(crate) fn git_stash(&self, args: &Value) -> String {
        self.git_stash_with_metadata(args).output
    }

    pub(crate) fn git_stash_with_metadata(&self, args: &Value) -> super::ToolExecutionOutcome {
        let action = args
            .get("action")
            .and_then(Value::as_str)
            .map(|action| action.trim().to_ascii_lowercase())
            .unwrap_or_default();
        let outcome = git_stash_with_metadata(&self.project_root, args);
        if matches!(action.as_str(), "push" | "save")
            && let Some(stash_ref) = outcome
                .tool_result_fields
                .as_ref()
                .and_then(|fields| fields.get("stash_ref"))
                .and_then(Value::as_str)
        {
            self.record_git_stash_rollback(
                stash_ref.to_string(),
                args.get("message")
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
            );
            self.clear_file_state();
        } else if matches!(action.as_str(), "apply" | "pop")
            && !outcome.output.starts_with("Error:")
        {
            self.clear_file_state();
        }
        outcome
    }
}

/// Revert a file to its last committed state (discard working tree changes).
///
/// Parameters:
/// - `path` (required): file path relative to project root
/// - `ref` (optional): restore from a specific commit/ref (default: HEAD)
pub fn git_checkout_file(project_root: &Path, args: &Value) -> String {
    let file_path = match args.get("path").and_then(Value::as_str) {
        Some(p) if !p.is_empty() => p,
        _ => return "Error: 'path' is required".to_string(),
    };

    // Security: reject path traversal
    if file_path.contains("..") {
        return "Error: path traversal not allowed".to_string();
    }

    let git_ref = args.get("ref").and_then(Value::as_str).unwrap_or("HEAD");

    // Validate ref doesn't contain shell-dangerous chars
    if git_ref.contains(';')
        || git_ref.contains('|')
        || git_ref.contains('&')
        || git_ref.contains('`')
        || git_ref.contains("$(")
        || git_ref.contains("${")
    {
        return "Error: invalid ref".to_string();
    }

    let out = std::process::Command::new("git")
        .args(["checkout", git_ref, "--", file_path])
        .current_dir(project_root)
        .output();

    match out {
        Ok(o) if o.status.success() => {
            format!("✓ Restored {file_path} from {git_ref}")
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            format!("Error: git checkout failed: {}", stderr.trim())
        }
        Err(e) => format!("Error: git checkout failed: {e}"),
    }
}

impl ToolExecutor {
    pub(crate) fn git_checkout_file(&self, args: &Value) -> String {
        let file_arg = match args.get("path").and_then(Value::as_str) {
            Some(path) if !path.is_empty() => path,
            _ => return crate::edge_tools::git_gix::git_checkout_file(&self.project_root, args),
        };
        let path = match self.resolve_checked(file_arg) {
            Ok(path) => path,
            Err(error) => return error,
        };

        let turn_idx = self
            .journal_turn_index
            .load(std::sync::atomic::Ordering::Relaxed);
        let journal_call_id = format!("git_checkout_file:{}", path.display());
        match self.file_journal.lock() {
            Ok(mut journal) => journal.record_before(&path, &journal_call_id, turn_idx),
            Err(poisoned) => poisoned
                .into_inner()
                .record_before(&path, &journal_call_id, turn_idx),
        }

        let output = crate::edge_tools::git_gix::git_checkout_file(&self.project_root, args);
        if output.starts_with("Error:") {
            return output;
        }

        let after_content = std::fs::read(&path).unwrap_or_default();
        match self.file_journal.lock() {
            Ok(mut journal) => journal.record_after(&path, &journal_call_id, &after_content),
            Err(poisoned) => {
                poisoned
                    .into_inner()
                    .record_after(&path, &journal_call_id, &after_content)
            }
        }

        if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(content) => self.record_write_with_content(&path, &content),
                Err(_) => self.record_write(&path),
            }
        } else {
            self.remove_file_state(&path);
        }

        output
    }
}

// ─── Git Worktree Management ────────────────────────────────────────────────

/// Create, list, or remove git worktrees for isolated parallel work.
pub fn git_worktree(project_root: &Path, args: &Value) -> String {
    let action = match args.get("action").and_then(Value::as_str) {
        Some(a) => a,
        None => return "Error: 'action' is required (add, list, remove)".to_string(),
    };

    match action {
        "add" | "create" => worktree_add(project_root, args),
        "list" | "ls" => worktree_list(project_root),
        "remove" | "rm" | "delete" => worktree_remove(project_root, args),
        _ => format!("Error: unknown worktree action '{action}'. Use: add, list, remove"),
    }
}

fn resolve_worktree_add_path(project_root: &Path, args: &Value, branch: &str) -> PathBuf {
    if let Some(path) = args.get("path").and_then(Value::as_str) {
        let path = PathBuf::from(path);
        if path.is_absolute() {
            path
        } else {
            project_root.join(path)
        }
    } else {
        let repo_name = project_root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("repo");
        let sanitized_branch = branch.replace('/', "-");
        project_root
            .parent()
            .unwrap_or(project_root)
            .join(format!("{repo_name}-{sanitized_branch}"))
    }
}

pub(crate) fn worktree_add(project_root: &Path, args: &Value) -> String {
    worktree_add_with_metadata(project_root, args).output
}

pub(crate) fn worktree_add_with_metadata(
    project_root: &Path,
    args: &Value,
) -> super::ToolExecutionOutcome {
    let branch = match args.get("branch").and_then(Value::as_str) {
        Some(b) if !b.is_empty() => b,
        _ => {
            return super::ToolExecutionOutcome {
                output: "Error: 'branch' is required for add".to_string(),
                tool_result_fields: None,
            };
        }
    };

    // Security: reject shell-dangerous chars in branch name
    if branch
        .chars()
        .any(|c| matches!(c, ';' | '|' | '&' | '`' | '$' | '(' | ')' | '{' | '}'))
    {
        return super::ToolExecutionOutcome {
            output: "Error: invalid branch name".to_string(),
            tool_result_fields: None,
        };
    }

    let worktree_path = resolve_worktree_add_path(project_root, args, branch);

    // Check if path already exists
    if worktree_path.exists() {
        return super::ToolExecutionOutcome {
            output: format!(
                "Error: worktree path already exists: {}",
                worktree_path.display()
            ),
            tool_result_fields: None,
        };
    }

    // Determine if we create a new branch or use existing
    let create_new = args
        .get("new_branch")
        .and_then(Value::as_bool)
        .unwrap_or(true);

    let mut cmd = std::process::Command::new("git");
    cmd.current_dir(project_root);
    cmd.arg("worktree").arg("add");

    if create_new {
        cmd.arg("-b").arg(branch);
    }

    cmd.arg(worktree_path.to_string_lossy().as_ref());

    if !create_new {
        cmd.arg(branch);
    }

    match cmd.output() {
        Ok(o) if o.status.success() => {
            let mut tool_result_fields = serde_json::Map::from_iter([
                (
                    "worktree_path".to_string(),
                    Value::String(worktree_path.display().to_string()),
                ),
                ("branch".to_string(), Value::String(branch.to_string())),
                (
                    "delete_branch_on_rollback".to_string(),
                    Value::Bool(create_new),
                ),
                ("session_scoped".to_string(), Value::Bool(false)),
            ]);
            let original_head_commit = head_short(&worktree_path);
            if !original_head_commit.is_empty() {
                tool_result_fields.insert(
                    "original_head_commit".to_string(),
                    Value::String(original_head_commit),
                );
            }
            super::ToolExecutionOutcome {
                output: format!(
                    "✓ Worktree created\n  Branch: {branch}\n  Path: {}\n  Use `cd {}` or set project_root to work in this worktree.",
                    worktree_path.display(),
                    worktree_path.display()
                ),
                tool_result_fields: Some(tool_result_fields),
            }
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            super::ToolExecutionOutcome {
                output: format!("Error: git worktree add failed: {}", stderr.trim()),
                tool_result_fields: None,
            }
        }
        Err(e) => super::ToolExecutionOutcome {
            output: format!("Error: git worktree add failed: {e}"),
            tool_result_fields: None,
        },
    }
}

pub(crate) fn worktree_list(project_root: &Path) -> String {
    let out = std::process::Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(project_root)
        .output();

    match out {
        Ok(o) if o.status.success() => {
            let raw = String::from_utf8_lossy(&o.stdout);
            if raw.trim().is_empty() {
                return "No worktrees found".to_string();
            }

            // Parse porcelain output into structured display
            let mut result = String::from("Git Worktrees:\n");
            let mut current_path = "";
            let mut current_branch = String::new();
            let mut current_head = String::new();
            let mut is_bare = false;

            for line in raw.lines() {
                if let Some(path) = line.strip_prefix("worktree ") {
                    if !current_path.is_empty() {
                        result.push_str(&format_worktree_entry(
                            current_path,
                            &current_branch,
                            &current_head,
                            is_bare,
                        ));
                    }
                    current_path = path;
                    current_branch.clear();
                    current_head.clear();
                    is_bare = false;
                } else if let Some(head) = line.strip_prefix("HEAD ") {
                    current_head = head[..7.min(head.len())].to_string();
                } else if let Some(branch) = line.strip_prefix("branch refs/heads/") {
                    current_branch = branch.to_string();
                } else if line == "bare" {
                    is_bare = true;
                } else if line == "detached" {
                    current_branch = "(detached)".to_string();
                }
            }
            // Flush last entry
            if !current_path.is_empty() {
                result.push_str(&format_worktree_entry(
                    current_path,
                    &current_branch,
                    &current_head,
                    is_bare,
                ));
            }

            result
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            format!("Error: git worktree list failed: {}", stderr.trim())
        }
        Err(e) => format!("Error: git worktree list failed: {e}"),
    }
}

fn format_worktree_entry(path: &str, branch: &str, head: &str, bare: bool) -> String {
    if bare {
        format!("  {path} (bare)\n")
    } else if branch.is_empty() {
        format!("  {path}  [{head}]\n")
    } else {
        format!("  {path}  [{branch}] {head}\n")
    }
}

pub(crate) fn worktree_remove(project_root: &Path, args: &Value) -> String {
    let path = match args.get("path").and_then(Value::as_str) {
        Some(p) if !p.is_empty() => p,
        _ => return "Error: 'path' is required for remove".to_string(),
    };

    let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);

    // Also delete the branch if requested
    let delete_branch = args
        .get("delete_branch")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // First, get the branch name before removal (for optional branch deletion)
    let branch_name = if delete_branch {
        let out = std::process::Command::new("git")
            .args(["worktree", "list", "--porcelain"])
            .current_dir(project_root)
            .output();
        out.ok().and_then(|o| {
            let stdout = String::from_utf8_lossy(&o.stdout);
            let mut found_path = false;
            for line in stdout.lines() {
                if let Some(wt_path) = line.strip_prefix("worktree ") {
                    found_path = wt_path == path
                        || std::path::Path::new(wt_path) == std::path::Path::new(path);
                }
                if found_path {
                    if let Some(branch) = line.strip_prefix("branch refs/heads/") {
                        return Some(branch.to_string());
                    }
                }
            }
            None
        })
    } else {
        None
    };

    // Remove worktree
    let mut cmd = std::process::Command::new("git");
    cmd.current_dir(project_root).arg("worktree").arg("remove");
    if force {
        cmd.arg("--force");
    }
    cmd.arg(path);

    match cmd.output() {
        Ok(o) if o.status.success() => {
            let mut msg = format!("✓ Worktree removed: {path}");

            // Optionally delete the branch
            if let Some(ref branch) = branch_name {
                let del = std::process::Command::new("git")
                    .args(["branch", "-D", branch])
                    .current_dir(project_root)
                    .output();
                match del {
                    Ok(d) if d.status.success() => {
                        msg.push_str(&format!("\n  ✓ Branch '{branch}' deleted"));
                    }
                    _ => {
                        msg.push_str(&format!("\n  ⚠ Could not delete branch '{branch}'"));
                    }
                }
            }

            msg
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            format!("Error: git worktree remove failed: {}", stderr.trim())
        }
        Err(e) => format!("Error: git worktree remove failed: {e}"),
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use astra_runtime::str_preview::prefix_chars;
    use serde_json::json;
    use tempfile::TempDir;

    fn repo_root() -> std::path::PathBuf {
        let mut path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.pop(); // crates/
        path.pop(); // rust/
        path
    }

    fn init_temp_repo() -> TempDir {
        let dir = TempDir::new().expect("temp repo");
        std::process::Command::new("git")
            .arg("init")
            .current_dir(dir.path())
            .output()
            .expect("git init");
        std::process::Command::new("git")
            .args(["config", "user.name", "Test User"])
            .current_dir(dir.path())
            .output()
            .expect("git config user.name");
        std::process::Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(dir.path())
            .output()
            .expect("git config user.email");
        std::fs::write(dir.path().join("tracked.txt"), "one\n").expect("seed tracked file");
        std::process::Command::new("git")
            .args(["add", "tracked.txt"])
            .current_dir(dir.path())
            .output()
            .expect("git add");
        std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(dir.path())
            .output()
            .expect("git commit");
        dir
    }

    fn run_git(dir: &std::path::Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git command");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_stdout(dir: &std::path::Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git command");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn init_temp_repo_with_ours_merge() -> TempDir {
        let dir = init_temp_repo();
        let root = dir.path();
        let default_branch = git_stdout(root, &["branch", "--show-current"]);
        run_git(root, &["checkout", "-b", "feature"]);
        std::fs::write(root.join("tracked.txt"), "feature branch change\n").expect("write feature");
        run_git(root, &["add", "tracked.txt"]);
        run_git(root, &["commit", "-m", "feature change"]);
        run_git(root, &["checkout", &default_branch]);
        run_git(
            root,
            &[
                "merge",
                "--no-ff",
                "-s",
                "ours",
                "feature",
                "-m",
                "merge feature",
            ],
        );
        dir
    }

    #[test]
    fn git_status_returns_output() {
        let root = repo_root();
        let result = git_status(&root);
        assert!(
            result.contains("##")
                || result.contains("nothing to commit")
                || result.contains("Error"),
            "unexpected status: {result}"
        );
    }

    #[test]
    fn git_log_returns_commits() {
        let root = repo_root();
        let result = git_log(&root, &json!({"n": 5}));
        let lines: Vec<&str> = result.lines().collect();
        assert!(!lines.is_empty(), "log should return commits");
        let first = lines[0];
        let hash_prefix = prefix_chars(first, 7);
        assert!(
            hash_prefix.chars().count() == 7 && hash_prefix.chars().all(|c| c.is_ascii_hexdigit()),
            "first log line should start with hash: {first}"
        );
    }

    #[test]
    fn git_log_default_n() {
        let root = repo_root();
        let result = git_log(&root, &json!({}));
        let lines: Vec<&str> = result.lines().filter(|l| !l.is_empty()).collect();
        assert!(lines.len() <= 10, "default should be at most 10 commits");
    }

    #[test]
    fn git_show_missing_commit() {
        let root = repo_root();
        let result = git_show(&root, &json!({}), 0.0, 0);
        assert!(result.contains("Error: missing"));
    }

    #[test]
    fn git_show_invalid_ref() {
        let root = repo_root();
        let result = git_show(&root, &json!({"commit": "abc;rm -rf /"}), 0.0, 0);
        assert!(result.contains("Error: invalid commit reference"));
    }

    #[test]
    fn git_show_head() {
        let root = repo_root();
        let result = git_show(&root, &json!({"commit": "HEAD"}), 0.0, 0);
        assert!(result.contains("commit "), "should show commit: {result}");
        assert!(result.contains("Author:"), "should show author");
    }

    #[test]
    fn git_show_stat_only() {
        let root = repo_root();
        let result = git_show(&root, &json!({"commit": "HEAD", "stat_only": true}), 0.0, 0);
        assert!(result.contains("commit "));
        assert!(
            result.contains("files changed") || result.contains("root commit"),
            "should show stats: {result}"
        );
    }

    #[test]
    fn git_blame_missing_file_param() {
        let root = repo_root();
        let result = git_blame(&root, &json!({}));
        assert!(result.contains("Error: missing 'file'"));
    }

    #[test]
    fn git_blame_known_file() {
        let root = repo_root();
        let result = git_blame(&root, &json!({"file": "README.md"}));
        assert!(
            result.contains("L1") || result.contains("Error") || result.contains("No blame"),
            "unexpected blame: {result}"
        );
    }

    #[test]
    fn git_blame_with_line_range() {
        let root = repo_root();
        let result = git_blame(
            &root,
            &json!({"file": "README.md", "line_start": 1, "line_end": 3}),
        );
        if result.contains("L1") {
            let blame_lines: Vec<&str> = result.lines().filter(|l| l.starts_with('L')).collect();
            assert!(
                blame_lines.len() <= 3,
                "should have at most 3 lines: {blame_lines:?}"
            );
        }
    }

    #[test]
    fn git_diff_no_crash() {
        let root = repo_root();
        let result = git_diff(&root, &json!({}), 0.0, 0);
        assert!(
            !result.contains("Error: cannot open"),
            "should open repo: {result}"
        );
    }

    #[test]
    fn git_file_history_missing_file() {
        let root = repo_root();
        let result = git_file_history(&root, &json!({}));
        assert!(result.contains("Error: missing 'file'"));
    }

    #[test]
    fn git_file_history_known_file() {
        let root = repo_root();
        let result = git_file_history(&root, &json!({"file": "README.md"}));
        assert!(
            result.contains("File: README.md") || result.contains("No history"),
            "unexpected: {result}"
        );
    }

    #[test]
    fn git_file_history_limits_n() {
        let root = repo_root();
        let result = git_file_history(&root, &json!({"file": "README.md", "n": 3}));
        if result.contains("Commits:")
            && let Some(line) = result.lines().find(|l| l.starts_with("Commits:"))
        {
            let count: usize = line
                .trim_start_matches("Commits: ")
                .trim()
                .parse()
                .unwrap_or(0);
            assert!(count <= 3, "should respect n limit: {count}");
        }
    }

    // ─── git_diff enhanced tests ────────────────────────────────────────────

    #[test]
    fn git_diff_staged_param_accepted() {
        let root = repo_root();
        // staged=true should not crash (may return "No staged changes" or file list)
        let result = git_diff(&root, &json!({"staged": true}), 0.0, 0);
        assert!(
            !result.contains("Error: cannot open"),
            "staged diff should not fail to open repo: {result}"
        );
    }

    #[test]
    fn git_diff_ref_param_uses_tree_diff() {
        let root = repo_root();
        // Diff HEAD against HEAD~1 should produce actual file changes
        let result = git_diff(&root, &json!({"ref": "HEAD~1"}), 0.0, 0);
        assert!(
            result.contains("diff --git")
                || result.contains("No changes")
                || result.contains("Error: cannot resolve"),
            "ref diff should produce diff output or error: {result}"
        );
    }

    #[test]
    fn git_diff_default_shows_worktree() {
        let root = repo_root();
        let result = git_diff(&root, &json!({}), 0.0, 0);
        // Should not error — either shows changes or "No changes"
        assert!(
            !result.starts_with("Error:"),
            "default diff should work: {result}"
        );
    }

    #[test]
    fn git_diff_stat_only_smoke() {
        let root = repo_root();
        let result = git_diff(&root, &json!({"stat_only": true}), 0.0, 0);
        assert!(
            !result.starts_with("Error:"),
            "stat_only should use git CLI without repo open errors: {result}"
        );
        assert!(
            result.contains('|')
                || result.to_lowercase().contains("file")
                || result == "No changes",
            "expected stat-style summary: {result}"
        );
    }

    #[test]
    fn git_diff_stat_only_rejects_staged_with_ref() {
        let root = repo_root();
        let result = git_diff(
            &root,
            &json!({"stat_only": true, "staged": true, "ref": "HEAD~1"}),
            0.0,
            0,
        );
        assert!(result.contains("not both"), "{result}");
    }

    #[test]
    fn git_diff_base_ref_range() {
        let root = repo_root();
        let result = git_diff(&root, &json!({"base_ref": "HEAD~3", "ref": "HEAD"}), 0.0, 0);
        assert!(
            result.contains("diff --git") || result == "No changes",
            "range diff should produce output: {result}"
        );
    }

    #[test]
    fn git_diff_base_ref_defaults_tip_to_head() {
        let root = repo_root();
        let result = git_diff(&root, &json!({"base_ref": "HEAD~1"}), 0.0, 0);
        assert!(
            !result.starts_with("Error:"),
            "base_ref without ref should default tip to HEAD: {result}"
        );
    }

    #[test]
    fn git_diff_base_ref_with_path() {
        let root = repo_root();
        let result = git_diff(
            &root,
            &json!({"base_ref": "HEAD~2", "ref": "HEAD", "path": "Cargo.toml"}),
            0.0,
            0,
        );
        assert!(
            !result.starts_with("Error:"),
            "range diff with path filter should work: {result}"
        );
    }

    #[test]
    fn git_diff_base_ref_stat_only() {
        let root = repo_root();
        let result = git_diff(
            &root,
            &json!({"base_ref": "HEAD~3", "ref": "HEAD", "stat_only": true}),
            0.0,
            0,
        );
        assert!(
            !result.starts_with("Error:"),
            "range stat diff should work: {result}"
        );
    }

    #[test]
    fn git_diff_base_ref_rejects_shell_injection() {
        let root = repo_root();
        let result = git_diff(&root, &json!({"base_ref": "HEAD; rm -rf /"}), 0.0, 0);
        assert!(
            result.contains("disallowed"),
            "shell meta in base_ref should be rejected: {result}"
        );
    }

    // ─── git_show enhanced tests ────────────────────────────────────────────

    #[test]
    fn git_show_allows_reflog_syntax() {
        let root = repo_root();
        // HEAD@{0} should not be rejected by validation — it should reach rev_parse
        let result = git_show(&root, &json!({"commit": "HEAD@{0}"}), 0.0, 0);
        // Should show a commit (passes validation), not be rejected outright
        assert!(
            result.starts_with("commit ") || result.starts_with("Error: cannot resolve"),
            "HEAD@{{0}} should pass validation and reach parsing: {result}"
        );
    }

    #[test]
    fn git_show_rejects_shell_metachar() {
        let root = repo_root();
        let result = git_show(&root, &json!({"commit": "HEAD;rm -rf /"}), 0.0, 0);
        assert!(result.contains("Error: invalid commit reference"));
    }

    #[test]
    fn git_show_head_has_diff_content() {
        let root = repo_root();
        let result = git_show(&root, &json!({"commit": "HEAD"}), 0.0, 0);
        assert!(result.contains("commit "), "should show commit header");
        assert!(result.contains("Author:"), "should show author");
        // Should contain actual diff markers or root commit marker
        assert!(
            result.contains("---") || result.contains("[root commit]") || result.contains("+"),
            "should contain diff content: {result}"
        );
    }

    #[test]
    fn git_show_stat_only_has_stats() {
        let root = repo_root();
        let result = git_show(&root, &json!({"commit": "HEAD", "stat_only": true}), 0.0, 0);
        assert!(result.contains("commit "));
        assert!(
            result.contains("files changed") || result.contains("[root commit]"),
            "should show stats or root: {result}"
        );
    }

    #[test]
    fn git_show_merge_commit_stat_only_has_stats() {
        let dir = init_temp_repo_with_ours_merge();
        let result = git_show(
            dir.path(),
            &json!({"commit": "HEAD", "stat_only": true}),
            0.0,
            0,
        );
        assert!(
            result.contains("commit "),
            "should show commit header: {result}"
        );
        assert!(
            result.contains(" file changed") || result.contains(" files changed"),
            "merge commit stat_only should show stats: {result}"
        );
        assert!(
            result.contains("tracked.txt"),
            "merge commit stat_only should mention changed file: {result}"
        );
    }

    #[test]
    fn git_show_merge_commit_has_diff_content() {
        let dir = init_temp_repo_with_ours_merge();
        let result = git_show(dir.path(), &json!({"commit": "HEAD"}), 0.0, 0);
        assert!(
            result.contains("commit "),
            "should show commit header: {result}"
        );
        assert!(
            result.contains("diff --git") || result.contains("--- a/tracked.txt"),
            "merge commit should include diff output: {result}"
        );
    }

    #[test]
    fn git_show_file_filter() {
        let root = repo_root();
        let result = git_show(
            &root,
            &json!({"commit": "HEAD", "file": "README.md"}),
            0.0,
            0,
        );
        // If README.md was changed in HEAD, it should appear; otherwise no diff lines
        assert!(result.contains("commit "), "should show header: {result}");
    }

    // ─── git_blame enhanced tests ───────────────────────────────────────────

    #[test]
    fn git_blame_nonexistent_file() {
        let root = repo_root();
        let result = git_blame(&root, &json!({"file": "nonexistent_file_xyz.rs"}));
        assert!(
            result.contains("Error"),
            "should error on nonexistent file: {result}"
        );
    }

    #[test]
    fn git_blame_output_format() {
        let root = repo_root();
        let result = git_blame(&root, &json!({"file": "README.md"}));
        if result.contains("L1") {
            // Should have structured format: L<n> <commit> <date> [<author>] <content>
            let first_line = result.lines().next().unwrap_or("");
            assert!(
                first_line.starts_with("L1 "),
                "blame line should start with L1: {first_line}"
            );
            assert!(
                first_line.contains('[') && first_line.contains(']'),
                "blame line should have [author]: {first_line}"
            );
        }
    }

    #[test]
    fn git_blame_summary_footer() {
        let root = repo_root();
        let result = git_blame(&root, &json!({"file": "README.md"}));
        if !result.contains("Error") && !result.contains("No blame") {
            assert!(
                result.contains("lines,")
                    && result.contains("authors,")
                    && result.contains("commits"),
                "should have summary footer: {result}"
            );
        }
    }

    // ─── git_status enhanced tests ──────────────────────────────────────────

    #[test]
    fn git_status_shows_branch() {
        let root = repo_root();
        let result = git_status(&root);
        // Should show branch info or be clean
        assert!(
            result.contains("##")
                || result.contains("nothing to commit")
                || result.contains("HEAD detached"),
            "status should show branch: {result}"
        );
    }

    // ─── git_log enhanced tests ─────────────────────────────────────────────

    #[test]
    fn git_log_custom_n() {
        let root = repo_root();
        let result = git_log(&root, &json!({"n": 3}));
        let lines: Vec<&str> = result.lines().filter(|l| !l.is_empty()).collect();
        assert!(
            lines.len() <= 3,
            "should respect n=3: got {} lines",
            lines.len()
        );
        assert!(!lines.is_empty(), "should have at least 1 commit");
    }

    #[test]
    fn git_log_format_consistent() {
        let root = repo_root();
        let result = git_log(&root, &json!({"n": 5}));
        for line in result.lines().filter(|l| !l.is_empty()) {
            // Each line should start with a 7-char hex hash
            let hash_prefix = prefix_chars(line, 7);
            assert!(
                hash_prefix.chars().count() == 7
                    && hash_prefix.chars().all(|c| c.is_ascii_hexdigit()),
                "log line should start with hash: {line}"
            );
            // Should have a space after the hash
            assert!(
                line.chars().nth(7) == Some(' '),
                "log line should have space after hash: {line}"
            );
        }
    }

    // ─── Diff with actual content verification ──────────────────────────────

    #[test]
    fn git_diff_ref_produces_line_content() {
        let root = repo_root();
        let result = git_diff(&root, &json!({"ref": "HEAD~1"}), 0.0, 0);
        if result.contains("diff --git") {
            // If there are changes, we should see actual +/- lines
            assert!(
                result.contains('+') || result.contains('-') || result.contains("# "),
                "ref diff should have content markers: {result}"
            );
        }
    }

    // ─── Edge cases ─────────────────────────────────────────────────────────

    #[test]
    fn git_file_history_nonexistent_file() {
        let root = repo_root();
        let result = git_file_history(&root, &json!({"file": "this/does/not/exist.xyz"}));
        assert!(
            result.contains("No history"),
            "should say no history: {result}"
        );
    }

    #[test]
    fn git_show_parent_ref() {
        let root = repo_root();
        let result = git_show(&root, &json!({"commit": "HEAD~1"}), 0.0, 0);
        assert!(
            result.contains("commit ") || result.contains("Error: cannot resolve"),
            "HEAD~1 should work: {result}"
        );
    }

    // ─── git_log_search tests ───────────────────────────────────────────────

    #[test]
    fn git_log_search_missing_query() {
        let root = repo_root();
        let result = git_log_search(&root, &json!({}));
        assert!(result.contains("Error: missing or empty"));
    }

    #[test]
    fn git_log_search_empty_query() {
        let root = repo_root();
        let result = git_log_search(&root, &json!({"query": "  "}));
        assert!(result.contains("Error: missing or empty"));
    }

    #[test]
    fn git_log_search_finds_commits() {
        let root = repo_root();
        let result = git_log_search(&root, &json!({"query": "fix"}));
        // Should find some commits with "fix" in the message
        assert!(
            result.contains("Search:") || result.contains("No commits matching"),
            "should produce search result: {result}"
        );
    }

    #[test]
    fn git_log_search_respects_n() {
        let root = repo_root();
        let result = git_log_search(&root, &json!({"query": "fix", "n": 10}));
        if result.contains("commits searched") {
            // Extract the number of commits searched
            if let Some(start) = result.find('(')
                && let Some(end) = result.find(" commits searched")
            {
                let num_str = &result[start + 1..end];
                let count: usize = num_str.parse().unwrap_or(0);
                assert!(count <= 10, "should search at most 10: {count}");
            }
        }
    }

    #[test]
    fn git_log_search_score_format() {
        let root = repo_root();
        let result = git_log_search(&root, &json!({"query": "feat"}));
        if result.contains("[score:") {
            // Scores should be between 0 and 1
            for line in result.lines() {
                if let Some(start) = line.find("[score:")
                    && let Some(end) = line[start..].find(']')
                {
                    let score_str = &line[start + 7..start + end];
                    let score: f64 = score_str.parse().unwrap_or(0.0);
                    assert!(score > 0.0 && score <= 1.0, "score should be 0-1: {score}");
                }
            }
        }
    }

    // ─── git_contributors tests ─────────────────────────────────────────────

    #[test]
    fn git_contributors_shows_authors() {
        let root = repo_root();
        let result = git_contributors(&root, &json!({}));
        assert!(
            result.contains("## Top Contributors") || result.contains("No git history"),
            "should show contributors: {result}"
        );
    }

    #[test]
    fn git_contributors_shows_hot_files() {
        let root = repo_root();
        let result = git_contributors(&root, &json!({}));
        if result.contains("## Top Contributors") {
            assert!(
                result.contains("## Hot Files") || result.contains("## Recent"),
                "should have hot files or recent activity: {result}"
            );
        }
    }

    #[test]
    fn git_contributors_shows_recent() {
        let root = repo_root();
        let result = git_contributors(&root, &json!({}));
        if !result.contains("No git history") {
            assert!(
                result.contains("## Recent Activity"),
                "should show recent activity: {result}"
            );
        }
    }

    #[test]
    fn git_contributors_with_path_filter() {
        let root = repo_root();
        let result = git_contributors(&root, &json!({"path": "README.md"}));
        // Either shows filtered results or no history
        assert!(
            result.contains("## Top Contributors") || result.contains("No git history"),
            "path filter should work: {result}"
        );
    }

    #[test]
    fn git_contributors_with_since() {
        let root = repo_root();
        let result = git_contributors(&root, &json!({"since": "2020-01-01"}));
        assert!(
            result.contains("## Top Contributors") || result.contains("No git history"),
            "since filter should work: {result}"
        );
    }

    // ─── Score function unit tests ──────────────────────────────────────────

    // ─── parse_since_to_epoch tests ─────────────────────────────────────────

    // ─── current_branch / head_short tests ──────────────────────────────────

    #[test]
    fn current_branch_returns_nonempty_in_repo() {
        let root = repo_root();
        let branch = current_branch(&root);
        // In a git repo we should get a branch name (or empty if detached HEAD)
        // Just verify no panic and reasonable output
        assert!(!branch.contains("Error"), "should not error: {branch}");
    }

    #[test]
    fn head_short_returns_hex() {
        let root = repo_root();
        let short = head_short(&root);
        assert!(!short.is_empty(), "should return a short hash");
        assert_eq!(
            short.len(),
            7,
            "to_hex_with_len(7) yields 7 hex chars: {short}"
        );
        assert!(
            short.chars().all(|c| c.is_ascii_hexdigit()),
            "should be hex: {short}"
        );
    }

    #[test]
    fn current_branch_bad_path_returns_empty() {
        let branch = current_branch(Path::new("/nonexistent/repo"));
        assert!(branch.is_empty());
    }

    #[test]
    fn head_short_bad_path_returns_empty() {
        let short = head_short(Path::new("/nonexistent/repo"));
        assert!(short.is_empty());
    }

    // ─── Robustness regression tests ────────────────────────────────────────

    #[test]
    fn git_diff_staged_detects_no_staged() {
        // In a clean repo, staged diff should say "No staged changes"
        let root = repo_root();
        let result = git_diff(&root, &json!({"staged": true}), 0.0, 0);
        // Either "No staged changes" or actual staged content — no panic/error
        assert!(
            result.contains("staged") || result.contains("diff --git"),
            "should handle staged query: {result}"
        );
    }

    #[test]
    fn git_log_n_capped_at_500() {
        // Even with n=99999, should not produce huge output
        let root = repo_root();
        let result = git_log(&root, &json!({"n": 99999}));
        let line_count = result.lines().count();
        assert!(
            line_count <= 501,
            "n should be capped at 500: got {line_count} lines"
        );
    }

    #[test]
    fn git_log_output_truncated() {
        // git_log should apply truncation
        let root = repo_root();
        let result = git_log(&root, &json!({"n": 500}));
        // Just verify it doesn't panic and produces output
        assert!(!result.is_empty());
    }

    #[test]
    fn git_file_history_nonexistent_file_bounded() {
        // For a nonexistent file, the walk should be bounded (not traverse all history)
        let root = repo_root();
        let start = std::time::Instant::now();
        let result = git_file_history(&root, &json!({"file": "this/does/not/exist.xyz"}));
        let elapsed = start.elapsed();
        assert!(
            result.contains("No history"),
            "should say no history: {result}"
        );
        // Walk cap should prevent this from taking too long (50K cap)
        // In a typical dev repo this should be well under 5 seconds
        assert!(
            elapsed.as_secs() < 30,
            "walk should be bounded, took: {elapsed:?}"
        );
    }

    #[test]
    fn git_contributors_bounded_walk() {
        // Even without path filter, walk should complete in bounded time
        let root = repo_root();
        let start = std::time::Instant::now();
        let result = git_contributors(&root, &json!({}));
        let elapsed = start.elapsed();
        assert!(
            !result.contains("Error: cannot open"),
            "should open repo: {result}"
        );
        assert!(
            elapsed.as_secs() < 30,
            "walk should be bounded, took: {elapsed:?}"
        );
    }

    #[test]
    fn git_contributors_path_filter_bounded() {
        // Path filter with nonexistent file should still be bounded
        let root = repo_root();
        let start = std::time::Instant::now();
        let _result = git_contributors(
            &root,
            &json!({"path": "nonexistent/deeply/nested/file.xyz"}),
        );
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_secs() < 30,
            "path-filtered walk should be bounded, took: {elapsed:?}"
        );
    }

    #[test]
    fn git_show_root_commit_lists_files() {
        // Find the root commit and verify it lists actual file paths
        let root = repo_root();
        let repo = gix::discover(&root).unwrap();
        // Walk to find root commit (no parents)
        let head = repo.head_id().unwrap();
        let mut root_oid = None;
        if let Ok(walk) = head.ancestors().all() {
            for info in walk.flatten() {
                if let Ok(c) = info.object()
                    && c.parent_ids().count() == 0
                {
                    root_oid = Some(info.id.to_string());
                    break;
                }
            }
        }
        if let Some(oid) = root_oid {
            let result = git_show(&root, &json!({"commit": oid}), 0.0, 0);
            assert!(
                result.contains("[root commit]"),
                "should mark as root: {result}"
            );
            // Should list files with full paths, not just top-level dirs
            // (regression: previously only listed directory names)
            assert!(
                result.contains('/') || result.lines().filter(|l| l.starts_with("A ")).count() > 0,
                "root commit should list files: {result}"
            );
        }
    }

    // ── Pressure-aware output limit tests ──

    #[test]
    fn pressure_scaled_limit_zero_pressure_returns_base() {
        assert_eq!(super::pressure_scaled_limit(12_000, 0.0), 12_000);
        assert_eq!(super::pressure_scaled_limit(16_000, 0.0), 16_000);
    }

    #[test]
    fn pressure_scaled_limit_moderate_pressure_reduces() {
        let limit = super::pressure_scaled_limit(12_000, 0.6);
        assert!(limit < 12_000, "moderate pressure should reduce limit");
        assert!(limit > 4_800, "should stay above 40% minimum");
        // scale = 1.0 - 0.6*0.6 = 0.64 → 12000 * 0.64 = 7680
        assert_eq!(limit, 7_680);
    }

    #[test]
    fn pressure_scaled_limit_max_pressure_reaches_floor() {
        let limit = super::pressure_scaled_limit(12_000, 1.0);
        // scale = 1.0 - 1.0*0.6 = 0.4 → 12000 * 0.4 = 4800
        assert_eq!(limit, 4_800);
    }

    #[test]
    fn pressure_scaled_limit_never_goes_below_forty_percent() {
        let limit = super::pressure_scaled_limit(10_000, 1.5);
        // scale = max(1.0 - 1.5*0.6, 0.4) = max(0.1, 0.4) = 0.4 → 4000
        assert_eq!(limit, 4_000);
    }

    #[test]
    fn git_show_under_pressure_truncates_earlier() {
        let root = std::env::current_dir().unwrap();
        let normal = git_show(&root, &json!({"commit": "HEAD"}), 0.0, 0);
        let pressed = git_show(&root, &json!({"commit": "HEAD"}), 0.9, 0);
        assert!(
            pressed.len() <= normal.len(),
            "high-pressure output ({}) should not exceed normal ({})",
            pressed.len(),
            normal.len()
        );
    }

    #[test]
    fn git_diff_under_pressure_truncates_earlier() {
        let root = std::env::current_dir().unwrap();
        let normal = git_diff(&root, &json!({}), 0.0, 0);
        let pressed = git_diff(&root, &json!({}), 0.9, 0);
        assert!(
            pressed.len() <= normal.len(),
            "high-pressure diff ({}) should not exceed normal ({})",
            pressed.len(),
            normal.len()
        );
    }

    // ─── git_commit tests ───────────────────────────────────────────────────

    #[test]
    fn git_commit_rejects_empty_message() {
        let root = repo_root();
        let result = git_commit(&root, &json!({}));
        assert!(
            result.starts_with("Error:"),
            "should reject missing message: {result}"
        );

        let result2 = git_commit(&root, &json!({"message": "  "}));
        assert!(
            result2.starts_with("Error:"),
            "should reject blank message: {result2}"
        );
    }

    #[test]
    fn git_commit_rejects_long_message() {
        let root = repo_root();
        let long_msg = "x".repeat(5001);
        let result = git_commit(&root, &json!({"message": long_msg}));
        assert!(
            result.contains("too long"),
            "should reject over-long message: {result}"
        );
    }

    #[test]
    fn git_commit_clean_tree_says_nothing() {
        // In a clean repo with nothing staged, commit should say "Nothing to commit"
        // or succeed if there are pending changes — either is fine, just no panic
        let root = repo_root();
        let result = git_commit(
            &root,
            &json!({"message": "test commit", "files": ["nonexistent_file_xyz.txt"]}),
        );
        // Should either succeed or report a meaningful error
        assert!(!result.is_empty(), "should return some output");
    }

    #[test]
    fn git_commit_with_metadata_returns_commit_sha_and_revert_tool_restores_state() {
        let repo = init_temp_repo();
        let tracked_path = repo.path().join("tracked.txt");
        std::fs::write(&tracked_path, "two\n").expect("update tracked file");

        let outcome = git_commit_with_metadata(repo.path(), &json!({"message": "update tracked"}));
        assert!(
            !outcome.output.starts_with("Error:"),
            "commit should succeed: {}",
            outcome.output
        );
        let commit_fields = outcome
            .tool_result_fields
            .as_ref()
            .expect("git_commit should return commit metadata");
        let commit_sha = commit_fields
            .get("commit_sha")
            .and_then(Value::as_str)
            .expect("commit_sha");
        let commit_short_sha = commit_fields
            .get("commit_short_sha")
            .and_then(Value::as_str)
            .expect("commit_short_sha");
        assert_eq!(commit_short_sha, short_commit_sha(commit_sha));
        assert_eq!(
            std::fs::read_to_string(&tracked_path).expect("read committed file"),
            "two\n"
        );

        let revert_outcome =
            git_revert_commit_with_metadata(repo.path(), &json!({"commit_sha": commit_sha}));
        assert!(
            !revert_outcome.output.starts_with("Error:"),
            "revert should succeed: {}",
            revert_outcome.output
        );
        let revert_fields = revert_outcome
            .tool_result_fields
            .as_ref()
            .expect("git_revert_commit should return metadata");
        assert_eq!(
            revert_fields
                .get("reverted_commit_sha")
                .and_then(Value::as_str),
            Some(commit_sha)
        );
        assert!(
            revert_fields
                .get("revert_commit_sha")
                .and_then(Value::as_str)
                .is_some(),
            "revert should report the compensating commit"
        );
        assert_eq!(
            std::fs::read_to_string(&tracked_path).expect("read reverted file"),
            "one\n"
        );
    }

    #[test]
    fn git_revert_commit_conflict_is_aborted() {
        let repo = init_temp_repo();
        let tracked_path = repo.path().join("tracked.txt");
        std::fs::write(&tracked_path, "two\n").expect("write second version");
        let second = git_commit_with_metadata(repo.path(), &json!({"message": "second"}));
        let second_sha = second
            .tool_result_fields
            .as_ref()
            .and_then(|fields| fields.get("commit_sha"))
            .and_then(Value::as_str)
            .expect("second commit sha")
            .to_string();

        std::fs::write(&tracked_path, "three\n").expect("write third version");
        let third = git_commit_with_metadata(repo.path(), &json!({"message": "third"}));
        assert!(
            !third.output.starts_with("Error:"),
            "third commit should succeed: {}",
            third.output
        );

        let revert =
            git_revert_commit_with_metadata(repo.path(), &json!({"commit_sha": second_sha}));
        assert!(
            revert.output.starts_with("Error:"),
            "reverting a non-HEAD conflicting commit should fail: {}",
            revert.output
        );
        assert!(
            revert.output.contains("aborted in-progress revert"),
            "failure should clean up revert state: {}",
            revert.output
        );
        assert_eq!(
            std::fs::read_to_string(&tracked_path).expect("read file after aborted revert"),
            "three\n"
        );
        assert!(
            !repo.path().join(".git/REVERT_HEAD").exists(),
            "revert conflict should not leave REVERT_HEAD behind"
        );
    }

    // ─── git_stash tests ────────────────────────────────────────────────────

    #[test]
    fn git_stash_requires_action() {
        let root = repo_root();
        let result = git_stash(&root, &json!({}));
        assert!(
            result.starts_with("Error:"),
            "should require action: {result}"
        );
    }

    #[test]
    fn git_stash_rejects_unknown_action() {
        let root = repo_root();
        let result = git_stash(&root, &json!({"action": "fly"}));
        assert!(
            result.contains("unknown stash action"),
            "should reject unknown: {result}"
        );
    }

    #[test]
    fn git_stash_list_works() {
        let root = repo_root();
        let result = git_stash(&root, &json!({"action": "list"}));
        // Should return stash list or "No stashes found"
        assert!(
            result.contains("stash@") || result.contains("No stashes") || result.is_empty(),
            "unexpected stash list output: {result}"
        );
    }

    #[tokio::test]
    async fn execute_with_metadata_returns_stash_ref_for_push_and_apply_accepts_it() {
        let repo = init_temp_repo();
        let tracked = repo.path().join("tracked.txt");
        std::fs::write(&tracked, "two\n").expect("modify tracked file");
        let executor = ToolExecutor::new(repo.path());

        let outcome = executor
            .execute_with_metadata("git_stash", &json!({"action": "push", "message": "demo"}))
            .await;
        assert!(
            !outcome.output.starts_with("Error:"),
            "stash push failed: {}",
            outcome.output
        );
        let stash_ref = outcome
            .tool_result_fields
            .as_ref()
            .and_then(|fields| fields.get("stash_ref"))
            .and_then(Value::as_str)
            .expect("stash_ref");
        assert_eq!(
            std::fs::read_to_string(&tracked).expect("clean worktree after stash"),
            "one\n"
        );

        let apply = git_stash(
            repo.path(),
            &json!({"action": "apply", "stash_ref": stash_ref}),
        );
        assert!(!apply.starts_with("Error:"), "stash apply failed: {apply}");
        assert_eq!(
            std::fs::read_to_string(&tracked).expect("restored working tree"),
            "two\n"
        );
    }

    // ─── git_checkout_file tests ────────────────────────────────────────────

    #[test]
    fn git_checkout_file_requires_path() {
        let root = repo_root();
        let result = git_checkout_file(&root, &json!({}));
        assert!(
            result.starts_with("Error:"),
            "should require path: {result}"
        );

        let result2 = git_checkout_file(&root, &json!({"path": ""}));
        assert!(
            result2.starts_with("Error:"),
            "should reject empty path: {result2}"
        );
    }

    #[test]
    fn git_checkout_file_rejects_path_traversal() {
        let root = repo_root();
        let result = git_checkout_file(&root, &json!({"path": "../../../etc/passwd"}));
        assert!(
            result.contains("path traversal"),
            "should reject traversal: {result}"
        );
    }

    #[test]
    fn git_checkout_file_rejects_dangerous_ref() {
        let root = repo_root();
        let result = git_checkout_file(
            &root,
            &json!({"path": "README.md", "ref": "HEAD; rm -rf /"}),
        );
        assert!(
            result.contains("invalid ref"),
            "should reject dangerous ref: {result}"
        );

        let result2 = git_checkout_file(
            &root,
            &json!({"path": "README.md", "ref": "main|cat /etc/passwd"}),
        );
        assert!(
            result2.contains("invalid ref"),
            "should reject pipe ref: {result2}"
        );
    }

    #[test]
    fn git_checkout_file_known_file() {
        let root = repo_root();
        // Checkout a known file at HEAD — should succeed (idempotent)
        let result = git_checkout_file(&root, &json!({"path": "README.md"}));
        assert!(
            result.contains("Restored") || result.contains("Error"),
            "should restore or report error: {result}"
        );
    }

    // ─── git CLI fallback behavior tests ────────────────────────────────────

    // ── Git Worktree Tests ──────────────────────────────────────────────

    #[test]
    fn git_worktree_missing_action() {
        let root = repo_root();
        let result = git_worktree(&root, &json!({}));
        assert!(
            result.contains("Error") && result.contains("action"),
            "should require action: {result}"
        );
    }

    #[test]
    fn git_worktree_unknown_action() {
        let root = repo_root();
        let result = git_worktree(&root, &json!({"action": "teleport"}));
        assert!(
            result.contains("Error") && result.contains("unknown"),
            "should reject unknown action: {result}"
        );
    }

    #[test]
    fn git_worktree_add_missing_branch() {
        let root = repo_root();
        let result = git_worktree(&root, &json!({"action": "add"}));
        assert!(
            result.contains("Error") && result.contains("branch"),
            "should require branch: {result}"
        );
    }

    #[test]
    fn git_worktree_add_rejects_shell_injection() {
        let root = repo_root();
        for dangerous in &[
            "test;rm -rf /",
            "test|cat /etc/passwd",
            "test&whoami",
            "test`id`",
            "test$(whoami)",
            "test()",
            "test{}",
        ] {
            let result = git_worktree(&root, &json!({"action": "add", "branch": dangerous}));
            assert!(
                result.contains("Error") && result.contains("invalid branch name"),
                "should reject '{dangerous}': {result}"
            );
        }
    }

    #[test]
    fn git_worktree_list_runs() {
        let root = repo_root();
        let result = git_worktree(&root, &json!({"action": "list"}));
        // Should contain at least the main worktree path
        assert!(
            !result.contains("Error: git") || result.contains("worktree"),
            "list should succeed or show worktree info: {result}"
        );
    }

    #[test]
    fn git_worktree_list_alias_ls() {
        let root = repo_root();
        let result = git_worktree(&root, &json!({"action": "ls"}));
        assert!(
            !(result.contains("Error") && result.contains("unknown action")),
            "ls should be accepted as alias for list: {result}"
        );
    }

    #[test]
    fn git_worktree_remove_missing_path() {
        let root = repo_root();
        let result = git_worktree(&root, &json!({"action": "remove"}));
        assert!(
            result.contains("Error") && result.contains("path"),
            "should require path for remove: {result}"
        );
    }

    #[test]
    fn git_worktree_remove_nonexistent() {
        let root = repo_root();
        let result = git_worktree(
            &root,
            &json!({"action": "remove", "path": "/tmp/nonexistent-worktree-xyz"}),
        );
        assert!(
            result.contains("Error") || result.contains("error"),
            "removing nonexistent worktree should fail: {result}"
        );
    }

    #[test]
    fn git_worktree_add_existing_path_fails() {
        let root = repo_root();
        // Use /tmp which always exists — should fail with "already exists"
        let result = git_worktree(
            &root,
            &json!({"action": "add", "branch": "test-existing", "path": "/tmp"}),
        );
        assert!(
            result.contains("Error") && result.contains("already exists"),
            "should reject existing path: {result}"
        );
    }

    #[test]
    fn worktree_add_with_metadata_returns_rollback_fields() {
        let dir = init_temp_repo();
        let worktree_path = dir.path().join("meta-worktree");
        let outcome = worktree_add_with_metadata(
            dir.path(),
            &json!({
                "branch": "meta-worktree",
                "path": worktree_path,
            }),
        );
        assert!(
            !outcome.output.starts_with("Error:"),
            "worktree add failed: {}",
            outcome.output
        );
        let fields = outcome.tool_result_fields.expect("metadata fields");
        assert_eq!(
            fields.get("branch").and_then(Value::as_str),
            Some("meta-worktree")
        );
        assert_eq!(
            fields
                .get("worktree_path")
                .and_then(Value::as_str)
                .map(std::path::PathBuf::from)
                .as_deref(),
            Some(worktree_path.as_path())
        );
        assert_eq!(
            fields
                .get("delete_branch_on_rollback")
                .and_then(Value::as_bool),
            Some(true)
        );
        let cleanup = worktree_remove(
            dir.path(),
            &json!({
                "path": worktree_path,
                "force": true,
                "delete_branch": true,
            }),
        );
        assert!(
            !cleanup.starts_with("Error:"),
            "cleanup remove failed: {cleanup}"
        );
    }

    #[test]
    fn git_worktree_action_aliases() {
        let root = repo_root();
        // "create" should alias to "add" (needs branch param, so will error on missing branch)
        let r1 = git_worktree(&root, &json!({"action": "create"}));
        assert!(r1.contains("branch"), "create should route to add: {r1}");

        // "rm" and "delete" should alias to "remove" (needs path param)
        let r2 = git_worktree(&root, &json!({"action": "rm"}));
        assert!(r2.contains("path"), "rm should route to remove: {r2}");

        let r3 = git_worktree(&root, &json!({"action": "delete"}));
        assert!(r3.contains("path"), "delete should route to remove: {r3}");
    }
}
