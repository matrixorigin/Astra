use crate::auth::DatabaseUserRecord;
use crate::auth::session::SessionRecord;
use astra_core::{
    DedicatedPool, ErrorResponse, MatrixOneSettings, connect_matrixone, internal_error,
};
use axum::{Json, http::StatusCode};
use fs2::FileExt;
use sqlx::{Executor, MySql, Pool, QueryBuilder, Row, query};
use std::collections::HashSet;
use std::collections::{BTreeSet, HashMap};
use std::fs::OpenOptions;
use std::sync::OnceLock;
use uuid::Uuid;

const CAUSAL_EDGE_KIND: &str = "causal";
static CORE_SCHEMA_INIT_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

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

pub async fn load_agent_event_count<'e, E>(
    executor: E,
    session_id: &str,
) -> Result<i64, sqlx::Error>
where
    E: Executor<'e, Database = MySql>,
{
    let row = query("SELECT COUNT(*) AS event_count FROM agent_events WHERE session_id = ?")
        .bind(session_id)
        .fetch_one(executor)
        .await?;
    Ok(row.try_get::<i64, _>("event_count").unwrap_or(0))
}

pub async fn upsert_agent_session_event_count<'e, E>(
    executor: E,
    session_id: &str,
    user_id: &str,
    event_count: i64,
) -> Result<(), sqlx::Error>
where
    E: Executor<'e, Database = MySql>,
{
    query(
        "INSERT INTO agent_sessions \
         (session_id, user_id, status, event_count, created_at, updated_at, last_active_at) \
         VALUES (?, ?, 'active', ?, NOW(), NOW(), NOW()) \
         ON DUPLICATE KEY UPDATE \
         event_count = ?, \
         updated_at = NOW(), \
         last_active_at = NOW()",
    )
    .bind(session_id)
    .bind(user_id)
    .bind(event_count)
    .bind(event_count)
    .execute(executor)
    .await?;
    Ok(())
}

pub async fn insert_agent_event_edges<'e, E>(
    executor: E,
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
        "INSERT IGNORE INTO agent_event_edges \
         (child_event_id, parent_event_id, relation_kind, parent_order) ",
    );
    builder.push_values(
        normalized.iter().enumerate(),
        |mut row, (idx, parent_event_id)| {
            row.push_bind(child_event_id)
                .push_bind(parent_event_id)
                .push_bind(CAUSAL_EDGE_KIND)
                .push_bind(i32::try_from(idx).unwrap_or(i32::MAX));
        },
    );
    builder.build().execute(executor).await?;
    Ok(())
}

pub async fn load_agent_event_parent_ids<'e, E>(
    executor: E,
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
         FROM agent_event_edges WHERE relation_kind = ",
    );
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

pub async fn delete_agent_event_edges_for_event_ids<'e, E>(
    executor: E,
    event_ids: &[String],
) -> Result<u64, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = MySql>,
{
    let event_ids = unique_event_ids(event_ids);
    if event_ids.is_empty() {
        return Ok(0);
    }

    let mut builder =
        QueryBuilder::<MySql>::new("DELETE FROM agent_event_edges WHERE child_event_id IN (");
    {
        let mut child_ids = builder.separated(", ");
        for event_id in &event_ids {
            child_ids.push_bind(*event_id);
        }
        child_ids.push_unseparated(")");
    }
    builder.push(" OR parent_event_id IN (");
    {
        let mut parent_ids = builder.separated(", ");
        for event_id in &event_ids {
            parent_ids.push_bind(*event_id);
        }
        parent_ids.push_unseparated(")");
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

async fn column_exists(
    pool: &Pool<MySql>,
    schema: &str,
    table: &str,
    column: &str,
) -> Result<bool, sqlx::Error> {
    let row = query(
        "SELECT COUNT(*) AS count FROM INFORMATION_SCHEMA.COLUMNS
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND COLUMN_NAME = ?",
    )
    .bind(schema)
    .bind(table)
    .bind(column)
    .fetch_one(pool)
    .await?;
    Ok(row.try_get::<i64, _>("count").unwrap_or(0) > 0)
}

async fn varchar_column_max_len(
    pool: &Pool<MySql>,
    schema: &str,
    table: &str,
    column: &str,
) -> Result<Option<i64>, sqlx::Error> {
    let row = query(
        "SELECT CHARACTER_MAXIMUM_LENGTH AS max_len
         FROM INFORMATION_SCHEMA.COLUMNS
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND COLUMN_NAME = ?
         LIMIT 1",
    )
    .bind(schema)
    .bind(table)
    .bind(column)
    .fetch_optional(pool)
    .await?;
    Ok(row.and_then(|row| row.try_get::<i64, _>("max_len").ok()))
}

async fn widen_varchar_if_shorter(
    pool: &Pool<MySql>,
    schema: &str,
    table: &str,
    column: &str,
    min_len: i64,
    ddl: &str,
) -> Result<(), sqlx::Error> {
    if let Some(current_len) = varchar_column_max_len(pool, schema, table, column).await?
        && current_len < min_len
    {
        query(ddl).execute(pool).await?;
    }
    Ok(())
}

async fn index_exists(
    pool: &Pool<MySql>,
    schema: &str,
    table: &str,
    index: &str,
) -> Result<bool, sqlx::Error> {
    let row = query(
        "SELECT COUNT(*) AS count FROM INFORMATION_SCHEMA.STATISTICS
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND INDEX_NAME = ?",
    )
    .bind(schema)
    .bind(table)
    .bind(index)
    .fetch_one(pool)
    .await?;
    Ok(row.try_get::<i64, _>("count").unwrap_or(0) > 0)
}

async fn add_column_if_missing(
    pool: &Pool<MySql>,
    schema: &str,
    table: &str,
    column: &str,
    ddl: &str,
) -> Result<(), sqlx::Error> {
    if !column_exists(pool, schema, table, column).await? {
        query(ddl).execute(pool).await?;
    }
    Ok(())
}

async fn drop_column_if_exists(
    pool: &Pool<MySql>,
    schema: &str,
    table: &str,
    column: &str,
) -> Result<(), sqlx::Error> {
    debug_assert!(
        !table.contains('`') && !column.contains('`'),
        "identifiers must not contain backticks"
    );
    if column_exists(pool, schema, table, column).await? {
        let ddl = format!("ALTER TABLE `{table}` DROP COLUMN `{column}`");
        query(&ddl).execute(pool).await?;
    }
    Ok(())
}

async fn add_index_if_missing(
    pool: &Pool<MySql>,
    schema: &str,
    table: &str,
    index: &str,
    ddl: &str,
) -> Result<(), sqlx::Error> {
    if !index_exists(pool, schema, table, index).await? {
        query(ddl).execute(pool).await?;
    }
    Ok(())
}

async fn drop_index_if_exists(
    pool: &Pool<MySql>,
    schema: &str,
    table: &str,
    index: &str,
) -> Result<(), sqlx::Error> {
    debug_assert!(
        !table.contains('`') && !index.contains('`'),
        "identifiers must not contain backticks"
    );
    if index_exists(pool, schema, table, index).await? {
        let ddl = format!("ALTER TABLE `{table}` DROP INDEX `{index}`");
        query(&ddl).execute(pool).await?;
    }
    Ok(())
}

fn sql_decode_error(message: impl Into<String>) -> sqlx::Error {
    sqlx::Error::Decode(Box::new(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        message.into(),
    )))
}

fn json_value_type(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

fn promote_legacy_fallback_model_quirks(raw: &str) -> Result<Option<String>, String> {
    let mut value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| format!("parse quirks JSON: {e}"))?;
    let Some(object) = value.as_object_mut() else {
        return Ok(None);
    };
    let Some(legacy_value) = object.remove("fallback_model") else {
        return Ok(None);
    };

    let fallback_model = match legacy_value {
        serde_json::Value::String(model) => model.trim().to_string(),
        serde_json::Value::Null => String::new(),
        other => {
            return Err(format!(
                "legacy fallback_model must be a string, got {}",
                json_value_type(&other)
            ));
        }
    };

    if !fallback_model.is_empty() && !object.contains_key("fallback_chain") {
        object.insert(
            "fallback_chain".to_string(),
            serde_json::Value::Array(vec![serde_json::Value::String(fallback_model)]),
        );
    }

    serde_json::to_string(&value)
        .map(Some)
        .map_err(|e| format!("serialize migrated quirks JSON: {e}"))
}

async fn migrate_model_fallback_chain(pool: &Pool<MySql>) -> Result<u64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let rows = query(
        "SELECT model_id, CAST(quirks AS CHAR) AS quirks_json \
         FROM infra_llm_models \
         WHERE quirks IS NOT NULL",
    )
    .fetch_all(&mut *tx)
    .await?;

    let mut migrated = 0;
    for row in rows {
        let model_id: String = row.try_get("model_id")?;
        let quirks_json: String = row.try_get("quirks_json")?;
        let Some(updated_quirks) =
            promote_legacy_fallback_model_quirks(&quirks_json).map_err(|e| {
                sql_decode_error(format!(
                    "fallback_model to fallback_chain migration failed for model_id={model_id}: {e}"
                ))
            })?
        else {
            continue;
        };

        let result = query("UPDATE infra_llm_models SET quirks = ? WHERE model_id = ?")
            .bind(updated_quirks)
            .bind(&model_id)
            .execute(&mut *tx)
            .await?;
        migrated += result.rows_affected();
    }

    tx.commit().await?;
    Ok(migrated)
}

