use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub(crate) enum TaskStatus {
    Idle,
    Dispatching,
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
            TaskStatus::TurnRunning { started_at }
            | TaskStatus::ToolExecuting { started_at, .. } => Some(started_at.elapsed()),
            _ => None,
        }
    }

    pub fn display_label(&self) -> &str {
        match self {
            TaskStatus::Idle => "",
            TaskStatus::Dispatching => "Sending",
            TaskStatus::TurnRunning { .. } => "Thinking",
            TaskStatus::ToolExecuting { .. } => "Running tool",
            TaskStatus::WaitingApproval { .. } => "Awaiting approval",
            TaskStatus::WaitingModel => "Waiting for model",
        }
    }

    pub fn objective_label(&self) -> Option<String> {
        match self {
            TaskStatus::Idle => None,
            TaskStatus::Dispatching => Some("Sending".to_string()),
            TaskStatus::TurnRunning { .. } => Some("Thinking".to_string()),
            TaskStatus::ToolExecuting { name, .. } => Some(format!("Running {name}")),
            TaskStatus::WaitingApproval { tool } => {
                if tool.is_empty() {
                    Some("Awaiting approval".to_string())
                } else {
                    Some(format!("Awaiting {tool} approval"))
                }
            }
            TaskStatus::WaitingModel => Some("Waiting for model".to_string()),
        }
    }
}

impl Default for TaskStatus {
    fn default() -> Self {
        Self::Idle
    }
}

#[cfg(test)]
mod tests {
    use super::TaskStatus;

    #[test]
    fn waiting_approval_without_tool_uses_generic_label() {
        let status = TaskStatus::WaitingApproval {
            tool: String::new(),
        };
        assert_eq!(
            status.objective_label().as_deref(),
            Some("Awaiting approval")
        );
    }

    #[test]
    fn dispatching_is_active_and_user_visible() {
        let status = TaskStatus::Dispatching;
        assert!(status.is_active());
        assert_eq!(status.display_label(), "Sending");
        assert_eq!(status.objective_label().as_deref(), Some("Sending"));
    }
}
