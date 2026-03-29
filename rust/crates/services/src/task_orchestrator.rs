//! Long-task orchestration: decompose, track, pause, resume across sessions.
//!
//! # Architecture
//!
//! ```text
//! TaskOrchestrator
//!   ├─ create_task(title, plan)     → task_id
//!   ├─ add_subtask(parent, title)   → subtask_id
//!   ├─ update_progress(task_id, %)  → ok
//!   ├─ checkpoint(task_id, state)   → ok (saves resumable state)
//!   ├─ pause(task_id)               → ok (status → paused)
//!   ├─ resume(task_id)              → TaskState (load checkpoint + continue)
//!   └─ complete(task_id, summary)   → ok
//! ```
//!
//! Tasks survive session boundaries. A new session can resume a paused task
//! by loading its checkpoint_json and continuing from the last saved state.
//!
//! # Storage
//!
//! - `agent_tasks` table in MatrixOne (DDL in storage.rs)
//! - Local fallback: `~/.mo-agent/tasks/{task_id}.json`

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

// ─── Task Model ─────────────────────────────────────────────────────────────

/// Task lifecycle status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    #[default]
    Pending,
    InProgress,
    Paused,
    Completed,
    Failed,
    Cancelled,
}

impl TaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse_status(s: &str) -> Self {
        match s {
            "pending" => Self::Pending,
            "in_progress" => Self::InProgress,
            "paused" => Self::Paused,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            _ => Self::Pending,
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

/// A single subtask within a plan.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubtaskPlan {
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub depends_on: Vec<String>,
    pub status: TaskStatus,
    /// Estimated effort: "small" (< 30 lines), "medium" (30-100), "large" (100+)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Files likely to be modified (relative paths)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    /// Acceptance criteria — how to verify this subtask is done
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acceptance: Option<String>,
}

/// Decomposed plan for a complex task.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskPlan {
    pub subtasks: Vec<SubtaskPlan>,
    /// Free-form notes about the approach.
    pub notes: Option<String>,
}

impl TaskPlan {
    /// Get subtasks that are ready to execute (all deps completed).
    pub fn ready_subtasks(&self) -> Vec<&SubtaskPlan> {
        self.subtasks
            .iter()
            .filter(|st| st.status == TaskStatus::Pending)
            .filter(|st| {
                st.depends_on.iter().all(|dep_id| {
                    self.subtasks
                        .iter()
                        .any(|d| d.id == *dep_id && d.status == TaskStatus::Completed)
                })
            })
            .collect()
    }

    /// Compute overall progress as percentage.
    pub fn progress_pct(&self) -> u32 {
        if self.subtasks.is_empty() {
            return 0;
        }
        let done = self
            .subtasks
            .iter()
            .filter(|st| st.status.is_terminal())
            .count();
        ((done as f64 / self.subtasks.len() as f64) * 100.0) as u32
    }

    /// Count completed items.
    pub fn items_done(&self) -> u32 {
        self.subtasks
            .iter()
            .filter(|st| st.status == TaskStatus::Completed)
            .count() as u32
    }
}

/// Resumable checkpoint: the state needed to continue a task.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskCheckpoint {
    /// Which subtask was last active.
    pub active_subtask_id: Option<String>,
    /// Turn number within the session when checkpointed.
    pub turn: u32,
    /// Session ID that created this checkpoint.
    pub session_id: Option<String>,
    /// Arbitrary key-value state for the task.
    pub state: serde_json::Map<String, serde_json::Value>,
}

/// Full task record (matches `agent_tasks` table).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    pub task_id: String,
    pub user_id: String,
    pub session_id: Option<String>,
    pub parent_task_id: Option<String>,
    pub title: String,
    pub description: Option<String>,
    pub status: TaskStatus,
    pub progress_pct: u32,
    pub items_done: u32,
    pub items_total: u32,
    pub plan: Option<TaskPlan>,
    pub checkpoint: Option<TaskCheckpoint>,
    pub error_message: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub completed_at: Option<String>,
}

/// Request to create a new task.
pub struct TaskCreateRequest {
    pub title: String,
    pub description: Option<String>,
    pub plan: Option<TaskPlan>,
    pub parent_task_id: Option<String>,
}

