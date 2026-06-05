use astra_services::{
    TaskCheckpoint, TaskClaimability, TaskListItem, TaskOutcome, TaskRecord, TaskStatus,
};

const TASK_ERROR_KIND_PREFIX: &str = "[astra_task_error_kind=";

pub(crate) struct TaskCheckpointSurface<'a> {
    pub(crate) error_kind: Option<&'a str>,
    pub(crate) final_state: Option<&'a str>,
    pub(crate) interruption_kind: Option<&'a str>,
    pub(crate) persistence_error: Option<&'a str>,
    pub(crate) full_text: Option<&'a str>,
    pub(crate) output_file: Option<&'a str>,
    pub(crate) prompt_tokens: Option<u64>,
    pub(crate) completion_tokens: u64,
    pub(crate) tool_calls_count: u64,
}

pub(crate) fn task_checkpoint_surface(checkpoint: &TaskCheckpoint) -> TaskCheckpointSurface<'_> {
    TaskCheckpointSurface {
        error_kind: checkpoint
            .state
            .get("error_kind")
            .and_then(|value| value.as_str()),
        final_state: checkpoint
            .state
            .get("final_state")
            .and_then(|value| value.as_str()),
        interruption_kind: checkpoint
            .state
            .get("interruption_kind")
            .and_then(|value| value.as_str()),
        persistence_error: checkpoint
            .state
            .get("persistence_error")
            .and_then(|value| value.as_str()),
        full_text: checkpoint
            .state
            .get("full_text")
            .and_then(|value| value.as_str()),
        output_file: checkpoint
            .state
            .get("output_file")
            .and_then(|value| value.as_str()),
        prompt_tokens: checkpoint
            .state
            .get("prompt_tokens")
            .and_then(|value| value.as_u64()),
        completion_tokens: checkpoint
            .state
            .get("completion_tokens")
            .and_then(|value| value.as_u64())
            .unwrap_or(0),
        tool_calls_count: checkpoint
            .state
            .get("tool_calls_count")
            .and_then(|value| value.as_u64())
            .unwrap_or(0),
    }
}

pub(crate) fn task_status_label(status: TaskStatus, outcome: Option<TaskOutcome>) -> &'static str {
    match (status, outcome) {
        (TaskStatus::Completed, Some(TaskOutcome::Partial)) => "partial",
        _ => status.as_str(),
    }
}

pub(crate) fn encode_task_failure_message(error_kind: &str, detail: &str) -> String {
    format!("{TASK_ERROR_KIND_PREFIX}{error_kind}] {detail}")
}

pub(crate) fn parse_task_failure_message(message: &str) -> (Option<&str>, &str) {
    let Some(rest) = message.strip_prefix(TASK_ERROR_KIND_PREFIX) else {
        return (None, message);
    };
    let Some((kind, detail)) = rest.split_once("] ") else {
        return (None, message);
    };
    if kind.is_empty() || detail.is_empty() {
        return (None, message);
    }
    (Some(kind), detail)
}

pub(crate) fn task_record_error_kind(task: &TaskRecord) -> Option<&str> {
    if let Some(message) = task.error_message.as_deref() {
        let (kind, _) = parse_task_failure_message(message);
        if kind.is_some() {
            return kind;
        }
    }
    if let Some(checkpoint) = task.checkpoint.as_ref() {
        let surface = task_checkpoint_surface(checkpoint);
        if surface.error_kind.is_some() {
            return surface.error_kind;
        }
        if surface.persistence_error.is_some() {
            return Some("persistence_error");
        }
        if surface.final_state == Some("interrupted") {
            return Some("partial");
        }
    }
    if task.status == TaskStatus::Completed && task.outcome == Some(TaskOutcome::Partial) {
        return Some("partial");
    }
    None
}

pub(crate) fn task_record_error_detail(task: &TaskRecord) -> Option<&str> {
    task.error_message
        .as_deref()
        .map(|message| parse_task_failure_message(message).1)
}

