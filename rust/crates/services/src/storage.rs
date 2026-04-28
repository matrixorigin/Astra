use crate::auth::DatabaseUserRecord;
use crate::auth::session::SessionRecord;
use astra_core::{ErrorResponse, MatrixOneSettings, connect_matrixone, internal_error};
use axum::{Json, http::StatusCode};
use fs2::FileExt;
use sqlx::{Executor, MySql, QueryBuilder, Row, query};
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

/// When `ASTRA_AUTO_CREATE_DATABASE=1`, connect to a bootstrap catalog (default `mysql`) and
/// run `CREATE DATABASE IF NOT EXISTS` for [`MatrixOneSettings::database`] before normal DDL.
async fn ensure_matrixone_database_exists(settings: &MatrixOneSettings) -> Result<(), sqlx::Error> {
    use std::error::Error;

    crate::snapshot_sql::validate_sql_identifier(&settings.database, "matrixone database")
        .map_err(|e| {
            sqlx::Error::Configuration(Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                e,
            )) as Box<dyn Error + Send + Sync>)
        })?;
    let catalog =
        std::env::var("MATRIXONE_BOOTSTRAP_CATALOG").unwrap_or_else(|_| "mysql".to_string());
    crate::snapshot_sql::validate_sql_identifier(&catalog, "matrixone bootstrap catalog").map_err(
        |e| {
            sqlx::Error::Configuration(Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                e,
            )) as Box<dyn Error + Send + Sync>)
        },
    )?;
    let mut admin_settings = settings.clone();
    admin_settings.database = catalog;
    let admin_pool = connect_matrixone(&admin_settings).await?;
    let ddl = format!(
        "CREATE DATABASE IF NOT EXISTS {}",
        crate::snapshot_sql::quote_mysql_identifier(&settings.database)
    );
    query(&ddl).execute(&admin_pool).await?;
    admin_pool.close().await;
    Ok(())
}

