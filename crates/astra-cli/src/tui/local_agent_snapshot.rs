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
}
