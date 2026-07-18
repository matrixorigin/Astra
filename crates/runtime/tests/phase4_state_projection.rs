use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use astra_services::{
    BubbleUpTarget, COMPACTION_INVARIANT_SQL, DatabaseRunStateStore, DatabaseStateProjectionStore,
    DelegationProjectionUpsert, SkillActivationLlmProbe, StateProjectionError,
};
use serde_json::json;
use sqlx::Row;
use uuid::Uuid;

fn require_db_it_env() -> astra_core::MatrixOneSettings {
    assert_eq!(
        std::env::var("ASTRA_TEST_DB_IT").as_deref(),
        Ok("1"),
        "set ASTRA_TEST_DB_IT=1 for ignored integration tests"
    );
    astra_core::MatrixOneSettings::from_env()
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
        format!("session-{suffix}"),
        format!("user-{suffix}"),
        format!("run-{suffix}"),
    )
}

async fn insert_session(pool: &astra_core::SharedPool, session_id: &str, user_id: &str) {
    sqlx::query(
        "INSERT INTO agent_sessions
         (session_id, user_id, agent_id, title, status, metadata, created_at, updated_at)
         VALUES (?, ?, 'phase4-agent', 'phase4 session', 'active', '{}', NOW(6), NOW(6))",
    )
    .bind(session_id)
    .bind(user_id)
    .execute(pool.get())
    .await
    .unwrap();
}

#[allow(clippy::too_many_arguments)]
async fn insert_run(
    pool: &astra_core::SharedPool,
    session_id: &str,
    user_id: &str,
    run_id: &str,
    parent_run_id: Option<&str>,
    root_run_id: &str,
    ancestor_path: &str,
    depth: i64,
    status: &str,
) {
    sqlx::query(
        "INSERT INTO agent_runs
         (run_id, user_id, session_id, parent_run_id, root_run_id, ancestor_path, depth,
          retry_scope, status, last_event_idx, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, 'node', ?, -1, NOW(6), NOW(6))",
    )
    .bind(run_id)
    .bind(user_id)
    .bind(session_id)
    .bind(parent_run_id)
    .bind(root_run_id)
    .bind(ancestor_path)
    .bind(depth)
    .bind(status)
    .execute(pool.get())
    .await
    .unwrap();
}

#[allow(clippy::too_many_arguments)]
async fn insert_state_item(
    pool: &astra_core::SharedPool,
    session_id: &str,
    user_id: &str,
    scope: &str,
    category: &str,
    item_key: &str,
    status: &str,
    version: i64,
    token_estimate: i64,
) -> String {
    let item_id = format!("state-{category}-{item_key}-{}", Uuid::new_v4());
    sqlx::query(
        "INSERT INTO session_state_items
         (item_id, user_id, session_id, scope, category, item_key, status, priority, source,
          title, summary_text, payload_json, token_estimate, version, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, 10, 'phase4-test', ?, ?, '{}', ?, ?, NOW(6), NOW(6))",
    )
    .bind(&item_id)
    .bind(user_id)
    .bind(session_id)
    .bind(scope)
    .bind(category)
    .bind(item_key)
    .bind(status)
    .bind(format!("{category} {item_key}"))
    .bind(format!("summary for {category} {item_key}"))
    .bind(token_estimate)
    .bind(version)
    .execute(pool.get())
    .await
    .unwrap();
    item_id
}

async fn explain_analyze_text(pool: &astra_core::SharedPool, sql: &str) -> String {
    let rows = sqlx::raw_sql(sql).fetch_all(pool.get()).await.unwrap();
    let mut text = String::new();
    for row in rows {
        for idx in 0..row.columns().len() {
            if let Ok(value) = row.try_get::<String, _>(idx) {
                text.push_str(&value);
                text.push('\n');
            } else if let Ok(value) = row.try_get::<i64, _>(idx) {
                text.push_str(&value.to_string());
                text.push('\n');
            }
        }
    }
    text
}

