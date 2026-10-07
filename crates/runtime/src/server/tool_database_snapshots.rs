pub(crate) use astra_turn_core::database_snapshots::{
    DatabaseSnapshotRollbackEntry, DatabaseSnapshotRollbackJournal, journal_checkpoint,
};
use astra_turn_core::safety_middleware::sql_requires_pre_state_snapshot as mo_query_requires_pre_state_snapshot;
use std::process::Command;
use std::sync::Mutex;

use astra_services::snapshot_sql::{
    create_snapshot_for_db_sql as mo_create_snapshot_sql,
    drop_snapshot_sql as mo_drop_snapshot_sql,
    restore_database_from_snapshot_sql as mo_restore_snapshot_sql,
};

use serde_json::Value;

pub(crate) fn mo_pre_state_snapshot_name() -> String {
    format!("moq_{}", uuid::Uuid::now_v7().simple())
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

fn execute_snapshot_rollback(
    entry: &DatabaseSnapshotRollbackEntry,
    operation: astra_turn_core::database_snapshots::SnapshotRollbackOperation,
) -> Result<(), String> {
    use astra_turn_core::database_snapshots::SnapshotRollbackOperation;
    let sql = match operation {
        SnapshotRollbackOperation::Restore => mo_restore_snapshot_sql(
            &entry.snapshot_id,
            mo_current_account(),
            &resolved_mo_database(entry.database.as_deref()),
        ),
        SnapshotRollbackOperation::Drop => mo_drop_snapshot_sql(&entry.snapshot_id),
    };
    let output = mo_execute_sql(&sql, None);
    if is_mo_error(&output) {
        Err(output)
    } else {
        Ok(())
    }
}

pub(crate) fn execute_mo_query(
    journal: &Mutex<DatabaseSnapshotRollbackJournal>,
    args: &Value,
    turn_index: u32,
    cancel_token: Option<&tokio_util::sync::CancellationToken>,
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

    if !mo_query_requires_pre_state_snapshot(sql, allow_destructive) {
        let output = mo_execute_sql(sql, args.get("database").and_then(Value::as_str));
        return astra_tools::ToolResult {
            is_error: is_mo_error(&output),
            output,
            metadata: None,
            exit_semantics: None,
        };
    }
    astra_turn_core::database_snapshots::with_journal_mut(journal, "execute_mo_query", |journal| {
        if cancel_token.is_some_and(tokio_util::sync::CancellationToken::is_cancelled) {
            return astra_tools::cancelled_tool_result("mo_query", false);
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
            journal.record(
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
    })
}

pub(crate) fn rollback_database_snapshots(
    journal: &Mutex<DatabaseSnapshotRollbackJournal>,
    args: &Value,
    current_turn_index: u32,
) -> String {
    astra_turn_core::database_snapshots::rollback_database_snapshots(
        journal,
        args,
        current_turn_index,
        &resolved_mo_database(None),
        execute_snapshot_rollback,
    )
}