// ─── Task Service Trait ─────────────────────────────────────────────────────

#[async_trait]
pub trait TaskService: Send + Sync {
    /// Create a new task. Returns task_id.
    async fn create_task(
        &self,
        user_id: &str,
        session_id: &str,
        req: TaskCreateRequest,
    ) -> Result<String, String>;

    /// Get a task by ID.
    async fn get_task(&self, task_id: &str) -> Result<Option<TaskRecord>, String>;

    /// List tasks for a user (optionally filter by status).
    async fn list_tasks(
        &self,
        user_id: &str,
        status_filter: Option<TaskStatus>,
    ) -> Result<Vec<TaskRecord>, String>;

    /// Update task status.
    async fn update_status(&self, task_id: &str, status: TaskStatus) -> Result<(), String>;

    /// Update progress counters.
    async fn update_progress(
        &self,
        task_id: &str,
        progress_pct: u32,
        items_done: u32,
        items_total: u32,
    ) -> Result<(), String>;

    /// Save a checkpoint (resumable state).
    async fn save_checkpoint(
        &self,
        task_id: &str,
        checkpoint: &TaskCheckpoint,
    ) -> Result<(), String>;

    /// Update the plan (e.g., mark subtask as done).
    async fn update_plan(&self, task_id: &str, plan: &TaskPlan) -> Result<(), String>;

    /// Mark task as failed with error message.
    async fn fail_task(&self, task_id: &str, error: &str) -> Result<(), String>;

    /// Mark task as completed.
    async fn complete_task(&self, task_id: &str) -> Result<(), String>;
}

// ─── MatrixOne Implementation ───────────────────────────────────────────────

/// Task service backed by MatrixOne `agent_tasks` table.
pub struct MatrixOneTaskService {
    pool: sqlx::Pool<sqlx::MySql>,
}

impl MatrixOneTaskService {
    pub fn new(pool: sqlx::Pool<sqlx::MySql>) -> Self {
        Self { pool }
    }

    pub fn from_shared(shared: &mo_agent_core::SharedPool) -> Self {
        Self {
            pool: shared.get().clone(),
        }
    }

    fn record_from_row(row: &sqlx::mysql::MySqlRow) -> Result<TaskRecord, String> {
        use sqlx::Row;

        let plan_json: Option<String> = row.try_get("plan_json").ok().flatten();
        let plan: Option<TaskPlan> = plan_json
            .as_deref()
            .and_then(|j| serde_json::from_str(j).ok());

        let ckpt_json: Option<String> = row.try_get("checkpoint_json").ok().flatten();
        let checkpoint: Option<TaskCheckpoint> = ckpt_json
            .as_deref()
            .and_then(|j| serde_json::from_str(j).ok());

        let status_str: String = row.try_get("status").map_err(|e| e.to_string())?;

        Ok(TaskRecord {
            task_id: row.try_get("task_id").map_err(|e| e.to_string())?,
            user_id: row.try_get("user_id").map_err(|e| e.to_string())?,
            session_id: row.try_get("session_id").ok().flatten(),
            parent_task_id: row.try_get("parent_task_id").ok().flatten(),
            title: row.try_get("title").map_err(|e| e.to_string())?,
            description: row.try_get("description").ok().flatten(),
            status: TaskStatus::parse_status(&status_str),
            progress_pct: row.try_get::<i32, _>("progress_pct").unwrap_or(0) as u32,
            items_done: row.try_get::<i32, _>("items_done").unwrap_or(0) as u32,
            items_total: row.try_get::<i32, _>("items_total").unwrap_or(0) as u32,
            plan,
            checkpoint,
            error_message: row.try_get("error_message").ok().flatten(),
            created_at: row.try_get::<String, _>("created_at").unwrap_or_default(),
            updated_at: row.try_get::<String, _>("updated_at").unwrap_or_default(),
            completed_at: row.try_get("completed_at").ok().flatten(),
        })
    }
}

