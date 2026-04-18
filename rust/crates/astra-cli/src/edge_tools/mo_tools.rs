//! MatrixOne convergence tools: SQL query, snapshot, and branch operations.
//!
//! These tools bridge code versioning (git) with data versioning (MatrixOne),
//! enabling coordinated management of code and database state.
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

use std::process::Command;

use super::*;
use crate::tool_safety_guard::check_sql_safety;
#[cfg(test)]
use crate::tool_safety_guard::strip_sql_comments;
use uuid::Uuid;

// ─── MatrixOne connection helper ────────────────────────────────────────────

const MO_CONNECT_TIMEOUT_SECS: u32 = 5;

/// Cached account name — queried once via `SELECT current_account_name()`.
fn mo_current_account() -> &'static str {
    use std::sync::OnceLock;
    static ACCOUNT: OnceLock<String> = OnceLock::new();
    ACCOUNT.get_or_init(|| {
        let out = mo_execute_sql("SELECT current_account_name() AS name", None);
        // Parse the value from mysql --table output.
        out.lines()
            .filter(|l| !l.starts_with('+') && !l.contains("name"))
            .find_map(|l| {
                let trimmed = l.trim().trim_matches('|').trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                }
            })
            .unwrap_or_else(|| "sys".to_string())
    })
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
    format!(
        "CREATE SNAPSHOT `{name}` FOR DATABASE `{}`",
        resolved_mo_database(database)
    )
}

fn mo_restore_snapshot_sql(name: &str, database: Option<&str>) -> String {
    let account = mo_current_account();
    format!(
        "RESTORE ACCOUNT `{account}` DATABASE `{}` FROM SNAPSHOT `{name}`",
        resolved_mo_database(database)
    )
}

fn mo_query_requires_pre_state_snapshot(sql: &str, allow_destructive: bool) -> bool {
    match sql
        .split_whitespace()
        .next()
        .map(|keyword| keyword.trim_matches(|c: char| c == '(' || c == ';'))
        .map(str::to_ascii_uppercase)
        .as_deref()
    {
        Some("INSERT" | "UPDATE" | "REPLACE" | "CREATE") => true,
        Some("DROP" | "DELETE" | "TRUNCATE" | "ALTER" | "GRANT" | "REVOKE") => true,
        _ => allow_destructive,
    }
}

fn mo_pre_state_snapshot_name() -> String {
    format!("moq_{}", Uuid::now_v7().simple())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DatabaseSnapshotRollbackEntry {
    sequence: u64,
    pub snapshot_id: String,
    pub database: Option<String>,
    pub turn_index: u32,
}

#[derive(Debug, Default)]
pub(crate) struct DatabaseSnapshotRollbackJournal {
    entries: Vec<DatabaseSnapshotRollbackEntry>,
    next_sequence: u64,
}

impl DatabaseSnapshotRollbackJournal {
    fn record(
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

    fn list(&self) -> Vec<DatabaseSnapshotRollbackEntry> {
        self.entries.iter().rev().cloned().collect()
    }

    fn entry_for_snapshot(&self, snapshot_id: &str) -> Option<DatabaseSnapshotRollbackEntry> {
        self.entries
            .iter()
            .rev()
            .find(|entry| entry.snapshot_id == snapshot_id)
            .cloned()
    }

    fn restore_plan_for_turn(&self, turn_index: u32) -> Vec<DatabaseSnapshotRollbackEntry> {
        self.restore_plan_for_turn_since(turn_index, 0)
    }

    fn restore_plan_for_turn_since(
        &self,
        turn_index: u32,
        checkpoint: u64,
    ) -> Vec<DatabaseSnapshotRollbackEntry> {
        let mut seen_databases = std::collections::HashSet::new();
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

    fn checkpoint(&self) -> u64 {
        self.next_sequence
    }

    fn remove_snapshot(&mut self, snapshot_id: &str) -> bool {
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

fn is_mo_error(output: &str) -> bool {
    output.trim_start().starts_with("Error:")
}

/// Build a mysql Command with connection parameters from environment.
fn mo_mysql_cmd(database: Option<&str>) -> Command {
    astra_core::warn_default_credentials_once();
    let host = std::env::var("MATRIXONE_HOST").unwrap_or_else(|_| "localhost".to_string());
    let port = std::env::var("MATRIXONE_PORT").unwrap_or_else(|_| "6001".to_string());
    let user = std::env::var("MATRIXONE_USER").unwrap_or_else(|_| "root".to_string());
    let password = std::env::var("MATRIXONE_PASSWORD")
        .unwrap_or_else(|_| astra_core::DEV_MATRIXONE_PASSWORD.to_string());
    let db = database
        .map(String::from)
        .unwrap_or_else(|| astra_core::resolve_database_name(&|k| std::env::var(k).ok()));

    let mut cmd = Command::new("mysql");
    cmd.arg(format!("-h{}", host))
        .arg(format!("-P{}", port))
        .arg(format!("-u{}", user))
        .env("MYSQL_PWD", &password) // pass via env, not CLI (hidden from ps)
        .arg(&db)
        .arg(format!("--connect-timeout={MO_CONNECT_TIMEOUT_SECS}"))
        .arg("--table"); // Pretty-print results
    cmd
}

/// Execute a SQL statement against MatrixOne via the mysql CLI.
fn mo_execute_sql(sql: &str, database: Option<&str>) -> String {
    let mut cmd = mo_mysql_cmd(database);
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
                msg
            } else if stdout.is_empty() {
                "OK (no results)".to_string()
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
                    t
                } else {
                    result
                }
            }
        }
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                "Error: mysql client not found. Install mysql-client or mariadb-client to use MatrixOne tools.\nHint: apt install mariadb-client OR brew install mysql-client".to_string()
            } else {
                format!("Error: failed to execute mysql: {e}")
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
            let desc = mo_execute_sql(&format!("DESCRIBE {table}"), database);
            if !desc.starts_with("Error:") {
                return Some(desc);
            }
        }
    } else if lower_err.contains("table") && lower_err.contains("does not exist") {
        // Table not found → SHOW TABLES in the database
        let db = database
            .map(String::from)
            .unwrap_or_else(|| astra_core::resolve_database_name(&|k| std::env::var(k).ok()));
        if !db.is_empty() {
            let tables = mo_execute_sql(&format!("SHOW TABLES IN `{db}`"), None);
            if !tables.starts_with("Error:") {
                return Some(tables);
            }
        }
    }
    None
}

