use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use axum::{
    Json, body,
    http::{HeaderMap, Request, StatusCode},
};
use mo_agent_runtime::marketplace::{
    CredentialRequestData, InstallRequestData, InstallationResponse, InstalledListResponse,
    MarketplaceService, StatusResponse,
};
use mo_agent_runtime::{
    AppState, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
    AuthTokenRecord, AuthUserRecord, ErrorResponse, FernetTokenEncryptor, HealthChecker,
    ServiceInfo, build_app,
};
use tokio::sync::Mutex;
use tower::util::ServiceExt;

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
        _: AuthRegisterRequestData,
    ) -> Result<AuthUserRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }

    async fn login(
        &self,
        _: AuthLoginRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }

    async fn refresh(
        &self,
        _: AuthRefreshRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }

    async fn logout(
        &self,
        _: AuthRefreshRequestData,
    ) -> Result<(), (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }

    async fn current_user(
        &self,
        headers: &HeaderMap,
    ) -> Result<AuthUserRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        let user_id = headers
            .get("X-User-Id")
            .and_then(|v| v.to_str().ok())
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                (
                    StatusCode::UNAUTHORIZED,
                    axum::Json(ErrorResponse {
                        detail: "Missing X-User-Id header".to_string(),
                    }),
                )
            })?;

        Ok(AuthUserRecord {
            user_id: user_id.to_string(),
            username: format!("user-{user_id}"),
            email: format!("{user_id}@example.test"),
            display_name: None,
        })
    }
}

#[derive(Clone)]
struct InMemoryMarketplaceService {
    installations: Arc<Mutex<HashMap<(String, String), InstallationResponse>>>,
    credentials: Arc<Mutex<HashSet<(String, String, String)>>>,
}

impl InMemoryMarketplaceService {
    fn new() -> Self {
        Self {
            installations: Arc::new(Mutex::new(HashMap::new())),
            credentials: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    fn installation_id(user_id: &str, skill_name: &str) -> String {
        format!("{user_id}:{skill_name}")
    }
}

#[async_trait]
impl MarketplaceService for InMemoryMarketplaceService {
    async fn install_skill(
        &self,
        user_id: String,
        request: InstallRequestData,
    ) -> Result<InstallationResponse, (StatusCode, Json<ErrorResponse>)> {
        if request.skill_name == "missing-skill" {
            return Err((
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    detail: "Skill not found".to_string(),
                }),
            ));
        }

        let response = InstallationResponse {
            installation_id: Self::installation_id(&user_id, &request.skill_name),
            skill_name: request.skill_name,
            skill_version: "1.0.0".to_string(),
            status: "installed".to_string(),
            installed_at: "2026-01-01T00:00:00".to_string(),
        };

        self.installations
            .lock()
            .await
            .insert((user_id, response.skill_name.clone()), response.clone());

        Ok(response)
    }

    async fn uninstall_skill(
        &self,
        user_id: String,
        request: InstallRequestData,
    ) -> Result<StatusResponse, (StatusCode, Json<ErrorResponse>)> {
        let removed = self
            .installations
            .lock()
            .await
            .remove(&(user_id, request.skill_name));

        if removed.is_none() {
            return Err((
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    detail: "Skill not installed".to_string(),
                }),
            ));
        }

        Ok(StatusResponse {
            status: "uninstalled".to_string(),
        })
    }

    async fn upgrade_skill(
        &self,
        user_id: String,
        request: InstallRequestData,
    ) -> Result<InstallationResponse, (StatusCode, Json<ErrorResponse>)> {
        let mut installations = self.installations.lock().await;
        let installation = installations
            .get_mut(&(user_id.clone(), request.skill_name.clone()))
            .ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    Json(ErrorResponse {
                        detail: "Skill not installed".to_string(),
                    }),
                )
            })?;

        installation.skill_version = "2.0.0".to_string();
        installation.status = "upgraded".to_string();
        Ok(installation.clone())
    }

    async fn rollback_skill(
        &self,
        user_id: String,
        request: InstallRequestData,
    ) -> Result<InstallationResponse, (StatusCode, Json<ErrorResponse>)> {
        let mut installations = self.installations.lock().await;
        let installation = installations
            .get_mut(&(user_id.clone(), request.skill_name.clone()))
            .ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    Json(ErrorResponse {
                        detail: "Skill not installed".to_string(),
                    }),
                )
            })?;

        installation.skill_version = "1.0.0".to_string();
        installation.status = "rolled_back".to_string();
        Ok(installation.clone())
    }

    async fn list_installed(
        &self,
        user_id: String,
        limit: i64,
        offset: i64,
    ) -> Result<InstalledListResponse, (StatusCode, Json<ErrorResponse>)> {
        let mut installations: Vec<InstallationResponse> = self
            .installations
            .lock()
            .await
            .iter()
            .filter(|((uid, _), _)| uid == &user_id)
            .map(|(_, installation)| installation.clone())
            .collect();

        installations.sort_by(|a, b| a.skill_name.cmp(&b.skill_name));

        let total = installations.len() as i64;
        let start = offset.max(0) as usize;
        let end = (start + limit.max(0) as usize).min(installations.len());
        let paged = if start < installations.len() {
            installations[start..end].to_vec()
        } else {
            Vec::new()
        };

        Ok(InstalledListResponse {
            installations: paged,
            total,
            limit,
            offset,
        })
    }

    async fn save_credential(
        &self,
        user_id: String,
        request: CredentialRequestData,
        _encryptor: &FernetTokenEncryptor,
    ) -> Result<StatusResponse, (StatusCode, Json<ErrorResponse>)> {
        self.credentials.lock().await.insert((
            user_id,
            request.skill_name,
            request.credential_name,
        ));

        Ok(StatusResponse {
            status: "saved".to_string(),
        })
    }

    async fn delete_credential(
        &self,
        user_id: String,
        skill_name: String,
        credential_name: String,
    ) -> Result<StatusResponse, (StatusCode, Json<ErrorResponse>)> {
        let removed = self
            .credentials
            .lock()
            .await
            .remove(&(user_id, skill_name, credential_name));

        if !removed {
            return Err((
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    detail: "Credential not found".to_string(),
                }),
            ));
        }

        Ok(StatusResponse {
            status: "deleted".to_string(),
        })
    }

    async fn publish_skill(
        &self,
        _user_id: String,
        _skill_name: String,
    ) -> Result<StatusResponse, (StatusCode, Json<ErrorResponse>)> {
        Ok(StatusResponse {
            status: "published".to_string(),
        })
    }

    async fn deprecate_skill(
        &self,
        _user_id: String,
        _skill_name: String,
    ) -> Result<StatusResponse, (StatusCode, Json<ErrorResponse>)> {
        Ok(StatusResponse {
            status: "deprecated".to_string(),
        })
    }
}