pub async fn ensure_core_schema(
    settings: &MatrixOneSettings,
    bootstrap_catalog: &str,
) -> Result<(), sqlx::Error> {
    // Tests and startup paths can race on schema bootstrap inside the same process.
    // Serialize schema setup so migration markers and DDL stay idempotent.
    let _init_guard = CORE_SCHEMA_INIT_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let _file_lock = acquire_core_schema_file_lock(settings)?;

    if std::env::var("ASTRA_AUTO_CREATE_DATABASE")
        .map(|v| v == "1")
        .unwrap_or(false)
    {
        ensure_matrixone_database_exists(settings, bootstrap_catalog).await?;
    }
    let pool = connect_matrixone(settings).await?;

    // Auth
    query(
        "CREATE TABLE IF NOT EXISTS auth_users (
            user_id VARCHAR(64) PRIMARY KEY,
            username VARCHAR(50) NOT NULL UNIQUE,
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
            id BIGINT AUTO_INCREMENT PRIMARY KEY,
            user_id VARCHAR(64) NOT NULL,
            role_id VARCHAR(64) NOT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            UNIQUE KEY uq_auth_user_roles_user_role (user_id, role_id),
            INDEX idx_auth_user_roles_user_id (user_id),
            INDEX idx_auth_user_roles_role_id (role_id)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS auth_refresh_tokens (
            token_id VARCHAR(64) PRIMARY KEY,
            user_id VARCHAR(64) NOT NULL,
            token_hash VARCHAR(255) NOT NULL,
            token_prefix VARCHAR(16) NULL,
            expires_at DATETIME(6) NOT NULL,
            is_revoked SMALLINT NOT NULL DEFAULT 0,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            UNIQUE KEY uq_auth_refresh_tokens_hash (token_hash),
            INDEX idx_auth_refresh_tokens_user_expires (user_id, expires_at),
            INDEX idx_auth_refresh_tokens_expires_at (expires_at),
            INDEX idx_auth_refresh_tokens_prefix (token_prefix)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS auth_tokens (
            token_id VARCHAR(64) PRIMARY KEY,
            type VARCHAR(50) NOT NULL,
            provider VARCHAR(50) NOT NULL,
            encrypted_value TEXT NULL,
            secret_ref VARCHAR(255) NULL,
            is_active SMALLINT NOT NULL DEFAULT 1,
            scope_user_id VARCHAR(64) NULL,
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
        "CREATE TABLE IF NOT EXISTS auth_audit_logs (
            log_id VARCHAR(64) PRIMARY KEY,
            user_id VARCHAR(64) NOT NULL,
            action VARCHAR(50) NOT NULL,
            resource_type VARCHAR(50) NULL,
            resource_id VARCHAR(64) NULL,
            details JSON NULL,
            ip_address VARCHAR(45) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_auth_audit_logs_user_created (user_id, created_at)
        )",
    )
    .execute(&pool)
    .await?;

    // Sessions / events core
    query(
        "CREATE TABLE IF NOT EXISTS agent_sessions (
            session_id VARCHAR(64) PRIMARY KEY,
            user_id VARCHAR(64) NOT NULL,
            agent_id VARCHAR(64) NULL,
            title VARCHAR(255) NULL,
            status VARCHAR(20) NOT NULL DEFAULT 'active',
            event_count BIGINT NOT NULL DEFAULT 0,
            last_event_id VARCHAR(64) NULL,
            summary_status VARCHAR(20) NULL,
            summary_job_id VARCHAR(64) NULL,
            vector_db_snapshot_id VARCHAR(64) NULL,
            metadata JSON NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            ended_at DATETIME(6) NULL,
            last_active_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            active_plan_id VARCHAR(64) NULL,
            INDEX idx_agent_sessions_user_status_updated (user_id, status, updated_at),
            INDEX idx_agent_sessions_user_last_active (user_id, last_active_at),
            INDEX idx_agent_sessions_agent_status (agent_id, status),
            INDEX idx_agent_sessions_active_plan_id (active_plan_id)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS agent_events (
            event_id VARCHAR(64) PRIMARY KEY,
            session_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(64) NOT NULL,
            agent_id VARCHAR(64) NULL,
            agent_version VARCHAR(32) NULL,
            event_type VARCHAR(64) NOT NULL,
            content LONGTEXT NULL,
            parent_event_id VARCHAR(64) NULL,
            causal_chain_id VARCHAR(64) NULL,
            run_id VARCHAR(64) NULL,
            parent_run_id VARCHAR(64) NULL,
            turn_id VARCHAR(64) NULL,
            turn_seq BIGINT NULL,
            round_index BIGINT NULL,
            tool_call_id VARCHAR(128) NULL,
            parent_agent_id VARCHAR(128) NULL,
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
            INDEX idx_agent_events_session_created (session_id, created_at),
            INDEX idx_agent_events_session_type_created (session_id, event_type, created_at),
            INDEX idx_agent_events_session_model_created (session_id, llm_model_used, created_at DESC),
            INDEX idx_agent_events_session_parent (session_id, parent_event_id),
            INDEX idx_agent_events_user_created (user_id, created_at),
            INDEX idx_agent_events_causal_chain_id (causal_chain_id),
            INDEX idx_agent_events_skill_created (skill_name, created_at),
            INDEX idx_agent_events_created_at (created_at),
            INDEX idx_agent_events_tool_name (meta_tool_name),
            INDEX idx_agent_events_trace (session_id, turn_id, created_at),
            INDEX idx_agent_events_run (session_id, run_id, created_at),
            INDEX idx_agent_events_parent_run (session_id, parent_run_id, created_at),
            INDEX idx_agent_events_tool_call (session_id, tool_call_id)
        )",
    )
    .execute(&pool)
    .await?;

    for (column, ddl) in [
        (
            "run_id",
            "ALTER TABLE agent_events ADD COLUMN run_id VARCHAR(64) NULL",
        ),
        (
            "parent_run_id",
            "ALTER TABLE agent_events ADD COLUMN parent_run_id VARCHAR(64) NULL",
        ),
        (
            "turn_id",
            "ALTER TABLE agent_events ADD COLUMN turn_id VARCHAR(64) NULL",
        ),
        (
            "turn_seq",
            "ALTER TABLE agent_events ADD COLUMN turn_seq BIGINT NULL",
        ),
        (
            "round_index",
            "ALTER TABLE agent_events ADD COLUMN round_index BIGINT NULL",
        ),
        (
            "tool_call_id",
            "ALTER TABLE agent_events ADD COLUMN tool_call_id VARCHAR(128) NULL",
        ),
        (
            "parent_agent_id",
            "ALTER TABLE agent_events ADD COLUMN parent_agent_id VARCHAR(128) NULL",
        ),
        (
            "trace_kind",
            "ALTER TABLE agent_events ADD COLUMN trace_kind VARCHAR(64) NULL",
        ),
    ] {
        add_column_if_missing(&pool, &settings.database, "agent_events", column, ddl).await?;
    }

    for (index, ddl) in [
        (
            "idx_agent_events_trace",
            "ALTER TABLE agent_events ADD INDEX idx_agent_events_trace (session_id, turn_id, created_at)",
        ),
        (
            "idx_agent_events_run",
            "ALTER TABLE agent_events ADD INDEX idx_agent_events_run (session_id, run_id, created_at)",
        ),
        (
            "idx_agent_events_parent_run",
            "ALTER TABLE agent_events ADD INDEX idx_agent_events_parent_run (session_id, parent_run_id, created_at)",
        ),
        (
            "idx_agent_events_tool_call",
            "ALTER TABLE agent_events ADD INDEX idx_agent_events_tool_call (session_id, tool_call_id)",
        ),
    ] {
        add_index_if_missing(&pool, &settings.database, "agent_events", index, ddl).await?;
    }

    // ── Durable web-agent run state (Phase 1 / G15 + G19) ────────────────
    query(
        "CREATE TABLE IF NOT EXISTS agent_runs (
            run_id VARCHAR(64) PRIMARY KEY,
            user_id VARCHAR(64) NOT NULL,
            session_id VARCHAR(64) NOT NULL,
            parent_run_id VARCHAR(64) NULL,
            root_run_id VARCHAR(64) NOT NULL,
            ancestor_path VARCHAR(2048) NOT NULL,
            depth INT NOT NULL DEFAULT 0,
            delegation_id VARCHAR(64) NULL,
            agent_id VARCHAR(128) NULL,
            retry_of VARCHAR(64) NULL,
            retry_scope VARCHAR(16) NOT NULL DEFAULT 'node',
            status VARCHAR(32) NOT NULL,
            execution_mode VARCHAR(32) NOT NULL DEFAULT 'web_agent',
            trigger_type VARCHAR(64) NULL,
            trigger_event_id VARCHAR(64) NULL,
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
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            CONSTRAINT chk_agent_runs_retry_scope CHECK (retry_scope IN ('node', 'subtree', 'siblings')),
            INDEX idx_agent_runs_user_updated (user_id, updated_at),
            INDEX idx_agent_runs_session_updated (session_id, updated_at),
            INDEX idx_agent_runs_root_depth (root_run_id, depth, created_at),
            INDEX idx_agent_runs_parent (parent_run_id, created_at),
            INDEX idx_agent_runs_retry_of (retry_of),
            INDEX idx_agent_runs_status_lease (status, owner_lease_expires_at),
            INDEX idx_agent_runs_owner_lease (owner_pod_id, owner_lease_expires_at)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS agent_run_events (
            id VARCHAR(64) PRIMARY KEY,
            run_id VARCHAR(64) NOT NULL,
            event_idx BIGINT NOT NULL,
            user_id VARCHAR(64) NOT NULL,
            session_id VARCHAR(64) NOT NULL,
            event_type VARCHAR(64) NOT NULL,
            event_id VARCHAR(128) NOT NULL,
            agent_id VARCHAR(128) NULL,
            idempotency_key VARCHAR(128) NULL,
            event_hash VARCHAR(64) NOT NULL,
            producer_pod_id VARCHAR(128) NULL,
            payload_json LONGTEXT NOT NULL,
            request_id VARCHAR(64) NULL,
            trace_id VARCHAR(64) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            UNIQUE KEY uq_run_event_idx (run_id, event_idx),
            UNIQUE KEY uq_run_event_idempotency (run_id, idempotency_key),
            INDEX idx_agent_run_events_run_created (run_id, created_at),
            INDEX idx_agent_run_events_session_created (session_id, created_at),
            INDEX idx_agent_run_events_user_created (user_id, created_at),
            INDEX idx_agent_run_events_event_id (event_id)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS run_checkpoints (
            checkpoint_id VARCHAR(64) PRIMARY KEY,
            run_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(64) NOT NULL,
            session_id VARCHAR(64) NOT NULL,
            node_seq BIGINT NOT NULL DEFAULT 0,
            checkpoint_kind VARCHAR(32) NOT NULL,
            checkpoint_version VARCHAR(32) NOT NULL,
            idempotency_key VARCHAR(191) NOT NULL,
            checkpoint_json LONGTEXT NOT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            UNIQUE KEY uniq_run_checkpoint_idem (run_id, checkpoint_kind, idempotency_key),
            INDEX idx_run_checkpoints_run_created (run_id, created_at),
            INDEX idx_run_checkpoints_session_kind_created (session_id, checkpoint_kind, created_at)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS run_display_projections (
            run_id VARCHAR(64) PRIMARY KEY,
            user_id VARCHAR(64) NOT NULL,
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
            INDEX idx_run_display_projections_session_updated (session_id, updated_at),
            INDEX idx_run_display_projections_user_updated (user_id, updated_at)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS session_tool_output_batches (
            batch_id VARCHAR(64) PRIMARY KEY,
            session_id VARCHAR(64) NOT NULL,
            run_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(64) NOT NULL,
            output_count INT NOT NULL,
            payload_bytes BIGINT NOT NULL,
            status VARCHAR(32) NOT NULL DEFAULT 'committed',
            request_id VARCHAR(64) NULL,
            trace_id VARCHAR(64) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_tool_output_batches_run_created (run_id, created_at),
            INDEX idx_tool_output_batches_session_created (session_id, created_at),
            INDEX idx_tool_output_batches_user_created (user_id, created_at)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS session_tool_outputs (
            output_id VARCHAR(64) PRIMARY KEY,
            batch_id VARCHAR(64) NOT NULL,
            session_id VARCHAR(64) NOT NULL,
            run_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(64) NOT NULL,
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
            UNIQUE KEY uq_tool_outputs_batch_idx (batch_id, output_idx),
            INDEX idx_tool_outputs_run_created (run_id, created_at),
            INDEX idx_tool_outputs_session_created (session_id, created_at),
            INDEX idx_tool_outputs_parent (parent_output_id),
            INDEX idx_tool_outputs_artifact_ref (artifact_ref)
        )",
    )
    .execute(&pool)
    .await?;

    for (table, column, ddl) in [
        (
            "session_tool_outputs",
            "parent_output_id",
            "ALTER TABLE session_tool_outputs ADD COLUMN parent_output_id VARCHAR(64) NULL",
        ),
        (
            "session_tool_outputs",
            "preview_text",
            "ALTER TABLE session_tool_outputs ADD COLUMN preview_text LONGTEXT NULL",
        ),
        (
            "session_tool_outputs",
            "preview_status",
            "ALTER TABLE session_tool_outputs ADD COLUMN preview_status VARCHAR(32) NOT NULL DEFAULT 'template'",
        ),
        (
            "session_tool_outputs",
            "artifact_ref",
            "ALTER TABLE session_tool_outputs ADD COLUMN artifact_ref VARCHAR(255) NULL",
        ),
        (
            "session_tool_outputs",
            "content_hash",
            "ALTER TABLE session_tool_outputs ADD COLUMN content_hash VARCHAR(128) NULL",
        ),
        (
            "session_tool_outputs",
            "normalize_version",
            "ALTER TABLE session_tool_outputs ADD COLUMN normalize_version VARCHAR(32) NOT NULL DEFAULT 'raw_v1'",
        ),
    ] {
        if let Err(e) = add_column_if_missing(&pool, &settings.database, table, column, ddl).await {
            tracing::warn!(
                target: "astra_services::storage",
                table,
                column,
                error = %e,
                "failed to migrate session_tool_outputs Phase 6 column"
            );
        }
    }
    for (table, index, ddl) in [
        (
            "session_tool_outputs",
            "idx_tool_outputs_parent",
            "ALTER TABLE session_tool_outputs ADD INDEX idx_tool_outputs_parent (parent_output_id)",
        ),
        (
            "session_tool_outputs",
            "idx_tool_outputs_artifact_ref",
            "ALTER TABLE session_tool_outputs ADD INDEX idx_tool_outputs_artifact_ref (artifact_ref)",
        ),
    ] {
        if let Err(e) = add_index_if_missing(&pool, &settings.database, table, index, ddl).await {
            tracing::warn!(
                target: "astra_services::storage",
                table,
                index,
                error = %e,
                "failed to migrate session_tool_outputs Phase 6 index"
            );
        }
    }

    // ── Web transcript hydration + device lease state (Phase 2 / G13+G19+G25) ──
    query(
        "CREATE TABLE IF NOT EXISTS session_transcript_items (
            session_id VARCHAR(64) NOT NULL,
            item_seq BIGINT NOT NULL,
            user_id VARCHAR(64) NOT NULL,
            run_id VARCHAR(64) NULL,
            role VARCHAR(32) NOT NULL,
            content LONGTEXT NOT NULL,
            source_event_id VARCHAR(128) NULL,
            source_event_idx BIGINT NULL,
            content_hash VARCHAR(128) NOT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (session_id, item_seq),
            INDEX idx_transcript_user_session_seq (user_id, session_id, item_seq),
            INDEX idx_transcript_run_event (run_id, source_event_idx)
        )",
    )
    .execute(&pool)
    .await?;
    widen_varchar_if_shorter(
        &pool,
        &settings.database,
        "session_transcript_items",
        "content_hash",
        128,
        "ALTER TABLE session_transcript_items MODIFY COLUMN content_hash VARCHAR(128) NOT NULL",
    )
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS transcript_pages (
            session_id VARCHAR(64) NOT NULL,
            page_seq BIGINT NOT NULL,
            start_item_seq BIGINT NOT NULL,
            end_item_seq BIGINT NOT NULL,
            item_count INT NOT NULL,
            page_hash VARCHAR(128) NOT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (session_id, page_seq),
            INDEX idx_transcript_pages_session_end (session_id, end_item_seq),
            INDEX idx_transcript_pages_session_updated (session_id, updated_at)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS prompt_request_records (
            request_id VARCHAR(64) PRIMARY KEY,
            session_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(64) NOT NULL,
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
            UNIQUE KEY uq_prompt_request_attempt (session_id, turn, round, source, attempt),
            INDEX idx_prompt_requests_session_created (session_id, created_at),
            INDEX idx_prompt_requests_run_created (run_id, created_at)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS prompt_deltas (
            delta_id VARCHAR(64) PRIMARY KEY,
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
            UNIQUE KEY uq_prompt_delta_request_seq (request_id, delta_seq),
            INDEX idx_prompt_deltas_request_position (request_id, position, delta_seq)
        )",
    )
    .execute(&pool)
    .await?;
    drop_column_if_exists(&pool, &settings.database, "prompt_deltas", "payload_json").await?;

    query(
        "CREATE TABLE IF NOT EXISTS session_state_revisions (
            session_id VARCHAR(64) PRIMARY KEY,
            user_id VARCHAR(64) NOT NULL,
            monotonic_id BIGINT NOT NULL DEFAULT 0,
            revision_hash VARCHAR(96) NOT NULL,
            device_fingerprint VARCHAR(128) NOT NULL,
            transcript_high_watermark BIGINT NOT NULL DEFAULT 0,
            run_event_high_watermark BIGINT NOT NULL DEFAULT 0,
            state_projection_hash VARCHAR(96) NOT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_state_revisions_user_updated (user_id, updated_at)
        )",
    )
    .execute(&pool)
    .await?;
    widen_varchar_if_shorter(
        &pool,
        &settings.database,
        "session_state_revisions",
        "revision_hash",
        96,
        "ALTER TABLE session_state_revisions MODIFY COLUMN revision_hash VARCHAR(96) NOT NULL",
    )
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS session_device_leases (
            lease_id VARCHAR(128) PRIMARY KEY,
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
            UNIQUE KEY uq_session_device (session_id, device_id),
            INDEX idx_device_leases_user_session (user_id, session_id, status, updated_at),
            INDEX idx_device_leases_fingerprint (user_id, device_fingerprint, status),
            INDEX idx_device_leases_expiry (status, expires_at)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS session_device_lease_events (
            lease_event_id VARCHAR(128) PRIMARY KEY,
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
            INDEX idx_lease_events_user_created (user_id, created_at),
            INDEX idx_lease_events_session_device (session_id, device_id, created_at),
            INDEX idx_lease_events_type_created (event_type, created_at)
        )",
    )
    .execute(&pool)
    .await?;

    // ── Sweeper leader election (prevents duplicate background work in
    // multi-pod deployments). One row per sweeper type; pods CAS-update
    // the lease every TTL/2 seconds. Only the lease holder runs work.
    query("DROP TABLE IF EXISTS sweeper_leases")
        .execute(&pool)
        .await?;
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
            INDEX idx_ctx_manifest_session_turn (session_id, turn_id),
            INDEX idx_ctx_manifest_run (run_id),
            INDEX idx_ctx_manifest_user_created (user_id, created_at)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS context_manifest_items (
            id BIGINT AUTO_INCREMENT PRIMARY KEY,
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
            UNIQUE KEY uq_manifest_item_order (manifest_id, item_order),
            INDEX idx_manifest_items_source (source_table, source_id),
            INDEX idx_manifest_items_session_zone (session_id, zone, included),
            INDEX idx_manifest_items_raw_ref (raw_ref)
        )",
    )
    .execute(&pool)
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
            UNIQUE KEY uq_state_current (session_id, scope, category, item_key),
            INDEX idx_state_session_category (session_id, category, status, priority),
            INDEX idx_state_user_category (user_id, category, status, updated_at),
            INDEX idx_state_user_scope_category (user_id, scope, category, status, priority),
            INDEX idx_state_origin_session (origin_session_id, category, status),
            INDEX idx_state_expires (expires_at),
            INDEX idx_state_provenance (provenance_event_id)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS session_state_item_events (
            id BIGINT AUTO_INCREMENT PRIMARY KEY,
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
            INDEX idx_state_events_item_created (item_id, created_at, id),
            INDEX idx_state_events_session_created (session_id, created_at, id),
            INDEX idx_state_events_category_created (category, created_at)
        )",
    )
    .execute(&pool)
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
            agent_id VARCHAR(128) NULL,
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
            INDEX idx_delegations_root_depth (root_run_id, depth, created_at),
            INDEX idx_delegations_parent (parent_run_id, created_at),
            INDEX idx_delegations_session_status (session_id, status, updated_at),
            INDEX idx_delegations_retry_of (retry_of)
        )",
    )
    .execute(&pool)
    .await?;

    // PR #330 plan-driven todo tracking. NAMING: distinct from
    // `session_todos` (Tier 1 task scratchpad created further below)
    // because the two schemas are incompatible — earlier revisions
    // shared the name, which caused the second `CREATE TABLE IF NOT
    // EXISTS` to silently no-op and code that expected the other
    // shape to fail with `column does not exist`. The two tables now
    // co-exist with disjoint columns and disjoint consumers:
    //   - `session_plan_todos`: `state_projection` + plan/backlog pool
    //   - `session_todos`     : `astra_tools::task_mgmt` (TaskManager)
    query(
        "CREATE TABLE IF NOT EXISTS session_plan_todos (
            todo_id VARCHAR(128) PRIMARY KEY,
            user_id VARCHAR(128) NOT NULL,
            session_id VARCHAR(128) NOT NULL,
            plan_id VARCHAR(128) NULL,
            parent_todo_id VARCHAR(128) NULL,
            backlog_pool_id VARCHAR(128) NULL,
            title VARCHAR(255) NOT NULL,
            status VARCHAR(32) NOT NULL DEFAULT 'active',
            priority INT NOT NULL DEFAULT 0,
            depth INT NOT NULL DEFAULT 0,
            token_estimate INT NOT NULL DEFAULT 0,
            payload_json LONGTEXT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_session_plan_todos_active (session_id, status, priority, updated_at),
            INDEX idx_session_plan_todos_pool (user_id, backlog_pool_id, status, updated_at),
            INDEX idx_session_plan_todos_tree (session_id, parent_todo_id, priority)
        )",
    )
    .execute(&pool)
    .await?;

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
            INDEX idx_history_session_seq (session_id, seq_start, seq_end),
            INDEX idx_history_source_session (source_session_id, chunk_type, created_at)
        )",
    )
    .execute(&pool)
    .await?;

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
            UNIQUE KEY uq_artifacts_grant_target (artifact_id, grant_scope, target_run_id, target_delegation_id),
            INDEX idx_artifacts_grants_root (root_run_id, grant_scope, created_at),
            INDEX idx_artifacts_grants_target (target_run_id, artifact_id, expires_at),
            INDEX idx_artifacts_grants_delegation_target (target_delegation_id, artifact_id, expires_at)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS agent_event_edges (
            child_event_id VARCHAR(64) NOT NULL,
            parent_event_id VARCHAR(64) NOT NULL,
            relation_kind VARCHAR(32) NOT NULL DEFAULT 'causal',
            parent_order INT NOT NULL DEFAULT 0,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (child_event_id, parent_event_id, relation_kind),
            INDEX idx_agent_event_edges_child (child_event_id, parent_order),
            INDEX idx_agent_event_edges_parent (parent_event_id, parent_order)
        )",
    )
    .execute(&pool)
    .await?;

    // Harness diagnostic snapshots — separated from agent_events to avoid
    // polluting session event counts and to carry causal_chain_id natively.
    query(
        "CREATE TABLE IF NOT EXISTS harness_snapshots (
            snapshot_id VARCHAR(64) PRIMARY KEY,
            session_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(64) NOT NULL,
            hook_point VARCHAR(32) NOT NULL,
            turn_number INT UNSIGNED NOT NULL DEFAULT 0,
            snapshot_json LONGTEXT NOT NULL,
            causal_chain_id VARCHAR(128),
            created_at DATETIME(6) DEFAULT NOW(6),
            INDEX idx_harness_session (session_id),
            INDEX idx_harness_session_turn (session_id, turn_number),
            INDEX idx_harness_chain (causal_chain_id)
        )",
    )
    .execute(&pool)
    .await?;

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
            INDEX idx_harness_runs_session (session_id, updated_at)
        )",
    )
    .execute(&pool)
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

    for (table, column, ddl) in [
        (
            "harness_items",
            "decision_history_json",
            "ALTER TABLE harness_items ADD COLUMN decision_history_json LONGTEXT NULL",
        ),
        (
            "harness_skill_drafts",
            "decision_history_json",
            "ALTER TABLE harness_skill_drafts ADD COLUMN decision_history_json LONGTEXT NULL",
        ),
        (
            "harness_skill_rules",
            "decision_history_json",
            "ALTER TABLE harness_skill_rules ADD COLUMN decision_history_json LONGTEXT NULL",
        ),
        (
            "harness_citations",
            "skill_draft_id",
            "ALTER TABLE harness_citations ADD COLUMN skill_draft_id VARCHAR(128) NULL",
        ),
        (
            "harness_citations",
            "skill_rule_id",
            "ALTER TABLE harness_citations ADD COLUMN skill_rule_id VARCHAR(128) NULL",
        ),
        (
            "harness_citations",
            "source_snapshot_ref",
            "ALTER TABLE harness_citations ADD COLUMN source_snapshot_ref VARCHAR(128) NULL",
        ),
        (
            "harness_citations",
            "source_content_hash",
            "ALTER TABLE harness_citations ADD COLUMN source_content_hash VARCHAR(128) NULL",
        ),
        (
            "harness_citations",
            "source_metadata_json",
            "ALTER TABLE harness_citations ADD COLUMN source_metadata_json LONGTEXT NULL",
        ),
    ] {
        add_column_if_missing(&pool, &settings.database, table, column, ddl).await?;
    }
    for (table, index, ddl) in [(
        "harness_citations",
        "idx_harness_citations_skill_rule",
        "ALTER TABLE harness_citations ADD INDEX idx_harness_citations_skill_rule (skill_rule_id, created_at)",
    )] {
        add_index_if_missing(&pool, &settings.database, table, index, ddl).await?;
    }

    // Context / decisions / evaluation essentials used by turn persistence
    query(
        "CREATE TABLE IF NOT EXISTS ctx_snapshots (
            context_capture_id VARCHAR(64) PRIMARY KEY,
            session_id VARCHAR(64) NOT NULL,
            event_id VARCHAR(64) NOT NULL,
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
            INDEX idx_ctx_snapshots_session_created (session_id, created_at),
            INDEX idx_ctx_snapshots_event_id (event_id)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS ctx_decision_audits (
            decision_id VARCHAR(64) PRIMARY KEY,
            session_id VARCHAR(64) NOT NULL,
            event_id VARCHAR(64) NULL,
            context_capture_id VARCHAR(64) NULL,
            decision_type VARCHAR(64) NOT NULL,
            decision_output JSON NULL,
            model_params JSON NULL,
            model_used VARCHAR(128) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_ctx_decisions_session_type_created (session_id, decision_type, created_at),
            INDEX idx_ctx_decisions_event_id (event_id),
            INDEX idx_ctx_decisions_context_capture_id (context_capture_id)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS skill_selection_events (
            event_id VARCHAR(64) PRIMARY KEY,
            session_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(64) NULL,
            agent_id VARCHAR(64) NULL,
            user_query LONGTEXT NULL,
            selected_skills JSON NULL,
            skill_name VARCHAR(255) NULL,
            skill_version VARCHAR(64) NULL,
            selection_method VARCHAR(64) NULL,
            execution_success BIGINT NULL,
            execution_time_ms BIGINT NULL,
            user_feedback_score BIGINT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_skill_selection_session_created (session_id, created_at),
            INDEX idx_skill_selection_user_created (user_id, created_at),
            INDEX idx_skill_selection_skill_created (skill_name, created_at)
        )",
    )
    .execute(&pool)
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
            context_window INT NOT NULL DEFAULT 128000,
            max_completion_tokens INT NULL,
            input_modalities JSON NULL,
            output_modalities JSON NULL,
            supported_parameters JSON NULL,
            pricing JSON NULL,
            architecture VARCHAR(100) NULL,
            tags JSON NULL,
            quirks JSON NULL,
            thinking_capability VARCHAR(20) NULL,
            thinking_probe_error TEXT NULL,
            created_by VARCHAR(128) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_infra_llm_models_active_provider_name (is_active, provider, model_name)
        )",
    )
    .execute(&pool)
    .await?;

    // Migration: add thinking_capability columns for existing deployments.
    // MatrixOne does not support MySQL's `ADD COLUMN IF NOT EXISTS`, so use
    // INFORMATION_SCHEMA first and only issue plain ALTER when the column is
    // absent. This keeps startup logs clean and avoids swallowing syntax errors.
    for (column, ddl) in [
        (
            "thinking_capability",
            "ALTER TABLE infra_llm_models ADD COLUMN thinking_capability VARCHAR(20) NULL",
        ),
        (
            "thinking_probe_error",
            "ALTER TABLE infra_llm_models ADD COLUMN thinking_probe_error TEXT NULL",
        ),
    ] {
        add_column_if_missing(&pool, &settings.database, "infra_llm_models", column, ddl).await?;
    }

    // Migration: promote legacy quirks.fallback_model into quirks.fallback_chain.
    // Keep this in Rust instead of SQL JSON functions because MatrixOne does not
    // support the full MySQL JSON mutation surface.
    let migrated_model_rows = migrate_model_fallback_chain(&pool).await?;
    if migrated_model_rows > 0 {
        tracing::info!(
            rows = migrated_model_rows,
            "migrated legacy model fallback_chain config"
        );
    }

    // Server-wide admin config KV store. Holds settings that the admin explicitly manages
    // via `astra-admin config set/get/unset` (first key: `reasoning_model_name`).
    query(
        "CREATE TABLE IF NOT EXISTS admin_config (
            config_key VARCHAR(100) PRIMARY KEY,
            config_value TEXT NOT NULL,
            updated_by VARCHAR(64) NULL,
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS user_preferences (
            pref_id VARCHAR(64) PRIMARY KEY,
            user_id VARCHAR(64) NOT NULL,
            pref_key VARCHAR(100) NOT NULL,
            pref_value LONGTEXT NOT NULL,
            version INT NOT NULL DEFAULT 1,
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            UNIQUE KEY idx_prefs_user_key (user_id, pref_key)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS session_sync_log (
            sync_id VARCHAR(64) PRIMARY KEY,
            user_id VARCHAR(64) NOT NULL,
            session_id VARCHAR(64) NOT NULL,
            sync_type VARCHAR(50) NOT NULL,
            sync_direction VARCHAR(10) NOT NULL DEFAULT 'push',
            payload_size INT NOT NULL DEFAULT 0,
            status VARCHAR(20) NOT NULL DEFAULT 'pending',
            error_message TEXT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_sync_user_session_created (user_id, session_id, created_at),
            INDEX idx_sync_user_status_created (user_id, status, created_at)
        )",
    )
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
            code_hash VARCHAR(128) NULL,
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
    widen_varchar_if_shorter(
        &pool,
        &settings.database,
        "skills_registry",
        "created_by",
        128,
        "ALTER TABLE skills_registry MODIFY COLUMN created_by VARCHAR(128) NULL",
    )
    .await?;
    // Skill triggers were removed; drop the dead column from existing dev/CI databases.
    drop_column_if_exists(&pool, &settings.database, "skills_registry", "triggers").await?;

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
    add_column_if_missing(
        &pool,
        &settings.database,
        "skill_metrics",
        "metric_slot",
        "ALTER TABLE skill_metrics ADD COLUMN metric_slot VARCHAR(255) NOT NULL DEFAULT 'legacy'",
    )
    .await?;
    query(
        "UPDATE skill_metrics
         SET metric_slot = CASE
             WHEN metric_type = 'aggregate' THEN 'aggregate'
             ELSE metric_id
         END
         WHERE metric_slot = 'legacy' OR metric_slot IS NULL OR metric_slot = ''",
    )
    .execute(&pool)
    .await?;
    add_index_if_missing(
        &pool,
        &settings.database,
        "skill_metrics",
        "uq_skill_metrics_slot",
        "ALTER TABLE skill_metrics ADD UNIQUE INDEX uq_skill_metrics_slot (skill_name, metric_type, metric_slot)",
    )
    .await?;

    // ── Long-task orchestration (Phase H) ──

    query(
        "CREATE TABLE IF NOT EXISTS agent_tasks (
            task_id VARCHAR(64) PRIMARY KEY,
            user_id VARCHAR(64) NOT NULL,
            session_id VARCHAR(64) NULL,
            agent_id VARCHAR(128) NULL,
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
            INDEX idx_tasks_user_status_updated (user_id, status, updated_at),
            INDEX idx_tasks_user_updated (user_id, updated_at),
            INDEX idx_tasks_session_updated (session_id, updated_at),
            INDEX idx_tasks_parent_updated (parent_task_id, updated_at)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS edge_agent_registry (
            registry_id VARCHAR(64) PRIMARY KEY,
            user_id VARCHAR(64) NOT NULL,
            edge_agent_id VARCHAR(128) NOT NULL,
            edge_id VARCHAR(128) NOT NULL,
            hostname VARCHAR(255) NULL,
            worktree_path VARCHAR(512) NULL,
            capabilities_json JSON NULL,
            registered_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            last_heartbeat_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            UNIQUE KEY uq_edge_registry_user_agent (user_id, edge_agent_id),
            INDEX idx_edge_registry_user_heartbeat (user_id, last_heartbeat_at)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS edge_pending_dispatch (
            dispatch_id BIGINT AUTO_INCREMENT PRIMARY KEY,
            user_id VARCHAR(64) NOT NULL,
            edge_agent_id VARCHAR(128) NOT NULL,
            request_id VARCHAR(128) NOT NULL,
            payload_json JSON NOT NULL,
            result_json JSON NULL,
            status VARCHAR(16) NOT NULL DEFAULT 'pending',
            pod_id VARCHAR(128) NULL,
            dispatched_at DATETIME(6) NULL,
            completed_at DATETIME(6) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            UNIQUE KEY uq_edge_dispatch_request_id (request_id),
            INDEX idx_edge_dispatch_user_status (user_id, edge_agent_id, status),
            INDEX idx_edge_dispatch_created (created_at)
        )",
    )
    .execute(&pool)
    .await?;

    // Migration: enforce request_id uniqueness on edge_pending_dispatch
    add_index_if_missing(
        &pool,
        &settings.database,
        "edge_pending_dispatch",
        "uq_edge_dispatch_request_id",
        "ALTER TABLE edge_pending_dispatch ADD UNIQUE KEY uq_edge_dispatch_request_id (request_id)",
    )
    .await?;

    // Migration: add columns that may be missing on tables created by
    // earlier iterations of this branch's DDL (before schema was finalised).
    add_column_if_missing(
        &pool,
        &settings.database,
        "edge_pending_dispatch",
        "result_json",
        "ALTER TABLE edge_pending_dispatch ADD COLUMN result_json JSON NULL",
    )
    .await?;
    add_column_if_missing(
        &pool,
        &settings.database,
        "edge_pending_dispatch",
        "pod_id",
        "ALTER TABLE edge_pending_dispatch ADD COLUMN pod_id VARCHAR(128) NULL",
    )
    .await?;
    add_column_if_missing(
        &pool,
        &settings.database,
        "edge_pending_dispatch",
        "dispatched_at",
        "ALTER TABLE edge_pending_dispatch ADD COLUMN dispatched_at DATETIME(6) NULL",
    )
    .await?;
    add_column_if_missing(
        &pool,
        &settings.database,
        "edge_pending_dispatch",
        "completed_at",
        "ALTER TABLE edge_pending_dispatch ADD COLUMN completed_at DATETIME(6) NULL",
    )
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS mcp_servers (
            id BIGINT AUTO_INCREMENT PRIMARY KEY,
            owner_user_id VARCHAR(128) NOT NULL,
            name VARCHAR(128) NOT NULL,
            description TEXT NULL,
            transport VARCHAR(32) NOT NULL,
            url TEXT NOT NULL,
            is_active SMALLINT NOT NULL DEFAULT 1,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            UNIQUE KEY uq_mcp_servers_owner_name (owner_user_id, name),
            INDEX idx_mcp_servers_owner_active (owner_user_id, is_active, updated_at)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS mcp_bindings (
            id BIGINT AUTO_INCREMENT PRIMARY KEY,
            owner_user_id VARCHAR(128) NOT NULL,
            mcp_id BIGINT NOT NULL,
            key_hash VARCHAR(128) NOT NULL,
            key_value_encrypted TEXT NOT NULL,
            comment TEXT NULL,
            is_active SMALLINT NOT NULL DEFAULT 1,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            UNIQUE KEY uq_mcp_bindings_owner_mcp_key (owner_user_id, mcp_id, key_hash),
            INDEX idx_mcp_bindings_owner_active (owner_user_id, is_active, updated_at),
            INDEX idx_mcp_bindings_mcp_id (mcp_id)
        )",
    )
    .execute(&pool)
    .await?;
    add_column_if_missing(
        &pool,
        &settings.database,
        "mcp_bindings",
        "key_hash",
        "ALTER TABLE mcp_bindings ADD COLUMN key_hash VARCHAR(128) NULL",
    )
    .await?;
    drop_index_if_exists(
        &pool,
        &settings.database,
        "mcp_bindings",
        "uq_mcp_bindings_owner_alias",
    )
    .await?;
    drop_column_if_exists(&pool, &settings.database, "mcp_bindings", "alias").await?;
    add_index_if_missing(
        &pool,
        &settings.database,
        "mcp_bindings",
        "uq_mcp_bindings_owner_mcp_key",
        "ALTER TABLE mcp_bindings ADD UNIQUE KEY uq_mcp_bindings_owner_mcp_key (owner_user_id, mcp_id, key_hash)",
    )
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS mcp_tools (
            id BIGINT AUTO_INCREMENT PRIMARY KEY,
            binding_id BIGINT NOT NULL,
            tool_name VARCHAR(256) NOT NULL,
            public_name VARCHAR(384) NOT NULL,
            description TEXT NULL,
            input_schema_json JSON NULL,
            output_schema_json JSON NULL,
            schema_hash VARCHAR(128) NOT NULL,
            discovered_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            UNIQUE KEY uq_mcp_tools_binding_tool (binding_id, tool_name),
            UNIQUE KEY uq_mcp_tools_binding_public (binding_id, public_name),
            INDEX idx_mcp_tools_binding (binding_id)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS task_leases (
            task_id VARCHAR(64) PRIMARY KEY,
            user_id VARCHAR(64) NOT NULL,
            holder_agent_id VARCHAR(128) NOT NULL,
            holder_edge_id VARCHAR(128) NULL,
            expires_at DATETIME(6) NOT NULL,
            lease_version BIGINT NOT NULL DEFAULT 1,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_task_leases_user_expires (user_id, expires_at)
        )",
    )
    .execute(&pool)
    .await?;

    // ── Plan templates table (learning successful patterns) ──
    query(
        "CREATE TABLE IF NOT EXISTS plan_templates (
            template_id VARCHAR(64) PRIMARY KEY,
            user_id VARCHAR(64) NULL,
            goal_pattern VARCHAR(500) NOT NULL,
            project_type VARCHAR(50) NULL,
            template_json LONGTEXT NOT NULL,
            success_rate FLOAT NOT NULL DEFAULT 0.0,
            avg_completion_time INT NULL,
            use_count INT NOT NULL DEFAULT 0,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_tpl_user_goal_project (user_id, goal_pattern, project_type),
            INDEX idx_tpl_project_success (project_type, success_rate)
        )",
    )
    .execute(&pool)
    .await?;

    // ── Plans: cloud-authoritative plan state (user-owned, session-linked) ──
    // `subtask_count` is denormalized so list endpoints don't need to parse
    // `plan_json` just to render a card. Maintained by `PlanRepository::save`.
    query(
        "CREATE TABLE IF NOT EXISTS plans (
            plan_id       VARCHAR(64) PRIMARY KEY,
            user_id       VARCHAR(64) NOT NULL,
            session_id    VARCHAR(64) NULL,
            goal          TEXT NOT NULL,
            phase         VARCHAR(32) NOT NULL,
            version       BIGINT NOT NULL DEFAULT 0,
            plan_json     LONGTEXT NOT NULL,
            plan_md       LONGTEXT NULL,
            progress_pct  INT NOT NULL DEFAULT 0,
            subtask_count INT NOT NULL DEFAULT 0,
            created_by    VARCHAR(64) NULL,
            created_at    DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at    DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_plans_user_updated (user_id, updated_at DESC),
            INDEX idx_plans_session (session_id),
            INDEX idx_plans_user_phase (user_id, phase)
        )",
    )
    .execute(&pool)
    .await?;

    // ── Plan step runs: append-only attempt chain for every subtask ──
    query(
        "CREATE TABLE IF NOT EXISTS plan_step_runs (
            run_id       VARCHAR(64) PRIMARY KEY,
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
            INDEX idx_step_runs_plan_started (plan_id, started_at DESC),
            UNIQUE KEY uq_step_runs_subtask_attempt (plan_id, subtask_id, attempt),
            INDEX idx_step_runs_session (session_id, started_at DESC)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS session_checkpoints (
            checkpoint_id VARCHAR(64) PRIMARY KEY,
            session_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(64) NOT NULL,
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
            UNIQUE KEY idx_ckpt_session_number (session_id, number),
            INDEX idx_ckpt_session_turn (session_id, turn),
            INDEX idx_ckpt_user_created (user_id, created_at)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS session_artifacts (
            artifact_id VARCHAR(64) PRIMARY KEY,
            session_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(64) NOT NULL,
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
            CONSTRAINT chk_session_artifacts_status CHECK (status IN ('active', 'expiring', 'expired')),
            INDEX idx_session_artifacts_session_kind_created (session_id, artifact_kind, created_at),
            INDEX idx_session_artifacts_session_source_created (session_id, source, created_at),
            INDEX idx_session_artifacts_user_created (user_id, created_at),
            INDEX idx_artifacts_root_scope (root_run_id, access_scope, status, updated_at),
            INDEX idx_artifacts_owner_run (owner_run_id, status, updated_at),
            INDEX idx_artifacts_retention (status, retention_until, retention_policy),
            INDEX idx_artifacts_project (project_id, status, retention_until),
            INDEX idx_artifacts_derived (derived_from_artifact_id)
        )",
    )
    .execute(&pool)
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
            tracing::warn!("phase4 additive column migration skipped: {table}.{column}: {e}");
        }
    }

    for (table, index, ddl) in [(
        "agent_sessions",
        "idx_sessions_project",
        "ALTER TABLE agent_sessions ADD INDEX idx_sessions_project (user_id, project_id, updated_at)",
    )] {
        if let Err(e) = add_index_if_missing(&pool, &settings.database, table, index, ddl).await {
            tracing::debug!("phase4 additive index migration skipped: {table}.{index}: {e}");
        }
    }

    // Session task scratchpad (Tier 1 — ClaudeCode-style task board).
    // Authoritative store for the live task board. Both edge and cloud read
    // the same rows for a given session_id; per-host `TaskManager` instances
    // are caches over this table. See `docs/plans/task-system-design.md` §2.1
    // for rationale; CLAUDE.md §3 compliance: primary key matches the only
    // access pattern (filter by session_id); single secondary index covers
    // the "open todos ordered by recency" query.
    query(
        "CREATE TABLE IF NOT EXISTS session_todos (
            session_id VARCHAR(64) NOT NULL,
            todo_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(64) NOT NULL,
            ordinal INT NOT NULL,
            title VARCHAR(512) NOT NULL,
            description TEXT NULL,
            active_form VARCHAR(512) NULL,
            status VARCHAR(16) NOT NULL,
            owner VARCHAR(128) NULL,
            metadata JSON NULL,
            blocks JSON NULL,
            blocked_by JSON NULL,
            subtasks JSON NULL,
            archived_at DATETIME(6) NULL,
            created_at DATETIME(6) NOT NULL,
            updated_at DATETIME(6) NOT NULL,
            PRIMARY KEY (session_id, todo_id),
            INDEX idx_session_todos_session_status_updated (session_id, status, updated_at),
            INDEX idx_session_todos_user_status_updated (user_id, status, updated_at)
        )",
    )
    .execute(&pool)
    .await?;

    // Per-session monotonic counter used to mint `task-<n>` ids. Kept in a
    // separate table (not on `session_todos`) because a todo can be deleted
    // but its id must never be reused for the session.
    query(
        "CREATE TABLE IF NOT EXISTS session_todo_counters (
            session_id VARCHAR(64) PRIMARY KEY,
            next_id BIGINT NOT NULL
        )",
    )
    .execute(&pool)
    .await?;

    // ── Durable Task System ─────────────────────────────────────────────────

    // Task contracts: verifiable acceptance criteria for long-term tasks
    query(
        "CREATE TABLE IF NOT EXISTS task_contracts (
            contract_id    VARCHAR(36) PRIMARY KEY,
            task_id        VARCHAR(36) NOT NULL,
            session_id     VARCHAR(36) NOT NULL,
            user_id        VARCHAR(36) NOT NULL,
            goal           TEXT NOT NULL,
            scope_json     JSON,
            subtasks_json  JSON NOT NULL,
            criteria_json  JSON NOT NULL,
            version        INT NOT NULL DEFAULT 1,
            status         VARCHAR(20) NOT NULL DEFAULT 'draft',
            created_at     DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at     DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_tc_task (task_id),
            INDEX idx_tc_user_status (user_id, status)
        )",
    )
    .execute(&pool)
    .await?;

    // Verification results: audit trail of pass/fail evidence per criterion
    query(
        "CREATE TABLE IF NOT EXISTS task_verification_results (
            result_id      VARCHAR(36) PRIMARY KEY,
            contract_id    VARCHAR(36) NOT NULL,
            task_id        VARCHAR(36) NOT NULL,
            subtask_id     VARCHAR(64) NOT NULL,
            criterion_id   VARCHAR(64) NOT NULL,
            session_id     VARCHAR(36) NOT NULL,
            passed         SMALLINT NOT NULL,
            evidence       LONGTEXT,
            expected       TEXT,
            duration_ms    INT,
            error_message  TEXT,
            attempt        INT NOT NULL DEFAULT 1,
            created_at     DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_tvr_task_subtask (task_id, subtask_id),
            INDEX idx_tvr_contract (contract_id, created_at)
        )",
    )
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

    query(
        "CREATE TABLE IF NOT EXISTS user_skill_evaluations (
            evaluation_id VARCHAR(128) PRIMARY KEY,
            source_id VARCHAR(128) NOT NULL,
            version_id VARCHAR(128) NOT NULL,
            run_id VARCHAR(128) NULL,
            hits BIGINT NOT NULL DEFAULT 0,
            suspects BIGINT NOT NULL DEFAULT 0,
            false_positives BIGINT NOT NULL DEFAULT 0,
            payload_json LONGTEXT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_user_skill_eval_source_created (source_id, created_at),
            INDEX idx_user_skill_eval_version_created (version_id, created_at),
            INDEX idx_user_skill_eval_run (run_id)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS skill_installations (
            installation_id  VARCHAR(36) PRIMARY KEY,
            user_id          VARCHAR(36) NOT NULL,
            skill_name       VARCHAR(128) NOT NULL,
            skill_version    VARCHAR(32) NOT NULL,
            status           VARCHAR(32) NOT NULL DEFAULT 'active',
            previous_version VARCHAR(32),
            installed_at     DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at       DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6),
            UNIQUE INDEX idx_si_user_skill (user_id, skill_name),
            INDEX idx_si_status (status)
        )",
    )
    .execute(&pool)
    .await?;

    for (table, column, ddl) in [
        (
            "skill_installations",
            "scope",
            "ALTER TABLE skill_installations ADD COLUMN scope VARCHAR(32) NOT NULL DEFAULT 'user'",
        ),
        (
            "skill_installations",
            "session_id",
            "ALTER TABLE skill_installations ADD COLUMN session_id VARCHAR(128) NULL",
        ),
        (
            "skill_installations",
            "workspace_id",
            "ALTER TABLE skill_installations ADD COLUMN workspace_id VARCHAR(128) NULL",
        ),
        (
            "skill_installations",
            "auto_activate_on_topic_match",
            "ALTER TABLE skill_installations ADD COLUMN auto_activate_on_topic_match SMALLINT NOT NULL DEFAULT 0",
        ),
    ] {
        if let Err(e) = add_column_if_missing(&pool, &settings.database, table, column, ddl).await {
            tracing::warn!(
                target: "astra_services::storage",
                table,
                column,
                error = %e,
                "failed to migrate skill_installations column"
            );
        }
    }
    for (table, index, ddl) in [
        (
            "skill_installations",
            "idx_si_scope_target",
            "ALTER TABLE skill_installations ADD INDEX idx_si_scope_target (user_id, scope, session_id, workspace_id, skill_name)",
        ),
        (
            "skill_installations",
            "idx_si_auto_activate",
            "ALTER TABLE skill_installations ADD INDEX idx_si_auto_activate (user_id, auto_activate_on_topic_match, status)",
        ),
    ] {
        if let Err(e) = add_index_if_missing(&pool, &settings.database, table, index, ddl).await {
            tracing::warn!(
                target: "astra_services::storage",
                table,
                index,
                error = %e,
                "failed to migrate skill_installations index"
            );
        }
    }

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
            updated_by    VARCHAR(36),
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
            created_by    VARCHAR(36),
            updated_by    VARCHAR(36),
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
            user_id       VARCHAR(36) NOT NULL,
            skill_name    VARCHAR(128) NOT NULL,
            resource_type VARCHAR(64) NOT NULL,
            resource_key  VARCHAR(128) NOT NULL,
            binding_name  VARCHAR(128) NOT NULL,
            binding_value TEXT,
            is_secret     SMALLINT NOT NULL DEFAULT 0,
            updated_by    VARCHAR(36),
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
            user_id         VARCHAR(36) NOT NULL,
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
            user_id      VARCHAR(36) NOT NULL,
            agent_id     VARCHAR(36),
            trigger_type VARCHAR(32) NOT NULL,
            name         VARCHAR(128) NOT NULL,
            user_input   TEXT,
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
            agent_id       VARCHAR(36) PRIMARY KEY,
            agent_name     VARCHAR(128) NOT NULL,
            agent_type     VARCHAR(64) NOT NULL DEFAULT 'general',
            owner_user_id  VARCHAR(36) NOT NULL,
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
            user_id      VARCHAR(36) NOT NULL,
            description  TEXT,
            created_by   VARCHAR(36),
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
            user_id         VARCHAR(36) NOT NULL,
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
            user_id         VARCHAR(36) NULL,
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
            user_id       VARCHAR(36) NULL,
            target_id     VARCHAR(64) NOT NULL,
            score         DECIMAL(5,4) NOT NULL,
            step_count    INT NOT NULL DEFAULT 0,
            level         VARCHAR(32) NOT NULL DEFAULT 'unknown',
            created_at    DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at    DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6),
            INDEX idx_eqa_user_level_updated (user_id, level, updated_at),
            INDEX idx_eqa_target (target_id),
            INDEX idx_eqa_level (level)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS eval_training_datasets (
            dataset_id        VARCHAR(36) PRIMARY KEY,
            user_id           VARCHAR(36) NOT NULL,
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
            user_id       VARCHAR(36) NOT NULL,
            agent_id      VARCHAR(64),
            session_id    VARCHAR(36),
            turn_id       VARCHAR(36),
            feedback_type VARCHAR(64) NOT NULL,
            rating        INT,
            comment       TEXT,
            created_at    DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_euf_user (user_id),
            INDEX idx_euf_agent_created (agent_id, created_at),
            INDEX idx_euf_created (created_at),
            INDEX idx_euf_session (session_id),
            INDEX idx_euf_type_created (feedback_type, created_at)
        )",
    )
    .execute(&pool)
    .await?;

    // ─── Team definitions ───────────────────────────────────────────────────────

    query(
        "CREATE TABLE IF NOT EXISTS team_definitions (
            team_id       VARCHAR(64)  PRIMARY KEY,
            user_id       VARCHAR(64)  NOT NULL,
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
            user_id       VARCHAR(64)  NOT NULL,
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
            user_id              VARCHAR(64)  NOT NULL,
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
            session_id    VARCHAR(64) NOT NULL,
            seq           BIGINT NOT NULL,
            turn          INT NOT NULL,
            entry_type    TINYINT NOT NULL,
            trace_id      VARCHAR(64) DEFAULT NULL,
            message_count INT DEFAULT NULL,
            payload       MEDIUMTEXT NOT NULL,
            created_at    DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            PRIMARY KEY (session_id, seq),
            INDEX idx_csl_snapshot (session_id, entry_type, seq DESC),
            INDEX idx_csl_turn (session_id, turn)
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

    // ─── Schema migration tracking ──────────────────────────────────────────────

    query(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version     INT PRIMARY KEY,
            description VARCHAR(255) NOT NULL,
            applied_at  DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6)
        )",
    )
    .execute(&pool)
    .await?;

    run_migrations(&pool).await?;

    Ok(())
}

