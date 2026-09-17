//! Real model service + isolated DB + strict, non-billable provider fixture.
//! Run with ASTRA_TEST_DB_IT=1, ASTRA_TEST_DATABASE=$ASTRA_DATABASE,
//! ASTRA_ALLOW_INSECURE_DEFAULTS=1 and ASTRA_BYOK_DEEPSEEK_BASE_URL set to
//! an unused loopback HTTP origin (e.g. http://127.0.0.1:18994).
//! Explicitly selected with --features external-contract-tests; ordinary
//! online lanes do not provide this operator-only endpoint override.
mod common;
#[path = "common/isolated_database.rs"]
mod isolated_database;

use astra_services::{
    DatabaseModelService, FernetTokenEncryptor, ModelService,
    models::{UserModelCreateRequestData, UserModelUpdateRequestData},
};
use axum::{Json, Router, http::StatusCode, routing::post};
use serde_json::{Value, json};
use std::sync::Arc;

#[tokio::test]
#[ignore = "requires isolated MatrixOne DB and loopback DeepSeek fixture override"]
async fn user_model_create_rotate_and_probe_enforce_provider_wire_contract() {
    let settings = common::require_db_it_env();
    isolated_database::require_isolated_database(&settings.database);
    assert!(
        isolated_database::is_schema_rehearsal_database(&settings.database),
        "schema rehearsal requires an astra_test_probe_* disposable database"
    );
    assert_eq!(
        std::env::var("ASTRA_ALLOW_INSECURE_DEFAULTS").as_deref(),
        Ok("1")
    );
    let base = std::env::var("ASTRA_BYOK_DEEPSEEK_BASE_URL").expect("loopback fixture origin");
    let url = reqwest::Url::parse(&base).unwrap();
    assert_eq!(url.scheme(), "http");
    assert_eq!(url.host_str(), Some("127.0.0.1"));
    let fail_probe = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let app = Router::new()
        .route(
            "/chat/completions",
            post(
                |axum::extract::State(fail): axum::extract::State<
                    Arc<std::sync::atomic::AtomicBool>,
                >,
                 headers: axum::http::HeaderMap,
                 Json(body): Json<Value>| async move {
                    // Each simulated upstream enforces its own documented
                    // field; do not reuse the serializer under test here.
                    let (limit, forbidden) = match body["model"].as_str() {
                        Some("deepseek-chat") => ("max_tokens", "max_completion_tokens"),
                        _ => ("max_completion_tokens", "max_tokens"),
                    };
                    let status = if headers
                        .get("authorization")
                        .is_none_or(|v| v != "Bearer valid-key" && v != "Bearer rotated-key")
                    {
                        StatusCode::UNAUTHORIZED
                    } else if body["model"] != "deepseek-chat" && body["model"] != "o3" {
                        StatusCode::NOT_FOUND
                    } else if (body[limit] != 32 && body[limit] != 1024)
                        || body.get(forbidden).is_some()
                    {
                        StatusCode::BAD_REQUEST
                    } else {
                        StatusCode::OK
                    };
                    if status == StatusCode::OK && body[limit] == 1024 {
                        if fail.load(std::sync::atomic::Ordering::SeqCst) {
                            return (
                                StatusCode::SERVICE_UNAVAILABLE,
                                Json(json!({"error":"fixture failure"})),
                            );
                        }
                        assert!(body.get("temperature").is_none());
                        assert!(body.get("enable_thinking").is_none());
                        assert!(body.get("reasoning_effort").is_none());
                        let mut message = json!({"content":"391"});
                        if body["thinking"]["type"] == "enabled" {
                            message["reasoning_content"] = json!("multiplication");
                        }
                        return (
                            status,
                            Json(json!({"choices":[{"message":message,"finish_reason":"stop"}]})),
                        );
                    }
                    (
                        status,
                        Json(json!({"error":{"message":"strict fixture rejection"}})),
                    )
                },
            ),
        )
        .route(
            "/v1/messages",
            post(
                |headers: axum::http::HeaderMap, Json(body): Json<Value>| async move {
                    let status = if headers
                        .get("x-api-key")
                        .is_none_or(|v| v != "valid-key" && v != "rotated-key")
                    {
                        StatusCode::UNAUTHORIZED
                    } else if body["model"] != "claude-sonnet-4-5" {
                        StatusCode::NOT_FOUND
                    } else if headers
                        .get("anthropic-version")
                        .is_none_or(|v| v != "2023-06-01")
                        || body["max_tokens"] != 32
                        || body.get("max_completion_tokens").is_some()
                    {
                        StatusCode::BAD_REQUEST
                    } else {
                        StatusCode::OK
                    };
                    (
                        status,
                        Json(json!({"error":{"message":"strict fixture rejection"}})),
                    )
                },
            ),
        )
        .with_state(fail_probe.clone());
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", url.port().unwrap()))
        .await
        .unwrap();
    let fixture = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let (pool, settings) = common::setup_pool_and_settings().await;
    let encryptor = FernetTokenEncryptor::new("provider-wire-test-only-key").unwrap();
    let service = DatabaseModelService::new(settings.clone(), Arc::new(encryptor.clone()))
        .with_pool(pool.clone());
    let owner = uuid::Uuid::new_v4().to_string();
    let make_request = |name: &str, model: &str, key: &str| UserModelCreateRequestData {
        name: name.into(),
        provider: "deepseek".into(),
        model: model.into(),
        base_url: None,
        api_key: key.into(),
        context_window: 128000,
        is_default: true,
    };
    for (model, key) in [
        ("deepseek-chat", "invalid-key"),
        ("missing-model", "valid-key"),
    ] {
        let error = service
            .create_user_model(owner.clone(), make_request("rejected", model, key))
            .await
            .unwrap_err();
        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert!(
            service
                .list_user_models(owner.clone())
                .await
                .unwrap()
                .is_empty(),
            "failed create persisted data"
        );
    }
    let created = service
        .create_user_model(
            owner.clone(),
            make_request("fixture", "deepseek-chat", "valid-key"),
        )
        .await
        .unwrap();
    // Simulate the pre-upgrade schema in this explicitly designated disposable
    // database. Preserve a real old model row across two idempotent bootstraps.
    let personal_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM user_llm_models")
        .fetch_one(pool.get())
        .await
        .unwrap();
    let administrator_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM infra_llm_models")
        .fetch_one(pool.get())
        .await
        .unwrap();
    assert_eq!(
        personal_rows, 1,
        "schema rehearsal requires only this test's model row"
    );
    assert_eq!(
        administrator_rows, 0,
        "schema rehearsal refuses existing administrator models"
    );
    for table in ["infra_llm_models", "user_llm_models"] {
        sqlx::query(&format!(
            "ALTER TABLE {table} DROP COLUMN thinking_probe_json"
        ))
        .execute(pool.get())
        .await
        .unwrap();
    }
    sqlx::query("UPDATE astra_schema_contracts SET contract_version = '2026-09-04-v70' WHERE component = 'astra-core'")
        .execute(pool.get()).await.unwrap();
    for _ in 0..2 {
        astra_services::storage::ensure_core_schema(&settings, "mysql")
            .await
            .unwrap();
    }
    let migrated = service
        .get_user_model(owner.clone(), created.model_id.clone())
        .await
        .unwrap();
    assert_eq!(migrated.name, created.name);
    assert!(migrated.thinking_probe.is_none());
    // Fixed official endpoints are not user-overridable. Seed their *test row*
    // with this loopback fixture to exercise the real rotate/probe service paths
    // without adding a production transport bypass or spending a live API key.
    for (provider, model) in [
        ("deepseek", "deepseek-chat"),
        ("openai", "o3"),
        ("anthropic", "claude-sonnet-4-5"),
    ] {
        sqlx::query("UPDATE user_llm_models SET provider = ?, model_name = ?, api_key_encrypted = ? WHERE user_id = ? AND model_id = ?")
            .bind(provider).bind(model).bind(encryptor.encrypt("valid-key").unwrap()).bind(&owner).bind(&created.model_id).execute(pool.get()).await.unwrap();
        let checked = service
            .check_user_model(owner.clone(), created.model_id.clone())
            .await
            .unwrap();
        if provider == "deepseek" {
            assert_eq!(
                checked.thinking_probe.as_ref().unwrap().capability,
                astra_services::models::ThinkingCapability::Both
            );
            assert!(checked.thinking_probe.as_ref().unwrap().error.is_none());
            let execution = astra_services::models::revalidate_admitted_model_execution(
                &settings,
                &encryptor,
                &owner,
                &created.model_id,
                Some(pool.get()),
            )
            .await
            .unwrap();
            assert_eq!(
                execution.thinking_capability,
                Some(astra_services::models::ThinkingCapability::Both)
            );
            fail_probe.store(true, std::sync::atomic::Ordering::SeqCst);
            let failed_check = service
                .check_user_model(owner.clone(), created.model_id.clone())
                .await
                .unwrap();
            let observation = failed_check.thinking_probe.unwrap();
            assert_eq!(
                observation.capability,
                astra_services::models::ThinkingCapability::Both
            );
            assert!(observation.error.is_some());
            let retained = astra_services::models::revalidate_admitted_model_execution(
                &settings,
                &encryptor,
                &owner,
                &created.model_id,
                Some(pool.get()),
            )
            .await
            .unwrap();
            assert_eq!(
                retained.thinking_capability,
                Some(astra_services::models::ThinkingCapability::Both)
            );
            fail_probe.store(false, std::sync::atomic::Ordering::SeqCst);
        }
        let before: String = sqlx::query_scalar(
            "SELECT api_key_encrypted FROM user_llm_models WHERE user_id = ? AND model_id = ?",
        )
        .bind(&owner)
        .bind(&created.model_id)
        .fetch_one(pool.get())
        .await
        .unwrap();
        let error = service
            .update_user_model(
                owner.clone(),
                created.model_id.clone(),
                UserModelUpdateRequestData {
                    api_key: Some("invalid-key".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.0, StatusCode::BAD_REQUEST, "{provider}: {error:?}");
        let after: String = sqlx::query_scalar(
            "SELECT api_key_encrypted FROM user_llm_models WHERE user_id = ? AND model_id = ?",
        )
        .bind(&owner)
        .bind(&created.model_id)
        .fetch_one(pool.get())
        .await
        .unwrap();
        assert_eq!(before, after, "failed rotation overwrote credential");
        service
            .update_user_model(
                owner.clone(),
                created.model_id.clone(),
                UserModelUpdateRequestData {
                    api_key: Some("rotated-key".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let after: String = sqlx::query_scalar(
            "SELECT api_key_encrypted FROM user_llm_models WHERE user_id = ? AND model_id = ?",
        )
        .bind(&owner)
        .bind(&created.model_id)
        .fetch_one(pool.get())
        .await
        .unwrap();
        assert_eq!(encryptor.decrypt(&after).unwrap(), "rotated-key");
        assert!(
            service
                .get_user_model(owner.clone(), created.model_id.clone())
                .await
                .unwrap()
                .thinking_probe
                .is_none(),
            "rotation must invalidate the previous observation"
        );
        service
            .check_user_model(owner.clone(), created.model_id.clone())
            .await
            .unwrap();
    }
    // Administrator legacy hints survive an inconclusive first check, and
    // fresh successful observations survive a later transient probe failure.
    use astra_services::models::{
        ModelCreateRequestData, PricingData, QuirksData, ThinkingCapability,
    };
    service
        .create_model(
            owner.clone(),
            ModelCreateRequestData {
                name: "admin-fixture".into(),
                provider: "deepseek".into(),
                api_key: "valid-key".into(),
                base_url: Some(base),
                description: None,
                context_window: Some(128000),
                max_completion_tokens: None,
                input_modalities: vec!["text".into()],
                output_modalities: vec!["text".into()],
                supported_parameters: vec![],
                pricing: PricingData {
                    prompt: 0.0,
                    completion: 0.0,
                    cache_read: None,
                    cache_write: None,
                },
                architecture: None,
                tags: vec![],
                quirks: Some(QuirksData {
                    wire_model_name: Some("deepseek-chat".into()),
                    ..Default::default()
                }),
            },
        )
        .await
        .unwrap();
    sqlx::query("UPDATE infra_llm_models SET thinking_capability = 'native_only' WHERE model_name = 'admin-fixture'")
        .execute(pool.get()).await.unwrap();
    for (fail, expected) in [
        (true, ThinkingCapability::NativeOnly),
        (false, ThinkingCapability::Both),
        (true, ThinkingCapability::Both),
    ] {
        fail_probe.store(fail, std::sync::atomic::Ordering::SeqCst);
        let checked = service.check_model("admin-fixture".into()).await.unwrap();
        let result = checked.thinking_probe.unwrap();
        assert_eq!(result.capability, expected);
        assert_eq!(result.error.is_some(), fail);
    }
    service.delete_model("admin-fixture".into()).await.unwrap();
    service
        .delete_user_model(owner, created.model_id)
        .await
        .unwrap();
    fixture.abort();
}