fn build_test_app() -> axum::Router {
    let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
        .with_auth_service(Arc::new(StubAuthService))
        .with_marketplace_service(Arc::new(InMemoryMarketplaceService::new()));
    build_app(state)
}

async fn response_json(resp: axum::http::Response<body::Body>) -> serde_json::Value {
    let bytes = body::to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn install_list_uninstall_flow_is_global_and_deterministic() {
    let app = build_test_app();

    let install_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/marketplace/install")
                .header("X-User-Id", "user-1")
                .header("content-type", "application/json")
                .body(body::Body::from(r#"{"skill_name":"skill-a"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(install_resp.status(), StatusCode::OK);

    let list_after_install = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/marketplace/installed?limit=10&offset=0")
                .header("X-User-Id", "user-1")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(list_after_install.status(), StatusCode::OK);
    let list_json = response_json(list_after_install).await;
    assert_eq!(list_json["total"], 1);
    assert_eq!(list_json["installations"][0]["skill_name"], "skill-a");

    let uninstall_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/marketplace/uninstall")
                .header("X-User-Id", "user-1")
                .header("content-type", "application/json")
                .body(body::Body::from(r#"{"skill_name":"skill-a"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(uninstall_resp.status(), StatusCode::NO_CONTENT);

    let list_after_uninstall = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/marketplace/installed?limit=10&offset=0")
                .header("X-User-Id", "user-1")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(list_after_uninstall.status(), StatusCode::OK);
    let empty_json = response_json(list_after_uninstall).await;
    assert_eq!(empty_json["total"], 0);
}

#[tokio::test]
async fn credential_save_and_delete_flow_returns_expected_statuses() {
    let app = build_test_app();

    let save_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/marketplace/credentials")
                .header("X-User-Id", "user-1")
                .header("content-type", "application/json")
                .body(body::Body::from(
                    r#"{"skill_name":"skill-a","credential_name":"api_key","value":"secret"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(save_resp.status(), StatusCode::OK);
    let save_json = response_json(save_resp).await;
    assert_eq!(save_json["status"], "saved");

    let delete_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/marketplace/credentials?skill_name=skill-a&credential_name=api_key")
                .header("X-User-Id", "user-1")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(delete_resp.status(), StatusCode::NO_CONTENT);

    let delete_again_resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/marketplace/credentials?skill_name=skill-a&credential_name=api_key")
                .header("X-User-Id", "user-1")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(delete_again_resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn upgrade_not_found_returns_404() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/marketplace/upgrade")
                .header("X-User-Id", "user-1")
                .header("content-type", "application/json")
                .body(body::Body::from(r#"{"skill_name":"skill-a"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn publish_and_deprecate_return_ok() {
    let app = build_test_app();

    let publish_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/marketplace/skills/skill-a/publish")
                .header("X-User-Id", "user-1")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(publish_resp.status(), StatusCode::OK);

    let deprecate_resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/marketplace/skills/skill-a/deprecate")
                .header("X-User-Id", "user-1")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(deprecate_resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn missing_user_id_returns_401() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/marketplace/install")
                .header("content-type", "application/json")
                .body(body::Body::from(r#"{"skill_name":"skill-a"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
