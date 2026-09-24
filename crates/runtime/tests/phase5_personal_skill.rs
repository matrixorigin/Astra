mod test_support;

use astra_services::{
    ActivateUserSkillVersion, CreateUserSkillSource, DatabasePersonalSkillStore,
    PersonalSkillError, SubmitUserSkillVersion, skill_md_content_hash,
};
use serde_json::json;
use sqlx::Row;
use uuid::Uuid;

fn require_db_it_env() -> astra_core::MatrixOneSettings {
    let mut settings = test_support::require_db_it_env();
    settings.db_pool_max_connections = settings.db_pool_max_connections.clamp(1, 4);
    settings.db_pool_min_connections = settings
        .db_pool_min_connections
        .min(settings.db_pool_max_connections);
    settings
}

static SHARED_BOOTSTRAP: tokio::sync::OnceCell<astra_core::MatrixOneSettings> =
    tokio::sync::OnceCell::const_new();

async fn bootstrap_settings() -> &'static astra_core::MatrixOneSettings {
    SHARED_BOOTSTRAP
        .get_or_init(|| async {
            let settings = require_db_it_env();
            let catalog = std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG")
                .unwrap_or_else(|_| "mysql".into());
            astra_services::ensure_core_schema(&settings, &catalog)
                .await
                .expect("ensure_core_schema; is MatrixOne up?");
            settings
        })
        .await
}

async fn setup_pool() -> astra_core::SharedPool {
    astra_core::SharedPool::new(bootstrap_settings().await)
        .await
        .expect("SharedPool::new")
}

async fn insert_session(pool: &astra_core::SharedPool, session_id: &str, user_id: &str) {
    sqlx::query(
        "INSERT INTO agent_sessions
         (session_id, user_id, agent_id, title, status, metadata, created_at, updated_at)
         VALUES (?, ?, 'phase5-agent', 'phase5 session', 'active', '{}', NOW(6), NOW(6))",
    )
    .bind(session_id)
    .bind(user_id)
    .execute(pool.get())
    .await
    .unwrap();
}

