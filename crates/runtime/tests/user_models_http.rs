use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use astra_core::{ErrorResponse, error_response};
use astra_runtime::{AppState, HealthChecker, ServiceInfo, build_app};
use astra_services::{
    ModelCreateRequestData, ModelListItem, ModelRecord, ModelService, ModelUpdateRequestData,
    ResolvedModelOffering, UserModelCreateRequestData, UserModelRecord, UserModelUpdateRequestData,
    auth::{
        AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
        AuthTokenRecord, AuthUserRecord,
    },
};
use async_trait::async_trait;
use axum::{
    Json,
    body::{self, Body},
    http::{HeaderMap, Request, StatusCode},
};
use tower::ServiceExt;

type HttpError = (StatusCode, Json<ErrorResponse>);

#[derive(Clone, Default)]
struct TestModelService {
    rows: Arc<Mutex<HashMap<(String, String), UserModelRecord>>>,
    preflights: Arc<Mutex<Vec<(String, String)>>>,
    preflight_network_failure: bool,
    allows_deployment: bool,
    catalog: Vec<ModelListItem>,
}

fn unsupported<T>() -> Result<T, HttpError> {
    Err(error_response(
        StatusCode::NOT_IMPLEMENTED,
        "unsupported in test",
    ))
}

#[async_trait]
impl ModelService for TestModelService {
    async fn allows_deployment_models(&self, _: String) -> Result<bool, HttpError> {
        Ok(self.allows_deployment)
    }

    async fn validate_user_model_endpoint(
        &self,
        user_id: String,
        base_url: String,
    ) -> Result<(), HttpError> {
        astra_services::byok_endpoint::parse_endpoint(&base_url)
            .map_err(|error| error_response(StatusCode::BAD_REQUEST, error))?;
        if self.preflight_network_failure {
            return Err(astra_core::error_response_coded(
                StatusCode::BAD_GATEWAY,
                "Astra Server could not resolve the model endpoint",
                "model_endpoint_network",
            ));
        }
        self.preflights.lock().unwrap().push((user_id, base_url));
        Ok(())
    }

    async fn create_user_model(
        &self,
        user_id: String,
        request: UserModelCreateRequestData,
    ) -> Result<UserModelRecord, HttpError> {
        let mut rows = self.rows.lock().expect("model rows lock");
        if rows
            .iter()
            .any(|((owner, _), row)| owner == &user_id && row.name == request.name)
        {
            return Err(error_response(StatusCode::CONFLICT, "duplicate user model"));
        }
        let model_id = format!("model-{}-{}", user_id, rows.len() + 1);
        let record = UserModelRecord {
            thinking_probe: None,
            model_id: model_id.clone(),
            name: request.name,
            provider: request.provider,
            model: request.model,
            base_url: request
                .base_url
                .unwrap_or_else(|| "https://provider.example/v1".into()),
            context_window: request.context_window,
            is_default: request.is_default,
            is_active: true,
            credential_configured: !request.api_key.is_empty(),
            created_at: "2026-09-06 00:00:00.000000".into(),
            updated_at: "2026-09-06 00:00:00.000000".into(),
        };
        rows.insert((user_id, model_id), record.clone());
        Ok(record)
    }

    async fn list_user_models(&self, user_id: String) -> Result<Vec<UserModelRecord>, HttpError> {
        let rows = self.rows.lock().expect("model rows lock");
        Ok(rows
            .iter()
            .filter(|((owner, _), _)| owner == &user_id)
            .map(|(_, row)| row.clone())
            .collect())
    }

    async fn get_user_model(
        &self,
        user_id: String,
        model_id: String,
    ) -> Result<UserModelRecord, HttpError> {
        self.rows
            .lock()
            .expect("model rows lock")
            .get(&(user_id, model_id))
            .cloned()
            .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "User model not found"))
    }

    async fn update_user_model(
        &self,
        user_id: String,
        model_id: String,
        request: UserModelUpdateRequestData,
    ) -> Result<UserModelRecord, HttpError> {
        let mut rows = self.rows.lock().expect("model rows lock");
        let row = rows
            .get_mut(&(user_id, model_id))
            .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "User model not found"))?;
        if let Some(context_window) = request.context_window {
            row.context_window = context_window;
        }
        if let Some(is_default) = request.is_default {
            row.is_default = is_default;
        }
        if let Some(is_active) = request.is_active {
            row.is_active = is_active;
        }
        if let Some(api_key) = request.api_key {
            row.credential_configured = !api_key.is_empty();
        }
        Ok(row.clone())
    }

    async fn delete_user_model(&self, user_id: String, model_id: String) -> Result<(), HttpError> {
        self.rows
            .lock()
            .expect("model rows lock")
            .remove(&(user_id, model_id))
            .map(|_| ())
            .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "User model not found"))
    }

    async fn check_user_model(
        &self,
        user_id: String,
        model_id: String,
    ) -> Result<UserModelRecord, HttpError> {
        self.get_user_model(user_id, model_id).await
    }

    async fn create_model(
        &self,
        _: String,
        _: ModelCreateRequestData,
    ) -> Result<ModelRecord, HttpError> {
        unsupported()
    }
    async fn list_models(&self, _: String, _: bool) -> Result<Vec<ModelListItem>, HttpError> {
        Ok(self.catalog.clone())
    }
    async fn get_model(&self, _: String) -> Result<ModelRecord, HttpError> {
        unsupported()
    }
    async fn resolve_model_offering(&self, _: String) -> Result<ResolvedModelOffering, HttpError> {
        unsupported()
    }
    async fn update_model(
        &self,
        _: String,
        _: ModelUpdateRequestData,
    ) -> Result<ModelRecord, HttpError> {
        unsupported()
    }
    async fn delete_model(&self, _: String) -> Result<(), HttpError> {
        unsupported()
    }
    async fn check_model(&self, _: String) -> Result<ModelRecord, HttpError> {
        unsupported()
    }
}

