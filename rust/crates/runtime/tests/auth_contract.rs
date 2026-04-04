use std::{fs, path::PathBuf, sync::Arc};

use astra_runtime::{
    AppState, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
    AuthTokenRecord, AuthUserRecord, ErrorResponse, HealthChecker, ServiceInfo, build_app,
};
use async_trait::async_trait;
use axum::{
    Router, body,
    http::{Request, StatusCode},
};
use serde::Deserialize;
use tower::util::ServiceExt;

#[derive(Deserialize)]
struct ResponseContract {
    status: u16,
    json: serde_json::Value,
}

#[derive(Deserialize)]
struct RequestContract {
    request: serde_json::Value,
    status: u16,
    json: serde_json::Value,
}

#[derive(Deserialize)]
struct AuthContract {
    auth_error: ResponseContract,
    auth_register: RequestContract,
    auth_register_duplicate_username: RequestContract,
    auth_register_duplicate_email: RequestContract,
    auth_login: RequestContract,
    auth_login_invalid: RequestContract,
    auth_refresh: ResponseContract,
    auth_refresh_invalid: RequestContract,
    auth_refresh_wrong_type: ResponseContract,
    auth_logout: ResponseContract,
    auth_me: ResponseContract,
}

#[derive(Clone)]
struct StubHealthChecker;

#[async_trait]
impl HealthChecker for StubHealthChecker {
    async fn database_healthy(&self) -> bool {
        true
    }
}

#[derive(Clone)]
struct StubAuthService;

#[async_trait]
impl AuthService for StubAuthService {
    async fn register(
        &self,
        request: AuthRegisterRequestData,
    ) -> Result<AuthUserRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        if request.username == "contract-existing-username" {
            return Err((
                StatusCode::BAD_REQUEST,
                axum::Json(ErrorResponse {
                    detail: "Username already exists".to_string(),
                }),
            ));
        }
        if request.email == "contract-existing-email@test.com" {
            return Err((
                StatusCode::BAD_REQUEST,
                axum::Json(ErrorResponse {
                    detail: "Email already exists".to_string(),
                }),
            ));
        }

        Ok(AuthUserRecord {
            user_id: "contract-auth-user-id".to_string(),
            username: request.username,
            email: request.email,
            display_name: request.display_name,
        })
    }

    async fn login(
        &self,
        request: AuthLoginRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        // Accept both the login contract user and the register contract user (register auto-issues tokens)
        let valid_user = matches!(
            request.username.as_str(),
            "contract-login-user" | "contract-auth-user"
        );
        if !valid_user || request.password != "password123" {
            return Err((
                StatusCode::UNAUTHORIZED,
                axum::Json(ErrorResponse {
                    detail: "Invalid username or password".to_string(),
                }),
            ));
        }

        Ok(AuthTokenRecord {
            access_token: "contract-access-token".to_string(),
            refresh_token: "contract-refresh-token".to_string(),
            token_type: "bearer".to_string(),
            expires_in: 3600,
        })
    }

    async fn refresh(
        &self,
        request: AuthRefreshRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        match request.refresh_token.as_str() {
            "invalid_token" => Err((
                StatusCode::UNAUTHORIZED,
                axum::Json(ErrorResponse {
                    detail: "Invalid token".to_string(),
                }),
            )),
            "contract-access-token" => Err((
                StatusCode::UNAUTHORIZED,
                axum::Json(ErrorResponse {
                    detail: "Invalid token type".to_string(),
                }),
            )),
            "contract-refresh-token" => Ok(AuthTokenRecord {
                access_token: "contract-new-access-token".to_string(),
                refresh_token: "contract-new-refresh-token".to_string(),
                token_type: "bearer".to_string(),
                expires_in: 3600,
            }),
            _ => Err((
                StatusCode::UNAUTHORIZED,
                axum::Json(ErrorResponse {
                    detail: "Invalid token".to_string(),
                }),
            )),
        }
    }

    async fn logout(
        &self,
        _request: AuthRefreshRequestData,
    ) -> Result<(), (StatusCode, axum::Json<ErrorResponse>)> {
        Ok(())
    }

    async fn current_user(
        &self,
        headers: &axum::http::HeaderMap,
    ) -> Result<AuthUserRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        match headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
        {
            Some("Bearer contract-access-token") => Ok(AuthUserRecord {
                user_id: "contract-me-user-id".to_string(),
                username: "contract-me-user".to_string(),
                email: "contract-me-user@test.com".to_string(),
                display_name: None,
            }),
            _ => Err((
                StatusCode::UNAUTHORIZED,
                axum::Json(ErrorResponse {
                    detail: "Not authenticated".to_string(),
                }),
            )),
        }
    }
}

fn load_contract() -> AuthContract {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("fixtures/contracts/auth_contract.json");
    let content = fs::read_to_string(path).expect("auth contract fixture should exist");
    serde_json::from_str(&content).expect("auth contract fixture should be valid JSON")
}

fn build_app_with_auth() -> Router {
    build_app(
        AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
            .with_auth_service(Arc::new(StubAuthService)),
    )
}

async fn read_json(
    app: Router,
    path: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, serde_json::Value) {
    let response = app
        .oneshot(build_request("GET", path, headers))
        .await
        .unwrap();
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap();
    (status, json)
}

async fn post_json(
    app: Router,
    path: &str,
    headers: &[(&str, &str)],
    payload: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let response = app
        .oneshot(build_request_with_json("POST", path, headers, payload))
        .await
        .unwrap();
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap();
    (status, json)
}

fn build_request(method: &str, path: &str, headers: &[(&str, &str)]) -> Request<body::Body> {
    let mut builder = Request::builder().method(method).uri(path);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(body::Body::empty()).unwrap()
}