// ─── Snapshot name validation ───────────────────────────────────────────────

/// Validate snapshot name: alphanumeric + underscore + hyphen only.
fn is_valid_snapshot_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
}

// ─── Tool implementations ───────────────────────────────────────────────────

impl ToolExecutor {
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

    fn database_snapshot_entries(&self) -> Vec<DatabaseSnapshotRollbackEntry> {
        match self.database_snapshot_journal.lock() {
            Ok(journal) => journal.list(),
            Err(poisoned) => poisoned.into_inner().list(),
        }
    }

    fn database_snapshot_entry_for_snapshot(
        &self,
        snapshot_id: &str,
    ) -> Option<DatabaseSnapshotRollbackEntry> {
        match self.database_snapshot_journal.lock() {
            Ok(journal) => journal.entry_for_snapshot(snapshot_id),
            Err(poisoned) => poisoned.into_inner().entry_for_snapshot(snapshot_id),
        }
    }

    fn database_snapshot_restore_plan_for_turn(
        &self,
        turn_index: u32,
    ) -> Vec<DatabaseSnapshotRollbackEntry> {
        match self.database_snapshot_journal.lock() {
            Ok(journal) => journal.restore_plan_for_turn(turn_index),
            Err(poisoned) => poisoned.into_inner().restore_plan_for_turn(turn_index),
        }
    }

    fn database_snapshot_restore_plan_for_turn_since(
        &self,
        turn_index: u32,
        checkpoint: u64,
    ) -> Vec<DatabaseSnapshotRollbackEntry> {
        match self.database_snapshot_journal.lock() {
            Ok(journal) => journal.restore_plan_for_turn_since(turn_index, checkpoint),
            Err(poisoned) => poisoned
                .into_inner()
                .restore_plan_for_turn_since(turn_index, checkpoint),
        }
    }

    pub(crate) fn database_snapshot_journal_checkpoint(&self) -> u64 {
        match self.database_snapshot_journal.lock() {
            Ok(journal) => journal.checkpoint(),
            Err(poisoned) => poisoned.into_inner().checkpoint(),
        }
    }

    fn remove_database_snapshot_rollback(&self, snapshot_id: &str) {
        match self.database_snapshot_journal.lock() {
            Ok(mut journal) => {
                journal.remove_snapshot(snapshot_id);
            }
            Err(poisoned) => {
                poisoned.into_inner().remove_snapshot(snapshot_id);
            }
        }
    }

    fn rollback_database_snapshot_entry_json(entry: &DatabaseSnapshotRollbackEntry) -> Value {
        let mut value = serde_json::Map::from_iter([
            (
                "snapshot_id".to_string(),
                Value::String(entry.snapshot_id.clone()),
            ),
            ("turn_index".to_string(), Value::from(entry.turn_index)),
        ]);
        if let Some(database) = entry.database.as_ref() {
            value.insert("database".to_string(), Value::String(database.clone()));
        }
        Value::Object(value)
    }

