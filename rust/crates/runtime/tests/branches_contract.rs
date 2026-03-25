use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use axum::{
    Json, body,
    http::{HeaderMap, Request, StatusCode},
};
use mo_agent_runtime::branches::{
    BranchService, CostEstimateData, CostEstimateResponse, CreateBranchData, CreateBranchResponse,
    DeleteBranchData, DiffData, DiffResponse, MergeData, MergeResponse, StatusResponse,
};
use mo_agent_runtime::{
    AppState, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
    AuthTokenRecord, AuthUserRecord, ErrorResponse, HealthChecker, ServiceInfo, build_app,
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
struct InMemoryBranchService {
    branches: Arc<Mutex<HashSet<String>>>,
}

impl InMemoryBranchService {
    fn new() -> Self {
        let mut branches = HashSet::new();
        branches.insert("main".to_string());
        Self {
            branches: Arc::new(Mutex::new(branches)),
        }
    }

    async fn branch_exists(&self, name: &str) -> bool {
        self.branches.lock().await.contains(name)
    }
}

#[async_trait]
impl BranchService for InMemoryBranchService {
    async fn create_branch(
        &self,
        _user_id: String,
        request: CreateBranchData,
    ) -> Result<CreateBranchResponse, (StatusCode, Json<ErrorResponse>)> {
        if !self.branch_exists(&request.source).await {
            return Err((
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    detail: format!("Source branch '{}' not found", request.source),
                }),
            ));
        }

        self.branches.lock().await.insert(request.name.clone());
        let snapshot = request
            .snapshot
            .unwrap_or_else(|| format!("{}__snap", request.name));

        Ok(CreateBranchResponse {
            name: request.name,
            source: request.source,
            snapshot,
            created_at: "2026-01-01T00:00:00".to_string(),
        })
    }

    async fn diff_branch(
        &self,
        _user_id: String,
        request: DiffData,
    ) -> Result<DiffResponse, (StatusCode, Json<ErrorResponse>)> {
        if !self.branch_exists(&request.source).await || !self.branch_exists(&request.target).await
        {
            return Err((
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    detail: "Branch not found".to_string(),
                }),
            ));
        }

        let rows = if request.output.as_deref() == Some("count") {
            Vec::new()
        } else {
            vec![serde_json::json!({ "change": "modified" })]
        };

        Ok(DiffResponse { rows, count: 1 })
    }

    async fn merge_branch(
        &self,
        _user_id: String,
        request: MergeData,
    ) -> Result<MergeResponse, (StatusCode, Json<ErrorResponse>)> {
        if !self.branch_exists(&request.source).await || !self.branch_exists(&request.target).await
        {
            return Err((
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    detail: "Branch not found".to_string(),
                }),
            ));
        }

        Ok(MergeResponse {
            status: "merged".to_string(),
            source: request.source,
            target: request.target,
            rows_affected: 3,
        })
    }

    async fn delete_branch(
        &self,
        _user_id: String,
        request: DeleteBranchData,
    ) -> Result<StatusResponse, (StatusCode, Json<ErrorResponse>)> {
        let removed = self.branches.lock().await.remove(&request.name);
        if !removed {
            return Err((
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    detail: format!("Branch '{}' not found", request.name),
                }),
            ));
        }

        Ok(StatusResponse {
            status: "deleted".to_string(),
        })
    }

    async fn estimate_cost(
        &self,
        request: CostEstimateData,
    ) -> Result<CostEstimateResponse, (StatusCode, Json<ErrorResponse>)> {
        let estimated_tokens = request.session_count.unwrap_or(1) * 500;
        let estimated_cost = (estimated_tokens as f64) * 0.00001;
        let budget = request.budget_remaining.unwrap_or(10.0);

        Ok(CostEstimateResponse {
            operation: request.operation,
            model: request.model,
            estimated_tokens,
            estimated_cost,
            exceeds_budget: estimated_cost > budget,
            alternatives: Vec::new(),
        })
    }
}

fn build_test_app() -> axum::Router {
    let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
        .with_auth_service(Arc::new(StubAuthService))
        .with_branch_service(Arc::new(InMemoryBranchService::new()));
    build_app(state)
}

async fn response_json(resp: axum::http::Response<body::Body>) -> serde_json::Value {
    let bytes = body::to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn create_branch_returns_201() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/branches")
                .header("X-User-Id", "user-1")
                .header("content-type", "application/json")
                .body(body::Body::from(r#"{"name":"feature-a","source":"main"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::CREATED);
    let json = response_json(resp).await;
    assert_eq!(json["name"], "feature-a");
    assert_eq!(json["source"], "main");
    assert_eq!(json["snapshot"], "feature-a__snap");
}

#[tokio::test]
async fn diff_and_merge_return_ok() {
    let app = build_test_app();

    let create_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/branches")
                .header("X-User-Id", "user-1")
                .header("content-type", "application/json")
                .body(body::Body::from(r#"{"name":"feature-a","source":"main"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_resp.status(), StatusCode::CREATED);

    let diff_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/branches/diff")
                .header("X-User-Id", "user-1")
                .header("content-type", "application/json")
                .body(body::Body::from(
                    r#"{"target":"feature-a","source":"main"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(diff_resp.status(), StatusCode::OK);
    let diff_json = response_json(diff_resp).await;
    assert_eq!(diff_json["count"], 1);

    let merge_resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/branches/merge")
                .header("X-User-Id", "user-1")
                .header("content-type", "application/json")
                .body(body::Body::from(
                    r#"{"source":"feature-a","target":"main","on_conflict":"accept"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(merge_resp.status(), StatusCode::OK);
    let merge_json = response_json(merge_resp).await;
    assert_eq!(merge_json["status"], "merged");
}

#[tokio::test]
async fn estimate_cost_returns_ok() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/branches/cost-estimate")
                .header("X-User-Id", "user-1")
                .header("content-type", "application/json")
                .body(body::Body::from(
                    r#"{"operation":"merge","model":"gpt-5","session_count":2,"budget_remaining":0.001}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = response_json(resp).await;
    assert_eq!(json["operation"], "merge");
    assert_eq!(json["model"], "gpt-5");
    assert_eq!(json["estimated_tokens"], 1000);
}

#[tokio::test]
async fn delete_branch_not_found_returns_404() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/branches")
                .header("X-User-Id", "user-1")
                .header("content-type", "application/json")
                .body(body::Body::from(
                    r#"{"name":"missing-branch","is_database":false}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn missing_user_id_returns_401() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/branches")
                .header("content-type", "application/json")
                .body(body::Body::from(r#"{"name":"feature-a","source":"main"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
