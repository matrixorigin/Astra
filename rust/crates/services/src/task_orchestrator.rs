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
//! - Local fallback: `~/.astra/tasks/{task_id}.json`

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::db_row::{
    RowExt as LearningStatsDbRow, RowExt as TaskListDbRow, RowExt as TaskRecordDbRow,
    RowExt as TaskStatusGuardDbRow, RowExt as TemplateRecommendationDbRow,
};
use crate::verification::VerifierKind;

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

    pub fn parse_status(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "in_progress" => Some(Self::InProgress),
            "paused" => Some(Self::Paused),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }

    pub fn is_claimable(&self) -> bool {
        matches!(self, Self::Pending | Self::InProgress)
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
    /// Structured verification checks — machine-executable, no heuristic parsing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acceptance_checks: Vec<VerifierKind>,
}

impl SubtaskPlan {
    /// Reset this subtask so it will be re-executed on the next run.
    ///
    /// Distinct from rewind (which resets this subtask and every subtask after it):
    /// `reset_for_redo` clears **only** the runtime status of this one subtask back to
    /// `Pending`. The authoring fields (`title`, `description`, `depends_on`, `files`,
    /// `effort`, `acceptance_checks`) are preserved verbatim so a redo re-runs the
    /// exact same work definition that was originally approved.
    pub fn reset_for_redo(&mut self) {
        self.status = TaskStatus::Pending;
    }
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
    ///
    /// An empty plan (no subtasks) reports 0% — it has not been generated yet,
    /// so progress is undefined. Reporting 100% caused user-visible bugs where
    /// failed plan generation was misreported as "all subtasks completed".
    pub fn progress_pct(&self) -> u32 {
        if self.subtasks.is_empty() {
            return 0;
        }
        // Only count successfully completed subtasks as progress.
        // Failed/Cancelled subtasks are NOT progress — they represent work that
        // needs to be retried or was abandoned.
        let done = self
            .subtasks
            .iter()
            .filter(|st| st.status == TaskStatus::Completed)
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
    // Learning feedback fields
    #[serde(default)]
    pub user_rating: Option<u8>,
    #[serde(default)]
    pub completion_time_sec: Option<i32>,
    #[serde(default)]
    pub replan_count: u32,
    #[serde(default)]
    pub auto_adjustments: u32,
    #[serde(default)]
    pub outcome: Option<TaskOutcome>,
    #[serde(default)]
    pub project_type: Option<String>,
    #[serde(default)]
    pub goal_pattern: Option<String>,
    /// Edge / worker that owns execution (Phase 3 lease + `agent_tasks.agent_id`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
}

/// Lightweight task summary for list views.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskListItem {
    pub task_id: String,
    pub title: String,
    #[serde(default)]
    pub session_id: Option<String>,
    pub status: TaskStatus,
    pub progress_pct: u32,
    pub items_done: u32,
    pub items_total: u32,
    pub created_at: String,
    pub updated_at: String,
    pub completed_at: Option<String>,
    #[serde(default)]
    pub outcome: Option<TaskOutcome>,
    #[serde(default)]
    pub error_message: Option<String>,
    #[serde(default)]
    pub project_type: Option<String>,
    #[serde(default)]
    pub claimability: Option<TaskClaimability>,
}

/// Task outcome for learning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskOutcome {
    Success,
    Partial,
    Failed,
    Cancelled,
}

impl TaskOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Partial => "partial",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "success" => Some(Self::Success),
            "partial" => Some(Self::Partial),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

/// Why a task appears in the worker claimable queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskClaimability {
    Pending,
    RecoverableInProgress,
}

impl TaskClaimability {
    pub fn for_status(status: TaskStatus) -> Option<Self> {
        match status {
            TaskStatus::Pending => Some(Self::Pending),
            TaskStatus::InProgress => Some(Self::RecoverableInProgress),
            _ => None,
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "recoverable_in_progress" => Some(Self::RecoverableInProgress),
            _ => None,
        }
    }
}

/// Request to create a new task.
#[derive(Default, Debug, Clone, Serialize, Deserialize)]
pub struct TaskCreateRequest {
    pub title: String,
    pub description: Option<String>,
    pub plan: Option<TaskPlan>,
    pub parent_task_id: Option<String>,
    /// Project type for pattern learning (e.g., "rust", "python", "typescript")
    pub project_type: Option<String>,
    /// Goal pattern for matching similar tasks
    pub goal_pattern: Option<String>,
}

// ─── Template Types ─────────────────────────────────────────────────────────

/// A plan template extracted from successful completions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanTemplate {
    pub template_id: String,
    pub user_id: Option<String>,
    pub goal_pattern: String,
    pub project_type: Option<String>,
    pub template: TaskPlan,
    pub success_rate: f32,
    pub avg_completion_time: Option<i32>,
    pub use_count: u32,
    pub created_at: String,
    pub updated_at: String,
}

/// A recommended template with relevance score.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateRecommendation {
    pub template: PlanTemplate,
    /// Relevance score 0.0 - 1.0
    pub score: f32,
    /// Why this template was recommended
    pub reason: String,
}

/// Learning stats for inferring success without explicit rating.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LearningStats {
    /// Total tasks with this pattern
    pub total_tasks: u32,
    /// Tasks that finished with successful outcome.
    pub completed_tasks: u32,
    /// Average rating (only rated tasks)
    pub avg_rating: Option<f32>,
    /// Average replan count
    pub avg_replan_count: f32,
    /// Inferred success rate (0.0 - 1.0)
    pub inferred_success_rate: f32,
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

    /// Get a task by owner and ID.
    async fn get_task(&self, user_id: &str, task_id: &str) -> Result<Option<TaskRecord>, String>;

    /// List recent task history for a user (optionally filter by status).
    async fn list_recent_tasks(
        &self,
        user_id: &str,
        status_filter: Option<TaskStatus>,
    ) -> Result<Vec<TaskListItem>, String>;

    /// List recent task history for a single session (optionally filter by status).
    async fn list_recent_tasks_for_session(
        &self,
        user_id: &str,
        session_id: &str,
        status_filter: Option<TaskStatus>,
    ) -> Result<Vec<TaskListItem>, String>;

    /// Search tasks for a user using the shared CLI lookup semantics.
    ///
    /// Matching is fail-closed and tiered:
    /// 1. exact `task_id`
    /// 2. `task_id` prefix
    /// 3. case-insensitive exact title
    /// 4. case-insensitive title substring
    ///
    /// Only matches from the best tier are returned, ordered by newest first.
    async fn search_tasks(
        &self,
        user_id: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<TaskListItem>, String>;

    /// List claimable tasks for worker claim order.
    ///
    /// This is intentionally distinct from `list_recent_tasks(status=pending)`: workers
    /// need claim semantics, not a recent-list surface. Results are ordered
    /// oldest-first so older claimable tasks cannot be starved by newer ones.
    ///
    /// "Claimable" means:
    /// - `pending`
    /// - `in_progress` with no active lease (or an expired lease)
    async fn list_claimable_tasks_for_worker(
        &self,
        user_id: &str,
        limit: usize,
    ) -> Result<Vec<TaskListItem>, String>;

    /// Update task status.
    async fn update_status(
        &self,
        user_id: &str,
        task_id: &str,
        status: TaskStatus,
    ) -> Result<(), String>;

    /// Update progress counters.
    async fn update_progress(
        &self,
        user_id: &str,
        task_id: &str,
        progress_pct: u32,
        items_done: u32,
        items_total: u32,
    ) -> Result<(), String>;

    /// Save a checkpoint (resumable state).
    async fn save_checkpoint(
        &self,
        user_id: &str,
        task_id: &str,
        checkpoint: &TaskCheckpoint,
    ) -> Result<(), String>;

    /// Update the plan (e.g., mark subtask as done).
    async fn update_plan(
        &self,
        user_id: &str,
        task_id: &str,
        plan: &TaskPlan,
    ) -> Result<(), String>;

    /// Mark task as failed with error message.
    async fn fail_task(&self, user_id: &str, task_id: &str, error: &str) -> Result<(), String>;

    /// Mark task as completed.
    async fn complete_task(&self, user_id: &str, task_id: &str) -> Result<(), String>;

    /// Mark a non-plan task as completed with an explicit outcome.
    ///
    /// Used by one-shot and background job runs where the task has no
    /// subtask progress but may still finish partially.
    async fn complete_task_with_outcome(
        &self,
        user_id: &str,
        task_id: &str,
        outcome: TaskOutcome,
    ) -> Result<(), String>;

    /// Mark a plan-run task finished with explicit progress and learning outcome.
    ///
    /// Sets `status = completed`, `completed_at`, and `outcome` (e.g. `success` vs `partial`).
    /// Used when the background plan executor finishes so job status matches delivery state.
    async fn complete_plan_run(
        &self,
        user_id: &str,
        task_id: &str,
        progress_pct: u32,
        items_done: u32,
        items_total: u32,
        outcome: TaskOutcome,
    ) -> Result<(), String>;

    /// Record user feedback for learning.
    async fn record_feedback(
        &self,
        user_id: &str,
        task_id: &str,
        rating: u8,
        outcome: TaskOutcome,
        completion_time_sec: Option<i32>,
    ) -> Result<(), String>;

    /// Increment replan count.
    async fn increment_replan_count(&self, user_id: &str, task_id: &str) -> Result<(), String>;

    // ─── Learning Methods ───

    /// Extract a template from a successful task (rating >= 4 or inferred success).
    /// Returns the template_id if created.
    async fn extract_template(
        &self,
        user_id: &str,
        task_id: &str,
        goal_pattern: &str,
    ) -> Result<Option<String>, String>;

    /// Recommend templates for a goal based on similarity and success rate.
    async fn recommend_templates(
        &self,
        user_id: &str,
        goal: &str,
        project_type: Option<&str>,
        limit: usize,
    ) -> Result<Vec<TemplateRecommendation>, String>;

    /// Get learning stats for a goal pattern.
    async fn get_learning_stats(
        &self,
        user_id: &str,
        goal_pattern: &str,
    ) -> Result<LearningStats, String>;

    /// Increment template use count (called when a template is instantiated).
    async fn record_template_usage(&self, user_id: &str, template_id: &str) -> Result<(), String>;
}

// ─── MatrixOne Implementation ───────────────────────────────────────────────

/// Task service backed by MatrixOne `agent_tasks` table.
pub struct MatrixOneTaskService {
    pool: sqlx::Pool<sqlx::MySql>,
}

const MAX_TASK_LIST_ROWS: usize = 200;
const MAX_TASK_QUERY_MATCH_ROWS: usize = 8;
const AGENT_TASK_KNOWN_NON_TERMINAL_STATUS_GUARD: &str =
    "AND status IN ('pending', 'in_progress', 'paused')";

fn guarded_agent_task_update_sql(set_clause: &str) -> String {
    format!(
        "UPDATE agent_tasks SET {set_clause} \
         WHERE user_id = ? AND task_id = ? {AGENT_TASK_KNOWN_NON_TERMINAL_STATUS_GUARD}"
    )
}

fn task_query_match_rank(task_id: &str, title: &str, query: &str) -> Option<u8> {
    if task_id == query {
        return Some(1);
    }
    if task_id.starts_with(query) {
        return Some(2);
    }

    let title_lower = title.to_lowercase();
    let query_lower = query.to_lowercase();
    if title_lower == query_lower {
        return Some(3);
    }
    if title_lower.contains(&query_lower) {
        return Some(4);
    }
    None
}

fn select_best_task_query_matches(
    tasks: Vec<TaskListItem>,
    query: &str,
    limit: usize,
) -> Vec<TaskListItem> {
    let mut ranked: Vec<(u8, TaskListItem)> = tasks
        .into_iter()
        .filter_map(|task| {
            task_query_match_rank(&task.task_id, &task.title, query).map(|rank| (rank, task))
        })
        .collect();
    ranked.sort_by(|(left_rank, left_task), (right_rank, right_task)| {
        left_rank
            .cmp(right_rank)
            .then_with(|| right_task.updated_at.cmp(&left_task.updated_at))
    });

    let Some(best_rank) = ranked.first().map(|(rank, _)| *rank) else {
        return Vec::new();
    };

    ranked
        .into_iter()
        .filter(|(rank, _)| *rank == best_rank)
        .take(limit)
        .map(|(_, task)| task)
        .collect()
}

