use crate::auth::DatabaseUserRecord;
use crate::auth::session::SessionRecord;
use astra_core::{
    DedicatedPool, ErrorResponse, MatrixOneSettings, connect_matrixone, identity::USER_ID_MAX_LEN,
    internal_error,
};
use axum::{Json, http::StatusCode};
use fs2::FileExt;
use sqlx::{Executor, MySql, QueryBuilder, Row, query};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::OpenOptions;
use std::sync::OnceLock;
use uuid::Uuid;

const CAUSAL_EDGE_KIND: &str = "causal";
const EDGE_PENDING_DISPATCH_IDENTITY_COLUMNS: &[&str] = &[
    "user_id",
    "session_id",
    "run_id",
    "turn_chain_id",
    "request_id",
];
const EDGE_PENDING_DISPATCH_LEGACY_COLUMNS: &[&str] = &[
    "user_id",
    "edge_agent_id",
    "request_id",
    "payload_json",
    "result_json",
    "status",
    "pod_id",
    "dispatched_at",
    "completed_at",
    "created_at",
];
const EDGE_PENDING_DISPATCH_LEGACY_PRIMARY_KEY: &[&str] = &["user_id", "request_id"];
const EDGE_PENDING_DISPATCH_LEGACY_ARCHIVE_TABLE: &str =
    "edge_pending_dispatch_legacy_owner_request_v1";

/// Standard column width for `agent_id` across all tables.
/// All `agent_id`, `edge_agent_id`, `holder_agent_id`, and `parent_agent_id`
/// columns MUST use this width for consistency and join compatibility.
pub const AGENT_ID_LEN: usize = 255;
/// Width for application event identifiers and references.
///
/// Journal ingestion uses `evt-` plus a full SHA-256 digest (68 chars), and
/// several runtime paths use UUID-like event ids. Keep the schema wider than
/// the current longest generated id so evidence/trace writes do not fail after
/// the run has already started.
pub const AGENT_EVENT_ID_LEN: usize = 128;
static CORE_SCHEMA_INIT_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

pub(crate) fn rows_affected_to_i64(rows: u64, context: &str) -> Result<i64, sqlx::Error> {
    i64::try_from(rows).map_err(|_| {
        sqlx::Error::Protocol(format!("{context}: rows_affected {rows} exceeds i64::MAX"))
    })
}

const AGENT_EVENTS_OWNER_SESSION_TURN_INDEX_DECL: &str =
    "INDEX idx_agent_events_owner_session_turn (user_id, session_id, turn_seq)";
const AGENT_EVENTS_OWNER_SESSION_TURN_INDEX_ALTER_SQL: &str = "ALTER TABLE agent_events ADD INDEX idx_agent_events_owner_session_turn (user_id, session_id, turn_seq)";

fn agent_events_create_sql() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS agent_events (
            event_id VARCHAR({AGENT_EVENT_ID_LEN}) NOT NULL,
            session_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            agent_id VARCHAR(255) NULL,
            agent_version VARCHAR(32) NULL,
            event_type VARCHAR(64) NOT NULL,
            content LONGTEXT NULL,
            parent_event_id VARCHAR({AGENT_EVENT_ID_LEN}) NULL,
            causal_chain_id VARCHAR({AGENT_EVENT_ID_LEN}) NULL,
            run_id VARCHAR(64) NULL,
            parent_run_id VARCHAR(64) NULL,
            turn_id VARCHAR(64) NULL,
            turn_seq BIGINT NULL,
            round_index BIGINT NULL,
            tool_call_id VARCHAR(128) NULL,
            parent_agent_id VARCHAR(255) NULL,
            trace_kind VARCHAR(64) NULL,
            token_usage JSON NULL,
            llm_model_used VARCHAR(128) NULL,
            llm_params JSON NULL,
            metadata JSON NULL,
            skill_name VARCHAR(255) NULL,
            skill_version VARCHAR(64) NULL,
            reasoning_content LONGTEXT NULL,
            token_input  BIGINT NULL,
            token_output BIGINT NULL,
            token_total  BIGINT NULL,
            meta_tool_name VARCHAR(255) NULL,
            meta_duration_ms INT NULL,
            user_feedback_score BIGINT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, event_id),
            INDEX idx_agent_events_owner_session_created (user_id, session_id, created_at),
            INDEX idx_agent_events_owner_session_type_created (user_id, session_id, event_type, created_at),
            INDEX idx_agent_events_owner_session_model_created (user_id, session_id, llm_model_used, created_at DESC),
            INDEX idx_agent_events_owner_session_parent (user_id, session_id, parent_event_id),
            {AGENT_EVENTS_OWNER_SESSION_TURN_INDEX_DECL},
            INDEX idx_agent_events_user_created (user_id, created_at),
            INDEX idx_agent_events_owner_causal_chain_created (user_id, causal_chain_id, created_at, event_id),
            INDEX idx_agent_events_skill_created (skill_name, created_at),
            INDEX idx_agent_events_created_at (created_at),
            INDEX idx_agent_events_tool_name (meta_tool_name),
            INDEX idx_agent_events_trace (user_id, session_id, turn_id, created_at),
            INDEX idx_agent_events_run (user_id, session_id, run_id, created_at),
            INDEX idx_agent_events_parent_run (user_id, session_id, parent_run_id, created_at),
            INDEX idx_agent_events_tool_call (user_id, session_id, tool_call_id)
        )"
    )
}

const EVAL_CALIBRATION_ASSESSMENTS_CREATE_SQL: &str =
    "CREATE TABLE IF NOT EXISTS eval_calibration_assessments (
            calibration_id VARCHAR(64) NOT NULL,
            user_id        VARCHAR(128) NOT NULL,
            agent_id       VARCHAR(255),
            session_id     VARCHAR(64) NOT NULL,
            confidence     DECIMAL(5,4) NOT NULL,
            quality_score  DECIMAL(5,4) NOT NULL,
            created_at     DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, calibration_id),
            INDEX idx_eval_calibration_user_created (user_id, created_at),
            INDEX idx_eval_calibration_user_agent_created (user_id, agent_id, created_at),
            INDEX idx_eval_calibration_session (user_id, session_id, created_at)
        )";

struct CoreSchemaFileLock {
    file: std::fs::File,
}

impl Drop for CoreSchemaFileLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn schema_lock_component(raw: &str) -> String {
    raw.chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect()
}

fn acquire_core_schema_file_lock(
    settings: &MatrixOneSettings,
) -> Result<CoreSchemaFileLock, sqlx::Error> {
    let user = schema_lock_component(
        &std::env::var("USER")
            .or_else(|_| std::env::var("LOGNAME"))
            .unwrap_or_else(|_| "unknown".into()),
    );
    let lock_dir = std::env::temp_dir().join(format!("astra-engine-locks-{user}"));
    std::fs::create_dir_all(&lock_dir).map_err(sqlx::Error::Io)?;
    let lock_path = lock_dir.join(format!(
        "core-schema-{}-{}-{}.lock",
        schema_lock_component(&settings.host),
        settings.port,
        schema_lock_component(&settings.database),
    ));
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(lock_path)
        .map_err(sqlx::Error::Io)?;
    file.lock_exclusive().map_err(sqlx::Error::Io)?;
    Ok(CoreSchemaFileLock { file })
}

async fn acquire_core_schema_file_lock_blocking(
    settings: MatrixOneSettings,
) -> Result<CoreSchemaFileLock, sqlx::Error> {
    tokio::task::spawn_blocking(move || acquire_core_schema_file_lock(&settings))
        .await
        .map_err(|error| {
            sqlx::Error::Protocol(format!("core schema file lock task failed: {error}"))
        })?
}

fn unique_event_ids(event_ids: &[String]) -> Vec<&str> {
    let mut seen = HashSet::new();
    event_ids
        .iter()
        .map(String::as_str)
        .filter(|event_id| !event_id.trim().is_empty() && seen.insert(*event_id))
        .collect()
}

pub fn normalized_parent_event_ids(
    primary_parent_event_id: Option<&str>,
    parent_event_ids: Option<&[String]>,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    if let Some(primary_parent_event_id) = primary_parent_event_id.map(str::trim)
        && !primary_parent_event_id.is_empty()
        && seen.insert(primary_parent_event_id.to_string())
    {
        out.push(primary_parent_event_id.to_string());
    }

    if let Some(parent_event_ids) = parent_event_ids {
        for parent_event_id in parent_event_ids {
            let parent_event_id = parent_event_id.trim();
            if parent_event_id.is_empty() {
                continue;
            }
            if seen.insert(parent_event_id.to_string()) {
                out.push(parent_event_id.to_string());
            }
        }
    }

    out
}

pub async fn agent_session_exists_for_user<'e, E>(
    executor: E,
    session_id: &str,
    user_id: &str,
) -> Result<bool, sqlx::Error>
where
    E: Executor<'e, Database = MySql>,
{
    let row =
        query("SELECT 1 AS owned FROM agent_sessions WHERE session_id = ? AND user_id = ? LIMIT 1")
            .bind(session_id)
            .bind(user_id)
            .fetch_optional(executor)
            .await?;
    Ok(row.is_some())
}

pub async fn agent_event_exists_for_user_session<'e, E>(
    executor: E,
    event_id: &str,
    session_id: &str,
    user_id: &str,
) -> Result<bool, sqlx::Error>
where
    E: Executor<'e, Database = MySql>,
{
    let row = query(
        "SELECT 1 AS owned FROM agent_events \
         WHERE event_id = ? AND session_id = ? AND user_id = ? LIMIT 1",
    )
    .bind(event_id)
    .bind(session_id)
    .bind(user_id)
    .fetch_optional(executor)
    .await?;
    Ok(row.is_some())
}

pub async fn bump_agent_session_event_count<'e, E>(
    executor: E,
    session_id: &str,
    user_id: &str,
    delta: i64,
    last_event_id: Option<&str>,
) -> Result<(), sqlx::Error>
where
    E: Executor<'e, Database = MySql>,
{
    let result = if delta >= 0 {
        query(
            "UPDATE agent_sessions \
             SET event_count = event_count + ?, \
                 updated_at = IF(last_active_at < DATE_SUB(NOW(6), INTERVAL 1 SECOND), NOW(6), updated_at), \
                 last_active_at = IF(last_active_at < DATE_SUB(NOW(6), INTERVAL 1 SECOND), NOW(6), last_active_at), \
                 last_event_id = COALESCE(?, last_event_id) \
             WHERE session_id = ? AND user_id = ?",
        )
        .bind(delta)
        .bind(last_event_id)
        .bind(session_id)
        .bind(user_id)
        .execute(executor)
        .await?
    } else {
        let decrement = delta.saturating_abs();
        query(
            "UPDATE agent_sessions \
             SET event_count = CASE \
                     WHEN event_count >= ? THEN event_count - ? \
                     ELSE 0 \
                 END, \
                 updated_at = IF(last_active_at < DATE_SUB(NOW(6), INTERVAL 1 SECOND), NOW(6), updated_at), \
                 last_active_at = IF(last_active_at < DATE_SUB(NOW(6), INTERVAL 1 SECOND), NOW(6), last_active_at), \
                 last_event_id = COALESCE(?, last_event_id) \
             WHERE session_id = ? AND user_id = ?",
        )
        .bind(decrement)
        .bind(decrement)
        .bind(last_event_id)
        .bind(session_id)
        .bind(user_id)
        .execute(executor)
        .await?
    };
    if result.rows_affected() == 0 {
        return Err(sqlx::Error::RowNotFound);
    }
    Ok(())
}

const ADD_AGENT_SESSION_EVENT_COUNT_OR_CREATE_SQL: &str = "INSERT INTO agent_sessions \
         (session_id, user_id, status, event_count, last_event_id, created_at, updated_at, last_active_at) \
         SELECT ?, ?, 'active', ?, ?, NOW(6), NOW(6), NOW(6) \
         FROM DUAL \
         WHERE NOT EXISTS ( \
             SELECT 1 FROM agent_sessions \
             WHERE session_id = ? AND user_id <> ? \
             LIMIT 1 \
         ) \
         ON DUPLICATE KEY UPDATE \
         event_count = IF(user_id = VALUES(user_id), event_count + VALUES(event_count), event_count), \
         last_event_id = IF(user_id = VALUES(user_id), COALESCE(VALUES(last_event_id), last_event_id), last_event_id), \
         updated_at = IF(user_id = VALUES(user_id) AND last_active_at < DATE_SUB(NOW(6), INTERVAL 1 SECOND), NOW(6), updated_at), \
         last_active_at = IF(user_id = VALUES(user_id) AND last_active_at < DATE_SUB(NOW(6), INTERVAL 1 SECOND), NOW(6), last_active_at)";

pub async fn add_agent_session_event_count_or_create<'e, E>(
    executor: E,
    session_id: &str,
    user_id: &str,
    delta: i64,
    last_event_id: Option<&str>,
) -> Result<(), sqlx::Error>
where
    E: Executor<'e, Database = MySql>,
{
    if delta < 0 {
        return Err(sqlx::Error::Protocol(
            "add_agent_session_event_count_or_create requires a non-negative delta".into(),
        ));
    }

    let result = query(ADD_AGENT_SESSION_EVENT_COUNT_OR_CREATE_SQL)
        .bind(session_id)
        .bind(user_id)
        .bind(delta)
        .bind(last_event_id)
        .bind(session_id)
        .bind(user_id)
        .execute(executor)
        .await?;
    if result.rows_affected() == 0 {
        return Err(sqlx::Error::RowNotFound);
    }
    Ok(())
}

pub async fn touch_agent_session_activity<'e, E>(
    executor: E,
    session_id: &str,
    user_id: &str,
    last_event_id: Option<&str>,
) -> Result<(), sqlx::Error>
where
    E: Executor<'e, Database = MySql> + Copy,
{
    let result = query(
        "UPDATE agent_sessions \
         SET updated_at = IF(last_active_at < DATE_SUB(NOW(6), INTERVAL 1 SECOND), NOW(6), updated_at), \
             last_active_at = IF(last_active_at < DATE_SUB(NOW(6), INTERVAL 1 SECOND), NOW(6), last_active_at), \
             last_event_id = COALESCE(?, last_event_id) \
         WHERE session_id = ? AND user_id = ?",
    )
    .bind(last_event_id)
    .bind(session_id)
    .bind(user_id)
    .execute(executor)
    .await?;
    if result.rows_affected() == 0 {
        let exists = agent_session_exists_for_user(executor, session_id, user_id).await?;
        if !exists {
            return Err(sqlx::Error::RowNotFound);
        }
    }
    Ok(())
}

pub async fn insert_agent_event_edges<'e, E>(
    executor: E,
    user_id: &str,
    session_id: &str,
    child_event_id: &str,
    primary_parent_event_id: Option<&str>,
    parent_event_ids: &[String],
) -> Result<(), sqlx::Error>
where
    E: sqlx::Executor<'e, Database = MySql>,
{
    let normalized = normalized_parent_event_ids(primary_parent_event_id, Some(parent_event_ids));
    if normalized.is_empty() {
        return Ok(());
    }

    let mut builder = QueryBuilder::<MySql>::new(
        "INSERT INTO agent_event_edges \
         (user_id, session_id, child_event_id, parent_event_id, relation_kind, parent_order) ",
    );
    builder.push_values(
        normalized.iter().enumerate(),
        |mut row, (idx, parent_event_id)| {
            row.push_bind(user_id)
                .push_bind(session_id)
                .push_bind(child_event_id)
                .push_bind(parent_event_id)
                .push_bind(CAUSAL_EDGE_KIND)
                .push_bind(i32::try_from(idx).unwrap_or(i32::MAX));
        },
    );
    builder.push(
        " ON DUPLICATE KEY UPDATE \
         session_id = VALUES(session_id), \
         parent_order = VALUES(parent_order)",
    );
    builder.build().execute(executor).await?;
    Ok(())
}

pub async fn load_agent_event_parent_ids<'e, E>(
    executor: E,
    user_id: &str,
    event_ids: &[String],
) -> Result<HashMap<String, Vec<String>>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = MySql>,
{
    let mut out = HashMap::new();
    let event_ids = unique_event_ids(event_ids);
    if event_ids.is_empty() {
        return Ok(out);
    }

    let mut builder = QueryBuilder::<MySql>::new(
        "SELECT child_event_id, parent_event_id \
         FROM agent_event_edges WHERE user_id = ",
    );
    builder.push_bind(user_id);
    builder.push(" AND relation_kind = ");
    builder.push_bind(CAUSAL_EDGE_KIND);
    builder.push(" AND child_event_id IN (");
    let mut separated = builder.separated(", ");
    for event_id in &event_ids {
        separated.push_bind(*event_id);
    }
    separated.push_unseparated(")");
    builder.push(" ORDER BY child_event_id ASC, parent_order ASC, parent_event_id ASC");

    let rows = builder.build().fetch_all(executor).await?;
    for row in rows {
        let child_event_id: String = row.try_get("child_event_id")?;
        let parent_event_id: String = row.try_get("parent_event_id")?;
        out.entry(child_event_id).or_default().push(parent_event_id);
    }
    Ok(out)
}

pub async fn delete_agent_event_edges_for_owned_event_ids<'e, E>(
    executor: E,
    owned_event_ids: &[(String, String)],
) -> Result<u64, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = MySql>,
{
    let owned_event_ids: Vec<(String, String)> = owned_event_ids
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if owned_event_ids.is_empty() {
        return Ok(0);
    }

    let mut builder = QueryBuilder::<MySql>::new(
        "DELETE FROM agent_event_edges WHERE (user_id, child_event_id) IN (",
    );
    {
        let mut child_keys = builder.separated(", ");
        for (user_id, event_id) in &owned_event_ids {
            child_keys
                .push_unseparated("(")
                .push_bind(user_id)
                .push_unseparated(", ")
                .push_bind(event_id)
                .push_unseparated(")");
        }
        child_keys.push_unseparated(")");
    }
    builder.push(" OR (user_id, parent_event_id) IN (");
    {
        let mut parent_keys = builder.separated(", ");
        for (user_id, event_id) in &owned_event_ids {
            parent_keys
                .push_unseparated("(")
                .push_bind(user_id)
                .push_unseparated(", ")
                .push_bind(event_id)
                .push_unseparated(")");
        }
        parent_keys.push_unseparated(")");
    }

    let result = builder.build().execute(executor).await?;
    Ok(result.rows_affected())
}

/// When `ASTRA_AUTO_CREATE_DATABASE=1`, connect to `bootstrap_catalog` and
/// run `CREATE DATABASE IF NOT EXISTS` for [`MatrixOneSettings::database`] before normal DDL.
async fn ensure_matrixone_database_exists(
    settings: &MatrixOneSettings,
    bootstrap_catalog: &str,
) -> Result<(), sqlx::Error> {
    use std::error::Error;

    crate::snapshot_sql::validate_sql_identifier(&settings.database, "matrixone database")
        .map_err(|e| {
            sqlx::Error::Configuration(Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                e,
            )) as Box<dyn Error + Send + Sync>)
        })?;
    crate::snapshot_sql::validate_sql_identifier(bootstrap_catalog, "matrixone bootstrap catalog")
        .map_err(|e| {
            sqlx::Error::Configuration(Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                e,
            )) as Box<dyn Error + Send + Sync>)
        })?;
    let mut admin_settings = settings.clone();
    admin_settings.database = bootstrap_catalog.to_string();
    let admin_pool = DedicatedPool::new(
        connect_matrixone(&admin_settings).await?,
        admin_settings.db_pool_max_connections as u64,
    );
    let ddl = format!(
        "CREATE DATABASE IF NOT EXISTS {}",
        crate::snapshot_sql::quote_mysql_identifier(&settings.database)
    );
    query(&ddl).execute(&*admin_pool).await?;
    // DedicatedPool::drop releases the global connection quota.
    Ok(())
}

fn validate_schema_identifier(raw: &str, kind: &str) -> Result<(), sqlx::Error> {
    use std::error::Error;

    crate::snapshot_sql::validate_sql_identifier(raw, kind).map_err(|e| {
        sqlx::Error::Configuration(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            e,
        )) as Box<dyn Error + Send + Sync>)
    })
}

const USER_IDENTITY_COLUMN_NAMES: [&str; 6] = [
    "user_id",
    "owner_user_id",
    "scope_user_id",
    "created_by",
    "updated_by",
    "username",
];

fn identity_column_widening_ddl(
    table: &str,
    column: &str,
    is_nullable: bool,
) -> Result<String, sqlx::Error> {
    validate_schema_identifier(table, "matrixone table")?;
    validate_schema_identifier(column, "matrixone column")?;
    let nullability = if is_nullable { "NULL" } else { "NOT NULL" };
    Ok(format!(
        "ALTER TABLE {} MODIFY COLUMN {} VARCHAR({USER_ID_MAX_LEN}) {nullability}",
        crate::snapshot_sql::quote_mysql_identifier(table),
        crate::snapshot_sql::quote_mysql_identifier(column),
    ))
}

/// Widens legacy identity columns to the repository-wide principal contract.
///
/// This is an explicit, idempotent schema migration. It never truncates data,
/// changes nullability, or rewrites UUID identifier columns.
async fn migrate_user_identity_column_widths(
    pool: &sqlx::Pool<MySql>,
    database: &str,
) -> Result<(), sqlx::Error> {
    validate_schema_identifier(database, "matrixone database")?;
    let rows = query(
        "SELECT TABLE_NAME, COLUMN_NAME, DATA_TYPE, CHARACTER_MAXIMUM_LENGTH, \
                IS_NULLABLE, COLUMN_DEFAULT \
         FROM information_schema.COLUMNS \
         WHERE TABLE_SCHEMA = ? \
           AND COLUMN_NAME IN \
               ('user_id', 'owner_user_id', 'scope_user_id', 'created_by', 'updated_by', 'username') \
         ORDER BY TABLE_NAME, COLUMN_NAME",
    )
    .bind(database)
    .fetch_all(pool)
    .await?;

    for row in rows {
        let table: String = row.try_get("TABLE_NAME")?;
        let column: String = row.try_get("COLUMN_NAME")?;
        let data_type: String = row.try_get("DATA_TYPE")?;
        let width: Option<i64> = row.try_get("CHARACTER_MAXIMUM_LENGTH")?;
        let is_nullable: String = row.try_get("IS_NULLABLE")?;
        let default_value: Option<String> = row.try_get("COLUMN_DEFAULT")?;

        if !USER_IDENTITY_COLUMN_NAMES.contains(&column.as_str()) {
            return Err(sqlx::Error::Protocol(format!(
                "identity column migration selected unexpected column {table}.{column}"
            )));
        }
        if !data_type.eq_ignore_ascii_case("varchar") {
            return Err(sqlx::Error::Protocol(format!(
                "identity column {table}.{column} must be VARCHAR, found {data_type}"
            )));
        }
        let width = width.ok_or_else(|| {
            sqlx::Error::Protocol(format!(
                "identity column {table}.{column} has no bounded VARCHAR width"
            ))
        })?;
        if width < 0 {
            return Err(sqlx::Error::Protocol(format!(
                "identity column {table}.{column} has invalid width {width}"
            )));
        }
        if width >= USER_ID_MAX_LEN as i64 {
            continue;
        }
        if default_value.is_some() {
            return Err(sqlx::Error::Protocol(format!(
                "identity column {table}.{column} has a default value that must be preserved by an explicit migration"
            )));
        }
        let is_nullable = match is_nullable.as_str() {
            "YES" => true,
            "NO" => false,
            value => {
                return Err(sqlx::Error::Protocol(format!(
                    "identity column {table}.{column} has invalid IS_NULLABLE value {value}"
                )));
            }
        };
        let ddl = identity_column_widening_ddl(&table, &column, is_nullable)?;
        query(&ddl).execute(pool).await?;
        tracing::info!(
            table,
            column,
            previous_width = width,
            new_width = USER_ID_MAX_LEN,
            "widened legacy user identity column"
        );
    }

    Ok(())
}

async fn add_column_if_missing(
    pool: &sqlx::Pool<MySql>,
    database: &str,
    table: &str,
    column: &str,
    ddl: &str,
) -> Result<(), sqlx::Error> {
    validate_schema_identifier(database, "matrixone database")?;
    validate_schema_identifier(table, "matrixone table")?;
    validate_schema_identifier(column, "matrixone column")?;

    let exists = query(
        "SELECT 1 FROM information_schema.COLUMNS \
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND COLUMN_NAME = ? LIMIT 1",
    )
    .bind(database)
    .bind(table)
    .bind(column)
    .fetch_optional(pool)
    .await?
    .is_some();
    if exists {
        return Ok(());
    }

    query(ddl).execute(pool).await?;
    Ok(())
}

async fn add_index_if_missing(
    pool: &sqlx::Pool<MySql>,
    database: &str,
    table: &str,
    index: &str,
    ddl: &str,
) -> Result<(), sqlx::Error> {
    validate_schema_identifier(database, "matrixone database")?;
    validate_schema_identifier(table, "matrixone table")?;
    validate_schema_identifier(index, "matrixone index")?;

    let exists = query(
        "SELECT 1 FROM information_schema.STATISTICS \
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND INDEX_NAME = ? LIMIT 1",
    )
    .bind(database)
    .bind(table)
    .bind(index)
    .fetch_optional(pool)
    .await?
    .is_some();
    if exists {
        return Ok(());
    }

    query(ddl).execute(pool).await?;
    Ok(())
}

async fn existing_table_columns(
    pool: &sqlx::Pool<MySql>,
    database: &str,
    table: &str,
) -> Result<BTreeSet<String>, sqlx::Error> {
    validate_schema_identifier(database, "matrixone database")?;
    validate_schema_identifier(table, "matrixone table")?;
    query(
        "SELECT COLUMN_NAME FROM information_schema.COLUMNS \
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
    )
    .bind(database)
    .bind(table)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|row| row.try_get::<String, _>("COLUMN_NAME"))
    .collect::<Result<BTreeSet<_>, _>>()
}

async fn table_exists(
    pool: &sqlx::Pool<MySql>,
    database: &str,
    table: &str,
) -> Result<bool, sqlx::Error> {
    validate_schema_identifier(database, "matrixone database")?;
    validate_schema_identifier(table, "matrixone table")?;
    query(
        "SELECT 1 FROM information_schema.TABLES \
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? LIMIT 1",
    )
    .bind(database)
    .bind(table)
    .fetch_optional(pool)
    .await
    .map(|row| row.is_some())
}

async fn fail_if_obsolete_shape(
    pool: &sqlx::Pool<MySql>,
    database: &str,
    table: &str,
    required_columns: &[&str],
    obsolete_columns: &[&str],
    obsolete_indexes: &[&str],
) -> Result<(), sqlx::Error> {
    validate_schema_identifier(database, "matrixone database")?;
    validate_schema_identifier(table, "matrixone table")?;
    for column in required_columns {
        validate_schema_identifier(column, "matrixone column")?;
    }
    for column in obsolete_columns {
        validate_schema_identifier(column, "matrixone column")?;
    }
    for index in obsolete_indexes {
        validate_schema_identifier(index, "matrixone index")?;
    }

    let columns = existing_table_columns(pool, database, table).await?;
    if columns.is_empty() {
        return Ok(());
    }

    let index_rows = query(
        "SELECT DISTINCT INDEX_NAME FROM information_schema.STATISTICS \
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
    )
    .bind(database)
    .bind(table)
    .fetch_all(pool)
    .await?;
    let indexes = index_rows
        .into_iter()
        .map(|row| row.try_get::<String, _>("INDEX_NAME"))
        .collect::<Result<BTreeSet<_>, _>>()?;

    let mut reasons = Vec::new();
    for column in required_columns {
        if !columns.contains(*column) {
            reasons.push(format!("missing column {column}"));
        }
    }
    for column in obsolete_columns {
        if columns.contains(*column) {
            reasons.push(format!("obsolete column {column}"));
        }
    }
    for index in obsolete_indexes {
        if indexes.contains(*index) {
            reasons.push(format!("obsolete index {index}"));
        }
    }
    if reasons.is_empty() {
        return Ok(());
    }

    Err(sqlx::Error::Protocol(format!(
        "obsolete core schema table {table} requires manual migration before startup: {}",
        reasons.join(", ")
    )))
}

fn is_legacy_edge_pending_dispatch_shape(
    columns: &BTreeSet<String>,
    primary_key: &[String],
) -> bool {
    EDGE_PENDING_DISPATCH_LEGACY_COLUMNS
        .iter()
        .all(|column| columns.contains(*column))
        && EDGE_PENDING_DISPATCH_IDENTITY_COLUMNS
            .iter()
            .skip(1)
            .take(3)
            .all(|column| !columns.contains(*column))
        && primary_key
            .iter()
            .map(String::as_str)
            .eq(EDGE_PENDING_DISPATCH_LEGACY_PRIMARY_KEY.iter().copied())
}

