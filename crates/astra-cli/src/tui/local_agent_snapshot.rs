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