async fn run_migration(
    pool: &sqlx::Pool<MySql>,
    version: i32,
    description: &str,
    sql: &str,
) -> Result<(), sqlx::Error> {
    let already_applied: bool = query("SELECT 1 FROM schema_migrations WHERE version = ?")
        .bind(version)
        .fetch_optional(pool)
        .await?
        .is_some();

    if already_applied {
        return Ok(());
    }

    execute_idempotent_migration_sql(pool, sql).await?;

    query("INSERT IGNORE INTO schema_migrations (version, description) VALUES (?, ?)")
        .bind(version)
        .bind(description)
        .execute(pool)
        .await?;

    Ok(())
}

async fn run_migration_batch(
    pool: &sqlx::Pool<MySql>,
    version: i32,
    description: &str,
    statements: &[&str],
) -> Result<(), sqlx::Error> {
    let already_applied: bool = query("SELECT 1 FROM schema_migrations WHERE version = ?")
        .bind(version)
        .fetch_optional(pool)
        .await?
        .is_some();

    if already_applied {
        return Ok(());
    }

    for statement in statements {
        execute_idempotent_migration_sql(pool, statement).await?;
    }

    query("INSERT IGNORE INTO schema_migrations (version, description) VALUES (?, ?)")
        .bind(version)
        .bind(description)
        .execute(pool)
        .await?;

    Ok(())
}