pub(crate) fn task_record_outcome(task: &TaskRecord) -> Option<TaskOutcome> {
    if let Some(error_kind) = task_record_error_kind(task) {
        return Some(match error_kind {
            "partial" => TaskOutcome::Partial,
            "force_stop" => TaskOutcome::Cancelled,
            _ => TaskOutcome::Failed,
        });
    }
    if task.status == TaskStatus::Failed {
        return Some(task.outcome.unwrap_or(TaskOutcome::Failed));
    }
    if task.status == TaskStatus::Cancelled {
        return Some(task.outcome.unwrap_or(TaskOutcome::Cancelled));
    }
    task.outcome
}

pub(crate) fn task_record_status_label(task: &TaskRecord) -> &'static str {
    if let Some(error_kind) = task_record_error_kind(task) {
        if error_kind == "partial" {
            return "partial";
        }
        return "failed";
    }
    task_status_label(task.status, task.outcome)
}

pub(crate) fn task_record_status_icon(task: &TaskRecord) -> &'static str {
    if let Some(error_kind) = task_record_error_kind(task) {
        if error_kind == "partial" {
            return task_status_icon(
                TaskStatus::Completed,
                Some(TaskOutcome::Partial),
                task.items_done,
                task.items_total,
            );
        }
        return task_status_icon(
            TaskStatus::Failed,
            task_record_outcome(task),
            task.items_done,
            task.items_total,
        );
    }
    task_status_icon(task.status, task.outcome, task.items_done, task.items_total)
}

pub(crate) fn task_list_item_error_kind(task: &TaskListItem) -> Option<&str> {
    if let Some(message) = task.error_message.as_deref() {
        let (kind, _) = parse_task_failure_message(message);
        if kind.is_some() {
            return kind;
        }
    }
    if task.status == TaskStatus::Completed && task.outcome == Some(TaskOutcome::Partial) {
        return Some("partial");
    }
    None
}

pub(crate) fn task_list_item_outcome(task: &TaskListItem) -> Option<TaskOutcome> {
    if let Some(error_kind) = task_list_item_error_kind(task) {
        return Some(match error_kind {
            "partial" => TaskOutcome::Partial,
            "force_stop" => TaskOutcome::Cancelled,
            _ => TaskOutcome::Failed,
        });
    }
    if task.status == TaskStatus::Failed {
        return Some(task.outcome.unwrap_or(TaskOutcome::Failed));
    }
    if task.status == TaskStatus::Cancelled {
        return Some(task.outcome.unwrap_or(TaskOutcome::Cancelled));
    }
    task.outcome
}

pub(crate) fn task_list_item_status_label(task: &TaskListItem) -> &'static str {
    if let Some(error_kind) = task_list_item_error_kind(task) {
        if error_kind == "partial" {
            return "partial";
        }
        return "failed";
    }
    task_status_label(task.status, task.outcome)
}

pub(crate) fn task_list_item_status_icon(task: &TaskListItem) -> &'static str {
    if let Some(error_kind) = task_list_item_error_kind(task) {
        if error_kind == "partial" {
            return task_status_icon(
                TaskStatus::Completed,
                Some(TaskOutcome::Partial),
                task.items_done,
                task.items_total,
            );
        }
        return task_status_icon(
            TaskStatus::Failed,
            task_list_item_outcome(task),
            task.items_done,
            task.items_total,
        );
    }
    task_status_icon(task.status, task.outcome, task.items_done, task.items_total)
}

pub(crate) fn task_list_item_claimability_label(task: &TaskListItem) -> Option<&'static str> {
    match task.claimability {
        Some(TaskClaimability::Pending) => Some("pending"),
        Some(TaskClaimability::RecoverableInProgress) => Some("recoverable"),
        None => None,
    }
}

pub(crate) fn task_list_item_claimability_icon(task: &TaskListItem) -> Option<&'static str> {
    match task.claimability {
        Some(TaskClaimability::Pending) => Some("○"),
        Some(TaskClaimability::RecoverableInProgress) => Some("↺"),
        None => None,
    }
}

