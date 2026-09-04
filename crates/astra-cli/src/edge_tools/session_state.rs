use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use astra_runtime::observability::ObservabilitySessionRollbackSnapshot;
use serde_json::Value;

use super::ToolExecutor;

#[derive(Debug, Clone)]
pub(crate) enum SessionStateRollbackAction {
    ConfigOverride {
        path: String,
        old_value: Value,
        snapshot: ObservabilitySessionRollbackSnapshot,
        expected_revision: Option<Arc<AtomicU64>>,
    },
    Compression {
        turn: u32,
        snapshot: ObservabilitySessionRollbackSnapshot,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct SessionStateRollbackEntry {
    sequence: u64,
    pub turn_index: u32,
    pub timestamp: SystemTime,
    pub label: String,
    pub action: SessionStateRollbackAction,
}

#[derive(Debug, Default)]
pub(crate) struct SessionStateRollbackJournal {
    entries: Vec<SessionStateRollbackEntry>,
    next_sequence: u64,
}

impl SessionStateRollbackJournal {
    fn record(&mut self, turn_index: u32, label: String, action: SessionStateRollbackAction) {
        self.entries.push(SessionStateRollbackEntry {
            sequence: self.next_sequence,
            turn_index,
            timestamp: SystemTime::now(),
            label,
            action,
        });
        self.next_sequence = self.next_sequence.saturating_add(1);
    }

    fn list(&self) -> Vec<SessionStateRollbackEntry> {
        self.entries.iter().rev().cloned().collect()
    }

    fn restore_plan_for_turn(&self, turn_index: u32) -> Vec<SessionStateRollbackEntry> {
        self.restore_plan_for_turn_since(turn_index, 0)
    }

    fn restore_plan_for_turn_since(
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

    fn checkpoint(&self) -> u64 {
        self.next_sequence
    }

    fn settle_restored_sequence(
        &mut self,
        sequence: u64,
        next_config_owner: Option<(u64, u64)>,
    ) -> bool {
        let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.sequence == sequence)
        else {
            return false;
        };
        if let Some((next_sequence, revision)) = next_config_owner
            && let Some(SessionStateRollbackAction::ConfigOverride {
                expected_revision: Some(expected_revision),
                ..
            }) = self
                .entries
                .iter()
                .find(|entry| entry.sequence == next_sequence)
                .map(|entry| &entry.action)
        {
            expected_revision.store(revision, Ordering::Relaxed);
        }
        self.entries.remove(index);
        true
    }
}

fn action_kind(action: &SessionStateRollbackAction) -> &'static str {
    match action {
        SessionStateRollbackAction::ConfigOverride { .. } => "config_override",
        SessionStateRollbackAction::Compression { .. } => "compression",
    }
}

impl ToolExecutor {
    fn record_session_state_rollback(&self, label: String, action: SessionStateRollbackAction) {
        let turn_index = self
            .journal_turn_index
            .load(std::sync::atomic::Ordering::Relaxed);
        match self.session_state_journal.lock() {
            Ok(mut journal) => journal.record(turn_index, label, action),
            Err(poisoned) => poisoned.into_inner().record(turn_index, label, action),
        }
    }

    pub(crate) fn record_adjust_config_rollback(
        &self,
        path: impl Into<String>,
        old_value: Value,
        snapshot: ObservabilitySessionRollbackSnapshot,
        expected_revision: Option<u64>,
    ) {
        let path = path.into();
        self.record_session_state_rollback(
            format!("adjust_config:{path}"),
            SessionStateRollbackAction::ConfigOverride {
                path,
                old_value,
                snapshot,
                expected_revision: expected_revision
                    .map(|revision| Arc::new(AtomicU64::new(revision))),
            },
        );
    }

    pub(crate) fn record_compression_rollback(
        &self,
        turn: u32,
        snapshot: ObservabilitySessionRollbackSnapshot,
    ) {
        self.record_session_state_rollback(
            format!("compress_context:turn-{turn}"),
            SessionStateRollbackAction::Compression { turn, snapshot },
        );
    }