async fn execute_idempotent_migration_sql(
    pool: &sqlx::Pool<MySql>,
    sql: &str,
) -> Result<(), sqlx::Error> {
    // Idempotent migrations need tolerance for vendor-specific error codes
    // when the target is already in the desired state:
    //   * 1060 — ALTER ADD COLUMN on an existing column (fresh DB where
    //     CREATE TABLE already installed it).
    //   * 1061 — ALTER ADD INDEX on an existing index (same reason).
    //   * 1091 — MySQL's "Can't DROP; check column/key exists".
    //   * 20101 — MatrixOne's internal error for the same DROP-missing case.
    //   * 1062 — duplicate key value on an index being added; handled by
    //     explicit dedupe migrations, not here.
    //
    // MySQL's SQLSTATE is a generic "HY000" for these; the real signal is
    // the numeric error code, which we read via downcast to the driver's
    // `MySqlDatabaseError`.
    match query(sql).execute(pool).await {
        Ok(_) => {}
        Err(sqlx::Error::Database(db_err)) => {
            let number = db_err
                .try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>()
                .map(|e| e.number());
            if matches!(number, Some(1060) | Some(1061) | Some(1091) | Some(20101)) {
                // Already present (or already absent for DROP) — fresh DB
                // created the schema via CREATE TABLE. Record and continue.
            } else {
                return Err(sqlx::Error::Database(db_err));
            }
        }
        Err(e) => return Err(e),
    }
    Ok(())
}

