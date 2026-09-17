//! Presentation-only projection of the canonical server-owned Work graph.
//!
//! These types deliberately contain no storage or mutation API. They let CLI
//! surfaces render Work without creating a second task authority beside the
//! versioned Work graph owned by `astra-services`.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkBoardTask {
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub status: WorkBoardTaskStatus,
    pub subtasks: Vec<WorkBoardSubtask>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_form: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocks: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_by: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkBoardSubtask {
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub status: WorkBoardTaskStatus,
    pub depends_on: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

// Rendering code historically used session-task terminology. Keep the short
// local aliases while the module boundary makes the authority explicit; these
// are projection types, not a persisted task model.
pub type SessionTask = WorkBoardTask;
pub type SessionSubtask = WorkBoardSubtask;
pub type SessionTaskStatusKind = WorkBoardTaskStatus;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkBoardTaskStatus {
    InProgress,
    Pending,
    Paused,
    Completed,
    Failed,
    Cancelled,
    #[serde(skip_serializing)]
    Other,
}

impl WorkBoardTaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InProgress => "in_progress",
            Self::Pending => "pending",
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Other => "other",
        }
    }

    pub fn is_active(self) -> bool {
        matches!(self, Self::InProgress | Self::Pending)
    }

    pub fn is_open_work(self) -> bool {
        matches!(self, Self::InProgress | Self::Pending | Self::Paused)
    }

    pub fn is_completed(self) -> bool {
        self == Self::Completed
    }

    pub fn is_in_progress(self) -> bool {
        self == Self::InProgress
    }

    pub fn is_pending(self) -> bool {
        self == Self::Pending
    }

    pub fn is_unsuccessful(self) -> bool {
        matches!(self, Self::Failed | Self::Cancelled)
    }

    pub fn status_marker(self) -> &'static str {
        match self {
            Self::InProgress => "▸",
            Self::Paused => "⏸",
            Self::Pending | Self::Other => "·",
            Self::Completed => "✓",
            Self::Failed => "✗",
            Self::Cancelled => "⏹",
        }
    }

    pub fn active_priority(self) -> u8 {
        match self {
            Self::InProgress => 0,
            Self::Pending => 1,
            Self::Paused => 2,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Other => 3,
        }
    }
}

impl std::fmt::Display for WorkBoardTaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<&str> for WorkBoardTaskStatus {
    fn from(status: &str) -> Self {
        match status.trim().to_ascii_lowercase().as_str() {
            "in_progress" => Self::InProgress,
            "pending" => Self::Pending,
            "paused" => Self::Paused,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            _ => Self::Other,
        }
    }
}

impl<'de> serde::Deserialize<'de> for WorkBoardTaskStatus {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let status = <String as serde::Deserialize>::deserialize(deserializer)?;
        Ok(Self::from(status.as_str()))
    }
}

/// Structured availability of the canonical Work projection read path.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WorkBoardProjectionHealth {
    #[default]
    Unknown,
    Ready,
    AuthenticationRequired,
    SessionUnavailable,
    ServiceUnavailable,
    TransportUnavailable,
    ProtocolMismatch,
}

pub type TaskStoreHealth = WorkBoardProjectionHealth;

pub fn unresolved_task_blocker_ids(tasks: &[WorkBoardTask], task: &WorkBoardTask) -> Vec<String> {
    unresolved_work_blocker_ids(tasks, task)
}

pub fn unresolved_work_blocker_ids(tasks: &[WorkBoardTask], task: &WorkBoardTask) -> Vec<String> {
    let statuses = tasks
        .iter()
        .map(|candidate| (candidate.id.as_str(), candidate.status))
        .collect::<HashMap<_, _>>();
    let mut seen = HashSet::new();
    let mut unresolved = task
        .blocked_by
        .iter()
        .cloned()
        .chain(
            tasks
                .iter()
                .filter(|candidate| candidate.blocks.iter().any(|id| id == &task.id))
                .map(|candidate| candidate.id.clone()),
        )
        .filter(|id| seen.insert(id.clone()))
        .filter(|id| {
            !statuses
                .get(id.as_str())
                .is_some_and(|status| status.is_completed())
        })
        .collect::<Vec<_>>();
    unresolved.sort();
    unresolved
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, status: WorkBoardTaskStatus) -> WorkBoardTask {
        WorkBoardTask {
            id: id.to_string(),
            title: id.to_string(),
            description: None,
            status,
            subtasks: Vec::new(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            active_form: None,
            owner: None,
            metadata: None,
            blocks: Vec::new(),
            blocked_by: Vec::new(),
        }
    }

    #[test]
    fn unknown_status_is_non_actionable() {
        let status = WorkBoardTaskStatus::from("future_state");
        assert_eq!(status, WorkBoardTaskStatus::Other);
        assert!(!status.is_active());
        assert!(!status.is_open_work());
    }

    #[test]
    fn missing_and_incomplete_dependencies_remain_blocking() {
        let completed = task("done", WorkBoardTaskStatus::Completed);
        let pending = task("pending", WorkBoardTaskStatus::Pending);
        let mut target = task("target", WorkBoardTaskStatus::Pending);
        target.blocked_by = vec!["done".into(), "pending".into(), "missing".into()];

        assert_eq!(
            unresolved_work_blocker_ids(&[completed, pending, target.clone()], &target),
            ["missing", "pending"]
        );
    }
}
