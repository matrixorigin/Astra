use astra_runtime::server::artifact_retention_sweeper::run_artifact_retention_gc_once;
use astra_services::{
    DatabaseRunStateStore, budget_for_turn_intent, build_presigned_artifact_download,
    content_hash_with_normalize_version, expired_artifact_placeholder, runs::ToolOutputBatchItem,
};
use serde_json::json;
use sqlx::Row;
use std::time::Instant;
use uuid::Uuid;

fn require_db_it_env() -> astra_core::MatrixOneSettings {
    assert_eq!(
        std::env::var("ASTRA_TEST_DB_IT").as_deref(),
        Ok("1"),
        "set ASTRA_TEST_DB_IT=1 for ignored integration tests"
    );
    astra_core::MatrixOneSettings::from_env()
}

fn strict_online_perf_enabled() -> bool {
    std::env::var("ASTRA_STRICT_ONLINE_PERF").as_deref() == Ok("1")
}

async fn setup_pool() -> astra_core::SharedPool {
    let settings = require_db_it_env();
    let catalog =
        std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG").unwrap_or_else(|_| "mysql".into());
    astra_services::ensure_core_schema(&settings, &catalog)
        .await
        .expect("ensure_core_schema; is MatrixOne up?");
    astra_core::SharedPool::new(&settings)
        .await
        .expect("SharedPool::new")
}

fn ids() -> (String, String, String) {
    let suffix = Uuid::new_v4();
    (
        format!("phase6-user-{suffix}"),
        format!("phase6-session-{suffix}"),
        format!("phase6-run-{suffix}"),
    )
}

async fn insert_session(pool: &astra_core::SharedPool, user_id: &str, session_id: &str) {
    sqlx::query(
        "INSERT INTO agent_sessions
         (session_id, user_id, agent_id, title, status, metadata, created_at, updated_at)
         VALUES (?, ?, 'phase6-agent', 'phase6 session', 'active', '{}', NOW(6), NOW(6))",
    )
    .bind(session_id)
    .bind(user_id)
    .execute(pool.get())
    .await
    .unwrap();
}

struct ArtifactSeed<'a> {
    user_id: &'a str,
    session_id: &'a str,
    artifact_id: &'a str,
    kind: &'a str,
    policy: &'a str,
    status: &'a str,
    retention_days: i64,
    manifest_refs: i64,
}

async fn insert_artifact(pool: &astra_core::SharedPool, seed: ArtifactSeed<'_>) {
    let retention_until =
        (chrono::Utc::now() + chrono::Duration::days(seed.retention_days)).naive_utc();
    sqlx::query(
        "INSERT INTO session_artifacts
         (artifact_id, session_id, user_id, artifact_kind, content_json, metadata,
          retention_policy, retention_until, status, referenced_by_manifest_count,
         created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW(6), NOW(6))",
    )
    .bind(seed.artifact_id)
    .bind(seed.session_id)
    .bind(seed.user_id)
    .bind(seed.kind)
    .bind(json!({"summary": seed.kind}).to_string())
    .bind(json!({"byte_size": 3_221_225_472_u64}).to_string())
    .bind(seed.policy)
    .bind(retention_until)
    .bind(seed.status)
    .bind(seed.manifest_refs)
    .execute(pool.get())
    .await
    .unwrap();
}

