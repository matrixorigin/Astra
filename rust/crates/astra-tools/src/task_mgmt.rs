//! Explicit scratchpad task management for the CLI.
//!
//! Runtime-owned continuity state is the authoritative source for agent progress.
//! These tools are only an explicit user/model scratchpad and must not be relied
//! on for multi-turn continuity or resume.

#![allow(dead_code)]
use serde_json::{Value, json};
use std::sync::{
    Mutex,
    atomic::{AtomicU32, Ordering},
};

/// A scratchpad task tracked within the current CLI session.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionTask {
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub status: String,
    pub subtasks: Vec<SessionSubtask>,
    pub created_at: String,
    pub updated_at: String,
    /// Present-continuous form shown in spinner while in_progress.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_form: Option<String>,
    /// Which agent owns this task (for multi-agent sessions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// Arbitrary key-value metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Map<String, Value>>,
    /// Task IDs that this task blocks (cannot start until this completes).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocks: Vec<String>,
    /// Task IDs that must complete before this task can start.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_by: Vec<String>,
}

/// A subtask within a SessionTask.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionSubtask {
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub status: String,
    pub depends_on: Vec<String>,
}

/// In-memory scratchpad store for the current session.
pub struct TaskManager {
    tasks: Mutex<Vec<SessionTask>>,
    id_counter: AtomicU32,
}

#[derive(Debug, Clone)]
pub struct TaskManagerSnapshot {
    pub tasks: Vec<SessionTask>,
    pub next_task_id: u32,
}

impl Default for TaskManager {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskManager {
    pub fn new() -> Self {
        Self {
            tasks: Mutex::new(Vec::new()),
            id_counter: AtomicU32::new(1),
        }
    }

    /// Get a snapshot of all tasks (for brief/diagnostics).
    pub fn snapshot(&self) -> Vec<SessionTask> {
        self.tasks.lock().map(|g| g.clone()).unwrap_or_default()
    }

    pub fn snapshot_state(&self) -> TaskManagerSnapshot {
        TaskManagerSnapshot {
            tasks: self.snapshot(),
            next_task_id: self.id_counter.load(Ordering::SeqCst),
        }
    }

    pub fn restore_snapshot(&self, snapshot: &TaskManagerSnapshot) -> Result<(), String> {
        let mut tasks = self
            .tasks
            .lock()
            .map_err(|_| "failed to access task list".to_string())?;
        *tasks = snapshot.tasks.clone();
        self.id_counter
            .store(snapshot.next_task_id, Ordering::SeqCst);
        Ok(())
    }

