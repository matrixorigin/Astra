mod common;

use astra_services::runtime_maintenance::{RuntimeMaintenancePolicy, maintain_runtime_storage};
use astra_services::{
    PromptRequestPersistInput, PromptRequestPlanInput, persist_prompt_request, plan_prompt_request,
};
use serde_json::json;
use sqlx::{MySql, Pool};
use uuid::Uuid;

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn prompt_diagnostics_expire_after_ninety_days_without_deleting_active_session() {
    let shared = common::setup_pool().await;
    let pool = shared.get().clone();
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("prompt-ttl-user-{suffix}");
    let session_id = format!("prompt-ttl-session-{suffix}");
    let expired_request_id = format!("prompt-ttl-expired-{suffix}");
    let fresh_request_id = format!("prompt-ttl-fresh-{suffix}");

    let mut tx = pool.begin().await.expect("begin active session fixture");
    astra_services::storage::add_agent_session_event_count_or_create(
        &mut tx,
        &session_id,
        &user_id,
        0,
        None,
    )
    .await
    .expect("insert active session fixture");
    tx.commit().await.expect("commit active session fixture");

    insert_prompt_diagnostic(&pool, &user_id, &session_id, &expired_request_id, 1, true).await;
    insert_prompt_diagnostic(&pool, &user_id, &session_id, &fresh_request_id, 2, false).await;

    let result = maintain_runtime_storage(
        &shared,
        None,
        &RuntimeMaintenancePolicy {
            batch_limit: 1_000,
            ..RuntimeMaintenancePolicy::default()
        },
    )
    .await;
    assert!(
        result.cleanup_errors.is_empty(),
        "runtime maintenance errors: {:?}",
        result.cleanup_errors
    );
    assert!(result.prompt_request_records_expired >= 1);
    assert!(result.prompt_deltas_expired >= 1);

    assert_eq!(
        prompt_diagnostic_count(&pool, &user_id, &expired_request_id).await,
        (0, 0)
    );
    assert_eq!(
        prompt_diagnostic_count(&pool, &user_id, &fresh_request_id).await,
        (1, 1)
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM agent_sessions WHERE user_id = ? AND session_id = ? AND status = 'active'",
        )
        .bind(&user_id)
        .bind(&session_id)
        .fetch_one(&pool)
        .await
        .expect("count active session"),
        1,
        "diagnostic expiry must not change durable session lifecycle"
    );

    cleanup_fixture(&pool, &user_id, &session_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn prompt_expiry_retains_an_expired_reuse_prefix_ancestor() {
    let shared = common::setup_pool().await;
    let pool = shared.get().clone();
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("prompt-ancestor-user-{suffix}");
    let session_id = format!("prompt-ancestor-session-{suffix}");
    let parent_request_id = format!("prompt-ancestor-parent-{suffix}");
    let child_request_id = format!("prompt-ancestor-child-{suffix}");

    let mut tx = pool.begin().await.expect("begin active session fixture");
    astra_services::storage::add_agent_session_event_count_or_create(
        &mut tx,
        &session_id,
        &user_id,
        0,
        None,
    )
    .await
    .expect("insert active session fixture");
    tx.commit().await.expect("commit active session fixture");

    insert_prompt_diagnostic(&pool, &user_id, &session_id, &parent_request_id, 1, true).await;
    insert_reuse_prefix_child(
        &pool,
        &user_id,
        &session_id,
        &child_request_id,
        &parent_request_id,
    )
    .await;

    let result = maintain_runtime_storage(
        &shared,
        None,
        &RuntimeMaintenancePolicy {
            batch_limit: 1_000,
            ..RuntimeMaintenancePolicy::default()
        },
    )
    .await;
    assert!(
        result.cleanup_errors.is_empty(),
        "runtime maintenance errors: {:?}",
        result.cleanup_errors
    );
    assert_eq!(
        prompt_diagnostic_count(&pool, &user_id, &parent_request_id).await,
        (1, 1),
        "a fresh reuse-prefix child keeps its expired ancestor readable"
    );

    let messages = [json!({"role": "user", "content": "retained ancestry"})];
    let plan = plan_prompt_request(PromptRequestPlanInput {
        user_id: &user_id,
        session_id: &session_id,
        turn: 3,
        round: 0,
        attempt: 0,
        source: "retention-test",
        messages: &messages,
        tools: &[],
        max_output_tokens: None,
    })
    .expect("plan successor prompt request");
    persist_prompt_request(
        &shared,
        &PromptRequestPersistInput {
            session_id: session_id.clone(),
            user_id: user_id.clone(),
            run_id: None,
            turn: 3,
            round: 0,
            attempt: 0,
            source: "retention-test".to_string(),
            model: "test-model".to_string(),
            provider: "test-provider".to_string(),
        },
        &plan,
    )
    .await
    .expect("a successor must reconstruct the retained reuse-prefix chain");

    cleanup_fixture(&pool, &user_id, &session_id).await;
}

async fn insert_prompt_diagnostic(
    pool: &Pool<MySql>,
    user_id: &str,
    session_id: &str,
    request_id: &str,
    turn: i64,
    expired: bool,
) {
    let age = if expired {
        "DATE_SUB(NOW(6), INTERVAL 91 DAY)"
    } else {
        "NOW(6)"
    };
    let request_sql = format!(
        "INSERT INTO prompt_request_records
         (request_id, session_id, user_id, turn, round, attempt, source, model, provider,
          message_count, tool_count, request_hash, summary_json, created_at, created_at_unix_ms)
         VALUES (?, ?, ?, ?, 0, 0, 'retention-test', 'test-model', 'test-provider',
                 1, 0, REPEAT('a', 64), '{{}}', {age}, UNIX_TIMESTAMP({age}) * 1000)"
    );
    sqlx::query(&request_sql)
        .bind(request_id)
        .bind(session_id)
        .bind(user_id)
        .bind(turn)
        .execute(pool)
        .await
        .expect("insert prompt request diagnostic");

    let delta_sql = format!(
        "INSERT INTO prompt_deltas
         (user_id, session_id, request_id, delta_seq, logical_key, chunk_kind, position,
          op, chunk_id, chunk_hash, created_at)
         VALUES (?, ?, ?, 0, 'message:0:user', 'message', 0,
                 'append', ?, REPEAT('b', 64), {age})"
    );
    sqlx::query(&delta_sql)
        .bind(user_id)
        .bind(session_id)
        .bind(request_id)
        .bind(format!("chunk-{request_id}"))
        .execute(pool)
        .await
        .expect("insert prompt delta diagnostic");
}

async fn insert_reuse_prefix_child(
    pool: &Pool<MySql>,
    user_id: &str,
    session_id: &str,
    request_id: &str,
    previous_request_id: &str,
) {
    sqlx::query(
        "INSERT INTO prompt_request_records
         (request_id, session_id, user_id, turn, round, attempt, source, model, provider,
          message_count, tool_count, previous_request_id, request_hash, summary_json,
          created_at, created_at_unix_ms)
         VALUES (?, ?, ?, 2, 0, 0, 'retention-test', 'test-model', 'test-provider',
                 1, 0, ?, REPEAT('c', 64), '{}', NOW(6), UNIX_TIMESTAMP(NOW(6)) * 1000)",
    )
    .bind(request_id)
    .bind(session_id)
    .bind(user_id)
    .bind(previous_request_id)
    .execute(pool)
    .await
    .expect("insert fresh reuse-prefix child request");
    sqlx::query(
        "INSERT INTO prompt_deltas
         (user_id, session_id, request_id, delta_seq, logical_key, chunk_kind, position,
          op, reuse_count, created_at)
         VALUES (?, ?, ?, 0, 'prefix:1', 'prefix', 0, 'reuse_prefix', 1, NOW(6))",
    )
    .bind(user_id)
    .bind(session_id)
    .bind(request_id)
    .execute(pool)
    .await
    .expect("insert fresh reuse-prefix child delta");
}

async fn prompt_diagnostic_count(
    pool: &Pool<MySql>,
    user_id: &str,
    request_id: &str,
) -> (i64, i64) {
    let requests = sqlx::query_scalar(
        "SELECT COUNT(*) FROM prompt_request_records WHERE user_id = ? AND request_id = ?",
    )
    .bind(user_id)
    .bind(request_id)
    .fetch_one(pool)
    .await
    .expect("count prompt request diagnostics");
    let deltas = sqlx::query_scalar(
        "SELECT COUNT(*) FROM prompt_deltas WHERE user_id = ? AND request_id = ?",
    )
    .bind(user_id)
    .bind(request_id)
    .fetch_one(pool)
    .await
    .expect("count prompt delta diagnostics");
    (requests, deltas)
}

async fn cleanup_fixture(pool: &Pool<MySql>, user_id: &str, session_id: &str) {
    for table in [
        "prompt_deltas",
        "prompt_request_records",
        "agent_sessions",
        "agent_session_lifecycle_fences",
    ] {
        let sql = format!("DELETE FROM {table} WHERE user_id = ? AND session_id = ?");
        sqlx::query(&sql)
            .bind(user_id)
            .bind(session_id)
            .execute(pool)
            .await
            .unwrap_or_else(|error| panic!("cleanup {table}: {error}"));
    }
}
