//! Opt-in contract against a running Memoria API, not a whoami fixture.
mod common;
#[path = "common/isolated_database.rs"]
mod isolated_database;

use astra_core::JwtSettings;
use astra_services::{
    AuthService, DatabaseAuthService, FernetTokenEncryptor,
    auth::{AuthRefreshRequestData, memoria::MemoryAccess},
};
use axum::http::StatusCode;
use serde_json::{Value, json};

#[tokio::test]
#[ignore = "requires isolated ASTRA_TEST_DATABASE and ASTRA_TEST_MEMORIA_URL / ASTRA_TEST_MEMORIA_MASTER_KEY"]
async fn scoped_key_api_v1_preserves_identity_modes_and_revocation() {
    isolated_database::require_isolated_database(&common::require_db_it_env().database);
    let base = std::env::var("ASTRA_TEST_MEMORIA_URL").unwrap();
    let master = std::env::var("ASTRA_TEST_MEMORIA_MASTER_KEY").unwrap();
    let (pool, settings) = common::setup_pool_and_settings().await;
    let auth = DatabaseAuthService::new(
        settings,
        JwtSettings {
            secret_key: "isolated-live-contract-jwt".into(),
            algorithm: "HS256".into(),
            access_token_expire_minutes: 90,
            refresh_token_expire_days: 7,
        },
    )
    .with_pool(pool)
    .with_encryptor(FernetTokenEncryptor::new("isolated-live-contract-encryption").unwrap())
    .with_memoria_base_url(base.clone());
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap();
    let owner = format!("review-704-{}", uuid::Uuid::new_v4());
    let mut account = None;

    // Broad/master keys cannot become ordinary Astra account credentials.
    assert_eq!(
        auth.login_memoria(&master).await.err().unwrap().0,
        StatusCode::UNAUTHORIZED
    );
    for (access, scopes) in [
        (MemoryAccess::None, vec!["identity:read"]),
        (MemoryAccess::ReadOnly, vec!["identity:read", "memory:read"]),
        (
            MemoryAccess::ReadWrite,
            vec!["identity:read", "memory:read", "memory:write"],
        ),
    ] {
        let issued: Value = client
            .post(format!("{base}/auth/keys"))
            .bearer_auth(&master)
            .json(&json!({"user_id": owner, "name": "isolated-astra-contract", "scopes": scopes}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let key = issued["raw_key"].as_str().unwrap();
        let key_id = issued["key_id"].as_str().unwrap();
        let login = auth.login_memoria(key).await.unwrap();
        assert_eq!(login.memory_access, access);
        if let Some(previous) = account.as_ref() {
            assert_eq!(&login.tokens.user_id, previous);
        } else {
            account = Some(login.tokens.user_id.clone());
        }
        let credential = auth
            .memoria_credentials()
            .unwrap()
            .resolve(&login.tokens.user_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(credential.owner, owner);
        assert_eq!(credential.generation, key_id);
        assert_eq!(credential.access, access);
        let refreshed = auth
            .refresh(AuthRefreshRequestData {
                refresh_token: login.tokens.refresh_token,
            })
            .await
            .unwrap();
        assert_eq!(refreshed.expires_in, 900);

        client
            .delete(format!("{base}/auth/keys/{key_id}"))
            .bearer_auth(&master)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        assert_eq!(
            auth.refresh(AuthRefreshRequestData {
                refresh_token: refreshed.refresh_token,
            })
            .await
            .unwrap_err()
            .0,
            StatusCode::UNAUTHORIZED
        );
    }
    let account = account.unwrap();
    auth.disconnect_memoria(&account).await.unwrap();
    assert!(
        auth.memoria_credentials()
            .unwrap()
            .resolve(&account)
            .await
            .unwrap()
            .is_none()
    );
}
