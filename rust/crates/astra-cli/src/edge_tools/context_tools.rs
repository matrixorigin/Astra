//! Context tools: share_context, query_context, and brief for session state.

use serde_json::{Value, json};

use super::{AGGREGATE_OUTPUT_BUDGET, ToolExecutor};
use astra_tools::task_mgmt::SessionTask;

fn task_brief_item(task: &SessionTask) -> Value {
    json!({
        "id": task.id,
        "title": task.title,
        "status": task.status,
        "subtasks": task.subtasks.len(),
        "updated_at": task.updated_at,
    })
}

fn prioritized_task_brief_items(tasks: &[SessionTask], max_items: usize) -> Vec<Value> {
    let mut picked: Vec<&SessionTask> = tasks
        .iter()
        .filter(|task| task.status.is_open_work())
        .take(max_items)
        .collect();
    if picked.len() < max_items {
        picked.extend(
            tasks
                .iter()
                .filter(|task| !task.status.is_open_work())
                .take(max_items - picked.len()),
        );
    }
    let mut items: Vec<Value> = picked.into_iter().map(task_brief_item).collect();
    if tasks.len() > items.len() {
        items.push(json!({
            "more": tasks.len() - items.len()
        }));
    }
    items
}

impl ToolExecutor {
    /// Share context with other agents via SharedContextCache.
    pub(super) fn share_context(&self, args: &Value) -> String {
        let cache = match &self.context_cache {
            Some(c) => c,
            None => {
                return json!({
                    "success": false,
                    "error": "Context sharing not available - no cache configured"
                })
                .to_string();
            }
        };
        let agent_id = self.agent_id.as_deref().unwrap_or("unknown");
        super::context_sharing::execute_share_context(cache, agent_id, args).to_string()
    }

    /// Query shared context from other agents via SharedContextCache.
    pub(super) fn query_context(&self, args: &Value) -> String {
        let cache = match &self.context_cache {
            Some(c) => c,
            None => {
                return json!({
                    "success": false,
                    "error": "Context sharing not available - no cache configured"
                })
                .to_string();
            }
        };
        super::context_sharing::execute_query_context(cache, args).to_string()
    }

    /// Return a compact summary of the current session state.
    pub(super) async fn brief(&self, args: &Value) -> String {
        let focus = args.get("focus").and_then(Value::as_str).unwrap_or("all");
        let max_items = args
            .get("max_items")
            .and_then(Value::as_u64)
            .map(|n| n.clamp(1, 20) as usize)
            .unwrap_or(5);

        let effective_root = self.effective_project_root();
        let mut result = serde_json::Map::new();
        result.insert(
            "effective_project_root".to_string(),
            json!(effective_root.display().to_string()),
        );

        if focus == "all" || focus == "session" {
            result.insert(
                "session".to_string(),
                json!({
                    "in_worktree_session": self.in_worktree_session(),
                    "aggregate_output_bytes": self.aggregate_output_bytes.load(std::sync::atomic::Ordering::Relaxed),
                    "aggregate_output_budget": AGGREGATE_OUTPUT_BUDGET,
                    "scaled_output_limit": self.scaled_output_limit(),
                }),
            );

            if let Some(worktree) = self.get_worktree_session() {
                result.insert(
                    "worktree".to_string(),
                    json!({
                        "path": worktree.worktree_path.display().to_string(),
                        "branch": worktree.branch_name,
                        "original_root": worktree.original_root.display().to_string(),
                        "baseline_commit": worktree.original_head_commit,
                    }),
                );
            }
        }

        if focus == "all" || focus == "git" {
            let branch = std::process::Command::new("git")
                .args(["branch", "--show-current"])
                .current_dir(&effective_root)
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|s| !s.is_empty());

            let porcelain = std::process::Command::new("git")
                .args(["status", "--porcelain"])
                .current_dir(&effective_root)
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .unwrap_or_default();

            let mut modified = 0usize;
            let mut added = 0usize;
            let mut deleted = 0usize;
            let mut untracked = 0usize;
            let mut renamed = 0usize;
            for line in porcelain.lines() {
                if line.starts_with(super::git_status::UNTRACKED_PREFIX) {
                    untracked += 1;
                    continue;
                }
                let x = line.chars().next().unwrap_or(' ');
                let y = line.chars().nth(1).unwrap_or(' ');
                for status in [x, y] {
                    match status {
                        super::git_status::MODIFIED => modified += 1,
                        super::git_status::ADDED => added += 1,
                        super::git_status::DELETED => deleted += 1,
                        super::git_status::RENAMED => renamed += 1,
                        _ => {}
                    }
                }
            }

            result.insert(
                "git".to_string(),
                json!({
                    "branch": branch,
                    "modified": modified,
                    "added": added,
                    "deleted": deleted,
                    "renamed": renamed,
                    "untracked": untracked,
                    "dirty": !porcelain.trim().is_empty(),
                }),
            );
        }

        if focus == "all" || focus == "tasks" {
            match self.task_manager.load_tasks().await {
                Ok(tasks) => {
                    let open_work_count = tasks
                        .iter()
                        .filter(|task| task.status.is_open_work())
                        .count();
                    let task_summaries = prioritized_task_brief_items(&tasks, max_items);
                    result.insert(
                        "tasks".to_string(),
                        json!({
                            "available": true,
                            "count": tasks.len(),
                            "open_work_count": open_work_count,
                            "items": task_summaries,
                        }),
                    );
                }
                Err(error) => {
                    result.insert(
                        "tasks".to_string(),
                        json!({
                            "available": false,
                            "error": error,
                            "message": "Task board could not be loaded; do not treat this as zero tasks.",
                        }),
                    );
                }
            }
        }

        if focus == "all" || focus == "files" {
            let recent_files: Vec<String> = self
                .recently_read_files(max_items)
                .into_iter()
                .map(|p| {
                    p.strip_prefix(&effective_root)
                        .unwrap_or(&p)
                        .display()
                        .to_string()
                })
                .collect();
            result.insert(
                "files".to_string(),
                json!({
                    "recently_read": recent_files,
                }),
            );
        }

        Value::Object(result).to_string()
    }
}