async fn migrate_legacy_edge_pending_dispatch_if_needed(
    pool: &sqlx::Pool<MySql>,
    database: &str,
) -> Result<(), sqlx::Error> {
    let columns = existing_table_columns(pool, database, "edge_pending_dispatch").await?;
    if columns.is_empty() {
        return Ok(());
    }
    let primary_key =
        existing_index_columns(pool, database, "edge_pending_dispatch", "PRIMARY").await?;
    if !is_legacy_edge_pending_dispatch_shape(&columns, &primary_key) {
        return Ok(());
    }
    if table_exists(pool, database, EDGE_PENDING_DISPATCH_LEGACY_ARCHIVE_TABLE).await? {
        return Err(sqlx::Error::Protocol(format!(
            "legacy edge_pending_dispatch migration archive {EDGE_PENDING_DISPATCH_LEGACY_ARCHIVE_TABLE} already exists; inspect the previous migration before startup"
        )));
    }

    let active_row = query(
        "SELECT 1 AS active_row FROM edge_pending_dispatch \
         WHERE status IN ('pending', 'dispatched') LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;
    if active_row.is_some() {
        return Err(sqlx::Error::Protocol(
            "legacy edge_pending_dispatch contains active rows without session/run/turn identity; drain them with the pre-turn-scoped Astra release before upgrade"
                .to_string(),
        ));
    }
    query(&format!(
        "RENAME TABLE {} TO {}",
        crate::snapshot_sql::quote_mysql_identifier("edge_pending_dispatch"),
        crate::snapshot_sql::quote_mysql_identifier(EDGE_PENDING_DISPATCH_LEGACY_ARCHIVE_TABLE),
    ))
    .execute(pool)
    .await?;
    tracing::info!(
        legacy_table = "edge_pending_dispatch",
        archive_table = EDGE_PENDING_DISPATCH_LEGACY_ARCHIVE_TABLE,
        "archived terminal legacy edge dispatch rows before turn-scoped schema creation"
    );
    Ok(())
}

async fn fail_if_required_columns_missing_or_nullable(
    pool: &sqlx::Pool<MySql>,
    database: &str,
    table: &str,
    required_not_null_columns: &[&str],
) -> Result<(), sqlx::Error> {
    validate_schema_identifier(database, "matrixone database")?;
    validate_schema_identifier(table, "matrixone table")?;
    for column in required_not_null_columns {
        validate_schema_identifier(column, "matrixone column")?;
    }

    let rows = query(
        "SELECT COLUMN_NAME, IS_NULLABLE FROM information_schema.COLUMNS \
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
    )
    .bind(database)
    .bind(table)
    .fetch_all(pool)
    .await?;
    if rows.is_empty() {
        return Ok(());
    }

    let mut present_columns = BTreeSet::new();
    let mut nullable_columns = Vec::new();
    for row in rows {
        let column: String = row.try_get("COLUMN_NAME")?;
        let is_nullable: String = row.try_get("IS_NULLABLE")?;
        present_columns.insert(column.clone());
        if required_not_null_columns.contains(&column.as_str()) && is_nullable == "YES" {
            nullable_columns.push(column);
        }
    }
    let missing_columns = required_not_null_columns
        .iter()
        .copied()
        .filter(|column| !present_columns.contains(*column))
        .collect::<Vec<_>>();
    if missing_columns.is_empty() && nullable_columns.is_empty() {
        return Ok(());
    }

    let mut reasons = Vec::new();
    reasons.extend(
        missing_columns
            .into_iter()
            .map(|column| format!("missing NOT NULL column {column}")),
    );
    reasons.extend(
        nullable_columns
            .into_iter()
            .map(|column| format!("nullable owner column {column}")),
    );
    Err(sqlx::Error::Protocol(format!(
        "obsolete core schema table {table} requires manual migration before startup: {}",
        reasons.join(", ")
    )))
}

async fn fail_if_varchar_columns_shorter_than(
    pool: &sqlx::Pool<MySql>,
    database: &str,
    table: &str,
    required_widths: &[(&str, u64)],
) -> Result<(), sqlx::Error> {
    validate_schema_identifier(database, "matrixone database")?;
    validate_schema_identifier(table, "matrixone table")?;
    for (column, _) in required_widths {
        validate_schema_identifier(column, "matrixone column")?;
    }

    let rows = query(
        "SELECT COLUMN_NAME, CHARACTER_MAXIMUM_LENGTH FROM information_schema.COLUMNS \
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
    )
    .bind(database)
    .bind(table)
    .fetch_all(pool)
    .await?;
    if rows.is_empty() {
        return Ok(());
    }

    let required_widths = required_widths.iter().copied().collect::<HashMap<_, _>>();
    let mut reasons = Vec::new();
    for row in rows {
        let column: String = row.try_get("COLUMN_NAME")?;
        let Some(required_width) = required_widths.get(column.as_str()).copied() else {
            continue;
        };
        match row.try_get::<Option<i64>, _>("CHARACTER_MAXIMUM_LENGTH")? {
            Some(actual_width) if actual_width < 0 => reasons.push(format!(
                "column {column} has invalid negative width {actual_width}"
            )),
            Some(actual_width) => {
                let actual_width = u64::try_from(actual_width).map_err(|_| {
                    sqlx::Error::Protocol(format!(
                        "column {column} width {actual_width} cannot be represented as u64"
                    ))
                })?;
                if actual_width < required_width {
                    reasons.push(format!(
                        "column {column} width {actual_width} below {required_width}"
                    ));
                }
            }
            None => reasons.push(format!("column {column} is not a bounded varchar")),
        }
    }

    if reasons.is_empty() {
        return Ok(());
    }

    Err(sqlx::Error::Protocol(format!(
        "obsolete core schema table {table} requires manual migration before startup: {}",
        reasons.join(", ")
    )))
}

async fn existing_index_columns(
    pool: &sqlx::Pool<MySql>,
    database: &str,
    table: &str,
    index: &str,
) -> Result<Vec<String>, sqlx::Error> {
    validate_schema_identifier(database, "matrixone database")?;
    validate_schema_identifier(table, "matrixone table")?;
    validate_schema_identifier(index, "matrixone index")?;

    let rows = query(
        "SELECT COLUMN_NAME FROM information_schema.STATISTICS \
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND INDEX_NAME = ? \
         ORDER BY SEQ_IN_INDEX",
    )
    .bind(database)
    .bind(table)
    .bind(index)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| row.try_get::<String, _>("COLUMN_NAME"))
        .collect::<Result<Vec<_>, _>>()
}

async fn drop_index_if_present(
    pool: &sqlx::Pool<MySql>,
    database: &str,
    table: &str,
    index: &str,
) -> Result<(), sqlx::Error> {
    let columns = existing_index_columns(pool, database, table, index).await?;
    if columns.is_empty() {
        return Ok(());
    }
    let ddl = format!(
        "ALTER TABLE {} DROP INDEX {}",
        crate::snapshot_sql::quote_mysql_identifier(table),
        crate::snapshot_sql::quote_mysql_identifier(index)
    );
    query(&ddl).execute(pool).await?;
    Ok(())
}

async fn ensure_index_shape(
    pool: &sqlx::Pool<MySql>,
    database: &str,
    table: &str,
    index: &str,
    expected_columns: &[&str],
    ddl: &str,
) -> Result<(), sqlx::Error> {
    for column in expected_columns {
        validate_schema_identifier(column, "matrixone column")?;
    }
    let existing = existing_index_columns(pool, database, table, index).await?;
    if existing
        .iter()
        .map(String::as_str)
        .eq(expected_columns.iter().copied())
    {
        return Ok(());
    }
    if !existing.is_empty() {
        return Err(sqlx::Error::Protocol(format!(
            "index shape mismatch for {table}.{index}: existing ({}) != expected ({})",
            existing.join(", "),
            expected_columns.join(", ")
        )));
    }
    query(ddl).execute(pool).await?;
    Ok(())
}

async fn ensure_primary_key_shape(
    pool: &sqlx::Pool<MySql>,
    database: &str,
    table: &str,
    expected_columns: &[&str],
    ddl: &str,
) -> Result<(), sqlx::Error> {
    for column in expected_columns {
        validate_schema_identifier(column, "matrixone column")?;
    }
    let existing = existing_index_columns(pool, database, table, "PRIMARY").await?;
    if existing
        .iter()
        .map(String::as_str)
        .eq(expected_columns.iter().copied())
    {
        return Ok(());
    }
    if !existing.is_empty() {
        return Err(sqlx::Error::Protocol(format!(
            "primary key shape mismatch for {table}: existing ({}) != expected ({})",
            existing.join(", "),
            expected_columns.join(", ")
        )));
    }
    query(ddl).execute(pool).await?;
    Ok(())
}

