use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use astra_tools::task_mgmt::{TaskManager, TaskManagerSnapshot};
use serde_json::Value;

use crate::server::server_tool_executor::SessionConfigInner;
use crate::server::tool_session_config::{persist_config_override, persist_tool_preferences};

#[derive(Debug, Clone)]
pub(crate) enum SessionStateRollbackAction {
    ToolPreferences {
        previous_prioritized_tools: Vec<String>,
        previous_deprioritized_tools: Vec<String>,
    },
    ConfigOverride {
        path: String,
        old_value: Value,
        snapshot: crate::observability::ObservabilitySessionRollbackSnapshot,
    },
    Compression {
        turn: u32,
        snapshot: crate::observability::ObservabilitySessionRollbackSnapshot,
    },
    TaskState {
        snapshot: TaskManagerSnapshot,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct SessionStateRollbackEntry {
    pub(crate) sequence: u64,
    pub(crate) turn_index: u32,
    timestamp: SystemTime,
    pub(crate) label: String,
    pub(crate) action: SessionStateRollbackAction,
}

pub(crate) struct SessionStateRestoreContext<'a> {
    pub(crate) session_id: &'a str,
    pub(crate) observability_session:
        Option<&'a Arc<RwLock<crate::observability::ObservabilitySession>>>,
    pub(crate) config: &'a Mutex<SessionConfigInner>,
    pub(crate) task_manager: &'a TaskManager,
}

pub(crate) struct RollbackSessionStateContext<'a> {
    pub(crate) journal: &'a Mutex<SessionStateRollbackJournal>,
    pub(crate) current_turn_index: u32,
    pub(crate) restore_context: SessionStateRestoreContext<'a>,
}

#[derive(Debug, Default)]
pub(crate) struct SessionStateRollbackJournal {
    entries: Vec<SessionStateRollbackEntry>,
    next_sequence: u64,
}

impl SessionStateRollbackJournal {
    pub(crate) fn record(
        &mut self,
        turn_index: u32,
        label: String,
        action: SessionStateRollbackAction,
    ) {
        self.entries.push(SessionStateRollbackEntry {
            sequence: self.next_sequence,
            turn_index,
            timestamp: SystemTime::now(),
            label,
            action,
        });
        self.next_sequence = self.next_sequence.saturating_add(1);
    }

    pub(crate) fn list(&self) -> Vec<SessionStateRollbackEntry> {
        self.entries.iter().rev().cloned().collect()
    }

    pub(crate) fn restore_plan_for_turn(&self, turn_index: u32) -> Vec<SessionStateRollbackEntry> {
        self.restore_plan_for_turn_since(turn_index, 0)
    }

    pub(crate) fn restore_plan_for_turn_since(
        &self,
        turn_index: u32,
        checkpoint: u64,
    ) -> Vec<SessionStateRollbackEntry> {
        self.entries
            .iter()
            .rev()
            .filter(|entry| entry.turn_index == turn_index && entry.sequence >= checkpoint)
            .cloned()
            .collect()
    }

    pub(crate) fn checkpoint(&self) -> u64 {
        self.next_sequence
    }

    pub(crate) fn remove_sequence(&mut self, sequence: u64) -> bool {
        if let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.sequence == sequence)
        {
            self.entries.remove(index);
            true
        } else {
            false
        }
    }

    pub(crate) fn drop_task_state_entries(&mut self) -> usize {
        let before = self.entries.len();
        self.entries
            .retain(|entry| !matches!(entry.action, SessionStateRollbackAction::TaskState { .. }));
        before - self.entries.len()
    }
}

fn with_journal_mut<T>(
    journal: &Mutex<SessionStateRollbackJournal>,
    operation: &'static str,
    f: impl FnOnce(&mut SessionStateRollbackJournal) -> T,
) -> T {
    match journal.lock() {
        Ok(mut journal) => f(&mut journal),
        Err(poisoned) => {
            tracing::warn!(
                operation,
                "session_state_journal mutex poisoned; recovering inner journal"
            );
            let mut journal = poisoned.into_inner();
            f(&mut journal)
        }
    }
}

