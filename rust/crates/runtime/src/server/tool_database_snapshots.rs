use std::collections::HashSet;
use std::process::Command;
use std::sync::Mutex;

use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DatabaseSnapshotRollbackEntry {
    pub(crate) sequence: u64,
    pub(crate) snapshot_id: String,
    pub(crate) database: Option<String>,
    pub(crate) turn_index: u32,
}

#[derive(Debug, Default)]
pub(crate) struct DatabaseSnapshotRollbackJournal {
    entries: Vec<DatabaseSnapshotRollbackEntry>,
    next_sequence: u64,
}

impl DatabaseSnapshotRollbackJournal {
    pub(crate) fn record(
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
        });
        self.next_sequence = self.next_sequence.saturating_add(1);
    }

    pub(crate) fn list(&self) -> Vec<DatabaseSnapshotRollbackEntry> {
        self.entries.iter().rev().cloned().collect()
    }

    pub(crate) fn entry_for_snapshot(
        &self,
        snapshot_id: &str,
    ) -> Option<DatabaseSnapshotRollbackEntry> {
        self.entries
            .iter()
            .rev()
            .find(|entry| entry.snapshot_id == snapshot_id)
            .cloned()
    }

    pub(crate) fn restore_plan_for_turn(
        &self,
        turn_index: u32,
    ) -> Vec<DatabaseSnapshotRollbackEntry> {
        self.restore_plan_for_turn_since(turn_index, 0)
    }

    pub(crate) fn restore_plan_for_turn_since(
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
            if seen_databases.insert(entry.database.clone()) {
                plan.push(entry.clone());
            }
        }
        plan
    }

    pub(crate) fn checkpoint(&self) -> u64 {
        self.next_sequence
    }

    pub(crate) fn remove_snapshot(&mut self, snapshot_id: &str) -> bool {
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

fn with_journal_mut<T>(
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

pub(crate) fn journal_checkpoint(journal: &Mutex<DatabaseSnapshotRollbackJournal>) -> u64 {
    with_journal(journal, "database_snapshot_journal_checkpoint", |journal| {
        journal.checkpoint()
    })
}

pub(crate) fn record_rollback(
    journal: &Mutex<DatabaseSnapshotRollbackJournal>,
    snapshot_id: impl Into<String>,
    database: Option<String>,
    turn_index: u32,
) {
    with_journal_mut(journal, "record_database_snapshot_rollback", |journal| {
        journal.record(snapshot_id, database, turn_index)
    });
}

pub(crate) fn entries(
    journal: &Mutex<DatabaseSnapshotRollbackJournal>,
) -> Vec<DatabaseSnapshotRollbackEntry> {
    with_journal(journal, "database_snapshot_entries", |journal| {
        journal.list()
    })
}

pub(crate) fn entry_for_snapshot(
    journal: &Mutex<DatabaseSnapshotRollbackJournal>,
    snapshot_id: &str,
) -> Option<DatabaseSnapshotRollbackEntry> {
    with_journal(journal, "database_snapshot_entry_for_snapshot", |journal| {
        journal.entry_for_snapshot(snapshot_id)
    })
}

pub(crate) fn restore_plan_for_turn(
    journal: &Mutex<DatabaseSnapshotRollbackJournal>,
    turn_index: u32,
) -> Vec<DatabaseSnapshotRollbackEntry> {
    with_journal(
        journal,
        "database_snapshot_restore_plan_for_turn",
        |journal| journal.restore_plan_for_turn(turn_index),
    )
}

pub(crate) fn restore_plan_for_turn_since(
    journal: &Mutex<DatabaseSnapshotRollbackJournal>,
    turn_index: u32,
    checkpoint: u64,
) -> Vec<DatabaseSnapshotRollbackEntry> {
    with_journal(
        journal,
        "database_snapshot_restore_plan_for_turn_since",
        |journal| journal.restore_plan_for_turn_since(turn_index, checkpoint),
    )
}

pub(crate) fn remove_snapshot(
    journal: &Mutex<DatabaseSnapshotRollbackJournal>,
    snapshot_id: &str,
) -> bool {
    with_journal_mut(journal, "remove_database_snapshot_rollback", |journal| {
        journal.remove_snapshot(snapshot_id)
    })
}

pub(crate) fn rollback_entry_json(entry: &DatabaseSnapshotRollbackEntry) -> Value {
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
    Value::Object(value)
}

pub(crate) fn mo_create_snapshot_sql(name: &str, database: &str) -> String {
    format!("CREATE SNAPSHOT `{name}` FOR DATABASE `{database}`")
}

pub(crate) fn mo_restore_snapshot_sql(name: &str, account: &str, database: &str) -> String {
    format!("RESTORE ACCOUNT `{account}` DATABASE `{database}` FROM SNAPSHOT `{name}`")
}

pub(crate) fn mo_drop_snapshot_sql(name: &str) -> String {
    format!("DROP SNAPSHOT IF EXISTS `{name}`")
}

pub(crate) fn mo_query_requires_pre_state_snapshot(sql: &str, allow_destructive: bool) -> bool {
    // A multi-statement batch can hide a destructive statement behind a
    // benign-looking first statement (e.g. "SELECT 1; DROP TABLE t"). Snapshot
    // if ANY statement in the batch is mutating — a single destructive tail
    // statement must not bypass rollback safety.
    sql.split(';')
        .map(str::trim)
        .filter(|stmt| !stmt.is_empty())
        .any(|stmt| single_statement_requires_snapshot(stmt, allow_destructive))
}

fn single_statement_requires_snapshot(stmt: &str, allow_destructive: bool) -> bool {
    match stmt
        .split_whitespace()
        .next()
        .map(|keyword| keyword.trim_matches(|c| c == '('))
        .map(str::to_ascii_uppercase)
        .as_deref()
    {
        // Mutating statements — always snapshot for rollback safety.
        // LOAD is mutating (LOAD DATA [LOCAL] INFILE inserts rows into a
        // table); it must not be classified as a pure read.
        Some(
            "INSERT" | "UPDATE" | "REPLACE" | "CREATE" | "DROP" | "DELETE" | "TRUNCATE" | "ALTER"
            | "GRANT" | "REVOKE" | "LOAD",
        ) => true,
        // Pure reads / transaction control — never mutate state; skip the
        // snapshot cost regardless of allow_destructive (the flag gates
        // *execution* of writes, not snapshot capture on reads).
        Some(
            "SELECT" | "SHOW" | "EXPLAIN" | "DESC" | "DESCRIBE" | "USE" | "HELP" | "SOURCE"
            | "START_TRANSACTION" | "BEGIN" | "COMMIT" | "ROLLBACK" | "SET" | "PREPARE",
        ) => false,
        // Unknown keyword: snapshot only when destructive ops are permitted,
        // so unrecognized potentially-mutating statements are still covered.
        _ => allow_destructive,
    }
}

pub(crate) fn mo_pre_state_snapshot_name() -> String {
    format!("moq_{}", uuid::Uuid::now_v7().simple())
}

pub(crate) fn is_valid_snapshot_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

/// Cached account name — queried once via `SELECT current_account_name()`.
///
/// Only successful (non-error, non-empty) resolutions are cached. If MO is
/// unreachable at first call, the fallback "sys" is returned but NOT cached —
/// each subsequent call retries the query so snapshot ops recover once MO
/// comes back, rather than permanently targeting the wrong account.
fn mo_current_account() -> &'static str {
    use std::sync::Mutex;

    // Leaked on success so we can hand out a &'static str without lifetime
    // gymnastics; process-lifetime cache, exactly one allocation per process.
    static ACCOUNT: Mutex<Option<&'static str>> = Mutex::new(None);

    if let Ok(guard) = ACCOUNT.lock() {
        if let Some(cached) = *guard {
            return cached;
        }
    }

    let output = mo_execute_sql("SELECT current_account_name() AS name", None);
    let parsed = output
        .lines()
        .filter(|line| !line.starts_with('+') && !line.contains("name"))
        .find_map(|line| {
            let trimmed = line.trim().trim_matches('|').trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        });

    match parsed {
        Some(account) if !account.is_empty() && !is_mo_error(&output) => {
            let leaked: &'static str = Box::leak(account.into_boxed_str());
            if let Ok(mut guard) = ACCOUNT.lock() {
                if guard.is_none() {
                    *guard = Some(leaked);
                }
            }
            leaked
        }
        // Query failed or empty — do NOT cache the fallback; next call retries.
        _ => "sys",
    }
}

fn mo_database() -> &'static str {
    use std::sync::OnceLock;

    static DB: OnceLock<String> = OnceLock::new();
    DB.get_or_init(|| astra_core::resolve_database_name(&|key| std::env::var(key).ok()))
}

