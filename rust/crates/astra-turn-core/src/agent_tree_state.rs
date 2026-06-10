//! Live agent progress tree state.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, SystemTime};

use crate::orchestration_progress::{AgentProgressEvent, ProgressEventType};
use crate::orchestration_types::{AgentStatus, SpawnedAgentInfo, SpawnedAgentMetrics};

const MAX_AGENT_RECORDS: usize = 256;

#[derive(Debug, Clone)]
pub struct AgentTreeSnapshot {
    pub roots: Vec<SpawnedAgentInfo>,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    pub failed_agents: usize,
}

#[derive(Debug, Clone)]
struct AgentTreeRecord {
    info: SpawnedAgentInfo,
}

#[derive(Debug, Default, Clone)]
pub struct AgentTreeState {
    agents: HashMap<String, AgentTreeRecord>,
    run_id_by_agent_id: HashMap<String, String>,
}

impl AgentTreeState {
    pub fn apply(&mut self, event: AgentProgressEvent) {
        let now = SystemTime::UNIX_EPOCH + Duration::from_millis(event.timestamp_epoch_ms);
        match event.event_type {
            ProgressEventType::AgentSpawned {
                run_id,
                parent_run_id,
                agent_type,
                description,
                fanout_slot,
            } => {
                self.run_id_by_agent_id
                    .insert(event.agent_id.clone(), run_id.clone());
                self.agents.insert(
                    run_id.clone(),
                    AgentTreeRecord {
                        info: SpawnedAgentInfo {
                            agent_id: event.agent_id,
                            run_id,
                            parent_run_id,
                            agent_type,
                            description,
                            status: AgentStatus::Initializing,
                            started_at: now,
                            metrics: SpawnedAgentMetrics::default(),
                            has_permission_issues: false,
                            run_in_background: false,
                            fanout_slot,
                        },
                    },
                );
            }
            other => {
                let run_id = self
                    .run_id_by_agent_id
                    .get(&event.agent_id)
                    .map(String::as_str)
                    .unwrap_or(event.agent_id.as_str());
                let Some(record) = self.agents.get_mut(run_id) else {
                    return;
                };
                match other {
                    ProgressEventType::Started { description } => {
                        record.info.status = AgentStatus::Running {
                            activity: description,
                        };
                    }
                    ProgressEventType::Busy { activity } => {
                        record.info.status = AgentStatus::Running { activity };
                    }
                    ProgressEventType::Idle => record.info.status = AgentStatus::Idle,
                    ProgressEventType::TurnCompleted {
                        turn,
                        tool_calls_this_turn,
                        activity,
                    } => {
                        record.info.status = AgentStatus::Running { activity };
                        record.info.metrics.turns_completed =
                            record.info.metrics.turns_completed.max(turn);
                        record.info.metrics.tool_calls = record
                            .info
                            .metrics
                            .tool_calls
                            .saturating_add(tool_calls_this_turn);
                    }
                    ProgressEventType::MetricsUpdate {
                        turn,
                        total_prompt_tokens,
                        total_completion_tokens,
                        total_tool_calls,
                        ..
                    } => {
                        record.info.metrics.turns_completed =
                            record.info.metrics.turns_completed.max(turn);
                        record.info.metrics.prompt_tokens = total_prompt_tokens;
                        record.info.metrics.completion_tokens = total_completion_tokens;
                        record.info.metrics.tool_calls = total_tool_calls;
                    }
                    ProgressEventType::Completed {
                        result_summary,
                        total_tool_calls,
                        total_tokens,
                        ..
                    } => {
                        record.info.status = AgentStatus::Completed {
                            result: result_summary,
                            finish_reason: None,
                        };
                        record.info.metrics.tool_calls = total_tool_calls;
                        record.info.metrics.prompt_tokens = total_tokens.0;
                        record.info.metrics.completion_tokens = total_tokens.1;
                    }
                    ProgressEventType::Interrupted {
                        reason,
                        partial_summary,
                        total_tool_calls,
                        total_tokens,
                        ..
                    } => {
                        record.info.status = AgentStatus::Completed {
                            result: partial_summary,
                            finish_reason: Some(reason),
                        };
                        record.info.metrics.tool_calls = total_tool_calls;
                        record.info.metrics.prompt_tokens = total_tokens.0;
                        record.info.metrics.completion_tokens = total_tokens.1;
                    }
                    ProgressEventType::Failed { error } => {
                        record.info.status = AgentStatus::Failed {
                            error,
                            finish_reason: None,
                        };
                    }
                    ProgressEventType::Cancelled { .. } => {
                        record.info.status = AgentStatus::cancelled_anonymous();
                    }
                    ProgressEventType::PermissionDenied { .. } => {
                        record.info.has_permission_issues = true;
                        record.info.metrics.tools_blocked =
                            record.info.metrics.tools_blocked.saturating_add(1);
                    }
                    ProgressEventType::ToolExecuting { .. }
                    | ProgressEventType::LlmCallStarted { .. }
                    | ProgressEventType::LlmCallCompleted { .. }
                    | ProgressEventType::AgentSpawned { .. } => {}
                }
            }
        }
        self.prune_completed_leaf_agents();
    }

