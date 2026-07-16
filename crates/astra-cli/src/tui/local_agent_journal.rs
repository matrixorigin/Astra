//! Durable local-agent index reconstructed from the canonical session journal.
//!
//! A local agent's transcript is durable even after its in-memory spawner has
//! been dropped. The workbench must therefore recover its rows from the same
//! journal, rather than treating the live runtime cache as the source of
//! truth for what users may inspect.

use std::collections::BTreeMap;

use astra_services::session_journal::{JournalEvent, JournalEventType, read_journal_tail};

/// Enough lifecycle events to reconstruct a useful recent workbench without
/// rereading an ever-growing JSONL journal on every session bind.
const LOCAL_AGENT_EVENT_TAIL: usize = 4096;
pub(crate) const RECENT_TERMINAL_RUN_LIMIT: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalJournalAgentRun {
    pub(crate) agent_id: String,
    pub(crate) run_id: String,
    pub(crate) parent_run_id: Option<String>,
    pub(crate) description: String,
    pub(crate) status: String,
    pub(crate) duration_ms: u64,
    pub(crate) tool_calls: usize,
}

/// Read only terminal runs. A `transcript_item` alone proves that a
/// conversation exists, but not the execution outcome; surfacing it as a
/// completed/failed workbench row would fabricate a lifecycle fact. Current
/// local spawners append both `agent_spawned` and `agent_terminated` events.
pub(crate) fn load_terminal_runs(session_id: &str) -> Result<Vec<LocalJournalAgentRun>, String> {
    let events = read_journal_tail(session_id, LOCAL_AGENT_EVENT_TAIL)
        .map_err(|error| format!("Could not read local agent history: {error}"))?;
    Ok(terminal_runs_from_events(events))
}

fn terminal_runs_from_events(
    events: impl IntoIterator<Item = JournalEvent>,
) -> Vec<LocalJournalAgentRun> {
    let mut pending = BTreeMap::<String, PendingRun>::new();

    for (ordinal, event) in events.into_iter().enumerate() {
        let metadata = event.metadata.as_ref();
        match event.event_type {
            JournalEventType::AgentSpawned => {
                let Some(run_id) = metadata_string(metadata, "run_id") else {
                    continue;
                };
                let Some(agent_id) = metadata_string(metadata, "agent_id") else {
                    continue;
                };
                let run = pending.entry(run_id.clone()).or_default();
                run.first_seen = run.first_seen.min(ordinal);
                run.agent_id = Some(agent_id);
                run.parent_run_id = metadata_string(metadata, "parent_run_id");
                run.description = metadata_string(metadata, "description");
            }
            JournalEventType::AgentTerminated => {
                let Some(run_id) = metadata_string(metadata, "run_id") else {
                    continue;
                };
                let Some(agent_id) = metadata_string(metadata, "agent_id") else {
                    continue;
                };
                let Some(status) = metadata_string(metadata, "status") else {
                    continue;
                };
                let run = pending.entry(run_id.clone()).or_default();
                run.first_seen = run.first_seen.min(ordinal);
                run.agent_id.get_or_insert(agent_id);
                run.status = Some(status);
                run.duration_ms = metadata_u64(metadata, "duration_ms").unwrap_or(0);
                run.tool_calls = metadata_u64(metadata, "tool_calls")
                    .and_then(|value| usize::try_from(value).ok())
                    .unwrap_or(0);
            }
            _ => {}
        }
    }

    let mut runs = pending
        .into_iter()
        .filter_map(|(run_id, run)| {
            Some((
                run.first_seen,
                LocalJournalAgentRun {
                    agent_id: run.agent_id?,
                    run_id,
                    parent_run_id: run.parent_run_id,
                    description: run.description.unwrap_or_default(),
                    status: run.status?,
                    duration_ms: run.duration_ms,
                    tool_calls: run.tool_calls,
                },
            ))
        })
        .collect::<Vec<_>>();
    runs.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then(left.1.run_id.cmp(&right.1.run_id))
            .then(left.1.agent_id.cmp(&right.1.agent_id))
    });
    let keep_from = runs.len().saturating_sub(RECENT_TERMINAL_RUN_LIMIT);
    runs.into_iter()
        .skip(keep_from)
        .map(|(_, run)| run)
        .collect()
}