pub(crate) fn task_status_icon(
    status: TaskStatus,
    outcome: Option<TaskOutcome>,
    items_done: u32,
    items_total: u32,
) -> &'static str {
    match status {
        TaskStatus::Completed
            if outcome == Some(TaskOutcome::Partial)
                || (items_total > 0 && items_done < items_total) =>
        {
            "△"
        }
        TaskStatus::Completed => "✓",
        TaskStatus::Failed => "✗",
        TaskStatus::InProgress => "▶",
        TaskStatus::Paused => "⏸",
        _ => "○",
    }
}

pub(crate) fn unfinished_task_notice(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Pending => "Task is unfinished and may still be waiting to be claimed.",
        TaskStatus::InProgress => "Task is unfinished and may be running or recoverable.",
        TaskStatus::Paused => "Task is paused and has no result yet.",
        _ => "Task is unfinished.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkpoint(entries: [(&str, serde_json::Value); 4]) -> TaskCheckpoint {
        TaskCheckpoint {
            active_subtask_id: None,
            turn: 0,
            session_id: Some("sess-1".into()),
            state: serde_json::Map::from_iter(
                entries
                    .into_iter()
                    .map(|(key, value)| (key.to_string(), value)),
            ),
        }
    }

    fn base_task() -> TaskRecord {
        TaskRecord {
            task_id: "task-1".into(),
            user_id: "user-1".into(),
            session_id: Some("sess-1".into()),
            parent_task_id: None,
            title: "test".into(),
            description: None,
            status: TaskStatus::Completed,
            progress_pct: 100,
            items_done: 1,
            items_total: 1,
            plan: None,
            checkpoint: None,
            error_message: None,
            created_at: "now".into(),
            updated_at: "now".into(),
            completed_at: None,
            user_rating: None,
            completion_time_sec: None,
            replan_count: 0,
            auto_adjustments: 0,
            outcome: Some(TaskOutcome::Success),
            project_type: None,
            goal_pattern: None,
            agent_id: None,
        }
    }

    #[test]
    fn task_status_label_prefers_partial_outcome_over_completed_status() {
        assert_eq!(
            task_status_label(TaskStatus::Completed, Some(TaskOutcome::Partial)),
            "partial"
        );
        assert_eq!(
            task_status_label(TaskStatus::Completed, Some(TaskOutcome::Success)),
            "completed"
        );
    }

    #[test]
    fn task_status_icon_marks_partial_completed_tasks() {
        assert_eq!(
            task_status_icon(TaskStatus::Completed, Some(TaskOutcome::Partial), 3, 3),
            "△"
        );
        assert_eq!(
            task_status_icon(TaskStatus::Completed, Some(TaskOutcome::Success), 3, 3),
            "✓"
        );
    }

    #[test]
    fn task_checkpoint_surface_reads_machine_readable_task_metadata() {
        let checkpoint = checkpoint([
            ("error_kind", serde_json::json!("partial")),
            ("final_state", serde_json::json!("interrupted")),
            ("interruption_kind", serde_json::json!("budget_exhausted")),
            (
                "persistence_error",
                serde_json::json!("write task output: permission denied"),
            ),
        ]);
        let surface = task_checkpoint_surface(&checkpoint);

        assert_eq!(surface.error_kind, Some("partial"));
        assert_eq!(surface.final_state, Some("interrupted"));
        assert_eq!(surface.interruption_kind, Some("budget_exhausted"));
        assert_eq!(
            surface.persistence_error,
            Some("write task output: permission denied")
        );
    }

    #[test]
    fn task_record_status_label_prefers_partial_checkpoint_when_outcome_missing() {
        let mut task = base_task();
        task.outcome = None;
        task.checkpoint = Some(checkpoint([
            ("error_kind", serde_json::json!("partial")),
            ("final_state", serde_json::json!("interrupted")),
            ("interruption_kind", serde_json::json!("budget_exhausted")),
            ("persistence_error", serde_json::Value::Null),
        ]));
        assert_eq!(task_record_status_label(&task), "partial");
    }

    #[test]
    fn task_record_status_label_prefers_failed_checkpoint_over_completed_status() {
        let mut task = base_task();
        task.outcome = None;
        task.checkpoint = Some(checkpoint([
            ("error_kind", serde_json::json!("persistence_error")),
            ("final_state", serde_json::json!("completed")),
            ("interruption_kind", serde_json::Value::Null),
            (
                "persistence_error",
                serde_json::json!("failed to append turn event"),
            ),
        ]));
        assert_eq!(task_record_status_label(&task), "failed");
    }

    #[test]
    fn task_failure_message_round_trips_kind_and_detail() {
        let encoded = encode_task_failure_message("persistence_error", "disk full");
        let (kind, detail) = parse_task_failure_message(&encoded);
        assert_eq!(kind, Some("persistence_error"));
        assert_eq!(detail, "disk full");
    }

    #[test]
    fn task_record_error_kind_prefers_structured_error_message_over_checkpoint() {
        let mut task = base_task();
        task.status = TaskStatus::Failed;
        task.error_message = Some(encode_task_failure_message(
            "persistence_error",
            "failed to save background task result: disk full",
        ));
        task.checkpoint = Some(checkpoint([
            ("error_kind", serde_json::Value::Null),
            ("final_state", serde_json::json!("completed")),
            ("interruption_kind", serde_json::Value::Null),
            ("persistence_error", serde_json::Value::Null),
        ]));
        assert_eq!(task_record_error_kind(&task), Some("persistence_error"));
        assert_eq!(
            task_record_error_detail(&task),
            Some("failed to save background task result: disk full")
        );
    }

    #[test]
    fn task_record_outcome_prefers_failed_when_structured_error_overrides_success() {
        let mut task = base_task();
        task.error_message = Some(encode_task_failure_message(
            "persistence_error",
            "failed to append turn event",
        ));
        assert_eq!(task_record_outcome(&task), Some(TaskOutcome::Failed));
        assert_eq!(task_record_status_icon(&task), "✗");
    }

    #[test]
    fn task_list_item_status_prefers_structured_error_over_completed_row() {
        let item = TaskListItem {
            task_id: "task-1".into(),
            title: "test".into(),
            session_id: Some("sess-1".into()),
            status: TaskStatus::Completed,
            progress_pct: 100,
            items_done: 1,
            items_total: 1,
            created_at: "now".into(),
            updated_at: "now".into(),
            completed_at: None,
            outcome: Some(TaskOutcome::Success),
            error_message: Some(encode_task_failure_message(
                "persistence_error",
                "failed to append turn event",
            )),
            project_type: None,
            claimability: None,
        };
        assert_eq!(task_list_item_status_label(&item), "failed");
        assert_eq!(task_list_item_outcome(&item), Some(TaskOutcome::Failed));
        assert_eq!(task_list_item_status_icon(&item), "✗");
    }

    #[test]
    fn task_list_item_claimability_helpers_surface_recoverable_in_progress() {
        let item = TaskListItem {
            task_id: "task-1".into(),
            title: "test".into(),
            session_id: Some("sess-1".into()),
            status: TaskStatus::InProgress,
            progress_pct: 50,
            items_done: 1,
            items_total: 2,
            created_at: "now".into(),
            updated_at: "now".into(),
            completed_at: None,
            outcome: None,
            error_message: None,
            project_type: None,
            claimability: Some(TaskClaimability::RecoverableInProgress),
        };
        assert_eq!(
            task_list_item_claimability_label(&item),
            Some("recoverable")
        );
        assert_eq!(task_list_item_claimability_icon(&item), Some("↺"));
        assert_eq!(task_list_item_status_label(&item), "in_progress");
    }

    #[test]
    fn unfinished_task_notice_distinguishes_pending_from_in_progress() {
        assert_eq!(
            unfinished_task_notice(TaskStatus::Pending),
            "Task is unfinished and may still be waiting to be claimed."
        );
        assert_eq!(
            unfinished_task_notice(TaskStatus::InProgress),
            "Task is unfinished and may be running or recoverable."
        );
    }
}
