use std::sync::Arc;

use astra_runtime::{
    AdminAuthorizer, AppState, AuthenticatedUser, ErrorResponse, HealthChecker, ServiceInfo,
    build_app,
};
use astra_services::llm_trusted_domains::{
    LlmTrustedDomainDeleteResponse, LlmTrustedDomainRecord, LlmTrustedDomainService,
    LlmTrustedDomainUpsertRequestData,
};
use async_trait::async_trait;
use axum::{
    Json, Router, body,
    http::{HeaderMap, Request, StatusCode},
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
struct StubAdminAuthorizer;

#[async_trait]
impl AdminAuthorizer for StubAdminAuthorizer {
    async fn require_admin(
        &self,
        headers: &HeaderMap,
    ) -> Result<AuthenticatedUser, (StatusCode, Json<ErrorResponse>)> {
        match headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
        {
            Some("Bearer admin-token") => Ok(AuthenticatedUser {
                user_id: "admin-1".to_string(),
                username: Some("admin".to_string()),
            }),
            Some("Bearer user-token") => Err((
                StatusCode::FORBIDDEN,
                Json(ErrorResponse::new("Admin role required".to_string())),
            )),
            _ => Err((
                StatusCode::UNAUTHORIZED,
                Json(ErrorResponse::new("Not authenticated".to_string())),
            )),
        }
    }
}

#[derive(Clone)]
struct StubLlmTrustedDomainService {
    records: Arc<Mutex<Vec<LlmTrustedDomainRecord>>>,
    last_updated_by: Arc<Mutex<Option<String>>>,
}

impl StubLlmTrustedDomainService {
    fn new() -> Self {
        Self {
            records: Arc::new(Mutex::new(vec![LlmTrustedDomainRecord {
                domain_id: "domain-1".to_string(),
                domain_host: "trusted.example.com".to_string(),
                domain_port: None,
                is_enabled: true,
                description: Some("seed".to_string()),
                created_at: "2026-02-01T00:00:00.000000Z".to_string(),
                updated_at: "2026-02-01T00:00:00.000000Z".to_string(),
            }])),
            last_updated_by: Arc::new(Mutex::new(None)),
        }
    }
}

#[async_trait]
impl LlmTrustedDomainService for StubLlmTrustedDomainService {
    async fn list_trusted_domains(
        &self,
    ) -> Result<Vec<LlmTrustedDomainRecord>, (StatusCode, Json<ErrorResponse>)> {
        Ok(self.records.lock().await.clone())
    }

    async fn upsert_trusted_domain(
        &self,
        updated_by: Option<&str>,
        request: LlmTrustedDomainUpsertRequestData,
    ) -> Result<LlmTrustedDomainRecord, (StatusCode, Json<ErrorResponse>)> {
        let domain_host = request.domain_host.trim().to_ascii_lowercase();
        if domain_host.is_empty() {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(
                    "domain_host must not be empty".to_string(),
                )),
            ));
        }

        let mut last_updated_by = self.last_updated_by.lock().await;
        *last_updated_by = updated_by
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(String::from);
        drop(last_updated_by);

        let mut records = self.records.lock().await;
        if let Some(existing) = records.iter_mut().find(|record| {
            record.domain_host == domain_host && record.domain_port == request.domain_port
        }) {
            existing.is_enabled = request.is_enabled;
            existing.description = request.description;
            existing.updated_at = "2026-02-02T00:00:00.000000Z".to_string();
            return Ok(existing.clone());
        }

        let domain_id = format!("domain-{}", records.len() + 1);
        let created = LlmTrustedDomainRecord {
            domain_id,
            domain_host,
            domain_port: request.domain_port,
            is_enabled: request.is_enabled,
            description: request.description,
            created_at: "2026-02-02T00:00:00.000000Z".to_string(),
            updated_at: "2026-02-02T00:00:00.000000Z".to_string(),
        };
        records.push(created.clone());
        Ok(created)
    }

    async fn delete_trusted_domain(
        &self,
        domain_id: &str,
    ) -> Result<LlmTrustedDomainDeleteResponse, (StatusCode, Json<ErrorResponse>)> {
        let mut records = self.records.lock().await;
        if let Some(index) = records
            .iter()
            .position(|record| record.domain_id == domain_id)
        {
            records.remove(index);
            return Ok(LlmTrustedDomainDeleteResponse {
                status: "deleted".to_string(),
            });
        }
        Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new(format!(
                "trusted domain '{domain_id}' not found"
            ))),
        ))
    }
}

fn build_app_with_admin(service: Arc<StubLlmTrustedDomainService>) -> Router {
    build_app(
        AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
            .with_admin_authorizer(Arc::new(StubAdminAuthorizer))
            .with_llm_trusted_domain_service(service),
    )
}

fn build_request(
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    payload: Option<serde_json::Value>,
) -> Request<body::Body> {
    let mut builder = Request::builder().method(method).uri(path);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    if let Some(payload) = payload {
        builder = builder.header("content-type", "application/json");
        builder.body(body::Body::from(payload.to_string())).unwrap()
    } else {
        builder.body(body::Body::empty()).unwrap()
    }
}

async fn send_json(
    app: Router,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    payload: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let response = app
        .oneshot(build_request(method, path, headers, payload))
        .await
        .unwrap();
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap();
    (status, json)
}

#[tokio::test]
async fn llm_trusted_domains_require_admin_role() {
    let service = Arc::new(StubLlmTrustedDomainService::new());
    let app = build_app_with_admin(service);

    let (status, json) =
        send_json(app.clone(), "GET", "/admin/llm/trusted-domains", &[], None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(json["detail"], "Not authenticated");

    let (status, json) = send_json(
        app,
        "PUT",
        "/admin/llm/trusted-domains",
        &[("authorization", "Bearer user-token")],
        Some(serde_json::json!({"domain_host":"catalog.local"})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(json["detail"], "Admin role required");
}

#[tokio::test]
async fn llm_trusted_domains_admin_crud_flow() {
    let service = Arc::new(StubLlmTrustedDomainService::new());
    let app = build_app_with_admin(service.clone());
    let admin_headers = &[
        ("authorization", "Bearer admin-token"),
        ("x-user-id", "spoofed-user-id"),
    ];

    let (status, json) = send_json(
        app.clone(),
        "GET",
        "/admin/llm/trusted-domains",
        &[("authorization", "Bearer admin-token")],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json.as_array().map(Vec::len), Some(1));

    let (status, json) = send_json(
        app.clone(),
        "PUT",
        "/admin/llm/trusted-domains",
        admin_headers,
        Some(
            serde_json::json!({"domain_host":"CATALOG.local","domain_port":8081,"description":"catalog","is_enabled":true}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["domain_host"], "catalog.local");
    assert_eq!(json["domain_port"], 8081);
    let created_domain_id = json["domain_id"]
        .as_str()
        .expect("domain_id should be present")
        .to_string();

    let last_updated_by = service.last_updated_by.lock().await.clone();
    assert_eq!(last_updated_by.as_deref(), Some("admin-1"));

    let delete_path = format!("/admin/llm/trusted-domains/{created_domain_id}");
    let (status, json) = send_json(
        app.clone(),
        "DELETE",
        &delete_path,
        &[("authorization", "Bearer admin-token")],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["status"], "deleted");

    let (status, json) = send_json(
        app,
        "GET",
        "/admin/llm/trusted-domains",
        &[("authorization", "Bearer admin-token")],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json.as_array().map(Vec::len), Some(1));
    assert_eq!(json[0]["domain_id"], "domain-1");
}
