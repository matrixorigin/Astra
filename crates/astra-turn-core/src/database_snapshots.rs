//! Execution-local MatrixOne snapshot rollback records and restore planning.
//! Connection, credentials and SQL execution remain on the selected adapter.

use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseSnapshotRollbackEntry {
    pub sequence: u64,
    pub snapshot_id: String,
    pub database: Option<String>,
    pub turn_index: u32,
    restore_completed: bool,
}

#[derive(Debug, Default)]
pub struct DatabaseSnapshotRollbackJournal {
    entries: Vec<DatabaseSnapshotRollbackEntry>,
    next_sequence: u64,
}

impl DatabaseSnapshotRollbackJournal {
    pub fn record(
        &mut self,
        snapshot_id: impl Into<String>,
        database: Option<String>,
        turn_index: u32,
    ) {
        self.entries.push(DatabaseSnapshotRollbackEntry {
            sequence: self.next_sequence,
            snapshot_id: snapshot_id.into(),
            database,
            turn_index,
            restore_completed: false,
        });
        self.next_sequence = self.next_sequence.saturating_add(1);
    }

    pub fn list(&self) -> Vec<DatabaseSnapshotRollbackEntry> {
        self.entries.iter().rev().cloned().collect()
    }

    pub fn entry_for_snapshot(&self, snapshot_id: &str) -> Option<DatabaseSnapshotRollbackEntry> {
        self.entries
            .iter()
            .rev()
            .find(|entry| entry.snapshot_id == snapshot_id)
            .cloned()
    }

    pub fn restore_plan_for_turn(&self, turn_index: u32) -> Vec<DatabaseSnapshotRollbackEntry> {
        self.restore_plan_for_turn_since(turn_index, 0)
    }

    pub fn restore_plan_for_turn_since(
        &self,
        turn_index: u32,
        checkpoint: u64,
    ) -> Vec<DatabaseSnapshotRollbackEntry> {
        let mut seen_databases = HashSet::new();
        let mut plan = Vec::new();
        for entry in self
            .entries
            .iter()
            .filter(|entry| entry.turn_index == turn_index && entry.sequence >= checkpoint)
        {
            if entry.restore_completed || seen_databases.insert(entry.database.clone()) {
                plan.push(entry.clone());
            }
        }
        plan
    }

    pub fn checkpoint(&self) -> u64 {
        self.next_sequence
    }

    pub fn remove_snapshot(&mut self, snapshot_id: &str) -> bool {
        if let Some(index) = self
            .entries
            .iter()
            .rposition(|entry| entry.snapshot_id == snapshot_id)
        {
            self.entries.remove(index);
            true
        } else {
            false
        }
    }
}
pub fn with_journal_mut<T>(
    journal: &Mutex<DatabaseSnapshotRollbackJournal>,
    operation: &'static str,
    f: impl FnOnce(&mut DatabaseSnapshotRollbackJournal) -> T,
) -> T {
    match journal.lock() {
        Ok(mut journal) => f(&mut journal),
        Err(poisoned) => {
            tracing::warn!(
                operation,
                "database_snapshot_journal mutex poisoned; recovering inner journal"
            );
            let mut journal = poisoned.into_inner();
            f(&mut journal)
        }
    }
}

fn with_journal<T>(
    journal: &Mutex<DatabaseSnapshotRollbackJournal>,
    operation: &'static str,
    f: impl FnOnce(&DatabaseSnapshotRollbackJournal) -> T,
) -> T {
    match journal.lock() {
        Ok(journal) => f(&journal),
        Err(poisoned) => {
            tracing::warn!(
                operation,
                "database_snapshot_journal mutex poisoned; recovering inner journal"
            );
            let journal = poisoned.into_inner();
            f(&journal)
        }
    }
}

pub fn journal_checkpoint(journal: &Mutex<DatabaseSnapshotRollbackJournal>) -> u64 {
    with_journal(journal, "database_snapshot_journal_checkpoint", |journal| {
        journal.checkpoint()
    })
}