async fn run_migrations(pool: &sqlx::Pool<MySql>) -> Result<(), sqlx::Error> {
    run_migration(
        pool,
        2,
        "add covering index on skills_registry for listing queries",
        "SELECT 1", // index already in CREATE TABLE above; marker only
    )
    .await?;

    run_migration(
        pool,
        3,
        "add active_plan_id to agent_sessions for plan-mode linkage",
        "ALTER TABLE agent_sessions ADD COLUMN active_plan_id VARCHAR(64) NULL",
    )
    .await?;

    run_migration(
        pool,
        4,
        "add subtask_count to plans for denormalized list rendering",
        "ALTER TABLE plans ADD COLUMN subtask_count INT NOT NULL DEFAULT 0",
    )
    .await?;

    // Index on active_plan_id covers the cascade clear in `delete_plan` and
    // `set_active_plan` (`UPDATE agent_sessions SET active_plan_id = NULL
    // WHERE active_plan_id = ?`). Without it, every plan deletion triggers a
    // full table scan of `agent_sessions`.
    run_migration(
        pool,
        5,
        "add index on agent_sessions.active_plan_id for cascade clears",
        "ALTER TABLE agent_sessions \
         ADD INDEX idx_agent_sessions_active_plan_id (active_plan_id)",
    )
    .await?;

    // Enforce uniqueness on (plan_id, subtask_id, attempt). Two concurrent
    // `redo_step` calls would each compute `next_attempt = max+1` from a
    // stale read and both insert the same tuple; the UNIQUE index makes
    // exactly one INSERT win so `record_step_run` can surface a Conflict.
    //
    // Migrations 6-8 are sequenced for safety on DBs that already
    // accumulated duplicates under the prior non-unique index:
    //   6: drop the old non-unique index (1091/20101 tolerance covers the
    //      fresh-DB case where CREATE TABLE never installed it).
    //   7: DELETE duplicate rows, keeping the lexicographically smallest
    //      run_id per (plan_id, subtask_id, attempt). Idempotent — on a
    //      DB with no dupes this is a no-op DELETE.
    //   8: ADD UNIQUE. With 7 finished no 1062 can occur; the 1061
    //      tolerance handles fresh DBs whose CREATE TABLE already installed
    //      the unique key.
    run_migration(
        pool,
        6,
        "drop non-unique idx_step_runs_subtask_attempt before upgrading to UNIQUE",
        "ALTER TABLE plan_step_runs DROP INDEX idx_step_runs_subtask_attempt",
    )
    .await?;

    run_migration(
        pool,
        7,
        "dedupe plan_step_runs on (plan_id, subtask_id, attempt) keeping oldest run_id",
        // MatrixOne silently no-ops the MySQL `DELETE t FROM t JOIN ...`
        // form, so we use a NOT IN subquery. Plain MySQL also forbids
        // SELECT from the same target table in a direct subquery, which we
        // dodge by wrapping the GROUP BY in a derived table.
        "DELETE FROM plan_step_runs WHERE run_id NOT IN ( \
             SELECT keep_id FROM ( \
                 SELECT MIN(run_id) AS keep_id FROM plan_step_runs \
                 GROUP BY plan_id, subtask_id, attempt \
             ) keep \
         )",
    )
    .await?;

    run_migration(
        pool,
        8,
        "add UNIQUE (plan_id, subtask_id, attempt) on plan_step_runs",
        "ALTER TABLE plan_step_runs \
         ADD UNIQUE KEY uq_step_runs_subtask_attempt (plan_id, subtask_id, attempt)",
    )
    .await?;

    run_migration(
        pool,
        9,
        "add trace_id, message_count, idx_csl_turn to conversation_log",
        "ALTER TABLE conversation_log \
         ADD COLUMN trace_id VARCHAR(64) DEFAULT NULL, \
         ADD COLUMN message_count INT DEFAULT NULL, \
         ADD INDEX idx_csl_turn (session_id, turn)",
    )
    .await?;

    // Step 4a — content-addressed config versions.
    //
    // Two migrations so fresh DBs get everything via the CREATE TABLE
    // above and existing DBs catch up here.

    run_migration(
        pool,
        10,
        "create config_versions table (Step 4a cloud mirror of local version store)",
        crate::config_version_cloud::CONFIG_VERSIONS_CREATE_SQL,
    )
    .await?;

    run_migration(
        pool,
        11,
        "add config_version_id pointer column to agent_sessions",
        "ALTER TABLE agent_sessions \
         ADD COLUMN config_version_id VARCHAR(24) DEFAULT NULL",
    )
    .await?;

    run_migration(
        pool,
        12,
        "add config_version_id index on agent_sessions",
        "ALTER TABLE agent_sessions \
         ADD INDEX idx_agent_sessions_config_version (config_version_id)",
    )
    .await?;

    run_migration(
        pool,
        13,
        "widen session_todo_counters.next_id for exhausted task id sentinel",
        "ALTER TABLE session_todo_counters MODIFY COLUMN next_id BIGINT NOT NULL",
    )
    .await?;

    run_migration(
        pool,
        14,
        "add DB traceability columns and indexes to agent_events",
        "SELECT 1",
    )
    .await?;

    run_migration_batch(
        pool,
        15,
        "drop obsolete harness and memory tables once",
        &[
            "DROP TABLE IF EXISTS harness_artifacts",
            "DROP TABLE IF EXISTS harness_agent_roles",
            "DROP TABLE IF EXISTS harness_subagent_runs",
            "DROP TABLE IF EXISTS harness_blackboard_entries",
            "DROP TABLE IF EXISTS prompt_chunks",
            "DROP TABLE IF EXISTS harness_sources",
            "DROP TABLE IF EXISTS harness_decisions",
            "DROP TABLE IF EXISTS wf_definitions",
            "DROP TABLE IF EXISTS wf_runs",
            "DROP TABLE IF EXISTS skill_marketplace_stats",
            "DROP TABLE IF EXISTS skill_quality_reports",
            "DROP TABLE IF EXISTS step_idempotency_cache",
            "DROP TABLE IF EXISTS mem_memories",
            "DROP TABLE IF EXISTS sk_knowledge_entries",
            "DROP TABLE IF EXISTS governance_runs",
            "DROP TABLE IF EXISTS eval_llm_feedback",
            "DROP TABLE IF EXISTS user_preference_history",
        ],
    )
    .await?;

    Ok(())
}

