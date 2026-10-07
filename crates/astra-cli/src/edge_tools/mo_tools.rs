//! MatrixOne SQL query and rollback support.
//!
//! Connection details are read from environment variables:
//!   MATRIXONE_HOST (default: localhost)
//!   MATRIXONE_PORT (default: 6001)
//!   MATRIXONE_USER (default: root)
//!   MATRIXONE_PASSWORD (default: dev-only; set for production!)
//!   ASTRA_DATABASE (default: astra_runtime)
//!   ASTRA_DATABASE_PREFIX (optional; effective DB = prefix + ASTRA_DATABASE)
//!
//! Uses the `mysql` CLI client (MySQL protocol compatible), same pattern as
//! git tools — shell out to native CLI for zero Rust-side connection overhead.

pub(crate) use astra_turn_core::database_snapshots::{
    DatabaseSnapshotRollbackEntry, DatabaseSnapshotRollbackJournal,
};
use astra_turn_core::safety_middleware::sql_requires_pre_state_snapshot as mo_query_requires_pre_state_snapshot;
use std::process::Command;

use super::{ToolExecutionOutcome, ToolExecutor};
use crate::tool_safety_guard::check_sql_safety;
use serde_json::Value;
use uuid::Uuid;

// ─── MatrixOne connection helper ────────────────────────────────────────────

/// Cached account name — queried once via `SELECT current_account_name()`.
///
/// Only successful (non-empty) resolutions are cached. If MatrixOne is
/// unreachable at first call, the fallback "sys" is NOT cached — each
/// subsequent call retries the query so snapshot ops recover once MO comes
/// back, rather than permanently targeting the wrong account.
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

    let out = mo_execute_sql("SELECT current_account_name() AS name", None).unwrap_or_else(|e| e);
    // Parse the value from mysql --table output.
    let parsed = out
        .lines()
        .filter(|l| !l.starts_with('+') && !l.contains("name"))
        .find_map(|l| {
            let trimmed = l.trim().trim_matches('|').trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        });

    match parsed {
        Some(account) if !account.is_empty() && !is_mo_error(&out) => {
            // Cache only verified successes.
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
    DB.get_or_init(|| astra_core::resolve_database_name(&|k| std::env::var(k).ok()))
}

fn resolved_mo_database(database: Option<&str>) -> String {
    database
        .map(str::trim)
        .filter(|database| !database.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| mo_database().to_string())
}

fn mo_create_snapshot_sql(name: &str, database: Option<&str>) -> String {
    astra_services::snapshot_sql::create_snapshot_for_db_sql(name, &resolved_mo_database(database))
}

fn mo_restore_snapshot_sql(name: &str, database: Option<&str>) -> String {
    astra_services::snapshot_sql::restore_database_from_snapshot_sql(
        name,
        mo_current_account(),
        &resolved_mo_database(database),
    )
}

fn mo_pre_state_snapshot_name() -> String {
    format!("moq_{}", Uuid::now_v7().simple())
}

fn is_mo_error(output: &str) -> bool {
    output.trim_start().starts_with("Error:")
}

fn mo_mysql_cmd(database: Option<&str>) -> Result<Command, String> {
    let settings = astra_core::MatrixOneSettings::from_env_strict()
        .map_err(|e| format!("Error: {e}. Set MATRIXONE_PASSWORD before using MatrixOne tools."))?;
    Ok(settings.mysql_cmd(database))
}

/// Execute a SQL statement against MatrixOne via the mysql CLI.
///
/// Returns `Err` with an explanatory message when `MATRIXONE_PASSWORD` is unset.
/// Tool callers that need a `String` output should use `.unwrap_or_else(|e| e)`.
fn mo_execute_sql(sql: &str, database: Option<&str>) -> Result<String, String> {
    let mut cmd = mo_mysql_cmd(database)?;
    cmd.arg("-e").arg(sql);

    match cmd.output() {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !out.status.success() {
                let err = if stderr.is_empty() {
                    stdout.to_string()
                } else {
                    stderr.to_string()
                };
                let trimmed = err.trim();
                let mut msg = format!("Error: {trimmed}");
                // Schema enrichment: on column/table not found, auto-append schema info
                // so the agent can self-correct without an extra round-trip.
                let lower = trimmed.to_lowercase();
                if let Some(hint) = schema_hint_for_error(&lower, sql, database) {
                    msg.push_str("\n--- auto-fetched schema ---\n");
                    msg.push_str(&hint);
                }
                Ok(msg)
            } else if stdout.is_empty() {
                Ok("OK (no results)".to_string())
            } else {
                let result = stdout.to_string();
                if result.len() > 20_000 {
                    let rows_shown = result[..20_000].matches('\n').count();
                    let total_rows = result.matches('\n').count();
                    let mut t = result[..20_000].to_string();
                    t.push_str(&format!(
                        "\n[truncated at 20KB: showing ~{} of {} rows. Use LIMIT to narrow results.]",
                        rows_shown, total_rows
                    ));
                    Ok(t)
                } else {
                    Ok(result)
                }
            }
        }
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                Ok("Error: mysql client not found. Install mysql-client or mariadb-client to use MatrixOne tools.\nHint: apt install mariadb-client OR brew install mysql-client".to_string())
            } else {
                Ok(format!("Error: failed to execute mysql: {e}"))
            }
        }
    }
}