#[cfg(test)]
fn record_rollback(
    journal: &Mutex<DatabaseSnapshotRollbackJournal>,
    snapshot_id: impl Into<String>,
    database: Option<String>,
    turn_index: u32,
) {
    with_journal_mut(journal, "record_database_snapshot_rollback", |journal| {
        journal.record(snapshot_id, database, turn_index)
    });
}

#[cfg(test)]
fn entries(journal: &Mutex<DatabaseSnapshotRollbackJournal>) -> Vec<DatabaseSnapshotRollbackEntry> {
    with_journal(journal, "database_snapshot_entries", |journal| {
        journal.list()
    })
}

pub fn rollback_entry_json(entry: &DatabaseSnapshotRollbackEntry) -> Value {
    let mut value = serde_json::Map::from_iter([
        (
            "snapshot_id".to_string(),
            Value::String(entry.snapshot_id.clone()),
        ),
        (
            "turn_index".to_string(),
            Value::Number(serde_json::Number::from(entry.turn_index)),
        ),
    ]);
    if let Some(database) = entry.database.as_ref() {
        value.insert("database".to_string(), Value::String(database.clone()));
    }
    value.insert(
        "restore_completed".to_string(),
        Value::Bool(entry.restore_completed),
    );
    Value::Object(value)
}

pub fn is_valid_snapshot_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

/// The adapter executes SQL using its explicitly selected credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotRollbackOperation {
    Restore,
    Drop,
}