pub async fn ensure_core_schema(
    settings: &MatrixOneSettings,
    bootstrap_catalog: &str,
) -> Result<(), sqlx::Error> {
    // Tests and startup paths can race on schema bootstrap inside the same process.
    // Serialize schema setup so version markers and DDL stay idempotent.
    let _init_guard = CORE_SCHEMA_INIT_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let _file_lock = acquire_core_schema_file_lock_blocking(settings.clone()).await?;

    if std::env::var("ASTRA_AUTO_CREATE_DATABASE")
        .map(|v| v == "1")
        .unwrap_or(false)
    {
        ensure_matrixone_database_exists(settings, bootstrap_catalog).await?;
    }
    let pool = connect_matrixone(settings).await?;

    // Existing deployments may still have UUID-sized identity columns. Widen
    // them before table-specific shape checks run so every persistence path
    // observes the same principal contract during startup.
    migrate_user_identity_column_widths(&pool, &settings.database).await?;

    // Auth
    query(
        "CREATE TABLE IF NOT EXISTS auth_users (
            user_id VARCHAR(128) PRIMARY KEY,
            username VARCHAR(128) NOT NULL UNIQUE,
            email VARCHAR(255) NOT NULL UNIQUE,
            password_hash VARCHAR(255) NOT NULL,
            display_name VARCHAR(100) NULL,
            is_active SMALLINT NOT NULL DEFAULT 1,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            last_login_at DATETIME(6) NULL
        )",
    )
    .execute(&pool)
    .await?;
    query(
        "CREATE TABLE IF NOT EXISTS auth_roles (
            role_id VARCHAR(64) PRIMARY KEY,
            role_name VARCHAR(50) NOT NULL UNIQUE,
            description VARCHAR(255) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS auth_user_roles (
            user_id VARCHAR(128) NOT NULL,
            role_id VARCHAR(64) NOT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, role_id),
            INDEX idx_auth_user_roles_role_id (role_id)
        )",
    )
    .execute(&pool)
    .await?;
    fail_if_obsolete_shape(
        &pool,
        &settings.database,
        "auth_user_roles",
        &["user_id", "role_id"],
        &["id"],
        &["uq_auth_user_roles_user_role"],
    )
    .await?;
    drop_index_if_present(
        &pool,
        &settings.database,
        "auth_user_roles",
        "idx_auth_user_roles_user_id",
    )
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "auth_user_roles",
        &["user_id", "role_id"],
        "ALTER TABLE auth_user_roles ADD PRIMARY KEY (user_id, role_id)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "auth_user_roles",
        "idx_auth_user_roles_role_id",
        &["role_id"],
        "ALTER TABLE auth_user_roles ADD INDEX idx_auth_user_roles_role_id (role_id)",
    )
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS auth_refresh_tokens (
            token_id VARCHAR(64) PRIMARY KEY,
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(64) NULL,
            token_hash VARCHAR(255) NOT NULL,
            token_prefix VARCHAR(16) NULL,
            expires_at DATETIME(6) NOT NULL,
            is_revoked SMALLINT NOT NULL DEFAULT 0,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            UNIQUE KEY uq_auth_refresh_tokens_hash (token_hash),
            INDEX idx_auth_refresh_tokens_session (session_id),
            INDEX idx_auth_refresh_tokens_user_expires (user_id, expires_at),
            INDEX idx_auth_refresh_tokens_expires_at (expires_at),
            INDEX idx_auth_refresh_tokens_prefix (token_prefix)
        )",
    )
    .execute(&pool)
    .await?;

    add_column_if_missing(
        &pool,
        &settings.database,
        "auth_refresh_tokens",
        "session_id",
        "ALTER TABLE auth_refresh_tokens ADD COLUMN session_id VARCHAR(64) NULL",
    )
    .await?;
    add_index_if_missing(
        &pool,
        &settings.database,
        "auth_refresh_tokens",
        "idx_auth_refresh_tokens_session",
        "ALTER TABLE auth_refresh_tokens ADD INDEX idx_auth_refresh_tokens_session (session_id)",
    )
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS auth_tokens (
            token_id VARCHAR(64) PRIMARY KEY,
            type VARCHAR(50) NOT NULL,
            provider VARCHAR(50) NOT NULL,
            encrypted_value TEXT NULL,
            secret_ref VARCHAR(255) NULL,
            is_active SMALLINT NOT NULL DEFAULT 1,
            scope_user_id VARCHAR(128) NULL,
            scope_repo VARCHAR(255) NULL,
            metadata JSON NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_auth_tokens_active (is_active),
            INDEX idx_auth_tokens_scope_user (scope_user_id),
            INDEX idx_auth_tokens_scope_repo (scope_repo)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS auth_provider_request_replay (
            provider VARCHAR(64) NOT NULL,
            request_authorization_id VARCHAR(512) NOT NULL,
            external_subject VARCHAR(255) NOT NULL,
            request_id VARCHAR(255) NOT NULL,
            expires_at_unix BIGINT NOT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (provider, request_authorization_id),
            INDEX idx_auth_provider_request_replay_expires (expires_at_unix, provider, request_authorization_id),
            INDEX idx_auth_provider_request_replay_request (provider, request_id)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS auth_audit_logs (
            log_id VARCHAR(64) PRIMARY KEY,
            user_id VARCHAR(128) NOT NULL,
            action VARCHAR(50) NOT NULL,
            resource_type VARCHAR(50) NULL,
            resource_id VARCHAR(64) NULL,
            details JSON NULL,
            ip_address VARCHAR(45) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_auth_audit_logs_user_created (user_id, created_at),
            INDEX idx_auth_audit_logs_user_resource_created (user_id, resource_type, resource_id, created_at)
        )",
    )
    .execute(&pool)
    .await?;

    // Sessions / events core
    query(
        "CREATE TABLE IF NOT EXISTS agent_sessions (
            session_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            agent_id VARCHAR(255) NULL,
            title VARCHAR(255) NULL,
            status VARCHAR(20) NOT NULL DEFAULT 'active',
            event_count BIGINT NOT NULL DEFAULT 0,
            last_event_id VARCHAR(128) NULL,
            summary_status VARCHAR(20) NULL,
            summary_job_id VARCHAR(64) NULL,
            vector_db_snapshot_id VARCHAR(64) NULL,
            metadata JSON NULL,
            project_id VARCHAR(128) NULL,
            project_retention_policy VARCHAR(32) NOT NULL DEFAULT 'session',
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            ended_at DATETIME(6) NULL,
            last_active_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            active_plan_id VARCHAR(64) NULL,
            config_version_id VARCHAR(24) NULL,
            PRIMARY KEY (user_id, session_id),
            INDEX idx_agent_sessions_user_status_updated (user_id, status, updated_at),
            INDEX idx_agent_sessions_user_last_active (user_id, last_active_at),
            INDEX idx_agent_sessions_agent_status (agent_id, status),
            INDEX idx_agent_sessions_active_plan_id (active_plan_id),
            INDEX idx_agent_sessions_config_version (config_version_id),
            INDEX idx_sessions_project (user_id, project_id, updated_at)
        )",
    )
    .execute(&pool)
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "agent_sessions",
        &["user_id", "session_id"],
        "ALTER TABLE agent_sessions ADD PRIMARY KEY (user_id, session_id)",
    )
    .await?;
    fail_if_varchar_columns_shorter_than(
        &pool,
        &settings.database,
        "agent_sessions",
        &[("last_event_id", AGENT_EVENT_ID_LEN as u64)],
    )
    .await?;

    let agent_events_sql = agent_events_create_sql();
    query(&agent_events_sql).execute(&pool).await?;
    fail_if_varchar_columns_shorter_than(
        &pool,
        &settings.database,
        "agent_events",
        &[
            ("event_id", AGENT_EVENT_ID_LEN as u64),
            ("parent_event_id", AGENT_EVENT_ID_LEN as u64),
            ("causal_chain_id", AGENT_EVENT_ID_LEN as u64),
        ],
    )
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "agent_events",
        &["user_id", "event_id"],
        "ALTER TABLE agent_events ADD PRIMARY KEY (user_id, event_id)",
    )
    .await?;
    for removed_index in [
        "idx_agent_events_session_created",
        "idx_agent_events_session_type_created",
        "idx_agent_events_session_model_created",
        "idx_agent_events_session_parent",
        "idx_agent_events_causal_chain_id",
    ] {
        drop_index_if_present(&pool, &settings.database, "agent_events", removed_index).await?;
    }
    for (index, expected_columns, ddl) in [
        (
            "idx_agent_events_owner_session_created",
            &["user_id", "session_id", "created_at"][..],
            "ALTER TABLE agent_events ADD INDEX idx_agent_events_owner_session_created (user_id, session_id, created_at)",
        ),
        (
            "idx_agent_events_owner_session_type_created",
            &["user_id", "session_id", "event_type", "created_at"][..],
            "ALTER TABLE agent_events ADD INDEX idx_agent_events_owner_session_type_created (user_id, session_id, event_type, created_at)",
        ),
        (
            "idx_agent_events_owner_session_model_created",
            &["user_id", "session_id", "llm_model_used", "created_at"][..],
            "ALTER TABLE agent_events ADD INDEX idx_agent_events_owner_session_model_created (user_id, session_id, llm_model_used, created_at DESC)",
        ),
        (
            "idx_agent_events_owner_session_parent",
            &["user_id", "session_id", "parent_event_id"][..],
            "ALTER TABLE agent_events ADD INDEX idx_agent_events_owner_session_parent (user_id, session_id, parent_event_id)",
        ),
        (
            "idx_agent_events_owner_causal_chain_created",
            &["user_id", "causal_chain_id", "created_at", "event_id"][..],
            "ALTER TABLE agent_events ADD INDEX idx_agent_events_owner_causal_chain_created (user_id, causal_chain_id, created_at, event_id)",
        ),
        (
            "idx_agent_events_trace",
            &["user_id", "session_id", "turn_id", "created_at"][..],
            "ALTER TABLE agent_events ADD INDEX idx_agent_events_trace (user_id, session_id, turn_id, created_at)",
        ),
        (
            "idx_agent_events_run",
            &["user_id", "session_id", "run_id", "created_at"][..],
            "ALTER TABLE agent_events ADD INDEX idx_agent_events_run (user_id, session_id, run_id, created_at)",
        ),
        (
            "idx_agent_events_parent_run",
            &["user_id", "session_id", "parent_run_id", "created_at"][..],
            "ALTER TABLE agent_events ADD INDEX idx_agent_events_parent_run (user_id, session_id, parent_run_id, created_at)",
        ),
        (
            "idx_agent_events_tool_call",
            &["user_id", "session_id", "tool_call_id"][..],
            "ALTER TABLE agent_events ADD INDEX idx_agent_events_tool_call (user_id, session_id, tool_call_id)",
        ),
        (
            "idx_agent_events_owner_session_turn",
            &["user_id", "session_id", "turn_seq"][..],
            AGENT_EVENTS_OWNER_SESSION_TURN_INDEX_ALTER_SQL,
        ),
    ] {
        ensure_index_shape(
            &pool,
            &settings.database,
            "agent_events",
            index,
            expected_columns,
            ddl,
        )
        .await?;
    }
    crate::workspace_records::ensure_workspace_record_tables(&pool).await?;

    // ── Durable web-agent run state (Phase 1 / G15 + G19) ────────────────
    query(
        "CREATE TABLE IF NOT EXISTS agent_runs (
            run_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(64) NOT NULL,
            parent_run_id VARCHAR(64) NULL,
            root_run_id VARCHAR(64) NOT NULL,
            ancestor_path VARCHAR(2048) NOT NULL,
            depth INT NOT NULL DEFAULT 0,
            delegation_id VARCHAR(64) NULL,
            agent_id VARCHAR(255) NULL,
            retry_of VARCHAR(64) NULL,
            retry_scope VARCHAR(16) NOT NULL DEFAULT 'node',
            status VARCHAR(32) NOT NULL,
            execution_mode VARCHAR(32) NOT NULL DEFAULT 'web_agent',
            trigger_type VARCHAR(64) NULL,
            trigger_event_id VARCHAR(128) NULL,
            waiting_for VARCHAR(64) NULL,
            owner_pod_id VARCHAR(128) NULL,
            owner_lease_expires_at DATETIME(6) NULL,
            run_generation BIGINT NOT NULL DEFAULT 0,
            last_event_idx BIGINT NOT NULL DEFAULT -1,
            checkpoint_version VARCHAR(32) NULL,
            checkpoint_json LONGTEXT NULL,
            error_code VARCHAR(128) NULL,
            error_message TEXT NULL,
            retry_count INT NOT NULL DEFAULT 0,
            total_prompt_tokens BIGINT NOT NULL DEFAULT 0,
            total_completion_tokens BIGINT NOT NULL DEFAULT 0,
            total_tool_calls BIGINT NOT NULL DEFAULT 0,
            request_id VARCHAR(64) NULL,
            trace_id VARCHAR(64) NULL,
            agent_binding_id VARCHAR(64) NULL,
            agent_binding_name VARCHAR(255) NULL,
            agent_binding_schema_version VARCHAR(32) NULL,
            selected_model_json LONGTEXT NULL,
            selected_model_name VARCHAR(255) NULL,
            selected_model_gateway VARCHAR(128) NULL,
            capability_server_refs_json LONGTEXT NULL,
            runtime_profile VARCHAR(64) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            CONSTRAINT chk_agent_runs_retry_scope CHECK (retry_scope IN ('node', 'subtree', 'siblings')),
            PRIMARY KEY (user_id, run_id),
            INDEX idx_agent_runs_user_updated_run (user_id, updated_at, run_id),
            INDEX idx_agent_runs_user_session_status_updated (user_id, session_id, status, updated_at),
            INDEX idx_agent_runs_owner_root_depth (user_id, root_run_id, depth, created_at),
            INDEX idx_agent_runs_owner_parent_status_updated (user_id, parent_run_id, status, updated_at),
            INDEX idx_agent_runs_owner_retry_of (user_id, retry_of),
            INDEX idx_agent_runs_recovery_scan (status, owner_lease_expires_at, updated_at, user_id, run_id),
            INDEX idx_agent_runs_owner_lease (owner_pod_id, owner_lease_expires_at),
            INDEX idx_agent_runs_binding (agent_binding_id, created_at),
            INDEX idx_agent_runs_model_gateway (selected_model_gateway, created_at)
        )",
    )
    .execute(&pool)
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "agent_runs",
        &["user_id", "run_id"],
        "ALTER TABLE agent_runs ADD PRIMARY KEY (user_id, run_id)",
    )
    .await?;
    fail_if_varchar_columns_shorter_than(
        &pool,
        &settings.database,
        "agent_runs",
        &[("trigger_event_id", AGENT_EVENT_ID_LEN as u64)],
    )
    .await?;
    for removed_index in [
        "idx_agent_runs_session_updated",
        "idx_agent_runs_root_depth",
        "idx_agent_runs_parent",
        "idx_agent_runs_retry_of",
        "idx_agent_runs_user_updated",
        "idx_agent_runs_status_lease",
    ] {
        drop_index_if_present(&pool, &settings.database, "agent_runs", removed_index).await?;
    }
    for (index, expected_columns, ddl) in [
        (
            "idx_agent_runs_user_updated_run",
            &["user_id", "updated_at", "run_id"][..],
            "ALTER TABLE agent_runs ADD INDEX idx_agent_runs_user_updated_run (user_id, updated_at, run_id)",
        ),
        (
            "idx_agent_runs_user_session_status_updated",
            &["user_id", "session_id", "status", "updated_at"][..],
            "ALTER TABLE agent_runs ADD INDEX idx_agent_runs_user_session_status_updated (user_id, session_id, status, updated_at)",
        ),
        (
            "idx_agent_runs_owner_root_depth",
            &["user_id", "root_run_id", "depth", "created_at"][..],
            "ALTER TABLE agent_runs ADD INDEX idx_agent_runs_owner_root_depth (user_id, root_run_id, depth, created_at)",
        ),
        (
            "idx_agent_runs_owner_parent_status_updated",
            &["user_id", "parent_run_id", "status", "updated_at"][..],
            "ALTER TABLE agent_runs ADD INDEX idx_agent_runs_owner_parent_status_updated (user_id, parent_run_id, status, updated_at)",
        ),
        (
            "idx_agent_runs_owner_retry_of",
            &["user_id", "retry_of"][..],
            "ALTER TABLE agent_runs ADD INDEX idx_agent_runs_owner_retry_of (user_id, retry_of)",
        ),
        (
            "idx_agent_runs_recovery_scan",
            &[
                "status",
                "owner_lease_expires_at",
                "updated_at",
                "user_id",
                "run_id",
            ][..],
            "ALTER TABLE agent_runs ADD INDEX idx_agent_runs_recovery_scan (status, owner_lease_expires_at, updated_at, user_id, run_id)",
        ),
    ] {
        ensure_index_shape(
            &pool,
            &settings.database,
            "agent_runs",
            index,
            expected_columns,
            ddl,
        )
        .await?;
    }

    for (column, ddl) in [
        (
            "agent_binding_id",
            "ALTER TABLE agent_runs ADD COLUMN agent_binding_id VARCHAR(64) NULL",
        ),
        (
            "agent_binding_name",
            "ALTER TABLE agent_runs ADD COLUMN agent_binding_name VARCHAR(255) NULL",
        ),
        (
            "agent_binding_schema_version",
            "ALTER TABLE agent_runs ADD COLUMN agent_binding_schema_version VARCHAR(32) NULL",
        ),
        (
            "selected_model_json",
            "ALTER TABLE agent_runs ADD COLUMN selected_model_json LONGTEXT NULL",
        ),
        (
            "selected_model_name",
            "ALTER TABLE agent_runs ADD COLUMN selected_model_name VARCHAR(255) NULL",
        ),
        (
            "selected_model_gateway",
            "ALTER TABLE agent_runs ADD COLUMN selected_model_gateway VARCHAR(128) NULL",
        ),
        (
            "capability_server_refs_json",
            "ALTER TABLE agent_runs ADD COLUMN capability_server_refs_json LONGTEXT NULL",
        ),
        (
            "runtime_profile",
            "ALTER TABLE agent_runs ADD COLUMN runtime_profile VARCHAR(64) NULL",
        ),
    ] {
        add_column_if_missing(&pool, &settings.database, "agent_runs", column, ddl).await?;
    }

    for (index, ddl) in [
        (
            "idx_agent_runs_binding",
            "ALTER TABLE agent_runs ADD INDEX idx_agent_runs_binding (agent_binding_id, created_at)",
        ),
        (
            "idx_agent_runs_model_gateway",
            "ALTER TABLE agent_runs ADD INDEX idx_agent_runs_model_gateway (selected_model_gateway, created_at)",
        ),
    ] {
        add_index_if_missing(&pool, &settings.database, "agent_runs", index, ddl).await?;
    }

    query(
        "CREATE TABLE IF NOT EXISTS agent_session_execution_slots (
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(64) NOT NULL,
            run_id VARCHAR(64) NOT NULL,
            acquired_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, session_id),
            INDEX idx_session_execution_slots_run (user_id, run_id),
            INDEX idx_session_execution_slots_updated (updated_at)
        )",
    )
    .execute(&pool)
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "agent_session_execution_slots",
        &["user_id", "session_id"],
        "ALTER TABLE agent_session_execution_slots ADD PRIMARY KEY (user_id, session_id)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "agent_session_execution_slots",
        "idx_session_execution_slots_run",
        &["user_id", "run_id"],
        "ALTER TABLE agent_session_execution_slots ADD INDEX idx_session_execution_slots_run (user_id, run_id)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "agent_session_execution_slots",
        "idx_session_execution_slots_updated",
        &["updated_at"],
        "ALTER TABLE agent_session_execution_slots ADD INDEX idx_session_execution_slots_updated (updated_at)",
    )
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS agent_run_events (
            id VARCHAR(64) NOT NULL,
            run_id VARCHAR(64) NOT NULL,
            event_idx BIGINT NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(64) NOT NULL,
            event_type VARCHAR(64) NOT NULL,
            event_id VARCHAR(128) NOT NULL,
            agent_id VARCHAR(255) NULL,
            subject_run_id VARCHAR(64) NULL,
            interaction_request_id VARCHAR(128) NULL,
            idempotency_key VARCHAR(128) NULL,
            event_hash VARCHAR(64) NOT NULL,
            producer_pod_id VARCHAR(128) NULL,
            payload_json LONGTEXT NOT NULL,
            request_id VARCHAR(64) NULL,
            trace_id VARCHAR(64) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, id),
            UNIQUE KEY uq_run_event_idx (user_id, run_id, event_idx),
            UNIQUE KEY uq_run_event_idempotency (user_id, run_id, idempotency_key),
            INDEX idx_agent_run_events_owner_session_run_idx (user_id, session_id, run_id, event_idx),
            INDEX idx_agent_run_events_owner_session_subject (user_id, session_id, event_type, subject_run_id, event_idx),
            INDEX idx_agent_run_events_interaction (user_id, run_id, interaction_request_id, event_type, event_idx),
            INDEX idx_agent_run_events_user_created (user_id, created_at),
            INDEX idx_agent_run_events_event_id (event_id)
        )",
    )
    .execute(&pool)
    .await?;
    add_column_if_missing(
        &pool,
        &settings.database,
        "agent_run_events",
        "subject_run_id",
        "ALTER TABLE agent_run_events ADD COLUMN subject_run_id VARCHAR(64) NULL",
    )
    .await?;
    add_column_if_missing(
        &pool,
        &settings.database,
        "agent_run_events",
        "interaction_request_id",
        "ALTER TABLE agent_run_events ADD COLUMN interaction_request_id VARCHAR(128) NULL",
    )
    .await?;
    for (index, expected_columns, ddl) in [
        (
            "uq_run_event_idx",
            &["user_id", "run_id", "event_idx"][..],
            "ALTER TABLE agent_run_events ADD UNIQUE KEY uq_run_event_idx (user_id, run_id, event_idx)",
        ),
        (
            "uq_run_event_idempotency",
            &["user_id", "run_id", "idempotency_key"][..],
            "ALTER TABLE agent_run_events ADD UNIQUE KEY uq_run_event_idempotency (user_id, run_id, idempotency_key)",
        ),
        (
            "idx_agent_run_events_owner_session_run_idx",
            &["user_id", "session_id", "run_id", "event_idx"][..],
            "ALTER TABLE agent_run_events ADD INDEX idx_agent_run_events_owner_session_run_idx (user_id, session_id, run_id, event_idx)",
        ),
        (
            "idx_agent_run_events_owner_session_subject",
            &[
                "user_id",
                "session_id",
                "event_type",
                "subject_run_id",
                "event_idx",
            ][..],
            "ALTER TABLE agent_run_events ADD INDEX idx_agent_run_events_owner_session_subject (user_id, session_id, event_type, subject_run_id, event_idx)",
        ),
        (
            "idx_agent_run_events_interaction",
            &[
                "user_id",
                "run_id",
                "interaction_request_id",
                "event_type",
                "event_idx",
            ][..],
            "ALTER TABLE agent_run_events ADD INDEX idx_agent_run_events_interaction (user_id, run_id, interaction_request_id, event_type, event_idx)",
        ),
    ] {
        ensure_index_shape(
            &pool,
            &settings.database,
            "agent_run_events",
            index,
            expected_columns,
            ddl,
        )
        .await?;
    }
    for removed_index in [
        "idx_agent_run_events_run_created",
        "idx_agent_run_events_session_created",
    ] {
        drop_index_if_present(&pool, &settings.database, "agent_run_events", removed_index).await?;
    }
    query(
        "CREATE TABLE IF NOT EXISTS run_checkpoints (
            checkpoint_id VARCHAR(64) NOT NULL,
            run_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(64) NOT NULL,
            node_seq BIGINT NOT NULL DEFAULT 0,
            checkpoint_kind VARCHAR(32) NOT NULL,
            checkpoint_version VARCHAR(32) NOT NULL,
            idempotency_key VARCHAR(191) NOT NULL,
            checkpoint_json LONGTEXT NOT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, checkpoint_id),
            UNIQUE KEY uniq_run_checkpoint_idem (user_id, run_id, checkpoint_kind, idempotency_key),
            INDEX idx_run_checkpoints_user_run_created (user_id, run_id, created_at),
            INDEX idx_run_checkpoints_session_kind_created (user_id, session_id, checkpoint_kind, created_at)
        )",
    )
    .execute(&pool)
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "run_checkpoints",
        &["user_id", "checkpoint_id"],
        "ALTER TABLE run_checkpoints ADD PRIMARY KEY (user_id, checkpoint_id)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "run_checkpoints",
        "uniq_run_checkpoint_idem",
        &["user_id", "run_id", "checkpoint_kind", "idempotency_key"],
        "ALTER TABLE run_checkpoints ADD UNIQUE KEY uniq_run_checkpoint_idem (user_id, run_id, checkpoint_kind, idempotency_key)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "run_checkpoints",
        "idx_run_checkpoints_user_run_created",
        &["user_id", "run_id", "created_at"],
        "ALTER TABLE run_checkpoints ADD INDEX idx_run_checkpoints_user_run_created (user_id, run_id, created_at)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "run_checkpoints",
        "idx_run_checkpoints_session_kind_created",
        &["user_id", "session_id", "checkpoint_kind", "created_at"],
        "ALTER TABLE run_checkpoints ADD INDEX idx_run_checkpoints_session_kind_created (user_id, session_id, checkpoint_kind, created_at)",
    )
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS run_display_projections (
            run_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(64) NOT NULL,
            status VARCHAR(32) NOT NULL,
            waiting_for VARCHAR(64) NULL,
            error_message TEXT NULL,
            projection_event_idx BIGINT NOT NULL DEFAULT -1,
            latest_event_type VARCHAR(64) NULL,
            latest_checkpoint_id VARCHAR(64) NULL,
            latest_checkpoint_kind VARCHAR(32) NULL,
            latest_checkpoint_version VARCHAR(32) NULL,
            total_prompt_tokens BIGINT NOT NULL DEFAULT 0,
            total_completion_tokens BIGINT NOT NULL DEFAULT 0,
            total_tool_calls BIGINT NOT NULL DEFAULT 0,
            projection_hash VARCHAR(64) NOT NULL,
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, run_id),
            INDEX idx_run_display_projections_owner_session_updated (user_id, session_id, updated_at),
            INDEX idx_run_display_projections_user_updated (user_id, updated_at)
        )",
    )
    .execute(&pool)
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "run_display_projections",
        &["user_id", "run_id"],
        "ALTER TABLE run_display_projections ADD PRIMARY KEY (user_id, run_id)",
    )
    .await?;
    drop_index_if_present(
        &pool,
        &settings.database,
        "run_display_projections",
        "idx_run_display_projections_session_updated",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "run_display_projections",
        "idx_run_display_projections_owner_session_updated",
        &["user_id", "session_id", "updated_at"],
        "ALTER TABLE run_display_projections ADD INDEX idx_run_display_projections_owner_session_updated (user_id, session_id, updated_at)",
    )
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS session_tool_output_batches (
            batch_id VARCHAR(64) NOT NULL,
            session_id VARCHAR(64) NOT NULL,
            run_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            output_count INT NOT NULL,
            payload_bytes BIGINT NOT NULL,
            status VARCHAR(32) NOT NULL DEFAULT 'committed',
            request_id VARCHAR(64) NULL,
            trace_id VARCHAR(64) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, session_id, batch_id),
            INDEX idx_tool_output_batches_user_run_created (user_id, run_id, created_at, batch_id),
            INDEX idx_tool_output_batches_user_session_created (user_id, session_id, created_at, batch_id)
        )",
    )
    .execute(&pool)
    .await?;
    for removed_index in [
        "idx_tool_output_batches_session",
        "idx_tool_output_batches_run_status",
        "idx_tool_output_batches_run_created",
        "idx_tool_output_batches_session_created",
    ] {
        drop_index_if_present(
            &pool,
            &settings.database,
            "session_tool_output_batches",
            removed_index,
        )
        .await?;
    }
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "session_tool_output_batches",
        &["user_id", "session_id", "batch_id"],
        "ALTER TABLE session_tool_output_batches ADD PRIMARY KEY (user_id, session_id, batch_id)",
    )
    .await?;
    for (index, expected_columns, ddl) in [
        (
            "idx_tool_output_batches_user_run_created",
            &["user_id", "run_id", "created_at", "batch_id"][..],
            "ALTER TABLE session_tool_output_batches ADD INDEX idx_tool_output_batches_user_run_created (user_id, run_id, created_at, batch_id)",
        ),
        (
            "idx_tool_output_batches_user_session_created",
            &["user_id", "session_id", "created_at", "batch_id"][..],
            "ALTER TABLE session_tool_output_batches ADD INDEX idx_tool_output_batches_user_session_created (user_id, session_id, created_at, batch_id)",
        ),
    ] {
        ensure_index_shape(
            &pool,
            &settings.database,
            "session_tool_output_batches",
            index,
            expected_columns,
            ddl,
        )
        .await?;
    }

    query(
        "CREATE TABLE IF NOT EXISTS session_tool_outputs (
            output_id VARCHAR(64) NOT NULL,
            batch_id VARCHAR(64) NOT NULL,
            session_id VARCHAR(64) NOT NULL,
            run_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            output_idx INT NOT NULL,
            parent_output_id VARCHAR(64) NULL,
            tool_call_id VARCHAR(128) NULL,
            tool_name VARCHAR(128) NOT NULL,
            output_json LONGTEXT NOT NULL,
            payload_bytes BIGINT NOT NULL,
            preview_text LONGTEXT NULL,
            preview_status VARCHAR(32) NOT NULL DEFAULT 'template',
            artifact_ref VARCHAR(255) NULL,
            content_hash VARCHAR(128) NULL,
            normalize_version VARCHAR(32) NOT NULL DEFAULT 'raw_v1',
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, session_id, output_id),
            UNIQUE KEY uq_tool_outputs_batch_idx (user_id, session_id, batch_id, output_idx),
            INDEX idx_tool_outputs_user_run_created (user_id, run_id, created_at, output_id),
            INDEX idx_tool_outputs_user_session_created (user_id, session_id, created_at, output_id),
            INDEX idx_tool_outputs_parent (user_id, parent_output_id),
            INDEX idx_tool_outputs_artifact_ref (user_id, artifact_ref)
        )",
    )
    .execute(&pool)
    .await?;
    for removed_index in [
        "idx_tool_outputs_tool_created",
        "idx_tool_outputs_session_tool_score",
        "idx_tool_outputs_status_created",
        "idx_tool_outputs_batch",
        "idx_tool_outputs_run_created",
        "idx_tool_outputs_session_created",
    ] {
        drop_index_if_present(
            &pool,
            &settings.database,
            "session_tool_outputs",
            removed_index,
        )
        .await?;
    }
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "session_tool_outputs",
        &["user_id", "session_id", "output_id"],
        "ALTER TABLE session_tool_outputs ADD PRIMARY KEY (user_id, session_id, output_id)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "session_tool_outputs",
        "uq_tool_outputs_batch_idx",
        &["user_id", "session_id", "batch_id", "output_idx"],
        "ALTER TABLE session_tool_outputs ADD UNIQUE KEY uq_tool_outputs_batch_idx (user_id, session_id, batch_id, output_idx)",
    )
    .await?;
    for (index, expected_columns, ddl) in [
        (
            "idx_tool_outputs_user_run_created",
            &["user_id", "run_id", "created_at", "output_id"][..],
            "ALTER TABLE session_tool_outputs ADD INDEX idx_tool_outputs_user_run_created (user_id, run_id, created_at, output_id)",
        ),
        (
            "idx_tool_outputs_user_session_created",
            &["user_id", "session_id", "created_at", "output_id"][..],
            "ALTER TABLE session_tool_outputs ADD INDEX idx_tool_outputs_user_session_created (user_id, session_id, created_at, output_id)",
        ),
        (
            "idx_tool_outputs_parent",
            &["user_id", "parent_output_id"][..],
            "ALTER TABLE session_tool_outputs ADD INDEX idx_tool_outputs_parent (user_id, parent_output_id)",
        ),
        (
            "idx_tool_outputs_artifact_ref",
            &["user_id", "artifact_ref"][..],
            "ALTER TABLE session_tool_outputs ADD INDEX idx_tool_outputs_artifact_ref (user_id, artifact_ref)",
        ),
    ] {
        ensure_index_shape(
            &pool,
            &settings.database,
            "session_tool_outputs",
            index,
            expected_columns,
            ddl,
        )
        .await?;
    }

    // Retired semantic name+arguments dedup storage. Durable delivery is owned
    // by `tool_invocation_ledger`; keeping this table would preserve a second,
    // contradictory authority for external side effects.
    query("DROP TABLE IF EXISTS tool_exactly_once_results")
        .execute(&pool)
        .await?;

    query(
        "CREATE TABLE IF NOT EXISTS tool_invocation_ledger (
            user_id             VARCHAR(128) NOT NULL,
            session_id          VARCHAR(128) NOT NULL,
            run_id              VARCHAR(128) NOT NULL,
            turn_chain_id       VARCHAR(128) NOT NULL,
            invocation_id       VARCHAR(128) NOT NULL,
            identity_key        VARCHAR(71) NOT NULL,
            fingerprint_json    JSON NOT NULL,
            decision_json       JSON NOT NULL,
            state               VARCHAR(32) NOT NULL,
            dispatch_certainty  VARCHAR(32) NOT NULL,
            attempt_count       BIGINT UNSIGNED NOT NULL DEFAULT 0,
            dispatch_owner      VARCHAR(64) NULL,
            dispatch_lease_expires_at DATETIME(6) NULL,
            outcome_json        JSON NULL,
            completion_source_json JSON NULL,
            created_at          DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at          DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, session_id, run_id, turn_chain_id, invocation_id),
            INDEX idx_tool_invocation_updated (updated_at),
            INDEX idx_tool_invocation_run_compaction
                (user_id, session_id, run_id, state, identity_key)
        )",
    )
    .execute(&pool)
    .await?;
    add_column_if_missing(
        &pool,
        &settings.database,
        "tool_invocation_ledger",
        "identity_key",
        "ALTER TABLE tool_invocation_ledger ADD COLUMN identity_key VARCHAR(71) NULL",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "tool_invocation_ledger",
        "idx_tool_invocation_run_compaction",
        &["user_id", "session_id", "run_id", "state", "identity_key"],
        "ALTER TABLE tool_invocation_ledger ADD INDEX idx_tool_invocation_run_compaction (user_id, session_id, run_id, state, identity_key)",
    )
    .await?;
    query(
        "CREATE TABLE IF NOT EXISTS tool_invocation_archive_chunks (
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(64) NOT NULL,
            run_id VARCHAR(64) NOT NULL,
            chunk_index BIGINT UNSIGNED NOT NULL,
            artifact_id VARCHAR(64) NOT NULL,
            first_identity_key VARCHAR(71) NOT NULL,
            last_identity_key VARCHAR(71) NOT NULL,
            record_count BIGINT UNSIGNED NOT NULL,
            encoded_bytes BIGINT UNSIGNED NOT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, session_id, run_id, chunk_index),
            UNIQUE KEY uq_tool_invocation_archive_artifact
                (user_id, session_id, artifact_id),
            INDEX idx_tool_invocation_archive_lookup
                (user_id, session_id, run_id, first_identity_key, last_identity_key)
        )",
    )
    .execute(&pool)
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "tool_invocation_archive_chunks",
        &["user_id", "session_id", "run_id", "chunk_index"],
        "ALTER TABLE tool_invocation_archive_chunks ADD PRIMARY KEY (user_id, session_id, run_id, chunk_index)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "tool_invocation_archive_chunks",
        "idx_tool_invocation_archive_lookup",
        &[
            "user_id",
            "session_id",
            "run_id",
            "first_identity_key",
            "last_identity_key",
        ],
        "ALTER TABLE tool_invocation_archive_chunks ADD INDEX idx_tool_invocation_archive_lookup (user_id, session_id, run_id, first_identity_key, last_identity_key)",
    )
    .await?;
    add_column_if_missing(
        &pool,
        &settings.database,
        "tool_invocation_ledger",
        "decision_json",
        "ALTER TABLE tool_invocation_ledger ADD COLUMN decision_json JSON NULL",
    )
    .await?;
    add_column_if_missing(
        &pool,
        &settings.database,
        "tool_invocation_ledger",
        "dispatch_owner",
        "ALTER TABLE tool_invocation_ledger ADD COLUMN dispatch_owner VARCHAR(64) NULL",
    )
    .await?;
    add_column_if_missing(
        &pool,
        &settings.database,
        "tool_invocation_ledger",
        "dispatch_lease_expires_at",
        "ALTER TABLE tool_invocation_ledger ADD COLUMN dispatch_lease_expires_at DATETIME(6) NULL",
    )
    .await?;
    add_column_if_missing(
        &pool,
        &settings.database,
        "tool_invocation_ledger",
        "outcome_json",
        "ALTER TABLE tool_invocation_ledger ADD COLUMN outcome_json JSON NULL",
    )
    .await?;
    add_column_if_missing(
        &pool,
        &settings.database,
        "tool_invocation_ledger",
        "completion_source_json",
        "ALTER TABLE tool_invocation_ledger ADD COLUMN completion_source_json JSON NULL",
    )
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS semantic_read_observation_budgets (
            user_id             VARCHAR(128) NOT NULL,
            session_id          VARCHAR(128) NOT NULL,
            created_at          DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at          DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, session_id)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS semantic_read_observations (
            user_id             VARCHAR(128) NOT NULL,
            session_id          VARCHAR(128) NOT NULL,
            key_id              VARCHAR(71) NOT NULL,
            key_json            JSON NOT NULL,
            state               VARCHAR(16) NOT NULL,
            fill_owner          VARCHAR(64) NULL,
            fill_lease_expires_at DATETIME(6) NULL,
            observation_json    JSON NULL,
            observation_bytes   BIGINT UNSIGNED NULL,
            created_at          DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at          DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            last_accessed_at    DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, session_id, key_id),
            INDEX idx_semantic_read_observations_session_state_access
                (user_id, session_id, state, last_accessed_at)
        )",
    )
    .execute(&pool)
    .await?;

    // ── Web transcript hydration + device lease state (Phase 2 / G13+G19+G25) ──
    query(
        "CREATE TABLE IF NOT EXISTS session_transcript_items (
            session_id VARCHAR(64) NOT NULL,
            item_seq BIGINT NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            run_id VARCHAR(64) NULL,
            role VARCHAR(32) NOT NULL,
            content LONGTEXT NOT NULL,
            payload_json LONGTEXT NULL,
            source_event_id VARCHAR(128) NULL,
            source_event_idx BIGINT NULL,
            content_hash VARCHAR(128) NOT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, session_id, item_seq),
            INDEX idx_transcript_owner_run_event (user_id, run_id, source_event_idx),
            INDEX idx_transcript_owner_session_source_event (user_id, session_id, source_event_id)
        )",
    )
    .execute(&pool)
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "session_transcript_items",
        &["user_id", "session_id", "item_seq"],
        "ALTER TABLE session_transcript_items ADD PRIMARY KEY (user_id, session_id, item_seq)",
    )
    .await?;
    add_column_if_missing(
        &pool,
        &settings.database,
        "session_transcript_items",
        "payload_json",
        "ALTER TABLE session_transcript_items ADD COLUMN payload_json LONGTEXT NULL",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "session_transcript_items",
        "idx_transcript_owner_run_event",
        &["user_id", "run_id", "source_event_idx"],
        "ALTER TABLE session_transcript_items ADD INDEX idx_transcript_owner_run_event (user_id, run_id, source_event_idx)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "session_transcript_items",
        "idx_transcript_owner_session_source_event",
        &["user_id", "session_id", "source_event_id"],
        "ALTER TABLE session_transcript_items ADD INDEX idx_transcript_owner_session_source_event (user_id, session_id, source_event_id)",
    )
    .await?;
    query(
        "CREATE TABLE IF NOT EXISTS transcript_pages (
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(64) NOT NULL,
            page_seq BIGINT NOT NULL,
            start_item_seq BIGINT NOT NULL,
            end_item_seq BIGINT NOT NULL,
            item_count INT NOT NULL,
            page_hash VARCHAR(128) NOT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, session_id, page_seq),
            INDEX idx_transcript_pages_owner_session_end (user_id, session_id, end_item_seq),
            INDEX idx_transcript_pages_owner_session_updated (user_id, session_id, updated_at)
        )",
    )
    .execute(&pool)
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "transcript_pages",
        &["user_id", "session_id", "page_seq"],
        "ALTER TABLE transcript_pages ADD PRIMARY KEY (user_id, session_id, page_seq)",
    )
    .await?;
    for (index, expected_columns, ddl) in [
        (
            "idx_transcript_pages_owner_session_end",
            &["user_id", "session_id", "end_item_seq"][..],
            "ALTER TABLE transcript_pages ADD INDEX idx_transcript_pages_owner_session_end (user_id, session_id, end_item_seq)",
        ),
        (
            "idx_transcript_pages_owner_session_updated",
            &["user_id", "session_id", "updated_at"][..],
            "ALTER TABLE transcript_pages ADD INDEX idx_transcript_pages_owner_session_updated (user_id, session_id, updated_at)",
        ),
    ] {
        ensure_index_shape(
            &pool,
            &settings.database,
            "transcript_pages",
            index,
            expected_columns,
            ddl,
        )
        .await?;
    }
    query(
        "CREATE TABLE IF NOT EXISTS prompt_request_records (
            request_id VARCHAR(64) NOT NULL,
            session_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            run_id VARCHAR(64) NULL,
            turn BIGINT NOT NULL,
            round BIGINT NOT NULL,
            attempt BIGINT NOT NULL,
            source VARCHAR(64) NOT NULL,
            model VARCHAR(128) NOT NULL,
            provider VARCHAR(64) NOT NULL,
            max_output_tokens BIGINT NULL,
            message_count BIGINT NOT NULL,
            tool_count BIGINT NOT NULL,
            previous_request_id VARCHAR(64) NULL,
            request_hash VARCHAR(64) NOT NULL,
            summary_json LONGTEXT NOT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            created_at_unix_ms BIGINT NULL,
            PRIMARY KEY (user_id, request_id),
            UNIQUE KEY uq_prompt_request_attempt (user_id, session_id, turn, round, source, attempt),
            INDEX idx_prompt_requests_owner_session_created (user_id, session_id, created_at, turn, round, attempt),
            INDEX idx_prompt_requests_owner_run_created (user_id, run_id, created_at, turn, round, attempt),
            INDEX idx_prompt_requests_retention_ms (created_at_unix_ms, user_id, request_id, session_id)
        )",
    )
    .execute(&pool)
    .await?;
    add_column_if_missing(
        &pool,
        &settings.database,
        "prompt_request_records",
        "created_at_unix_ms",
        "ALTER TABLE prompt_request_records ADD COLUMN created_at_unix_ms BIGINT NULL",
    )
    .await?;
    query(
        "UPDATE prompt_request_records
         SET created_at_unix_ms = UNIX_TIMESTAMP(created_at) * 1000
         WHERE created_at_unix_ms IS NULL",
    )
    .execute(&pool)
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "prompt_request_records",
        &["user_id", "request_id"],
        "ALTER TABLE prompt_request_records ADD PRIMARY KEY (user_id, request_id)",
    )
    .await?;
    for removed_index in [
        "idx_prompt_requests_session_created",
        "idx_prompt_requests_run_created",
    ] {
        drop_index_if_present(
            &pool,
            &settings.database,
            "prompt_request_records",
            removed_index,
        )
        .await?;
    }
    for (index, expected_columns, ddl) in [
        (
            "idx_prompt_requests_owner_session_created",
            &[
                "user_id",
                "session_id",
                "created_at",
                "turn",
                "round",
                "attempt",
            ][..],
            "ALTER TABLE prompt_request_records ADD INDEX idx_prompt_requests_owner_session_created (user_id, session_id, created_at, turn, round, attempt)",
        ),
        (
            "idx_prompt_requests_owner_run_created",
            &[
                "user_id",
                "run_id",
                "created_at",
                "turn",
                "round",
                "attempt",
            ][..],
            "ALTER TABLE prompt_request_records ADD INDEX idx_prompt_requests_owner_run_created (user_id, run_id, created_at, turn, round, attempt)",
        ),
        (
            "idx_prompt_requests_retention_ms",
            &["created_at_unix_ms", "user_id", "request_id", "session_id"][..],
            "ALTER TABLE prompt_request_records ADD INDEX idx_prompt_requests_retention_ms (created_at_unix_ms, user_id, request_id, session_id)",
        ),
    ] {
        ensure_index_shape(
            &pool,
            &settings.database,
            "prompt_request_records",
            index,
            expected_columns,
            ddl,
        )
        .await?;
    }
    fail_if_obsolete_shape(
        &pool,
        &settings.database,
        "prompt_deltas",
        &[
            "user_id",
            "session_id",
            "request_id",
            "delta_seq",
            "logical_key",
            "chunk_kind",
            "position",
            "op",
            "chunk_id",
            "chunk_hash",
            "previous_chunk_hash",
            "created_at",
        ],
        &["delta_id", "payload_json"],
        &[
            "uq_prompt_delta_request_seq",
            "idx_prompt_deltas_request_position",
        ],
    )
    .await?;
    query(
        "CREATE TABLE IF NOT EXISTS prompt_deltas (
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(64) NOT NULL,
            request_id VARCHAR(64) NOT NULL,
            delta_seq INT NOT NULL,
            logical_key VARCHAR(191) NOT NULL,
            chunk_kind VARCHAR(32) NOT NULL,
            position INT NOT NULL,
            op VARCHAR(16) NOT NULL,
            chunk_id VARCHAR(80) NULL,
            chunk_hash VARCHAR(64) NULL,
            previous_chunk_hash VARCHAR(64) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, session_id, request_id, delta_seq),
            INDEX idx_prompt_deltas_owner_request_position (user_id, session_id, request_id, position, delta_seq)
        )",
    )
    .execute(&pool)
    .await?;
    query(
        "CREATE TABLE IF NOT EXISTS session_state_revisions (
            session_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            monotonic_id BIGINT NOT NULL DEFAULT 0,
            revision_hash VARCHAR(96) NOT NULL,
            device_fingerprint VARCHAR(128) NOT NULL,
            transcript_high_watermark BIGINT NOT NULL DEFAULT 0,
            run_event_high_watermark BIGINT NOT NULL DEFAULT 0,
            state_projection_hash VARCHAR(96) NOT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, session_id),
            INDEX idx_state_revisions_user_updated (user_id, updated_at)
        )",
    )
    .execute(&pool)
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "session_state_revisions",
        &["user_id", "session_id"],
        "ALTER TABLE session_state_revisions ADD PRIMARY KEY (user_id, session_id)",
    )
    .await?;
    query(
        "CREATE TABLE IF NOT EXISTS session_device_leases (
            lease_id VARCHAR(128) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(128) NOT NULL,
            device_id VARCHAR(128) NOT NULL,
            device_fingerprint VARCHAR(128) NOT NULL,
            trust_level VARCHAR(32) NOT NULL DEFAULT 'new_device',
            status VARCHAR(32) NOT NULL DEFAULT 'active',
            last_monotonic_id BIGINT NOT NULL DEFAULT 0,
            expires_at DATETIME(6) NOT NULL,
            revoked_at DATETIME(6) NULL,
            request_id VARCHAR(128) NULL,
            trace_id VARCHAR(128) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, lease_id),
            UNIQUE KEY uq_session_device (user_id, session_id, device_id),
            INDEX idx_device_leases_user_session (user_id, session_id, status, updated_at),
            INDEX idx_device_leases_fingerprint (user_id, device_fingerprint, status),
            INDEX idx_device_leases_expiry (status, expires_at)
        )",
    )
    .execute(&pool)
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "session_device_leases",
        &["user_id", "lease_id"],
        "ALTER TABLE session_device_leases ADD PRIMARY KEY (user_id, lease_id)",
    )
    .await?;
    query(
        "CREATE TABLE IF NOT EXISTS session_device_lease_events (
            lease_event_id VARCHAR(128) NOT NULL,
            lease_id VARCHAR(128) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(128) NOT NULL,
            device_id VARCHAR(128) NOT NULL,
            device_fingerprint VARCHAR(128) NOT NULL,
            event_type VARCHAR(64) NOT NULL,
            reason VARCHAR(64) NOT NULL,
            ended_at_server DATETIME(6) NOT NULL,
            request_id VARCHAR(128) NULL,
            trace_id VARCHAR(128) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, lease_event_id),
            INDEX idx_lease_events_user_created (user_id, created_at),
            INDEX idx_lease_events_owner_session_device (user_id, session_id, device_id, created_at),
            INDEX idx_lease_events_type_created (event_type, created_at)
        )",
    )
    .execute(&pool)
    .await?;
    drop_index_if_present(
        &pool,
        &settings.database,
        "session_device_lease_events",
        "idx_lease_events_session_device",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "session_device_lease_events",
        "idx_lease_events_owner_session_device",
        &["user_id", "session_id", "device_id", "created_at"],
        "ALTER TABLE session_device_lease_events ADD INDEX idx_lease_events_owner_session_device (user_id, session_id, device_id, created_at)",
    )
    .await?;

    // ── Sweeper leader election (prevents duplicate background work in
    // multi-pod deployments). One row per sweeper type; pods CAS-update
    // the lease every TTL/2 seconds. Only the lease holder runs work.
    // Table created idempotently via IF NOT EXISTS — no DROP, no data loss.
    query(
        "CREATE TABLE IF NOT EXISTS sweeper_leases (
            sweeper_name VARCHAR(128) PRIMARY KEY,
            owner_pod_id VARCHAR(256) NOT NULL,
            expires_at DATETIME(6) NOT NULL,
            version INT UNSIGNED NOT NULL DEFAULT 0,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6)
        )",
    )
    .execute(&pool)
    .await?;

    // Durable keyset cursors keep bounded maintenance jobs fair across pod
    // changes and process restarts. The cursor is progress, never authority:
    // every mutation performed by a sweeper remains independently idempotent.
    query(
        "CREATE TABLE IF NOT EXISTS maintenance_sweep_cursors (
            sweep_name VARCHAR(128) PRIMARY KEY,
            cursor_updated_at DATETIME(6) NOT NULL,
            cursor_user_id VARCHAR(128) NOT NULL,
            cursor_run_id VARCHAR(64) NOT NULL,
            scan_generation BIGINT UNSIGNED NOT NULL DEFAULT 0,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6)
        )",
    )
    .execute(&pool)
    .await?;

    // ── Context manifest v1 (Phase 3 / G1+G3+G10+G26+G27) ───────────────
    query(
        "CREATE TABLE IF NOT EXISTS context_manifests (
            manifest_id VARCHAR(128) PRIMARY KEY,
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(128) NOT NULL,
            run_id VARCHAR(128) NULL,
            turn_id VARCHAR(128) NOT NULL,
            model_provider VARCHAR(64) NOT NULL,
            model_name VARCHAR(128) NOT NULL,
            context_window_tokens INT NOT NULL,
            max_output_tokens INT NOT NULL,
            total_estimated_tokens INT NOT NULL,
            stable_prefix_hash VARCHAR(128) NULL,
            prompt_cache_key VARCHAR(255) NULL,
            compaction_version VARCHAR(64) NULL,
            policy_version VARCHAR(64) NOT NULL,
            tokenizer_id VARCHAR(128) NULL,
            budget_template_id VARCHAR(64) NULL,
            turn_intent VARCHAR(64) NULL,
            reason VARCHAR(64) NOT NULL,
            dropped_count INT NOT NULL DEFAULT 0,
            manifest_json LONGTEXT NOT NULL,
            request_id VARCHAR(128) NULL,
            trace_id VARCHAR(128) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_ctx_manifest_owner_session_created (user_id, session_id, created_at, manifest_id),
            INDEX idx_ctx_manifest_owner_session_run_created (user_id, session_id, run_id, created_at, manifest_id),
            INDEX idx_ctx_manifest_user_created (user_id, created_at)
        )",
    )
    .execute(&pool)
    .await?;
    for removed_index in ["idx_ctx_manifest_session_turn", "idx_ctx_manifest_run"] {
        drop_index_if_present(
            &pool,
            &settings.database,
            "context_manifests",
            removed_index,
        )
        .await?;
    }
    for (index, expected_columns, ddl) in [
        (
            "idx_ctx_manifest_owner_session_created",
            &["user_id", "session_id", "created_at", "manifest_id"][..],
            "ALTER TABLE context_manifests ADD INDEX idx_ctx_manifest_owner_session_created (user_id, session_id, created_at, manifest_id)",
        ),
        (
            "idx_ctx_manifest_owner_session_run_created",
            &[
                "user_id",
                "session_id",
                "run_id",
                "created_at",
                "manifest_id",
            ][..],
            "ALTER TABLE context_manifests ADD INDEX idx_ctx_manifest_owner_session_run_created (user_id, session_id, run_id, created_at, manifest_id)",
        ),
    ] {
        ensure_index_shape(
            &pool,
            &settings.database,
            "context_manifests",
            index,
            expected_columns,
            ddl,
        )
        .await?;
    }
    query(
        "CREATE TABLE IF NOT EXISTS context_manifest_items (
            manifest_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(128) NOT NULL,
            item_order INT NOT NULL,
            zone VARCHAR(64) NOT NULL,
            source_table VARCHAR(64) NOT NULL,
            source_id VARCHAR(128) NOT NULL,
            source_hash VARCHAR(128) NULL,
            included SMALLINT NOT NULL,
            token_estimate INT NOT NULL DEFAULT 0,
            budget_tokens INT NOT NULL DEFAULT 0,
            reason VARCHAR(128) NOT NULL,
            render_mode VARCHAR(64) NOT NULL,
            raw_ref VARCHAR(255) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (manifest_id, item_order),
            INDEX idx_manifest_items_source (source_table, source_id),
            INDEX idx_manifest_items_manifest_zone (manifest_id, zone, included),
            INDEX idx_manifest_items_raw_ref (raw_ref)
        )",
    )
    .execute(&pool)
    .await?;
    fail_if_obsolete_shape(
        &pool,
        &settings.database,
        "context_manifest_items",
        &["manifest_id", "item_order"],
        &["id"],
        &["uq_manifest_item_order"],
    )
    .await?;
    drop_index_if_present(
        &pool,
        &settings.database,
        "context_manifest_items",
        "idx_manifest_items_session_zone",
    )
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "context_manifest_items",
        &["manifest_id", "item_order"],
        "ALTER TABLE context_manifest_items ADD PRIMARY KEY (manifest_id, item_order)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "context_manifest_items",
        "idx_manifest_items_manifest_zone",
        &["manifest_id", "zone", "included"],
        "ALTER TABLE context_manifest_items ADD INDEX idx_manifest_items_manifest_zone (manifest_id, zone, included)",
    )
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS preview_template_registry (
            tool_name VARCHAR(128) NOT NULL,
            version VARCHAR(64) NOT NULL,
            status VARCHAR(32) NOT NULL DEFAULT 'active',
            max_preview_bytes INT NOT NULL DEFAULT 400,
            default_chunk_type VARCHAR(64) NOT NULL DEFAULT 'tool_output_preview',
            first_class_columns_json LONGTEXT NOT NULL,
            fts_field_weights_json LONGTEXT NOT NULL,
            normalize_version VARCHAR(32) NOT NULL DEFAULT 'v1',
            schema_json LONGTEXT NOT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (tool_name, version),
            INDEX idx_preview_templates_status (tool_name, status, updated_at)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS raw_ref_scheme_registry (
            scheme VARCHAR(64) PRIMARY KEY,
            resolver_name VARCHAR(128) NOT NULL,
            backing_store VARCHAR(64) NOT NULL,
            access_check VARCHAR(64) NOT NULL,
            canonical_example VARCHAR(255) NOT NULL,
            is_active SMALLINT NOT NULL DEFAULT 1,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6)
        )",
    )
    .execute(&pool)
    .await?;

    for (scheme, resolver, backing, access_check, example) in [
        (
            "artifact",
            "artifact_resolver",
            "matrixone",
            "session_artifact_acl",
            "artifact://session/artifact_id@sha256:...",
        ),
        (
            "s3",
            "s3_resolver",
            "object_store",
            "presigned_url_acl",
            "s3://bucket/key@sha256:...",
        ),
        (
            "conversation_log",
            "conversation_log_resolver",
            "matrixone",
            "session_owner",
            "conversation_log://session/item_seq@sha256:...",
        ),
        (
            "object_store",
            "object_store_resolver",
            "object_store",
            "object_acl",
            "object_store://namespace/key@sha256:...",
        ),
        (
            "cold_storage",
            "cold_storage_resolver",
            "cold_storage",
            "archive_acl",
            "cold_storage://archive/key@sha256:...",
        ),
        (
            "blob",
            "blob_resolver",
            "matrixone",
            "blob_acl",
            "blob://table/blob_id@sha256:...",
        ),
        (
            "tool_output",
            "tool_output_resolver",
            "matrixone",
            "session_owner",
            "tool_output://session/output_id@sha256:...",
        ),
        (
            "chunk",
            "history_chunk_resolver",
            "matrixone",
            "session_or_user_scope",
            "chunk://session/chunk_id@sha256:...",
        ),
        (
            "state_item",
            "state_item_resolver",
            "matrixone",
            "session_or_user_scope",
            "state_item://session/item_id@sha256:...",
        ),
    ] {
        query(
            "INSERT IGNORE INTO raw_ref_scheme_registry
             (scheme, resolver_name, backing_store, access_check, canonical_example, is_active, created_at)
             VALUES (?, ?, ?, ?, ?, 1, NOW(6))",
        )
        .bind(scheme)
        .bind(resolver)
        .bind(backing)
        .bind(access_check)
        .bind(example)
        .execute(&pool)
        .await?;
    }

    for (tool_name, max_preview_bytes, normalize_version) in
        crate::context_manifest::BASELINE_PREVIEW_TEMPLATES
    {
        let fts_field_weights =
            crate::context_manifest::preview_template_fts_field_weights(normalize_version);
        query(
            "INSERT IGNORE INTO preview_template_registry
             (tool_name, version, status, max_preview_bytes, default_chunk_type,
              first_class_columns_json, fts_field_weights_json, normalize_version, schema_json,
              created_at, updated_at)
             VALUES (?, 'v1', 'active', ?, 'tool_output_preview', '[]', ?, ?, '{}', NOW(6), NOW(6))",
        )
        .bind(tool_name)
        .bind(i64::from(*max_preview_bytes))
        .bind(fts_field_weights)
        .bind(normalize_version)
        .execute(&pool)
        .await?;

        query(
            "UPDATE preview_template_registry
             SET fts_field_weights_json = ?, updated_at = NOW(6)
             WHERE tool_name = ? AND version = 'v1' AND fts_field_weights_json = '{}'",
        )
        .bind(fts_field_weights)
        .bind(tool_name)
        .execute(&pool)
        .await?;
    }

    // ── State projection v1 (Phase 4 / G2+G4+G5+G6+G14+G16+G20) ────────
    query(
        "CREATE TABLE IF NOT EXISTS session_state_items (
            item_id VARCHAR(128) PRIMARY KEY,
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(128) NOT NULL,
            scope VARCHAR(32) NOT NULL DEFAULT 'session',
            category VARCHAR(64) NOT NULL,
            item_key VARCHAR(255) NOT NULL,
            status VARCHAR(32) NOT NULL DEFAULT 'active',
            priority INT NOT NULL DEFAULT 0,
            source VARCHAR(64) NOT NULL,
            provenance_event_id VARCHAR(128) NULL,
            run_id VARCHAR(128) NULL,
            title VARCHAR(255) NULL,
            summary_text TEXT NULL,
            payload_json LONGTEXT NULL,
            payload_hash VARCHAR(128) NULL,
            token_estimate INT NOT NULL DEFAULT 0,
            version BIGINT NOT NULL DEFAULT 1,
            origin_session_id VARCHAR(128) NULL,
            origin_chunk_id VARCHAR(128) NULL,
            origin_state_item_id VARCHAR(128) NULL,
            expires_at DATETIME(6) NULL,
            request_id VARCHAR(128) NULL,
            trace_id VARCHAR(128) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            CONSTRAINT chk_session_state_items_scope CHECK (scope IN ('session', 'user', 'project', 'workspace')),
            UNIQUE KEY uq_state_current (user_id, session_id, scope, category, item_key),
            INDEX idx_state_owner_session_status_category (user_id, session_id, status, category),
            INDEX idx_state_user_category (user_id, category, status, updated_at),
            INDEX idx_state_user_scope_category (user_id, scope, category, status, priority),
            INDEX idx_state_owner_origin_session (user_id, origin_session_id, category, status),
            INDEX idx_state_expires (expires_at),
            INDEX idx_state_provenance (provenance_event_id)
        )",
    )
    .execute(&pool)
    .await?;
    for removed_index in ["idx_state_session_category", "idx_state_origin_session"] {
        drop_index_if_present(
            &pool,
            &settings.database,
            "session_state_items",
            removed_index,
        )
        .await?;
    }
    for (index, expected_columns, ddl) in [
        (
            "idx_state_owner_session_status_category",
            &["user_id", "session_id", "status", "category"][..],
            "ALTER TABLE session_state_items ADD INDEX idx_state_owner_session_status_category (user_id, session_id, status, category)",
        ),
        (
            "idx_state_user_scope_category",
            &["user_id", "scope", "category", "status", "priority"][..],
            "ALTER TABLE session_state_items ADD INDEX idx_state_user_scope_category (user_id, scope, category, status, priority)",
        ),
        (
            "idx_state_owner_origin_session",
            &["user_id", "origin_session_id", "category", "status"][..],
            "ALTER TABLE session_state_items ADD INDEX idx_state_owner_origin_session (user_id, origin_session_id, category, status)",
        ),
    ] {
        ensure_index_shape(
            &pool,
            &settings.database,
            "session_state_items",
            index,
            expected_columns,
            ddl,
        )
        .await?;
    }
    query(
        "CREATE TABLE IF NOT EXISTS session_state_item_events (
            event_id VARCHAR(64) NOT NULL,
            item_id VARCHAR(128) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(128) NOT NULL,
            category VARCHAR(64) NOT NULL,
            item_key VARCHAR(255) NOT NULL,
            mutation VARCHAR(32) NOT NULL,
            previous_hash VARCHAR(128) NULL,
            next_hash VARCHAR(128) NULL,
            previous_version BIGINT NULL,
            next_version BIGINT NULL,
            payload_json LONGTEXT NULL,
            provenance_event_id VARCHAR(128) NULL,
            request_id VARCHAR(128) NULL,
            trace_id VARCHAR(128) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            CONSTRAINT chk_state_item_event_mutation CHECK (mutation IN ('insert', 'update', 'replace', 'archive', 'delete', 'bubble_up', 'apply_suggestion', 'activate')),
            PRIMARY KEY (user_id, event_id),
            INDEX idx_state_events_item_created (item_id, created_at, event_id),
            INDEX idx_state_events_owner_session_created (user_id, session_id, created_at, event_id),
            INDEX idx_state_events_category_created (category, created_at, event_id)
        )",
    )
    .execute(&pool)
    .await?;
    fail_if_obsolete_shape(
        &pool,
        &settings.database,
        "session_state_item_events",
        &["user_id", "event_id"],
        &["id"],
        &[],
    )
    .await?;
    drop_index_if_present(
        &pool,
        &settings.database,
        "session_state_item_events",
        "idx_state_events_session_created",
    )
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "session_state_item_events",
        &["user_id", "event_id"],
        "ALTER TABLE session_state_item_events ADD PRIMARY KEY (user_id, event_id)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "session_state_item_events",
        "idx_state_events_item_created",
        &["item_id", "created_at", "event_id"],
        "ALTER TABLE session_state_item_events ADD INDEX idx_state_events_item_created (item_id, created_at, event_id)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "session_state_item_events",
        "idx_state_events_owner_session_created",
        &["user_id", "session_id", "created_at", "event_id"],
        "ALTER TABLE session_state_item_events ADD INDEX idx_state_events_owner_session_created (user_id, session_id, created_at, event_id)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "session_state_item_events",
        "idx_state_events_category_created",
        &["category", "created_at", "event_id"],
        "ALTER TABLE session_state_item_events ADD INDEX idx_state_events_category_created (category, created_at, event_id)",
    )
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS session_delegations (
            delegation_id VARCHAR(128) PRIMARY KEY,
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(128) NOT NULL,
            parent_run_id VARCHAR(128) NOT NULL,
            child_run_id VARCHAR(128) NOT NULL,
            root_run_id VARCHAR(128) NOT NULL,
            ancestor_path VARCHAR(2048) NOT NULL,
            depth INT NOT NULL DEFAULT 0,
            agent_id VARCHAR(255) NULL,
            title VARCHAR(255) NULL,
            status VARCHAR(32) NOT NULL DEFAULT 'running',
            retry_of VARCHAR(128) NULL,
            retry_scope VARCHAR(32) NOT NULL DEFAULT 'node',
            last_summary_ref VARCHAR(255) NULL,
            last_summary_text TEXT NULL,
            sibling_exposed_artifacts_json LONGTEXT NULL,
            request_id VARCHAR(128) NULL,
            trace_id VARCHAR(128) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            CONSTRAINT chk_session_delegations_retry_scope CHECK (retry_scope IN ('node', 'subtree', 'siblings')),
            UNIQUE KEY uq_session_delegations_child (child_run_id),
            INDEX idx_delegations_owner_root_depth (user_id, root_run_id, depth, created_at),
            INDEX idx_delegations_owner_parent_status_updated (user_id, parent_run_id, status, updated_at),
            INDEX idx_delegations_owner_session_status (user_id, session_id, status, updated_at),
            INDEX idx_delegations_retry_of (retry_of)
        )",
    )
    .execute(&pool)
    .await?;
    for removed_index in [
        "idx_delegations_root_depth",
        "idx_delegations_parent",
        "idx_delegations_session_status",
    ] {
        drop_index_if_present(
            &pool,
            &settings.database,
            "session_delegations",
            removed_index,
        )
        .await?;
    }
    for (index, expected_columns, ddl) in [
        (
            "idx_delegations_owner_root_depth",
            &["user_id", "root_run_id", "depth", "created_at"][..],
            "ALTER TABLE session_delegations ADD INDEX idx_delegations_owner_root_depth (user_id, root_run_id, depth, created_at)",
        ),
        (
            "idx_delegations_owner_parent_status_updated",
            &["user_id", "parent_run_id", "status", "updated_at"][..],
            "ALTER TABLE session_delegations ADD INDEX idx_delegations_owner_parent_status_updated (user_id, parent_run_id, status, updated_at)",
        ),
        (
            "idx_delegations_owner_session_status",
            &["user_id", "session_id", "status", "updated_at"][..],
            "ALTER TABLE session_delegations ADD INDEX idx_delegations_owner_session_status (user_id, session_id, status, updated_at)",
        ),
    ] {
        ensure_index_shape(
            &pool,
            &settings.database,
            "session_delegations",
            index,
            expected_columns,
            ddl,
        )
        .await?;
    }

    query(
        "CREATE TABLE IF NOT EXISTS session_history_chunks (
            chunk_id VARCHAR(128) PRIMARY KEY,
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(128) NOT NULL,
            source_session_id VARCHAR(128) NULL,
            seq_start BIGINT NOT NULL DEFAULT 0,
            seq_end BIGINT NOT NULL DEFAULT 0,
            item_seq_start BIGINT NULL,
            item_seq_end BIGINT NULL,
            turn_start BIGINT NULL,
            turn_end BIGINT NULL,
            chunk_type VARCHAR(64) NOT NULL,
            source_table VARCHAR(64) NOT NULL,
            source_id VARCHAR(128) NOT NULL,
            content_text LONGTEXT NOT NULL,
            content_hash VARCHAR(128) NOT NULL,
            token_estimate INT NOT NULL DEFAULT 0,
            provenance_json LONGTEXT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_history_user_chunk_created (user_id, chunk_type, created_at),
            INDEX idx_history_owner_session_seq (user_id, session_id, seq_start, seq_end),
            INDEX idx_history_owner_source_session (user_id, source_session_id, chunk_type, created_at)
        )",
    )
    .execute(&pool)
    .await?;
    for removed_index in ["idx_history_session_seq", "idx_history_source_session"] {
        drop_index_if_present(
            &pool,
            &settings.database,
            "session_history_chunks",
            removed_index,
        )
        .await?;
    }
    for (index, expected_columns, ddl) in [
        (
            "idx_history_owner_session_seq",
            &["user_id", "session_id", "seq_start", "seq_end"][..],
            "ALTER TABLE session_history_chunks ADD INDEX idx_history_owner_session_seq (user_id, session_id, seq_start, seq_end)",
        ),
        (
            "idx_history_owner_source_session",
            &["user_id", "source_session_id", "chunk_type", "created_at"][..],
            "ALTER TABLE session_history_chunks ADD INDEX idx_history_owner_source_session (user_id, source_session_id, chunk_type, created_at)",
        ),
    ] {
        ensure_index_shape(
            &pool,
            &settings.database,
            "session_history_chunks",
            index,
            expected_columns,
            ddl,
        )
        .await?;
    }

    query("DROP TABLE IF EXISTS session_artifact_grants")
        .execute(&pool)
        .await?;

    query(
        "CREATE TABLE IF NOT EXISTS session_artifacts_grants (
            grant_id VARCHAR(128) PRIMARY KEY,
            artifact_id VARCHAR(128) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(128) NOT NULL,
            root_run_id VARCHAR(128) NOT NULL,
            source_run_id VARCHAR(128) NOT NULL,
            target_run_id VARCHAR(128) NULL,
            target_delegation_id VARCHAR(128) NULL,
            grant_scope VARCHAR(32) NOT NULL,
            granted_by VARCHAR(128) NOT NULL,
            reason VARCHAR(128) NULL,
            expires_at DATETIME(6) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            UNIQUE KEY uq_artifacts_grant_target (user_id, session_id, artifact_id, grant_scope, target_run_id, target_delegation_id),
            INDEX idx_artifacts_grants_root (user_id, root_run_id, grant_scope, created_at),
            INDEX idx_artifacts_grants_target (user_id, session_id, target_run_id, artifact_id, expires_at),
            INDEX idx_artifacts_grants_delegation_target (user_id, session_id, target_delegation_id, artifact_id, expires_at)
        )",
    )
    .execute(&pool)
    .await?;
    for (index, expected_columns, ddl) in [
        (
            "uq_artifacts_grant_target",
            &[
                "user_id",
                "session_id",
                "artifact_id",
                "grant_scope",
                "target_run_id",
                "target_delegation_id",
            ][..],
            "ALTER TABLE session_artifacts_grants ADD UNIQUE KEY uq_artifacts_grant_target (user_id, session_id, artifact_id, grant_scope, target_run_id, target_delegation_id)",
        ),
        (
            "idx_artifacts_grants_target",
            &[
                "user_id",
                "session_id",
                "target_run_id",
                "artifact_id",
                "expires_at",
            ][..],
            "ALTER TABLE session_artifacts_grants ADD INDEX idx_artifacts_grants_target (user_id, session_id, target_run_id, artifact_id, expires_at)",
        ),
        (
            "idx_artifacts_grants_delegation_target",
            &[
                "user_id",
                "session_id",
                "target_delegation_id",
                "artifact_id",
                "expires_at",
            ][..],
            "ALTER TABLE session_artifacts_grants ADD INDEX idx_artifacts_grants_delegation_target (user_id, session_id, target_delegation_id, artifact_id, expires_at)",
        ),
    ] {
        ensure_index_shape(
            &pool,
            &settings.database,
            "session_artifacts_grants",
            index,
            expected_columns,
            ddl,
        )
        .await?;
    }

    fail_if_obsolete_shape(
        &pool,
        &settings.database,
        "agent_event_edges",
        &[
            "user_id",
            "session_id",
            "child_event_id",
            "parent_event_id",
            "relation_kind",
            "parent_order",
        ],
        &[],
        &[
            "idx_agent_event_edges_child",
            "idx_agent_event_edges_parent",
        ],
    )
    .await?;
    fail_if_required_columns_missing_or_nullable(
        &pool,
        &settings.database,
        "agent_event_edges",
        &["user_id", "session_id"],
    )
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS agent_event_edges (
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(128) NOT NULL,
            child_event_id VARCHAR(128) NOT NULL,
            parent_event_id VARCHAR(128) NOT NULL,
            relation_kind VARCHAR(32) NOT NULL DEFAULT 'causal',
            parent_order INT NOT NULL DEFAULT 0,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, child_event_id, parent_event_id, relation_kind),
            INDEX idx_agent_event_edges_owner_session_child (user_id, session_id, child_event_id),
            INDEX idx_agent_event_edges_owner_child (user_id, child_event_id, parent_order),
            INDEX idx_agent_event_edges_owner_parent (user_id, parent_event_id, parent_order)
        )",
    )
    .execute(&pool)
    .await?;
    fail_if_varchar_columns_shorter_than(
        &pool,
        &settings.database,
        "agent_event_edges",
        &[
            ("child_event_id", AGENT_EVENT_ID_LEN as u64),
            ("parent_event_id", AGENT_EVENT_ID_LEN as u64),
        ],
    )
    .await?;

    // Harness diagnostic snapshots — separated from agent_events to avoid
    // polluting session event counts and to carry causal_chain_id natively.
    query(
        "CREATE TABLE IF NOT EXISTS harness_snapshots (
            snapshot_id VARCHAR(64) PRIMARY KEY,
            session_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            hook_point VARCHAR(32) NOT NULL,
            turn_number INT UNSIGNED NOT NULL DEFAULT 0,
            snapshot_json LONGTEXT NOT NULL,
            causal_chain_id VARCHAR(128),
            created_at DATETIME(6) DEFAULT NOW(6),
            INDEX idx_harness_owner_session_created (user_id, session_id, created_at),
            INDEX idx_harness_owner_session_turn (user_id, session_id, turn_number),
            INDEX idx_harness_owner_chain (user_id, causal_chain_id)
        )",
    )
    .execute(&pool)
    .await?;
    for removed_index in [
        "idx_harness_session",
        "idx_harness_session_turn",
        "idx_harness_chain",
    ] {
        drop_index_if_present(
            &pool,
            &settings.database,
            "harness_snapshots",
            removed_index,
        )
        .await?;
    }
    for (index, expected_columns, ddl) in [
        (
            "idx_harness_owner_session_created",
            &["user_id", "session_id", "created_at"][..],
            "ALTER TABLE harness_snapshots ADD INDEX idx_harness_owner_session_created (user_id, session_id, created_at)",
        ),
        (
            "idx_harness_owner_session_turn",
            &["user_id", "session_id", "turn_number"][..],
            "ALTER TABLE harness_snapshots ADD INDEX idx_harness_owner_session_turn (user_id, session_id, turn_number)",
        ),
        (
            "idx_harness_owner_chain",
            &["user_id", "causal_chain_id"][..],
            "ALTER TABLE harness_snapshots ADD INDEX idx_harness_owner_chain (user_id, causal_chain_id)",
        ),
    ] {
        ensure_index_shape(
            &pool,
            &settings.database,
            "harness_snapshots",
            index,
            expected_columns,
            ddl,
        )
        .await?;
    }

    // Product harness workflow state. This is separate from the diagnostic
    // `harness_snapshots` table above: these rows are the durable product model
    // for reusable user workflows such as Skillify.
    query(
        "CREATE TABLE IF NOT EXISTS harness_runs (
            harness_run_id VARCHAR(128) PRIMARY KEY,
            harness_id VARCHAR(128) NOT NULL,
            version_id VARCHAR(128) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(128) NULL,
            workflow_run_id VARCHAR(128) NULL,
            agent_run_id VARCHAR(128) NULL,
            parent_agent_run_id VARCHAR(128) NULL,
            status VARCHAR(64) NOT NULL,
            input_json LONGTEXT NOT NULL,
            output_json LONGTEXT NOT NULL,
            error TEXT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_harness_runs_user_status_updated (user_id, status, updated_at),
            INDEX idx_harness_runs_harness_user (harness_id, user_id, updated_at),
            INDEX idx_harness_runs_owner_session_updated (user_id, session_id, updated_at)
        )",
    )
    .execute(&pool)
    .await?;
    drop_index_if_present(
        &pool,
        &settings.database,
        "harness_runs",
        "idx_harness_runs_session",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "harness_runs",
        "idx_harness_runs_owner_session_updated",
        &["user_id", "session_id", "updated_at"],
        "ALTER TABLE harness_runs ADD INDEX idx_harness_runs_owner_session_updated (user_id, session_id, updated_at)",
    )
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS harness_items (
            item_id VARCHAR(128) PRIMARY KEY,
            harness_run_id VARCHAR(128) NOT NULL,
            parent_item_id VARCHAR(128) NULL,
            item_type VARCHAR(64) NOT NULL,
            locator_json LONGTEXT NOT NULL,
            input_json LONGTEXT NOT NULL,
            proposed_output_json LONGTEXT NOT NULL,
            final_output_json LONGTEXT NOT NULL,
            decision_history_json LONGTEXT NULL,
            status VARCHAR(64) NOT NULL,
            confidence DOUBLE NULL,
            assigned_to VARCHAR(128) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_harness_items_run_status (harness_run_id, status, updated_at),
            INDEX idx_harness_items_assigned (assigned_to, status, updated_at)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS harness_skill_drafts (
            skill_draft_id VARCHAR(128) PRIMARY KEY,
            harness_run_id VARCHAR(128) NOT NULL,
            candidate_name VARCHAR(128) NOT NULL,
            description TEXT NOT NULL,
            target_scope VARCHAR(32) NOT NULL,
            publish_visibility VARCHAR(32) NOT NULL,
            content_markdown LONGTEXT NOT NULL,
            source_summary_json LONGTEXT NOT NULL,
            decision_history_json LONGTEXT NULL,
            status VARCHAR(64) NOT NULL,
            confidence DOUBLE NULL,
            created_by_node_id VARCHAR(128) NULL,
            revision BIGINT NOT NULL DEFAULT 1,
            published_version_id VARCHAR(128) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_harness_skill_drafts_run_status (harness_run_id, status, updated_at),
            INDEX idx_harness_skill_drafts_run_revision (harness_run_id, revision)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS harness_skill_rules (
            skill_rule_id VARCHAR(128) PRIMARY KEY,
            skill_draft_id VARCHAR(128) NOT NULL,
            harness_run_id VARCHAR(128) NOT NULL,
            rule_type VARCHAR(64) NOT NULL,
            statement TEXT NOT NULL,
            rationale TEXT NOT NULL,
            decision_history_json LONGTEXT NULL,
            status VARCHAR(64) NOT NULL,
            confidence DOUBLE NULL,
            source_count BIGINT NOT NULL DEFAULT 0,
            created_by_node_id VARCHAR(128) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_harness_skill_rules_draft_status (skill_draft_id, status, updated_at),
            INDEX idx_harness_skill_rules_run_status (harness_run_id, status, updated_at)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS harness_citations (
            citation_id VARCHAR(128) PRIMARY KEY,
            harness_run_id VARCHAR(128) NOT NULL,
            item_id VARCHAR(128) NOT NULL,
            skill_draft_id VARCHAR(128) NULL,
            skill_rule_id VARCHAR(128) NULL,
            source_id VARCHAR(128) NULL,
            source_locator_json LONGTEXT NOT NULL,
            source_snapshot_ref VARCHAR(128) NULL,
            source_content_hash VARCHAR(128) NULL,
            source_metadata_json LONGTEXT NULL,
            artifact_id VARCHAR(128) NULL,
            quote_hash VARCHAR(128) NULL,
            evidence_text_preview TEXT NULL,
            relevance_score DOUBLE NULL,
            created_by_node_id VARCHAR(128) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_harness_citations_item (item_id, created_at),
            INDEX idx_harness_citations_skill_rule (skill_rule_id, created_at),
            INDEX idx_harness_citations_run (harness_run_id, created_at)
        )",
    )
    .execute(&pool)
    .await?;

    // Context / decisions / evaluation essentials used by turn persistence
    query(
        "CREATE TABLE IF NOT EXISTS ctx_snapshots (
            context_capture_id VARCHAR(64) PRIMARY KEY,
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(64) NOT NULL,
            event_id VARCHAR(128) NOT NULL,
            context_data JSON NULL,
            llm_request_id VARCHAR(64) NULL,
            llm_response_id VARCHAR(64) NULL,
            token_budget INT NULL,
            total_tokens BIGINT NULL,
            assembly_time_ms BIGINT NULL,
            relevance_scores JSON NULL,
            token_usage JSON NULL,
            task_type VARCHAR(64) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_ctx_snapshots_owner_session_created (user_id, session_id, created_at),
            INDEX idx_ctx_snapshots_owner_event_id (user_id, event_id)
        )",
    )
    .execute(&pool)
    .await?;
    fail_if_varchar_columns_shorter_than(
        &pool,
        &settings.database,
        "ctx_snapshots",
        &[("event_id", AGENT_EVENT_ID_LEN as u64)],
    )
    .await?;
    for removed_index in [
        "idx_ctx_snapshots_session_created",
        "idx_ctx_snapshots_event_id",
    ] {
        drop_index_if_present(&pool, &settings.database, "ctx_snapshots", removed_index).await?;
    }
    for (index, expected_columns, ddl) in [
        (
            "idx_ctx_snapshots_owner_session_created",
            &["user_id", "session_id", "created_at"][..],
            "ALTER TABLE ctx_snapshots ADD INDEX idx_ctx_snapshots_owner_session_created (user_id, session_id, created_at)",
        ),
        (
            "idx_ctx_snapshots_owner_event_id",
            &["user_id", "event_id"][..],
            "ALTER TABLE ctx_snapshots ADD INDEX idx_ctx_snapshots_owner_event_id (user_id, event_id)",
        ),
    ] {
        ensure_index_shape(
            &pool,
            &settings.database,
            "ctx_snapshots",
            index,
            expected_columns,
            ddl,
        )
        .await?;
    }

    query(
        "CREATE TABLE IF NOT EXISTS ctx_decision_audits (
            decision_id VARCHAR(64) PRIMARY KEY,
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(64) NOT NULL,
            event_id VARCHAR(128) NULL,
            context_capture_id VARCHAR(64) NULL,
            decision_type VARCHAR(64) NOT NULL,
            decision_output JSON NULL,
            model_params JSON NULL,
            model_used VARCHAR(128) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_ctx_decisions_owner_session_type_created (user_id, session_id, decision_type, created_at),
            INDEX idx_ctx_decisions_owner_event_id (user_id, event_id),
            INDEX idx_ctx_decisions_owner_context_capture_id (user_id, context_capture_id)
        )",
    )
    .execute(&pool)
    .await?;
    fail_if_varchar_columns_shorter_than(
        &pool,
        &settings.database,
        "ctx_decision_audits",
        &[("event_id", AGENT_EVENT_ID_LEN as u64)],
    )
    .await?;
    for removed_index in [
        "idx_ctx_decisions_session_type_created",
        "idx_ctx_decisions_event_id",
        "idx_ctx_decisions_context_capture_id",
    ] {
        drop_index_if_present(
            &pool,
            &settings.database,
            "ctx_decision_audits",
            removed_index,
        )
        .await?;
    }
    for (index, expected_columns, ddl) in [
        (
            "idx_ctx_decisions_owner_session_type_created",
            &["user_id", "session_id", "decision_type", "created_at"][..],
            "ALTER TABLE ctx_decision_audits ADD INDEX idx_ctx_decisions_owner_session_type_created (user_id, session_id, decision_type, created_at)",
        ),
        (
            "idx_ctx_decisions_owner_event_id",
            &["user_id", "event_id"][..],
            "ALTER TABLE ctx_decision_audits ADD INDEX idx_ctx_decisions_owner_event_id (user_id, event_id)",
        ),
        (
            "idx_ctx_decisions_owner_context_capture_id",
            &["user_id", "context_capture_id"][..],
            "ALTER TABLE ctx_decision_audits ADD INDEX idx_ctx_decisions_owner_context_capture_id (user_id, context_capture_id)",
        ),
    ] {
        ensure_index_shape(
            &pool,
            &settings.database,
            "ctx_decision_audits",
            index,
            expected_columns,
            ddl,
        )
        .await?;
    }

    fail_if_required_columns_missing_or_nullable(
        &pool,
        &settings.database,
        "skill_selection_events",
        &["user_id"],
    )
    .await?;
    query(
        "CREATE TABLE IF NOT EXISTS skill_selection_events (
            event_id VARCHAR(64) PRIMARY KEY,
            session_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            agent_id VARCHAR(255) NULL,
            user_query LONGTEXT NULL,
            selected_skills JSON NULL,
            skill_name VARCHAR(255) NULL,
            skill_version VARCHAR(64) NULL,
            selection_method VARCHAR(64) NULL,
            execution_success BIGINT NULL,
            execution_time_ms BIGINT NULL,
            user_feedback_score BIGINT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_skill_selection_owner_session_created (user_id, session_id, created_at),
            INDEX idx_skill_selection_user_created (user_id, created_at),
            INDEX idx_skill_selection_skill_created (skill_name, created_at)
        )",
    )
    .execute(&pool)
    .await?;
    drop_index_if_present(
        &pool,
        &settings.database,
        "skill_selection_events",
        "idx_skill_selection_session_created",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "skill_selection_events",
        "idx_skill_selection_owner_session_created",
        &["user_id", "session_id", "created_at"],
        "ALTER TABLE skill_selection_events ADD INDEX idx_skill_selection_owner_session_created (user_id, session_id, created_at)",
    )
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS infra_llm_models (
            model_id VARCHAR(64) PRIMARY KEY,
            model_name VARCHAR(100) NOT NULL UNIQUE,
            provider VARCHAR(50) NOT NULL,
            api_key_encrypted TEXT NULL,
            base_url VARCHAR(500) NULL,
            description TEXT NULL,
            is_active SMALLINT NOT NULL DEFAULT 1,
            context_window INT NOT NULL,
            max_completion_tokens INT NULL,
            input_modalities JSON NOT NULL,
            output_modalities JSON NOT NULL,
            supported_parameters JSON NOT NULL,
            pricing JSON NOT NULL,
            architecture VARCHAR(100) NULL,
            tags JSON NOT NULL,
            quirks JSON NOT NULL,
            thinking_capability VARCHAR(20) NULL,
            thinking_probe_error TEXT NULL,
            created_by VARCHAR(128) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            CONSTRAINT chk_infra_llm_models_context_window CHECK (context_window > 0),
            INDEX idx_infra_llm_models_active_provider_name (is_active, provider, model_name)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS model_gateways (
            id VARCHAR(128) PRIMARY KEY,
            resolve_url LONGTEXT NOT NULL,
            model_protocol VARCHAR(64) NOT NULL,
            status VARCHAR(32) NOT NULL DEFAULT 'active',
            metadata_json LONGTEXT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            disabled_at DATETIME(6) NULL,
            INDEX idx_model_gateways_status_created (status, created_at)
        )",
    )
    .execute(&pool)
    .await?;

    // Server-wide admin config KV store. Holds settings that the admin explicitly manages
    // via `astra admin config set/get/unset` (first key: `reasoning_model_name`).
    query(
        "CREATE TABLE IF NOT EXISTS admin_config (
            config_key VARCHAR(100) PRIMARY KEY,
            config_value TEXT NOT NULL,
            updated_by VARCHAR(128) NULL,
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS user_preferences (
            pref_id VARCHAR(64) PRIMARY KEY,
            user_id VARCHAR(128) NOT NULL,
            pref_key VARCHAR(100) NOT NULL,
            pref_value LONGTEXT NOT NULL,
            version INT NOT NULL DEFAULT 1,
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            UNIQUE KEY idx_prefs_user_key (user_id, pref_key)
        )",
    )
    .execute(&pool)
    .await?;

    query("DROP TABLE IF EXISTS session_sync_log")
        .execute(&pool)
        .await?;

    // Skills registry — master catalog for database-backed skills.
    //
    // Important visibility contract:
    // - Local filesystem skills discovered by the CLI stay local to the CLI.
    // - Web/runtime skills must live in this table.
    // - Query paths expose only `created_by = current_user OR is_public = 1`.
    //
    // The paired visibility indexes below support that union without forcing
    // MatrixOne to scan every active skill when a user has many private skills
    // and the public catalog is also large.
    query(
        "CREATE TABLE IF NOT EXISTS skills_registry (
            skill_id VARCHAR(64) PRIMARY KEY,
            skill_name VARCHAR(255) NOT NULL,
            version VARCHAR(64) NOT NULL,
            description TEXT NULL,
            skill_definition JSON NULL,
            dependencies JSON NULL,
            manifest JSON NULL,
            publisher_id VARCHAR(255) NULL,
            publisher_verified SMALLINT NOT NULL DEFAULT 0,
            trust_tier VARCHAR(32) NULL,
            min_runtime_version VARCHAR(50) NULL,
            compatibility_platforms JSON NULL,
            category VARCHAR(64) NULL,
            priority INT NULL,
            is_active SMALLINT NOT NULL DEFAULT 1,
            status VARCHAR(20) NOT NULL DEFAULT 'active',
            source VARCHAR(50) NOT NULL DEFAULT 'user',
            is_public SMALLINT NOT NULL DEFAULT 0,
            created_by VARCHAR(128) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            UNIQUE KEY uq_skill_name_version (skill_name, version),
            INDEX idx_skill_active_name (is_active, status, skill_name),
            INDEX idx_skill_active_created_at (is_active, created_at),
            INDEX idx_skill_visible_owner (is_active, created_by, created_at),
            INDEX idx_skill_visible_public (is_active, is_public, created_at),
            INDEX idx_skill_visible_name_owner (is_active, skill_name, created_by, is_public, created_at),
            INDEX idx_skill_source_name (source, skill_name),
            INDEX idx_skill_active_name_ver (is_active, skill_name, version)
        )",
    )
    .execute(&pool)
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "skills_registry",
        "idx_skill_active_name_ver",
        &["is_active", "skill_name", "version"],
        "ALTER TABLE skills_registry ADD INDEX idx_skill_active_name_ver (is_active, skill_name, version)",
    )
    .await?;
    query(
        "CREATE TABLE IF NOT EXISTS skill_metrics (
            metric_id            VARCHAR(255) PRIMARY KEY,
            skill_name           VARCHAR(255) NOT NULL,
            metric_type          VARCHAR(32) NOT NULL,
            metric_slot          VARCHAR(255) NOT NULL,
            skill_version        VARCHAR(50) NULL,
            runtime_version      VARCHAR(50) NULL,
            publisher_id         VARCHAR(255) NULL,
            total_installs       BIGINT NOT NULL DEFAULT 0,
            active_users_7d      INT NOT NULL DEFAULT 0,
            avg_quality          FLOAT NOT NULL DEFAULT 0.0,
            avg_rating           FLOAT NOT NULL DEFAULT 0.0,
            report_count         INT NOT NULL DEFAULT 0,
            compatibility_score  FLOAT NOT NULL DEFAULT 0.0,
            trust_tier           VARCHAR(32) NULL,
            success_rate         FLOAT NULL,
            avg_tokens           FLOAT NULL,
            invocation_count     INT NULL,
            created_at           DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at           DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            UNIQUE KEY uq_skill_metrics_slot (skill_name, metric_type, metric_slot),
            INDEX idx_skill_metrics_lookup (skill_name, metric_type, created_at),
            INDEX idx_skill_metrics_ranking (metric_type, avg_quality, active_users_7d),
            INDEX idx_skill_metrics_tier (metric_type, trust_tier, updated_at)
        )",
    )
    .execute(&pool)
    .await?;
    // ── Long-task orchestration (Phase H) ──

    query(
        "CREATE TABLE IF NOT EXISTS agent_tasks (
            task_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(64) NULL,
            agent_id VARCHAR(255) NULL,
            parent_task_id VARCHAR(64) NULL,
            title VARCHAR(500) NOT NULL,
            description LONGTEXT NULL,
            status VARCHAR(20) NOT NULL DEFAULT 'pending',
            progress_pct INT NOT NULL DEFAULT 0,
            items_done INT NOT NULL DEFAULT 0,
            items_total INT NOT NULL DEFAULT 0,
            plan_json LONGTEXT NULL,
            checkpoint_json LONGTEXT NULL,
            error_message TEXT NULL,
            user_rating TINYINT NULL,
            completion_time_sec INT NULL,
            replan_count INT NOT NULL DEFAULT 0,
            auto_adjustments INT NOT NULL DEFAULT 0,
            outcome VARCHAR(20) NULL,
            project_type VARCHAR(50) NULL,
            goal_pattern VARCHAR(500) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            completed_at DATETIME(6) NULL,
            PRIMARY KEY (user_id, task_id),
            INDEX idx_tasks_user_status_updated (user_id, status, updated_at),
            INDEX idx_tasks_user_updated (user_id, updated_at),
            INDEX idx_tasks_owner_session_updated (user_id, session_id, updated_at)
        )",
    )
    .execute(&pool)
    .await?;
    for removed_index in ["idx_tasks_session_updated", "idx_tasks_parent_updated"] {
        drop_index_if_present(&pool, &settings.database, "agent_tasks", removed_index).await?;
    }
    // Upgrade: old schema used single-column PK (task_id). Rebuild to composite.
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "agent_tasks",
        &["user_id", "task_id"],
        "ALTER TABLE agent_tasks ADD PRIMARY KEY (user_id, task_id)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "agent_tasks",
        "idx_tasks_owner_session_updated",
        &["user_id", "session_id", "updated_at"],
        "ALTER TABLE agent_tasks ADD INDEX idx_tasks_owner_session_updated (user_id, session_id, updated_at)",
    )
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS edge_agent_registry (
            user_id VARCHAR(128) NOT NULL,
            registry_id VARCHAR(64) NOT NULL,
            edge_agent_id VARCHAR(255) NOT NULL,
            edge_id VARCHAR(128) NOT NULL,
            hostname VARCHAR(255) NULL,
            worktree_path VARCHAR(512) NULL,
            capabilities_json TEXT NULL,
            workspace_id VARCHAR(512) NULL,
            registered_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            last_heartbeat_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, registry_id),
            UNIQUE KEY uq_edge_registry_user_agent (user_id, edge_agent_id),
            INDEX idx_edge_registry_user_heartbeat (user_id, last_heartbeat_at),
            INDEX idx_edge_registry_agent_workspace (edge_agent_id, workspace_id)
        )",
    )
    .execute(&pool)
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "edge_agent_registry",
        &["user_id", "registry_id"],
        "ALTER TABLE edge_agent_registry ADD PRIMARY KEY (user_id, registry_id)",
    )
    .await?;
    add_column_if_missing(
        &pool,
        &settings.database,
        "edge_agent_registry",
        "workspace_id",
        "ALTER TABLE edge_agent_registry ADD COLUMN workspace_id VARCHAR(512) NULL",
    )
    .await?;
    add_index_if_missing(
        &pool,
        &settings.database,
        "edge_agent_registry",
        "idx_edge_registry_agent_workspace",
        "ALTER TABLE edge_agent_registry ADD INDEX idx_edge_registry_agent_workspace (edge_agent_id, workspace_id)",
    )
    .await?;

    migrate_legacy_edge_pending_dispatch_if_needed(&pool, &settings.database).await?;
    query(
        "CREATE TABLE IF NOT EXISTS edge_pending_dispatch (
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(128) NOT NULL,
            run_id VARCHAR(128) NOT NULL,
            turn_chain_id VARCHAR(128) NOT NULL,
            edge_agent_id VARCHAR(255) NOT NULL,
            request_id VARCHAR(128) NOT NULL,
            payload_json JSON NOT NULL,
            result_json JSON NULL,
            status VARCHAR(16) NOT NULL DEFAULT 'pending',
            pod_id VARCHAR(128) NULL,
            dispatched_at DATETIME(6) NULL,
            completed_at DATETIME(6) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, session_id, run_id, turn_chain_id, request_id),
            INDEX idx_edge_dispatch_user_status (user_id, edge_agent_id, status, created_at, session_id, run_id, turn_chain_id, request_id),
            INDEX idx_edge_dispatch_created (created_at)
        )",
    )
    .execute(&pool)
    .await?;
    fail_if_obsolete_shape(
        &pool,
        &settings.database,
        "edge_pending_dispatch",
        EDGE_PENDING_DISPATCH_IDENTITY_COLUMNS,
        &["dispatch_id"],
        &["uq_edge_dispatch_owner_request"],
    )
    .await?;
    drop_index_if_present(
        &pool,
        &settings.database,
        "edge_pending_dispatch",
        "uq_edge_dispatch_request_id",
    )
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "edge_pending_dispatch",
        EDGE_PENDING_DISPATCH_IDENTITY_COLUMNS,
        "ALTER TABLE edge_pending_dispatch ADD PRIMARY KEY (user_id, session_id, run_id, turn_chain_id, request_id)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "edge_pending_dispatch",
        "idx_edge_dispatch_user_status",
        &[
            "user_id",
            "edge_agent_id",
            "status",
            "created_at",
            "session_id",
            "run_id",
            "turn_chain_id",
            "request_id",
        ],
        "ALTER TABLE edge_pending_dispatch ADD INDEX idx_edge_dispatch_user_status (user_id, edge_agent_id, status, created_at, session_id, run_id, turn_chain_id, request_id)",
    )
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS agent_bindings (
            id VARCHAR(64) PRIMARY KEY,
            binding_name VARCHAR(255) NOT NULL,
            idempotency_key VARCHAR(255) NOT NULL,
            status VARCHAR(32) NOT NULL DEFAULT 'active',
            agent_md LONGTEXT NOT NULL,
            capability_servers_json LONGTEXT NOT NULL,
            runtime_policy_json LONGTEXT NOT NULL,
            metadata_json LONGTEXT NULL,
            binding_schema_version VARCHAR(32) NOT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            disabled_at DATETIME(6) NULL,
            UNIQUE KEY uq_agent_bindings_name (binding_name),
            UNIQUE KEY uq_agent_bindings_idempotency_key (idempotency_key),
            INDEX idx_agent_bindings_status_created (status, created_at)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS mcp_servers (
            id VARCHAR(64) NOT NULL,
            owner_user_id VARCHAR(128) NOT NULL,
            name VARCHAR(128) NOT NULL,
            description TEXT NULL,
            transport VARCHAR(32) NOT NULL,
            url TEXT NOT NULL,
            is_active SMALLINT NOT NULL DEFAULT 1,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (owner_user_id, id),
            UNIQUE KEY uq_mcp_servers_owner_name (owner_user_id, name),
            INDEX idx_mcp_servers_owner_active (owner_user_id, is_active, updated_at)
        )",
    )
    .execute(&pool)
    .await?;
    fail_if_varchar_columns_shorter_than(&pool, &settings.database, "mcp_servers", &[("id", 64)])
        .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "mcp_servers",
        &["owner_user_id", "id"],
        "ALTER TABLE mcp_servers ADD PRIMARY KEY (owner_user_id, id)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "mcp_servers",
        "uq_mcp_servers_owner_name",
        &["owner_user_id", "name"],
        "ALTER TABLE mcp_servers ADD UNIQUE INDEX uq_mcp_servers_owner_name (owner_user_id, name)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "mcp_servers",
        "idx_mcp_servers_owner_active",
        &["owner_user_id", "is_active", "updated_at"],
        "ALTER TABLE mcp_servers ADD INDEX idx_mcp_servers_owner_active (owner_user_id, is_active, updated_at)",
    )
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS mcp_bindings (
            id VARCHAR(64) NOT NULL,
            owner_user_id VARCHAR(128) NOT NULL,
            mcp_id VARCHAR(64) NOT NULL,
            key_hash VARCHAR(128) NOT NULL,
            key_value_encrypted TEXT NOT NULL,
            comment TEXT NULL,
            is_active SMALLINT NOT NULL DEFAULT 1,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (owner_user_id, id),
            UNIQUE KEY uq_mcp_bindings_owner_mcp_key (owner_user_id, mcp_id, key_hash),
            INDEX idx_mcp_bindings_owner_active (owner_user_id, is_active, updated_at),
            INDEX idx_mcp_bindings_owner_mcp (owner_user_id, mcp_id)
        )",
    )
    .execute(&pool)
    .await?;
    fail_if_varchar_columns_shorter_than(
        &pool,
        &settings.database,
        "mcp_bindings",
        &[("id", 64), ("mcp_id", 64)],
    )
    .await?;
    drop_index_if_present(
        &pool,
        &settings.database,
        "mcp_bindings",
        "idx_mcp_bindings_mcp_id",
    )
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "mcp_bindings",
        &["owner_user_id", "id"],
        "ALTER TABLE mcp_bindings ADD PRIMARY KEY (owner_user_id, id)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "mcp_bindings",
        "uq_mcp_bindings_owner_mcp_key",
        &["owner_user_id", "mcp_id", "key_hash"],
        "ALTER TABLE mcp_bindings ADD UNIQUE INDEX uq_mcp_bindings_owner_mcp_key (owner_user_id, mcp_id, key_hash)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "mcp_bindings",
        "idx_mcp_bindings_owner_active",
        &["owner_user_id", "is_active", "updated_at"],
        "ALTER TABLE mcp_bindings ADD INDEX idx_mcp_bindings_owner_active (owner_user_id, is_active, updated_at)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "mcp_bindings",
        "idx_mcp_bindings_owner_mcp",
        &["owner_user_id", "mcp_id"],
        "ALTER TABLE mcp_bindings ADD INDEX idx_mcp_bindings_owner_mcp (owner_user_id, mcp_id)",
    )
    .await?;
    query(
        "CREATE TABLE IF NOT EXISTS mcp_tools (
            owner_user_id VARCHAR(128) NOT NULL,
            binding_id VARCHAR(64) NOT NULL,
            tool_name VARCHAR(256) NOT NULL,
            public_name VARCHAR(384) NOT NULL,
            description TEXT NULL,
            input_schema_json JSON NULL,
            output_schema_json JSON NULL,
            schema_hash VARCHAR(128) NOT NULL,
            discovered_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (owner_user_id, binding_id, tool_name),
            UNIQUE KEY uq_mcp_tools_owner_binding_public (owner_user_id, binding_id, public_name),
            INDEX idx_mcp_tools_owner_binding (owner_user_id, binding_id)
        )",
    )
    .execute(&pool)
    .await?;
    fail_if_obsolete_shape(
        &pool,
        &settings.database,
        "mcp_tools",
        &["owner_user_id", "binding_id", "tool_name"],
        &["id"],
        &["uq_mcp_tools_binding_tool", "uq_mcp_tools_binding_public"],
    )
    .await?;
    fail_if_varchar_columns_shorter_than(
        &pool,
        &settings.database,
        "mcp_tools",
        &[("binding_id", 64)],
    )
    .await?;
    drop_index_if_present(
        &pool,
        &settings.database,
        "mcp_tools",
        "idx_mcp_tools_binding",
    )
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "mcp_tools",
        &["owner_user_id", "binding_id", "tool_name"],
        "ALTER TABLE mcp_tools ADD PRIMARY KEY (owner_user_id, binding_id, tool_name)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "mcp_tools",
        "uq_mcp_tools_owner_binding_public",
        &["owner_user_id", "binding_id", "public_name"],
        "ALTER TABLE mcp_tools ADD UNIQUE INDEX uq_mcp_tools_owner_binding_public (owner_user_id, binding_id, public_name)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "mcp_tools",
        "idx_mcp_tools_owner_binding",
        &["owner_user_id", "binding_id"],
        "ALTER TABLE mcp_tools ADD INDEX idx_mcp_tools_owner_binding (owner_user_id, binding_id)",
    )
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS task_leases (
            task_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            holder_agent_id VARCHAR(255) NOT NULL,
            holder_edge_id VARCHAR(128) NULL,
            expires_at DATETIME(6) NOT NULL,
            lease_version BIGINT NOT NULL DEFAULT 1,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, task_id),
            INDEX idx_task_leases_expires (expires_at)
        )",
    )
    .execute(&pool)
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "task_leases",
        &["user_id", "task_id"],
        "ALTER TABLE task_leases ADD PRIMARY KEY (user_id, task_id)",
    )
    .await?;
    drop_index_if_present(
        &pool,
        &settings.database,
        "task_leases",
        "idx_task_leases_user_expires",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "task_leases",
        "idx_task_leases_expires",
        &["expires_at"],
        "ALTER TABLE task_leases ADD INDEX idx_task_leases_expires (expires_at)",
    )
    .await?;

    // ── Plan templates table (learning successful patterns) ──
    query(
        "CREATE TABLE IF NOT EXISTS plan_templates (
            template_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            goal_pattern VARCHAR(500) NOT NULL,
            project_type VARCHAR(50) NULL,
            template_json LONGTEXT NOT NULL,
            success_rate FLOAT NOT NULL DEFAULT 0.0,
            avg_completion_time INT NULL,
            use_count INT NOT NULL DEFAULT 0,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, template_id),
            INDEX idx_tpl_user_goal_project (user_id, goal_pattern, project_type),
            INDEX idx_tpl_project_success (project_type, success_rate)
        )",
    )
    .execute(&pool)
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "plan_templates",
        &["user_id", "template_id"],
        "ALTER TABLE plan_templates ADD PRIMARY KEY (user_id, template_id)",
    )
    .await?;

    // ── Plans: cloud-authoritative plan state (user-owned, session-linked) ──
    // `subtask_count` is denormalized so list endpoints don't need to parse
    // `plan_json` just to render a card. Maintained by `PlanRepository::save`.
    query(
        "CREATE TABLE IF NOT EXISTS plans (
            user_id       VARCHAR(128) NOT NULL,
            plan_id       VARCHAR(64) NOT NULL,
            session_id    VARCHAR(64) NULL,
            goal          TEXT NOT NULL,
            phase         VARCHAR(32) NOT NULL,
            version       BIGINT NOT NULL DEFAULT 0,
            plan_json     LONGTEXT NOT NULL,
            plan_md       LONGTEXT NULL,
            progress_pct  INT NOT NULL DEFAULT 0,
            subtask_count INT NOT NULL DEFAULT 0,
            created_by    VARCHAR(128) NULL,
            created_at    DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at    DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, plan_id),
            INDEX idx_plans_user_updated (user_id, updated_at DESC),
            INDEX idx_plans_owner_session_updated (user_id, session_id, updated_at DESC),
            INDEX idx_plans_user_phase (user_id, phase)
        )",
    )
    .execute(&pool)
    .await?;
    drop_index_if_present(&pool, &settings.database, "plans", "idx_plans_session").await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "plans",
        &["user_id", "plan_id"],
        "ALTER TABLE plans ADD PRIMARY KEY (user_id, plan_id)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "plans",
        "idx_plans_owner_session_updated",
        &["user_id", "session_id", "updated_at"],
        "ALTER TABLE plans ADD INDEX idx_plans_owner_session_updated (user_id, session_id, updated_at DESC)",
    )
    .await?;

    // ── Plan step runs: append-only attempt chain for every subtask ──
    query(
        "CREATE TABLE IF NOT EXISTS plan_step_runs (
            run_id       VARCHAR(64) NOT NULL,
            user_id      VARCHAR(128) NOT NULL,
            plan_id      VARCHAR(64) NOT NULL,
            subtask_id   VARCHAR(64) NOT NULL,
            attempt      INT NOT NULL,
            status       VARCHAR(16) NOT NULL,
            session_id   VARCHAR(64) NOT NULL,
            started_at   DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            finished_at  DATETIME(6) NULL,
            request_id   VARCHAR(64) NOT NULL,
            error        TEXT NULL,
            artifact_ref VARCHAR(255) NULL,
            PRIMARY KEY (user_id, run_id),
            INDEX idx_step_runs_plan_started (user_id, plan_id, started_at DESC),
            UNIQUE KEY uq_step_runs_subtask_attempt (user_id, plan_id, subtask_id, attempt)
        )",
    )
    .execute(&pool)
    .await?;
    drop_index_if_present(
        &pool,
        &settings.database,
        "plan_step_runs",
        "idx_step_runs_session",
    )
    .await?;
    // Upgrade: old schema used single-column PK (run_id). Rebuild to composite.
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "plan_step_runs",
        &["user_id", "run_id"],
        "ALTER TABLE plan_step_runs ADD PRIMARY KEY (user_id, run_id)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "plan_step_runs",
        "idx_step_runs_plan_started",
        &["user_id", "plan_id", "started_at"],
        "ALTER TABLE plan_step_runs ADD INDEX idx_step_runs_plan_started (user_id, plan_id, started_at DESC)",
    )
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS session_checkpoints (
            checkpoint_id VARCHAR(64) NOT NULL,
            session_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            number INT NOT NULL,
            turn INT NOT NULL,
            title VARCHAR(500) NULL,
            summary LONGTEXT NULL,
            tools_json JSON NULL,
            state_json LONGTEXT NULL,
            contract_state_json LONGTEXT NULL,
            total_tokens BIGINT NOT NULL DEFAULT 0,
            had_stalls SMALLINT NOT NULL DEFAULT 0,
            error_count INT NOT NULL DEFAULT 0,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, session_id, checkpoint_id),
            UNIQUE KEY uq_session_checkpoints_owner_number (user_id, session_id, number),
            INDEX idx_ckpt_owner_session_turn (user_id, session_id, turn),
            INDEX idx_ckpt_user_created (user_id, created_at)
        )",
    )
    .execute(&pool)
    .await?;
    for removed_index in ["idx_ckpt_session_number", "idx_ckpt_session_turn"] {
        drop_index_if_present(
            &pool,
            &settings.database,
            "session_checkpoints",
            removed_index,
        )
        .await?;
    }
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "session_checkpoints",
        &["user_id", "session_id", "checkpoint_id"],
        "ALTER TABLE session_checkpoints ADD PRIMARY KEY (user_id, session_id, checkpoint_id)",
    )
    .await?;
    for (index, expected_columns, ddl) in [
        (
            "uq_session_checkpoints_owner_number",
            &["user_id", "session_id", "number"][..],
            "ALTER TABLE session_checkpoints ADD UNIQUE KEY uq_session_checkpoints_owner_number (user_id, session_id, number)",
        ),
        (
            "idx_ckpt_owner_session_turn",
            &["user_id", "session_id", "turn"][..],
            "ALTER TABLE session_checkpoints ADD INDEX idx_ckpt_owner_session_turn (user_id, session_id, turn)",
        ),
        (
            "idx_ckpt_user_created",
            &["user_id", "created_at"][..],
            "ALTER TABLE session_checkpoints ADD INDEX idx_ckpt_user_created (user_id, created_at)",
        ),
    ] {
        ensure_index_shape(
            &pool,
            &settings.database,
            "session_checkpoints",
            index,
            expected_columns,
            ddl,
        )
        .await?;
    }

    query(
        "CREATE TABLE IF NOT EXISTS session_artifacts (
            artifact_id VARCHAR(64) NOT NULL,
            session_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            project_id VARCHAR(128) NULL,
            owner_run_id VARCHAR(128) NULL,
            owner_delegation_id VARCHAR(128) NULL,
            root_run_id VARCHAR(128) NULL,
            artifact_kind VARCHAR(64) NOT NULL,
            source VARCHAR(64) NULL,
            turn INT NULL,
            round INT NULL,
            content_json LONGTEXT NOT NULL,
            metadata JSON NULL,
            access_scope VARCHAR(32) NOT NULL DEFAULT 'delegation',
            retention_policy VARCHAR(32) NOT NULL DEFAULT 'default',
            retention_until DATETIME(6) NULL,
            status VARCHAR(32) NOT NULL DEFAULT 'active',
            normalize_version VARCHAR(16) NULL,
            cold_storage_ref VARCHAR(255) NULL,
            derived_from_artifact_id VARCHAR(128) NULL,
            referenced_by_manifest_count INT NOT NULL DEFAULT 0,
            referenced_by_state_items_count INT NOT NULL DEFAULT 0,
            referenced_by_citation_count INT NOT NULL DEFAULT 0,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            CONSTRAINT chk_session_artifacts_access_scope CHECK (access_scope IN ('private', 'delegation', 'delegation_direct', 'same_root_tree', 'user')),
            CONSTRAINT chk_session_artifacts_retention_policy CHECK (retention_policy IN ('default', 'permanent', 'project_long_term')),
            CONSTRAINT chk_session_artifacts_status CHECK (status IN ('active', 'expiring', 'expired')),
            PRIMARY KEY (user_id, session_id, artifact_id),
            INDEX idx_session_artifacts_owner_kind_order (user_id, session_id, artifact_kind, created_at, artifact_id),
            INDEX idx_session_artifacts_owner_session_order (user_id, session_id, created_at, artifact_id),
            INDEX idx_session_artifacts_owner_source_order (user_id, session_id, source, created_at, artifact_id),
            INDEX idx_session_artifacts_owner_created (user_id, created_at, session_id, artifact_id),
            INDEX idx_artifacts_root_scope (user_id, root_run_id, access_scope, status, updated_at, artifact_id),
            INDEX idx_artifacts_owner_run (user_id, owner_run_id, status, updated_at, artifact_id),
            INDEX idx_artifacts_retention (status, retention_until, retention_policy, user_id, session_id, artifact_id),
            INDEX idx_artifacts_project (user_id, project_id, status, retention_until, artifact_id),
            INDEX idx_artifacts_derived (user_id, session_id, derived_from_artifact_id)
        )",
    )
    .execute(&pool)
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "session_artifacts",
        &["user_id", "session_id", "artifact_id"],
        "ALTER TABLE session_artifacts ADD PRIMARY KEY (user_id, session_id, artifact_id)",
    )
    .await?;
    for (index, expected_columns, ddl) in [
        (
            "idx_session_artifacts_owner_kind_order",
            &[
                "user_id",
                "session_id",
                "artifact_kind",
                "created_at",
                "artifact_id",
            ][..],
            "ALTER TABLE session_artifacts ADD INDEX idx_session_artifacts_owner_kind_order (user_id, session_id, artifact_kind, created_at, artifact_id)",
        ),
        (
            "idx_session_artifacts_owner_session_order",
            &["user_id", "session_id", "created_at", "artifact_id"][..],
            "ALTER TABLE session_artifacts ADD INDEX idx_session_artifacts_owner_session_order (user_id, session_id, created_at, artifact_id)",
        ),
        (
            "idx_session_artifacts_owner_source_order",
            &[
                "user_id",
                "session_id",
                "source",
                "created_at",
                "artifact_id",
            ][..],
            "ALTER TABLE session_artifacts ADD INDEX idx_session_artifacts_owner_source_order (user_id, session_id, source, created_at, artifact_id)",
        ),
        (
            "idx_artifacts_root_scope",
            &[
                "user_id",
                "root_run_id",
                "access_scope",
                "status",
                "updated_at",
                "artifact_id",
            ][..],
            "ALTER TABLE session_artifacts ADD INDEX idx_artifacts_root_scope (user_id, root_run_id, access_scope, status, updated_at, artifact_id)",
        ),
    ] {
        ensure_index_shape(
            &pool,
            &settings.database,
            "session_artifacts",
            index,
            expected_columns,
            ddl,
        )
        .await?;
    }
    ensure_index_shape(
        &pool,
        &settings.database,
        "session_artifacts",
        "idx_artifacts_retention",
        &[
            "status",
            "retention_until",
            "retention_policy",
            "user_id",
            "session_id",
            "artifact_id",
        ],
        "ALTER TABLE session_artifacts ADD INDEX idx_artifacts_retention (status, retention_until, retention_policy, user_id, session_id, artifact_id)",
    )
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS session_artifact_references (
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(64) NOT NULL,
            artifact_id VARCHAR(64) NOT NULL,
            reference_kind VARCHAR(32) NOT NULL,
            reference_id VARCHAR(128) NOT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            CONSTRAINT chk_artifact_reference_kind CHECK (
                reference_kind IN ('invocation_ledger', 'manifest', 'state_item', 'citation')
            ),
            PRIMARY KEY (user_id, session_id, artifact_id, reference_kind, reference_id),
            INDEX idx_artifact_references_owner_reference
                (user_id, session_id, reference_kind, reference_id, artifact_id)
        )",
    )
    .execute(&pool)
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "session_artifact_references",
        &[
            "user_id",
            "session_id",
            "artifact_id",
            "reference_kind",
            "reference_id",
        ],
        "ALTER TABLE session_artifact_references ADD PRIMARY KEY (user_id, session_id, artifact_id, reference_kind, reference_id)",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "session_artifact_references",
        "idx_artifact_references_owner_reference",
        &[
            "user_id",
            "session_id",
            "reference_kind",
            "reference_id",
            "artifact_id",
        ],
        "ALTER TABLE session_artifact_references ADD INDEX idx_artifact_references_owner_reference (user_id, session_id, reference_kind, reference_id, artifact_id)",
    )
    .await?;

    for (table, column, ddl) in [
        (
            "agent_sessions",
            "project_id",
            "ALTER TABLE agent_sessions ADD COLUMN project_id VARCHAR(128) NULL",
        ),
        (
            "agent_sessions",
            "project_retention_policy",
            "ALTER TABLE agent_sessions ADD COLUMN project_retention_policy VARCHAR(32) NOT NULL DEFAULT 'session'",
        ),
    ] {
        if let Err(e) = add_column_if_missing(&pool, &settings.database, table, column, ddl).await {
            tracing::warn!("core schema additive column skipped: {table}.{column}: {e}");
        }
    }

    for (table, index, ddl) in [
        (
            "agent_sessions",
            "idx_sessions_project",
            "ALTER TABLE agent_sessions ADD INDEX idx_sessions_project (user_id, project_id, updated_at)",
        ),
        (
            "agent_events",
            "idx_agent_events_owner_session_turn",
            AGENT_EVENTS_OWNER_SESSION_TURN_INDEX_ALTER_SQL,
        ),
    ] {
        if let Err(e) = add_index_if_missing(&pool, &settings.database, table, index, ddl).await {
            tracing::debug!("phase4 additive index migration skipped: {table}.{index}: {e}");
        }
    }

    // Session task scratchpad (Tier 1 — reference-agent-style task board).
    // Authoritative store for the live task board. Edge and cloud hosts read
    // the same rows for a given owner/session pair; per-host `TaskManager`
    // instances are caches over this table. The uniqueness boundary is
    // owner-first, matching the rest of the session schema and avoiding
    // session-id-only ownership assumptions.
    query(
        "CREATE TABLE IF NOT EXISTS session_todos (
            session_id VARCHAR(64) NOT NULL,
            todo_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            ordinal INT NOT NULL,
            title VARCHAR(512) NOT NULL,
            description TEXT NULL,
            active_form VARCHAR(512) NULL,
            status VARCHAR(16) NOT NULL,
            owner VARCHAR(128) NULL,
            metadata LONGTEXT NULL,
            blocks LONGTEXT NULL,
            blocked_by LONGTEXT NULL,
            subtasks LONGTEXT NULL,
            archived_at DATETIME(6) NULL,
            created_at DATETIME(6) NOT NULL,
            updated_at DATETIME(6) NOT NULL,
            PRIMARY KEY (user_id, session_id, todo_id),
            INDEX idx_session_todos_owner_session_ordinal (user_id, session_id, ordinal),
            INDEX idx_session_todos_owner_session_status_updated (user_id, session_id, status, updated_at),
            INDEX idx_session_todos_user_status_updated (user_id, status, updated_at)
        )",
    )
    .execute(&pool)
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "session_todos",
        &["user_id", "session_id", "todo_id"],
        "ALTER TABLE session_todos ADD PRIMARY KEY (user_id, session_id, todo_id)",
    )
    .await?;
    for removed_index in [
        "idx_session_todos_session_status_updated",
        "idx_session_todos_status_updated_owner",
        "idx_session_todos_archived_gc_owner",
    ] {
        drop_index_if_present(&pool, &settings.database, "session_todos", removed_index).await?;
    }
    for (index, expected_columns, ddl) in [
        (
            "idx_session_todos_owner_session_ordinal",
            &["user_id", "session_id", "ordinal"][..],
            "ALTER TABLE session_todos ADD INDEX idx_session_todos_owner_session_ordinal (user_id, session_id, ordinal)",
        ),
        (
            "idx_session_todos_owner_session_status_updated",
            &["user_id", "session_id", "status", "updated_at"][..],
            "ALTER TABLE session_todos ADD INDEX idx_session_todos_owner_session_status_updated (user_id, session_id, status, updated_at)",
        ),
        (
            "idx_session_todos_user_status_updated",
            &["user_id", "status", "updated_at"][..],
            "ALTER TABLE session_todos ADD INDEX idx_session_todos_user_status_updated (user_id, status, updated_at)",
        ),
    ] {
        ensure_index_shape(
            &pool,
            &settings.database,
            "session_todos",
            index,
            expected_columns,
            ddl,
        )
        .await?;
    }

    // Per owner/session monotonic counter used to mint `task-<n>` ids. Kept in
    // a separate table (not on `session_todos`) because a todo can be deleted
    // but its id must never be reused for that owner/session board.
    query(
        "CREATE TABLE IF NOT EXISTS session_todo_counters (
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(64) NOT NULL,
            next_id BIGINT NOT NULL,
            version BIGINT NOT NULL DEFAULT 0,
            PRIMARY KEY (user_id, session_id)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS session_todo_idempotency (
            session_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            action VARCHAR(32) NOT NULL,
            idempotency_key VARCHAR(128) NOT NULL,
            args_json LONGTEXT NOT NULL,
            output LONGTEXT NULL,
            created_at DATETIME(6) NOT NULL,
            updated_at DATETIME(6) NOT NULL,
            PRIMARY KEY (user_id, session_id, action, idempotency_key)
        )",
    )
    .execute(&pool)
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "session_todo_idempotency",
        &["user_id", "session_id", "action", "idempotency_key"],
        "ALTER TABLE session_todo_idempotency ADD PRIMARY KEY (user_id, session_id, action, idempotency_key)",
    )
    .await?;

    // ── Durable Task System ─────────────────────────────────────────────────

    // Task contracts: verifiable acceptance criteria for long-term tasks
    query(
        "CREATE TABLE IF NOT EXISTS task_contracts (
            contract_id    VARCHAR(64) NOT NULL,
            task_id        VARCHAR(64) NOT NULL,
            session_id     VARCHAR(64) NOT NULL,
            user_id        VARCHAR(128) NOT NULL,
            goal           TEXT NOT NULL,
            scope_json     JSON,
            subtasks_json  JSON NOT NULL,
            criteria_json  JSON NOT NULL,
            version        INT NOT NULL DEFAULT 1,
            status         VARCHAR(20) NOT NULL DEFAULT 'draft',
            created_at     DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at     DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, contract_id),
            INDEX idx_tc_owner_task_status_version (user_id, task_id, status, version),
            INDEX idx_tc_user_status (user_id, status)
        )",
    )
    .execute(&pool)
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "task_contracts",
        &["user_id", "contract_id"],
        "ALTER TABLE task_contracts ADD PRIMARY KEY (user_id, contract_id)",
    )
    .await?;
    drop_index_if_present(&pool, &settings.database, "task_contracts", "idx_tc_task").await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "task_contracts",
        "idx_tc_owner_task_status_version",
        &["user_id", "task_id", "status", "version"],
        "ALTER TABLE task_contracts ADD INDEX idx_tc_owner_task_status_version (user_id, task_id, status, version)",
    )
    .await?;

    // Verification results: audit trail of pass/fail evidence per criterion.
    // `result_id` is the owner-scoped row identity; contract/subtask dimensions
    // are query axes with explicit indexes, not part of the result identity.
    // Final table name is `verification_results`; the old
    // `task_verification_results` shape is intentionally dropped below.
    query(
        "CREATE TABLE IF NOT EXISTS verification_results (
            result_id      VARCHAR(64) NOT NULL,
            contract_id    VARCHAR(64) NOT NULL,
            task_id        VARCHAR(64) NOT NULL,
            subtask_id     VARCHAR(64) NOT NULL,
            criterion_id   VARCHAR(64) NOT NULL,
            session_id     VARCHAR(64) NOT NULL,
            user_id        VARCHAR(128) NOT NULL,
            status         VARCHAR(20) NOT NULL,
            evidence       LONGTEXT,
            expected       TEXT,
            duration_ms    INT,
            error_message  TEXT,
            attempt        INT NOT NULL DEFAULT 1,
            created_at     DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            CONSTRAINT chk_verification_results_status
                CHECK (status IN ('passed', 'failed')),
            PRIMARY KEY (user_id, result_id),
            INDEX idx_verification_results_contract_created (user_id, contract_id, created_at, result_id),
            INDEX idx_verification_results_contract_subtask (user_id, contract_id, subtask_id, created_at),
            INDEX idx_verification_results_status_created (user_id, status, created_at)
        )",
    )
    .execute(&pool)
    .await?;
    query("DROP TABLE IF EXISTS task_verification_results")
        .execute(&pool)
        .await?;

    // ─── Skill management tables ─────────────────────────────────────────────────

    query(
        "CREATE TABLE IF NOT EXISTS user_skill_sources (
            source_id VARCHAR(128) PRIMARY KEY,
            owner_user_id VARCHAR(128) NOT NULL,
            skill_name VARCHAR(128) NOT NULL,
            visibility VARCHAR(32) NOT NULL DEFAULT 'private',
            status VARCHAR(32) NOT NULL DEFAULT 'active',
            description TEXT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            CONSTRAINT chk_user_skill_sources_visibility CHECK (visibility IN ('private', 'workspace', 'public')),
            CONSTRAINT chk_user_skill_sources_status CHECK (status IN ('active', 'archived')),
            UNIQUE KEY uq_user_skill_source_name (owner_user_id, skill_name),
            INDEX idx_user_skill_owner_name (owner_user_id, skill_name),
            INDEX idx_user_skill_status_updated (owner_user_id, status, updated_at)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS user_skill_versions (
            version_id VARCHAR(128) PRIMARY KEY,
            source_id VARCHAR(128) NOT NULL,
            owner_user_id VARCHAR(128) NOT NULL,
            skill_name VARCHAR(128) NOT NULL,
            version VARCHAR(64) NOT NULL,
            manifest_json LONGTEXT NOT NULL,
            content_markdown LONGTEXT NOT NULL,
            content_hash VARCHAR(128) NOT NULL,
            normalize_version VARCHAR(32) NOT NULL DEFAULT 'skill_md_v1',
            token_estimate INT NOT NULL DEFAULT 0,
            status VARCHAR(32) NOT NULL DEFAULT 'draft',
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            CONSTRAINT chk_user_skill_versions_status CHECK (status IN ('draft', 'published', 'superseded', 'quarantined')),
            UNIQUE KEY uq_user_skill_source_version (source_id, version),
            INDEX idx_user_skill_versions_owner_name (owner_user_id, skill_name, status, created_at),
            INDEX idx_user_skill_versions_source (source_id, created_at),
            INDEX idx_user_skill_versions_hash (content_hash)
        )",
    )
    .execute(&pool)
    .await?;

    fail_if_obsolete_shape(
        &pool,
        &settings.database,
        "user_skill_evaluations",
        &[
            "evaluation_id",
            "owner_user_id",
            "source_id",
            "version_id",
            "run_id",
            "hits",
            "suspects",
            "false_positives",
            "payload_json",
            "created_at",
        ],
        &[],
        &[
            "idx_user_skill_eval_source_created",
            "idx_user_skill_eval_version_created",
            "idx_user_skill_eval_run",
        ],
    )
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS user_skill_evaluations (
            evaluation_id VARCHAR(128) PRIMARY KEY,
            owner_user_id VARCHAR(128) NOT NULL,
            source_id VARCHAR(128) NOT NULL,
            version_id VARCHAR(128) NOT NULL,
            run_id VARCHAR(128) NULL,
            hits BIGINT NOT NULL DEFAULT 0,
            suspects BIGINT NOT NULL DEFAULT 0,
            false_positives BIGINT NOT NULL DEFAULT 0,
            payload_json LONGTEXT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_user_skill_eval_owner_source_created (owner_user_id, source_id, created_at),
            INDEX idx_user_skill_eval_owner_version_created (owner_user_id, version_id, created_at),
            INDEX idx_user_skill_eval_owner_run (owner_user_id, run_id)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS skill_installations (
            installation_id  VARCHAR(36) PRIMARY KEY,
            user_id          VARCHAR(128) NOT NULL,
            skill_name       VARCHAR(128) NOT NULL,
            skill_version    VARCHAR(32) NOT NULL,
            status           VARCHAR(32) NOT NULL DEFAULT 'active',
            previous_version VARCHAR(32),
            scope            VARCHAR(32) NOT NULL DEFAULT 'user',
            session_id       VARCHAR(128) NULL,
            workspace_id     VARCHAR(128) NULL,
            auto_activate_on_topic_match SMALLINT NOT NULL DEFAULT 0,
            installed_at     DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at       DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6),
            UNIQUE INDEX idx_si_user_skill (user_id, skill_name),
            INDEX idx_si_status (status),
            INDEX idx_si_scope_target (user_id, scope, session_id, workspace_id, skill_name),
            INDEX idx_si_auto_activate (user_id, auto_activate_on_topic_match, status)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS skill_settings (
            setting_id    VARCHAR(36) PRIMARY KEY,
            skill_id      VARCHAR(36),
            skill_name    VARCHAR(128) NOT NULL,
            setting_name  VARCHAR(128) NOT NULL,
            setting_value TEXT,
            is_secret     SMALLINT NOT NULL DEFAULT 0,
            scope_type    VARCHAR(32) NOT NULL DEFAULT 'global',
            scope_id      VARCHAR(36),
            updated_by    VARCHAR(128),
            created_at    DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at    DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6),
            UNIQUE INDEX idx_ss_skill_setting_scope (skill_name, setting_name, scope_type, scope_id),
            INDEX idx_ss_skill (skill_name)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS runtime_llm_trusted_domains (
            domain_id     VARCHAR(36) PRIMARY KEY,
            domain_host   VARCHAR(255) NOT NULL,
            domain_port   INT NOT NULL DEFAULT 0,
            is_enabled    SMALLINT NOT NULL DEFAULT 1,
            description   VARCHAR(255),
            created_by    VARCHAR(128),
            updated_by    VARCHAR(128),
            created_at    DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at    DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6),
            UNIQUE INDEX idx_rld_host_port (domain_host, domain_port),
            INDEX idx_rld_enabled (is_enabled)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS skill_resource_bindings (
            binding_id    VARCHAR(36) PRIMARY KEY,
            user_id       VARCHAR(128) NOT NULL,
            skill_name    VARCHAR(128) NOT NULL,
            resource_type VARCHAR(64) NOT NULL,
            resource_key  VARCHAR(128) NOT NULL,
            binding_name  VARCHAR(128) NOT NULL,
            binding_value TEXT,
            is_secret     SMALLINT NOT NULL DEFAULT 0,
            updated_by    VARCHAR(128),
            created_at    DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at    DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6),
            INDEX idx_srb_user_skill (user_id, skill_name),
            INDEX idx_srb_resource (resource_type, resource_key)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS skill_user_credentials (
            credential_id   VARCHAR(36) PRIMARY KEY,
            user_id         VARCHAR(128) NOT NULL,
            skill_name      VARCHAR(128) NOT NULL,
            credential_name VARCHAR(128) NOT NULL,
            value_encrypted TEXT NOT NULL,
            created_at      DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at      DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6),
            UNIQUE INDEX idx_suc_user_skill_cred (user_id, skill_name, credential_name)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS wf_triggers (
            trigger_id   VARCHAR(36) PRIMARY KEY,
            user_id      VARCHAR(128) NOT NULL,
            agent_id     VARCHAR(255) NOT NULL,
            trigger_type VARCHAR(32) NOT NULL,
            name         VARCHAR(128) NOT NULL,
            user_input   TEXT NOT NULL,
            context      LONGTEXT,
            cron_expr    VARCHAR(64),
            secret       VARCHAR(128),
            session_id   VARCHAR(36),
            is_active    SMALLINT NOT NULL DEFAULT 1,
            created_at   DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at   DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6),
            INDEX idx_wft_user (user_id),
            INDEX idx_wft_type (trigger_type),
            INDEX idx_wft_active (is_active)
        )",
    )
    .execute(&pool)
    .await?;

    // ─── Agent management tables ─────────────────────────────────────────────────

    query(
        "CREATE TABLE IF NOT EXISTS agent_agents (
            agent_id       VARCHAR(255) PRIMARY KEY,
            agent_name     VARCHAR(128) NOT NULL,
            agent_type     VARCHAR(64) NOT NULL DEFAULT 'general',
            owner_user_id  VARCHAR(128) NOT NULL,
            is_active      SMALLINT NOT NULL DEFAULT 1,
            agent_config   LONGTEXT,
            data_source    TEXT,
            created_at     DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at     DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6),
            UNIQUE INDEX idx_aa_owner_name (owner_user_id, agent_name),
            INDEX idx_aa_type (agent_type)
        )",
    )
    .execute(&pool)
    .await?;

    // ─── Infrastructure tables ───────────────────────────────────────────────────

    query(
        "CREATE TABLE IF NOT EXISTS infra_sandbox_metadata (
            sandbox_name VARCHAR(128) PRIMARY KEY,
            user_id      VARCHAR(128) NOT NULL,
            description  TEXT NOT NULL,
            created_by   VARCHAR(128) NOT NULL,
            status       VARCHAR(32) NOT NULL DEFAULT 'active',
            created_at   DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at   DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6),
            INDEX idx_ism_user (user_id),
            INDEX idx_ism_status (status)
        )",
    )
    .execute(&pool)
    .await?;

    // ─── Data versioning tables ──────────────────────────────────────────────────

    query(
        "CREATE TABLE IF NOT EXISTS data_versioning_checkpoints (
            checkpoint_id   VARCHAR(36) PRIMARY KEY,
            checkpoint_name VARCHAR(128) NOT NULL,
            user_id         VARCHAR(128) NOT NULL,
            description     TEXT,
            created_at      DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            UNIQUE INDEX idx_dvc_user_name (user_id, checkpoint_name)
        )",
    )
    .execute(&pool)
    .await?;

    // ─── Evaluation tables ───────────────────────────────────────────────────────

    query(
        "CREATE TABLE IF NOT EXISTS eval_gate_results (
            gate_id         VARCHAR(36) PRIMARY KEY,
            user_id         VARCHAR(128) NULL,
            change_type     VARCHAR(64) NOT NULL,
            change_id       VARCHAR(64) NOT NULL,
            sessions_tested INT NOT NULL DEFAULT 0,
            error_rate      DECIMAL(5,4),
            score_delta     DECIMAL(5,4),
            passed          SMALLINT NOT NULL DEFAULT 0,
            created_at      DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_egr_user_created (user_id, created_at),
            INDEX idx_egr_change (change_type, change_id),
            INDEX idx_egr_passed (passed)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS eval_quality_assessments (
            assessment_id VARCHAR(64) PRIMARY KEY,
            user_id       VARCHAR(128) NULL,
            target_id     VARCHAR(64) NOT NULL,
            score         DECIMAL(5,4) NOT NULL,
            step_count    INT NOT NULL DEFAULT 0,
            level         VARCHAR(32) NOT NULL DEFAULT 'unknown',
            created_at    DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at    DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6),
            INDEX idx_eqa_user_level_updated (user_id, level, updated_at),
            INDEX idx_eqa_target (target_id),
            INDEX idx_eqa_level (level),
            INDEX idx_eqa_user_level_target (user_id, level, target_id)
        )",
    )
    .execute(&pool)
    .await?;

    query(EVAL_CALIBRATION_ASSESSMENTS_CREATE_SQL)
        .execute(&pool)
        .await?;
    fail_if_varchar_columns_shorter_than(
        &pool,
        &settings.database,
        "eval_calibration_assessments",
        &[("user_id", USER_ID_MAX_LEN as u64)],
    )
    .await?;
    ensure_primary_key_shape(
        &pool,
        &settings.database,
        "eval_calibration_assessments",
        &["user_id", "calibration_id"],
        "ALTER TABLE eval_calibration_assessments ADD PRIMARY KEY (user_id, calibration_id)",
    )
    .await?;

    for (index, ddl) in [
        (
            "idx_eval_calibration_user_created",
            "ALTER TABLE eval_calibration_assessments ADD INDEX idx_eval_calibration_user_created (user_id, created_at)",
        ),
        (
            "idx_eval_calibration_user_agent_created",
            "ALTER TABLE eval_calibration_assessments ADD INDEX idx_eval_calibration_user_agent_created (user_id, agent_id, created_at)",
        ),
        (
            "idx_eval_calibration_session",
            "ALTER TABLE eval_calibration_assessments ADD INDEX idx_eval_calibration_session (user_id, session_id, created_at)",
        ),
    ] {
        if let Err(e) = add_index_if_missing(
            &pool,
            &settings.database,
            "eval_calibration_assessments",
            index,
            ddl,
        )
        .await
        {
            tracing::debug!("eval calibration additive index migration skipped: {index}: {e}");
        }
    }

    query(
        "CREATE TABLE IF NOT EXISTS eval_training_datasets (
            dataset_id        VARCHAR(36) PRIMARY KEY,
            user_id           VARCHAR(128) NOT NULL,
            request_json      JSON NULL,
            dataset_json      LONGTEXT NOT NULL,
            sample_count      INT NOT NULL DEFAULT 0,
            quality_threshold DECIMAL(5,4) NOT NULL DEFAULT 0.7000,
            status            VARCHAR(32) NOT NULL DEFAULT 'ready',
            created_at        DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at        DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6),
            INDEX idx_eval_training_datasets_user_created (user_id, created_at)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS eval_user_feedback (
            feedback_id   VARCHAR(36) PRIMARY KEY,
            user_id       VARCHAR(128) NOT NULL,
            agent_id      VARCHAR(255),
            session_id    VARCHAR(36),
            turn_id       VARCHAR(36),
            feedback_type VARCHAR(64) NOT NULL,
            rating        INT,
            comment       TEXT,
            created_at    DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_euf_user (user_id),
            INDEX idx_euf_agent_created (agent_id, created_at),
            INDEX idx_euf_created (created_at),
            INDEX idx_euf_owner_session_created (user_id, session_id, created_at),
            INDEX idx_euf_type_created (feedback_type, created_at)
        )",
    )
    .execute(&pool)
    .await?;
    drop_index_if_present(
        &pool,
        &settings.database,
        "eval_user_feedback",
        "idx_euf_session",
    )
    .await?;
    ensure_index_shape(
        &pool,
        &settings.database,
        "eval_user_feedback",
        "idx_euf_owner_session_created",
        &["user_id", "session_id", "created_at"],
        "ALTER TABLE eval_user_feedback ADD INDEX idx_euf_owner_session_created (user_id, session_id, created_at)",
    )
    .await?;

    // ─── Team definitions ───────────────────────────────────────────────────────

    query(
        "CREATE TABLE IF NOT EXISTS team_definitions (
            team_id       VARCHAR(64)  PRIMARY KEY,
            user_id       VARCHAR(128)  NOT NULL,
            name          VARCHAR(128) NOT NULL,
            description   TEXT,
            coordination  TEXT         NOT NULL,
            members_json  TEXT         NOT NULL,
            context_json  TEXT,
            worktree_mode VARCHAR(32)  DEFAULT 'shared',
            budget_json   TEXT,
            max_parallel  INT UNSIGNED NOT NULL DEFAULT 0,
            created_at    DATETIME(6)  NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at    DATETIME(6)  NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            UNIQUE KEY uq_team_user_name (user_id, name)
        )",
    )
    .execute(&pool)
    .await?;

    // ─── Team execution history ─────────────────────────────────────────────────

    query(
        "CREATE TABLE IF NOT EXISTS team_execution_history (
            execution_id  VARCHAR(64)  PRIMARY KEY,
            team_id       VARCHAR(64)  NOT NULL,
            user_id       VARCHAR(128)  NOT NULL,
            `task`        TEXT         NOT NULL,
            status        VARCHAR(32)  NOT NULL DEFAULT 'pending',
            result_json   LONGTEXT,
            started_at    DATETIME(6)  NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            completed_at  DATETIME(6),
            INDEX idx_teh_team (team_id, started_at),
            INDEX idx_teh_user (user_id, started_at)
        )",
    )
    .execute(&pool)
    .await?;

    // ─── Team snapshots ─────────────────────────────────────────────────────────

    query(
        "CREATE TABLE IF NOT EXISTS team_snapshots (
            snapshot_id          VARCHAR(64)  PRIMARY KEY,
            team_name            VARCHAR(128) NOT NULL,
            user_id              VARCHAR(128)  NOT NULL,
            label                VARCHAR(255) DEFAULT '',
            git_commit           VARCHAR(64),
            session_id           VARCHAR(64),
            team_definition_json LONGTEXT,
            created_at           DATETIME(6)  NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_ts_user_team (user_id, team_name, created_at)
        )",
    )
    .execute(&pool)
    .await?;

    // ─── Conversation State Log (CSL) ──────────────────────────────────────────

    query(
        "CREATE TABLE IF NOT EXISTS conversation_log (
            user_id       VARCHAR(128) NOT NULL,
            session_id    VARCHAR(64) NOT NULL,
            seq           BIGINT NOT NULL,
            turn          INT NOT NULL,
            entry_type    TINYINT NOT NULL,
            trace_id      VARCHAR(64) DEFAULT NULL,
            message_count INT DEFAULT NULL,
            payload       MEDIUMTEXT NOT NULL,
            created_at    DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (user_id, session_id, seq),
            INDEX idx_csl_owner_snapshot (user_id, session_id, entry_type, seq DESC),
            INDEX idx_csl_owner_turn (user_id, session_id, turn)
        )",
    )
    .execute(&pool)
    .await?;
    // ─── Content-addressed config versions (Step 4a) ────────────────────────────
    //
    // One row per unique RuntimeConfig hash per tenant. Populated by
    // the CLI via enqueue_journal_events → IngestionEvent::ConfigVersionSaved,
    // consumed by `astra config sync pull` on new machines. See
    // `crate::config_version_cloud` for the DDL string and bind helpers
    // that the push / pull pipeline uses.

    query(crate::config_version_cloud::CONFIG_VERSIONS_CREATE_SQL)
        .execute(&pool)
        .await?;

    Ok(())
}

