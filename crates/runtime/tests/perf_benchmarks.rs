mod test_support;

use test_support::require_db_it_env;

use std::time::Instant;

use astra_core::SharedPool;
use serde_json::json;
use sqlx::Row;
use uuid::Uuid;

async fn setup_pool() -> SharedPool {
    let settings = require_db_it_env();
    let catalog =
        std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG").unwrap_or_else(|_| "mysql".to_string());
    astra_services::ensure_core_schema(&settings, &catalog)
        .await
        .expect("ensure_core_schema must pass before perf benchmarks");
    SharedPool::new(&settings)
        .await
        .expect("SharedPool::new must connect to MatrixOne")
}

fn id(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4().simple())
}

async fn insert_session(pool: &SharedPool, user_id: &str, session_id: &str) {
    sqlx::query(
        "INSERT INTO agent_sessions
         (session_id, user_id, agent_id, title, status, metadata, created_at, updated_at)
         VALUES (?, ?, 'perf-agent', 'perf session', 'active', '{}', NOW(6), NOW(6))",
    )
    .bind(session_id)
    .bind(user_id)
    .execute(pool.get())
    .await
    .expect("perf insert_session must succeed");
}

fn millis(started: Instant) -> u128 {
    started.elapsed().as_millis()
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1; perf_benchmark"]
async fn perf_benchmark_1_hot_path_query_under_50ms_p99() {
    let pool = setup_pool().await;
    let user_id = id("user");
    let session_id = id("session");
    insert_session(&pool, &user_id, &session_id).await;
    for i in 0..64 {
        sqlx::query(
            "INSERT INTO session_artifacts
             (artifact_id, session_id, user_id, artifact_kind, content_json, metadata,
              retention_policy, status, created_at, updated_at)
             VALUES (?, ?, ?, 'cargo', ?, ?, 'default', 'active', NOW(6), NOW(6))",
        )
        .bind(id("artifact"))
        .bind(&session_id)
        .bind(&user_id)
        .bind(json!({"preview_text": format!("artifact {i}")}).to_string())
        .bind(json!({"byte_size": 2048}).to_string())
        .execute(pool.get())
        .await
        .expect("PERF-1 artifact seed must insert");
    }

    let mut samples = Vec::with_capacity(40);
    for _ in 0..40 {
        let started = Instant::now();
        let row = sqlx::query(
            "SELECT COUNT(*) AS c
             FROM session_artifacts FORCE INDEX (idx_session_artifacts_owner_kind_order)
             WHERE user_id = ? AND session_id = ? AND artifact_kind = 'cargo'",
        )
        .bind(&user_id)
        .bind(&session_id)
        .fetch_one(pool.get())
        .await
        .expect("PERF-1 hot path query must succeed");
        let count = row.try_get::<i64, _>("c").unwrap_or_default();
        assert!(
            count >= 64,
            "PERF-1 hot path query must see seeded rows, got {count}"
        );
        samples.push(millis(started));
    }
    samples.sort_unstable();
    let p99 = samples[samples.len() - 1];
    assert!(
        p99 < 50,
        "PERF-1 hot path query p99 must be <50ms, got {p99}ms; samples={samples:?}"
    );
}
