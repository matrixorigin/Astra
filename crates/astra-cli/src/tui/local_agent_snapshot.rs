//! Shared, bounded-cadence snapshot of the local dynamic-agent runtime.
//!
//! The TUI previously queried and cloned the same spawner history several
//! times on every 50 ms UI tick: once for background persistence, once for the
//! task switcher, and independently for Agent monitoring. Capturing one
//! snapshot lets all three projections observe the same generation while live
//! stream events continue to provide low-latency incremental updates.

use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Clone, Default)]
pub(crate) struct LocalAgentSnapshot {
    pub available: bool,
    pub agents: Vec<astra_turn_core::orchestration_types::SpawnedAgentInfo>,
    pub fanout_groups: Vec<astra_turn_core::orchestration_fanout_group::AgentFanoutGroupProjection>,
}

impl LocalAgentSnapshot {
    pub(crate) async fn capture(
        spawner: Option<&Arc<astra_runtime::orchestration::DynamicAgentSpawner>>,
    ) -> Self {
        let Some(spawner) = spawner else {
            return Self::default();
        };
        let (mut agents, fanout_groups) = tokio::join!(
            spawner.get_agent_history(None),
            spawner.list_fanout_groups()
        );
        agents.sort_by(|left, right| {
            left.started_at
                .cmp(&right.started_at)
                .then_with(|| left.agent_id.cmp(&right.agent_id))
        });
        Self {
            available: true,
            agents,
            fanout_groups,
        }
    }

    pub(crate) fn fanout_titles(&self) -> BTreeMap<String, String> {
        self.fanout_groups
            .iter()
            .map(|group| (group.group_id.clone(), group.title.clone()))
            .collect()
    }

    /// Return false only for a machine-owned child attention hint whose
    /// result was already collected by the active parent turn. The hint may
    /// have been queued just before `agent_fanout.get_results` returned; in
    /// that case replaying it after settlement would create a redundant idle
    /// model turn for a fact the parent has already consumed.
    pub(crate) fn notification_still_requires_reconciliation(&self, notification: &str) -> bool {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(notification) else {
            return true;
        };
        if value.get("schema").and_then(serde_json::Value::as_str)
            != Some("agent_attention_hint.v1")
            || value.get("event").and_then(serde_json::Value::as_str)
                != Some("agent_status_changed")
        {
            return true;
        }
        let Some(agent_id) = value.get("agent_id").and_then(serde_json::Value::as_str) else {
            return true;
        };
        let Some(run_id) = value.get("run_id").and_then(serde_json::Value::as_str) else {
            return true;
        };

        !self
            .fanout_groups
            .iter()
            .flat_map(|group| &group.slots)
            .any(|slot| {
                slot.result_collected
                    && slot.agent_id.as_deref() == Some(agent_id)
                    && slot.run_id.as_deref() == Some(run_id)
            })
    }

    /// Return only newly attention-worthy child transitions. Ordinary running
    /// progress stays in the task UI; terminal/waiting facts wake the parent
    /// exactly once when the snapshot crosses that lifecycle boundary.
    pub(crate) fn attention_notifications_since(&self, previous: &Self) -> Vec<String> {
        let previous_status = previous
            .agents
            .iter()
            .map(|agent| (agent.run_id.as_str(), &agent.status))
            .collect::<BTreeMap<_, _>>();
        self.agents
            .iter()
            .filter(|agent| {
                agent.status.is_terminal()
                    || matches!(
                        agent.status,
                        astra_turn_core::orchestration_types::AgentStatus::Waiting { .. }
                    )
            })
            .filter(|agent| {
                previous_status
                    .get(agent.run_id.as_str())
                    .is_none_or(|status| *status != &agent.status)
            })
            .map(|agent| {
                let (status, detail) = match &agent.status {
                    astra_turn_core::orchestration_types::AgentStatus::Completed {
                        result, ..
                    } => ("completed", result.as_str()),
                    astra_turn_core::orchestration_types::AgentStatus::Interrupted {
                        partial_result,
                        ..
                    } => ("interrupted", partial_result.as_str()),
                    astra_turn_core::orchestration_types::AgentStatus::Failed { error, .. } => {
                        ("failed", error.as_str())
                    }
                    astra_turn_core::orchestration_types::AgentStatus::Cancelled {
                        reason, ..
                    } => ("cancelled", reason.as_str()),
                    astra_turn_core::orchestration_types::AgentStatus::Waiting { reason } => {
                        ("needs_input", reason.as_str())
                    }
                    _ => unreachable!("filtered to attention-worthy states"),
                };
                // This snapshot transition is primarily a wake hint. The
                // authoritative child payload is delivered through the parent
                // mailbox; keep only a bounded preview as a fallback instead
                // of injecting the same multi-kilobyte result twice.
                let detail_preview = detail.chars().take(500).collect::<String>();
                serde_json::json!({
                    "schema": "agent_attention_hint.v1",
                    "event": "agent_status_changed",
                    "agent_id": agent.agent_id,
                    "run_id": agent.run_id,
                    "parent_run_id": agent.parent_run_id,
                    "description": agent.description,
                    "status": status,
                    "detail_preview": detail_preview,
                    "authoritative_delivery": "parent_mailbox",
                })
                .to_string()
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::LocalAgentSnapshot;
    use astra_turn_core::orchestration_fanout_group::{
        AgentFanoutGroupProjection, AgentFanoutSlotStatus,
    };

    #[test]
    fn collected_fanout_result_suppresses_only_its_stale_attention_hint() {
        let mut group = AgentFanoutGroupProjection::new("review", "Review", 1);
        group
            .record_spawn_accepted_with_run(0, "reviewer@run-review", Some("run-review".into()))
            .unwrap();
        group
            .record_terminal_by_agent(
                "reviewer@run-review",
                AgentFanoutSlotStatus::Completed,
                None,
            )
            .unwrap();
        let notification = serde_json::json!({
            "schema": "agent_attention_hint.v1",
            "event": "agent_status_changed",
            "agent_id": "reviewer@run-review",
            "run_id": "run-review",
        })
        .to_string();

        let uncollected = LocalAgentSnapshot {
            available: true,
            fanout_groups: vec![group.clone()],
            ..LocalAgentSnapshot::default()
        };
        assert!(uncollected.notification_still_requires_reconciliation(&notification));

        assert!(group.mark_result_collected("reviewer@run-review"));
        let collected = LocalAgentSnapshot {
            available: true,
            fanout_groups: vec![group],
            ..LocalAgentSnapshot::default()
        };
        assert!(!collected.notification_still_requires_reconciliation(&notification));
        assert!(collected.notification_still_requires_reconciliation(
            r#"{"schema":"agent_attention_hint.v1","agent_id":"other","run_id":"other"}"#,
        ));
        assert!(collected.notification_still_requires_reconciliation(
            "<task_notification>done</task_notification>"
        ));
    }
}
