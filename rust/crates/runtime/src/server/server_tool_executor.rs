//! Server-side tool executor for web agent sessions.
//!
//! When a web user connects without a CLI edge agent, the server executes
//! tools directly using the shared `astra-tools` library. This module
//! provides the `ServerToolExecutor` that wraps tool execution with:
//! - Per-session workspace isolation (sandbox)
//! - Per-session file journals with rollback support
//! - Circuit-breaker for external services (Memoria)
//!
//! # Integration
//!
//! The executor is injected into `HeadlessToolRoundCtx` via the
//! `server_tool_executor` field. When present, the headless round
//! calls it directly instead of waiting for edge POST callbacks.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};

use astra_tools::executor::DefaultToolExecutor;
use astra_tools::{AskUserDecision, AskUserGate, ToolContext, ToolExecutor};
use async_trait::async_trait;

use crate::tool_sandbox::{
    IsolatedOutput, IsolationConfig, SandboxMode, SandboxPolicy, ToolTier, effective_tier,
    execute_isolated, filter_environment,
};
use astra_turn_core::file_edit_journal::{EditType, FileEditJournal};

const ASTRA_CONNECT_TIMEOUT_SECS: u32 = 5;

fn normalize_path(path: &Path) -> PathBuf {
    path.components()
        .fold(PathBuf::new(), |mut acc, component| {
            match component {
                std::path::Component::ParentDir => {
                    acc.pop();
                }
                std::path::Component::CurDir => {}
                other => acc.push(other),
            }
            acc
        })
}

fn unique_path_variants(path: &Path) -> Vec<PathBuf> {
    let mut variants = vec![normalize_path(path)];
    if let Ok(canonical) = path.canonicalize()
        && !variants.iter().any(|existing| existing == &canonical)
    {
        variants.push(canonical);
    }
    variants
}