    fn session_state_entries(&self) -> Vec<SessionStateRollbackEntry> {
        match self.session_state_journal.lock() {
            Ok(journal) => journal.list(),
            Err(poisoned) => poisoned.into_inner().list(),
        }
    }

    fn session_state_restore_plan_for_turn(
        &self,
        turn_index: u32,
    ) -> Vec<SessionStateRollbackEntry> {
        match self.session_state_journal.lock() {
            Ok(journal) => journal.restore_plan_for_turn(turn_index),
            Err(poisoned) => poisoned.into_inner().restore_plan_for_turn(turn_index),
        }
    }

    fn session_state_restore_plan_for_turn_since(
        &self,
        turn_index: u32,
        checkpoint: u64,
    ) -> Vec<SessionStateRollbackEntry> {
        match self.session_state_journal.lock() {
            Ok(journal) => journal.restore_plan_for_turn_since(turn_index, checkpoint),
            Err(poisoned) => poisoned
                .into_inner()
                .restore_plan_for_turn_since(turn_index, checkpoint),
        }
    }

    pub(crate) fn session_state_journal_checkpoint(&self) -> u64 {
        match self.session_state_journal.lock() {
            Ok(journal) => journal.checkpoint(),
            Err(poisoned) => poisoned.into_inner().checkpoint(),
        }
    }

    fn settle_session_state_rollback(&self, sequence: u64, next_config_owner: Option<(u64, u64)>) {
        match self.session_state_journal.lock() {
            Ok(mut journal) => {
                journal.settle_restored_sequence(sequence, next_config_owner);
            }
            Err(poisoned) => {
                poisoned
                    .into_inner()
                    .settle_restored_sequence(sequence, next_config_owner);
            }
        }
    }

    fn restore_observability_snapshot(
        &self,
        snapshot: &ObservabilitySessionRollbackSnapshot,
    ) -> Result<(), String> {
        let Some(obs) = self.observability_session.as_ref() else {
            return Err("No observability session available".to_string());
        };
        let mut session = obs
            .write()
            .map_err(|_| "Failed to acquire observability session".to_string())?;
        session.restore_rollback_snapshot(snapshot);
        Ok(())
    }

    fn rollback_session_state_entry_json(entry: &SessionStateRollbackEntry) -> Value {
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
        match &entry.action {
            SessionStateRollbackAction::ConfigOverride {
                path,
                expected_revision,
                ..
            } => {
                value.insert("path".to_string(), Value::String(path.clone()));
                if let Some(expected_revision) = expected_revision {
                    value.insert(
                        "expected_revision".to_string(),
                        Value::from(expected_revision.load(Ordering::Relaxed)),
                    );
                }
            }
            SessionStateRollbackAction::Compression { turn, .. } => {
                value.insert(
                    "turn".to_string(),
                    Value::Number(serde_json::Number::from(*turn)),
                );
            }
        }
        Value::Object(value)
    }