fn resolved_mo_database(database: Option<&str>) -> String {
    database
        .map(str::trim)
        .filter(|database| !database.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| mo_database().to_string())
}

fn is_mo_error(output: &str) -> bool {
    output.trim_start().starts_with("Error:")
}

fn mo_mysql_cmd(database: Option<&str>) -> Result<Command, String> {
    let settings = astra_core::MatrixOneSettings::from_env();
    Ok(settings.mysql_cmd(database))
}

fn mo_execute_sql(sql: &str, database: Option<&str>) -> String {
    let mut cmd = match mo_mysql_cmd(database) {
        Ok(command) => command,
        Err(error) => return error,
    };
    cmd.arg("-e").arg(sql);

    match cmd.output() {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !output.status.success() {
                let error = if stderr.is_empty() {
                    stdout.to_string()
                } else {
                    stderr.to_string()
                };
                format!("Error: {}", error.trim())
            } else if stdout.is_empty() {
                "OK (no results)".to_string()
            } else {
                stdout.to_string()
            }
        }
        Err(error) => format!("Error: failed to execute mysql client: {error}"),
    }
}

fn restore_database_snapshot_entry(entry: &DatabaseSnapshotRollbackEntry) -> Result<(), String> {
    let account = mo_current_account();
    let database = resolved_mo_database(entry.database.as_deref());
    let restore_output = mo_execute_sql(
        &mo_restore_snapshot_sql(&entry.snapshot_id, account, &database),
        None,
    );
    if is_mo_error(&restore_output) {
        return Err(restore_output);
    }

    let drop_output = mo_execute_sql(&mo_drop_snapshot_sql(&entry.snapshot_id), None);
    if is_mo_error(&drop_output) {
        Err(format!(
            "restored MatrixOne snapshot `{}` but failed to drop it afterwards.\n{}",
            entry.snapshot_id, drop_output
        ))
    } else {
        Ok(())
    }
}

