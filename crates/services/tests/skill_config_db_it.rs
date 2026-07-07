//! Live MatrixOne tests for skill configuration persistence.
//!
//! Run with:
//! ASTRA_TEST_DB_IT=1 ASTRA_AUTO_CREATE_DATABASE=1 cargo test -p astra-services --test skill_config_db_it -- --ignored

mod common;

use astra_services::{DatabaseSkillConfigService, FernetTokenEncryptor, SkillConfigService};
use axum::http::StatusCode;
use serde_json::json;
use sqlx::Row;
use std::collections::HashMap;
use uuid::Uuid;

fn test_encryptor() -> FernetTokenEncryptor {
    FernetTokenEncryptor::new("skill-config-db-it-key").expect("test encryptor")
}

async fn cleanup_skill_config_rows(
    pool: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    skill_name: &str,
) {
    sqlx::query("DELETE FROM skill_resource_bindings WHERE user_id = ? AND skill_name = ?")
        .bind(user_id)
        .bind(skill_name)
        .execute(pool)
        .await
        .expect("cleanup resource bindings");

    sqlx::query("DELETE FROM skill_settings WHERE skill_name = ?")
        .bind(skill_name)
        .execute(pool)
        .await
        .expect("cleanup skill settings");

    sqlx::query("DELETE FROM skills_registry WHERE skill_name = ?")
        .bind(skill_name)
        .execute(pool)
        .await
        .expect("cleanup skill registry");
}

async fn insert_skill_registry_row(pool: &sqlx::Pool<sqlx::MySql>, skill_name: &str) {
    let manifest = json!({
        "secrets": [
            {"name": "api_key"}
        ],
        "resources": [
            {
                "type": "generic",
                "bindings": [
                    {"name": "password", "type": "secret"},
                    {"name": "host", "type": "string"}
                ]
            }
        ]
    });

    sqlx::query(
        "INSERT INTO skills_registry \
         (skill_id, skill_name, version, description, skill_definition, manifest, \
          is_active, status, source, is_public, created_by, created_at, updated_at) \
         VALUES (?, ?, '1.0.0', 'skill config integration test', CAST(? AS JSON), CAST(? AS JSON), \
                 1, 'active', 'user', 0, ?, NOW(6), NOW(6))",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(skill_name)
    .bind(json!({"instructions": "configure me"}).to_string())
    .bind(manifest.to_string())
    .bind(Uuid::new_v4().to_string())
    .execute(pool)
    .await
    .expect("insert skill registry row");
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn skill_config_service_round_trips_settings_and_resources_on_live_matrixone() {
    let (shared, settings) = common::setup_pool_and_settings().await;
    let pool = shared.get().clone();
    let user_id = Uuid::new_v4().to_string();
    let skill_name = format!("skillcfg-{}", Uuid::new_v4().simple());
    cleanup_skill_config_rows(&pool, &user_id, &skill_name).await;
    insert_skill_registry_row(&pool, &skill_name).await;

    let svc = DatabaseSkillConfigService::new(settings).with_pool(shared);
    let encryptor = test_encryptor();

    let _ = svc
        .set_setting(
            &user_id,
            &skill_name,
            "timeout",
            "user",
            json!("45s"),
            &encryptor,
        )
        .await
        .expect("set non-secret setting");

    let _ = svc
        .set_setting(
            &user_id,
            &skill_name,
            "api_key",
            "user",
            json!("plain-secret"),
            &encryptor,
        )
        .await
        .expect("set secret setting");

    let stored_secret: String = sqlx::query(
        "SELECT setting_value FROM skill_settings \
         WHERE skill_name = ? AND setting_name = 'api_key' AND scope_id = ?",
    )
    .bind(&skill_name)
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("load stored secret setting")
    .try_get("setting_value")
    .expect("decode stored secret setting");
    assert_ne!(stored_secret, "plain-secret");

    let validation = svc
        .validate_config(&user_id, &skill_name, None)
        .await
        .expect("validate config")
        .0;
    assert!(
        validation.valid,
        "unexpected validation errors: {:?}",
        validation.errors.len()
    );

    let effective = svc
        .get_effective_config(&user_id, &skill_name)
        .await
        .expect("get effective config")
        .0;
    assert_eq!(effective.settings.get("timeout"), Some(&json!("45s")));
    assert_eq!(
        effective.secrets.get("api_key").map(String::as_str),
        Some("***")
    );
    assert_eq!(effective.resources_configured, 0);

    let _ = svc
        .bind_resource(
            &user_id,
            &skill_name,
            "primary-db",
            HashMap::from([
                ("host".to_string(), json!("db.internal")),
                ("password".to_string(), json!("db-password")),
            ]),
            &encryptor,
        )
        .await
        .expect("bind resource");

    let resources = svc
        .list_resources(&user_id, &skill_name)
        .await
        .expect("list resources")
        .0;
    assert_eq!(resources.len(), 1);
    assert_eq!(resources[0].resource_key, "primary-db");
    assert_eq!(resources[0].resource_type, "generic");

    let stored_binding_secret: String = sqlx::query(
        "SELECT binding_value FROM skill_resource_bindings \
         WHERE user_id = ? AND skill_name = ? AND resource_key = 'primary-db' AND binding_name = 'password'",
    )
    .bind(&user_id)
    .bind(&skill_name)
    .fetch_one(&pool)
    .await
    .expect("load stored resource secret")
    .try_get("binding_value")
    .expect("decode stored resource secret");
    assert_ne!(stored_binding_secret, "db-password");

    let effective = svc
        .get_effective_config(&user_id, &skill_name)
        .await
        .expect("get effective config after bind")
        .0;
    assert_eq!(effective.resources_configured, 1);

    let unbound = svc
        .unbind_resource(&user_id, &skill_name, "primary-db")
        .await
        .expect("unbind resource")
        .0;
    assert_eq!(unbound.count, 2);

    cleanup_skill_config_rows(&pool, &user_id, &skill_name).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn skill_config_null_setting_value_fails_loud_on_live_matrixone() {
    let (shared, settings) = common::setup_pool_and_settings().await;
    let pool = shared.get().clone();
    let user_id = Uuid::new_v4().to_string();
    let skill_name = format!("skillcfg-{}", Uuid::new_v4().simple());
    cleanup_skill_config_rows(&pool, &user_id, &skill_name).await;
    insert_skill_registry_row(&pool, &skill_name).await;

    sqlx::query(
        "INSERT INTO skill_settings \
         (setting_id, skill_name, setting_name, setting_value, is_secret, scope_type, scope_id, updated_by) \
         VALUES (?, ?, 'broken', NULL, 0, 'user', ?, ?)",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(&skill_name)
    .bind(&user_id)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("insert corrupt setting row");

    let svc = DatabaseSkillConfigService::new(settings).with_pool(shared);
    let (status, error) = svc
        .get_effective_config(&user_id, &skill_name)
        .await
        .expect_err("NULL setting_value must not be converted to an empty string");
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        error.0.detail.contains("setting_value") || error.0.detail.contains("Decode"),
        "unexpected error detail: {}",
        error.0.detail
    );

    cleanup_skill_config_rows(&pool, &user_id, &skill_name).await;
}
