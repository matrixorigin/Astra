use astra_services::{
    ActivateUserSkillVersion, CreateUserSkillSource, DatabasePersonalSkillStore, InstallUserSkill,
    PersonalSkillError, RecordUserSkillEvaluation, SubmitUserSkillVersion, skill_md_content_hash,
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
    let mut settings = astra_core::MatrixOneSettings::from_env();
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
async fn l2_45_active_switch_does_not_mutate_draft_or_auto_activate_install() {
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
        .install_skill(
            &user_id,
            &skill_name,
            InstallUserSkill {
                version_id: Some(v1.version_id.clone()),
                scope: Some("workspace".to_string()),
                session_id: None,
                workspace_id: Some("workspace-test".to_string()),
                auto_activate_on_topic_match: Some(false),
            },
        )
        .await
        .unwrap();
    store
        .activate_version(&user_id, &session_id, &skill_name, &v1.version_id)
        .await
        .unwrap();

    let row = sqlx::query(
        "SELECT
          (SELECT status FROM user_skill_versions WHERE version_id = ?) AS draft_status,
          (SELECT auto_activate_on_topic_match FROM skill_installations
           WHERE user_id = ? AND skill_name = ?) AS auto_activate,
          (SELECT payload_json FROM session_state_items
           WHERE session_id = ? AND user_id = ? AND category = 'active_skill' AND item_key = ?) AS payload_json",
    )
    .bind(&v2.version_id)
    .bind(&user_id)
    .bind(&skill_name)
    .bind(&session_id)
    .bind(&user_id)
    .bind(&skill_name)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(row.try_get::<String, _>("draft_status").unwrap(), "draft");
    assert_eq!(row.try_get::<i64, _>("auto_activate").unwrap(), 0);
    assert!(
        row.try_get::<String, _>("payload_json")
            .unwrap()
            .contains(&v1.version_id)
    );
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
async fn l2_46_skill_evaluations_use_independent_table_and_unified_denominator() {
    let pool = setup_pool().await;
    let store = DatabasePersonalSkillStore::new(pool.clone());
    let (user_id, skill_name) = test_ids();
    let version = store
        .submit_version(&user_id, &skill_name, submit_request("v1", "published"))
        .await
        .unwrap();
    let evaluation = store
        .record_evaluation(
            &user_id,
            &skill_name,
            RecordUserSkillEvaluation {
                source_id: version.source_id.clone(),
                version_id: version.version_id.clone(),
                run_id: Some(format!("run-{}", Uuid::new_v4())),
                hits: 7,
                suspects: 10,
                false_positives: 2,
                payload_json: Some(json!({"denominator": "suspects", "hit_rate": 0.7})),
            },
        )
        .await
        .unwrap();
    assert_eq!(evaluation.owner_user_id, user_id);
    assert_eq!(evaluation.hits, 7);
    assert_eq!(evaluation.suspects, 10);
    let foreign_user_id = Uuid::new_v4().to_string();
    let rejected = store
        .record_evaluation(
            &foreign_user_id,
            &skill_name,
            RecordUserSkillEvaluation {
                source_id: version.source_id.clone(),
                version_id: version.version_id.clone(),
                run_id: Some(format!("run-{}", Uuid::new_v4())),
                hits: 1,
                suspects: 1,
                false_positives: 0,
                payload_json: Some(json!({"should_not_insert": true})),
            },
        )
        .await
        .expect_err("foreign owner must not record evaluation for another user's skill version");
    assert!(
        matches!(
            rejected,
            PersonalSkillError::VersionNotFound {
                ref owner_user_id,
                ref version_id,
                ..
            } if owner_user_id == &foreign_user_id && version_id == &version.version_id
        ),
        "unexpected foreign-owner error: {rejected:?}"
    );
    let row = sqlx::query(
        "SELECT
          (SELECT COUNT(*) FROM user_skill_evaluations WHERE owner_user_id = ? AND version_id = ?) AS eval_count,
          (SELECT COUNT(*) FROM user_skill_evaluations WHERE owner_user_id = ?) AS foreign_eval_count,
          (SELECT COUNT(*) FROM session_state_items WHERE category = 'skill_evaluation') AS state_count",
    )
    .bind(&user_id)
    .bind(&version.version_id)
    .bind(&foreign_user_id)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(row.try_get::<i64, _>("eval_count").unwrap(), 1);
    assert_eq!(row.try_get::<i64, _>("foreign_eval_count").unwrap(), 0);
    assert_eq!(row.try_get::<i64, _>("state_count").unwrap(), 0);
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
async fn l2_48_auto_activate_topic_match_switch_controls_candidates() {
    let pool = setup_pool().await;
    let store = DatabasePersonalSkillStore::new(pool.clone());
    let (user_id, skill_name) = test_ids();
    let version = store
        .submit_version(&user_id, &skill_name, submit_request("v1", "published"))
        .await
        .unwrap();
    store
        .install_skill(
            &user_id,
            &skill_name,
            InstallUserSkill {
                version_id: Some(version.version_id.clone()),
                scope: Some("user".to_string()),
                session_id: None,
                workspace_id: None,
                auto_activate_on_topic_match: Some(false),
            },
        )
        .await
        .unwrap();
    assert!(
        !store
            .auto_activate_candidates(&user_id)
            .await
            .unwrap()
            .contains(&skill_name)
    );
    store
        .install_skill(
            &user_id,
            &skill_name,
            InstallUserSkill {
                version_id: Some(version.version_id),
                scope: Some("user".to_string()),
                session_id: None,
                workspace_id: None,
                auto_activate_on_topic_match: Some(true),
            },
        )
        .await
        .unwrap();
    assert!(
        store
            .auto_activate_candidates(&user_id)
            .await
            .unwrap()
            .contains(&skill_name)
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
        .activate_version(&user_id, &session_id, &skill_name, &v2.version_id)
        .await
        .unwrap();
    assert!(
        store
            .activate_version(&user_id, &session_id, &skill_name, &versions[6].version_id)
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
    };
}
