mod common;

use std::sync::Arc;

use astra_services::{
    DatabaseModelService, FernetTokenEncryptor, ModelOfferingResolutionError, ModelService,
    resolve_active_llm_offering,
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

    let error = resolve_active_llm_offering(&settings, &encryptor, &model_name, Some(&pool))
        .await
        .expect_err("model display/name must not act as Offering identity");
    assert_eq!(
        error,
        ModelOfferingResolutionError::NotFound {
            offering_id: model_name.clone(),
        }
    );

    astra_services::models::invalidate_active_llm_model_resolution_cache();
    sqlx::query("UPDATE infra_llm_models SET is_active = 0 WHERE model_id = ?")
        .bind(&offering_id)
        .execute(&pool)
        .await
        .expect("disable Offering fixture");
    let error = resolve_active_llm_offering(&settings, &encryptor, &offering_id, Some(&pool))
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
