use crate::auth::DatabaseUserRecord;
use crate::auth::session::SessionRecord;
use axum::{Json, http::StatusCode};
use mo_agent_core::{ErrorResponse, MatrixOneSettings, connect_matrixone, internal_error};
use sqlx::{MySql, QueryBuilder, Row, query};
use std::collections::{BTreeSet, HashMap};
use uuid::Uuid;

fn is_missing_skills_registry_message(message: &str) -> bool {
    message.contains("skills_registry") && message.contains("does not exist")
}

pub async fn ensure_core_schema(settings: &MatrixOneSettings) -> Result<(), sqlx::Error> {
    let pool = connect_matrixone(settings).await?;

    // Auth
    query(
        "CREATE TABLE IF NOT EXISTS auth_users (
            user_id VARCHAR(36) PRIMARY KEY,
            username VARCHAR(50) NOT NULL UNIQUE,
            email VARCHAR(255) NOT NULL UNIQUE,
            password_hash VARCHAR(255) NOT NULL,
            display_name VARCHAR(100) NULL,
            is_active SMALLINT NOT NULL DEFAULT 1,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            last_login_at DATETIME(6) NULL,
            INDEX idx_auth_users_username (username),
            INDEX idx_auth_users_email (email)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS auth_roles (
            role_id VARCHAR(36) PRIMARY KEY,
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
            user_id VARCHAR(36) NOT NULL,
            role_id VARCHAR(36) NOT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            UNIQUE KEY uq_auth_user_roles_user_role (user_id, role_id),
            INDEX idx_auth_user_roles_user_id (user_id),
            INDEX idx_auth_user_roles_role_id (role_id),
            CONSTRAINT fk_user_roles_user FOREIGN KEY (user_id) REFERENCES auth_users(user_id) ON DELETE CASCADE,
            CONSTRAINT fk_user_roles_role FOREIGN KEY (role_id) REFERENCES auth_roles(role_id) ON DELETE CASCADE
        )",
    )
    .execute(&pool)
    .await?;
    // Add FK constraints for existing tables that predate this migration (ignore if already present)
    query(
        "ALTER TABLE auth_user_roles ADD CONSTRAINT fk_user_roles_user \
         FOREIGN KEY (user_id) REFERENCES auth_users(user_id) ON DELETE CASCADE",
    )
    .execute(&pool)
    .await
    .ok();
    query(
        "ALTER TABLE auth_user_roles ADD CONSTRAINT fk_user_roles_role \
         FOREIGN KEY (role_id) REFERENCES auth_roles(role_id) ON DELETE CASCADE",
    )
    .execute(&pool)
    .await
    .ok();

    query(
        "CREATE TABLE IF NOT EXISTS auth_refresh_tokens (
            token_id VARCHAR(36) PRIMARY KEY,
            user_id VARCHAR(36) NOT NULL,
            token_hash VARCHAR(255) NOT NULL,
            token_prefix VARCHAR(16) NULL,
            expires_at DATETIME(6) NOT NULL,
            is_revoked SMALLINT NOT NULL DEFAULT 0,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_auth_refresh_tokens_user_id (user_id),
            INDEX idx_auth_refresh_tokens_expires_at (expires_at),
            INDEX idx_auth_refresh_tokens_prefix (token_prefix),
            CONSTRAINT fk_refresh_tokens_user FOREIGN KEY (user_id) REFERENCES auth_users(user_id) ON DELETE CASCADE
        )",
    )
    .execute(&pool)
    .await?;
    query(
        "ALTER TABLE auth_refresh_tokens ADD CONSTRAINT fk_refresh_tokens_user \
         FOREIGN KEY (user_id) REFERENCES auth_users(user_id) ON DELETE CASCADE",
    )
    .execute(&pool)
    .await
    .ok();

    query(
        "CREATE TABLE IF NOT EXISTS auth_tokens (
            token_id VARCHAR(36) PRIMARY KEY,
            type VARCHAR(50) NOT NULL,
            provider VARCHAR(50) NOT NULL,
            encrypted_value TEXT NULL,
            secret_ref VARCHAR(255) NULL,
            is_active SMALLINT NOT NULL DEFAULT 1,
            scope_user_id VARCHAR(36) NULL,
            scope_repo VARCHAR(255) NULL,
            metadata JSON NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            expires_at DATETIME(6) NULL,
            INDEX idx_auth_tokens_scope_user (scope_user_id),
            INDEX idx_auth_tokens_scope_repo (scope_repo)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS auth_audit_logs (
            log_id VARCHAR(36) PRIMARY KEY,
            user_id VARCHAR(36) NOT NULL,
            action VARCHAR(50) NOT NULL,
            resource_type VARCHAR(50) NULL,
            resource_id VARCHAR(64) NULL,
            details JSON NULL,
            ip_address VARCHAR(45) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_auth_audit_logs_user_id (user_id),
            INDEX idx_auth_audit_logs_created_at (created_at)
        )",
    )
    .execute(&pool)
    .await?;

    // Sessions / events core
    query(
        "CREATE TABLE IF NOT EXISTS agent_sessions (
            session_id VARCHAR(36) PRIMARY KEY,
            user_id VARCHAR(36) NOT NULL,
            agent_id VARCHAR(64) NULL,
            title VARCHAR(255) NULL,
            status VARCHAR(20) NOT NULL DEFAULT 'active',
            event_count BIGINT NOT NULL DEFAULT 0,
            last_event_id VARCHAR(36) NULL,
            summary_status VARCHAR(20) NULL,
            summary_job_id VARCHAR(36) NULL,
            vector_db_snapshot_id VARCHAR(64) NULL,
            metadata JSON NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            ended_at DATETIME(6) NULL,
            last_active_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_agent_sessions_user_id (user_id),
            INDEX idx_agent_sessions_agent_id (agent_id),
            INDEX idx_agent_sessions_status (status),
            CONSTRAINT fk_sessions_user FOREIGN KEY (user_id) REFERENCES auth_users(user_id) ON DELETE CASCADE
        )",
    )
    .execute(&pool)
    .await?;
    query(
        "ALTER TABLE agent_sessions ADD CONSTRAINT fk_sessions_user \
         FOREIGN KEY (user_id) REFERENCES auth_users(user_id) ON DELETE CASCADE",
    )
    .execute(&pool)
    .await
    .ok();

    query(
        "CREATE TABLE IF NOT EXISTS agent_events (
            event_id VARCHAR(36) PRIMARY KEY,
            session_id VARCHAR(36) NOT NULL,
            user_id VARCHAR(36) NOT NULL,
            agent_id VARCHAR(64) NULL,
            agent_version VARCHAR(32) NULL,
            event_type VARCHAR(64) NOT NULL,
            content LONGTEXT NULL,
            parent_event_id VARCHAR(36) NULL,
            causal_chain_id VARCHAR(36) NULL,
            token_usage JSON NULL,
            llm_model_used VARCHAR(128) NULL,
            llm_params JSON NULL,
            metadata JSON NULL,
            skill_name VARCHAR(255) NULL,
            skill_version VARCHAR(64) NULL,
            reasoning_content LONGTEXT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_agent_events_session_id (session_id),
            INDEX idx_agent_events_user_id (user_id),
            INDEX idx_agent_events_event_type (event_type),
            INDEX idx_agent_events_causal_chain_id (causal_chain_id),
            INDEX idx_agent_events_created_at (created_at)
        )",
    )
    .execute(&pool)
    .await?;

    // Context / decisions / evaluation essentials used by turn persistence
    query(
        "CREATE TABLE IF NOT EXISTS ctx_snapshots (
            context_capture_id VARCHAR(36) PRIMARY KEY,
            session_id VARCHAR(36) NOT NULL,
            event_id VARCHAR(36) NOT NULL,
            context_data JSON NULL,
            llm_request_id VARCHAR(64) NULL,
            llm_response_id VARCHAR(64) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_ctx_snapshots_session_id (session_id),
            INDEX idx_ctx_snapshots_event_id (event_id),
            INDEX idx_ctx_snapshots_created_at (created_at)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS ctx_decision_audits (
            decision_id VARCHAR(36) PRIMARY KEY,
            session_id VARCHAR(36) NOT NULL,
            event_id VARCHAR(36) NULL,
            context_capture_id VARCHAR(36) NULL,
            decision_type VARCHAR(64) NOT NULL,
            decision_output JSON NULL,
            model_params JSON NULL,
            model_used VARCHAR(128) NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_ctx_decisions_session_id (session_id),
            INDEX idx_ctx_decisions_event_id (event_id),
            INDEX idx_ctx_decisions_type (decision_type),
            INDEX idx_ctx_decisions_created_at (created_at)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS skill_selection_events (
            event_id VARCHAR(36) PRIMARY KEY,
            session_id VARCHAR(36) NOT NULL,
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
            INDEX idx_skill_selection_session_id (session_id),
            INDEX idx_skill_selection_skill_name (skill_name),
            INDEX idx_skill_selection_created_at (created_at)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS eval_llm_feedback (
            feedback_id VARCHAR(36) PRIMARY KEY,
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
            model_id VARCHAR(36) PRIMARY KEY,
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
            INDEX idx_infra_llm_models_provider (provider),
            INDEX idx_infra_llm_models_is_active (is_active)
        )",
    )
    .execute(&pool)
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
    let _ = query(
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
    .await;
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

    let rows = match query_builder.build().fetch_all(pool).await {
        Ok(rows) => rows,
        Err(error) if is_missing_skills_registry_message(&error.to_string()) => {
            return Ok(HashMap::new());
        }
        Err(error) => return Err(error),
    };
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

#[cfg(test)]
mod tests {
    use super::is_missing_skills_registry_message;

    #[test]
    fn detects_missing_skills_registry_message() {
        assert!(is_missing_skills_registry_message(
            "error returned from database: 1064 (HY000): SQL parser error: table \"skills_registry\" does not exist"
        ));
    }

    #[test]
    fn ignores_unrelated_messages() {
        assert!(!is_missing_skills_registry_message(
            "error returned from database: duplicate key value"
        ));
    }
}