async fn artifact_status(
    pool: &astra_core::SharedPool,
    artifact_id: &str,
) -> (String, Option<String>) {
    let row =
        sqlx::query("SELECT status, cold_storage_ref FROM session_artifacts WHERE artifact_id = ?")
            .bind(artifact_id)
            .fetch_one(pool.get())
            .await
            .unwrap();
    (
        row.try_get("status").unwrap(),
        row.try_get::<Option<String>, _>("cold_storage_ref")
            .unwrap_or(None),
    )
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_50_gc_archives_or_extends_artifacts_with_active_references() {
    let pool = setup_pool().await;
    let (user_id, session_id, _) = ids();
    insert_session(&pool, &user_id, &session_id).await;
    let artifact_id = format!("artifact-{}", Uuid::new_v4());
    insert_artifact(
        &pool,
        ArtifactSeed {
            user_id: &user_id,
            session_id: &session_id,
            artifact_id: &artifact_id,
            kind: "pg_dump",
            policy: "default",
            status: "active",
            retention_days: 1,
            manifest_refs: 1,
        },
    )
    .await;

    let outcome = run_artifact_retention_gc_once(pool.clone(), 100)
        .await
        .unwrap();
    let (status, cold_ref) = artifact_status(&pool, &artifact_id).await;
    assert!(outcome.scanned >= 1);
    assert_eq!(status, "active");
    assert!(
        cold_ref
            .as_deref()
            .is_some_and(|value| value.starts_with("cold_storage://")),
        "referenced artifact should move to cold storage, got {cold_ref:?}"
    );
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_51_unknown_tool_uses_400b_fallback_and_writes_warning_event() {
    let pool = setup_pool().await;
    let (user_id, session_id, run_id) = ids();
    insert_session(&pool, &user_id, &session_id).await;
    let store = DatabaseRunStateStore::new(pool.clone());
    let tool_name = format!("unknown_phase6_{}", Uuid::new_v4().simple());
    store
        .insert_tool_output_batch(
            &format!("batch-{}", Uuid::new_v4()),
            &session_id,
            &run_id,
            &user_id,
            &[ToolOutputBatchItem {
                output_id: format!("out-{}", Uuid::new_v4()),
                tool_call_id: Some("call-1".to_string()),
                tool_name: tool_name.clone(),
                output_json: json!({"text": "x".repeat(1_200)}),
            }],
        )
        .await
        .unwrap();
    let row = sqlx::query(
        "SELECT
           (SELECT preview_status FROM session_tool_outputs WHERE tool_name = ? LIMIT 1) AS preview_status,
           (SELECT CHAR_LENGTH(preview_text) FROM session_tool_outputs WHERE tool_name = ? LIMIT 1) AS preview_len,
           (SELECT COUNT(*) FROM agent_events WHERE session_id = ? AND event_type = 'preview_template_missing' AND meta_tool_name = ?) AS warning_count",
    )
    .bind(&tool_name)
    .bind(&tool_name)
    .bind(&session_id)
    .bind(&tool_name)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(
        row.try_get::<String, _>("preview_status").unwrap(),
        "fallback"
    );
    assert!(row.try_get::<u64, _>("preview_len").unwrap() <= 400);
    assert_eq!(row.try_get::<i64, _>("warning_count").unwrap(), 1);
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_52_preview_template_normalize_versions_are_seeded_and_deterministic() {
    let pool = setup_pool().await;
    let rows = sqlx::query(
        "SELECT tool_name, normalize_version FROM preview_template_registry
         WHERE tool_name IN ('pg_dump', 'fetch_url', 'parse_pdf', 'SKILL.md')
           AND status = 'active'",
    )
    .fetch_all(pool.get())
    .await
    .unwrap();
    let seeded = rows
        .into_iter()
        .map(|row| {
            (
                row.try_get::<String, _>("tool_name").unwrap(),
                row.try_get::<String, _>("normalize_version").unwrap(),
            )
        })
        .collect::<std::collections::HashMap<_, _>>();
    for expected in ["pg_dump", "fetch_url", "parse_pdf", "SKILL.md"] {
        let normalize_version = seeded.get(expected).expect("baseline template missing");
        let a = content_hash_with_normalize_version("sha256:content", Some(normalize_version));
        let b = content_hash_with_normalize_version("sha256:content", Some(normalize_version));
        assert_eq!(a, b);
        assert!(a.starts_with("sha256:"));
    }
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_53_large_pg_dump_uses_artifact_ref_and_never_prompt_raw_payload() {
    let pool = setup_pool().await;
    let (user_id, session_id, run_id) = ids();
    insert_session(&pool, &user_id, &session_id).await;
    let store = DatabaseRunStateStore::new(pool.clone());
    let output_id = format!("out-{}", Uuid::new_v4());
    let raw = "CREATE TABLE t(a INT);\n".repeat(2_000);
    store
        .insert_tool_output_batch(
            &format!("batch-{}", Uuid::new_v4()),
            &session_id,
            &run_id,
            &user_id,
            &[ToolOutputBatchItem {
                output_id: output_id.clone(),
                tool_call_id: Some("call-pg".to_string()),
                tool_name: "pg_dump".to_string(),
                output_json: json!({"declared_size_bytes": 3_221_225_472_u64, "raw": raw}),
            }],
        )
        .await
        .unwrap();
    let row = sqlx::query(
        "SELECT artifact_ref, CHAR_LENGTH(preview_text) AS preview_len, payload_bytes
         FROM session_tool_outputs WHERE output_id = ?",
    )
    .bind(&output_id)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert!(
        row.try_get::<Option<String>, _>("artifact_ref")
            .unwrap()
            .as_deref()
            .is_some_and(|value| value.starts_with("tool_output://"))
    );
    assert!(row.try_get::<u64, _>("preview_len").unwrap() <= 1000);
    assert!(row.try_get::<i64, _>("payload_bytes").unwrap() > 1000);
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_54_project_long_term_artifact_is_extended_not_expired() {
    let pool = setup_pool().await;
    let (user_id, session_id, _) = ids();
    insert_session(&pool, &user_id, &session_id).await;
    let artifact_id = format!("artifact-{}", Uuid::new_v4());
    insert_artifact(
        &pool,
        ArtifactSeed {
            user_id: &user_id,
            session_id: &session_id,
            artifact_id: &artifact_id,
            kind: "benchmark",
            policy: "project_long_term",
            status: "active",
            retention_days: -1,
            manifest_refs: 0,
        },
    )
    .await;
    run_artifact_retention_gc_once(pool.clone(), 100)
        .await
        .unwrap();
    let (status, _) = artifact_status(&pool, &artifact_id).await;
    assert_eq!(status, "active");
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_55_presigned_download_contains_ttl_and_signature() {
    let signed = build_presigned_artifact_download(
        "/sessions/s/artifacts/a/download/presigned",
        "user-1",
        "session-1",
        "artifact-1",
        "secret",
        chrono::Utc::now(),
        300,
    );
    assert_eq!(signed.method, "GET");
    assert!(signed.download_url.contains("expires_at="));
    assert!(
        signed.download_url.contains("signature=sha256%3A")
            || signed.download_url.contains("signature=sha256:")
    );
    assert!(signed.signature.starts_with("sha256:"));
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_56_expired_artifact_renders_historical_placeholder() {
    let placeholder = expired_artifact_placeholder("artifact-x", Some("row count preserved"));
    assert!(placeholder.contains("historical, raw no longer available"));
    assert!(placeholder.contains("summary preserved"));
    assert!(placeholder.contains("row count preserved"));
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_57_benchmark_comparison_expands_tool_previews_from_recent_tail() {
    let normal = budget_for_turn_intent(Some("normal"));
    let benchmark = budget_for_turn_intent(Some("benchmark_comparison"));
    assert!(!normal.flex_applied);
    assert!(benchmark.flex_applied);
    assert_eq!(benchmark.budget.tool_previews, 2_500);
    assert_eq!(benchmark.budget.recent_tail, 1_600);
    assert_eq!(
        benchmark.borrowed_from_recent_tail,
        normal.budget.recent_tail - benchmark.budget.recent_tail
    );
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l3_17_s08_dba_audit_large_artifacts_batch_and_gc() {
    let pool = setup_pool().await;
    let (user_id, session_id, run_id) = ids();
    insert_session(&pool, &user_id, &session_id).await;
    let artifact_ids = ["pg-dump", "tool-batch", "slowlog"]
        .into_iter()
        .map(|kind| format!("{kind}-{}", Uuid::new_v4()))
        .collect::<Vec<_>>();
    for (idx, artifact_id) in artifact_ids.iter().enumerate() {
        insert_artifact(
            &pool,
            ArtifactSeed {
                user_id: &user_id,
                session_id: &session_id,
                artifact_id,
                kind: if idx == 2 { "slowlog" } else { "pg_dump" },
                policy: "default",
                status: "active",
                retention_days: 1,
                manifest_refs: if idx == 0 { 1 } else { 0 },
            },
        )
        .await;
    }

    let store = DatabaseRunStateStore::new(pool.clone());
    let mut items = Vec::with_capacity(500);
    for i in 0..1_000 {
        items.push(ToolOutputBatchItem {
            output_id: format!("out-{i}-{}", Uuid::new_v4()),
            tool_call_id: Some(format!("call-{i}")),
            tool_name: "slow_query_analyzer".to_string(),
            output_json: json!({"idx": i, "declared_size_bytes": 838_860_800_u64, "line": "slow query"}),
        });
        if items.len() == 500 {
            store
                .insert_tool_output_batch(
                    &format!("batch-{i}-{}", Uuid::new_v4()),
                    &session_id,
                    &run_id,
                    &user_id,
                    &items,
                )
                .await
                .unwrap();
            items.clear();
        }
    }
    let started = Instant::now();
    let row = sqlx::query(
        "SELECT
          (SELECT COUNT(*) FROM session_tool_outputs FORCE INDEX (idx_tool_outputs_run_created) WHERE run_id = ?) AS output_count,
          (SELECT COUNT(*) FROM session_artifacts FORCE INDEX (idx_session_artifacts_session_kind_created) WHERE session_id = ? AND artifact_kind = 'pg_dump') AS dump_count,
          (SELECT COUNT(*) FROM session_artifacts FORCE INDEX (idx_artifacts_retention) WHERE status IN ('active', 'expiring') AND retention_until IS NOT NULL) AS retention_count",
    )
    .bind(&run_id)
    .bind(&session_id)
    .fetch_one(pool.get())
    .await
    .unwrap();
    let query_ms = started.elapsed().as_millis();
    assert_eq!(row.try_get::<i64, _>("output_count").unwrap(), 1_000);
    assert!(row.try_get::<i64, _>("dump_count").unwrap() >= 2);
    assert!(row.try_get::<i64, _>("retention_count").unwrap() >= 1);
    let max_query_ms = if strict_online_perf_enabled() {
        50
    } else {
        500
    };
    assert!(
        query_ms < max_query_ms,
        "indexed artifact/tool queries took {query_ms}ms; limit={max_query_ms}ms"
    );

    run_artifact_retention_gc_once(pool.clone(), 100)
        .await
        .unwrap();
    let (_, cold_ref) = artifact_status(&pool, &artifact_ids[0]).await;
    assert!(
        cold_ref.is_some(),
        "referenced pg_dump artifact should be preserved via cold storage"
    );
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l3_18_s12_14_day_review_retention_and_benchmark_budget_flex() {
    let pool = setup_pool().await;
    let (user_id, session_id, _) = ids();
    insert_session(&pool, &user_id, &session_id).await;
    for i in 0..250 {
        let policy = if i % 25 == 0 {
            "project_long_term"
        } else {
            "default"
        };
        let artifact_id = format!("review-artifact-{i}-{}", Uuid::new_v4());
        insert_artifact(
            &pool,
            ArtifactSeed {
                user_id: &user_id,
                session_id: &session_id,
                artifact_id: &artifact_id,
                kind: if i % 2 == 0 { "fetch_url" } else { "parse_pdf" },
                policy,
                status: "active",
                retention_days: (i % 14) as i64 - 7,
                manifest_refs: 0,
            },
        )
        .await;
    }
    run_artifact_retention_gc_once(pool.clone(), 500)
        .await
        .unwrap();
    let row = sqlx::query(
        "SELECT
           COUNT(*) AS total_artifacts,
           SUM(CASE WHEN retention_policy = 'project_long_term' AND status = 'active' THEN 1 ELSE 0 END) AS long_term_active
         FROM session_artifacts WHERE session_id = ?",
    )
    .bind(&session_id)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(row.try_get::<i64, _>("total_artifacts").unwrap(), 250);
    assert_eq!(row.try_get::<i64, _>("long_term_active").unwrap(), 10);
    let budget = budget_for_turn_intent(Some("benchmark_comparison"));
    assert_eq!(budget.budget.tool_previews, 2_500);
    assert!(budget.borrowed_from_recent_tail > 0);
}