fn escape_sql_like_fragment(fragment: &str) -> String {
    fragment
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

pub(crate) const AGENT_TASK_DETAIL_SELECT_COLUMNS: &str = "task_id, user_id, session_id, parent_task_id, title, description, \
     status, progress_pct, items_done, items_total, plan_json, checkpoint_json, \
     error_message, user_rating, completion_time_sec, replan_count, auto_adjustments, \
     outcome, project_type, goal_pattern, agent_id, \
     CAST(created_at AS CHAR) AS created_at, \
     CAST(updated_at AS CHAR) AS updated_at, \
     completed_at";

pub(crate) const AGENT_TASK_LIST_SELECT_COLUMNS: &str = "task_id, user_id, session_id, parent_task_id, title, \
     NULL AS description, status, progress_pct, items_done, items_total, \
     NULL AS plan_json, NULL AS checkpoint_json, error_message, \
     user_rating, completion_time_sec, replan_count, auto_adjustments, \
     outcome, project_type, goal_pattern, agent_id, NULL AS claimability, \
     CAST(created_at AS CHAR) AS created_at, \
     CAST(updated_at AS CHAR) AS updated_at, \
     completed_at";

fn decode_task_status_guard_row(
    row: &impl TaskStatusGuardDbRow,
    task_id: &str,
) -> Result<TaskStatus, String> {
    let raw = row
        .string_column("status")
        .map_err(|e| format!("task status guard row decode `status`: {e}"))?;
    TaskStatus::parse_status(&raw)
        .ok_or_else(|| format!("task {task_id} has unknown persisted status: {raw}"))
}

fn task_list_decode_error(context: &str, column: &'static str, error: sqlx::Error) -> String {
    format!("task list {context} decode `{column}`: {error}")
}

fn task_list_optional_string(
    row: &impl TaskListDbRow,
    column: &'static str,
) -> Result<Option<String>, String> {
    row.optional_string_column(column)
        .map_err(|e| task_list_decode_error("row", column, e))
}

fn task_list_non_negative_u32(
    row: &impl TaskListDbRow,
    column: &'static str,
) -> Result<u32, String> {
    let value = row
        .i32_column(column)
        .map_err(|e| task_list_decode_error("row", column, e))?;
    u32::try_from(value).map_err(|_| {
        format!("task list row decode `{column}` expected non-negative integer, got {value}")
    })
}

fn task_list_progress_pct(row: &impl TaskListDbRow) -> Result<u32, String> {
    let value = task_list_non_negative_u32(row, "progress_pct")?;
    if value > 100 {
        return Err(format!(
            "task list row decode `progress_pct` expected 0..=100, got {value}"
        ));
    }
    Ok(value)
}

fn task_list_outcome(row: &impl TaskListDbRow) -> Result<Option<TaskOutcome>, String> {
    let Some(raw) = task_list_optional_string(row, "outcome")? else {
        return Ok(None);
    };
    TaskOutcome::parse(&raw)
        .map(Some)
        .ok_or_else(|| format!("task list row decode `outcome` unknown value: {raw}"))
}

fn task_list_claimability(row: &impl TaskListDbRow) -> Result<Option<TaskClaimability>, String> {
    let Some(raw) = task_list_optional_string(row, "claimability")? else {
        return Ok(None);
    };
    TaskClaimability::parse(&raw)
        .map(Some)
        .ok_or_else(|| format!("task list row decode `claimability` unknown value: {raw}"))
}

fn decode_task_list_item(row: &impl TaskListDbRow) -> Result<TaskListItem, String> {
    let status_str = row
        .string_column("status")
        .map_err(|e| task_list_decode_error("row", "status", e))?;
    let status = TaskStatus::parse_status(&status_str)
        .ok_or_else(|| format!("unknown persisted task status: {status_str}"))?;

    Ok(TaskListItem {
        task_id: row
            .string_column("task_id")
            .map_err(|e| task_list_decode_error("row", "task_id", e))?,
        title: row
            .string_column("title")
            .map_err(|e| task_list_decode_error("row", "title", e))?,
        session_id: task_list_optional_string(row, "session_id")?,
        status,
        progress_pct: task_list_progress_pct(row)?,
        items_done: task_list_non_negative_u32(row, "items_done")?,
        items_total: task_list_non_negative_u32(row, "items_total")?,
        created_at: row
            .string_column("created_at")
            .map_err(|e| task_list_decode_error("row", "created_at", e))?,
        updated_at: row
            .string_column("updated_at")
            .map_err(|e| task_list_decode_error("row", "updated_at", e))?,
        completed_at: task_list_optional_string(row, "completed_at")?,
        outcome: task_list_outcome(row)?,
        error_message: task_list_optional_string(row, "error_message")?,
        project_type: task_list_optional_string(row, "project_type")?,
        claimability: task_list_claimability(row)?,
    })
}

fn template_recommendation_decode_error(
    column: &'static str,
    error: impl std::fmt::Display,
) -> String {
    format!("template recommendation row decode `{column}`: {error}")
}

fn template_recommendation_string(
    row: &impl TemplateRecommendationDbRow,
    column: &'static str,
) -> Result<String, String> {
    row.string_column(column)
        .map_err(|e| template_recommendation_decode_error(column, e))
}

fn template_recommendation_optional_string(
    row: &impl TemplateRecommendationDbRow,
    column: &'static str,
) -> Result<Option<String>, String> {
    row.optional_string_column(column)
        .map_err(|e| template_recommendation_decode_error(column, e))
}

fn template_recommendation_i32(
    row: &impl TemplateRecommendationDbRow,
    column: &'static str,
) -> Result<i32, String> {
    row.i32_column(column)
        .map_err(|e| template_recommendation_decode_error(column, e))
}

fn template_recommendation_non_negative_u32(
    row: &impl TemplateRecommendationDbRow,
    column: &'static str,
) -> Result<u32, String> {
    let value = template_recommendation_i32(row, column)?;
    u32::try_from(value).map_err(|_| {
        template_recommendation_decode_error(
            column,
            format!("expected non-negative integer, got {value}"),
        )
    })
}

fn template_recommendation_optional_i32(
    row: &impl TemplateRecommendationDbRow,
    column: &'static str,
) -> Result<Option<i32>, String> {
    row.optional_i32_column(column)
        .map_err(|e| template_recommendation_decode_error(column, e))
}

fn template_recommendation_f32(
    row: &impl TemplateRecommendationDbRow,
    column: &'static str,
) -> Result<f32, String> {
    row.f32_column(column)
        .map_err(|e| template_recommendation_decode_error(column, e))
}

fn template_recommendation_is_own(row: &impl TemplateRecommendationDbRow) -> Result<bool, String> {
    match template_recommendation_i32(row, "is_own")? {
        0 => Ok(false),
        1 => Ok(true),
        value => Err(template_recommendation_decode_error(
            "is_own",
            format!("expected 0 or 1, got {value}"),
        )),
    }
}

fn decode_template_recommendation(
    row: &impl TemplateRecommendationDbRow,
) -> Result<TemplateRecommendation, String> {
    let template_json = template_recommendation_string(row, "template_json")?;
    let template_plan: TaskPlan = serde_json::from_str(&template_json)
        .map_err(|e| template_recommendation_decode_error("template_json", e))?;
    let is_own = template_recommendation_is_own(row)?;
    let goal_pattern = template_recommendation_string(row, "goal_pattern")?;
    let use_count = template_recommendation_non_negative_u32(row, "use_count")?;
    let reason = if is_own {
        format!("Your successful pattern: {goal_pattern}")
    } else {
        format!("Community pattern ({use_count}x used): {goal_pattern}")
    };

    Ok(TemplateRecommendation {
        template: PlanTemplate {
            template_id: template_recommendation_string(row, "template_id")?,
            user_id: template_recommendation_optional_string(row, "user_id")?,
            goal_pattern,
            project_type: template_recommendation_optional_string(row, "project_type")?,
            template: template_plan,
            success_rate: template_recommendation_f32(row, "success_rate")?,
            avg_completion_time: template_recommendation_optional_i32(row, "avg_completion_time")?,
            use_count,
            created_at: template_recommendation_string(row, "created_at")?,
            updated_at: template_recommendation_string(row, "updated_at")?,
        },
        score: template_recommendation_f32(row, "score")?,
        reason,
    })
}

fn learning_stats_decode_error(column: &'static str, error: impl std::fmt::Display) -> String {
    format!("learning stats row decode `{column}`: {error}")
}

fn learning_stats_non_negative_u32(
    row: &impl LearningStatsDbRow,
    column: &'static str,
) -> Result<u32, String> {
    let value = row
        .i64_column(column)
        .map_err(|e| learning_stats_decode_error(column, e))?;
    u32::try_from(value).map_err(|_| {
        learning_stats_decode_error(
            column,
            format!("expected non-negative integer, got {value}"),
        )
    })
}

fn learning_stats_optional_f32(
    row: &impl LearningStatsDbRow,
    column: &'static str,
) -> Result<Option<f32>, String> {
    row.optional_f32_column(column)
        .map_err(|e| learning_stats_decode_error(column, e))
}

fn decode_learning_stats(row: &impl LearningStatsDbRow) -> Result<LearningStats, String> {
    let total_tasks = learning_stats_non_negative_u32(row, "total_tasks")?;
    let completed_tasks = learning_stats_non_negative_u32(row, "completed_tasks")?;
    if completed_tasks > total_tasks {
        return Err(format!(
            "learning stats row decode `completed_tasks` expected <= total_tasks {total_tasks}, got {completed_tasks}"
        ));
    }
    let avg_rating = learning_stats_optional_f32(row, "avg_rating")?;
    let avg_replan_count =
        learning_stats_optional_f32(row, "avg_replan_count")?.unwrap_or_default();

    let inferred_success_rate = if total_tasks == 0 {
        0.0
    } else {
        let completion_factor = completed_tasks as f32 / total_tasks as f32;
        let replan_penalty = (avg_replan_count / 3.0).min(1.0);
        (completion_factor * (1.0 - replan_penalty * 0.3)).clamp(0.0, 1.0)
    };

    Ok(LearningStats {
        total_tasks,
        completed_tasks,
        avg_rating,
        avg_replan_count,
        inferred_success_rate,
    })
}

fn task_record_decode_error(context: &str, column: &'static str, error: sqlx::Error) -> String {
    format!("task record {context} decode `{column}`: {error}")
}

fn task_record_optional_string(
    row: &impl TaskRecordDbRow,
    column: &'static str,
) -> Result<Option<String>, String> {
    row.optional_string_column(column)
        .map_err(|e| task_record_decode_error("row", column, e))
}

fn task_record_non_negative_u32(
    row: &impl TaskRecordDbRow,
    column: &'static str,
) -> Result<u32, String> {
    let value = row
        .i32_column(column)
        .map_err(|e| task_record_decode_error("row", column, e))?;
    u32::try_from(value).map_err(|_| {
        format!("task record row decode `{column}` expected non-negative integer, got {value}")
    })
}

fn task_record_progress_pct(row: &impl TaskRecordDbRow) -> Result<u32, String> {
    let value = task_record_non_negative_u32(row, "progress_pct")?;
    if value > 100 {
        return Err(format!(
            "task record row decode `progress_pct` expected 0..=100, got {value}"
        ));
    }
    Ok(value)
}

fn task_record_optional_non_negative_i32(
    row: &impl TaskRecordDbRow,
    column: &'static str,
) -> Result<Option<i32>, String> {
    let Some(value) = row
        .optional_i32_column(column)
        .map_err(|e| task_record_decode_error("row", column, e))?
    else {
        return Ok(None);
    };
    if value < 0 {
        return Err(format!(
            "task record row decode `{column}` expected non-negative integer, got {value}"
        ));
    }
    Ok(Some(value))
}

fn task_record_user_rating(row: &impl TaskRecordDbRow) -> Result<Option<u8>, String> {
    let Some(value) = row
        .optional_i8_column("user_rating")
        .map_err(|e| task_record_decode_error("row", "user_rating", e))?
    else {
        return Ok(None);
    };
    u8::try_from(value).map(Some).map_err(|_| {
        format!("task record row decode `user_rating` expected non-negative integer, got {value}")
    })
}

fn task_record_optional_json<T: serde::de::DeserializeOwned>(
    row: &impl TaskRecordDbRow,
    column: &'static str,
) -> Result<Option<T>, String> {
    let Some(raw) = task_record_optional_string(row, column)? else {
        return Ok(None);
    };
    serde_json::from_str(&raw)
        .map(Some)
        .map_err(|e| format!("task record row decode `{column}` invalid JSON: {e}"))
}

fn task_record_outcome(row: &impl TaskRecordDbRow) -> Result<Option<TaskOutcome>, String> {
    let Some(raw) = task_record_optional_string(row, "outcome")? else {
        return Ok(None);
    };
    TaskOutcome::parse(&raw)
        .map(Some)
        .ok_or_else(|| format!("task record row decode `outcome` unknown value: {raw}"))
}

fn decode_task_record(row: &impl TaskRecordDbRow) -> Result<TaskRecord, String> {
    let status_str = row
        .string_column("status")
        .map_err(|e| task_record_decode_error("row", "status", e))?;
    let status = TaskStatus::parse_status(&status_str)
        .ok_or_else(|| format!("unknown persisted task status: {status_str}"))?;

    Ok(TaskRecord {
        task_id: row
            .string_column("task_id")
            .map_err(|e| task_record_decode_error("row", "task_id", e))?,
        user_id: row
            .string_column("user_id")
            .map_err(|e| task_record_decode_error("row", "user_id", e))?,
        session_id: task_record_optional_string(row, "session_id")?,
        parent_task_id: task_record_optional_string(row, "parent_task_id")?,
        title: row
            .string_column("title")
            .map_err(|e| task_record_decode_error("row", "title", e))?,
        description: task_record_optional_string(row, "description")?,
        status,
        progress_pct: task_record_progress_pct(row)?,
        items_done: task_record_non_negative_u32(row, "items_done")?,
        items_total: task_record_non_negative_u32(row, "items_total")?,
        plan: task_record_optional_json(row, "plan_json")?,
        checkpoint: task_record_optional_json(row, "checkpoint_json")?,
        error_message: task_record_optional_string(row, "error_message")?,
        created_at: row
            .string_column("created_at")
            .map_err(|e| task_record_decode_error("row", "created_at", e))?,
        updated_at: row
            .string_column("updated_at")
            .map_err(|e| task_record_decode_error("row", "updated_at", e))?,
        completed_at: task_record_optional_string(row, "completed_at")?,
        user_rating: task_record_user_rating(row)?,
        completion_time_sec: task_record_optional_non_negative_i32(row, "completion_time_sec")?,
        replan_count: task_record_non_negative_u32(row, "replan_count")?,
        auto_adjustments: task_record_non_negative_u32(row, "auto_adjustments")?,
        outcome: task_record_outcome(row)?,
        project_type: task_record_optional_string(row, "project_type")?,
        goal_pattern: task_record_optional_string(row, "goal_pattern")?,
        agent_id: task_record_optional_string(row, "agent_id")?,
    })
}

impl MatrixOneTaskService {
    pub fn new(pool: sqlx::Pool<sqlx::MySql>) -> Self {
        Self { pool }
    }

    pub fn from_shared(shared: &astra_core::SharedPool) -> Self {
        Self {
            pool: shared.get().clone(),
        }
    }

    /// Parse a row from `agent_tasks` (shared with task pack sync / multi-agent helpers).
    pub fn parse_mysql_row(row: &sqlx::mysql::MySqlRow) -> Result<TaskRecord, String> {
        decode_task_record(row)
    }

    /// Convert a 0-rows-affected outcome from a guarded UPDATE into a structured
    /// error: distinguish "not found" from "terminal-state immutability" by
    /// reading the row's current status.
    async fn report_terminal_guard_violation(
        &self,
        user_id: &str,
        task_id: &str,
        attempted: &str,
    ) -> Result<(), String> {
        let row = sqlx::query("SELECT status FROM agent_tasks WHERE user_id = ? AND task_id = ?")
            .bind(user_id)
            .bind(task_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| format!("terminal_guard check: {e}"))?;
        match row {
            None => Err(format!("task not found: {task_id}")),
            Some(r) => {
                let cur_status = decode_task_status_guard_row(&r, task_id)?;
                if cur_status.is_terminal() {
                    Err(format!(
                        "invalid task status transition: {} → {} (terminal states are immutable)",
                        cur_status.as_str(),
                        attempted
                    ))
                } else {
                    // The guard rejected the update for a non-status reason
                    // (should not happen given current SQL); surface a generic
                    // structured error rather than a silent success.
                    Err(format!(
                        "task {task_id} update rejected (status={}, attempted={attempted})",
                        cur_status.as_str()
                    ))
                }
            }
        }
    }

    pub fn parse_mysql_list_row(row: &sqlx::mysql::MySqlRow) -> Result<TaskListItem, String> {
        decode_task_list_item(row)
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

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| format!("create_task begin: {e}"))?;

        let result = sqlx::query(
            "INSERT INTO agent_tasks \
             (task_id, user_id, session_id, parent_task_id, title, description, status, \
              items_total, plan_json, project_type, goal_pattern, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, 'pending', ?, ?, ?, ?, NOW(), NOW())",
        )
        .bind(&task_id)
        .bind(user_id)
        .bind(session_id)
        .bind(&req.parent_task_id)
        .bind(&req.title)
        .bind(&req.description)
        .bind(items_total)
        .bind(&plan_json)
        .bind(&req.project_type)
        .bind(&req.goal_pattern)
        .execute(&mut *tx)
        .await;

        if let Err(e) = result {
            let _ = tx.rollback().await;
            return Err(format!("create_task: {e}"));
        }

        tx.commit()
            .await
            .map_err(|e| format!("create_task commit: {e}"))?;

        tracing::info!(
            target: "astra_services::task_orchestrator",
            task_id = %task_id,
            user_id,
            session_id,
            "durable task created"
        );

        Ok(task_id)
    }

    async fn get_task(&self, user_id: &str, task_id: &str) -> Result<Option<TaskRecord>, String> {
        let row = sqlx::query(&format!(
            "SELECT {AGENT_TASK_DETAIL_SELECT_COLUMNS} FROM agent_tasks WHERE user_id = ? AND task_id = ?"
        ))
        .bind(user_id)
        .bind(task_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| format!("get_task: {e}"))?;

        match row {
            Some(ref r) => Ok(Some(Self::parse_mysql_row(r)?)),
            None => Ok(None),
        }
    }

    async fn list_recent_tasks(
        &self,
        user_id: &str,
        status_filter: Option<TaskStatus>,
    ) -> Result<Vec<TaskListItem>, String> {
        let rows = if let Some(status) = status_filter {
            sqlx::query(&format!(
                "SELECT {AGENT_TASK_LIST_SELECT_COLUMNS} \
                 FROM agent_tasks WHERE user_id = ? AND status = ? ORDER BY updated_at DESC LIMIT {}",
                MAX_TASK_LIST_ROWS
            ))
            .bind(user_id)
            .bind(status.as_str())
            .fetch_all(&self.pool)
            .await
        } else {
            sqlx::query(&format!(
                "SELECT {AGENT_TASK_LIST_SELECT_COLUMNS} \
                 FROM agent_tasks WHERE user_id = ? ORDER BY updated_at DESC LIMIT {}",
                MAX_TASK_LIST_ROWS
            ))
            .bind(user_id)
            .fetch_all(&self.pool)
            .await
        }
        .map_err(|e| format!("list_recent_tasks: {e}"))?;

        rows.iter().map(Self::parse_mysql_list_row).collect()
    }

    async fn list_recent_tasks_for_session(
        &self,
        user_id: &str,
        session_id: &str,
        status_filter: Option<TaskStatus>,
    ) -> Result<Vec<TaskListItem>, String> {
        let rows = if let Some(status) = status_filter {
            sqlx::query(&format!(
                "SELECT {AGENT_TASK_LIST_SELECT_COLUMNS} \
                 FROM agent_tasks WHERE user_id = ? AND session_id = ? AND status = ? \
                 ORDER BY updated_at DESC LIMIT {}",
                MAX_TASK_LIST_ROWS
            ))
            .bind(user_id)
            .bind(session_id)
            .bind(status.as_str())
            .fetch_all(&self.pool)
            .await
        } else {
            sqlx::query(&format!(
                "SELECT {AGENT_TASK_LIST_SELECT_COLUMNS} \
                 FROM agent_tasks WHERE user_id = ? AND session_id = ? \
                 ORDER BY updated_at DESC LIMIT {}",
                MAX_TASK_LIST_ROWS
            ))
            .bind(user_id)
            .bind(session_id)
            .fetch_all(&self.pool)
            .await
        }
        .map_err(|e| format!("list_recent_tasks_for_session: {e}"))?;

        rows.iter().map(Self::parse_mysql_list_row).collect()
    }

    async fn search_tasks(
        &self,
        user_id: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<TaskListItem>, String> {
        let query = query.trim();
        if query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }

        let like_fragment = escape_sql_like_fragment(query);
        let task_id_prefix = format!("{like_fragment}%");
        let title_substring = format!("%{like_fragment}%");
        let sql_limit = limit.clamp(1, MAX_TASK_QUERY_MATCH_ROWS);
        let rows = sqlx::query(&format!(
            "SELECT {AGENT_TASK_LIST_SELECT_COLUMNS}, \
                    CASE \
                        WHEN task_id = ? THEN 1 \
                        WHEN task_id LIKE ? ESCAPE '\\\\' THEN 2 \
                        WHEN LOWER(title) = LOWER(?) THEN 3 \
                        WHEN LOWER(title) LIKE LOWER(?) ESCAPE '\\\\' THEN 4 \
                        ELSE 5 \
                    END AS match_rank \
             FROM agent_tasks \
             WHERE user_id = ? AND ( \
                    task_id = ? \
                 OR task_id LIKE ? ESCAPE '\\\\' \
                 OR LOWER(title) = LOWER(?) \
                 OR LOWER(title) LIKE LOWER(?) ESCAPE '\\\\' \
             ) \
             ORDER BY match_rank ASC, updated_at DESC \
             LIMIT {}",
            sql_limit
        ))
        .bind(query)
        .bind(&task_id_prefix)
        .bind(query)
        .bind(&title_substring)
        .bind(user_id)
        .bind(query)
        .bind(&task_id_prefix)
        .bind(query)
        .bind(&title_substring)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| format!("search_tasks: {e}"))?;

        let mut matches = Vec::new();
        let mut best_rank: Option<i32> = None;
        for row in rows {
            let rank: i32 = row.try_get("match_rank").map_err(|e| e.to_string())?;
            if best_rank.is_none() {
                best_rank = Some(rank);
            }
            if Some(rank) != best_rank {
                break;
            }
            matches.push(Self::parse_mysql_list_row(&row)?);
        }
        Ok(matches)
    }

    async fn list_claimable_tasks_for_worker(
        &self,
        user_id: &str,
        limit: usize,
    ) -> Result<Vec<TaskListItem>, String> {
        crate::multi_agent::task_lease::list_claimable_tasks_mysql(&self.pool, user_id, limit).await
    }

    async fn update_status(
        &self,
        user_id: &str,
        task_id: &str,
        status: TaskStatus,
    ) -> Result<(), String> {
        // Atomic transition guard: the WHERE clause rejects terminal and unknown
        // persisted states, eliminating the TOCTOU race of a separate SELECT + UPDATE.
        let result = if status.is_terminal() {
            sqlx::query(&guarded_agent_task_update_sql(
                "status = ?, updated_at = NOW(), completed_at = NOW()",
            ))
            .bind(status.as_str())
            .bind(user_id)
            .bind(task_id)
            .execute(&self.pool)
            .await
            .map_err(|e| format!("update_status: {e}"))?
        } else {
            sqlx::query(&guarded_agent_task_update_sql(
                "status = ?, updated_at = NOW(), completed_at = NULL",
            ))
            .bind(status.as_str())
            .bind(user_id)
            .bind(task_id)
            .execute(&self.pool)
            .await
            .map_err(|e| format!("update_status: {e}"))?
        };

        if result.rows_affected() == 0 {
            // Either task doesn't exist or is in a terminal state.
            let current_row =
                sqlx::query("SELECT status FROM agent_tasks WHERE user_id = ? AND task_id = ?")
                    .bind(user_id)
                    .bind(task_id)
                    .fetch_optional(&self.pool)
                    .await
                    .map_err(|e| format!("update_status check: {e}"))?;
            if let Some(row) = current_row {
                let current = decode_task_status_guard_row(&row, task_id)?;
                if current.is_terminal() && status != current {
                    return Err(format!(
                        "invalid task status transition: {} → {} (terminal states are immutable)",
                        current.as_str(),
                        status.as_str()
                    ));
                }
            } else {
                return Err(format!("task not found: {task_id}"));
            }
        }
        if status.is_terminal() {
            tracing::info!(
                target: "astra_services::task_orchestrator",
                task_id,
                status = status.as_str(),
                "task status terminal"
            );
        }
        Ok(())
    }

    async fn update_progress(
        &self,
        user_id: &str,
        task_id: &str,
        progress_pct: u32,
        items_done: u32,
        items_total: u32,
    ) -> Result<(), String> {
        let result = sqlx::query(&guarded_agent_task_update_sql(
            "progress_pct = ?, items_done = ?, items_total = ?, updated_at = NOW()",
        ))
        .bind(progress_pct as i32)
        .bind(items_done as i32)
        .bind(items_total as i32)
        .bind(user_id)
        .bind(task_id)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("update_progress: {e}"))?;
        if result.rows_affected() == 0 {
            return self
                .report_terminal_guard_violation(user_id, task_id, "progress")
                .await;
        }
        Ok(())
    }

    async fn save_checkpoint(
        &self,
        user_id: &str,
        task_id: &str,
        checkpoint: &TaskCheckpoint,
    ) -> Result<(), String> {
        let ckpt_json =
            serde_json::to_string(checkpoint).map_err(|e| format!("serialize ckpt: {e}"))?;
        let result = sqlx::query(&guarded_agent_task_update_sql(
            "checkpoint_json = ?, updated_at = NOW()",
        ))
        .bind(&ckpt_json)
        .bind(user_id)
        .bind(task_id)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("save_checkpoint: {e}"))?;
        if result.rows_affected() == 0 {
            return self
                .report_terminal_guard_violation(user_id, task_id, "checkpoint")
                .await;
        }
        Ok(())
    }

    async fn update_plan(
        &self,
        user_id: &str,
        task_id: &str,
        plan: &TaskPlan,
    ) -> Result<(), String> {
        let plan_json = serde_json::to_string(plan).map_err(|e| format!("serialize plan: {e}"))?;
        let progress = plan.progress_pct();
        let done = plan.items_done();
        let total = plan.subtasks.len() as i32;

        let result = sqlx::query(&guarded_agent_task_update_sql(
            "plan_json = ?, progress_pct = ?, items_done = ?, items_total = ?, updated_at = NOW()",
        ))
        .bind(&plan_json)
        .bind(progress as i32)
        .bind(done as i32)
        .bind(total)
        .bind(user_id)
        .bind(task_id)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("update_plan: {e}"))?;
        if result.rows_affected() == 0 {
            return self
                .report_terminal_guard_violation(user_id, task_id, "plan")
                .await;
        }
        Ok(())
    }

    async fn fail_task(&self, user_id: &str, task_id: &str, error: &str) -> Result<(), String> {
        let result = sqlx::query(&guarded_agent_task_update_sql(
            "status = 'failed', outcome = 'failed', error_message = ?, \
             updated_at = NOW(), completed_at = NOW()",
        ))
        .bind(error)
        .bind(user_id)
        .bind(task_id)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("fail_task: {e}"))?;
        if result.rows_affected() == 0 {
            return self
                .report_terminal_guard_violation(user_id, task_id, "failed")
                .await;
        }
        let preview: String = error.chars().take(200).collect();
        tracing::warn!(
            target: "astra_services::task_orchestrator",
            task_id,
            error = %preview,
            "task marked failed"
        );
        Ok(())
    }

    async fn complete_task(&self, user_id: &str, task_id: &str) -> Result<(), String> {
        self.complete_task_with_outcome(user_id, task_id, TaskOutcome::Success)
            .await
    }

    async fn complete_task_with_outcome(
        &self,
        user_id: &str,
        task_id: &str,
        outcome: TaskOutcome,
    ) -> Result<(), String> {
        let result = sqlx::query(&guarded_agent_task_update_sql(
            "status = 'completed', progress_pct = 100, \
             outcome = ?, error_message = NULL, \
             updated_at = NOW(), completed_at = NOW()",
        ))
        .bind(outcome.as_str())
        .bind(user_id)
        .bind(task_id)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("complete_task_with_outcome: {e}"))?;
        if result.rows_affected() == 0 {
            return self
                .report_terminal_guard_violation(user_id, task_id, "completed")
                .await;
        }
        tracing::info!(
            target: "astra_services::task_orchestrator",
            task_id,
            outcome = outcome.as_str(),
            "task completed"
        );
        Ok(())
    }

    async fn complete_plan_run(
        &self,
        user_id: &str,
        task_id: &str,
        progress_pct: u32,
        items_done: u32,
        items_total: u32,
        outcome: TaskOutcome,
    ) -> Result<(), String> {
        let result = sqlx::query(&guarded_agent_task_update_sql(
            "status = 'completed', progress_pct = ?, items_done = ?, \
             items_total = ?, outcome = ?, error_message = NULL, \
             updated_at = NOW(), completed_at = NOW()",
        ))
        .bind(progress_pct as i32)
        .bind(items_done as i32)
        .bind(items_total as i32)
        .bind(outcome.as_str())
        .bind(user_id)
        .bind(task_id)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("complete_plan_run: {e}"))?;
        if result.rows_affected() == 0 {
            return self
                .report_terminal_guard_violation(user_id, task_id, "completed")
                .await;
        }
        tracing::info!(
            target: "astra_services::task_orchestrator",
            task_id,
            outcome = outcome.as_str(),
            progress_pct,
            items_done,
            items_total,
            "plan run completed"
        );
        Ok(())
    }

    async fn record_feedback(
        &self,
        user_id: &str,
        task_id: &str,
        rating: u8,
        outcome: TaskOutcome,
        completion_time_sec: Option<i32>,
    ) -> Result<(), String> {
        let result = sqlx::query(
            "UPDATE agent_tasks SET user_rating = ?, outcome = ?, completion_time_sec = ?, \
             updated_at = NOW() WHERE user_id = ? AND task_id = ?",
        )
        .bind(rating as i8)
        .bind(outcome.as_str())
        .bind(completion_time_sec)
        .bind(user_id)
        .bind(task_id)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("record_feedback: {e}"))?;
        if result.rows_affected() == 0 {
            return Err(format!("task not found: {task_id}"));
        }
        Ok(())
    }

    async fn increment_replan_count(&self, user_id: &str, task_id: &str) -> Result<(), String> {
        let result = sqlx::query(
            "UPDATE agent_tasks SET replan_count = replan_count + 1, updated_at = NOW() WHERE user_id = ? AND task_id = ?",
        )
        .bind(user_id)
        .bind(task_id)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("increment_replan_count: {e}"))?;
        if result.rows_affected() == 0 {
            return Err(format!("task not found: {task_id}"));
        }
        Ok(())
    }

    async fn extract_template(
        &self,
        user_id: &str,
        task_id: &str,
        goal_pattern: &str,
    ) -> Result<Option<String>, String> {
        use sqlx::Row;

        // Fetch the task
        let task = self
            .get_task(user_id, task_id)
            .await?
            .ok_or_else(|| format!("task not found: {task_id}"))?;

        // Check if task is eligible for template extraction.
        // Criteria: rating >= 4 OR (successful outcome AND replan_count <= 1).
        let eligible = task.user_rating.map(|r| r >= 4).unwrap_or(false)
            || (task.outcome == Some(TaskOutcome::Success) && task.replan_count <= 1);

        if !eligible || task.plan.is_none() {
            return Ok(None);
        }

        let plan = task
            .plan
            .as_ref()
            .ok_or_else(|| format!("task plan missing after eligibility check: {task_id}"))?;
        let template_id = uuid::Uuid::new_v4().to_string();
        let template_json =
            serde_json::to_string(plan).map_err(|e| format!("serialize plan: {e}"))?;

        // Check if similar template exists
        let existing: Option<String> = sqlx::query(
            "SELECT template_id FROM plan_templates \
             WHERE user_id = ? AND goal_pattern = ? AND project_type <=> ? LIMIT 1",
        )
        .bind(&task.user_id)
        .bind(goal_pattern)
        .bind(&task.project_type)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| format!("check existing template: {e}"))?
        .and_then(|row| row.try_get("template_id").ok());

        if let Some(existing_id) = existing {
            // Update existing template with better version
            let rating = task.user_rating.unwrap_or(4) as f32;
            sqlx::query(
                "UPDATE plan_templates SET \
                 template_json = ?, \
                 success_rate = (success_rate * use_count + ?) / (use_count + 1), \
                 avg_completion_time = COALESCE(?, avg_completion_time), \
                 use_count = use_count + 1, \
                 updated_at = NOW() \
                 WHERE user_id = ? AND template_id = ?",
            )
            .bind(&template_json)
            .bind(rating / 5.0)
            .bind(task.completion_time_sec)
            .bind(&task.user_id)
            .bind(&existing_id)
            .execute(&self.pool)
            .await
            .map_err(|e| format!("update template: {e}"))?;

            return Ok(Some(existing_id));
        }

        // Insert new template
        let success_rate = task.user_rating.map(|r| r as f32 / 5.0).unwrap_or(0.8);
        sqlx::query(
            "INSERT INTO plan_templates \
             (template_id, user_id, goal_pattern, project_type, template_json, \
              success_rate, avg_completion_time, use_count, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, 1, NOW(), NOW())",
        )
        .bind(&template_id)
        .bind(&task.user_id)
        .bind(goal_pattern)
        .bind(&task.project_type)
        .bind(&template_json)
        .bind(success_rate)
        .bind(task.completion_time_sec)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("insert template: {e}"))?;

        Ok(Some(template_id))
    }

    async fn recommend_templates(
        &self,
        user_id: &str,
        goal: &str,
        project_type: Option<&str>,
        limit: usize,
    ) -> Result<Vec<TemplateRecommendation>, String> {
        // Extract keywords from goal for matching
        let keywords: Vec<&str> = goal
            .split_whitespace()
            .filter(|w| w.len() > 2)
            .take(5)
            .collect();

        if keywords.is_empty() {
            return Ok(vec![]);
        }

        // Build parameterised LIKE patterns: CONCAT('%', ?, '%')
        let like_conditions: Vec<String> = (0..keywords.len())
            .map(|_| "goal_pattern LIKE CONCAT('%', ?, '%')".to_string())
            .collect();
        let like_clause = like_conditions.join(" OR ");

        // Build bind arrays: user_id appears twice (is_own check + score calc),
        // then keywords, then project_type x2, then limit.
        let sql = format!(
            "SELECT template_id, user_id, goal_pattern, project_type, template_json, \
             success_rate, avg_completion_time, use_count, created_at, updated_at, \
             CASE WHEN user_id = ? THEN 1 ELSE 0 END as is_own, \
             (success_rate * 0.4 + (CASE WHEN use_count < 10 THEN use_count ELSE 10 END) / 10.0 * 0.3 + \
              CASE WHEN user_id = ? THEN 0.3 ELSE 0.0 END) as score \
             FROM plan_templates \
             WHERE user_id = ? AND ({}) \
             AND (project_type IS NULL OR project_type = ? OR ? IS NULL) \
             ORDER BY score DESC, use_count DESC \
             LIMIT ?",
            like_clause
        );
        let mut query = sqlx::query(&sql).bind(user_id).bind(user_id).bind(user_id);

        for kw in &keywords {
            query = query.bind(kw);
        }
        query = query
            .bind(project_type)
            .bind(project_type)
            .bind(limit as i32);

        let rows = query
            .fetch_all(&self.pool)
            .await
            .map_err(|e| format!("query templates: {e}"))?;

        let mut recommendations = Vec::new();
        for row in rows {
            recommendations.push(decode_template_recommendation(&row)?);
        }

        Ok(recommendations)
    }

    async fn get_learning_stats(
        &self,
        user_id: &str,
        goal_pattern: &str,
    ) -> Result<LearningStats, String> {
        // Extract keywords for pattern matching
        let keywords: Vec<&str> = goal_pattern
            .split_whitespace()
            .filter(|w| w.len() > 2)
            .take(3)
            .collect();

        if keywords.is_empty() {
            return Ok(LearningStats::default());
        }

        // Build parameterised LIKE patterns: CONCAT('%', ?, '%')
        let like_conditions: Vec<String> = (0..keywords.len())
            .map(|_| "title LIKE CONCAT('%', ?, '%')".to_string())
            .collect();
        let like_clause = like_conditions.join(" OR ");

        let sql = format!(
            "SELECT \
             COUNT(*) as total_tasks, \
             COALESCE(SUM(CASE WHEN outcome = 'success' THEN 1 ELSE 0 END), 0) as completed_tasks, \
             AVG(user_rating) as avg_rating, \
             AVG(replan_count) as avg_replan_count \
             FROM agent_tasks \
             WHERE user_id = ? AND ({})",
            like_clause
        );
        let mut query = sqlx::query(&sql).bind(user_id);

        for kw in &keywords {
            query = query.bind(kw);
        }

        let row = query
            .fetch_one(&self.pool)
            .await
            .map_err(|e| format!("query stats: {e}"))?;

        decode_learning_stats(&row)
    }

    async fn record_template_usage(&self, user_id: &str, template_id: &str) -> Result<(), String> {
        let result = sqlx::query(
            "UPDATE plan_templates SET use_count = use_count + 1, updated_at = NOW() \
             WHERE user_id = ? AND template_id = ?",
        )
        .bind(user_id)
        .bind(template_id)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("record_template_usage: {e}"))?;
        if result.rows_affected() == 0 {
            return Err(format!("template not found: {template_id}"));
        }
        Ok(())
    }
}

