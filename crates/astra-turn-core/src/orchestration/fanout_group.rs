//! Pure accounting model for sub-agent fanout groups.
//!
//! The runtime may still execute `agent(action='spawn')` as separate tool
//! calls, but user intent is a group with a fixed target count and slots. This
//! module keeps that invariant explicit so retries, failed spawns, user
//! cancellations, and budget cancellations do not inflate or blur the group.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::SystemTime;

use astra_core::work_unit::WorkUnitStatus;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentFanoutSlotIdentity {
    pub group_id: String,
    pub target_count: usize,
    pub slot_index: usize,
    pub slot_id: Option<String>,
}

impl AgentFanoutSlotIdentity {
    pub fn new(
        group_id: impl Into<String>,
        target_count: usize,
        slot_index: usize,
        slot_id: Option<String>,
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
        let slot_id = match slot_id {
            Some(slot_id) => {
                let slot_id = slot_id.trim();
                if slot_id.is_empty() {
                    return Err("fanout metadata requires non-empty fanout_slot_id".to_string());
                }
                Some(slot_id.to_string())
            }
            None => None,
        };
        Ok(Self {
            group_id: group_id.to_string(),
            target_count,
            slot_index,
            slot_id,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentFanoutGroupProjection {
    pub group_id: String,
    pub title: String,
    pub target_count: usize,
    pub created_by_tool_use_id: Option<String>,
    pub parent_run_id: Option<String>,
    pub slots: Vec<AgentFanoutSlot>,
    pub status: AgentFanoutStatus,
    /// Producer-owned material-state revision. Reads and LRU touches never
    /// change it; every accepted projection mutation does.
    pub revision: u64,
    /// Monotonic timestamp of last mutation or access.  Used for
    /// LRU eviction when the fanout-groups map exceeds its cap.
    pub last_touched: SystemTime,
    summary_cache: AgentFanoutSummary,
    agent_slot_index: HashMap<String, usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentFanoutSlot {
    pub slot_index: usize,
    pub slot_id: Option<String>,
    pub role: String,
    pub requested_description: String,
    pub agent_id: Option<String>,
    /// Immutable execution identity assigned when the child spawn is accepted.
    /// This keeps a recovered fanout receipt able to reopen the exact child
    /// conversation when the original tool-result delivery was lost.
    pub run_id: Option<String>,
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

impl AgentFanoutStatus {
    /// Lowercase snake_case label for JSON/API output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Running => "running",
            Self::Finished => "finished",
            Self::Incomplete => "incomplete",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentFanoutSlotStatus {
    Planned,
    SpawnAccepted,
    SpawnRejected,
    Running,
    Completed,
    Interrupted,
    Failed,
    CancelledByUser,
    CancelledByParentBudget,
    TimedOut,
}

impl AgentFanoutSlotStatus {
    /// Lowercase snake_case label for JSON/API output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::SpawnAccepted => "spawn_accepted",
            Self::SpawnRejected => "spawn_rejected",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Interrupted => "interrupted",
            Self::Failed => "failed",
            Self::CancelledByUser => "cancelled_by_user",
            Self::CancelledByParentBudget => "cancelled_by_parent_budget",
            Self::TimedOut => "timed_out",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AgentFanoutSummary {
    pub target_count: usize,
    pub planned: usize,
    pub accepted: usize,
    pub active: usize,
    pub terminal: usize,
    pub completed: usize,
    pub interrupted: usize,
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
                slot_id: None,
                role: String::new(),
                requested_description: String::new(),
                agent_id: None,
                run_id: None,
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
            parent_run_id: None,
            slots,
            status: AgentFanoutStatus::Planned,
            revision: 0,
            last_touched: SystemTime::now(),
            summary_cache: AgentFanoutSummary {
                target_count,
                planned: target_count,
                ..AgentFanoutSummary::default()
            },
            agent_slot_index: HashMap::new(),
        }
    }

    /// Bump the `last_touched` timestamp.  Call on every access path
    /// that reads or mutates the group so LRU eviction has a consistent
    /// ordering signal.
    pub fn touch(&mut self) {
        self.last_touched = SystemTime::now();
    }

    /// True when the fixed-size group is settled: every slot has either
    /// reached a terminal status or the group can no longer make progress.
    /// Planned slots are not terminal; they still belong to the launch
    /// contract and must be allowed to transition.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.status,
            AgentFanoutStatus::Finished | AgentFanoutStatus::Incomplete
        )
    }

    /// Canonical lifecycle projection shared by runtime, CLI, and UI lanes.
    ///
    /// A fanout is one fixed-size work unit. Individual child failures,
    /// interruptions, cancellations, timeouts, or rejected spawns therefore
    /// settle the group *with issues*; they do not turn the same group into
    /// `failed` on one surface and `completed` on another. Consumers must use
    /// this producer-owned projection instead of re-counting slot causes.
    pub fn work_unit_status(&self) -> WorkUnitStatus {
        if !self.is_terminal() {
            return if self.summary_cache.active > 0 {
                WorkUnitStatus::Running
            } else {
                WorkUnitStatus::Pending
            };
        }
        if self.summary_cache.completed == self.target_count {
            WorkUnitStatus::Completed
        } else {
            WorkUnitStatus::CompletedWithIssues
        }
    }

    pub fn set_slot_request(
        &mut self,
        slot_index: usize,
        slot_id: Option<String>,
        role: impl Into<String>,
        requested_description: impl Into<String>,
    ) -> Result<(), String> {
        let slot = self.slot_mut(slot_index)?;
        slot.slot_id = slot_id;
        slot.role = role.into();
        slot.requested_description = requested_description.into();
        self.revision = self.revision.saturating_add(1);
        Ok(())
    }

    pub fn record_spawn_rejected(
        &mut self,
        slot_index: usize,
        reason: impl Into<String>,
    ) -> Result<(), String> {
        let old_status = {
            let slot = self.slot_mut(slot_index)?;
            if slot.agent_id.is_some() {
                return Err(format!(
                    "fanout slot {slot_index} already has an accepted agent; reject cannot replace it"
                ));
            }
            let old_status = slot.status;
            slot.status = AgentFanoutSlotStatus::SpawnRejected;
            slot.terminal_reason = Some(reason.into());
            old_status
        };
        self.apply_slot_status_transition(
            old_status,
            AgentFanoutSlotStatus::SpawnRejected,
            false,
            false,
        );
        self.recompute_status_from_cache();
        self.revision = self.revision.saturating_add(1);
        Ok(())
    }

    pub fn record_spawn_accepted(
        &mut self,
        slot_index: usize,
        agent_id: impl Into<String>,
    ) -> Result<(), String> {
        self.record_spawn_accepted_with_run(slot_index, agent_id, None)
    }

    /// Record an accepted child with its immutable execution identity.
    /// Runtime callers use this form because an accepted fanout slot is a
    /// durable conversation address, not merely a display label.
    pub fn record_spawn_accepted_with_run(
        &mut self,
        slot_index: usize,
        agent_id: impl Into<String>,
        run_id: Option<String>,
    ) -> Result<(), String> {
        let agent_id = agent_id.into();
        let old_status = {
            let slot = self.slot_mut(slot_index)?;
            if let Some(existing) = slot.agent_id.as_ref() {
                return Err(format!(
                    "fanout slot {slot_index} already accepted agent {existing}; explicit replacement is required"
                ));
            }
            let old_status = slot.status;
            slot.agent_id = Some(agent_id.clone());
            slot.run_id = run_id;
            slot.status = AgentFanoutSlotStatus::Running;
            slot.terminal_reason = None;
            old_status
        };
        self.summary_cache.accepted += 1;
        self.apply_slot_status_transition(old_status, AgentFanoutSlotStatus::Running, true, false);
        self.agent_slot_index.insert(agent_id, slot_index);
        self.recompute_status_from_cache();
        self.revision = self.revision.saturating_add(1);
        Ok(())
    }

    pub fn mark_result_collected(&mut self, agent_id: &str) -> bool {
        let Some(slot_index) = self.agent_slot_index.get(agent_id).copied() else {
            return false;
        };
        let Some(slot) = self.slots.get_mut(slot_index) else {
            self.agent_slot_index.remove(agent_id);
            return false;
        };
        if !slot.status.is_terminal() {
            return false;
        }
        if !slot.result_collected {
            slot.result_collected = true;
            self.summary_cache.collected += 1;
            self.summary_cache.uncollected = self.summary_cache.uncollected.saturating_sub(1);
            self.revision = self.revision.saturating_add(1);
        }
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
        let Some(slot_index) = self.agent_slot_index.get(agent_id).copied() else {
            return Err(format!("fanout agent {agent_id} is not assigned to a slot"));
        };
        let Some(slot) = self.slots.get_mut(slot_index) else {
            self.agent_slot_index.remove(agent_id);
            return Err(format!("fanout agent {agent_id} is not assigned to a slot"));
        };
        if slot.status.is_terminal() {
            return Err(format!(
                "fanout agent {agent_id} already recorded terminal status {:?}",
                slot.status
            ));
        }
        let (old_status, result_collected) = {
            let old_status = slot.status;
            slot.status = status;
            slot.terminal_reason = reason;
            (old_status, slot.result_collected)
        };
        self.apply_slot_status_transition(old_status, status, true, result_collected);
        self.recompute_status_from_cache();
        self.revision = self.revision.saturating_add(1);
        Ok(())
    }

    pub fn summary(&self) -> AgentFanoutSummary {
        self.summary_cache
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

    fn apply_slot_status_transition(
        &mut self,
        old_status: AgentFanoutSlotStatus,
        new_status: AgentFanoutSlotStatus,
        has_agent: bool,
        result_collected: bool,
    ) {
        adjust_summary_for_status(
            &mut self.summary_cache,
            old_status,
            has_agent,
            result_collected,
            -1,
        );
        adjust_summary_for_status(
            &mut self.summary_cache,
            new_status,
            has_agent,
            result_collected,
            1,
        );
    }

    fn recompute_status_from_cache(&mut self) {
        let summary = self.summary_cache;
        self.status = if summary.terminal == self.target_count {
            AgentFanoutStatus::Finished
        } else if summary.active > 0 || (summary.terminal > 0 && summary.planned > 0) {
            AgentFanoutStatus::Running
        } else if summary.terminal > 0 {
            AgentFanoutStatus::Incomplete
        } else {
            AgentFanoutStatus::Planned
        };
    }
}

fn adjust_summary_value(value: &mut usize, delta: i8) {
    match delta.cmp(&0) {
        std::cmp::Ordering::Greater => *value += delta as usize,
        std::cmp::Ordering::Less => *value = value.saturating_sub((-delta) as usize),
        std::cmp::Ordering::Equal => {}
    }
}

fn adjust_summary_for_status(
    summary: &mut AgentFanoutSummary,
    status: AgentFanoutSlotStatus,
    has_agent: bool,
    result_collected: bool,
    delta: i8,
) {
    if matches!(status, AgentFanoutSlotStatus::Planned) {
        adjust_summary_value(&mut summary.planned, delta);
    }
    if matches!(
        status,
        AgentFanoutSlotStatus::Running | AgentFanoutSlotStatus::SpawnAccepted
    ) {
        adjust_summary_value(&mut summary.active, delta);
    }
    if status.is_terminal() {
        adjust_summary_value(&mut summary.terminal, delta);
        if has_agent && !result_collected {
            adjust_summary_value(&mut summary.uncollected, delta);
        }
    }
    match status {
        AgentFanoutSlotStatus::Completed => adjust_summary_value(&mut summary.completed, delta),
        AgentFanoutSlotStatus::Interrupted => {
            adjust_summary_value(&mut summary.interrupted, delta);
        }
        AgentFanoutSlotStatus::Failed => adjust_summary_value(&mut summary.failed, delta),
        AgentFanoutSlotStatus::CancelledByUser => {
            adjust_summary_value(&mut summary.cancelled_by_user, delta);
        }
        AgentFanoutSlotStatus::CancelledByParentBudget => {
            adjust_summary_value(&mut summary.cancelled_by_parent_budget, delta);
        }
        AgentFanoutSlotStatus::TimedOut => adjust_summary_value(&mut summary.timed_out, delta),
        AgentFanoutSlotStatus::SpawnRejected => {
            adjust_summary_value(&mut summary.spawn_rejected, delta);
        }
        AgentFanoutSlotStatus::Planned
        | AgentFanoutSlotStatus::SpawnAccepted
        | AgentFanoutSlotStatus::Running => {}
    }
}

impl AgentFanoutSlotStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed
                | Self::Interrupted
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
    if summary.interrupted > 0 {
        parts.push(format_count(summary.interrupted, "interrupted"));
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
    fn revision_changes_only_for_material_projection_mutations() {
        let mut group = AgentFanoutGroupProjection::new("review-1", "Review fanout", 1);
        assert_eq!(group.revision, 0);

        group.touch();
        assert_eq!(group.revision, 0, "reads and LRU touches are not progress");

        group
            .set_slot_request(0, None, "reviewer", "Review auth")
            .unwrap();
        assert_eq!(group.revision, 1);
        group.record_spawn_accepted(0, "reviewer@run-1").unwrap();
        assert_eq!(group.revision, 2);
        group
            .record_terminal_by_agent("reviewer@run-1", AgentFanoutSlotStatus::Completed, None)
            .unwrap();
        assert_eq!(group.revision, 3);
        assert!(group.mark_result_collected("reviewer@run-1"));
        assert_eq!(group.revision, 4);
        assert!(group.mark_result_collected("reviewer@run-1"));
        assert_eq!(
            group.revision, 4,
            "idempotent collection cannot manufacture progress"
        );
    }

    #[test]
    fn spawn_reject_does_not_inflate_target_and_allows_same_slot_retry() {
        let mut group = AgentFanoutGroupProjection::new("review-1", "Review fanout", 3);

        group.record_spawn_rejected(1, "model denied").unwrap();
        let summary = group.summary();
        assert_eq!(summary.spawn_rejected, 1);
        assert_eq!(summary.planned, 2);
        assert_eq!(summary.terminal, 1);
        assert_eq!(group.target_count, 3);
        assert_eq!(group.slots.len(), 3);
        assert_eq!(group.status, AgentFanoutStatus::Running);
        assert!(
            !group.is_terminal(),
            "unattempted planned slots mean the fixed fanout group is still launchable"
        );

        group.record_spawn_accepted(1, "storage@abc").unwrap();
        let summary = group.summary();
        assert_eq!(summary.target_count, 3);
        assert_eq!(summary.accepted, 1);
        assert_eq!(summary.spawn_rejected, 0);
        assert_eq!(summary.planned, 2);
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
    fn accepted_slot_retains_its_canonical_run_identity() {
        let mut group = AgentFanoutGroupProjection::new("review-1", "Review fanout", 1);

        group
            .record_spawn_accepted_with_run(0, "storage@agent-run-1", Some("agent-run-1".into()))
            .unwrap();

        assert_eq!(
            group.slots[0].agent_id.as_deref(),
            Some("storage@agent-run-1")
        );
        assert_eq!(group.slots[0].run_id.as_deref(), Some("agent-run-1"));
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
    fn work_unit_status_is_one_canonical_projection_for_every_slot_outcome() {
        let planned = AgentFanoutGroupProjection::new("planned", "Planned", 1);
        assert_eq!(planned.work_unit_status(), WorkUnitStatus::Pending);

        let mut completed = AgentFanoutGroupProjection::new("completed", "Completed", 1);
        completed.record_spawn_accepted(0, "reviewer").unwrap();
        assert_eq!(completed.work_unit_status(), WorkUnitStatus::Running);
        completed
            .record_terminal_by_agent("reviewer", AgentFanoutSlotStatus::Completed, None)
            .unwrap();
        assert_eq!(completed.work_unit_status(), WorkUnitStatus::Completed);

        for (index, terminal_status) in [
            AgentFanoutSlotStatus::Interrupted,
            AgentFanoutSlotStatus::Failed,
            AgentFanoutSlotStatus::CancelledByUser,
            AgentFanoutSlotStatus::CancelledByParentBudget,
            AgentFanoutSlotStatus::TimedOut,
        ]
        .into_iter()
        .enumerate()
        {
            let agent_id = format!("reviewer-{index}");
            let mut group =
                AgentFanoutGroupProjection::new(format!("issue-{index}"), "Terminal issue", 1);
            group.record_spawn_accepted(0, &agent_id).unwrap();
            group
                .record_terminal_by_agent(&agent_id, terminal_status, Some("cause".into()))
                .unwrap();
            assert_eq!(
                group.work_unit_status(),
                WorkUnitStatus::CompletedWithIssues,
                "{terminal_status:?} must not change meaning across consumers"
            );
        }

        let mut rejected = AgentFanoutGroupProjection::new("rejected", "Rejected", 1);
        rejected.record_spawn_rejected(0, "quota").unwrap();
        assert_eq!(
            rejected.work_unit_status(),
            WorkUnitStatus::CompletedWithIssues
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
    fn pending_planned_slots_keep_group_launchable_after_terminal_slot() {
        let mut group = AgentFanoutGroupProjection::new("review-1", "Review fanout", 2);
        group.record_spawn_accepted(0, "auth@aaa").unwrap();
        group
            .record_terminal_by_agent("auth@aaa", AgentFanoutSlotStatus::Completed, None)
            .unwrap();

        let summary = group.summary();
        assert_eq!(summary.terminal, 1);
        assert_eq!(summary.active, 0);
        assert_eq!(summary.planned, 1);
        assert_eq!(group.status, AgentFanoutStatus::Running);
        assert!(
            !group.is_terminal(),
            "planned slots have not been attempted, so the group must still accept them"
        );

        group.record_spawn_accepted(1, "storage@bbb").unwrap();
        assert_eq!(group.summary().planned, 0);
        assert_eq!(group.summary().active, 1);
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

    #[test]
    fn repeated_terminal_or_collection_updates_do_not_double_count_summary() {
        let mut group = AgentFanoutGroupProjection::new("review-1", "Review fanout", 2);
        group.record_spawn_accepted(0, "auth@aaa").unwrap();
        group.record_spawn_accepted(1, "api@bbb").unwrap();
        group
            .record_terminal_by_agent("auth@aaa", AgentFanoutSlotStatus::Completed, None)
            .unwrap();
        assert!(
            group
                .record_terminal_by_agent("auth@aaa", AgentFanoutSlotStatus::Failed, None)
                .is_err()
        );
        assert!(group.mark_result_collected("auth@aaa"));
        assert!(group.mark_result_collected("auth@aaa"));

        let summary = group.summary();
        assert_eq!(summary.accepted, 2);
        assert_eq!(summary.active, 1);
        assert_eq!(summary.terminal, 1);
        assert_eq!(summary.completed, 1);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.collected, 1);
        assert_eq!(summary.uncollected, 0);
    }
}