#[async_trait]
impl TaskService for MatrixOneTaskService {
    async fn create_task(
        &self,
        user_id: &str,
        session_id: &str,
        req: TaskCreateRequest,
    ) -> Result<String, String> {
        let task_id = uuid::Uuid::new_v4().to_string();
        let plan_json = req
            .plan
            .as_ref()
            .and_then(|p| serde_json::to_string(p).ok());
        let items_total = req
            .plan
            .as_ref()
            .map(|p| p.subtasks.len() as i64)
            .unwrap_or(0);

        sqlx::query(
            "INSERT INTO agent_tasks \
             (task_id, user_id, session_id, parent_task_id, title, description, status, \
              items_total, plan_json, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, 'pending', ?, ?, NOW(), NOW())",
        )
        .bind(&task_id)
        .bind(user_id)
        .bind(session_id)
        .bind(&req.parent_task_id)
        .bind(&req.title)
        .bind(&req.description)
        .bind(items_total)
        .bind(&plan_json)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("create_task: {e}"))?;

        Ok(task_id)
    }

    async fn get_task(&self, task_id: &str) -> Result<Option<TaskRecord>, String> {
        let row = sqlx::query("SELECT * FROM agent_tasks WHERE task_id = ?")
            .bind(task_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| format!("get_task: {e}"))?;

        match row {
            Some(ref r) => Ok(Some(Self::record_from_row(r)?)),
            None => Ok(None),
        }
    }

    async fn list_tasks(
        &self,
        user_id: &str,
        status_filter: Option<TaskStatus>,
    ) -> Result<Vec<TaskRecord>, String> {
        let rows = if let Some(status) = status_filter {
            sqlx::query(
                "SELECT task_id, user_id, session_id, parent_task_id, title, description, \
                 status, progress_pct, items_done, items_total, plan_json, checkpoint_json, \
                 error_message, \
                 CAST(created_at AS CHAR) AS created_at, \
                 CAST(updated_at AS CHAR) AS updated_at, \
                 completed_at \
                 FROM agent_tasks WHERE user_id = ? AND status = ? ORDER BY updated_at DESC",
            )
            .bind(user_id)
            .bind(status.as_str())
            .fetch_all(&self.pool)
            .await
        } else {
            sqlx::query(
                "SELECT task_id, user_id, session_id, parent_task_id, title, description, \
                 status, progress_pct, items_done, items_total, plan_json, checkpoint_json, \
                 error_message, \
                 CAST(created_at AS CHAR) AS created_at, \
                 CAST(updated_at AS CHAR) AS updated_at, \
                 completed_at \
                 FROM agent_tasks WHERE user_id = ? ORDER BY updated_at DESC",
            )
            .bind(user_id)
            .fetch_all(&self.pool)
            .await
        }
        .map_err(|e| format!("list_tasks: {e}"))?;

        rows.iter().map(Self::record_from_row).collect()
    }

    async fn update_status(&self, task_id: &str, status: TaskStatus) -> Result<(), String> {
        let completed_at = if status.is_terminal() {
            "NOW()"
        } else {
            "NULL"
        };
        let sql = format!(
            "UPDATE agent_tasks SET status = ?, updated_at = NOW(), completed_at = {} WHERE task_id = ?",
            completed_at
        );
        sqlx::query(&sql)
            .bind(status.as_str())
            .bind(task_id)
            .execute(&self.pool)
            .await
            .map_err(|e| format!("update_status: {e}"))?;
        Ok(())
    }

    async fn update_progress(
        &self,
        task_id: &str,
        progress_pct: u32,
        items_done: u32,
        items_total: u32,
    ) -> Result<(), String> {
        sqlx::query(
            "UPDATE agent_tasks SET progress_pct = ?, items_done = ?, items_total = ?, updated_at = NOW() WHERE task_id = ?",
        )
        .bind(progress_pct as i32)
        .bind(items_done as i32)
        .bind(items_total as i32)
        .bind(task_id)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("update_progress: {e}"))?;
        Ok(())
    }

    async fn save_checkpoint(
        &self,
        task_id: &str,
        checkpoint: &TaskCheckpoint,
    ) -> Result<(), String> {
        let ckpt_json =
            serde_json::to_string(checkpoint).map_err(|e| format!("serialize ckpt: {e}"))?;
        sqlx::query(
            "UPDATE agent_tasks SET checkpoint_json = ?, updated_at = NOW() WHERE task_id = ?",
        )
        .bind(&ckpt_json)
        .bind(task_id)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("save_checkpoint: {e}"))?;
        Ok(())
    }

    async fn update_plan(&self, task_id: &str, plan: &TaskPlan) -> Result<(), String> {
        let plan_json = serde_json::to_string(plan).map_err(|e| format!("serialize plan: {e}"))?;
        let progress = plan.progress_pct();
        let done = plan.items_done();
        let total = plan.subtasks.len() as i32;

        sqlx::query(
            "UPDATE agent_tasks SET plan_json = ?, progress_pct = ?, items_done = ?, items_total = ?, updated_at = NOW() WHERE task_id = ?",
        )
        .bind(&plan_json)
        .bind(progress as i32)
        .bind(done as i32)
        .bind(total)
        .bind(task_id)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("update_plan: {e}"))?;
        Ok(())
    }

    async fn fail_task(&self, task_id: &str, error: &str) -> Result<(), String> {
        sqlx::query(
            "UPDATE agent_tasks SET status = 'failed', error_message = ?, \
             updated_at = NOW(), completed_at = NOW() WHERE task_id = ?",
        )
        .bind(error)
        .bind(task_id)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("fail_task: {e}"))?;
        Ok(())
    }

    async fn complete_task(&self, task_id: &str) -> Result<(), String> {
        sqlx::query(
            "UPDATE agent_tasks SET status = 'completed', progress_pct = 100, \
             updated_at = NOW(), completed_at = NOW() WHERE task_id = ?",
        )
        .bind(task_id)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("complete_task: {e}"))?;
        Ok(())
    }
}