// ─── Local-Only Implementation (Offline) ────────────────────────────────────

/// File-based task service for offline/edge-only mode.
/// Stores tasks as JSON files in `~/.astra/tasks/`.
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

    fn load_owned_task(&self, user_id: &str, task_id: &str) -> Result<Option<TaskRecord>, String> {
        match self.load_task(task_id)? {
            Some(record) if record.user_id == user_id => Ok(Some(record)),
            _ => Ok(None),
        }
    }

    fn require_owned_task(&self, user_id: &str, task_id: &str) -> Result<TaskRecord, String> {
        self.load_owned_task(user_id, task_id)?
            .ok_or_else(|| format!("task not found: {task_id}"))
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
            // Learning fields
            user_rating: None,
            completion_time_sec: None,
            replan_count: 0,
            auto_adjustments: 0,
            outcome: None,
            project_type: req.project_type,
            goal_pattern: req.goal_pattern,
            agent_id: None,
        };
        self.save_task(&record)?;
        Ok(task_id)
    }

    async fn get_task(&self, user_id: &str, task_id: &str) -> Result<Option<TaskRecord>, String> {
        self.load_owned_task(user_id, task_id)
    }

    async fn list_recent_tasks(
        &self,
        user_id: &str,
        status_filter: Option<TaskStatus>,
    ) -> Result<Vec<TaskListItem>, String> {
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
                tasks.push(TaskListItem {
                    task_id: record.task_id,
                    title: record.title,
                    session_id: record.session_id,
                    status: record.status,
                    progress_pct: record.progress_pct,
                    items_done: record.items_done,
                    items_total: record.items_total,
                    created_at: record.created_at,
                    updated_at: record.updated_at,
                    completed_at: record.completed_at,
                    outcome: record.outcome,
                    error_message: record.error_message,
                    project_type: record.project_type,
                    claimability: None,
                });
            }
        }
        tasks.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(tasks)
    }

    async fn list_recent_tasks_for_session(
        &self,
        user_id: &str,
        session_id: &str,
        status_filter: Option<TaskStatus>,
    ) -> Result<Vec<TaskListItem>, String> {
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
                && record.session_id.as_deref() == Some(session_id)
                && status_filter
                    .as_ref()
                    .is_none_or(|filter| record.status == *filter)
            {
                tasks.push(TaskListItem {
                    task_id: record.task_id,
                    title: record.title,
                    session_id: record.session_id,
                    status: record.status,
                    progress_pct: record.progress_pct,
                    items_done: record.items_done,
                    items_total: record.items_total,
                    created_at: record.created_at,
                    updated_at: record.updated_at,
                    completed_at: record.completed_at,
                    outcome: record.outcome,
                    error_message: record.error_message,
                    project_type: record.project_type,
                    claimability: None,
                });
            }
        }
        tasks.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(tasks)
    }

    async fn search_tasks(
        &self,
        user_id: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<TaskListItem>, String> {
        let query = query.trim();
        if query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }

        let tasks = self.list_recent_tasks(user_id, None).await?;
        Ok(select_best_task_query_matches(
            tasks,
            query,
            limit.clamp(1, MAX_TASK_QUERY_MATCH_ROWS),
        ))
    }

    async fn list_claimable_tasks_for_worker(
        &self,
        user_id: &str,
        limit: usize,
    ) -> Result<Vec<TaskListItem>, String> {
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
                && record.status.is_claimable()
            {
                tasks.push(TaskListItem {
                    task_id: record.task_id,
                    title: record.title,
                    session_id: record.session_id,
                    status: record.status,
                    progress_pct: record.progress_pct,
                    items_done: record.items_done,
                    items_total: record.items_total,
                    created_at: record.created_at,
                    updated_at: record.updated_at,
                    completed_at: record.completed_at,
                    outcome: record.outcome,
                    error_message: record.error_message,
                    project_type: record.project_type,
                    claimability: TaskClaimability::for_status(record.status),
                });
            }
        }
        tasks.sort_by(|left, right| left.created_at.cmp(&right.created_at));
        tasks.truncate(limit.max(1));
        Ok(tasks)
    }

    async fn update_status(
        &self,
        user_id: &str,
        task_id: &str,
        status: TaskStatus,
    ) -> Result<(), String> {
        let mut record = self.require_owned_task(user_id, task_id)?;
        if record.status.is_terminal() && record.status != status {
            return Err(format!(
                "invalid task status transition: {} → {} (terminal states are immutable)",
                record.status.as_str(),
                status.as_str()
            ));
        }
        record.status = status;
        record.updated_at = chrono::Utc::now().to_rfc3339();
        if status.is_terminal() {
            record.completed_at = Some(record.updated_at.clone());
        }
        self.save_task(&record)
    }

    async fn update_progress(
        &self,
        user_id: &str,
        task_id: &str,
        progress_pct: u32,
        items_done: u32,
        items_total: u32,
    ) -> Result<(), String> {
        let mut record = self.require_owned_task(user_id, task_id)?;
        if record.status.is_terminal() {
            return Err(format!(
                "cannot update progress on terminal task {task_id} (status={})",
                record.status.as_str()
            ));
        }
        record.progress_pct = progress_pct;
        record.items_done = items_done;
        record.items_total = items_total;
        record.updated_at = chrono::Utc::now().to_rfc3339();
        self.save_task(&record)
    }

    async fn save_checkpoint(
        &self,
        user_id: &str,
        task_id: &str,
        checkpoint: &TaskCheckpoint,
    ) -> Result<(), String> {
        let mut record = self.require_owned_task(user_id, task_id)?;
        if record.status.is_terminal() {
            return Err(format!(
                "cannot save checkpoint on terminal task {task_id} (status={})",
                record.status.as_str()
            ));
        }
        record.checkpoint = Some(checkpoint.clone());
        record.updated_at = chrono::Utc::now().to_rfc3339();
        self.save_task(&record)
    }

    async fn update_plan(
        &self,
        user_id: &str,
        task_id: &str,
        plan: &TaskPlan,
    ) -> Result<(), String> {
        let mut record = self.require_owned_task(user_id, task_id)?;
        if record.status.is_terminal() {
            return Err(format!(
                "cannot update plan on terminal task {task_id} (status={})",
                record.status.as_str()
            ));
        }
        record.progress_pct = plan.progress_pct();
        record.items_done = plan.items_done();
        record.items_total = plan.subtasks.len() as u32;
        record.plan = Some(plan.clone());
        record.updated_at = chrono::Utc::now().to_rfc3339();
        self.save_task(&record)
    }

    async fn fail_task(&self, user_id: &str, task_id: &str, error: &str) -> Result<(), String> {
        let mut record = self.require_owned_task(user_id, task_id)?;
        if record.status.is_terminal() {
            return Err(format!(
                "invalid task status transition: {} → failed (terminal states are immutable)",
                record.status.as_str()
            ));
        }
        record.status = TaskStatus::Failed;
        record.outcome = Some(TaskOutcome::Failed);
        record.error_message = Some(error.to_string());
        let now = chrono::Utc::now().to_rfc3339();
        record.updated_at = now.clone();
        record.completed_at = Some(now);
        self.save_task(&record)
    }

    async fn complete_task(&self, user_id: &str, task_id: &str) -> Result<(), String> {
        self.complete_task_with_outcome(user_id, task_id, TaskOutcome::Success)
            .await
    }

    async fn complete_task_with_outcome(
        &self,
        user_id: &str,
        task_id: &str,
        outcome: TaskOutcome,
    ) -> Result<(), String> {
        let mut record = self.require_owned_task(user_id, task_id)?;
        if record.status.is_terminal() {
            return Err(format!(
                "invalid task status transition: {} → completed (terminal states are immutable)",
                record.status.as_str()
            ));
        }
        record.status = TaskStatus::Completed;
        record.progress_pct = 100;
        record.outcome = Some(outcome);
        record.error_message = None;
        let now = chrono::Utc::now().to_rfc3339();
        record.updated_at = now.clone();
        record.completed_at = Some(now);
        self.save_task(&record)
    }

    async fn complete_plan_run(
        &self,
        user_id: &str,
        task_id: &str,
        progress_pct: u32,
        items_done: u32,
        items_total: u32,
        outcome: TaskOutcome,
    ) -> Result<(), String> {
        let mut record = self.require_owned_task(user_id, task_id)?;
        if record.status.is_terminal() {
            return Err(format!(
                "invalid task status transition: {} → completed (terminal states are immutable)",
                record.status.as_str()
            ));
        }
        record.status = TaskStatus::Completed;
        record.progress_pct = progress_pct;
        record.items_done = items_done;
        record.items_total = items_total;
        record.outcome = Some(outcome);
        record.error_message = None;
        let now = chrono::Utc::now().to_rfc3339();
        record.updated_at = now.clone();
        record.completed_at = Some(now);
        self.save_task(&record)
    }

    async fn record_feedback(
        &self,
        user_id: &str,
        task_id: &str,
        rating: u8,
        outcome: TaskOutcome,
        completion_time_sec: Option<i32>,
    ) -> Result<(), String> {
        let mut record = self.require_owned_task(user_id, task_id)?;
        record.user_rating = Some(rating);
        record.outcome = Some(outcome);
        record.completion_time_sec = completion_time_sec;
        record.updated_at = chrono::Utc::now().to_rfc3339();
        self.save_task(&record)
    }

    async fn increment_replan_count(&self, user_id: &str, task_id: &str) -> Result<(), String> {
        let mut record = self.require_owned_task(user_id, task_id)?;
        record.replan_count += 1;
        record.updated_at = chrono::Utc::now().to_rfc3339();
        self.save_task(&record)
    }

    // ─── Learning Methods (Local Storage) ───

    async fn extract_template(
        &self,
        user_id: &str,
        task_id: &str,
        goal_pattern: &str,
    ) -> Result<Option<String>, String> {
        let task = self.require_owned_task(user_id, task_id)?;

        // Check eligibility
        let eligible = task.user_rating.map(|r| r >= 4).unwrap_or(false)
            || (task.outcome == Some(TaskOutcome::Success) && task.replan_count <= 1);

        if !eligible || task.plan.is_none() {
            return Ok(None);
        }

        // Store template locally
        let template_id = uuid::Uuid::new_v4().to_string();
        let templates_dir = self
            .tasks_dir
            .parent()
            .unwrap_or(&self.tasks_dir)
            .join("templates");
        std::fs::create_dir_all(&templates_dir).map_err(|e| format!("mkdir templates: {e}"))?;

        let template = PlanTemplate {
            template_id: template_id.clone(),
            user_id: Some(task.user_id.clone()),
            goal_pattern: goal_pattern.to_string(),
            project_type: task.project_type.clone(),
            template: task
                .plan
                .clone()
                .ok_or_else(|| format!("task plan missing after eligibility check: {task_id}"))?,
            success_rate: task.user_rating.map(|r| r as f32 / 5.0).unwrap_or(0.8),
            avg_completion_time: task.completion_time_sec,
            use_count: 1,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
        };

        let path = templates_dir.join(format!("{}.json", template_id));
        let json = serde_json::to_string_pretty(&template)
            .map_err(|e| format!("serialize template: {e}"))?;
        std::fs::write(&path, &json).map_err(|e| format!("write template: {e}"))?;

        Ok(Some(template_id))
    }

    async fn recommend_templates(
        &self,
        _user_id: &str,
        goal: &str,
        project_type: Option<&str>,
        limit: usize,
    ) -> Result<Vec<TemplateRecommendation>, String> {
        let templates_dir = self
            .tasks_dir
            .parent()
            .unwrap_or(&self.tasks_dir)
            .join("templates");

        if !templates_dir.exists() {
            return Ok(vec![]);
        }

        let keywords: Vec<&str> = goal.split_whitespace().filter(|w| w.len() > 2).collect();

        let mut recommendations = Vec::new();

        // Read all templates and score them
        if let Ok(entries) = std::fs::read_dir(&templates_dir) {
            for entry in entries.flatten() {
                if entry
                    .path()
                    .extension()
                    .map(|e| e == "json")
                    .unwrap_or(false)
                    && let Ok(data) = std::fs::read_to_string(entry.path())
                    && let Ok(template) = serde_json::from_str::<PlanTemplate>(&data)
                {
                    // Score based on keyword match
                    let pattern_lower = template.goal_pattern.to_lowercase();
                    let matches = keywords
                        .iter()
                        .filter(|k| pattern_lower.contains(&k.to_lowercase()))
                        .count();

                    if matches == 0 {
                        continue;
                    }

                    // Check project type
                    if let Some(pt) = project_type
                        && let Some(ref tpt) = template.project_type
                        && !tpt.eq_ignore_ascii_case(pt)
                    {
                        continue;
                    }

                    let score = (matches as f32 / keywords.len().max(1) as f32) * 0.5
                        + template.success_rate * 0.3
                        + (template.use_count.min(10) as f32 / 10.0) * 0.2;

                    recommendations.push(TemplateRecommendation {
                        score,
                        reason: format!("Local pattern: {}", template.goal_pattern),
                        template,
                    });
                }
            }
        }

        // Sort by score descending
        recommendations.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        recommendations.truncate(limit);

        Ok(recommendations)
    }

    async fn get_learning_stats(
        &self,
        _user_id: &str,
        goal_pattern: &str,
    ) -> Result<LearningStats, String> {
        let keywords: Vec<&str> = goal_pattern
            .split_whitespace()
            .filter(|w| w.len() > 2)
            .collect();

        if keywords.is_empty() {
            return Ok(LearningStats::default());
        }

        let mut total_tasks = 0u32;
        let mut completed_tasks = 0u32;
        let mut rating_sum = 0.0f32;
        let mut rating_count = 0u32;
        let mut replan_sum = 0u32;

        // Scan all task files
        if let Ok(entries) = std::fs::read_dir(&self.tasks_dir) {
            for entry in entries.flatten() {
                if entry
                    .path()
                    .extension()
                    .map(|e| e == "json")
                    .unwrap_or(false)
                    && let Ok(data) = std::fs::read_to_string(entry.path())
                    && let Ok(task) = serde_json::from_str::<TaskRecord>(&data)
                {
                    // Check if task matches pattern
                    let title_lower = task.title.to_lowercase();
                    let matches = keywords
                        .iter()
                        .any(|k| title_lower.contains(&k.to_lowercase()));

                    if !matches {
                        continue;
                    }

                    total_tasks += 1;
                    if task.outcome == Some(TaskOutcome::Success) {
                        completed_tasks += 1;
                    }
                    if let Some(r) = task.user_rating {
                        rating_sum += r as f32;
                        rating_count += 1;
                    }
                    replan_sum += task.replan_count;
                }
            }
        }

        let avg_rating = if rating_count > 0 {
            Some(rating_sum / rating_count as f32)
        } else {
            None
        };

        let avg_replan_count = if total_tasks > 0 {
            replan_sum as f32 / total_tasks as f32
        } else {
            0.0
        };

        let inferred_success_rate = if total_tasks == 0 {
            0.0
        } else {
            let completion_factor = completed_tasks as f32 / total_tasks as f32;
            let replan_penalty = (avg_replan_count / 3.0).min(1.0);
            (completion_factor * (1.0 - replan_penalty * 0.3)).clamp(0.0, 1.0)
        };

        Ok(LearningStats {
            total_tasks,
            completed_tasks,
            avg_rating,
            avg_replan_count,
            inferred_success_rate,
        })
    }

    async fn record_template_usage(&self, _user_id: &str, template_id: &str) -> Result<(), String> {
        let templates_dir = self
            .tasks_dir
            .parent()
            .unwrap_or(&self.tasks_dir)
            .join("templates");
        let path = templates_dir.join(format!("{}.json", template_id));

        if !path.exists() {
            return Err(format!("template not found: {template_id}"));
        }

        let data = std::fs::read_to_string(&path).map_err(|e| format!("read template: {e}"))?;
        let mut template: PlanTemplate =
            serde_json::from_str(&data).map_err(|e| format!("parse template: {e}"))?;

        template.use_count += 1;
        template.updated_at = chrono::Utc::now().to_rfc3339();

        let json = serde_json::to_string_pretty(&template)
            .map_err(|e| format!("serialize template: {e}"))?;
        std::fs::write(&path, &json).map_err(|e| format!("write template: {e}"))?;

        Ok(())
    }
}

