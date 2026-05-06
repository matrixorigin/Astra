//! First-class working memory for goal and task continuity.
//!
//! This is the compact, prompt-facing state that must survive compaction and
//! checkpoint restore: original goal, progress summary, decisions, blockers,
//! and next action. Runtime can still provide richer side-channel state, but the
//! pipeline owns this distilled representation so goal retention is testable.

use serde::{Deserialize, Serialize};

use astra_services::session_workspace::GoalProgressSnapshot;

/// Session-scoped, prompt-facing working memory.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkingMemoryState {
    goal_progress: Option<GoalProgressSnapshot>,
    decisions: Vec<String>,
    blockers: Vec<String>,
    next_action: Option<String>,
}

impl WorkingMemoryState {
    /// Set or replace the original session goal.
    pub fn set_goal(&mut self, goal: impl Into<String>) {
        let goal = goal.into();
        if goal.trim().is_empty() {
            return;
        }
        self.goal_progress = Some(GoalProgressSnapshot {
            goal,
            completion_score: 0.0,
            momentum: 0.0,
            milestone_count: 0,
            summary: "No milestones recorded yet.".to_string(),
            weighted_progress: 0.0,
            negative_signals: 0.0,
            milestones: Vec::new(),
        });
    }

    /// Replace the goal progress snapshot, usually from [`crate::goal_tracker`].
    pub fn set_goal_progress(&mut self, snapshot: GoalProgressSnapshot) {
        if !snapshot.goal.trim().is_empty() {
            self.goal_progress = Some(snapshot);
        }
    }

    /// Current goal progress snapshot, if any.
    #[must_use]
    pub fn goal_progress(&self) -> Option<&GoalProgressSnapshot> {
        self.goal_progress.as_ref()
    }

    /// Add a stable decision. Duplicate entries are ignored.
    pub fn push_decision(&mut self, decision: impl Into<String>) {
        push_unique_capped(&mut self.decisions, decision.into(), 8);
    }

    /// Add a current blocker. Duplicate entries are ignored.
    pub fn push_blocker(&mut self, blocker: impl Into<String>) {
        push_unique_capped(&mut self.blockers, blocker.into(), 6);
    }

    /// Set the next action the assistant should resume with.
    pub fn set_next_action(&mut self, next_action: impl Into<String>) {
        let next_action = next_action.into();
        self.next_action = (!next_action.trim().is_empty()).then_some(next_action);
    }

    /// Whether the working memory would render an empty prompt section.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.goal_progress.is_none()
            && self.decisions.is_empty()
            && self.blockers.is_empty()
            && self.next_action.is_none()
    }

    /// Render a concise, stable prompt section.
    #[must_use]
    pub fn render_prompt_section(&self) -> String {
        if self.is_empty() {
            return String::new();
        }

        let mut out = String::from("## Working Memory\n");
        if let Some(goal) = &self.goal_progress {
            out.push_str(&format!("Goal: {}\n", goal.goal));
            if !goal.summary.is_empty() || goal.milestone_count > 0 {
                out.push_str(&format!(
                    "Progress: {} ({:.0}% complete, momentum {:.2}, {} milestones)\n",
                    if goal.summary.is_empty() {
                        "No summary"
                    } else {
                        &goal.summary
                    },
                    goal.completion_score.clamp(0.0, 1.0) * 100.0,
                    goal.momentum.clamp(-1.0, 1.0),
                    goal.milestone_count
                ));
            }
        }
        if !self.decisions.is_empty() {
            out.push_str("Decisions:\n");
            for decision in &self.decisions {
                out.push_str("- ");
                out.push_str(decision);
                out.push('\n');
            }
        }
        if !self.blockers.is_empty() {
            out.push_str("Blockers:\n");
            for blocker in &self.blockers {
                out.push_str("- ");
                out.push_str(blocker);
                out.push('\n');
            }
        }
        if let Some(next) = &self.next_action {
            out.push_str("Next action: ");
            out.push_str(next);
            out.push('\n');
        }
        out.trim_end().to_string()
    }
}

fn push_unique_capped(items: &mut Vec<String>, value: String, cap: usize) {
    let value = value.trim();
    if value.is_empty() || items.iter().any(|existing| existing == value) {
        return;
    }
    items.push(value.to_string());
    if items.len() > cap {
        let excess = items.len() - cap;
        items.drain(0..excess);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_empty_is_empty() {
        assert!(
            WorkingMemoryState::default()
                .render_prompt_section()
                .is_empty()
        );
    }

    #[test]
    fn render_goal_decisions_blockers_next_action() {
        let mut wm = WorkingMemoryState::default();
        wm.set_goal("ship context pipeline");
        wm.push_decision("Keep core deterministic.");
        wm.push_blocker("Need restore coverage.");
        wm.set_next_action("Run focused tests.");

        let rendered = wm.render_prompt_section();
        assert!(rendered.contains("## Working Memory"));
        assert!(rendered.contains("Goal: ship context pipeline"));
        assert!(rendered.contains("- Keep core deterministic."));
        assert!(rendered.contains("- Need restore coverage."));
        assert!(rendered.contains("Next action: Run focused tests."));
    }
}
