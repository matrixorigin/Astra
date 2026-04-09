use super::*;
use crate::cli_utils::{CredentialsFile, Profile};

// ── auth_flow ─────────────────────────────────────────────────────────

#[tokio::test]
async fn do_login_success() {
    let _creds_dir = isolate_credentials();
    let app = Router::new().route(
        "/auth/login",
        post(|| async {
            axum::Json(serde_json::json!({
                "access_token": "tok-abc",
                "refresh_token": "ref-xyz"
            }))
        }),
    );
    let base = spawn_mock(app).await;
    let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
    let result = do_login(&api, Some("__test__"), "user1", "pass1").await;
    assert_eq!(result.unwrap(), "tok-abc");
}

#[tokio::test]
async fn do_login_failure_returns_error() {
    let _creds_dir = isolate_credentials();
    let app = Router::new().route(
        "/auth/login",
        post(|| async {
            (
                axum::http::StatusCode::UNAUTHORIZED,
                axum::Json(serde_json::json!({"detail": "bad credentials"})),
            )
        }),
    );
    let base = spawn_mock(app).await;
    let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
    let result = do_login(&api, Some("test-profile"), "user1", "wrong").await;
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("401"));
}

#[tokio::test]
async fn do_login_preserves_existing_memoria_api_key() {
    let _creds_dir = isolate_credentials();
    let mut creds = CredentialsFile::default();
    creds.profiles.insert(
        "test-profile".to_string(),
        Profile {
            memoria_api_key: Some("mem-key".to_string()),
            ..Default::default()
        },
    );
    save_credentials(&creds).unwrap();

    let app = Router::new().route(
        "/auth/login",
        post(|| async {
            axum::Json(serde_json::json!({
                "access_token": "tok-abc",
                "refresh_token": "ref-xyz"
            }))
        }),
    );
    let base = spawn_mock(app).await;
    let api = astra_thin_client::ThinClient::new(&base, None).unwrap();

    let result = do_login(&api, Some("test-profile"), "user1", "pass1").await;
    assert_eq!(result.unwrap(), "tok-abc");

    let creds = load_credentials();
    let profile = creds.profiles.get("test-profile").unwrap();
    assert_eq!(profile.memoria_api_key.as_deref(), Some("mem-key"));
    assert_eq!(profile.access_token.as_deref(), Some("tok-abc"));
    assert_eq!(profile.refresh_token.as_deref(), Some("ref-xyz"));
}

#[tokio::test]
async fn do_register_success() {
    let _creds_dir = isolate_credentials();
    let app = Router::new().route(
        "/auth/register",
        post(|| async { axum::Json(serde_json::json!({"ok": true})) }),
    );
    let base = spawn_mock(app).await;
    let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
    let result = do_register(&api, "newuser", "a@b.com", "pass").await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn do_register_conflict_returns_error() {
    let _creds_dir = isolate_credentials();
    let app = Router::new().route(
        "/auth/register",
        post(|| async {
            (
                axum::http::StatusCode::CONFLICT,
                axum::Json(serde_json::json!({"detail": "username taken"})),
            )
        }),
    );
    let base = spawn_mock(app).await;
    let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
    let result = do_register(&api, "taken", "a@b.com", "pass").await;
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("409"));
}
