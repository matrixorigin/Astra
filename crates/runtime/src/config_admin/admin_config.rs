//! HTTP handlers for server-wide admin configuration (`/admin/config`).
//!
//! All routes require `astra_admin` role (via [`AdminAuthorizer::require_admin`]).

use crate::AppState;
use astra_core::{ErrorResponse, error_response, internal_error};
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use serde::{Deserialize, Serialize};

const JUDGMENT_MODEL: &str = "judgment_model";

fn storage_key(key: &str) -> &str {
    if key == JUDGMENT_MODEL {
        astra_services::ADMIN_CONFIG_KEY_JUDGMENT_OFFERING
    } else {
        key
    }
}

fn judgment_model_id<'a>(
    catalog: &'a [astra_services::models::ModelListItem],
    name: &str,
) -> Result<&'a str, String> {
    let matches = catalog
        .iter()
        .filter(|m| {
            m.name == name && m.access_kind == astra_services::models::ModelAccessKind::SelfHosted
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => Err(format!(
            "Judgment model '{name}' not found. Load it with astra admin model load first."
        )),
        [model] if !model.is_active => Err(format!(
            "Judgment model '{name}' is inactive. Run: astra admin model check {name}"
        )),
        [model] => Ok(&model.offering_id),
        models => Err(format!(
            "Judgment model '{name}' is ambiguous: {}. Select an exact judgment_offering_id instead.",
            models
                .iter()
                .map(|m| format!("{} ({})", m.offering_id, m.provider))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

#[derive(Debug, Serialize)]
pub struct AdminConfigEntry {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Serialize)]
pub struct AdminConfigListResponse {
    pub entries: Vec<AdminConfigEntry>,
}

#[derive(Debug, Serialize)]
pub struct AdminConfigGetResponse {
    pub key: String,
    pub value: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AdminConfigSetRequest {
    pub value: String,
}

#[derive(Debug, Serialize)]
pub struct AdminConfigDeleteResponse {
    pub deleted: bool,
}

pub async fn list_admin_config_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<AdminConfigListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let _admin = state.admin.authorizer.require_admin(&headers).await?;
    let rows = state
        .admin
        .config_service
        .list()
        .await
        .map_err(internal_error)?;
    Ok(Json(AdminConfigListResponse {
        entries: rows
            .into_iter()
            .map(|(key, value)| AdminConfigEntry { key, value })
            .collect(),
    }))
}

pub async fn get_admin_config_handler(
    State(state): State<AppState>,
    Path(key): Path<String>,
    headers: HeaderMap,
) -> Result<Json<AdminConfigGetResponse>, (StatusCode, Json<ErrorResponse>)> {
    let admin = state.admin.authorizer.require_admin(&headers).await?;
    let value = state
        .admin
        .config_service
        .get(storage_key(&key))
        .await
        .map_err(|e| error_response(StatusCode::BAD_REQUEST, e))?;
    match value {
        Some(mut v) => {
            if key == JUDGMENT_MODEL {
                let catalog = state.model_service.list_models(admin.user_id, true).await?;
                v = catalog.iter().find(|m| m.offering_id == v && m.access_kind == astra_services::models::ModelAccessKind::SelfHosted)
                    .map(|m| m.name.clone()).ok_or_else(|| error_response(StatusCode::NOT_FOUND, "Configured judgment model no longer exists; select a new judgment_model or unset it"))?;
            }
            Ok(Json(AdminConfigGetResponse {
                key,
                value: Some(v),
            }))
        }
        None => Err(error_response(
            StatusCode::NOT_FOUND,
            format!("admin config key '{key}' is not set"),
        )),
    }
}

pub async fn set_admin_config_handler(
    State(state): State<AppState>,
    Path(key): Path<String>,
    headers: HeaderMap,
    Json(request): Json<AdminConfigSetRequest>,
) -> Result<Json<AdminConfigEntry>, (StatusCode, Json<ErrorResponse>)> {
    let admin = state.admin.authorizer.require_admin(&headers).await?;
    let mut value = request.value.clone();
    if key == JUDGMENT_MODEL {
        let catalog = state
            .model_service
            .list_models(admin.user_id.clone(), true)
            .await?;
        value = judgment_model_id(&catalog, &request.value)
            .map_err(|e| error_response(StatusCode::BAD_REQUEST, e))?
            .to_string();
    }
    let persisted_key = storage_key(&key);
    if persisted_key == astra_services::ADMIN_CONFIG_KEY_REASONING_OFFERING
        || persisted_key == astra_services::ADMIN_CONFIG_KEY_JUDGMENT_OFFERING
    {
        // Fail at configuration time, while the operator still has the
        // relevant context, instead of breaking every later background
        // inference with a stale or misspelled identity.
        let offering = state
            .model_service
            .resolve_model_offering(value.clone())
            .await?;
        if offering.model.provider == "typesafe" {
            if persisted_key == astra_services::ADMIN_CONFIG_KEY_REASONING_OFFERING {
                return Err(error_response(
                    StatusCode::BAD_REQUEST,
                    "TypeSafe supports memory judgments, not reasoning",
                ));
            }
            if offering.model.api_key.trim().is_empty() {
                return Err(error_response(
                    StatusCode::BAD_REQUEST,
                    "TypeSafe API key is not configured",
                ));
            }
        }
    }
    state
        .admin
        .config_service
        .set(persisted_key, &value, Some(&admin.user_id))
        .await
        .map_err(|e| error_response(StatusCode::BAD_REQUEST, e))?;
    Ok(Json(AdminConfigEntry {
        key,
        value: request.value,
    }))
}

pub async fn delete_admin_config_handler(
    State(state): State<AppState>,
    Path(key): Path<String>,
    headers: HeaderMap,
) -> Result<Json<AdminConfigDeleteResponse>, (StatusCode, Json<ErrorResponse>)> {
    let _admin = state.admin.authorizer.require_admin(&headers).await?;
    let deleted = state
        .admin
        .config_service
        .unset(storage_key(&key))
        .await
        .map_err(internal_error)?;
    Ok(Json(AdminConfigDeleteResponse { deleted }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::{ADMIN_CONFIG_KEY_REASONING_OFFERING, AdminConfigService};
    use async_trait::async_trait;
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::get;
    use std::sync::{Arc, Mutex};
    use tower::ServiceExt;

    struct AlwaysHealthy;
    #[async_trait]
    impl crate::app_state::HealthChecker for AlwaysHealthy {
        async fn database_healthy(&self) -> bool {
            true
        }
    }

    struct AllowAllAdmin;
    #[async_trait]
    impl astra_services::AdminAuthorizer for AllowAllAdmin {
        async fn require_admin(
            &self,
            _headers: &HeaderMap,
        ) -> Result<astra_services::AuthenticatedUser, (StatusCode, Json<ErrorResponse>)> {
            Ok(astra_services::AuthenticatedUser {
                user_id: "test-admin".to_string(),
                username: Some("admin".to_string()),
            })
        }
    }

    struct StubAdminConfigService {
        store: Mutex<std::collections::HashMap<String, String>>,
    }

    impl StubAdminConfigService {
        fn empty() -> Arc<Self> {
            Arc::new(Self {
                store: Mutex::new(Default::default()),
            })
        }

        fn with_entry(key: &str, value: &str) -> Arc<Self> {
            let mut m = std::collections::HashMap::new();
            m.insert(key.to_string(), value.to_string());
            Arc::new(Self {
                store: Mutex::new(m),
            })
        }
    }

    #[async_trait]
    impl AdminConfigService for StubAdminConfigService {
        async fn get(&self, key: &str) -> Result<Option<String>, String> {
            if !astra_services::ADMIN_CONFIG_ALLOWED_KEYS.contains(&key) {
                return Err(format!("unknown admin config key '{key}'"));
            }
            Ok(self.store.lock().unwrap().get(key).cloned())
        }

        async fn list(&self) -> Result<Vec<(String, String)>, String> {
            let mut v: Vec<_> = self
                .store
                .lock()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            v.sort_by(|a, b| a.0.cmp(&b.0));
            Ok(v)
        }

        async fn set(
            &self,
            key: &str,
            value: &str,
            _updated_by: Option<&str>,
        ) -> Result<(), String> {
            self.store
                .lock()
                .unwrap()
                .insert(key.to_string(), value.to_string());
            Ok(())
        }

        async fn unset(&self, key: &str) -> Result<bool, String> {
            Ok(self.store.lock().unwrap().remove(key).is_some())
        }
    }

    fn app_with_service(svc: Arc<dyn AdminConfigService>) -> Router {
        let state = crate::AppState::new(
            crate::app_state::ServiceInfo::default(),
            Arc::new(AlwaysHealthy),
        )
        .with_auth_service(Arc::new(astra_services::auth::StubAuthService))
        .with_admin_authorizer(Arc::new(AllowAllAdmin))
        .with_admin_config_service(svc);
        Router::new()
            .route("/admin/config/{key}", get(get_admin_config_handler))
            .with_state(state)
    }

    use astra_services::models::*;

    struct JudgmentModels {
        items: Vec<ModelListItem>,
        key: String,
    }
    #[async_trait]
    impl ModelService for JudgmentModels {
        async fn list_models(
            &self,
            _: String,
            admin: bool,
        ) -> Result<Vec<ModelListItem>, (StatusCode, Json<ErrorResponse>)> {
            assert!(admin);
            Ok(self.items.clone())
        }
        async fn resolve_model_offering(
            &self,
            id: String,
        ) -> Result<ResolvedModelOffering, (StatusCode, Json<ErrorResponse>)> {
            assert_eq!(id, "offer-jev");
            Ok(ResolvedModelOffering {
                offering_id: id,
                model: ResolvedActiveLlmModel {
                    model_name: "jev".into(),
                    wire_model_name: None,
                    api_key: self.key.clone(),
                    base_url: "http://unused.invalid".into(),
                    provider: "typesafe".into(),
                    fallback_chain: vec![],
                    tags: vec![],
                    request_body_overrides: None,
                    fixed_temperature: None,
                    thinking_protocol: None,
                    prompt_cache_capability: None,
                    thinking_capability: None,
                    context_window: Some(1000),
                    max_completion_tokens: None,
                    request_headers: None,
                },
            })
        }
        async fn create_model(
            &self,
            _: String,
            _: ModelCreateRequestData,
        ) -> Result<ModelRecord, (StatusCode, Json<ErrorResponse>)> {
            panic!("unexpected model mutation or provider probe")
        }
        async fn get_model(
            &self,
            _: String,
        ) -> Result<ModelRecord, (StatusCode, Json<ErrorResponse>)> {
            panic!("unexpected model mutation or provider probe")
        }
        async fn update_model(
            &self,
            _: String,
            _: ModelUpdateRequestData,
        ) -> Result<ModelRecord, (StatusCode, Json<ErrorResponse>)> {
            panic!("unexpected model mutation or provider probe")
        }
        async fn delete_model(&self, _: String) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
            panic!("unexpected model mutation or provider probe")
        }
        async fn check_model(
            &self,
            _: String,
        ) -> Result<ModelRecord, (StatusCode, Json<ErrorResponse>)> {
            panic!("unexpected model mutation or provider probe")
        }
    }

    fn jev_item(active: bool) -> ModelListItem {
        ModelListItem {
            offering_id: "offer-jev".into(),
            access_id: "deployment".into(),
            access_kind: ModelAccessKind::SelfHosted,
            access_label: "Server".into(),
            execution_placement: ModelExecutionPlacement::Server,
            name: "jev".into(),
            provider: "typesafe".into(),
            description: None,
            is_active: active,
            context_window: 1000,
            max_completion_tokens: None,
            architecture: None,
            thinking_capability: None,
        }
    }

    fn judgment_app(
        config: Arc<StubAdminConfigService>,
        items: Vec<ModelListItem>,
        key: &str,
    ) -> Router {
        let state = crate::AppState::new(
            crate::app_state::ServiceInfo::default(),
            Arc::new(AlwaysHealthy),
        )
        .with_auth_service(Arc::new(astra_services::auth::StubAuthService))
        .with_admin_authorizer(Arc::new(AllowAllAdmin))
        .with_admin_config_service(config)
        .with_model_service(Arc::new(JudgmentModels {
            items,
            key: key.into(),
        }));
        Router::new()
            .route(
                "/admin/config/{key}",
                get(get_admin_config_handler)
                    .put(set_admin_config_handler)
                    .delete(delete_admin_config_handler),
            )
            .with_state(state)
    }

    fn config_request(method: &str) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri("/admin/config/judgment_model")
            .header("content-type", "application/json")
            .body(if method == "PUT" {
                Body::from(r#"{"value":"jev"}"#)
            } else {
                Body::empty()
            })
            .unwrap()
    }

    #[tokio::test]
    async fn judgment_name_set_get_unset_uses_only_canonical_binding() {
        let config = StubAdminConfigService::empty();
        let app = judgment_app(config.clone(), vec![jev_item(true)], "fake-key");
        assert_eq!(
            app.clone()
                .oneshot(config_request("PUT"))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            config.list().await.unwrap(),
            vec![(
                astra_services::ADMIN_CONFIG_KEY_JUDGMENT_OFFERING.into(),
                "offer-jev".into()
            )]
        );
        let response = app.clone().oneshot(config_request("GET")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()["value"],
            "jev"
        );
        assert_eq!(
            app.oneshot(config_request("DELETE"))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        assert!(config.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn invalid_judgment_selection_preserves_previous_binding() {
        let mut duplicate = jev_item(false);
        duplicate.provider = "other".into();
        duplicate.offering_id = "other-offering".into();
        let mut personal = jev_item(true);
        personal.access_kind = ModelAccessKind::ThisDevice;
        for (items, key) in [
            (vec![], "fake-key"),
            (vec![jev_item(false)], "fake-key"),
            (vec![jev_item(true), duplicate], "fake-key"),
            (vec![personal], "fake-key"),
            (vec![jev_item(true)], "  "),
        ] {
            let config = StubAdminConfigService::with_entry(
                astra_services::ADMIN_CONFIG_KEY_JUDGMENT_OFFERING,
                "previous",
            );
            let app = judgment_app(config.clone(), items, key);
            assert_eq!(
                app.oneshot(config_request("PUT")).await.unwrap().status(),
                StatusCode::BAD_REQUEST
            );
            assert_eq!(
                config
                    .get(astra_services::ADMIN_CONFIG_KEY_JUDGMENT_OFFERING)
                    .await
                    .unwrap()
                    .as_deref(),
                Some("previous")
            );
        }
    }

    // GET /admin/config/{key} for a missing key must return 404, not 200+null.
    #[tokio::test]
    async fn get_missing_key_returns_404() {
        let app = app_with_service(StubAdminConfigService::empty());
        let req = Request::builder()
            .uri(format!(
                "/admin/config/{}",
                ADMIN_CONFIG_KEY_REASONING_OFFERING
            ))
            .header("authorization", "Bearer stub-admin-token")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "missing key must return 404"
        );
    }

    // GET /admin/config/{key} for an existing key must return 200 with the value.
    #[tokio::test]
    async fn get_existing_key_returns_200() {
        let app = app_with_service(StubAdminConfigService::with_entry(
            ADMIN_CONFIG_KEY_REASONING_OFFERING,
            "offer-reasoning",
        ));
        let req = Request::builder()
            .uri(format!(
                "/admin/config/{}",
                ADMIN_CONFIG_KEY_REASONING_OFFERING
            ))
            .header("authorization", "Bearer stub-admin-token")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["value"], "offer-reasoning");
    }

    // GET /admin/config/{key} for an unknown key must return 400, not 500.
    #[tokio::test]
    async fn get_unknown_key_returns_400() {
        let app = app_with_service(StubAdminConfigService::empty());
        let req = Request::builder()
            .uri("/admin/config/not_a_real_key")
            .header("authorization", "Bearer stub-admin-token")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "unknown key must return 400, not 500"
        );
    }
}