    async fn rollback_session_state_entry(
        &self,
        entry: &SessionStateRollbackEntry,
    ) -> Result<Option<u64>, String> {
        match &entry.action {
            SessionStateRollbackAction::ConfigOverride {
                path,
                old_value,
                snapshot,
                expected_revision,
            } => {
                let Some(expected_revision) = expected_revision else {
                    return self.restore_observability_snapshot(snapshot).map(|_| None);
                };
                let session_id = self
                    .active_session_id()
                    .ok_or_else(|| "durable config rollback has no active session".to_string())?;
                let expected = expected_revision.load(Ordering::Relaxed);
                match crate::cli::self_command::restore_config_override(
                        &session_id,
                        path,
                        old_value.clone(),
                        expected,
                    )
                    .map_err(|error| {
                        format!("failed to persist restored config override for {path}: {error}")
                    })?
                {
                    astra_services::session_workspace::WorkspaceConfigRestoreOutcome::Applied {
                        config, revision, ..
                    } => {
                        if let Err(error) = self.restore_observability_snapshot(snapshot) {
                            tracing::warn!(
                                session_id,
                                path,
                                revision,
                                error,
                                "config rollback committed but observability snapshot could not be restored"
                            );
                        }
                        if let Some(obs) = self.observability_session.as_ref() {
                            match obs.write() {
                                Ok(mut session) => session.config = *config,
                                Err(_) => tracing::warn!(
                                    session_id,
                                    path,
                                    revision,
                                    "config rollback committed but observability config could not be projected"
                                ),
                            }
                        }
                        Ok(Some(revision))
                    }
                    astra_services::session_workspace::WorkspaceConfigRestoreOutcome::Rejected {
                        current_revision,
                        current_config,
                        ..
                    } => {
                        if let Some(obs) = self.observability_session.as_ref() {
                            obs.write()
                                .map_err(|_| "Failed to acquire observability session".to_string())?
                                .config = *current_config;
                        }
                        Err(format!(
                            "config rollback revision conflict for {path}: expected {expected}, current {current_revision}"
                        ))
                    }
                    astra_services::session_workspace::WorkspaceConfigRestoreOutcome::OutcomeUnknown {
                        revision,
                        reason,
                        observed_config,
                        retry_revision,
                    } => {
                        if let Some(retry_revision) = retry_revision {
                            expected_revision.store(retry_revision, Ordering::Relaxed);
                        }
                        if let Some(config) = observed_config
                            && let Some(obs) = self.observability_session.as_ref()
                        {
                            obs.write()
                                .map_err(|_| "Failed to acquire observability session".to_string())?
                                .config = *config;
                        }
                        Err(format!(
                            "config rollback outcome unknown for {path} at revision {revision}: {reason}"
                        ))
                    }
                }
            }
            SessionStateRollbackAction::Compression { snapshot, .. } => {
                self.restore_observability_snapshot(snapshot).map(|_| None)
            }
        }
    }

