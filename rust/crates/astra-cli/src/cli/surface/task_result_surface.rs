use crate::cli::surface::task_checkpoint_surface::{
    task_checkpoint_surface, task_record_error_detail, task_record_error_kind, task_record_outcome,
    task_record_status_label,
};
use astra_services::TaskRecord;
use crossterm::style::{StyledContent, Stylize};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TaskResultHeaderSurface<'a> {
    pub(crate) is_unfinished: bool,
    pub(crate) display_status: &'static str,
    pub(crate) raw_task_status: Option<&'static str>,
    pub(crate) outcome: Option<astra_services::TaskOutcome>,
    pub(crate) error_detail: Option<&'a str>,
    pub(crate) error_kind: Option<&'a str>,
    pub(crate) final_state: Option<&'a str>,
    pub(crate) interruption_kind: Option<&'a str>,
    pub(crate) persistence_error: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub(crate) struct TaskResultReadSurface<'a> {
    pub(crate) task: &'a TaskRecord,
    pub(crate) exit_code: crate::cli::exit_code::ExitCode,
    pub(crate) header: TaskResultHeaderSurface<'a>,
    pub(crate) effective_error_kind: Option<&'a str>,
    pub(crate) missing_text: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TaskResultArtifactSurface<'a> {
    pub(crate) full_text: &'a str,
    pub(crate) prompt_tokens: Option<u64>,
    pub(crate) completion_tokens: u64,
    pub(crate) tool_calls_count: u64,
    pub(crate) output_file: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskResultHeaderFieldKind {
    Status,
    TaskStatus,
    Outcome,
    Error,
    ErrorKind,
    FinalState,
    Interrupt,
    Persistence,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TaskResultHeaderField<'a> {
    pub(crate) label: &'static str,
    pub(crate) value: &'a str,
    pub(crate) kind: TaskResultHeaderFieldKind,
}

impl<'a> TaskResultReadSurface<'a> {
    pub(crate) fn from_task(task: &'a TaskRecord) -> Self {
        let exit_code = task_result_lookup_exit_code(task);
        let header = task_result_header_surface(task);
        let effective_error_kind = header
            .error_kind
            .or_else(|| crate::cli::command_router::error_kind_for_exit_code(exit_code));
        let missing_text = task_result_missing_text(task);
        Self {
            task,
            exit_code,
            header,
            effective_error_kind,
            missing_text,
        }
    }

