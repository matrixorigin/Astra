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

    /// Canonical fanout observations captured at the same instant as the UI
    /// projection. Active-run guidance carries these through a typed runtime
    /// lane so final-answer settlement does not need to parse XML or infer
    /// group truth from child events.
    pub(crate) fn fanout_work_unit_observations(
        &self,
    ) -> Vec<astra_core::work_unit::WorkUnitObservation> {
        use astra_core::work_unit::{
            WorkUnitObservation, WorkUnitObservationMode, WorkUnitWakePolicy,
        };

        self.fanout_groups
            .iter()
            .filter_map(|group| {
                WorkUnitObservation::new(
                    &group.group_id,
                    "agent_fanout",
                    group.work_unit_status(),
                    group.revision.to_string(),
                    WorkUnitObservationMode::Current,
                )
                .map(|observation| observation.with_wake_policy(WorkUnitWakePolicy::OnTerminal))
            })
            .collect()
    }

    /// Immediate, model-free receipt for guidance accepted during an active
    /// run. It confirms delivery and surfaces bounded current progress while
    /// the foreground tool still owns the next model boundary.
    pub(crate) fn active_guidance_receipt(&self) -> String {
        let active_groups = self
            .fanout_groups
            .iter()
            .filter(|group| !group.is_terminal())
            .collect::<Vec<_>>();
        if active_groups.len() == 1 {
            let group = active_groups[0];
            let summary = group.summary();
            let title = group.title.trim();
            let title = if title.is_empty() {
                "Agent fanout"
            } else {
                title
            };
            return format!(
                "Guidance queued · {title}: {}/{} settled, {} running · Shift+↓ inspect",
                summary.terminal, summary.target_count, summary.active,
            );
        }
        if !active_groups.is_empty() {
            let running = active_groups
                .iter()
                .map(|group| group.summary().active)
                .sum::<usize>();
            return format!(
                "Guidance queued · {} agent groups, {running} agents running · Shift+↓ inspect",
                active_groups.len(),
            );
        }
        "Guidance queued for the current run.".to_string()
    }

    /// Describe newly launched user-visible work units without involving the
    /// model.  A fanout is one receipt even though its slots enter the runtime
    /// independently; emitting one line per child makes normal concurrency
    /// look like repeated replanning and leaves the user guessing whether the
    /// parent is still waiting.
    pub(crate) fn launch_receipts_since(&self, previous: &Self) -> Vec<String> {
        let previous_groups = previous
            .fanout_groups
            .iter()
            .filter(|group| {
                let summary = group.summary();
                summary.accepted > 0 || summary.spawn_rejected > 0
            })
            .map(|group| group.group_id.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        let mut receipts = self
            .fanout_groups
            .iter()
            .filter(|group| {
                let summary = group.summary();
                summary.accepted > 0 || summary.spawn_rejected > 0
            })
            .filter(|group| !previous_groups.contains(group.group_id.as_str()))
            .map(|group| {
                let title = group.title.trim();
                let title = if title.is_empty() {
                    group.group_id.as_str()
                } else {
                    title
                };
                let member_ids = group
                    .slots
                    .iter()
                    .filter_map(|slot| slot.agent_id.as_deref())
                    .collect::<std::collections::BTreeSet<_>>();
                let members = self
                    .agents
                    .iter()
                    .filter(|agent| member_ids.contains(agent.agent_id.as_str()))
                    .collect::<Vec<_>>();
                let explicitly_background = !member_ids.is_empty()
                    && members.len() == member_ids.len()
                    && members.iter().all(|agent| agent.run_in_background);
                if explicitly_background {
                    format!(
                        "{title} · {} parallel agents started in background · one update after the group settles · Shift+↓ inspect",
                        group.target_count
                    )
                } else {
                    format!(
                        "{title} · {} parallel agents started · parent waits for the complete group before synthesizing · Shift+↓ inspect · Ctrl+B move to background",
                        group.target_count
                    )
                }
            })
            .collect::<Vec<_>>();

        let current_fanout_agents = self
            .fanout_groups
            .iter()
            .flat_map(|group| &group.slots)
            .filter_map(|slot| slot.agent_id.as_deref())
            .collect::<std::collections::BTreeSet<_>>();
        let previous_run_ids = previous
            .agents
            .iter()
            .map(|agent| agent.run_id.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        receipts.extend(
            self.agents
                .iter()
                .filter(|agent| !agent.status.is_terminal())
                .filter(|agent| !previous_run_ids.contains(agent.run_id.as_str()))
                .filter(|agent| !current_fanout_agents.contains(agent.agent_id.as_str()))
                .map(|agent| {
                    let title = agent.description.trim();
                    let title = if title.is_empty() {
                        agent.agent_id.as_str()
                    } else {
                        title
                    };
                    if agent.run_in_background {
                        format!(
                            "{title} started in background · Astra will update once it needs attention or finishes · Shift+↓ inspect"
                        )
                    } else {
                        format!(
                            "{title} started · parent waits for its result · Shift+↓ inspect · Ctrl+B move to background"
                        )
                    }
                }),
        );
        receipts
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
        {
            return true;
        }
        if value.get("event").and_then(serde_json::Value::as_str) == Some("fanout_group_settled") {
            let Some(group_id) = value.get("group_id").and_then(serde_json::Value::as_str) else {
                return true;
            };
            return !self.fanout_groups.iter().any(|group| {
                group.group_id == group_id
                    && group.is_terminal()
                    && group.summary().uncollected == 0
            });
        }
        if value.get("event").and_then(serde_json::Value::as_str) != Some("agent_status_changed") {
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

    /// Return only newly attention-worthy work-unit transitions. Ordinary
    /// running progress and individual terminal fanout slots stay in the task
    /// UI. A fanout wakes the parent once, when the fixed-size group settles;
    /// otherwise three reviewers finishing means three wasteful model turns
    /// and three contradictory partial summaries.
    pub(crate) fn attention_notifications_since(&self, previous: &Self) -> Vec<String> {
        let previous_status = previous
            .agents
            .iter()
            .map(|agent| (agent.run_id.as_str(), &agent.status))
            .collect::<BTreeMap<_, _>>();
        let fanout_agent_ids = self
            .fanout_groups
            .iter()
            .flat_map(|group| &group.slots)
            .filter_map(|slot| slot.agent_id.as_deref())
            .collect::<std::collections::BTreeSet<_>>();
        let mut notifications = self
            .agents
            .iter()
            // Foreground children return through the tool result that is
            // already blocking their parent.  Waking a second model turn for
            // the same terminal fact is both wasteful and user-visible as
            // repeated analysis.
            .filter(|agent| agent.run_in_background)
            .filter(|agent| {
                matches!(
                    agent.status,
                    astra_turn_core::orchestration_types::AgentStatus::Waiting { .. }
                ) || (agent.status.is_terminal()
                    && !fanout_agent_ids.contains(agent.agent_id.as_str()))
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
            .collect::<Vec<_>>();

        let previous_groups = previous
            .fanout_groups
            .iter()
            .map(|group| (group.group_id.as_str(), group))
            .collect::<BTreeMap<_, _>>();
        notifications.extend(
            self.fanout_groups
                .iter()
                .filter(|group| group.is_terminal())
                .filter(|group| {
                    previous_groups
                        .get(group.group_id.as_str())
                        .is_none_or(|previous| !previous.is_terminal())
                })
                .map(|group| {
                    let summary = group.summary();
                    serde_json::json!({
                        "schema": "agent_attention_hint.v1",
                        "event": "fanout_group_settled",
                        "group_id": group.group_id,
                        "title": group.title,
                        "parent_run_id": group.parent_run_id,
                        "status": group.status.as_str(),
                        "target_count": summary.target_count,
                        "terminal": summary.terminal,
                        "completed": summary.completed,
                        "failed": summary.failed,
                        "interrupted": summary.interrupted,
                        "cancelled_by_user": summary.cancelled_by_user,
                        "cancelled_by_parent_budget": summary.cancelled_by_parent_budget,
                        "timed_out": summary.timed_out,
                        "spawn_rejected": summary.spawn_rejected,
                        "uncollected": summary.uncollected,
                        "authoritative_result_call": {
                            "tool": "agent_fanout",
                            "action": "get_results",
                            "group_id": group.group_id,
                        },
                        "instruction": "Reconcile this fanout once as one work unit. Use only this canonical group_id; do not analyze individual slot completions separately.",
                    })
                    .to_string()
                }),
        );
        notifications
    }
}

#[cfg(test)]
mod tests {
    use super::LocalAgentSnapshot;
    use astra_core::work_unit::{WorkUnitStatus, WorkUnitWakePolicy};
    use astra_turn_core::orchestration_fanout_group::{
        AgentFanoutGroupProjection, AgentFanoutSlotStatus,
    };
    use astra_turn_core::orchestration_types::{
        AgentStatus, SpawnedAgentInfo, SpawnedAgentMetrics,
    };

    fn agent(agent_id: &str, run_id: &str, status: AgentStatus) -> SpawnedAgentInfo {
        SpawnedAgentInfo {
            agent_id: agent_id.into(),
            run_id: run_id.into(),
            parent_run_id: "root-run".into(),
            agent_type: "review".into(),
            description: format!("review {agent_id}"),
            ended_at: status.is_terminal().then(std::time::SystemTime::now),
            status,
            started_at: std::time::SystemTime::now(),
            metrics: SpawnedAgentMetrics::default(),
            has_permission_issues: false,
            run_in_background: true,
            spawn_tool_call_id: None,
            fanout_slot: None,
        }
    }

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

    #[test]
    fn fanout_work_observation_uses_group_revision_and_terminal_truth() {
        let mut group = AgentFanoutGroupProjection::new("review", "Review", 2);
        group
            .record_spawn_accepted_with_run(0, "reviewer-1", Some("run-1".into()))
            .unwrap();
        group
            .record_spawn_accepted_with_run(1, "reviewer-2", Some("run-2".into()))
            .unwrap();
        group
            .record_terminal_by_agent("reviewer-1", AgentFanoutSlotStatus::Completed, None)
            .unwrap();
        let running_revision = group.revision;
        let running = LocalAgentSnapshot {
            available: true,
            fanout_groups: vec![group.clone()],
            ..LocalAgentSnapshot::default()
        }
        .fanout_work_unit_observations();
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].id, "review");
        assert_eq!(running[0].status, WorkUnitStatus::Running);
        assert_eq!(running[0].version, running_revision.to_string());
        assert_eq!(running[0].wake_policy, WorkUnitWakePolicy::OnTerminal);
        let receipt = LocalAgentSnapshot {
            available: true,
            fanout_groups: vec![group.clone()],
            ..LocalAgentSnapshot::default()
        }
        .active_guidance_receipt();
        assert!(receipt.contains("1/2 settled"), "{receipt}");
        assert!(receipt.contains("1 running"), "{receipt}");
        assert!(receipt.contains("Guidance queued"), "{receipt}");

        group
            .record_terminal_by_agent("reviewer-2", AgentFanoutSlotStatus::Completed, None)
            .unwrap();
        let terminal = LocalAgentSnapshot {
            available: true,
            fanout_groups: vec![group],
            ..LocalAgentSnapshot::default()
        }
        .fanout_work_unit_observations();
        assert_eq!(terminal[0].status, WorkUnitStatus::Completed);
        assert_ne!(terminal[0].version, running_revision.to_string());
    }

    #[test]
    fn fanout_slot_completions_wake_once_only_when_the_group_settles() {
        let mut running = AgentFanoutGroupProjection::new("review-group", "Three reviews", 3);
        running.parent_run_id = Some("root-run".into());
        for index in 0..3 {
            running
                .record_spawn_accepted_with_run(
                    index,
                    format!("reviewer-{index}"),
                    Some(format!("run-{index}")),
                )
                .unwrap();
        }
        let before = LocalAgentSnapshot {
            available: true,
            agents: (0..3)
                .map(|index| {
                    agent(
                        &format!("reviewer-{index}"),
                        &format!("run-{index}"),
                        AgentStatus::Running {
                            activity: "reviewing".into(),
                        },
                    )
                })
                .collect(),
            fanout_groups: vec![running.clone()],
        };

        running
            .record_terminal_by_agent("reviewer-0", AgentFanoutSlotStatus::Completed, None)
            .unwrap();
        let first_done = LocalAgentSnapshot {
            available: true,
            agents: vec![
                agent(
                    "reviewer-0",
                    "run-0",
                    AgentStatus::Completed {
                        result: "one".into(),
                        finish_reason: None,
                    },
                ),
                agent(
                    "reviewer-1",
                    "run-1",
                    AgentStatus::Running {
                        activity: "reviewing".into(),
                    },
                ),
                agent(
                    "reviewer-2",
                    "run-2",
                    AgentStatus::Running {
                        activity: "reviewing".into(),
                    },
                ),
            ],
            fanout_groups: vec![running.clone()],
        };
        assert!(first_done.attention_notifications_since(&before).is_empty());

        running
            .record_terminal_by_agent("reviewer-1", AgentFanoutSlotStatus::Completed, None)
            .unwrap();
        let second_done = LocalAgentSnapshot {
            available: true,
            agents: vec![
                agent(
                    "reviewer-0",
                    "run-0",
                    AgentStatus::Completed {
                        result: "one".into(),
                        finish_reason: None,
                    },
                ),
                agent(
                    "reviewer-1",
                    "run-1",
                    AgentStatus::Completed {
                        result: "two".into(),
                        finish_reason: None,
                    },
                ),
                agent(
                    "reviewer-2",
                    "run-2",
                    AgentStatus::Running {
                        activity: "reviewing".into(),
                    },
                ),
            ],
            fanout_groups: vec![running.clone()],
        };
        assert!(
            second_done
                .attention_notifications_since(&first_done)
                .is_empty()
        );

        running
            .record_terminal_by_agent("reviewer-2", AgentFanoutSlotStatus::Completed, None)
            .unwrap();
        let settled = LocalAgentSnapshot {
            available: true,
            agents: vec![
                agent(
                    "reviewer-0",
                    "run-0",
                    AgentStatus::Completed {
                        result: "one".into(),
                        finish_reason: None,
                    },
                ),
                agent(
                    "reviewer-1",
                    "run-1",
                    AgentStatus::Completed {
                        result: "two".into(),
                        finish_reason: None,
                    },
                ),
                agent(
                    "reviewer-2",
                    "run-2",
                    AgentStatus::Completed {
                        result: "three".into(),
                        finish_reason: None,
                    },
                ),
            ],
            fanout_groups: vec![running],
        };
        let notifications = settled.attention_notifications_since(&second_done);
        assert_eq!(notifications.len(), 1, "{notifications:?}");
        let value: serde_json::Value = serde_json::from_str(&notifications[0]).unwrap();
        assert_eq!(value["event"], "fanout_group_settled");
        assert_eq!(value["group_id"], "review-group");
        assert_eq!(value["completed"], 3);
    }

    #[test]
    fn foreground_child_terminal_result_does_not_schedule_a_second_model_turn() {
        let mut running = agent(
            "foreground",
            "run-foreground",
            AgentStatus::Running {
                activity: "reviewing".into(),
            },
        );
        running.run_in_background = false;
        let before = LocalAgentSnapshot {
            available: true,
            agents: vec![running],
            ..LocalAgentSnapshot::default()
        };
        let mut completed = agent(
            "foreground",
            "run-foreground",
            AgentStatus::Completed {
                result: "done".into(),
                finish_reason: None,
            },
        );
        completed.run_in_background = false;
        let after = LocalAgentSnapshot {
            available: true,
            agents: vec![completed],
            ..LocalAgentSnapshot::default()
        };

        assert!(after.attention_notifications_since(&before).is_empty());
    }

    #[test]
    fn fanout_launch_is_one_runtime_receipt_even_as_later_slots_arrive() {
        let mut group = AgentFanoutGroupProjection::new("review-group", "Three reviews", 3);
        group
            .record_spawn_accepted_with_run(0, "reviewer-0", Some("run-0".into()))
            .unwrap();
        let mut first_agent = agent(
            "reviewer-0",
            "run-0",
            AgentStatus::Running {
                activity: "reviewing".into(),
            },
        );
        first_agent.run_in_background = false;
        let first = LocalAgentSnapshot {
            available: true,
            agents: vec![first_agent],
            fanout_groups: vec![group.clone()],
        };

        let receipts = first.launch_receipts_since(&LocalAgentSnapshot::default());
        assert_eq!(receipts.len(), 1, "{receipts:?}");
        assert!(receipts[0].contains("3 parallel agents"), "{receipts:?}");
        assert!(receipts[0].contains("parent waits"), "{receipts:?}");
        assert!(receipts[0].contains("Shift+↓ inspect"), "{receipts:?}");
        assert!(receipts[0].contains("Ctrl+B"), "{receipts:?}");

        group
            .record_spawn_accepted_with_run(1, "reviewer-1", Some("run-1".into()))
            .unwrap();
        let mut second_agent = agent(
            "reviewer-1",
            "run-1",
            AgentStatus::Running {
                activity: "reviewing".into(),
            },
        );
        second_agent.run_in_background = false;
        let later = LocalAgentSnapshot {
            available: true,
            agents: vec![first.agents[0].clone(), second_agent],
            fanout_groups: vec![group],
        };
        assert!(later.launch_receipts_since(&first).is_empty());
    }
}
