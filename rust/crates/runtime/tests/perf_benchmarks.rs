use std::time::Instant;

use astra_core::SharedPool;
use astra_services::runs::ToolOutputBatchItem;
use astra_services::{
    ContextManifestItemWrite, ContextManifestWrite, DatabaseContextManifestStore,
    DatabaseRunStateStore, DatabaseStateProjectionStore,
};
use serde_json::json;
use sqlx::Row;
use uuid::Uuid;

fn require_db_it_env() -> astra_core::MatrixOneSettings {
    let enabled = std::env::var("ASTRA_TEST_DB_IT").unwrap_or_default();
    assert!(
        enabled == "1",
        "set ASTRA_TEST_DB_IT=1 for ignored perf benchmarks; got {enabled:?}"
    );
    astra_core::MatrixOneSettings::from_env()
}

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

async fn insert_completed_run(pool: &SharedPool, user_id: &str, session_id: &str, run_id: &str) {
    sqlx::query(
        "INSERT INTO agent_runs
         (run_id, user_id, session_id, root_run_id, ancestor_path, depth, retry_scope,
          status, last_event_idx, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, 0, 'node', 'completed', -1, NOW(6), NOW(6))",
    )
    .bind(run_id)
    .bind(user_id)
    .bind(session_id)
    .bind(run_id)
    .bind(run_id)
    .execute(pool.get())
    .await
    .expect("perf insert_completed_run must succeed");
}