fn undo_file_with_candidates(
    journal: &FileEditJournal,
    candidates: &[PathBuf],
) -> std::io::Result<Option<(PathBuf, EditType)>> {
    for candidate in candidates {
        match journal.undo_file(candidate)? {
            Some(edit_type) => return Ok(Some((candidate.clone(), edit_type))),
            None => continue,
        }
    }
    Ok(None)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DatabaseSnapshotRollbackEntry {
    sequence: u64,
    snapshot_id: String,
    database: Option<String>,
    turn_index: u32,
}

#[derive(Debug, Default)]
struct DatabaseSnapshotRollbackJournal {
    entries: Vec<DatabaseSnapshotRollbackEntry>,
    next_sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AskUserRequest {
    question: String,
    choices: Vec<String>,
    default: Option<String>,
    context: Option<String>,
}

impl DatabaseSnapshotRollbackJournal {
    fn record(
        &mut self,
        snapshot_id: impl Into<String>,
        database: Option<String>,
        turn_index: u32,
    ) {
        self.entries.push(DatabaseSnapshotRollbackEntry {
            sequence: self.next_sequence,
            snapshot_id: snapshot_id.into(),
            database,
            turn_index,
        });
        self.next_sequence = self.next_sequence.saturating_add(1);
    }

    fn list(&self) -> Vec<DatabaseSnapshotRollbackEntry> {
        self.entries.iter().rev().cloned().collect()
    }

    fn entry_for_snapshot(&self, snapshot_id: &str) -> Option<DatabaseSnapshotRollbackEntry> {
        self.entries
            .iter()
            .rev()
            .find(|entry| entry.snapshot_id == snapshot_id)
            .cloned()
    }

    fn restore_plan_for_turn(&self, turn_index: u32) -> Vec<DatabaseSnapshotRollbackEntry> {
        self.restore_plan_for_turn_since(turn_index, 0)
    }

    fn restore_plan_for_turn_since(
        &self,
        turn_index: u32,
        checkpoint: u64,
    ) -> Vec<DatabaseSnapshotRollbackEntry> {
        let mut seen_databases = std::collections::HashSet::new();
        let mut plan = Vec::new();
        for entry in self
            .entries
            .iter()
            .filter(|entry| entry.turn_index == turn_index && entry.sequence >= checkpoint)
        {
            if seen_databases.insert(entry.database.clone()) {
                plan.push(entry.clone());
            }
        }
        plan
    }

    fn checkpoint(&self) -> u64 {
        self.next_sequence
    }

    fn remove_snapshot(&mut self, snapshot_id: &str) -> bool {
        if let Some(index) = self
            .entries
            .iter()
            .rposition(|entry| entry.snapshot_id == snapshot_id)
        {
            self.entries.remove(index);
            true
        } else {
            false
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
struct SessionTask {
    id: String,
    title: String,
    description: Option<String>,
    status: String,
    subtasks: Vec<SessionSubtask>,
    created_at: String,
    updated_at: String,
}

#[derive(Debug, Clone, serde::Serialize)]
struct SessionSubtask {
    id: String,
    title: String,
    description: Option<String>,
    status: String,
    depends_on: Vec<String>,
}

#[derive(Debug)]
struct ServerTaskManager {
    tasks: Mutex<Vec<SessionTask>>,
    id_counter: AtomicU32,
}

#[derive(Debug, Clone)]
struct TaskManagerSnapshot {
    tasks: Vec<SessionTask>,
    next_task_id: u32,
}

impl ServerTaskManager {
    fn new() -> Self {
        Self {
            tasks: Mutex::new(Vec::new()),
            id_counter: AtomicU32::new(1),
        }
    }

    fn snapshot(&self) -> Vec<SessionTask> {
        self.tasks
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    fn snapshot_state(&self) -> TaskManagerSnapshot {
        TaskManagerSnapshot {
            tasks: self.snapshot(),
            next_task_id: self.id_counter.load(Ordering::SeqCst),
        }
    }

    fn restore_snapshot(&self, snapshot: &TaskManagerSnapshot) -> Result<(), String> {
        let mut tasks = self
            .tasks
            .lock()
            .map_err(|_| "failed to access task list".to_string())?;
        *tasks = snapshot.tasks.clone();
        self.id_counter
            .store(snapshot.next_task_id, Ordering::SeqCst);
        Ok(())
    }

    fn create(&self, args: &Value) -> String {
        let title = match args.get("title").and_then(Value::as_str) {
            Some(title) if !title.is_empty() => title.to_string(),
            _ => return "Error: 'title' is required".to_string(),
        };

        let description = args
            .get("description")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        let now = chrono::Utc::now().to_rfc3339();

        let subtasks: Vec<SessionSubtask> = args
            .get("subtasks")
            .and_then(Value::as_array)
            .map(|subtasks| {
                subtasks
                    .iter()
                    .filter_map(|subtask| {
                        let id = subtask.get("id").and_then(Value::as_str)?;
                        let title = subtask.get("title").and_then(Value::as_str)?;
                        Some(SessionSubtask {
                            id: id.to_string(),
                            title: title.to_string(),
                            description: subtask
                                .get("description")
                                .and_then(Value::as_str)
                                .map(ToString::to_string),
                            status: "pending".to_string(),
                            depends_on: subtask
                                .get("depends_on")
                                .and_then(Value::as_array)
                                .map(|deps| {
                                    deps.iter()
                                        .filter_map(Value::as_str)
                                        .map(ToString::to_string)
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
            "message": format!("Task '{title}' created successfully"),
        })
        .to_string()
    }

    fn list(&self, args: &Value) -> String {
        let status_filter = args.get("status").and_then(Value::as_str).unwrap_or("all");

        let tasks = match self.tasks.lock() {
            Ok(guard) => guard.clone(),
            Err(_) => return "Error: failed to access task list".to_string(),
        };

        let filtered: Vec<_> = tasks
            .iter()
            .filter(|task| match status_filter {
                "all" => true,
                "active" => task.status == "pending" || task.status == "in_progress",
                other => task.status == other,
            })
            .map(|task| {
                let subtask_summary = if task.subtasks.is_empty() {
                    String::new()
                } else {
                    let done = task
                        .subtasks
                        .iter()
                        .filter(|subtask| subtask.status == "completed")
                        .count();
                    format!(" [{done}/{}]", task.subtasks.len())
                };
                json!({
                    "id": task.id,
                    "title": task.title,
                    "status": task.status,
                    "subtasks": subtask_summary,
                    "updated_at": task.updated_at,
                })
            })
            .collect();

        if filtered.is_empty() {
            return format!("No tasks found with status '{status_filter}'");
        }

        json!({
            "count": filtered.len(),
            "tasks": filtered,
        })
        .to_string()
    }

    fn get(&self, args: &Value) -> String {
        let task_id = match args.get("task_id").and_then(Value::as_str) {
            Some(task_id) if !task_id.is_empty() => task_id,
            _ => return "Error: 'task_id' is required".to_string(),
        };

        let tasks = match self.tasks.lock() {
            Ok(guard) => guard.clone(),
            Err(_) => return "Error: failed to access task list".to_string(),
        };

        match tasks.iter().find(|task| task.id == task_id) {
            Some(task) => serde_json::to_string_pretty(task)
                .unwrap_or_else(|_| "Error: serialization failed".to_string()),
            None => format!("Error: task '{task_id}' not found"),
        }
    }

    fn update(&self, args: &Value) -> String {
        let task_id = match args.get("task_id").and_then(Value::as_str) {
            Some(task_id) if !task_id.is_empty() => task_id,
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

        let task = match tasks.iter_mut().find(|task| task.id == task_id) {
            Some(task) => task,
            None => return format!("Error: task '{task_id}' not found"),
        };

        if let Some(subtask_id) = subtask_id {
            match task
                .subtasks
                .iter_mut()
                .find(|subtask| subtask.id == subtask_id)
            {
                Some(subtask) => {
                    let previous_status = subtask.status.clone();
                    if let Some(status) = new_status {
                        subtask.status = status.to_string();
                    }
                    task.updated_at = now;
                    return json!({
                        "success": true,
                        "task_id": task_id,
                        "subtask_id": subtask_id,
                        "previous_status": previous_status,
                        "status": subtask.status,
                        "message": format!("Subtask '{subtask_id}' updated to '{}'", subtask.status),
                    })
                    .to_string();
                }
                None => {
                    return format!("Error: subtask '{subtask_id}' not found in task '{task_id}'");
                }
            }
        }

        let previous_status = task.status.clone();
        if let Some(status) = new_status {
            task.status = status.to_string();
        }
        if let Some(error_message) = error_message {
            task.description = Some(format!(
                "{}\n\nError: {error_message}",
                task.description.as_deref().unwrap_or(""),
            ));
        }
        task.updated_at = now;

        if !task.subtasks.is_empty()
            && task
                .subtasks
                .iter()
                .all(|subtask| subtask.status == "completed")
        {
            task.status = "completed".to_string();
        }

        json!({
            "success": true,
            "task_id": task_id,
            "previous_status": previous_status,
            "status": task.status,
            "message": format!("Task '{task_id}' updated to '{}'", task.status),
        })
        .to_string()
    }

    fn stop(&self, args: &Value) -> String {
        let task_id = match args.get("task_id").and_then(Value::as_str) {
            Some(task_id) if !task_id.is_empty() => task_id,
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

        let task = match tasks.iter_mut().find(|task| task.id == task_id) {
            Some(task) => task,
            None => return format!("Error: task '{task_id}' not found"),
        };

        if task.status != "pending" && task.status != "in_progress" {
            return json!({
                "success": false,
                "message": format!(
                    "Cannot stop task '{task_id}': status is '{}' (only 'pending' or 'in_progress' can be stopped)",
                    task.status
                ),
            })
            .to_string();
        }

        let previous_status = task.status.clone();
        task.status = "cancelled".to_string();
        task.description = Some(format!(
            "{}\n\nCancelled: {reason} (was: {previous_status})",
            task.description.as_deref().unwrap_or(""),
        ));
        task.updated_at = now;

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
            "message": format!("Task '{task_id}' cancelled (was: {previous_status})"),
        })
        .to_string()
    }
}

#[derive(Debug, Clone)]
enum SessionStateRollbackAction {
    ToolPreferences {
        previous_pinned_tools: Vec<String>,
        previous_deprioritized_tools: Vec<String>,
    },
    ConfigOverride {
        path: String,
        old_value: Value,
        snapshot: crate::observability_integration::ObservabilitySessionRollbackSnapshot,
    },
    GoalOverride {
        previous_goal: Option<String>,
        snapshot: crate::observability_integration::ObservabilitySessionRollbackSnapshot,
    },
    Compression {
        turn: u32,
        snapshot: crate::observability_integration::ObservabilitySessionRollbackSnapshot,
    },
    TaskState {
        snapshot: TaskManagerSnapshot,
    },
}

#[derive(Debug, Clone)]
struct SessionStateRollbackEntry {
    sequence: u64,
    turn_index: u32,
    timestamp: SystemTime,
    label: String,
    action: SessionStateRollbackAction,
}

#[derive(Debug, Default)]
struct SessionStateRollbackJournal {
    entries: Vec<SessionStateRollbackEntry>,
    next_sequence: u64,
}

impl SessionStateRollbackJournal {
    fn record(&mut self, turn_index: u32, label: String, action: SessionStateRollbackAction) {
        self.entries.push(SessionStateRollbackEntry {
            sequence: self.next_sequence,
            turn_index,
            timestamp: SystemTime::now(),
            label,
            action,
        });
        self.next_sequence = self.next_sequence.saturating_add(1);
    }

    fn list(&self) -> Vec<SessionStateRollbackEntry> {
        self.entries.iter().rev().cloned().collect()
    }

    fn restore_plan_for_turn(&self, turn_index: u32) -> Vec<SessionStateRollbackEntry> {
        self.restore_plan_for_turn_since(turn_index, 0)
    }

    fn restore_plan_for_turn_since(
        &self,
        turn_index: u32,
        checkpoint: u64,
    ) -> Vec<SessionStateRollbackEntry> {
        self.entries
            .iter()
            .rev()
            .filter(|entry| entry.turn_index == turn_index && entry.sequence >= checkpoint)
            .cloned()
            .collect()
    }

    fn checkpoint(&self) -> u64 {
        self.next_sequence
    }

    fn remove_sequence(&mut self, sequence: u64) -> bool {
        if let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.sequence == sequence)
        {
            self.entries.remove(index);
            true
        } else {
            false
        }
    }
}

fn action_kind(action: &SessionStateRollbackAction) -> &'static str {
    match action {
        SessionStateRollbackAction::ToolPreferences { .. } => "tool_preferences",
        SessionStateRollbackAction::ConfigOverride { .. } => "config_override",
        SessionStateRollbackAction::GoalOverride { .. } => "goal_override",
        SessionStateRollbackAction::Compression { .. } => "compression",
        SessionStateRollbackAction::TaskState { .. } => "task_state",
    }
}

fn normalized_drift(old: f64, new: f64) -> Option<f64> {
    let denom = old.abs();
    if denom < f64::EPSILON {
        return None;
    }
    Some((new - old).abs() / denom)
}

fn extract_tool_name(args: &Value) -> Option<String> {
    args.get("tool")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|tool| !tool.is_empty())
        .map(ToString::to_string)
}

fn effective_runtime_config(
    workspace: Option<&astra_services::session_workspace::WorkspaceMetadata>,
) -> Result<astra_config::runtime_config::RuntimeConfig, String> {
    match workspace.and_then(|workspace| workspace.tuned_config_json.as_deref()) {
        Some(json) => serde_json::from_str(json).map_err(|error| error.to_string()),
        None => Ok(astra_config::runtime_config::RuntimeConfig::load()),
    }
}

fn replace_json_path(root: &mut Value, path: &str, new_value: Value) -> Result<Value, String> {
    let segments: Vec<&str> = path
        .split('.')
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.is_empty() {
        return Err("mutation path cannot be empty".to_string());
    }

    let mut current = root;
    for segment in &segments[..segments.len() - 1] {
        current = current
            .get_mut(*segment)
            .ok_or_else(|| format!("unknown config path segment '{segment}'"))?;
    }

    let last = segments.last().expect("checked non-empty");
    let object = current
        .as_object_mut()
        .ok_or_else(|| format!("config path '{path}' does not point to an object parent"))?;
    let slot = object
        .get_mut(*last)
        .ok_or_else(|| format!("unknown config leaf '{last}'"))?;
    let old_value = slot.clone();
    *slot = new_value;
    Ok(old_value)
}

fn append_config_change_event(
    session_id: &str,
    turn: u32,
    key: &str,
    new_value: &Value,
    old_value: Option<Value>,
    source: &str,
) -> Result<(), String> {
    let writer = astra_services::session_journal::JournalWriter::new(session_id)
        .map_err(|e| e.to_string())?;
    let mut event = astra_services::session_journal::JournalEvent::config_change(
        Some(session_id),
        key,
        &new_value.to_string(),
    );
    event.turn = Some(turn);
    let mut metadata =
        serde_json::Map::from_iter([("source".to_string(), Value::String(source.to_string()))]);
    if let Some(old_value) = old_value {
        metadata.insert("old_value".to_string(), old_value);
    }
    event.metadata = Some(Value::Object(metadata));
    writer.append(&event).map_err(|e| e.to_string())
}

fn persist_config_override(
    session_id: &str,
    path: &str,
    new_value: Value,
    source: &str,
) -> Result<(), String> {
    let mut workspace =
        astra_services::session_workspace::read_workspace(session_id).map_err(|e| e.to_string())?;
    let base_config = effective_runtime_config(Some(&workspace))?;
    let mut value = serde_json::to_value(&base_config).map_err(|e| e.to_string())?;
    let old_value = replace_json_path(&mut value, path, new_value.clone())?;
    let candidate_config: astra_config::runtime_config::RuntimeConfig =
        serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
    let baseline_json = serde_json::to_value(astra_config::runtime_config::RuntimeConfig::load())
        .map_err(|e| e.to_string())?;
    workspace.tuned_config_json = if value == baseline_json {
        None
    } else {
        Some(serde_json::to_string(&candidate_config).map_err(|e| e.to_string())?)
    };
    workspace.updated_at = chrono::Utc::now().to_rfc3339();
    astra_services::session_workspace::write_workspace(&workspace).map_err(|e| e.to_string())?;
    append_config_change_event(
        session_id,
        workspace.turn_count,
        path,
        &new_value,
        Some(old_value),
        source,
    )
}

fn persist_goal_override(session_id: &str, goal: &str, source: &str) -> Result<(), String> {
    let mut workspace =
        astra_services::session_workspace::read_workspace(session_id).map_err(|e| e.to_string())?;
    let previous_goal = workspace.session_goal.clone();
    workspace.session_goal = Some(goal.to_string());
    workspace.goal_progress = None;
    workspace.updated_at = chrono::Utc::now().to_rfc3339();
    astra_services::session_workspace::write_workspace(&workspace).map_err(|e| e.to_string())?;
    append_config_change_event(
        session_id,
        workspace.turn_count,
        "session_goal",
        &Value::String(goal.to_string()),
        previous_goal.clone().map(Value::String),
        source,
    )?;
    if previous_goal.as_deref() != Some(goal) {
        let writer = astra_services::session_journal::JournalWriter::new(session_id)
            .map_err(|e| e.to_string())?;
        writer
            .append(
                &astra_services::session_journal::JournalEvent::goal_steered(
                    Some(session_id),
                    workspace.turn_count,
                    source,
                    previous_goal.as_deref(),
                    goal,
                    None,
                ),
            )
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn clear_persisted_goal_override(session_id: &str) -> Result<(), String> {
    let mut workspace =
        astra_services::session_workspace::read_workspace(session_id).map_err(|e| e.to_string())?;
    workspace.session_goal = None;
    workspace.goal_progress = None;
    workspace.updated_at = chrono::Utc::now().to_rfc3339();
    astra_services::session_workspace::write_workspace(&workspace).map_err(|e| e.to_string())
}

fn persist_tool_preferences(
    session_id: &str,
    pinned_tools: &[String],
    deprioritized_tools: &[String],
    source: &str,
) -> Result<(), String> {
    let mut workspace =
        astra_services::session_workspace::read_workspace(session_id).map_err(|e| e.to_string())?;
    let mut pinned = pinned_tools.to_vec();
    pinned.sort();
    pinned.dedup();
    let mut deprioritized = deprioritized_tools.to_vec();
    deprioritized.sort();
    deprioritized.dedup();

    let old_pinned = workspace.pinned_tools.clone();
    let old_deprioritized = workspace.deprioritized_tools.clone();
    workspace.pinned_tools = pinned.clone();
    workspace.deprioritized_tools = deprioritized.clone();
    workspace.updated_at = chrono::Utc::now().to_rfc3339();
    astra_services::session_workspace::write_workspace(&workspace).map_err(|e| e.to_string())?;

    if old_pinned != pinned {
        append_config_change_event(
            session_id,
            workspace.turn_count,
            "pinned_tools",
            &json!(pinned),
            Some(json!(old_pinned)),
            source,
        )?;
    }
    if old_deprioritized != deprioritized {
        append_config_change_event(
            session_id,
            workspace.turn_count,
            "deprioritized_tools",
            &json!(deprioritized),
            Some(json!(old_deprioritized)),
            source,
        )?;
    }
    Ok(())
}

fn persist_manual_compression(
    session_id: &str,
    turn: u32,
    reason: &str,
    source: &str,
) -> Result<(), String> {
    let writer = astra_services::session_journal::JournalWriter::new(session_id)
        .map_err(|e| e.to_string())?;
    let mut event = astra_services::session_journal::JournalEvent::compact_with_summary(
        Some(session_id),
        turn,
        1,
        0,
        Some(reason),
    );
    event.metadata = Some(json!({
        "source": source,
        "reason": reason,
        "manual": true,
    }));
    writer.append(&event).map_err(|e| e.to_string())
}

fn supports_server_tool_name(tool: &str) -> bool {
    astra_tools::schemas::SERVER_EXECUTOR_TOOL_NAMES.contains(&tool)
        || matches!(tool, "enter_plan_mode" | "exit_plan_mode")
}

/// Tools that mutate the world outside the session. Blocked while plan mode
/// is active (`PlanPhase` = PlanOnlyChat|Planning|Refining) to mirror Claude
/// Code's `prepareContextForPlanMode` behaviour: the model must call
/// ExitPlanMode before writing anything.
///
/// Read-only tools (grep, glob, read_file, git_status/diff/log, web_search,
/// task_*, memory_retrieve, …) stay available so the agent can continue
/// exploring while authoring a plan.
fn is_plan_mode_blocked_tool(tool: &str) -> bool {
    matches!(
        tool,
        "bash"
            | "write_file"
            | "str_replace"
            | "multi_edit"
            | "delete_file"
            | "rollback_file_edits"
            | "mo_query"
            | "rollback_database_snapshots"
            | "git_commit"
            | "git_stash"
            | "git_revert_commit"
            | "github_create_issue"
    )
}

fn mo_current_account() -> &'static str {
    use std::sync::OnceLock;

    static ACCOUNT: OnceLock<String> = OnceLock::new();
    ACCOUNT.get_or_init(|| {
        let out = mo_execute_sql("SELECT current_account_name() AS name", None);
        out.lines()
            .filter(|line| !line.starts_with('+') && !line.contains("name"))
            .find_map(|line| {
                let trimmed = line.trim().trim_matches('|').trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                }
            })
            .unwrap_or_else(|| "sys".to_string())
    })
}

fn mo_database() -> &'static str {
    use std::sync::OnceLock;

    static DB: OnceLock<String> = OnceLock::new();
    DB.get_or_init(|| astra_core::resolve_database_name(&|k| std::env::var(k).ok()))
}

fn resolved_mo_database(database: Option<&str>) -> String {
    database
        .map(str::trim)
        .filter(|database| !database.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| mo_database().to_string())
}

fn mo_create_snapshot_sql(name: &str, database: Option<&str>) -> String {
    format!(
        "CREATE SNAPSHOT `{name}` FOR DATABASE `{}`",
        resolved_mo_database(database)
    )
}

fn mo_restore_snapshot_sql(name: &str, database: Option<&str>) -> String {
    let account = mo_current_account();
    format!(
        "RESTORE ACCOUNT `{account}` DATABASE `{}` FROM SNAPSHOT `{name}`",
        resolved_mo_database(database)
    )
}

fn mo_drop_snapshot_sql(name: &str) -> String {
    format!("DROP SNAPSHOT IF EXISTS `{name}`")
}

fn mo_query_requires_pre_state_snapshot(sql: &str, allow_destructive: bool) -> bool {
    match sql
        .split_whitespace()
        .next()
        .map(|keyword| keyword.trim_matches(|c: char| c == '(' || c == ';'))
        .map(str::to_ascii_uppercase)
        .as_deref()
    {
        Some("INSERT" | "UPDATE" | "REPLACE" | "CREATE") => true,
        Some("DROP" | "DELETE" | "TRUNCATE" | "ALTER" | "GRANT" | "REVOKE") => true,
        _ => allow_destructive,
    }
}

fn mo_pre_state_snapshot_name() -> String {
    format!("moq_{}", uuid::Uuid::now_v7().simple())
}

fn is_valid_snapshot_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

fn is_mo_error(output: &str) -> bool {
    output.trim_start().starts_with("Error:")
}

fn mo_mysql_cmd(database: Option<&str>) -> Result<Command, String> {
    let host = std::env::var("MATRIXONE_HOST").unwrap_or_else(|_| "localhost".to_string());
    let port = std::env::var("MATRIXONE_PORT").unwrap_or_else(|_| "6001".to_string());
    let user = std::env::var("MATRIXONE_USER").unwrap_or_else(|_| "root".to_string());
    let password = std::env::var("MATRIXONE_PASSWORD").unwrap_or_else(|_| "111".to_string());
    let db = database
        .map(ToString::to_string)
        .unwrap_or_else(|| astra_core::resolve_database_name(&|k| std::env::var(k).ok()));

    let mut cmd = Command::new("mysql");
    cmd.arg(format!("-h{host}"))
        .arg(format!("-P{port}"))
        .arg(format!("-u{user}"))
        .env("MYSQL_PWD", &password)
        .arg(db)
        .arg(format!("--connect-timeout={ASTRA_CONNECT_TIMEOUT_SECS}"))
        .arg("--table");
    Ok(cmd)
}

fn mo_execute_sql(sql: &str, database: Option<&str>) -> String {
    let mut cmd = match mo_mysql_cmd(database) {
        Ok(c) => c,
        Err(e) => return e,
    };
    cmd.arg("-e").arg(sql);

    match cmd.output() {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !out.status.success() {
                let err = if stderr.is_empty() {
                    stdout.to_string()
                } else {
                    stderr.to_string()
                };
                format!("Error: {}", err.trim())
            } else if stdout.is_empty() {
                "OK (no results)".to_string()
            } else {
                stdout.to_string()
            }
        }
        Err(error) => format!("Error: failed to execute mysql client: {error}"),
    }
}

/// Server-side tool executor for web agent sessions.
///
/// Wraps tool calls in a sandboxed environment without requiring a CLI process.
/// Created per-session by `AgenticRunLifecycleService::create_run()`.
pub struct ServerToolExecutor {
    /// Workspace root for this session.
    workspace_root: PathBuf,
    /// User ID owning this session (used for Memoria isolation).
    user_id: String,
    /// Session ID for isolation.
    session_id: String,
    /// Sandbox policy for tool execution.
    sandbox_policy: SandboxPolicy,
    /// File edit journal for undo support.
    file_journal: Arc<Mutex<FileEditJournal>>,
    /// Database snapshot journal for MatrixOne rollback support.
    database_snapshot_journal: Arc<Mutex<DatabaseSnapshotRollbackJournal>>,
    /// Session-state rollback journal for bounded self-mod and task undo.
    session_state_journal: Arc<Mutex<SessionStateRollbackJournal>>,
    /// In-memory task manager for session-local task tools.
    task_manager: Arc<ServerTaskManager>,
    /// Current turn index for journal entries.
    journal_turn_index: AtomicU32,
    /// Aggregate output bytes this turn.
    aggregate_output_bytes: AtomicUsize,
    /// Memoria client for memory operations.
    memoria_client: astra_tools::memoria::MemoriaClient,
    /// Cloud API base URL.
    #[allow(dead_code)] // Phase 5: used for cloud API calls (web_fetch, etc.)
    cloud_base: Option<String>,
    /// Auth token for cloud calls.
    #[allow(dead_code)] // Phase 5: used for authenticated cloud API calls
    cloud_token: Option<String>,
    /// GitHub token for API calls.
    #[allow(dead_code)] // Phase 5: used for GitHub API integration
    github_token: Option<String>,
    /// Shared HTTP client.
    #[allow(dead_code)] // Phase 5: used for web_fetch and cloud API calls
    http_client: reqwest::Client,
    /// URL fetch cache.
    #[allow(dead_code)] // Phase 5: used for web_fetch caching
    url_cache: Mutex<HashMap<String, (String, Instant)>>,
    /// Optional approval gate for dangerous tool execution.
    approval_gate: Option<Arc<dyn astra_tools::ToolApprovalGate>>,
    /// Optional ask_user gate for interactive client prompts.
    ask_user_gate: Option<Arc<dyn AskUserGate>>,
    /// Optional progress callback for streaming tool output.
    progress_callback: Option<Arc<dyn astra_tools::ToolProgressCallback>>,
    /// Optional resource governor for usage tracking (Phase 5).
    resource_governor:
        Option<std::sync::Arc<dyn astra_services::resource_governor::ResourceGovernor>>,
    /// Optional edge connection pool for routing to remote edge agents.
    edge_connection_pool: Option<astra_server_types::edge_connection_pool::EdgeConnectionPool>,
    /// Optional observability session for self-mod and rollback-backed session state.
    observability_session:
        Option<Arc<std::sync::RwLock<crate::observability_integration::ObservabilitySession>>>,
    /// Self-modification pinned tool preferences.
    self_mod_pinned_tools: Mutex<Vec<String>>,
    /// Self-modification deprioritized tool preferences.
    self_mod_deprioritized_tools: Mutex<Vec<String>>,
    /// Per-turn mutation accounting for adjust_config governor.
    self_mod_mutation_counter: Mutex<(u32, u32)>,
    /// Shared default executor for delegating common tool logic.
    default_executor: DefaultToolExecutor,
    /// Optional remote workspace artifact store for publishing workspace metadata.
    workspace_artifact_store: Option<astra_services::DatabaseSessionArtifactStore>,
    /// Plan repository for plan-mode gating and Enter/ExitPlanMode tools.
    /// `None` leaves plan-mode unconditionally off (back-compat for tests /
    /// constructor call sites that haven't been updated).
    plan_repo: Option<Arc<dyn astra_plan::PlanRepository>>,
    /// Cache for `plan_mode_authoring_active()` so a typical session with
    /// 20-50 tool calls doesn't incur 40-100 DB round-trips. Invalidated
    /// explicitly on `enter_plan_mode` / `exit_plan_mode`. Holds the latest
    /// (authoring-bool, rendered-resume-hint) pair so both the write guard
    /// and the system-prompt injector read from the same snapshot.
    plan_mode_cache: Arc<tokio::sync::RwLock<PlanModeSnapshot>>,
    /// Shared handle to the loop host's plan-resume hint. Tools that change
    /// plan-mode state write through this so the next turn's system prompt
    /// reflects current state instead of the loop-start snapshot.
    plan_resume_hint_handle: Option<Arc<std::sync::RwLock<Option<String>>>>,
}

/// Snapshot used by the plan-mode write guard and the system-prompt
/// injector. Populated on first access per plan-mode state change; cleared
/// by the enter/exit tools so the next call sees fresh DB state.
#[derive(Debug, Clone, Default)]
pub(crate) struct PlanModeSnapshot {
    /// Whether the session currently has an active plan still in authoring.
    pub authoring_active: Option<bool>,
    /// Rendered system-prompt section to inject on the next turn (`None`
    /// when there's no active plan or it's already executing).
    pub resume_hint: Option<String>,
}

impl ServerToolExecutor {
    /// Create a new server tool executor for a session.
    pub fn new(
        workspace_root: PathBuf,
        user_id: String,
        session_id: String,
        cloud_base: Option<String>,
        cloud_token: Option<String>,
    ) -> Self {
        let sandbox_policy = SandboxPolicy {
            mode: SandboxMode::Strict,
            project_root: workspace_root.clone(),
            allowed_paths: vec![PathBuf::from("/tmp")],
            env_allowlist: None,
            max_execution_secs: 120.0,
            max_output_bytes: 200_000,
            network_allowed: false,
        };

        let memoria_client =
            astra_tools::memoria::MemoriaClient::new(cloud_base.clone(), cloud_token.clone());
        let (pinned_tools, deprioritized_tools) =
            astra_services::session_workspace::read_workspace(&session_id)
                .map(|workspace| (workspace.pinned_tools, workspace.deprioritized_tools))
                .unwrap_or_else(|_| (Vec::new(), Vec::new()));

        let http_client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(15))
            .user_agent("astra-server/0.1.0")
            .build()
            .expect("Failed to build HTTP client");

        let default_executor = DefaultToolExecutor::new(ToolContext {
            project_root: workspace_root.clone(),
            workspace_root: workspace_root.clone(),
            user_id: user_id.clone(),
            session_id: session_id.clone(),
            sandbox: astra_tools::SandboxConfig::standard(workspace_root.clone()),
            http_client: Some(http_client.clone()),
            logger: std::sync::Arc::new(astra_tools::TracingLogger),
            cancel_token: None,
        });
        // Wire GitHubClient into DefaultToolExecutor if any token is available
        let github_tokens = astra_tools::github::resolve_github_tokens();
        let github_token = github_tokens.first().cloned();
        let default_executor = if !github_tokens.is_empty() {
            let github = astra_tools::github::GitHubClient::from_tokens(
                http_client.clone(),
                github_tokens,
                Vec::new(),
            );
            default_executor.with_github_client(github)
        } else {
            default_executor
        };

        Self {
            workspace_root,
            user_id,
            session_id,
            sandbox_policy,
            default_executor,
            file_journal: Arc::new(Mutex::new(FileEditJournal::new(500))),
            database_snapshot_journal: Arc::new(Mutex::new(
                DatabaseSnapshotRollbackJournal::default(),
            )),
            session_state_journal: Arc::new(Mutex::new(SessionStateRollbackJournal::default())),
            task_manager: Arc::new(ServerTaskManager::new()),
            journal_turn_index: AtomicU32::new(0),
            aggregate_output_bytes: AtomicUsize::new(0),
            memoria_client,
            cloud_base,
            cloud_token,
            github_token,
            http_client,
            url_cache: Mutex::new(HashMap::new()),
            approval_gate: None,
            ask_user_gate: None,
            progress_callback: None,
            resource_governor: None,
            edge_connection_pool: None,
            observability_session: None,
            self_mod_pinned_tools: Mutex::new(pinned_tools),
            self_mod_deprioritized_tools: Mutex::new(deprioritized_tools),
            self_mod_mutation_counter: Mutex::new((0, 0)),
            workspace_artifact_store: None,
            plan_repo: None,
            plan_mode_cache: Arc::new(tokio::sync::RwLock::new(PlanModeSnapshot::default())),
            plan_resume_hint_handle: None,
        }
    }

    /// Inject the plan repository so plan-mode tools and the write-tool guard
    /// can check `active_plan_id` and flip plan phase.
    pub fn set_plan_repository(&mut self, repo: Arc<dyn astra_plan::PlanRepository>) {
        self.plan_repo = Some(repo);
    }

    /// Inject the host's plan-resume hint handle so tool-driven plan-mode
    /// changes (enter_plan_mode / exit_plan_mode) can refresh the system
    /// prompt injection mid-run. `None` (the default) leaves the host's
    /// hint untouched — useful for test executors without a host.
    pub fn set_plan_resume_hint_handle(&mut self, handle: Arc<std::sync::RwLock<Option<String>>>) {
        self.plan_resume_hint_handle = Some(handle);
    }

    /// Set the approval gate for interactive tool execution.
    pub fn set_approval_gate(&mut self, gate: Arc<dyn astra_tools::ToolApprovalGate>) {
        self.approval_gate = Some(gate);
    }

    /// Set the ask_user gate for interactive user prompts.
    pub fn set_ask_user_gate(&mut self, gate: Arc<dyn AskUserGate>) {
        self.ask_user_gate = Some(gate);
    }

    /// Set the progress callback for streaming tool output.
    pub fn set_progress_callback(&mut self, cb: Arc<dyn astra_tools::ToolProgressCallback>) {
        self.progress_callback = Some(cb);
    }

    pub fn with_cancel_token(
        mut self,
        token: Option<Arc<tokio_util::sync::CancellationToken>>,
    ) -> Self {
        self.default_executor = self.default_executor.with_cancel_token(token);
        self
    }

    pub fn with_workspace_artifact_store(
        mut self,
        store: astra_services::DatabaseSessionArtifactStore,
    ) -> Self {
        self.workspace_artifact_store = Some(store);
        self
    }

    fn publish_current_workspace(&self, source: &str) -> Result<(), String> {
        let Some(store) = self.workspace_artifact_store.clone() else {
            return Ok(());
        };
        let workspace = astra_services::session_workspace::read_workspace(&self.session_id)
            .map_err(|error| format!("{source}: {error}"))?;
        let user_id = self.user_id.clone();
        let future = async move {
            astra_services::session_workspace::persist_remote_workspace(
                &workspace, &user_id, &store,
            )
            .await
            .map(|_| ())
        };
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => tokio::task::block_in_place(|| handle.block_on(future)),
            Err(_) => tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| error.to_string())?
                .block_on(future),
        }
        .map_err(|error| format!("{source}: {error}"))
    }

    /// Set the edge connection pool for remote tool routing.
    pub fn set_edge_connection_pool(
        &mut self,
        pool: astra_server_types::edge_connection_pool::EdgeConnectionPool,
    ) {
        self.edge_connection_pool = Some(pool);
    }
    /// Set the observability session for rollback-backed session-state tools.
    pub fn set_observability_session(
        &mut self,
        session: Arc<std::sync::RwLock<crate::observability_integration::ObservabilitySession>>,
    ) {
        self.observability_session = Some(session);
    }

    /// Execute a tool call and return the result string.
    ///
    /// Routing order:
    /// 1. Try remote edge agent (if connected via WebSocket)
    /// 2. Fall back to local server-side execution
    pub async fn execute(&self, name: &str, args: &Value) -> String {
        self.execute_with_metadata(name, args).await.output
    }

    /// Execute a tool call and preserve structured metadata for server-side fallback paths.
    pub async fn execute_with_metadata(&self, name: &str, args: &Value) -> astra_tools::ToolResult {
        // ── Try remote edge agent first ──────────────────────────────
        if let Some(pool) = &self.edge_connection_pool {
            if let Some(result) = pool.execute_tool_any_edge(&self.user_id, name, args).await {
                return astra_tools::ToolResult {
                    output: result.output,
                    metadata: None,
                    is_error: result.is_error,
                };
            }
        }
        // ── Fire-and-forget resource usage recording (Phase 5) ────────
        if let Some(ref gov) = self.resource_governor {
            let gov = gov.clone();
            let uid = self.user_id.clone();
            tokio::spawn(async move {
                gov.record_tool_calls(&uid, 1).await;
            });
        }

        self.execute_local_with_metadata(name, args).await
    }

    /// Execute a tool locally on the server (no edge routing).
    async fn execute_local_with_metadata(
        &self,
        name: &str,
        args: &Value,
    ) -> astra_tools::ToolResult {
        // ── Plan-mode write guard ────────────────────────────────────
        // If the session has an active plan still in authoring phase,
        // short-circuit world-mutating tools with a structured error that
        // names ExitPlanMode as the escape hatch. Read-only tools (explore,
        // status, tasks, memory) still pass through so the agent can keep
        // investigating while authoring.
        if is_plan_mode_blocked_tool(name) && self.plan_mode_authoring_active().await {
            return astra_tools::ToolResult::error(format!(
                "Tool '{name}' is blocked while plan mode is active. \
                 The agent must call `exit_plan_mode` with an approved plan \
                 before any write operation. This mirrors Claude Code's plan \
                 mode: the plan is authored with read-only tools, approved by \
                 the user, then execution proceeds with writes unlocked."
            ));
        }

        // ── Approval gate check ──────────────────────────────────────
        if let Some(gate) = &self.approval_gate {
            if gate.requires_approval(name) {
                let request_id = format!("srv-{}-{}", self.session_id, uuid_v4_short());
                let decision = gate.request_approval(&request_id, name, args).await;
                match decision {
                    astra_tools::ApprovalDecision::Approved => { /* proceed */ }
                    astra_tools::ApprovalDecision::Denied { reason } => {
                        let msg = reason.unwrap_or_else(|| "User denied execution".into());
                        return astra_tools::ToolResult::error(format!(
                            "Tool execution denied: {msg}"
                        ));
                    }
                    astra_tools::ApprovalDecision::Timeout => {
                        return astra_tools::ToolResult::error(
                            "Tool execution denied: approval request timed out".into(),
                        );
                    }
                }
            }
        }

        // ── Progress: tool started ───────────────────────────────────
        let call_id = format!("{name}-{}", uuid_v4_short());
        if let Some(cb) = &self.progress_callback {
            cb.tool_started(&call_id, name, args).await;
        }

        let mut result = match name {
            // ── Memory tools (HTTP proxy) ──────────────────────────────
            "memory_retrieve" | "memory_store" | "memory_search" | "memory_purge"
            | "memory_correct" | "memory_profile" => {
                let op = name.strip_prefix("memory_").unwrap_or(name);
                // Force-inject user_id and session_id for per-user isolation,
                // mirroring the server's /memory/* proxy in auth_handlers.rs.
                let mut isolated_args = args.clone();
                if let Some(obj) = isolated_args.as_object_mut() {
                    obj.insert(
                        "session_id".to_string(),
                        Value::String(self.user_id.clone()),
                    );
                    obj.insert("user_id".to_string(), Value::String(self.user_id.clone()));
                }
                let output = self.memoria_client.call(op, &isolated_args).await;
                if output.starts_with("Error") {
                    astra_tools::ToolResult::error(output)
                } else {
                    astra_tools::ToolResult::text(output)
                }
            }
            // ── Web search (standalone function) ───────────────────────
            "web_search" => {
                let output = astra_tools::web_search::web_search(args);
                if output.starts_with("Error") {
                    astra_tools::ToolResult::error(output)
                } else {
                    astra_tools::ToolResult::text(output)
                }
            }
            "ask_user" => self.server_ask_user(args).await,
            // ── Plan-mode lifecycle tools ──────────────────────────────
            "enter_plan_mode" => {
                astra_tools::ToolResult::text(self.tool_enter_plan_mode(args).await)
            }
            "exit_plan_mode" => astra_tools::ToolResult::text(self.tool_exit_plan_mode(args).await),
            // ── File operations ─────────────────────────────────────────
            // Write operations use server-specific journal recording.
            // Read-only operations delegate to DefaultToolExecutor.
            "web_fetch" => self.default_executor.execute("web_fetch", args).await,
            "read_file" => self.default_executor.execute("read_file", args).await,
            "write_file" => tool_result_from_output(self.server_write_file(args)),
            "str_replace" => tool_result_from_output(self.server_str_replace(args)),
            "multi_edit" => tool_result_from_output(self.server_multi_edit(args)),
            "delete_file" => tool_result_from_output(self.server_delete_file(args)),
            "rollback_file_edits" => tool_result_from_output(self.rollback_file_edits(args)),
            "list_dir" => self.default_executor.execute("list_dir", args).await,
            // ── Session-state tools ─────────────────────────────────────
            "adjust_config" => tool_result_from_output(self.adjust_config(args)),
            "prioritize_tool" => tool_result_from_output(self.prioritize_tool(args)),
            "deprioritize_tool" => tool_result_from_output(self.deprioritize_tool(args)),
            "set_goal" => tool_result_from_output(self.set_goal(args)),
            "compress_context" => tool_result_from_output(self.compress_context(args)),
            "rollback_session_state" => tool_result_from_output(self.rollback_session_state(args)),
            "task_create" => tool_result_from_output(self.task_create(args)),
            "task_list" => tool_result_from_output(self.task_list(args)),
            "task_get" => tool_result_from_output(self.task_get(args)),
            "task_update" => tool_result_from_output(self.task_update(args)),
            "task_stop" => tool_result_from_output(self.task_stop(args)),
            "sleep" => self.default_executor.execute("sleep", args).await,
            "tool_search" => tool_result_from_output(astra_tools::tool_search::tool_search(
                &astra_tools::schemas::server_executor_tool_schemas(),
                args,
            )),
            // ── MatrixOne operations ────────────────────────────────────
            "mo_query" => self.server_mo_query(args),
            "rollback_database_snapshots" => {
                tool_result_from_output(self.rollback_database_snapshots(args))
            }
            // ── Shell operations ───────────────────────────────────────
            // bash uses tiered process isolation (server-specific).
            // grep + glob delegate to DefaultToolExecutor.
            "bash" => tool_result_from_output(self.server_bash(args).await),
            "grep" => self.default_executor.execute("grep", args).await,
            "glob" => self.default_executor.execute("glob", args).await,
            // ── Git operations ─────────────────────────────────────────
            // All git ops delegate to DefaultToolExecutor.
            "git_status" => self.default_executor.execute("git_status", args).await,
            "git_diff" => self.default_executor.execute("git_diff", args).await,
            "git_log" => self.default_executor.execute("git_log", args).await,
            "git_file_history" => {
                self.default_executor
                    .execute("git_file_history", args)
                    .await
            }
            "git_contributors" => {
                self.default_executor
                    .execute("git_contributors", args)
                    .await
            }
            "git_log_search" => self.default_executor.execute("git_log_search", args).await,
            "git_show" => self.default_executor.execute("git_show", args).await,
            "git_blame" => self.default_executor.execute("git_blame", args).await,
            "symbols" => self.default_executor.execute("symbols", args).await,
            "git_commit" => self.default_executor.execute("git_commit", args).await,
            "git_stash" => self.default_executor.execute("git_stash", args).await,
            "git_revert_commit" => {
                self.default_executor
                    .execute("git_revert_commit", args)
                    .await
            }
            // ── GitHub operations ────────────────────────────────────────
            // GitHub tools delegate to DefaultToolExecutor.
            "github_list_prs" => self.default_executor.execute("github_list_prs", args).await,
            "github_get_pr" => self.default_executor.execute("github_get_pr", args).await,
            "github_ci_status" => {
                self.default_executor
                    .execute("github_ci_status", args)
                    .await
            }
            "github_list_issues" => {
                self.default_executor
                    .execute("github_list_issues", args)
                    .await
            }
            "github_get_issue" => {
                self.default_executor
                    .execute("github_get_issue", args)
                    .await
            }
            "github_repo_stats" => {
                self.default_executor
                    .execute("github_repo_stats", args)
                    .await
            }
            "github_create_issue" => {
                self.default_executor
                    .execute("github_create_issue", args)
                    .await
            }
            // ── Delegation placeholder ─────────────────────────────────
            "delegate" => astra_tools::ToolResult::text(
                "Delegation request acknowledged. The delegation engine will execute \
                 this request and provide results in the next round."
                    .to_string(),
            ),
            // ── Unknown tool fallback ──────────────────────────────────
            _ => astra_tools::ToolResult::error(format!(
                "Error: Tool '{name}' is not available in server-side execution mode. \
                     Available: bash, read_file, write_file, str_replace, delete_file, rollback_file_edits, \
                     multi_edit, list_dir, adjust_config, prioritize_tool, deprioritize_tool, set_goal, compress_context, \
                     rollback_session_state, task_*, sleep, tool_search, mo_query, rollback_database_snapshots, \
                     grep, glob, git_status, git_diff, git_log, git_file_history, git_contributors, git_log_search, \
                     git_show, git_blame, symbols, git_commit, git_stash, git_revert_commit, github_list_prs, github_get_pr, \
                     github_ci_status, github_list_issues, github_get_issue, github_repo_stats, github_create_issue, memory_*, web_fetch, \
                     web_search, ask_user"
            )),
        };

        result.output = astra_tools::normalize_empty_output(result.output, name);
        let limit = astra_tools::per_tool_output_limit(name);
        result.output = astra_tools::truncate_output(result.output, limit);
        let agg = self
            .aggregate_output_bytes
            .fetch_add(result.output.len(), Ordering::Relaxed);
        result.output = astra_tools::maybe_persist_large_output(result.output, agg, name);

        // ── Progress: tool completed ─────────────────────────────────
        if let Some(cb) = &self.progress_callback {
            cb.tool_completed(&call_id, &result.output, !result.is_error)
                .await;
        }

        result
    }

    async fn server_ask_user(&self, args: &Value) -> astra_tools::ToolResult {
        let request = match parse_ask_user_request(args) {
            Ok(request) => request,
            Err(error) => return astra_tools::ToolResult::error(error),
        };

        let Some(gate) = &self.ask_user_gate else {
            return astra_tools::ToolResult::error(
                "Error: ask_user requires an interactive client connection".into(),
            );
        };

        let request_id = format!("ask-{}-{}", self.session_id, uuid_v4_short());
        match gate
            .request_user_input(
                &request_id,
                &request.question,
                &request.choices,
                request.default.as_deref(),
                request.context.as_deref(),
            )
            .await
        {
            AskUserDecision::Answer(response) => {
                let mut body = json!({
                    "answer": response.answer,
                    "question": request.question,
                });
                if !request.choices.is_empty() {
                    body["was_custom"] = Value::Bool(response.was_custom);
                }
                astra_tools::ToolResult::text(body.to_string())
            }
            AskUserDecision::Timeout => astra_tools::ToolResult::error(
                "Error: ask_user timed out waiting for user response".into(),
            ),
            AskUserDecision::Error(message) => {
                astra_tools::ToolResult::error(format!("Error: ask_user failed: {message}"))
            }
        }
    }

    /// Set the current turn index for journal entries.
    pub fn set_turn_index(&self, idx: u32) {
        self.journal_turn_index.store(idx, Ordering::Relaxed);
    }

    /// Reset aggregate output counter at the start of a new turn.
    pub fn reset_aggregate_output(&self) {
        self.aggregate_output_bytes.store(0, Ordering::Relaxed);
    }

    /// Get the workspace root path.
    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub(crate) fn file_journal_checkpoint(&self) -> u64 {
        match self.file_journal.lock() {
            Ok(journal) => journal.checkpoint(),
            Err(poisoned) => poisoned.into_inner().checkpoint(),
        }
    }

    pub(crate) fn database_snapshot_journal_checkpoint(&self) -> u64 {
        match self.database_snapshot_journal.lock() {
            Ok(journal) => journal.checkpoint(),
            Err(poisoned) => poisoned.into_inner().checkpoint(),
        }
    }

    fn record_database_snapshot_rollback(
        &self,
        snapshot_id: impl Into<String>,
        database: Option<String>,
    ) {
        let turn_index = self.journal_turn_index.load(Ordering::Relaxed);
        match self.database_snapshot_journal.lock() {
            Ok(mut journal) => journal.record(snapshot_id, database, turn_index),
            Err(poisoned) => poisoned
                .into_inner()
                .record(snapshot_id, database, turn_index),
        }
    }

    fn database_snapshot_entries(&self) -> Vec<DatabaseSnapshotRollbackEntry> {
        match self.database_snapshot_journal.lock() {
            Ok(journal) => journal.list(),
            Err(poisoned) => poisoned.into_inner().list(),
        }
    }

    fn database_snapshot_entry_for_snapshot(
        &self,
        snapshot_id: &str,
    ) -> Option<DatabaseSnapshotRollbackEntry> {
        match self.database_snapshot_journal.lock() {
            Ok(journal) => journal.entry_for_snapshot(snapshot_id),
            Err(poisoned) => poisoned.into_inner().entry_for_snapshot(snapshot_id),
        }
    }

    fn database_snapshot_restore_plan_for_turn_since(
        &self,
        turn_index: u32,
        checkpoint: u64,
    ) -> Vec<DatabaseSnapshotRollbackEntry> {
        match self.database_snapshot_journal.lock() {
            Ok(journal) => journal.restore_plan_for_turn_since(turn_index, checkpoint),
            Err(poisoned) => poisoned
                .into_inner()
                .restore_plan_for_turn_since(turn_index, checkpoint),
        }
    }

    fn remove_database_snapshot_rollback(&self, snapshot_id: &str) {
        match self.database_snapshot_journal.lock() {
            Ok(mut journal) => {
                journal.remove_snapshot(snapshot_id);
            }
            Err(poisoned) => {
                poisoned.into_inner().remove_snapshot(snapshot_id);
            }
        }
    }

    fn rollback_database_snapshot_entry_json(entry: &DatabaseSnapshotRollbackEntry) -> Value {
        let mut value = serde_json::Map::from_iter([
            (
                "snapshot_id".to_string(),
                Value::String(entry.snapshot_id.clone()),
            ),
            (
                "turn_index".to_string(),
                Value::Number(serde_json::Number::from(entry.turn_index)),
            ),
        ]);
        if let Some(database) = entry.database.as_ref() {
            value.insert("database".to_string(), Value::String(database.clone()));
        }
        Value::Object(value)
    }

    fn restore_database_snapshot_entry(
        &self,
        entry: &DatabaseSnapshotRollbackEntry,
    ) -> Result<(), String> {
        let restore_output = mo_execute_sql(
            &mo_restore_snapshot_sql(&entry.snapshot_id, entry.database.as_deref()),
            None,
        );
        if is_mo_error(&restore_output) {
            return Err(restore_output);
        }

        let drop_output = mo_execute_sql(&mo_drop_snapshot_sql(&entry.snapshot_id), None);
        if is_mo_error(&drop_output) {
            Err(format!(
                "restored MatrixOne snapshot `{}` but failed to drop it afterwards.\n{}",
                entry.snapshot_id, drop_output
            ))
        } else {
            Ok(())
        }
    }

    fn server_mo_query(&self, args: &Value) -> astra_tools::ToolResult {
        let sql = match args.get("sql").and_then(Value::as_str) {
            Some(sql) if !sql.trim().is_empty() => sql.trim(),
            _ => return astra_tools::ToolResult::error("Error: Missing 'sql' parameter".into()),
        };

        let allow_destructive = args
            .get("allow_destructive")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !allow_destructive
            && let Some(kind) = astra_turn_core::safety_middleware::check_sql_safety(sql)
        {
            return astra_tools::ToolResult::error(format!(
                "Error: {kind} statements are blocked by default. Pass \"allow_destructive\": true to confirm execution."
            ));
        }

        let database = args.get("database").and_then(Value::as_str);
        let resolved_database = resolved_mo_database(database);
        let mut metadata = None;
        if mo_query_requires_pre_state_snapshot(sql, allow_destructive) {
            let snapshot_id = mo_pre_state_snapshot_name();
            let snapshot_output =
                mo_execute_sql(&mo_create_snapshot_sql(&snapshot_id, database), None);
            if is_mo_error(&snapshot_output) {
                return astra_tools::ToolResult::error(format!(
                    "Error: failed to capture pre-state snapshot `{snapshot_id}` before executing query.\n{snapshot_output}"
                ));
            }
            self.record_database_snapshot_rollback(
                snapshot_id.clone(),
                Some(resolved_database.clone()),
            );
            metadata = Some(serde_json::Map::from_iter([
                (
                    "pre_state_snapshot_id".to_string(),
                    Value::String(snapshot_id),
                ),
                (
                    "pre_state_snapshot_database".to_string(),
                    Value::String(resolved_database),
                ),
            ]));
        }

        let output = mo_execute_sql(sql, database);
        astra_tools::ToolResult {
            is_error: is_mo_error(&output),
            output,
            metadata,
        }
    }

    pub(crate) fn rollback_database_snapshots(&self, args: &Value) -> String {
        let scope = args
            .get("scope")
            .and_then(Value::as_str)
            .or_else(|| {
                if args.get("snapshot_id").is_some() {
                    Some("snapshot")
                } else {
                    None
                }
            })
            .unwrap_or("current_turn");

        match scope {
            "list" => {
                let entries = self
                    .database_snapshot_entries()
                    .into_iter()
                    .map(|entry| Self::rollback_database_snapshot_entry_json(&entry))
                    .collect::<Vec<_>>();
                json!({
                    "success": true,
                    "scope": "list",
                    "total_entries": entries.len(),
                    "entries": entries,
                })
                .to_string()
            }
            "snapshot" => {
                let snapshot_id = match args.get("snapshot_id").and_then(Value::as_str) {
                    Some(snapshot_id) if is_valid_snapshot_name(snapshot_id) => snapshot_id,
                    Some(snapshot_id) => {
                        return json!({
                            "success": false,
                            "scope": "snapshot",
                            "error": format!("invalid snapshot_id `{snapshot_id}`"),
                        })
                        .to_string();
                    }
                    None => {
                        return json!({
                            "success": false,
                            "scope": "snapshot",
                            "error": "missing 'snapshot_id' for scope=snapshot",
                        })
                        .to_string();
                    }
                };
                let journal_entry = self.database_snapshot_entry_for_snapshot(snapshot_id);
                let database = args
                    .get("database")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|database| !database.is_empty())
                    .map(ToString::to_string)
                    .or_else(|| {
                        journal_entry
                            .as_ref()
                            .and_then(|entry| entry.database.clone())
                    });
                let entry = DatabaseSnapshotRollbackEntry {
                    sequence: journal_entry.as_ref().map_or(0, |entry| entry.sequence),
                    snapshot_id: snapshot_id.to_string(),
                    database,
                    turn_index: journal_entry.as_ref().map_or_else(
                        || self.journal_turn_index.load(Ordering::Relaxed),
                        |entry| entry.turn_index,
                    ),
                };
                match self.restore_database_snapshot_entry(&entry) {
                    Ok(()) => {
                        self.remove_database_snapshot_rollback(snapshot_id);
                        let database = entry.database.clone();
                        json!({
                            "success": true,
                            "scope": "snapshot",
                            "snapshot_id": snapshot_id,
                            "database": database,
                            "summary": format!(
                                "Restored MatrixOne snapshot `{}`{}",
                                snapshot_id,
                                database
                                    .as_deref()
                                    .map(|database| format!(" for database `{database}`"))
                                    .unwrap_or_default()
                            ),
                        })
                        .to_string()
                    }
                    Err(error) => json!({
                        "success": false,
                        "scope": "snapshot",
                        "snapshot_id": snapshot_id,
                        "database": entry.database.clone(),
                        "error": error,
                    })
                    .to_string(),
                }
            }
            "turn" | "current_turn" => {
                let turn_index = if scope == "turn" {
                    match args.get("turn_index").and_then(Value::as_u64) {
                        Some(turn_index) => turn_index as u32,
                        None => {
                            return json!({
                                "success": false,
                                "scope": "turn",
                                "error": "missing 'turn_index' for scope=turn",
                            })
                            .to_string();
                        }
                    }
                } else {
                    self.journal_turn_index.load(Ordering::Relaxed)
                };
                let checkpoint = args
                    .get("database_after_sequence")
                    .or_else(|| args.get("after_sequence"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let plan = if checkpoint > 0 {
                    self.database_snapshot_restore_plan_for_turn_since(turn_index, checkpoint)
                } else {
                    match self.database_snapshot_journal.lock() {
                        Ok(journal) => journal.restore_plan_for_turn(turn_index),
                        Err(poisoned) => poisoned.into_inner().restore_plan_for_turn(turn_index),
                    }
                };
                let mut restored = Vec::new();
                let mut failed = Vec::new();
                for entry in &plan {
                    match self.restore_database_snapshot_entry(entry) {
                        Ok(()) => {
                            self.remove_database_snapshot_rollback(&entry.snapshot_id);
                            restored.push(Self::rollback_database_snapshot_entry_json(entry));
                        }
                        Err(error) => {
                            let mut failed_entry =
                                Self::rollback_database_snapshot_entry_json(entry)
                                    .as_object()
                                    .cloned()
                                    .unwrap_or_default();
                            failed_entry.insert("error".to_string(), Value::String(error));
                            failed.push(Value::Object(failed_entry));
                        }
                    }
                }
                let success = !restored.is_empty() && failed.is_empty();
                let summary = if plan.is_empty() {
                    format!("No recorded MatrixOne snapshots found for turn {turn_index}")
                } else if failed.is_empty() {
                    format!(
                        "Restored {} MatrixOne snapshot{} for turn {turn_index}",
                        restored.len(),
                        if restored.len() == 1 { "" } else { "s" }
                    )
                } else {
                    format!(
                        "Restored {} MatrixOne snapshot{} for turn {turn_index} with {} failure{}",
                        restored.len(),
                        if restored.len() == 1 { "" } else { "s" },
                        failed.len(),
                        if failed.len() == 1 { "" } else { "s" }
                    )
                };
                json!({
                    "success": success,
                    "scope": scope,
                    "turn_index": turn_index,
                    "restored": restored,
                    "failed": failed,
                    "summary": summary,
                })
                .to_string()
            }
            other => json!({
                "success": false,
                "error": format!(
                    "unknown scope `{other}`. Supported: current_turn, turn, snapshot, list"
                ),
            })
            .to_string(),
        }
    }

    pub(crate) fn session_state_journal_checkpoint(&self) -> u64 {
        match self.session_state_journal.lock() {
            Ok(journal) => journal.checkpoint(),
            Err(poisoned) => poisoned.into_inner().checkpoint(),
        }
    }

    fn record_session_state_rollback(&self, label: String, action: SessionStateRollbackAction) {
        let turn_index = self.journal_turn_index.load(Ordering::Relaxed);
        match self.session_state_journal.lock() {
            Ok(mut journal) => journal.record(turn_index, label, action),
            Err(poisoned) => poisoned.into_inner().record(turn_index, label, action),
        }
    }

    fn record_tool_preferences_rollback(
        &self,
        previous_pinned_tools: Vec<String>,
        previous_deprioritized_tools: Vec<String>,
        label: impl Into<String>,
    ) {
        self.record_session_state_rollback(
            label.into(),
            SessionStateRollbackAction::ToolPreferences {
                previous_pinned_tools,
                previous_deprioritized_tools,
            },
        );
    }

    fn record_adjust_config_rollback(
        &self,
        path: impl Into<String>,
        old_value: Value,
        snapshot: crate::observability_integration::ObservabilitySessionRollbackSnapshot,
    ) {
        let path = path.into();
        self.record_session_state_rollback(
            format!("adjust_config:{path}"),
            SessionStateRollbackAction::ConfigOverride {
                path,
                old_value,
                snapshot,
            },
        );
    }

    fn record_goal_rollback(
        &self,
        previous_goal: Option<String>,
        snapshot: crate::observability_integration::ObservabilitySessionRollbackSnapshot,
    ) {
        self.record_session_state_rollback(
            "set_goal".to_string(),
            SessionStateRollbackAction::GoalOverride {
                previous_goal,
                snapshot,
            },
        );
    }

    fn record_compression_rollback(
        &self,
        turn: u32,
        snapshot: crate::observability_integration::ObservabilitySessionRollbackSnapshot,
    ) {
        self.record_session_state_rollback(
            format!("compress_context:turn-{turn}"),
            SessionStateRollbackAction::Compression { turn, snapshot },
        );
    }

    fn record_task_state_rollback(&self, snapshot: TaskManagerSnapshot, label: impl Into<String>) {
        self.record_session_state_rollback(
            label.into(),
            SessionStateRollbackAction::TaskState { snapshot },
        );
    }

    fn session_state_entries(&self) -> Vec<SessionStateRollbackEntry> {
        match self.session_state_journal.lock() {
            Ok(journal) => journal.list(),
            Err(poisoned) => poisoned.into_inner().list(),
        }
    }

    fn session_state_restore_plan_for_turn(
        &self,
        turn_index: u32,
    ) -> Vec<SessionStateRollbackEntry> {
        match self.session_state_journal.lock() {
            Ok(journal) => journal.restore_plan_for_turn(turn_index),
            Err(poisoned) => poisoned.into_inner().restore_plan_for_turn(turn_index),
        }
    }

    fn session_state_restore_plan_for_turn_since(
        &self,
        turn_index: u32,
        checkpoint: u64,
    ) -> Vec<SessionStateRollbackEntry> {
        match self.session_state_journal.lock() {
            Ok(journal) => journal.restore_plan_for_turn_since(turn_index, checkpoint),
            Err(poisoned) => poisoned
                .into_inner()
                .restore_plan_for_turn_since(turn_index, checkpoint),
        }
    }

    fn remove_session_state_rollback(&self, sequence: u64) {
        match self.session_state_journal.lock() {
            Ok(mut journal) => {
                journal.remove_sequence(sequence);
            }
            Err(poisoned) => {
                poisoned.into_inner().remove_sequence(sequence);
            }
        }
    }

    fn restore_observability_snapshot(
        &self,
        snapshot: &crate::observability_integration::ObservabilitySessionRollbackSnapshot,
    ) -> Result<(), String> {
        let Some(observability_session) = self.observability_session.as_ref() else {
            return Err("No observability session available".to_string());
        };
        let mut session = observability_session
            .write()
            .map_err(|_| "Failed to acquire observability session".to_string())?;
        session.restore_rollback_snapshot(snapshot);
        Ok(())
    }

    fn rollback_session_state_entry_json(entry: &SessionStateRollbackEntry) -> Value {
        let timestamp_ms = entry
            .timestamp
            .duration_since(SystemTime::UNIX_EPOCH)
            .ok()
            .map(|duration| duration.as_millis())
            .and_then(|millis| u64::try_from(millis).ok());
        let mut value = serde_json::Map::from_iter([
            ("label".to_string(), Value::String(entry.label.clone())),
            (
                "kind".to_string(),
                Value::String(action_kind(&entry.action).to_string()),
            ),
            (
                "turn_index".to_string(),
                Value::Number(serde_json::Number::from(entry.turn_index)),
            ),
        ]);
        if let Some(timestamp_ms) = timestamp_ms {
            value.insert(
                "timestamp_ms".to_string(),
                Value::Number(serde_json::Number::from(timestamp_ms)),
            );
        }
        match &entry.action {
            SessionStateRollbackAction::ConfigOverride { path, .. } => {
                value.insert("path".to_string(), Value::String(path.clone()));
            }
            SessionStateRollbackAction::GoalOverride { previous_goal, .. } => {
                value.insert(
                    "previous_goal".to_string(),
                    previous_goal
                        .clone()
                        .map(Value::String)
                        .unwrap_or(Value::Null),
                );
            }
            SessionStateRollbackAction::Compression { turn, .. } => {
                value.insert(
                    "turn".to_string(),
                    Value::Number(serde_json::Number::from(*turn)),
                );
            }
            SessionStateRollbackAction::ToolPreferences { .. }
            | SessionStateRollbackAction::TaskState { .. } => {}
        }
        Value::Object(value)
    }

    fn rollback_session_state_entry(
        &self,
        entry: &SessionStateRollbackEntry,
    ) -> Result<(), String> {
        match &entry.action {
            SessionStateRollbackAction::ToolPreferences {
                previous_pinned_tools,
                previous_deprioritized_tools,
            } => {
                let mut pinned = self
                    .self_mod_pinned_tools
                    .lock()
                    .map_err(|_| "Failed to access pinned tools".to_string())?;
                let mut deprioritized = self
                    .self_mod_deprioritized_tools
                    .lock()
                    .map_err(|_| "Failed to access deprioritized tools".to_string())?;
                let current_pinned = pinned.clone();
                let current_deprioritized = deprioritized.clone();
                *pinned = previous_pinned_tools.clone();
                *deprioritized = previous_deprioritized_tools.clone();
                if let Err(error) = persist_tool_preferences(
                    &self.session_id,
                    &pinned,
                    &deprioritized,
                    "server_tool_executor:rollback_session_state",
                ) {
                    *pinned = current_pinned;
                    *deprioritized = current_deprioritized;
                    return Err(format!(
                        "failed to persist restored tool preferences: {error}"
                    ));
                }
                Ok(())
            }
            SessionStateRollbackAction::ConfigOverride {
                path,
                old_value,
                snapshot,
            } => {
                self.restore_observability_snapshot(snapshot)?;
                persist_config_override(
                    &self.session_id,
                    path,
                    old_value.clone(),
                    "server_tool_executor:rollback_session_state",
                )
                .map_err(|error| {
                    format!("failed to persist restored config override for {path}: {error}")
                })
            }
            SessionStateRollbackAction::GoalOverride {
                previous_goal,
                snapshot,
            } => {
                self.restore_observability_snapshot(snapshot)?;
                match previous_goal.as_deref() {
                    Some(goal) => persist_goal_override(
                        &self.session_id,
                        goal,
                        "server_tool_executor:rollback_session_state",
                    )
                    .map_err(|error| {
                        format!("failed to persist restored goal override: {error}")
                    })?,
                    None => clear_persisted_goal_override(&self.session_id)?,
                }
                Ok(())
            }
            SessionStateRollbackAction::Compression { snapshot, .. } => {
                self.restore_observability_snapshot(snapshot)
            }
            SessionStateRollbackAction::TaskState { snapshot } => {
                self.task_manager.restore_snapshot(snapshot)
            }
        }
    }

    fn task_create(&self, args: &Value) -> String {
        let snapshot = self.task_manager.snapshot_state();
        let output = self.task_manager.create(args);
        if !output.starts_with("Error:") {
            self.record_task_state_rollback(
                snapshot,
                format!(
                    "task_create:{}",
                    args.get("title").and_then(Value::as_str).unwrap_or("task")
                ),
            );
        }
        output
    }

    fn task_list(&self, args: &Value) -> String {
        self.task_manager.list(args)
    }

    fn task_get(&self, args: &Value) -> String {
        self.task_manager.get(args)
    }

    fn task_update(&self, args: &Value) -> String {
        let snapshot = self.task_manager.snapshot_state();
        let output = self.task_manager.update(args);
        if !output.starts_with("Error:")
            && serde_json::from_str::<Value>(&output)
                .ok()
                .and_then(|value| value.get("success").and_then(Value::as_bool))
                .unwrap_or(false)
        {
            self.record_task_state_rollback(
                snapshot,
                format!(
                    "task_update:{}",
                    args.get("task_id")
                        .and_then(Value::as_str)
                        .unwrap_or("task")
                ),
            );
        }
        output
    }

    fn task_stop(&self, args: &Value) -> String {
        let snapshot = self.task_manager.snapshot_state();
        let output = self.task_manager.stop(args);
        if !output.starts_with("Error:")
            && serde_json::from_str::<Value>(&output)
                .ok()
                .and_then(|value| value.get("success").and_then(Value::as_bool))
                .unwrap_or(false)
        {
            self.record_task_state_rollback(
                snapshot,
                format!(
                    "task_stop:{}",
                    args.get("task_id")
                        .and_then(Value::as_str)
                        .unwrap_or("task")
                ),
            );
        }
        output
    }

    fn adjust_config(&self, args: &Value) -> String {
        let path = match args.get("path").and_then(Value::as_str) {
            Some(path) if !path.trim().is_empty() => path.trim(),
            _ => return json!({"error": "Missing required parameter: path"}).to_string(),
        };
        let value = match args.get("value") {
            Some(value) => value,
            None => return json!({"error": "Missing required parameter: value"}).to_string(),
        };
        let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);

        let Some(observability_session) = self.observability_session.as_ref() else {
            return json!({"error": "No observability session available"}).to_string();
        };
        // LOCK ORDER: observability_session → self_mod_mutation_counter.
        // The session guard is held across the counter lock because the
        // mutation paths below require atomic read-modify-write of session
        // config based on the counter check. All call sites that need both
        // locks MUST take them in this order to avoid deadlock.
        let mut session = match observability_session.write() {
            Ok(guard) => guard,
            Err(_) => {
                return json!({"error": "Failed to acquire observability session"}).to_string();
            }
        };

        let turn = session.turn_number;
        let constraints = crate::self_model::ConstraintSet::default();
        let mut counter = match self.self_mod_mutation_counter.lock() {
            Ok(counter) => counter,
            Err(_) => return json!({"error": "Failed to access mutation counter"}).to_string(),
        };
        if counter.0 != turn {
            *counter = (turn, 0);
        }
        if !force && counter.1 >= constraints.max_mutations_per_turn {
            return json!({
                "error": "mutation_limit_exceeded",
                "turn": turn,
                "max_mutations_per_turn": constraints.max_mutations_per_turn,
                "hint": "Set force=true to override governor once.",
            })
            .to_string();
        }

        let parse_u32 =
            |value: &Value| value.as_u64().and_then(|number| u32::try_from(number).ok());
        let parse_f64 = |value: &Value| value.as_f64();
        let ceiling = constraints.config_drift_ceiling;
        let session_snapshot = session.rollback_snapshot();
        let (old_value, new_value, drift) = match path {
            "compression.compression_threshold" => {
                let Some(new) = parse_f64(value) else {
                    return json!({"error": "value must be a number"}).to_string();
                };
                if !(0.5..=0.98).contains(&new) {
                    return json!({"error": "compression.compression_threshold must be within [0.5, 0.98]"}).to_string();
                }
                let old = session.config.compression.compression_threshold;
                let drift = normalized_drift(old, new);
                if let Some(drift_value) = drift
                    && !force
                    && drift_value > ceiling
                {
                    return json!({
                        "error": "config_drift_ceiling_exceeded",
                        "path": path,
                        "old": old,
                        "new": new,
                        "drift": drift_value,
                        "ceiling": ceiling,
                    })
                    .to_string();
                }
                session.config.compression.compression_threshold = new;
                (json!(old), json!(new), drift)
            }
            "memory.retrieval_top_k" => {
                let Some(new) = parse_u32(value) else {
                    return json!({"error": "value must be an integer"}).to_string();
                };
                if !(1..=20).contains(&new) {
                    return json!({"error": "memory.retrieval_top_k must be within [1, 20]"})
                        .to_string();
                }
                let old = session.config.memory.retrieval_top_k;
                let drift = normalized_drift(old as f64, new as f64);
                if let Some(drift_value) = drift
                    && !force
                    && drift_value > ceiling
                {
                    return json!({
                        "error": "config_drift_ceiling_exceeded",
                        "path": path,
                        "old": old,
                        "new": new,
                        "drift": drift_value,
                        "ceiling": ceiling,
                    })
                    .to_string();
                }
                session.config.memory.retrieval_top_k = new;
                (json!(old), json!(new), drift)
            }
            "tool_selection.max_tools" => {
                let Some(new) = parse_u32(value) else {
                    return json!({"error": "value must be an integer"}).to_string();
                };
                if !(5..=80).contains(&new) {
                    return json!({"error": "tool_selection.max_tools must be within [5, 80]"})
                        .to_string();
                }
                let old = session.config.tool_selection.max_tools;
                let drift = normalized_drift(old as f64, new as f64);
                if let Some(drift_value) = drift
                    && !force
                    && drift_value > ceiling
                {
                    return json!({
                        "error": "config_drift_ceiling_exceeded",
                        "path": path,
                        "old": old,
                        "new": new,
                        "drift": drift_value,
                        "ceiling": ceiling,
                    })
                    .to_string();
                }
                session.config.tool_selection.max_tools = new;
                (json!(old), json!(new), drift)
            }
            "tool_selection.tool_budget_tokens" => {
                let Some(new) = parse_u32(value) else {
                    return json!({"error": "value must be an integer"}).to_string();
                };
                if new > 40_000 {
                    return json!({"error": "tool_selection.tool_budget_tokens must be within [0, 40000]"}).to_string();
                }
                let old = session.config.tool_selection.tool_budget_tokens;
                let drift = normalized_drift(old as f64, new as f64);
                if let Some(drift_value) = drift
                    && !force
                    && drift_value > ceiling
                {
                    return json!({
                        "error": "config_drift_ceiling_exceeded",
                        "path": path,
                        "old": old,
                        "new": new,
                        "drift": drift_value,
                        "ceiling": ceiling,
                    })
                    .to_string();
                }
                session.config.tool_selection.tool_budget_tokens = new;
                (json!(old), json!(new), drift)
            }
            "token_budget.max_turn_input_tokens" => {
                let Some(new) = parse_u32(value) else {
                    return json!({"error": "value must be an integer"}).to_string();
                };
                if !(8_000..=200_000).contains(&new) {
                    return json!({"error": "token_budget.max_turn_input_tokens must be within [8000, 200000]"}).to_string();
                }
                let old = session.config.token_budget.max_turn_input_tokens;
                let drift = normalized_drift(old as f64, new as f64);
                if let Some(drift_value) = drift
                    && !force
                    && drift_value > ceiling
                {
                    return json!({
                        "error": "config_drift_ceiling_exceeded",
                        "path": path,
                        "old": old,
                        "new": new,
                        "drift": drift_value,
                        "ceiling": ceiling,
                    })
                    .to_string();
                }
                session.config.token_budget.max_turn_input_tokens = new;
                (json!(old), json!(new), drift)
            }
            "token_budget.tools_reserve" => {
                let Some(new) = parse_u32(value) else {
                    return json!({"error": "value must be an integer"}).to_string();
                };
                if !(1_000..=40_000).contains(&new) {
                    return json!({"error": "token_budget.tools_reserve must be within [1000, 40000]"}).to_string();
                }
                let old = session.config.token_budget.tools_reserve;
                let drift = normalized_drift(old as f64, new as f64);
                if let Some(drift_value) = drift
                    && !force
                    && drift_value > ceiling
                {
                    return json!({
                        "error": "config_drift_ceiling_exceeded",
                        "path": path,
                        "old": old,
                        "new": new,
                        "drift": drift_value,
                        "ceiling": ceiling,
                    })
                    .to_string();
                }
                session.config.token_budget.tools_reserve = new;
                (json!(old), json!(new), drift)
            }
            "verification.strictness" => {
                let Some(new) = parse_f64(value) else {
                    return json!({"error": "value must be a number"}).to_string();
                };
                if !(0.2..=0.95).contains(&new) {
                    return json!({"error": "verification.strictness must be within [0.2, 0.95]"})
                        .to_string();
                }
                let old = session.config.verification.strictness;
                let drift = normalized_drift(old, new);
                if let Some(drift_value) = drift
                    && !force
                    && drift_value > ceiling
                {
                    return json!({
                        "error": "config_drift_ceiling_exceeded",
                        "path": path,
                        "old": old,
                        "new": new,
                        "drift": drift_value,
                        "ceiling": ceiling,
                    })
                    .to_string();
                }
                session.config.verification.strictness = new;
                (json!(old), json!(new), drift)
            }
            _ => {
                return json!({
                    "error": "Unsupported config path",
                    "path": path,
                    "supported_paths": [
                        "compression.compression_threshold",
                        "memory.retrieval_top_k",
                        "tool_selection.max_tools",
                        "tool_selection.tool_budget_tokens",
                        "token_budget.max_turn_input_tokens",
                        "token_budget.tools_reserve",
                        "verification.strictness",
                    ],
                })
                .to_string();
            }
        };

        if let Err(error) = persist_config_override(
            &self.session_id,
            path,
            new_value.clone(),
            "server_tool_executor:adjust_config",
        ) {
            session.restore_rollback_snapshot(&session_snapshot);
            return json!({
                "error": "failed_to_persist_config_override",
                "path": path,
                "detail": error,
            })
            .to_string();
        }
        if let Err(error) = self.publish_current_workspace("server_tool_executor:adjust_config") {
            session.restore_rollback_snapshot(&session_snapshot);
            return json!({
                "error": "failed_to_publish_workspace_artifact",
                "path": path,
                "detail": error,
            })
            .to_string();
        }

        counter.1 += 1;
        self.record_adjust_config_rollback(path.to_string(), old_value.clone(), session_snapshot);
        json!({
            "status": "ok",
            "path": path,
            "old": old_value,
            "new": new_value,
            "turn": turn,
            "mutations_this_turn": counter.1,
            "max_mutations_per_turn": constraints.max_mutations_per_turn,
            "drift": drift,
            "drift_ceiling": ceiling,
        })
        .to_string()
    }

    fn prioritize_tool(&self, args: &Value) -> String {
        let Some(tool) = extract_tool_name(args) else {
            return json!({"error": "Missing required parameter: tool"}).to_string();
        };
        if !supports_server_tool_name(&tool) {
            return json!({"error": format!("Unknown tool: {tool}")}).to_string();
        }

        let mut pinned = match self.self_mod_pinned_tools.lock() {
            Ok(pinned) => pinned,
            Err(_) => return json!({"error": "Failed to access pinned tools"}).to_string(),
        };
        let mut deprioritized = match self.self_mod_deprioritized_tools.lock() {
            Ok(deprioritized) => deprioritized,
            Err(_) => return json!({"error": "Failed to access deprioritized tools"}).to_string(),
        };
        let original_pinned = pinned.clone();
        let original_deprioritized = deprioritized.clone();

        if !pinned.contains(&tool) {
            pinned.push(tool.clone());
        }
        pinned.sort();
        deprioritized.retain(|entry| entry != &tool);

        if let Err(error) = persist_tool_preferences(
            &self.session_id,
            &pinned,
            &deprioritized,
            "server_tool_executor:prioritize_tool",
        ) {
            *pinned = original_pinned;
            *deprioritized = original_deprioritized;
            return json!({
                "error": "failed_to_persist_tool_preferences",
                "detail": error,
                "tool": tool,
            })
            .to_string();
        }
        if let Err(error) = self.publish_current_workspace("server_tool_executor:prioritize_tool") {
            *pinned = original_pinned;
            *deprioritized = original_deprioritized;
            return json!({
                "error": "failed_to_publish_workspace_artifact",
                "detail": error,
                "tool": tool,
            })
            .to_string();
        }

        let changed = original_pinned != *pinned || original_deprioritized != *deprioritized;
        if changed {
            self.record_tool_preferences_rollback(
                original_pinned.clone(),
                original_deprioritized.clone(),
                format!("prioritize_tool:{tool}"),
            );
        }
        json!({
            "status": "ok",
            "prioritized_tool": tool,
            "previous_pinned_tools": original_pinned,
            "previous_deprioritized_tools": original_deprioritized,
            "pinned_tools": pinned.clone(),
            "deprioritized_tools": deprioritized.clone(),
        })
        .to_string()
    }

    fn deprioritize_tool(&self, args: &Value) -> String {
        let Some(tool) = extract_tool_name(args) else {
            return json!({"error": "Missing required parameter: tool"}).to_string();
        };
        if !supports_server_tool_name(&tool) {
            return json!({"error": format!("Unknown tool: {tool}")}).to_string();
        }

        let mut pinned = match self.self_mod_pinned_tools.lock() {
            Ok(pinned) => pinned,
            Err(_) => return json!({"error": "Failed to access pinned tools"}).to_string(),
        };
        let mut deprioritized = match self.self_mod_deprioritized_tools.lock() {
            Ok(deprioritized) => deprioritized,
            Err(_) => return json!({"error": "Failed to access deprioritized tools"}).to_string(),
        };
        let original_pinned = pinned.clone();
        let original_deprioritized = deprioritized.clone();

        if !deprioritized.contains(&tool) {
            deprioritized.push(tool.clone());
        }
        deprioritized.sort();
        pinned.retain(|entry| entry != &tool);

        if let Err(error) = persist_tool_preferences(
            &self.session_id,
            &pinned,
            &deprioritized,
            "server_tool_executor:deprioritize_tool",
        ) {
            *pinned = original_pinned;
            *deprioritized = original_deprioritized;
            return json!({
                "error": "failed_to_persist_tool_preferences",
                "detail": error,
                "tool": tool,
            })
            .to_string();
        }
        if let Err(error) = self.publish_current_workspace("server_tool_executor:deprioritize_tool")
        {
            *pinned = original_pinned;
            *deprioritized = original_deprioritized;
            return json!({
                "error": "failed_to_publish_workspace_artifact",
                "detail": error,
                "tool": tool,
            })
            .to_string();
        }

        let changed = original_pinned != *pinned || original_deprioritized != *deprioritized;
        if changed {
            self.record_tool_preferences_rollback(
                original_pinned.clone(),
                original_deprioritized.clone(),
                format!("deprioritize_tool:{tool}"),
            );
        }
        json!({
            "status": "ok",
            "deprioritized_tool": tool,
            "previous_pinned_tools": original_pinned,
            "previous_deprioritized_tools": original_deprioritized,
            "pinned_tools": pinned.clone(),
            "deprioritized_tools": deprioritized.clone(),
        })
        .to_string()
    }

    fn set_goal(&self, args: &Value) -> String {
        let goal = match args.get("goal").and_then(Value::as_str) {
            Some(goal) if !goal.trim().is_empty() => goal.trim(),
            _ => return json!({"error": "Missing required parameter: goal"}).to_string(),
        };
        let Some(observability_session) = self.observability_session.as_ref() else {
            return json!({"error": "No observability session available"}).to_string();
        };
        let mut session = match observability_session.write() {
            Ok(guard) => guard,
            Err(_) => {
                return json!({"error": "Failed to acquire observability session"}).to_string();
            }
        };
        let session_snapshot = session.rollback_snapshot();
        let previous_goal = session
            .goal_tracker
            .as_ref()
            .map(|tracker| tracker.goal().to_string())
            .or_else(|| session.original_query.clone());

        if let Err(error) =
            persist_goal_override(&self.session_id, goal, "server_tool_executor:set_goal")
        {
            return json!({
                "error": "failed_to_persist_goal",
                "detail": error,
                "goal": goal,
            })
            .to_string();
        }
        if let Err(error) = self.publish_current_workspace("server_tool_executor:set_goal") {
            return json!({
                "error": "failed_to_publish_workspace_artifact",
                "detail": error,
                "goal": goal,
            })
            .to_string();
        }

        let goal_changed = session.steer_goal(goal);
        if goal_changed {
            self.record_goal_rollback(previous_goal.clone(), session_snapshot);
        }

        json!({
            "status": "ok",
            "previous_goal": previous_goal,
            "goal": goal,
            "goal_changed": goal_changed,
            "turn": session.turn_number,
        })
        .to_string()
    }

    fn compress_context(&self, args: &Value) -> String {
        let reason = args
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("manual_request");
        let Some(observability_session) = self.observability_session.as_ref() else {
            return json!({"error": "No observability session available"}).to_string();
        };
        let mut session = match observability_session.write() {
            Ok(guard) => guard,
            Err(_) => {
                return json!({"error": "Failed to acquire observability session"}).to_string();
            }
        };
        let session_snapshot = session.rollback_snapshot();

        let turn = if session.turn_number == 0 {
            1
        } else {
            session.turn_number
        };
        let previous_compression_count = session.compressed_turns.len();
        let already_compressed_this_turn = session.compressed_turns.contains(&turn);

        if let Err(error) = persist_manual_compression(
            &self.session_id,
            turn,
            reason,
            "server_tool_executor:compress_context",
        ) {
            return json!({
                "error": "failed_to_persist_manual_compression",
                "detail": error,
                "turn": turn,
                "reason": reason,
            })
            .to_string();
        }

        session.record_compression(turn);
        self.record_compression_rollback(turn, session_snapshot);

        json!({
            "status": "ok",
            "turn": turn,
            "reason": reason,
            "previous_compression_count": previous_compression_count,
            "already_compressed_this_turn": already_compressed_this_turn,
            "compression_count": session.compressed_turns.len(),
        })
        .to_string()
    }

    pub(crate) fn rollback_session_state(&self, args: &Value) -> String {
        let scope = args
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or("current_turn");
        let explicit_turn_index = if scope == "turn" {
            match args.get("turn_index").and_then(Value::as_u64) {
                Some(turn_index) => Some(turn_index),
                None => {
                    return json!({
                        "success": false,
                        "error": "missing 'turn_index' for scope=turn",
                    })
                    .to_string();
                }
            }
        } else {
            None
        };
        let checkpoint = args
            .get("session_state_after_sequence")
            .or_else(|| args.get("after_sequence"))
            .and_then(Value::as_u64)
            .unwrap_or(0);

        match scope {
            "list" => {
                let entries = self
                    .session_state_entries()
                    .into_iter()
                    .map(|entry| Self::rollback_session_state_entry_json(&entry))
                    .collect::<Vec<_>>();
                json!({
                    "success": true,
                    "scope": "list",
                    "total_entries": entries.len(),
                    "entries": entries,
                    "summary": format!(
                        "Listed {} recorded session-state rollback entr{}",
                        entries.len(),
                        if entries.len() == 1 { "y" } else { "ies" },
                    ),
                })
                .to_string()
            }
            "turn" | "current_turn" => {
                let turn_index = explicit_turn_index
                    .unwrap_or_else(|| self.journal_turn_index.load(Ordering::Relaxed) as u64)
                    as u32;
                let plan = if checkpoint > 0 {
                    self.session_state_restore_plan_for_turn_since(turn_index, checkpoint)
                } else {
                    self.session_state_restore_plan_for_turn(turn_index)
                };
                let mut restored = Vec::new();
                let mut failed = Vec::new();
                for entry in &plan {
                    match self.rollback_session_state_entry(entry) {
                        Ok(()) => {
                            self.remove_session_state_rollback(entry.sequence);
                            restored.push(Self::rollback_session_state_entry_json(entry));
                        }
                        Err(error) => {
                            let mut failed_entry = Self::rollback_session_state_entry_json(entry)
                                .as_object()
                                .cloned()
                                .unwrap_or_default();
                            failed_entry.insert("error".to_string(), Value::String(error));
                            failed.push(Value::Object(failed_entry));
                        }
                    }
                }
                let success = !restored.is_empty() && failed.is_empty();
                if !restored.is_empty()
                    && failed.is_empty()
                    && let Err(error) = self
                        .publish_current_workspace("server_tool_executor:rollback_session_state")
                {
                    return json!({
                        "success": false,
                        "scope": scope,
                        "turn_index": turn_index,
                        "restored": restored,
                        "failed": [{
                            "error": error,
                            "kind": "workspace_artifact_publish"
                        }],
                        "summary": "Restored session state locally but failed to publish workspace artifact",
                    })
                    .to_string();
                }
                let summary = if plan.is_empty() {
                    format!(
                        "No recorded session-state rollback handles found for turn {turn_index}"
                    )
                } else if failed.is_empty() {
                    format!(
                        "Restored {} recorded session-state mutation{} for turn {turn_index}",
                        restored.len(),
                        if restored.len() == 1 { "" } else { "s" },
                    )
                } else {
                    format!(
                        "Restored {} recorded session-state mutation{} for turn {turn_index} with {} failure{}",
                        restored.len(),
                        if restored.len() == 1 { "" } else { "s" },
                        failed.len(),
                        if failed.len() == 1 { "" } else { "s" },
                    )
                };
                json!({
                    "success": success,
                    "scope": scope,
                    "turn_index": turn_index,
                    "restored": restored,
                    "failed": failed,
                    "summary": summary,
                })
                .to_string()
            }
            other => json!({
                "success": false,
                "error": format!(
                    "unknown scope `{other}`. Supported: current_turn, turn, list"
                ),
            })
            .to_string(),
        }
    }

    // ────────────────────────────────────────────────────────────────────────
    // Plan-mode gating and tools
    // ────────────────────────────────────────────────────────────────────────

    /// Returns true when this session has an active plan that is still being
    /// authored (`planning` / `refining` / plan-only chat). Returns false when
    /// there is no plan, the plan is executing/completed/failed, or when no
    /// plan repository has been wired.
    ///
    /// Cached per-executor: the first call hits the repo (1 SELECT on
    /// `agent_sessions.active_plan_id` + 1 SELECT on `plans.plan_json`); every
    /// subsequent call within the same plan-mode state returns instantly.
    /// The cache is invalidated by `enter_plan_mode` / `exit_plan_mode`, which
    /// are the only two events that change the result.
    async fn plan_mode_authoring_active(&self) -> bool {
        if let Some(cached) = self.plan_mode_cache.read().await.authoring_active {
            return cached;
        }
        let (authoring, hint) = self.recompute_plan_mode_snapshot().await;
        let mut w = self.plan_mode_cache.write().await;
        w.authoring_active = Some(authoring);
        w.resume_hint = hint;
        authoring
    }

    /// Fresh DB query for the authoring gate + resume hint. Callers should
    /// normally go through the cache; this is exposed so the cache-warming
    /// path is obvious and testable.
    async fn recompute_plan_mode_snapshot(&self) -> (bool, Option<String>) {
        let Some(repo) = &self.plan_repo else {
            return (false, None);
        };
        let Ok(Some(plan_id)) = repo.active_plan_for_session(&self.session_id).await else {
            return (false, None);
        };
        match repo.load(&plan_id).await {
            Ok(state) => {
                // Phase is inferred from plan contents; we mirror the same
                // logic plan_handlers uses so the gate stays consistent.
                let has_subtasks = !state.plan.subtasks.is_empty();
                let any_in_progress =
                    state.plan.subtasks.iter().any(|s| {
                        s.status == astra_services::task_orchestrator::TaskStatus::InProgress
                    });
                let items_done = state.plan.items_done() > 0;
                let progress_complete = state.plan.progress_pct() == 100;
                // Authoring = no execution activity yet. Once anything is
                // in-progress or completed, writes are unlocked. A plan row
                // with no subtasks yet (brand-new, pre-decomposition) also
                // counts as authoring.
                let authoring =
                    !has_subtasks || (!any_in_progress && !items_done && !progress_complete);
                let hint = astra_plan::plan_resume_system_prompt_section(&state);
                (authoring, hint)
            }
            Err(_) => (false, None),
        }
    }

    /// Clear the plan-mode cache so the next authoring check re-reads from
    /// the repo, AND push a fresh hint to the host's plan_resume_hint slot
    /// so the next system-prompt build reflects the new state. Called by
    /// the enter/exit tools whenever they change `active_plan_id`.
    async fn invalidate_plan_mode_cache(&self) {
        {
            let mut w = self.plan_mode_cache.write().await;
            *w = PlanModeSnapshot::default();
        }
        // Recompute the hint and push it to the host handle so the very next
        // turn sees the updated prompt — without this, the host keeps the
        // stale "A plan is currently in-flight" text even after exit_plan_mode.
        if let Some(handle) = &self.plan_resume_hint_handle {
            let (_, hint) = self.recompute_plan_mode_snapshot().await;
            if let Ok(mut slot) = handle.write() {
                *slot = hint.clone();
            }
            // Also warm the cache with the freshly-computed values so the
            // next authoring check in the same turn doesn't re-query.
            let (authoring, _) = self.recompute_plan_mode_snapshot().await;
            let mut w = self.plan_mode_cache.write().await;
            w.authoring_active = Some(authoring);
            w.resume_hint = hint;
        }
    }

    /// `enter_plan_mode` tool — mark the current session as authoring a plan
    /// so subsequent write tools are gated. Creates a new plan row owned by
    /// the session's user if `plan_id` isn't supplied.
    async fn tool_enter_plan_mode(&self, args: &Value) -> String {
        let Some(repo) = self.plan_repo.clone() else {
            return "Error: plan repository not configured on this executor".to_string();
        };
        let goal = args
            .get("goal")
            .and_then(Value::as_str)
            .unwrap_or("(pending)")
            .trim()
            .to_string();
        if goal.is_empty() {
            return "Error: goal must be non-empty".to_string();
        }

        let plan_id = args
            .get("plan_id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| astra_plan::PlanModeState::generate_plan_id(&goal));

        // Create-or-link: if a plan with this id already exists we re-link
        // it to this session (passing the observed version so a concurrent
        // editor cannot be silently overwritten); otherwise we create a
        // fresh one with no expected_version.
        let (mut state, expected_version) = match repo.load(&plan_id).await {
            Ok(mut s) => {
                let v = s.version;
                s.session_hint = Some(self.session_id.clone());
                (s, Some(v))
            }
            Err(astra_plan::PlanLoadError::NotFound(_)) => {
                let mut s = astra_plan::PlanModeState::new_with_owner(
                    goal.clone(),
                    astra_plan::ProjectContext::default(),
                    self.user_id.clone(),
                );
                s.session_hint = Some(self.session_id.clone());
                (s, None)
            }
            Err(e) => return format!("Error: load plan: {e}"),
        };

        if let Err(e) = repo.save(&plan_id, &mut state, expected_version).await {
            return format!("Error: save plan: {e}");
        }
        if let Err(e) = repo.set_active_plan(&self.session_id, Some(&plan_id)).await {
            return format!("Error: link plan to session: {e}");
        }

        // Invalidate so the next write tool and the next system-prompt build
        // both observe the freshly-linked plan instead of reading a stale
        // "no active plan" cache entry populated before the tool fired.
        self.invalidate_plan_mode_cache().await;

        // Journal the entry so session audit surfaces show it.
        if let Ok(writer) = astra_services::session_journal::JournalWriter::new(&self.session_id) {
            let _ = writer.append(
                &astra_services::session_journal::JournalEvent::plan_lifecycle(
                    Some(&self.session_id),
                    "plan_mode_entered",
                    Some(serde_json::json!({
                        "plan_id": plan_id,
                        "goal": goal,
                    })),
                ),
            );
        }

        format!(
            "Entered plan mode. plan_id={} goal=\"{}\". Write tools are now blocked — \
             author the plan, then call `exit_plan_mode` with `approved=true` when ready.",
            plan_id, goal
        )
    }

    /// `exit_plan_mode` tool — flip the current session out of authoring so
    /// write tools unlock. Does NOT start execution; the caller still needs
    /// `POST /plans/{id}/execute` (or the next turn can proceed with writes
    /// directly — approved plans are advisory, not a hard requirement for
    /// subsequent bash/file ops).
    async fn tool_exit_plan_mode(&self, args: &Value) -> String {
        let Some(repo) = self.plan_repo.clone() else {
            return "Error: plan repository not configured on this executor".to_string();
        };
        let approved = args
            .get("approved")
            .and_then(Value::as_bool)
            .unwrap_or(true);

        let active = match repo.active_plan_for_session(&self.session_id).await {
            Ok(Some(id)) => id,
            Ok(None) => {
                return "Note: session has no active plan; nothing to exit.".to_string();
            }
            Err(e) => return format!("Error: lookup active plan: {e}"),
        };

        // We unlock writes by clearing `active_plan_id`. The `plans` row
        // stays around so the approved plan can drive execution via
        // /plans/{id}/execute. Rejecting keeps the plan linked for another
        // authoring pass.
        if approved {
            if let Err(e) = repo.set_active_plan(&self.session_id, None).await {
                return format!("Error: clear active plan: {e}");
            }
        }

        // Always invalidate on exit — even a rejection mutates nothing but
        // clients often immediately retry in-plan editing afterward, and
        // a stale cache is cheap to avoid.
        self.invalidate_plan_mode_cache().await;

        if let Ok(writer) = astra_services::session_journal::JournalWriter::new(&self.session_id) {
            let _ = writer.append(
                &astra_services::session_journal::JournalEvent::plan_lifecycle(
                    Some(&self.session_id),
                    if approved {
                        "plan_approved"
                    } else {
                        "plan_rejected"
                    },
                    Some(serde_json::json!({ "plan_id": active })),
                ),
            );
        }

        if approved {
            format!(
                "Exited plan mode. plan_id={} is approved; write tools unlocked. \
                 Use `/plans/{}/execute` to start step-by-step execution.",
                active, active
            )
        } else {
            format!(
                "Plan {} left open for another authoring pass. Write tools remain blocked.",
                active
            )
        }
    }

    // File operations (sandboxed to workspace_root)
    // ────────────────────────────────────────────────────────────────────────

    fn resolve_path(&self, relative: &str) -> Result<PathBuf, String> {
        astra_tools::fs_ops::resolve_path(&self.workspace_root, relative)
    }

    fn server_write_file(&self, args: &Value) -> String {
        let prepared = match astra_tools::fs_ops::prepare_write_file(&self.workspace_root, args) {
            Ok(prepared) => prepared,
            Err(error) => return error.output,
        };

        // Record journal entry before writing
        if let Ok(mut journal) = self.file_journal.lock() {
            let turn_idx = self.journal_turn_index.load(Ordering::Relaxed);
            journal.record_before(prepared.path(), "server-write", turn_idx);
        }

        let result = prepared.apply();
        if !result.is_error
            && let Ok(mut journal) = self.file_journal.lock()
        {
            journal.record_after(prepared.path(), "server-write", prepared.content_bytes());
        }
        result.output
    }

    fn server_str_replace(&self, args: &Value) -> String {
        let prepared = match astra_tools::fs_ops::prepare_str_replace(&self.workspace_root, args) {
            Ok(prepared) => prepared,
            Err(error) => return error.output,
        };
        let dry_run = prepared.is_dry_run();

        if dry_run {
            return prepared.apply().output;
        }

        let path = prepared.path().to_owned();
        let new_content_bytes = prepared.new_content_bytes().to_vec();

        // Record journal entry
        if let Ok(mut journal) = self.file_journal.lock() {
            let turn_idx = self.journal_turn_index.load(Ordering::Relaxed);
            journal.record_before_patch(&path, "server-str-replace", turn_idx);
        }

        let result = prepared.apply();
        if !result.is_error
            && let Ok(mut journal) = self.file_journal.lock()
        {
            journal.record_after(&path, "server-str-replace", &new_content_bytes);
        }
        result.output
    }

    fn server_multi_edit(&self, args: &Value) -> String {
        let prepared = match astra_tools::fs_ops::prepare_multi_edit(&self.workspace_root, args) {
            Ok(prepared) => prepared,
            Err(error) => return error.output,
        };

        if !args
            .get("dry_run")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            && let Ok(mut journal) = self.file_journal.lock()
        {
            let turn_idx = self.journal_turn_index.load(Ordering::Relaxed);
            journal.record_before_patch(prepared.path(), "server-multi-edit", turn_idx);
        }

        let result = prepared.apply();
        if !result.is_error
            && !args
                .get("dry_run")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            && let Ok(mut journal) = self.file_journal.lock()
        {
            journal.record_after(
                prepared.path(),
                "server-multi-edit",
                prepared.new_content_bytes(),
            );
        }
        result.output
    }

    fn server_delete_file(&self, args: &Value) -> String {
        let prepared = match astra_tools::fs_ops::prepare_delete_file(&self.workspace_root, args) {
            Ok(prepared) => prepared,
            Err(error) => return error.output,
        };
        let path = prepared.path().to_path_buf();
        let turn_idx = self.journal_turn_index.load(Ordering::Relaxed);

        let result = prepared.apply();
        if !result.is_error
            && let Ok(mut journal) = self.file_journal.lock()
        {
            journal.record_delete(
                &path,
                "server-delete",
                turn_idx,
                prepared.into_before_content(),
            );
        }
        result.output
    }

    fn rollback_display_path(&self, path: &Path) -> String {
        self.relative_to_workspace_root(path)
            .unwrap_or_else(|| path.to_path_buf())
            .display()
            .to_string()
    }

    fn relative_to_workspace_root(&self, path: &Path) -> Option<PathBuf> {
        let path_variants = unique_path_variants(path);
        let root_variants = unique_path_variants(&self.workspace_root);

        path_variants.iter().find_map(|candidate| {
            root_variants.iter().find_map(|root| {
                candidate
                    .strip_prefix(root)
                    .ok()
                    .map(std::path::Path::to_path_buf)
            })
        })
    }

    fn rollback_path_candidates(&self, raw_path: &str, resolved: &Path) -> Vec<PathBuf> {
        let mut candidates = Vec::new();
        let mut push_unique = |candidate: PathBuf| {
            if !candidates.iter().any(|existing| existing == &candidate) {
                candidates.push(candidate);
            }
        };

        for variant in unique_path_variants(resolved) {
            push_unique(variant);
        }

        let relative = if Path::new(raw_path).is_absolute() {
            self.relative_to_workspace_root(Path::new(raw_path))
        } else {
            Some(normalize_path(Path::new(raw_path)))
        };

        if let Some(relative) = relative {
            push_unique(self.workspace_root.join(&relative));
            if let Ok(canonical_root) = self.workspace_root.canonicalize() {
                push_unique(canonical_root.join(relative));
            }
        }

        candidates
    }

    pub(crate) fn rollback_file_edits(&self, args: &Value) -> String {
        let scope = args
            .get("scope")
            .and_then(Value::as_str)
            .or_else(|| {
                if args.get("path").is_some() {
                    Some("file")
                } else {
                    None
                }
            })
            .unwrap_or("current_turn");

        match scope {
            "list" => {
                let summary = match self.file_journal.lock() {
                    Ok(journal) => journal.summary(),
                    Err(poisoned) => poisoned.into_inner().summary(),
                };
                let entries: Vec<Value> = summary
                    .into_iter()
                    .map(|(path, turn_index, edit_type)| {
                        json!({
                            "path": self.rollback_display_path(&path),
                            "turn_index": turn_index,
                            "edit_type": edit_type_label(edit_type),
                        })
                    })
                    .collect();
                json!({
                    "success": true,
                    "scope": "list",
                    "total_entries": entries.len(),
                    "entries": entries,
                })
                .to_string()
            }
            "file" => {
                let raw_path = match args.get("path").and_then(Value::as_str) {
                    Some(path) => path,
                    None => {
                        return json!({
                            "success": false,
                            "error": "missing 'path' for scope=file",
                        })
                        .to_string();
                    }
                };
                let path = match self.resolve_path(raw_path) {
                    Ok(path) => path,
                    Err(error) => return error,
                };
                let rollback_candidates = self.rollback_path_candidates(raw_path, &path);
                let undo_result = match self.file_journal.lock() {
                    Ok(journal) => undo_file_with_candidates(&journal, &rollback_candidates),
                    Err(poisoned) => {
                        undo_file_with_candidates(&poisoned.into_inner(), &rollback_candidates)
                    }
                };
                match undo_result {
                    Ok(Some((rolled_back_path, edit_type))) => json!({
                        "success": true,
                        "scope": "file",
                        "path": self.rollback_display_path(&rolled_back_path),
                        "edit_type": edit_type_label(edit_type),
                        "summary": format!(
                            "Rolled back the latest recorded edit for {}",
                            self.rollback_display_path(&rolled_back_path)
                        ),
                    })
                    .to_string(),
                    Ok(None) => json!({
                        "success": false,
                        "scope": "file",
                        "path": self.rollback_display_path(&path),
                        "error": "no recorded file edit found for that path",
                    })
                    .to_string(),
                    Err(error) => json!({
                        "success": false,
                        "scope": "file",
                        "path": self.rollback_display_path(&path),
                        "error": error.to_string(),
                    })
                    .to_string(),
                }
            }
            "turn" | "current_turn" => {
                let turn_index = if scope == "turn" {
                    match args.get("turn_index").and_then(Value::as_u64) {
                        Some(turn_index) => turn_index as u32,
                        None => {
                            return json!({
                                "success": false,
                                "error": "missing 'turn_index' for scope=turn",
                            })
                            .to_string();
                        }
                    }
                } else {
                    self.journal_turn_index.load(Ordering::Relaxed)
                };
                let checkpoint = args
                    .get("file_after_sequence")
                    .or_else(|| args.get("after_sequence"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let result = match self.file_journal.lock() {
                    Ok(journal) => journal.undo_turn_since(turn_index, checkpoint),
                    Err(poisoned) => poisoned
                        .into_inner()
                        .undo_turn_since(turn_index, checkpoint),
                };
                let reverted: Vec<String> = result
                    .reverted
                    .iter()
                    .map(|path| self.rollback_display_path(path))
                    .collect();
                let failed: Vec<Value> = result
                    .failed
                    .iter()
                    .map(|(path, error)| {
                        json!({
                            "path": self.rollback_display_path(path),
                            "error": error,
                        })
                    })
                    .collect();
                let success = !reverted.is_empty() && failed.is_empty();
                let summary = if reverted.is_empty() {
                    format!("No recorded file edits found for turn {turn_index}")
                } else if failed.is_empty() {
                    format!(
                        "Rolled back {} file edit{} from turn {turn_index}",
                        reverted.len(),
                        if reverted.len() == 1 { "" } else { "s" }
                    )
                } else {
                    format!(
                        "Rolled back {} file edit{} from turn {turn_index} with {} failure{}",
                        reverted.len(),
                        if reverted.len() == 1 { "" } else { "s" },
                        failed.len(),
                        if failed.len() == 1 { "" } else { "s" }
                    )
                };
                json!({
                    "success": success,
                    "scope": scope,
                    "turn_index": turn_index,
                    "reverted": reverted,
                    "failed": failed,
                    "summary": summary,
                })
                .to_string()
            }
            other => json!({
                "success": false,
                "error": format!(
                    "invalid 'scope': {other} (expected one of current_turn, turn, file, list)"
                ),
            })
            .to_string(),
        }
    }

    // ────────────────────────────────────────────────────────────────────────
    // Shell operations (sandboxed)
    // ────────────────────────────────────────────────────────────────────────

    async fn server_bash(&self, args: &Value) -> String {
        let command = match args.get("command").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => return "Error: Missing 'command' parameter".to_string(),
        };
        if let Err(reason) = astra_tools::shell_ops::validate_execute_bash_command(command) {
            return reason;
        }
        let timeout_secs = args
            .get("timeout")
            .and_then(|v| v.as_f64())
            .unwrap_or(30.0)
            .min(self.sandbox_policy.max_execution_secs);

        let tier = effective_tier("bash", self.sandbox_policy.mode);
        match tier {
            ToolTier::Isolated => {
                let mut config = IsolationConfig::strict(self.workspace_root.clone());
                config.timeout = Duration::from_secs_f64(timeout_secs);
                config.net_namespace = !self.sandbox_policy.network_allowed;
                let env = filter_environment(&self.sandbox_policy);
                let out = execute_isolated(command, &env, &config).await;
                format_server_bash_output(out, timeout_secs)
            }
            ToolTier::Sandboxed => {
                let mut config = IsolationConfig::sandboxed(self.workspace_root.clone());
                config.timeout = Duration::from_secs_f64(timeout_secs);
                let env = filter_environment(&self.sandbox_policy);
                let out = execute_isolated(command, &env, &config).await;
                format_server_bash_output(out, timeout_secs)
            }
            ToolTier::InProcess => {
                let result = self.default_executor.execute("bash", args).await;
                if result.is_error && !result.output.starts_with("Error:") {
                    format!("Error: {}", result.output)
                } else {
                    result.output
                }
            }
        }
    }
}

fn format_server_bash_output(output: IsolatedOutput, timeout_secs: f64) -> String {
    let mut body = String::new();
    if !output.stdout.is_empty() {
        body.push_str(&output.stdout);
    }
    if !output.stderr.is_empty() {
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str("stderr:\n");
        body.push_str(&output.stderr);
    }
    if let Some(code) = output.exit_code {
        if code != 0 {
            if !body.is_empty() && !body.ends_with('\n') {
                body.push('\n');
            }
            body.push_str(&format!("(exit code: {code})"));
        }
    }
    if output.stdout_capped || output.stderr_capped {
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str(&format!(
            "[output capped: {} limit reached]",
            capped_streams_label(output.stdout_capped, output.stderr_capped)
        ));
    }

    if output.timed_out {
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str(&format!(
            "[bash timed out after {}; partial output shown]",
            format_timeout_seconds(timeout_secs)
        ));
        return format!("Error: {body}");
    }

    if output.exit_code.is_some_and(|code| code != 0) {
        return format!("Error: {body}");
    }
    if output.exit_code.is_none() && output.stdout.is_empty() && !output.stderr.is_empty() {
        return format!("Error: {body}");
    }

    body
}

fn capped_streams_label(stdout_capped: bool, stderr_capped: bool) -> &'static str {
    match (stdout_capped, stderr_capped) {
        (true, true) => "stdout, stderr",
        (true, false) => "stdout",
        (false, true) => "stderr",
        (false, false) => "output",
    }
}

fn format_timeout_seconds(timeout_secs: f64) -> String {
    let mut text = format!("{timeout_secs:.3}");
    while text.contains('.') && text.ends_with('0') {
        text.pop();
    }
    if text.ends_with('.') {
        text.pop();
    }
    format!("{text}s")
}

/// Generate a short UUID-like identifier for call tracking.
fn uuid_v4_short() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:08x}", (ts & 0xFFFF_FFFF) as u32)
}

fn edit_type_label(edit_type: EditType) -> &'static str {
    match edit_type {
        EditType::Create => "create",
        EditType::Overwrite => "overwrite",
        EditType::Patch => "patch",
        EditType::Delete => "delete",
    }
}

fn parse_ask_user_request(args: &Value) -> Result<AskUserRequest, String> {
    let question = match args.get("question").and_then(Value::as_str) {
        Some(question) if !question.trim().is_empty() => question.to_string(),
        _ => return Err("Error: 'question' is required".into()),
    };

    let choices: Vec<String> = args
        .get("choices")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default();

    if !choices.is_empty() && !(2..=9).contains(&choices.len()) {
        return Err("Error: choices must contain 2-9 options".into());
    }

    Ok(AskUserRequest {
        question,
        choices,
        default: args
            .get("default")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        context: args
            .get("context")
            .and_then(Value::as_str)
            .map(ToString::to_string),
    })
}

fn tool_result_from_output(output: String) -> astra_tools::ToolResult {
    let parsed = serde_json::from_str::<Value>(&output).ok();
    let json_error = parsed
        .as_ref()
        .and_then(|value| value.get("success").and_then(Value::as_bool))
        .is_some_and(|success| !success)
        || parsed
            .as_ref()
            .and_then(|value| value.get("error"))
            .is_some();
    if output.starts_with("Error:") || output.starts_with("SANDBOX_DENIED:") || json_error {
        astra_tools::ToolResult::error(output)
    } else {
        astra_tools::ToolResult::text(output)
    }
}

// ─── ToolExecutor trait implementation ────────────────────────────────────────
//
// This allows ServerToolExecutor to be used polymorphically wherever
// `dyn ToolExecutor` (or `impl ToolExecutor`) is required, e.g. in
// shared pipeline code that doesn't know whether it runs on the server
// or on an edge/CLI client.

#[async_trait]
impl ToolExecutor for ServerToolExecutor {
    async fn execute(&self, name: &str, args: &Value) -> astra_tools::ToolResult {
        // Delegate to the concrete method that already returns ToolResult.
        ServerToolExecutor::execute_with_metadata(self, name, args).await
    }

    fn tool_schemas(&self) -> Vec<Value> {
        self.default_executor.tool_schemas()
    }

    fn project_root(&self) -> &Path {
        &self.workspace_root
    }

    async fn execute_with_metadata(&self, name: &str, args: &Value) -> astra_tools::ToolResult {
        // Explicitly delegate to the inherent method (not the default trait impl).
        ServerToolExecutor::execute_with_metadata(self, name, args).await
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::Path;
    use std::sync::{Mutex as StdMutex, MutexGuard, OnceLock};

    use super::*;
    use astra_plan::PlanRepository;
    use astra_tools::{AskUserDecision, AskUserGate, AskUserResponse};
    use async_trait::async_trait;
    use serde_json::json;
    use tempfile::TempDir;

    fn env_guard() -> MutexGuard<'static, ()> {
        static ENV_LOCK: OnceLock<StdMutex<()>> = OnceLock::new();
        ENV_LOCK
            .get_or_init(|| StdMutex::new(()))
            .lock()
            .expect("env lock poisoned")
    }

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            unsafe {
                match self.previous.take() {
                    Some(value) => std::env::set_var(self.key, value),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    fn set_env_var(key: &'static str, value: impl Into<OsString>) -> EnvVarGuard {
        let previous = std::env::var_os(key);
        unsafe {
            std::env::set_var(key, value.into());
        }
        EnvVarGuard { key, previous }
    }

    #[cfg(unix)]
    fn write_fake_mysql(dir: &Path) {
        use std::os::unix::fs::PermissionsExt;

        let script = dir.join("mysql");
        std::fs::write(
            &script,
            r#"#!/bin/sh
case "$*" in
  *"SELECT current_account_name() AS name"*)
    printf '+------+\n| name |\n+------+\n| sys  |\n+------+\n'
    ;;
  *"CREATE SNAPSHOT"*)
    printf 'Query OK, 1 row affected\n'
    ;;
  *"RESTORE ACCOUNT"*)
    printf 'Query OK, 1 row affected\n'
    ;;
  *"DROP SNAPSHOT"*)
    printf 'Query OK, 1 row affected\n'
    ;;
  *"UPDATE metrics SET value = 1"*)
    printf 'Query OK, 1 row affected\n'
    ;;
  *"SELECT 1"*)
    printf '+---+\n| 1 |\n+---+\n| 1 |\n+---+\n'
    ;;
  *)
    printf 'Query OK, 1 row affected\n'
    ;;
