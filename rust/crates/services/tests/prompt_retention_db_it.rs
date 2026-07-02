mod common;

use astra_services::{RetentionPolicy, cleanup_expired_data};
use serial_test::serial;
use sqlx::{MySql, Pool};
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn prompt_retention_prunes_inactive_parent_and_child_rows_only() {
    let shared_pool = common::setup_pool().await;
    let pool = shared_pool.get().clone();
    cleanup_prompt_retention_fixtures(&pool).await;

    let user_id = format!("prompt-retention-user-{}", Uuid::new_v4());
    let ended_session = format!("prompt-retention-ended-{}", Uuid::new_v4());
    let active_session = format!("prompt-retention-active-{}", Uuid::new_v4());
    let running_session = format!("prompt-retention-running-{}", Uuid::new_v4());
    let completed_session = format!("prompt-retention-completed-{}", Uuid::new_v4());
    let running_run = format!("prompt-retention-run-{}", Uuid::new_v4());
    let completed_run = format!("prompt-retention-run-{}", Uuid::new_v4());

    for (session_id, status) in [
        (&ended_session, "ended"),
        (&active_session, "active"),
        (&running_session, "ended"),
        (&completed_session, "ended"),
    ] {
        insert_session(&pool, &user_id, session_id, status).await;
    }
    insert_run(&pool, &user_id, &running_session, &running_run, "running").await;
    insert_run(
        &pool,
        &user_id,
        &completed_session,
        &completed_run,
        "completed",
    )
    .await;

    let delete_no_run = insert_prompt_fixture(&pool, &user_id, &ended_session, None, 1).await;
    let keep_active_session =
        insert_prompt_fixture(&pool, &user_id, &active_session, None, 2).await;
    let keep_running_run =
        insert_prompt_fixture(&pool, &user_id, &running_session, Some(&running_run), 3).await;
    let delete_completed_run =
        insert_prompt_fixture(&pool, &user_id, &completed_session, Some(&completed_run), 4).await;

    let policy = RetentionPolicy {
        prompt_request_days: 1,
        refresh_token_days: 10_000,
        auth_token_days: 10_000,
        task_lease_days: 10_000,
        sync_log_days: 10_000,
        audit_log_days: 10_000,
        event_days: 10_000,
    };
    let results = cleanup_expired_data(&pool, &policy)
        .await
        .expect("cleanup prompt retention");

    assert_eq!(
        rows_deleted(&results, "prompt_request_records"),
        2,
        "only inactive/terminal prompt parent rows should be pruned"
    );
    assert_eq!(
        rows_deleted(&results, "prompt_deltas"),
        2,
        "child prompt_deltas should be pruned with selected parents"
    );
    assert_prompt_absent(&pool, &user_id, &ended_session, &delete_no_run).await;
    assert_prompt_absent(&pool, &user_id, &completed_session, &delete_completed_run).await;
    assert_prompt_present(&pool, &user_id, &active_session, &keep_active_session).await;
    assert_prompt_present(&pool, &user_id, &running_session, &keep_running_run).await;

    cleanup_user(&pool, &user_id).await;
}

async fn insert_session(pool: &Pool<MySql>, user_id: &str, session_id: &str, status: &str) {
    sqlx::query(
        "INSERT INTO agent_sessions
         (session_id, user_id, title, status, event_count, ended_at)
         VALUES (?, ?, 'prompt retention test', ?, 0, DATE_SUB(NOW(6), INTERVAL 2 DAY))",
    )
    .bind(session_id)
    .bind(user_id)
    .bind(status)
    .execute(pool)
    .await
    .expect("insert test session");
}

async fn insert_run(
    pool: &Pool<MySql>,
    user_id: &str,
    session_id: &str,
    run_id: &str,
    status: &str,
) {
    sqlx::query(
        "INSERT INTO agent_runs
         (run_id, user_id, session_id, root_run_id, ancestor_path, status)
         VALUES (?, ?, ?, ?, '/', ?)",
    )
    .bind(run_id)
    .bind(user_id)
    .bind(session_id)
    .bind(run_id)
    .bind(status)
    .execute(pool)
    .await
    .expect("insert test run");
}