pub fn database_user_from_row(row: sqlx::mysql::MySqlRow) -> DatabaseUserRecord {
    DatabaseUserRecord {
        user_id: row.try_get("user_id").unwrap_or_default(),
        username: row.try_get("username").unwrap_or_default(),
        email: row.try_get("email").unwrap_or_default(),
        password_hash: row.try_get("password_hash").unwrap_or_default(),
        display_name: row.try_get("display_name").ok(),
        is_active: row.try_get::<i64, _>("is_active").unwrap_or(1) != 0,
    }
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
    skill_version: &str,
) -> Result<(), sqlx::Error> {
    query("UPDATE skill_selection_events SET skill_version = ? WHERE event_id = ?")
        .bind(skill_version)
        .bind(event_id)
        .execute(&mut **tx)
        .await?;
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
        let skill_name = row.try_get::<String, _>("skill_name").unwrap_or_default();
        if skill_name.is_empty() || versions.contains_key(&skill_name) {
            continue;
        }
        let version = row.try_get::<String, _>("version").unwrap_or_default();
        if !version.is_empty() {
            versions.insert(skill_name, version);
        }
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
    /// Max age in days for sync log entries (default: 30)
    pub sync_log_days: u32,
    /// Max age in days for audit logs (default: 90)
    pub audit_log_days: u32,
    /// Max age in days for agent events (default: 90)
    pub event_days: u32,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            refresh_token_days: 7,
            auth_token_days: 30,
            task_lease_days: 7,
            sync_log_days: 30,
            audit_log_days: 90,
            event_days: 90,
        }
    }
}