pub(crate) fn execute_mo_query(
    journal: &Mutex<DatabaseSnapshotRollbackJournal>,
    args: &Value,
    turn_index: u32,
) -> astra_tools::ToolResult {
    let sql = match args.get("sql").and_then(Value::as_str) {
        Some(sql) if !sql.trim().is_empty() => sql.trim(),
        _ => return astra_tools::ToolResult::error("Error: Missing 'sql' parameter".into()),
    };

    let allow_destructive = args
        .get("allow_destructive")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !allow_destructive
        && let Some(kind) = astra_turn_core::safety_middleware::check_sql_safety(sql)
    {
        return astra_tools::ToolResult::error(format!(
            "Error: {kind} statements are blocked by default. Pass \"allow_destructive\": true to confirm execution."
        ));
    }

    let database = args.get("database").and_then(Value::as_str);
    let resolved_database = resolved_mo_database(database);
    let mut metadata = None;
    if mo_query_requires_pre_state_snapshot(sql, allow_destructive) {
        let snapshot_id = mo_pre_state_snapshot_name();
        let snapshot_output = mo_execute_sql(
            &mo_create_snapshot_sql(&snapshot_id, &resolved_database),
            None,
        );
        if is_mo_error(&snapshot_output) {
            return astra_tools::ToolResult::error(format!(
                "Error: failed to capture pre-state snapshot `{snapshot_id}` before executing query.\n{snapshot_output}"
            ));
        }
        record_rollback(
            journal,
            snapshot_id.clone(),
            Some(resolved_database.clone()),
            turn_index,
        );
        metadata = Some(serde_json::Map::from_iter([
            (
                "pre_state_snapshot_id".to_string(),
                Value::String(snapshot_id),
            ),
            (
                "pre_state_snapshot_database".to_string(),
                Value::String(resolved_database),
            ),
        ]));
    }

    let output = mo_execute_sql(sql, database);
    astra_tools::ToolResult {
        is_error: is_mo_error(&output),
        output,
        metadata,
        exit_semantics: None,
    }
}