    pub(crate) fn header_fields(&'a self) -> Vec<TaskResultHeaderField<'a>> {
        let mut fields = Vec::new();
        fields.push(TaskResultHeaderField {
            label: "status:",
            value: self.header.display_status,
            kind: TaskResultHeaderFieldKind::Status,
        });
        if let Some(task_status) = self.header.raw_task_status {
            fields.push(TaskResultHeaderField {
                label: "task status:",
                value: task_status,
                kind: TaskResultHeaderFieldKind::TaskStatus,
            });
        }
        if let Some(outcome) = self.header.outcome {
            fields.push(TaskResultHeaderField {
                label: "outcome:",
                value: outcome.as_str(),
                kind: TaskResultHeaderFieldKind::Outcome,
            });
        }
        if let Some(error_detail) = self.header.error_detail {
            fields.push(TaskResultHeaderField {
                label: "error:",
                value: error_detail,
                kind: TaskResultHeaderFieldKind::Error,
            });
        }
        if let Some(error_kind) = self.effective_error_kind {
            fields.push(TaskResultHeaderField {
                label: "error kind:",
                value: error_kind,
                kind: TaskResultHeaderFieldKind::ErrorKind,
            });
        }
        if let Some(final_state) = self.header.final_state {
            fields.push(TaskResultHeaderField {
                label: "final state:",
                value: final_state,
                kind: TaskResultHeaderFieldKind::FinalState,
            });
        }
        if let Some(interruption_kind) = self.header.interruption_kind {
            fields.push(TaskResultHeaderField {
                label: "interrupt:",
                value: interruption_kind,
                kind: TaskResultHeaderFieldKind::Interrupt,
            });
        }
        if let Some(persistence_error) = self.header.persistence_error {
            fields.push(TaskResultHeaderField {
                label: "persistence:",
                value: persistence_error,
                kind: TaskResultHeaderFieldKind::Persistence,
            });
        }
        fields
    }

    pub(crate) fn json_payload(
        &self,
        artifact: Option<TaskResultArtifactSurface<'_>>,
    ) -> serde_json::Value {
        let mut payload = serde_json::Map::from_iter([
            ("task_id".to_string(), serde_json::json!(self.task.task_id)),
            ("title".to_string(), serde_json::json!(self.task.title)),
            (
                "status".to_string(),
                serde_json::json!(self.header.display_status),
            ),
            (
                "exit_code".to_string(),
                serde_json::json!(i32::from(self.exit_code)),
            ),
        ]);
        if let Some(task_status) = self.header.raw_task_status {
            payload.insert("task_status".to_string(), serde_json::json!(task_status));
        }
        if let Some(outcome) = self.header.outcome {
            payload.insert("outcome".to_string(), serde_json::json!(outcome.as_str()));
        }
        if let Some(error_detail) = self.header.error_detail {
            payload.insert("error".to_string(), serde_json::json!(error_detail));
        }
        if let Some(error_kind) = self.effective_error_kind {
            payload.insert("error_kind".to_string(), serde_json::json!(error_kind));
        }
        if let Some(final_state) = self.header.final_state {
            payload.insert("final_state".to_string(), serde_json::json!(final_state));
        }
        if let Some(interruption_kind) = self.header.interruption_kind {
            payload.insert(
                "interruption_kind".to_string(),
                serde_json::json!(interruption_kind),
            );
        }
        if let Some(persistence_error) = self.header.persistence_error {
            payload.insert(
                "persistence_error".to_string(),
                serde_json::json!(persistence_error),
            );
        }
        match artifact {
            Some(artifact) => {
                payload.insert(
                    "full_text".to_string(),
                    serde_json::json!(artifact.full_text),
                );
                payload.insert(
                    "prompt_tokens".to_string(),
                    serde_json::json!(artifact.prompt_tokens.unwrap_or(0)),
                );
                payload.insert(
                    "completion_tokens".to_string(),
                    serde_json::json!(artifact.completion_tokens),
                );
                payload.insert(
                    "tool_calls_count".to_string(),
                    serde_json::json!(artifact.tool_calls_count),
                );
                if let Some(output_file) = artifact.output_file {
                    payload.insert("output_file".to_string(), serde_json::json!(output_file));
                }
            }
            None => {
                payload.insert("result".to_string(), serde_json::Value::Null);
            }
        }
        serde_json::Value::Object(payload)
    }
}

pub(crate) fn load_task_result_read_surface(task: &TaskRecord) -> TaskResultReadSurface<'_> {
    TaskResultReadSurface::from_task(task)
}

pub(crate) fn task_result_header_surface(task: &TaskRecord) -> TaskResultHeaderSurface<'_> {
    let is_unfinished = task_result_is_unfinished(task);
    let checkpoint_surface = task.checkpoint.as_ref().map(task_checkpoint_surface);
    TaskResultHeaderSurface {
        is_unfinished,
        display_status: if is_unfinished {
            "unfinished"
        } else {
            task_record_status_label(task)
        },
        raw_task_status: if is_unfinished {
            Some(task.status.as_str())
        } else {
            None
        },
        outcome: task_record_outcome(task),
        error_detail: task_record_error_detail(task),
        error_kind: task_record_error_kind(task),
        final_state: checkpoint_surface
            .as_ref()
            .and_then(|surface| surface.final_state),
        interruption_kind: checkpoint_surface
            .as_ref()
            .and_then(|surface| surface.interruption_kind),
        persistence_error: checkpoint_surface
            .as_ref()
            .and_then(|surface| surface.persistence_error),
    }
}