    fn restore_database_snapshot_entry(
        &self,
        entry: &DatabaseSnapshotRollbackEntry,
    ) -> Result<String, String> {
        let restore_output = mo_execute_sql(
            &mo_restore_snapshot_sql(&entry.snapshot_id, entry.database.as_deref()),
            None,
        );
        if is_mo_error(&restore_output) {
            Err(restore_output)
        } else {
            Ok(restore_output)
        }
    }

    /// `mo_query`: Execute a SQL query against MatrixOne.
    /// Foundation tool for all database operations.
    /// Blocks destructive DDL/DML (DROP, DELETE, TRUNCATE, ALTER, GRANT, REVOKE)
    /// unless the caller explicitly passes `"allow_destructive": true`.
    /// Mutating queries capture a pre-state snapshot before execution so the
    /// runtime can surface a concrete rollback hint on staged mutations.
    pub(crate) fn mo_query(&self, args: &Value) -> String {
        self.mo_query_with_metadata(args).output
    }

    pub(crate) fn mo_query_with_metadata(&self, args: &Value) -> ToolExecutionOutcome {
        let sql = match args.get("sql").and_then(Value::as_str) {
            Some(s) if !s.trim().is_empty() => s,
            _ => {
                return ToolExecutionOutcome::text(
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
            return ToolExecutionOutcome::text(format!(
                "Error: {kind} statements are blocked by default. \
                     Pass \"allow_destructive\": true to confirm execution."
            ));
        }

        let database = args.get("database").and_then(Value::as_str);
        let resolved_database = resolved_mo_database(database);
        let mut tool_result_fields = None;
        if mo_query_requires_pre_state_snapshot(sql, allow_destructive) {
            let snapshot_id = mo_pre_state_snapshot_name();
            let snapshot_output =
                mo_execute_sql(&mo_create_snapshot_sql(&snapshot_id, database), None);
            if is_mo_error(&snapshot_output) {
                return ToolExecutionOutcome::text(format!(
                    "Error: failed to capture pre-state snapshot `{snapshot_id}` before executing query.\n{snapshot_output}"
                ));
            }
            self.record_database_snapshot_rollback(
                snapshot_id.clone(),
                Some(resolved_database.clone()),
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

        ToolExecutionOutcome {
            output: mo_execute_sql(sql, database),
            tool_result_fields,
        }
    }

    pub(crate) fn rollback_database_snapshots(&self, args: &Value) -> String {
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
                let entries: Vec<Value> = self
                    .database_snapshot_entries()
                    .into_iter()
                    .map(|entry| Self::rollback_database_snapshot_entry_json(&entry))
                    .collect();
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
                let journal_entry = self.database_snapshot_entry_for_snapshot(snapshot_id);
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
                    turn_index: journal_entry.as_ref().map_or_else(
                        || {
                            self.journal_turn_index
                                .load(std::sync::atomic::Ordering::Relaxed)
                        },
                        |entry| entry.turn_index,
                    ),
                };
                match self.restore_database_snapshot_entry(&entry) {
                    Ok(_) => {
                        self.remove_database_snapshot_rollback(snapshot_id);
                        let database = entry.database.clone();
                        let summary = format!(
                            "Restored MatrixOne snapshot `{}`{}",
                            snapshot_id,
                            database
                                .as_deref()
                                .map(|database| format!(" for database `{database}`"))
                                .unwrap_or_default()
                        );
                        json!({
                            "success": true,
                            "scope": "snapshot",
                            "snapshot_id": snapshot_id,
                            "database": database,
                            "summary": summary,
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
                    self.journal_turn_index
                        .load(std::sync::atomic::Ordering::Relaxed)
                };
                let checkpoint = args
                    .get("database_after_sequence")
                    .or_else(|| args.get("after_sequence"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let plan =
                    self.database_snapshot_restore_plan_for_turn_since(turn_index, checkpoint);
                let mut restored = Vec::new();
                let mut failed = Vec::new();
                for entry in &plan {
                    match self.restore_database_snapshot_entry(entry) {
                        Ok(_) => {
                            self.remove_database_snapshot_rollback(&entry.snapshot_id);
                            restored.push(Self::rollback_database_snapshot_entry_json(entry));
                        }
                        Err(error) => {
                            let mut failed_entry =
                                Self::rollback_database_snapshot_entry_json(entry)
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

    /// `mo_snapshot`: Create, list, or delete MatrixOne snapshots.
    /// Snapshots capture point-in-time database state for rollback and branching.
    pub(crate) fn mo_snapshot(&self, args: &Value) -> String {
        let action = args.get("action").and_then(Value::as_str).unwrap_or("list");
        let database = args.get("database").and_then(Value::as_str);

        match action {
            "create" => {
                let name = match args.get("name").and_then(Value::as_str) {
                    Some(n) if is_valid_snapshot_name(n) => n,
                    Some(n) => {
                        return format!(
                            "Error: invalid snapshot name '{}'. Use alphanumeric, underscore, or hyphen (max 64 chars)",
                            n
                        );
                    }
                    None => return "Error: missing 'name' for snapshot creation".to_string(),
                };
                // MatrixOne database-level snapshot
                let sql = mo_create_snapshot_sql(name, database);
                mo_execute_sql(&sql, None)
            }
            "list" => mo_execute_sql("SHOW SNAPSHOTS", None),
            "drop" | "delete" => {
                let name = match args.get("name").and_then(Value::as_str) {
                    Some(n) if is_valid_snapshot_name(n) => n,
                    Some(n) => {
                        return format!(
                            "Error: invalid snapshot name '{}'. Use alphanumeric, underscore, or hyphen",
                            n
                        );
                    }
                    None => return "Error: missing 'name' for snapshot deletion".to_string(),
                };
                let sql = format!("DROP SNAPSHOT IF EXISTS `{}`", name);
                mo_execute_sql(&sql, None)
            }
            "restore" => {
                let name = match args.get("name").and_then(Value::as_str) {
                    Some(n) if is_valid_snapshot_name(n) => n,
                    Some(n) => {
                        return format!(
                            "Error: invalid snapshot name '{}'. Use alphanumeric, underscore, or hyphen",
                            n
                        );
                    }
                    None => return "Error: missing 'name' for snapshot restore".to_string(),
                };
                let sql = mo_restore_snapshot_sql(name, database);
                mo_execute_sql(&sql, None)
            }
            other => format!(
                "Error: unknown action '{}'. Supported: create, list, drop, restore",
                other
            ),
        }
    }

    /// `mo_branch`: Coordinate git branches with MatrixOne data branches.
    /// Creates/lists data branches that mirror git branches for experiment isolation.
    pub(crate) fn mo_branch(&self, args: &Value) -> String {
        let action = args.get("action").and_then(Value::as_str).unwrap_or("list");

        match action {
            "list" => {
                // Show snapshots as "branches" + current git branch for context
                let mut result = String::new();

                // Current git branch
                let git_branch = super::git_gix::current_branch(&self.project_root);
                if !git_branch.is_empty() {
                    result.push_str(&format!("## Current Git Branch: {}\n\n", git_branch));
                }

                // MatrixOne snapshots (serve as data branches)
                let snapshots = mo_execute_sql("SHOW SNAPSHOTS", None);
                result.push_str("## MatrixOne Snapshots (Data Branches)\n");
                result.push_str(&snapshots);

                result
            }
            "create" => {
                let name = match args.get("name").and_then(Value::as_str) {
                    Some(n) if is_valid_snapshot_name(n) => n,
                    Some(n) => {
                        return format!(
                            "Error: invalid branch name '{}'. Use alphanumeric, underscore, or hyphen",
                            n
                        );
                    }
                    None => {
                        // Auto-generate name from git branch
                        let branch = super::git_gix::current_branch(&self.project_root);
                        if branch.is_empty() {
                            return "Error: missing 'name' and no git branch detected".to_string();
                        }
                        // Sanitize: keep only alphanumeric and underscore
                        let auto_name: String = format!("br_{}", branch.replace(['/', '-'], "_"))
                            .chars()
                            .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                            .collect();
                        if !is_valid_snapshot_name(&auto_name) {
                            return format!(
                                "Error: git branch '{}' produced invalid snapshot name '{}'",
                                branch, auto_name
                            );
                        }
                        let sql = mo_create_snapshot_sql(&auto_name, None);
                        return format!(
                            "Creating data branch '{}' aligned with git branch '{}'\n\n{}",
                            auto_name,
                            branch,
                            mo_execute_sql(&sql, None)
                        );
                    }
                };
                let sql = mo_create_snapshot_sql(name, None);
                mo_execute_sql(&sql, None)
            }
            "sync" => {
                // Show the relationship between git state and MO state
                let mut result = String::from("## Git ↔ MatrixOne Sync Status\n\n");

                let git_branch = super::git_gix::current_branch(&self.project_root);
                let git_head = super::git_gix::head_short(&self.project_root);
                result.push_str(&format!("Git: branch={}, head={}\n", git_branch, git_head));

                let snapshots = mo_execute_sql("SHOW SNAPSHOTS", None);
                result.push_str(&format!("MatrixOne snapshots:\n{}\n", snapshots));

                // Check if there's a matching snapshot for current branch
                let expected_name = format!("br_{}", git_branch.replace(['/', '-'], "_"));
                if snapshots.contains(&expected_name) {
                    result.push_str(&format!(
                        "\n✅ Data branch '{}' exists for git branch '{}'",
                        expected_name, git_branch
                    ));
                } else {
                    result.push_str(&format!(
                        "\n⚠ No data branch found for git branch '{}'. Create with: mo_branch action=create",
                        git_branch
                    ));
                }

                result
            }
            other => format!(
                "Error: unknown action '{}'. Supported: list, create, sync",
                other
            ),
        }
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
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
    fn mo_snapshot_invalid_action() {
        let executor = ToolExecutor::new(std::env::temp_dir());
        let result = executor.mo_snapshot(&serde_json::json!({"action": "fly"}));
        assert!(result.contains("Error"), "should error: {result}");
        assert!(
            result.contains("create, list, drop, restore"),
            "should show supported actions: {result}"
        );
    }

    #[test]
    fn mo_snapshot_create_missing_name() {
        let executor = ToolExecutor::new(std::env::temp_dir());
        let result = executor.mo_snapshot(&serde_json::json!({"action": "create"}));
        assert!(result.contains("Error"), "should error: {result}");
    }

    #[test]
    fn mo_snapshot_create_invalid_name() {
        let executor = ToolExecutor::new(std::env::temp_dir());
        let result =
            executor.mo_snapshot(&serde_json::json!({"action": "create", "name": "bad;name"}));
        assert!(
            result.contains("Error"),
            "should reject SQL-injection name: {result}"
        );
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

    #[test]
    fn mo_branch_invalid_action() {
        let executor = ToolExecutor::new(std::env::temp_dir());
        let result = executor.mo_branch(&serde_json::json!({"action": "merge"}));
        assert!(result.contains("Error"), "should error: {result}");
    }

    // ── mo_execute_sql tests (mysql client may not be available) ──

    #[test]
    fn mo_execute_sql_returns_graceful_error_if_no_mysql() {
        let _guard = env_guard();
        unsafe {
            std::env::set_var("MATRIXONE_HOST", "nonexistent-host-12345");
            std::env::set_var("MATRIXONE_PORT", "6001");
        }
        let result = mo_execute_sql("SELECT 1", None);
        assert!(
            result.contains("Error") || result.contains("error") || result.contains("not found"),
            "should handle missing mysql gracefully: {result}"
        );
        unsafe {
            std::env::remove_var("MATRIXONE_HOST");
            std::env::remove_var("MATRIXONE_PORT");
        }
    }

    #[test]
    fn mo_mysql_cmd_uses_env_vars() {
        let _guard = env_guard();
        unsafe {
            std::env::set_var("MATRIXONE_HOST", "testhost");
            std::env::set_var("MATRIXONE_PORT", "7001");
        }
        let cmd = mo_mysql_cmd(Some("testdb"));
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
        }
    }

    #[test]
    fn mo_mysql_cmd_default_database() {
        let _guard = env_guard();
        unsafe {
            std::env::remove_var("ASTRA_DATABASE");
            std::env::remove_var("ASTRA_DATABASE_PREFIX");
        }
        let cmd = mo_mysql_cmd(None);
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(
            args.iter().any(|a| a == "astra_runtime"),
            "should default to astra_runtime: {:?}",
            args
        );
    }

    #[test]
    fn mo_mysql_cmd_default_database_applies_prefix() {
        let _guard = env_guard();
        unsafe {
            std::env::remove_var("ASTRA_DATABASE_PREFIX");
            std::env::set_var("ASTRA_DATABASE_PREFIX", "it_");
            std::env::set_var("ASTRA_DATABASE", "astra_runtime");
        }
        let cmd = mo_mysql_cmd(None);
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
    fn strip_sql_comments_preserves_content() {
        assert_eq!(
            strip_sql_comments("SELECT /* col */ name FROM t").trim(),
            "SELECT   name FROM t"
        );
        assert_eq!(
            strip_sql_comments("SELECT 1 -- inline\nFROM t").trim(),
            "SELECT 1  FROM t"
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
            extract_table_from_sql("DESCRIBE agent_tasks"),
            Some("agent_tasks".into())
        );
        assert_eq!(extract_table_from_sql("SHOW DATABASES"), None);
    }
}