/// Purge expired data across all tables with TTL/expiry semantics.
///
/// Returns a list of per-table cleanup results showing how many rows were deleted.
/// Each DELETE uses a LIMIT to avoid long-running locks; callers should invoke
/// repeatedly until all results show 0 rows deleted for a full sweep.
pub async fn cleanup_expired_data(
    pool: &sqlx::Pool<MySql>,
    policy: &RetentionPolicy,
) -> Vec<CleanupResult> {
    const BATCH_LIMIT: u32 = 1000;
    let mut results = Vec::new();

    // 1. Expired + revoked refresh tokens
    let deleted = sqlx::query(
        "DELETE FROM auth_refresh_tokens \
         WHERE (expires_at < NOW(6) OR is_revoked = 1) \
           AND created_at < DATE_SUB(NOW(6), INTERVAL ? DAY) \
         LIMIT ?",
    )
    .bind(policy.refresh_token_days)
    .bind(BATCH_LIMIT)
    .execute(pool)
    .await
    .map(|r| r.rows_affected())
    .unwrap_or(0);
    results.push(CleanupResult {
        table: "auth_refresh_tokens",
        rows_deleted: deleted,
    });

    // 2. Inactive auth tokens
    let deleted = sqlx::query(
        "DELETE FROM auth_tokens \
         WHERE is_active = 0 \
           AND created_at < DATE_SUB(NOW(6), INTERVAL ? DAY) \
         LIMIT ?",
    )
    .bind(policy.auth_token_days)
    .bind(BATCH_LIMIT)
    .execute(pool)
    .await
    .map(|r| r.rows_affected())
    .unwrap_or(0);
    results.push(CleanupResult {
        table: "auth_tokens",
        rows_deleted: deleted,
    });

    // 3. Expired task leases
    let deleted = sqlx::query(
        "DELETE FROM task_leases \
         WHERE expires_at < DATE_SUB(NOW(6), INTERVAL ? DAY) \
         LIMIT ?",
    )
    .bind(policy.task_lease_days)
    .bind(BATCH_LIMIT)
    .execute(pool)
    .await
    .map(|r| r.rows_affected())
    .unwrap_or(0);
    results.push(CleanupResult {
        table: "task_leases",
        rows_deleted: deleted,
    });

    // 4. Old sync log entries
    let deleted = sqlx::query(
        "DELETE FROM session_sync_log \
         WHERE created_at < DATE_SUB(NOW(6), INTERVAL ? DAY) \
         LIMIT ?",
    )
    .bind(policy.sync_log_days)
    .bind(BATCH_LIMIT)
    .execute(pool)
    .await
    .map(|r| r.rows_affected())
    .unwrap_or(0);
    results.push(CleanupResult {
        table: "session_sync_log",
        rows_deleted: deleted,
    });

    // 5. Old audit logs
    let deleted = sqlx::query(
        "DELETE FROM auth_audit_logs \
         WHERE created_at < DATE_SUB(NOW(6), INTERVAL ? DAY) \
         LIMIT ?",
    )
    .bind(policy.audit_log_days)
    .bind(BATCH_LIMIT)
    .execute(pool)
    .await
    .map(|r| r.rows_affected())
    .unwrap_or(0);
    results.push(CleanupResult {
        table: "auth_audit_logs",
        rows_deleted: deleted,
    });

    // 6. Old agent events
    let expired_event_rows = sqlx::query(
        "SELECT event_id FROM agent_events \
         WHERE created_at < DATE_SUB(NOW(6), INTERVAL ? DAY) \
         ORDER BY created_at ASC \
         LIMIT ?",
    )
    .bind(policy.event_days)
    .bind(BATCH_LIMIT)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let expired_event_ids: Vec<String> = expired_event_rows
        .into_iter()
        .filter_map(|row| row.try_get("event_id").ok())
        .collect();
    if !expired_event_ids.is_empty() {
        let _ = delete_agent_event_edges_for_event_ids(pool, &expired_event_ids).await;
    }
    let deleted = if expired_event_ids.is_empty() {
        0
    } else {
        let mut builder =
            QueryBuilder::<MySql>::new("DELETE FROM agent_events WHERE event_id IN (");
        let mut event_ids = builder.separated(", ");
        for event_id in &expired_event_ids {
            event_ids.push_bind(event_id);
        }
        event_ids.push_unseparated(")");
        builder
            .build()
            .execute(pool)
            .await
            .map(|r| r.rows_affected())
            .unwrap_or(0)
    };
    results.push(CleanupResult {
        table: "agent_events",
        rows_deleted: deleted,
    });

    results
}

