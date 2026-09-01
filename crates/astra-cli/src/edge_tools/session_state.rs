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

    fn remove_sequence(&mut self, sequence: u64) -> bool {
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
    ) {
        let path = path.into();
        self.record_session_state_rollback(
            format!("adjust_config:{path}"),
            SessionStateRollbackAction::ConfigOverride {
                path,
                old_value,
                snapshot,
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

    fn remove_session_state_rollback(&self, sequence: u64) {
        match self.session_state_journal.lock() {
            Ok(mut journal) => {
                journal.remove_sequence(sequence);
            }
            Err(poisoned) => {
                poisoned.into_inner().remove_sequence(sequence);
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
            SessionStateRollbackAction::ConfigOverride { path, .. } => {
                value.insert("path".to_string(), Value::String(path.clone()));
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
    ) -> Result<(), String> {
        match &entry.action {
            SessionStateRollbackAction::ConfigOverride {
                path,
                old_value,
                snapshot,
            } => {
                self.restore_observability_snapshot(snapshot)?;
                if let Some(session_id) = self.active_session_id() {
                    crate::cli::self_command::persist_config_override(
                        &session_id,
                        path,
                        old_value.clone(),
                    )
                    .map(|_| ())
                    .map_err(|error| {
                        format!("failed to persist restored config override for {path}: {error}")
                    })?;
                }
                Ok(())
            }
            SessionStateRollbackAction::Compression { snapshot, .. } => {
                self.restore_observability_snapshot(snapshot)
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
                for entry in &plan {
                    match self.rollback_session_state_entry(entry).await {
                        Ok(()) => {
                            self.remove_session_state_rollback(entry.sequence);
                            restored.push(Self::rollback_session_state_entry_json(entry));
                        }
                        Err(error) => {
                            let mut failed_entry = Self::rollback_session_state_entry_json(entry)
                                .as_object()
                                .cloned()
                                .unwrap_or_default();
                            failed_entry.insert("error".to_string(), Value::String(error));
                            failed.push(Value::Object(failed_entry));
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