pub(crate) fn rollback_database_snapshots(
    journal: &Mutex<DatabaseSnapshotRollbackJournal>,
    args: &Value,
    current_turn_index: u32,
) -> String {
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
            let entries = entries(journal)
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
            let journal_entry = entry_for_snapshot(journal, snapshot_id);
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
                });
            let entry = DatabaseSnapshotRollbackEntry {
                sequence: journal_entry.as_ref().map_or(0, |entry| entry.sequence),
                snapshot_id: snapshot_id.to_string(),
                database,
                turn_index: journal_entry
                    .as_ref()
                    .map_or(current_turn_index, |entry| entry.turn_index),
            };
            match restore_database_snapshot_entry(&entry) {
                Ok(()) => {
                    remove_snapshot(journal, snapshot_id);
                    let database = entry.database.clone();
                    json!({
                        "success": true,
                        "scope": "snapshot",
                        "snapshot_id": snapshot_id,
                        "database": database,
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
                .or_else(|| args.get("after_sequence"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let plan = if checkpoint > 0 {
                restore_plan_for_turn_since(journal, turn_index, checkpoint)
            } else {
                restore_plan_for_turn(journal, turn_index)
            };
            let mut restored = Vec::new();
            let mut failed = Vec::new();
            for entry in &plan {
                match restore_database_snapshot_entry(entry) {
                    Ok(()) => {
                        remove_snapshot(journal, &entry.snapshot_id);
                        restored.push(rollback_entry_json(entry));
                    }
                    Err(error) => {
                        let mut failed_entry = rollback_entry_json(entry)
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
                format!("No recorded MatrixOne snapshots found for turn {turn_index}")
            } else if failed.is_empty() {
                format!(
                    "Restored {} MatrixOne snapshot{} for turn {turn_index}",
                    restored.len(),
                    if restored.len() == 1 { "" } else { "s" }
                )
            } else {
                format!(
                    "Restored {} MatrixOne snapshot{} for turn {turn_index} with {} failure{}",
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
    fn restore_plan_keeps_earliest_snapshot_per_database_after_checkpoint() {
        let mut journal = DatabaseSnapshotRollbackJournal::default();
        journal.record("before_checkpoint", Some("main".to_string()), 7);
        let checkpoint = journal.checkpoint();
        journal.record("first_main", Some("main".to_string()), 7);
        journal.record("second_main", Some("main".to_string()), 7);
        journal.record("first_other", Some("other".to_string()), 7);
        journal.record("wrong_turn", Some("third".to_string()), 8);

        let plan = journal.restore_plan_for_turn_since(7, checkpoint);

        let snapshot_ids = plan
            .into_iter()
            .map(|entry| entry.snapshot_id)
            .collect::<Vec<_>>();
        assert_eq!(snapshot_ids, vec!["first_main", "first_other"]);
    }

    #[test]
    fn journal_helpers_record_list_and_render_entries() {
        let journal = Mutex::new(DatabaseSnapshotRollbackJournal::default());

        record_rollback(&journal, "snap_1", Some("main".to_string()), 4);
        assert_eq!(journal_checkpoint(&journal), 1);
        let listed = entries(&journal);
        assert_eq!(listed.len(), 1);
        assert_eq!(
            entry_for_snapshot(&journal, "snap_1")
                .as_ref()
                .map(|entry| entry.database.as_deref()),
            Some(Some("main"))
        );

        let rendered = rollback_entry_json(&listed[0]);
        assert_eq!(rendered["snapshot_id"].as_str(), Some("snap_1"));
        assert_eq!(rendered["database"].as_str(), Some("main"));
        assert_eq!(rendered["turn_index"].as_u64(), Some(4));
        assert!(remove_snapshot(&journal, "snap_1"));
        assert!(entries(&journal).is_empty());
    }

    #[test]
    fn snapshot_name_validation_rejects_empty_long_or_unsafe_names() {
        assert!(is_valid_snapshot_name("moq_abc-123"));
        assert!(!is_valid_snapshot_name(""));
        assert!(!is_valid_snapshot_name(&"a".repeat(65)));
        assert!(!is_valid_snapshot_name("moq_abc;DROP"));
        assert!(!is_valid_snapshot_name("moq/abc"));
    }

    #[test]
    fn query_snapshot_policy_covers_destructive_and_explicit_confirmed_queries() {
        assert!(mo_query_requires_pre_state_snapshot(
            "UPDATE users SET enabled = false",
            false
        ));
        assert!(mo_query_requires_pre_state_snapshot(
            "DROP TABLE users",
            false
        ));
        // Pure reads never snapshot, even when destructive ops are allowed —
        // allow_destructive gates *execution*, not read-side snapshot capture.
        assert!(!mo_query_requires_pre_state_snapshot("SELECT 1", true));
        assert!(!mo_query_requires_pre_state_snapshot("SELECT 1", false));
        assert!(!mo_query_requires_pre_state_snapshot("SHOW TABLES", true));
        assert!(!mo_query_requires_pre_state_snapshot(
            "EXPLAIN SELECT 1",
            true
        ));
        // Unknown keyword still covered when destructive ops are permitted.
        assert!(mo_query_requires_pre_state_snapshot(
            "MERGE INTO t USING ...",
            true
        ));
    }

    #[test]
    fn load_classified_as_mutating() {
        // LOAD DATA [LOCAL] INFILE inserts rows into a table — must snapshot.
        assert!(mo_query_requires_pre_state_snapshot(
            "LOAD DATA INFILE '/tmp/a.csv' INTO TABLE t",
            false
        ));
        assert!(mo_query_requires_pre_state_snapshot(
            "LOAD DATA LOCAL INFILE '/tmp/a.csv' INTO TABLE t",
            true
        ));
    }

    #[test]
    fn multi_statement_batch_snapshots_when_any_statement_mutates() {
        // A destructive statement hidden behind a benign one must not bypass
        // rollback safety — the first keyword alone is insufficient.
        assert!(mo_query_requires_pre_state_snapshot(
            "SELECT 1; DROP TABLE users",
            false
        ));
        assert!(mo_query_requires_pre_state_snapshot(
            "SELECT 1; DELETE FROM users",
            false
        ));
        assert!(mo_query_requires_pre_state_snapshot(
            "SHOW TABLES; UPDATE users SET enabled = false",
            true
        ));
        // Trailing semicolon / whitespace tolerated by a pure read.
        assert!(!mo_query_requires_pre_state_snapshot("SELECT 1; ", false));
        // Empty batch is non-mutating.
        assert!(!mo_query_requires_pre_state_snapshot("", false));
        assert!(!mo_query_requires_pre_state_snapshot(";", false));
        // Read-only batch of multiple reads stays non-mutating.
        assert!(!mo_query_requires_pre_state_snapshot(
            "SELECT 1; SELECT 2; SHOW TABLES",
            true
        ));
    }

    #[test]
    fn snapshot_sql_is_deterministic_and_uses_explicit_context() {
        assert_eq!(
            mo_create_snapshot_sql("snap_1", "astra"),
            "CREATE SNAPSHOT `snap_1` FOR DATABASE `astra`"
        );
        assert_eq!(
            mo_restore_snapshot_sql("snap_1", "sys", "astra"),
            "RESTORE ACCOUNT `sys` DATABASE `astra` FROM SNAPSHOT `snap_1`"
        );
        assert_eq!(
            mo_drop_snapshot_sql("snap_1"),
            "DROP SNAPSHOT IF EXISTS `snap_1`"
        );
    }
}
