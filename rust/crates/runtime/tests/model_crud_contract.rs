use std::sync::{Arc, Mutex};

use astra_runtime::{
    AdminAuthorizer, AppState, AuthLoginRequestData, AuthRefreshRequestData,
    AuthRegisterRequestData, AuthService, AuthTokenRecord, AuthUserRecord, AuthenticatedUser,
    ErrorResponse, HealthChecker, ModelCreateRequestData, ModelRecord, ModelService,
    ModelUpdateRequestData, PricingData, QuirksData, ServiceInfo, build_app,
};
use async_trait::async_trait;
use axum::{
    body,
    http::{HeaderMap, Request, StatusCode},
};
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
        match headers.get("authorization").and_then(|v| v.to_str().ok()) {
            Some("Bearer contract-model-token") => Ok(AuthUserRecord {
                user_id: "contract-model-user-id".to_string(),
                username: "contract-model-user".to_string(),
                email: "model-user@test.com".to_string(),
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

#[derive(Clone)]
struct StubAdminAuthorizer;

#[async_trait]
impl AdminAuthorizer for StubAdminAuthorizer {
    async fn require_admin(
        &self,
        headers: &HeaderMap,
    ) -> Result<AuthenticatedUser, (StatusCode, axum::Json<ErrorResponse>)> {
        match headers.get("authorization").and_then(|v| v.to_str().ok()) {
            Some("Bearer contract-model-token") => Ok(AuthenticatedUser {
                user_id: "contract-model-user-id".to_string(),
                username: Some("contract-model-user".to_string()),
            }),
            _ => Err((
                StatusCode::FORBIDDEN,
                axum::Json(ErrorResponse {
                    detail: "Admin role required".to_string(),
                }),
            )),
        }
    }
}

#[derive(Clone)]
struct StubModelService {
    state: Arc<Mutex<Vec<ModelRecord>>>,
}

fn make_model(name: &str) -> ModelRecord {
    ModelRecord {
        model_id: format!("model-id-{}", name),
        name: name.to_string(),
        provider: "openai".to_string(),
        base_url: None,
        description: Some(format!("Test model {}", name)),
        is_active: true,
        context_window: 128000,
        max_completion_tokens: Some(4096),
        input_modalities: vec!["text".to_string()],
        output_modalities: vec!["text".to_string()],
        supported_parameters: vec!["temperature".to_string()],
        pricing: PricingData::default(),
        architecture: Some("transformer".to_string()),
        tags: vec!["test".to_string()],
        quirks: QuirksData::default(),
        connectivity: None,
    }
}

impl StubModelService {
    fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(vec![
                make_model("gpt-4o"),
                make_model("claude-sonnet"),
            ])),
        }
    }
}

