//! Shared plan state persisted and mirrored across runtime and CLI flows.

use std::fmt;

use serde::{Deserialize, Serialize};

use astra_services::task_orchestrator::TaskPlan;

/// Plan lifecycle phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlanPhase {
    /// Authoring the plan, no subtasks yet.
    Planning,
    /// Plan has subtasks but none started.
    Refining,
    /// At least one subtask in progress or done.
    Executing,
    /// All subtasks completed.
    Completed,
}

impl PlanPhase {
    /// Serialize as the lowercase kebab-compatible static string.
    pub fn as_str(self) -> &'static str {
        match self {
            PlanPhase::Planning => "planning",
            PlanPhase::Refining => "refining",
            PlanPhase::Executing => "executing",
            PlanPhase::Completed => "completed",
        }
    }
}

impl fmt::Display for PlanPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for PlanPhase {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for PlanPhase {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        match s.as_str() {
            "planning" => Ok(PlanPhase::Planning),
            "refining" => Ok(PlanPhase::Refining),
            "executing" => Ok(PlanPhase::Executing),
            "completed" => Ok(PlanPhase::Completed),
            other => Err(serde::de::Error::unknown_variant(
                other,
                &["planning", "refining", "executing", "completed"],
            )),
        }
    }
}

impl PlanModeState {
    /// Infer the current phase from plan progress.
    pub fn infer_phase(&self) -> PlanPhase {
        if self.plan.progress_pct() == 100 {
            PlanPhase::Completed
        } else if self.plan.subtasks.is_empty() {
            PlanPhase::Planning
        } else if self.plan.items_done() > 0 {
            PlanPhase::Executing
        } else {
            PlanPhase::Refining
        }
    }
}

fn default_version() -> u64 {
    1
}

/// Cloud-backed plan authoring state mirrored into CLI/server flows.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlanModeState {
    /// The original user goal.
    pub goal: String,
    /// Current executable plan.
    pub plan: TaskPlan,
    /// Optional rendered markdown artifact for UI/sync consumers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_md: Option<String>,
    /// Whether the mirrored plan has loaded or local edits.
    #[serde(default)]
    pub modified: bool,
    /// Execution timeline used by active plan/executor flows.
    #[serde(default)]
    pub timeline: ExecutionTimeline,
    /// Monotonic version counter for optimistic concurrency control.
    /// Incremented on every save; checked on update to detect lost writes.
    #[serde(default = "default_version")]
    pub version: u64,
    /// User who created this plan (for ownership filtering).
    #[serde(default)]
    pub created_by: Option<String>,
    /// Most-recent session that touched this plan.
    #[serde(skip)]
    pub session_hint: Option<String>,
}

impl PlanModeState {
    /// Create a new plan state with the initial goal.
    pub fn new(goal: String) -> Self {
        Self {
            goal,
            plan: TaskPlan::default(),
            plan_md: None,
            modified: false,
            timeline: ExecutionTimeline::default(),
            version: 1,
            created_by: None,
            session_hint: None,
        }
    }

    /// Create a new plan with an owner user ID.
    pub fn new_with_owner(goal: String, user_id: String) -> Self {
        let mut state = Self::new(goal);
        state.created_by = Some(user_id);
        state
    }

    /// Generate a unique plan ID from the goal.
    pub fn generate_plan_id(goal: &str) -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let slug: String = goal
            .split_whitespace()
            .take(3)
            .collect::<Vec<_>>()
            .join("-")
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '-')
            .take(30)
            .collect();

        let mut hasher = DefaultHasher::new();
        goal.hash(&mut hasher);
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        ts.hash(&mut hasher);
        let hash = hasher.finish();

        if slug.is_empty() {
            format!("plan-{:08x}", hash as u32)
        } else {
            format!("{}-{:04x}", slug.to_lowercase(), (hash & 0xFFFF) as u16)
        }
    }
}

/// Configuration for plan execution behavior.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlanExecutionConfig {
    /// If true, prompt user for confirmation before executing each subtask.
    pub step_by_step: bool,
}

/// Types of events that can occur during plan execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TimelineEventKind {
    /// Plan was rewound — subtask at `from_idx` and every subtask after it
    /// reset to pending. `reset_count` is the number that actually flipped.
    SubtaskRewound {
        anchor: String,
        from_idx: usize,
        reset_count: usize,
        reason: Option<String>,
    },
    /// A single subtask was reset for re-execution (distinct from a rewind).
    SubtaskRedone {
        subtask_id: String,
        title: String,
        attempt: u32,
    },
}

/// A single event in the execution timeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelineEvent {
    /// ISO 8601 timestamp
    pub timestamp: String,
    /// The event details
    pub event: TimelineEventKind,
}

impl TimelineEvent {
    /// Create a new timeline event with current timestamp.
    pub fn new(event: TimelineEventKind) -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        Self {
            timestamp: now.to_string(),
            event,
        }
    }
}

/// Execution timeline tracking all events during plan execution.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExecutionTimeline {
    /// All recorded events, in chronological order.
    pub events: Vec<TimelineEvent>,
}

impl ExecutionTimeline {
    /// Record a new event.
    pub fn record(&mut self, kind: TimelineEventKind) {
        self.events.push(TimelineEvent::new(kind));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_execution_config_defaults() {
        let config = PlanExecutionConfig::default();
        assert!(!config.step_by_step);
    }
}
