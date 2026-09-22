mod test_support;

use test_support::require_db_it_env;

use std::{sync::Arc, time::Instant};

use astra_core::SharedPool;
use astra_services::runs::ToolOutputBatchItem;
use astra_services::{
    ContextManifestItemWrite, ContextManifestWrite, DatabaseContextManifestStore,
    DatabaseRunStateStore,
};
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

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1; perf_benchmark"]
async fn perf_benchmark_3_one_thousand_tool_outputs_under_1000ms() {
    let pool = setup_pool().await;
    let user_id = id("user");
    let session_id = id("session");
    let run_id = id("run");
    insert_session(&pool, &user_id, &session_id).await;
    let store = DatabaseRunStateStore::new(pool.clone());
    let started = Instant::now();
    for batch in 0..2 {
        let mut items = Vec::with_capacity(500);
        for i in 0..500 {
            let idx = batch * 500 + i;
            items.push(ToolOutputBatchItem {
                output_id: id("out"),
                tool_call_id: Some(format!("call-{idx}")),
                tool_name: "slow_query_analyzer".to_string(),
                result: astra_turn_types::ToolInvocationResultPayload::new(
                    format!(r#"{{"idx":{idx},"line":"slow query","duration_ms":123}}"#),
                    Default::default(),
                    None,
                )
                .unwrap(),
            });
        }
        store
            .insert_tool_output_batch(&id("batch"), &session_id, &run_id, &user_id, &items)
            .await
            .expect("PERF-3 tool output batch insert must succeed");
    }
    let elapsed_ms = millis(started);
    let count = sqlx::query(
        "SELECT COUNT(*) AS c FROM session_tool_outputs
         WHERE user_id = ? AND session_id = ? AND run_id = ?",
    )
    .bind(&user_id)
    .bind(&session_id)
    .bind(&run_id)
    .fetch_one(pool.get())
    .await
    .expect("PERF-3 tool output count query must succeed")
    .try_get::<i64, _>("c")
    .unwrap_or_default();
    assert!(
        count == 1_000,
        "PERF-3 must persist exactly 1000 tool output rows, got {count}"
    );
    let max_ms = 1_000;
    assert!(
        elapsed_ms < max_ms,
        "PERF-3 1000 tool output rows must insert in <{max_ms}ms, got {elapsed_ms}ms"
    );
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1; perf_benchmark"]
async fn perf_benchmark_6_manifest_batches_across_users_and_sessions() {
    const WRITERS: usize = 8;
    const ITEMS_PER_MANIFEST: usize = 257;

    let pool = setup_pool().await;
    let users = (0..4).map(|_| id("perf-user")).collect::<Vec<_>>();
    let mut workloads = Vec::with_capacity(WRITERS);
    for writer in 0..WRITERS {
        // Use more than one session per user so the fixture covers both
        // owner isolation and independent-session concurrency.
        let user_id = users[writer % users.len()].clone();
        let session_id = id(&format!("perf-session-{writer}"));
        let run_id = id("run");
        insert_session(&pool, &user_id, &session_id).await;
        sqlx::query(
            "INSERT INTO agent_session_lifecycle_fences
             (session_id, user_id, created_at, updated_at)
             VALUES (?, ?, NOW(6), NOW(6))",
        )
        .bind(&session_id)
        .bind(&user_id)
        .execute(pool.get())
        .await
        .expect("PERF-6 session fence seed must insert");

        let manifest_id = id(&format!("manifest-{writer}"));
        let items = (0..ITEMS_PER_MANIFEST)
            .map(|item_order| ContextManifestItemWrite {
                session_id: session_id.clone(),
                item_order: item_order as i32,
                zone: "recent_tail".to_string(),
                source_table: "runtime_messages".to_string(),
                source_id: format!("{run_id}:message:{item_order}"),
                source_hash: None,
                included: true,
                token_estimate: 8,
                budget_tokens: 16,
                reason: "normal_turn".to_string(),
                render_mode: "plain_text".to_string(),
                raw_ref: None,
            })
            .collect::<Vec<_>>();
        let manifest = ContextManifestWrite {
            manifest_id: manifest_id.clone(),
            user_id: user_id.clone(),
            session_id: session_id.clone(),
            run_id: Some(run_id.clone()),
            turn_id: id("turn"),
            model_provider: "mock".to_string(),
            model_name: "perf-batch-llm".to_string(),
            context_window_tokens: 8_000,
            max_output_tokens: 700,
            total_estimated_tokens: 2_000,
            policy_version: "context_manifest_v1".to_string(),
            tokenizer_id: Some("estimated_v1".to_string()),
            budget_template_id: Some("budget_v1_8k".to_string()),
            turn_intent: Some("normal".to_string()),
            reason: "normal_turn".to_string(),
            manifest_json: json!({
                "writer": writer,
                "item_count": ITEMS_PER_MANIFEST,
            }),
        };
        workloads.push((user_id, session_id, run_id, manifest, items));
    }

    let store = DatabaseContextManifestStore::new(pool.clone());
    let barrier = Arc::new(tokio::sync::Barrier::new(WRITERS + 1));
    let mut tasks = Vec::with_capacity(workloads.len());
    for (user_id, session_id, run_id, manifest, items) in workloads {
        let store = store.clone();
        let barrier = Arc::clone(&barrier);
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            store
                .save_manifest(manifest, items)
                .await
                .expect("PERF-6 batched manifest write must succeed");
            (user_id, session_id, run_id)
        }));
    }
    let started = Instant::now();
    barrier.wait().await;

    let mut completed_writes = Vec::with_capacity(WRITERS);
    for task in tasks {
        completed_writes.push(task.await.expect("PERF-6 writer task must join"));
    }
    let elapsed_ms = millis(started);

    let mut persisted = 0_i64;
    let mut first_session = None;
    for (user_id, session_id, run_id) in completed_writes {
        let row = sqlx::query(
            "SELECT item.item_order, item.session_id, item.source_id
             FROM context_manifest_items AS item
             INNER JOIN context_manifests AS manifest
               ON manifest.manifest_id = item.manifest_id
             WHERE manifest.user_id = ? AND manifest.session_id = ? AND manifest.run_id = ?
             ORDER BY item.item_order",
        )
        .bind(&user_id)
        .bind(&session_id)
        .bind(&run_id)
        .fetch_all(pool.get())
        .await
        .expect("PERF-6 persisted item query must succeed");
        assert_eq!(
            row.len(),
            ITEMS_PER_MANIFEST,
            "PERF-6 each manifest must persist exactly its own items"
        );
        for (item_order, row) in row.iter().enumerate() {
            assert_eq!(
                row.try_get::<String, _>("session_id")
                    .expect("PERF-6 item session must decode"),
                session_id,
                "PERF-6 item must retain its source session"
            );
            assert_eq!(
                row.try_get::<i32, _>("item_order")
                    .expect("PERF-6 item order must decode"),
                item_order as i32
            );
            assert_eq!(
                row.try_get::<String, _>("source_id")
                    .expect("PERF-6 item source must decode"),
                format!("{run_id}:message:{item_order}")
            );
        }
        persisted += row.len() as i64;
        first_session.get_or_insert(session_id);
    }
    let expected = (WRITERS * ITEMS_PER_MANIFEST) as i64;
    assert_eq!(
        persisted, expected,
        "PERF-6 all concurrent manifests must persist every item"
    );
    let owner_session = first_session.expect("PERF-6 must have a workload");
    let wrong_owner_count = sqlx::query(
        "SELECT COUNT(*) AS c
         FROM context_manifests AS manifest
         INNER JOIN context_manifest_items AS item
           ON item.manifest_id = manifest.manifest_id
         WHERE manifest.user_id = ? AND manifest.session_id = ?",
    )
    .bind("perf-user-not-owner")
    .bind(owner_session)
    .fetch_one(pool.get())
    .await
    .expect("PERF-6 wrong-owner query must succeed")
    .try_get::<i64, _>("c")
    .unwrap_or_default();
    assert_eq!(
        wrong_owner_count, 0,
        "PERF-6 must not cross owner boundaries"
    );
    println!(
        "PERF_RESULT benchmark=manifest_multi_session_write users={} sessions={WRITERS} items_per_manifest={ITEMS_PER_MANIFEST} total_items={expected} elapsed_ms={elapsed_ms}",
        users.len(),
    );
    assert!(
        elapsed_ms < 10_000,
        "PERF-6 {WRITERS} multi-user/session manifest writes must complete in <10s, got {elapsed_ms}ms"
    );
}