#[async_trait]
impl ModelService for StubModelService {
    async fn create_model(
        &self,
        _user_id: String,
        request: ModelCreateRequestData,
    ) -> Result<ModelRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        let record = ModelRecord {
            model_id: format!("model-id-{}", request.name),
            name: request.name,
            provider: request.provider,
            base_url: request.base_url,
            description: request.description,
            is_active: true,
            context_window: request.context_window.unwrap_or(128000),
            max_completion_tokens: request.max_completion_tokens,
            input_modalities: request.input_modalities,
            output_modalities: request.output_modalities,
            supported_parameters: request.supported_parameters,
            pricing: request.pricing,
            architecture: request.architecture,
            tags: request.tags,
            quirks: request.quirks.unwrap_or_default(),
            connectivity: None,
        };
        self.state.lock().unwrap().push(record.clone());
        Ok(record)
    }

    async fn list_models(
        &self,
        _user_id: String,
        _is_admin: bool,
    ) -> Result<Vec<ModelRecord>, (StatusCode, axum::Json<ErrorResponse>)> {
        Ok(self.state.lock().unwrap().clone())
    }

    async fn get_model(
        &self,
        model_name: String,
    ) -> Result<ModelRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        self.state
            .lock()
            .unwrap()
            .iter()
            .find(|m| m.name == model_name)
            .cloned()
            .ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    axum::Json(ErrorResponse {
                        detail: format!("Model {} not found", model_name),
                    }),
                )
            })
    }

    async fn update_model(
        &self,
        model_name: String,
        request: ModelUpdateRequestData,
    ) -> Result<ModelRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        let mut models = self.state.lock().unwrap();
        let model = models
            .iter_mut()
            .find(|m| m.name == model_name)
            .ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    axum::Json(ErrorResponse {
                        detail: format!("Model {} not found", model_name),
                    }),
                )
            })?;

        if let Some(desc) = request.description {
            model.description = Some(desc);
        }
        if let Some(active) = request.is_active {
            model.is_active = active;
        }
        if let Some(cw) = request.context_window {
            model.context_window = cw;
        }
        if let Some(mct) = request.max_completion_tokens {
            model.max_completion_tokens = Some(mct);
        }
        if let Some(im) = request.input_modalities {
            model.input_modalities = im;
        }
        if let Some(om) = request.output_modalities {
            model.output_modalities = om;
        }
        if let Some(sp) = request.supported_parameters {
            model.supported_parameters = sp;
        }
        if let Some(p) = request.pricing {
            model.pricing = p;
        }
        if let Some(a) = request.architecture {
            model.architecture = Some(a);
        }
        if let Some(t) = request.tags {
            model.tags = t;
        }
        if let Some(q) = request.quirks {
            model.quirks = q;
        }
        Ok(model.clone())
    }

    async fn delete_model(
        &self,
        model_name: String,
    ) -> Result<(), (StatusCode, axum::Json<ErrorResponse>)> {
        let mut models = self.state.lock().unwrap();
        let idx = models
            .iter()
            .position(|m| m.name == model_name)
            .ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    axum::Json(ErrorResponse {
                        detail: format!("Model {} not found", model_name),
                    }),
                )
            })?;
        models.remove(idx);
        Ok(())
    }

    async fn check_model(
        &self,
        model_name: String,
    ) -> Result<ModelRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        let models = self.state.lock().unwrap();
        let mut model = models
            .iter()
            .find(|m| m.name == model_name)
            .cloned()
            .ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    axum::Json(ErrorResponse {
                        detail: format!("Model {} not found", model_name),
                    }),
                )
            })?;
        model.connectivity = Some("ok".to_string());
        Ok(model)
    }
}

fn build_test_app() -> axum::Router {
    let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
        .with_auth_service(Arc::new(StubAuthService))
        .with_admin_authorizer(Arc::new(StubAdminAuthorizer))
        .with_model_service(Arc::new(StubModelService::new()));
    build_app(state)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn create_model_returns_201() {
    let app = build_test_app();
    let payload = serde_json::json!({
        "name": "gpt-5",
        "provider": "openai",
        "api_key": "sk-test",
        "input_modalities": ["text"],
        "output_modalities": ["text"],
        "supported_parameters": ["temperature"],
        "pricing": { "prompt": 0.01, "completion": 0.03 },
        "tags": ["new"]
    });
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/models")
                .header("authorization", "Bearer contract-model-token")
                .header("content-type", "application/json")
                .body(body::Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["name"], "gpt-5");
    assert_eq!(json["provider"], "openai");
}

#[tokio::test]
async fn list_models_returns_array() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/models")
                .header("authorization", "Bearer contract-model-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let arr = json.as_array().unwrap();
    assert_eq!(arr.len(), 2);
}

#[tokio::test]
async fn get_model_by_name() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/models/gpt-4o")
                .header("authorization", "Bearer contract-model-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["name"], "gpt-4o");
}

#[tokio::test]
async fn get_model_not_found() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/models/nonexistent")
                .header("authorization", "Bearer contract-model-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn update_model_returns_ok() {
    let app = build_test_app();
    let payload = serde_json::json!({
        "description": "Updated description"
    });
    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/models/gpt-4o")
                .header("authorization", "Bearer contract-model-token")
                .header("content-type", "application/json")
                .body(body::Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["description"], "Updated description");
}

#[tokio::test]
async fn delete_model_returns_204() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/models/gpt-4o")
                .header("authorization", "Bearer contract-model-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn delete_model_not_found() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/models/nonexistent")
                .header("authorization", "Bearer contract-model-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn check_model_connectivity() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/models/gpt-4o/check")
                .header("authorization", "Bearer contract-model-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["name"], "gpt-4o");
    assert_eq!(json["connectivity"], "ok");
}
