use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub(crate) enum TaskStatus {
    Idle,
    TurnRunning { started_at: Instant },
    ToolExecuting { name: String, started_at: Instant },
    WaitingApproval { tool: String },
    WaitingModel,
}

impl TaskStatus {
    pub fn is_active(&self) -> bool {
        !matches!(self, TaskStatus::Idle)
    }

    pub fn elapsed(&self) -> Option<Duration> {
        match self {
            TaskStatus::TurnRunning { started_at } | TaskStatus::ToolExecuting { started_at, .. } => {
                Some(started_at.elapsed())
            }
            _ => None,
        }
    }

    pub fn display_label(&self) -> &str {
        match self {
            TaskStatus::Idle => "",
            TaskStatus::TurnRunning { .. } => "Thinking",
            TaskStatus::ToolExecuting { .. } => "Running tool",
            TaskStatus::WaitingApproval { .. } => "Awaiting approval",
            TaskStatus::WaitingModel => "Waiting for model",
        }
    }
}

impl Default for TaskStatus {
    fn default() -> Self {
        Self::Idle
    }
}
