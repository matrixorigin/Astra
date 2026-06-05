use crate::cli::task_surface::task_checkpoint_surface;
use astra_services::TaskRecord;
use std::io::ErrorKind;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TaskResultArtifact {
    pub(crate) full_text: String,
    pub(crate) prompt_tokens: Option<u64>,
    pub(crate) completion_tokens: u64,
    pub(crate) tool_calls_count: u64,
    pub(crate) output_file: Option<String>,
}

fn task_output_dir() -> Result<PathBuf, String> {
    Ok(dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".astra")
        .join("tasks")
        .join("outputs"))
}

pub(crate) fn task_output_path(task_id: &str) -> Result<PathBuf, String> {
    if !task_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!("unsafe task id for output path: {task_id}"));
    }
    Ok(task_output_dir()?.join(format!("{task_id}.output")))
}

pub(crate) fn write_task_output(task_id: &str, text: &str) -> Result<PathBuf, String> {
    let dir = task_output_dir()?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("create task output dir: {e}"))?;
    let path = dir.join(format!("{task_id}.output"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|e| format!("open task output: {e}"))?;
        use std::io::Write as _;
        file.write_all(text.as_bytes())
            .map_err(|e| format!("write task output: {e}"))?;
    }
    #[cfg(not(unix))]
    {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&path)
            .map_err(|e| format!("open task output: {e}"))?;
        use std::io::Write as _;
        file.write_all(text.as_bytes())
            .map_err(|e| format!("write task output: {e}"))?;
    }
    Ok(path)
}

pub(crate) fn load_task_result_artifact(
    task: &TaskRecord,
) -> Result<Option<TaskResultArtifact>, String> {
    if let Some(ref checkpoint) = task.checkpoint {
        let checkpoint = task_checkpoint_surface(checkpoint);
        if let Some(full_text) = checkpoint.full_text {
            return Ok(Some(TaskResultArtifact {
                full_text: full_text.to_string(),
                prompt_tokens: checkpoint.prompt_tokens,
                completion_tokens: checkpoint.completion_tokens,
                tool_calls_count: checkpoint.tool_calls_count,
                output_file: checkpoint.output_file.map(str::to_string),
            }));
        }
    }

    let output_path = task_output_path(&task.task_id)?;
    match std::fs::read_to_string(&output_path) {
        Ok(text) if !text.trim().is_empty() => Ok(Some(TaskResultArtifact {
            full_text: text,
            prompt_tokens: None,
            completion_tokens: 0,
            tool_calls_count: 0,
            output_file: Some(output_path.display().to_string()),
        })),
        Ok(_) => Ok(None),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("read task output: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::{TaskCheckpoint, TaskRecord, TaskStatus};
    use serial_test::serial;

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
    #[serial]
    fn load_task_result_artifact_prefers_checkpoint_over_output_file() {
        let temp = tempfile::TempDir::new().unwrap();
        temp_env::with_var("HOME", Some(temp.path()), || {
            write_task_output("task-1", "from-output-file").unwrap();

            let mut task = base_task();
            task.checkpoint = Some(TaskCheckpoint {
                active_subtask_id: None,
                turn: 0,
                session_id: Some("sess-1".into()),
                state: serde_json::Map::from_iter([
                    (
                        "full_text".to_string(),
                        serde_json::json!("from-checkpoint"),
                    ),
                    ("prompt_tokens".to_string(), serde_json::json!(10)),
                    ("completion_tokens".to_string(), serde_json::json!(20)),
                    ("tool_calls_count".to_string(), serde_json::json!(2)),
                ]),
            });

            let artifact = load_task_result_artifact(&task).unwrap().unwrap();
            assert_eq!(artifact.full_text, "from-checkpoint");
            assert_eq!(artifact.prompt_tokens, Some(10));
            assert_eq!(artifact.completion_tokens, 20);
            assert_eq!(artifact.tool_calls_count, 2);
        });
    }

    #[test]
    #[serial]
    fn load_task_result_artifact_reads_output_file_when_checkpoint_missing() {
        let temp = tempfile::TempDir::new().unwrap();
        temp_env::with_var("HOME", Some(temp.path()), || {
            let output = write_task_output("task-1", "from-output-file").unwrap();

            let artifact = load_task_result_artifact(&base_task()).unwrap().unwrap();
            assert_eq!(artifact.full_text, "from-output-file");
            assert_eq!(artifact.prompt_tokens, None);
            assert_eq!(
                artifact.output_file.as_deref(),
                Some(output.display().to_string().as_str())
            );
        });
    }

    #[test]
    #[serial]
    fn load_task_result_artifact_missing_artifact_returns_none_without_creating_dir() {
        let temp = tempfile::TempDir::new().unwrap();
        temp_env::with_var("HOME", Some(temp.path()), || {
            let artifact = load_task_result_artifact(&base_task()).unwrap();
            assert!(artifact.is_none());
            assert!(
                !temp
                    .path()
                    .join(".astra")
                    .join("tasks")
                    .join("outputs")
                    .exists()
            );
        });
    }

    #[test]
    #[serial]
    fn load_task_result_artifact_surfaces_unreadable_existing_output_path() {
        let temp = tempfile::TempDir::new().unwrap();
        temp_env::with_var("HOME", Some(temp.path()), || {
            let path = task_output_path("task-1").unwrap();
            std::fs::create_dir_all(&path).unwrap();

            let err = load_task_result_artifact(&base_task()).unwrap_err();
            assert!(err.contains("read task output"));
        });
    }

    #[test]
    #[serial]
    fn task_output_path_uses_home_as_storage_root() {
        let temp = tempfile::TempDir::new().unwrap();
        temp_env::with_var("HOME", Some(temp.path()), || {
            let path = task_output_path("task-1").unwrap();
            assert_eq!(
                path,
                temp.path()
                    .join(".astra")
                    .join("tasks")
                    .join("outputs")
                    .join("task-1.output")
            );
        });
    }

    #[test]
    #[serial]
    fn task_output_path_rejects_unsafe_task_id() {
        let temp = tempfile::TempDir::new().unwrap();
        temp_env::with_var("HOME", Some(temp.path()), || {
            let err = task_output_path("../task-1").unwrap_err();
            assert!(err.contains("unsafe task id"));
        });
    }
}
