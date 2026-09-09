mod common;

use std::sync::Arc;

use astra_services::{
    DatabaseModelService, FernetTokenEncryptor, ModelAccessKind, ModelOfferingResolutionError,
    ModelService, resolve_active_llm_offering, revalidate_active_llm_offering,
};
use axum::http::StatusCode;
use serial_test::serial;
use uuid::Uuid;

async fn seed_model(pool: &sqlx::Pool<sqlx::MySql>, model_name: &str) -> String {
    let model_id = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO infra_llm_models \
         (model_id, model_name, provider, base_url, is_active, context_window, \
          input_modalities, output_modalities, supported_parameters, pricing, tags, quirks) \
         VALUES (?, ?, 'mock', 'http://127.0.0.1:1', 1, 128000, \
          ?, ?, ?, ?, ?, ?)",
    )
    .bind(&model_id)
    .bind(model_name)
    .bind(r#"["text"]"#)
    .bind(r#"["text"]"#)
    .bind("[]")
    .bind("{}")
    .bind("[]")
    .bind("{}")
    .execute(pool)
    .await
    .expect("seed model");
    model_id
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn database_model_corrupt_capability_and_json_shape_fail_loud() {
    let (shared_pool, settings) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get().clone();
    let service = DatabaseModelService::new(
        settings,
        Arc::new(FernetTokenEncryptor::new("models-db-it-key").expect("test encryptor")),
    )
    .with_pool(shared_pool);
    let model_name = format!("model_{}", Uuid::new_v4().simple());
    seed_model(&pool, &model_name).await;

    sqlx::query("UPDATE infra_llm_models SET thinking_capability = ? WHERE model_name = ?")
        .bind("mystery")
        .bind(&model_name)
        .execute(&pool)
        .await
        .expect("corrupt thinking_capability");

    let err = match service.get_model(model_name.clone()).await {
        Ok(_) => panic!("unknown persisted thinking_capability must fail loudly"),
        Err(err) => err,
    };
    assert_eq!(err.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        err.1
            .detail
            .contains("infra_llm_models.thinking_capability"),
        "unexpected error detail: {}",
        err.1.detail
    );

    sqlx::query("UPDATE infra_llm_models SET thinking_capability = NULL, input_modalities = ? WHERE model_name = ?")
        .bind("null")
        .bind(&model_name)
        .execute(&pool)
        .await
        .expect("corrupt input_modalities shape");

    let err = match service.get_model(model_name.clone()).await {
        Ok(_) => panic!("invalid persisted input_modalities shape must fail loudly"),
        Err(err) => err,
    };
    assert_eq!(err.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        err.1
            .detail
            .contains("infra_llm_models.input_modalities_json"),
        "unexpected error detail: {}",
        err.1.detail
    );

    sqlx::query("DELETE FROM infra_llm_models WHERE model_name = ?")
        .bind(&model_name)
        .execute(&pool)
        .await
        .expect("clean corrupt model fixture");
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn effective_offering_resolution_is_exact_active_and_secret_safe() {
    let (shared_pool, settings) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get().clone();
    let encryptor = FernetTokenEncryptor::new("models-offering-db-it-key").expect("test encryptor");
    let model_name = format!("offering_model_{}", Uuid::new_v4().simple());
    let offering_id = seed_model(&pool, &model_name).await;
    let encrypted_key = encryptor
        .encrypt("offering-secret")
        .expect("encrypt API key");
    sqlx::query("UPDATE infra_llm_models SET api_key_encrypted = ? WHERE model_id = ?")
        .bind(encrypted_key)
        .bind(&offering_id)
        .execute(&pool)
        .await
        .expect("attach encrypted API key");

    let resolved = resolve_active_llm_offering(&settings, &encryptor, &offering_id, Some(&pool))
        .await
        .expect("resolve exact Offering ID");
    assert_eq!(resolved.offering_id, offering_id);
    assert_eq!(resolved.model.model_name, model_name);
    assert_eq!(resolved.model.api_key, "offering-secret");
    assert!(!format!("{resolved:?}").contains("offering-secret"));

    let service = DatabaseModelService::new(settings.clone(), Arc::new(encryptor.clone()))
        .with_pool(shared_pool.clone());
    let admitted = service
        .resolve_model_offering(offering_id.clone())
        .await
        .expect("ModelService must materialize the same exact Offering");
    assert_eq!(admitted.offering_id, offering_id);
    assert_eq!(admitted.model.model_name, model_name);

    let rotated_key = encryptor
        .encrypt("rotated-offering-secret")
        .expect("encrypt rotated API key");
    sqlx::query("UPDATE infra_llm_models SET api_key_encrypted = ? WHERE model_id = ?")
        .bind(rotated_key)
        .bind(&offering_id)
        .execute(&pool)
        .await
        .expect("rotate Offering credential without process-local cache invalidation");
    let rotated = revalidate_active_llm_offering(&settings, &encryptor, &offering_id, Some(&pool))
        .await
        .expect("provider request boundary must materialize current route data");
    assert_eq!(rotated.model.api_key, "rotated-offering-secret");

    let error = resolve_active_llm_offering(&settings, &encryptor, &model_name, Some(&pool))
        .await
        .expect_err("model display/name must not act as Offering identity");
    assert_eq!(
        error,
        ModelOfferingResolutionError::NotFound {
            offering_id: model_name.clone(),
        }
    );

    sqlx::query("UPDATE infra_llm_models SET is_active = 0 WHERE model_id = ?")
        .bind(&offering_id)
        .execute(&pool)
        .await
        .expect("disable Offering fixture");
    let error = revalidate_active_llm_offering(&settings, &encryptor, &offering_id, Some(&pool))
        .await
        .expect_err("disabled Offering must fail closed");
    assert_eq!(
        error,
        ModelOfferingResolutionError::Inactive {
            offering_id: offering_id.clone(),
            model_name: model_name.clone(),
        }
    );

    sqlx::query("DELETE FROM infra_llm_models WHERE model_id = ?")
        .bind(&offering_id)
        .execute(&pool)
        .await
        .expect("clean Offering fixture");
    astra_services::models::invalidate_active_llm_model_resolution_cache();
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn user_byok_models_are_owner_scoped_encrypted_and_admitted_for_owner_only() {
    let (shared_pool, settings) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get().clone();
    let encryptor = FernetTokenEncryptor::new("user-models-db-it-key").expect("test encryptor");
    let service =
        DatabaseModelService::new(settings, Arc::new(encryptor.clone())).with_pool(shared_pool);
    let alias = format!("byok_{}", Uuid::new_v4().simple());
    let user_a = format!("user_a_{}", Uuid::new_v4().simple());
    let user_b = format!("user_b_{}", Uuid::new_v4().simple());
    let model_a = Uuid::new_v4().to_string();
    let model_b = Uuid::new_v4().to_string();
    let secret_a = "user-a-provider-secret";
    let secret_b = "user-b-provider-secret";

    for (user_id, model_id, secret) in
        [(&user_a, &model_a, secret_a), (&user_b, &model_b, secret_b)]
    {
        sqlx::query(
            "INSERT INTO user_llm_models \
             (model_id, user_id, model_alias, model_name, provider, api_key_encrypted, base_url, \
              context_window, is_default, is_active) \
             VALUES (?, ?, ?, 'deepseek-chat', 'deepseek', ?, 'https://api.deepseek.com', \
              128000, 1, 1)",
        )
        .bind(model_id)
        .bind(user_id)
        .bind(&alias)
        .bind(encryptor.encrypt(secret).expect("encrypt secret"))
        .execute(&pool)
        .await
        .expect("seed user model");
    }

    let stored: String = sqlx::query_scalar(
        "SELECT api_key_encrypted FROM user_llm_models WHERE user_id = ? AND model_id = ?",
    )
    .bind(&user_a)
    .bind(&model_a)
    .fetch_one(&pool)
    .await
    .expect("read encrypted secret");
    assert_ne!(stored, secret_a);
    assert!(!stored.contains(secret_a));

    let listed_a = service
        .list_user_models(user_a.clone())
        .await
        .expect("list user A");
    let listed_b = service
        .list_user_models(user_b.clone())
        .await
        .expect("list user B");
    assert_eq!(listed_a.len(), 1);
    assert_eq!(listed_b.len(), 1);
    assert_eq!(listed_a[0].name, alias);
    assert_eq!(
        listed_b[0].name, alias,
        "aliases are unique per owner, not globally"
    );

    let admitted = service
        .revalidate_model_execution(user_a.clone(), model_a.clone())
        .await
        .expect("owner admits model");
    assert_eq!(admitted.access_kind, ModelAccessKind::CloudByok);
    assert_eq!(admitted.server_material().unwrap().api_key, secret_a);
    assert_eq!(admitted.wire_model_name.as_deref(), Some("deepseek-chat"));
    assert!(!format!("{admitted:?}").contains(secret_a));

    let error = service
        .revalidate_model_execution(user_b.clone(), model_a.clone())
        .await
        .expect_err("another user cannot admit the Offering");
    assert_eq!(error.0, StatusCode::NOT_FOUND);

    for (user_id, model_id) in [(&user_a, &model_a), (&user_b, &model_b)] {
        sqlx::query("DELETE FROM user_llm_models WHERE user_id = ? AND model_id = ?")
            .bind(user_id)
            .bind(model_id)
            .execute(&pool)
            .await
            .expect("clean user model fixture");
    }
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn compatible_byok_admission_rechecks_trust_and_owner() {
    let (shared_pool, settings) = common::setup_pool_and_settings().await;
    // Run this same production entrypoint in both deployment modes.
    let strict = std::env::var("ASTRA_BYOK_ENDPOINT_POLICY").as_deref() == Ok("trusted-domains");
    let pool = shared_pool.get().clone();
    let encryptor = FernetTokenEncryptor::new("compatible-db-it-key").unwrap();
    let service =
        DatabaseModelService::new(settings, Arc::new(encryptor.clone())).with_pool(shared_pool);
    let owner = format!("compatible_{}", Uuid::new_v4().simple());
    let model_id = Uuid::new_v4().to_string();
    let host = format!("{}.example.com", Uuid::new_v4().simple());
    let domain_id = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO user_llm_models (model_id, user_id, model_alias, model_name, provider, \
        api_key_encrypted, base_url, context_window, is_default, is_active) \
        VALUES (?, ?, 'gateway', 'upstream-model', 'openai-compatible', ?, ?, 128000, 1, 1)",
    )
    .bind(&model_id)
    .bind(&owner)
    .bind(encryptor.encrypt("test-secret").unwrap())
    .bind(format!("https://{host}/v1"))
    .execute(&pool)
    .await
    .unwrap();
    let initial = service
        .revalidate_model_execution(owner.clone(), model_id.clone())
        .await;
    assert_eq!(
        initial.is_err(),
        strict,
        "default public policy must not require registry setup"
    );
    sqlx::query("INSERT INTO runtime_llm_trusted_domains (domain_id, domain_host, domain_port, is_enabled) VALUES (?, ?, 443, 1)")
        .bind(&domain_id).bind(&host).execute(&pool).await.unwrap();
    let admitted = service
        .revalidate_model_execution(owner.clone(), model_id.clone())
        .await
        .unwrap();
    assert_eq!(admitted.wire_model_name.as_deref(), Some("upstream-model"));
    assert_eq!(admitted.provider, "openai-compatible");
    sqlx::query("UPDATE user_llm_models SET base_url = ? WHERE model_id = ?")
        .bind(format!("https://{host}:8443/v1"))
        .bind(&model_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        service
            .revalidate_model_execution(owner.clone(), model_id.clone())
            .await
            .is_err(),
        strict,
        "strict mode must not allow unapproved ports"
    );
    sqlx::query("UPDATE user_llm_models SET base_url = ? WHERE model_id = ?")
        .bind(format!("https://{host}/v1"))
        .bind(&model_id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        service
            .revalidate_model_execution("another-user".into(), model_id.clone())
            .await
            .is_err()
    );
    sqlx::query("UPDATE runtime_llm_trusted_domains SET is_enabled = 0 WHERE domain_id = ?")
        .bind(&domain_id)
        .execute(&pool)
        .await
        .unwrap();
    let revoked = service
        .revalidate_model_execution(owner.clone(), model_id.clone())
        .await;
    assert_eq!(
        revoked.is_err(),
        strict,
        "strict policy must recheck revocation at admission"
    );
    sqlx::query("UPDATE user_llm_models SET base_url = 'https://127.0.0.1/v1' WHERE model_id = ?")
        .bind(&model_id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        service
            .revalidate_model_execution(owner.clone(), model_id.clone())
            .await
            .is_err(),
        "unsafe persisted endpoints must be denied in both modes"
    );
    let error = service
        .check_user_model(owner.clone(), model_id.clone())
        .await
        .unwrap_err();
    assert_eq!(error.0, StatusCode::BAD_REQUEST);
    assert!(!error.1.detail.contains("test-secret"));
    let error = service
        .validate_user_model_endpoint(owner.clone(), "https://169.254.169.254/v1".into())
        .await
        .unwrap_err();
    assert_eq!(error.0, StatusCode::BAD_REQUEST);
    sqlx::query("DELETE FROM user_llm_models WHERE user_id = ? AND model_id = ?")
        .bind(&owner)
        .bind(&model_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM runtime_llm_trusted_domains WHERE domain_id = ?")
        .bind(&domain_id)
        .execute(&pool)
        .await
        .unwrap();
}