    pub(crate) async fn rollback_session_state(&self, args: &Value) -> String {
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
        let after_sequence = args
            .get("session_state_after_sequence")
            .and_then(Value::as_u64)
            .unwrap_or(0);

        match scope {
            "list" => {
                let entries = self
                    .session_state_entries()
                    .into_iter()
                    .map(|entry| Self::rollback_session_state_entry_json(&entry))
                    .collect::<Vec<_>>();
                serde_json::json!({
                    "success": true,
                    "scope": "list",
                    "total_entries": entries.len(),
                    "entries": entries,
                    "summary": format!(
                        "Listed {} recorded session-state rollback entr{}",
                        entries.len(),
                        if entries.len() == 1 { "y" } else { "ies" }
                    ),
                })
                .to_string()
            }
            "turn" | "current_turn" => {
                let turn_index = explicit_turn_index.unwrap_or_else(|| {
                    self.journal_turn_index
                        .load(std::sync::atomic::Ordering::Relaxed) as u64
                }) as u32;
                let plan = if after_sequence > 0 {
                    self.session_state_restore_plan_for_turn_since(turn_index, after_sequence)
                } else {
                    self.session_state_restore_plan_for_turn(turn_index)
                };
                let mut restored = Vec::new();
                let mut failed = Vec::new();
                for (index, entry) in plan.iter().enumerate() {
                    match self.rollback_session_state_entry(entry).await {
                        Ok(next_revision) => {
                            let next_config_owner = next_revision.and_then(|revision| {
                                plan[index + 1..]
                                    .iter()
                                    .find(|entry| {
                                        matches!(
                                            &entry.action,
                                            SessionStateRollbackAction::ConfigOverride {
                                                expected_revision: Some(_),
                                                ..
                                            }
                                        )
                                    })
                                    .map(|entry| (entry.sequence, revision))
                            });
                            self.settle_session_state_rollback(entry.sequence, next_config_owner);
                            restored.push(Self::rollback_session_state_entry_json(entry));
                        }
                        Err(error) => {
                            let mut failed_entry = Self::rollback_session_state_entry_json(entry)
                                .as_object()
                                .cloned()
                                .unwrap_or_default();
                            failed_entry.insert("error".to_string(), Value::String(error));
                            failed.push(Value::Object(failed_entry));
                            break;
                        }
                    }
                }
                let success = !restored.is_empty() && failed.is_empty();
                let summary = if plan.is_empty() {
                    format!(
                        "No recorded session-state rollback handles found for turn {turn_index}"
                    )
                } else if failed.is_empty() {
                    format!(
                        "Restored {} recorded session-state mutation{} for turn {turn_index}",
                        restored.len(),
                        if restored.len() == 1 { "" } else { "s" }
                    )
                } else {
                    format!(
                        "Restored {} recorded session-state mutation{} for turn {turn_index} with {} failure{}",
                        restored.len(),
                        if restored.len() == 1 { "" } else { "s" },
                        failed.len(),
                        if failed.len() == 1 { "" } else { "s" }
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
            other => serde_json::json!({
                "success": false,
                "error": format!(
                    "unknown scope `{other}`. Supported: current_turn, turn, list"
                ),
            })
            .to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::session_journal::JournalDirGuard;
    use astra_services::session_workspace::{self, WorkspaceMetadata};
    use serde_json::json;

    #[tokio::test]
    #[serial_test::serial]
    async fn partial_cli_rollback_transfers_revision_across_calls() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path());
        let session_id = "cli-partial-config-rollback";
        session_workspace::write_workspace(&WorkspaceMetadata::new(session_id, "test-model"))
            .unwrap();
        let observability = Arc::new(std::sync::RwLock::new(
            astra_runtime::observability::ObservabilitySession::new_simple(session_id),
        ));
        let baseline = observability.read().unwrap().config.memory.retrieval_top_k;
        let first = (1..=20).find(|value| *value != baseline).unwrap();
        let second = (1..=20)
            .find(|value| *value != baseline && *value != first)
            .unwrap();
        let executor = ToolExecutor::new(temp.path())
            .with_active_session_id(session_id)
            .with_observability_session(observability.clone());
        let first_result: Value = serde_json::from_str(
            &executor
                .execute(
                    "adjust_config",
                    &json!({"path": "memory.retrieval_top_k", "value": first, "force": true}),
                )
                .await,
        )
        .unwrap();
        assert_eq!(first_result["status"], "completed");
        let snapshot = observability.read().unwrap().rollback_snapshot();
        executor.record_session_state_rollback(
            "invalid-owner".into(),
            SessionStateRollbackAction::ConfigOverride {
                path: "missing.path".into(),
                old_value: Value::Null,
                snapshot,
                expected_revision: Some(Arc::new(AtomicU64::new(2))),
            },
        );
        let second_result: Value = serde_json::from_str(
            &executor
                .execute(
                    "adjust_config",
                    &json!({"path": "memory.retrieval_top_k", "value": second, "force": true}),
                )
                .await,
        )
        .unwrap();
        assert_eq!(second_result["status"], "completed");

        let partial: Value = serde_json::from_str(
            &executor
                .execute("rollback_session_state", &json!({"scope": "current_turn"}))
                .await,
        )
        .unwrap();
        assert_eq!(partial["restored"].as_array().map(Vec::len), Some(1));
        assert_eq!(partial["failed"].as_array().map(Vec::len), Some(1));
        {
            let mut journal = executor.session_state_journal.lock().unwrap();
            let failed = journal
                .entries
                .iter_mut()
                .find(|entry| entry.sequence == 1)
                .unwrap();
            let SessionStateRollbackAction::ConfigOverride {
                path,
                old_value,
                expected_revision: Some(expected_revision),
                ..
            } = &mut failed.action
            else {
                panic!("failed rollback entry must remain config-owned");
            };
            assert_eq!(expected_revision.load(Ordering::Relaxed), 3);
            *path = "memory.retrieval_top_k".into();
            *old_value = json!(first);
        }

        let completed: Value = serde_json::from_str(
            &executor
                .execute("rollback_session_state", &json!({"scope": "current_turn"}))
                .await,
        )
        .unwrap();
        assert_eq!(completed["success"], true);
        assert_eq!(completed["restored"].as_array().map(Vec::len), Some(2));
        assert!(
            executor
                .session_state_journal
                .lock()
                .unwrap()
                .entries
                .is_empty()
        );
        let workspace = session_workspace::read_workspace(session_id).unwrap();
        assert_eq!(workspace.config_mutation_revision, 5);
        assert!(workspace.tuned_config_json.is_none());
        assert_eq!(
            observability.read().unwrap().config.memory.retrieval_top_k,
            baseline
        );
    }
}