fn with_journal<T>(
    journal: &Mutex<SessionStateRollbackJournal>,
    operation: &'static str,
    f: impl FnOnce(&SessionStateRollbackJournal) -> T,
) -> T {
    match journal.lock() {
        Ok(journal) => f(&journal),
        Err(poisoned) => {
            tracing::warn!(
                operation,
                "session_state_journal mutex poisoned; recovering inner journal"
            );
            let journal = poisoned.into_inner();
            f(&journal)
        }
    }
}

pub(crate) fn journal_checkpoint(journal: &Mutex<SessionStateRollbackJournal>) -> u64 {
    with_journal(journal, "session_state_journal_checkpoint", |journal| {
        journal.checkpoint()
    })
}

pub(crate) fn record(
    journal: &Mutex<SessionStateRollbackJournal>,
    turn_index: u32,
    label: String,
    action: SessionStateRollbackAction,
) {
    with_journal_mut(journal, "record_session_state_rollback", |journal| {
        journal.record(turn_index, label, action)
    });
}

pub(crate) fn entries(
    journal: &Mutex<SessionStateRollbackJournal>,
) -> Vec<SessionStateRollbackEntry> {
    with_journal(journal, "session_state_entries", |journal| journal.list())
}

pub(crate) fn restore_plan_for_turn(
    journal: &Mutex<SessionStateRollbackJournal>,
    turn_index: u32,
) -> Vec<SessionStateRollbackEntry> {
    with_journal(journal, "session_state_restore_plan_for_turn", |journal| {
        journal.restore_plan_for_turn(turn_index)
    })
}

pub(crate) fn restore_plan_for_turn_since(
    journal: &Mutex<SessionStateRollbackJournal>,
    turn_index: u32,
    checkpoint: u64,
) -> Vec<SessionStateRollbackEntry> {
    with_journal(
        journal,
        "session_state_restore_plan_for_turn_since",
        |journal| journal.restore_plan_for_turn_since(turn_index, checkpoint),
    )
}

pub(crate) fn remove_sequence(journal: &Mutex<SessionStateRollbackJournal>, sequence: u64) -> bool {
    with_journal_mut(journal, "remove_session_state_rollback", |journal| {
        journal.remove_sequence(sequence)
    })
}

pub(crate) fn drop_task_state_entries(journal: &Mutex<SessionStateRollbackJournal>) -> usize {
    with_journal_mut(journal, "drop_task_state_entries", |journal| {
        journal.drop_task_state_entries()
    })
}

pub(crate) fn action_kind(action: &SessionStateRollbackAction) -> &'static str {
    match action {
        SessionStateRollbackAction::ToolPreferences { .. } => "tool_preferences",
        SessionStateRollbackAction::ConfigOverride { .. } => "config_override",
        SessionStateRollbackAction::Compression { .. } => "compression",
        SessionStateRollbackAction::TaskState { .. } => "task_state",
    }
}

pub(crate) fn rollback_session_state_entry_json(entry: &SessionStateRollbackEntry) -> Value {
    let timestamp_ms = entry
        .timestamp
        .duration_since(UNIX_EPOCH)
        .inspect_err(|e| {
            tracing::warn!(
                error = %e,
                "rollback timestamp predates UNIX_EPOCH, using 0"
            );
        })
        .ok()
        .map(|duration| duration.as_millis())
        .and_then(|millis| {
            u64::try_from(millis)
                .inspect_err(|e| {
                    tracing::warn!(
                        millis,
                        error = %e,
                        "rollback timestamp u64 overflow, using 0"
                    );
                })
                .ok()
        });
    let mut value = serde_json::Map::from_iter([
        ("label".to_string(), Value::String(entry.label.clone())),
        (
            "kind".to_string(),
            Value::String(action_kind(&entry.action).to_string()),
        ),
        (
            "turn_index".to_string(),
            Value::Number(serde_json::Number::from(entry.turn_index)),
        ),
    ]);
    if let Some(timestamp_ms) = timestamp_ms {
        value.insert(
            "timestamp_ms".to_string(),
            Value::Number(serde_json::Number::from(timestamp_ms)),
        );
    }
    match &entry.action {
        SessionStateRollbackAction::ConfigOverride { path, .. } => {
            value.insert("path".to_string(), Value::String(path.clone()));
        }
        SessionStateRollbackAction::Compression { turn, .. } => {
            value.insert(
                "turn".to_string(),
                Value::Number(serde_json::Number::from(*turn)),
            );
        }
        SessionStateRollbackAction::ToolPreferences { .. }
        | SessionStateRollbackAction::TaskState { .. } => {}
    }
    Value::Object(value)
}