// ─── Unconfigured Fallback ──────────────────────────────────────────────────

/// Placeholder service used when no database or local backend is wired.
pub struct UnconfiguredTaskService;

#[async_trait]
impl TaskService for UnconfiguredTaskService {
    async fn create_task(&self, _: &str, _: &str, _: TaskCreateRequest) -> Result<String, String> {
        Err("task service not configured".into())
    }
    async fn get_task(&self, _: &str, _: &str) -> Result<Option<TaskRecord>, String> {
        Err("task service not configured".into())
    }
    async fn list_recent_tasks(
        &self,
        _: &str,
        _: Option<TaskStatus>,
    ) -> Result<Vec<TaskListItem>, String> {
        Err("task service not configured".into())
    }
    async fn list_recent_tasks_for_session(
        &self,
        _: &str,
        _: &str,
        _: Option<TaskStatus>,
    ) -> Result<Vec<TaskListItem>, String> {
        Err("task service not configured".into())
    }
    async fn search_tasks(&self, _: &str, _: &str, _: usize) -> Result<Vec<TaskListItem>, String> {
        Err("task service not configured".into())
    }
    async fn list_claimable_tasks_for_worker(
        &self,
        _: &str,
        _: usize,
    ) -> Result<Vec<TaskListItem>, String> {
        Err("task service not configured".into())
    }
    async fn update_status(&self, _: &str, _: &str, _: TaskStatus) -> Result<(), String> {
        Err("task service not configured".into())
    }
    async fn update_progress(
        &self,
        _: &str,
        _: &str,
        _: u32,
        _: u32,
        _: u32,
    ) -> Result<(), String> {
        Err("task service not configured".into())
    }
    async fn save_checkpoint(&self, _: &str, _: &str, _: &TaskCheckpoint) -> Result<(), String> {
        Err("task service not configured".into())
    }
    async fn update_plan(&self, _: &str, _: &str, _: &TaskPlan) -> Result<(), String> {
        Err("task service not configured".into())
    }
    async fn fail_task(&self, _: &str, _: &str, _: &str) -> Result<(), String> {
        Err("task service not configured".into())
    }
    async fn complete_task(&self, _: &str, _: &str) -> Result<(), String> {
        Err("task service not configured".into())
    }
    async fn complete_task_with_outcome(
        &self,
        _: &str,
        _: &str,
        _: TaskOutcome,
    ) -> Result<(), String> {
        Err("task service not configured".into())
    }
    async fn complete_plan_run(
        &self,
        _: &str,
        _: &str,
        _: u32,
        _: u32,
        _: u32,
        _: TaskOutcome,
    ) -> Result<(), String> {
        Err("task service not configured".into())
    }
    async fn record_feedback(
        &self,
        _: &str,
        _: &str,
        _: u8,
        _: TaskOutcome,
        _: Option<i32>,
    ) -> Result<(), String> {
        Err("task service not configured".into())
    }
    async fn increment_replan_count(&self, _: &str, _: &str) -> Result<(), String> {
        Err("task service not configured".into())
    }
    async fn extract_template(&self, _: &str, _: &str, _: &str) -> Result<Option<String>, String> {
        Err("task service not configured".into())
    }
    async fn recommend_templates(
        &self,
        _: &str,
        _: &str,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<TemplateRecommendation>, String> {
        Err("task service not configured".into())
    }
    async fn get_learning_stats(&self, _: &str, _: &str) -> Result<LearningStats, String> {
        Err("task service not configured".into())
    }
    async fn record_template_usage(&self, _: &str, _: &str) -> Result<(), String> {
        Err("task service not configured".into())
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeTaskStatusGuardRow {
        status: &'static str,
        fail_status: bool,
    }

    impl FakeTaskStatusGuardRow {
        fn with_status(status: &'static str) -> Self {
            Self {
                status,
                fail_status: false,
            }
        }

        fn missing_status() -> Self {
            Self {
                status: "in_progress",
                fail_status: true,
            }
        }
    }

    impl TaskStatusGuardDbRow for FakeTaskStatusGuardRow {
        fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
            if self.fail_status && column == "status" {
                return Err(sqlx::Error::ColumnNotFound(column.to_string()));
            }
            match column {
                "status" => Ok(self.status.to_string()),
                _ => Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
        }
    }

    #[test]
    fn task_status_guard_row_decode_preserves_database_status() {
        let status =
            decode_task_status_guard_row(&FakeTaskStatusGuardRow::with_status("completed"), "t-1")
                .unwrap();
        assert_eq!(status, TaskStatus::Completed);
    }

    #[test]
    fn task_status_guard_row_decode_fails_loudly_on_missing_status_column() {
        let error = decode_task_status_guard_row(&FakeTaskStatusGuardRow::missing_status(), "t-1")
            .unwrap_err();
        assert!(
            error.contains("task status guard row decode `status`") && error.contains("status"),
            "missing status should fail loudly with column context: {error}"
        );
    }

    #[test]
    fn task_status_guard_row_decode_rejects_unknown_persisted_status() {
        let error =
            decode_task_status_guard_row(&FakeTaskStatusGuardRow::with_status("mystery"), "t-1")
                .unwrap_err();
        assert!(
            error.contains("task t-1 has unknown persisted status: mystery"),
            "unknown persisted status should fail loudly: {error}"
        );
    }

    struct FakeTaskListRow {
        failed_column: Option<&'static str>,
        session_id: Option<&'static str>,
        status: &'static str,
        progress_pct: i32,
        items_done: i32,
        items_total: i32,
        completed_at: Option<&'static str>,
        outcome: Option<&'static str>,
        error_message: Option<&'static str>,
        project_type: Option<&'static str>,
        claimability: Option<&'static str>,
    }

    impl FakeTaskListRow {
        fn complete() -> Self {
            Self {
                failed_column: None,
                session_id: Some("session-1"),
                status: "in_progress",
                progress_pct: 40,
                items_done: 2,
                items_total: 5,
                completed_at: None,
                outcome: Some("partial"),
                error_message: Some("needs retry"),
                project_type: Some("rust"),
                claimability: Some("recoverable_in_progress"),
            }
        }

        fn without_optional_values() -> Self {
            Self {
                session_id: None,
                completed_at: None,
                outcome: None,
                error_message: None,
                project_type: None,
                claimability: None,
                ..Self::complete()
            }
        }

        fn fail_on(column: &'static str) -> Self {
            Self {
                failed_column: Some(column),
                ..Self::complete()
            }
        }

        fn with_status(status: &'static str) -> Self {
            Self {
                status,
                ..Self::complete()
            }
        }

        fn with_outcome(outcome: &'static str) -> Self {
            Self {
                outcome: Some(outcome),
                ..Self::complete()
            }
        }

        fn with_claimability(claimability: &'static str) -> Self {
            Self {
                claimability: Some(claimability),
                ..Self::complete()
            }
        }

        fn with_i32(column: &'static str, value: i32) -> Self {
            let mut row = Self::complete();
            match column {
                "progress_pct" => row.progress_pct = value,
                "items_done" => row.items_done = value,
                "items_total" => row.items_total = value,
                _ => unreachable!("unexpected i32 column: {column}"),
            }
            row
        }
    }

    impl TaskListDbRow for FakeTaskListRow {
        fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
            if self.failed_column == Some(column) {
                return Err(sqlx::Error::ColumnNotFound(column.to_string()));
            }

            Ok(match column {
                "task_id" => "task-1",
                "title" => "Refactor task parser",
                "status" => self.status,
                "created_at" => "2026-06-26 09:00:00.000000",
                "updated_at" => "2026-06-26 10:00:00.000000",
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
            .to_string())
        }

        fn optional_string_column(&self, column: &str) -> Result<Option<String>, sqlx::Error> {
            if self.failed_column == Some(column) {
                return Err(sqlx::Error::ColumnNotFound(column.to_string()));
            }

            Ok(match column {
                "session_id" => self.session_id,
                "completed_at" => self.completed_at,
                "outcome" => self.outcome,
                "error_message" => self.error_message,
                "project_type" => self.project_type,
                "claimability" => self.claimability,
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
            .map(str::to_string))
        }

        fn i32_column(&self, column: &str) -> Result<i32, sqlx::Error> {
            if self.failed_column == Some(column) {
                return Err(sqlx::Error::ColumnNotFound(column.to_string()));
            }

            match column {
                "progress_pct" => Ok(self.progress_pct),
                "items_done" => Ok(self.items_done),
                "items_total" => Ok(self.items_total),
                _ => Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
        }
    }

    #[test]
    fn task_list_row_decode_preserves_database_values() {
        let item = decode_task_list_item(&FakeTaskListRow::complete()).unwrap();

        assert_eq!(item.task_id, "task-1");
        assert_eq!(item.title, "Refactor task parser");
        assert_eq!(item.session_id.as_deref(), Some("session-1"));
        assert_eq!(item.status, TaskStatus::InProgress);
        assert_eq!(item.progress_pct, 40);
        assert_eq!(item.items_done, 2);
        assert_eq!(item.items_total, 5);
        assert_eq!(item.created_at, "2026-06-26 09:00:00.000000");
        assert_eq!(item.updated_at, "2026-06-26 10:00:00.000000");
        assert_eq!(item.completed_at, None);
        assert_eq!(item.outcome, Some(TaskOutcome::Partial));
        assert_eq!(item.error_message.as_deref(), Some("needs retry"));
        assert_eq!(item.project_type.as_deref(), Some("rust"));
        assert_eq!(
            item.claimability,
            Some(TaskClaimability::RecoverableInProgress)
        );
    }

    #[test]
    fn task_list_row_decode_preserves_sql_null_optional_values() {
        let item = decode_task_list_item(&FakeTaskListRow::without_optional_values()).unwrap();

        assert_eq!(item.session_id, None);
        assert_eq!(item.completed_at, None);
        assert_eq!(item.outcome, None);
        assert_eq!(item.error_message, None);
        assert_eq!(item.project_type, None);
        assert_eq!(item.claimability, None);
    }

    #[test]
    fn task_list_row_decode_fails_loudly_on_any_selected_column_error() {
        for column in [
            "task_id",
            "title",
            "session_id",
            "status",
            "progress_pct",
            "items_done",
            "items_total",
            "created_at",
            "updated_at",
            "completed_at",
            "outcome",
            "error_message",
            "project_type",
            "claimability",
        ] {
            let error = decode_task_list_item(&FakeTaskListRow::fail_on(column)).unwrap_err();
            assert!(
                error.contains("task list row decode") && error.contains(column),
                "decode error should identify selected column `{column}`: {error}"
            );
        }
    }

    #[test]
    fn task_list_row_decode_rejects_invalid_enums_and_numeric_bounds() {
        let status = decode_task_list_item(&FakeTaskListRow::with_status("mystery")).unwrap_err();
        assert!(
            status.contains("unknown persisted task status: mystery"),
            "invalid status should fail loudly: {status}"
        );

        let outcome = decode_task_list_item(&FakeTaskListRow::with_outcome("mystery")).unwrap_err();
        assert!(
            outcome.contains("task list row decode `outcome` unknown value: mystery"),
            "invalid outcome should fail loudly: {outcome}"
        );

        let claimability =
            decode_task_list_item(&FakeTaskListRow::with_claimability("mystery")).unwrap_err();
        assert!(
            claimability.contains("task list row decode `claimability` unknown value: mystery"),
            "invalid claimability should fail loudly: {claimability}"
        );

        for column in ["progress_pct", "items_done", "items_total"] {
            let error = decode_task_list_item(&FakeTaskListRow::with_i32(column, -1)).unwrap_err();
            assert!(
                error.contains(column) && error.contains("non-negative integer"),
                "negative numeric column should fail loudly for `{column}`: {error}"
            );
        }

        let too_large =
            decode_task_list_item(&FakeTaskListRow::with_i32("progress_pct", 101)).unwrap_err();
        assert!(
            too_large.contains("progress_pct") && too_large.contains("0..=100"),
            "progress_pct above 100 should fail loudly: {too_large}"
        );
    }

    #[test]
    fn task_list_select_columns_declares_claimability_column() {
        assert!(
            AGENT_TASK_LIST_SELECT_COLUMNS.contains("NULL AS claimability"),
            "base task list SELECT must project claimability explicitly so row decode can distinguish SQL NULL from a missing column"
        );
    }

    struct FakeTemplateRecommendationRow {
        failed_column: Option<&'static str>,
        template_json: &'static str,
        is_own: i32,
        use_count: i32,
        user_id: Option<&'static str>,
        project_type: Option<&'static str>,
        avg_completion_time: Option<i32>,
    }

    impl FakeTemplateRecommendationRow {
        fn complete() -> Self {
            Self {
                failed_column: None,
                template_json: r#"{"subtasks":[],"notes":"reuse known path"}"#,
                is_own: 1,
                use_count: 4,
                user_id: Some("user-1"),
                project_type: Some("rust"),
                avg_completion_time: Some(120),
            }
        }

        fn fail_on(column: &'static str) -> Self {
            Self {
                failed_column: Some(column),
                ..Self::complete()
            }
        }

        fn with_template_json(template_json: &'static str) -> Self {
            Self {
                template_json,
                ..Self::complete()
            }
        }

        fn with_is_own(is_own: i32) -> Self {
            Self {
                is_own,
                ..Self::complete()
            }
        }

        fn with_use_count(use_count: i32) -> Self {
            Self {
                use_count,
                ..Self::complete()
            }
        }

        fn community() -> Self {
            Self {
                is_own: 0,
                user_id: None,
                project_type: None,
                avg_completion_time: None,
                ..Self::complete()
            }
        }

        fn maybe_fail(&self, column: &str) -> Result<(), sqlx::Error> {
            if self.failed_column == Some(column) {
                Err(sqlx::Error::ColumnNotFound(column.to_string()))
            } else {
                Ok(())
            }
        }
    }

    impl TemplateRecommendationDbRow for FakeTemplateRecommendationRow {
        fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
            self.maybe_fail(column)?;
            let value = match column {
                "template_id" => "template-1",
                "goal_pattern" => "ship parser",
                "template_json" => self.template_json,
                "created_at" => "2026-06-26 09:00:00.000000",
                "updated_at" => "2026-06-26 10:00:00.000000",
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            };
            Ok(value.to_string())
        }

        fn optional_string_column(&self, column: &str) -> Result<Option<String>, sqlx::Error> {
            self.maybe_fail(column)?;
            Ok(match column {
                "user_id" => self.user_id,
                "project_type" => self.project_type,
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
            .map(str::to_string))
        }

        fn i32_column(&self, column: &str) -> Result<i32, sqlx::Error> {
            self.maybe_fail(column)?;
            match column {
                "is_own" => Ok(self.is_own),
                "use_count" => Ok(self.use_count),
                _ => Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
        }

        fn optional_i32_column(&self, column: &str) -> Result<Option<i32>, sqlx::Error> {
            self.maybe_fail(column)?;
            match column {
                "avg_completion_time" => Ok(self.avg_completion_time),
                _ => Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
        }

        fn f32_column(&self, column: &str) -> Result<f32, sqlx::Error> {
            self.maybe_fail(column)?;
            match column {
                "success_rate" => Ok(0.8),
                "score" => Ok(0.72),
                _ => Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
        }
    }

    #[test]
    fn template_recommendation_row_decode_preserves_database_values() {
        let recommendation =
            decode_template_recommendation(&FakeTemplateRecommendationRow::complete()).unwrap();

        assert_eq!(recommendation.template.template_id, "template-1");
        assert_eq!(recommendation.template.user_id.as_deref(), Some("user-1"));
        assert_eq!(recommendation.template.goal_pattern, "ship parser");
        assert_eq!(
            recommendation.template.project_type.as_deref(),
            Some("rust")
        );
        assert_eq!(
            recommendation.template.template.notes.as_deref(),
            Some("reuse known path")
        );
        assert_eq!(recommendation.template.success_rate, 0.8);
        assert_eq!(recommendation.template.avg_completion_time, Some(120));
        assert_eq!(recommendation.template.use_count, 4);
        assert_eq!(
            recommendation.template.created_at,
            "2026-06-26 09:00:00.000000"
        );
        assert_eq!(
            recommendation.template.updated_at,
            "2026-06-26 10:00:00.000000"
        );
        assert_eq!(recommendation.score, 0.72);
        assert_eq!(
            recommendation.reason,
            "Your successful pattern: ship parser"
        );
    }

    #[test]
    fn template_recommendation_row_decode_preserves_null_optional_values() {
        let recommendation =
            decode_template_recommendation(&FakeTemplateRecommendationRow::community()).unwrap();

        assert_eq!(recommendation.template.user_id, None);
        assert_eq!(recommendation.template.project_type, None);
        assert_eq!(recommendation.template.avg_completion_time, None);
        assert_eq!(
            recommendation.reason,
            "Community pattern (4x used): ship parser"
        );
    }

    #[test]
    fn template_recommendation_row_decode_fails_loudly_on_any_selected_column_error() {
        for column in [
            "template_id",
            "user_id",
            "goal_pattern",
            "project_type",
            "template_json",
            "success_rate",
            "avg_completion_time",
            "use_count",
            "created_at",
            "updated_at",
            "is_own",
            "score",
        ] {
            let error =
                decode_template_recommendation(&FakeTemplateRecommendationRow::fail_on(column))
                    .unwrap_err();
            assert!(
                error.contains("template recommendation row decode") && error.contains(column),
                "decode error should identify selected column `{column}`: {error}"
            );
        }
    }

    #[test]
    fn template_recommendation_row_decode_rejects_corrupt_values() {
        let bad_json = decode_template_recommendation(
            &FakeTemplateRecommendationRow::with_template_json("not-json"),
        )
        .unwrap_err();
        assert!(
            bad_json.contains("template_json"),
            "invalid template_json should fail loudly: {bad_json}"
        );

        let bad_owner =
            decode_template_recommendation(&FakeTemplateRecommendationRow::with_is_own(2))
                .unwrap_err();
        assert!(
            bad_owner.contains("is_own") && bad_owner.contains("expected 0 or 1"),
            "invalid is_own should fail loudly: {bad_owner}"
        );

        let bad_use_count =
            decode_template_recommendation(&FakeTemplateRecommendationRow::with_use_count(-1))
                .unwrap_err();
        assert!(
            bad_use_count.contains("use_count") && bad_use_count.contains("non-negative integer"),
            "negative use_count should fail loudly: {bad_use_count}"
        );
    }

    struct FakeLearningStatsRow {
        failed_column: Option<&'static str>,
        total_tasks: i64,
        completed_tasks: i64,
        avg_rating: Option<f32>,
        avg_replan_count: Option<f32>,
    }

    impl FakeLearningStatsRow {
        fn complete() -> Self {
            Self {
                failed_column: None,
                total_tasks: 5,
                completed_tasks: 3,
                avg_rating: Some(4.2),
                avg_replan_count: Some(1.5),
            }
        }

        fn empty() -> Self {
            Self {
                total_tasks: 0,
                completed_tasks: 0,
                avg_rating: None,
                avg_replan_count: None,
                ..Self::complete()
            }
        }

        fn fail_on(column: &'static str) -> Self {
            Self {
                failed_column: Some(column),
                ..Self::complete()
            }
        }

        fn with_counts(total_tasks: i64, completed_tasks: i64) -> Self {
            Self {
                total_tasks,
                completed_tasks,
                ..Self::complete()
            }
        }

        fn maybe_fail(&self, column: &str) -> Result<(), sqlx::Error> {
            if self.failed_column == Some(column) {
                Err(sqlx::Error::ColumnNotFound(column.to_string()))
            } else {
                Ok(())
            }
        }
    }

    impl LearningStatsDbRow for FakeLearningStatsRow {
        fn i64_column(&self, column: &str) -> Result<i64, sqlx::Error> {
            self.maybe_fail(column)?;
            match column {
                "total_tasks" => Ok(self.total_tasks),
                "completed_tasks" => Ok(self.completed_tasks),
                _ => Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
        }

        fn optional_f32_column(&self, column: &str) -> Result<Option<f32>, sqlx::Error> {
            self.maybe_fail(column)?;
            match column {
                "avg_rating" => Ok(self.avg_rating),
                "avg_replan_count" => Ok(self.avg_replan_count),
                _ => Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
        }
    }

    #[test]
    fn learning_stats_row_decode_preserves_database_values() {
        let stats = decode_learning_stats(&FakeLearningStatsRow::complete()).unwrap();

        assert_eq!(stats.total_tasks, 5);
        assert_eq!(stats.completed_tasks, 3);
        assert_eq!(stats.avg_rating, Some(4.2));
        assert_eq!(stats.avg_replan_count, 1.5);
        assert!((stats.inferred_success_rate - 0.51).abs() < 0.001);
    }

    #[test]
    fn learning_stats_row_decode_preserves_null_avg_values() {
        let stats = decode_learning_stats(&FakeLearningStatsRow::empty()).unwrap();

        assert_eq!(stats.total_tasks, 0);
        assert_eq!(stats.completed_tasks, 0);
        assert_eq!(stats.avg_rating, None);
        assert_eq!(stats.avg_replan_count, 0.0);
        assert_eq!(stats.inferred_success_rate, 0.0);
    }

    #[test]
    fn learning_stats_row_decode_fails_loudly_on_any_selected_column_error() {
        for column in [
            "total_tasks",
            "completed_tasks",
            "avg_rating",
            "avg_replan_count",
        ] {
            let error = decode_learning_stats(&FakeLearningStatsRow::fail_on(column)).unwrap_err();
            assert!(
                error.contains("learning stats row decode") && error.contains(column),
                "decode error should identify selected column `{column}`: {error}"
            );
        }
    }

    #[test]
    fn learning_stats_row_decode_rejects_corrupt_counts() {
        for (total_tasks, completed_tasks, expected) in [
            (-1, 0, "total_tasks"),
            (1, -1, "completed_tasks"),
            (1, 2, "expected <= total_tasks"),
        ] {
            let error = decode_learning_stats(&FakeLearningStatsRow::with_counts(
                total_tasks,
                completed_tasks,
            ))
            .unwrap_err();
            assert!(
                error.contains(expected),
                "corrupt counts should fail with `{expected}` context: {error}"
            );
        }
    }

    struct FakeTaskRecordRow {
        failed_column: Option<&'static str>,
        session_id: Option<&'static str>,
        parent_task_id: Option<&'static str>,
        description: Option<&'static str>,
        status: &'static str,
        progress_pct: i32,
        items_done: i32,
        items_total: i32,
        plan_json: Option<&'static str>,
        checkpoint_json: Option<&'static str>,
        error_message: Option<&'static str>,
        completed_at: Option<&'static str>,
        user_rating: Option<i8>,
        completion_time_sec: Option<i32>,
        replan_count: i32,
        auto_adjustments: i32,
        outcome: Option<&'static str>,
        project_type: Option<&'static str>,
        goal_pattern: Option<&'static str>,
        agent_id: Option<&'static str>,
    }

    impl FakeTaskRecordRow {
        fn complete() -> Self {
            Self {
                failed_column: None,
                session_id: Some("session-1"),
                parent_task_id: Some("parent-1"),
                description: Some("full task row"),
                status: "in_progress",
                progress_pct: 40,
                items_done: 2,
                items_total: 5,
                plan_json: Some(r#"{"subtasks":[],"notes":"plan"}"#),
                checkpoint_json: Some(
                    r#"{"active_subtask_id":null,"turn":3,"session_id":"session-1","state":{}}"#,
                ),
                error_message: Some("needs retry"),
                completed_at: None,
                user_rating: Some(4),
                completion_time_sec: Some(120),
                replan_count: 1,
                auto_adjustments: 2,
                outcome: Some("partial"),
                project_type: Some("rust"),
                goal_pattern: Some("refactor *"),
                agent_id: Some("agent-1"),
            }
        }

        fn without_optional_values() -> Self {
            Self {
                session_id: None,
                parent_task_id: None,
                description: None,
                plan_json: None,
                checkpoint_json: None,
                error_message: None,
                completed_at: None,
                user_rating: None,
                completion_time_sec: None,
                outcome: None,
                project_type: None,
                goal_pattern: None,
                agent_id: None,
                ..Self::complete()
            }
        }

        fn fail_on(column: &'static str) -> Self {
            Self {
                failed_column: Some(column),
                ..Self::complete()
            }
        }

        fn with_status(status: &'static str) -> Self {
            Self {
                status,
                ..Self::complete()
            }
        }

        fn with_outcome(outcome: &'static str) -> Self {
            Self {
                outcome: Some(outcome),
                ..Self::complete()
            }
        }

        fn with_optional_string(column: &'static str, value: &'static str) -> Self {
            let mut row = Self::complete();
            match column {
                "plan_json" => row.plan_json = Some(value),
                "checkpoint_json" => row.checkpoint_json = Some(value),
                _ => unreachable!("unexpected optional string column: {column}"),
            }
            row
        }

        fn with_i32(column: &'static str, value: i32) -> Self {
            let mut row = Self::complete();
            match column {
                "progress_pct" => row.progress_pct = value,
                "items_done" => row.items_done = value,
                "items_total" => row.items_total = value,
                "replan_count" => row.replan_count = value,
                "auto_adjustments" => row.auto_adjustments = value,
                _ => unreachable!("unexpected i32 column: {column}"),
            }
            row
        }

        fn with_optional_i32(column: &'static str, value: i32) -> Self {
            let mut row = Self::complete();
            match column {
                "completion_time_sec" => row.completion_time_sec = Some(value),
                _ => unreachable!("unexpected optional i32 column: {column}"),
            }
            row
        }

        fn with_user_rating(value: i8) -> Self {
            Self {
                user_rating: Some(value),
                ..Self::complete()
            }
        }
    }

    impl TaskRecordDbRow for FakeTaskRecordRow {
        fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
            if self.failed_column == Some(column) {
                return Err(sqlx::Error::ColumnNotFound(column.to_string()));
            }

            Ok(match column {
                "task_id" => "task-1",
                "user_id" => "user-1",
                "title" => "Refactor task record parser",
                "status" => self.status,
                "created_at" => "2026-06-26 09:00:00.000000",
                "updated_at" => "2026-06-26 10:00:00.000000",
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
            .to_string())
        }

        fn optional_string_column(&self, column: &str) -> Result<Option<String>, sqlx::Error> {
            if self.failed_column == Some(column) {
                return Err(sqlx::Error::ColumnNotFound(column.to_string()));
            }

            Ok(match column {
                "session_id" => self.session_id,
                "parent_task_id" => self.parent_task_id,
                "description" => self.description,
                "plan_json" => self.plan_json,
                "checkpoint_json" => self.checkpoint_json,
                "error_message" => self.error_message,
                "completed_at" => self.completed_at,
                "outcome" => self.outcome,
                "project_type" => self.project_type,
                "goal_pattern" => self.goal_pattern,
                "agent_id" => self.agent_id,
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
            .map(str::to_string))
        }

        fn i32_column(&self, column: &str) -> Result<i32, sqlx::Error> {
            if self.failed_column == Some(column) {
                return Err(sqlx::Error::ColumnNotFound(column.to_string()));
            }

            match column {
                "progress_pct" => Ok(self.progress_pct),
                "items_done" => Ok(self.items_done),
                "items_total" => Ok(self.items_total),
                "replan_count" => Ok(self.replan_count),
                "auto_adjustments" => Ok(self.auto_adjustments),
                _ => Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
        }

        fn optional_i32_column(&self, column: &str) -> Result<Option<i32>, sqlx::Error> {
            if self.failed_column == Some(column) {
                return Err(sqlx::Error::ColumnNotFound(column.to_string()));
            }

            match column {
                "completion_time_sec" => Ok(self.completion_time_sec),
                _ => Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
        }

        fn optional_i8_column(&self, column: &str) -> Result<Option<i8>, sqlx::Error> {
            if self.failed_column == Some(column) {
                return Err(sqlx::Error::ColumnNotFound(column.to_string()));
            }

            match column {
                "user_rating" => Ok(self.user_rating),
                _ => Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
        }
    }

    #[test]
    fn task_record_row_decode_preserves_database_values() {
        let record = decode_task_record(&FakeTaskRecordRow::complete()).unwrap();

        assert_eq!(record.task_id, "task-1");
        assert_eq!(record.user_id, "user-1");
        assert_eq!(record.session_id.as_deref(), Some("session-1"));
        assert_eq!(record.parent_task_id.as_deref(), Some("parent-1"));
        assert_eq!(record.title, "Refactor task record parser");
        assert_eq!(record.description.as_deref(), Some("full task row"));
        assert_eq!(record.status, TaskStatus::InProgress);
        assert_eq!(record.progress_pct, 40);
        assert_eq!(record.items_done, 2);
        assert_eq!(record.items_total, 5);
        assert_eq!(
            record.plan.as_ref().and_then(|plan| plan.notes.as_deref()),
            Some("plan")
        );
        assert_eq!(
            record.checkpoint.as_ref().map(|checkpoint| checkpoint.turn),
            Some(3)
        );
        assert_eq!(record.error_message.as_deref(), Some("needs retry"));
        assert_eq!(record.created_at, "2026-06-26 09:00:00.000000");
        assert_eq!(record.updated_at, "2026-06-26 10:00:00.000000");
        assert_eq!(record.completed_at, None);
        assert_eq!(record.user_rating, Some(4));
        assert_eq!(record.completion_time_sec, Some(120));
        assert_eq!(record.replan_count, 1);
        assert_eq!(record.auto_adjustments, 2);
        assert_eq!(record.outcome, Some(TaskOutcome::Partial));
        assert_eq!(record.project_type.as_deref(), Some("rust"));
        assert_eq!(record.goal_pattern.as_deref(), Some("refactor *"));
        assert_eq!(record.agent_id.as_deref(), Some("agent-1"));
    }

    #[test]
    fn task_record_row_decode_preserves_sql_null_optional_values() {
        let record = decode_task_record(&FakeTaskRecordRow::without_optional_values()).unwrap();

        assert_eq!(record.session_id, None);
        assert_eq!(record.parent_task_id, None);
        assert_eq!(record.description, None);
        assert!(record.plan.is_none());
        assert!(record.checkpoint.is_none());
        assert_eq!(record.error_message, None);
        assert_eq!(record.completed_at, None);
        assert_eq!(record.user_rating, None);
        assert_eq!(record.completion_time_sec, None);
        assert_eq!(record.outcome, None);
        assert_eq!(record.project_type, None);
        assert_eq!(record.goal_pattern, None);
        assert_eq!(record.agent_id, None);
    }

    #[test]
    fn task_record_row_decode_fails_loudly_on_any_selected_column_error() {
        for column in [
            "task_id",
            "user_id",
            "session_id",
            "parent_task_id",
            "title",
            "description",
            "status",
            "progress_pct",
            "items_done",
            "items_total",
            "plan_json",
            "checkpoint_json",
            "error_message",
            "created_at",
            "updated_at",
            "completed_at",
            "user_rating",
            "completion_time_sec",
            "replan_count",
            "auto_adjustments",
            "outcome",
            "project_type",
            "goal_pattern",
            "agent_id",
        ] {
            let error = decode_task_record(&FakeTaskRecordRow::fail_on(column)).unwrap_err();
            assert!(
                error.contains("task record row decode") && error.contains(column),
                "decode error should identify selected column `{column}`: {error}"
            );
        }
    }

    #[test]
    fn task_record_row_decode_rejects_invalid_json_enums_and_numeric_bounds() {
        let status = decode_task_record(&FakeTaskRecordRow::with_status("mystery")).unwrap_err();
        assert!(
            status.contains("unknown persisted task status: mystery"),
            "invalid status should fail loudly: {status}"
        );

        let outcome = decode_task_record(&FakeTaskRecordRow::with_outcome("mystery")).unwrap_err();
        assert!(
            outcome.contains("task record row decode `outcome` unknown value: mystery"),
            "invalid outcome should fail loudly: {outcome}"
        );

        for column in ["plan_json", "checkpoint_json"] {
            let error =
                decode_task_record(&FakeTaskRecordRow::with_optional_string(column, "not-json"))
                    .unwrap_err();
            assert!(
                error.contains(column) && error.contains("invalid JSON"),
                "invalid JSON should fail loudly for `{column}`: {error}"
            );
        }

        for column in [
            "progress_pct",
            "items_done",
            "items_total",
            "replan_count",
            "auto_adjustments",
        ] {
            let error = decode_task_record(&FakeTaskRecordRow::with_i32(column, -1)).unwrap_err();
            assert!(
                error.contains(column) && error.contains("non-negative integer"),
                "negative numeric column should fail loudly for `{column}`: {error}"
            );
        }

        let too_large =
            decode_task_record(&FakeTaskRecordRow::with_i32("progress_pct", 101)).unwrap_err();
        assert!(
            too_large.contains("progress_pct") && too_large.contains("0..=100"),
            "progress_pct above 100 should fail loudly: {too_large}"
        );

        let negative_completion = decode_task_record(&FakeTaskRecordRow::with_optional_i32(
            "completion_time_sec",
            -1,
        ))
        .unwrap_err();
        assert!(
            negative_completion.contains("completion_time_sec")
                && negative_completion.contains("non-negative integer"),
            "negative completion_time_sec should fail loudly: {negative_completion}"
        );

        let negative_rating =
            decode_task_record(&FakeTaskRecordRow::with_user_rating(-1)).unwrap_err();
        assert!(
            negative_rating.contains("user_rating")
                && negative_rating.contains("non-negative integer"),
            "negative user_rating should fail loudly: {negative_rating}"
        );
    }

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
            assert_eq!(TaskStatus::parse_status(status.as_str()), Some(status));
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
    fn task_status_claimable_accepts_only_unfinished_worker_states() {
        assert!(TaskStatus::Pending.is_claimable());
        assert!(TaskStatus::InProgress.is_claimable());
        assert!(!TaskStatus::Paused.is_claimable());
        assert!(!TaskStatus::Completed.is_claimable());
        assert!(!TaskStatus::Failed.is_claimable());
        assert!(!TaskStatus::Cancelled.is_claimable());
    }

    #[test]
    fn task_claimability_for_status_surfaces_pending_and_recoverable_in_progress() {
        assert_eq!(
            TaskClaimability::for_status(TaskStatus::Pending),
            Some(TaskClaimability::Pending)
        );
        assert_eq!(
            TaskClaimability::for_status(TaskStatus::InProgress),
            Some(TaskClaimability::RecoverableInProgress)
        );
        assert_eq!(TaskClaimability::for_status(TaskStatus::Paused), None);
        assert_eq!(TaskClaimability::for_status(TaskStatus::Completed), None);
    }

    #[test]
    fn task_status_unknown_rejects_instead_of_defaulting_to_pending() {
        assert_eq!(TaskStatus::parse_status("unknown"), None);
    }

    // ── TaskPlan ──

    #[test]
    fn empty_plan_progress_is_zero_not_complete() {
        // B5: An empty plan reports 0% — it has not been generated yet.
        // Previously returned 100%, which caused REPL to misreport
        // failed plan generation as "all subtasks completed".
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
            // Learning fields
            user_rating: Some(4),
            completion_time_sec: Some(1200),
            replan_count: 1,
            auto_adjustments: 0,
            outcome: Some(TaskOutcome::Success),
            project_type: Some("Rust".into()),
            goal_pattern: Some("refactor *".into()),
            agent_id: Some("edge-a".into()),
        };
        let json = serde_json::to_string(&record).unwrap();
        let loaded: TaskRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.task_id, "t-1");
        assert_eq!(loaded.status, TaskStatus::InProgress);
        assert_eq!(loaded.progress_pct, 50);
        assert_eq!(loaded.user_rating, Some(4));
        assert_eq!(loaded.outcome, Some(TaskOutcome::Success));
        assert_eq!(loaded.agent_id.as_deref(), Some("edge-a"));
    }

    #[test]
    fn task_list_item_json_roundtrip() {
        let item = TaskListItem {
            task_id: "t-1".into(),
            title: "Refactor auth".into(),
            session_id: Some("sess-1".into()),
            status: TaskStatus::InProgress,
            progress_pct: 50,
            items_done: 2,
            items_total: 4,
            created_at: "2025-01-01T00:00:00Z".into(),
            updated_at: "2025-01-01T01:00:00Z".into(),
            completed_at: None,
            outcome: Some(TaskOutcome::Partial),
            error_message: Some("disk full".into()),
            project_type: Some("Rust".into()),
            claimability: Some(TaskClaimability::RecoverableInProgress),
        };
        let json = serde_json::to_string(&item).unwrap();
        let loaded: TaskListItem = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.task_id, "t-1");
        assert_eq!(loaded.title, "Refactor auth");
        assert_eq!(loaded.session_id.as_deref(), Some("sess-1"));
        assert_eq!(loaded.status, TaskStatus::InProgress);
        assert_eq!(loaded.items_total, 4);
        assert_eq!(loaded.outcome, Some(TaskOutcome::Partial));
        assert_eq!(loaded.error_message.as_deref(), Some("disk full"));
        assert_eq!(
            loaded.claimability,
            Some(TaskClaimability::RecoverableInProgress)
        );
    }

    #[test]
    fn task_list_limit_is_bounded() {
        assert_eq!(MAX_TASK_LIST_ROWS, 200);
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
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let loaded = svc.get_task("user1", &task_id).await.unwrap().unwrap();
        assert_eq!(loaded.title, "Test Task");
        assert_eq!(loaded.status, TaskStatus::Pending);
        assert_eq!(loaded.outcome, None);
    }

    #[tokio::test]
    async fn local_task_owner_mismatch_is_not_readable_or_mutable() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = LocalTaskService::new(tmp.path().to_path_buf());
        let task_id = svc
            .create_task(
                "owner-a",
                "sess1",
                TaskCreateRequest {
                    title: "owned task".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert!(
            svc.get_task("owner-b", &task_id).await.unwrap().is_none(),
            "foreign owner must see the same surface as a missing task"
        );
        assert!(
            svc.update_status("owner-b", &task_id, TaskStatus::InProgress)
                .await
                .is_err()
        );
        assert!(
            svc.fail_task("owner-b", &task_id, "foreign mutation")
                .await
                .is_err()
        );

        let task = svc.get_task("owner-a", &task_id).await.unwrap().unwrap();
        assert_eq!(task.status, TaskStatus::Pending);
        assert_eq!(task.error_message, None);
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
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        // Pending → InProgress
        svc.update_status("user1", &tid, TaskStatus::InProgress)
            .await
            .unwrap();
        let t = svc.get_task("user1", &tid).await.unwrap().unwrap();
        assert_eq!(t.status, TaskStatus::InProgress);

        // Update progress
        svc.update_progress("user1", &tid, 50, 5, 10).await.unwrap();
        let t = svc.get_task("user1", &tid).await.unwrap().unwrap();
        assert_eq!(t.progress_pct, 50);
        assert_eq!(t.items_done, 5);

        // Save checkpoint
        let ckpt = TaskCheckpoint {
            active_subtask_id: Some("sub-1".into()),
            turn: 10,
            session_id: Some("sess1".into()),
            state: serde_json::Map::new(),
        };
        svc.save_checkpoint("user1", &tid, &ckpt).await.unwrap();
        let t = svc.get_task("user1", &tid).await.unwrap().unwrap();
        assert!(t.checkpoint.is_some());
        assert_eq!(t.checkpoint.unwrap().turn, 10);

        // Complete
        svc.complete_task("user1", &tid).await.unwrap();
        let t = svc.get_task("user1", &tid).await.unwrap().unwrap();
        assert_eq!(t.status, TaskStatus::Completed);
        assert_eq!(t.outcome, Some(TaskOutcome::Success));
        assert_eq!(t.error_message, None);
        assert!(t.completed_at.is_some());
    }

    #[tokio::test]
    async fn local_task_complete_plan_run_sets_progress_and_outcome() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = LocalTaskService::new(tmp.path().to_path_buf());
        let tid = svc
            .create_task(
                "user1",
                "s1",
                TaskCreateRequest {
                    title: "goal".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        svc.complete_plan_run("user1", &tid, 66, 2, 3, TaskOutcome::Partial)
            .await
            .unwrap();
        let t = svc.get_task("user1", &tid).await.unwrap().unwrap();
        assert_eq!(t.status, TaskStatus::Completed);
        assert_eq!(t.progress_pct, 66);
        assert_eq!(t.items_done, 2);
        assert_eq!(t.items_total, 3);
        assert_eq!(t.outcome, Some(TaskOutcome::Partial));
        assert!(t.completed_at.is_some());
    }

    #[tokio::test]
    async fn local_task_complete_task_with_partial_outcome_preserves_non_plan_shape() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = LocalTaskService::new(tmp.path().to_path_buf());
        let tid = svc
            .create_task(
                "user1",
                "s1",
                TaskCreateRequest {
                    title: "goal".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        svc.complete_task_with_outcome("user1", &tid, TaskOutcome::Partial)
            .await
            .unwrap();
        let t = svc.get_task("user1", &tid).await.unwrap().unwrap();
        assert_eq!(t.status, TaskStatus::Completed);
        assert_eq!(t.progress_pct, 100);
        assert_eq!(t.items_done, 0);
        assert_eq!(t.items_total, 0);
        assert_eq!(t.outcome, Some(TaskOutcome::Partial));
        assert!(t.completed_at.is_some());
    }

    // ── Terminal-state guards ──
    //
    // Once a task reaches Completed/Failed/Cancelled, all mutations to its
    // status, outcome, error_message, or progress must be rejected. The
    // historical behavior was: the SQL UPDATE would fire unconditionally,
    // letting a late `fail_task` overwrite a successful completion (and erase
    // the success record), or letting `update_progress` bump progress on a
    // task that already finished (post-mortem mutation).

    #[tokio::test]
    async fn local_task_completed_cannot_be_failed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = LocalTaskService::new(tmp.path().to_path_buf());
        let tid = svc
            .create_task(
                "user1",
                "s",
                TaskCreateRequest {
                    title: "t".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        svc.complete_task("user1", &tid).await.unwrap();
        let before = svc.get_task("user1", &tid).await.unwrap().unwrap();

        let err = svc.fail_task("user1", &tid, "should be rejected").await;
        assert!(err.is_err(), "fail_task on completed must error");
        let after = svc.get_task("user1", &tid).await.unwrap().unwrap();
        assert_eq!(after.status, TaskStatus::Completed);
        assert_eq!(after.outcome, Some(TaskOutcome::Success));
        assert_eq!(after.error_message, None);
        assert_eq!(after.completed_at, before.completed_at);
    }

    #[tokio::test]
    async fn local_task_failed_cannot_be_completed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = LocalTaskService::new(tmp.path().to_path_buf());
        let tid = svc
            .create_task(
                "user1",
                "s",
                TaskCreateRequest {
                    title: "t".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        svc.fail_task("user1", &tid, "boom").await.unwrap();

        let err = svc.complete_task("user1", &tid).await;
        assert!(err.is_err(), "complete_task on failed must error");
        let err = svc
            .complete_task_with_outcome("user1", &tid, TaskOutcome::Success)
            .await;
        assert!(
            err.is_err(),
            "complete_task_with_outcome on failed must error"
        );
        let err = svc
            .complete_plan_run("user1", &tid, 100, 1, 1, TaskOutcome::Success)
            .await;
        assert!(err.is_err(), "complete_plan_run on failed must error");

        let after = svc.get_task("user1", &tid).await.unwrap().unwrap();
        assert_eq!(after.status, TaskStatus::Failed);
        assert_eq!(after.outcome, Some(TaskOutcome::Failed));
        assert_eq!(after.error_message.as_deref(), Some("boom"));
    }

    #[tokio::test]
    async fn local_task_progress_rejected_on_terminal() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = LocalTaskService::new(tmp.path().to_path_buf());
        let tid = svc
            .create_task(
                "user1",
                "s",
                TaskCreateRequest {
                    title: "t".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        svc.complete_task("user1", &tid).await.unwrap();
        let before = svc.get_task("user1", &tid).await.unwrap().unwrap();

        let err = svc.update_progress("user1", &tid, 50, 5, 10).await;
        assert!(err.is_err(), "update_progress on completed must error");
        let after = svc.get_task("user1", &tid).await.unwrap().unwrap();
        assert_eq!(after.progress_pct, before.progress_pct);
        assert_eq!(after.updated_at, before.updated_at);
    }

    #[tokio::test]
    async fn local_task_cancel_terminal_then_no_overwrite() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = LocalTaskService::new(tmp.path().to_path_buf());
        let tid = svc
            .create_task(
                "user1",
                "s",
                TaskCreateRequest {
                    title: "t".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        svc.update_status("user1", &tid, TaskStatus::Cancelled)
            .await
            .unwrap();

        assert!(svc.fail_task("user1", &tid, "late").await.is_err());
        assert!(svc.complete_task("user1", &tid).await.is_err());
        assert!(svc.update_progress("user1", &tid, 1, 1, 1).await.is_err());
        assert!(
            svc.update_status("user1", &tid, TaskStatus::InProgress)
                .await
                .is_err()
        );
        assert!(
            svc.save_checkpoint(
                "user1",
                &tid,
                &TaskCheckpoint {
                    active_subtask_id: Some("late".into()),
                    turn: 99,
                    session_id: Some("s".into()),
                    state: serde_json::Map::new(),
                },
            )
            .await
            .is_err()
        );
        assert!(
            svc.update_plan(
                "user1",
                &tid,
                &TaskPlan {
                    subtasks: vec![SubtaskPlan {
                        id: "late".into(),
                        title: "late mutation".into(),
                        ..Default::default()
                    }],
                    notes: Some("late".into()),
                },
            )
            .await
            .is_err()
        );

        let after = svc.get_task("user1", &tid).await.unwrap().unwrap();
        assert_eq!(after.status, TaskStatus::Cancelled);
        assert!(after.checkpoint.is_none());
        assert!(after.plan.is_none());
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
                ..Default::default()
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
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        svc.complete_task("user1", &tid2).await.unwrap();

        let all = svc.list_recent_tasks("user1", None).await.unwrap();
        assert_eq!(all.len(), 2);

        let pending = svc
            .list_recent_tasks("user1", Some(TaskStatus::Pending))
            .await
            .unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].title, "Active");

        let completed = svc
            .list_recent_tasks("user1", Some(TaskStatus::Completed))
            .await
            .unwrap();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].title, "Done");
    }

    #[tokio::test]
    async fn local_task_list_preserves_partial_outcome() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = LocalTaskService::new(tmp.path().to_path_buf());

        let tid = svc
            .create_task(
                "user1",
                "s1",
                TaskCreateRequest {
                    title: "Plan".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        svc.complete_plan_run("user1", &tid, 100, 3, 3, TaskOutcome::Partial)
            .await
            .unwrap();

        let completed = svc
            .list_recent_tasks("user1", Some(TaskStatus::Completed))
            .await
            .unwrap();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].task_id, tid);
        assert_eq!(completed[0].outcome, Some(TaskOutcome::Partial));
    }

    #[tokio::test]
    async fn local_task_list_for_session_filters_foreign_sessions() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = LocalTaskService::new(tmp.path().to_path_buf());

        let local = svc
            .create_task(
                "user1",
                "sess-local",
                TaskCreateRequest {
                    title: "Local".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let foreign = svc
            .create_task(
                "user1",
                "sess-foreign",
                TaskCreateRequest {
                    title: "Foreign".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        svc.complete_task("user1", &local).await.unwrap();
        svc.complete_task("user1", &foreign).await.unwrap();

        let local_tasks = svc
            .list_recent_tasks_for_session("user1", "sess-local", Some(TaskStatus::Completed))
            .await
            .unwrap();
        assert_eq!(local_tasks.len(), 1);
        assert_eq!(local_tasks[0].task_id, local);
        assert_eq!(local_tasks[0].session_id.as_deref(), Some("sess-local"));
    }

    #[tokio::test]
    async fn local_task_search_prefers_exact_title_over_substring_matches() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = LocalTaskService::new(tmp.path().to_path_buf());

        let exact = svc
            .create_task(
                "user1",
                "s1",
                TaskCreateRequest {
                    title: "Build auth".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        svc.create_task(
            "user1",
            "s1",
            TaskCreateRequest {
                title: "Build auth module".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let matches = svc.search_tasks("user1", "Build auth", 8).await.unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].task_id, exact);
    }

    #[tokio::test]
    async fn local_task_search_returns_all_best_tier_prefix_matches() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = LocalTaskService::new(tmp.path().to_path_buf());

        let prefix = "task-prefix-";
        let first = format!("{prefix}001");
        let second = format!("{prefix}002");
        let now = chrono::Utc::now().to_rfc3339();
        svc.save_task(&TaskRecord {
            task_id: first.clone(),
            user_id: "user1".into(),
            session_id: Some("s1".into()),
            parent_task_id: None,
            title: "First".into(),
            description: None,
            status: TaskStatus::Pending,
            progress_pct: 0,
            items_done: 0,
            items_total: 0,
            plan: None,
            checkpoint: None,
            error_message: None,
            created_at: now.clone(),
            updated_at: now.clone(),
            completed_at: None,
            user_rating: None,
            completion_time_sec: None,
            replan_count: 0,
            auto_adjustments: 0,
            outcome: None,
            project_type: None,
            goal_pattern: None,
            agent_id: None,
        })
        .unwrap();
        svc.save_task(&TaskRecord {
            task_id: second.clone(),
            user_id: "user1".into(),
            session_id: Some("s1".into()),
            parent_task_id: None,
            title: "Second".into(),
            description: None,
            status: TaskStatus::Pending,
            progress_pct: 0,
            items_done: 0,
            items_total: 0,
            plan: None,
            checkpoint: None,
            error_message: None,
            created_at: now.clone(),
            updated_at: now,
            completed_at: None,
            user_rating: None,
            completion_time_sec: None,
            replan_count: 0,
            auto_adjustments: 0,
            outcome: None,
            project_type: None,
            goal_pattern: None,
            agent_id: None,
        })
        .unwrap();

        let matches = svc.search_tasks("user1", prefix, 8).await.unwrap();
        let ids: Vec<&str> = matches.iter().map(|task| task.task_id.as_str()).collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&first.as_str()));
        assert!(ids.contains(&second.as_str()));
    }

    #[tokio::test]
    async fn local_claimable_tasks_for_worker_returns_oldest_tasks_first() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = LocalTaskService::new(tmp.path().to_path_buf());
        for (task_id, created_at) in [
            ("task-oldest", "2025-01-01T00:00:00Z"),
            ("task-middle", "2025-01-02T00:00:00Z"),
            ("task-newest", "2025-01-03T00:00:00Z"),
        ] {
            svc.save_task(&TaskRecord {
                task_id: task_id.into(),
                user_id: "user1".into(),
                session_id: Some("s1".into()),
                parent_task_id: None,
                title: task_id.into(),
                description: None,
                status: TaskStatus::Pending,
                progress_pct: 0,
                items_done: 0,
                items_total: 0,
                plan: None,
                checkpoint: None,
                error_message: None,
                created_at: created_at.into(),
                updated_at: created_at.into(),
                completed_at: None,
                user_rating: None,
                completion_time_sec: None,
                replan_count: 0,
                auto_adjustments: 0,
                outcome: None,
                project_type: None,
                goal_pattern: None,
                agent_id: None,
            })
            .unwrap();
        }

        let tasks = svc
            .list_claimable_tasks_for_worker("user1", 2)
            .await
            .unwrap();
        let ids: Vec<&str> = tasks.iter().map(|task| task.task_id.as_str()).collect();
        assert_eq!(ids, vec!["task-oldest", "task-middle"]);
        assert_eq!(
            tasks
                .iter()
                .map(|task| task.claimability)
                .collect::<Vec<_>>(),
            vec![
                Some(TaskClaimability::Pending),
                Some(TaskClaimability::Pending)
            ]
        );
    }

    #[tokio::test]
    async fn local_claimable_tasks_for_worker_includes_in_progress_ordered_by_created_at() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = LocalTaskService::new(tmp.path().to_path_buf());
        for (task_id, status, created_at) in [
            (
                "task-oldest-in-progress",
                TaskStatus::InProgress,
                "2025-01-01T00:00:00Z",
            ),
            (
                "task-middle-pending",
                TaskStatus::Pending,
                "2025-01-02T00:00:00Z",
            ),
            ("task-failed", TaskStatus::Failed, "2025-01-03T00:00:00Z"),
        ] {
            svc.save_task(&TaskRecord {
                task_id: task_id.into(),
                user_id: "user1".into(),
                session_id: Some("s1".into()),
                parent_task_id: None,
                title: task_id.into(),
                description: None,
                status,
                progress_pct: 0,
                items_done: 0,
                items_total: 0,
                plan: None,
                checkpoint: None,
                error_message: None,
                created_at: created_at.into(),
                updated_at: created_at.into(),
                completed_at: None,
                user_rating: None,
                completion_time_sec: None,
                replan_count: 0,
                auto_adjustments: 0,
                outcome: None,
                project_type: None,
                goal_pattern: None,
                agent_id: None,
            })
            .unwrap();
        }

        let tasks = svc
            .list_claimable_tasks_for_worker("user1", 10)
            .await
            .unwrap();
        let ids: Vec<&str> = tasks.iter().map(|task| task.task_id.as_str()).collect();
        assert_eq!(ids, vec!["task-oldest-in-progress", "task-middle-pending"]);
        assert_eq!(
            tasks
                .iter()
                .map(|task| task.claimability)
                .collect::<Vec<_>>(),
            vec![
                Some(TaskClaimability::RecoverableInProgress),
                Some(TaskClaimability::Pending)
            ]
        );
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
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        svc.fail_task("user1", &tid, "network timeout")
            .await
            .unwrap();
        let t = svc.get_task("user1", &tid).await.unwrap().unwrap();
        assert_eq!(t.status, TaskStatus::Failed);
        assert_eq!(t.outcome, Some(TaskOutcome::Failed));
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
                    plan: Some(plan.clone()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        // Mark first subtask complete
        let mut updated_plan = plan;
        updated_plan.subtasks[0].status = TaskStatus::Completed;
        svc.update_plan("user1", &tid, &updated_plan).await.unwrap();

        let t = svc.get_task("user1", &tid).await.unwrap().unwrap();
        assert_eq!(t.progress_pct, 50);
        assert_eq!(t.items_done, 1);
        assert_eq!(t.items_total, 2);
    }

    #[tokio::test]
    async fn local_task_nonexistent_returns_none() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = LocalTaskService::new(tmp.path().to_path_buf());
        let result = svc.get_task("user1", "nonexistent-id").await.unwrap();
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
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        svc.update_status("user1", &tid, TaskStatus::InProgress)
            .await
            .unwrap();
        svc.save_checkpoint(
            "user1",
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
        svc.update_status("user1", &tid, TaskStatus::Paused)
            .await
            .unwrap();

        // Session 2: new session loads the task and resumes
        let task = svc.get_task("user1", &tid).await.unwrap().unwrap();
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
        svc.update_status("user1", &tid, TaskStatus::InProgress)
            .await
            .unwrap();
        let resumed = svc.get_task("user1", &tid).await.unwrap().unwrap();
        assert_eq!(resumed.status, TaskStatus::InProgress);
    }

    // ── Learning functionality tests ──

    #[tokio::test]
    async fn learning_stats_empty_for_no_matches() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = LocalTaskService::new(tmp.path().to_path_buf());

        // No tasks exist
        let stats = svc
            .get_learning_stats("user1", "refactor auth")
            .await
            .unwrap();
        assert_eq!(stats.total_tasks, 0);
        assert_eq!(stats.inferred_success_rate, 0.0);
    }

    #[tokio::test]
    async fn learning_stats_computes_from_tasks() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = LocalTaskService::new(tmp.path().to_path_buf());

        // Create some matching tasks
        let tid1 = svc
            .create_task(
                "user1",
                "s1",
                TaskCreateRequest {
                    title: "refactor auth module".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        svc.complete_task("user1", &tid1).await.unwrap();
        svc.record_feedback("user1", &tid1, 5, TaskOutcome::Success, Some(600))
            .await
            .unwrap();

        let tid2 = svc
            .create_task(
                "user1",
                "s2",
                TaskCreateRequest {
                    title: "refactor auth handlers".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        svc.complete_task("user1", &tid2).await.unwrap();

        let stats = svc
            .get_learning_stats("user1", "refactor auth")
            .await
            .unwrap();
        assert_eq!(stats.total_tasks, 2);
        assert_eq!(stats.completed_tasks, 2);
        assert!(stats.inferred_success_rate > 0.9);
    }

    #[tokio::test]
    async fn learning_stats_excludes_partial_outcomes_from_completed_count() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = LocalTaskService::new(tmp.path().to_path_buf());

        let tid = svc
            .create_task(
                "user1",
                "s1",
                TaskCreateRequest {
                    title: "refactor auth module".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        svc.complete_plan_run("user1", &tid, 100, 3, 3, TaskOutcome::Partial)
            .await
            .unwrap();

        let stats = svc
            .get_learning_stats("user1", "refactor auth")
            .await
            .unwrap();
        assert_eq!(stats.total_tasks, 1);
        assert_eq!(stats.completed_tasks, 0);
        assert_eq!(stats.inferred_success_rate, 0.0);
    }

    #[tokio::test]
    async fn template_extraction_requires_eligibility() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = LocalTaskService::new(tmp.path().to_path_buf());

        // Create task without high rating
        let tid = svc
            .create_task(
                "user1",
                "s1",
                TaskCreateRequest {
                    title: "test task".into(),
                    plan: Some(TaskPlan {
                        subtasks: vec![SubtaskPlan {
                            id: "s1".into(),
                            title: "Step 1".into(),
                            ..Default::default()
                        }],
                        notes: None,
                    }),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        // Not eligible (no rating, not completed)
        let template_id = svc.extract_template("user1", &tid, "test *").await.unwrap();
        assert!(template_id.is_none());

        // Complete with low rating
        svc.complete_task("user1", &tid).await.unwrap();
        svc.record_feedback("user1", &tid, 2, TaskOutcome::Partial, None)
            .await
            .unwrap();

        // Still not eligible: partial outcome must not seed templates.
        let template_id = svc.extract_template("user1", &tid, "test *").await.unwrap();
        assert!(template_id.is_none());

        // A successful completion with the same replan count is eligible.
        svc.record_feedback("user1", &tid, 5, TaskOutcome::Success, None)
            .await
            .unwrap();
        let template_id = svc.extract_template("user1", &tid, "test *").await.unwrap();
        assert!(template_id.is_some());
    }

    #[tokio::test]
    async fn template_recommendation_finds_matches() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = LocalTaskService::new(tmp.path().to_path_buf());

        // Create task and extract template
        let tid = svc
            .create_task(
                "user1",
                "s1",
                TaskCreateRequest {
                    title: "add authentication".into(),
                    plan: Some(TaskPlan {
                        subtasks: vec![
                            SubtaskPlan {
                                id: "s1".into(),
                                title: "Setup JWT".into(),
                                ..Default::default()
                            },
                            SubtaskPlan {
                                id: "s2".into(),
                                title: "Add middleware".into(),
                                ..Default::default()
                            },
                        ],
                        notes: None,
                    }),
                    project_type: Some("Rust".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        svc.complete_task("user1", &tid).await.unwrap();
        svc.record_feedback("user1", &tid, 5, TaskOutcome::Success, Some(1200))
            .await
            .unwrap();
        svc.extract_template("user1", &tid, "add authentication")
            .await
            .unwrap();

        // Should find the template
        let recs = svc
            .recommend_templates("user1", "add auth module", Some("Rust"), 5)
            .await
            .unwrap();
        assert!(!recs.is_empty(), "Should find matching template");
        assert!(recs[0].template.goal_pattern.contains("authentication"));
    }

    #[tokio::test]
    async fn template_usage_increments_count() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = LocalTaskService::new(tmp.path().to_path_buf());

        // Create and extract template
        let tid = svc
            .create_task(
                "user1",
                "s1",
                TaskCreateRequest {
                    title: "test".into(),
                    plan: Some(TaskPlan {
                        subtasks: vec![SubtaskPlan {
                            id: "s1".into(),
                            title: "Step".into(),
                            ..Default::default()
                        }],
                        notes: None,
                    }),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        svc.complete_task("user1", &tid).await.unwrap();
        svc.record_feedback("user1", &tid, 5, TaskOutcome::Success, None)
            .await
            .unwrap();
        let template_id = svc
            .extract_template("user1", &tid, "test")
            .await
            .unwrap()
            .unwrap();

        // Use the template
        svc.record_template_usage("user1", &template_id)
            .await
            .unwrap();
        svc.record_template_usage("user1", &template_id)
            .await
            .unwrap();

        // Check count increased
        let recs = svc
            .recommend_templates("user1", "test", None, 5)
            .await
            .unwrap();
        assert!(!recs.is_empty());
        assert!(recs[0].template.use_count >= 2);
    }

    // ── Unhappy path / edge-case tests ──

    #[test]
    fn task_status_parse_empty_string() {
        assert_eq!(TaskStatus::parse_status(""), None);
    }

    #[test]
    fn task_status_parse_case_sensitive() {
        // "In_Progress" should not match (case-sensitive)
        assert_eq!(TaskStatus::parse_status("In_Progress"), None);
        assert_eq!(TaskStatus::parse_status("COMPLETED"), None);
    }

    #[test]
    fn task_status_serde_roundtrip() {
        for status in [
            TaskStatus::Pending,
            TaskStatus::InProgress,
            TaskStatus::Paused,
            TaskStatus::Completed,
            TaskStatus::Failed,
            TaskStatus::Cancelled,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let restored: TaskStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(restored, status);
        }
    }

    #[test]
    fn task_status_default_is_pending() {
        assert_eq!(TaskStatus::default(), TaskStatus::Pending);
    }

    #[test]
    fn task_outcome_roundtrip() {
        for (s, o) in [
            ("success", TaskOutcome::Success),
            ("partial", TaskOutcome::Partial),
            ("failed", TaskOutcome::Failed),
            ("cancelled", TaskOutcome::Cancelled),
        ] {
            assert_eq!(TaskOutcome::parse(s), Some(o));
            assert_eq!(o.as_str(), s);
        }
    }

    #[test]
    fn task_outcome_parse_unknown_returns_none() {
        assert_eq!(TaskOutcome::parse("unknown"), None);
        assert_eq!(TaskOutcome::parse(""), None);
    }

    #[test]
    fn task_outcome_serde_roundtrip() {
        let json = serde_json::to_string(&TaskOutcome::Success).unwrap();
        let restored: TaskOutcome = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, TaskOutcome::Success);
    }

    #[test]
    fn subtask_plan_default() {
        let st = SubtaskPlan::default();
        assert!(st.id.is_empty());
        assert!(st.depends_on.is_empty());
        assert_eq!(st.status, TaskStatus::Pending);
        assert!(st.effort.is_none());
        assert!(st.files.is_empty());
        assert!(st.acceptance_checks.is_empty());
    }

    #[test]
    fn subtask_plan_skip_serializing_empty() {
        let st = SubtaskPlan {
            id: "s1".into(),
            title: "test".into(),
            ..Default::default()
        };
        let json = serde_json::to_string(&st).unwrap();
        assert!(!json.contains("effort"));
        assert!(!json.contains("files"));
        assert!(!json.contains("acceptance_checks"));
    }

    #[test]
    fn task_plan_all_completed_100_pct() {
        let plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "a".into(),
                    status: TaskStatus::Completed,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "b".into(),
                    status: TaskStatus::Completed,
                    ..Default::default()
                },
            ],
            notes: None,
        };
        assert_eq!(plan.progress_pct(), 100);
        assert_eq!(plan.items_done(), 2);
    }

    #[test]
    fn task_plan_failed_counts_as_terminal() {
        let plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "a".into(),
                    status: TaskStatus::Failed,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "b".into(),
                    status: TaskStatus::Pending,
                    ..Default::default()
                },
            ],
            notes: None,
        };
        // Failed is NOT progress — only Completed counts.
        // A failed subtask needs retry or manual intervention.
        assert_eq!(plan.progress_pct(), 0);
        // items_done only counts Completed
        assert_eq!(plan.items_done(), 0);
    }

    #[test]
    fn task_plan_ready_with_all_pending_no_deps() {
        let plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "a".into(),
                    status: TaskStatus::Pending,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "b".into(),
                    status: TaskStatus::Pending,
                    ..Default::default()
                },
            ],
            notes: None,
        };
        assert_eq!(plan.ready_subtasks().len(), 2);
    }

    #[test]
    fn task_plan_ready_with_unmet_dependency() {
        let plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "a".into(),
                    status: TaskStatus::InProgress,
                    depends_on: vec![],
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "b".into(),
                    status: TaskStatus::Pending,
                    depends_on: vec!["a".into()],
                    ..Default::default()
                },
            ],
            notes: None,
        };
        // b depends on a which is InProgress (not Completed), so b is not ready
        assert!(plan.ready_subtasks().is_empty());
    }

    #[test]
    fn task_plan_ready_with_nonexistent_dependency() {
        let plan = TaskPlan {
            subtasks: vec![SubtaskPlan {
                id: "a".into(),
                status: TaskStatus::Pending,
                depends_on: vec!["nonexistent".into()],
                ..Default::default()
            }],
            notes: None,
        };
        // Dependency doesn't exist, so "a" is blocked
        assert!(plan.ready_subtasks().is_empty());
    }

    #[test]
    fn task_checkpoint_default() {
        let cp = TaskCheckpoint::default();
        assert!(cp.active_subtask_id.is_none());
        assert_eq!(cp.turn, 0);
        assert!(cp.session_id.is_none());
        assert!(cp.state.is_empty());
    }

    #[test]
    fn task_checkpoint_serde_roundtrip() {
        let mut state = serde_json::Map::new();
        state.insert("key".into(), serde_json::json!("value"));
        let cp = TaskCheckpoint {
            active_subtask_id: Some("s1".into()),
            turn: 5,
            session_id: Some("sess1".into()),
            state,
        };
        let json = serde_json::to_string(&cp).unwrap();
        let restored: TaskCheckpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.active_subtask_id, cp.active_subtask_id);
        assert_eq!(restored.turn, 5);
        assert_eq!(restored.state["key"], "value");
    }

    #[test]
    fn task_record_serde_defaults() {
        let json = r#"{
            "task_id": "t1",
            "user_id": "u1",
            "title": "test",
            "status": "pending",
            "progress_pct": 0,
            "items_done": 0,
            "items_total": 0,
            "created_at": "2024-01-01",
            "updated_at": "2024-01-01"
        }"#;
        let r: TaskRecord = serde_json::from_str(json).unwrap();
        assert_eq!(r.replan_count, 0);
        assert_eq!(r.auto_adjustments, 0);
        assert!(r.user_rating.is_none());
        assert!(r.outcome.is_none());
        assert!(r.project_type.is_none());
        assert!(r.agent_id.is_none());
    }

    #[test]
    fn learning_stats_default() {
        let ls = LearningStats::default();
        assert_eq!(ls.total_tasks, 0);
        assert_eq!(ls.completed_tasks, 0);
        assert!(ls.avg_rating.is_none());
        assert_eq!(ls.avg_replan_count, 0.0);
        assert_eq!(ls.inferred_success_rate, 0.0);
    }

    #[test]
    fn task_guarded_update_sql_is_owner_bound_and_non_terminal_only() {
        let sql = guarded_agent_task_update_sql("updated_at = NOW()");
        assert_eq!(
            sql,
            "UPDATE agent_tasks SET updated_at = NOW() \
         WHERE user_id = ? AND task_id = ? AND status IN ('pending', 'in_progress', 'paused')"
        );

        for status in [
            TaskStatus::Pending,
            TaskStatus::InProgress,
            TaskStatus::Paused,
        ] {
            assert!(
                AGENT_TASK_KNOWN_NON_TERMINAL_STATUS_GUARD.contains(status.as_str()),
                "non-terminal status {} must be admitted by the SQL guard",
                status.as_str()
            );
            assert!(!status.is_terminal());
        }

        for status in [
            TaskStatus::Completed,
            TaskStatus::Failed,
            TaskStatus::Cancelled,
        ] {
            assert!(
                !AGENT_TASK_KNOWN_NON_TERMINAL_STATUS_GUARD.contains(status.as_str()),
                "terminal status {} must not be admitted by the SQL guard",
                status.as_str()
            );
            assert!(status.is_terminal());
        }
    }

    /// P2-C: progress_pct must only count Completed subtasks, not Failed/Cancelled.
    #[test]
    fn progress_pct_excludes_failed_and_cancelled() {
        let plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "1".into(),
                    title: "a".into(),
                    status: TaskStatus::Completed,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "2".into(),
                    title: "b".into(),
                    status: TaskStatus::Failed,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "3".into(),
                    title: "c".into(),
                    status: TaskStatus::Cancelled,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "4".into(),
                    title: "d".into(),
                    status: TaskStatus::Pending,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        // Only 1 of 4 is Completed → 25%
        assert_eq!(
            plan.progress_pct(),
            25,
            "only Completed subtasks count as progress (not Failed/Cancelled)"
        );
    }
}