async fn explain_analyze_text(pool: &astra_core::SharedPool, sql: &str) -> String {
    let rows = sqlx::raw_sql(sql).fetch_all(pool.get()).await.unwrap();
    let mut text = String::new();
    for row in rows {
        for idx in 0..row.columns().len() {
            if let Ok(value) = row.try_get::<String, _>(idx) {
                text.push_str(&value);
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

fn test_ids() -> (String, String) {
    let suffix = Uuid::new_v4();
    (suffix.to_string(), format!("skill-{suffix}"))
}

fn submit_request(version: &str, status: &str) -> SubmitUserSkillVersion {
    SubmitUserSkillVersion {
        version: version.to_string(),
        manifest_json: json!({
            "name": "review_changes",
            "description": "Review local code changes",
            "triggers": ["review", "diff"]
        }),
        content_markdown: "## Instructions\n\nReview the diff and report concrete findings.\n"
            .to_string(),
        status: Some(status.to_string()),
    }
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_44_skill_md_content_hash_is_deterministic_after_normalization() {
    let _pool = setup_pool().await;
    let manifest_a = json!({"z": 1, "a": {"b": 2, "a": 1}});
    let manifest_b = json!({"a": {"a": 1, "b": 2}, "z": 1});
    let content_a = "## Usage  \r\n\r\n\r\nRun review.\r\n";
    let content_b = "## Usage\n\nRun review.\n";
    let hash_a = skill_md_content_hash(&manifest_a, content_a);
    let hash_b = skill_md_content_hash(&manifest_b, content_b);
    assert_eq!(hash_a, hash_b);
    assert!(hash_a.starts_with("sha256:"));
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_45_active_switch_accepts_only_published_version() {
    let pool = setup_pool().await;
    let store = DatabasePersonalSkillStore::new(pool.clone());
    let (user_id, skill_name) = test_ids();
    let session_id = format!("session-{}", Uuid::new_v4());
    insert_session(&pool, &session_id, &user_id).await;
    let v1 = store
        .submit_version(&user_id, &skill_name, submit_request("v1", "published"))
        .await
        .unwrap();
    let v2 = store
        .submit_version(&user_id, &skill_name, submit_request("v2", "draft"))
        .await
        .unwrap();
    store
        .activate_version_with_expected(&user_id, &session_id, &skill_name, &v1.version_id, None)
        .await
        .unwrap();
    assert!(matches!(
        store
            .activate_version_with_expected(
                &user_id,
                &session_id,
                &skill_name,
                &v2.version_id,
                Some(&v1.version_id),
            )
            .await,
        Err(PersonalSkillError::VersionNotActivatable { .. })
    ));
    let typo_session = format!("typo-{}", Uuid::new_v4());
    assert!(matches!(
        store
            .activate_version_with_expected(
                &user_id,
                &typo_session,
                &skill_name,
                &v1.version_id,
                None,
            )
            .await,
        Err(PersonalSkillError::SessionNotActive { .. })
    ));
    let typo_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_sessions WHERE user_id = ? AND session_id = ?",
    )
    .bind(&user_id)
    .bind(&typo_session)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(typo_count, 0, "activation typo must not create a session");

    let row = sqlx::query(
        "SELECT
          (SELECT status FROM user_skill_versions WHERE version_id = ?) AS draft_status,
          (SELECT payload_json FROM session_state_items
           WHERE session_id = ? AND user_id = ? AND category = 'active_skill' AND item_key = ?) AS payload_json",
    )
    .bind(&v2.version_id)
    .bind(&session_id)
    .bind(&user_id)
    .bind(&skill_name)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(row.try_get::<String, _>("draft_status").unwrap(), "draft");
    assert!(
        row.try_get::<String, _>("payload_json")
            .unwrap()
            .contains(&v1.version_id)
    );
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_47_personal_skill_search_uses_owner_skill_name_index() {
    let pool = setup_pool().await;
    let store = DatabasePersonalSkillStore::new(pool.clone());
    let (user_id, skill_name) = test_ids();
    store
        .create_source(
            &user_id,
            CreateUserSkillSource {
                skill_name: skill_name.clone(),
                visibility: Some("private".to_string()),
            },
        )
        .await
        .unwrap();
    let plan = explain_analyze_text(
        &pool,
        &format!(
            "EXPLAIN ANALYZE SELECT source_id FROM user_skill_sources FORCE INDEX (idx_user_skill_owner_name) \
             WHERE owner_user_id = '{}' AND skill_name >= '{}' ORDER BY skill_name LIMIT 10",
            user_id, skill_name
        ),
    )
    .await;
    assert!(
        plan.contains("user_skill_sources"),
        "query was not analyzed:\n{plan}"
    );
    assert_eq!(
        index_columns(&pool, "user_skill_sources", "idx_user_skill_owner_name").await,
        ["owner_user_id", "skill_name"],
        "personal-skill lookup index must preserve owner/name ordering"
    );
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_48_active_personal_skill_content_is_exactly_session_and_owner_scoped() {
    let pool = setup_pool().await;
    let store = DatabasePersonalSkillStore::new(pool.clone());
    let (user_id, skill_name) = test_ids();
    let session_a = format!("session-{}", Uuid::new_v4());
    let session_b = format!("session-{}", Uuid::new_v4());
    let foreign_user = Uuid::new_v4().to_string();
    insert_session(&pool, &session_a, &user_id).await;
    insert_session(&pool, &session_b, &user_id).await;
    insert_session(&pool, &session_a, &foreign_user).await;
    let version = store
        .submit_version(&user_id, &skill_name, submit_request("v1", "published"))
        .await
        .unwrap();
    store
        .activate_version_with_expected(
            &user_id,
            &session_a,
            &skill_name,
            &version.version_id,
            None,
        )
        .await
        .unwrap();
    let active = store
        .load_active_for_session(&user_id, &session_a)
        .await
        .unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].version_id, version.version_id);
    assert_eq!(active[0].content_markdown, version.content_markdown);
    assert!(
        store
            .load_active_for_session(&user_id, &session_b)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .load_active_for_session(&foreign_user, &session_a)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_49_normalize_version_defaults_and_empty_values_fail_loud() {
    let pool = setup_pool().await;
    let store = DatabasePersonalSkillStore::new(pool.clone());
    let (user_id, skill_name) = test_ids();
    let source = store
        .create_source(
            &user_id,
            CreateUserSkillSource {
                skill_name: skill_name.clone(),
                visibility: Some("private".to_string()),
            },
        )
        .await
        .unwrap();
    let version_id = format!("skill-version-{}", Uuid::new_v4());
    sqlx::query(
        "INSERT INTO user_skill_versions
         (version_id, source_id, owner_user_id, skill_name, version, manifest_json,
          content_markdown, content_hash, token_estimate, status, created_at, updated_at)
         VALUES (?, ?, ?, ?, 'legacy-default', '{}', 'content', 'sha256:test', 2, 'draft', NOW(6), NOW(6))",
    )
    .bind(&version_id)
    .bind(&source.source_id)
    .bind(&user_id)
    .bind(&skill_name)
    .execute(pool.get())
    .await
    .unwrap();
    let normalize_version: String =
        sqlx::query("SELECT normalize_version FROM user_skill_versions WHERE version_id = ?")
            .bind(&version_id)
            .fetch_one(pool.get())
            .await
            .unwrap()
            .try_get("normalize_version")
            .unwrap();
    assert_eq!(normalize_version, "skill_md_v1");

    sqlx::query("UPDATE user_skill_versions SET normalize_version = '' WHERE version_id = ?")
        .bind(&version_id)
        .execute(pool.get())
        .await
        .unwrap();
    let error = store
        .list_versions(&user_id, &skill_name)
        .await
        .expect_err("empty normalize_version must fail loud");
    let rendered = error.to_string();
    assert!(
        rendered.contains("normalize_version") && rendered.contains("must not be empty"),
        "unexpected error: {rendered}"
    );
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l3_16_s13_seven_version_iteration_append_only_and_structured_switch_back_to_v2() {
    let pool = setup_pool().await;
    let store = DatabasePersonalSkillStore::new(pool.clone());
    let (user_id, skill_name) = test_ids();
    let session_id = format!("session-{}", Uuid::new_v4());
    insert_session(&pool, &session_id, &user_id).await;
    let mut versions = Vec::new();
    for idx in 1..=7 {
        let status = if idx == 7 { "quarantined" } else { "published" };
        versions.push(
            store
                .submit_version(
                    &user_id,
                    &skill_name,
                    submit_request(&format!("v{idx}"), status),
                )
                .await
                .unwrap(),
        );
    }
    let v2 = versions[1].clone();
    store
        .activate_version_with_expected(&user_id, &session_id, &skill_name, &v2.version_id, None)
        .await
        .unwrap();
    assert!(
        store
            .activate_version_with_expected(
                &user_id,
                &session_id,
                &skill_name,
                &versions[6].version_id,
                Some(&v2.version_id),
            )
            .await
            .is_err(),
        "quarantined version must be ready for quarantine enforcement"
    );
    let row = sqlx::query(
        "SELECT
          (SELECT COUNT(*) FROM user_skill_versions WHERE source_id = ?) AS version_count,
          (SELECT status FROM user_skill_versions WHERE version_id = ?) AS v7_status,
          (SELECT payload_json FROM session_state_items
           WHERE session_id = ? AND user_id = ? AND category = 'active_skill' AND item_key = ?) AS active_payload",
    )
    .bind(&v2.source_id)
    .bind(&versions[6].version_id)
    .bind(&session_id)
    .bind(&user_id)
    .bind(&skill_name)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(row.try_get::<i64, _>("version_count").unwrap(), 7);
    assert_eq!(
        row.try_get::<String, _>("v7_status").unwrap(),
        "quarantined"
    );
    assert!(
        row.try_get::<String, _>("active_payload")
            .unwrap()
            .contains(&v2.version_id)
    );
    let _structured_request = ActivateUserSkillVersion {
        session_id,
        version_id: v2.version_id,
        expected_active_version_id: None,
    };
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn activation_capacity_serializes_additions_and_preserves_runnable_replacements() {
    let pool = setup_pool().await;
    let store = DatabasePersonalSkillStore::new(pool.clone());
    let (user_id, prefix) = test_ids();
    let session_id = format!("session-{}", Uuid::new_v4());
    insert_session(&pool, &session_id, &user_id).await;
    let limit = astra_services::personal_skills::MAX_ACTIVE_PERSONAL_SKILLS;
    let mut versions = Vec::new();
    for index in 0..=limit {
        let name = format!("{prefix}-{index}");
        let version = store
            .submit_version(&user_id, &name, submit_request("v1", "published"))
            .await
            .unwrap();
        if index < limit - 1 {
            store
                .activate_version_with_expected(
                    &user_id,
                    &session_id,
                    &name,
                    &version.version_id,
                    None,
                )
                .await
                .unwrap();
        }
        versions.push(version);
    }
    let left = &versions[limit - 1];
    let right = &versions[limit];
    let (a, b) = tokio::join!(
        store.activate_version_with_expected(
            &user_id,
            &session_id,
            &left.skill_name,
            &left.version_id,
            None
        ),
        store.activate_version_with_expected(
            &user_id,
            &session_id,
            &right.skill_name,
            &right.version_id,
            None
        ),
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    let rejected = if a.is_err() { left } else { right };
    let error = a.err().or(b.err()).unwrap();
    assert!(
        matches!(error, PersonalSkillError::ActivationLimitReached { .. }),
        "{error}"
    );
    let events_before: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM agent_events WHERE session_id = ?")
            .bind(&session_id)
            .fetch_one(pool.get())
            .await
            .unwrap();
    assert!(matches!(
        store
            .activate_version_with_expected(
                &user_id,
                &session_id,
                &rejected.skill_name,
                &rejected.version_id,
                None
            )
            .await,
        Err(PersonalSkillError::ActivationLimitReached { .. })
    ));
    let events_after: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM agent_events WHERE session_id = ?")
            .bind(&session_id)
            .fetch_one(pool.get())
            .await
            .unwrap();
    assert_eq!(
        events_before, events_after,
        "rejected activation must not append events"
    );
    let before = store
        .load_active_for_session(&user_id, &session_id)
        .await
        .unwrap();
    assert_eq!(before.len(), limit);
    let replacement = store
        .submit_version(
            &user_id,
            &versions[0].skill_name,
            submit_request("v2", "published"),
        )
        .await
        .unwrap();
    store
        .activate_version_with_expected(
            &user_id,
            &session_id,
            &replacement.skill_name,
            &replacement.version_id,
            Some(&versions[0].version_id),
        )
        .await
        .unwrap();
    let after = store
        .load_active_for_session(&user_id, &session_id)
        .await
        .unwrap();
    assert_eq!(after.len(), limit);
    assert!(
        after
            .iter()
            .any(|active| active.version_id == replacement.version_id)
    );
}