#[cfg(test)]
mod tests {
    use super::*;

    fn migrate(raw: &str) -> serde_json::Value {
        let updated = promote_legacy_fallback_model_quirks(raw)
            .expect("migration should parse")
            .expect("migration should update");
        serde_json::from_str(&updated).expect("updated quirks should be valid JSON")
    }

    #[test]
    fn fallback_model_migration_creates_fallback_chain() {
        let value = migrate(r#"{"fallback_model":"model-b","no_system_message":true}"#);

        assert_eq!(value.get("fallback_model"), None);
        assert_eq!(
            value.get("fallback_chain"),
            Some(&serde_json::json!(["model-b"]))
        );
        assert_eq!(
            value.get("no_system_message"),
            Some(&serde_json::Value::Bool(true))
        );
    }

    #[test]
    fn fallback_model_migration_preserves_existing_fallback_chain() {
        let value = migrate(r#"{"fallback_model":"model-b","fallback_chain":["model-c"]}"#);

        assert_eq!(value.get("fallback_model"), None);
        assert_eq!(
            value.get("fallback_chain"),
            Some(&serde_json::json!(["model-c"]))
        );
    }

    #[test]
    fn fallback_model_migration_ignores_rows_without_legacy_key() {
        let updated = promote_legacy_fallback_model_quirks(r#"{"fallback_chain":["model-c"]}"#)
            .expect("migration should parse");

        assert!(updated.is_none());
    }

    #[test]
    fn fallback_model_migration_rejects_non_string_legacy_key() {
        let error = promote_legacy_fallback_model_quirks(r#"{"fallback_model":["model-b"]}"#)
            .expect_err("non-string legacy fallback_model should fail");

        assert!(error.contains("must be a string"));
    }
}