#[tokio::test]
async fn model_access_matches_run_eligibility_across_catalog_pages() {
    use astra_services::{ModelAccessKind, ModelExecutionPlacement};
    let offering = |name: &str, kind: ModelAccessKind| ModelListItem {
        offering_id: name.into(),
        access_id: if kind == ModelAccessKind::CloudByok {
            "cloud-byok"
        } else {
            "self-hosted"
        }
        .into(),
        access_kind: kind,
        access_label: if kind == ModelAccessKind::CloudByok {
            "Cloud BYOK"
        } else {
            "Self-hosted"
        }
        .into(),
        execution_placement: ModelExecutionPlacement::Server,
        name: name.into(),
        provider: "mock".into(),
        description: None,
        is_active: true,
        context_window: 128000,
        max_completion_tokens: None,
        architecture: None,
        thinking_capability: None,
    };
    for (allows_deployment, catalog, expected) in [
        (
            true,
            vec![offering("a-deployment", ModelAccessKind::SelfHosted)],
            vec!["self-hosted"],
        ),
        (false, vec![], vec!["cloud-byok"]),
        (
            true,
            vec![
                offering("a-deployment", ModelAccessKind::SelfHosted),
                offering("z-personal", ModelAccessKind::CloudByok),
            ],
            vec!["cloud-byok", "self-hosted"],
        ),
    ] {
        let service = TestModelService {
            allows_deployment,
            catalog,
            ..Default::default()
        };
        let (status, body) = request(
            app(service),
            "GET",
            "/model-access?limit=1",
            Some("ordinary-user"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        let mut ids = body["accesses"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["id"].as_str().unwrap())
            .collect::<Vec<_>>();
        ids.sort_unstable();
        assert_eq!(ids, expected);
    }
}

struct HeaderAuthService;

#[async_trait]
impl AuthService for HeaderAuthService {
    async fn register(&self, _: AuthRegisterRequestData) -> Result<AuthUserRecord, HttpError> {
        unsupported()
    }
    async fn login(&self, _: AuthLoginRequestData) -> Result<AuthTokenRecord, HttpError> {
        unsupported()
    }
    async fn refresh(&self, _: AuthRefreshRequestData) -> Result<AuthTokenRecord, HttpError> {
        unsupported()
    }
    async fn logout(&self, _: AuthRefreshRequestData) -> Result<(), HttpError> {
        unsupported()
    }

    async fn current_user(&self, headers: &HeaderMap) -> Result<AuthUserRecord, HttpError> {
        let token = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .filter(|value| !value.is_empty())
            .ok_or_else(|| error_response(StatusCode::UNAUTHORIZED, "Missing bearer token"))?;
        Ok(AuthUserRecord {
            user_id: token.to_string(),
            username: token.to_string(),
            email: format!("{token}@example.com"),
            display_name: None,
        })
    }
}

struct Healthy;

#[async_trait]
impl HealthChecker for Healthy {
    async fn database_healthy(&self) -> bool {
        true
    }
}

fn app(service: TestModelService) -> axum::Router {
    build_app(
        AppState::new(ServiceInfo::default(), Arc::new(Healthy))
            .with_auth_service(Arc::new(HeaderAuthService))
            .with_model_service(Arc::new(service)),
    )
}

async fn request(
    app: axum::Router,
    method: &str,
    path: &str,
    user: Option<&str>,
    json: Option<serde_json::Value>,
) -> (StatusCode, String) {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(user) = user {
        builder = builder.header("authorization", format!("Bearer {user}"));
    }
    let body = if let Some(json) = json {
        builder = builder.header("content-type", "application/json");
        Body::from(json.to_string())
    } else {
        Body::empty()
    };
    let response = app
        .oneshot(builder.body(body).expect("request"))
        .await
        .expect("response");
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    (
        status,
        String::from_utf8(bytes.to_vec()).expect("utf8 body"),
    )
}

#[tokio::test]
async fn user_model_crud_is_authenticated_owner_scoped_and_secret_negative() {
    let service = TestModelService::default();
    let (status, _) = request(app(service.clone()), "GET", "/me/models", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let secret = "sk-private-user-a";
    let create = serde_json::json!({
        "name": "deepseek",
        "provider": "deepseek",
        "model": "deepseek-chat",
        "api_key": secret,
        "is_default": true
    });
    let (status, body) = request(
        app(service.clone()),
        "POST",
        "/me/models",
        Some("user-a"),
        Some(create.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert!(!body.contains(secret));
    let created: serde_json::Value = serde_json::from_str(&body).expect("create json");
    let model_id = created["model_id"].as_str().expect("model id");
    assert_eq!(created["credential_configured"], true);

    let (status, body) = request(
        app(service.clone()),
        "GET",
        "/me/models",
        Some("user-a"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains(secret));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let path = format!("/me/models/{model_id}");
    let (status, _) = request(app(service.clone()), "GET", &path, Some("user-b"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = request(
        app(service.clone()),
        "PUT",
        &path,
        Some("user-b"),
        Some(serde_json::json!({"is_active": false})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = request(app(service.clone()), "DELETE", &path, Some("user-b"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, body) = request(
        app(service.clone()),
        "POST",
        "/me/models",
        Some("user-b"),
        Some(create),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "same alias must be allowed across users: {body}"
    );

    let check_path = format!("{path}/check");
    let (status, body) = request(
        app(service.clone()),
        "POST",
        &check_path,
        Some("user-a"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body.contains(secret));

    let (status, body) = request(app(service), "DELETE", &path, Some("user-a"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
}

#[tokio::test]
async fn user_model_payload_rejects_unknown_fields_without_echoing_secret() {
    let secret = "sk-unknown-field-secret";
    let (status, body) = request(
        app(TestModelService::default()),
        "POST",
        "/me/models",
        Some("user-a"),
        Some(serde_json::json!({
            "name": "deepseek",
            "provider": "deepseek",
            "model": "deepseek-chat",
            "api_key": secret,
            "owner": "user-b"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(!body.contains(secret));
}

#[tokio::test]
async fn compatible_model_configuration_reaches_owner_scoped_service() {
    let (status, body) = request(
        app(TestModelService::default()),
        "POST",
        "/me/models",
        Some("user-a"),
        Some(serde_json::json!({
            "name": "gateway", "provider": "openai-compatible", "model": "custom-model",
            "base_url": "https://gateway.example/v1", "api_key": "sk-test-secret"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let result: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(result["provider"], "openai-compatible");
    assert_eq!(result["base_url"], "https://gateway.example/v1");
    assert!(!body.contains("sk-test-secret"));
}

#[tokio::test]
async fn endpoint_preflight_is_authenticated_credential_free_and_does_not_create_a_model() {
    let service = TestModelService::default();
    let path = "/me/models/validate-endpoint";
    let endpoint = "https://api.moonshot.cn/v1";
    let payload = serde_json::json!({ "base_url": endpoint });
    let (status, _) = request(
        app(service.clone()),
        "POST",
        path,
        None,
        Some(payload.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(service.preflights.lock().unwrap().is_empty());
    let (status, body) = request(
        app(service.clone()),
        "POST",
        path,
        Some("user-a"),
        Some(payload),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(
        *service.preflights.lock().unwrap(),
        vec![("user-a".into(), endpoint.into())]
    );
    assert!(service.rows.lock().unwrap().is_empty());
    let (status, _) = request(
        app(service.clone()),
        "POST",
        path,
        Some("user-a"),
        Some(serde_json::json!({ "base_url": "https://127.0.0.1/v1" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    for extra in ["api_key", "user_id"] {
        let mut payload = serde_json::json!({ "base_url": endpoint });
        payload[extra] = "do-not-accept-or-echo".into();
        let (status, body) = request(
            app(service.clone()),
            "POST",
            path,
            Some("user-a"),
            Some(payload),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(!body.contains("do-not-accept-or-echo"));
    }
    assert_eq!(service.preflights.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn endpoint_preflight_preserves_server_network_error_code() {
    let service = TestModelService {
        preflight_network_failure: true,
        ..Default::default()
    };
    let (status, body) = request(
        app(service.clone()),
        "POST",
        "/me/models/validate-endpoint",
        Some("user-a"),
        Some(serde_json::json!({ "base_url": "https://provider.example/v1" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    let response: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(response["error_code"], "model_endpoint_network");
    assert!(service.rows.lock().unwrap().is_empty());
}

#[tokio::test]
async fn cloud_byok_completion_without_personal_default_does_not_fall_back() {
    let service = TestModelService::default();
    let payload = serde_json::json!({
        "operation": "memory_extraction",
        "session_id": "test-session",
        "turn": 1,
        "round": 1,
        "logical_attempt": 1,
        "messages": [{"role": "user", "content": "test"}]
    });
    let (status, _) = request(
        app(service.clone()),
        "POST",
        "/v1/chat/completions",
        None,
        Some(payload.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // No database or provider is configured. Reaching either fallback would
    // produce a different error instead of the required selection response.
    let (status, body) = request(
        app(service),
        "POST",
        "/v1/chat/completions",
        Some("user-a"),
        Some(payload),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let response: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(response["error_code"], "missing_model_selection");
}
