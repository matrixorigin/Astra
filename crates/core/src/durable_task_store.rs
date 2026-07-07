//! Durable Task Store — trait for checkpointable long-running tasks.
//!
//! The trait is storage-agnostic. Consumers provide their own implementation:
//! - Gateway: MySQL (`gw_durable_tasks` table)
//! - CLI: local JSON files
//! - Runtime: server-side DB

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskId(pub String);

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DurableTaskStatus {
    Created,
    Running,
    Suspended,
    Completed,
    Failed,
    Cancelled,
}

impl DurableTaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Running => "running",
            Self::Suspended => "suspended",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "created" => Some(Self::Created),
            "running" => Some(Self::Running),
            "suspended" => Some(Self::Suspended),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }

    pub fn is_active(&self) -> bool {
        matches!(self, Self::Created | Self::Running | Self::Suspended)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSpec {
    pub name: String,
    pub description: Option<String>,
    pub owner_id: String,
    pub initial_state: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default)]
pub struct TaskFilter {
    pub owner_id: Option<String>,
    pub status: Option<DurableTaskStatus>,
    pub id_prefix: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DurableTask {
    pub id: TaskId,
    pub name: String,
    pub description: Option<String>,
    pub owner_id: String,
    pub status: DurableTaskStatus,
    pub progress_pct: u8,
    pub step_description: Option<String>,
    pub checkpoint: Option<serde_json::Value>,
    pub error_message: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

const MAX_CHECKPOINT_SIZE: usize = 1_048_576; // 1MB

pub fn validate_checkpoint(data: &serde_json::Value) -> Result<(), String> {
    let serialized = serde_json::to_string(data).map_err(|e| format!("invalid JSON: {e}"))?;
    if serialized.len() > MAX_CHECKPOINT_SIZE {
        return Err(format!(
            "checkpoint too large: {} bytes (max {})",
            serialized.len(),
            MAX_CHECKPOINT_SIZE
        ));
    }
    Ok(())
}

#[async_trait::async_trait]
pub trait DurableTaskStore: Send + Sync {
    async fn create(&self, spec: &TaskSpec) -> Result<TaskId, String>;
    async fn get(&self, id: &TaskId) -> Result<Option<DurableTask>, String>;
    async fn list(&self, filter: TaskFilter) -> Result<Vec<DurableTask>, String>;
    async fn checkpoint(
        &self,
        id: &TaskId,
        state: &serde_json::Value,
        progress_pct: Option<u8>,
        step_description: Option<&str>,
    ) -> Result<(), String>;
    async fn update_status(
        &self,
        id: &TaskId,
        status: DurableTaskStatus,
        error_message: Option<&str>,
    ) -> Result<(), String>;
    async fn resume(&self, id: &TaskId) -> Result<Option<serde_json::Value>, String>;
    async fn delete(&self, id: &TaskId) -> Result<bool, String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskSelectorError {
    Empty,
    NotFound { selector: String },
    Ambiguous { selector: String, matches: usize },
}

impl std::fmt::Display for TaskSelectorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("task id cannot be empty"),
            Self::NotFound { selector } => write!(f, "task `{selector}` not found"),
            Self::Ambiguous { selector, matches } => {
                write!(f, "task `{selector}` is ambiguous ({matches} matches)")
            }
        }
    }
}

impl std::error::Error for TaskSelectorError {}

pub fn resolve_task_selector<'a>(
    tasks: &'a [DurableTask],
    selector: &str,
) -> Result<&'a DurableTask, TaskSelectorError> {
    let selector = selector.trim();
    if selector.is_empty() {
        return Err(TaskSelectorError::Empty);
    }

    if let Some(exact) = tasks.iter().find(|task| task.id.0 == selector) {
        return Ok(exact);
    }

    let matches: Vec<&DurableTask> = tasks
        .iter()
        .filter(|task| task.id.0.starts_with(selector))
        .collect();

    match matches.as_slice() {
        [] => Err(TaskSelectorError::NotFound {
            selector: selector.to_string(),
        }),
        [task] => Ok(task),
        many => Err(TaskSelectorError::Ambiguous {
            selector: selector.to_string(),
            matches: many.len(),
        }),
    }
}

