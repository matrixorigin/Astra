mod common;

use astra_services::{RetentionPolicy, cleanup_expired_data};
use serial_test::serial;
use sqlx::{MySql, Pool, QueryBuilder};
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
    let fresh_session = format!("prompt-retention-fresh-{}", Uuid::new_v4());
    let running_run = format!("prompt-retention-run-{}", Uuid::new_v4());
    let completed_run = format!("prompt-retention-run-{}", Uuid::new_v4());

    for (session_id, status) in [
        (&ended_session, "ended"),
        (&active_session, "active"),
        (&running_session, "ended"),
        (&completed_session, "ended"),
        (&fresh_session, "ended"),
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

    let delete_no_run = insert_prompt_fixture(
        &pool,
        &user_id,
        &ended_session,
        None,
        1,
        PromptFixtureAge::Expired,
    )
    .await;
    let keep_active_session = insert_prompt_fixture(
        &pool,
        &user_id,
        &active_session,
        None,
        2,
        PromptFixtureAge::Expired,
    )
    .await;
    let keep_running_run = insert_prompt_fixture(
        &pool,
        &user_id,
        &running_session,
        Some(&running_run),
        3,
        PromptFixtureAge::Expired,
    )
    .await;
    let delete_completed_run = insert_prompt_fixture(
        &pool,
        &user_id,
        &completed_session,
        Some(&completed_run),
        4,
        PromptFixtureAge::Expired,
    )
    .await;
    let keep_fresh_inactive = insert_prompt_fixture(
        &pool,
        &user_id,
        &fresh_session,
        None,
        5,
        PromptFixtureAge::Fresh,
    )
    .await;

    let policy = RetentionPolicy {
        prompt_request_days: 1,
        refresh_token_days: 10_000,
        auth_token_days: 10_000,
        task_lease_days: 10_000,
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
    assert_prompt_present(&pool, &user_id, &fresh_session, &keep_fresh_inactive).await;

    cleanup_user(&pool, &user_id).await;
}

#[tokio::test]
#[ignore = "requires live DB pressure run: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn prompt_retention_pressure_probe() {
    let shared_pool = common::setup_pool().await;
    let pool = shared_pool.get().clone();
    cleanup_prompt_retention_fixtures(&pool).await;

    let user_id = format!("prp-user-{}", Uuid::new_v4().simple());
    let ended_session = format!("prp-end-{}", Uuid::new_v4().simple());
    let active_session = format!("prp-act-{}", Uuid::new_v4().simple());
    let fresh_session = format!("prp-fresh-{}", Uuid::new_v4().simple());
    insert_session(&pool, &user_id, &ended_session, "ended").await;
    insert_session(&pool, &user_id, &active_session, "active").await;
    insert_session(&pool, &user_id, &fresh_session, "ended").await;

    let delete_rows = cleanup_pressure_rows("ASTRA_CLEANUP_PRESSURE_PROMPT_ROWS", 5_000, 1_001);
    let keep_rows = cleanup_pressure_rows("ASTRA_CLEANUP_PRESSURE_PROMPT_KEEP_ROWS", 128, 1);
    let fresh_rows = cleanup_pressure_rows("ASTRA_CLEANUP_PRESSURE_PROMPT_FRESH_ROWS", 128, 1);

    let insert_started = std::time::Instant::now();
    insert_prompt_fixtures_bulk(
        &pool,
        &user_id,
        &ended_session,
        None,
        0,
        delete_rows,
        "delete",
        PromptFixtureAge::Expired,
    )
    .await;
    insert_prompt_fixtures_bulk(
        &pool,
        &user_id,
        &active_session,
        None,
        delete_rows,
        keep_rows,
        "keep",
        PromptFixtureAge::Expired,
    )
    .await;
    insert_prompt_fixtures_bulk(
        &pool,
        &user_id,
        &fresh_session,
        None,
        delete_rows + keep_rows,
        fresh_rows,
        "fresh",
        PromptFixtureAge::Fresh,
    )
    .await;
    let insert_ms = insert_started.elapsed().as_millis();
    assert_eq!(
        prompt_session_count(&pool, &user_id, &ended_session).await,
        (delete_rows, delete_rows)
    );
    assert_eq!(
        prompt_session_count(&pool, &user_id, &active_session).await,
        (keep_rows, keep_rows)
    );
    assert_eq!(
        prompt_session_count(&pool, &user_id, &fresh_session).await,
        (fresh_rows, fresh_rows)
    );

    let policy = RetentionPolicy {
        prompt_request_days: 1,
        refresh_token_days: 10_000,
        auth_token_days: 10_000,
        task_lease_days: 10_000,
        audit_log_days: 10_000,
        event_days: 10_000,
    };
    let cleanup_started = std::time::Instant::now();
    let mut reported_prompt_requests = 0_u64;
    let mut reported_prompt_deltas = 0_u64;
    let mut cleanup_calls = 0_u64;
    loop {
        let results = cleanup_expired_data(&pool, &policy)
            .await
            .expect("cleanup prompt retention pressure");
        cleanup_calls += 1;
        reported_prompt_requests += rows_deleted(&results, "prompt_request_records");
        reported_prompt_deltas += rows_deleted(&results, "prompt_deltas");

        let remaining = prompt_session_count(&pool, &user_id, &ended_session).await;
        if remaining == (0, 0) {
            break;
        }
        assert!(
            cleanup_calls <= ((delete_rows / 1_000) + 10) as u64,
            "prompt cleanup should converge near the configured batch limit"
        );
    }
    let cleanup_ms = cleanup_started.elapsed().as_millis();
    let max_rows_per_cleanup = 10_000_i64;
    let expected_cleanup_calls: u64 = ((delete_rows + max_rows_per_cleanup - 1)
        / max_rows_per_cleanup)
        .try_into()
        .expect("expected cleanup calls fit u64");

    assert_eq!(
        prompt_session_count(&pool, &user_id, &ended_session).await,
        (0, 0)
    );
    assert_eq!(
        prompt_session_count(&pool, &user_id, &active_session).await,
        (keep_rows, keep_rows),
        "active-session prompt rows must be guarded during pressure cleanup"
    );
    assert_eq!(
        prompt_session_count(&pool, &user_id, &fresh_session).await,
        (fresh_rows, fresh_rows),
        "fresh inactive prompt rows must not be pruned before the retention age"
    );
    assert!(
        reported_prompt_requests >= delete_rows as u64,
        "cleanup result should account for at least the pressure parent rows"
    );
    assert!(
        reported_prompt_deltas >= delete_rows as u64,
        "cleanup result should account for at least the pressure child rows"
    );
    assert_eq!(
        cleanup_calls, expected_cleanup_calls,
        "prompt cleanup should drain up to 10 bounded batches per cleanup invocation"
    );

    eprintln!(
        "CLEANUP_PRESSURE_RESULT {}",
        serde_json::json!({
            "path": "prompt_request_records.retention",
            "rows_inserted": delete_rows + keep_rows + fresh_rows,
            "rows_deleted": delete_rows,
            "guarded_rows": keep_rows,
            "fresh_rows": fresh_rows,
            "batch_limit": 1000,
            "max_batches_per_cleanup": 10,
            "cleanup_calls": cleanup_calls,
            "reported_prompt_requests": reported_prompt_requests,
            "reported_prompt_deltas": reported_prompt_deltas,
            "insert_ms": insert_ms,
            "cleanup_ms": cleanup_ms,
            "remaining_inactive_rows": prompt_session_count(&pool, &user_id, &ended_session).await.0,
            "remaining_guarded_rows": prompt_session_count(&pool, &user_id, &active_session).await.0,
            "remaining_fresh_rows": prompt_session_count(&pool, &user_id, &fresh_session).await.0,
        })
    );

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
    age: PromptFixtureAge,
) -> String {
    let request_id = format!("promptreq-{}", Uuid::new_v4().simple());
    let query = format!(
        "INSERT INTO prompt_request_records
         (request_id, session_id, user_id, run_id, turn, round, attempt, source,
          model, provider, max_output_tokens, message_count, tool_count,
          previous_request_id, request_hash, summary_json, created_at, created_at_unix_ms)
         VALUES (?, ?, ?, ?, ?, 0, 0, 'retention-test',
                 'test-model', 'test-provider', NULL, 1, 0,
                 NULL, REPEAT('a', 64), '{{}}', {},
                 {})",
        age.created_at_sql(),
        age.created_at_unix_ms_sql()
    );
    sqlx::query(&query)
        .bind(&request_id)
        .bind(session_id)
        .bind(user_id)
        .bind(run_id)
        .bind(turn)
        .execute(pool)
        .await
        .expect("insert prompt request");
    let delta_query = format!(
        "INSERT INTO prompt_deltas
         (user_id, session_id, request_id, delta_seq, logical_key, chunk_kind, position,
          op, chunk_id, chunk_hash, previous_chunk_hash, created_at)
         VALUES (?, ?, ?, 0, 'message:0:user', 'message', 0,
                 'append', ?, REPEAT('b', 64), NULL, {})",
        age.created_at_sql()
    );
    sqlx::query(&delta_query)
        .bind(user_id)
        .bind(session_id)
        .bind(&request_id)
        .bind(format!("chunk-{request_id}"))
        .execute(pool)
        .await
        .expect("insert prompt delta");
    request_id
}