trait DatabaseUserRow {
    fn string_column(&self, column: &'static str) -> Result<String, sqlx::Error>;
    fn optional_string_column(&self, column: &'static str) -> Result<Option<String>, sqlx::Error>;
    fn i16_column(&self, column: &'static str) -> Result<i16, sqlx::Error>;
}

impl DatabaseUserRow for sqlx::mysql::MySqlRow {
    fn string_column(&self, column: &'static str) -> Result<String, sqlx::Error> {
        self.try_get(column)
    }

    fn optional_string_column(&self, column: &'static str) -> Result<Option<String>, sqlx::Error> {
        self.try_get(column)
    }

    fn i16_column(&self, column: &'static str) -> Result<i16, sqlx::Error> {
        self.try_get(column)
    }
}

fn decode_database_user_row(row: &impl DatabaseUserRow) -> Result<DatabaseUserRecord, sqlx::Error> {
    Ok(DatabaseUserRecord {
        user_id: row.string_column("user_id")?,
        username: row.string_column("username")?,
        email: row.string_column("email")?,
        password_hash: row.string_column("password_hash")?,
        display_name: row.optional_string_column("display_name")?,
        is_active: row.i16_column("is_active")? != 0,
    })
}

pub fn database_user_from_row(
    row: sqlx::mysql::MySqlRow,
) -> Result<DatabaseUserRecord, sqlx::Error> {
    decode_database_user_row(&row)
}

trait ActiveSkillVersionRow {
    fn string_column(&self, column: &'static str) -> Result<String, sqlx::Error>;
}

impl ActiveSkillVersionRow for sqlx::mysql::MySqlRow {
    fn string_column(&self, column: &'static str) -> Result<String, sqlx::Error> {
        self.try_get(column)
    }
}

fn invalid_active_skill_version_value(column: &'static str, value: &str) -> sqlx::Error {
    sqlx::Error::Decode(Box::new(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("skills_registry decode column `{column}`: empty value `{value}`"),
    )))
}

fn decode_active_skill_version_row(
    row: &impl ActiveSkillVersionRow,
) -> Result<(String, String), sqlx::Error> {
    let skill_name = row.string_column("skill_name")?;
    if skill_name.trim().is_empty() {
        return Err(invalid_active_skill_version_value(
            "skill_name",
            &skill_name,
        ));
    }
    let version = row.string_column("version")?;
    if version.trim().is_empty() {
        return Err(invalid_active_skill_version_value("version", &version));
    }
    Ok((skill_name, version))
}

pub fn session_record_from_row(
    row: sqlx::mysql::MySqlRow,
) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)> {
    let metadata_json: String = row.try_get("metadata_json").map_err(internal_error)?;
    let metadata =
        serde_json::from_str::<serde_json::Value>(&metadata_json).map_err(internal_error)?;
    let metadata = match metadata {
        serde_json::Value::Object(map) => map,
        serde_json::Value::Null => serde_json::Map::new(),
        _ => {
            return Err(internal_error(
                "session metadata must deserialize to a JSON object",
            ));
        }
    };

    Ok(SessionRecord {
        session_id: row.try_get("session_id").map_err(internal_error)?,
        user_id: row.try_get("user_id").map_err(internal_error)?,
        agent_id: row.try_get("agent_id").map_err(internal_error)?,
        title: row.try_get("title").map_err(internal_error)?,
        metadata,
        status: row.try_get("status").map_err(internal_error)?,
        event_count: row.try_get("event_count").map_err(internal_error)?,
        created_at: row.try_get("created_at").map_err(internal_error)?,
        updated_at: row.try_get("updated_at").map_err(internal_error)?,
        ended_at: row.try_get("ended_at").map_err(internal_error)?,
    })
}

pub async fn log_session_audit(
    pool: &sqlx::Pool<MySql>,
    user_id: &str,
    action: &str,
    session_id: &str,
    details: serde_json::Value,
) {
    if let Err(e) = query(
        "INSERT INTO auth_audit_logs \
         (log_id, user_id, action, resource_type, resource_id, details, created_at) \
         VALUES (?, ?, ?, 'session', ?, ?, NOW())",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(user_id)
    .bind(action)
    .bind(session_id)
    .bind(details.to_string())
    .execute(pool)
    .await
    {
        tracing::warn!(
            target: "astra_services::storage",
            user_id = %user_id,
            action = %action,
            session_id = %session_id,
            error = %e,
            "failed to write auth audit log"
        );
    }
}

pub async fn update_turn_skill_selection_version(
    tx: &mut sqlx::Transaction<'_, MySql>,
    event_id: &str,
    user_id: &str,
    session_id: &str,
    skill_version: &str,
) -> Result<(), sqlx::Error> {
    let result = query(
        "UPDATE skill_selection_events
         SET skill_version = ?
         WHERE event_id = ? AND user_id = ? AND session_id = ?",
    )
    .bind(skill_version)
    .bind(event_id)
    .bind(user_id)
    .bind(session_id)
    .execute(&mut **tx)
    .await?;
    if result.rows_affected() == 0 {
        return Err(sqlx::Error::RowNotFound);
    }
    Ok(())
}

pub async fn resolve_active_skill_versions(
    pool: &sqlx::Pool<MySql>,
    skill_names: BTreeSet<&str>,
) -> Result<HashMap<String, String>, sqlx::Error> {
    if skill_names.is_empty() {
        return Ok(HashMap::new());
    }

    let mut query_builder = QueryBuilder::<MySql>::new(
        "SELECT skill_name, version FROM skills_registry WHERE is_active = 1 AND skill_name IN (",
    );
    {
        let mut separated = query_builder.separated(", ");
        for skill_name in &skill_names {
            separated.push_bind(skill_name);
        }
    }
    query_builder.push(") ORDER BY skill_name ASC, version DESC");

    let rows = query_builder.build().fetch_all(pool).await?;
    let mut versions = HashMap::new();
    for row in rows {
        let (skill_name, version) = decode_active_skill_version_row(&row)?;
        if versions.contains_key(&skill_name) {
            continue;
        }
        versions.insert(skill_name, version);
    }
    Ok(versions)
}

// ─── Expired Data Cleanup ────────────────────────────────────────────────────

/// Result of a single table cleanup operation.
#[derive(Debug, Clone)]
pub struct CleanupResult {
    pub table: &'static str,
    pub rows_deleted: u64,
}

/// Configuration for data retention policies.
pub struct RetentionPolicy {
    /// Max age in days for expired/revoked refresh tokens (default: 7)
    pub refresh_token_days: u32,
    /// Max age in days for inactive auth tokens (default: 30)
    pub auth_token_days: u32,
    /// Max age in days for expired task leases (default: 7)
    pub task_lease_days: u32,
    /// Max age in days for audit logs (default: 90)
    pub audit_log_days: u32,
    /// Max age in days for prompt observability rows after their session/run is inactive (default: 90)
    pub prompt_request_days: u32,
    /// Max age in days for agent events (default: 90)
    pub event_days: u32,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            refresh_token_days: 7,
            auth_token_days: 30,
            task_lease_days: 7,
            audit_log_days: 90,
            prompt_request_days: 90,
            event_days: 90,
        }
    }
}

trait ExpiredAgentEventRow {
    fn string_column(&self, column: &str) -> Result<String, sqlx::Error>;
}

impl ExpiredAgentEventRow for sqlx::mysql::MySqlRow {
    fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
        self.try_get(column)
    }
}