pub(crate) fn task_result_lookup_exit_code(task: &TaskRecord) -> crate::cli::exit_code::ExitCode {
    if let Some(error_kind) =
        task_record_error_kind(task).and_then(crate::cli::command_router::exit_code_for_error_kind)
    {
        return error_kind;
    }

    if !task.status.is_terminal() {
        return crate::cli::exit_code::ExitCode::Unfinished;
    }

    match (task.status, task.outcome) {
        (astra_services::TaskStatus::Completed, Some(astra_services::TaskOutcome::Partial)) => {
            crate::cli::exit_code::ExitCode::Partial
        }
        (astra_services::TaskStatus::Completed, _) => crate::cli::exit_code::ExitCode::Success,
        (astra_services::TaskStatus::Cancelled, _) => crate::cli::exit_code::ExitCode::ForceStop,
        (astra_services::TaskStatus::Failed, _) => crate::cli::exit_code::ExitCode::ToolFailure,
        _ => unreachable!("non-terminal statuses return Unfinished above"),
    }
}

pub(crate) fn task_result_is_unfinished(task: &TaskRecord) -> bool {
    task_result_lookup_exit_code(task) == crate::cli::exit_code::ExitCode::Unfinished
}

pub(crate) fn task_result_effective_error_kind<'a>(
    header: &'a TaskResultHeaderSurface<'a>,
    exit_code: crate::cli::exit_code::ExitCode,
) -> Option<&'a str> {
    header
        .error_kind
        .or_else(|| crate::cli::command_router::error_kind_for_exit_code(exit_code))
}

pub(crate) fn task_result_missing_text(task: &TaskRecord) -> &'static str {
    if task_result_is_unfinished(task) {
        crate::cli::surface::task_checkpoint_surface::unfinished_task_notice(task.status)
    } else {
        "No result available."
    }
}

pub(crate) fn task_result_header_fields<'a>(
    read: &'a TaskResultReadSurface<'a>,
) -> Vec<TaskResultHeaderField<'a>> {
    read.header_fields()
}

pub(crate) fn task_result_json_payload(
    read: &TaskResultReadSurface<'_>,
    artifact: Option<TaskResultArtifactSurface<'_>>,
) -> serde_json::Value {
    read.json_payload(artifact)
}

