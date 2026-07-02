//! MatrixOne integration: session reaper idle → ended transitions.
//!
//! ```text
//! ASTRA_TEST_DB_IT=1 cargo test -p astra-services --test session_reaper_db_integration -- --ignored
//! ```

use astra_services::session_journal::{JournalDirGuard, journal_file_path_for_user};
use astra_services::session_reaper::{SessionReaperPolicy, reap_sessions};
use sqlx::Pool;
use sqlx::Row;
use sqlx::mysql::MySql;
use uuid::Uuid;

mod common;

async fn session_status(pool: &Pool<MySql>, user_id: &str, session_id: &str) -> String {
    sqlx::query_scalar("SELECT status FROM agent_sessions WHERE session_id = ? AND user_id = ?")
        .bind(session_id)
        .bind(user_id)
        .fetch_one(pool)
        .await
        .expect("session status")
}

async fn session_exists(pool: &Pool<MySql>, user_id: &str, session_id: &str) -> bool {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_sessions WHERE session_id = ? AND user_id = ?",
    )
    .bind(session_id)
    .bind(user_id)
    .fetch_one(pool)
    .await
    .expect("session count");
    count > 0
}

async fn count_owner_rows(
    pool: &Pool<MySql>,
    table: &'static str,
    user_id: &str,
    session_id: &str,
) -> i64 {
    let sql = format!("SELECT COUNT(*) FROM {table} WHERE session_id = ? AND user_id = ?");
    sqlx::query_scalar(&sql)
        .bind(session_id)
        .bind(user_id)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|error| panic!("count {table} owner rows: {error}"))
}

async fn cleanup_owner_session_rows(pool: &Pool<MySql>, user_id: &str, session_id: &str) {
    for table in [
        "session_tool_outputs",
        "session_tool_output_batches",
        "agent_run_events",
        "run_display_projections",
        "run_checkpoints",
        "agent_runs",
        "conversation_log",
        "prompt_deltas",
        "prompt_request_records",
        "agent_event_edges",
        "agent_events",
        "session_artifacts",
        "agent_sessions",
    ] {
        let sql = format!("DELETE FROM {table} WHERE session_id = ? AND user_id = ?");
        let _ = sqlx::query(&sql)
            .bind(session_id)
            .bind(user_id)
            .execute(pool)
            .await;
    }
}

/// Shared CI DB may contain many stale rows; `reap_sessions` uses `LIMIT batch_limit`
/// without `ORDER BY`, so loop until *this* session reaches the expected status.
async fn reap_until(
    pool: &Pool<MySql>,
    policy: &SessionReaperPolicy,
    user_id: &str,
    session_id: &str,
    want: &str,
    max_rounds: u32,
) {
    for _ in 0..max_rounds {
        if session_status(pool, user_id, session_id).await == want {
            return;
        }
        let result = reap_sessions(pool, policy).await.expect("reap sessions");
        if result.marked_idle + result.marked_ended + result.deleted == 0 {
            break;
        }
    }
    assert_eq!(
        session_status(pool, user_id, session_id).await,
        want,
        "session {session_id} did not reach '{want}' within {max_rounds} reap rounds"
    );
}

async fn reap_until_deleted(
    pool: &Pool<MySql>,
    policy: &SessionReaperPolicy,
    user_id: &str,
    session_id: &str,
    max_rounds: u32,
) -> (u64, u64) {
    let mut database_rows_deleted = 0_u64;
    let mut local_bytes_freed = 0_u64;
    for _ in 0..max_rounds {
        if !session_exists(pool, user_id, session_id).await {
            return (database_rows_deleted, local_bytes_freed);
        }
        let result = reap_sessions(pool, policy).await.expect("reap sessions");
        database_rows_deleted += result.database_rows_deleted;
        local_bytes_freed += result.local_bytes_freed;
        if result.marked_idle + result.marked_ended + result.deleted == 0 {
            break;
        }
    }
    assert!(
        !session_exists(pool, user_id, session_id).await,
        "session {session_id} was not deleted within {max_rounds} reap rounds"
    );
    (database_rows_deleted, local_bytes_freed)
}