/// Extract a table name from SQL (best-effort, handles common patterns).
fn extract_table_from_sql(sql: &str) -> Option<String> {
    let upper = sql.to_uppercase();
    // FROM table, FROM db.table, INTO table, UPDATE table, DESCRIBE table
    for kw in &["FROM ", "INTO ", "UPDATE ", "DESCRIBE ", "TABLE "] {
        if let Some(pos) = upper.find(kw) {
            let rest = &sql[pos + kw.len()..];
            let token: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '.' || *c == '`')
                .collect();
            let clean = token.trim_matches('`');
            if !clean.is_empty() {
                return Some(clean.to_string());
            }
        }
    }
    None
}

/// On column/table-not-found errors, auto-fetch schema so the agent can self-correct.
fn schema_hint_for_error(lower_err: &str, sql: &str, database: Option<&str>) -> Option<String> {
    if lower_err.contains("column") && lower_err.contains("does not exist") {
        // Column not found → DESCRIBE the table
        if let Some(table) = extract_table_from_sql(sql) {
            return mo_execute_sql(&format!("DESCRIBE {table}"), database)
                .ok()
                .filter(|desc| !desc.starts_with("Error:"));
        }
    } else if lower_err.contains("table") && lower_err.contains("does not exist") {
        // Table not found → SHOW TABLES in the database
        let db = database
            .map(String::from)
            .unwrap_or_else(|| astra_core::resolve_database_name(&|k| std::env::var(k).ok()));
        if !db.is_empty() {
            return mo_execute_sql(&format!("SHOW TABLES IN `{db}`"), None)
                .ok()
                .filter(|t| !t.starts_with("Error:"));
        }
    }
    None
}

// ─── Tool implementations ───────────────────────────────────────────────────

impl ToolExecutor {
    #[cfg(test)]
    fn record_database_snapshot_rollback(
        &self,
        snapshot_id: impl Into<String>,
        database: Option<String>,
    ) {
        let turn_index = self
            .journal_turn_index
            .load(std::sync::atomic::Ordering::Relaxed);
        match self.database_snapshot_journal.lock() {
            Ok(mut journal) => journal.record(snapshot_id, database, turn_index),
            Err(poisoned) => poisoned
                .into_inner()
                .record(snapshot_id, database, turn_index),
        }
    }

    pub(crate) fn database_snapshot_journal_checkpoint(&self) -> u64 {
        match self.database_snapshot_journal.lock() {
            Ok(journal) => journal.checkpoint(),
            Err(poisoned) => poisoned.into_inner().checkpoint(),
        }
    }

    fn execute_snapshot_rollback(
        &self,
        entry: &DatabaseSnapshotRollbackEntry,
        operation: astra_turn_core::database_snapshots::SnapshotRollbackOperation,
    ) -> Result<(), String> {
        use astra_turn_core::database_snapshots::SnapshotRollbackOperation;
        let sql = match operation {
            SnapshotRollbackOperation::Restore => {
                mo_restore_snapshot_sql(&entry.snapshot_id, entry.database.as_deref())
            }
            SnapshotRollbackOperation::Drop => {
                astra_services::snapshot_sql::drop_snapshot_sql(&entry.snapshot_id)
            }
        };
        let output = mo_execute_sql(&sql, None)?;
        if is_mo_error(&output) {
            Err(output)
        } else {
            Ok(())
        }
    }