pub(crate) async fn restore_entry(
    context: &SessionStateRestoreContext<'_>,
    entry: &SessionStateRollbackEntry,
) -> Result<(), String> {
    const ROLLBACK_STEP_TIMEOUT: Duration = Duration::from_secs(30);

    match &entry.action {
        SessionStateRollbackAction::ToolPreferences {
            previous_prioritized_tools,
            previous_deprioritized_tools,
        } => {
            let mut inner = context
                .config
                .lock()
                .map_err(|_| "Failed to access session config".to_string())?;
            let current_prioritized = inner.prioritized_tools.clone();
            let current_deprioritized = inner.deprioritized_tools.clone();
            inner.prioritized_tools = previous_prioritized_tools.clone();
            inner.deprioritized_tools = previous_deprioritized_tools.clone();
            if let Err(error) = persist_tool_preferences(
                context.session_id,
                &inner.prioritized_tools,
                &inner.deprioritized_tools,
                "tool_session_state_rollback:restore_entry",
            ) {
                inner.prioritized_tools = current_prioritized;
                inner.deprioritized_tools = current_deprioritized;
                return Err(format!(
                    "failed to persist restored tool preferences: {error}"
                ));
            }
            Ok(())
        }
        SessionStateRollbackAction::ConfigOverride {
            path,
            old_value,
            snapshot,
        } => {
            restore_observability_snapshot(context.observability_session, snapshot)?;
            persist_config_override(
                context.session_id,
                path,
                old_value.clone(),
                "tool_session_state_rollback:restore_entry",
            )
            .map_err(|error| {
                format!("failed to persist restored config override for {path}: {error}")
            })
        }
        SessionStateRollbackAction::Compression { snapshot, .. } => {
            restore_observability_snapshot(context.observability_session, snapshot)
        }
        SessionStateRollbackAction::TaskState { snapshot } => {
            match tokio::time::timeout(
                ROLLBACK_STEP_TIMEOUT,
                context.task_manager.restore_snapshot(snapshot),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => Err(format!(
                    "task_manager.restore_snapshot timed out after {}s",
                    ROLLBACK_STEP_TIMEOUT.as_secs()
                )),
            }
        }
    }
}

pub(crate) async fn execute_rollback_session_state(
    context: RollbackSessionStateContext<'_>,
    args: &Value,
    publish_current_workspace: impl FnOnce() -> Result<(), String>,
) -> String {
    let scope = args
        .get("scope")
        .and_then(Value::as_str)
        .unwrap_or("current_turn");
    let explicit_turn_index = if scope == "turn" {
        match args.get("turn_index").and_then(Value::as_u64) {
            Some(turn_index) => Some(turn_index),
            None => {
                return serde_json::json!({
                    "success": false,
                    "error": "missing 'turn_index' for scope=turn",
                })
                .to_string();
            }
        }
    } else {
        None
    };
    let checkpoint = args
        .get("session_state_after_sequence")
        .or_else(|| args.get("after_sequence"))
        .and_then(Value::as_u64)
        .unwrap_or(0);

    match scope {
        "list" => rollback_session_state_list(context.journal),
        "turn" | "current_turn" => {
            rollback_session_state_turn(
                context,
                scope,
                explicit_turn_index,
                checkpoint,
                publish_current_workspace,
            )
            .await
        }
        other => serde_json::json!({
            "success": false,
            "error": format!(
                "unknown scope `{other}`. Supported: current_turn, turn, list"
            ),
        })
        .to_string(),
    }
}