fn decode_expired_agent_event_ref(
    row: &impl ExpiredAgentEventRow,
) -> Result<(String, String), String> {
    let user_id = row
        .string_column("user_id")
        .map_err(|e| format!("cleanup agent_events decode user_id: {e}"))?;
    let event_id = row
        .string_column("event_id")
        .map_err(|e| format!("cleanup agent_events decode event_id: {e}"))?;
    Ok((user_id, event_id))
}

trait ExpiredPromptRequestRow {
    fn string_column(&self, column: &str) -> Result<String, sqlx::Error>;
}

impl ExpiredPromptRequestRow for sqlx::mysql::MySqlRow {
    fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
        self.try_get(column)
    }
}

fn decode_expired_prompt_request_ref(
    row: &impl ExpiredPromptRequestRow,
) -> Result<(String, String, String), String> {
    let user_id = row
        .string_column("user_id")
        .map_err(|e| format!("cleanup prompt_request_records decode user_id: {e}"))?;
    let session_id = row
        .string_column("session_id")
        .map_err(|e| format!("cleanup prompt_request_records decode session_id: {e}"))?;
    let request_id = row
        .string_column("request_id")
        .map_err(|e| format!("cleanup prompt_request_records decode request_id: {e}"))?;
    Ok((user_id, session_id, request_id))
}

