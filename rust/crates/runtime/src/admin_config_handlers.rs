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
    let _admin = state.admin_authorizer.require_admin(&headers).await?;
    let rows = state
        .admin_config_service
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
    let _admin = state.admin_authorizer.require_admin(&headers).await?;
    let value = state
        .admin_config_service
        .get(&key)
        .await
        .map_err(internal_error)?;
    match value {
        Some(v) => Ok(Json(AdminConfigGetResponse {
            key,
            value: Some(v),
        })),
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
    let admin = state.admin_authorizer.require_admin(&headers).await?;
    state
        .admin_config_service
        .set(&key, &request.value, Some(&admin.user_id))
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
    let _admin = state.admin_authorizer.require_admin(&headers).await?;
    let deleted = state
        .admin_config_service
        .unset(&key)
        .await
        .map_err(internal_error)?;
    Ok(Json(AdminConfigDeleteResponse { deleted }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::{ADMIN_CONFIG_KEY_REASONING_MODEL, AdminConfigService};
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

    // GET /admin/config/{key} for a missing key must return 404, not 200+null.
    #[tokio::test]
    async fn get_missing_key_returns_404() {
        let app = app_with_service(StubAdminConfigService::empty());
        let req = Request::builder()
            .uri(format!(
                "/admin/config/{}",
                ADMIN_CONFIG_KEY_REASONING_MODEL
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
            ADMIN_CONFIG_KEY_REASONING_MODEL,
            "gpt-4o-mini",
        ));
        let req = Request::builder()
            .uri(format!(
                "/admin/config/{}",
                ADMIN_CONFIG_KEY_REASONING_MODEL
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
        assert_eq!(parsed["value"], "gpt-4o-mini");
    }
}
