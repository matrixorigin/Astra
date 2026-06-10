//! Pure accounting model for sub-agent fanout groups.
//!
//! The runtime may still execute `agent(action='spawn')` as separate tool
//! calls, but user intent is a group with a fixed target count and slots. This
//! module keeps that invariant explicit so retries, failed spawns, user
//! cancellations, and budget cancellations do not inflate or blur the group.

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentFanoutSlotIdentity {
    pub group_id: String,
    pub target_count: usize,
    pub slot_index: usize,
}

impl AgentFanoutSlotIdentity {
    pub fn new(
        group_id: impl Into<String>,
        target_count: usize,
        slot_index: usize,
    ) -> Result<Self, String> {
        let group_id = group_id.into();
        let group_id = group_id.trim();
        if group_id.is_empty() {
            return Err("fanout metadata requires non-empty fanout_group_id".to_string());
        }
        if target_count == 0 {
            return Err(format!(
                "fanout group '{group_id}' requires fanout_target_count >= 1"
            ));
        }
        if slot_index >= target_count {
            return Err(format!(
                "fanout slot_index {slot_index} is outside target_count {target_count}"
            ));
        }
        Ok(Self {
            group_id: group_id.to_string(),
            target_count,
            slot_index,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentFanoutGroupProjection {
    pub group_id: String,
    pub title: String,
    pub target_count: usize,
    pub created_by_tool_use_id: Option<String>,
    pub slots: Vec<AgentFanoutSlot>,
    pub status: AgentFanoutStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentFanoutSlot {
    pub slot_index: usize,
    pub role: String,
    pub requested_description: String,
    pub agent_id: Option<String>,
    pub status: AgentFanoutSlotStatus,
    pub result_collected: bool,
    pub terminal_reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentFanoutStatus {
    Planned,
    Running,
    Finished,
    Incomplete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentFanoutSlotStatus {
    Planned,
    SpawnAccepted,
    SpawnRejected,
    Running,
    Completed,
    Failed,
    CancelledByUser,
    CancelledByParentBudget,
    TimedOut,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AgentFanoutSummary {
    pub target_count: usize,
    pub planned: usize,
    pub accepted: usize,
    pub active: usize,
    pub terminal: usize,
    pub completed: usize,
    pub failed: usize,
    pub cancelled_by_user: usize,
    pub cancelled_by_parent_budget: usize,
    pub timed_out: usize,
    pub spawn_rejected: usize,
    pub collected: usize,
    pub uncollected: usize,
}

impl AgentFanoutGroupProjection {
    pub fn new(group_id: impl Into<String>, title: impl Into<String>, target_count: usize) -> Self {
        let target_count = target_count.max(1);
        let slots = (0..target_count)
            .map(|idx| AgentFanoutSlot {
                slot_index: idx,
                role: String::new(),
                requested_description: String::new(),
                agent_id: None,
                status: AgentFanoutSlotStatus::Planned,
                result_collected: false,
                terminal_reason: None,
            })
            .collect();
        Self {
            group_id: group_id.into(),
            title: title.into(),
            target_count,
            created_by_tool_use_id: None,
            slots,
            status: AgentFanoutStatus::Planned,
        }
    }

    pub fn set_slot_request(
        &mut self,
        slot_index: usize,
        role: impl Into<String>,
        requested_description: impl Into<String>,
    ) -> Result<(), String> {
        let slot = self.slot_mut(slot_index)?;
        slot.role = role.into();
        slot.requested_description = requested_description.into();
        Ok(())
    }

    pub fn record_spawn_rejected(
        &mut self,
        slot_index: usize,
        reason: impl Into<String>,
    ) -> Result<(), String> {
        let slot = self.slot_mut(slot_index)?;
        if slot.agent_id.is_some() {
            return Err(format!(
                "fanout slot {slot_index} already has an accepted agent; reject cannot replace it"
            ));
        }
        slot.status = AgentFanoutSlotStatus::SpawnRejected;
        slot.terminal_reason = Some(reason.into());
        self.recompute_status();
        Ok(())
    }

    pub fn record_spawn_accepted(
        &mut self,
        slot_index: usize,
        agent_id: impl Into<String>,
    ) -> Result<(), String> {
        let agent_id = agent_id.into();
        let slot = self.slot_mut(slot_index)?;
        if let Some(existing) = slot.agent_id.as_ref() {
            return Err(format!(
                "fanout slot {slot_index} already accepted agent {existing}; explicit replacement is required"
            ));
        }
        slot.agent_id = Some(agent_id);
        slot.status = AgentFanoutSlotStatus::Running;
        slot.terminal_reason = None;
        self.recompute_status();
        Ok(())
    }

    pub fn mark_result_collected(&mut self, agent_id: &str) -> bool {
        let Some(slot) = self
            .slots
            .iter_mut()
            .find(|slot| slot.agent_id.as_deref() == Some(agent_id))
        else {
            return false;
        };
        if !slot.status.is_terminal() {
            return false;
        }
        slot.result_collected = true;
        true
    }

    pub fn record_terminal_by_agent(
        &mut self,
        agent_id: &str,
        status: AgentFanoutSlotStatus,
        reason: Option<String>,
    ) -> Result<(), String> {
        if !status.is_terminal() {
            return Err("fanout terminal update requires a terminal slot status".to_string());
        }
        let Some(slot) = self
            .slots
            .iter_mut()
            .find(|slot| slot.agent_id.as_deref() == Some(agent_id))
        else {
            return Err(format!("fanout agent {agent_id} is not assigned to a slot"));
        };
        slot.status = status;
        slot.terminal_reason = reason;
        self.recompute_status();
        Ok(())
    }

    pub fn summary(&self) -> AgentFanoutSummary {
        let mut summary = AgentFanoutSummary {
            target_count: self.target_count,
            planned: self.slots.len(),
            ..AgentFanoutSummary::default()
        };
        for slot in &self.slots {
            if slot.agent_id.is_some() {
                summary.accepted += 1;
            }
            if matches!(
                slot.status,
                AgentFanoutSlotStatus::Running | AgentFanoutSlotStatus::SpawnAccepted
            ) {
                summary.active += 1;
            }
            if slot.status.is_terminal() {
                summary.terminal += 1;
                if slot.agent_id.is_some() && !slot.result_collected {
                    summary.uncollected += 1;
                }
            }
            if slot.result_collected {
                summary.collected += 1;
            }
            match slot.status {
                AgentFanoutSlotStatus::Completed => summary.completed += 1,
                AgentFanoutSlotStatus::Failed => summary.failed += 1,
                AgentFanoutSlotStatus::CancelledByUser => summary.cancelled_by_user += 1,
                AgentFanoutSlotStatus::CancelledByParentBudget => {
                    summary.cancelled_by_parent_budget += 1;
                }
                AgentFanoutSlotStatus::TimedOut => summary.timed_out += 1,
                AgentFanoutSlotStatus::SpawnRejected => summary.spawn_rejected += 1,
                AgentFanoutSlotStatus::Planned
                | AgentFanoutSlotStatus::SpawnAccepted
                | AgentFanoutSlotStatus::Running => {}
            }
        }
        summary
    }

    pub fn summary_sentence(&self) -> String {
        let summary = self.summary();
        let label = if summary.spawn_rejected > 0 {
            "fanout failed to start fully"
        } else if summary.cancelled_by_parent_budget > 0 {
            "fanout interrupted by budget"
        } else if summary.active > 0 && summary.terminal > 0 {
            "fanout incomplete"
        } else {
            match self.status {
                AgentFanoutStatus::Finished => "fanout finished",
                AgentFanoutStatus::Incomplete => "fanout incomplete",
                AgentFanoutStatus::Running => "fanout running",
                AgentFanoutStatus::Planned => "fanout planned",
            }
        };
        let parts = summary_sentence_parts(label, summary);
        format!(
            "{}-agent {}: {}.",
            summary.target_count,
            label,
            parts.join(", ")
        )
    }

    fn slot_mut(&mut self, slot_index: usize) -> Result<&mut AgentFanoutSlot, String> {
        self.slots.get_mut(slot_index).ok_or_else(|| {
            format!(
                "fanout slot {slot_index} is outside target_count {}",
                self.target_count
            )
        })
    }

    fn recompute_status(&mut self) {
        let summary = self.summary();
        self.status = if summary.terminal == self.target_count {
            AgentFanoutStatus::Finished
        } else if summary.active > 0 {
            AgentFanoutStatus::Running
        } else if summary.terminal > 0 {
            AgentFanoutStatus::Incomplete
        } else {
            AgentFanoutStatus::Planned
        };
    }
}

impl AgentFanoutSlotStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed
                | Self::Failed
                | Self::CancelledByUser
                | Self::CancelledByParentBudget
                | Self::TimedOut
                | Self::SpawnRejected
        )
    }
}

fn format_count(count: usize, label: &str) -> String {
    format!("{count} {label}")
}

fn summary_sentence_parts(label: &str, summary: AgentFanoutSummary) -> Vec<String> {
    if label == "fanout failed to start fully" {
        let mut parts = Vec::new();
        if summary.accepted > 0 {
            parts.push(format_count(summary.accepted, "accepted"));
        }
        if summary.spawn_rejected > 0 {
            parts.push(format_count(summary.spawn_rejected, "spawn rejected"));
        }
        if summary.uncollected > 0 {
            parts.push(format_count(summary.uncollected, "uncollected"));
        }
        return parts;
    }

    let mut parts = Vec::new();
    if summary.completed > 0 {
        parts.push(format_count(summary.completed, "completed"));
    }
    if summary.cancelled_by_user > 0 {
        parts.push(format_count(summary.cancelled_by_user, "stopped by user"));
    }
    if summary.cancelled_by_parent_budget > 0 {
        parts.push(format_count(
            summary.cancelled_by_parent_budget,
            "cancelled by parent budget",
        ));
    }
    if summary.failed > 0 {
        parts.push(format_count(summary.failed, "failed"));
    }
    if summary.spawn_rejected > 0 {
        parts.push(format_count(summary.spawn_rejected, "spawn rejected"));
    }
    if summary.timed_out > 0 {
        parts.push(format_count(summary.timed_out, "timed out"));
    }
    if summary.active > 0 {
        parts.push(format_count(summary.active, "still running"));
    }
    if summary.uncollected > 0 {
        parts.push(format_count(summary.uncollected, "uncollected"));
    }
    if parts.is_empty() {
        parts.push(format_count(summary.planned, "planned"));
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_target_count_creates_fixed_slots() {
        let group = AgentFanoutGroupProjection::new("review-1", "Review fanout", 3);

        assert_eq!(group.target_count, 3);
        assert_eq!(group.slots.len(), 3);
        assert_eq!(group.slots[2].slot_index, 2);
    }

    #[test]
    fn spawn_reject_does_not_inflate_target_and_allows_same_slot_retry() {
        let mut group = AgentFanoutGroupProjection::new("review-1", "Review fanout", 3);

        group.record_spawn_rejected(1, "model denied").unwrap();
        assert_eq!(group.summary().spawn_rejected, 1);
        assert_eq!(group.target_count, 3);
        assert_eq!(group.slots.len(), 3);

        group.record_spawn_accepted(1, "storage@abc").unwrap();
        let summary = group.summary();
        assert_eq!(summary.target_count, 3);
        assert_eq!(summary.accepted, 1);
        assert_eq!(summary.spawn_rejected, 0);
        assert_eq!(group.slots[1].agent_id.as_deref(), Some("storage@abc"));
    }

    #[test]
    fn accepted_slot_rejects_silent_replacement() {
        let mut group = AgentFanoutGroupProjection::new("review-1", "Review fanout", 3);
        group.record_spawn_accepted(1, "storage@abc").unwrap();

        let err = group
            .record_spawn_accepted(1, "storage@replacement")
            .expect_err("accepted slot must not be silently replaced");
        assert!(err.contains("explicit replacement"), "{err}");
        assert_eq!(group.summary().accepted, 1);
        assert_eq!(group.target_count, 3);
    }

    #[test]
    fn user_cancel_counts_as_intentional_not_missing() {
        let mut group = AgentFanoutGroupProjection::new("review-1", "Review fanout", 3);
        group.record_spawn_accepted(0, "auth@aaa").unwrap();
        group.record_spawn_accepted(1, "storage@bbb").unwrap();
        group.record_spawn_accepted(2, "api@ccc").unwrap();

        group
            .record_terminal_by_agent("auth@aaa", AgentFanoutSlotStatus::Completed, None)
            .unwrap();
        group
            .record_terminal_by_agent(
                "storage@bbb",
                AgentFanoutSlotStatus::CancelledByUser,
                Some("Ctrl+G x".to_string()),
            )
            .unwrap();
        group
            .record_terminal_by_agent("api@ccc", AgentFanoutSlotStatus::Completed, None)
            .unwrap();

        let summary = group.summary();
        assert_eq!(summary.completed, 2);
        assert_eq!(summary.cancelled_by_user, 1);
        assert_eq!(summary.active, 0);
        assert_eq!(group.status, AgentFanoutStatus::Finished);
        let sentence = group.summary_sentence();
        assert!(sentence.contains("2 completed"), "{sentence}");
        assert!(sentence.contains("1 stopped by user"), "{sentence}");
        assert!(!sentence.contains("partial agents returned"), "{sentence}");
    }

    #[test]
    fn parent_budget_cancel_is_distinct_from_user_cancel() {
        let mut group = AgentFanoutGroupProjection::new("review-1", "Review fanout", 3);
        group.record_spawn_accepted(0, "auth@aaa").unwrap();
        group.record_spawn_accepted(1, "storage@bbb").unwrap();
        group.record_spawn_accepted(2, "api@ccc").unwrap();

        group
            .record_terminal_by_agent("auth@aaa", AgentFanoutSlotStatus::Completed, None)
            .unwrap();
        group
            .record_terminal_by_agent(
                "storage@bbb",
                AgentFanoutSlotStatus::CancelledByParentBudget,
                Some("turn budget exhausted".to_string()),
            )
            .unwrap();

        let summary = group.summary();
        assert_eq!(summary.completed, 1);
        assert_eq!(summary.cancelled_by_parent_budget, 1);
        assert_eq!(summary.cancelled_by_user, 0);
        assert_eq!(summary.active, 1);
        let sentence = group.summary_sentence();
        assert!(
            sentence.contains("1 cancelled by parent budget"),
            "{sentence}"
        );
        assert!(!sentence.contains("stopped by user"), "{sentence}");
    }

    #[test]
    fn summary_sentence_names_parent_budget_interrupt() {
        let mut group = AgentFanoutGroupProjection::new("review-1", "Review fanout", 3);
        group.record_spawn_accepted(0, "auth@aaa").unwrap();
        group.record_spawn_accepted(1, "storage@bbb").unwrap();
        group.record_spawn_accepted(2, "api@ccc").unwrap();

        group
            .record_terminal_by_agent("auth@aaa", AgentFanoutSlotStatus::Completed, None)
            .unwrap();
        group
            .record_terminal_by_agent(
                "storage@bbb",
                AgentFanoutSlotStatus::CancelledByParentBudget,
                Some("turn budget exhausted".to_string()),
            )
            .unwrap();
        group
            .record_terminal_by_agent(
                "api@ccc",
                AgentFanoutSlotStatus::CancelledByParentBudget,
                Some("turn budget exhausted".to_string()),
            )
            .unwrap();

        assert_eq!(
            group.summary_sentence(),
            "3-agent fanout interrupted by budget: 1 completed, 2 cancelled by parent budget, 3 uncollected."
        );
    }

    #[test]
    fn summary_sentence_names_spawn_rejected_start_failure() {
        let mut group = AgentFanoutGroupProjection::new("review-1", "Review fanout", 3);
        group.record_spawn_accepted(0, "auth@aaa").unwrap();
        group.record_spawn_accepted(2, "api@ccc").unwrap();
        group
            .record_spawn_rejected(1, "concurrency cap reached")
            .unwrap();

        assert_eq!(
            group.summary_sentence(),
            "3-agent fanout failed to start fully: 2 accepted, 1 spawn rejected."
        );
    }

    #[test]
    fn summary_sentence_names_partial_running_as_incomplete() {
        let mut group = AgentFanoutGroupProjection::new("review-1", "Review fanout", 3);
        group.record_spawn_accepted(0, "auth@aaa").unwrap();
        group.record_spawn_accepted(1, "storage@bbb").unwrap();
        group.record_spawn_accepted(2, "api@ccc").unwrap();
        group
            .record_terminal_by_agent("auth@aaa", AgentFanoutSlotStatus::Completed, None)
            .unwrap();
        group
            .record_terminal_by_agent("storage@bbb", AgentFanoutSlotStatus::Completed, None)
            .unwrap();

        assert_eq!(
            group.summary_sentence(),
            "3-agent fanout incomplete: 2 completed, 1 still running, 2 uncollected."
        );
    }

    #[test]
    fn result_collection_is_tracked_separately_from_terminal_status() {
        let mut group = AgentFanoutGroupProjection::new("review-1", "Review fanout", 2);
        group.record_spawn_accepted(0, "auth@aaa").unwrap();
        group.record_spawn_accepted(1, "api@bbb").unwrap();
        group
            .record_terminal_by_agent("auth@aaa", AgentFanoutSlotStatus::Completed, None)
            .unwrap();

        assert!(group.mark_result_collected("auth@aaa"));
        assert!(!group.mark_result_collected("missing@id"));
        let summary = group.summary();
        assert_eq!(summary.completed, 1);
        assert_eq!(summary.collected, 1);
        assert_eq!(summary.uncollected, 0);
        assert_eq!(summary.active, 1);
    }

    #[test]
    fn running_slots_cannot_be_marked_result_collected() {
        let mut group = AgentFanoutGroupProjection::new("review-1", "Review fanout", 2);
        group.record_spawn_accepted(0, "auth@aaa").unwrap();

        assert!(
            !group.mark_result_collected("auth@aaa"),
            "result collection must only apply after a slot is terminal"
        );
        let summary = group.summary();
        assert_eq!(summary.active, 1);
        assert_eq!(summary.collected, 0);
        assert_eq!(summary.uncollected, 0);
    }

    #[test]
    fn uncollected_terminal_slots_are_first_class_summary_state() {
        let mut group = AgentFanoutGroupProjection::new("review-1", "Review fanout", 2);
        group.record_spawn_accepted(0, "auth@aaa").unwrap();
        group.record_spawn_accepted(1, "api@bbb").unwrap();
        group
            .record_terminal_by_agent("auth@aaa", AgentFanoutSlotStatus::Completed, None)
            .unwrap();
        group
            .record_terminal_by_agent("api@bbb", AgentFanoutSlotStatus::Failed, None)
            .unwrap();

        let summary = group.summary();
        assert_eq!(summary.terminal, 2);
        assert_eq!(summary.collected, 0);
        assert_eq!(summary.uncollected, 2);
        assert_eq!(
            group.summary_sentence(),
            "2-agent fanout finished: 1 completed, 1 failed, 2 uncollected."
        );

        group.mark_result_collected("auth@aaa");
        let summary = group.summary();
        assert_eq!(summary.collected, 1);
        assert_eq!(summary.uncollected, 1);
        assert_eq!(
            group.summary_sentence(),
            "2-agent fanout finished: 1 completed, 1 failed, 1 uncollected."
        );
    }

    #[test]
    fn spawn_rejected_slots_are_terminal_but_not_uncollected_results() {
        let mut group = AgentFanoutGroupProjection::new("review-1", "Review fanout", 2);
        group.record_spawn_accepted(0, "auth@aaa").unwrap();
        group
            .record_terminal_by_agent("auth@aaa", AgentFanoutSlotStatus::Completed, None)
            .unwrap();
        group.record_spawn_rejected(1, "quota").unwrap();

        let summary = group.summary();
        assert_eq!(summary.terminal, 2);
        assert_eq!(summary.spawn_rejected, 1);
        assert_eq!(summary.uncollected, 1);
    }
}
