use std::collections::HashMap;
use std::sync::Arc;

use astra_runtime::skill_config::{
    BindResourceResponse, ConfigResponse, ResourceEntry, SkillConfigService, StatusResponse,
    UnbindResourceResponse, ValidationResponse,
};
use astra_runtime::{
    AppState, ErrorResponse, FernetTokenEncryptor, HealthChecker, ServiceInfo, build_app,
};
use async_trait::async_trait;
use axum::{
    Json, body,
    http::{Request, StatusCode},
};
use tokio::sync::Mutex;
use tower::util::ServiceExt;

// ── Stubs ────────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct StubHealthChecker;

#[async_trait]
impl HealthChecker for StubHealthChecker {
    async fn database_healthy(&self) -> bool {
        true
    }
}

// ── InMemory SkillConfigService ──────────────────────────────────────────────

#[derive(Clone, Debug)]
struct StoredSetting {
    value: serde_json::Value,
}

#[derive(Clone, Debug)]
struct StoredResource {
    resource_type: String,
}

type SettingsMap = Arc<Mutex<HashMap<(String, String, String, String), StoredSetting>>>;
type ResourcesMap = Arc<Mutex<HashMap<(String, String, String), StoredResource>>>;

#[derive(Clone)]
struct InMemorySkillConfigService {
    /// key: (user_id, skill_name, setting_name, scope)
    settings: SettingsMap,
    /// key: (user_id, skill_name, resource_key)
    resources: ResourcesMap,
}