fn build_request_with_json(
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    payload: serde_json::Value,
) -> Request<body::Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(body::Body::from(payload.to_string())).unwrap()
}

#[tokio::test]
async fn auth_me_requires_auth() {
    let contract = load_contract();

    let (status, json) = read_json(build_app_with_auth(), "/auth/me", &[]).await;

    assert_eq!(status.as_u16(), contract.auth_error.status);
    assert_eq!(json, contract.auth_error.json);
}

#[tokio::test]
async fn auth_register_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = post_json(
        build_app_with_auth(),
        "/auth/register",
        &[],
        contract.auth_register.request.clone(),
    )
    .await;

    assert_eq!(status.as_u16(), contract.auth_register.status);
    assert_eq!(json["username"], contract.auth_register.json["username"]);
    assert_eq!(json["email"], contract.auth_register.json["email"]);
    assert_eq!(
        json["display_name"],
        contract.auth_register.json["display_name"]
    );
    assert_eq!(
        json["user_id"],
        serde_json::Value::String("contract-auth-user-id".into())
    );
    // Register now returns tokens — no extra login round-trip needed
    assert_eq!(
        json["access_token"],
        contract.auth_register.json["access_token"]
    );
    assert_eq!(
        json["refresh_token"],
        contract.auth_register.json["refresh_token"]
    );
    assert_eq!(
        json["token_type"],
        contract.auth_register.json["token_type"]
    );
}

#[tokio::test]
async fn auth_register_duplicate_username_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = post_json(
        build_app_with_auth(),
        "/auth/register",
        &[],
        contract.auth_register_duplicate_username.request.clone(),
    )
    .await;

    assert_eq!(
        status.as_u16(),
        contract.auth_register_duplicate_username.status
    );
    assert_eq!(json, contract.auth_register_duplicate_username.json);
}

#[tokio::test]
async fn auth_register_duplicate_email_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = post_json(
        build_app_with_auth(),
        "/auth/register",
        &[],
        contract.auth_register_duplicate_email.request.clone(),
    )
    .await;

    assert_eq!(
        status.as_u16(),
        contract.auth_register_duplicate_email.status
    );
    assert_eq!(json, contract.auth_register_duplicate_email.json);
}

#[tokio::test]
async fn auth_login_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = post_json(
        build_app_with_auth(),
        "/auth/login",
        &[],
        contract.auth_login.request.clone(),
    )
    .await;

    assert_eq!(status.as_u16(), contract.auth_login.status);
    assert_eq!(json["token_type"], contract.auth_login.json["token_type"]);
    assert_eq!(json["expires_in"], contract.auth_login.json["expires_in"]);
    assert_eq!(
        json["access_token"],
        serde_json::Value::String("contract-access-token".into())
    );
    assert_eq!(
        json["refresh_token"],
        serde_json::Value::String("contract-refresh-token".into())
    );
}

#[tokio::test]
async fn auth_login_invalid_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = post_json(
        build_app_with_auth(),
        "/auth/login",
        &[],
        contract.auth_login_invalid.request.clone(),
    )
    .await;

    assert_eq!(status.as_u16(), contract.auth_login_invalid.status);
    assert_eq!(json, contract.auth_login_invalid.json);
}

#[tokio::test]
async fn auth_refresh_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = post_json(
        build_app_with_auth(),
        "/auth/refresh",
        &[],
        serde_json::json!({"refresh_token": "contract-refresh-token"}),
    )
    .await;

    assert_eq!(status.as_u16(), contract.auth_refresh.status);
    assert_eq!(json["token_type"], contract.auth_refresh.json["token_type"]);
    assert_eq!(json["expires_in"], contract.auth_refresh.json["expires_in"]);
    assert_eq!(
        json["access_token"],
        serde_json::Value::String("contract-new-access-token".into())
    );
    assert_eq!(
        json["refresh_token"],
        serde_json::Value::String("contract-new-refresh-token".into())
    );
}

#[tokio::test]
async fn auth_refresh_invalid_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = post_json(
        build_app_with_auth(),
        "/auth/refresh",
        &[],
        contract.auth_refresh_invalid.request.clone(),
    )
    .await;

    assert_eq!(status.as_u16(), contract.auth_refresh_invalid.status);
    assert_eq!(json, contract.auth_refresh_invalid.json);
}

#[tokio::test]
async fn auth_refresh_wrong_type_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = post_json(
        build_app_with_auth(),
        "/auth/refresh",
        &[],
        serde_json::json!({"refresh_token": "contract-access-token"}),
    )
    .await;

    assert_eq!(status.as_u16(), contract.auth_refresh_wrong_type.status);
    assert_eq!(json, contract.auth_refresh_wrong_type.json);
}

#[tokio::test]
async fn auth_logout_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = post_json(
        build_app_with_auth(),
        "/auth/logout",
        &[],
        serde_json::json!({"refresh_token": "contract-refresh-token"}),
    )
    .await;

    assert_eq!(status.as_u16(), contract.auth_logout.status);
    assert_eq!(json, contract.auth_logout.json);
}

#[tokio::test]
async fn auth_me_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = read_json(
        build_app_with_auth(),
        "/auth/me",
        &[("authorization", "Bearer contract-access-token")],
    )
    .await;

    assert_eq!(status.as_u16(), contract.auth_me.status);
    assert_eq!(json["username"], contract.auth_me.json["username"]);
    assert_eq!(json["email"], contract.auth_me.json["email"]);
    assert_eq!(json["display_name"], contract.auth_me.json["display_name"]);
    assert_eq!(
        json["user_id"],
        serde_json::Value::String("contract-me-user-id".into())
    );
}