/// Purge expired data across all tables with TTL/expiry semantics.
///
/// Returns a list of per-table cleanup results showing how many rows were deleted.
/// Each DELETE uses a LIMIT to avoid long-running locks; callers should invoke
/// repeatedly until all results show 0 rows deleted for a full sweep.
pub async fn cleanup_expired_data(
    pool: &sqlx::Pool<MySql>,
    policy: &RetentionPolicy,
) -> Result<Vec<CleanupResult>, String> {
    const AUTH_REFRESH_TOKEN_BATCH_LIMIT: u32 = 1000;
    const AUTH_TOKEN_BATCH_LIMIT: u32 = 1000;
    const AUTH_PROVIDER_REQUEST_REPLAY_BATCH_LIMIT: u32 = 1000;
    const TASK_LEASE_BATCH_LIMIT: u32 = 1000;
    const AUTH_AUDIT_LOG_BATCH_LIMIT: u32 = 1000;
    const PROMPT_REQUEST_BATCH_LIMIT: u32 = 1000;
    const PROMPT_REQUEST_MAX_BATCHES_PER_RUN: u32 = 10;
    const PROMPT_REQUEST_DELETE_CHUNK_SIZE: usize = 250;
    const AGENT_EVENT_BATCH_LIMIT: u32 = 1000;
    const AGENT_EVENT_DELETE_CHUNK_SIZE: usize = 250;
    let mut results = Vec::new();

    // 1. Expired + revoked refresh tokens
    let deleted = sqlx::query(
        "DELETE FROM auth_refresh_tokens \
         WHERE (expires_at < NOW(6) OR is_revoked = 1) \
           AND created_at < DATE_SUB(NOW(6), INTERVAL ? DAY) \
         ORDER BY created_at ASC, token_id ASC \
         LIMIT ?",
    )
    .bind(policy.refresh_token_days)
    .bind(AUTH_REFRESH_TOKEN_BATCH_LIMIT)
    .execute(pool)
    .await
    .map(|r| r.rows_affected())
    .map_err(|e| format!("cleanup auth_refresh_tokens: {e}"))?;
    results.push(CleanupResult {
        table: "auth_refresh_tokens",
        rows_deleted: deleted,
    });

    // 2. Inactive auth tokens
    let deleted = sqlx::query(
        "DELETE FROM auth_tokens \
         WHERE is_active = 0 \
           AND created_at < DATE_SUB(NOW(6), INTERVAL ? DAY) \
         ORDER BY created_at ASC, token_id ASC \
         LIMIT ?",
    )
    .bind(policy.auth_token_days)
    .bind(AUTH_TOKEN_BATCH_LIMIT)
    .execute(pool)
    .await
    .map(|r| r.rows_affected())
    .map_err(|e| format!("cleanup auth_tokens: {e}"))?;
    results.push(CleanupResult {
        table: "auth_tokens",
        rows_deleted: deleted,
    });

    // 3. Expired provider request replay guards. These are replay-prevention
    // facts, not audit facts: after the capability token expiry passes, the
    // row only consumes index space and can no longer authorize anything.
    let deleted = sqlx::query(
        "DELETE FROM auth_provider_request_replay \
         WHERE expires_at_unix < UNIX_TIMESTAMP() \
         ORDER BY expires_at_unix ASC, provider ASC, request_authorization_id ASC \
         LIMIT ?",
    )
    .bind(AUTH_PROVIDER_REQUEST_REPLAY_BATCH_LIMIT)
    .execute(pool)
    .await
    .map(|r| r.rows_affected())
    .map_err(|e| format!("cleanup auth_provider_request_replay: {e}"))?;
    results.push(CleanupResult {
        table: "auth_provider_request_replay",
        rows_deleted: deleted,
    });

    // 4. Expired task leases
    let deleted = sqlx::query(
        "DELETE FROM task_leases \
         WHERE expires_at < DATE_SUB(NOW(6), INTERVAL ? DAY) \
         ORDER BY expires_at ASC, user_id ASC, task_id ASC \
         LIMIT ?",
    )
    .bind(policy.task_lease_days)
    .bind(TASK_LEASE_BATCH_LIMIT)
    .execute(pool)
    .await
    .map(|r| r.rows_affected())
    .map_err(|e| format!("cleanup task_leases: {e}"))?;
    results.push(CleanupResult {
        table: "task_leases",
        rows_deleted: deleted,
    });

    // 5. Old audit logs
    let deleted = sqlx::query(
        "DELETE FROM auth_audit_logs \
         WHERE created_at < DATE_SUB(NOW(6), INTERVAL ? DAY) \
         ORDER BY created_at ASC, log_id ASC \
         LIMIT ?",
    )
    .bind(policy.audit_log_days)
    .bind(AUTH_AUDIT_LOG_BATCH_LIMIT)
    .execute(pool)
    .await
    .map(|r| r.rows_affected())
    .map_err(|e| format!("cleanup auth_audit_logs: {e}"))?;
    results.push(CleanupResult {
        table: "auth_audit_logs",
        rows_deleted: deleted,
    });

    // 6. Old prompt observability rows. Select parent request records first so
    // child prompt_deltas and parent prompt_request_records are pruned together.
    let prompt_request_retention_select_sql = format!(
        "SELECT p.user_id, p.session_id, p.request_id
             FROM prompt_request_records p
             LEFT JOIN agent_sessions s
               ON s.user_id = p.user_id AND s.session_id = p.session_id
             LEFT JOIN agent_runs r
               ON r.user_id = p.user_id AND r.run_id = p.run_id
             WHERE p.created_at_unix_ms < UNIX_TIMESTAMP(DATE_SUB(NOW(6), INTERVAL {} DAY)) * 1000
               AND (s.session_id IS NULL OR s.status IN ('ended', 'closed', 'cancelled', 'deleting'))
               AND (p.run_id IS NULL OR r.run_id IS NULL OR r.status IN ('completed', 'delegated', 'failed', 'cancelled'))
             ORDER BY p.created_at_unix_ms ASC, p.user_id ASC, p.request_id ASC
             LIMIT ?",
        policy.prompt_request_days
    );
    let mut prompt_delta_deleted = 0_u64;
    let mut prompt_request_deleted = 0_u64;
    for _ in 0..PROMPT_REQUEST_MAX_BATCHES_PER_RUN {
        let mut tx = pool
            .begin()
            .await
            .map_err(|e| format!("cleanup prompt_request_records begin transaction: {e}"))?;
        let expired_prompt_rows = sqlx::query(&prompt_request_retention_select_sql)
            .bind(PROMPT_REQUEST_BATCH_LIMIT)
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| format!("cleanup prompt_request_records select expired ids: {e}"))?;
        let expired_prompt_request_ids: Vec<(String, String, String)> = expired_prompt_rows
            .iter()
            .map(decode_expired_prompt_request_ref)
            .collect::<Result<Vec<_>, _>>()?;
        if expired_prompt_request_ids.is_empty() {
            tx.commit()
                .await
                .map_err(|e| format!("cleanup prompt_request_records commit empty batch: {e}"))?;
            break;
        }
        for chunk in expired_prompt_request_ids.chunks(PROMPT_REQUEST_DELETE_CHUNK_SIZE) {
            let mut builder = QueryBuilder::<MySql>::new(
                "DELETE FROM prompt_deltas WHERE (user_id, session_id, request_id) IN (",
            );
            for (index, (user_id, session_id, request_id)) in chunk.iter().enumerate() {
                if index > 0 {
                    builder.push(", ");
                }
                builder
                    .push("(")
                    .push_bind(user_id)
                    .push(", ")
                    .push_bind(session_id)
                    .push(", ")
                    .push_bind(request_id)
                    .push(")");
            }
            builder.push(")");
            let deleted = builder
                .build()
                .execute(&mut *tx)
                .await
                .map(|r| r.rows_affected())
                .map_err(|e| format!("cleanup prompt_deltas: {e}"))?;
            prompt_delta_deleted = prompt_delta_deleted.saturating_add(deleted);
        }
        for chunk in expired_prompt_request_ids.chunks(PROMPT_REQUEST_DELETE_CHUNK_SIZE) {
            let mut builder = QueryBuilder::<MySql>::new(
                "DELETE FROM prompt_request_records WHERE (user_id, session_id, request_id) IN (",
            );
            for (index, (user_id, session_id, request_id)) in chunk.iter().enumerate() {
                if index > 0 {
                    builder.push(", ");
                }
                builder
                    .push("(")
                    .push_bind(user_id)
                    .push(", ")
                    .push_bind(session_id)
                    .push(", ")
                    .push_bind(request_id)
                    .push(")");
            }
            builder.push(")");
            let deleted = builder
                .build()
                .execute(&mut *tx)
                .await
                .map(|r| r.rows_affected())
                .map_err(|e| format!("cleanup prompt_request_records delete expired ids: {e}"))?;
            prompt_request_deleted = prompt_request_deleted.saturating_add(deleted);
        }
        tx.commit()
            .await
            .map_err(|e| format!("cleanup prompt_request_records commit transaction: {e}"))?;
        if expired_prompt_request_ids.len() < PROMPT_REQUEST_BATCH_LIMIT as usize {
            break;
        }
    }
    results.push(CleanupResult {
        table: "prompt_deltas",
        rows_deleted: prompt_delta_deleted,
    });
    results.push(CleanupResult {
        table: "prompt_request_records",
        rows_deleted: prompt_request_deleted,
    });

    // 7. Old agent events
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| format!("cleanup agent_events begin transaction: {e}"))?;
    let expired_event_rows = sqlx::query(
        "SELECT user_id, event_id FROM agent_events \
         WHERE created_at < DATE_SUB(NOW(6), INTERVAL ? DAY) \
         ORDER BY created_at ASC, user_id ASC, event_id ASC \
         LIMIT ?",
    )
    .bind(policy.event_days)
    .bind(AGENT_EVENT_BATCH_LIMIT)
    .fetch_all(&mut *tx)
    .await
    .map_err(|e| format!("cleanup agent_events select expired ids: {e}"))?;
    let expired_event_ids: Vec<(String, String)> = expired_event_rows
        .iter()
        .map(decode_expired_agent_event_ref)
        .collect::<Result<Vec<_>, _>>()?;
    let mut edge_deleted = 0_u64;
    if !expired_event_ids.is_empty() {
        for chunk in expired_event_ids.chunks(AGENT_EVENT_DELETE_CHUNK_SIZE) {
            let deleted = delete_agent_event_edges_for_owned_event_ids(&mut *tx, chunk)
                .await
                .map_err(|e| format!("cleanup agent_event_edges: {e}"))?;
            edge_deleted = edge_deleted.saturating_add(deleted);
        }
    }
    let deleted = if expired_event_ids.is_empty() {
        0
    } else {
        let mut total_deleted = 0_u64;
        for chunk in expired_event_ids.chunks(AGENT_EVENT_DELETE_CHUNK_SIZE) {
            let mut builder = QueryBuilder::<MySql>::new(
                "DELETE FROM agent_events WHERE (user_id, event_id) IN (",
            );
            let mut event_ids = builder.separated(", ");
            for (user_id, event_id) in chunk {
                event_ids
                    .push_unseparated("(")
                    .push_bind(user_id)
                    .push_unseparated(", ")
                    .push_bind(event_id)
                    .push_unseparated(")");
            }
            event_ids.push_unseparated(")");
            let deleted = builder
                .build()
                .execute(&mut *tx)
                .await
                .map(|r| r.rows_affected())
                .map_err(|e| format!("cleanup agent_events delete expired ids: {e}"))?;
            total_deleted = total_deleted.saturating_add(deleted);
        }
        total_deleted
    };
    tx.commit()
        .await
        .map_err(|e| format!("cleanup agent_events commit transaction: {e}"))?;
    results.push(CleanupResult {
        table: "agent_event_edges",
        rows_deleted: edge_deleted,
    });
    results.push(CleanupResult {
        table: "agent_events",
        rows_deleted: deleted,
    });

    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_column_widening_preserves_nullability() {
        assert_eq!(
            identity_column_widening_ddl("eval_quality_assessments", "user_id", true)
                .expect("nullable identity migration DDL"),
            "ALTER TABLE `eval_quality_assessments` MODIFY COLUMN `user_id` VARCHAR(128) NULL"
        );
        assert_eq!(
            identity_column_widening_ddl("auth_users", "username", false)
                .expect("required identity migration DDL"),
            "ALTER TABLE `auth_users` MODIFY COLUMN `username` VARCHAR(128) NOT NULL"
        );
    }

    #[test]
    fn production_ddl_identity_columns_meet_width_contract() {
        let identity_column = regex::Regex::new(
            r"(?i)\b(user_id|owner_user_id|scope_user_id|created_by|updated_by|username)\s+VARCHAR\((\d+)\)",
        )
        .expect("identity column regex");
        for (path, source) in [
            ("storage.rs", include_str!("storage.rs")),
            (
                "config_version_cloud.rs",
                include_str!("config_version_cloud.rs"),
            ),
            ("resource_governor.rs", include_str!("resource_governor.rs")),
            ("workspace_records.rs", include_str!("workspace_records.rs")),
            (
                "astra-messaging/db_transport.rs",
                include_str!("../../astra-messaging/src/db_transport.rs"),
            ),
            (
                "runtime/llm_provider_admission.rs",
                include_str!("../../runtime/src/llm_provider_admission.rs"),
            ),
        ] {
            for captures in identity_column.captures_iter(source) {
                let column = captures.get(1).expect("identity column name").as_str();
                let width = captures
                    .get(2)
                    .expect("identity column width")
                    .as_str()
                    .parse::<usize>()
                    .expect("numeric identity column width");
                assert!(
                    width >= USER_ID_MAX_LEN,
                    "{path} defines {column} as VARCHAR({width}), below the {USER_ID_MAX_LEN}-character identity contract"
                );
            }
        }
    }

    struct FakeDatabaseUserRow {
        failed_column: Option<&'static str>,
        display_name: Option<String>,
        is_active: i16,
    }

    impl FakeDatabaseUserRow {
        fn complete() -> Self {
            Self {
                failed_column: None,
                display_name: Some("Test User".to_string()),
                is_active: 1,
            }
        }

        fn fail_on(column: &'static str) -> Self {
            Self {
                failed_column: Some(column),
                ..Self::complete()
            }
        }

        fn fail_if_needed(&self, column: &'static str) -> Result<(), sqlx::Error> {
            if self.failed_column == Some(column) {
                Err(sqlx::Error::ColumnNotFound(column.to_string()))
            } else {
                Ok(())
            }
        }
    }

    impl DatabaseUserRow for FakeDatabaseUserRow {
        fn string_column(&self, column: &'static str) -> Result<String, sqlx::Error> {
            self.fail_if_needed(column)?;
            Ok(match column {
                "user_id" => "user-1",
                "username" => "test-user",
                "email" => "test@example.com",
                "password_hash" => "bcrypt-hash",
                _ => unreachable!("unexpected string column: {column}"),
            }
            .to_string())
        }

        fn optional_string_column(
            &self,
            column: &'static str,
        ) -> Result<Option<String>, sqlx::Error> {
            self.fail_if_needed(column)?;
            assert_eq!(column, "display_name");
            Ok(self.display_name.clone())
        }

        fn i16_column(&self, column: &'static str) -> Result<i16, sqlx::Error> {
            self.fail_if_needed(column)?;
            assert_eq!(column, "is_active");
            Ok(self.is_active)
        }
    }

    #[test]
    fn database_user_row_decode_preserves_database_values() {
        let user = decode_database_user_row(&FakeDatabaseUserRow::complete()).unwrap();

        assert_eq!(user.user_id, "user-1");
        assert_eq!(user.username, "test-user");
        assert_eq!(user.email, "test@example.com");
        assert_eq!(user.password_hash, "bcrypt-hash");
        assert_eq!(user.display_name.as_deref(), Some("Test User"));
        assert!(user.is_active);

        let inactive = decode_database_user_row(&FakeDatabaseUserRow {
            display_name: None,
            is_active: 0,
            ..FakeDatabaseUserRow::complete()
        })
        .unwrap();
        assert_eq!(inactive.display_name, None);
        assert!(!inactive.is_active);
    }

    #[test]
    fn database_user_row_decode_fails_loudly_on_any_missing_column() {
        for column in [
            "user_id",
            "username",
            "email",
            "password_hash",
            "display_name",
            "is_active",
        ] {
            match decode_database_user_row(&FakeDatabaseUserRow::fail_on(column)).unwrap_err() {
                sqlx::Error::ColumnNotFound(name) => assert_eq!(name, column),
                err => panic!("expected ColumnNotFound({column}), got {err:?}"),
            }
        }
    }

    struct FakeExpiredAgentEventRow {
        failed_column: Option<&'static str>,
    }

    impl FakeExpiredAgentEventRow {
        fn complete() -> Self {
            Self {
                failed_column: None,
            }
        }

        fn fail_on(column: &'static str) -> Self {
            Self {
                failed_column: Some(column),
            }
        }
    }

    impl ExpiredAgentEventRow for FakeExpiredAgentEventRow {
        fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
            if self.failed_column == Some(column) {
                return Err(sqlx::Error::ColumnNotFound(column.to_string()));
            }

            Ok(match column {
                "user_id" => "user-1",
                "event_id" => "event-1",
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
            .to_string())
        }
    }

    #[test]
    fn expired_agent_event_ref_decode_preserves_owner_and_event_id() {
        let (user_id, event_id) =
            decode_expired_agent_event_ref(&FakeExpiredAgentEventRow::complete()).unwrap();

        assert_eq!(user_id, "user-1");
        assert_eq!(event_id, "event-1");
    }

    #[test]
    fn expired_agent_event_ref_decode_fails_loudly_on_missing_columns() {
        for column in ["user_id", "event_id"] {
            let error = decode_expired_agent_event_ref(&FakeExpiredAgentEventRow::fail_on(column))
                .unwrap_err();
            assert!(
                error.contains(column),
                "cleanup decode error should identify `{column}`: {error}"
            );
        }
    }

    struct FakeActiveSkillVersionRow {
        failed_column: Option<&'static str>,
        skill_name: &'static str,
        version: &'static str,
    }

    impl FakeActiveSkillVersionRow {
        fn complete() -> Self {
            Self {
                failed_column: None,
                skill_name: "skill-a",
                version: "v2",
            }
        }

        fn fail_on(column: &'static str) -> Self {
            Self {
                failed_column: Some(column),
                ..Self::complete()
            }
        }

        fn with_values(skill_name: &'static str, version: &'static str) -> Self {
            Self {
                failed_column: None,
                skill_name,
                version,
            }
        }
    }

    impl ActiveSkillVersionRow for FakeActiveSkillVersionRow {
        fn string_column(&self, column: &'static str) -> Result<String, sqlx::Error> {
            if self.failed_column == Some(column) {
                return Err(sqlx::Error::ColumnNotFound(column.to_string()));
            }

            Ok(match column {
                "skill_name" => self.skill_name,
                "version" => self.version,
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
            .to_string())
        }
    }

    #[test]
    fn active_skill_version_row_decode_preserves_database_values() {
        let (skill_name, version) =
            decode_active_skill_version_row(&FakeActiveSkillVersionRow::complete()).unwrap();

        assert_eq!(skill_name, "skill-a");
        assert_eq!(version, "v2");
    }

    #[test]
    fn active_skill_version_row_decode_fails_loudly_on_missing_columns() {
        for column in ["skill_name", "version"] {
            match decode_active_skill_version_row(&FakeActiveSkillVersionRow::fail_on(column))
                .unwrap_err()
            {
                sqlx::Error::ColumnNotFound(name) => assert_eq!(name, column),
                err => panic!("expected ColumnNotFound({column}), got {err:?}"),
            }
        }
    }

    #[test]
    fn active_skill_version_row_decode_fails_loudly_on_empty_values() {
        for (column, row) in [
            (
                "skill_name",
                FakeActiveSkillVersionRow::with_values("   ", "v2"),
            ),
            (
                "version",
                FakeActiveSkillVersionRow::with_values("skill-a", ""),
            ),
        ] {
            let error = decode_active_skill_version_row(&row).unwrap_err();
            assert!(
                matches!(error, sqlx::Error::Decode(_)),
                "expected decode error for `{column}`, got {error:?}"
            );
            assert!(
                error.to_string().contains(column),
                "decode error should identify `{column}`: {error}"
            );
        }
    }

    /// Every `agent_id`, `edge_agent_id`, `holder_agent_id`, and
    /// `parent_agent_id` column in DDL MUST use the width encoded in
    /// [`AGENT_ID_LEN`].
    #[test]
    fn agent_id_columns_match_agreed_width() {
        // sanity: AGENT_ID_LEN must produce a reasonable VARCHAR width
        if AGENT_ID_LEN < 32 {
            panic!("AGENT_ID_LEN ({AGENT_ID_LEN}) is too small");
        }
    }

    #[test]
    fn agent_events_turn_seq_inference_index_is_declared_and_reconciled() {
        let create_sql = agent_events_create_sql();
        assert!(
            create_sql.contains("PRIMARY KEY (user_id, event_id)"),
            "agent_events identity must be owner-bound so INSERT IGNORE does not suppress another tenant"
        );
        assert!(
            !create_sql.contains("event_id VARCHAR(64) PRIMARY KEY"),
            "agent_events must not use global event_id identity"
        );
        assert!(
            create_sql.contains("event_id VARCHAR(128) NOT NULL"),
            "agent_events must fit full content-addressed ingestion event ids"
        );
        assert!(
            create_sql.contains("parent_event_id VARCHAR(128) NULL"),
            "agent_events parent references must fit full event ids"
        );
        assert!(
            create_sql.contains("causal_chain_id VARCHAR(128) NULL"),
            "agent_events causal chains often reuse full event ids"
        );
        assert!(
            create_sql.contains(AGENT_EVENTS_OWNER_SESSION_TURN_INDEX_DECL),
            "agent_events must index the session-turn inference path in CREATE TABLE"
        );
        assert!(
            AGENT_EVENTS_OWNER_SESSION_TURN_INDEX_ALTER_SQL
                .contains("idx_agent_events_owner_session_turn (user_id, session_id, turn_seq)"),
            "agent_events must reconcile the session-turn inference index in schema ensure"
        );
    }

    #[test]
    fn agent_event_edges_schema_matches_owner_bound_write_path() {
        let source = include_str!("storage.rs");
        let ddl = source
            .split("CREATE TABLE IF NOT EXISTS agent_event_edges")
            .nth(1)
            .and_then(|rest| rest.split("// Harness diagnostic snapshots").next())
            .expect("agent_event_edges DDL");
        for expected in [
            "user_id VARCHAR(128) NOT NULL",
            "session_id VARCHAR(128) NOT NULL",
            "child_event_id VARCHAR(128) NOT NULL",
            "parent_event_id VARCHAR(128) NOT NULL",
            "PRIMARY KEY (user_id, child_event_id, parent_event_id, relation_kind)",
            "idx_agent_event_edges_owner_session_child (user_id, session_id, child_event_id)",
        ] {
            assert!(
                ddl.contains(expected),
                "agent_event_edges DDL must include owner/session write-path field: {expected}"
            );
        }
        let insert_body = source
            .split("pub async fn insert_agent_event_edges")
            .nth(1)
            .and_then(|rest| {
                rest.split("pub async fn load_agent_event_parent_ids")
                    .next()
            })
            .expect("insert_agent_event_edges body");
        assert!(
            insert_body.contains("(user_id, session_id, child_event_id, parent_event_id, relation_kind, parent_order)"),
            "agent_event_edges insert columns must stay aligned with owner-bound DDL"
        );
        assert!(
            insert_body.contains("ON DUPLICATE KEY UPDATE")
                && insert_body.contains("parent_order = VALUES(parent_order)"),
            "agent_event_edges duplicate writes must refresh ordering instead of being silently ignored"
        );
        assert!(
            !insert_body.contains("INSERT IGNORE INTO agent_event_edges"),
            "agent_event_edges must not hide ordering conflicts with INSERT IGNORE"
        );
    }

    #[test]
    fn run_checkpoints_identity_is_owner_bound() {
        let source = include_str!("storage.rs");
        let ddl = source
            .split("CREATE TABLE IF NOT EXISTS run_checkpoints")
            .nth(1)
            .and_then(|rest| {
                rest.split("CREATE TABLE IF NOT EXISTS run_display_projections")
                    .next()
            })
            .expect("run_checkpoints DDL");

        assert!(
            ddl.contains("checkpoint_id VARCHAR(64) NOT NULL"),
            "checkpoint_id must be explicit data, not the global table identity"
        );
        assert!(
            ddl.contains("PRIMARY KEY (user_id, checkpoint_id)"),
            "run_checkpoints primary key must be owner-bound"
        );
        assert!(
            !ddl.contains("checkpoint_id VARCHAR(64) PRIMARY KEY"),
            "run_checkpoints must not use a global checkpoint_id primary key"
        );
        assert!(
            source.contains("ALTER TABLE run_checkpoints ADD PRIMARY KEY (user_id, checkpoint_id)"),
            "schema bootstrap must reconcile only an absent owner-bound primary key"
        );
    }

    #[test]
    fn core_session_and_run_identities_are_owner_bound() {
        let source = include_str!("storage.rs");
        fn table_ddl<'a>(source: &'a str, table: &str) -> &'a str {
            let marker = format!("CREATE TABLE IF NOT EXISTS {table}");
            source
                .split(&marker)
                .nth(1)
                .and_then(|rest| rest.split(")\"").next())
                .expect("table DDL")
        }

        for (table, id_decl, old_decl, primary_key_ddl, ensure_ddl) in [
            (
                "agent_sessions",
                "session_id VARCHAR(64) NOT NULL",
                concat!("session_id VARCHAR(64) ", "PRIMARY KEY"),
                "PRIMARY KEY (user_id, session_id)",
                "ALTER TABLE agent_sessions ADD PRIMARY KEY (user_id, session_id)",
            ),
            (
                "agent_runs",
                "run_id VARCHAR(64) NOT NULL",
                concat!("run_id VARCHAR(64) ", "PRIMARY KEY"),
                "PRIMARY KEY (user_id, run_id)",
                "ALTER TABLE agent_runs ADD PRIMARY KEY (user_id, run_id)",
            ),
            (
                "run_display_projections",
                "run_id VARCHAR(64) NOT NULL",
                concat!("run_id VARCHAR(64) ", "PRIMARY KEY"),
                "PRIMARY KEY (user_id, run_id)",
                "ALTER TABLE run_display_projections ADD PRIMARY KEY (user_id, run_id)",
            ),
            (
                "prompt_request_records",
                "request_id VARCHAR(64) NOT NULL",
                concat!("request_id VARCHAR(64) ", "PRIMARY KEY"),
                "PRIMARY KEY (user_id, request_id)",
                "ALTER TABLE prompt_request_records ADD PRIMARY KEY (user_id, request_id)",
            ),
            (
                "session_state_revisions",
                "session_id VARCHAR(64) NOT NULL",
                concat!("session_id VARCHAR(64) ", "PRIMARY KEY"),
                "PRIMARY KEY (user_id, session_id)",
                "ALTER TABLE session_state_revisions ADD PRIMARY KEY (user_id, session_id)",
            ),
            (
                "session_device_leases",
                "lease_id VARCHAR(128) NOT NULL",
                concat!("lease_id VARCHAR(128) ", "PRIMARY KEY"),
                "PRIMARY KEY (user_id, lease_id)",
                "ALTER TABLE session_device_leases ADD PRIMARY KEY (user_id, lease_id)",
            ),
        ] {
            let ddl = table_ddl(source, table);
            assert!(
                ddl.contains(id_decl),
                "{table} identity column must stay ordinary data"
            );
            assert!(
                ddl.contains(primary_key_ddl),
                "{table} primary key must include user_id first"
            );
            assert!(
                !ddl.contains(old_decl),
                "{table} must not use a global id primary key"
            );
            assert!(
                source.contains(ensure_ddl),
                "{table} schema bootstrap must verify owner-bound primary key shape"
            );
        }
    }

    #[test]
    fn agent_runs_list_index_matches_seek_pagination_order() {
        let source = include_str!("storage.rs");
        assert!(
            source.contains("INDEX idx_agent_runs_user_updated_run (user_id, updated_at, run_id)"),
            "agent_runs run-list index must include the stable seek tie-breaker"
        );
        assert!(
            source.contains(
                "ALTER TABLE agent_runs ADD INDEX idx_agent_runs_user_updated_run (user_id, updated_at, run_id)"
            ),
            "agent_runs schema bootstrap must create the seek pagination index"
        );
        assert!(
            source.contains("\"idx_agent_runs_user_updated\""),
            "agent_runs schema bootstrap must remove the obsolete two-column run-list index"
        );
    }

    #[test]
    fn primary_key_shape_mismatch_fails_without_dropping_existing_key() {
        let source = include_str!("storage.rs");
        assert!(
            source.contains("primary key shape mismatch"),
            "schema bootstrap must fail loudly on incompatible primary keys"
        );
        assert!(
            !source.contains(concat!("DROP ", "PRIMARY KEY")),
            "schema bootstrap must not leave a table without a primary key if ADD PRIMARY KEY fails"
        );
    }

    #[test]
    fn owner_bound_edge_registry_pk_check_runs_after_create_table() {
        let source = include_str!("storage.rs");
        let create_pos = source
            .find("CREATE TABLE IF NOT EXISTS edge_agent_registry")
            .expect("edge_agent_registry CREATE TABLE");
        let ensure_pos = source
            .find("\"ALTER TABLE edge_agent_registry ADD PRIMARY KEY (user_id, registry_id)\"")
            .expect("edge_agent_registry primary-key ensure");

        assert!(
            create_pos < ensure_pos,
            "fresh databases must create edge_agent_registry before checking its primary-key shape"
        );
    }

    #[test]
    fn edge_pending_dispatch_identity_is_turn_scoped() {
        let source = include_str!("storage.rs");
        let production_source = source.split("#[cfg(test)]").next().unwrap_or(source);
        assert_eq!(
            EDGE_PENDING_DISPATCH_IDENTITY_COLUMNS,
            &[
                "user_id",
                "session_id",
                "run_id",
                "turn_chain_id",
                "request_id"
            ],
            "edge_pending_dispatch identity must be scoped to the owning turn"
        );
        assert!(
            !production_source.contains("dispatch_id BIGINT AUTO_INCREMENT"),
            "edge_pending_dispatch must not reintroduce a global AUTO_INCREMENT surrogate"
        );
        assert!(
            production_source.contains("&[\"dispatch_id\"]"),
            "legacy dispatch_id schemas must fail startup instead of silently preserving the old hot surrogate"
        );
    }

    #[test]
    fn canonical_owner_request_dispatch_schema_is_migrated_before_turn_scoped_creation() {
        let columns = EDGE_PENDING_DISPATCH_LEGACY_COLUMNS
            .iter()
            .map(|column| (*column).to_string())
            .collect::<BTreeSet<_>>();
        let primary_key = EDGE_PENDING_DISPATCH_LEGACY_PRIMARY_KEY
            .iter()
            .map(|column| (*column).to_string())
            .collect::<Vec<_>>();
        assert!(
            is_legacy_edge_pending_dispatch_shape(&columns, &primary_key),
            "the known 48f owner/request schema must select the explicit migration"
        );

        let mut mixed_columns = columns.clone();
        mixed_columns.insert("session_id".to_string());
        assert!(
            !is_legacy_edge_pending_dispatch_shape(&mixed_columns, &primary_key),
            "a partially migrated table must not be archived as the known legacy shape"
        );

        let source = include_str!("storage.rs");
        let migration_pos = source
            .find("migrate_legacy_edge_pending_dispatch_if_needed(&pool, &settings.database)")
            .expect("legacy edge dispatch migration invocation");
        let create_pos = source
            .find("CREATE TABLE IF NOT EXISTS edge_pending_dispatch")
            .expect("turn-scoped edge dispatch CREATE TABLE");
        assert!(
            migration_pos < create_pos,
            "legacy table must be archived before current edge dispatch schema creation"
        );
        assert!(
            source.contains("RENAME TABLE")
                && source.contains("WHERE status IN ('pending', 'dispatched')"),
            "migration must preserve terminal history and block active rows without inventing turn identity"
        );
    }

    #[test]
    fn context_manifest_items_identity_is_manifest_order_bound() {
        let source = include_str!("storage.rs");
        let ddl = source
            .split("CREATE TABLE IF NOT EXISTS context_manifest_items")
            .nth(1)
            .and_then(|rest| rest.split(")\"").next())
            .expect("context_manifest_items DDL");

        assert!(
            ddl.contains("PRIMARY KEY (manifest_id, item_order)"),
            "context_manifest_items must use the manifest-local item ordering identity"
        );
        assert!(
            !ddl.contains("id BIGINT AUTO_INCREMENT"),
            "context_manifest_items must not reintroduce a global AUTO_INCREMENT surrogate"
        );
        assert!(
            source.contains(
                "ALTER TABLE context_manifest_items ADD PRIMARY KEY (manifest_id, item_order)"
            ),
            "schema bootstrap must verify the manifest/order primary key"
        );
        assert!(
            source.contains("&[\"uq_manifest_item_order\"]"),
            "legacy unique-key-plus-surrogate schemas must fail startup instead of preserving the old shape"
        );
    }

    #[test]
    fn session_state_item_events_identity_is_owner_event_bound() {
        let source = include_str!("storage.rs");
        let ddl = source
            .split("CREATE TABLE IF NOT EXISTS session_state_item_events")
            .nth(1)
            .and_then(|rest| rest.split(")\"").next())
            .expect("session_state_item_events DDL");

        assert!(
            ddl.contains("event_id VARCHAR(64) NOT NULL"),
            "session_state_item_events must use an application-generated event identity"
        );
        assert!(
            ddl.contains("PRIMARY KEY (user_id, event_id)"),
            "session_state_item_events must keep event identity owner-bound"
        );
        assert!(
            ddl.contains("idx_state_events_item_created (item_id, created_at, event_id)"),
            "item audit queries must keep a deterministic event_id tie-breaker"
        );
        assert!(
            ddl.contains(
                "idx_state_events_owner_session_created (user_id, session_id, created_at, event_id)"
            ),
            "owner/session audit queries must keep a deterministic event_id tie-breaker"
        );
        assert!(
            !ddl.contains("id BIGINT AUTO_INCREMENT"),
            "session_state_item_events must not reintroduce a global AUTO_INCREMENT surrogate"
        );
        assert!(
            source.contains(
                "ALTER TABLE session_state_item_events ADD PRIMARY KEY (user_id, event_id)"
            ),
            "schema bootstrap must verify the owner/event primary key"
        );
        assert!(
            source.contains(
                "\"session_state_item_events\",\n        &[\"user_id\", \"event_id\"],\n        &[\"id\"]"
            ),
            "legacy id schemas must fail startup instead of preserving the old hot surrogate"
        );
    }

    #[test]
    fn auth_join_tables_use_product_identity_without_surrogate_ids() {
        let source = include_str!("storage.rs");
        let user_roles = source
            .split("CREATE TABLE IF NOT EXISTS auth_user_roles")
            .nth(1)
            .and_then(|rest| rest.split(")\"").next())
            .expect("auth_user_roles DDL");

        assert!(
            user_roles.contains("PRIMARY KEY (user_id, role_id)"),
            "auth_user_roles identity is the user/role grant"
        );
        assert!(
            !user_roles.contains("id BIGINT AUTO_INCREMENT"),
            "auth_user_roles must not reintroduce a global AUTO_INCREMENT surrogate"
        );
        assert!(
            !user_roles.contains("idx_auth_user_roles_user_id"),
            "auth_user_roles primary key already covers user_id lookups"
        );
        assert!(
            source.contains("ALTER TABLE auth_user_roles ADD PRIMARY KEY (user_id, role_id)"),
            "schema bootstrap must verify auth_user_roles primary-key shape"
        );
        assert!(
            source.contains("&[\"uq_auth_user_roles_user_role\"]"),
            "legacy auth_user_roles unique-key-plus-surrogate schemas must fail startup"
        );
    }

    #[test]
    fn provider_request_replay_schema_is_shared_atomic_and_expiring() {
        let source = include_str!("storage.rs");
        let replay = source
            .split("CREATE TABLE IF NOT EXISTS auth_provider_request_replay")
            .nth(1)
            .and_then(|rest| rest.split(")\"").next())
            .expect("auth_provider_request_replay DDL");
        assert!(
            replay.contains("PRIMARY KEY (provider, request_authorization_id)"),
            "provider request replay identity must be shared and provider-scoped"
        );
        assert!(
            replay.contains("request_authorization_id VARCHAR(512) NOT NULL"),
            "provider request replay identity must fit the full signed request nonce"
        );
        assert!(
            replay.contains("expires_at_unix BIGINT NOT NULL")
                && replay.contains(
                    "idx_auth_provider_request_replay_expires (expires_at_unix, provider, request_authorization_id)"
                ),
            "provider request replay rows must have an indexed TTL boundary"
        );
        assert!(
            !replay.contains("id BIGINT AUTO_INCREMENT"),
            "provider request replay must not reintroduce ownerless surrogate identity"
        );
    }

    #[test]
    fn mcp_registry_tables_use_owner_bound_string_identity() {
        let source = include_str!("storage.rs");
        let servers = source
            .split("CREATE TABLE IF NOT EXISTS mcp_servers")
            .nth(1)
            .and_then(|rest| rest.split(")\"").next())
            .expect("mcp_servers DDL");
        let bindings = source
            .split("CREATE TABLE IF NOT EXISTS mcp_bindings")
            .nth(1)
            .and_then(|rest| rest.split(")\"").next())
            .expect("mcp_bindings DDL");
        let tools = source
            .split("CREATE TABLE IF NOT EXISTS mcp_tools")
            .nth(1)
            .and_then(|rest| rest.split(")\"").next())
            .expect("mcp_tools DDL");

        assert!(servers.contains("id VARCHAR(64) NOT NULL"));
        assert!(servers.contains("PRIMARY KEY (owner_user_id, id)"));
        assert!(bindings.contains("id VARCHAR(64) NOT NULL"));
        assert!(bindings.contains("mcp_id VARCHAR(64) NOT NULL"));
        assert!(bindings.contains("PRIMARY KEY (owner_user_id, id)"));
        assert!(tools.contains("owner_user_id VARCHAR(128) NOT NULL"));
        assert!(tools.contains("binding_id VARCHAR(64) NOT NULL"));
        assert!(tools.contains("PRIMARY KEY (owner_user_id, binding_id, tool_name)"));

        for (table, ddl) in [
            ("mcp_servers", servers),
            ("mcp_bindings", bindings),
            ("mcp_tools", tools),
        ] {
            assert!(
                !ddl.contains("AUTO_INCREMENT"),
                "{table} must not reintroduce a global AUTO_INCREMENT surrogate"
            );
        }
        assert!(source.contains("ALTER TABLE mcp_servers ADD PRIMARY KEY (owner_user_id, id)"));
        assert!(source.contains("ALTER TABLE mcp_bindings ADD PRIMARY KEY (owner_user_id, id)"));
        assert!(source.contains(
            "ALTER TABLE mcp_tools ADD PRIMARY KEY (owner_user_id, binding_id, tool_name)"
        ));
        assert!(
            source.contains("[('id', 64), ('mcp_id', 64)]")
                || source.contains("[(\"id\", 64), (\"mcp_id\", 64)]"),
            "legacy numeric mcp_bindings ids must fail varchar width checks"
        );
        assert!(
            source.contains("&[\"uq_mcp_tools_binding_tool\", \"uq_mcp_tools_binding_public\"]"),
            "legacy tool unique-key-plus-surrogate schemas must fail startup"
        );
    }

    #[test]
    fn index_shape_mismatch_fails_without_drop_add_window() {
        let source = include_str!("storage.rs");
        let body = source
            .split("async fn ensure_index_shape")
            .nth(1)
            .and_then(|rest| rest.split("async fn ensure_primary_key_shape").next())
            .expect("ensure_index_shape body");
        assert!(
            body.contains("index shape mismatch"),
            "schema bootstrap must fail loudly on incompatible index shapes"
        );
        assert!(
            !body.contains("drop_index_if_present"),
            "index shape mismatch must not leave a table temporarily without the expected index"
        );
    }

    #[test]
    fn core_schema_file_lock_uses_blocking_pool() {
        let source = include_str!("storage.rs");
        let body = source
            .split("async fn acquire_core_schema_file_lock_blocking")
            .nth(1)
            .and_then(|rest| rest.split("fn unique_event_ids").next())
            .expect("file lock async wrapper body");
        assert!(
            body.contains("tokio::task::spawn_blocking"),
            "blocking file locks must not run on async worker threads"
        );
    }

    #[test]
    fn obsolete_shape_checks_fail_without_dropping_tables() {
        let source = include_str!("storage.rs");
        let obsolete_shape_body = source
            .split("async fn fail_if_obsolete_shape")
            .nth(1)
            .and_then(|rest| {
                rest.split("async fn fail_if_required_columns_missing_or_nullable")
                    .next()
            })
            .expect("obsolete shape helper body");
        let missing_columns_body = source
            .split("async fn fail_if_required_columns_missing_or_nullable")
            .nth(1)
            .and_then(|rest| {
                rest.split("async fn fail_if_varchar_columns_shorter_than")
                    .next()
            })
            .expect("required column helper body");
        let varchar_width_body = source
            .split("async fn fail_if_varchar_columns_shorter_than")
            .nth(1)
            .and_then(|rest| rest.split("async fn existing_index_columns").next())
            .expect("varchar width helper body");

        for body in [
            obsolete_shape_body,
            missing_columns_body,
            varchar_width_body,
        ] {
            assert!(
                body.contains("requires manual migration before startup"),
                "incompatible existing tables must fail startup instead of being recreated"
            );
            assert!(
                !body.contains("DROP TABLE"),
                "incompatible existing table shape must not silently drop production data"
            );
        }
        assert!(
            varchar_width_body.contains("try_get::<Option<i64>, _>(\"CHARACTER_MAXIMUM_LENGTH\")"),
            "MatrixOne exposes CHARACTER_MAXIMUM_LENGTH as signed BIGINT; decoding as u64 breaks startup"
        );
        assert!(
            !varchar_width_body.contains("try_get::<Option<u64>, _>(\"CHARACTER_MAXIMUM_LENGTH\")"),
            "varchar width introspection must not decode signed information_schema values as unsigned"
        );
    }

    #[test]
    fn retired_external_auth_tables_are_not_dropped_by_schema_bootstrap() {
        let source = include_str!("storage.rs");
        let ensure_body = source
            .split("pub async fn ensure_core_schema")
            .nth(1)
            .and_then(|rest| rest.split("#[cfg(test)]").next())
            .expect("ensure_core_schema body");
        for table in ["auth_external_sessions", "auth_external_identities"] {
            assert!(
                !ensure_body.contains(&format!("DROP TABLE IF EXISTS {table}")),
                "retired external auth table {table} must be left for an explicit migration, not dropped during startup"
            );
        }
    }

    #[test]
    fn model_registry_context_window_has_no_default() {
        let source = include_str!("storage.rs");
        let ddl_source = source
            .split("CREATE TABLE IF NOT EXISTS infra_llm_models")
            .nth(1)
            .and_then(|rest| {
                rest.split("CREATE TABLE IF NOT EXISTS model_gateways")
                    .next()
            })
            .expect("infra_llm_models DDL");

        assert!(
            ddl_source.contains("context_window INT NOT NULL,"),
            "model registry context_window must be explicit metadata"
        );
        assert!(
            ddl_source.contains(
                "CONSTRAINT chk_infra_llm_models_context_window CHECK (context_window > 0)"
            ),
            "model registry must reject non-positive context_window values at the schema boundary"
        );
        assert!(
            !ddl_source.contains("context_window INT NOT NULL DEFAULT"),
            "model registry must not silently replace missing context_window with 200K"
        );
    }

    #[test]
    fn core_schema_has_no_duplicate_create_table_declarations() {
        let source = include_str!("storage.rs");
        let ddl_source = source
            .split("#[cfg(test)]")
            .next()
            .expect("production storage DDL source");

        let marker = "CREATE TABLE IF NOT EXISTS ";
        let mut counts = std::collections::BTreeMap::<String, usize>::new();
        for rest in ddl_source.split(marker).skip(1) {
            let table = rest
                .chars()
                .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
                .collect::<String>();
            assert!(
                !table.is_empty(),
                "CREATE TABLE declaration must include a parseable table name"
            );
            *counts.entry(table).or_default() += 1;
        }

        let duplicates: Vec<_> = counts
            .iter()
            .filter_map(|(table, count)| (*count > 1).then_some(format!("{table}:{count}")))
            .collect();
        assert!(
            duplicates.is_empty(),
            "duplicate CREATE TABLE declarations are not allowed: {}",
            duplicates.join(", ")
        );
    }

    #[test]
    fn session_artifacts_schema_bounds_retention_policy_values() {
        let source = include_str!("storage.rs");
        let ddl_source = source
            .split("#[cfg(test)]")
            .next()
            .expect("storage production source");
        assert!(
            ddl_source.contains("chk_session_artifacts_retention_policy")
                && ddl_source
                    .contains("retention_policy IN ('default', 'permanent', 'project_long_term')"),
            "session_artifacts must reject retention policies the sweeper cannot interpret"
        );
    }

    #[test]
    fn cleanup_expired_agent_events_uses_bounded_delete_chunks() {
        let source = include_str!("storage.rs");
        let body = source
            .split("pub async fn cleanup_expired_data")
            .nth(1)
            .and_then(|rest| rest.split("#[cfg(test)]").next())
            .expect("cleanup_expired_data body");
        assert!(body.contains("AGENT_EVENT_DELETE_CHUNK_SIZE"));
        assert!(
            body.contains(".chunks(AGENT_EVENT_DELETE_CHUNK_SIZE)"),
            "expired event cleanup must not build one unbounded DELETE tuple-IN statement"
        );
        assert!(
            body.contains("let mut tx = pool") && body.contains("tx.commit()"),
            "expired event edge/event cleanup must run inside one transaction"
        );
        let edge_delete = body
            .find("delete_agent_event_edges_for_owned_event_ids(&mut *tx")
            .expect("expired event cleanup must delete edge rows with the transaction executor");
        let event_delete = body
            .find("DELETE FROM agent_events WHERE (user_id, event_id) IN")
            .expect("expired event cleanup must delete selected agent event rows");
        assert!(
            edge_delete < event_delete,
            "expired event cleanup must delete edge rows before event rows inside the same transaction"
        );
        assert!(
            body.contains("table: \"agent_event_edges\""),
            "expired event cleanup should report edge cleanup separately"
        );
    }

    #[test]
    fn cleanup_expired_data_uses_ordered_per_table_batches() {
        let source = include_str!("storage.rs");
        let body = source
            .split("pub async fn cleanup_expired_data")
            .nth(1)
            .and_then(|rest| rest.split("#[cfg(test)]").next())
            .expect("cleanup_expired_data body");
        for constant in [
            "AUTH_REFRESH_TOKEN_BATCH_LIMIT",
            "AUTH_TOKEN_BATCH_LIMIT",
            "AUTH_PROVIDER_REQUEST_REPLAY_BATCH_LIMIT",
            "TASK_LEASE_BATCH_LIMIT",
            "AUTH_AUDIT_LOG_BATCH_LIMIT",
            "PROMPT_REQUEST_BATCH_LIMIT",
            "PROMPT_REQUEST_MAX_BATCHES_PER_RUN",
            "AGENT_EVENT_BATCH_LIMIT",
        ] {
            assert!(
                body.contains(constant),
                "cleanup_expired_data must keep independent batch constants; missing {constant}"
            );
        }
        for ordering in [
            "ORDER BY created_at ASC, token_id ASC",
            "ORDER BY expires_at_unix ASC, provider ASC, request_authorization_id ASC",
            "ORDER BY expires_at ASC, user_id ASC, task_id ASC",
            "ORDER BY created_at ASC, log_id ASC",
            "ORDER BY p.created_at_unix_ms ASC, p.user_id ASC, p.request_id ASC",
            "ORDER BY created_at ASC, user_id ASC, event_id ASC",
        ] {
            assert!(
                body.contains(ordering),
                "cleanup_expired_data DELETE/SELECT batches must be deterministic; missing {ordering}"
            );
        }
    }

    #[test]
    fn cleanup_expired_prompt_requests_are_parent_bound_and_active_guarded() {
        let source = include_str!("storage.rs");
        let body = source
            .split("pub async fn cleanup_expired_data")
            .nth(1)
            .and_then(|rest| rest.split("#[cfg(test)]").next())
            .expect("cleanup_expired_data body");
        assert!(
            body.contains("policy.prompt_request_days"),
            "prompt cleanup must be governed by explicit retention policy"
        );
        assert!(
            body.contains(
                "WHERE p.created_at_unix_ms < UNIX_TIMESTAMP(DATE_SUB(NOW(6), INTERVAL {} DAY)) * 1000"
            ),
            "prompt cleanup must use the numeric retention key instead of MatrixOne DATETIME comparisons"
        );
        assert!(
            body.contains("policy.prompt_request_days") && body.contains("format!("),
            "prompt cleanup may only inline the retention day literal from its typed u32 policy"
        );
        assert!(
            body.contains("PROMPT_REQUEST_DELETE_CHUNK_SIZE")
                && body.contains(".chunks(PROMPT_REQUEST_DELETE_CHUNK_SIZE)"),
            "prompt cleanup must chunk tuple deletes"
        );
        assert!(
            body.contains("for _ in 0..PROMPT_REQUEST_MAX_BATCHES_PER_RUN")
                && body.contains(
                    "expired_prompt_request_ids.len() < PROMPT_REQUEST_BATCH_LIMIT as usize"
                ),
            "prompt cleanup must drain multiple bounded batches and stop when the selected batch is partial"
        );
        assert!(
            body.contains("FROM prompt_request_records p")
                && body.contains("LEFT JOIN agent_sessions s")
                && body.contains("LEFT JOIN agent_runs r"),
            "prompt cleanup must select parent request rows with session/run guards"
        );
        assert!(
            body.contains("s.status IN ('ended', 'closed', 'cancelled', 'deleting')")
                && body.contains("r.status IN ('completed', 'delegated', 'failed', 'cancelled')"),
            "prompt cleanup must avoid active sessions and non-terminal runs"
        );
        let child_delete = body
            .find("DELETE FROM prompt_deltas WHERE (user_id, session_id, request_id) IN")
            .expect("prompt cleanup must delete child delta rows");
        let parent_delete = body
            .find("DELETE FROM prompt_request_records WHERE (user_id, session_id, request_id) IN")
            .expect("prompt cleanup must delete selected parent rows");
        assert!(
            child_delete < parent_delete,
            "prompt cleanup must delete child deltas before parent request records"
        );
        assert!(
            body.contains("table: \"prompt_deltas\"")
                && body.contains("table: \"prompt_request_records\""),
            "prompt cleanup should report child and parent row counts separately"
        );
    }

    #[test]
    fn cleanup_expired_data_does_not_age_delete_replay_or_tool_output_facts() {
        let source = include_str!("storage.rs");
        let body = source
            .split("pub async fn cleanup_expired_data")
            .nth(1)
            .and_then(|rest| rest.split("#[cfg(test)]").next())
            .expect("cleanup_expired_data body");
        for table in [
            "DELETE FROM agent_run_events",
            "DELETE FROM session_tool_outputs",
            "DELETE FROM session_tool_output_batches",
        ] {
            assert!(
                !body.contains(table),
                "global age-based cleanup must not directly delete replay/tool-output facts: {table}"
            );
        }
        assert!(
            body.contains("DELETE FROM auth_provider_request_replay")
                && body.contains("WHERE expires_at_unix < UNIX_TIMESTAMP()"),
            "provider request replay guards are nonce TTL facts and must expire by their signed token boundary, not generic retention age"
        );
    }

    #[test]
    fn tool_output_schema_drops_ownerless_legacy_indexes() {
        let source = include_str!("storage.rs");
        for removed in [
            "idx_tool_output_batches_session",
            "idx_tool_output_batches_run_status",
            "idx_tool_output_batches_run_created",
            "idx_tool_output_batches_session_created",
            "idx_tool_outputs_tool_created",
            "idx_tool_outputs_session_tool_score",
            "idx_tool_outputs_status_created",
            "idx_tool_outputs_batch",
            "idx_tool_outputs_run_created",
            "idx_tool_outputs_session_created",
        ] {
            assert!(
                source.contains(removed),
                "schema bootstrap must explicitly drop legacy ownerless index {removed}"
            );
        }
        assert!(
            source.contains(
                "ALTER TABLE session_tool_outputs ADD INDEX idx_tool_outputs_parent (user_id, parent_output_id)"
            ),
            "parent output index shape must be reconciled, not left with a legacy ownerless shape"
        );
    }

    #[test]
    fn rows_affected_to_i64_fails_loudly_on_overflow() {
        assert_eq!(
            rows_affected_to_i64(i64::MAX as u64, "event_count_delta").unwrap(),
            i64::MAX
        );
        let err = rows_affected_to_i64(i64::MAX as u64 + 1, "event_count_delta")
            .expect_err("overflow must fail");
        let err = err.to_string();
        assert!(
            err.contains("event_count_delta") && err.contains("exceeds i64::MAX"),
            "error should identify conversion context and overflow: {err}"
        );
    }

    #[test]
    fn session_activity_timestamp_updates_are_coalesced() {
        let source = include_str!("storage.rs");
        let coalesced = "last_active_at < DATE_SUB(NOW(6), INTERVAL 1 SECOND)";
        assert!(
            source.matches(coalesced).count() >= 6,
            "session activity hot-path timestamp updates should be coalesced to reduce indexed timestamp churn"
        );
        assert!(
            !source.contains(
                "SET event_count = event_count + ?, \\\n                 updated_at = NOW(6),"
            ),
            "event_count bump must not force indexed timestamp columns on every event"
        );
    }

    #[test]
    fn eval_calibration_assessments_schema_matches_runtime_queries() {
        let ddl = EVAL_CALIBRATION_ASSESSMENTS_CREATE_SQL;
        for column in [
            "calibration_id VARCHAR(64) NOT NULL",
            "user_id        VARCHAR(128) NOT NULL",
            "agent_id       VARCHAR(255)",
            "session_id     VARCHAR(64) NOT NULL",
            "confidence     DECIMAL(5,4) NOT NULL",
            "quality_score  DECIMAL(5,4) NOT NULL",
            "created_at     DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6)",
            "PRIMARY KEY (user_id, calibration_id)",
        ] {
            assert!(ddl.contains(column), "missing calibration column: {column}");
        }
        for index in [
            "INDEX idx_eval_calibration_user_created (user_id, created_at)",
            "INDEX idx_eval_calibration_user_agent_created (user_id, agent_id, created_at)",
            "INDEX idx_eval_calibration_session (user_id, session_id, created_at)",
        ] {
            assert!(ddl.contains(index), "missing calibration index: {index}");
        }
        let source = include_str!("storage.rs");
        for reconcile in [
            "ALTER TABLE eval_calibration_assessments ADD INDEX idx_eval_calibration_user_created (user_id, created_at)",
            "ALTER TABLE eval_calibration_assessments ADD INDEX idx_eval_calibration_user_agent_created (user_id, agent_id, created_at)",
            "ALTER TABLE eval_calibration_assessments ADD INDEX idx_eval_calibration_session (user_id, session_id, created_at)",
            "ALTER TABLE eval_calibration_assessments ADD PRIMARY KEY (user_id, calibration_id)",
        ] {
            assert!(
                source.contains(reconcile),
                "missing calibration index reconcile: {reconcile}"
            );
        }
    }
}