async fn index_columns(pool: &astra_core::SharedPool, table: &str, key: &str) -> Vec<String> {
    let schema = sqlx::query("SELECT DATABASE() AS schema_name")
        .fetch_one(pool.get())
        .await
        .unwrap()
        .try_get::<String, _>("schema_name")
        .unwrap();
    sqlx::query(
        "SELECT COLUMN_NAME
         FROM information_schema.STATISTICS
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND INDEX_NAME = ?
         ORDER BY SEQ_IN_INDEX",
    )
    .bind(schema)
    .bind(table)
    .bind(key)
    .fetch_all(pool.get())
    .await
    .unwrap()
    .into_iter()
    .map(|row| row.try_get::<String, _>("COLUMN_NAME").unwrap())
    .collect()
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_32_compaction_invariants_return_zero_after_compaction() {
    let pool = setup_pool().await;
    let (session_id, user_id, run_id) = ids();
    insert_session(&pool, &session_id, &user_id).await;
    insert_run(
        &pool,
        &session_id,
        &user_id,
        &run_id,
        None,
        &run_id,
        &run_id,
        0,
        "completed",
    )
    .await;
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
            &session_id,
            &user_id,
            "session",
            category,
            &format!("{category}-key"),
            "active",
            1,
            20,
        )
        .await;
    }
    let store = DatabaseStateProjectionStore::new(pool.clone());
    let results = store
        .compact_session_state(&user_id, &session_id, &run_id, 640)
        .await
        .unwrap();
    assert_eq!(results.len(), COMPACTION_INVARIANT_SQL.len());
    assert!(results.iter().all(|(_, violations)| *violations == 0));
    let plan = explain_analyze_text(
        &pool,
        &format!(
            "EXPLAIN ANALYZE SELECT item_id FROM session_state_items FORCE INDEX (idx_state_owner_session_status_category) \
             WHERE user_id = '{}' AND session_id = '{}' AND category = 'plan_state' AND status = 'active' ORDER BY updated_at DESC LIMIT 5",
            user_id, session_id
        ),
    )
    .await;
    assert!(
        plan.contains("session_state_items"),
        "query was not analyzed:\n{plan}"
    );
    assert_eq!(
        index_columns(
            &pool,
            "session_state_items",
            "idx_state_owner_session_status_category"
        )
        .await,
        ["user_id", "session_id", "status", "category"],
        "state lookup index must preserve owner/session/status/category ordering"
    );
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_33_active_structured_state_survives_compaction() {
    let pool = setup_pool().await;
    let (session_id, user_id, run_id) = ids();
    insert_session(&pool, &session_id, &user_id).await;
    insert_run(
        &pool,
        &session_id,
        &user_id,
        &run_id,
        None,
        &run_id,
        &run_id,
        0,
        "completed",
    )
    .await;
    let categories = [
        "plan_state",
        "decision",
        "finding",
        "benchmark",
        "citation",
        "todo_state",
        "error_state",
        "delegation_state",
    ];
    for category in categories {
        insert_state_item(
            &pool,
            &session_id,
            &user_id,
            "session",
            category,
            &format!("active-{category}"),
            "active",
            3,
            24,
        )
        .await;
    }
    DatabaseStateProjectionStore::new(pool.clone())
        .compact_session_state(&user_id, &session_id, &run_id, 500)
        .await
        .unwrap();
    let active_count = sqlx::query(
        "SELECT COUNT(*) AS c FROM session_state_items
         WHERE session_id = ? AND user_id = ? AND status = 'active'
           AND category IN ('plan_state','decision','finding','benchmark','citation',
                            'todo_state','error_state','delegation_state')",
    )
    .bind(&session_id)
    .bind(&user_id)
    .fetch_one(pool.get())
    .await
    .unwrap()
    .try_get::<i64, _>("c")
    .unwrap();
    assert_eq!(active_count, categories.len() as i64);
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_34_plan_state_version_does_not_bump_during_compaction() {
    let pool = setup_pool().await;
    let (session_id, user_id, run_id) = ids();
    insert_session(&pool, &session_id, &user_id).await;
    insert_run(
        &pool,
        &session_id,
        &user_id,
        &run_id,
        None,
        &run_id,
        &run_id,
        0,
        "completed",
    )
    .await;
    insert_state_item(
        &pool,
        &session_id,
        &user_id,
        "session",
        "plan_state",
        "active-plan",
        "active",
        7,
        64,
    )
    .await;
    DatabaseStateProjectionStore::new(pool.clone())
        .compact_session_state(&user_id, &session_id, &run_id, 480)
        .await
        .unwrap();
    let version = sqlx::query(
        "SELECT version FROM session_state_items
         WHERE session_id = ? AND user_id = ? AND category = 'plan_state' AND item_key = 'active-plan'",
    )
    .bind(&session_id)
    .bind(&user_id)
    .fetch_one(pool.get())
    .await
    .unwrap()
    .try_get::<i64, _>("version")
    .unwrap();
    assert_eq!(version, 7);
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_35_compaction_rejects_running_or_waiting_runs() {
    let pool = setup_pool().await;
    let (session_id, user_id, run_id) = ids();
    insert_session(&pool, &session_id, &user_id).await;
    insert_run(
        &pool,
        &session_id,
        &user_id,
        &run_id,
        None,
        &run_id,
        &run_id,
        0,
        "running",
    )
    .await;
    let error = DatabaseStateProjectionStore::new(pool)
        .compact_session_state(&user_id, &session_id, &run_id, 320)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        StateProjectionError::ActiveRunCompaction { .. }
    ));
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_36_delegation_projection_and_retry_supersede_are_transactional() {
    let pool = setup_pool().await;
    let (session_id, user_id, root_run_id) = ids();
    let child_run_id = format!("child-{root_run_id}");
    let retry_run_id = format!("retry-{root_run_id}");
    insert_session(&pool, &session_id, &user_id).await;
    insert_run(
        &pool,
        &session_id,
        &user_id,
        &root_run_id,
        None,
        &root_run_id,
        &root_run_id,
        0,
        "completed",
    )
    .await;
    insert_run(
        &pool,
        &session_id,
        &user_id,
        &child_run_id,
        Some(&root_run_id),
        &root_run_id,
        &format!("{root_run_id}/{child_run_id}"),
        1,
        "failed",
    )
    .await;
    let store = DatabaseStateProjectionStore::new(pool.clone());
    store
        .upsert_delegation_projection(DelegationProjectionUpsert {
            delegation_id: format!("delegation-{root_run_id}"),
            user_id: user_id.clone(),
            session_id: session_id.clone(),
            parent_run_id: root_run_id.clone(),
            child_run_id: child_run_id.clone(),
            root_run_id: root_run_id.clone(),
            ancestor_path: format!("{root_run_id}/{child_run_id}"),
            depth: 1,
            agent_id: Some("reviewer".to_string()),
            title: Some("Review child".to_string()),
            status: "failed".to_string(),
            retry_of: None,
            retry_scope: "subtree".to_string(),
            last_summary_ref: Some("artifact://summary".to_string()),
            last_summary_text: Some("child failed with blocker".to_string()),
            sibling_exposed_artifacts_json: None,
        })
        .await
        .unwrap();
    store
        .create_retry_run_and_supersede(&user_id, &child_run_id, &retry_run_id, "subtree")
        .await
        .unwrap();

    let row = sqlx::query(
        "SELECT
            (SELECT COUNT(*) FROM session_delegations WHERE child_run_id = ? AND user_id = ?) AS delegation_count,
            (SELECT COUNT(*) FROM session_state_items
             WHERE session_id = ? AND user_id = ? AND category = 'delegation_state') AS state_count,
            (SELECT status FROM agent_runs WHERE run_id = ? AND user_id = ?) AS old_status,
            (SELECT retry_scope FROM agent_runs WHERE run_id = ? AND user_id = ?) AS retry_scope",
    )
    .bind(&child_run_id)
    .bind(&user_id)
    .bind(&session_id)
    .bind(&user_id)
    .bind(&child_run_id)
    .bind(&user_id)
    .bind(&retry_run_id)
    .bind(&user_id)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(row.try_get::<i64, _>("delegation_count").unwrap(), 1);
    assert_eq!(row.try_get::<i64, _>("state_count").unwrap(), 1);
    assert_eq!(
        row.try_get::<String, _>("old_status").unwrap(),
        "superseded"
    );
    assert_eq!(row.try_get::<String, _>("retry_scope").unwrap(), "subtree");
    let plan = explain_analyze_text(
        &pool,
        &format!(
            "EXPLAIN ANALYZE SELECT delegation_id FROM session_delegations FORCE INDEX (idx_delegations_owner_parent_status_updated) \
             WHERE user_id = '{}' AND parent_run_id = '{}' AND status = 'running' ORDER BY updated_at DESC LIMIT 5",
            user_id, root_run_id
        ),
    )
    .await;
    assert!(
        plan.contains("session_delegations"),
        "query was not analyzed:\n{plan}"
    );
    assert_eq!(
        index_columns(
            &pool,
            "session_delegations",
            "idx_delegations_owner_parent_status_updated"
        )
        .await,
        ["user_id", "parent_run_id", "status", "updated_at"],
        "delegation lookup index must preserve owner/parent/status/update ordering"
    );
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn create_retry_run_and_supersede_rejects_wrong_owner_without_mutation() {
    let pool = setup_pool().await;
    let (session_id, user_id, run_id) = ids();
    let retry_run_id = format!("retry-{run_id}");
    let wrong_user_id = format!("wrong-{user_id}");
    insert_session(&pool, &session_id, &user_id).await;
    insert_run(
        &pool,
        &session_id,
        &user_id,
        &run_id,
        None,
        &run_id,
        &run_id,
        0,
        "failed",
    )
    .await;

    let err = DatabaseStateProjectionStore::new(pool.clone())
        .create_retry_run_and_supersede(&wrong_user_id, &run_id, &retry_run_id, "node")
        .await
        .expect_err("wrong owner must not supersede or retry another owner's run");
    assert!(
        err.to_string().contains("load_old_retry_run"),
        "wrong-owner retry should fail at owner-bound old-run load: {err}"
    );

    let row = sqlx::query(
        "SELECT
            (SELECT status FROM agent_runs WHERE user_id = ? AND run_id = ?) AS old_status,
            (SELECT COUNT(*) FROM agent_runs WHERE run_id = ?) AS retry_count",
    )
    .bind(&user_id)
    .bind(&run_id)
    .bind(&retry_run_id)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(row.try_get::<String, _>("old_status").unwrap(), "failed");
    assert_eq!(row.try_get::<i64, _>("retry_count").unwrap(), 0);
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_37_bubble_up_writes_one_event_per_ancestor_layer() {
    let pool = setup_pool().await;
    let (session_id, user_id, root_run_id) = ids();
    insert_session(&pool, &session_id, &user_id).await;
    let targets = (0..5)
        .map(|depth| BubbleUpTarget {
            session_id: session_id.clone(),
            run_id: format!("{root_run_id}-L{depth}"),
            depth,
        })
        .collect::<Vec<_>>();
    DatabaseStateProjectionStore::new(pool.clone())
        .bubble_up_finding(
            &user_id,
            &format!("{root_run_id}-L4"),
            "finding-critical",
            "critical",
            "critical schema drift found",
            &targets,
        )
        .await
        .unwrap();
    let count = sqlx::query(
        "SELECT COUNT(*) AS c FROM session_state_item_events
         WHERE session_id = ? AND user_id = ? AND mutation = 'bubble_up'",
    )
    .bind(&session_id)
    .bind(&user_id)
    .fetch_one(pool.get())
    .await
    .unwrap()
    .try_get::<i64, _>("c")
    .unwrap();
    assert_eq!(count, targets.len() as i64);
    let plan = explain_analyze_text(
        &pool,
        &format!(
            "EXPLAIN ANALYZE SELECT event_id FROM session_state_item_events FORCE INDEX (idx_state_events_owner_session_created) \
             WHERE user_id = '{}' AND session_id = '{}' AND mutation = 'bubble_up' ORDER BY created_at DESC LIMIT 5",
            user_id, session_id
        ),
    )
    .await;
    assert!(
        plan.contains("session_state_item_events"),
        "EXPLAIN ANALYZE should execute the state-event history query, got:\n{plan}"
    );
    assert_eq!(
        index_columns(
            &pool,
            "session_state_item_events",
            "idx_state_events_owner_session_created"
        )
        .await,
        ["user_id", "session_id", "created_at", "event_id"],
        "state-event history index must stay owner/session ordered with event_id tie-breaker"
    );
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_38_same_root_tree_allows_sibling_artifact_access() {
    let pool = setup_pool().await;
    let (session_id, user_id, root_run_id) = ids();
    let dba_run = format!("dba-{root_run_id}");
    let be_run = format!("be-{root_run_id}");
    let artifact_id = format!("artifact-{root_run_id}");
    insert_session(&pool, &session_id, &user_id).await;
    insert_run(
        &pool,
        &session_id,
        &user_id,
        &root_run_id,
        None,
        &root_run_id,
        &root_run_id,
        0,
        "completed",
    )
    .await;
    insert_run(
        &pool,
        &session_id,
        &user_id,
        &dba_run,
        Some(&root_run_id),
        &root_run_id,
        &format!("{root_run_id}/{dba_run}"),
        1,
        "completed",
    )
    .await;
    insert_run(
        &pool,
        &session_id,
        &user_id,
        &be_run,
        Some(&root_run_id),
        &root_run_id,
        &format!("{root_run_id}/{be_run}"),
        1,
        "completed",
    )
    .await;
    sqlx::query(
        "INSERT INTO session_artifacts
         (artifact_id, session_id, user_id, owner_run_id, root_run_id, artifact_kind,
          content_json, access_scope, status, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, 'migration_sql', ?, 'same_root_tree', 'active', NOW(6), NOW(6))",
    )
    .bind(&artifact_id)
    .bind(&session_id)
    .bind(&user_id)
    .bind(&dba_run)
    .bind(&root_run_id)
    .bind(json!({"sql": "ALTER TABLE account ADD COLUMN region TEXT"}).to_string())
    .execute(pool.get())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO session_artifacts_grants
         (grant_id, artifact_id, session_id, user_id, root_run_id, source_run_id,
          target_run_id, grant_scope, granted_by, reason, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, 'run', 'phase4-test', 'phase4 acl explain', NOW(6))",
    )
    .bind(format!("grant-{artifact_id}"))
    .bind(&artifact_id)
    .bind(&session_id)
    .bind(&user_id)
    .bind(&root_run_id)
    .bind(&dba_run)
    .bind(&be_run)
    .execute(pool.get())
    .await
    .unwrap();
    assert!(
        DatabaseStateProjectionStore::new(pool.clone())
            .can_access_artifact(&artifact_id, &user_id, &be_run, None)
            .await
            .unwrap()
    );
    let visible_artifacts: Vec<String> = sqlx::query_scalar(
        "SELECT artifact_id FROM session_artifacts FORCE INDEX (idx_artifacts_root_scope) \
         WHERE user_id = ? AND root_run_id = ? AND access_scope = 'same_root_tree' AND status = 'active' \
         ORDER BY updated_at DESC LIMIT 5",
    )
    .bind(&user_id)
    .bind(&root_run_id)
    .fetch_all(pool.get())
    .await
    .unwrap();
    assert_eq!(visible_artifacts, vec![artifact_id.clone()]);

    let visible_grants: Vec<String> = sqlx::query_scalar(
        "SELECT grant_id FROM session_artifacts_grants FORCE INDEX (idx_artifacts_grants_target) \
         WHERE user_id = ? AND session_id = ? AND target_run_id = ? AND artifact_id = ? \
           AND (expires_at IS NULL OR expires_at > NOW(6)) LIMIT 5",
    )
    .bind(&user_id)
    .bind(&session_id)
    .bind(&be_run)
    .bind(&artifact_id)
    .fetch_all(pool.get())
    .await
    .unwrap();
    assert_eq!(visible_grants, vec![format!("grant-{artifact_id}")]);
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_39_user_scope_memory_loads_into_anchor_budget() {
    let pool = setup_pool().await;
    let (session_id, user_id, _) = ids();
    insert_session(&pool, &session_id, &user_id).await;
    for (idx, tokens) in [120_i64, 160, 180, 90].iter().enumerate() {
        insert_state_item(
            &pool,
            &session_id,
            &user_id,
            "user",
            "engineering_rule",
            &format!("rule-{idx}"),
            "active",
            1,
            *tokens,
        )
        .await;
    }
    let items = DatabaseStateProjectionStore::new(pool)
        .load_user_anchor_memory(&user_id, 400)
        .await
        .unwrap();
    let total = items.iter().map(|item| item.token_estimate).sum::<u32>();
    assert!(total <= 400);
    assert!(!items.is_empty());
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_40_cross_session_history_query_uses_user_chunk_created_index() {
    let pool = setup_pool().await;
    let (session_id, user_id, _) = ids();
    let source_session_id = format!("source-{session_id}");
    insert_session(&pool, &session_id, &user_id).await;
    sqlx::query(
        "INSERT INTO session_history_chunks
         (chunk_id, user_id, session_id, source_session_id, seq_start, seq_end,
          chunk_type, source_table, source_id, content_text, content_hash, token_estimate,
          provenance_json, created_at)
         VALUES (?, ?, ?, ?, 1, 5, 'decision', 'agent_events', 'event-1',
                 'historic decision about index use', 'sha256:phase4', 120, ?, NOW(6))",
    )
    .bind(format!("chunk-{session_id}"))
    .bind(&user_id)
    .bind(&session_id)
    .bind(&source_session_id)
    .bind(json!({"source_session_id": source_session_id}).to_string())
    .execute(pool.get())
    .await
    .unwrap();
    let plan = explain_analyze_text(
        &pool,
        &format!(
            "EXPLAIN ANALYZE SELECT chunk_id FROM session_history_chunks FORCE INDEX (idx_history_user_chunk_created) \
             WHERE user_id = '{}' AND chunk_type = 'decision' ORDER BY created_at DESC LIMIT 5",
            user_id
        ),
    )
    .await;
    assert!(
        plan.contains("session_history_chunks"),
        "query was not analyzed:\n{plan}"
    );
    assert_eq!(
        index_columns(
            &pool,
            "session_history_chunks",
            "idx_history_user_chunk_created"
        )
        .await,
        ["user_id", "chunk_type", "created_at"],
        "history lookup index must preserve owner/type/time ordering"
    );
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_41_personal_skill_activation_pins_frozen_version_id() {
    let pool = setup_pool().await;
    let (session_id, user_id, _) = ids();
    insert_session(&pool, &session_id, &user_id).await;
    let version_id = format!("version-{}", Uuid::new_v4());
    DatabaseStateProjectionStore::new(pool.clone())
        .activate_personal_skill_from_ui(&user_id, &session_id, "review_changes", &version_id)
        .await
        .unwrap();
    let payload = sqlx::query(
        "SELECT payload_json FROM session_state_items
         WHERE session_id = ? AND user_id = ? AND category = 'active_skill' AND item_key = 'review_changes'",
    )
    .bind(&session_id)
    .bind(&user_id)
    .fetch_one(pool.get())
    .await
    .unwrap()
    .try_get::<String, _>("payload_json")
    .unwrap();
    assert!(payload.contains(&version_id));
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_42_skill_activation_is_ui_structured_event_not_llm_turn() {
    let pool = setup_pool().await;
    let (session_id, user_id, _) = ids();
    insert_session(&pool, &session_id, &user_id).await;
    #[derive(Default)]
    struct CountingLlmProbe(AtomicUsize);
    impl SkillActivationLlmProbe for CountingLlmProbe {
        fn record_llm_call(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let probe = CountingLlmProbe::default();
    DatabaseStateProjectionStore::new(pool.clone())
        .activate_personal_skill_from_ui_with_probe(
            &user_id,
            &session_id,
            "debugger",
            "version-fixed",
            Some(&probe),
        )
        .await
        .unwrap();
    let row = sqlx::query(
        "SELECT
          (SELECT COUNT(*) FROM agent_events
           WHERE session_id = ? AND user_id = ? AND event_type = 'ui.skill.activate' AND llm_model_used IS NULL) AS ui_events,
          (SELECT COUNT(*) FROM session_state_item_events
           WHERE session_id = ? AND user_id = ? AND category = 'active_skill' AND mutation = 'activate') AS state_events",
    )
    .bind(&session_id)
    .bind(&user_id)
    .bind(&session_id)
    .bind(&user_id)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(row.try_get::<i64, _>("ui_events").unwrap(), 1);
    assert_eq!(row.try_get::<i64, _>("state_events").unwrap(), 1);
    assert_eq!(
        probe.0.load(Ordering::SeqCst),
        0,
        "UI structured skill activation must not call an LLM client"
    );
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l3_11b_real_run_engine_populates_projection() {
    let pool = setup_pool().await;
    let (session_id, user_id, root_run_id) = ids();
    insert_session(&pool, &session_id, &user_id).await;
    let child_run_id = format!("child-{}", Uuid::new_v4());
    let delegation_id = format!("delegation-{}", Uuid::new_v4());
    let projection_store = Arc::new(DatabaseStateProjectionStore::new(pool.clone()));
    let run_store = Arc::new(DatabaseRunStateStore::new(pool.clone()));
    let run_engine = astra_runtime::server::run::engine::RunEngine::new(run_store)
        .with_projection_store(projection_store);

    run_engine
        .start_run(&root_run_id, &user_id, &session_id)
        .await
        .unwrap();
    run_engine
        .start_run_ext(
            &child_run_id,
            &user_id,
            &session_id,
            Some(&root_run_id),
            Some(&delegation_id),
            Some("coder"),
            None,
        )
        .await
        .unwrap();
    run_engine
        .persist_status(
            &user_id,
            &child_run_id,
            "completed",
            None,
            Some("child run completed"),
        )
        .await
        .unwrap();

    let row = sqlx::query(
        "SELECT
          (SELECT COUNT(*) FROM session_delegations
           WHERE delegation_id = ? AND child_run_id = ? AND user_id = ? AND status = 'completed') AS delegations,
          (SELECT COUNT(*) FROM session_state_items
           WHERE session_id = ? AND user_id = ? AND category = 'delegation_state' AND item_key = ?) AS state_items",
    )
    .bind(&delegation_id)
    .bind(&child_run_id)
    .bind(&user_id)
    .bind(&session_id)
    .bind(&user_id)
    .bind(&delegation_id)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(row.try_get::<i64, _>("delegations").unwrap(), 1);
    assert_eq!(row.try_get::<i64, _>("state_items").unwrap(), 1);
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn delegation_projection_refresh_uses_current_run_status() {
    let pool = setup_pool().await;
    let (session_id, user_id, root_run_id) = ids();
    insert_session(&pool, &session_id, &user_id).await;
    let child_run_id = format!("child-{}", Uuid::new_v4());
    let delegation_id = format!("delegation-{}", Uuid::new_v4());
    let projection_store = Arc::new(DatabaseStateProjectionStore::new(pool.clone()));
    let run_store = Arc::new(DatabaseRunStateStore::new(pool.clone()));
    let run_engine = astra_runtime::server::run::engine::RunEngine::new(run_store)
        .with_projection_store(projection_store.clone());

    run_engine
        .start_run(&root_run_id, &user_id, &session_id)
        .await
        .unwrap();
    run_engine
        .start_run_ext(
            &child_run_id,
            &user_id,
            &session_id,
            Some(&root_run_id),
            Some(&delegation_id),
            Some("coder"),
            None,
        )
        .await
        .unwrap();
    run_engine
        .persist_status(&user_id, &child_run_id, "completed", None, None)
        .await
        .unwrap();

    sqlx::query(
        "UPDATE session_delegations
         SET status = 'running'
         WHERE delegation_id = ? AND user_id = ?",
    )
    .bind(&delegation_id)
    .bind(&user_id)
    .execute(pool.get())
    .await
    .unwrap();
    sqlx::query(
        "UPDATE session_state_items
         SET status = 'running'
         WHERE session_id = ? AND user_id = ? AND category = 'delegation_state' AND item_key = ?",
    )
    .bind(&session_id)
    .bind(&user_id)
    .bind(&delegation_id)
    .execute(pool.get())
    .await
    .unwrap();

    projection_store
        .upsert_delegation_projection_for_run(
            &user_id,
            &child_run_id,
            Some("coder"),
            Some("late projection refresh"),
        )
        .await
        .unwrap();

    let row = sqlx::query(
        "SELECT
          (SELECT status FROM session_delegations WHERE delegation_id = ? AND user_id = ?) AS delegation_status,
          (SELECT status FROM session_state_items
           WHERE session_id = ? AND user_id = ? AND category = 'delegation_state' AND item_key = ?) AS state_status",
    )
    .bind(&delegation_id)
    .bind(&user_id)
    .bind(&session_id)
    .bind(&user_id)
    .bind(&delegation_id)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(
        row.try_get::<String, _>("delegation_status").unwrap(),
        "completed"
    );
    assert_eq!(
        row.try_get::<String, _>("state_status").unwrap(),
        "completed"
    );
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn compact_session_state_rejects_active_runs_from_same_owner() {
    let pool = setup_pool().await;
    let (session_id, user_id, run_id) = ids();
    let other_user_id = format!("other-{user_id}");
    let other_run_id = format!("other-{run_id}");
    let compaction_run_id = format!("compaction-{run_id}");
    insert_session(&pool, &session_id, &user_id).await;
    insert_run(
        &pool,
        &session_id,
        &other_user_id,
        &other_run_id,
        None,
        &other_run_id,
        &other_run_id,
        0,
        "running",
    )
    .await;

    let store = DatabaseStateProjectionStore::new(pool.clone());
    // Other-owner active runs must not block compaction.
    store
        .compact_session_state(&user_id, &session_id, &compaction_run_id, 400)
        .await
        .expect("other-owner active runs must not block owner compaction");

    // Same-owner active run should block compaction.
    let compaction_run_id2 = format!("compaction2-{run_id}");
    let err = store
        .compact_session_state(&other_user_id, &session_id, &compaction_run_id2, 400)
        .await
        .expect_err("the other owner still has an active run in that session id");
    assert!(
        err.to_string().contains(&session_id),
        "compaction denial should include the session id for observability: {err}"
    );
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l3_13_s09_sibling_be_agent_reads_dba_migration_artifact() {
    let pool = setup_pool().await;
    let (session_id, user_id, root_run_id) = ids();
    let dba_run = format!("dba-agent-{root_run_id}");
    let be_run = format!("be-agent-{root_run_id}");
    let artifact_id = format!("migration-{root_run_id}");
    insert_session(&pool, &session_id, &user_id).await;
    insert_run(
        &pool,
        &session_id,
        &user_id,
        &root_run_id,
        None,
        &root_run_id,
        &root_run_id,
        0,
        "completed",
    )
    .await;
    for run_id in [&dba_run, &be_run] {
        insert_run(
            &pool,
            &session_id,
            &user_id,
            run_id,
            Some(&root_run_id),
            &root_run_id,
            &format!("{root_run_id}/{run_id}"),
            1,
            "completed",
        )
        .await;
    }
    let migration_sql = "ALTER TABLE orders ADD COLUMN shard_key VARCHAR(64)";
    sqlx::query(
        "INSERT INTO session_artifacts
         (artifact_id, session_id, user_id, owner_run_id, root_run_id, artifact_kind,
          content_json, access_scope, status, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, 'migration_sql', ?, 'same_root_tree', 'active', NOW(6), NOW(6))",
    )
    .bind(&artifact_id)
    .bind(&session_id)
    .bind(&user_id)
    .bind(&dba_run)
    .bind(&root_run_id)
    .bind(json!({"sql": migration_sql, "owner": "dba-agent"}).to_string())
    .execute(pool.get())
    .await
    .unwrap();
    let store = DatabaseStateProjectionStore::new(pool.clone());
    assert!(
        store
            .can_access_artifact(&artifact_id, &user_id, &be_run, None)
            .await
            .unwrap()
    );
    let content = sqlx::query(
        "SELECT content_json FROM session_artifacts
         WHERE user_id = ? AND session_id = ? AND artifact_id = ?",
    )
    .bind(&user_id)
    .bind(&session_id)
    .bind(&artifact_id)
    .fetch_one(pool.get())
    .await
    .unwrap()
    .try_get::<String, _>("content_json")
    .unwrap();
    assert!(content.contains(migration_sql));
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l3_14_s10_bubble_up_five_levels_writes_one_event_per_target() {
    let pool = setup_pool().await;
    let (session_id, user_id, root_run_id) = ids();
    insert_session(&pool, &session_id, &user_id).await;
    let targets = (0..5)
        .map(|depth| BubbleUpTarget {
            session_id: session_id.clone(),
            run_id: format!("{root_run_id}-L{depth}"),
            depth,
        })
        .collect::<Vec<_>>();
    DatabaseStateProjectionStore::new(pool.clone())
        .bubble_up_finding(
            &user_id,
            &format!("{root_run_id}-L4"),
            "finding-critical-l4",
            "critical",
            "L4 reviewer found migration would corrupt data",
            &targets,
        )
        .await
        .unwrap();
    let count = sqlx::query(
        "SELECT COUNT(*) AS c FROM session_state_item_events
         WHERE session_id = ? AND user_id = ? AND mutation = 'bubble_up'",
    )
    .bind(&session_id)
    .bind(&user_id)
    .fetch_one(pool.get())
    .await
    .unwrap()
    .try_get::<i64, _>("c")
    .unwrap();
    assert_eq!(count, 5);
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l3_15_s11_cross_session_decision_retrieval_has_provenance() {
    let pool = setup_pool().await;
    let (session_id, user_id, _) = ids();
    insert_session(&pool, &session_id, &user_id).await;
    let source_sessions = (0..3)
        .map(|idx| format!("history-{idx}-{session_id}"))
        .collect::<Vec<_>>();
    for (idx, source_session) in source_sessions.iter().enumerate() {
        insert_session(&pool, source_session, &user_id).await;
        sqlx::query(
            "INSERT INTO session_history_chunks
             (chunk_id, user_id, session_id, source_session_id, seq_start, seq_end,
              chunk_type, source_table, source_id, content_text, content_hash, token_estimate,
              provenance_json, created_at)
             VALUES (?, ?, ?, ?, ?, ?, 'decision', 'agent_events', ?, ?, ?, 180, ?, DATE_SUB(NOW(6), INTERVAL 90 DAY))",
        )
        .bind(format!("decision-chunk-{idx}-{session_id}"))
        .bind(&user_id)
        .bind(&session_id)
        .bind(source_session)
        .bind(idx as i64 * 10)
        .bind(idx as i64 * 10 + 9)
        .bind(format!("event-{idx}"))
        .bind(format!("decision {idx}: keep MatrixOne index for cross-session recall"))
        .bind(format!("sha256:decision-{idx}"))
        .bind(json!({"source_session_id": source_session, "retrieval_stage": "fts"}).to_string())
        .execute(pool.get())
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO agent_events
         (event_id, session_id, user_id, event_type, content, metadata, created_at)
         VALUES (?, ?, ?, 'retrieval.fts_hit', 'decision recall', ?, NOW(6))",
    )
    .bind(format!("retrieval-{}", Uuid::new_v4()))
    .bind(&session_id)
    .bind(&user_id)
    .bind(json!({"source_session_id": source_sessions[0], "chunk_type": "decision"}).to_string())
    .execute(pool.get())
    .await
    .unwrap();
    let plan = explain_analyze_text(
        &pool,
        &format!(
            "EXPLAIN ANALYZE SELECT chunk_id, provenance_json FROM session_history_chunks FORCE INDEX (idx_history_user_chunk_created) \
             WHERE user_id = '{}' AND chunk_type = 'decision' ORDER BY created_at DESC LIMIT 5",
            user_id
        ),
    )
    .await;
    assert!(
        plan.contains("session_history_chunks"),
        "query was not analyzed:\n{plan}"
    );
    assert_eq!(
        index_columns(
            &pool,
            "session_history_chunks",
            "idx_history_user_chunk_created"
        )
        .await,
        ["user_id", "chunk_type", "created_at"],
        "history lookup index must preserve owner/type/time ordering"
    );
    let row = sqlx::query(
        "SELECT provenance_json FROM session_history_chunks FORCE INDEX (idx_history_user_chunk_created)
         WHERE user_id = ? AND chunk_type = 'decision'
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(&user_id)
    .fetch_one(pool.get())
    .await
    .unwrap();
    let provenance = row.try_get::<String, _>("provenance_json").unwrap();
    assert!(
        source_sessions
            .iter()
            .any(|session| provenance.contains(session))
    );
}