async fn insert_prompt_fixtures_bulk(
    pool: &Pool<MySql>,
    user_id: &str,
    session_id: &str,
    run_id: Option<&str>,
    start_turn: i64,
    count: i64,
    label: &str,
    age: PromptFixtureAge,
) {
    let request_ids = (0..count)
        .map(|_| format!("promptreq-{label}-{}", Uuid::new_v4().simple()))
        .collect::<Vec<_>>();
    for (chunk_index, chunk) in request_ids.chunks(500).enumerate() {
        let chunk_base = start_turn + (chunk_index * 500) as i64;
        let mut requests = QueryBuilder::<MySql>::new(
            "INSERT INTO prompt_request_records
             (request_id, session_id, user_id, run_id, turn, round, attempt, source,
              model, provider, max_output_tokens, message_count, tool_count,
              previous_request_id, request_hash, summary_json, created_at, created_at_unix_ms) ",
        );
        requests.push_values(chunk.iter().enumerate(), |mut row, (index, request_id)| {
            let turn = chunk_base + index as i64;
            row.push_bind(request_id)
                .push_bind(session_id)
                .push_bind(user_id)
                .push_bind(run_id)
                .push_bind(turn)
                .push_bind(0_i32)
                .push_bind(0_i32)
                .push_bind("retention-pressure")
                .push_bind("test-model")
                .push_bind("test-provider")
                .push_bind(Option::<i32>::None)
                .push_bind(1_i32)
                .push_bind(0_i32)
                .push_bind(Option::<&str>::None)
                .push_bind(format!("{turn:064x}"))
                .push_bind("{}")
                .push(age.created_at_sql())
                .push(age.created_at_unix_ms_sql());
        });
        requests
            .build()
            .execute(pool)
            .await
            .expect("insert pressure prompt requests");

        let mut deltas = QueryBuilder::<MySql>::new(
            "INSERT INTO prompt_deltas
             (user_id, session_id, request_id, delta_seq, logical_key, chunk_kind, position,
              op, chunk_id, chunk_hash, previous_chunk_hash, created_at) ",
        );
        deltas.push_values(chunk.iter().enumerate(), |mut row, (index, request_id)| {
            let turn = chunk_base + index as i64;
            row.push_bind(user_id)
                .push_bind(session_id)
                .push_bind(request_id)
                .push_bind(0_i32)
                .push_bind("message:0:user")
                .push_bind("message")
                .push_bind(0_i32)
                .push_bind("append")
                .push_bind(format!("chunk-{request_id}"))
                .push_bind(format!("{:064x}", turn + 1_000_000))
                .push_bind(Option::<&str>::None)
                .push(age.created_at_sql());
        });
        deltas
            .build()
            .execute(pool)
            .await
            .expect("insert pressure prompt deltas");
    }
}