fn rollback_session_state_list(journal: &Mutex<SessionStateRollbackJournal>) -> String {
    let entries = entries(journal)
        .into_iter()
        .map(|entry| rollback_session_state_entry_json(&entry))
        .collect::<Vec<_>>();
    serde_json::json!({
        "success": true,
        "scope": "list",
        "total_entries": entries.len(),
        "entries": entries,
        "summary": format!(
            "Listed {} recorded session-state rollback entr{}",
            entries.len(),
            if entries.len() == 1 { "y" } else { "ies" },
        ),
    })
    .to_string()
}

async fn rollback_session_state_turn(
    context: RollbackSessionStateContext<'_>,
    scope: &str,
    explicit_turn_index: Option<u64>,
    checkpoint: u64,
    publish_current_workspace: impl FnOnce() -> Result<(), String>,
) -> String {
    let turn_index = explicit_turn_index.unwrap_or(u64::from(context.current_turn_index)) as u32;
    let plan = if checkpoint > 0 {
        restore_plan_for_turn_since(context.journal, turn_index, checkpoint)
    } else {
        restore_plan_for_turn(context.journal, turn_index)
    };
    let mut restored = Vec::new();
    let mut failed = Vec::new();
    for entry in &plan {
        match restore_entry(&context.restore_context, entry).await {
            Ok(()) => {
                remove_sequence(context.journal, entry.sequence);
                restored.push(rollback_session_state_entry_json(entry));
            }
            Err(error) => {
                let mut failed_entry = rollback_session_state_entry_json(entry)
                    .as_object()
                    .cloned()
                    .unwrap_or_default();
                failed_entry.insert("error".to_string(), Value::String(error));
                failed.push(Value::Object(failed_entry));
            }
        }
    }
    let success = !restored.is_empty() && failed.is_empty();
    if !restored.is_empty()
        && failed.is_empty()
        && let Err(error) = publish_current_workspace()
    {
        return serde_json::json!({
            "success": false,
            "scope": scope,
            "turn_index": turn_index,
            "restored": restored,
            "failed": [{
                "error": error,
                "kind": "workspace_artifact_publish"
            }],
            "summary": "Restored session state locally but failed to publish workspace artifact",
        })
        .to_string();
    }
    let summary = if plan.is_empty() {
        format!("No recorded session-state rollback handles found for turn {turn_index}")
    } else if failed.is_empty() {
        format!(
            "Restored {} recorded session-state mutation{} for turn {turn_index}",
            restored.len(),
            if restored.len() == 1 { "" } else { "s" },
        )
    } else {
        format!(
            "Restored {} recorded session-state mutation{} for turn {turn_index} with {} failure{}",
            restored.len(),
            if restored.len() == 1 { "" } else { "s" },
            failed.len(),
            if failed.len() == 1 { "" } else { "s" },
        )
    };
    serde_json::json!({
        "success": success,
        "scope": scope,
        "turn_index": turn_index,
        "restored": restored,
        "failed": failed,
        "summary": summary,
    })
    .to_string()
}