async fn insert_state_item(
    pool: &SharedPool,
    user_id: &str,
    session_id: &str,
    run_id: &str,
    category: &str,
    item_key: &str,
) {
    sqlx::query(
        "INSERT INTO session_state_items
         (item_id, user_id, session_id, scope, category, item_key, status, priority, source,
          run_id, title, summary_text, payload_json, token_estimate, version, created_at, updated_at)
         VALUES (?, ?, ?, 'session', ?, ?, 'active', 10, 'perf',
                 ?, ?, ?, '{}', 40, 1, NOW(6), NOW(6))",
    )
    .bind(id("state"))
    .bind(user_id)
    .bind(session_id)
    .bind(category)
    .bind(item_key)
    .bind(run_id)
    .bind(format!("{category} {item_key}"))
    .bind(format!("summary {category} {item_key}"))
    .execute(pool.get())
    .await
    .expect("perf insert_state_item must succeed");
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
             FROM session_artifacts FORCE INDEX (idx_session_artifacts_session_kind_created)
             WHERE session_id = ? AND artifact_kind = 'cargo'",
        )
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
async fn perf_benchmark_2_three_stage_retrieval_sla() {
    let pool = setup_pool().await;
    let user_id = id("user");
    let session_id = id("session");
    insert_session(&pool, &user_id, &session_id).await;
    for i in 0..120 {
        sqlx::query(
            "INSERT INTO session_history_chunks
             (chunk_id, user_id, session_id, source_session_id, seq_start, seq_end, chunk_type,
              source_table, source_id, content_text, content_hash, token_estimate, provenance_json, created_at)
             VALUES (?, ?, ?, ?, ?, ?, 'code_decision', 'session_transcript_items',
                     ?, ?, ?, 40, '{}', NOW(6))",
        )
        .bind(id("chunk"))
        .bind(&user_id)
        .bind(&session_id)
        .bind(&session_id)
        .bind(i as i64)
        .bind(i as i64 + 1)
        .bind(format!("turn-{i}"))
        .bind(format!("needle retrieval content row {i}"))
        .bind(id("hash"))
        .execute(pool.get())
        .await
        .expect("PERF-2 history chunk seed must insert");
    }

    let structured = Instant::now();
    let structured_row = sqlx::query(
        "SELECT chunk_id FROM session_history_chunks FORCE INDEX (idx_history_session_seq)
         WHERE session_id = ? AND seq_start <= 42 AND seq_end >= 42 LIMIT 1",
    )
    .bind(&session_id)
    .fetch_optional(pool.get())
    .await
    .expect("PERF-2 structured retrieval query must succeed");
    let structured_ms = millis(structured);
    assert!(
        structured_row.is_some() && structured_ms < 50,
        "PERF-2 structured retrieval must hit <50ms and find a row; row_present={} elapsed={}ms",
        structured_row.is_some(),
        structured_ms
    );

    let fts = Instant::now();
    let fts_row = sqlx::query(
        "SELECT chunk_id FROM session_history_chunks FORCE INDEX (idx_history_user_chunk_created)
         WHERE user_id = ? AND chunk_type = 'code_decision' AND content_text LIKE '%needle retrieval%'
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(&user_id)
    .fetch_optional(pool.get())
    .await
    .expect("PERF-2 FTS fallback query must succeed");
    let fts_ms = millis(fts);
    assert!(
        fts_row.is_some() && fts_ms < 200,
        "PERF-2 FTS retrieval must hit <200ms and find a row; row_present={} elapsed={}ms",
        fts_row.is_some(),
        fts_ms
    );

    let vector = Instant::now();
    let vector_row = sqlx::query(
        "SELECT chunk_id FROM session_history_chunks FORCE INDEX (idx_history_user_chunk_created)
         WHERE user_id = ? AND chunk_type = 'code_decision'
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(&user_id)
    .fetch_optional(pool.get())
    .await
    .expect("PERF-2 vector fallback freshness query must succeed");
    let vector_ms = millis(vector);
    assert!(
        vector_row.is_some() && vector_ms < 500,
        "PERF-2 vector retrieval must hit <500ms and find a row; row_present={} elapsed={}ms",
        vector_row.is_some(),
        vector_ms
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
                output_json: json!({"idx": idx, "line": "slow query", "duration_ms": 123}),
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
         WHERE session_id = ? AND run_id = ?",
    )
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
async fn perf_benchmark_4_compaction_assertions_under_100ms() {
    let pool = setup_pool().await;
    let user_id = id("user");
    let session_id = id("session");
    let run_id = id("run");
    insert_session(&pool, &user_id, &session_id).await;
    insert_completed_run(&pool, &user_id, &session_id, &run_id).await;
    for category in [
        "plan_state",
        "decision",
        "finding",
        "benchmark",
        "citation",
        "todo_state",
        "error_state",
        "delegation_state",
    ] {
        insert_state_item(
            &pool,
            &user_id,
            &session_id,
            &run_id,
            category,
            &format!("{category}-perf"),
        )
        .await;
    }
    DatabaseContextManifestStore::new(pool.clone())
        .save_manifest(
            ContextManifestWrite {
                manifest_id: id("manifest"),
                user_id: user_id.clone(),
                session_id: session_id.clone(),
                run_id: Some(run_id.clone()),
                turn_id: id("compaction-turn"),
                model_provider: "runtime".to_string(),
                model_name: "compaction_engine".to_string(),
                context_window_tokens: 8_000,
                max_output_tokens: 500,
                total_estimated_tokens: 640,
                policy_version: "context_manifest_v1".to_string(),
                tokenizer_id: Some("estimated_v1".to_string()),
                budget_template_id: Some("budget_v1_8k".to_string()),
                turn_intent: Some("compaction".to_string()),
                reason: "post_compaction".to_string(),
                manifest_json: json!({
                    "zones": {
                        "plan_todo": {"used_tokens": 640, "budget_tokens": 800}
                    }
                }),
            },
            vec![ContextManifestItemWrite {
                session_id: session_id.clone(),
                item_order: 0,
                zone: "plan_todo".to_string(),
                source_table: "session_state_items".to_string(),
                source_id: format!("{session_id}:plan"),
                source_hash: None,
                included: true,
                token_estimate: 640,
                budget_tokens: 800,
                reason: "post_compaction".to_string(),
                render_mode: "summary".to_string(),
                raw_ref: Some(format!("conversation_log://{session_id}/compaction")),
            }],
        )
        .await
        .expect("PERF-4 post_compaction manifest seed must satisfy invariant precondition");
    let store = DatabaseStateProjectionStore::new(pool.clone());
    let started = Instant::now();
    let results = store
        .run_compaction_assertions(&user_id, &session_id, &run_id)
        .await
        .expect("PERF-4 compaction invariant SQL must execute");
    let elapsed_ms = millis(started);
    assert!(
        results.iter().all(|(_, violations)| *violations == 0),
        "PERF-4 compaction invariants must all return zero violations, got {results:?}"
    );
    assert!(
        elapsed_ms < 100,
        "PERF-4 all compaction invariant SQL must run in <100ms, got {elapsed_ms}ms"
    );
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1; perf_benchmark"]
async fn perf_benchmark_5_manifest_build_under_100ms() {
    let pool = setup_pool().await;
    let user_id = id("user");
    let session_id = id("session");
    let run_id = id("run");
    insert_session(&pool, &user_id, &session_id).await;
    let weighted_templates = sqlx::query(
        "SELECT COUNT(*) AS c FROM preview_template_registry
         WHERE status = 'active' AND fts_field_weights_json <> '{}'",
    )
    .fetch_one(pool.get())
    .await
    .expect("PERF-5 preview template seed query must succeed")
    .try_get::<i64, _>("c")
    .unwrap_or_default();
    assert!(
        weighted_templates >= 18,
        "PERF-5 preview_template seed must include real fts_field_weights for baseline templates, got {weighted_templates}"
    );
    let store = DatabaseContextManifestStore::new(pool.clone());
    let manifest_id = id("manifest");
    let items = vec![ContextManifestItemWrite {
        session_id: session_id.clone(),
        item_order: 0,
        zone: "recent_tail".to_string(),
        source_table: "session_transcript_items".to_string(),
        source_id: format!("{session_id}:tail"),
        source_hash: None,
        included: true,
        token_estimate: 800,
        budget_tokens: 2_000,
        reason: "normal_turn".to_string(),
        render_mode: "plain_text".to_string(),
        raw_ref: Some(format!("conversation_log://{session_id}/tail")),
    }];
    let started = Instant::now();
    store
        .save_manifest(
            ContextManifestWrite {
                manifest_id: manifest_id.clone(),
                user_id: user_id.clone(),
                session_id: session_id.clone(),
                run_id: Some(run_id.clone()),
                turn_id: id("turn"),
                model_provider: "mock".to_string(),
                model_name: "perf-fixed-llm".to_string(),
                context_window_tokens: 8_000,
                max_output_tokens: 700,
                total_estimated_tokens: 1_200,
                policy_version: "context_manifest_v1".to_string(),
                tokenizer_id: Some("estimated_v1".to_string()),
                budget_template_id: Some("budget_v1_8k".to_string()),
                turn_intent: Some("normal".to_string()),
                reason: "normal_turn".to_string(),
                manifest_json: json!({
                    "zones": {
                        "recent_tail": {"used_tokens": 800, "budget_tokens": 2000}
                    }
                }),
            },
            items,
        )
        .await
        .expect("PERF-5 manifest build/write must succeed");
    let elapsed_ms = millis(started);
    let persisted =
        sqlx::query("SELECT COUNT(*) AS c FROM context_manifests WHERE manifest_id = ?")
            .bind(&manifest_id)
            .fetch_one(pool.get())
            .await
            .expect("PERF-5 manifest persisted query must succeed")
            .try_get::<i64, _>("c")
            .unwrap_or_default();
    assert!(
        persisted == 1,
        "PERF-5 manifest must be persisted exactly once, got {persisted}"
    );
    assert!(
        elapsed_ms < 100,
        "PERF-5 manifest build/write must complete in <100ms, got {elapsed_ms}ms"
    );
}
