use std::sync::Arc;

use astra_runtime::{AppState, HealthChecker, ServiceInfo, build_app};
use astra_services::auth::TrustedMoiAuthService;
use async_trait::async_trait;
use axum::{
    Router,
    body::{self, Body},
    http::{Request, StatusCode},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tower::util::ServiceExt;

const TEST_SECRET: &str = "trusted_moi_test_secret_key_123456";
const FAR_FUTURE_EXP: u64 = 4_102_444_800; // 2100-01-01T00:00:00Z

#[derive(Clone)]
struct StubHealthChecker;

#[async_trait]
impl HealthChecker for StubHealthChecker {
    async fn database_healthy(&self) -> bool {
        true
    }
}

fn build_test_app() -> Router {
    let auth = TrustedMoiAuthService::new(TEST_SECRET, "HS256", None, None, 30)
        .expect("trusted_moi service should be constructible");
    let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
        .with_auth_service(Arc::new(auth));
    build_app(state)
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK_SIZE: usize = 64;
    let mut normalized_key = [0u8; BLOCK_SIZE];
    if key.len() > BLOCK_SIZE {
        let digest = Sha256::digest(key);
        normalized_key[..32].copy_from_slice(&digest);
    } else {
        normalized_key[..key.len()].copy_from_slice(key);
    }

    let mut inner_pad = [0x36u8; BLOCK_SIZE];
    let mut outer_pad = [0x5cu8; BLOCK_SIZE];
    for i in 0..BLOCK_SIZE {
        inner_pad[i] ^= normalized_key[i];
        outer_pad[i] ^= normalized_key[i];
    }

    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(message);
    let inner_digest = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner_digest);
    let outer_digest = outer.finalize();

    let mut out = [0u8; 32];
    out.copy_from_slice(&outer_digest);
    out
}

fn build_hs256_jwt(secret: &str, claims: Value) -> String {
    let header = json!({
        "alg": "HS256",
        "typ": "JWT"
    });
    let header_b64 = URL_SAFE_NO_PAD.encode(header.to_string().as_bytes());
    let claims_b64 = URL_SAFE_NO_PAD.encode(claims.to_string().as_bytes());
    let signing_input = format!("{header_b64}.{claims_b64}");
    let signature = hmac_sha256(secret.as_bytes(), signing_input.as_bytes());
    let signature_b64 = URL_SAFE_NO_PAD.encode(signature);
    format!("{signing_input}.{signature_b64}")
}

async fn post_json(app: Router, path: &str, payload: Value) -> (StatusCode, Value) {
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = serde_json::from_slice(&body).unwrap_or(Value::Null);
    (status, json)
}

async fn get_json_with_token(app: Router, path: &str, token: &str) -> (StatusCode, Value) {
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(path)
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = serde_json::from_slice(&body).unwrap_or(Value::Null);
    (status, json)
}

#[tokio::test]
async fn trusted_moi_login_endpoint_returns_forbidden() {
    let app = build_test_app();
    let (status, json) = post_json(
        app,
        "/auth/login",
        json!({
            "username": "moi-user",
            "password": "ignored"
        }),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        json["detail"],
        json!("Local auth endpoints are disabled in trusted_moi mode")
    );
}

#[tokio::test]
async fn trusted_moi_auth_me_accepts_valid_external_token() {
    let app = build_test_app();
    let token = build_hs256_jwt(
        TEST_SECRET,
        json!({
            "sub": "moi-user-123",
            "username": "moi_user",
            "email": "moi_user@example.com",
            "name": "Moi User",
            "exp": FAR_FUTURE_EXP
        }),
    );

    let (status, json) = get_json_with_token(app, "/auth/me", &token).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["user_id"], json!("moi-user-123"));
    assert_eq!(json["username"], json!("moi_user"));
    assert_eq!(json["email"], json!("moi_user@example.com"));
    assert_eq!(json["display_name"], json!("Moi User"));
}

#[tokio::test]
async fn trusted_moi_auth_me_falls_back_to_uid_and_name() {
    let app = build_test_app();
    let token = build_hs256_jwt(
        TEST_SECRET,
        json!({
            "uid": "moi-uid-456",
            "name": "Fallback Name",
            "exp": FAR_FUTURE_EXP
        }),
    );

    let (status, json) = get_json_with_token(app, "/auth/me", &token).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["user_id"], json!("moi-uid-456"));
    assert_eq!(json["username"], json!("Fallback Name"));
    assert_eq!(json["email"], json!(""));
    assert_eq!(json["display_name"], json!("Fallback Name"));
}

#[tokio::test]
async fn trusted_moi_auth_me_rejects_bad_signature() {
    let app = build_test_app();
    let token = build_hs256_jwt(
        "different-secret",
        json!({
            "sub": "moi-user-123",
            "exp": FAR_FUTURE_EXP
        }),
    );

    let (status, json) = get_json_with_token(app, "/auth/me", &token).await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(json["detail"], json!("Invalid trusted_moi token"));
}