    #[must_use]
    pub fn snapshot(&self) -> AgentTreeSnapshot {
        let mut children_by_parent: HashMap<&str, Vec<&str>> = HashMap::new();
        let mut all_run_ids = HashSet::new();
        for record in self.agents.values() {
            all_run_ids.insert(record.info.run_id.as_str());
            children_by_parent
                .entry(record.info.parent_run_id.as_str())
                .or_default()
                .push(record.info.run_id.as_str());
        }
        let mut roots: Vec<SpawnedAgentInfo> = self
            .agents
            .values()
            .filter(|record| {
                record.info.parent_run_id == "root"
                    || !all_run_ids.contains(record.info.parent_run_id.as_str())
            })
            .map(|record| aggregate_info(&record.info, &self.agents, &children_by_parent))
            .collect();
        roots.sort_by(|a, b| a.run_id.cmp(&b.run_id));

        let mut total_prompt_tokens: u64 = 0;
        let mut total_completion_tokens: u64 = 0;
        let mut failed_agents = 0;
        for root in &roots {
            total_prompt_tokens = total_prompt_tokens.saturating_add(root.metrics.prompt_tokens);
            total_completion_tokens =
                total_completion_tokens.saturating_add(root.metrics.completion_tokens);
            if matches!(root.status, AgentStatus::Failed { .. }) {
                failed_agents += 1;
            }
        }

        AgentTreeSnapshot {
            roots,
            total_prompt_tokens,
            total_completion_tokens,
            failed_agents,
        }
    }

    fn prune_completed_leaf_agents(&mut self) {
        while self.agents.len() > MAX_AGENT_RECORDS {
            let child_parent_ids: HashSet<&str> = self
                .agents
                .values()
                .map(|record| record.info.parent_run_id.as_str())
                .collect();
            let Some(run_id) = self
                .agents
                .iter()
                .filter(|(run_id, record)| {
                    is_terminal_status(&record.info.status)
                        && !child_parent_ids.contains(run_id.as_str())
                })
                .min_by(|(left_run_id, left), (right_run_id, right)| {
                    left.info
                        .started_at
                        .cmp(&right.info.started_at)
                        .then_with(|| left_run_id.cmp(right_run_id))
                })
                .map(|(run_id, _)| run_id.clone())
            else {
                break;
            };
            if let Some(removed) = self.agents.remove(&run_id) {
                self.run_id_by_agent_id
                    .remove(removed.info.agent_id.as_str());
            }
        }
    }
}

fn is_terminal_status(status: &AgentStatus) -> bool {
    matches!(
        status,
        AgentStatus::Completed { .. } | AgentStatus::Failed { .. } | AgentStatus::Cancelled { .. }
    )
}