#[tokio::test]
#[ignore = "live MatrixOne; ASTRA_TEST_DB_IT=1"]
async fn reaper_marks_stale_active_session_idle_then_ended() {
    common::require_db_it_env();
    let pool = common::setup_pool().await.get().clone();

    let session_id = format!("reaper-it-{}", Uuid::new_v4());
    let user_id = format!("reaper-user-{}", Uuid::new_v4());

    sqlx::query(
        "INSERT INTO agent_sessions \
         (session_id, user_id, agent_id, status, title, event_count) \
         VALUES (?, ?, 'astra-cli', 'active', 'reaper test', 0)",
    )
    .bind(&session_id)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("insert active session");

    // Backdate activity in UPDATE (reliable on MatrixOne; DATE_SUB in INSERT VALUES is not).
    // 2 hours ago: stale for idle (60s) but not for end (86_400s) within a single reap sweep.
    sqlx::query(
        "UPDATE agent_sessions \
         SET last_active_at = DATE_SUB(NOW(6), INTERVAL 2 HOUR), \
             updated_at = DATE_SUB(NOW(6), INTERVAL 2 HOUR), \
             created_at = DATE_SUB(NOW(6), INTERVAL 2 HOUR) \
         WHERE session_id = ? AND user_id = ?",
    )
    .bind(&session_id)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("backdate session activity");

    let stale_secs: i64 = sqlx::query_scalar(
        "SELECT TIMESTAMPDIFF(SECOND, last_active_at, NOW(6)) \
         FROM agent_sessions WHERE session_id = ? AND user_id = ?",
    )
    .bind(&session_id)
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("stale seconds");
    assert!(
        stale_secs >= 7200,
        "backdate must leave session at least 2h stale, got {stale_secs}s"
    );
    assert_eq!(
        session_status(&pool, &user_id, &session_id).await,
        "active",
        "seed session must start active"
    );

    // Pass 1: mark stale actives as idle (do not end yet).
    let idle_only = SessionReaperPolicy {
        idle_after_secs: 60,
        end_after_idle_secs: 86_400,
        delete_after_ended_days: 365,
        batch_limit: 500,
    };
    reap_until(&pool, &idle_only, &user_id, &session_id, "idle", 50).await;

    // Pass 2: end sessions idle longer than the threshold.
    let end_policy = SessionReaperPolicy {
        idle_after_secs: 86_400,
        end_after_idle_secs: 60,
        delete_after_ended_days: 365,
        batch_limit: 500,
    };
    reap_until(&pool, &end_policy, &user_id, &session_id, "ended", 50).await;

    let row = sqlx::query(
        "SELECT status, ended_at FROM agent_sessions WHERE session_id = ? AND user_id = ?",
    )
    .bind(&session_id)
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("final row");
    let status = row.try_get::<String, _>("status").expect("status");
    assert_eq!(status, "ended");
    let ended_at_set: Option<i64> = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_sessions \
         WHERE session_id = ? AND user_id = ? AND ended_at IS NOT NULL",
    )
    .bind(&session_id)
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("ended_at count");
    assert_eq!(ended_at_set, Some(1), "ended_at should be set");

    cleanup_owner_session_rows(&pool, &user_id, &session_id).await;
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "live MatrixOne; ASTRA_TEST_DB_IT=1"]
async fn reaper_deletes_full_session_lifecycle_tables() {
    common::require_db_it_env();
    let pool = common::setup_pool().await.get().clone();
    let local_sessions = tempfile::tempdir().expect("local sessions tempdir");
    let _journal_guard = JournalDirGuard::new(local_sessions.path());

    let session_id = format!("reaper-delete-{}", Uuid::new_v4());
    let user_id = format!("reaper-user-{}", Uuid::new_v4());
    let request_id = format!("promptreq-{}", Uuid::new_v4().simple());
    let run_id = format!("run-{}", Uuid::new_v4().simple());
    let run_event_row_id = format!("run-event-row-{}", Uuid::new_v4().simple());
    let run_event_id = format!("run-event-{}", Uuid::new_v4().simple());
    let batch_id = format!("batch-{}", Uuid::new_v4().simple());
    let output_id = format!("output-{}", Uuid::new_v4().simple());
    let event_id = format!("event-{}", Uuid::new_v4().simple());
    let parent_event_id = format!("parent-event-{}", Uuid::new_v4().simple());
    let owner_journal = journal_file_path_for_user(&user_id, &session_id).expect("journal path");
    std::fs::create_dir_all(owner_journal.parent().expect("journal parent"))
        .expect("create journal parent");
    std::fs::write(&owner_journal, "{}\n").expect("seed owner journal");

    sqlx::query(
        "INSERT INTO agent_sessions \
         (session_id, user_id, agent_id, status, title, event_count) \
         VALUES (?, ?, 'astra-cli', 'ended', 'reaper delete test', 0)",
    )
    .bind(&session_id)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("insert ended session");
    sqlx::query(
        "UPDATE agent_sessions \
         SET ended_at = DATE_SUB(NOW(6), INTERVAL 2 DAY), \
             updated_at = DATE_SUB(NOW(6), INTERVAL 2 DAY), \
             last_active_at = DATE_SUB(NOW(6), INTERVAL 2 DAY) \
         WHERE session_id = ? AND user_id = ?",
    )
    .bind(&session_id)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("backdate ended session");

    sqlx::query(
        "INSERT INTO prompt_request_records
         (request_id, session_id, user_id, run_id, turn, round, attempt, source,
          model, provider, max_output_tokens, message_count, tool_count,
          previous_request_id, request_hash, summary_json, created_at)
         VALUES (?, ?, ?, NULL, 1, 0, 0, 'turn',
                 'test-model', 'test-provider', NULL, 1, 0,
                 NULL, REPEAT('b', 64), '{}', NOW(6))",
    )
    .bind(&request_id)
    .bind(&session_id)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("insert prompt request");
    sqlx::query(
        "INSERT INTO prompt_deltas
         (user_id, session_id, request_id, delta_seq, logical_key, chunk_kind, position,
          op, chunk_id, chunk_hash, previous_chunk_hash)
         VALUES (?, ?, ?, 0, 'message:0:user', 'message', 0,
                 'append', 'chunk-1', REPEAT('c', 64), NULL)",
    )
    .bind(&user_id)
    .bind(&session_id)
    .bind(&request_id)
    .execute(&pool)
    .await
    .expect("insert prompt delta");

    sqlx::query(
        "INSERT INTO agent_runs
         (run_id, user_id, session_id, root_run_id, ancestor_path, status)
         VALUES (?, ?, ?, ?, '/', 'completed')",
    )
    .bind(&run_id)
    .bind(&user_id)
    .bind(&session_id)
    .bind(&run_id)
    .execute(&pool)
    .await
    .expect("insert agent run");
    sqlx::query(
        "INSERT INTO agent_run_events
         (id, run_id, event_idx, user_id, session_id, event_type, event_id, event_hash, payload_json)
         VALUES (?, ?, 0, ?, ?, 'run.completed', ?, REPEAT('f', 64), '{}')",
    )
    .bind(&run_event_row_id)
    .bind(&run_id)
    .bind(&user_id)
    .bind(&session_id)
    .bind(&run_event_id)
    .execute(&pool)
    .await
    .expect("insert agent run event");
    sqlx::query(
        "INSERT INTO session_tool_output_batches
         (batch_id, session_id, run_id, user_id, output_count, payload_bytes)
         VALUES (?, ?, ?, ?, 1, 2)",
    )
    .bind(&batch_id)
    .bind(&session_id)
    .bind(&run_id)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("insert tool output batch");
    sqlx::query(
        "INSERT INTO session_tool_outputs
         (output_id, batch_id, session_id, run_id, user_id, output_idx, tool_name, output_json, payload_bytes)
         VALUES (?, ?, ?, ?, ?, 0, 'test_tool', '{}', 2)",
    )
    .bind(&output_id)
    .bind(&batch_id)
    .bind(&session_id)
    .bind(&run_id)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("insert tool output");
    sqlx::query(
        "INSERT INTO conversation_log
         (user_id, session_id, seq, turn, entry_type, payload)
         VALUES (?, ?, 1, 1, 0, ?)",
    )
    .bind(&user_id)
    .bind(&session_id)
    .bind(r#"{"type":"snapshot","seq":1,"turn":1,"messages":[],"session_state":{}}"#)
    .execute(&pool)
    .await
    .expect("insert conversation log");
    sqlx::query(
        "INSERT INTO agent_events (event_id, session_id, user_id, event_type, content)
         VALUES (?, ?, ?, 'test.event', '{}')",
    )
    .bind(&event_id)
    .bind(&session_id)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("insert agent event");
    sqlx::query(
        "INSERT INTO agent_event_edges
         (user_id, session_id, child_event_id, parent_event_id, relation_kind, parent_order)
         VALUES (?, ?, ?, ?, 'causal', 0)",
    )
    .bind(&user_id)
    .bind(&session_id)
    .bind(&event_id)
    .bind(&parent_event_id)
    .execute(&pool)
    .await
    .expect("insert agent event edge");

    let policy = SessionReaperPolicy {
        idle_after_secs: 86_400,
        end_after_idle_secs: 86_400,
        delete_after_ended_days: 1,
        batch_limit: 500,
    };
    let (database_rows_deleted, local_bytes_freed) =
        reap_until_deleted(&pool, &policy, &user_id, &session_id, 50).await;
    assert!(
        database_rows_deleted >= 3,
        "reaper must delete session row plus lifecycle child rows, got {database_rows_deleted}"
    );
    assert!(
        local_bytes_freed > 0,
        "reaper must report owner-bound local artifact bytes freed"
    );
    assert!(
        !owner_journal.exists(),
        "reaper must delete owner-bound local journal artifacts"
    );

    let prompt_requests: i64 =
        count_owner_rows(&pool, "prompt_request_records", &user_id, &session_id).await;
    let prompt_deltas: i64 = count_owner_rows(&pool, "prompt_deltas", &user_id, &session_id).await;
    assert_eq!(
        prompt_requests, 0,
        "reaper must delete prompt_request_records"
    );
    assert_eq!(prompt_deltas, 0, "reaper must delete prompt_deltas");
    for table in [
        "agent_events",
        "agent_event_edges",
        "agent_run_events",
        "conversation_log",
        "session_tool_output_batches",
        "session_tool_outputs",
    ] {
        assert_eq!(
            count_owner_rows(&pool, table, &user_id, &session_id).await,
            0,
            "reaper must delete {table}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "live MatrixOne; ASTRA_TEST_DB_IT=1"]
async fn reaper_delete_preserves_foreign_owner_child_rows_with_same_session_id() {
    common::require_db_it_env();
    let pool = common::setup_pool().await.get().clone();

    let session_id = format!("reaper-owner-scope-{}", Uuid::new_v4());
    let owner_user_id = format!("reaper-owner-{}", Uuid::new_v4());
    let foreign_user_id = format!("reaper-foreign-{}", Uuid::new_v4());
    let owner_request_id = format!("promptreq-{}", Uuid::new_v4().simple());
    let foreign_request_id = format!("promptreq-{}", Uuid::new_v4().simple());
    let owner_event_id = format!("event-{}", Uuid::new_v4().simple());
    let foreign_event_id = format!("event-{}", Uuid::new_v4().simple());
    let owner_artifact_id = format!("artifact-{}", Uuid::new_v4().simple());
    let foreign_artifact_id = format!("artifact-{}", Uuid::new_v4().simple());

    sqlx::query(
        "INSERT INTO agent_sessions \
         (session_id, user_id, agent_id, status, title, event_count) \
         VALUES (?, ?, 'astra-cli', 'ended', 'reaper owner-scope delete test', 0)",
    )
    .bind(&session_id)
    .bind(&owner_user_id)
    .execute(&pool)
    .await
    .expect("insert ended owner session");
    sqlx::query(
        "UPDATE agent_sessions \
         SET ended_at = DATE_SUB(NOW(6), INTERVAL 2 DAY), \
             updated_at = DATE_SUB(NOW(6), INTERVAL 2 DAY), \
             last_active_at = DATE_SUB(NOW(6), INTERVAL 2 DAY) \
         WHERE session_id = ? AND user_id = ?",
    )
    .bind(&session_id)
    .bind(&owner_user_id)
    .execute(&pool)
    .await
    .expect("backdate owner session");

    for (user_id, request_id, event_id, artifact_id, marker) in [
        (
            &owner_user_id,
            &owner_request_id,
            &owner_event_id,
            &owner_artifact_id,
            "owner",
        ),
        (
            &foreign_user_id,
            &foreign_request_id,
            &foreign_event_id,
            &foreign_artifact_id,
            "foreign",
        ),
    ] {
        sqlx::query(
            "INSERT INTO prompt_request_records
             (request_id, session_id, user_id, run_id, turn, round, attempt, source,
              model, provider, max_output_tokens, message_count, tool_count,
              previous_request_id, request_hash, summary_json, created_at)
             VALUES (?, ?, ?, NULL, 1, 0, 0, 'turn',
                     'test-model', 'test-provider', NULL, 1, 0,
                     NULL, REPEAT('d', 64), '{}', NOW(6))",
        )
        .bind(request_id)
        .bind(&session_id)
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("insert prompt request");
        sqlx::query(
            "INSERT INTO prompt_deltas
             (user_id, session_id, request_id, delta_seq, logical_key, chunk_kind, position,
              op, chunk_id, chunk_hash, previous_chunk_hash)
             VALUES (?, ?, ?, 0, 'message:0:user', 'message', 0,
                     'append', ?, REPEAT('e', 64), NULL)",
        )
        .bind(user_id)
        .bind(&session_id)
        .bind(request_id)
        .bind(format!("chunk-{marker}"))
        .execute(&pool)
        .await
        .expect("insert prompt delta");
        sqlx::query(
            "INSERT INTO agent_events (event_id, session_id, user_id, event_type, content) \
             VALUES (?, ?, ?, ?, '{}')",
        )
        .bind(event_id)
        .bind(&session_id)
        .bind(user_id)
        .bind(format!("{marker}_event"))
        .execute(&pool)
        .await
        .expect("insert event");
        sqlx::query(
            "INSERT INTO agent_event_edges \
             (user_id, session_id, child_event_id, parent_event_id, relation_kind, parent_order) \
             VALUES (?, ?, ?, ?, 'causal', 0)",
        )
        .bind(user_id)
        .bind(&session_id)
        .bind(event_id)
        .bind(format!("parent-{marker}"))
        .execute(&pool)
        .await
        .expect("insert event edge");
        sqlx::query(
            "INSERT INTO session_artifacts \
             (artifact_id, session_id, user_id, artifact_kind, source, content_json, metadata) \
             VALUES (?, ?, ?, 'test', 'reaper_owner_scope', '{}', CAST(? AS JSON))",
        )
        .bind(artifact_id)
        .bind(&session_id)
        .bind(user_id)
        .bind(format!("{{\"marker\":\"{marker}\"}}"))
        .execute(&pool)
        .await
        .expect("insert session artifact");
    }

    let policy = SessionReaperPolicy {
        idle_after_secs: 86_400,
        end_after_idle_secs: 86_400,
        delete_after_ended_days: 1,
        batch_limit: 500,
    };
    let (database_rows_deleted, _) =
        reap_until_deleted(&pool, &policy, &owner_user_id, &session_id, 50).await;
    assert!(
        database_rows_deleted >= 6,
        "owner delete should remove session plus owner child rows, got {database_rows_deleted}"
    );

    for table in [
        "prompt_request_records",
        "prompt_deltas",
        "agent_events",
        "agent_event_edges",
        "session_artifacts",
    ] {
        assert_eq!(
            count_owner_rows(&pool, table, &owner_user_id, &session_id).await,
            0,
            "{table} owner rows must be deleted"
        );
        assert_eq!(
            count_owner_rows(&pool, table, &foreign_user_id, &session_id).await,
            1,
            "{table} foreign rows with same session_id must survive"
        );
    }

    cleanup_owner_session_rows(&pool, &foreign_user_id, &session_id).await;
}