pub async fn ensure_core_schema(settings: &MatrixOneSettings) -> Result<(), sqlx::Error> {
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
        ensure_matrixone_database_exists(settings).await?;
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
            INDEX idx_agent_sessions_user_status_updated (user_id, status, updated_at),
            INDEX idx_agent_sessions_user_last_active (user_id, last_active_at),
            INDEX idx_agent_sessions_agent_status (agent_id, status)
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
            INDEX idx_agent_events_tool_name (meta_tool_name)
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
        "CREATE TABLE IF NOT EXISTS skill_selector_turn_metrics (
            event_id VARCHAR(64) PRIMARY KEY,
            session_id VARCHAR(64) NOT NULL,
            user_id VARCHAR(64) NULL,
            turn_number BIGINT NOT NULL,
            visible_skill_count BIGINT NOT NULL,
            chosen_skill_count BIGINT NOT NULL,
            shortlisted_chosen_count BIGINT NOT NULL,
            missed_chosen_count BIGINT NOT NULL,
            best_chosen_rank BIGINT NULL,
            selector_tier VARCHAR(64) NULL,
            elapsed_ms BIGINT NULL,
            total_catalog_size BIGINT NULL,
            extra JSON NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_skill_selector_metrics_created (created_at),
            INDEX idx_skill_selector_metrics_session_turn (session_id, turn_number),
            INDEX idx_skill_selector_metrics_tier (selector_tier)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS eval_llm_feedback (
            feedback_id VARCHAR(64) PRIMARY KEY,
            prompt_template_id VARCHAR(255) NULL,
            prompt_version VARCHAR(64) NULL,
            llm_request_id VARCHAR(64) NULL,
            rating BIGINT NULL,
            comment TEXT NULL,
            metadata JSON NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_eval_feedback_llm_request_id (llm_request_id),
            INDEX idx_eval_feedback_created_at (created_at)
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
            created_by VARCHAR(36) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_infra_llm_models_active_provider_name (is_active, provider, model_name)
        )",
    )
    .execute(&pool)
    .await?;

    // ── Learning state convergence (Phase F) ──

    query(
        "CREATE TABLE IF NOT EXISTS learning_snapshots (
            snapshot_id VARCHAR(64) PRIMARY KEY,
            user_id VARCHAR(64) NOT NULL,
            profile_name VARCHAR(100) NOT NULL,
            snapshot_json LONGTEXT NOT NULL,
            entity_count INT NOT NULL DEFAULT 0,
            pattern_count INT NOT NULL DEFAULT 0,
            has_calibration SMALLINT NOT NULL DEFAULT 0,
            version INT NOT NULL DEFAULT 1,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            UNIQUE KEY idx_learning_user_profile (user_id, profile_name),
            INDEX idx_learning_user_updated (user_id, updated_at)
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

    // Preference change history for audit trail and rollback
    query(
        "CREATE TABLE IF NOT EXISTS user_preference_history (
            history_id VARCHAR(64) PRIMARY KEY,
            user_id VARCHAR(64) NOT NULL,
            pref_key VARCHAR(100) NOT NULL,
            old_value LONGTEXT NULL,
            new_value LONGTEXT NOT NULL,
            old_version INT NULL,
            new_version INT NOT NULL,
            source VARCHAR(50) NOT NULL DEFAULT 'edge',
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_pref_history_user_key (user_id, pref_key, created_at)
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

    // Skills registry — master catalog for registered/marketplace skills.
    query(
        "CREATE TABLE IF NOT EXISTS skills_registry (
            skill_id VARCHAR(64) PRIMARY KEY,
            skill_name VARCHAR(255) NOT NULL,
            version VARCHAR(64) NOT NULL,
            description TEXT NULL,
            skill_definition JSON NULL,
            code_hash VARCHAR(128) NULL,
            triggers JSON NULL,
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
            created_by VARCHAR(36) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            UNIQUE KEY uq_skill_name_version (skill_name, version),
            INDEX idx_skill_active_name (is_active, status, skill_name),
            INDEX idx_skill_active_created_at (is_active, created_at),
            INDEX idx_skill_source_name (source, skill_name),
            INDEX idx_skill_active_name_ver (is_active, skill_name, version)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS skill_marketplace_stats (
            skill_name          VARCHAR(255) PRIMARY KEY,
            publisher_id        VARCHAR(255),
            total_installs      BIGINT DEFAULT 0,
            active_users_7d     INT DEFAULT 0,
            avg_quality         FLOAT DEFAULT 0.0,
            avg_rating          FLOAT DEFAULT 0.0,
            report_count        INT DEFAULT 0,
            compatibility_score FLOAT DEFAULT 0.0,
            trust_tier          VARCHAR(32),
            last_updated        TIMESTAMP,
            INDEX idx_ranking (avg_quality, active_users_7d)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS skill_quality_reports (
            id                  BIGINT AUTO_INCREMENT PRIMARY KEY,
            skill_name          VARCHAR(255) NOT NULL,
            skill_version       VARCHAR(50) NOT NULL,
            runtime_version     VARCHAR(50) NOT NULL,
            success_rate        FLOAT,
            avg_tokens          FLOAT,
            invocation_count    INT,
            reported_at         TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
            INDEX idx_skill (skill_name, skill_version)
        )",
    )
    .execute(&pool)
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
            INDEX idx_plans_user_updated (user_id, updated_at),
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
            INDEX idx_step_runs_plan_started (plan_id, started_at),
            INDEX idx_step_runs_subtask_attempt (plan_id, subtask_id, attempt),
            INDEX idx_step_runs_session (session_id, started_at)
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
            artifact_kind VARCHAR(64) NOT NULL,
            source VARCHAR(64) NULL,
            turn INT NULL,
            round INT NULL,
            content_json LONGTEXT NOT NULL,
            metadata JSON NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_session_artifacts_session_kind_created (session_id, artifact_kind, created_at),
            INDEX idx_session_artifacts_session_source_created (session_id, source, created_at),
            INDEX idx_session_artifacts_user_created (user_id, created_at)
        )",
    )
    .execute(&pool)
    .await?;

    // Step Protocol idempotency cache
    query(
        "CREATE TABLE IF NOT EXISTS step_idempotency_cache (
            cache_key VARCHAR(200) PRIMARY KEY,
            step_id VARCHAR(100) NOT NULL,
            tool_index INT NOT NULL,
            content_hash VARCHAR(64) NOT NULL,
            tool_name VARCHAR(100) NOT NULL,
            output LONGTEXT NOT NULL,
            is_error SMALLINT NOT NULL DEFAULT 0,
            cached_at BIGINT NOT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_idempotency_step_tool (step_id, tool_index),
            INDEX idx_idempotency_hash (content_hash)
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

    // ─── Workflow tables ─────────────────────────────────────────────────────────

    query(
        "CREATE TABLE IF NOT EXISTS wf_definitions (
            workflow_id  VARCHAR(36) PRIMARY KEY,
            name         VARCHAR(128) NOT NULL,
            version      VARCHAR(32) NOT NULL DEFAULT '1.0.0',
            description  TEXT,
            definition   LONGTEXT NOT NULL,
            is_active    SMALLINT NOT NULL DEFAULT 1,
            created_at   DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at   DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6),
            INDEX idx_wfd_name (name),
            INDEX idx_wfd_active (is_active)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS wf_runs (
            run_id           VARCHAR(36) PRIMARY KEY,
            workflow_id      VARCHAR(36) NOT NULL,
            agent_run_id     VARCHAR(36),
            status           VARCHAR(32) NOT NULL DEFAULT 'pending',
            waiting_for      VARCHAR(128),
            current_step_idx INT NOT NULL DEFAULT 0,
            step_results     LONGTEXT,
            error            TEXT,
            created_at       DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at       DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6),
            INDEX idx_wfr_workflow (workflow_id),
            INDEX idx_wfr_status (status),
            INDEX idx_wfr_agent_run (agent_run_id)
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

    // ─── Memory and knowledge tables ─────────────────────────────────────────────

    query(
        "CREATE TABLE IF NOT EXISTS mem_memories (
            memory_id          VARCHAR(36) PRIMARY KEY,
            user_id            VARCHAR(36) NOT NULL,
            content            TEXT NOT NULL,
            memory_type        VARCHAR(32) NOT NULL DEFAULT 'semantic',
            is_active          SMALLINT NOT NULL DEFAULT 1,
            initial_confidence DECIMAL(5,4) DEFAULT 0.5,
            observed_at        DATETIME(6),
            created_at         DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at         DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6),
            INDEX idx_mm_user (user_id),
            INDEX idx_mm_type (memory_type),
            INDEX idx_mm_active (is_active),
            INDEX idx_mm_user_active_type_updated (user_id, is_active, memory_type, updated_at)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS sk_knowledge_entries (
            entry_id     VARCHAR(36) PRIMARY KEY,
            skill_name   VARCHAR(128) NOT NULL,
            user_id      VARCHAR(36),
            entry_type   VARCHAR(64) NOT NULL,
            content      LONGTEXT NOT NULL,
            metadata     LONGTEXT,
            is_active    SMALLINT NOT NULL DEFAULT 1,
            created_at   DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at   DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6),
            INDEX idx_ske_skill (skill_name),
            INDEX idx_ske_user (user_id),
            INDEX idx_ske_type (entry_type)
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

    query(
        "CREATE TABLE IF NOT EXISTS governance_runs (
            run_id     VARCHAR(36) PRIMARY KEY,
            task_name  VARCHAR(128) NOT NULL,
            result     LONGTEXT,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_gr_task (task_name),
            INDEX idx_gr_created (created_at)
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

    // Idempotent migrations: ALTER ADD COLUMN on a table whose CREATE already
    // includes that column (fresh DB) returns MySQL error 1060 "Duplicate
    // column name"; ADD INDEX on an existing index returns 1061. Treat both
    // as "column/index already present" and still record the migration so we
    // don't try again on the next boot.
    //
    // MySQL's SQLSTATE is a generic "HY000" for these — the real signal is the
    // numeric error code, which we read via downcast to `MySqlDatabaseError`.
    match query(sql).execute(pool).await {
        Ok(_) => {}
        Err(sqlx::Error::Database(db_err)) => {
            let number = db_err
                .try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>()
                .map(|e| e.number());
            if matches!(number, Some(1060) | Some(1061)) {
                // Already present — fresh DB created the column/index via
                // the CREATE TABLE path. Record and continue.
            } else {
                return Err(sqlx::Error::Database(db_err));
            }
        }
        Err(e) => return Err(e),
    }

    query("INSERT IGNORE INTO schema_migrations (version, description) VALUES (?, ?)")
        .bind(version)
        .bind(description)
        .execute(pool)
        .await?;

    Ok(())
}

async fn run_migrations(pool: &sqlx::Pool<MySql>) -> Result<(), sqlx::Error> {
    run_migration(
        pool,
        1,
        "add composite index on mem_memories for profile queries",
        "SELECT 1", // index already in CREATE TABLE above; marker only
    )
    .await?;

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

/// Aggregated hit-rate / recall stats for either the overall recent window or
/// one selector tier inside it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SkillSelectorTierStats {
    pub sample_size: i64,
    pub hit_at_1_rate: f64,
    pub hit_at_5_rate: f64,
    pub hit_at_10_rate: f64,
    pub hit_at_20_rate: f64,
    pub hit_at_30_rate: f64,
    pub shortlist_recall_rate: f64,
    pub avg_best_chosen_rank: Option<f64>,
    pub avg_elapsed_ms: Option<f64>,
    pub avg_total_catalog_size: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SkillSelectorTierEntry {
    /// Selector tier label (e.g. "lexical", "embedding", "embedding+rerank").
    /// Empty string represents rows with NULL `selector_tier`.
    pub tier: String,
    pub stats: SkillSelectorTierStats,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SkillSelectorMetricSummary {
    /// Aggregate over all rows in the recent window (across tiers).
    pub overall: SkillSelectorTierStats,
    /// Per-tier breakdown, ordered by sample_size desc.
    pub per_tier: Vec<SkillSelectorTierEntry>,
}

impl SkillSelectorMetricSummary {
    /// Convenience accessor for total sample size in the recent window.
    pub fn sample_size(&self) -> i64 {
        self.overall.sample_size
    }
}

fn row_f64_or_zero(row: &sqlx::mysql::MySqlRow, column: &str) -> f64 {
    row.try_get::<f64, _>(column)
        .or_else(|_| {
            row.try_get::<String, _>(column)
                .map(|value| value.parse::<f64>().unwrap_or(0.0))
        })
        .or_else(|_| {
            row.try_get::<Vec<u8>, _>(column).map(|value| {
                std::str::from_utf8(&value)
                    .ok()
                    .and_then(|text| text.parse::<f64>().ok())
                    .unwrap_or(0.0)
            })
        })
        .unwrap_or(0.0)
}

fn row_optional_f64(row: &sqlx::mysql::MySqlRow, column: &str) -> Option<f64> {
    row.try_get::<Option<f64>, _>(column)
        .ok()
        .flatten()
        .or_else(|| {
            row.try_get::<Option<String>, _>(column)
                .ok()
                .flatten()
                .and_then(|value| value.parse::<f64>().ok())
        })
        .or_else(|| {
            row.try_get::<Option<Vec<u8>>, _>(column)
                .ok()
                .flatten()
                .and_then(|value| std::str::from_utf8(&value).ok().map(str::to_string))
                .and_then(|value| value.parse::<f64>().ok())
        })
}

pub async fn load_recent_skill_selector_metric_summary(
    pool: &sqlx::Pool<MySql>,
    limit: i64,
) -> Result<SkillSelectorMetricSummary, sqlx::Error> {
    let limit = limit.max(1);
    // Aggregate query: GROUP BY selector_tier inside the recent window. We compute
    // the "overall" row by adding a NULL grouping marker via WITH ROLLUP — but
    // MatrixOne support for ROLLUP is uneven, so we issue two queries instead:
    // one ungrouped overall, one GROUP BY tier.
    const SELECT_LIST: &str = "COUNT(*) AS sample_size,
                CAST(AVG(CASE WHEN best_chosen_rank IS NOT NULL AND best_chosen_rank <= 1  THEN 1.0 ELSE 0.0 END) AS CHAR) AS hit_at_1_rate,
                CAST(AVG(CASE WHEN best_chosen_rank IS NOT NULL AND best_chosen_rank <= 5  THEN 1.0 ELSE 0.0 END) AS CHAR) AS hit_at_5_rate,
                CAST(AVG(CASE WHEN best_chosen_rank IS NOT NULL AND best_chosen_rank <= 10 THEN 1.0 ELSE 0.0 END) AS CHAR) AS hit_at_10_rate,
                CAST(AVG(CASE WHEN best_chosen_rank IS NOT NULL AND best_chosen_rank <= 20 THEN 1.0 ELSE 0.0 END) AS CHAR) AS hit_at_20_rate,
                CAST(AVG(CASE WHEN best_chosen_rank IS NOT NULL AND best_chosen_rank <= 30 THEN 1.0 ELSE 0.0 END) AS CHAR) AS hit_at_30_rate,
                CAST(AVG(CASE
                        WHEN chosen_skill_count > 0
                        THEN (1.0 * shortlisted_chosen_count) / chosen_skill_count
                    END) AS CHAR) AS shortlist_recall_rate,
                CAST(AVG(best_chosen_rank) AS CHAR) AS avg_best_chosen_rank,
                CAST(AVG(elapsed_ms) AS CHAR) AS avg_elapsed_ms,
                CAST(AVG(total_catalog_size) AS CHAR) AS avg_total_catalog_size";

    let overall_sql = format!(
        "SELECT {SELECT_LIST}
         FROM (
             SELECT *
             FROM skill_selector_turn_metrics
             ORDER BY created_at DESC, event_id DESC
             LIMIT ?
         ) recent"
    );
    let overall_row = query(&overall_sql).bind(limit).fetch_one(pool).await?;
    let overall = decode_tier_stats(&overall_row);

    let per_tier_sql = format!(
        "SELECT COALESCE(selector_tier, '') AS tier, {SELECT_LIST}
         FROM (
             SELECT *
             FROM skill_selector_turn_metrics
             ORDER BY created_at DESC, event_id DESC
             LIMIT ?
         ) recent
         GROUP BY tier
         ORDER BY sample_size DESC"
    );
    let tier_rows = query(&per_tier_sql).bind(limit).fetch_all(pool).await?;
    let per_tier = tier_rows
        .iter()
        .map(|row| SkillSelectorTierEntry {
            tier: row.try_get::<String, _>("tier").unwrap_or_default(),
            stats: decode_tier_stats(row),
        })
        .collect();

    Ok(SkillSelectorMetricSummary { overall, per_tier })
}

fn decode_tier_stats(row: &sqlx::mysql::MySqlRow) -> SkillSelectorTierStats {
    SkillSelectorTierStats {
        sample_size: row.try_get("sample_size").unwrap_or(0),
        hit_at_1_rate: row_f64_or_zero(row, "hit_at_1_rate"),
        hit_at_5_rate: row_f64_or_zero(row, "hit_at_5_rate"),
        hit_at_10_rate: row_f64_or_zero(row, "hit_at_10_rate"),
        hit_at_20_rate: row_f64_or_zero(row, "hit_at_20_rate"),
        hit_at_30_rate: row_f64_or_zero(row, "hit_at_30_rate"),
        shortlist_recall_rate: row_f64_or_zero(row, "shortlist_recall_rate"),
        avg_best_chosen_rank: row_optional_f64(row, "avg_best_chosen_rank"),
        avg_elapsed_ms: row_optional_f64(row, "avg_elapsed_ms"),
        avg_total_catalog_size: row_optional_f64(row, "avg_total_catalog_size"),
    }
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
    /// Max age in days for idempotency cache entries (default: 3)
    pub idempotency_cache_days: u32,
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
            idempotency_cache_days: 3,
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

    // 4. Stale idempotency cache entries
    let deleted = sqlx::query(
        "DELETE FROM step_idempotency_cache \
         WHERE created_at < DATE_SUB(NOW(6), INTERVAL ? DAY) \
         LIMIT ?",
    )
    .bind(policy.idempotency_cache_days)
    .bind(BATCH_LIMIT)
    .execute(pool)
    .await
    .map(|r| r.rows_affected())
    .unwrap_or(0);
    results.push(CleanupResult {
        table: "step_idempotency_cache",
        rows_deleted: deleted,
    });

    // 5. Old sync log entries
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

    // 6. Old audit logs
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

    // 7. Old agent events
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