impl InMemorySkillConfigService {
    fn new() -> Self {
        Self {
            settings: Arc::new(Mutex::new(HashMap::new())),
            resources: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

#[async_trait]
impl SkillConfigService for InMemorySkillConfigService {
    async fn validate_config(
        &self,
        _user_id: &str,
        _skill_name: &str,
        _resource_key: Option<&str>,
    ) -> Result<Json<ValidationResponse>, (StatusCode, Json<ErrorResponse>)> {
        Ok(Json(ValidationResponse {
            valid: true,
            errors: vec![],
        }))
    }

    async fn get_effective_config(
        &self,
        user_id: &str,
        skill_name: &str,
    ) -> Result<Json<ConfigResponse>, (StatusCode, Json<ErrorResponse>)> {
        let settings_map = self.settings.lock().await;
        let mut settings = HashMap::new();
        for ((uid, sname, skey, _scope), stored) in settings_map.iter() {
            if uid == user_id && sname == skill_name {
                settings.insert(skey.clone(), stored.value.clone());
            }
        }
        let resources_map = self.resources.lock().await;
        let resources_configured = resources_map
            .keys()
            .filter(|(uid, sname, _)| uid == user_id && sname == skill_name)
            .count() as i64;
        Ok(Json(ConfigResponse {
            settings,
            secrets: HashMap::new(),
            resources_configured,
        }))
    }

    async fn set_setting(
        &self,
        user_id: &str,
        skill_name: &str,
        setting_name: &str,
        scope: &str,
        value: serde_json::Value,
        _encryptor: &FernetTokenEncryptor,
    ) -> Result<Json<StatusResponse>, (StatusCode, Json<ErrorResponse>)> {
        let key = (
            user_id.to_string(),
            skill_name.to_string(),
            setting_name.to_string(),
            scope.to_string(),
        );
        self.settings
            .lock()
            .await
            .insert(key, StoredSetting { value });
        Ok(Json(StatusResponse {
            status: "ok".to_string(),
        }))
    }

    async fn delete_setting(
        &self,
        user_id: &str,
        skill_name: &str,
        setting_name: &str,
        scope: &str,
    ) -> Result<Json<StatusResponse>, (StatusCode, Json<ErrorResponse>)> {
        let key = (
            user_id.to_string(),
            skill_name.to_string(),
            setting_name.to_string(),
            scope.to_string(),
        );
        let removed = self.settings.lock().await.remove(&key);
        if removed.is_none() {
            return Err((
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    detail: format!(
                        "Setting '{}' not found for skill '{}' at scope '{}'",
                        setting_name, skill_name, scope
                    ),
                }),
            ));
        }
        Ok(Json(StatusResponse {
            status: "deleted".to_string(),
        }))
    }

    async fn list_resources(
        &self,
        user_id: &str,
        skill_name: &str,
    ) -> Result<Json<Vec<ResourceEntry>>, (StatusCode, Json<ErrorResponse>)> {
        let resources_map = self.resources.lock().await;
        let entries: Vec<ResourceEntry> = resources_map
            .iter()
            .filter(|((uid, sname, _), _)| uid == user_id && sname == skill_name)
            .map(|((_, _, rkey), stored)| ResourceEntry {
                resource_key: rkey.clone(),
                resource_type: stored.resource_type.clone(),
            })
            .collect();
        Ok(Json(entries))
    }

    async fn bind_resource(
        &self,
        user_id: &str,
        skill_name: &str,
        resource_key: &str,
        _bindings: HashMap<String, serde_json::Value>,
        _encryptor: &FernetTokenEncryptor,
    ) -> Result<Json<BindResourceResponse>, (StatusCode, Json<ErrorResponse>)> {
        let key = (
            user_id.to_string(),
            skill_name.to_string(),
            resource_key.to_string(),
        );
        self.resources.lock().await.insert(
            key,
            StoredResource {
                resource_type: "generic".to_string(),
            },
        );
        Ok(Json(BindResourceResponse {
            status: "ok".to_string(),
            resource_key: resource_key.to_string(),
        }))
    }

    async fn unbind_resource(
        &self,
        user_id: &str,
        skill_name: &str,
        resource_key: &str,
    ) -> Result<Json<UnbindResourceResponse>, (StatusCode, Json<ErrorResponse>)> {
        let key = (
            user_id.to_string(),
            skill_name.to_string(),
            resource_key.to_string(),
        );
        let removed = self.resources.lock().await.remove(&key);
        if removed.is_none() {
            return Err((
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    detail: format!(
                        "No resource bindings found for key '{}' in skill '{}'",
                        resource_key, skill_name
                    ),
                }),
            ));
        }
        Ok(Json(UnbindResourceResponse {
            status: "deleted".to_string(),
            count: 1,
        }))
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn build_test_app() -> axum::Router {
    let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
        .with_skill_config_service(Arc::new(InMemorySkillConfigService::new()));
    build_app(state)
}

async fn response_json(resp: axum::http::Response<body::Body>) -> serde_json::Value {
    let bytes = body::to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn validate_config_returns_ok() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/skills/my-skill/config/validate")
                .header("X-User-Id", "user-1")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = response_json(resp).await;
    assert_eq!(json["valid"], true);
    assert_eq!(json["errors"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn get_effective_config_returns_ok() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/skills/my-skill/config")
                .header("X-User-Id", "user-1")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = response_json(resp).await;
    assert!(json["settings"].is_object());
    assert!(json["secrets"].is_object());
    assert_eq!(json["resources_configured"], 0);
}

#[tokio::test]
async fn set_setting_returns_ok() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/skills/my-skill/config/api_key")
                .header("X-User-Id", "user-1")
                .header("content-type", "application/json")
                .body(body::Body::from(r#"{"value":"test"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = response_json(resp).await;
    assert_eq!(json["status"], "ok");
}

#[tokio::test]
async fn delete_setting_returns_ok() {
    let svc = Arc::new(InMemorySkillConfigService::new());
    // Pre-populate a setting so delete succeeds
    svc.settings.lock().await.insert(
        (
            "user-1".to_string(),
            "my-skill".to_string(),
            "api_key".to_string(),
            "user".to_string(),
        ),
        StoredSetting {
            value: serde_json::json!("test"),
        },
    );

    let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
        .with_skill_config_service(svc);
    let app = build_app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/skills/my-skill/config/api_key")
                .header("X-User-Id", "user-1")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = response_json(resp).await;
    assert_eq!(json["status"], "deleted");
}

#[tokio::test]
async fn delete_setting_not_found() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/skills/my-skill/config/nonexistent")
                .header("X-User-Id", "user-1")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn list_resources_returns_ok() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/skills/my-skill/resources")
                .header("X-User-Id", "user-1")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = response_json(resp).await;
    assert!(json.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn bind_resource_returns_ok() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/skills/my-skill/resources/my-db")
                .header("X-User-Id", "user-1")
                .header("content-type", "application/json")
                .body(body::Body::from(
                    r#"{"bindings":{"url":"https://example.com"}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = response_json(resp).await;
    assert_eq!(json["status"], "ok");
    assert_eq!(json["resource_key"], "my-db");
}

#[tokio::test]
async fn unbind_resource_returns_ok() {
    let svc = Arc::new(InMemorySkillConfigService::new());
    // Pre-populate a resource so unbind succeeds
    svc.resources.lock().await.insert(
        (
            "user-1".to_string(),
            "my-skill".to_string(),
            "my-db".to_string(),
        ),
        StoredResource {
            resource_type: "generic".to_string(),
        },
    );

    let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
        .with_skill_config_service(svc);
    let app = build_app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/skills/my-skill/resources/my-db")
                .header("X-User-Id", "user-1")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = response_json(resp).await;
    assert_eq!(json["status"], "deleted");
    assert_eq!(json["count"], 1);
}

#[tokio::test]
async fn unbind_resource_not_found() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/skills/my-skill/resources/nonexistent")
                .header("X-User-Id", "user-1")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn set_setting_global_scope_without_admin_returns_403() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/skills/my-skill/config/api_key?scope=global")
                .header("X-User-Id", "user-1")
                .header("content-type", "application/json")
                .body(body::Body::from(r#"{"value":"test"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn set_setting_global_scope_with_admin_returns_ok() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/skills/my-skill/config/api_key?scope=global")
                .header("X-User-Id", "user-1")
                .header("X-User-Role", "astra_admin")
                .header("content-type", "application/json")
                .body(body::Body::from(r#"{"value":"test"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = response_json(resp).await;
    assert_eq!(json["status"], "ok");
}

#[tokio::test]
async fn delete_setting_global_scope_without_admin_returns_403() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/skills/my-skill/config/api_key?scope=global")
                .header("X-User-Id", "user-1")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn missing_user_id_returns_401() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/skills/my-skill/config/validate")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