#[derive(Clone, Copy)]
enum PromptFixtureAge {
    Expired,
    Fresh,
}

impl PromptFixtureAge {
    fn created_at_sql(self) -> &'static str {
        match self {
            Self::Expired => "DATE_SUB(NOW(6), INTERVAL 2 DAY)",
            Self::Fresh => "NOW(6)",
        }
    }

    fn created_at_unix_ms_sql(self) -> &'static str {
        match self {
            Self::Expired => "UNIX_TIMESTAMP(DATE_SUB(NOW(6), INTERVAL 2 DAY)) * 1000",
            Self::Fresh => "UNIX_TIMESTAMP(NOW(6)) * 1000",
        }
    }
}

async fn prompt_session_count(pool: &Pool<MySql>, user_id: &str, session_id: &str) -> (i64, i64) {
    let requests = sqlx::query_scalar(
        "SELECT COUNT(*) FROM prompt_request_records
         WHERE user_id = ? AND session_id = ?",
    )
    .bind(user_id)
    .bind(session_id)
    .fetch_one(pool)
    .await
    .expect("count prompt requests for session");
    let deltas = sqlx::query_scalar(
        "SELECT COUNT(*) FROM prompt_deltas
         WHERE user_id = ? AND session_id = ?",
    )
    .bind(user_id)
    .bind(session_id)
    .fetch_one(pool)
    .await
    .expect("count prompt deltas for session");
    (requests, deltas)
}

fn cleanup_pressure_rows(env_key: &str, default: i64, minimum: i64) -> i64 {
    let rows = std::env::var(env_key)
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(default);
    assert!(
        rows >= minimum,
        "{env_key} must be at least {minimum} rows to exercise pressure cleanup"
    );
    rows
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
    let _ = sqlx::query("DELETE FROM prompt_deltas WHERE user_id LIKE 'prp-user-%'")
        .execute(pool)
        .await;
    let _ = sqlx::query(
        "DELETE FROM prompt_request_records WHERE user_id LIKE 'prompt-retention-user-%'",
    )
    .execute(pool)
    .await;
    let _ = sqlx::query("DELETE FROM prompt_request_records WHERE user_id LIKE 'prp-user-%'")
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM agent_runs WHERE user_id LIKE 'prompt-retention-user-%'")
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM agent_runs WHERE user_id LIKE 'prp-user-%'")
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM agent_sessions WHERE user_id LIKE 'prompt-retention-user-%'")
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM agent_sessions WHERE user_id LIKE 'prp-user-%'")
        .execute(pool)
        .await;
}
