mod test_support;

use test_support::require_db_it_env;

use std::sync::Arc;

use astra_services::{
    BubbleUpTarget, DatabasePersonalSkillStore, DatabaseRunStateStore,
    DatabaseStateProjectionStore, DelegationProjectionUpsert, SubmitUserSkillVersion,
};
use serde_json::json;
use sqlx::Row;
use uuid::Uuid;

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

async fn publish_personal_skill_version(
    pool: &astra_core::SharedPool,
    user_id: &str,
    skill_name: &str,
) -> String {
    DatabasePersonalSkillStore::new(pool.clone())
        .submit_version(
            user_id,
            skill_name,
            SubmitUserSkillVersion {
                version: format!("1.0.0-{}", Uuid::new_v4()),
                manifest_json: json!({"name": skill_name}),
                content_markdown: format!("# {skill_name}\n\nPublished phase-4 fixture."),
                status: Some("published".into()),
            },
        )
        .await
        .expect("publish personal skill version fixture")
        .version_id
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
async fn personal_skill_activation_pins_version_and_records_ui_events() {
    let pool = setup_pool().await;
    let (session_id, user_id, _) = ids();
    insert_session(&pool, &session_id, &user_id).await;
    let version_id = publish_personal_skill_version(&pool, &user_id, "review_changes").await;
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
    let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(payload["version_id"], version_id);
    assert_eq!(payload["activation_source"], "ui_structured_intent");
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
            &session_id,
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
        .persist_status(
            &user_id,
            &session_id,
            &child_run_id,
            "completed",
            None,
            None,
        )
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