async fn insert_prompt_fixture(
    pool: &Pool<MySql>,
    user_id: &str,
    session_id: &str,
    run_id: Option<&str>,
    turn: i64,
) -> String {
    let request_id = format!("promptreq-{}", Uuid::new_v4().simple());
    sqlx::query(
        "INSERT INTO prompt_request_records
         (request_id, session_id, user_id, run_id, turn, round, attempt, source,
          model, provider, max_output_tokens, message_count, tool_count,
          previous_request_id, request_hash, summary_json, created_at)
         VALUES (?, ?, ?, ?, ?, 0, 0, 'retention-test',
                 'test-model', 'test-provider', NULL, 1, 0,
                 NULL, REPEAT('a', 64), '{}', DATE_SUB(NOW(6), INTERVAL 2 DAY))",
    )
    .bind(&request_id)
    .bind(session_id)
    .bind(user_id)
    .bind(run_id)
    .bind(turn)
    .execute(pool)
    .await
    .expect("insert prompt request");
    sqlx::query(
        "INSERT INTO prompt_deltas
         (user_id, session_id, request_id, delta_seq, logical_key, chunk_kind, position,
          op, chunk_id, chunk_hash, previous_chunk_hash, created_at)
         VALUES (?, ?, ?, 0, 'message:0:user', 'message', 0,
                 'append', ?, REPEAT('b', 64), NULL, DATE_SUB(NOW(6), INTERVAL 2 DAY))",
    )
    .bind(user_id)
    .bind(session_id)
    .bind(&request_id)
    .bind(format!("chunk-{request_id}"))
    .execute(pool)
    .await
    .expect("insert prompt delta");
    request_id
}

async fn assert_prompt_absent(
    pool: &Pool<MySql>,
    user_id: &str,
    session_id: &str,
    request_id: &str,
) {
    assert_eq!(
        prompt_count(pool, user_id, session_id, request_id).await,
        (0, 0)
    );
}

async fn assert_prompt_present(
    pool: &Pool<MySql>,
    user_id: &str,
    session_id: &str,
    request_id: &str,
) {
    assert_eq!(
        prompt_count(pool, user_id, session_id, request_id).await,
        (1, 1)
    );
}

async fn prompt_count(
    pool: &Pool<MySql>,
    user_id: &str,
    session_id: &str,
    request_id: &str,
) -> (i64, i64) {
    let requests = sqlx::query_scalar(
        "SELECT COUNT(*) FROM prompt_request_records
         WHERE user_id = ? AND session_id = ? AND request_id = ?",
    )
    .bind(user_id)
    .bind(session_id)
    .bind(request_id)
    .fetch_one(pool)
    .await
    .expect("count prompt request");
    let deltas = sqlx::query_scalar(
        "SELECT COUNT(*) FROM prompt_deltas
         WHERE user_id = ? AND session_id = ? AND request_id = ?",
    )
    .bind(user_id)
    .bind(session_id)
    .bind(request_id)
    .fetch_one(pool)
    .await
    .expect("count prompt deltas");
    (requests, deltas)
}

fn rows_deleted(results: &[astra_services::CleanupResult], table: &str) -> u64 {
    results
        .iter()
        .find(|result| result.table == table)
        .map(|result| result.rows_deleted)
        .unwrap_or(0)
}

async fn cleanup_user(pool: &Pool<MySql>, user_id: &str) {
    let _ = sqlx::query("DELETE FROM prompt_deltas WHERE user_id = ?")
        .bind(user_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM prompt_request_records WHERE user_id = ?")
        .bind(user_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM agent_runs WHERE user_id = ?")
        .bind(user_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM agent_sessions WHERE user_id = ?")
        .bind(user_id)
        .execute(pool)
        .await;
}

async fn cleanup_prompt_retention_fixtures(pool: &Pool<MySql>) {
    let _ = sqlx::query("DELETE FROM prompt_deltas WHERE user_id LIKE 'prompt-retention-user-%'")
        .execute(pool)
        .await;
    let _ = sqlx::query(
        "DELETE FROM prompt_request_records WHERE user_id LIKE 'prompt-retention-user-%'",
    )
    .execute(pool)
    .await;
    let _ = sqlx::query("DELETE FROM agent_runs WHERE user_id LIKE 'prompt-retention-user-%'")
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM agent_sessions WHERE user_id LIKE 'prompt-retention-user-%'")
        .execute(pool)
        .await;
}