pub async fn resolve_task_for_owner(
    store: &dyn DurableTaskStore,
    owner_id: &str,
    selector: &str,
) -> Result<DurableTask, String> {
    let selector = selector.trim();
    if selector.is_empty() {
        return Err(TaskSelectorError::Empty.to_string());
    }

    if let Some(task) = store.get(&TaskId(selector.to_string())).await?
        && task.owner_id == owner_id
    {
        return Ok(task);
    }

    let tasks = store
        .list(TaskFilter {
            owner_id: Some(owner_id.to_string()),
            id_prefix: Some(selector.to_string()),
            limit: Some(2),
            ..Default::default()
        })
        .await?;
    resolve_task_selector(&tasks, selector)
        .cloned()
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_as_str_roundtrip() {
        for status in [
            DurableTaskStatus::Created,
            DurableTaskStatus::Running,
            DurableTaskStatus::Suspended,
            DurableTaskStatus::Completed,
            DurableTaskStatus::Failed,
            DurableTaskStatus::Cancelled,
        ] {
            let s = status.as_str();
            let parsed = DurableTaskStatus::parse(s).unwrap();
            assert_eq!(status, parsed, "roundtrip failed for {s}");
        }
    }

    #[test]
    fn status_parse_unknown() {
        assert_eq!(DurableTaskStatus::parse("unknown"), None);
        assert_eq!(DurableTaskStatus::parse(""), None);
    }

    #[test]
    fn status_is_terminal() {
        assert!(!DurableTaskStatus::Created.is_terminal());
        assert!(!DurableTaskStatus::Running.is_terminal());
        assert!(!DurableTaskStatus::Suspended.is_terminal());
        assert!(DurableTaskStatus::Completed.is_terminal());
        assert!(DurableTaskStatus::Failed.is_terminal());
        assert!(DurableTaskStatus::Cancelled.is_terminal());
    }

    #[test]
    fn status_is_active() {
        assert!(DurableTaskStatus::Created.is_active());
        assert!(DurableTaskStatus::Running.is_active());
        assert!(DurableTaskStatus::Suspended.is_active());
        assert!(!DurableTaskStatus::Completed.is_active());
        assert!(!DurableTaskStatus::Failed.is_active());
        assert!(!DurableTaskStatus::Cancelled.is_active());
    }

    #[test]
    fn status_serde_roundtrip() {
        let status = DurableTaskStatus::Running;
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(json, "\"running\"");
        let parsed: DurableTaskStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, status);
    }

    #[test]
    fn task_spec_serde() {
        let spec = TaskSpec {
            name: "weekly report".into(),
            description: Some("collect github stats".into()),
            owner_id: "user1".into(),
            initial_state: Some(serde_json::json!({"repos": ["a", "b"]})),
        };
        let json = serde_json::to_value(&spec).unwrap();
        assert_eq!(json["name"], "weekly report");
        assert_eq!(json["initial_state"]["repos"][0], "a");
    }

    #[test]
    fn task_spec_without_optional_fields() {
        let spec = TaskSpec {
            name: "simple".into(),
            description: None,
            owner_id: "u".into(),
            initial_state: None,
        };
        let json = serde_json::to_value(&spec).unwrap();
        assert!(json["description"].is_null());
        assert!(json["initial_state"].is_null());
    }

    #[test]
    fn durable_task_serde() {
        let task = DurableTask {
            id: TaskId("abc-123".into()),
            name: "test".into(),
            description: None,
            owner_id: "user".into(),
            status: DurableTaskStatus::Running,
            progress_pct: 50,
            step_description: Some("step 3/6".into()),
            checkpoint: Some(serde_json::json!({"done": [1, 2, 3]})),
            error_message: None,
            created_at: "2026-01-01".into(),
            updated_at: "2026-01-02".into(),
        };
        let json = serde_json::to_string(&task).unwrap();
        let parsed: DurableTask = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, task.id);
        assert_eq!(parsed.progress_pct, 50);
        assert_eq!(parsed.status, DurableTaskStatus::Running);
    }

    #[test]
    fn task_filter_default() {
        let f = TaskFilter::default();
        assert!(f.owner_id.is_none());
        assert!(f.status.is_none());
        assert!(f.limit.is_none());
    }

    #[test]
    fn task_id_display() {
        let id = TaskId("xyz-789".into());
        assert_eq!(id.to_string(), "xyz-789");
    }

    #[test]
    fn task_id_clone_eq() {
        let a = TaskId("abc".into());
        let b = a.clone();
        assert_eq!(a, b);
    }

    fn task(id: &str, owner: &str) -> DurableTask {
        DurableTask {
            id: TaskId(id.into()),
            name: format!("task-{id}"),
            description: None,
            owner_id: owner.into(),
            status: DurableTaskStatus::Running,
            progress_pct: 0,
            step_description: None,
            checkpoint: None,
            error_message: None,
            created_at: "2026-01-01".into(),
            updated_at: "2026-01-01".into(),
        }
    }

    #[test]
    fn resolve_task_selector_accepts_full_or_short_id() {
        let tasks = vec![task("abcdef12-0000", "owner")];
        assert_eq!(
            resolve_task_selector(&tasks, "abcdef12-0000").unwrap().id.0,
            "abcdef12-0000"
        );
        assert_eq!(
            resolve_task_selector(&tasks, "abcdef12").unwrap().id.0,
            "abcdef12-0000"
        );
    }

    #[test]
    fn resolve_task_selector_rejects_ambiguous_prefix() {
        let tasks = vec![
            task("abcdef12-0000", "owner"),
            task("abcdef34-0000", "owner"),
        ];
        let err = resolve_task_selector(&tasks, "abcdef").unwrap_err();
        assert_eq!(
            err,
            TaskSelectorError::Ambiguous {
                selector: "abcdef".into(),
                matches: 2,
            }
        );
    }

    #[test]
    fn resolve_task_selector_prefers_exact_match() {
        let tasks = vec![
            task("abcdef", "owner"),
            task("abcdef12-0000", "owner"),
            task("abcdef34-0000", "owner"),
        ];
        assert_eq!(
            resolve_task_selector(&tasks, "abcdef").unwrap().id.0,
            "abcdef"
        );
    }

    #[test]
    fn resolve_task_selector_only_sees_provided_owner_scope() {
        let owner_a_tasks = vec![task("abcdef12-0000", "owner-a")];
        assert!(matches!(
            resolve_task_selector(&owner_a_tasks, "99999999"),
            Err(TaskSelectorError::NotFound { .. })
        ));
    }

    #[test]
    fn validate_checkpoint_ok() {
        let data = serde_json::json!({"step": 3, "results": [1, 2, 3]});
        assert!(validate_checkpoint(&data).is_ok());
    }

    #[test]
    fn validate_checkpoint_too_large() {
        let big = serde_json::json!({"data": "x".repeat(2_000_000)});
        let result = validate_checkpoint(&big);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("too large"));
    }
}