    /// `mo_query`: Execute a SQL query against MatrixOne.
    /// Foundation tool for all database operations.
    /// Blocks destructive DDL/DML (DROP, DELETE, TRUNCATE, ALTER, GRANT, REVOKE)
    /// unless the caller explicitly passes `"allow_destructive": true`.
    /// Mutating queries capture a pre-state snapshot before execution so the
    /// runtime can surface a concrete rollback hint on staged mutations.
    pub(crate) fn mo_query(&self, args: &Value) -> String {
        self.mo_query_with_metadata(args, None).output
    }

    pub(crate) fn mo_query_with_metadata(
        &self,
        args: &Value,
        cancel_token: Option<&tokio_util::sync::CancellationToken>,
    ) -> ToolExecutionOutcome {
        let sql = match args.get("sql").and_then(Value::as_str) {
            Some(s) if !s.trim().is_empty() => s,
            _ => {
                return ToolExecutionOutcome::error(
                    "Error: missing or empty 'sql' parameter".to_string(),
                );
            }
        };

        // Safety gate: block destructive operations unless explicitly allowed
        let allow_destructive = args
            .get("allow_destructive")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !allow_destructive && let Some(kind) = check_sql_safety(sql) {
            return ToolExecutionOutcome::error(format!(
                "Error: {kind} statements are blocked by default. \
                     Pass \"allow_destructive\": true to confirm execution."
            ));
        }

        if !mo_query_requires_pre_state_snapshot(sql, allow_destructive) {
            let output = mo_execute_sql(sql, args.get("database").and_then(Value::as_str))
                .unwrap_or_else(|error| error);
            return ToolExecutionOutcome {
                is_error: is_mo_error(&output),
                output,
                tool_result_fields: None,
            };
        }
        astra_turn_core::database_snapshots::with_journal_mut(
            &self.database_snapshot_journal,
            "execute_mo_query",
            |journal| {
                if cancel_token.is_some_and(tokio_util::sync::CancellationToken::is_cancelled) {
                    return super::cancelled_tool_execution_outcome("mo_query", false);
                }
                let database = args.get("database").and_then(Value::as_str);
                let resolved_database = resolved_mo_database(database);
                let mut tool_result_fields = None;
                if mo_query_requires_pre_state_snapshot(sql, allow_destructive) {
                    let snapshot_id = mo_pre_state_snapshot_name();
                    let snapshot_output =
                        mo_execute_sql(&mo_create_snapshot_sql(&snapshot_id, database), None)
                            .unwrap_or_else(|e| e);
                    if is_mo_error(&snapshot_output) {
                        return ToolExecutionOutcome::error(format!(
                            "Error: failed to capture pre-state snapshot `{snapshot_id}` before executing query.\n{snapshot_output}"
                        ));
                    }
                    journal.record(
                        snapshot_id.clone(),
                        Some(resolved_database.clone()),
                        self.journal_turn_index
                            .load(std::sync::atomic::Ordering::Relaxed),
                    );
                    tool_result_fields = Some(serde_json::Map::from_iter([
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

                let output = mo_execute_sql(sql, database).unwrap_or_else(|e| e);
                let is_error = is_mo_error(&output);
                ToolExecutionOutcome {
                    output,
                    tool_result_fields,
                    is_error,
                }
            },
        )
    }

    pub(crate) fn rollback_database_snapshots(&self, args: &Value) -> String {
        astra_turn_core::database_snapshots::rollback_database_snapshots(
            &self.database_snapshot_journal,
            args,
            self.journal_turn_index
                .load(std::sync::atomic::Ordering::Relaxed),
            &resolved_mo_database(None),
            |entry, operation| self.execute_snapshot_rollback(entry, operation),
        )
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::super::ToolExecutor;
    use super::{
        DatabaseSnapshotRollbackJournal, extract_table_from_sql, mo_create_snapshot_sql,
        mo_execute_sql, mo_mysql_cmd, mo_pre_state_snapshot_name,
        mo_query_requires_pre_state_snapshot,
    };
    use crate::tool_safety_guard::check_sql_safety;
    use astra_turn_core::database_snapshots::is_valid_snapshot_name;
    use serde_json::Value;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    fn env_guard() -> MutexGuard<'static, ()> {
        static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("env lock poisoned")
    }

    // ── Validation ──

    #[test]
    fn valid_snapshot_names() {
        assert!(is_valid_snapshot_name("my_snapshot"));
        assert!(is_valid_snapshot_name("snapshot-123"));
        assert!(is_valid_snapshot_name("br_main"));
        assert!(is_valid_snapshot_name("abc"));
    }

    #[test]
    fn invalid_snapshot_names() {
        assert!(!is_valid_snapshot_name(""));
        assert!(!is_valid_snapshot_name("snap shot")); // space
        assert!(!is_valid_snapshot_name("snap;shot")); // semicolon
        assert!(!is_valid_snapshot_name("snap'shot")); // quote
        assert!(!is_valid_snapshot_name(&"a".repeat(65))); // too long
    }

    #[test]
    fn mo_query_snapshot_guard_only_triggers_for_mutations() {
        assert!(!mo_query_requires_pre_state_snapshot(
            "SELECT * FROM metrics",
            false
        ));
        assert!(!mo_query_requires_pre_state_snapshot("SHOW TABLES", false));
        assert!(mo_query_requires_pre_state_snapshot(
            "UPDATE metrics SET value = 1",
            false
        ));
        assert!(mo_query_requires_pre_state_snapshot(
            "DELETE FROM metrics",
            true
        ));
        // Pure reads never snapshot, even with allow_destructive.
        assert!(!mo_query_requires_pre_state_snapshot("SELECT 1", true));
        assert!(!mo_query_requires_pre_state_snapshot(
            "EXPLAIN SELECT 1",
            true
        ));
        // Unknown keyword covered when destructive ops are permitted.
        assert!(mo_query_requires_pre_state_snapshot("MERGE INTO t", true));
        assert!(!mo_query_requires_pre_state_snapshot("MERGE INTO t", false));
    }

    #[test]
    fn mo_pre_state_snapshot_name_is_valid() {
        let name = mo_pre_state_snapshot_name();
        assert!(name.starts_with("moq_"));
        assert!(is_valid_snapshot_name(&name));
    }

    #[test]
    fn mo_create_snapshot_sql_honors_database_override() {
        assert_eq!(
            mo_create_snapshot_sql("snap_1", Some("analytics")),
            "CREATE SNAPSHOT `snap_1` FOR DATABASE `analytics`"
        );
    }

    #[test]
    fn database_snapshot_journal_turn_plan_uses_earliest_snapshot_per_database() {
        let mut journal = DatabaseSnapshotRollbackJournal::default();
        journal.record("snap_analytics_1", Some("analytics".into()), 7);
        journal.record("snap_analytics_2", Some("analytics".into()), 7);
        journal.record("snap_reporting_1", Some("reporting".into()), 7);
        journal.record("snap_other_turn", Some("analytics".into()), 8);

        let plan = journal.restore_plan_for_turn(7);
        let snapshot_ids: Vec<_> = plan
            .iter()
            .map(|entry| entry.snapshot_id.as_str())
            .collect();
        assert_eq!(snapshot_ids, vec!["snap_analytics_1", "snap_reporting_1"]);
        assert_eq!(plan[0].database.as_deref(), Some("analytics"));
        assert_eq!(plan[1].database.as_deref(), Some("reporting"));
    }

    #[test]
    fn database_snapshot_journal_turn_plan_since_checkpoint_uses_subset() {
        let mut journal = DatabaseSnapshotRollbackJournal::default();
        journal.record("snap_analytics_1", Some("analytics".into()), 7);
        let checkpoint = journal.checkpoint();
        journal.record("snap_analytics_2", Some("analytics".into()), 7);
        journal.record("snap_reporting_1", Some("reporting".into()), 7);

        let plan = journal.restore_plan_for_turn_since(7, checkpoint);
        let snapshot_ids: Vec<_> = plan
            .iter()
            .map(|entry| entry.snapshot_id.as_str())
            .collect();
        assert_eq!(snapshot_ids, vec!["snap_analytics_2", "snap_reporting_1"]);
    }

    #[test]
    fn rollback_database_snapshots_list_reports_recorded_entries() {
        let executor = ToolExecutor::new(std::env::temp_dir());
        executor
            .journal_turn_index
            .store(3, std::sync::atomic::Ordering::Relaxed);
        executor.record_database_snapshot_rollback("snap_1", Some("analytics".into()));
        executor
            .journal_turn_index
            .store(4, std::sync::atomic::Ordering::Relaxed);
        executor.record_database_snapshot_rollback("snap_2", Some("reporting".into()));

        let result = executor.rollback_database_snapshots(&serde_json::json!({"scope": "list"}));
        let value: Value = serde_json::from_str(&result).expect("rollback_database_snapshots json");
        assert_eq!(value["success"], true);
        assert_eq!(value["total_entries"], 2);
        assert_eq!(value["entries"][0]["snapshot_id"], "snap_2");
        assert_eq!(value["entries"][0]["database"], "reporting");
        assert_eq!(value["entries"][1]["snapshot_id"], "snap_1");
        assert_eq!(value["entries"][1]["turn_index"], 3);
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_entry_captures_batch_and_retries_only_cleanup() {
        use std::os::unix::fs::PermissionsExt;
        let _guard = env_guard();
        struct RestoreEnv(Vec<(&'static str, Option<std::ffi::OsString>)>);
        impl Drop for RestoreEnv {
            fn drop(&mut self) {
                for (key, value) in &self.0 {
                    unsafe {
                        match value {
                            Some(value) => std::env::set_var(key, value),
                            None => std::env::remove_var(key),
                        }
                    }
                }
            }
        }
        let _restore = RestoreEnv(
            ["PATH", "MATRIXONE_PASSWORD"]
                .into_iter()
                .map(|key| (key, std::env::var_os(key)))
                .collect(),
        );
        let fixture = tempfile::tempdir().unwrap();
        let mysql = fixture.path().join("mysql");
        std::fs::write(
            &mysql,
            include_str!("../../../astra-turn-core/tests/fixtures/mysql_snapshot/mysql.sh"),
        )
        .unwrap();
        std::fs::set_permissions(&mysql, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = std::env::var_os("PATH").unwrap_or_default();
        let path = std::env::join_paths(
            std::iter::once(fixture.path().to_path_buf()).chain(std::env::split_paths(&path)),
        )
        .unwrap();
        unsafe {
            std::env::set_var("PATH", path);
            std::env::set_var("MATRIXONE_PASSWORD", "offline-test-password");
        }
        let executor = ToolExecutor::new(fixture.path().to_path_buf());
        executor
            .journal_turn_index
            .store(7, std::sync::atomic::Ordering::Relaxed);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let query = "SELECT 1; UPDATE metrics SET value = 1";
        let result = runtime.block_on(executor.execute_with_metadata(
            "mo_query",
            &serde_json::json!({"sql": query, "database": "test`db"}),
        ));
        assert!(!result.is_error, "{}", result.output);
        assert_eq!(
            result.tool_result_fields.unwrap()["pre_state_snapshot_database"],
            "test`db"
        );
        std::fs::write(fixture.path().join("fail_drop"), "").unwrap();
        let failed: Value = serde_json::from_str(&runtime.block_on(executor.execute(
            "rollback_database_snapshots",
            &serde_json::json!({"scope": "current_turn"}),
        )))
        .unwrap();
        assert_eq!(failed["success"], false);
        assert_eq!(
            executor
                .database_snapshot_journal
                .lock()
                .unwrap()
                .list()
                .len(),
            1
        );
        std::fs::remove_file(fixture.path().join("fail_drop")).unwrap();
        let result: Value = serde_json::from_str(&runtime.block_on(executor.execute(
            "rollback_database_snapshots",
            &serde_json::json!({"scope": "current_turn"}),
        )))
        .unwrap();
        assert_eq!(result["success"], true);
        assert!(
            executor
                .database_snapshot_journal
                .lock()
                .unwrap()
                .list()
                .is_empty()
        );
        let sql = std::fs::read_to_string(fixture.path().join("sql.log")).unwrap();
        let commands: Vec<_> = sql.lines().collect();
        assert!(
            commands
                .iter()
                .position(|sql| sql.starts_with("CREATE SNAPSHOT") && sql.ends_with("`test``db`"))
                .unwrap()
                < commands.iter().position(|sql| *sql == query).unwrap()
        );
        assert_eq!(
            commands
                .iter()
                .filter(|sql| sql.starts_with("RESTORE ACCOUNT"))
                .count(),
            1
        );
        assert_eq!(
            commands
                .iter()
                .filter(|sql| sql.starts_with("DROP SNAPSHOT"))
                .count(),
            2
        );
        std::fs::write(fixture.path().join("fail_capture"), "").unwrap();
        let load = "LOAD DATA INFILE 'rows.csv' INTO TABLE metrics";
        assert!(
            runtime
                .block_on(
                    executor.execute_with_metadata("mo_query", &serde_json::json!({"sql": load}))
                )
                .is_error
        );
        assert!(
            executor
                .database_snapshot_journal
                .lock()
                .unwrap()
                .list()
                .is_empty()
        );
        assert!(
            !std::fs::read_to_string(fixture.path().join("sql.log"))
                .unwrap()
                .lines()
                .any(|sql| sql == load)
        );

        let before_cancel = std::fs::read_to_string(fixture.path().join("sql.log")).unwrap();
        let token = tokio_util::sync::CancellationToken::new();
        let journal_guard = executor.database_snapshot_journal.lock().unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let waiter = scope.spawn(|| {
                started_tx.send(()).unwrap();
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                runtime.block_on(executor.execute_with_metadata_cancelable(
                    "mo_query",
                    &serde_json::json!({"sql": query}),
                    Some(&token),
                ))
            });
            started_rx.recv().unwrap();
            token.cancel();
            drop(journal_guard);
            assert!(waiter.join().unwrap().is_error);
        });
        assert_eq!(
            std::fs::read_to_string(fixture.path().join("sql.log")).unwrap(),
            before_cancel
        );
        assert!(
            executor
                .database_snapshot_journal
                .lock()
                .unwrap()
                .list()
                .is_empty()
        );
        // Recheck cancellation in the synchronous handler after public preflight.
        assert!(
            executor
                .mo_query_with_metadata(&serde_json::json!({"sql": query}), Some(&token))
                .is_error
        );
        assert_eq!(
            std::fs::read_to_string(fixture.path().join("sql.log")).unwrap(),
            before_cancel
        );
    }

    // ── Parameter validation ──

    #[test]
    fn mo_query_missing_sql() {
        let executor = ToolExecutor::new(std::env::temp_dir());
        let result = executor.mo_query(&serde_json::json!({}));
        assert!(result.contains("Error"), "should error: {result}");
    }

    #[test]
    fn mo_query_empty_sql() {
        let executor = ToolExecutor::new(std::env::temp_dir());
        let result = executor.mo_query(&serde_json::json!({"sql": ""}));
        assert!(result.contains("Error"), "should error on empty: {result}");
    }

    #[test]
    fn rollback_database_snapshots_snapshot_scope_requires_snapshot_id() {
        let executor = ToolExecutor::new(std::env::temp_dir());
        let result =
            executor.rollback_database_snapshots(&serde_json::json!({"scope": "snapshot"}));
        let value: Value = serde_json::from_str(&result).expect("rollback_database_snapshots json");
        assert_eq!(value["success"], false);
        assert_eq!(value["scope"], "snapshot");
        assert!(
            value["error"]
                .as_str()
                .unwrap_or_default()
                .contains("missing 'snapshot_id'")
        );
    }

    // ── mo_execute_sql tests (mysql client may not be available) ──

    #[test]
    fn mo_execute_sql_returns_graceful_error_if_no_mysql() {
        let _guard = env_guard();
        unsafe {
            std::env::set_var("MATRIXONE_HOST", "nonexistent-host-12345");
            std::env::set_var("MATRIXONE_PORT", "6001");
            std::env::set_var("MATRIXONE_PASSWORD", "test-pw");
        }
        let result = mo_execute_sql("SELECT 1", None).expect("password set, should be Ok");
        assert!(
            result.contains("Error") || result.contains("error") || result.contains("not found"),
            "should handle missing mysql gracefully: {result}"
        );
        unsafe {
            std::env::remove_var("MATRIXONE_HOST");
            std::env::remove_var("MATRIXONE_PORT");
            std::env::remove_var("MATRIXONE_PASSWORD");
        }
    }

    #[test]
    fn mo_execute_sql_requires_matrixone_password() {
        let _guard = env_guard();
        unsafe {
            std::env::remove_var("MATRIXONE_PASSWORD");
        }
        let result = mo_execute_sql("SELECT 1", None);
        assert!(
            result.is_err(),
            "should error when MATRIXONE_PASSWORD is unset"
        );
        let msg = result.unwrap_err();
        assert!(
            msg.contains("MATRIXONE_PASSWORD"),
            "error should mention the missing var: {msg}"
        );
        assert!(
            !msg.contains("111"),
            "should not fall back to hardcoded password: {msg}"
        );
    }

    #[test]
    fn mo_mysql_cmd_requires_matrixone_password() {
        let _guard = env_guard();
        unsafe {
            std::env::remove_var("MATRIXONE_PASSWORD");
        }
        let result = mo_mysql_cmd(None);
        assert!(result.is_err());
    }

    #[test]
    fn mo_mysql_cmd_uses_env_vars() {
        let _guard = env_guard();
        unsafe {
            std::env::set_var("MATRIXONE_HOST", "testhost");
            std::env::set_var("MATRIXONE_PORT", "7001");
            std::env::set_var("MATRIXONE_PASSWORD", "test-pw");
        }
        let cmd = mo_mysql_cmd(Some("testdb")).expect("password set, should be Ok");
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(
            args.iter().any(|a| a.contains("testhost")),
            "should use MATRIXONE_HOST"
        );
        assert!(
            args.iter().any(|a| a.contains("7001")),
            "should use MATRIXONE_PORT"
        );
        assert!(
            args.iter().any(|a| a == "testdb"),
            "should use specified database"
        );
        // Password should NOT appear in args (security: hidden from ps)
        assert!(
            !args.iter().any(|a| a.contains("-p")),
            "password should not be in CLI args: {args:?}"
        );
        // Password should be in environment instead
        let envs: Vec<_> = cmd.get_envs().collect();
        assert!(
            envs.iter().any(|(k, _)| *k == "MYSQL_PWD"),
            "password should be in MYSQL_PWD env var"
        );
        unsafe {
            std::env::remove_var("MATRIXONE_HOST");
            std::env::remove_var("MATRIXONE_PORT");
            std::env::remove_var("MATRIXONE_PASSWORD");
        }
    }

    #[test]
    fn mo_mysql_cmd_default_database() {
        let _guard = env_guard();
        unsafe {
            std::env::remove_var("ASTRA_DATABASE");
            std::env::remove_var("ASTRA_DATABASE_PREFIX");
            std::env::set_var("MATRIXONE_PASSWORD", "test-pw");
        }
        let cmd = mo_mysql_cmd(None).expect("password set, should be Ok");
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(
            args.iter().any(|a| a == "astra_runtime"),
            "should default to astra_runtime: {:?}",
            args
        );
        unsafe {
            std::env::remove_var("MATRIXONE_PASSWORD");
        }
    }

    #[test]
    fn mo_mysql_cmd_default_database_applies_prefix() {
        let _guard = env_guard();
        unsafe {
            std::env::remove_var("ASTRA_DATABASE_PREFIX");
            std::env::set_var("ASTRA_DATABASE_PREFIX", "it_");
            std::env::set_var("ASTRA_DATABASE", "astra_runtime");
            std::env::set_var("MATRIXONE_PASSWORD", "test-pw");
        }
        let cmd = mo_mysql_cmd(None).expect("password set, should be Ok");
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(
            args.iter().any(|a| a == "it_astra_runtime"),
            "should concatenate prefix + base: {:?}",
            args
        );
        unsafe {
            std::env::remove_var("ASTRA_DATABASE_PREFIX");
            std::env::remove_var("ASTRA_DATABASE");
            std::env::remove_var("MATRIXONE_PASSWORD");
        }
    }

    // ── SQL safety validation ──

    #[test]
    fn sql_safety_blocks_destructive_operations() {
        assert_eq!(check_sql_safety("DROP TABLE users"), Some("DROP"));
        assert_eq!(check_sql_safety("delete from users"), Some("DELETE"));
        assert_eq!(check_sql_safety("TRUNCATE TABLE logs"), Some("TRUNCATE"));
        assert_eq!(
            check_sql_safety("ALTER TABLE users ADD col INT"),
            Some("ALTER")
        );
        assert_eq!(
            check_sql_safety("  GRANT ALL ON *.* TO root"),
            Some("GRANT")
        );
        assert_eq!(
            check_sql_safety("REVOKE INSERT ON db.t FROM u"),
            Some("REVOKE")
        );
    }

    #[test]
    fn sql_safety_allows_safe_operations() {
        assert_eq!(check_sql_safety("SELECT * FROM users"), None);
        assert_eq!(check_sql_safety("SHOW TABLES"), None);
        assert_eq!(check_sql_safety("EXPLAIN SELECT 1"), None);
        assert_eq!(check_sql_safety("INSERT INTO logs VALUES (1)"), None);
        assert_eq!(check_sql_safety("UPDATE users SET name='x'"), None);
        assert_eq!(check_sql_safety("CREATE TABLE t (id INT)"), None);
    }

    #[test]
    fn sql_safety_ignores_leading_comments() {
        assert_eq!(
            check_sql_safety("-- comment\nDROP TABLE users"),
            Some("DROP")
        );
        assert_eq!(check_sql_safety("-- safe comment\nSELECT 1"), None);
    }

    #[test]
    fn mo_query_blocks_destructive_by_default() {
        let executor = ToolExecutor::new(std::env::temp_dir());
        let result = executor.mo_query(&serde_json::json!({"sql": "DROP TABLE users"}));
        assert!(result.contains("blocked"), "should block DROP: {result}");
        assert!(
            result.contains("allow_destructive"),
            "should mention opt-in: {result}"
        );
    }

    #[test]
    fn mo_query_allows_destructive_with_opt_in() {
        let _guard = env_guard();
        unsafe {
            std::env::set_var("MATRIXONE_HOST", "127.0.0.1");
            std::env::set_var("MATRIXONE_PORT", "1");
        }
        let executor = ToolExecutor::new(std::env::temp_dir());
        // This will fail at the mysql connection level, but NOT at the safety check
        let result = executor.mo_query(
            &serde_json::json!({"sql": "DROP TABLE IF EXISTS _test_table", "allow_destructive": true}),
        );
        // Should NOT contain "blocked" — it should attempt execution
        assert!(
            !result.contains("blocked"),
            "should not block with opt-in: {result}"
        );
        unsafe {
            std::env::remove_var("MATRIXONE_HOST");
            std::env::remove_var("MATRIXONE_PORT");
        }
    }

    // ── SQL safety bypass prevention ──

    #[test]
    fn sql_safety_blocks_block_comments() {
        assert_eq!(
            check_sql_safety("/* harmless */ DROP TABLE users"),
            Some("DROP")
        );
        assert_eq!(check_sql_safety("/**/ DELETE FROM users"), Some("DELETE"));
    }

    #[test]
    fn sql_safety_blocks_multi_statement() {
        assert_eq!(check_sql_safety("SELECT 1; DROP TABLE users"), Some("DROP"));
        assert_eq!(
            check_sql_safety("SHOW TABLES; DELETE FROM logs; SELECT 1"),
            Some("DELETE")
        );
    }

    #[test]
    fn sql_safety_blocks_nested_block_comments() {
        assert_eq!(
            check_sql_safety("/* /* nested */ */ TRUNCATE TABLE t"),
            Some("TRUNCATE")
        );
    }

    #[test]
    fn sql_safety_blocks_mixed_comments_and_semicolons() {
        assert_eq!(
            check_sql_safety("-- safe\nSELECT 1; /* comment */ ALTER TABLE t ADD c INT"),
            Some("ALTER")
        );
    }

    #[test]
    fn extract_table_from_common_sql() {
        assert_eq!(
            extract_table_from_sql("SELECT * FROM astra_runtime.ctx_snapshots WHERE id = 1"),
            Some("astra_runtime.ctx_snapshots".into())
        );
        assert_eq!(
            extract_table_from_sql("SELECT col FROM `my_table` LIMIT 5"),
            Some("my_table".into())
        );
        assert_eq!(
            extract_table_from_sql("DESCRIBE works"),
            Some("works".into())
        );
        assert_eq!(extract_table_from_sql("SHOW DATABASES"), None);
    }
}