struct PendingRun {
    first_seen: usize,
    agent_id: Option<String>,
    parent_run_id: Option<String>,
    description: Option<String>,
    status: Option<String>,
    duration_ms: u64,
    tool_calls: usize,
}

impl Default for PendingRun {
    fn default() -> Self {
        Self {
            first_seen: usize::MAX,
            agent_id: None,
            parent_run_id: None,
            description: None,
            status: None,
            duration_ms: 0,
            tool_calls: 0,
        }
    }
}

fn metadata_string(metadata: Option<&serde_json::Value>, key: &str) -> Option<String> {
    metadata
        .and_then(|metadata| metadata.get(key))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn metadata_u64(metadata: Option<&serde_json::Value>, key: &str) -> Option<u64> {
    metadata
        .and_then(|metadata| metadata.get(key))
        .and_then(|value| value.as_u64().or_else(|| value.as_str()?.parse().ok()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_history_joins_spawn_metadata_to_the_exact_run() {
        let spawned = JournalEvent::agent_spawned(
            Some("session-1"),
            "reviewer",
            "run-1",
            "root-1",
            "code-review",
            "Review the storage change",
            None,
            false,
            None,
        );
        let terminated = JournalEvent::agent_terminated(
            Some("session-1"),
            "reviewer",
            "run-1",
            "code-review",
            "completed",
            Some("normal"),
            Some(3),
            7,
            0,
            0,
            42,
            None,
        );

        let runs = terminal_runs_from_events(vec![spawned, terminated]);

        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].run_id, "run-1");
        assert_eq!(runs[0].agent_id, "reviewer");
        assert_eq!(runs[0].parent_run_id.as_deref(), Some("root-1"));
        assert_eq!(runs[0].description, "Review the storage change");
        assert_eq!(runs[0].status, "completed");
        assert_eq!(runs[0].duration_ms, 42);
        assert_eq!(runs[0].tool_calls, 7);
    }

    #[test]
    fn transcript_or_spawn_without_terminal_state_does_not_fabricate_completion() {
        let spawned = JournalEvent::agent_spawned(
            Some("session-1"),
            "reviewer",
            "run-1",
            "root-1",
            "code-review",
            "Review the storage change",
            None,
            false,
            None,
        );

        assert!(terminal_runs_from_events(vec![spawned]).is_empty());
    }

    #[test]
    fn metadata_numbers_accept_wire_numbers_or_strings() {
        let numeric = serde_json::json!({"duration_ms": 42});
        let string = serde_json::json!({"duration_ms": "43"});
        assert_eq!(metadata_u64(Some(&numeric), "duration_ms"), Some(42));
        assert_eq!(metadata_u64(Some(&string), "duration_ms"), Some(43));
    }

    #[test]
    fn terminal_history_is_bounded_to_the_recent_working_set() {
        let mut events = Vec::new();
        for index in 0..(RECENT_TERMINAL_RUN_LIMIT + 5) {
            let run_id = format!("run-{index}");
            let agent_id = format!("agent-{index}");
            events.push(JournalEvent::agent_spawned(
                Some("session-1"),
                &agent_id,
                &run_id,
                "root",
                "review",
                "review",
                None,
                false,
                None,
            ));
            events.push(JournalEvent::agent_terminated(
                Some("session-1"),
                &agent_id,
                &run_id,
                "review",
                "completed",
                Some("normal"),
                Some(1),
                0,
                0,
                0,
                1,
                None,
            ));
        }

        let runs = terminal_runs_from_events(events);
        assert_eq!(runs.len(), RECENT_TERMINAL_RUN_LIMIT);
        assert_eq!(runs.first().map(|run| run.run_id.as_str()), Some("run-5"));
        let expected_last = format!("run-{}", RECENT_TERMINAL_RUN_LIMIT + 4);
        assert_eq!(
            runs.last().map(|run| run.run_id.as_str()),
            Some(expected_last.as_str())
        );
    }
}