fn restore_and_cleanup(
    journal: &mut DatabaseSnapshotRollbackJournal,
    entry: &DatabaseSnapshotRollbackEntry,
    turn_cutoff: Option<u64>,
    cleanup_attempts: &mut HashSet<String>,
    execute: &mut impl FnMut(
        &DatabaseSnapshotRollbackEntry,
        SnapshotRollbackOperation,
    ) -> Result<(), String>,
) -> Result<(), String> {
    let targets = {
        if let Some(saved) = journal
            .entries
            .iter_mut()
            .rfind(|saved| saved.snapshot_id == entry.snapshot_id)
        {
            saved.database = entry.database.clone();
        } else {
            journal.record(&entry.snapshot_id, entry.database.clone(), entry.turn_index);
        }
        journal
            .entries
            .iter()
            .filter(|saved| {
                saved.snapshot_id == entry.snapshot_id
                    || turn_cutoff.is_some_and(|cutoff| {
                        saved.turn_index == entry.turn_index
                            && saved.database == entry.database
                            && saved.sequence >= entry.sequence
                            && saved.sequence < cutoff
                            && (!entry.restore_completed || saved.restore_completed)
                    })
            })
            .cloned()
            .collect::<Vec<_>>()
    };
    if !entry.restore_completed {
        execute(entry, SnapshotRollbackOperation::Restore)?;
        let sequences: HashSet<_> = targets.iter().map(|target| target.sequence).collect();
        {
            for saved in &mut journal.entries {
                if sequences.contains(&saved.sequence) {
                    saved.restore_completed = true;
                }
            }
        }
    }
    let mut errors = Vec::new();
    for target in targets {
        if !cleanup_attempts.insert(target.snapshot_id.clone()) {
            continue;
        }
        match execute(&target, SnapshotRollbackOperation::Drop) {
            Ok(()) => {
                journal.remove_snapshot(&target.snapshot_id);
            }
            Err(error) => errors.push(format!(
                "snapshot `{}` was restored but cleanup failed: {error}",
                target.snapshot_id
            )),
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

/// Serialize rollback on the existing execution-local journal lock. The SQL
/// callback must not reenter this journal. Mutating query adapters use the same
/// lock for capture, recording and query execution.
pub fn rollback_database_snapshots(
    journal: &Mutex<DatabaseSnapshotRollbackJournal>,
    args: &Value,
    current_turn_index: u32,
    default_database: &str,
    execute: impl FnMut(&DatabaseSnapshotRollbackEntry, SnapshotRollbackOperation) -> Result<(), String>,
) -> String {
    with_journal_mut(journal, "rollback_database_snapshots", |journal| {
        rollback_locked(journal, args, current_turn_index, default_database, execute)
    })
}

fn rollback_locked(
    journal: &mut DatabaseSnapshotRollbackJournal,
    args: &Value,
    current_turn_index: u32,
    default_database: &str,
    mut execute: impl FnMut(
        &DatabaseSnapshotRollbackEntry,
        SnapshotRollbackOperation,
    ) -> Result<(), String>,
) -> String {
    let mut cleanup_attempts = HashSet::new();
    if args.get("after_sequence").is_some() {
        return json!({
            "success": false,
            "error": "unknown field 'after_sequence'; use 'database_after_sequence'",
        })
        .to_string();
    }
    let scope = args
        .get("scope")
        .and_then(Value::as_str)
        .or_else(|| {
            if args.get("snapshot_id").is_some() {
                Some("snapshot")
            } else {
                None
            }
        })
        .unwrap_or("current_turn");

    match scope {
        "list" => {
            let entries = journal
                .list()
                .into_iter()
                .map(|entry| rollback_entry_json(&entry))
                .collect::<Vec<_>>();
            json!({
                "success": true,
                "scope": "list",
                "total_entries": entries.len(),
                "entries": entries,
            })
            .to_string()
        }
        "snapshot" => {
            let snapshot_id = match args.get("snapshot_id").and_then(Value::as_str) {
                Some(snapshot_id) if is_valid_snapshot_name(snapshot_id) => snapshot_id,
                Some(snapshot_id) => {
                    return json!({
                        "success": false,
                        "scope": "snapshot",
                        "error": format!("invalid snapshot_id `{snapshot_id}`"),
                    })
                    .to_string();
                }
                None => {
                    return json!({
                        "success": false,
                        "scope": "snapshot",
                        "error": "missing 'snapshot_id' for scope=snapshot",
                    })
                    .to_string();
                }
            };
            let journal_entry = journal.entry_for_snapshot(snapshot_id);
            let database = args
                .get("database")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|database| !database.is_empty())
                .map(ToString::to_string)
                .or_else(|| {
                    journal_entry
                        .as_ref()
                        .and_then(|entry| entry.database.clone())
                })
                .or_else(|| Some(default_database.to_string()));
            let entry = DatabaseSnapshotRollbackEntry {
                sequence: journal_entry.as_ref().map_or(0, |entry| entry.sequence),
                snapshot_id: snapshot_id.to_string(),
                database,
                turn_index: journal_entry
                    .as_ref()
                    .map_or(current_turn_index, |entry| entry.turn_index),
                restore_completed: journal_entry
                    .as_ref()
                    .is_some_and(|entry| entry.restore_completed),
            };
            if entry.restore_completed
                && journal_entry.as_ref().is_some_and(|saved| {
                    saved.database.as_deref().unwrap_or(default_database)
                        != entry.database.as_deref().unwrap_or(default_database)
                })
            {
                return json!({"success": false, "error": "cannot change database while snapshot cleanup is pending"}).to_string();
            }
            let cutoff = journal_entry.as_ref().map(|_| journal.checkpoint());
            match restore_and_cleanup(journal, &entry, cutoff, &mut cleanup_attempts, &mut execute) {
                Ok(()) => {
                    journal.remove_snapshot(snapshot_id);
                    let database = entry.database.clone();
                    json!({
                        "success": true,
                        "scope": "snapshot",
                        "snapshot_id": snapshot_id,
                        "database": database,
                        "restore_completed": true,
                        "summary": format!(
                            "Restored MatrixOne snapshot `{}`{}",
                            snapshot_id,
                            database
                                .as_deref()
                                .map(|database| format!(" for database `{database}`"))
                                .unwrap_or_default()
                        ),
                    })
                    .to_string()
                }
                Err(error) => json!({
                    "success": false,
                    "scope": "snapshot",
                    "snapshot_id": snapshot_id,
                    "database": entry.database.clone(),
                    "restore_completed": journal.entry_for_snapshot(snapshot_id).is_none_or(|saved| saved.restore_completed),
                    "error": error,
                })
                .to_string(),
            }
        }
        "turn" | "current_turn" => {
            let turn_index = if scope == "turn" {
                match args.get("turn_index").and_then(Value::as_u64) {
                    Some(turn_index) => turn_index as u32,
                    None => {
                        return json!({
                            "success": false,
                            "scope": "turn",
                            "error": "missing 'turn_index' for scope=turn",
                        })
                        .to_string();
                    }
                }
            } else {
                current_turn_index
            };
            let checkpoint = args
                .get("database_after_sequence")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let plan = if checkpoint > 0 {
                journal.restore_plan_for_turn_since(turn_index, checkpoint)
            } else {
                journal.restore_plan_for_turn(turn_index)
            };
            let cutoff = journal.checkpoint();
            let mut restored = Vec::new();
            let mut failed = Vec::new();
            for entry in &plan {
                if cleanup_attempts.contains(&entry.snapshot_id)
                    || journal.entry_for_snapshot(&entry.snapshot_id).is_none()
                {
                    continue;
                }
                match restore_and_cleanup(
                    journal,
                    entry,
                    Some(cutoff),
                    &mut cleanup_attempts,
                    &mut execute,
                ) {
                    Ok(()) => {
                        journal.remove_snapshot(&entry.snapshot_id);
                        let mut completed = entry.clone();
                        completed.restore_completed = true;
                        restored.push(rollback_entry_json(&completed));
                    }
                    Err(error) => {
                        for pending in journal.list().into_iter().filter(|pending| {
                            pending.snapshot_id == entry.snapshot_id
                                || (pending.restore_completed
                                    && cleanup_attempts.contains(&pending.snapshot_id)
                                    && pending.turn_index == entry.turn_index
                                    && pending.database == entry.database
                                    && pending.sequence >= entry.sequence
                                    && pending.sequence < cutoff)
                        }) {
                            let mut failed_entry = rollback_entry_json(&pending)
                                .as_object()
                                .cloned()
                                .unwrap_or_default();
                            failed_entry.insert("error".to_string(), Value::String(error.clone()));
                            failed.push(Value::Object(failed_entry));
                        }
                    }
                }
            }
            let success = !restored.is_empty() && failed.is_empty();
            let summary = if plan.is_empty() {
                format!("No recorded MatrixOne snapshots found for turn {turn_index}")
            } else if failed.is_empty() {
                format!(
                    "Completed rollback and cleanup for {} MatrixOne snapshot{} for turn {turn_index}",
                    restored.len(),
                    if restored.len() == 1 { "" } else { "s" }
                )
            } else {
                format!(
                    "Completed rollback and cleanup for {} MatrixOne snapshot{} for turn {turn_index} with {} failure{}",
                    restored.len(),
                    if restored.len() == 1 { "" } else { "s" },
                    failed.len(),
                    if failed.len() == 1 { "" } else { "s" }
                )
            };
            json!({
                "success": success,
                "scope": scope,
                "turn_index": turn_index,
                "restored": restored,
                "failed": failed,
                "summary": summary,
            })
            .to_string()
        }
        other => json!({
            "success": false,
            "error": format!(
                "unknown scope `{other}`. Supported: current_turn, turn, snapshot, list"
            ),
        })
        .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_failure_retains_plan_and_retries_restore() {
        let journal = Mutex::new(DatabaseSnapshotRollbackJournal::default());
        record_rollback(&journal, "first", Some("db".into()), 1);
        let args = json!({"scope": "current_turn"});
        let failed: Value = serde_json::from_str(&rollback_database_snapshots(
            &journal,
            &args,
            1,
            "default_db",
            |_, operation| {
                assert_eq!(operation, SnapshotRollbackOperation::Restore);
                Err("restore failed".into())
            },
        ))
        .unwrap();
        assert_eq!(failed["success"], false);
        assert_eq!(entries(&journal).len(), 1);
        assert!(!entries(&journal)[0].restore_completed);
        let mut operations = Vec::new();
        let result: Value = serde_json::from_str(&rollback_database_snapshots(
            &journal,
            &args,
            1,
            "default_db",
            |_, operation| {
                operations.push(operation);
                Ok(())
            },
        ))
        .unwrap();
        assert_eq!(result["success"], true);
        assert_eq!(
            operations,
            [
                SnapshotRollbackOperation::Restore,
                SnapshotRollbackOperation::Drop
            ]
        );
        assert!(entries(&journal).is_empty());
    }

    #[test]
    fn cleanup_retry_never_restores_again_or_uses_later_snapshot() {
        let journal = Mutex::new(DatabaseSnapshotRollbackJournal::default());
        record_rollback(&journal, "before_cutoff", Some("db".into()), 1);
        let cutoff = journal_checkpoint(&journal);
        record_rollback(&journal, "first", Some("db".into()), 1);
        record_rollback(&journal, "later", Some("db".into()), 1);
        let args = json!({"scope": "turn", "turn_index": 1, "database_after_sequence": cutoff});
        let mut operations = Vec::new();
        let failed: Value = serde_json::from_str(&rollback_database_snapshots(
            &journal,
            &args,
            1,
            "default_db",
            |entry, operation| {
                operations.push((entry.snapshot_id.clone(), operation));
                if operation == SnapshotRollbackOperation::Drop {
                    Err("drop failed".into())
                } else {
                    Ok(())
                }
            },
        ))
        .unwrap();
        assert_eq!(failed["success"], false);
        assert_eq!(
            operations,
            [
                ("first".into(), SnapshotRollbackOperation::Restore),
                ("first".into(), SnapshotRollbackOperation::Drop),
                ("later".into(), SnapshotRollbackOperation::Drop)
            ]
        );
        assert_eq!(entries(&journal).len(), 3);
        operations.clear();
        let result: Value = serde_json::from_str(&rollback_database_snapshots(
            &journal,
            &args,
            1,
            "default_db",
            |entry, operation| {
                operations.push((entry.snapshot_id.clone(), operation));
                Ok(())
            },
        ))
        .unwrap();
        assert_eq!(result["success"], true);
        assert!(
            operations
                .iter()
                .all(|(_, operation)| *operation == SnapshotRollbackOperation::Drop)
        );
        assert_eq!(operations.len(), 2);
        assert_eq!(entries(&journal)[0].snapshot_id, "before_cutoff");
    }

    #[test]
    fn mixed_database_failure_retains_only_unfinished_entries() {
        let journal = Mutex::new(DatabaseSnapshotRollbackJournal::default());
        record_rollback(&journal, "good", Some("a".into()), 1);
        record_rollback(&journal, "bad", Some("b".into()), 1);
        let args = json!({"scope": "current_turn"});
        let result: Value = serde_json::from_str(&rollback_database_snapshots(
            &journal,
            &args,
            1,
            "default_db",
            |entry, _| {
                if entry.snapshot_id == "bad" {
                    Err("unavailable".into())
                } else {
                    Ok(())
                }
            },
        ))
        .unwrap();
        assert_eq!(result["success"], false);
        assert_eq!(result["restored"].as_array().unwrap().len(), 1);
        assert_eq!(result["restored"][0]["restore_completed"], true);
        assert_eq!(result["failed"].as_array().unwrap().len(), 1);
        assert_eq!(entries(&journal)[0].snapshot_id, "bad");
        assert_eq!(entries(&journal).len(), 1);
    }
    #[test]
    fn external_snapshot_cleanup_accepts_explicit_default_database() {
        let journal = Mutex::new(DatabaseSnapshotRollbackJournal::default());
        let args = json!({"snapshot_id": "external"});
        let result: Value = serde_json::from_str(&rollback_database_snapshots(
            &journal,
            &args,
            1,
            "default_db",
            |_, operation| {
                if operation == SnapshotRollbackOperation::Drop {
                    Err("drop failed".into())
                } else {
                    Ok(())
                }
            },
        ))
        .unwrap();
        assert_eq!(result["restore_completed"], true);
        let retry = json!({"snapshot_id": "external", "database": "default_db"});
        let result: Value = serde_json::from_str(&rollback_database_snapshots(
            &journal,
            &retry,
            1,
            "default_db",
            |_, operation| {
                assert_eq!(operation, SnapshotRollbackOperation::Drop);
                Ok(())
            },
        ))
        .unwrap();
        assert_eq!(result["success"], true);
        assert!(entries(&journal).is_empty());
    }

    #[test]
    fn snapshot_restore_invalidates_later_snapshots_and_reports_pending_cleanup() {
        let journal = Mutex::new(DatabaseSnapshotRollbackJournal::default());
        record_rollback(&journal, "first", Some("db".into()), 1);
        record_rollback(&journal, "later", Some("db".into()), 1);
        let result: Value = serde_json::from_str(&rollback_database_snapshots(
            &journal,
            &json!({"snapshot_id": "first"}),
            1,
            "db",
            |entry, operation| {
                if entry.snapshot_id == "later" && operation == SnapshotRollbackOperation::Drop {
                    Err("drop failed".into())
                } else {
                    Ok(())
                }
            },
        ))
        .unwrap();
        assert_eq!(result["success"], false);
        assert_eq!(entries(&journal)[0].snapshot_id, "later");
        assert!(entries(&journal)[0].restore_completed);
        let mut drops = 0;
        let result: Value = serde_json::from_str(&rollback_database_snapshots(
            &journal,
            &json!({"scope": "current_turn"}),
            1,
            "db",
            |_, operation| {
                assert_eq!(operation, SnapshotRollbackOperation::Drop);
                drops += 1;
                Err("still unavailable".into())
            },
        ))
        .unwrap();
        assert_eq!(drops, 1);
        assert_eq!(result["failed"][0]["snapshot_id"], "later");
        assert_eq!(result["failed"][0]["restore_completed"], true);
    }

    #[test]
    fn pending_cleanup_is_attempted_once_per_request() {
        let journal = Mutex::new(DatabaseSnapshotRollbackJournal::default());
        record_rollback(&journal, "first", Some("db".into()), 1);
        record_rollback(&journal, "later", Some("db".into()), 1);
        let args = json!({"scope": "current_turn"});
        rollback_database_snapshots(&journal, &args, 1, "db", |_, operation| {
            if operation == SnapshotRollbackOperation::Drop {
                Err("drop failed".into())
            } else {
                Ok(())
            }
        });
        let mut later_drops = 0;
        let result: Value = serde_json::from_str(&rollback_database_snapshots(
            &journal,
            &args,
            1,
            "db",
            |entry, operation| {
                assert_eq!(operation, SnapshotRollbackOperation::Drop);
                if entry.snapshot_id == "later" {
                    later_drops += 1;
                    Err("drop failed".into())
                } else {
                    Ok(())
                }
            },
        ))
        .unwrap();
        assert_eq!(later_drops, 1);
        assert_eq!(entries(&journal).len(), 1);
        assert_eq!(result["failed"][0]["snapshot_id"], "later");
    }

    #[test]
    fn rollback_holds_the_query_journal_boundary_until_sql_finishes() {
        use std::sync::{Arc, mpsc};
        let journal = Arc::new(Mutex::new(DatabaseSnapshotRollbackJournal::default()));
        record_rollback(&journal, "first", Some("db".into()), 1);
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let worker_journal = journal.clone();
        let worker = std::thread::spawn(move || {
            rollback_database_snapshots(
                &worker_journal,
                &json!({"scope": "current_turn"}),
                1,
                "db",
                |_, operation| {
                    if operation == SnapshotRollbackOperation::Restore {
                        entered_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                    }
                    Ok(())
                },
            )
        });
        entered_rx.recv().unwrap();
        assert!(journal.try_lock().is_err());
        release_tx.send(()).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&worker.join().unwrap()).unwrap()["success"],
            true
        );
        assert!(entries(&journal).is_empty());
    }
}
