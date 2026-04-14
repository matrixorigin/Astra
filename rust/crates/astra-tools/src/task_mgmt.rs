//! Session-local task management for the CLI.
//!
//! Tasks are in-memory only — they survive across tool calls but not across
//! CLI restarts. Each task can contain subtasks with dependency tracking.

#![allow(dead_code)]
use serde_json::{Value, json};
use std::sync::{
    Mutex,
    atomic::{AtomicU32, Ordering},
};

/// A task tracked within the current CLI session.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionTask {
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub status: String,
    pub subtasks: Vec<SessionSubtask>,
    pub created_at: String,
    pub updated_at: String,
}

/// A subtask within a SessionTask.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionSubtask {
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub status: String,
    pub depends_on: Vec<String>,
}

/// In-memory task store for the current session.
pub(crate) struct TaskManager {
    tasks: Mutex<Vec<SessionTask>>,
    id_counter: AtomicU32,
}

#[derive(Debug, Clone)]
pub(crate) struct TaskManagerSnapshot {
    pub tasks: Vec<SessionTask>,
    pub next_task_id: u32,
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

        let task = SessionTask {
            id: task_id.clone(),
            title: title.clone(),
            description,
            status: "pending".to_string(),
            subtasks,
            created_at: now.clone(),
            updated_at: now,
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
    pub async fn list(&self, args: &Value) -> String {
        let status_filter = args.get("status").and_then(Value::as_str).unwrap_or("all");

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
                json!({
                    "id": t.id,
                    "title": t.title,
                    "status": t.status,
                    "subtasks": subtask_summary,
                    "updated_at": t.updated_at,
                })
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

        let new_status = args.get("status").and_then(Value::as_str);
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