    /// Create a new task in the session-local task list.
    pub async fn create(&self, args: &Value) -> String {
        let title = match args.get("title").and_then(Value::as_str) {
            Some(t) if !t.is_empty() => t.to_string(),
            _ => return "Error: 'title' is required".to_string(),
        };

        let description = args
            .get("description")
            .and_then(Value::as_str)
            .map(String::from);
        let now = chrono::Utc::now().to_rfc3339();

        let subtasks: Vec<SessionSubtask> = args
            .get("subtasks")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|st| {
                        let id = st.get("id").and_then(Value::as_str)?;
                        let title = st.get("title").and_then(Value::as_str)?;
                        Some(SessionSubtask {
                            id: id.to_string(),
                            title: title.to_string(),
                            description: st
                                .get("description")
                                .and_then(Value::as_str)
                                .map(String::from),
                            status: "pending".to_string(),
                            depends_on: st
                                .get("depends_on")
                                .and_then(Value::as_array)
                                .map(|deps| {
                                    deps.iter()
                                        .filter_map(Value::as_str)
                                        .map(String::from)
                                        .collect()
                                })
                                .unwrap_or_default(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        let task_id = format!("task-{}", self.id_counter.fetch_add(1, Ordering::SeqCst));

        let active_form = args
            .get("active_form")
            .and_then(Value::as_str)
            .map(String::from);
        let owner = args.get("owner").and_then(Value::as_str).map(String::from);
        let metadata = args.get("metadata").and_then(Value::as_object).cloned();

        let task = SessionTask {
            id: task_id.clone(),
            title: title.clone(),
            description,
            status: "pending".to_string(),
            subtasks,
            created_at: now.clone(),
            updated_at: now,
            active_form,
            owner,
            metadata,
            blocks: Vec::new(),
            blocked_by: Vec::new(),
        };

        if let Ok(mut tasks) = self.tasks.lock() {
            tasks.push(task);
        }

        json!({
            "success": true,
            "task_id": task_id,
            "message": format!("Task '{}' created successfully", title)
        })
        .to_string()
    }

    /// List tasks in the session, optionally filtered by status.
    ///
    /// Prefers `status_filter`; legacy `status` is no longer accepted on list.
    pub async fn list(&self, args: &Value) -> String {
        let status_filter = args
            .get("status_filter")
            .and_then(Value::as_str)
            .unwrap_or("all");

        let tasks = match self.tasks.lock() {
            Ok(guard) => guard.clone(),
            Err(_) => return "Error: failed to access task list".to_string(),
        };

        let filtered: Vec<_> = tasks
            .iter()
            .filter(|t| match status_filter {
                "all" => true,
                "active" => t.status == "pending" || t.status == "in_progress",
                s => t.status == s,
            })
            .map(|t| {
                let subtask_summary = if t.subtasks.is_empty() {
                    String::new()
                } else {
                    let done = t
                        .subtasks
                        .iter()
                        .filter(|st| st.status == "completed")
                        .count();
                    format!(" [{}/{}]", done, t.subtasks.len())
                };
                let mut entry = json!({
                    "id": t.id,
                    "title": t.title,
                    "status": t.status,
                    "subtasks": subtask_summary,
                    "updated_at": t.updated_at,
                });
                if let Some(ref owner) = t.owner {
                    entry["owner"] = json!(owner);
                }
                if !t.blocked_by.is_empty() {
                    entry["blocked_by"] = json!(t.blocked_by);
                }
                entry
            })
            .collect();

        if filtered.is_empty() {
            return format!("No tasks found with status '{}'", status_filter);
        }

        json!({
            "count": filtered.len(),
            "tasks": filtered
        })
        .to_string()
    }

    /// Get full details of a task by ID.
    pub async fn get(&self, args: &Value) -> String {
        let task_id = match args.get("task_id").and_then(Value::as_str) {
            Some(id) if !id.is_empty() => id,
            _ => return "Error: 'task_id' is required".to_string(),
        };

        let tasks = match self.tasks.lock() {
            Ok(guard) => guard.clone(),
            Err(_) => return "Error: failed to access task list".to_string(),
        };

        match tasks.iter().find(|t| t.id == task_id) {
            Some(task) => serde_json::to_string_pretty(task)
                .unwrap_or_else(|_| "Error: serialization failed".to_string()),
            None => format!("Error: task '{}' not found", task_id),
        }
    }

    /// Update a task's status or a specific subtask's status.
    pub async fn update(&self, args: &Value) -> String {
        let task_id = match args.get("task_id").and_then(Value::as_str) {
            Some(id) if !id.is_empty() => id,
            _ => return "Error: 'task_id' is required".to_string(),
        };

        // Update accepts only `new_status`. The legacy `status` key
        // used to serve both list and update; schema is now split.
        let new_status = args.get("new_status").and_then(Value::as_str);
        // Reject terminal-only filters that used to share the enum.
        if matches!(new_status, Some("all") | Some("active")) {
            return format!(
                "Error: invalid new_status '{}' (valid: pending|in_progress|completed|failed|deleted)",
                new_status.unwrap()
            );
        }
        let subtask_id = args.get("subtask_id").and_then(Value::as_str);
        let error_message = args.get("error_message").and_then(Value::as_str);
        let now = chrono::Utc::now().to_rfc3339();

        let mut tasks = match self.tasks.lock() {
            Ok(guard) => guard,
            Err(_) => return "Error: failed to access task list".to_string(),
        };

        let task = match tasks.iter_mut().find(|t| t.id == task_id) {
            Some(t) => t,
            None => return format!("Error: task '{}' not found", task_id),
        };

        if let Some(st_id) = subtask_id {
            // Update subtask
            match task.subtasks.iter_mut().find(|st| st.id == st_id) {
                Some(subtask) => {
                    let previous_status = subtask.status.clone();
                    if let Some(status) = new_status {
                        subtask.status = status.to_string();
                    }
                    task.updated_at = now;
                    return json!({
                        "success": true,
                        "task_id": task_id,
                        "subtask_id": st_id,
                        "previous_status": previous_status,
                        "status": subtask.status,
                        "message": format!("Subtask '{}' updated to '{}'", st_id, subtask.status)
                    })
                    .to_string();
                }
                None => {
                    return format!("Error: subtask '{}' not found in task '{}'", st_id, task_id);
                }
            }
        }

        // Handle "deleted" — soft-remove from list (before taking &mut)
        if new_status == Some("deleted") {
            let previous_status = task.status.clone();
            let task_id_owned = task_id.to_string();
            tasks.retain(|t| t.id != task_id_owned);
            return json!({
                "success": true,
                "task_id": task_id_owned,
                "previous_status": previous_status,
                "status": "deleted",
                "message": format!("Task '{}' deleted", task_id_owned)
            })
            .to_string();
        }

        // Update main task
        let previous_status = task.status.clone();
        if let Some(status) = new_status {
            task.status = status.to_string();
        }
        if let Some(err) = error_message {
            task.description = Some(format!(
                "{}\n\nError: {}",
                task.description.as_deref().unwrap_or(""),
                err
            ));
        }

        // Update title/description if provided
        if let Some(title) = args.get("title").and_then(Value::as_str) {
            task.title = title.to_string();
        }
        if let Some(desc) = args.get("description").and_then(Value::as_str) {
            task.description = Some(desc.to_string());
        }

        // activeForm
        if let Some(af) = args.get("active_form").and_then(Value::as_str) {
            task.active_form = Some(af.to_string());
        }

        // Owner
        if let Some(owner) = args.get("owner").and_then(Value::as_str) {
            task.owner = Some(owner.to_string());
        }

        // Metadata (merge, not replace — set key to null to delete)
        if let Some(meta_update) = args.get("metadata").and_then(Value::as_object) {
            let meta = task.metadata.get_or_insert_with(serde_json::Map::new);
            for (k, v) in meta_update {
                if v.is_null() {
                    meta.remove(k);
                } else {
                    meta.insert(k.clone(), v.clone());
                }
            }
            if meta.is_empty() {
                task.metadata = None;
            }
        }

        // Blocking dependencies (additive, with cycle detection and
        // symmetric removal support).
        //
        // Edges represent `A blocks B`  ⇔  `B blocked_by A`. We maintain both
        // views in sync; cycles are rejected before mutation.
        let self_id = task_id.to_string();

        // Collect proposed new edges first so we can validate them against
        // the current graph without partial mutation.
        let mut proposed_blocks: Vec<String> = Vec::new();
        let mut proposed_blocked_by: Vec<String> = Vec::new();
        if let Some(add_blocks) = args.get("add_blocks").and_then(Value::as_array) {
            for id in add_blocks.iter().filter_map(Value::as_str) {
                let id = id.to_string();
                if id == self_id {
                    return format!("Error: task '{}' cannot block itself", self_id);
                }
                if !task.blocks.contains(&id) && !proposed_blocks.contains(&id) {
                    proposed_blocks.push(id);
                }
            }
        }
        if let Some(add_blocked_by) = args.get("add_blocked_by").and_then(Value::as_array) {
            for id in add_blocked_by.iter().filter_map(Value::as_str) {
                let id = id.to_string();
                if id == self_id {
                    return format!("Error: task '{}' cannot be blocked by itself", self_id);
                }
                if !task.blocked_by.contains(&id) && !proposed_blocked_by.contains(&id) {
                    proposed_blocked_by.push(id);
                }
            }
        }

        // Removals
        let remove_blocks: Vec<String> = args
            .get("remove_blocks")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        let remove_blocked_by: Vec<String> = args
            .get("remove_blocked_by")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();

        // Release the mutable borrow on `task` so we can re-scan the list
        // for cycle detection. We'll re-acquire it below after validation.
        let _ = task;

        // Build an adjacency map for "blocked_by" edges (who must finish before
        // the key). We overlay proposed additions and removals.
        if !proposed_blocks.is_empty() || !proposed_blocked_by.is_empty() {
            use std::collections::{HashMap, HashSet, VecDeque};
            let mut blocked_by: HashMap<String, HashSet<String>> = HashMap::new();
            for t in tasks.iter() {
                blocked_by
                    .entry(t.id.clone())
                    .or_default()
                    .extend(t.blocked_by.iter().cloned());
            }
            // Apply removals to the projection
            let entry = blocked_by.entry(self_id.clone()).or_default();
            for r in &remove_blocked_by {
                entry.remove(r);
            }
            // `self blocks X` ⇒ `X blocked_by self`
            for x in &proposed_blocks {
                blocked_by
                    .entry(x.clone())
                    .or_default()
                    .insert(self_id.clone());
            }
            // `self blocked_by Y` ⇒ add Y to self's set
            for y in &proposed_blocked_by {
                blocked_by
                    .entry(self_id.clone())
                    .or_default()
                    .insert(y.clone());
            }
            // Cycle check: BFS from self over "blocked_by" — if we reach self,
            // there's a cycle (self depends on something that depends on self).
            let mut visited: HashSet<String> = HashSet::new();
            let mut queue: VecDeque<String> = VecDeque::new();
            if let Some(seeds) = blocked_by.get(&self_id) {
                for s in seeds {
                    queue.push_back(s.clone());
                }
            }
            while let Some(node) = queue.pop_front() {
                if node == self_id {
                    return format!(
                        "Error: adding these dependencies would create a cycle involving '{}'",
                        self_id
                    );
                }
                if !visited.insert(node.clone()) {
                    continue;
                }
                if let Some(next) = blocked_by.get(&node) {
                    for n in next {
                        queue.push_back(n.clone());
                    }
                }
            }
        }

        // All validations passed — apply mutations to self.
        let task = match tasks.iter_mut().find(|t| t.id == self_id) {
            Some(t) => t,
            None => return format!("Error: task '{}' not found", self_id),
        };
        for id in proposed_blocks {
            task.blocks.push(id);
        }
        for id in proposed_blocked_by {
            task.blocked_by.push(id);
        }
        task.blocks.retain(|b| !remove_blocks.contains(b));
        task.blocked_by.retain(|b| !remove_blocked_by.contains(b));

        task.updated_at = now;

        // Auto-complete task if all subtasks are completed
        if !task.subtasks.is_empty() && task.subtasks.iter().all(|st| st.status == "completed") {
            task.status = "completed".to_string();
        }

        json!({
            "success": true,
            "task_id": task_id,
            "previous_status": previous_status,
            "status": task.status,
            "message": format!("Task '{}' updated to '{}'", task_id, task.status)
        })
        .to_string()
    }

    /// Stop/cancel a running task.
    pub async fn stop(&self, args: &Value) -> String {
        let task_id = match args.get("task_id").and_then(Value::as_str) {
            Some(id) if !id.is_empty() => id,
            _ => return "Error: 'task_id' is required".to_string(),
        };

        let reason = args
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("user requested");
        let now = chrono::Utc::now().to_rfc3339();

        let mut tasks = match self.tasks.lock() {
            Ok(guard) => guard,
            Err(_) => return "Error: failed to access task list".to_string(),
        };

        let task = match tasks.iter_mut().find(|t| t.id == task_id) {
            Some(t) => t,
            None => return format!("Error: task '{}' not found", task_id),
        };

        // Only allow stopping tasks that are running or pending
        if task.status != "pending" && task.status != "in_progress" {
            return json!({
                "success": false,
                "message": format!("Cannot stop task '{}': status is '{}' (only 'pending' or 'in_progress' can be stopped)", task_id, task.status)
            })
            .to_string();
        }

        let previous_status = task.status.clone();
        task.status = "cancelled".to_string();
        task.description = Some(format!(
            "{}\n\nCancelled: {} (was: {})",
            task.description.as_deref().unwrap_or(""),
            reason,
            previous_status
        ));
        task.updated_at = now;

        // Also cancel any in-progress subtasks
        let mut cancelled_subtasks = 0;
        for subtask in &mut task.subtasks {
            if subtask.status == "pending" || subtask.status == "in_progress" {
                subtask.status = "cancelled".to_string();
                cancelled_subtasks += 1;
            }
        }

        json!({
            "success": true,
            "task_id": task_id,
            "previous_status": previous_status,
            "reason": reason,
            "cancelled_subtasks": cancelled_subtasks,
            "message": format!("Task '{}' cancelled (was: {})", task_id, previous_status)
        })
        .to_string()
    }
}
