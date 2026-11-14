//! Shared plan state persisted and mirrored across runtime and CLI flows.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{TaskPlan, TaskStatus};

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
        } else if self
            .plan
            .subtasks
            .iter()
            .any(|task| task.status != TaskStatus::Pending)
        {
            // Any step that has left Pending (running, paused, finished or
            // failed) means execution started; never fall back to a draft.
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
        let slug: String = goal
            .split_whitespace()
            .take(3)
            .collect::<Vec<_>>()
            .join("-")
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '-')
            .take(30)
            .collect();

        // Random UUIDs avoid same-second collisions across concurrent sessions and
        // independent server replicas. Keep the human-readable goal prefix.
        let unique = uuid::Uuid::new_v4().simple();
        if slug.is_empty() {
            format!("plan-{unique}")
        } else {
            format!("{}-{unique}", slug.to_lowercase())
        }
    }
}

#[cfg(test)]
mod plan_id_regression_tests {
    use super::PlanModeState;

    #[test]
    fn repeated_goal_generates_distinct_ids_without_sleeping() {
        let ids: std::collections::HashSet<_> = (0..128)
            .map(|_| PlanModeState::generate_plan_id("Refactor the parser"))
            .collect();
        assert_eq!(ids.len(), 128);
        assert!(ids.iter().all(|id| id.starts_with("refactor-the-parser-")));
    }

    #[test]
    fn empty_goal_still_generates_unique_ids() {
        let a = PlanModeState::generate_plan_id("");
        let b = PlanModeState::generate_plan_id("");
        assert_ne!(a, b);
        assert!(a.starts_with("plan-"));
    }
}