fn restore_observability_snapshot(
    observability_session: Option<&Arc<RwLock<crate::observability::ObservabilitySession>>>,
    snapshot: &crate::observability::ObservabilitySessionRollbackSnapshot,
) -> Result<(), String> {
    let Some(observability_session) = observability_session else {
        return Err("No observability session available".to_string());
    };
    let mut session = observability_session
        .write()
        .map_err(|_| "Failed to acquire observability session".to_string())?;
    session.restore_rollback_snapshot(snapshot);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;

    fn observability_snapshot() -> crate::observability::ObservabilitySessionRollbackSnapshot {
        crate::observability::ObservabilitySessionRollbackSnapshot {
            config: astra_config::runtime_config::RuntimeConfig::default(),
            original_query: None,
            recent_queries: vec![],
            compressed_turns: vec![],
            user_corrections: vec![],
            context_traces: vec![],
            drift_min_severity_threshold: 0.5,
            drift_analysis_window: 5,
            last_reported_drift_turn: None,
            last_query_at: None,
        }
    }

    fn task_snapshot() -> TaskManagerSnapshot {
        TaskManagerSnapshot {
            tasks: vec![],
            next_task_id: 1,
            version: 0,
            restore_version: None,
        }
    }

    #[test]
    fn restore_plan_returns_newest_first_and_honors_checkpoint() {
        let mut journal = SessionStateRollbackJournal::default();
        journal.record(
            3,
            "before".to_string(),
            SessionStateRollbackAction::TaskState {
                snapshot: task_snapshot(),
            },
        );
        let checkpoint = journal.checkpoint();
        journal.record(
            3,
            "first".to_string(),
            SessionStateRollbackAction::ToolPreferences {
                previous_prioritized_tools: vec!["bash".to_string()],
                previous_deprioritized_tools: vec![],
            },
        );
        journal.record(
            3,
            "second".to_string(),
            SessionStateRollbackAction::TaskState {
                snapshot: task_snapshot(),
            },
        );
        journal.record(
            4,
            "wrong-turn".to_string(),
            SessionStateRollbackAction::TaskState {
                snapshot: task_snapshot(),
            },
        );

        let labels = journal
            .restore_plan_for_turn_since(3, checkpoint)
            .into_iter()
            .map(|entry| entry.label)
            .collect::<Vec<_>>();

        assert_eq!(labels, vec!["second", "first"]);
    }

    #[test]
    fn journal_helpers_record_list_remove_and_drop_task_state() {
        let journal = Mutex::new(SessionStateRollbackJournal::default());

        record(
            &journal,
            5,
            "task".to_string(),
            SessionStateRollbackAction::TaskState {
                snapshot: task_snapshot(),
            },
        );
        record(
            &journal,
            5,
            "prefs".to_string(),
            SessionStateRollbackAction::ToolPreferences {
                previous_prioritized_tools: vec!["bash".to_string()],
                previous_deprioritized_tools: vec![],
            },
        );

        assert_eq!(journal_checkpoint(&journal), 2);
        let listed = entries(&journal);
        assert_eq!(listed.len(), 2);
        assert_eq!(restore_plan_for_turn(&journal, 5).len(), 2);
        assert!(remove_sequence(&journal, listed[0].sequence));
        assert_eq!(entries(&journal).len(), 1);
        assert_eq!(drop_task_state_entries(&journal), 1);
        assert!(entries(&journal).is_empty());
    }

    #[test]
    fn drop_task_state_entries_preserves_store_independent_actions() {
        let mut journal = SessionStateRollbackJournal::default();
        journal.record(
            1,
            "task".to_string(),
            SessionStateRollbackAction::TaskState {
                snapshot: task_snapshot(),
            },
        );
        journal.record(
            1,
            "prefs".to_string(),
            SessionStateRollbackAction::ToolPreferences {
                previous_prioritized_tools: vec!["bash".to_string()],
                previous_deprioritized_tools: vec![],
            },
        );

        assert_eq!(journal.drop_task_state_entries(), 1);
        let entries = journal.list();
        assert_eq!(entries.len(), 1);
        assert!(matches!(
            entries[0].action,
            SessionStateRollbackAction::ToolPreferences { .. }
        ));
    }

    #[test]
    fn rollback_entry_json_omits_invalid_timestamp_instead_of_failing() {
        let entry = SessionStateRollbackEntry {
            sequence: 0,
            turn_index: 9,
            timestamp: UNIX_EPOCH - Duration::from_millis(1),
            label: "prefs".to_string(),
            action: SessionStateRollbackAction::ToolPreferences {
                previous_prioritized_tools: vec![],
                previous_deprioritized_tools: vec![],
            },
        };

        let value = rollback_session_state_entry_json(&entry);

        assert_eq!(value["label"], "prefs");
        assert_eq!(value["kind"], "tool_preferences");
        assert_eq!(value["turn_index"], 9);
        assert!(value.get("timestamp_ms").is_none());
    }

    #[test]
    fn remove_sequence_reports_missing_entries_without_mutating() {
        let mut journal = SessionStateRollbackJournal::default();
        journal.record(
            1,
            "prefs".to_string(),
            SessionStateRollbackAction::ToolPreferences {
                previous_prioritized_tools: vec![],
                previous_deprioritized_tools: vec![],
            },
        );

        assert!(!journal.remove_sequence(42));
        assert_eq!(journal.list().len(), 1);
        assert!(journal.remove_sequence(0));
        assert!(journal.list().is_empty());
    }

    #[tokio::test]
    async fn restore_entry_requires_observability_session_for_compression() {
        let entry = SessionStateRollbackEntry {
            sequence: 0,
            turn_index: 1,
            timestamp: UNIX_EPOCH,
            label: "compression".to_string(),
            action: SessionStateRollbackAction::Compression {
                turn: 1,
                snapshot: observability_snapshot(),
            },
        };
        let task_manager = TaskManager::in_memory();
        let config = Mutex::new(SessionConfigInner::default());

        let context = SessionStateRestoreContext {
            session_id: "session-1",
            observability_session: None,
            config: &config,
            task_manager: &task_manager,
        };

        let error = restore_entry(&context, &entry)
            .await
            .expect_err("missing observability session must fail closed");

        assert_eq!(error, "No observability session available");
    }

    #[tokio::test]
    async fn execute_rollback_session_state_requires_turn_index_for_turn_scope() {
        let journal = Mutex::new(SessionStateRollbackJournal::default());
        let config = Mutex::new(SessionConfigInner::default());
        let task_manager = TaskManager::in_memory();

        let output = execute_rollback_session_state(
            RollbackSessionStateContext {
                journal: &journal,
                current_turn_index: 1,
                restore_context: SessionStateRestoreContext {
                    session_id: "session-1",
                    observability_session: None,
                    config: &config,
                    task_manager: &task_manager,
                },
            },
            &serde_json::json!({"scope": "turn"}),
            || Ok(()),
        )
        .await;

        let value: Value = serde_json::from_str(&output).expect("rollback json");
        assert_eq!(value["success"], false);
        assert_eq!(
            value["error"].as_str(),
            Some("missing 'turn_index' for scope=turn")
        );
    }

    #[tokio::test]
    async fn execute_rollback_session_state_does_not_publish_when_plan_is_empty() {
        let journal = Mutex::new(SessionStateRollbackJournal::default());
        let task_manager = TaskManager::in_memory();
        let config = Mutex::new(SessionConfigInner::default());
        let publish_calls = std::sync::atomic::AtomicUsize::new(0);

        let output = execute_rollback_session_state(
            RollbackSessionStateContext {
                journal: &journal,
                current_turn_index: 9,
                restore_context: SessionStateRestoreContext {
                    session_id: "session-1",
                    observability_session: None,
                    config: &config,
                    task_manager: &task_manager,
                },
            },
            &serde_json::json!({"scope": "current_turn"}),
            || {
                publish_calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(())
            },
        )
        .await;

        let value: Value = serde_json::from_str(&output).expect("rollback json");
        assert_eq!(value["success"], false);
        assert_eq!(value["restored"].as_array().map(Vec::len), Some(0));
        assert_eq!(
            publish_calls.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "empty rollback plans must not publish workspace artifacts"
        );
    }

    #[tokio::test]
    async fn execute_rollback_session_state_reports_publish_failure_after_restore() {
        let journal = Mutex::new(SessionStateRollbackJournal::default());
        record(
            &journal,
            4,
            "task".to_string(),
            SessionStateRollbackAction::TaskState {
                snapshot: task_snapshot(),
            },
        );
        let task_manager = TaskManager::in_memory();
        let config = Mutex::new(SessionConfigInner::default());

        let output = execute_rollback_session_state(
            RollbackSessionStateContext {
                journal: &journal,
                current_turn_index: 4,
                restore_context: SessionStateRestoreContext {
                    session_id: "session-1",
                    observability_session: None,
                    config: &config,
                    task_manager: &task_manager,
                },
            },
            &serde_json::json!({"scope": "current_turn"}),
            || Err("publish failed".to_string()),
        )
        .await;

        let value: Value = serde_json::from_str(&output).expect("rollback json");
        assert_eq!(value["success"], false);
        assert_eq!(value["failed"][0]["kind"], "workspace_artifact_publish");
        assert_eq!(value["failed"][0]["error"], "publish failed");
    }
}