fn aggregate_info(
    info: &SpawnedAgentInfo,
    agents: &HashMap<String, AgentTreeRecord>,
    children_by_parent: &HashMap<&str, Vec<&str>>,
) -> SpawnedAgentInfo {
    let mut aggregated = info.clone();
    if let Some(children) = children_by_parent.get(info.run_id.as_str()) {
        for child_run_id in children {
            if let Some(child) = agents.get(*child_run_id) {
                let child_info = aggregate_info(&child.info, agents, children_by_parent);
                aggregated.metrics.prompt_tokens = aggregated
                    .metrics
                    .prompt_tokens
                    .saturating_add(child_info.metrics.prompt_tokens);
                aggregated.metrics.completion_tokens = aggregated
                    .metrics
                    .completion_tokens
                    .saturating_add(child_info.metrics.completion_tokens);
                aggregated.metrics.tool_calls = aggregated
                    .metrics
                    .tool_calls
                    .saturating_add(child_info.metrics.tool_calls);
                aggregated.has_permission_issues |= child_info.has_permission_issues;
                if matches!(child_info.status, AgentStatus::Failed { .. })
                    && !matches!(aggregated.status, AgentStatus::Failed { .. })
                {
                    aggregated.status = AgentStatus::Failed {
                        error: format!("child agent {} failed", child_info.agent_id),
                        finish_reason: None,
                    };
                }
            }
        }
    }
    aggregated
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(agent_id: &str, event_type: ProgressEventType) -> AgentProgressEvent {
        AgentProgressEvent {
            agent_id: agent_id.to_string(),
            event_type,
            timestamp_epoch_ms: 1_000,
        }
    }

    #[test]
    fn tree_state_rolls_child_metrics_into_parent() {
        let mut state = AgentTreeState::default();
        state.apply(event(
            "root-agent",
            ProgressEventType::AgentSpawned {
                run_id: "root-run".into(),
                parent_run_id: "root".into(),
                agent_type: "general-purpose".into(),
                description: "root".into(),
                fanout_slot: None,
            },
        ));
        state.apply(event(
            "child-agent",
            ProgressEventType::AgentSpawned {
                run_id: "child-run".into(),
                parent_run_id: "root-run".into(),
                agent_type: "task".into(),
                description: "child".into(),
                fanout_slot: None,
            },
        ));
        state.apply(event(
            "child-agent",
            ProgressEventType::Completed {
                result_summary: "done".into(),
                total_tool_calls: 3,
                total_tokens: (100, 25),
                duration_ms: 50,
            },
        ));

        let snap = state.snapshot();
        assert_eq!(snap.roots.len(), 1);
        assert_eq!(snap.roots[0].metrics.prompt_tokens, 100);
        assert_eq!(snap.roots[0].metrics.completion_tokens, 25);
        assert_eq!(snap.roots[0].metrics.tool_calls, 3);
    }

    #[test]
    fn child_failure_propagates_to_root_warning_status() {
        let mut state = AgentTreeState::default();
        state.apply(event(
            "root-agent",
            ProgressEventType::AgentSpawned {
                run_id: "root-run".into(),
                parent_run_id: "root".into(),
                agent_type: "general-purpose".into(),
                description: "root".into(),
                fanout_slot: None,
            },
        ));
        state.apply(event(
            "child-agent",
            ProgressEventType::AgentSpawned {
                run_id: "child-run".into(),
                parent_run_id: "root-run".into(),
                agent_type: "task".into(),
                description: "child".into(),
                fanout_slot: None,
            },
        ));
        state.apply(event(
            "child-agent",
            ProgressEventType::Failed {
                error: "boom".into(),
            },
        ));

        let snap = state.snapshot();
        assert!(matches!(snap.roots[0].status, AgentStatus::Failed { .. }));
        assert_eq!(snap.failed_agents, 1);
    }

    #[test]
    fn interrupted_completion_preserves_partial_result_and_finish_reason() {
        let mut state = AgentTreeState::default();
        state.apply(event(
            "agent",
            ProgressEventType::AgentSpawned {
                run_id: "run-1".into(),
                parent_run_id: "root".into(),
                agent_type: "task".into(),
                description: "agent".into(),
                fanout_slot: None,
            },
        ));
        state.apply(event(
            "agent",
            ProgressEventType::Interrupted {
                reason: "budget_exhausted".into(),
                partial_summary: "partial output".into(),
                total_tool_calls: 2,
                total_tokens: (9, 4),
                duration_ms: 25,
            },
        ));

        let snap = state.snapshot();
        assert!(matches!(
            &snap.roots[0].status,
            AgentStatus::Completed {
                result,
                finish_reason: Some(reason),
            } if result == "partial output" && reason == "budget_exhausted"
        ));
        assert_eq!(snap.roots[0].metrics.tool_calls, 2);
        assert_eq!(snap.roots[0].metrics.prompt_tokens, 9);
        assert_eq!(snap.roots[0].metrics.completion_tokens, 4);
    }

    #[test]
    fn deep_tree_snapshot_keeps_leaf_tokens() {
        let mut state = AgentTreeState::default();
        let mut parent = "root".to_string();
        for depth in 0..50 {
            let run_id = format!("run-{depth}");
            state.apply(event(
                &run_id,
                ProgressEventType::AgentSpawned {
                    run_id: run_id.clone(),
                    parent_run_id: parent,
                    agent_type: "task".into(),
                    description: format!("depth {depth}"),
                    fanout_slot: None,
                },
            ));
            parent = run_id;
        }
        state.apply(event(
            "run-49",
            ProgressEventType::Completed {
                result_summary: "done".into(),
                total_tool_calls: 1,
                total_tokens: (7, 11),
                duration_ms: 1,
            },
        ));

        let snap = state.snapshot();
        assert_eq!(snap.total_prompt_tokens, 7);
        assert_eq!(snap.total_completion_tokens, 11);
    }

    #[test]
    fn orphaned_agent_with_missing_parent_is_promoted_to_root() {
        let mut state = AgentTreeState::default();
        state.apply(event(
            "orphan-agent",
            ProgressEventType::AgentSpawned {
                run_id: "orphan-run".into(),
                parent_run_id: "missing-run".into(),
                agent_type: "task".into(),
                description: "orphan".into(),
                fanout_slot: None,
            },
        ));

        let snap = state.snapshot();
        assert_eq!(snap.roots.len(), 1);
        assert_eq!(snap.roots[0].run_id, "orphan-run");
    }

    #[test]
    fn agent_spawned_event_preserves_fanout_slot_identity() {
        let mut state = AgentTreeState::default();
        state.apply(event(
            "storage@run-1",
            ProgressEventType::AgentSpawned {
                run_id: "run-1".into(),
                parent_run_id: "root".into(),
                agent_type: "task".into(),
                description: "review storage".into(),
                fanout_slot: Some(
                    crate::orchestration_fanout_group::AgentFanoutSlotIdentity::new(
                        "review-1", 3, 1,
                    )
                    .unwrap(),
                ),
            },
        ));

        let snap = state.snapshot();
        let slot = snap.roots[0]
            .fanout_slot
            .as_ref()
            .expect("fanout slot should survive progress projection");
        assert_eq!(slot.group_id, "review-1");
        assert_eq!(slot.target_count, 3);
        assert_eq!(slot.slot_index, 1);
    }

    #[test]
    fn prunes_oldest_completed_leaf_agents_when_capacity_is_exceeded() {
        let mut state = AgentTreeState::default();
        for idx in 0..(MAX_AGENT_RECORDS as u64 + 8) {
            let run_id = format!("run-{idx}");
            state.apply(AgentProgressEvent {
                agent_id: format!("agent-{idx}"),
                event_type: ProgressEventType::AgentSpawned {
                    run_id: run_id.clone(),
                    parent_run_id: "root".into(),
                    agent_type: "task".into(),
                    description: format!("agent {idx}"),
                    fanout_slot: None,
                },
                timestamp_epoch_ms: idx,
            });
            state.apply(AgentProgressEvent {
                agent_id: format!("agent-{idx}"),
                event_type: ProgressEventType::Completed {
                    result_summary: "done".into(),
                    total_tool_calls: 1,
                    total_tokens: (idx, 0),
                    duration_ms: 1,
                },
                timestamp_epoch_ms: idx,
            });
        }

        assert_eq!(state.agents.len(), MAX_AGENT_RECORDS);
        let snap = state.snapshot();
        assert_eq!(snap.roots.len(), MAX_AGENT_RECORDS);
        assert!(!snap.roots.iter().any(|root| root.run_id == "run-0"));
        assert!(!snap.roots.iter().any(|root| root.run_id == "run-7"));
        assert!(snap.roots.iter().any(|root| root.run_id == "run-8"));
        assert!(snap.roots.iter().any(|root| root.run_id == "run-263"));
        assert_eq!(
            snap.total_prompt_tokens,
            (8..(MAX_AGENT_RECORDS as u64 + 8)).sum::<u64>()
        );
    }

    #[test]
    fn pruning_keeps_active_agents_even_when_capacity_is_exceeded() {
        let mut state = AgentTreeState::default();
        state.apply(AgentProgressEvent {
            agent_id: "active-agent".into(),
            event_type: ProgressEventType::AgentSpawned {
                run_id: "active-run".into(),
                parent_run_id: "root".into(),
                agent_type: "task".into(),
                description: "active".into(),
                fanout_slot: None,
            },
            timestamp_epoch_ms: 1,
        });
        state.apply(AgentProgressEvent {
            agent_id: "active-agent".into(),
            event_type: ProgressEventType::Started {
                description: "working".into(),
            },
            timestamp_epoch_ms: 2,
        });

        for idx in 0..MAX_AGENT_RECORDS {
            let run_id = format!("done-run-{idx}");
            let agent_id = format!("done-agent-{idx}");
            let timestamp = (idx + 10) as u64;
            state.apply(AgentProgressEvent {
                agent_id: agent_id.clone(),
                event_type: ProgressEventType::AgentSpawned {
                    run_id: run_id.clone(),
                    parent_run_id: "root".into(),
                    agent_type: "task".into(),
                    description: "done".into(),
                    fanout_slot: None,
                },
                timestamp_epoch_ms: timestamp,
            });
            state.apply(AgentProgressEvent {
                agent_id,
                event_type: ProgressEventType::Completed {
                    result_summary: "done".into(),
                    total_tool_calls: 1,
                    total_tokens: (1, 0),
                    duration_ms: 1,
                },
                timestamp_epoch_ms: timestamp,
            });
        }

        assert_eq!(state.agents.len(), MAX_AGENT_RECORDS);
        let snap = state.snapshot();
        assert!(snap.roots.iter().any(|root| root.run_id == "active-run"));
        assert!(!snap.roots.iter().any(|root| root.run_id == "done-run-0"));
    }
}
