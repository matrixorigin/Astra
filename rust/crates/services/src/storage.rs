use crate::auth::DatabaseUserRecord;
use crate::auth::session::SessionRecord;
use astra_core::{ErrorResponse, MatrixOneSettings, connect_matrixone, internal_error};
use axum::{Json, http::StatusCode};
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
            last_login_at DATETIME(6) NULL
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
            INDEX idx_auth_user_roles_role_id (role_id)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS auth_refresh_tokens (
            token_id VARCHAR(36) PRIMARY KEY,
            user_id VARCHAR(36) NOT NULL,
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
            INDEX idx_auth_audit_logs_user_created (user_id, created_at)
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
            INDEX idx_agent_sessions_user_status_updated (user_id, status, updated_at),
            INDEX idx_agent_sessions_user_last_active (user_id, last_active_at),
            INDEX idx_agent_sessions_agent_status (agent_id, status)
        )",
    )
    .execute(&pool)
    .await?;

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
            token_input  BIGINT NULL,
            token_output BIGINT NULL,
            token_total  BIGINT NULL,
            meta_tool_name VARCHAR(255) NULL,
            meta_duration_ms INT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_agent_events_session_created (session_id, created_at),
            INDEX idx_agent_events_session_type_created (session_id, event_type, created_at),
            INDEX idx_agent_events_session_parent (session_id, parent_event_id),
            INDEX idx_agent_events_user_created (user_id, created_at),
            INDEX idx_agent_events_causal_chain_id (causal_chain_id),
            INDEX idx_agent_events_skill_created (skill_name, created_at),
            INDEX idx_agent_events_tool_name (meta_tool_name)
        )",
    )
    .execute(&pool)
    .await?;

    // Child events (tool_call / tool_error) are fetched by parent turn via parent_event_id.
    if let Err(e) = query(
        "CREATE INDEX idx_agent_events_session_parent ON agent_events (session_id, parent_event_id)",
    )
    .execute(&pool)
    .await
    {
        let msg = e.to_string().to_lowercase();
        if !msg.contains("duplicate") && !msg.contains("already exists") {
            return Err(e);
        }
    }

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
            INDEX idx_ctx_snapshots_session_created (session_id, created_at),
            INDEX idx_ctx_snapshots_event_id (event_id)
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
            INDEX idx_ctx_decisions_session_type_created (session_id, decision_type, created_at),
            INDEX idx_ctx_decisions_event_id (event_id),
            INDEX idx_ctx_decisions_context_capture_id (context_capture_id)
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
            INDEX idx_skill_selection_session_created (session_id, created_at),
            INDEX idx_skill_selection_skill_created (skill_name, created_at)
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
            INDEX idx_infra_llm_models_active_provider_name (is_active, provider, model_name)
        )",
    )
    .execute(&pool)
    .await?;

    // ── Learning state convergence (Phase F) ──

    query(
        "CREATE TABLE IF NOT EXISTS learning_snapshots (
            snapshot_id VARCHAR(36) PRIMARY KEY,
            user_id VARCHAR(36) NOT NULL,
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
            pref_id VARCHAR(36) PRIMARY KEY,
            user_id VARCHAR(36) NOT NULL,
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
            history_id VARCHAR(36) PRIMARY KEY,
            user_id VARCHAR(36) NOT NULL,
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
            sync_id VARCHAR(36) PRIMARY KEY,
            user_id VARCHAR(36) NOT NULL,
            session_id VARCHAR(36) NOT NULL,
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
            skill_id VARCHAR(36) PRIMARY KEY,
            skill_name VARCHAR(255) NOT NULL,
            version VARCHAR(64) NOT NULL,
            description TEXT NULL,
            skill_definition JSON NULL,
            code_hash VARCHAR(128) NULL,
            triggers JSON NULL,
            dependencies JSON NULL,
            manifest JSON NULL,
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
            INDEX idx_skill_source_name (source, skill_name)
        )",
    )
    .execute(&pool)
    .await?;

    // ── Long-task orchestration (Phase H) ──

    query(
        "CREATE TABLE IF NOT EXISTS agent_tasks (
            task_id VARCHAR(36) PRIMARY KEY,
            user_id VARCHAR(36) NOT NULL,
            session_id VARCHAR(36) NULL,
            parent_task_id VARCHAR(36) NULL,
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

    // Phase 3: optional `agent_id` on tasks (ALTER is idempotent across versions).
    if let Err(e) = query("ALTER TABLE agent_tasks ADD COLUMN agent_id VARCHAR(128) NULL")
        .execute(&pool)
        .await
    {
        let msg = e.to_string();
        if !msg.to_lowercase().contains("duplicate")
            && !msg.to_lowercase().contains("already exists")
        {
            return Err(e);
        }
    }

    query(
        "CREATE TABLE IF NOT EXISTS edge_agent_registry (
            registry_id VARCHAR(36) PRIMARY KEY,
            user_id VARCHAR(36) NOT NULL,
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
            task_id VARCHAR(36) PRIMARY KEY,
            user_id VARCHAR(36) NOT NULL,
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
            template_id VARCHAR(36) PRIMARY KEY,
            user_id VARCHAR(36) NULL,
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

    query(
        "CREATE TABLE IF NOT EXISTS session_checkpoints (
            checkpoint_id VARCHAR(36) PRIMARY KEY,
            session_id VARCHAR(36) NOT NULL,
            user_id VARCHAR(36) NOT NULL,
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

    // Migration: add contract_state_json column for verification context in checkpoints.
    if let Err(e) =
        query("ALTER TABLE session_checkpoints ADD COLUMN contract_state_json LONGTEXT NULL")
            .execute(&pool)
            .await
    {
        let msg = e.to_string();
        if !msg.to_lowercase().contains("duplicate")
            && !msg.to_lowercase().contains("already exists")
        {
            return Err(e);
        }
    }

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

    // Durable agent runs — event-sourced run state with checkpoint support
    query(
        "CREATE TABLE IF NOT EXISTS agent_runs (
            run_id VARCHAR(36) PRIMARY KEY,
            user_id VARCHAR(36) NOT NULL,
            session_id VARCHAR(36) NOT NULL,
            parent_run_id VARCHAR(36) NULL,
            delegation_id VARCHAR(36) NULL,
            agent_id VARCHAR(64) NULL,
            status VARCHAR(20) NOT NULL DEFAULT 'running',
            waiting_for VARCHAR(200) NULL,
            checkpoint_json LONGTEXT NULL,
            error_message TEXT NULL,
            retry_count INT NOT NULL DEFAULT 0,
            total_prompt_tokens BIGINT NOT NULL DEFAULT 0,
            total_completion_tokens BIGINT NOT NULL DEFAULT 0,
            total_tool_calls INT NOT NULL DEFAULT 0,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            completed_at DATETIME(6) NULL,
            INDEX idx_runs_user_status (user_id, status),
            INDEX idx_runs_user_updated (user_id, updated_at),
            INDEX idx_runs_session (session_id),
            INDEX idx_runs_parent (parent_run_id),
            INDEX idx_runs_delegation (delegation_id)
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
            INDEX idx_mm_active (is_active)
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
            change_type     VARCHAR(64) NOT NULL,
            change_id       VARCHAR(64) NOT NULL,
            sessions_tested INT NOT NULL DEFAULT 0,
            error_rate      DECIMAL(5,4),
            score_delta     DECIMAL(5,4),
            passed          SMALLINT NOT NULL DEFAULT 0,
            created_at      DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_egr_change (change_type, change_id),
            INDEX idx_egr_passed (passed)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS eval_quality_assessments (
            assessment_id VARCHAR(36) PRIMARY KEY,
            target_id     VARCHAR(64) NOT NULL,
            score         DECIMAL(5,4) NOT NULL,
            step_count    INT NOT NULL DEFAULT 0,
            level         VARCHAR(32) NOT NULL DEFAULT 'unknown',
            created_at    DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at    DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6),
            INDEX idx_eqa_target (target_id),
            INDEX idx_eqa_level (level)
        )",
    )
    .execute(&pool)
    .await?;

    query(
        "CREATE TABLE IF NOT EXISTS eval_user_feedback (
            feedback_id   VARCHAR(36) PRIMARY KEY,
            user_id       VARCHAR(36) NOT NULL,
            session_id    VARCHAR(36),
            turn_id       VARCHAR(36),
            feedback_type VARCHAR(64) NOT NULL,
            rating        INT,
            comment       TEXT,
            created_at    DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            INDEX idx_euf_user (user_id),
            INDEX idx_euf_session (session_id),
            INDEX idx_euf_type (feedback_type)
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
    /// Max age in days for expired auth tokens (default: 30)
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

    // 2. Expired or inactive auth tokens
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
        table: "auth_tokens (inactive)",
        rows_deleted: deleted,
    });

    // Also clean expired auth tokens (those with expires_at in the past)
    let deleted = sqlx::query(
        "DELETE FROM auth_tokens \
         WHERE expires_at IS NOT NULL \
           AND expires_at < DATE_SUB(NOW(6), INTERVAL ? DAY) \
         LIMIT ?",
    )
    .bind(policy.auth_token_days)
    .bind(BATCH_LIMIT)
    .execute(pool)
    .await
    .map(|r| r.rows_affected())
    .unwrap_or(0);
    results.push(CleanupResult {
        table: "auth_tokens (expired)",
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
    let deleted = sqlx::query(
        "DELETE FROM agent_events \
         WHERE created_at < DATE_SUB(NOW(6), INTERVAL ? DAY) \
         LIMIT ?",
    )
    .bind(policy.event_days)
    .bind(BATCH_LIMIT)
    .execute(pool)
    .await
    .map(|r| r.rows_affected())
    .unwrap_or(0);
    results.push(CleanupResult {
        table: "agent_events",
        rows_deleted: deleted,
    });

    results
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
