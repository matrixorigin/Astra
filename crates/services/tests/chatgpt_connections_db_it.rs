//! Non-billable OAuth/storage journey. Requires an explicitly isolated database.
mod common;
#[path = "common/isolated_database.rs"]
mod isolated_database;
#[allow(dead_code)]
mod fixture {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/common/chatgpt_fixture.rs"
    ));
}
use astra_services::chatgpt::{
    ChatgptConfig, ChatgptConnections, CompleteAuthorization, ConnectionOperation,
    StartAuthorization,
};
use astra_services::{
    DatabaseModelService, FernetTokenEncryptor, ModelService, models::UserModelCreateRequestData,
};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    sync::{Arc, atomic::Ordering},
};

async fn start(service: &Arc<ChatgptConnections>, owner: &str, connection: Option<&str>) -> Value {
    service
        .operate(
            owner,
            ConnectionOperation::Start(StartAuthorization {
                redirect_uri: "http://127.0.0.1:14555/auth/callback".into(),
                label: Some("Personal".into()),
                connection_id: connection.map(str::to_owned),
                retry_authorization_id: None,
            }),
        )
        .await
        .unwrap()
}
fn callback(value: &Value, code: &str) -> CompleteAuthorization {
    CompleteAuthorization {
        state: value["state"].as_str().unwrap().into(),
        code: Some(code.into()),
        client_id: Some("oaiapp_fixture".into()),
        error: None,
    }
}
async fn expire(pool: &sqlx::MySqlPool, key: &FernetTokenEncryptor, owner: &str, id: &str) {
    let encrypted:String=sqlx::query_scalar("SELECT credentials_encrypted FROM user_llm_connections WHERE user_id=? AND connection_id=?")
        .bind(owner).bind(id).fetch_one(pool).await.unwrap();
    let mut value: Value = serde_json::from_str(&key.decrypt(&encrypted).unwrap()).unwrap();
    value["expires_at"] = json!(chrono::Utc::now().timestamp() - 1);
    sqlx::query("UPDATE user_llm_connections SET credentials_encrypted=?,token_expires_at=? WHERE user_id=? AND connection_id=?")
        .bind(key.encrypt(&value.to_string()).unwrap()).bind(chrono::Utc::now().timestamp()-1).bind(owner).bind(id).execute(pool).await.unwrap();
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1 and an explicitly isolated MatrixOne database"]
async fn subscription_connection_ownership_refresh_models_and_disconnect() {
    let settings = common::require_db_it_env();
    isolated_database::require_isolated_database(&settings.database);
    let (shared, settings) = common::setup_pool_and_settings().await;
    let pool = shared.get().clone();
    let provider = fixture::ProviderFixture::start().await;
    let config = ChatgptConfig::for_test(&provider.base);
    let key = Arc::new(FernetTokenEncryptor::new("chatgpt-database-journey-test-key").unwrap());
    let service = Arc::new(ChatgptConnections::new(
        Some(shared.clone()),
        key.clone(),
        Ok(config.clone()),
    ));
    let owner = format!("owner-{}", uuid::Uuid::new_v4());
    let other = format!("other-{}", uuid::Uuid::new_v4());
    let attempt = start(&service, &owner, None).await;
    let id = attempt["authorization_id"].as_str().unwrap();
    let code = provider.issue_code(&attempt, "account-one", true);
    let mut wrong = callback(&attempt, &code);
    wrong.state = "wrong-state".into();
    assert_eq!(
        service
            .operate(&owner, ConnectionOperation::Complete(id.into(), wrong))
            .await
            .unwrap_err()
            .0,
        axum::http::StatusCode::BAD_REQUEST
    );
    assert_eq!(provider.state.token_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        service
            .operate(&other, ConnectionOperation::Cancel(id.into()))
            .await
            .unwrap_err()
            .0,
        axum::http::StatusCode::NOT_FOUND
    );
    assert_eq!(
        service
            .operate(
                &other,
                ConnectionOperation::Complete(id.into(), callback(&attempt, &code))
            )
            .await
            .unwrap_err()
            .0,
        axum::http::StatusCode::NOT_FOUND
    );
    let linked = service
        .operate(
            &owner,
            ConnectionOperation::Complete(id.into(), callback(&attempt, &code)),
        )
        .await
        .unwrap();
    let connection = linked["connection"]["connection_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(linked["connection"]["status"], "connected");
    let repeated = service
        .operate(
            &owner,
            ConnectionOperation::Complete(id.into(), callback(&attempt, &code)),
        )
        .await
        .unwrap();
    assert_eq!(linked, repeated);
    assert_eq!(provider.state.token_calls.load(Ordering::SeqCst), 1);
    let stored:String=sqlx::query_scalar("SELECT credentials_encrypted FROM user_llm_connections WHERE user_id=? AND connection_id=?")
        .bind(&owner).bind(&connection).fetch_one(&pool).await.unwrap();
    assert!(!stored.contains("fixture-access-"));
    let plaintext = key.decrypt(&stored).unwrap();
    assert!(plaintext.contains("fixture-access-"));
    let mut legacy: Value = serde_json::from_str(&plaintext).unwrap();
    assert!(legacy.get("id_token").is_none());
    // A connection saved by the earlier PR revision must still refresh. Its
    // unused ID-token field is ignored and omitted on the next credential write.
    legacy["id_token"] = json!("obsolete-id-token");
    sqlx::query("UPDATE user_llm_connections SET credentials_encrypted=? WHERE user_id=? AND connection_id=?")
        .bind(key.encrypt(&legacy.to_string()).unwrap()).bind(&owner).bind(&connection).execute(&pool).await.unwrap();
    let metadata = service
        .operate(&owner, ConnectionOperation::List)
        .await
        .unwrap()
        .to_string();
    assert!(
        !metadata.contains("fixture-access-")
            && !metadata.contains("fixture-refresh-")
            && !metadata.contains("id_token")
    );
    assert_eq!(
        service
            .operate(&other, ConnectionOperation::Models(connection.clone()))
            .await
            .unwrap_err()
            .0,
        axum::http::StatusCode::NOT_FOUND
    );

    let models = DatabaseModelService::new(settings.clone(), key.clone())
        .with_pool(shared.clone())
        .with_chatgpt_config(config.clone());
    let mut model_ids = Vec::new();
    for (index, slug) in ["fixture-model", "fixture-second"].into_iter().enumerate() {
        let model = models
            .create_user_model(
                owner.clone(),
                UserModelCreateRequestData {
                    name: format!("subscription-{index}"),
                    provider: "chatgpt".into(),
                    model: slug.into(),
                    base_url: None,
                    api_key: String::new(),
                    connection_id: Some(connection.clone()),
                    context_window: 128000,
                    is_default: index == 0,
                },
            )
            .await
            .unwrap();
        model_ids.push(model.model_id);
    }
    let catalog = models.list_models(owner.clone(), false).await.unwrap();
    let own: Vec<_> = catalog
        .iter()
        .filter(|item| model_ids.contains(&item.offering_id))
        .collect();
    assert_eq!(own.len(), 2);
    assert!(own.iter().all(|item| item.access_id==format!("chatgpt:{connection}") && item.pricing.is_none()));
    assert!(
        models
            .list_models(other.clone(), false)
            .await
            .unwrap()
            .iter()
            .all(|item| !model_ids.contains(&item.offering_id))
    );
    let first = models
        .admit_model_offering(owner.clone(), model_ids[0].clone())
        .await
        .unwrap()
        .subscription
        .unwrap();
    let second = DatabaseModelService::new(settings.clone(), key.clone())
        .with_pool(shared.clone())
        .with_chatgpt_config(config.clone())
        .admit_model_offering(owner.clone(), model_ids[1].clone())
        .await
        .unwrap()
        .subscription
        .unwrap();
    expire(&pool, &key, &owner, &connection).await;
    let (a, b) = tokio::join!(first.acquire(false), second.acquire(false));
    let a = a.unwrap();
    let b = b.unwrap();
    assert_eq!(a.bearer(), b.bearer());
    assert_eq!(provider.state.refresh_calls.load(Ordering::SeqCst), 1);
    let refreshed: String=sqlx::query_scalar("SELECT credentials_encrypted FROM user_llm_connections WHERE user_id=? AND connection_id=?")
        .bind(&owner).bind(&connection).fetch_one(&pool).await.unwrap();
    let refreshed: Value = serde_json::from_str(&key.decrypt(&refreshed).unwrap()).unwrap();
    assert!(refreshed.get("id_token").is_none());
    drop((a, b));
    let live = first.acquire(false).await.unwrap();
    let disconnected = service
        .operate(&owner, ConnectionOperation::Disconnect(connection.clone()))
        .await
        .unwrap();
    assert_eq!(disconnected["status"], "disconnected");
    assert_eq!(disconnected["remote_revocation_confirmed"], true);
    assert!(first.ensure_current().await.is_err());
    // This binding belongs to another service instance: database fencing is
    // authoritative even when local cancellation registries are different.
    assert!(second.ensure_current().await.is_err());
    drop(live);
    assert_eq!(
        models
            .default_user_model_offering_id(owner.clone())
            .await
            .unwrap(),
        Some(model_ids[0].clone())
    );
    assert!(
        models
            .default_chat_model_offering_id(owner.clone())
            .await
            .is_err(),
        "a disconnected subscription default must not choose a deployment model"
    );
    assert!(
        models
            .admit_model_offering(owner.clone(), model_ids[0].clone())
            .await
            .is_err()
    );

    // A reauthorization belongs to the existing verified identity, not email.
    let relink = start(&service, &owner, Some(&connection)).await;
    let wrong_code = provider.issue_code(&relink, "another-account", true);
    assert!(
        service
            .operate(
                &owner,
                ConnectionOperation::Complete(
                    relink["authorization_id"].as_str().unwrap().into(),
                    callback(&relink, &wrong_code)
                )
            )
            .await
            .is_err()
    );
    let good = service
        .operate(
            &owner,
            ConnectionOperation::Start(StartAuthorization {
                redirect_uri: "http://127.0.0.1:14555/auth/callback".into(),
                label: None,
                connection_id: None,
                retry_authorization_id: Some(relink["authorization_id"].as_str().unwrap().into()),
            }),
        )
        .await
        .unwrap();
    let retry_url = reqwest::Url::parse(good["authorization_url"].as_str().unwrap()).unwrap();
    assert!(
        retry_url
            .query_pairs()
            .any(|(key, value)| key == "client_id" && value == "oaiapp_fixture")
    );
    assert!(
        !retry_url
            .query_pairs()
            .any(|(key, _)| key == "agent_name_hint")
    );
    let good_code = provider.issue_code(&good, "account-one", true);
    service
        .operate(
            &owner,
            ConnectionOperation::Complete(
                good["authorization_id"].as_str().unwrap().into(),
                callback(&good, &good_code),
            ),
        )
        .await
        .unwrap();
    let renewed = service.binding(&owner, &connection).await.unwrap();
    assert!(
        first.ensure_current().await.is_err(),
        "old generation must remain fenced"
    );
    let bytes = renewed.acquire(false).await.unwrap();
    assert!(!bytes.bearer().is_empty());
    drop(bytes);
    expire(&pool, &key, &owner, &connection).await;
    provider.state.fail_refresh.store(true, Ordering::SeqCst);
    assert!(renewed.acquire(false).await.is_err());
    assert_eq!(
        service
            .operate(&owner, ConnectionOperation::List)
            .await
            .unwrap()["items"][0]["status"],
        "reauth_required"
    );

    let no_plan = start(&service, &other, None).await;
    let no_plan_code = provider.issue_code(&no_plan, "account-two", false);
    let disabled = service
        .operate(
            &other,
            ConnectionOperation::Complete(
                no_plan["authorization_id"].as_str().unwrap().into(),
                callback(&no_plan, &no_plan_code),
            ),
        )
        .await
        .unwrap();
    let disabled_id = disabled["connection"]["connection_id"].as_str().unwrap();
    assert_eq!(disabled["connection"]["status"], "plan_disabled");
    assert_eq!(
        service.binding(&other, disabled_id).await.unwrap_err().0,
        axum::http::StatusCode::FORBIDDEN
    );
    let consent = start(&service, &other, Some(disabled_id)).await;
    let url = reqwest::Url::parse(consent["authorization_url"].as_str().unwrap()).unwrap();
    assert!(
        url.query_pairs()
            .any(|(key, value)| key == "prompt" && value == "consent")
    );
    assert!(!url.query_pairs().any(|(key, _)| key == "agent_name_hint"));
    service
        .operate(
            &other,
            ConnectionOperation::Cancel(consent["authorization_id"].as_str().unwrap().into()),
        )
        .await
        .unwrap();
    let codes: HashSet<_> = provider
        .state
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|request| request["model"].as_str().unwrap().to_owned())
        .collect();
    assert!(codes.contains("fixture-model") && codes.contains("fixture-second"));
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1 and an explicitly isolated MatrixOne database"]
async fn legacy_byok_rows_survive_idempotent_connection_migration() {
    let settings = common::require_db_it_env();
    isolated_database::require_isolated_database(&settings.database);
    let (shared, mut settings) = common::setup_pool_and_settings().await;
    let database = format!("sub_auth_migration_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE `{database}`")))
        .execute(shared.get())
        .await
        .unwrap();
    settings.database = database.clone();
    let migration_pool = astra_core::SharedPool::new(&settings).await.unwrap();
    let pool = migration_pool.get();
    sqlx::query("CREATE TABLE user_llm_models (model_id VARCHAR(64) PRIMARY KEY, api_key_encrypted TEXT NOT NULL)").execute(pool).await.unwrap();
    let key = FernetTokenEncryptor::new("legacy-migration-test-key").unwrap();
    let ciphertext = key.encrypt("legacy-fake-byok-key").unwrap();
    sqlx::query("INSERT INTO user_llm_models (model_id,api_key_encrypted) VALUES ('legacy',?)")
        .bind(&ciphertext)
        .execute(pool)
        .await
        .unwrap();
    astra_services::chatgpt::bootstrap(pool).await.unwrap();
    astra_services::chatgpt::bootstrap(pool).await.unwrap();
    let stored: (String, Option<String>) = sqlx::query_as(
        "SELECT api_key_encrypted,connection_id FROM user_llm_models WHERE model_id='legacy'",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(stored, (ciphertext, None));
    sqlx::query("INSERT INTO user_llm_models (model_id,api_key_encrypted,connection_id) VALUES ('subscription',NULL,'fixture-connection')").execute(pool).await.unwrap();
    migration_pool.get().close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP DATABASE `{database}`")))
        .execute(shared.get())
        .await
        .unwrap();
}
