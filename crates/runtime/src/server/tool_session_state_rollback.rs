use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::server::tool_session_config::{ConfigRestoreOutcome, restore_config_override};

#[derive(Debug, Clone)]
pub(crate) enum SessionStateRollbackAction {
    ConfigOverride {
        path: String,
        old_value: Value,
        expected_revision: Arc<AtomicU64>,
    },
    Compression {
        turn: u32,
        snapshot: Box<crate::observability::ObservabilitySessionRollbackSnapshot>,
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

pub(crate) struct SessionStateRestoreContext {
    pub(crate) user_id: String,
    pub(crate) session_id: String,
    pub(crate) observability_session:
        Option<Arc<RwLock<crate::observability::ObservabilitySession>>>,
}

pub(crate) struct RollbackSessionStateContext {
    pub(crate) journal: Arc<Mutex<SessionStateRollbackJournal>>,
    pub(crate) current_turn_index: u32,
    pub(crate) restore_context: SessionStateRestoreContext,
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

    #[cfg(test)]
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
                expected_revision, ..
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

#[cfg(test)]
pub(crate) fn remove_sequence(journal: &Mutex<SessionStateRollbackJournal>, sequence: u64) -> bool {
    with_journal_mut(journal, "remove_session_state_rollback", |journal| {
        journal.remove_sequence(sequence)
    })
}

fn settle_restored_sequence(
    journal: &Mutex<SessionStateRollbackJournal>,
    sequence: u64,
    next_config_owner: Option<(u64, u64)>,
) -> bool {
    with_journal_mut(journal, "settle_session_state_rollback", |journal| {
        journal.settle_restored_sequence(sequence, next_config_owner)
    })
}

pub(crate) fn action_kind(action: &SessionStateRollbackAction) -> &'static str {
    match action {
        SessionStateRollbackAction::ConfigOverride { .. } => "config_override",
        SessionStateRollbackAction::Compression { .. } => "compression",
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
        SessionStateRollbackAction::ConfigOverride {
            path,
            expected_revision,
            ..
        } => {
            value.insert("path".to_string(), Value::String(path.clone()));
            value.insert(
                "expected_revision".to_string(),
                Value::from(expected_revision.load(Ordering::Relaxed)),
            );
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

pub(crate) async fn restore_entry(
    context: &SessionStateRestoreContext,
    entry: &SessionStateRollbackEntry,
    chained_config_revision: Option<u64>,
) -> Result<Option<u64>, String> {
    match &entry.action {
        SessionStateRollbackAction::ConfigOverride {
            path,
            old_value,
            expected_revision,
        } => {
            let expected = chained_config_revision
                .unwrap_or_else(|| expected_revision.load(Ordering::Relaxed));
            let outcome = restore_config_override(
                &context.user_id,
                &context.session_id,
                path,
                old_value.clone(),
                expected,
                "tool_session_state_rollback:restore_entry",
            )?;
            settle_config_restore(context, path, expected_revision, expected, outcome)
        }
        SessionStateRollbackAction::Compression { snapshot, .. } => {
            restore_observability_snapshot(context.observability_session.as_ref(), snapshot)?;
            Ok(chained_config_revision)
        }
    }
}

fn settle_config_restore(
    context: &SessionStateRestoreContext,
    path: &str,
    owner_revision: &AtomicU64,
    expected_revision: u64,
    outcome: ConfigRestoreOutcome,
) -> Result<Option<u64>, String> {
    match outcome {
        ConfigRestoreOutcome::Applied {
            config, revision, ..
        } => {
            if let Some(observability_session) = context.observability_session.as_ref() {
                match observability_session.write() {
                    Ok(mut session) => session.config = *config,
                    Err(_) => tracing::warn!(
                        session_id = context.session_id,
                        path,
                        revision,
                        "config rollback committed but observability projection is unavailable"
                    ),
                }
            }
            Ok(Some(revision))
        }
        ConfigRestoreOutcome::Rejected {
            current_revision,
            current_config,
            ..
        } => {
            if let Some(observability_session) = context.observability_session.as_ref() {
                match observability_session.write() {
                    Ok(mut session) => session.config = *current_config,
                    Err(_) => tracing::warn!(
                        session_id = context.session_id,
                        path,
                        current_revision,
                        "config rollback conflict preserved authority but observability projection is unavailable"
                    ),
                }
            }
            Err(format!(
                "config rollback revision conflict for {path}: expected {expected_revision}, current {current_revision}",
            ))
        }
        ConfigRestoreOutcome::OutcomeUnknown {
            revision,
            reason,
            observed_config,
            retry_revision,
        } => {
            if let Some(retry_revision) = retry_revision {
                owner_revision.store(retry_revision, Ordering::Relaxed);
            }
            if let Some(config) = observed_config
                && let Some(observability_session) = context.observability_session.as_ref()
            {
                observability_session
                    .write()
                    .map_err(|_| "Failed to acquire observability session".to_string())?
                    .config = *config;
            }
            Err(format!(
                "config rollback outcome unknown for {path} at revision {revision}: {reason}"
            ))
        }
    }
}

pub(crate) async fn execute_rollback_session_state<PublishWorkspace, PublishFuture>(
    context: RollbackSessionStateContext,
    args: Value,
    publish_current_workspace: PublishWorkspace,
) -> String
where
    PublishWorkspace: FnOnce() -> PublishFuture + Send + 'static,
    PublishFuture: Future<Output = Result<(), String>> + Send + 'static,
{
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return serde_json::json!({
            "success": false,
            "error": "rollback_session_state requires an active Tokio runtime",
            "side_effects_maybe": false,
        })
        .to_string();
    };
    match handle
        .spawn(execute_rollback_session_state_owned(
            context,
            args,
            publish_current_workspace,
        ))
        .await
    {
        Ok(output) => output,
        Err(error) => serde_json::json!({
            "success": false,
            "error": "rollback_session_state settlement task failed",
            "detail": error.to_string().chars().take(240).collect::<String>(),
            "side_effects_maybe": true,
        })
        .to_string(),
    }
}

async fn execute_rollback_session_state_owned<PublishWorkspace, PublishFuture>(
    context: RollbackSessionStateContext,
    args: Value,
    publish_current_workspace: PublishWorkspace,
) -> String
where
    PublishWorkspace: FnOnce() -> PublishFuture,
    PublishFuture: Future<Output = Result<(), String>>,
{
    if args.get("after_sequence").is_some() {
        return serde_json::json!({
            "success": false,
            "error": "unknown field 'after_sequence'; use 'session_state_after_sequence'",
        })
        .to_string();
    }
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
        .and_then(Value::as_u64)
        .unwrap_or(0);

    match scope {
        "list" => rollback_session_state_list(&context.journal),
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

async fn rollback_session_state_turn<PublishWorkspace, PublishFuture>(
    context: RollbackSessionStateContext,
    scope: &str,
    explicit_turn_index: Option<u64>,
    checkpoint: u64,
    publish_current_workspace: PublishWorkspace,
) -> String
where
    PublishWorkspace: FnOnce() -> PublishFuture,
    PublishFuture: Future<Output = Result<(), String>>,
{
    let turn_index = explicit_turn_index.unwrap_or(u64::from(context.current_turn_index)) as u32;
    let plan = if checkpoint > 0 {
        restore_plan_for_turn_since(&context.journal, turn_index, checkpoint)
    } else {
        restore_plan_for_turn(&context.journal, turn_index)
    };
    let mut restored = Vec::new();
    let mut failed = Vec::new();
    let mut chained_config_revision = None;
    for (index, entry) in plan.iter().enumerate() {
        match restore_entry(&context.restore_context, entry, chained_config_revision).await {
            Ok(next_revision) => {
                chained_config_revision = next_revision;
                let next_config_owner = next_revision.and_then(|revision| {
                    plan[index + 1..]
                        .iter()
                        .find(|entry| {
                            matches!(
                                entry.action,
                                SessionStateRollbackAction::ConfigOverride { .. }
                            )
                        })
                        .map(|entry| (entry.sequence, revision))
                });
                settle_restored_sequence(&context.journal, entry.sequence, next_config_owner);
                restored.push(rollback_session_state_entry_json(entry));
            }
            Err(error) => {
                let mut failed_entry = rollback_session_state_entry_json(entry)
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
    let mut warnings = Vec::new();
    if !plan.is_empty() {
        if let Err(error) = publish_current_workspace().await {
            warnings.push(serde_json::json!({
                "error": error,
                "kind": "workspace_artifact_publish"
            }));
        }
    }
    let summary = if plan.is_empty() {
        format!("No recorded session-state rollback handles found for turn {turn_index}")
    } else if failed.is_empty() && !warnings.is_empty() {
        format!(
            "Restored {} recorded session-state mutation{} for turn {turn_index}; workspace artifact publish warning recorded",
            restored.len(),
            if restored.len() == 1 { "" } else { "s" },
        )
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
        "warnings": warnings,
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

    const TOP_K_PATH: &str = "memory.retrieval_top_k";

    struct ConfigRollbackFixture {
        _guard: astra_services::session_journal::JournalDirGuard,
        _temp: tempfile::TempDir,
        session_id: &'static str,
        observability: Arc<RwLock<crate::observability::ObservabilitySession>>,
        baseline: u32,
    }

    impl ConfigRollbackFixture {
        fn new(session_id: &'static str) -> Self {
            let temp = tempfile::tempdir().unwrap();
            let guard = astra_services::session_journal::JournalDirGuard::new(temp.path());
            astra_services::session_workspace::write_workspace(
                &astra_services::session_workspace::WorkspaceMetadata::new(session_id, "gpt-5"),
            )
            .unwrap();
            let baseline = crate::server::tool_session_config::effective_runtime_config(Some(
                &astra_services::session_workspace::read_workspace(session_id).unwrap(),
            ))
            .unwrap()
            .memory
            .retrieval_top_k;
            Self {
                _guard: guard,
                _temp: temp,
                session_id,
                observability: Arc::new(RwLock::new(
                    crate::observability::ObservabilitySession::new_simple(session_id),
                )),
                baseline,
            }
        }

        fn apply(&self, value: u32, expected_revision: u64) -> u64 {
            match restore_config_override(
                "test-user",
                self.session_id,
                TOP_K_PATH,
                serde_json::json!(value),
                expected_revision,
                "test:rollback-fixture",
            )
            .unwrap()
            {
                ConfigRestoreOutcome::Applied { revision, .. } => revision,
                _ => panic!("fixture config mutation must apply"),
            }
        }

        fn persisted(&self) -> (u32, u64) {
            let workspace =
                astra_services::session_workspace::read_workspace(self.session_id).unwrap();
            (
                crate::server::tool_session_config::effective_runtime_config(Some(&workspace))
                    .unwrap()
                    .memory
                    .retrieval_top_k,
                workspace.config_mutation_revision,
            )
        }

        async fn rollback(
            &self,
            journal: Arc<Mutex<SessionStateRollbackJournal>>,
            publish_calls: Arc<std::sync::atomic::AtomicUsize>,
        ) -> Value {
            let output = execute_rollback_session_state(
                RollbackSessionStateContext {
                    journal,
                    current_turn_index: 1,
                    restore_context: SessionStateRestoreContext {
                        user_id: "test-user".into(),
                        session_id: self.session_id.into(),
                        observability_session: Some(self.observability.clone()),
                    },
                },
                serde_json::json!({"scope": "current_turn"}),
                move || async move {
                    publish_calls.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                },
            )
            .await;
            serde_json::from_str(&output).unwrap()
        }
    }

    fn config_action(value: u32, expected_revision: u64) -> SessionStateRollbackAction {
        SessionStateRollbackAction::ConfigOverride {
            path: TOP_K_PATH.into(),
            old_value: serde_json::json!(value),
            expected_revision: Arc::new(AtomicU64::new(expected_revision)),
        }
    }

    fn observability_snapshot() -> Box<crate::observability::ObservabilitySessionRollbackSnapshot> {
        Box::new(crate::observability::ObservabilitySessionRollbackSnapshot {
            config: astra_config::runtime_config::RuntimeConfig::default(),
            original_query: None,
            recent_queries: vec![],
            compressed_turns: vec![],
            user_corrections: vec![],
            context_traces: vec![],
            last_query_at: None,
        })
    }

    #[test]
    fn restore_plan_returns_newest_first_and_honors_checkpoint() {
        let mut journal = SessionStateRollbackJournal::default();
        journal.record(
            3,
            "before".to_string(),
            SessionStateRollbackAction::Compression {
                turn: 2,
                snapshot: observability_snapshot(),
            },
        );
        let checkpoint = journal.checkpoint();
        journal.record(
            3,
            "first".to_string(),
            SessionStateRollbackAction::Compression {
                turn: 3,
                snapshot: observability_snapshot(),
            },
        );
        journal.record(
            3,
            "second".to_string(),
            SessionStateRollbackAction::Compression {
                turn: 3,
                snapshot: observability_snapshot(),
            },
        );
        journal.record(
            4,
            "wrong-turn".to_string(),
            SessionStateRollbackAction::Compression {
                turn: 4,
                snapshot: observability_snapshot(),
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
    fn journal_helpers_record_list_and_remove() {
        let journal = Arc::new(Mutex::new(SessionStateRollbackJournal::default()));

        record(
            &journal,
            5,
            "compression-1".to_string(),
            SessionStateRollbackAction::Compression {
                turn: 4,
                snapshot: observability_snapshot(),
            },
        );
        record(
            &journal,
            5,
            "compression".to_string(),
            SessionStateRollbackAction::Compression {
                turn: 5,
                snapshot: observability_snapshot(),
            },
        );

        assert_eq!(journal_checkpoint(&journal), 2);
        let listed = entries(&journal);
        assert_eq!(listed.len(), 2);
        assert_eq!(restore_plan_for_turn(&journal, 5).len(), 2);
        assert!(remove_sequence(&journal, listed[0].sequence));
        assert_eq!(entries(&journal).len(), 1);
    }

    #[test]
    fn rollback_entry_json_omits_invalid_timestamp_instead_of_failing() {
        let entry = SessionStateRollbackEntry {
            sequence: 0,
            turn_index: 9,
            timestamp: UNIX_EPOCH - Duration::from_millis(1),
            label: "compression".to_string(),
            action: SessionStateRollbackAction::Compression {
                turn: 9,
                snapshot: observability_snapshot(),
            },
        };

        let value = rollback_session_state_entry_json(&entry);

        assert_eq!(value["label"], "compression");
        assert_eq!(value["kind"], "compression");
        assert_eq!(value["turn_index"], 9);
        assert!(value.get("timestamp_ms").is_none());
    }

    #[test]
    fn remove_sequence_reports_missing_entries_without_mutating() {
        let mut journal = SessionStateRollbackJournal::default();
        journal.record(
            1,
            "compression".to_string(),
            SessionStateRollbackAction::Compression {
                turn: 1,
                snapshot: observability_snapshot(),
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
        let context = SessionStateRestoreContext {
            user_id: "test-user".into(),
            session_id: "session-1".into(),
            observability_session: None,
        };

        let error = restore_entry(&context, &entry, None)
            .await
            .expect_err("missing observability session must fail closed");

        assert_eq!(error, "No observability session available");
    }

    #[tokio::test]
    async fn execute_rollback_session_state_requires_turn_index_for_turn_scope() {
        let journal = Arc::new(Mutex::new(SessionStateRollbackJournal::default()));
        let output = execute_rollback_session_state(
            RollbackSessionStateContext {
                journal,
                current_turn_index: 1,
                restore_context: SessionStateRestoreContext {
                    user_id: "test-user".into(),
                    session_id: "session-1".into(),
                    observability_session: None,
                },
            },
            serde_json::json!({"scope": "turn"}),
            || async { Ok(()) },
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
        let journal = Arc::new(Mutex::new(SessionStateRollbackJournal::default()));
        let publish_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let output = execute_rollback_session_state(
            RollbackSessionStateContext {
                journal,
                current_turn_index: 9,
                restore_context: SessionStateRestoreContext {
                    user_id: "test-user".into(),
                    session_id: "session-1".into(),
                    observability_session: None,
                },
            },
            serde_json::json!({"scope": "current_turn"}),
            {
                let publish_calls = publish_calls.clone();
                move || async move {
                    publish_calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    Ok(())
                }
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
    async fn execute_rollback_session_state_keeps_restore_success_when_publish_fails() {
        let journal = Arc::new(Mutex::new(SessionStateRollbackJournal::default()));
        record(
            &journal,
            4,
            "compression".to_string(),
            SessionStateRollbackAction::Compression {
                turn: 4,
                snapshot: observability_snapshot(),
            },
        );
        let observability_session = Arc::new(RwLock::new(
            crate::observability::ObservabilitySession::new_simple("session-1"),
        ));

        let output = execute_rollback_session_state(
            RollbackSessionStateContext {
                journal,
                current_turn_index: 4,
                restore_context: SessionStateRestoreContext {
                    user_id: "test-user".into(),
                    session_id: "session-1".into(),
                    observability_session: Some(observability_session),
                },
            },
            serde_json::json!({"scope": "current_turn"}),
            || async { Err("publish failed".to_string()) },
        )
        .await;

        let value: Value = serde_json::from_str(&output).expect("rollback json");
        assert_eq!(value["success"], true);
        assert_eq!(value["failed"].as_array().map(Vec::len), Some(0));
        assert_eq!(value["warnings"][0]["kind"], "workspace_artifact_publish");
        assert_eq!(value["warnings"][0]["error"], "publish failed");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn multi_config_rollback_chains_fresh_owner_revisions() {
        let fixture = ConfigRollbackFixture::new("rollback-config-chain");
        let first = if fixture.baseline == 5 { 6 } else { 5 };
        let second = if first == 7 { 8 } else { 7 };
        assert_eq!(fixture.apply(first, 0), 1);
        assert_eq!(fixture.apply(second, 1), 2);
        let journal = Arc::new(Mutex::new(SessionStateRollbackJournal::default()));
        record(
            &journal,
            1,
            "first".into(),
            config_action(fixture.baseline, 1),
        );
        record(&journal, 1, "second".into(), config_action(first, 2));
        let publish_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let output = fixture
            .rollback(journal.clone(), publish_calls.clone())
            .await;

        assert_eq!(output["success"], true);
        assert_eq!(output["restored"].as_array().map(Vec::len), Some(2));
        assert_eq!(fixture.persisted(), (fixture.baseline, 4));
        assert_eq!(publish_calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn aba_writer_blocks_rollback_and_still_publishes_current() {
        let fixture = ConfigRollbackFixture::new("rollback-config-aba");
        let first = if fixture.baseline == 5 { 6 } else { 5 };
        let second = if first == 7 { 8 } else { 7 };
        assert_eq!(fixture.apply(first, 0), 1);
        let journal = Arc::new(Mutex::new(SessionStateRollbackJournal::default()));
        record(
            &journal,
            1,
            "owned".into(),
            config_action(fixture.baseline, 1),
        );
        assert_eq!(fixture.apply(first, 1), 2);
        assert_eq!(fixture.apply(second, 2), 3);
        assert_eq!(fixture.apply(first, 3), 4);
        let publish_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let output = fixture
            .rollback(journal.clone(), publish_calls.clone())
            .await;

        assert_eq!(output["success"], false);
        assert_eq!(output["failed"].as_array().map(Vec::len), Some(1));
        assert_eq!(fixture.persisted(), (first, 4));
        assert_eq!(
            fixture
                .observability
                .read()
                .unwrap()
                .config
                .memory
                .retrieval_top_k,
            first
        );
        assert_eq!(entries(&journal).len(), 1);
        assert_eq!(publish_calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn poisoned_projection_does_not_replace_revision_conflict() {
        let fixture = ConfigRollbackFixture::new("rollback-config-poisoned-conflict");
        let first = if fixture.baseline == 5 { 6 } else { 5 };
        let second = if first == 7 { 8 } else { 7 };
        fixture.apply(first, 0);
        let journal = Arc::new(Mutex::new(SessionStateRollbackJournal::default()));
        record(
            &journal,
            1,
            "owned".into(),
            config_action(fixture.baseline, 1),
        );
        fixture.apply(second, 1);
        let poisoned = fixture.observability.clone();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _guard = poisoned.write().unwrap();
            panic!("poison observability projection");
        }));
        let publish_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let output = fixture
            .rollback(journal.clone(), publish_calls.clone())
            .await;

        assert_eq!(output["success"], false);
        assert_eq!(output["failed"].as_array().map(Vec::len), Some(1));
        assert_eq!(fixture.persisted(), (second, 2));
        assert_eq!(entries(&journal).len(), 1);
        assert_eq!(publish_calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn partial_plan_transfers_owner_revision_across_calls() {
        let fixture = ConfigRollbackFixture::new("rollback-config-partial");
        let first = if fixture.baseline == 5 { 6 } else { 5 };
        let second = if first == 7 { 8 } else { 7 };
        fixture.apply(first, 0);
        fixture.apply(second, 1);
        let journal = Arc::new(Mutex::new(SessionStateRollbackJournal::default()));
        record(
            &journal,
            1,
            "unprocessed".into(),
            config_action(fixture.baseline, 1),
        );
        record(
            &journal,
            1,
            "failed".into(),
            SessionStateRollbackAction::ConfigOverride {
                path: "missing.path".into(),
                old_value: Value::Null,
                expected_revision: Arc::new(AtomicU64::new(2)),
            },
        );
        record(&journal, 1, "successful".into(), config_action(first, 2));
        let publish_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let output = fixture
            .rollback(journal.clone(), publish_calls.clone())
            .await;

        assert_eq!(output["restored"].as_array().map(Vec::len), Some(1));
        assert_eq!(output["failed"].as_array().map(Vec::len), Some(1));
        assert_eq!(fixture.persisted(), (first, 3));
        let remaining = entries(&journal);
        assert_eq!(remaining.len(), 2);
        assert_eq!(remaining[0].sequence, 1);
        let SessionStateRollbackAction::ConfigOverride {
            expected_revision, ..
        } = &remaining[0].action
        else {
            panic!("next rollback entry must be config-owned");
        };
        assert_eq!(expected_revision.load(Ordering::Relaxed), 3);
        assert_eq!(publish_calls.load(Ordering::Relaxed), 1);

        {
            let mut journal = journal.lock().unwrap();
            let SessionStateRollbackAction::ConfigOverride {
                path, old_value, ..
            } = &mut journal
                .entries
                .iter_mut()
                .find(|entry| entry.sequence == 1)
                .unwrap()
                .action
            else {
                panic!("failed entry must remain config-owned");
            };
            *path = "memory.retrieval_top_k".into();
            *old_value = Value::from(first);
        }
        let retry = fixture
            .rollback(journal.clone(), publish_calls.clone())
            .await;
        assert_eq!(retry["success"], true);
        assert_eq!(retry["restored"].as_array().map(Vec::len), Some(2));
        assert_eq!(fixture.persisted(), (fixture.baseline, 5));
        assert!(entries(&journal).is_empty());
        assert_eq!(publish_calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn outcome_unknown_readback_advances_retry_owner_then_succeeds() {
        let fixture = ConfigRollbackFixture::new("rollback-config-unknown-retry");
        let first = if fixture.baseline == 5 { 6 } else { 5 };
        let second = if first == 7 { 8 } else { 7 };
        fixture.apply(first, 0);
        fixture.apply(second, 1);
        let entry = SessionStateRollbackEntry {
            sequence: 0,
            turn_index: 1,
            timestamp: UNIX_EPOCH,
            label: "retry".into(),
            action: config_action(first, 1),
        };
        let context = SessionStateRestoreContext {
            user_id: "test-user".into(),
            session_id: fixture.session_id.into(),
            observability_session: Some(fixture.observability.clone()),
        };
        let current_config = crate::server::tool_session_config::effective_runtime_config(Some(
            &astra_services::session_workspace::read_workspace(fixture.session_id).unwrap(),
        ))
        .unwrap();
        let SessionStateRollbackAction::ConfigOverride {
            expected_revision, ..
        } = &entry.action
        else {
            unreachable!()
        };

        let error = settle_config_restore(
            &context,
            TOP_K_PATH,
            expected_revision,
            1,
            ConfigRestoreOutcome::OutcomeUnknown {
                revision: 2,
                reason: "directory sync outcome unknown".into(),
                observed_config: Some(Box::new(current_config)),
                retry_revision: Some(2),
            },
        )
        .unwrap_err();
        assert!(error.contains("outcome unknown"));
        assert_eq!(expected_revision.load(Ordering::Relaxed), 2);

        assert_eq!(
            restore_entry(&context, &entry, None).await.unwrap(),
            Some(3)
        );
        assert_eq!(fixture.persisted(), (first, 3));
    }
}
