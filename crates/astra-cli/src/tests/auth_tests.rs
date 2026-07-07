use super::spawn_mock;
use crate::cli::auth_flow::{do_login, do_register};
use crate::cli::cli_config::cli_utils::{
    CredentialsFile, Profile, load_credentials, save_credentials,
};
use crate::tests::isolate_credentials;
use axum::{Router, routing::post};

// ── auth_flow ─────────────────────────────────────────────────────────

#[serial_test::serial]
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

#[serial_test::serial]
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

#[serial_test::serial]
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

#[serial_test::serial]
#[tokio::test]
async fn do_login_preserves_last_session_for_same_username() {
    let _creds_dir = isolate_credentials();
    let mut creds = CredentialsFile::default();
    creds.profiles.insert(
        "test-profile".to_string(),
        Profile {
            username: Some("user1".to_string()),
            last_session_id: Some("sess-123".to_string()),
            ..Default::default()
        },
    );
    save_credentials(&creds).unwrap();

    let app = Router::new().route(
        "/auth/login",
        post(|| async {
            axum::Json(serde_json::json!({
                "access_token": "tok-new",
                "refresh_token": "ref-new"
            }))
        }),
    );
    let base = spawn_mock(app).await;
    let api = astra_thin_client::ThinClient::new(&base, None).unwrap();

    do_login(&api, Some("test-profile"), "user1", "pass1")
        .await
        .unwrap();

    let creds = load_credentials();
    assert_eq!(
        creds.profiles["test-profile"].last_session_id.as_deref(),
        Some("sess-123")
    );
}

#[serial_test::serial]
#[tokio::test]
async fn do_login_clears_last_session_for_different_username() {
    let _creds_dir = isolate_credentials();
    let mut creds = CredentialsFile::default();
    creds.profiles.insert(
        "test-profile".to_string(),
        Profile {
            username: Some("user1".to_string()),
            last_session_id: Some("sess-123".to_string()),
            memoria_api_key: Some("mem-key".to_string()),
            ..Default::default()
        },
    );
    save_credentials(&creds).unwrap();

    let app = Router::new().route(
        "/auth/login",
        post(|| async {
            axum::Json(serde_json::json!({
                "access_token": "tok-new",
                "refresh_token": "ref-new"
            }))
        }),
    );
    let base = spawn_mock(app).await;
    let api = astra_thin_client::ThinClient::new(&base, None).unwrap();

    do_login(&api, Some("test-profile"), "user2", "pass1")
        .await
        .unwrap();

    let creds = load_credentials();
    let profile = &creds.profiles["test-profile"];
    assert_eq!(profile.last_session_id, None);
    assert_eq!(profile.memoria_api_key.as_deref(), Some("mem-key"));
}

#[serial_test::serial]
#[tokio::test]
async fn do_login_uses_astra_profile_when_cli_profile_absent() {
    let _creds_dir = isolate_credentials();
    // Use a manual guard because this async test must keep ASTRA_PROFILE set
    // across awaits while the credentials env mutex serializes related tests.
    struct ProfileEnvGuard(Option<String>);
    impl Drop for ProfileEnvGuard {
        fn drop(&mut self) {
            match self.0.as_deref() {
                Some(value) => unsafe { std::env::set_var("ASTRA_PROFILE", value) },
                None => unsafe { std::env::remove_var("ASTRA_PROFILE") },
            }
        }
    }
    let _profile_env = ProfileEnvGuard(std::env::var("ASTRA_PROFILE").ok());
    unsafe { std::env::set_var("ASTRA_PROFILE", "from-env") };

    let app = Router::new().route(
        "/auth/login",
        post(|| async {
            axum::Json(serde_json::json!({
                "access_token": "tok-env",
                "refresh_token": "ref-env"
            }))
        }),
    );
    let base = spawn_mock(app).await;
    let api = astra_thin_client::ThinClient::new(&base, None).unwrap();

    do_login(&api, None, "user1", "pass1").await.unwrap();

    let creds = load_credentials();
    assert!(creds.profiles.contains_key("from-env"));
    assert_eq!(creds.current_profile.as_deref(), Some("from-env"));
}

#[serial_test::serial]
#[tokio::test]
async fn do_register_success() {
    let _creds_dir = isolate_credentials();
    let app = Router::new().route(
        "/auth/register",
        post(|| async {
            axum::Json(serde_json::json!({
                "user_id": "user-123",
                "username": "newuser",
                "email": "a@b.com",
                "display_name": null,
                "access_token": "tok-new",
                "refresh_token": "ref-new",
                "token_type": "Bearer",
                "expires_in": 3600
            }))
        }),
    );
    let base = spawn_mock(app).await;
    let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
    let result = do_register(&api, Some("test-profile"), "newuser", "a@b.com", "pass").await;
    assert_eq!(result.unwrap(), "tok-new");

    let creds = load_credentials();
    let profile = creds.profiles.get("test-profile").unwrap();
    assert_eq!(profile.username.as_deref(), Some("newuser"));
    assert_eq!(profile.access_token.as_deref(), Some("tok-new"));
    assert_eq!(profile.refresh_token.as_deref(), Some("ref-new"));
}

#[serial_test::serial]
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
    let result = do_register(&api, Some("test-profile"), "taken", "a@b.com", "pass").await;
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("409"));
}