// ─── Local-Only Implementation (Offline) ────────────────────────────────────

/// File-based task service for offline/edge-only mode.
/// Stores tasks as JSON files in `~/.mo-agent/tasks/`.
pub struct LocalTaskService {
    tasks_dir: std::path::PathBuf,
}

impl LocalTaskService {
    pub fn new(tasks_dir: std::path::PathBuf) -> Self {
        Self { tasks_dir }
    }

    fn task_path(&self, task_id: &str) -> std::path::PathBuf {
        self.tasks_dir.join(format!("{task_id}.json"))
    }

    fn load_task(&self, task_id: &str) -> Result<Option<TaskRecord>, String> {
        let path = self.task_path(task_id);
        if !path.exists() {
            return Ok(None);
        }
        let data = std::fs::read_to_string(&path).map_err(|e| format!("read task: {e}"))?;
        let record: TaskRecord =
            serde_json::from_str(&data).map_err(|e| format!("parse task: {e}"))?;
        Ok(Some(record))
    }

    fn save_task(&self, record: &TaskRecord) -> Result<(), String> {
        std::fs::create_dir_all(&self.tasks_dir).map_err(|e| format!("mkdir tasks: {e}"))?;
        let json =
            serde_json::to_string_pretty(record).map_err(|e| format!("serialize task: {e}"))?;
        let path = self.task_path(&record.task_id);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &json).map_err(|e| format!("write task: {e}"))?;
        std::fs::rename(&tmp, &path).map_err(|e| format!("rename task: {e}"))?;
        Ok(())
    }
}

#[async_trait]
impl TaskService for LocalTaskService {
    async fn create_task(
        &self,
        user_id: &str,
        session_id: &str,
        req: TaskCreateRequest,
    ) -> Result<String, String> {
        let task_id = uuid::Uuid::new_v4().to_string();
        let items_total = req
            .plan
            .as_ref()
            .map(|p| p.subtasks.len() as u32)
            .unwrap_or(0);
        let now = chrono::Utc::now().to_rfc3339();

        let record = TaskRecord {
            task_id: task_id.clone(),
            user_id: user_id.to_string(),
            session_id: Some(session_id.to_string()),
            parent_task_id: req.parent_task_id,
            title: req.title,
            description: req.description,
            status: TaskStatus::Pending,
            progress_pct: 0,
            items_done: 0,
            items_total,
            plan: req.plan,
            checkpoint: None,
            error_message: None,
            created_at: now.clone(),
            updated_at: now,
            completed_at: None,
        };
        self.save_task(&record)?;
        Ok(task_id)
    }

    async fn get_task(&self, task_id: &str) -> Result<Option<TaskRecord>, String> {
        self.load_task(task_id)
    }

