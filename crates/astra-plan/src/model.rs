//! Canonical Plan Mode model.

use astra_services::VerifierKind;
use serde::{Deserialize, Serialize};

/// Lifecycle of one authored plan step.
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
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse_status(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "in_progress" => Some(Self::InProgress),
            "paused" => Some(Self::Paused),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

/// One authored step in Plan Mode.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubtaskPlan {
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub depends_on: Vec<String>,
    pub status: TaskStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acceptance_checks: Vec<VerifierKind>,
}

impl SubtaskPlan {
    pub fn reset_for_redo(&mut self) {
        self.status = TaskStatus::Pending;
    }
}

/// Authored Plan Mode graph.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskPlan {
    pub subtasks: Vec<SubtaskPlan>,
    pub notes: Option<String>,
}

impl TaskPlan {
    pub fn ready_subtasks(&self) -> Vec<&SubtaskPlan> {
        self.subtasks
            .iter()
            .filter(|step| step.status == TaskStatus::Pending)
            .filter(|step| {
                step.depends_on.iter().all(|dependency_id| {
                    self.subtasks.iter().any(|candidate| {
                        candidate.id == *dependency_id && candidate.status == TaskStatus::Completed
                    })
                })
            })
            .collect()
    }

    pub fn progress_pct(&self) -> u32 {
        if self.subtasks.is_empty() {
            return 0;
        }
        let completed = self
            .subtasks
            .iter()
            .filter(|step| step.status == TaskStatus::Completed)
            .count();
        ((completed as f64 / self.subtasks.len() as f64) * 100.0) as u32
    }

    pub fn items_done(&self) -> u32 {
        self.subtasks
            .iter()
            .filter(|step| step.status == TaskStatus::Completed)
            .count() as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_steps_require_completed_dependencies() {
        let plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "a".into(),
                    status: TaskStatus::Completed,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "b".into(),
                    depends_on: vec!["a".into()],
                    ..Default::default()
                },
            ],
            notes: None,
        };
        assert_eq!(plan.ready_subtasks()[0].id, "b");
        assert_eq!(plan.progress_pct(), 50);
        assert_eq!(plan.items_done(), 1);
    }

    #[test]
    fn unknown_status_is_rejected() {
        assert_eq!(
            TaskStatus::parse_status("completed"),
            Some(TaskStatus::Completed)
        );
        assert_eq!(TaskStatus::parse_status("unknown"), None);
    }
}
