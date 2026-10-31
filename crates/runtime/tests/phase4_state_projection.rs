mod test_support;

use test_support::require_db_it_env;

use astra_services::{
    DatabasePersonalSkillStore, DatabaseStateProjectionStore, SubmitUserSkillVersion,
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
    let event: (String, i64) = sqlx::query_as(
        "SELECT CAST(metadata AS CHAR), (SELECT event_count FROM agent_sessions WHERE user_id = ? AND session_id = ?) FROM agent_events WHERE user_id = ? AND session_id = ? AND event_type = 'ui.skill.activate' AND llm_model_used IS NULL",
    )
    .bind(&user_id).bind(&session_id).bind(&user_id).bind(&session_id)
    .fetch_one(pool.get()).await.unwrap();
    let event_payload: serde_json::Value = serde_json::from_str(&event.0).unwrap();
    assert_eq!(
        event_payload, payload,
        "canonical event and current state retain the same activation"
    );
    assert_eq!(
        event.1, 1,
        "activation commits its canonical event and session count"
    );
}