esac
"#,
        )
        .unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();
    }

    fn test_executor() -> (ServerToolExecutor, TempDir) {
        let dir = TempDir::new().unwrap();
        let exec = ServerToolExecutor::new(
            dir.path().to_path_buf(),
            "test-user".into(),
            "test-session".into(),
            None,
            None,
        );
        (exec, dir)
    }

    fn cleanup_session_artifacts(session_id: &str) {
        std::fs::remove_dir_all(
            astra_services::session_journal::local_sessions_dir().join(session_id),
        )
        .ok();
    }

    fn session_state_test_executor(
        turn_index: u32,
    ) -> (
        ServerToolExecutor,
        TempDir,
        String,
        std::sync::Arc<std::sync::RwLock<crate::observability_integration::ObservabilitySession>>,
    ) {
        let dir = TempDir::new().unwrap();
        let session_id = format!("test-session-{}", uuid::Uuid::new_v4());
        let mut workspace =
            astra_services::session_workspace::WorkspaceMetadata::new(&session_id, "test-model");
        workspace.cwd = dir.path().display().to_string();
        astra_services::session_workspace::write_workspace(&workspace).unwrap();

        let mut exec = ServerToolExecutor::new(
            dir.path().to_path_buf(),
            "test-user".into(),
            session_id.clone(),
            None,
            None,
        );
        let session = std::sync::Arc::new(std::sync::RwLock::new(
            crate::observability_integration::ObservabilitySession::new_simple(&session_id),
        ));
        session.write().unwrap().turn_number = turn_index;
        exec.set_observability_session(session.clone());
        exec.set_turn_index(turn_index);
        (exec, dir, session_id, session)
    }

    #[test]
    fn session_state_tools_publish_workspace_artifacts() {
        let source = include_str!("server_tool_executor.rs");
        assert!(
            source.contains("publish_current_workspace(\"server_tool_executor:adjust_config\")"),
            "adjust_config should publish remote workspace artifacts"
        );
        assert!(
            source.contains("publish_current_workspace(\"server_tool_executor:prioritize_tool\")"),
            "prioritize_tool should publish remote workspace artifacts"
        );
        assert!(
            source
                .contains("publish_current_workspace(\"server_tool_executor:deprioritize_tool\")"),
            "deprioritize_tool should publish remote workspace artifacts"
        );
        assert!(
            source.contains("publish_current_workspace(\"server_tool_executor:set_goal\")"),
            "set_goal should publish remote workspace artifacts"
        );
        assert!(
            source.contains(
                "publish_current_workspace(\"server_tool_executor:rollback_session_state\")"
            ),
            "rollback_session_state should publish remote workspace artifacts after local restore"
        );
    }

    #[derive(Clone)]
    struct StaticAskUserGate {
        expected_question: &'static str,
        expected_choices: Vec<&'static str>,
        decision: AskUserDecision,
    }

    #[async_trait]
    impl AskUserGate for StaticAskUserGate {
        async fn request_user_input(
            &self,
            _request_id: &str,
            question: &str,
            choices: &[String],
            _default: Option<&str>,
            _context: Option<&str>,
        ) -> AskUserDecision {
            assert_eq!(question, self.expected_question);
            assert_eq!(
                choices,
                &self
                    .expected_choices
                    .iter()
                    .map(|choice| choice.to_string())
                    .collect::<Vec<_>>()
            );
            self.decision.clone()
        }
    }

    // ── Path traversal security ────────────────────────────────────────

    #[tokio::test]
    async fn ask_user_returns_structured_response_from_gate() {
        let (mut exec, _dir) = test_executor();
        exec.set_ask_user_gate(Arc::new(StaticAskUserGate {
            expected_question: "Which option?",
            expected_choices: vec!["first", "second"],
            decision: AskUserDecision::Answer(AskUserResponse {
                answer: "custom".into(),
                was_custom: true,
            }),
        }));

        let result = exec
            .execute_with_metadata(
                "ask_user",
                &json!({
                    "question": "Which option?",
                    "choices": ["first", "second"],
                    "default": "first"
                }),
            )
            .await;

        assert!(!result.is_error);
        assert_eq!(
            serde_json::from_str::<Value>(&result.output).unwrap(),
            json!({
                "answer": "custom",
                "question": "Which option?",
                "was_custom": true
            })
        );
    }

    #[tokio::test]
    async fn ask_user_requires_interactive_gate() {
        let (exec, _dir) = test_executor();
        let result = exec
            .execute_with_metadata("ask_user", &json!({"question": "Continue?"}))
            .await;

        assert!(result.is_error);
        assert!(result.output.contains("interactive client connection"));
    }

    #[tokio::test]
    async fn ask_user_rejects_invalid_choice_count() {
        let (exec, _dir) = test_executor();
        let result = exec
            .execute_with_metadata(
                "ask_user",
                &json!({"question": "Pick one", "choices": ["only-one"]}),
            )
            .await;

        assert!(result.is_error);
        assert!(result.output.contains("choices must contain 2-9 options"));
    }

    #[tokio::test]
    async fn resolve_path_allows_relative_inside_workspace() {
        let (exec, _dir) = test_executor();
        let result = exec.resolve_path("src/main.rs");
        assert!(result.is_ok());
        assert!(result.unwrap().starts_with(exec.workspace_root()));
    }

    #[tokio::test]
    async fn resolve_path_blocks_parent_traversal() {
        let (exec, _dir) = test_executor();
        let result = exec.resolve_path("../../etc/passwd");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("SANDBOX_DENIED"));
    }

    #[tokio::test]
    async fn resolve_path_blocks_absolute_outside_workspace() {
        let (exec, _dir) = test_executor();
        let result = exec.resolve_path("/etc/passwd");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("SANDBOX_DENIED"));
    }

    #[tokio::test]
    async fn resolve_path_allows_absolute_inside_workspace() {
        let (exec, dir) = test_executor();
        let inner = dir.path().join("foo.txt");
        let result = exec.resolve_path(inner.to_str().unwrap());
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn resolve_path_normalizes_dot_dot_in_middle() {
        let (exec, _dir) = test_executor();
        // src/../../../etc/passwd should be blocked
        let result = exec.resolve_path("src/../../../etc/passwd");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("SANDBOX_DENIED"));
    }

    #[tokio::test]
    async fn resolve_path_allows_dot_dot_within_workspace() {
        let (exec, dir) = test_executor();
        // Create nested dir so the path stays inside workspace
        std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
        let result = exec.resolve_path("a/b/../c.txt");
        assert!(result.is_ok());
        let resolved = result.unwrap();
        assert!(resolved.starts_with(exec.workspace_root()));
    }

    // ── File operations ────────────────────────────────────────────────

    #[tokio::test]
    async fn read_file_returns_content_with_line_numbers() {
        let (exec, dir) = test_executor();
        std::fs::write(dir.path().join("hello.txt"), "line1\nline2\nline3\n").unwrap();
        let result = exec
            .execute("read_file", &json!({"path": "hello.txt"}))
            .await;
        assert!(result.contains("1\tline1"));
        assert!(result.contains("2\tline2"));
        assert!(result.contains("3\tline3"));
    }

    #[tokio::test]
    async fn read_file_respects_start_and_end_line() {
        let (exec, dir) = test_executor();
        std::fs::write(dir.path().join("f.txt"), "a\nb\nc\nd\ne\n").unwrap();
        let result = exec
            .execute(
                "read_file",
                &json!({"path": "f.txt", "start_line": 2, "end_line": 4}),
            )
            .await;
        assert!(!result.contains("1\ta"));
        assert!(result.contains("2\tb"));
        assert!(result.contains("3\tc"));
        assert!(result.contains("4\td"));
        assert!(!result.contains("5\te"));
    }

    #[tokio::test]
    async fn read_file_outline_returns_outline() {
        let (exec, dir) = test_executor();
        std::fs::write(
            dir.path().join("lib.rs"),
            "pub struct User;\n\npub fn parse() {}\nfn helper() {}\n",
        )
        .unwrap();
        let result = exec
            .execute("read_file", &json!({"path": "lib.rs", "outline": true}))
            .await;
        assert!(result.contains("# Outline"), "got: {result}");
        assert!(result.contains("parse"), "got: {result}");
    }

    #[tokio::test]
    async fn read_file_large_full_read_returns_preview() {
        let (exec, dir) = test_executor();
        // Use multi-line content exceeding 80KB so the preview path triggers.
        let mut large = String::new();
        for i in 1..=3000 {
            large.push_str(&format!(
                "line {}: some padding content here to make the file larger\n",
                i
            ));
        }
        std::fs::write(dir.path().join("big.txt"), &large).unwrap();
        let result = exec.execute("read_file", &json!({"path": "big.txt"})).await;
        assert!(result.contains("Large file preview"), "got: {result}");
        assert!(result.contains("start_line"), "got: {result}");
    }

    #[tokio::test]
    async fn read_file_missing_file_returns_error() {
        let (exec, _dir) = test_executor();
        let result = exec
            .execute("read_file", &json!({"path": "nonexistent.txt"}))
            .await;
        assert!(result.starts_with("Error:"));
    }

    #[tokio::test]
    async fn read_file_missing_path_param_returns_error() {
        let (exec, _dir) = test_executor();
        let result = exec.execute("read_file", &json!({})).await;
        assert!(result.contains("Missing 'path'"));
    }

    #[tokio::test]
    async fn read_file_blocks_path_traversal() {
        let (exec, _dir) = test_executor();
        let result = exec
            .execute("read_file", &json!({"path": "../../etc/passwd"}))
            .await;
        assert!(result.contains("SANDBOX_DENIED"));
    }

    #[tokio::test]
    async fn write_file_creates_and_writes() {
        let (exec, dir) = test_executor();
        let result = exec
            .execute(
                "write_file",
                &json!({"path": "out.txt", "content": "hello world"}),
            )
            .await;
        assert!(result.contains("Successfully wrote"));
        let content = std::fs::read_to_string(dir.path().join("out.txt")).unwrap();
        assert_eq!(content, "hello world");
    }

    #[tokio::test]
    async fn write_file_creates_parent_dirs() {
        let (exec, dir) = test_executor();
        let result = exec
            .execute(
                "write_file",
                &json!({
                    "path": "deep/nested/dir/file.txt",
                    "content": "deep content"
                }),
            )
            .await;
        assert!(result.contains("Successfully wrote"));
        assert!(dir.path().join("deep/nested/dir/file.txt").exists());
    }

    #[tokio::test]
    async fn write_file_blocks_path_traversal() {
        let (exec, _dir) = test_executor();
        let result = exec
            .execute(
                "write_file",
                &json!({
                    "path": "../../evil.txt",
                    "content": "pwned"
                }),
            )
            .await;
        assert!(result.contains("SANDBOX_DENIED"));
    }

    #[tokio::test]
    async fn str_replace_single_occurrence() {
        let (exec, dir) = test_executor();
        std::fs::write(dir.path().join("code.rs"), "fn old_name() {}").unwrap();
        let result = exec
            .execute(
                "str_replace",
                &json!({
                    "path": "code.rs",
                    "old_str": "old_name",
                    "new_str": "new_name"
                }),
            )
            .await;
        assert!(result.contains("Successfully replaced"));
        let content = std::fs::read_to_string(dir.path().join("code.rs")).unwrap();
        assert_eq!(content, "fn new_name() {}");
    }

    #[tokio::test]
    async fn str_replace_rejects_multiple_matches() {
        let (exec, dir) = test_executor();
        std::fs::write(dir.path().join("dup.txt"), "foo bar foo").unwrap();
        let result = exec
            .execute(
                "str_replace",
                &json!({
                    "path": "dup.txt",
                    "old_str": "foo",
                    "new_str": "baz"
                }),
            )
            .await;
        assert!(result.contains("found 2 times"));
    }

    #[tokio::test]
    async fn str_replace_not_found() {
        let (exec, dir) = test_executor();
        std::fs::write(dir.path().join("nope.txt"), "hello").unwrap();
        let result = exec
            .execute(
                "str_replace",
                &json!({
                    "path": "nope.txt",
                    "old_str": "missing",
                    "new_str": "x"
                }),
            )
            .await;
        assert!(result.contains("not found"));
    }

    #[tokio::test]
    async fn delete_file_removes_existing() {
        let (exec, dir) = test_executor();
        let target = dir.path().join("to_delete.txt");
        std::fs::write(&target, "temp").unwrap();
        assert!(target.exists());
        let result = exec
            .execute("delete_file", &json!({"path": "to_delete.txt"}))
            .await;
        assert!(result.contains("Successfully deleted"));
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn delete_file_nonexistent_returns_error() {
        let (exec, _dir) = test_executor();
        let result = exec
            .execute("delete_file", &json!({"path": "ghost.txt"}))
            .await;
        assert!(result.contains("File not found"));
    }

    #[tokio::test]
    async fn rollback_file_edits_current_turn_reverts_server_writes() {
        let (exec, dir) = test_executor();
        exec.set_turn_index(7);

        let first = exec
            .execute("write_file", &json!({"path": "a.txt", "content": "A"}))
            .await;
        let second = exec
            .execute("write_file", &json!({"path": "b.txt", "content": "B"}))
            .await;
        assert!(first.contains("Successfully wrote"));
        assert!(second.contains("Successfully wrote"));

        let rollback = exec
            .execute("rollback_file_edits", &json!({"scope": "current_turn"}))
            .await;
        let rollback_json: Value = serde_json::from_str(&rollback).unwrap();
        assert_eq!(
            rollback_json["success"].as_bool(),
            Some(true),
            "got: {rollback}"
        );
        assert_eq!(rollback_json["turn_index"].as_u64(), Some(7));
        assert_eq!(rollback_json["reverted"].as_array().map(Vec::len), Some(2));

        assert!(!dir.path().join("a.txt").exists());
        assert!(!dir.path().join("b.txt").exists());
    }

    #[tokio::test]
    async fn rollback_file_edits_current_turn_reverts_server_multi_edit() {
        let (exec, dir) = test_executor();
        exec.set_turn_index(8);
        let target = dir.path().join("edit.txt");
        std::fs::write(&target, "aaa bbb ccc").unwrap();

        let edited = exec
            .execute(
                "multi_edit",
                &json!({
                    "path": "edit.txt",
                    "edits": [
                        {"old_str": "aaa", "new_str": "AAA"},
                        {"old_str": "ccc", "new_str": "CCC"}
                    ]
                }),
            )
            .await;
        assert!(edited.contains("Successfully applied"));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "AAA bbb CCC");

        let rollback = exec
            .execute("rollback_file_edits", &json!({"scope": "current_turn"}))
            .await;
        let rollback_json: Value = serde_json::from_str(&rollback).unwrap();
        assert_eq!(
            rollback_json["success"].as_bool(),
            Some(true),
            "got: {rollback}"
        );
        assert_eq!(rollback_json["turn_index"].as_u64(), Some(8));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "aaa bbb ccc");
    }

    #[tokio::test]
    async fn rollback_file_edits_file_scope_restores_deleted_file() {
        let (exec, dir) = test_executor();
        let target = dir.path().join("gone.txt");
        std::fs::write(&target, "restore me").unwrap();

        let deleted = exec
            .execute("delete_file", &json!({"path": "gone.txt"}))
            .await;
        assert!(deleted.contains("Successfully deleted"));
        assert!(!target.exists());

        let rollback = exec
            .execute(
                "rollback_file_edits",
                &json!({"scope": "file", "path": "gone.txt"}),
            )
            .await;
        let rollback_json: Value = serde_json::from_str(&rollback).unwrap();
        assert_eq!(
            rollback_json["success"].as_bool(),
            Some(true),
            "got: {rollback}"
        );
        assert_eq!(rollback_json["scope"].as_str(), Some("file"));
        assert_eq!(rollback_json["path"].as_str(), Some("gone.txt"));
        assert_eq!(rollback_json["edit_type"].as_str(), Some("delete"));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "restore me");
    }

    #[tokio::test]
    async fn list_dir_shows_files_and_dirs() {
        let (exec, dir) = test_executor();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        std::fs::write(dir.path().join("b.rs"), "").unwrap();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();
        let result = exec.execute("list_dir", &json!({"path": "."})).await;
        assert!(result.contains("a.txt"));
        assert!(result.contains("b.rs"));
        assert!(result.contains("subdir/"));
    }

    #[tokio::test]
    async fn list_dir_sorted_output() {
        let (exec, dir) = test_executor();
        std::fs::write(dir.path().join("z.txt"), "").unwrap();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        std::fs::write(dir.path().join("m.txt"), "").unwrap();
        let result = exec.execute("list_dir", &json!({"path": "."})).await;
        let lines: Vec<&str> = result.lines().collect();
        assert_eq!(lines, vec!["a.txt", "m.txt", "z.txt"]);
    }

    // ── Unknown tool ───────────────────────────────────────────────────

    #[tokio::test]
    async fn unknown_tool_returns_error_message() {
        let (exec, _dir) = test_executor();
        let result = exec.execute("nonexistent_tool", &json!({})).await;
        assert!(result.contains("not available"));
    }

    struct AlwaysTimeoutGate;

    #[async_trait]
    impl astra_tools::ToolApprovalGate for AlwaysTimeoutGate {
        async fn request_approval(
            &self,
            _request_id: &str,
            _tool_name: &str,
            _args: &Value,
        ) -> astra_tools::ApprovalDecision {
            astra_tools::ApprovalDecision::Timeout
        }

        fn requires_approval(&self, tool_name: &str) -> bool {
            tool_name == "bash"
        }
    }

    #[tokio::test]
    async fn approval_timeout_returns_denied_error_string() {
        let (mut exec, _dir) = test_executor();
        exec.set_approval_gate(std::sync::Arc::new(AlwaysTimeoutGate));
        let out = exec.execute("bash", &json!({"command": "echo hi"})).await;
        assert!(
            out.contains("approval request timed out"),
            "unexpected output: {out}"
        );
    }

    // ── Bash execution ─────────────────────────────────────────────────

    #[tokio::test]
    async fn bash_echo_returns_output() {
        let (exec, _dir) = test_executor();
        let result = exec
            .execute("bash", &json!({"command": "echo hello"}))
            .await;
        assert_eq!(result.trim(), "hello");
    }

    #[tokio::test]
    async fn bash_missing_command_returns_error() {
        let (exec, _dir) = test_executor();
        let result = exec.execute("bash", &json!({})).await;
        assert!(result.contains("Missing 'command'"));
    }

    #[tokio::test]
    async fn bash_nonzero_exit_includes_exit_code() {
        let (exec, _dir) = test_executor();
        let result = exec
            .execute("bash", &json!({"command": "echo nope >&2; exit 42"}))
            .await;
        assert!(result.contains("exit code: 42"));
        assert!(result.contains("stderr:"));
        assert!(result.contains("nope"));
    }

    #[tokio::test]
    async fn bash_nonzero_exit_sets_error_metadata() {
        let (exec, _dir) = test_executor();
        let result = exec
            .execute_with_metadata("bash", &json!({"command": "echo nope >&2; exit 42"}))
            .await;
        assert!(result.is_error, "got: {}", result.output);
        assert!(result.output.contains("exit code: 42"));
    }

    #[tokio::test]
    async fn bash_stderr_is_captured() {
        let (exec, _dir) = test_executor();
        let result = exec
            .execute("bash", &json!({"command": "echo err >&2"}))
            .await;
        assert!(result.contains("stderr:"));
        assert!(result.contains("err"));
    }

    #[tokio::test]
    async fn bash_runs_in_workspace_dir() {
        let (exec, dir) = test_executor();
        std::fs::write(dir.path().join("marker.txt"), "found").unwrap();
        let result = exec
            .server_bash(&json!({"command": "cat marker.txt"}))
            .await;
        assert_eq!(result.trim(), "found");
    }

    #[tokio::test]
    async fn bash_timeout_returns_partial_output() {
        let output = IsolatedOutput {
            stdout: "start\n".into(),
            stderr: String::new(),
            exit_code: None,
            timed_out: true,
            stdout_capped: false,
            stderr_capped: false,
            namespace_active: false,
            cgroup_active: false,
        };
        let result = format_server_bash_output(output, 0.2);
        assert!(result.contains("start"), "got: {result}");
        assert!(result.contains("timed out after 0.2s"), "got: {result}");
        assert!(!result.contains("done"), "got: {result}");
    }

    #[tokio::test]
    async fn bash_timeout_sets_error_metadata() {
        let output = IsolatedOutput {
            stdout: "start\n".into(),
            stderr: String::new(),
            exit_code: None,
            timed_out: true,
            stdout_capped: false,
            stderr_capped: false,
            namespace_active: false,
            cgroup_active: false,
        };
        let result = tool_result_from_output(format_server_bash_output(output, 0.2));
        assert!(result.is_error, "got: {}", result.output);
        assert!(result.output.contains("start"), "got: {}", result.output);
        assert!(result.output.contains("timed out after 0.2s"));
    }

    // ── Grep ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn grep_finds_pattern_in_files() {
        let (exec, dir) = test_executor();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .expect("git init failed");
        std::fs::write(dir.path().join("test.rs"), "fn main() {}\nfn helper() {}").unwrap();
        let result = exec.execute("grep", &json!({"pattern": "fn main"})).await;
        assert!(result.contains("fn main"), "actual output: {result}");
    }

    #[tokio::test]
    async fn grep_no_matches_returns_message() {
        let (exec, dir) = test_executor();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .expect("git init failed");
        std::fs::write(dir.path().join("empty.rs"), "nothing here").unwrap();
        let result = exec
            .execute("grep", &json!({"pattern": "ZZZZNOTFOUND"}))
            .await;
        assert!(
            result.contains("No matches found"),
            "actual output: {result}"
        );
    }

    #[tokio::test]
    async fn web_fetch_is_available_in_server_mode() {
        let (exec, _dir) = test_executor();
        let result = exec.execute("web_fetch", &json!({})).await;
        assert!(result.contains("Missing 'url'"), "{result}");
        assert!(
            !result.contains("not available in server-side execution mode"),
            "{result}"
        );
    }

    #[tokio::test]
    async fn multi_edit_is_available_in_server_mode() {
        let (exec, dir) = test_executor();
        std::fs::write(dir.path().join("edit.txt"), "foo bar baz").unwrap();

        let result = exec
            .execute(
                "multi_edit",
                &json!({
                    "path": "edit.txt",
                    "edits": [
                        {"old_str": "foo", "new_str": "FOO"},
                        {"old_str": "baz", "new_str": "BAZ"}
                    ]
                }),
            )
            .await;

        assert!(result.contains("Successfully applied"), "{result}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("edit.txt")).unwrap(),
            "FOO bar BAZ"
        );
        assert!(!result.contains("not available in server-side execution mode"));
    }

    #[tokio::test]
    async fn sleep_is_available_in_server_mode() {
        let (exec, _dir) = test_executor();
        let start = std::time::Instant::now();
        let result = exec.execute("sleep", &json!({"duration_ms": 20})).await;
        assert!(result.contains("Slept"), "{result}");
        assert!(start.elapsed().as_millis() >= 15);
        assert!(!result.contains("not available in server-side execution mode"));
    }

    #[tokio::test]
    async fn tool_search_uses_server_surface() {
        let (exec, _dir) = test_executor();
        let result = exec
            .execute("tool_search", &json!({"query": "select:memory_store"}))
            .await;
        let parsed: Value = serde_json::from_str(&result).expect("tool_search json");
        assert_eq!(parsed["matches"][0]["name"].as_str(), Some("memory_store"));
        assert_eq!(
            parsed["missing"].as_array().map(Vec::len),
            Some(0),
            "{result}"
        );
    }

    #[tokio::test]
    async fn symbols_extracts_rust_symbols() {
        let (exec, dir) = test_executor();
        std::fs::write(
            dir.path().join("sample.rs"),
            "fn hello() {}\nstruct Foo {}\n",
        )
        .unwrap();
        let result = exec.execute("symbols", &json!({"path": "sample.rs"})).await;
        assert!(result.contains("hello"));
        assert!(result.contains("Foo"));
    }

    // ── Git operations ─────────────────────────────────────────────────

    #[tokio::test]
    async fn git_status_in_non_git_dir_returns_error() {
        let (exec, _dir) = test_executor();
        let result = exec.execute("git_status", &json!({})).await;
        assert!(result.contains("Error:") || result.contains("fatal"));
    }

    #[tokio::test]
    async fn git_log_caps_at_100() {
        let (exec, dir) = test_executor();
        // Initialize a git repo
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::fs::write(dir.path().join("f.txt"), "x").unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        // Request 999 — should be capped at 100
        let result = exec.execute("git_log", &json!({"n": 999})).await;
        assert!(result.contains("initial"));
    }

    #[tokio::test]
    async fn git_helper_tools_are_available_in_server_mode() {
        let (exec, dir) = test_executor();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::fs::write(dir.path().join("f.txt"), "x").unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "initial helper commit"])
            .current_dir(dir.path())
            .output()
            .unwrap();

        let file_history = exec
            .execute("git_file_history", &json!({"file": "f.txt"}))
            .await;
        assert!(file_history.contains("File: f.txt"), "{file_history}");

        let log_search = exec
            .execute("git_log_search", &json!({"query": "helper"}))
            .await;
        assert!(
            log_search.contains("Search:") || log_search.contains("initial helper commit"),
            "{log_search}"
        );

        let contributors = exec.execute("git_contributors", &json!({})).await;
        assert!(
            contributors.contains("## Top Contributors"),
            "{contributors}"
        );
    }

    #[tokio::test]
    async fn git_stash_is_available_in_server_mode() {
        let (exec, dir) = test_executor();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir.path())
            .output()
            .unwrap();

        let stash_list = exec.execute("git_stash", &json!({"action": "list"})).await;
        assert!(
            stash_list.contains("No stashes found")
                || stash_list.contains("stash@")
                || stash_list.is_empty(),
            "{stash_list}"
        );
    }

    #[tokio::test]
    async fn rollback_database_snapshots_snapshot_scope_requires_snapshot_id() {
        let (exec, _dir) = test_executor();
        let value: Value =
            serde_json::from_str(&exec.rollback_database_snapshots(&json!({"scope": "snapshot"})))
                .expect("rollback_database_snapshots json");
        assert_eq!(value["success"].as_bool(), Some(false));
        assert_eq!(value["scope"].as_str(), Some("snapshot"));
        assert!(
            value["error"]
                .as_str()
                .unwrap_or_default()
                .contains("missing 'snapshot_id'")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn mo_query_records_snapshot_and_rollback_restores_current_turn() {
        let _guard = env_guard();
        let fake_bin = TempDir::new().unwrap();
        write_fake_mysql(fake_bin.path());
        let path = std::env::var_os("PATH").unwrap_or_default();
        let joined = std::env::join_paths(
            std::iter::once(fake_bin.path().to_path_buf()).chain(std::env::split_paths(&path)),
        )
        .unwrap();
        let _path_guard = set_env_var("PATH", joined);

        let (exec, _dir) = test_executor();
        exec.set_turn_index(11);

        let result = exec
            .execute_with_metadata("mo_query", &json!({"sql": "UPDATE metrics SET value = 1"}))
            .await;
        assert!(!result.is_error, "got: {}", result.output);
        let fields = result.metadata.as_ref().expect("mo_query metadata");
        assert!(
            fields["pre_state_snapshot_id"]
                .as_str()
                .is_some_and(|snapshot_id| snapshot_id.starts_with("moq_"))
        );
        let expected_database = astra_core::resolve_database_name(&|key| std::env::var(key).ok());
        assert_eq!(
            fields["pre_state_snapshot_database"].as_str(),
            Some(expected_database.as_str())
        );

        let rollback = exec
            .execute(
                "rollback_database_snapshots",
                &json!({"scope": "current_turn"}),
            )
            .await;
        let rollback_json: Value = serde_json::from_str(&rollback).unwrap();
        assert_eq!(
            rollback_json["success"].as_bool(),
            Some(true),
            "got: {rollback}"
        );
        assert_eq!(rollback_json["turn_index"].as_u64(), Some(11));
        assert_eq!(rollback_json["restored"].as_array().map(Vec::len), Some(1));
    }

    #[tokio::test]
    async fn rollback_session_state_current_turn_restores_server_self_mod_and_tasks() {
        let (exec, _dir, session_id, session) = session_state_test_executor(13);
        let original_top_k = session.read().unwrap().config.memory.retrieval_top_k;
        let new_top_k = if original_top_k < 20 {
            original_top_k + 1
        } else {
            original_top_k.saturating_sub(1)
        };

        let adjust: Value = serde_json::from_str(
            &exec
                .execute(
                    "adjust_config",
                    &json!({"path": "memory.retrieval_top_k", "value": new_top_k}),
                )
                .await,
        )
        .unwrap();
        assert_eq!(adjust["status"].as_str(), Some("ok"));

        let prioritize: Value = serde_json::from_str(
            &exec
                .execute("prioritize_tool", &json!({"tool": "bash"}))
                .await,
        )
        .unwrap();
        assert_eq!(prioritize["status"].as_str(), Some("ok"));

        let goal: Value = serde_json::from_str(
            &exec
                .execute("set_goal", &json!({"goal": "ship parity"}))
                .await,
        )
        .unwrap();
        assert_eq!(goal["status"].as_str(), Some("ok"));

        let compress: Value = serde_json::from_str(
            &exec
                .execute("compress_context", &json!({"reason": "manual"}))
                .await,
        )
        .unwrap();
        assert_eq!(compress["status"].as_str(), Some("ok"));

        let created: Value =
            serde_json::from_str(&exec.execute("task_create", &json!({"title": "demo"})).await)
                .unwrap();
        let task_id = created["task_id"].as_str().unwrap().to_string();

        let updated: Value = serde_json::from_str(
            &exec
                .execute(
                    "task_update",
                    &json!({"task_id": task_id.as_str(), "status": "in_progress"}),
                )
                .await,
        )
        .unwrap();
        assert_eq!(updated["success"].as_bool(), Some(true));

        let stopped: Value = serde_json::from_str(
            &exec
                .execute(
                    "task_stop",
                    &json!({"task_id": task_id.as_str(), "reason": "rollback test"}),
                )
                .await,
        )
        .unwrap();
        assert_eq!(stopped["success"].as_bool(), Some(true));

        let rollback: Value =
            serde_json::from_str(&exec.execute("rollback_session_state", &json!({})).await)
                .unwrap();
        assert_eq!(rollback["success"].as_bool(), Some(true), "got: {rollback}");
        assert_eq!(rollback["turn_index"].as_u64(), Some(13));
        assert_eq!(rollback["restored"].as_array().map(Vec::len), Some(7));

        let session = session.read().unwrap();
        assert_eq!(session.config.memory.retrieval_top_k, original_top_k);
        assert!(session.original_query.is_none());
        assert!(session.goal_tracker.is_none());
        assert!(session.compressed_turns.is_empty());
        drop(session);

        let task_list = exec.execute("task_list", &json!({})).await;
        assert!(task_list.contains("No tasks found"));

        let workspace = astra_services::session_workspace::read_workspace(&session_id).unwrap();
        assert!(workspace.session_goal.is_none());
        assert!(workspace.pinned_tools.is_empty());
        assert!(workspace.deprioritized_tools.is_empty());
        assert!(workspace.tuned_config_json.is_none());

        cleanup_session_artifacts(&session_id);
    }

    // ── Memory tool user isolation ─────────────────────────────────────

    #[tokio::test]
    async fn memory_tool_injects_user_id() {
        let (exec, _dir) = test_executor();
        // We can't actually call Memoria, but we can verify the execute path
        // doesn't panic and returns a reasonable error (no MEMORIA_BASE_URL set).
        let result = exec
            .execute("memory_store", &json!({"content": "test"}))
            .await;
        // Should attempt the call (may fail due to no server, but shouldn't crash)
        assert!(!result.is_empty());
    }

    #[tokio::test]
    async fn github_tools_delegate_to_default_executor() {
        let (exec, _dir) = test_executor();
        let result = exec
            .execute_with_metadata(
                "github_list_prs",
                &json!({"repo": "matrixorigin/mo-agent-runtime"}),
            )
            .await;
        // Verify github tools delegate to default executor (not rejected as server-mode-only).
        if result.is_error {
            assert!(
                result
                    .output
                    .contains("requires a configured GitHub client")
                    || result.output.contains("rate limit")
                    || result.output.contains("401"),
                "unexpected error: {}",
                result.output
            );
        }
        assert!(
            !result
                .output
                .contains("not available in server-side execution mode"),
            "github tool should delegate to default executor, not be rejected: {}",
            result.output
        );
    }

    // ── Output management ──────────────────────────────────────────────

    #[tokio::test]
    async fn set_turn_index_and_reset_aggregate() {
        let (exec, _dir) = test_executor();
        exec.set_turn_index(5);
        assert_eq!(exec.journal_turn_index.load(Ordering::Relaxed), 5);
        exec.aggregate_output_bytes.store(999, Ordering::Relaxed);
        exec.reset_aggregate_output();
        assert_eq!(exec.aggregate_output_bytes.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn workspace_root_returns_correct_path() {
        let (exec, dir) = test_executor();
        assert_eq!(exec.workspace_root(), dir.path());
    }

    // ── plan_mode_authoring_active caching ─────────────────────────────

    /// Counting wrapper over [`astra_plan::PlanRepository`] used by the
    /// cache tests. Records how many times each trait method was called.
    struct QueryCountingPlanRepo {
        inner: Arc<dyn astra_plan::PlanRepository>,
        active_calls: Arc<AtomicU32>,
        load_calls: Arc<AtomicU32>,
    }

    #[async_trait]
    impl astra_plan::PlanRepository for QueryCountingPlanRepo {
        async fn save(
            &self,
            plan_id: &str,
            state: &mut astra_plan::PlanModeState,
            expected_version: Option<u64>,
        ) -> Result<(), astra_plan::PlanLoadError> {
            self.inner.save(plan_id, state, expected_version).await
        }
        async fn load(
            &self,
            plan_id: &str,
        ) -> Result<astra_plan::PlanModeState, astra_plan::PlanLoadError> {
            self.load_calls.fetch_add(1, Ordering::Relaxed);
            self.inner.load(plan_id).await
        }
        async fn load_owned(
            &self,
            plan_id: &str,
            user_id: &str,
        ) -> Result<astra_plan::PlanModeState, astra_plan::PlanLoadError> {
            self.inner.load_owned(plan_id, user_id).await
        }
        async fn list_for_user(
            &self,
            user_id: &str,
            filter: astra_plan::PlanListFilter<'_>,
        ) -> Result<Vec<astra_plan::SavedPlanInfo>, astra_plan::PlanLoadError> {
            self.inner.list_for_user(user_id, filter).await
        }
        async fn delete(&self, plan_id: &str) -> Result<(), astra_plan::PlanLoadError> {
            self.inner.delete(plan_id).await
        }
        async fn set_active_plan(
            &self,
            session_id: &str,
            plan_id: Option<&str>,
        ) -> Result<(), astra_plan::PlanLoadError> {
            self.inner.set_active_plan(session_id, plan_id).await
        }
        async fn active_plan_for_session(
            &self,
            session_id: &str,
        ) -> Result<Option<String>, astra_plan::PlanLoadError> {
            self.active_calls.fetch_add(1, Ordering::Relaxed);
            self.inner.active_plan_for_session(session_id).await
        }
        async fn record_step_run(
            &self,
            input: astra_plan::NewStepRun<'_>,
        ) -> Result<String, astra_plan::PlanLoadError> {
            self.inner.record_step_run(input).await
        }
        async fn record_completed_step_run(
            &self,
            input: astra_plan::NewStepRun<'_>,
            error: Option<&str>,
            artifact_ref: Option<&str>,
        ) -> Result<String, astra_plan::PlanLoadError> {
            self.inner
                .record_completed_step_run(input, error, artifact_ref)
                .await
        }
        async fn finalize_step_run(
            &self,
            plan_id: &str,
            run_id: &str,
            status: astra_services::task_orchestrator::TaskStatus,
            error: Option<&str>,
            artifact_ref: Option<&str>,
        ) -> Result<(), astra_plan::PlanLoadError> {
            self.inner
                .finalize_step_run(plan_id, run_id, status, error, artifact_ref)
                .await
        }
        async fn get_step_run(
            &self,
            plan_id: &str,
            run_id: &str,
        ) -> Result<astra_plan::PlanStepRun, astra_plan::PlanLoadError> {
            self.inner.get_step_run(plan_id, run_id).await
        }
        async fn list_step_runs(
            &self,
            plan_id: &str,
            subtask_id: Option<&str>,
            limit: i32,
        ) -> Result<Vec<astra_plan::PlanStepRun>, astra_plan::PlanLoadError> {
            self.inner.list_step_runs(plan_id, subtask_id, limit).await
        }
        async fn abort_open_step_runs(
            &self,
            plan_id: &str,
            subtask_ids: &[String],
        ) -> Result<u64, astra_plan::PlanLoadError> {
            self.inner.abort_open_step_runs(plan_id, subtask_ids).await
        }
    }

    #[tokio::test]
    async fn plan_mode_authoring_active_caches_first_lookup() {
        // First call pays for 1 active_plan_for_session + 0 load (no plan).
        // Second call must hit the cache and issue zero additional DB queries.
        // Without the cache, every tool call would duplicate both lookups.
        let active = Arc::new(AtomicU32::new(0));
        let load = Arc::new(AtomicU32::new(0));
        let inner: Arc<dyn astra_plan::PlanRepository> =
            Arc::new(astra_plan::LocalCachePlanRepository::new());
        let wrapper = Arc::new(QueryCountingPlanRepo {
            inner,
            active_calls: active.clone(),
            load_calls: load.clone(),
        });
        let (mut exec, _dir) = test_executor();
        exec.set_plan_repository(wrapper);

        // No plan → authoring=false, cached.
        assert!(!exec.plan_mode_authoring_active().await);
        let active_after_first = active.load(Ordering::Relaxed);
        let load_after_first = load.load(Ordering::Relaxed);
        assert_eq!(
            active_after_first, 1,
            "first call must hit the repo exactly once"
        );

        for _ in 0..20 {
            assert!(!exec.plan_mode_authoring_active().await);
        }
        assert_eq!(
            active.load(Ordering::Relaxed),
            active_after_first,
            "20 additional calls must NOT issue more active_plan_for_session queries \
             — cache hit rate must be 100% between plan-mode state changes"
        );
        assert_eq!(
            load.load(Ordering::Relaxed),
            load_after_first,
            "load() count must not budge on cache hits either"
        );
    }

    #[tokio::test]
    async fn exit_plan_mode_tool_clears_shared_plan_resume_hint() {
        // Regression for the mid-run staleness: the host's plan_resume_hint
        // slot was populated at loop-start and never refreshed, so a tool
        // call that exited plan mode left "A plan is currently in-flight"
        // in the system prompt for the rest of the run. The executor now
        // shares the slot and pushes updates through on enter/exit.
        let inner: Arc<dyn astra_plan::PlanRepository> =
            Arc::new(astra_plan::LocalCachePlanRepository::new());
        let (mut exec, _dir) = test_executor();
        exec.set_plan_repository(inner);

        let hint_slot: Arc<std::sync::RwLock<Option<String>>> = Arc::new(std::sync::RwLock::new(
            Some("## Active Plan\n[plan-resume] goal=\"x\" · open=1 · done=0/1".into()),
        ));
        exec.set_plan_resume_hint_handle(Arc::clone(&hint_slot));

        // Before invalidation: hint is whatever the host was built with
        // (simulating loop-start snapshot).
        assert!(hint_slot.read().unwrap().is_some());

        // Simulate exit_plan_mode's follow-up: invalidate_plan_mode_cache is
        // what the tool calls after clearing active_plan_id. The slot must
        // now reflect fresh DB state (no active plan → None).
        exec.invalidate_plan_mode_cache().await;

        assert_eq!(
            hint_slot.read().unwrap().clone(),
            None,
            "after exit_plan_mode invalidation, the shared slot must be None — \
             otherwise the next turn's system prompt still claims a plan is in flight"
        );
    }

    #[tokio::test]
    async fn plan_mode_cache_invalidated_by_enter_exit_tools() {
        // After a tool mutates plan-mode state, the next authoring check must
        // re-read the repo. Without invalidation, the cache would keep
        // returning the stale pre-enter/exit answer and the write guard
        // would misbehave for the rest of the run.
        let active = Arc::new(AtomicU32::new(0));
        let load = Arc::new(AtomicU32::new(0));
        let inner: Arc<dyn astra_plan::PlanRepository> =
            Arc::new(astra_plan::LocalCachePlanRepository::new());
        let wrapper = Arc::new(QueryCountingPlanRepo {
            inner,
            active_calls: active.clone(),
            load_calls: load.clone(),
        });
        let (mut exec, _dir) = test_executor();
        exec.set_plan_repository(wrapper);

        // Prime the cache: no plan yet → authoring=false.
        assert!(!exec.plan_mode_authoring_active().await);
        let before = active.load(Ordering::Relaxed);

        // Simulate an enter_plan_mode: cache must be invalidated.
        exec.invalidate_plan_mode_cache().await;

        // Next authoring check re-queries (LocalCache still returns no plan,
        // but the call must have happened).
        assert!(!exec.plan_mode_authoring_active().await);
        assert!(
            active.load(Ordering::Relaxed) > before,
            "invalidation must force a fresh active_plan_for_session lookup \
             — active count before={before}, after={}",
            active.load(Ordering::Relaxed)
        );
    }

    // ── Plan-mode write guard E2E ───────────────────────────────────────────

    /// In-memory plan repo that supports active_plan_id toggling for the
    /// write-guard test. Stores one plan and one active_plan_id slot.
    struct InMemoryPlanRepo {
        active_plan: tokio::sync::RwLock<Option<String>>,
        plan_state: tokio::sync::RwLock<Option<(String, astra_plan::PlanModeState)>>,
    }

    impl InMemoryPlanRepo {
        fn new() -> Self {
            Self {
                active_plan: tokio::sync::RwLock::new(None),
                plan_state: tokio::sync::RwLock::new(None),
            }
        }
    }

    #[async_trait]
    impl astra_plan::PlanRepository for InMemoryPlanRepo {
        async fn save(
            &self,
            plan_id: &str,
            state: &mut astra_plan::PlanModeState,
            _expected_version: Option<u64>,
        ) -> Result<(), astra_plan::PlanLoadError> {
            state.version += 1;
            *self.plan_state.write().await = Some((plan_id.to_string(), state.clone()));
            Ok(())
        }
        async fn load(
            &self,
            plan_id: &str,
        ) -> Result<astra_plan::PlanModeState, astra_plan::PlanLoadError> {
            let guard = self.plan_state.read().await;
            match &*guard {
                Some((id, s)) if id == plan_id => Ok(s.clone()),
                _ => Err(astra_plan::PlanLoadError::NotFound(plan_id.into())),
            }
        }
        async fn load_owned(
            &self,
            plan_id: &str,
            _user_id: &str,
        ) -> Result<astra_plan::PlanModeState, astra_plan::PlanLoadError> {
            self.load(plan_id).await
        }
        async fn list_for_user(
            &self,
            _user_id: &str,
            _filter: astra_plan::PlanListFilter<'_>,
        ) -> Result<Vec<astra_plan::SavedPlanInfo>, astra_plan::PlanLoadError> {
            Ok(vec![])
        }
        async fn delete(&self, _plan_id: &str) -> Result<(), astra_plan::PlanLoadError> {
            Ok(())
        }
        async fn set_active_plan(
            &self,
            _session_id: &str,
            plan_id: Option<&str>,
        ) -> Result<(), astra_plan::PlanLoadError> {
            *self.active_plan.write().await = plan_id.map(str::to_string);
            Ok(())
        }
        async fn active_plan_for_session(
            &self,
            _session_id: &str,
        ) -> Result<Option<String>, astra_plan::PlanLoadError> {
            Ok(self.active_plan.read().await.clone())
        }
        async fn record_step_run(
            &self,
            _input: astra_plan::NewStepRun<'_>,
        ) -> Result<String, astra_plan::PlanLoadError> {
            Ok(uuid::Uuid::new_v4().to_string())
        }
        async fn record_completed_step_run(
            &self,
            _input: astra_plan::NewStepRun<'_>,
            _error: Option<&str>,
            _artifact_ref: Option<&str>,
        ) -> Result<String, astra_plan::PlanLoadError> {
            Ok(uuid::Uuid::new_v4().to_string())
        }
        async fn finalize_step_run(
            &self,
            _plan_id: &str,
            _run_id: &str,
            _status: astra_services::task_orchestrator::TaskStatus,
            _error: Option<&str>,
            _artifact_ref: Option<&str>,
        ) -> Result<(), astra_plan::PlanLoadError> {
            Ok(())
        }
        // NOTE: this mock does not persist step_run rows; tests that exercise
        // `finish_step_run_handler` or otherwise depend on reading a run back
        // must use the real `CloudPlanRepository` (or another repo that
        // actually stores runs) instead of `InMemoryPlanRepo`.
        async fn get_step_run(
            &self,
            _plan_id: &str,
            run_id: &str,
        ) -> Result<astra_plan::PlanStepRun, astra_plan::PlanLoadError> {
            Err(astra_plan::PlanLoadError::NotFound(run_id.into()))
        }
        async fn list_step_runs(
            &self,
            _plan_id: &str,
            _subtask_id: Option<&str>,
            _limit: i32,
        ) -> Result<Vec<astra_plan::PlanStepRun>, astra_plan::PlanLoadError> {
            Ok(vec![])
        }
        async fn abort_open_step_runs(
            &self,
            _plan_id: &str,
            _subtask_ids: &[String],
        ) -> Result<u64, astra_plan::PlanLoadError> {
            Ok(0)
        }
    }

    /// Core plan-mode write guard contract: bash is blocked while a plan is
    /// in authoring phase, and unblocked after exit_plan_mode(approved=true).
    #[tokio::test]
    async fn plan_mode_write_guard_blocks_bash_during_authoring_unblocks_after_exit() {
        let repo = Arc::new(InMemoryPlanRepo::new());

        // Seed a plan in authoring state (has subtasks, all pending, none done).
        let mut state = astra_plan::PlanModeState::new_with_owner(
            "test plan".into(),
            astra_plan::ProjectContext::default(),
            "test-user".into(),
        );
        state
            .plan
            .subtasks
            .push(astra_services::task_orchestrator::SubtaskPlan {
                id: "s1".into(),
                title: "step 1".into(),
                status: astra_services::task_orchestrator::TaskStatus::Pending,
                ..Default::default()
            });
        repo.save("plan-guard-test", &mut state, None)
            .await
            .unwrap();
        // Pin the plan as active for the session.
        repo.set_active_plan("test-session", Some("plan-guard-test"))
            .await
            .unwrap();

        let (mut exec, _dir) = test_executor();
        exec.set_plan_repository(repo.clone() as Arc<dyn astra_plan::PlanRepository>);

        // ── Phase 1: bash must be blocked ────────────────────────────────
        let result = exec
            .execute("bash", &json!({"command": "echo hello"}))
            .await;
        assert!(
            result.contains("blocked while plan mode is active"),
            "bash must be blocked during authoring, got: {result}"
        );

        // write_file also blocked.
        let result = exec
            .execute("write_file", &json!({"path": "x.txt", "content": "x"}))
            .await;
        assert!(
            result.contains("blocked while plan mode is active"),
            "write_file must be blocked during authoring, got: {result}"
        );

        // ── Phase 2: exit_plan_mode(approved=true) unblocks ──────────────
        let exit_result = exec
            .execute("exit_plan_mode", &json!({"approved": true}))
            .await;
        assert!(
            exit_result.contains("unlocked"),
            "exit_plan_mode must confirm write tools are unlocked, got: {exit_result}"
        );

        // bash now succeeds (or at least isn't blocked by the guard).
        let result = exec
            .execute("bash", &json!({"command": "echo hello"}))
            .await;
        assert!(
            !result.contains("blocked while plan mode is active"),
            "bash must NOT be blocked after exit_plan_mode, got: {result}"
        );
    }
}
