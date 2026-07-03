use std::{
    path::{Path, PathBuf},
    time::Instant,
};

use serde::Serialize;
use sqlx::{MySql, Pool, Row, query};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SessionDeleteStatement {
    label: &'static str,
    sql: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SessionBatchDeleteStatement {
    label: &'static str,
    sql: &'static str,
}

const SESSION_DELETE_BATCH_LIMIT: i64 = 1000;

const SESSION_DELETE_DERIVED_FROM_AGENT_RUNS: &[SessionDeleteStatement] =
    &[SessionDeleteStatement {
        label: "user_skill_evaluations",
        sql: "DELETE FROM user_skill_evaluations
             WHERE (owner_user_id, run_id) IN (
                 SELECT user_id, run_id FROM agent_runs
                 WHERE session_id = ? AND user_id = ?
             )",
    }];

const SESSION_DELETE_AGENT_EVENT_EDGES_SQL: &str = "DELETE FROM agent_event_edges
         WHERE session_id = ? AND user_id = ?
         ORDER BY child_event_id ASC, parent_event_id ASC, relation_kind ASC
         LIMIT ?";

const SESSION_DELETE_TASK_LEASES_SQL: &str = "DELETE FROM task_leases
         WHERE user_id = ?
           AND task_id IN (
               SELECT task_id FROM agent_tasks
               WHERE session_id = ? AND user_id = ?
           )";

const SESSION_DELETE_PLAN_STEP_RUNS_SQL: &str = "DELETE FROM plan_step_runs
         WHERE user_id = ?
           AND plan_id IN (
               SELECT plan_id FROM plans
               WHERE session_id = ? AND user_id = ?
           )";

const SESSION_DELETE_WORKSPACE_CLEANUP_DEBT_MESSAGE: &str =
    "session hard delete requested cloud workspace cleanup";

const SESSION_CLEAR_CONFIG_VERSION_FIRST_SEEN_SESSION_SQL: &str = "UPDATE config_versions \
     SET first_seen_session = NULL \
     WHERE user_id = ? AND first_seen_session = ?";

const SESSION_DELETE_SESSION_ORIGIN_TABLES: &[SessionDeleteStatement] = &[
    SessionDeleteStatement {
        label: "session_state_item_events",
        sql: "DELETE FROM session_state_item_events
             WHERE (session_id = ? AND user_id = ?)
                OR item_id IN (
                    SELECT item_id FROM session_state_items
                    WHERE origin_session_id = ? AND user_id = ?
                )",
    },
    SessionDeleteStatement {
        label: "session_state_items",
        sql: "DELETE FROM session_state_items
             WHERE (session_id = ? AND user_id = ?)
                OR (origin_session_id = ? AND user_id = ?)",
    },
    SessionDeleteStatement {
        label: "session_history_chunks",
        sql: "DELETE FROM session_history_chunks
             WHERE (session_id = ? AND user_id = ?)
                OR (source_session_id = ? AND user_id = ?)",
    },
];

const SESSION_DELETE_DERIVED_PARENT_TABLES: &[SessionDeleteStatement] = &[
    SessionDeleteStatement {
        label: "context_manifest_items",
        sql: "DELETE FROM context_manifest_items
             WHERE manifest_id IN (
                 SELECT manifest_id FROM context_manifests
                 WHERE session_id = ? AND user_id = ?
             )",
    },
    SessionDeleteStatement {
        label: "verification_results",
        sql: "DELETE FROM verification_results
             WHERE EXISTS (
                 SELECT 1 FROM task_contracts tc
                 WHERE tc.contract_id = verification_results.contract_id
                   AND tc.user_id = verification_results.user_id
                   AND tc.session_id = ?
                   AND tc.user_id = ?
             )",
    },
    SessionDeleteStatement {
        label: "harness_citations",
        sql: "DELETE FROM harness_citations
             WHERE harness_run_id IN (
                 SELECT harness_run_id FROM harness_runs
                 WHERE session_id = ? AND user_id = ?
             )",
    },
    SessionDeleteStatement {
        label: "harness_skill_rules",
        sql: "DELETE FROM harness_skill_rules
             WHERE harness_run_id IN (
                 SELECT harness_run_id FROM harness_runs
                 WHERE session_id = ? AND user_id = ?
             )",
    },
    SessionDeleteStatement {
        label: "harness_skill_drafts",
        sql: "DELETE FROM harness_skill_drafts
             WHERE harness_run_id IN (
                 SELECT harness_run_id FROM harness_runs
                 WHERE session_id = ? AND user_id = ?
             )",
    },
    SessionDeleteStatement {
        label: "harness_items",
        sql: "DELETE FROM harness_items
             WHERE harness_run_id IN (
                 SELECT harness_run_id FROM harness_runs
                 WHERE session_id = ? AND user_id = ?
             )",
    },
];

const SESSION_DELETE_DIRECT_TABLES: &[SessionDeleteStatement] = &[
    SessionDeleteStatement {
        label: "context_manifests",
        sql: "DELETE FROM context_manifests WHERE session_id = ? AND user_id = ?",
    },
    SessionDeleteStatement {
        label: "workspace_records",
        sql: "DELETE FROM workspace_records WHERE session_id = ? AND owner_id = ?",
    },
    SessionDeleteStatement {
        label: "session_artifacts_grants",
        sql: "DELETE FROM session_artifacts_grants WHERE session_id = ? AND user_id = ?",
    },
    SessionDeleteStatement {
        label: "session_artifacts",
        sql: "DELETE FROM session_artifacts WHERE session_id = ? AND user_id = ?",
    },
    SessionDeleteStatement {
        label: "session_device_lease_events",
        sql: "DELETE FROM session_device_lease_events WHERE session_id = ? AND user_id = ?",
    },
    SessionDeleteStatement {
        label: "session_device_leases",
        sql: "DELETE FROM session_device_leases WHERE session_id = ? AND user_id = ?",
    },
    SessionDeleteStatement {
        label: "session_transcript_items",
        sql: "DELETE FROM session_transcript_items WHERE session_id = ? AND user_id = ?",
    },
    SessionDeleteStatement {
        label: "transcript_pages",
        sql: "DELETE FROM transcript_pages WHERE session_id = ? AND user_id = ?",
    },
    SessionDeleteStatement {
        label: "ctx_snapshots",
        sql: "DELETE FROM ctx_snapshots WHERE session_id = ? AND user_id = ?",
    },
    SessionDeleteStatement {
        label: "ctx_decision_audits",
        sql: "DELETE FROM ctx_decision_audits WHERE session_id = ? AND user_id = ?",
    },
    SessionDeleteStatement {
        label: "session_state_revisions",
        sql: "DELETE FROM session_state_revisions WHERE session_id = ? AND user_id = ?",
    },
    SessionDeleteStatement {
        label: "session_delegations",
        sql: "DELETE FROM session_delegations WHERE session_id = ? AND user_id = ?",
    },
    SessionDeleteStatement {
        label: "session_todos",
        sql: "DELETE FROM session_todos WHERE session_id = ? AND user_id = ?",
    },
    SessionDeleteStatement {
        label: "session_todo_counters",
        sql: "DELETE FROM session_todo_counters WHERE session_id = ? AND user_id = ?",
    },
    SessionDeleteStatement {
        label: "session_todo_idempotency",
        sql: "DELETE FROM session_todo_idempotency WHERE session_id = ? AND user_id = ?",
    },
    SessionDeleteStatement {
        label: "session_plan_todos",
        sql: "DELETE FROM session_plan_todos WHERE session_id = ? AND user_id = ?",
    },
    SessionDeleteStatement {
        label: "harness_snapshots",
        sql: "DELETE FROM harness_snapshots WHERE session_id = ? AND user_id = ?",
    },
    SessionDeleteStatement {
        label: "harness_runs",
        sql: "DELETE FROM harness_runs WHERE session_id = ? AND user_id = ?",
    },
    SessionDeleteStatement {
        label: "skill_selection_events",
        sql: "DELETE FROM skill_selection_events WHERE session_id = ? AND user_id = ?",
    },
    SessionDeleteStatement {
        label: "agent_tasks",
        sql: "DELETE FROM agent_tasks WHERE session_id = ? AND user_id = ?",
    },
    SessionDeleteStatement {
        label: "plans",
        sql: "DELETE FROM plans WHERE session_id = ? AND user_id = ?",
    },
    SessionDeleteStatement {
        label: "session_checkpoints",
        sql: "DELETE FROM session_checkpoints WHERE session_id = ? AND user_id = ?",
    },
    SessionDeleteStatement {
        label: "task_contracts",
        sql: "DELETE FROM task_contracts WHERE session_id = ? AND user_id = ?",
    },
    SessionDeleteStatement {
        label: "skill_installations",
        sql: "DELETE FROM skill_installations WHERE session_id = ? AND user_id = ?",
    },
    SessionDeleteStatement {
        label: "wf_triggers",
        sql: "DELETE FROM wf_triggers WHERE session_id = ? AND user_id = ?",
    },
    SessionDeleteStatement {
        label: "eval_user_feedback",
        sql: "DELETE FROM eval_user_feedback WHERE session_id = ? AND user_id = ?",
    },
    SessionDeleteStatement {
        label: "eval_calibration_assessments",
        sql: "DELETE FROM eval_calibration_assessments WHERE session_id = ? AND user_id = ?",
    },
    SessionDeleteStatement {
        label: "team_snapshots",
        sql: "DELETE FROM team_snapshots WHERE session_id = ? AND user_id = ?",
    },
    SessionDeleteStatement {
        label: "tool_exactly_once_results",
        sql: "DELETE FROM tool_exactly_once_results WHERE session_id = ? AND user_id = ?",
    },
];

const SESSION_DELETE_DIRECT_BATCH_TABLES: &[SessionBatchDeleteStatement] = &[
    SessionBatchDeleteStatement {
        label: "session_tool_outputs",
        sql: "DELETE FROM session_tool_outputs
             WHERE session_id = ? AND user_id = ?
             ORDER BY created_at ASC, output_id ASC
             LIMIT ?",
    },
    SessionBatchDeleteStatement {
        label: "session_tool_output_batches",
        sql: "DELETE FROM session_tool_output_batches
             WHERE session_id = ? AND user_id = ?
             ORDER BY created_at ASC, batch_id ASC
             LIMIT ?",
    },
    SessionBatchDeleteStatement {
        label: "conversation_log",
        sql: "DELETE FROM conversation_log
             WHERE session_id = ? AND user_id = ?
             ORDER BY seq ASC
             LIMIT ?",
    },
    SessionBatchDeleteStatement {
        label: "prompt_deltas",
        sql: "DELETE FROM prompt_deltas
             WHERE session_id = ? AND user_id = ?
             ORDER BY request_id ASC, delta_seq ASC
             LIMIT ?",
    },
    SessionBatchDeleteStatement {
        label: "prompt_request_records",
        sql: "DELETE FROM prompt_request_records
             WHERE session_id = ? AND user_id = ?
             ORDER BY created_at ASC, request_id ASC
             LIMIT ?",
    },
];

const SESSION_DELETE_TERMINAL_BATCH_TABLES: &[SessionBatchDeleteStatement] = &[
    SessionBatchDeleteStatement {
        label: "agent_run_events",
        sql: "DELETE FROM agent_run_events
             WHERE session_id = ? AND user_id = ?
             ORDER BY run_id ASC, event_idx ASC, id ASC
             LIMIT ?",
    },
    SessionBatchDeleteStatement {
        label: "agent_events",
        sql: "DELETE FROM agent_events
             WHERE session_id = ? AND user_id = ?
             ORDER BY created_at ASC, event_id ASC
             LIMIT ?",
    },
];

const SESSION_DELETE_TERMINAL_TABLES: &[SessionDeleteStatement] = &[
    SessionDeleteStatement {
        label: "run_checkpoints",
        sql: "DELETE FROM run_checkpoints WHERE session_id = ? AND user_id = ?",
    },
    SessionDeleteStatement {
        label: "run_display_projections",
        sql: "DELETE FROM run_display_projections WHERE session_id = ? AND user_id = ?",
    },
    SessionDeleteStatement {
        label: "agent_runs",
        sql: "DELETE FROM agent_runs WHERE session_id = ? AND user_id = ?",
    },
    SessionDeleteStatement {
        label: "agent_sessions",
        sql: "DELETE FROM agent_sessions WHERE session_id = ? AND user_id = ?",
    },
    SessionDeleteStatement {
        label: "eval_quality_assessments",
        sql: "DELETE FROM eval_quality_assessments WHERE target_id = ? AND user_id = ? AND level = 'session'",
    },
];

const SESSION_DELETE_CORE_RESIDUAL_TABLES: &[&str] = &[
    "agent_sessions",
    "agent_events",
    "agent_event_edges",
    "agent_runs",
    "agent_tasks",
    "task_contracts",
];

async fn delete_session_rows_session_user(
    tx: &mut sqlx::Transaction<'_, MySql>,
    label: &'static str,
    statement: &'static str,
    session_id: &str,
    user_id: &str,
) -> Result<u64, String> {
    query(statement)
        .bind(session_id)
        .bind(user_id)
        .execute(&mut **tx)
        .await
        .map(|result| result.rows_affected())
        .map_err(|source| format!("delete_session.{label}: {source}"))
}

async fn delete_session_rows_session_user_batched(
    tx: &mut sqlx::Transaction<'_, MySql>,
    label: &'static str,
    statement: &'static str,
    session_id: &str,
    user_id: &str,
) -> Result<u64, String> {
    let mut total_deleted = 0_u64;
    loop {
        let rows_deleted = query(statement)
            .bind(session_id)
            .bind(user_id)
            .bind(SESSION_DELETE_BATCH_LIMIT)
            .execute(&mut **tx)
            .await
            .map(|result| result.rows_affected())
            .map_err(|source| format!("delete_session.{label}: {source}"))?;
        total_deleted = total_deleted
            .checked_add(rows_deleted)
            .ok_or_else(|| format!("delete_session.{label}: deleted row total overflow"))?;
        if rows_deleted == 0 {
            break;
        }
    }
    Ok(total_deleted)
}

async fn delete_session_rows_session_user_twice(
    tx: &mut sqlx::Transaction<'_, MySql>,
    label: &'static str,
    statement: &'static str,
    session_id: &str,
    user_id: &str,
) -> Result<u64, String> {
    query(statement)
        .bind(session_id)
        .bind(user_id)
        .bind(session_id)
        .bind(user_id)
        .execute(&mut **tx)
        .await
        .map(|result| result.rows_affected())
        .map_err(|source| format!("delete_session.{label}: {source}"))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct SessionTableDeleteOutcome {
    pub label: &'static str,
    pub rows_deleted: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SessionDatabaseDeleteOutcome {
    pub rows_deleted: u64,
    pub session_references_cleared: u64,
    pub workspace_cleanup_debts_enqueued: u64,
    pub tables: Vec<SessionTableDeleteOutcome>,
}

fn record_table_delete(
    outcome: &mut SessionDatabaseDeleteOutcome,
    label: &'static str,
    rows_deleted: u64,
) -> Result<(), String> {
    outcome.rows_deleted = outcome
        .rows_deleted
        .checked_add(rows_deleted)
        .ok_or_else(|| format!("delete_session.{label}: deleted row total overflow"))?;
    outcome.tables.push(SessionTableDeleteOutcome {
        label,
        rows_deleted,
    });
    Ok(())
}

async fn verify_core_session_tables_deleted(
    tx: &mut sqlx::Transaction<'_, MySql>,
    session_id: &str,
    user_id: &str,
) -> Result<(), String> {
    for table in SESSION_DELETE_CORE_RESIDUAL_TABLES {
        let sql = format!(
            "SELECT COUNT(*) FROM {} WHERE session_id = ? AND user_id = ?",
            crate::snapshot_sql::quote_mysql_identifier(table)
        );
        let remaining: i64 = sqlx::query_scalar(&sql)
            .bind(session_id)
            .bind(user_id)
            .fetch_one(&mut **tx)
            .await
            .map_err(|source| format!("delete_session.verify.{table}: {source}"))?;
        if remaining > 0 {
            return Err(format!(
                "delete_session.verify.{table}: {remaining} rows remain for session/user after delete"
            ));
        }
    }
    Ok(())
}

async fn enqueue_workspace_cleanup_debts_for_session_delete(
    tx: &mut sqlx::Transaction<'_, MySql>,
    session_id: &str,
    user_id: &str,
) -> Result<u64, String> {
    let rows = query(
        "SELECT workspace_id, run_id, CAST(record_json AS CHAR) AS record_json \
         FROM workspace_records \
         WHERE session_id = ? \
           AND owner_id = ? \
           AND kind = 'cloud_workspace' \
           AND persistence IN ('ephemeral', 'session')",
    )
    .bind(session_id)
    .bind(user_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(|source| format!("delete_session.workspace_records.select_cleanup_debts: {source}"))?;

    let mut enqueued = 0_u64;
    for row in rows {
        let workspace_id: String = row
            .try_get("workspace_id")
            .map_err(|source| format!("delete_session.workspace_records.workspace_id: {source}"))?;
        let run_id: Option<String> = row
            .try_get("run_id")
            .map_err(|source| format!("delete_session.workspace_records.run_id: {source}"))?;
        let record_json: String = row
            .try_get("record_json")
            .map_err(|source| format!("delete_session.workspace_records.record_json: {source}"))?;
        let debt_id = format!("session-delete-{}", uuid::Uuid::now_v7());
        let result = query(
            "INSERT INTO workspace_cleanup_debts \
             (debt_id, owner_id, session_id, run_id, workspace_id, reason, message, attempts, \
              record_json, created_at, updated_at, resolved_at) \
             SELECT ?, ?, ?, ?, ?, 'operator_requested', ?, 0, ?, NOW(6), NOW(6), NULL \
             FROM DUAL \
             WHERE NOT EXISTS ( \
                 SELECT 1 FROM workspace_cleanup_debts \
                 WHERE owner_id = ? AND workspace_id = ? AND resolved_at IS NULL \
                 LIMIT 1 \
             )",
        )
        .bind(debt_id)
        .bind(user_id)
        .bind(session_id)
        .bind(run_id.as_deref())
        .bind(&workspace_id)
        .bind(SESSION_DELETE_WORKSPACE_CLEANUP_DEBT_MESSAGE)
        .bind(record_json)
        .bind(user_id)
        .bind(&workspace_id)
        .execute(&mut **tx)
        .await
        .map_err(|source| {
            format!("delete_session.workspace_cleanup_debts.enqueue {workspace_id}: {source}")
        })?;
        enqueued = enqueued
            .checked_add(result.rows_affected())
            .ok_or_else(|| {
                "delete_session.workspace_cleanup_debts: enqueued debt total overflow".to_string()
            })?;
    }

    Ok(enqueued)
}

async fn clear_session_provenance_references(
    tx: &mut sqlx::Transaction<'_, MySql>,
    session_id: &str,
    user_id: &str,
) -> Result<u64, String> {
    query(SESSION_CLEAR_CONFIG_VERSION_FIRST_SEEN_SESSION_SQL)
        .bind(user_id)
        .bind(session_id)
        .execute(&mut **tx)
        .await
        .map(|result| result.rows_affected())
        .map_err(|source| format!("delete_session.config_versions.first_seen_session: {source}"))
}

pub(crate) async fn hard_delete_session_rows(
    tx: &mut sqlx::Transaction<'_, MySql>,
    session_id: &str,
    user_id: &str,
) -> Result<SessionDatabaseDeleteOutcome, String> {
    let mut outcome = SessionDatabaseDeleteOutcome::default();

    for statement in SESSION_DELETE_DERIVED_FROM_AGENT_RUNS {
        let rows_deleted = delete_session_rows_session_user(
            tx,
            statement.label,
            statement.sql,
            session_id,
            user_id,
        )
        .await?;
        record_table_delete(&mut outcome, statement.label, rows_deleted)?;
    }

    let rows_deleted = delete_session_rows_session_user_batched(
        tx,
        "agent_event_edges",
        SESSION_DELETE_AGENT_EVENT_EDGES_SQL,
        session_id,
        user_id,
    )
    .await?;
    record_table_delete(&mut outcome, "agent_event_edges", rows_deleted)?;

    for statement in SESSION_DELETE_SESSION_ORIGIN_TABLES {
        let rows_deleted = delete_session_rows_session_user_twice(
            tx,
            statement.label,
            statement.sql,
            session_id,
            user_id,
        )
        .await?;
        record_table_delete(&mut outcome, statement.label, rows_deleted)?;
    }

    let rows_deleted = query(SESSION_DELETE_TASK_LEASES_SQL)
        .bind(user_id)
        .bind(session_id)
        .bind(user_id)
        .execute(&mut **tx)
        .await
        .map(|result| result.rows_affected())
        .map_err(|source| format!("delete_session.task_leases: {source}"))?;
    record_table_delete(&mut outcome, "task_leases", rows_deleted)?;

    let rows_deleted = query(SESSION_DELETE_PLAN_STEP_RUNS_SQL)
        .bind(user_id)
        .bind(session_id)
        .bind(user_id)
        .execute(&mut **tx)
        .await
        .map(|result| result.rows_affected())
        .map_err(|source| format!("delete_session.plan_step_runs: {source}"))?;
    record_table_delete(&mut outcome, "plan_step_runs", rows_deleted)?;

    for statement in SESSION_DELETE_DERIVED_PARENT_TABLES {
        let rows_deleted = delete_session_rows_session_user(
            tx,
            statement.label,
            statement.sql,
            session_id,
            user_id,
        )
        .await?;
        record_table_delete(&mut outcome, statement.label, rows_deleted)?;
    }

    outcome.workspace_cleanup_debts_enqueued =
        enqueue_workspace_cleanup_debts_for_session_delete(tx, session_id, user_id).await?;
    outcome.session_references_cleared =
        clear_session_provenance_references(tx, session_id, user_id).await?;

    for statement in SESSION_DELETE_DIRECT_BATCH_TABLES {
        let rows_deleted = delete_session_rows_session_user_batched(
            tx,
            statement.label,
            statement.sql,
            session_id,
            user_id,
        )
        .await?;
        record_table_delete(&mut outcome, statement.label, rows_deleted)?;
    }

    for statement in SESSION_DELETE_DIRECT_TABLES {
        let rows_deleted = delete_session_rows_session_user(
            tx,
            statement.label,
            statement.sql,
            session_id,
            user_id,
        )
        .await?;
        record_table_delete(&mut outcome, statement.label, rows_deleted)?;
    }

    for statement in SESSION_DELETE_TERMINAL_BATCH_TABLES {
        let rows_deleted = delete_session_rows_session_user_batched(
            tx,
            statement.label,
            statement.sql,
            session_id,
            user_id,
        )
        .await?;
        record_table_delete(&mut outcome, statement.label, rows_deleted)?;
    }

    for statement in SESSION_DELETE_TERMINAL_TABLES {
        let rows_deleted = delete_session_rows_session_user(
            tx,
            statement.label,
            statement.sql,
            session_id,
            user_id,
        )
        .await?;
        record_table_delete(&mut outcome, statement.label, rows_deleted)?;
    }

    verify_core_session_tables_deleted(tx, session_id, user_id).await?;

    Ok(outcome)
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SessionHardDeleteOutcome {
    pub database_rows_deleted: u64,
    pub session_references_cleared: u64,
    pub workspace_cleanup_debts_enqueued: u64,
    pub database_tables_deleted: Vec<SessionTableDeleteOutcome>,
    pub local_bytes_freed: u64,
    pub workspaces_removed: u64,
    pub database_delete_ms: u64,
    pub local_artifact_delete_ms: u64,
    pub workspace_delete_ms: u64,
    pub total_delete_ms: u64,
    pub cleanup_errors: Vec<String>,
}

async fn mark_session_deleting(
    pool: &Pool<MySql>,
    session_id: &str,
    user_id: &str,
) -> Result<(), String> {
    let result = query(
        "UPDATE agent_sessions \
         SET status = 'deleting', \
             ended_at = COALESCE(ended_at, CURRENT_TIMESTAMP(6)), \
             updated_at = CURRENT_TIMESTAMP(6) \
         WHERE session_id = ? AND user_id = ?",
    )
    .bind(session_id)
    .bind(user_id)
    .execute(pool)
    .await
    .map_err(|source| format!("delete_session.mark_deleting: {source}"))?;

    if result.rows_affected() == 0 {
        let still_exists =
            query("SELECT 1 FROM agent_sessions WHERE session_id = ? AND user_id = ? LIMIT 1")
                .bind(session_id)
                .bind(user_id)
                .fetch_optional(pool)
                .await
                .map_err(|source| format!("delete_session.mark_deleting.confirm: {source}"))?
                .is_some();
        if still_exists {
            return Ok(());
        }
        return Err(
            "delete_session.mark_deleting: session not found or not owned by user".to_string(),
        );
    }

    Ok(())
}

pub(crate) async fn hard_delete_session(
    pool: &Pool<MySql>,
    session_id: &str,
    user_id: &str,
) -> Result<SessionHardDeleteOutcome, String> {
    let total_start = Instant::now();

    // Phase 0: Persist delete intent before destructive cleanup. The session
    // row remains until the database transaction commits, so a crash before
    // DB deletion is visible and retryable.
    mark_session_deleting(pool, session_id, user_id).await?;

    // Phase 1: DB transaction. Do not delete local files before commit: if
    // commit fails, the database still owns the session and must retain its
    // local state for retry/repair.
    let database_start = Instant::now();
    let mut tx = pool
        .begin()
        .await
        .map_err(|source| format!("delete_session.begin_transaction: {source}"))?;
    let database_delete = hard_delete_session_rows(&mut tx, session_id, user_id).await?;
    tx.commit()
        .await
        .map_err(|source| format!("delete_session.commit: {source}"))?;
    let database_delete_ms = elapsed_ms(database_start);

    // Phase 2: Post-commit local cleanup. At this point DB state is gone, so a
    // local cleanup failure is a resource leak to surface, not a DB/file split
    // brain that can make an existing session unreadable.
    let mut cleanup_errors = Vec::new();
    let local_start = Instant::now();
    let local_bytes_freed = match delete_owner_bound_local_session_artifacts(user_id, session_id) {
        Ok(bytes) => bytes,
        Err(error) => {
            let error = format!("delete_session.post_commit_file_cleanup: {error}");
            tracing::warn!(
                target: "astra_services::session_lifecycle",
                %session_id,
                %user_id,
                %error,
                "session hard delete local cleanup failed after database commit"
            );
            cleanup_errors.push(error);
            0
        }
    };
    let local_artifact_delete_ms = elapsed_ms(local_start);

    let workspace_start = Instant::now();
    let workspaces_removed = match delete_server_workspace(session_id) {
        Ok(removed) => u64::from(removed),
        Err(error) => {
            let error = format!("delete_session.post_commit_workspace_cleanup: {error}");
            tracing::warn!(
                target: "astra_services::session_lifecycle",
                %session_id,
                %user_id,
                %error,
                "session hard delete workspace cleanup failed after database commit"
            );
            cleanup_errors.push(error);
            0
        }
    };
    let workspace_delete_ms = elapsed_ms(workspace_start);

    let mut outcome = SessionHardDeleteOutcome {
        database_rows_deleted: database_delete.rows_deleted,
        session_references_cleared: database_delete.session_references_cleared,
        workspace_cleanup_debts_enqueued: database_delete.workspace_cleanup_debts_enqueued,
        database_tables_deleted: database_delete.tables,
        database_delete_ms,
        local_bytes_freed,
        workspaces_removed,
        local_artifact_delete_ms,
        workspace_delete_ms,
        cleanup_errors,
        ..SessionHardDeleteOutcome::default()
    };
    outcome.total_delete_ms = elapsed_ms(total_start);
    Ok(outcome)
}

fn elapsed_ms(start: Instant) -> u64 {
    start.elapsed().as_millis().min(u64::MAX as u128) as u64
}

fn delete_owner_bound_local_session_artifacts(
    user_id: &str,
    session_id: &str,
) -> Result<u64, String> {
    crate::session_journal::delete_session_for_user(user_id, session_id)
        .map_err(|source| format!("delete_session.local_files: {source}"))
}

fn delete_server_workspace(session_id: &str) -> Result<bool, String> {
    delete_server_workspace_under(&server_workspace_base(), session_id)
}

fn delete_server_workspace_under(base: &Path, session_id: &str) -> Result<bool, String> {
    let Some(path) = server_workspace_path_under(base, session_id) else {
        return Ok(false);
    };
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(source) => {
            return Err(format!(
                "delete_session.server_workspace.metadata: {source}"
            ));
        }
    };
    if metadata.file_type().is_symlink() {
        return std::fs::remove_file(&path)
            .map(|()| true)
            .map_err(|source| format!("delete_session.server_workspace.symlink: {source}"));
    }
    if !metadata.is_dir() {
        return Err("delete_session.server_workspace: path is not a directory".to_string());
    }
    std::fs::remove_dir_all(&path)
        .map(|()| true)
        .map_err(|source| format!("delete_session.server_workspace: {source}"))
}

fn server_workspace_base() -> PathBuf {
    std::env::var("ASTRA_SERVER_WORKSPACES")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("astra-workspaces"))
}

fn server_workspace_path_under(base: &Path, session_id: &str) -> Option<PathBuf> {
    if is_safe_workspace_id(session_id) {
        Some(base.join(session_id))
    } else {
        None
    }
}

fn is_safe_workspace_id(workspace_id: &str) -> bool {
    !workspace_id.is_empty()
        && workspace_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionArtifactStore;
    use std::collections::BTreeSet;

    fn session_delete_labels() -> BTreeSet<String> {
        let mut labels = BTreeSet::new();
        labels.insert("agent_event_edges".to_string());
        labels.insert("task_leases".to_string());
        labels.insert("plan_step_runs".to_string());
        for group in [
            SESSION_DELETE_DERIVED_FROM_AGENT_RUNS,
            SESSION_DELETE_SESSION_ORIGIN_TABLES,
            SESSION_DELETE_DERIVED_PARENT_TABLES,
            SESSION_DELETE_DIRECT_TABLES,
            SESSION_DELETE_TERMINAL_TABLES,
        ] {
            for statement in group {
                assert!(
                    labels.insert(statement.label.to_string()),
                    "duplicate session delete table label: {}",
                    statement.label
                );
            }
        }
        for group in [
            SESSION_DELETE_DIRECT_BATCH_TABLES,
            SESSION_DELETE_TERMINAL_BATCH_TABLES,
        ] {
            for statement in group {
                assert!(
                    labels.insert(statement.label.to_string()),
                    "duplicate session delete table label: {}",
                    statement.label
                );
            }
        }
        labels
    }

    fn ddl_tables_with_session_id(source: &str) -> BTreeSet<String> {
        source
            .split("CREATE TABLE IF NOT EXISTS ")
            .skip(1)
            .filter_map(|section| {
                let table = section
                    .split_whitespace()
                    .next()?
                    .trim_matches('`')
                    .trim_end_matches('(');
                section.contains("session_id").then(|| table.to_string())
            })
            .collect()
    }

    fn production_source(source: &'static str) -> &'static str {
        source.split("#[cfg(test)]").next().unwrap_or(source)
    }

    fn session_lifecycle_schema_tables_with_session_id() -> BTreeSet<String> {
        let mut tables = ddl_tables_with_session_id(production_source(include_str!("storage.rs")));
        tables.extend(ddl_tables_with_session_id(production_source(include_str!(
            "workspace_records.rs"
        ))));
        tables
    }

    #[test]
    fn server_workspace_path_requires_exact_safe_session_component() {
        let base = Path::new("/tmp/astra-workspaces");
        assert_eq!(
            server_workspace_path_under(base, "session-123_abc"),
            Some(base.join("session-123_abc"))
        );

        for unsafe_id in [
            "",
            "../session-123",
            "session/123",
            "session.123",
            " session",
        ] {
            assert_eq!(
                server_workspace_path_under(base, unsafe_id),
                None,
                "unsafe workspace id must not be sanitized into another deletable path: {unsafe_id:?}"
            );
        }
    }

    #[test]
    fn session_database_delete_outcome_records_table_rows_and_total() {
        let mut outcome = SessionDatabaseDeleteOutcome::default();
        record_table_delete(&mut outcome, "agent_events", 2).expect("record agent events");
        record_table_delete(&mut outcome, "agent_sessions", 1).expect("record agent session");

        assert_eq!(outcome.rows_deleted, 3);
        assert_eq!(
            outcome.tables,
            vec![
                SessionTableDeleteOutcome {
                    label: "agent_events",
                    rows_deleted: 2
                },
                SessionTableDeleteOutcome {
                    label: "agent_sessions",
                    rows_deleted: 1
                },
            ]
        );
    }

    #[test]
    fn session_database_delete_outcome_fails_loudly_on_row_total_overflow() {
        let mut outcome = SessionDatabaseDeleteOutcome {
            rows_deleted: u64::MAX,
            session_references_cleared: 0,
            workspace_cleanup_debts_enqueued: 0,
            tables: Vec::new(),
        };
        let err = record_table_delete(&mut outcome, "agent_events", 1)
            .expect_err("overflow must fail loudly");

        assert!(
            err.contains("delete_session.agent_events") && err.contains("overflow"),
            "error should identify the overflowing delete statement: {err}"
        );
        assert!(outcome.tables.is_empty());
    }

    #[test]
    fn session_delete_residual_verification_covers_core_tables() {
        assert_eq!(
            SESSION_DELETE_CORE_RESIDUAL_TABLES,
            [
                "agent_sessions",
                "agent_events",
                "agent_event_edges",
                "agent_runs",
                "agent_tasks",
                "task_contracts",
            ]
        );
    }

    #[test]
    fn hard_delete_session_rows_verifies_core_tables_before_success() {
        let source = include_str!("session_lifecycle.rs");
        assert!(
            source.contains("verify_core_session_tables_deleted(tx, session_id, user_id).await?"),
            "hard delete must verify core table residuals before reporting success"
        );
        assert!(
            source.contains("rows remain for session/user after delete"),
            "residual verification must fail loudly instead of only auditing rows_affected"
        );
    }

    #[test]
    fn high_growth_session_deletes_are_ordered_batched_and_owner_scoped() {
        let mut statements: Vec<SessionBatchDeleteStatement> = vec![SessionBatchDeleteStatement {
            label: "agent_event_edges",
            sql: SESSION_DELETE_AGENT_EVENT_EDGES_SQL,
        }];
        statements.extend_from_slice(SESSION_DELETE_DIRECT_BATCH_TABLES);
        statements.extend_from_slice(SESSION_DELETE_TERMINAL_BATCH_TABLES);

        let labels = statements
            .iter()
            .map(|statement| statement.label)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            labels,
            BTreeSet::from([
                "agent_event_edges",
                "agent_events",
                "agent_run_events",
                "conversation_log",
                "prompt_deltas",
                "prompt_request_records",
                "session_tool_output_batches",
                "session_tool_outputs",
            ])
        );
        for statement in statements {
            let normalized = statement
                .sql
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            assert!(
                normalized.contains("session_id = ? AND user_id = ?"),
                "{} must be scoped by session and owner",
                statement.label
            );
            assert!(
                normalized.contains(" ORDER BY "),
                "{} must delete in a deterministic order",
                statement.label
            );
            assert!(
                normalized.ends_with("LIMIT ?"),
                "{} must be bounded by the shared batch limit",
                statement.label
            );
        }
        assert!(SESSION_DELETE_BATCH_LIMIT > 0);
        assert!(SESSION_DELETE_BATCH_LIMIT <= 10_000);
    }

    #[test]
    fn prompt_session_hard_delete_keeps_child_before_parent_in_batched_path() {
        let labels = SESSION_DELETE_DIRECT_BATCH_TABLES
            .iter()
            .map(|statement| statement.label)
            .collect::<Vec<_>>();
        let child = labels
            .iter()
            .position(|label| *label == "prompt_deltas")
            .expect("prompt_deltas must use batched session hard delete");
        let parent = labels
            .iter()
            .position(|label| *label == "prompt_request_records")
            .expect("prompt_request_records must use batched session hard delete");

        assert!(
            child < parent,
            "prompt_deltas must be pruned before prompt_request_records to preserve parent-bound cleanup"
        );
        assert!(
            !SESSION_DELETE_DIRECT_TABLES
                .iter()
                .any(|statement| statement.label == "prompt_deltas"
                    || statement.label == "prompt_request_records"),
            "prompt high-growth tables must not regress to unbounded direct DELETE statements"
        );
    }

    #[test]
    fn high_growth_session_delete_helper_loops_until_empty_batch() {
        let source = include_str!("session_lifecycle.rs");
        let helper_body = source
            .split("async fn delete_session_rows_session_user_batched")
            .nth(1)
            .and_then(|rest| {
                rest.split("async fn delete_session_rows_session_user_twice")
                    .next()
            })
            .expect("batched delete helper body");
        assert!(
            helper_body.contains("loop {") && helper_body.contains("if rows_deleted == 0"),
            "batched delete helper must keep pruning until a batch deletes no rows"
        );
        assert!(
            helper_body.contains("SESSION_DELETE_BATCH_LIMIT"),
            "batched delete helper must bind the shared batch limit"
        );
        assert!(
            helper_body.contains("checked_add(rows_deleted)"),
            "batched delete helper must fail loudly on impossible row-total overflow"
        );
    }

    #[test]
    fn hard_delete_session_wraps_database_deletes_in_transaction() {
        let source = include_str!("session_lifecycle.rs");
        let hard_delete_body = source
            .split("pub(crate) async fn hard_delete_session(")
            .nth(1)
            .and_then(|rest| rest.split("fn elapsed_ms(").next())
            .expect("hard_delete_session body");
        assert!(
            hard_delete_body.contains(".begin()"),
            "hard delete must open a database transaction before table deletes"
        );
        assert!(
            hard_delete_body
                .contains("hard_delete_session_rows(&mut tx, session_id, user_id).await?"),
            "hard delete must execute table deletes through the transaction"
        );
        assert!(
            hard_delete_body.contains("tx.commit()"),
            "hard delete must commit only after all table deletes and residual checks pass"
        );
        let commit = hard_delete_body
            .find("tx.commit()")
            .expect("hard delete must commit database transaction");
        let local_cleanup = hard_delete_body
            .find("delete_owner_bound_local_session_artifacts(user_id, session_id)")
            .expect("hard delete must clean local artifacts");
        let workspace_cleanup = hard_delete_body
            .find("delete_server_workspace(session_id)")
            .expect("hard delete must clean server workspace");
        assert!(
            commit < local_cleanup && commit < workspace_cleanup,
            "hard delete must delete files only after the database commit succeeds"
        );
    }

    #[test]
    fn post_commit_cleanup_failures_are_reported_in_outcome() {
        let source = include_str!("session_lifecycle.rs");
        let hard_delete_body = source
            .split("pub(crate) async fn hard_delete_session(")
            .nth(1)
            .and_then(|rest| rest.split("fn elapsed_ms(").next())
            .expect("hard_delete_session body");

        assert!(
            hard_delete_body.contains("cleanup_errors.push(error)"),
            "post-commit cleanup failures must be surfaced through the delete outcome"
        );
        assert!(
            !hard_delete_body.contains("return Err(format!(\"delete_session.post_commit"),
            "post-commit cleanup failures must not masquerade as database delete failures"
        );
    }

    #[test]
    fn server_workspace_cleanup_uses_local_safe_id_check() {
        let source = include_str!("session_lifecycle.rs");
        assert!(
            !source.contains(concat!("astra_runtime_env", "::", "validate_workspace_id")),
            "session_lifecycle must not depend on runtime-env for simple workspace id validation"
        );
        assert!(is_safe_workspace_id("session-123"));
        assert!(is_safe_workspace_id("session_123"));
        assert!(!is_safe_workspace_id(""));
        assert!(!is_safe_workspace_id("session/123"));
        assert!(!is_safe_workspace_id("session.123"));
    }

    #[test]
    fn server_workspace_cleanup_removes_only_exact_safe_component() {
        let base = tempfile::tempdir().expect("workspace base");
        let safe_workspace = base.path().join("session-123");
        let sanitize_collision = base.path().join("session123");
        std::fs::create_dir_all(&safe_workspace).expect("create safe workspace");
        std::fs::create_dir_all(&sanitize_collision).expect("create collision workspace");

        assert!(
            delete_server_workspace_under(base.path(), "session-123").expect("delete workspace")
        );
        assert!(!safe_workspace.exists());

        assert!(
            !delete_server_workspace_under(base.path(), "session/123")
                .expect("unsafe id is skipped")
        );
        assert!(
            sanitize_collision.exists(),
            "unsafe id must not be sanitized into a different workspace name"
        );
    }

    #[cfg(unix)]
    #[test]
    fn server_workspace_cleanup_removes_symlink_without_following_target() {
        let base = tempfile::tempdir().expect("workspace base");
        let target = tempfile::tempdir().expect("workspace target");
        let target_marker = target.path().join("must-survive.txt");
        std::fs::write(&target_marker, "target").expect("write target marker");
        let link = base.path().join("session-123");
        std::os::unix::fs::symlink(target.path(), &link).expect("create workspace symlink");

        assert!(delete_server_workspace_under(base.path(), "session-123").expect("delete link"));
        assert!(!link.exists(), "cleanup must remove the workspace link");
        assert!(
            target_marker.exists(),
            "cleanup must not recurse into a symlink target"
        );
    }

    #[test]
    fn owner_bound_local_cleanup_preserves_same_session_id_for_other_owner() {
        let sessions = tempfile::tempdir().expect("sessions tempdir");
        let _guard = crate::session_journal::JournalDirGuard::new(sessions.path());
        let session_id = format!("lifecycle-local-{}", uuid::Uuid::new_v4());

        for (user_id, marker) in [("owner-a", "a"), ("owner-b", "b")] {
            let journal = crate::session_journal::journal_file_path_for_user(user_id, &session_id)
                .expect("journal path");
            std::fs::create_dir_all(journal.parent().expect("journal parent"))
                .expect("create journal parent");
            std::fs::write(&journal, format!("{{\"owner\":\"{marker}\"}}\n"))
                .expect("write journal");
            let owner = crate::OwnerScope::user(user_id).expect("owner scope");
            let session_dir = crate::local_session_artifact_store()
                .session_dir_for_owner(&owner, &session_id)
                .expect("session dir");
            std::fs::create_dir_all(&session_dir).expect("create session dir");
            std::fs::write(session_dir.join("artifact.txt"), marker).expect("write artifact");
        }

        let user_a_journal =
            crate::session_journal::journal_file_path_for_user("owner-a", &session_id)
                .expect("user a journal");
        let user_b_journal =
            crate::session_journal::journal_file_path_for_user("owner-b", &session_id)
                .expect("user b journal");
        let user_a_dir = crate::local_session_artifact_store()
            .session_dir_for_owner(
                &crate::OwnerScope::user("owner-a").expect("owner a"),
                &session_id,
            )
            .expect("user a dir");
        let user_b_dir = crate::local_session_artifact_store()
            .session_dir_for_owner(
                &crate::OwnerScope::user("owner-b").expect("owner b"),
                &session_id,
            )
            .expect("user b dir");

        let freed =
            delete_owner_bound_local_session_artifacts("owner-a", &session_id).expect("cleanup");
        assert!(freed > 0, "owner cleanup must report freed bytes");
        assert!(!user_a_journal.exists());
        assert!(!user_a_dir.exists());
        assert!(
            user_b_journal.exists(),
            "foreign owner journal must survive"
        );
        assert!(
            user_b_dir.exists(),
            "foreign owner artifact dir must survive"
        );
    }

    #[test]
    fn session_hard_delete_marks_deleting_before_local_cleanup() {
        let source = include_str!("session_lifecycle.rs");
        let mark = source
            .find("mark_session_deleting(pool, session_id, user_id).await?")
            .expect("hard delete must mark the session deleting first");
        let local_cleanup = source
            .find("delete_owner_bound_local_session_artifacts(user_id, session_id)")
            .expect("hard delete must still clean local artifacts");

        assert!(
            mark < local_cleanup,
            "session hard delete must persist delete intent before local artifact deletion"
        );
        assert!(
            source.contains("SET status = 'deleting',"),
            "mark phase must use an observable deleting status"
        );
        assert!(
            source.contains("ended_at = COALESCE(ended_at, CURRENT_TIMESTAMP(6))"),
            "mark phase must persist a retry timestamp for the reaper"
        );
        assert!(
            source.contains("delete_session.mark_deleting.confirm"),
            "mark phase must be idempotent for already-marked retry"
        );
    }

    #[test]
    fn session_delete_inventory_covers_session_lifecycle_tables() {
        let labels = session_delete_labels();
        let schema_session_tables = session_lifecycle_schema_tables_with_session_id();
        let intentionally_retained_session_tables = BTreeSet::from([
            // Auth session identifiers model login/provider sessions, not
            // agent-run lifecycle rows. Deleting an agent session must not revoke
            // unrelated user authentication state.
            "auth_external_sessions".to_string(),
            "auth_refresh_tokens".to_string(),
            // This is an operational repair queue. Unresolved cleanup debts must
            // survive session deletion until the retry worker settles the external resource.
            "workspace_cleanup_debts".to_string(),
        ]);
        let missing = schema_session_tables
            .difference(&labels)
            .filter(|table| !intentionally_retained_session_tables.contains(*table))
            .cloned()
            .collect::<Vec<_>>();
        assert!(
            missing.is_empty(),
            "session delete inventory must cover every storage table with session_id; missing: {missing:?}"
        );

        let allowed_derived_tables = BTreeSet::from([
            "eval_quality_assessments".to_string(),
            "harness_citations".to_string(),
            "harness_items".to_string(),
            "harness_skill_drafts".to_string(),
            "harness_skill_rules".to_string(),
            "task_leases".to_string(),
            "user_skill_evaluations".to_string(),
        ]);
        let stale_labels = labels
            .difference(&schema_session_tables)
            .filter(|label| !allowed_derived_tables.contains(*label))
            .cloned()
            .collect::<Vec<_>>();
        assert!(
            stale_labels.is_empty(),
            "session delete inventory must not keep stale table labels; unexpected extras: {stale_labels:?}"
        );

        for expected in [
            "session_todo_idempotency",
            "verification_results",
            "harness_citations",
            "harness_skill_rules",
            "harness_skill_drafts",
            "harness_items",
            "harness_runs",
            "eval_quality_assessments",
            "eval_calibration_assessments",
            "session_checkpoints",
            "session_artifacts",
            "workspace_records",
            "agent_sessions",
        ] {
            assert!(
                labels.contains(expected),
                "session delete inventory must include {expected}"
            );
        }
        assert!(
            !labels.contains("skill_selector_turn_metrics"),
            "removed tables must not remain in the executable delete inventory"
        );
        assert!(
            !labels.contains("task_verification_results"),
            "obsolete verification table name must not remain in the executable delete inventory"
        );
        assert!(
            !labels.contains("workspace_cleanup_debts"),
            "unresolved cleanup debts must remain available to the workspace cleanup retry worker"
        );
    }

    #[test]
    fn session_delete_statements_are_owner_scoped() {
        for group in [
            SESSION_DELETE_DERIVED_FROM_AGENT_RUNS,
            SESSION_DELETE_SESSION_ORIGIN_TABLES,
            SESSION_DELETE_DERIVED_PARENT_TABLES,
            SESSION_DELETE_DIRECT_TABLES,
            SESSION_DELETE_TERMINAL_TABLES,
        ] {
            for statement in group {
                let normalized = statement
                    .sql
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                let session_owner_scoped = normalized.contains("session_id = ? AND user_id = ?")
                    || normalized.contains("session_id = ? AND owner_id = ?");
                let session_quality_scoped = statement.label == "eval_quality_assessments"
                    && normalized.contains("target_id = ? AND user_id = ?")
                    && normalized.contains("level = 'session'");
                let verification_results_scoped = statement.label == "verification_results"
                    && normalized.contains("WHERE EXISTS")
                    && normalized.contains("tc.contract_id = verification_results.contract_id")
                    && normalized.contains("tc.user_id = verification_results.user_id")
                    && normalized.contains("tc.session_id = ?")
                    && normalized.contains("tc.user_id = ?");
                assert!(
                    session_owner_scoped || session_quality_scoped || verification_results_scoped,
                    "{} must be scoped by session identity and its owner",
                    statement.label
                );
            }
        }
        for group in [
            SESSION_DELETE_DIRECT_BATCH_TABLES,
            SESSION_DELETE_TERMINAL_BATCH_TABLES,
        ] {
            for statement in group {
                let normalized = statement
                    .sql
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                assert!(
                    normalized.contains("session_id = ? AND user_id = ?"),
                    "{} must be scoped by session identity and its owner",
                    statement.label
                );
            }
        }
        assert!(
            SESSION_DELETE_AGENT_EVENT_EDGES_SQL
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .contains("session_id = ? AND user_id = ?")
        );
        let task_leases_sql = SESSION_DELETE_TASK_LEASES_SQL
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(task_leases_sql.contains("user_id = ?"));
        assert!(task_leases_sql.contains("session_id = ? AND user_id = ?"));

        let plan_step_runs_sql = SESSION_DELETE_PLAN_STEP_RUNS_SQL
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(plan_step_runs_sql.contains("user_id = ?"));
        assert!(plan_step_runs_sql.contains("session_id = ? AND user_id = ?"));
    }

    #[test]
    fn verification_results_delete_uses_explicit_contract_owner_join() {
        let sql = SESSION_DELETE_DERIVED_PARENT_TABLES
            .iter()
            .find(|statement| statement.label == "verification_results")
            .expect("verification_results delete statement")
            .sql
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");

        assert!(sql.contains("WHERE EXISTS"));
        assert!(sql.contains("tc.contract_id = verification_results.contract_id"));
        assert!(sql.contains("tc.user_id = verification_results.user_id"));
        assert!(sql.contains("tc.session_id = ?"));
        assert!(sql.contains("tc.user_id = ?"));
        assert!(
            !sql.contains("(contract_id, user_id) IN"),
            "delete must not depend on tuple column order"
        );
    }
}