pub(crate) fn render_task_result_header_value<'a>(
    field: &TaskResultHeaderField<'a>,
) -> StyledContent<&'a str> {
    match field.kind {
        TaskResultHeaderFieldKind::Status
        | TaskResultHeaderFieldKind::Outcome
        | TaskResultHeaderFieldKind::FinalState => field.value.cyan(),
        TaskResultHeaderFieldKind::TaskStatus => field.value.magenta(),
        TaskResultHeaderFieldKind::Error => field.value.red(),
        TaskResultHeaderFieldKind::ErrorKind | TaskResultHeaderFieldKind::Interrupt => {
            field.value.yellow()
        }
        TaskResultHeaderFieldKind::Persistence => field.value.red(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        load_task_result_read_surface, task_result_effective_error_kind, task_result_header_fields,
        task_result_header_surface, task_result_json_payload, task_result_lookup_exit_code,
        task_result_missing_text, TaskResultArtifactSurface,
    };
    use astra_services::{TaskCheckpoint, TaskOutcome, TaskRecord, TaskStatus};

    fn base_task() -> TaskRecord {
        TaskRecord {
            task_id: "task-1".into(),
            user_id: "user-1".into(),
            session_id: Some("sess-1".into()),
            parent_task_id: None,
            title: "task".into(),
            description: None,
            status: TaskStatus::Completed,
            progress_pct: 100,
            items_done: 1,
            items_total: 1,
            plan: None,
            checkpoint: None,
            error_message: None,
            created_at: "2025-01-01T00:00:00Z".into(),
            updated_at: "2025-01-01T00:00:00Z".into(),
            completed_at: Some("2025-01-01T00:00:00Z".into()),
            user_rating: None,
            completion_time_sec: None,
            replan_count: 0,
            auto_adjustments: 0,
            outcome: None,
            project_type: None,
            goal_pattern: None,
            agent_id: None,
        }
    }

    #[test]
    fn task_result_header_surface_marks_unfinished_and_preserves_raw_status() {
        let mut task = base_task();
        task.status = TaskStatus::Paused;
        let header = task_result_header_surface(&task);

        assert!(header.is_unfinished);
        assert_eq!(header.display_status, "unfinished");
        assert_eq!(header.raw_task_status, Some("paused"));
        assert_eq!(header.outcome, None);
    }

    #[test]
    fn task_result_header_surface_exposes_structured_failure_metadata() {
        let mut task = base_task();
        task.status = TaskStatus::Failed;
        task.error_message = Some(
            crate::cli::surface::task_checkpoint_surface::encode_task_failure_message(
                "persistence_error",
                "disk full",
            ),
        );
        task.checkpoint = Some(TaskCheckpoint {
            active_subtask_id: None,
            turn: 0,
            session_id: Some("sess-1".into()),
            state: serde_json::Map::from_iter([
                ("final_state".to_string(), serde_json::json!("completed")),
                (
                    "persistence_error".to_string(),
                    serde_json::json!("disk full"),
                ),
            ]),
        });

        let header = task_result_header_surface(&task);
        assert!(!header.is_unfinished);
        assert_eq!(header.display_status, "failed");
        assert_eq!(header.raw_task_status, None);
        assert_eq!(header.outcome, Some(TaskOutcome::Failed));
        assert_eq!(header.error_detail, Some("disk full"));
        assert_eq!(header.error_kind, Some("persistence_error"));
        assert_eq!(header.final_state, Some("completed"));
        assert_eq!(header.persistence_error, Some("disk full"));
    }

    #[test]
    fn task_result_lookup_exit_code_prefers_checkpoint_error_kind() {
        let mut task = base_task();
        task.status = TaskStatus::Failed;
        task.error_message = Some("generic task failed".into());
        task.checkpoint = Some(TaskCheckpoint {
            active_subtask_id: None,
            turn: 0,
            session_id: Some("sess-1".into()),
            state: serde_json::Map::from_iter([
                (
                    "error_kind".to_string(),
                    serde_json::json!("persistence_error"),
                ),
                (
                    "persistence_error".to_string(),
                    serde_json::json!("write task output: permission denied"),
                ),
            ]),
        });

        assert_eq!(
            task_result_lookup_exit_code(&task),
            crate::cli::exit_code::ExitCode::PersistenceError
        );

        task.checkpoint
            .as_mut()
            .unwrap()
            .state
            .insert("error_kind".into(), serde_json::json!("partial"));
        assert_eq!(
            task_result_lookup_exit_code(&task),
            crate::cli::exit_code::ExitCode::Partial
        );
    }

    #[test]
    fn task_result_lookup_exit_code_marks_all_non_terminal_states_unfinished() {
        let mut task = base_task();
        task.status = TaskStatus::Pending;
        assert_eq!(
            task_result_lookup_exit_code(&task),
            crate::cli::exit_code::ExitCode::Unfinished
        );

        task.status = TaskStatus::InProgress;
        assert_eq!(
            task_result_lookup_exit_code(&task),
            crate::cli::exit_code::ExitCode::Unfinished
        );

        task.status = TaskStatus::Paused;
        assert_eq!(
            task_result_lookup_exit_code(&task),
            crate::cli::exit_code::ExitCode::Unfinished
        );

        task.status = TaskStatus::Completed;
        assert_eq!(
            task_result_lookup_exit_code(&task),
            crate::cli::exit_code::ExitCode::Success
        );
    }

    #[test]
    fn task_result_lookup_exit_code_prefers_structured_row_persistence_error_without_checkpoint() {
        let mut task = base_task();
        task.status = TaskStatus::Failed;
        task.error_message = Some(
            crate::cli::surface::task_checkpoint_surface::encode_task_failure_message(
                "persistence_error",
                "failed to save background task result: disk full",
            ),
        );

        assert_eq!(
            task_result_lookup_exit_code(&task),
            crate::cli::exit_code::ExitCode::PersistenceError
        );
    }

    #[test]
    fn task_result_lookup_exit_code_keeps_unfinished_even_with_checkpoint_output() {
        let mut task = base_task();
        task.status = TaskStatus::Paused;
        task.progress_pct = 50;
        task.items_done = 1;
        task.items_total = 2;
        task.checkpoint = Some(TaskCheckpoint {
            active_subtask_id: None,
            turn: 0,
            session_id: Some("sess-1".into()),
            state: serde_json::Map::from_iter([(
                "full_text".to_string(),
                serde_json::json!("partial output"),
            )]),
        });

        assert_eq!(
            task_result_lookup_exit_code(&task),
            crate::cli::exit_code::ExitCode::Unfinished
        );
    }

    #[test]
    fn task_result_json_payload_carries_artifact_and_header_fields() {
        let mut task = base_task();
        task.status = TaskStatus::Paused;
        let read = load_task_result_read_surface(&task);
        let artifact = TaskResultArtifactSurface {
            full_text: "partial text",
            prompt_tokens: Some(11),
            completion_tokens: 7,
            tool_calls_count: 3,
            output_file: Some("/tmp/out.txt"),
        };

        let payload = task_result_json_payload(&read, Some(artifact));

        assert_eq!(payload["status"], "unfinished");
        assert_eq!(payload["task_status"], "paused");
        assert_eq!(payload["error_kind"], "unfinished");
        assert_eq!(payload["full_text"], "partial text");
        assert_eq!(payload["prompt_tokens"], 11);
        assert_eq!(payload["completion_tokens"], 7);
        assert_eq!(payload["tool_calls_count"], 3);
        assert_eq!(payload["output_file"], "/tmp/out.txt");
        assert!(payload.get("result").is_none());
    }

    #[test]
    fn task_result_json_payload_marks_missing_artifact_as_null_result() {
        let task = base_task();
        let read = load_task_result_read_surface(&task);

        let payload = task_result_json_payload(&read, None);

        assert_eq!(payload["status"], "completed");
        assert!(payload.get("full_text").is_none());
        assert!(payload["result"].is_null());
    }

    #[test]
    fn task_result_effective_error_kind_falls_back_to_exit_code_semantics() {
        let mut task = base_task();
        task.status = TaskStatus::Paused;
        let header = task_result_header_surface(&task);
        assert_eq!(
            task_result_effective_error_kind(&header, crate::cli::exit_code::ExitCode::Unfinished,),
            Some("unfinished")
        );
    }

    #[test]
    fn task_result_missing_text_uses_unfinished_notice_for_non_terminal_status() {
        let mut task = base_task();
        task.status = TaskStatus::Pending;
        assert_eq!(
            task_result_missing_text(&task),
            crate::cli::surface::task_checkpoint_surface::unfinished_task_notice(
                TaskStatus::Pending
            )
        );

        task.status = TaskStatus::Completed;
        assert_eq!(task_result_missing_text(&task), "No result available.");
    }

    #[test]
    fn task_result_header_fields_include_effective_error_kind_fallback() {
        let mut task = base_task();
        task.status = TaskStatus::Paused;
        let read = load_task_result_read_surface(&task);
        let fields = task_result_header_fields(&read);

        assert_eq!(fields[0].label, "status:");
        assert_eq!(fields[0].value, "unfinished");
        assert_eq!(fields[1].label, "task status:");
        assert_eq!(fields[1].value, "paused");
        assert_eq!(fields[2].label, "error kind:");
        assert_eq!(fields[2].value, "unfinished");
    }

    #[test]
    fn load_task_result_read_surface_preserves_shared_semantics() {
        let mut task = base_task();
        task.status = TaskStatus::Paused;

        let read = load_task_result_read_surface(&task);

        assert_eq!(read.exit_code, crate::cli::exit_code::ExitCode::Unfinished);
        assert!(read.header.is_unfinished);
        assert_eq!(read.effective_error_kind, Some("unfinished"));
        assert_eq!(
            read.missing_text,
            crate::cli::surface::task_checkpoint_surface::unfinished_task_notice(
                TaskStatus::Paused
            )
        );
    }
}