    async fn list_tasks(
        &self,
        user_id: &str,
        status_filter: Option<TaskStatus>,
    ) -> Result<Vec<TaskRecord>, String> {
        let entries = match std::fs::read_dir(&self.tasks_dir) {
            Ok(e) => e,
            Err(_) => return Ok(Vec::new()),
        };
        let mut tasks = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map(|e| e == "json").unwrap_or(false)
                && let Ok(data) = std::fs::read_to_string(&path)
                && let Ok(record) = serde_json::from_str::<TaskRecord>(&data)
                && record.user_id == user_id
                && status_filter
                    .as_ref()
                    .is_none_or(|filter| record.status == *filter)
            {
                tasks.push(record);
            }
        }
        tasks.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(tasks)
    }

    async fn update_status(&self, task_id: &str, status: TaskStatus) -> Result<(), String> {
        let mut record = self
            .load_task(task_id)?
            .ok_or_else(|| format!("task not found: {task_id}"))?;
        record.status = status;
        record.updated_at = chrono::Utc::now().to_rfc3339();
        if status.is_terminal() {
            record.completed_at = Some(record.updated_at.clone());
        }
        self.save_task(&record)
    }

    async fn update_progress(
        &self,
        task_id: &str,
        progress_pct: u32,
        items_done: u32,
        items_total: u32,
    ) -> Result<(), String> {
        let mut record = self
            .load_task(task_id)?
            .ok_or_else(|| format!("task not found: {task_id}"))?;
        record.progress_pct = progress_pct;
        record.items_done = items_done;
        record.items_total = items_total;
        record.updated_at = chrono::Utc::now().to_rfc3339();
        self.save_task(&record)
    }

    async fn save_checkpoint(
        &self,
        task_id: &str,
        checkpoint: &TaskCheckpoint,
    ) -> Result<(), String> {
        let mut record = self
            .load_task(task_id)?
            .ok_or_else(|| format!("task not found: {task_id}"))?;
        record.checkpoint = Some(checkpoint.clone());
        record.updated_at = chrono::Utc::now().to_rfc3339();
        self.save_task(&record)
    }

    async fn update_plan(&self, task_id: &str, plan: &TaskPlan) -> Result<(), String> {
        let mut record = self
            .load_task(task_id)?
            .ok_or_else(|| format!("task not found: {task_id}"))?;
        record.progress_pct = plan.progress_pct();
        record.items_done = plan.items_done();
        record.items_total = plan.subtasks.len() as u32;
        record.plan = Some(plan.clone());
        record.updated_at = chrono::Utc::now().to_rfc3339();
        self.save_task(&record)
    }

    async fn fail_task(&self, task_id: &str, error: &str) -> Result<(), String> {
        let mut record = self
            .load_task(task_id)?
            .ok_or_else(|| format!("task not found: {task_id}"))?;
        record.status = TaskStatus::Failed;
        record.error_message = Some(error.to_string());
        let now = chrono::Utc::now().to_rfc3339();
        record.updated_at = now.clone();
        record.completed_at = Some(now);
        self.save_task(&record)
    }

    async fn complete_task(&self, task_id: &str) -> Result<(), String> {
        let mut record = self
            .load_task(task_id)?
            .ok_or_else(|| format!("task not found: {task_id}"))?;
        record.status = TaskStatus::Completed;
        record.progress_pct = 100;
        let now = chrono::Utc::now().to_rfc3339();
        record.updated_at = now.clone();
        record.completed_at = Some(now);
        self.save_task(&record)
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── TaskStatus ──

    #[test]
    fn task_status_roundtrip() {
        for status in [
            TaskStatus::Pending,
            TaskStatus::InProgress,
            TaskStatus::Paused,
            TaskStatus::Completed,
            TaskStatus::Failed,
            TaskStatus::Cancelled,
        ] {
            assert_eq!(TaskStatus::parse_status(status.as_str()), status);
        }
    }

    #[test]
    fn task_status_terminal() {
        assert!(!TaskStatus::Pending.is_terminal());
        assert!(!TaskStatus::InProgress.is_terminal());
        assert!(!TaskStatus::Paused.is_terminal());
        assert!(TaskStatus::Completed.is_terminal());
        assert!(TaskStatus::Failed.is_terminal());
        assert!(TaskStatus::Cancelled.is_terminal());
    }

    #[test]
    fn task_status_unknown_defaults_to_pending() {
        assert_eq!(TaskStatus::parse_status("unknown"), TaskStatus::Pending);
    }

    // ── TaskPlan ──

    #[test]
    fn empty_plan_zero_progress() {
        let plan = TaskPlan::default();
        assert_eq!(plan.progress_pct(), 0);
        assert_eq!(plan.items_done(), 0);
        assert!(plan.ready_subtasks().is_empty());
    }

    #[test]
    fn plan_progress_computation() {
        let plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "a".into(),
                    title: "Task A".into(),
                    description: None,
                    depends_on: vec![],
                    status: TaskStatus::Completed,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "b".into(),
                    title: "Task B".into(),
                    description: None,
                    depends_on: vec!["a".into()],
                    status: TaskStatus::Pending,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "c".into(),
                    title: "Task C".into(),
                    description: None,
                    depends_on: vec!["a".into()],
                    status: TaskStatus::Pending,
                    ..Default::default()
                },
            ],
            notes: None,
        };
        assert_eq!(plan.progress_pct(), 33); // 1/3
        assert_eq!(plan.items_done(), 1);
    }

    #[test]
    fn plan_ready_subtasks_respects_dependencies() {
        let plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "a".into(),
                    title: "Task A".into(),
                    description: None,
                    depends_on: vec![],
                    status: TaskStatus::Completed,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "b".into(),
                    title: "Task B".into(),
                    description: None,
                    depends_on: vec!["a".into()],
                    status: TaskStatus::Pending,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "c".into(),
                    title: "Task C".into(),
                    description: None,
                    depends_on: vec!["b".into()],
                    status: TaskStatus::Pending,
                    ..Default::default()
                },
            ],
            notes: None,
        };
        let ready = plan.ready_subtasks();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, "b"); // a done → b ready, c blocked on b
    }

    #[test]
    fn plan_multiple_ready_subtasks() {
        let plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "a".into(),
                    title: "Task A".into(),
                    description: None,
                    depends_on: vec![],
                    status: TaskStatus::Completed,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "b".into(),
                    title: "Task B".into(),
                    description: None,
                    depends_on: vec!["a".into()],
                    status: TaskStatus::Pending,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "c".into(),
                    title: "Task C".into(),
                    description: None,
                    depends_on: vec!["a".into()],
                    status: TaskStatus::Pending,
                    ..Default::default()
                },
            ],
            notes: None,
        };
        let ready = plan.ready_subtasks();
        assert_eq!(ready.len(), 2); // b and c both unblocked
    }

    // ── TaskCheckpoint ──

    #[test]
    fn checkpoint_json_roundtrip() {
        let ckpt = TaskCheckpoint {
            active_subtask_id: Some("b".into()),
            turn: 15,
            session_id: Some("sess-123".into()),
            state: {
                let mut m = serde_json::Map::new();
                m.insert("files_processed".into(), serde_json::json!(42));
                m
            },
        };
        let json = serde_json::to_string(&ckpt).unwrap();
        let loaded: TaskCheckpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.turn, 15);
        assert_eq!(loaded.active_subtask_id, Some("b".into()));
        assert_eq!(loaded.state["files_processed"], 42);
    }

    // ── TaskRecord ──

    #[test]
    fn task_record_json_roundtrip() {
        let record = TaskRecord {
            task_id: "t-1".into(),
            user_id: "u-1".into(),
            session_id: Some("s-1".into()),
            parent_task_id: None,
            title: "Refactor auth".into(),
            description: Some("Migrate to JWT".into()),
            status: TaskStatus::InProgress,
            progress_pct: 50,
            items_done: 2,
            items_total: 4,
            plan: Some(TaskPlan::default()),
            checkpoint: Some(TaskCheckpoint::default()),
            error_message: None,
            created_at: "2025-01-01T00:00:00Z".into(),
            updated_at: "2025-01-01T01:00:00Z".into(),
            completed_at: None,
        };
        let json = serde_json::to_string(&record).unwrap();
        let loaded: TaskRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.task_id, "t-1");
        assert_eq!(loaded.status, TaskStatus::InProgress);
        assert_eq!(loaded.progress_pct, 50);
    }

    // ── LocalTaskService ──

    #[tokio::test]
    async fn local_task_create_and_get() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = LocalTaskService::new(tmp.path().to_path_buf());

        let task_id = svc
            .create_task(
                "user1",
                "sess1",
                TaskCreateRequest {
                    title: "Test Task".into(),
                    description: Some("A test".into()),
                    plan: None,
                    parent_task_id: None,
                },
            )
            .await
            .unwrap();

        let loaded = svc.get_task(&task_id).await.unwrap().unwrap();
        assert_eq!(loaded.title, "Test Task");
        assert_eq!(loaded.status, TaskStatus::Pending);
    }

    #[tokio::test]
    async fn local_task_lifecycle() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = LocalTaskService::new(tmp.path().to_path_buf());

        let tid = svc
            .create_task(
                "user1",
                "sess1",
                TaskCreateRequest {
                    title: "Lifecycle Test".into(),
                    description: None,
                    plan: None,
                    parent_task_id: None,
                },
            )
            .await
            .unwrap();

        // Pending → InProgress
        svc.update_status(&tid, TaskStatus::InProgress)
            .await
            .unwrap();
        let t = svc.get_task(&tid).await.unwrap().unwrap();
        assert_eq!(t.status, TaskStatus::InProgress);

        // Update progress
        svc.update_progress(&tid, 50, 5, 10).await.unwrap();
        let t = svc.get_task(&tid).await.unwrap().unwrap();
        assert_eq!(t.progress_pct, 50);
        assert_eq!(t.items_done, 5);

        // Save checkpoint
        let ckpt = TaskCheckpoint {
            active_subtask_id: Some("sub-1".into()),
            turn: 10,
            session_id: Some("sess1".into()),
            state: serde_json::Map::new(),
        };
        svc.save_checkpoint(&tid, &ckpt).await.unwrap();
        let t = svc.get_task(&tid).await.unwrap().unwrap();
        assert!(t.checkpoint.is_some());
        assert_eq!(t.checkpoint.unwrap().turn, 10);

        // Complete
        svc.complete_task(&tid).await.unwrap();
        let t = svc.get_task(&tid).await.unwrap().unwrap();
        assert_eq!(t.status, TaskStatus::Completed);
        assert!(t.completed_at.is_some());
    }

    #[tokio::test]
    async fn local_task_list_with_filter() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = LocalTaskService::new(tmp.path().to_path_buf());

        svc.create_task(
            "user1",
            "s1",
            TaskCreateRequest {
                title: "Active".into(),
                description: None,
                plan: None,
                parent_task_id: None,
            },
        )
        .await
        .unwrap();

        let tid2 = svc
            .create_task(
                "user1",
                "s1",
                TaskCreateRequest {
                    title: "Done".into(),
                    description: None,
                    plan: None,
                    parent_task_id: None,
                },
            )
            .await
            .unwrap();
        svc.complete_task(&tid2).await.unwrap();

        let all = svc.list_tasks("user1", None).await.unwrap();
        assert_eq!(all.len(), 2);

        let pending = svc
            .list_tasks("user1", Some(TaskStatus::Pending))
            .await
            .unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].title, "Active");

        let completed = svc
            .list_tasks("user1", Some(TaskStatus::Completed))
            .await
            .unwrap();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].title, "Done");
    }

    #[tokio::test]
    async fn local_task_fail() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = LocalTaskService::new(tmp.path().to_path_buf());

        let tid = svc
            .create_task(
                "user1",
                "s1",
                TaskCreateRequest {
                    title: "Will Fail".into(),
                    description: None,
                    plan: None,
                    parent_task_id: None,
                },
            )
            .await
            .unwrap();

        svc.fail_task(&tid, "network timeout").await.unwrap();
        let t = svc.get_task(&tid).await.unwrap().unwrap();
        assert_eq!(t.status, TaskStatus::Failed);
        assert_eq!(t.error_message.as_deref(), Some("network timeout"));
        assert!(t.completed_at.is_some());
    }

    #[tokio::test]
    async fn local_task_plan_update() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = LocalTaskService::new(tmp.path().to_path_buf());

        let plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "a".into(),
                    title: "Step A".into(),
                    description: None,
                    depends_on: vec![],
                    status: TaskStatus::Pending,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "b".into(),
                    title: "Step B".into(),
                    description: None,
                    depends_on: vec!["a".into()],
                    status: TaskStatus::Pending,
                    ..Default::default()
                },
            ],
            notes: Some("two-step plan".into()),
        };

        let tid = svc
            .create_task(
                "user1",
                "s1",
                TaskCreateRequest {
                    title: "Plan Task".into(),
                    description: None,
                    plan: Some(plan.clone()),
                    parent_task_id: None,
                },
            )
            .await
            .unwrap();

        // Mark first subtask complete
        let mut updated_plan = plan;
        updated_plan.subtasks[0].status = TaskStatus::Completed;
        svc.update_plan(&tid, &updated_plan).await.unwrap();

        let t = svc.get_task(&tid).await.unwrap().unwrap();
        assert_eq!(t.progress_pct, 50);
        assert_eq!(t.items_done, 1);
        assert_eq!(t.items_total, 2);
    }

    #[tokio::test]
    async fn local_task_nonexistent_returns_none() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = LocalTaskService::new(tmp.path().to_path_buf());
        let result = svc.get_task("nonexistent-id").await.unwrap();
        assert!(result.is_none());
    }

    // ── Cross-session resume scenario ──

    #[tokio::test]
    async fn cross_session_task_resume() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = LocalTaskService::new(tmp.path().to_path_buf());

        // Session 1: create task, make progress, save checkpoint, pause
        let tid = svc
            .create_task(
                "user1",
                "session-1",
                TaskCreateRequest {
                    title: "Big Refactor".into(),
                    description: Some("3-step plan".into()),
                    plan: Some(TaskPlan {
                        subtasks: vec![
                            SubtaskPlan {
                                id: "s1".into(),
                                title: "Analysis".into(),
                                description: None,
                                depends_on: vec![],
                                status: TaskStatus::Completed,
                                ..Default::default()
                            },
                            SubtaskPlan {
                                id: "s2".into(),
                                title: "Implementation".into(),
                                description: None,
                                depends_on: vec!["s1".into()],
                                status: TaskStatus::InProgress,
                                ..Default::default()
                            },
                            SubtaskPlan {
                                id: "s3".into(),
                                title: "Testing".into(),
                                description: None,
                                depends_on: vec!["s2".into()],
                                status: TaskStatus::Pending,
                                ..Default::default()
                            },
                        ],
                        notes: None,
                    }),
                    parent_task_id: None,
                },
            )
            .await
            .unwrap();

        svc.update_status(&tid, TaskStatus::InProgress)
            .await
            .unwrap();
        svc.save_checkpoint(
            &tid,
            &TaskCheckpoint {
                active_subtask_id: Some("s2".into()),
                turn: 25,
                session_id: Some("session-1".into()),
                state: {
                    let mut m = serde_json::Map::new();
                    m.insert(
                        "files_modified".into(),
                        serde_json::json!(["auth.rs", "config.rs"]),
                    );
                    m
                },
            },
        )
        .await
        .unwrap();
        svc.update_status(&tid, TaskStatus::Paused).await.unwrap();

        // Session 2: new session loads the task and resumes
        let task = svc.get_task(&tid).await.unwrap().unwrap();
        assert_eq!(task.status, TaskStatus::Paused);
        assert!(task.checkpoint.is_some());

        let ckpt = task.checkpoint.unwrap();
        assert_eq!(ckpt.active_subtask_id, Some("s2".into()));
        assert_eq!(ckpt.turn, 25);
        assert_eq!(ckpt.session_id, Some("session-1".into()));

        // Can inspect plan to find ready subtasks
        let plan = task.plan.unwrap();
        // s1 completed, s2 in_progress → s2 is not "ready" (not pending)
        // But we know from checkpoint it was active
        assert_eq!(plan.progress_pct(), 33); // 1/3 completed

        // Resume: update status and continue
        svc.update_status(&tid, TaskStatus::InProgress)
            .await
            .unwrap();
        let resumed = svc.get_task(&tid).await.unwrap().unwrap();
        assert_eq!(resumed.status, TaskStatus::InProgress);
    }
}
