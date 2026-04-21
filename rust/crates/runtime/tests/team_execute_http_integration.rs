//! HTTP integration tests for `POST /teams/{name}/execute` (Tower oneshot, no network).
//!
//! Injects mock `SubRunExecutor` implementations so delegation runs without a real LLM.
//!
//! **CLI parity:** REPL command `/team run review review the latest commit` parses as
//! team `review` and task `review the latest commit` (see `splitn(2, ' ')` in
//! `astra-cli/src/cli/slash_team.rs`). The tests below use the same task string against
//! the built-in adversarial `review` team (`InMemoryTeamStore::with_builtins`).
//!
//! **Failure matrix:** custom `SubRunExecutor` types simulate hard `Err`, mid-run failure
//! after N successes (pipeline + adversarial in one test), role-specific failures, HTTP-ish
//! “200 + failed status” bodies, invalid JSON / missing `task`, and cross-user isolation —
//! asserting HTTP codes and `TeamExecutionReport` mapping (`failed` vs `partial` when only
//! some agents terminate successfully).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use axum::{
    Router,
    body::{self, Body},
    http::{HeaderMap, Request, StatusCode},
};
use serde_json::{Value, json};
use tower::util::ServiceExt;

use astra_runtime::server::delegation_engine::{
    DelegationEngine, DelegationTracker, StubSubRunExecutor, SubRunConfig, SubRunExecutor,
};
use astra_runtime::server::run_engine::RunEngine;
use astra_runtime::{
    AppState, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
    AuthTokenRecord, AuthUserRecord, ErrorResponse, HealthChecker, ServiceInfo, build_app,
};
use astra_services::coordination::{AgentProfile, AgentProfileRegistry, AgentResult, AgentTier};
use astra_services::runs::InMemoryRunStateStore;
use astra_services::team_persistence::{
    InMemoryTeamStore, TeamCoordination, TeamDefinition, TeamMemberDef, TeamPersistenceService,
    WorktreeMode,
};

// ─── Stubs (aligned with team_api_integration) ──────────────────────────────

#[derive(Clone)]
struct StubHealth;

#[async_trait]
impl HealthChecker for StubHealth {
    async fn database_healthy(&self) -> bool {
        true
    }
}

struct StubAuth;

#[async_trait]
impl AuthService for StubAuth {
    async fn register(
        &self,
        _r: AuthRegisterRequestData,
    ) -> Result<AuthUserRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }
    async fn login(
        &self,
        _r: AuthLoginRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }
    async fn refresh(
        &self,
        _r: AuthRefreshRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }
    async fn logout(
        &self,
        _r: AuthRefreshRequestData,
    ) -> Result<(), (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }
    async fn current_user(
        &self,
        headers: &HeaderMap,
    ) -> Result<AuthUserRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        match headers.get("authorization").and_then(|v| v.to_str().ok()) {
            Some(h) if h.starts_with("Bearer ") => {
                let user_id = h.trim_start_matches("Bearer ");
                Ok(AuthUserRecord {
                    user_id: user_id.to_string(),
                    username: format!("user-{user_id}"),
                    email: format!("{user_id}@test.local"),
                    display_name: None,
                })
            }
            _ => Err((
                StatusCode::UNAUTHORIZED,
                axum::Json(ErrorResponse::new("Not authenticated")),
            )),
        }
    }
}

fn auth(user: &str) -> Vec<(&str, String)> {
    vec![("authorization", format!("Bearer {user}"))]
}

async fn post_json(app: Router, path: &str, user: &str, payload: Value) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json");
    for (k, v) in auth(user) {
        builder = builder.header(k, v);
    }
    let response = app
        .oneshot(builder.body(Body::from(payload.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

/// Build app with team store + delegation engine using the given sub-run executor (mock LLM).
async fn build_app_with_delegation(
    team_store: Arc<InMemoryTeamStore>,
    executor: Arc<dyn SubRunExecutor>,
) -> Router {
    let registry = Arc::new(tokio::sync::RwLock::new(AgentProfileRegistry::new()));
    {
        let mut reg = registry.write().await;
        let _ = reg.register(AgentProfile::new(
            "orchestrator",
            "orchestrator",
            AgentTier::Orchestrator,
        ));
    }

    let run_store = Arc::new(InMemoryRunStateStore::new());
    let run_engine = Arc::new(RunEngine::new(run_store));
    let tracker = Arc::new(DelegationTracker::new());
    let delegation = Arc::new(DelegationEngine::with_executor(
        registry.clone(),
        run_engine,
        tracker,
        executor,
    ));

    let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealth))
        .with_auth_service(Arc::new(StubAuth))
        .with_team_store(team_store as Arc<dyn TeamPersistenceService>)
        .with_delegation_engine(delegation);

    build_app(state)
}

fn build_app_team_only(team_store: Arc<InMemoryTeamStore>) -> Router {
    let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealth))
        .with_auth_service(Arc::new(StubAuth))
        .with_team_store(team_store as Arc<dyn TeamPersistenceService>);
    build_app(state)
}

// ─── Mock executors ─────────────────────────────────────────────────────────

struct ErrorExecutor;

#[async_trait]
impl SubRunExecutor for ErrorExecutor {
    async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
        Err(format!("agent {} crashed", config.agent_profile.agent_id))
    }
}

struct HighTokenExecutor;

#[async_trait]
impl SubRunExecutor for HighTokenExecutor {
    async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
        Ok(AgentResult {
            agent_id: config.agent_profile.agent_id,
            run_id: config.run_id,
            status: astra_core::STATUS_COMPLETED.to_string(),
            output: Some("done".into()),
            error: None,
            prompt_tokens: 500,
            completion_tokens: 500,
            tool_calls: 0,
        })
    }
}

/// First `fail_after` sub-runs succeed; the next returns `Err` (infra / LLM hard failure).
struct FailAfterSuccessExecutor {
    calls: AtomicUsize,
    fail_after: usize,
}

impl FailAfterSuccessExecutor {
    fn new(fail_after: usize) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            fail_after,
        }
    }
}

#[async_trait]
impl SubRunExecutor for FailAfterSuccessExecutor {
    async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if n > self.fail_after {
            return Err(format!(
                "simulated hard failure after {} successful sub-runs (agent {})",
                self.fail_after, config.agent_profile.agent_id
            ));
        }
        Ok(AgentResult {
            agent_id: config.agent_profile.agent_id.clone(),
            run_id: config.run_id.clone(),
            status: astra_core::STATUS_COMPLETED.to_string(),
            output: Some(format!("[ok #{n}] {}", config.task)),
            error: None,
            prompt_tokens: 1,
            completion_tokens: 1,
            tool_calls: 0,
        })
    }
}

/// Returns `Err` only when `agent_id` contains `needle` (e.g. reviewer-only flake).
struct ErrWhenAgentIdContains {
    needle: &'static str,
}

#[async_trait]
impl SubRunExecutor for ErrWhenAgentIdContains {
    async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
        if config.agent_profile.agent_id.contains(self.needle) {
            return Err(format!(
                "simulated role-specific failure for {}",
                config.agent_profile.agent_id
            ));
        }
        Ok(AgentResult {
            agent_id: config.agent_profile.agent_id.clone(),
            run_id: config.run_id.clone(),
            status: astra_core::STATUS_COMPLETED.to_string(),
            output: Some("stub ok".into()),
            error: None,
            prompt_tokens: 1,
            completion_tokens: 1,
            tool_calls: 0,
        })
    }
}

/// Returns Ok but with terminal `status` = failed (some providers return 200 with error payload).
struct OkButFailedStatusExecutor;

#[async_trait]
impl SubRunExecutor for OkButFailedStatusExecutor {
    async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
        Ok(AgentResult {
            agent_id: config.agent_profile.agent_id.clone(),
            run_id: config.run_id.clone(),
            status: astra_core::STATUS_FAILED.to_string(),
            output: None,
            error: Some("provider returned failed status in body".into()),
            prompt_tokens: 0,
            completion_tokens: 0,
            tool_calls: 0,
        })
    }
}

async fn post_raw_body(
    app: Router,
    path: &str,
    user: Option<&str>,
    body: &str,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json");
    if let Some(u) = user {
        for (k, v) in auth(u) {
            builder = builder.header(k, v);
        }
    }
    let response = app
        .oneshot(builder.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn http_execute_review_latest_commit_happy_path_matches_cli_team_run() {
    // Parity: `/team run review review the latest commit` → team=review, task=this string.
    let store = Arc::new(InMemoryTeamStore::with_builtins("test-user"));
    let app = build_app_with_delegation(store, Arc::new(StubSubRunExecutor)).await;

    let (status, body) = post_json(
        app,
        "/teams/review/execute",
        "test-user",
        json!({
            "task": "review the latest commit",
            "session_id": "cli-parity-session",
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "completed");
    // Builtin `review` team: Adversarial, max_rounds=3 → 3 rounds × (producer + reviewer) = 6
    assert_eq!(body["agent_count"], 6);
    assert!(!body["delegation_id"].as_str().unwrap().is_empty());
}

#[tokio::test]
async fn http_execute_review_latest_commit_unhappy_subrun_errors() {
    let store = Arc::new(InMemoryTeamStore::with_builtins("test-user"));
    let app = build_app_with_delegation(store, Arc::new(ErrorExecutor)).await;

    let (status, body) = post_json(
        app,
        "/teams/review/execute",
        "test-user",
        json!({ "task": "review the latest commit" }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "failed");
    let err = body["error"].as_str().unwrap();
    assert!(
        err.contains("failed") || err.contains("crashed"),
        "expected sub-run failure summary in body, got {err}"
    );
}

#[tokio::test]
async fn http_execute_research_team_stub_success() {
    let store = Arc::new(InMemoryTeamStore::with_builtins("test-user"));
    let app = build_app_with_delegation(store, Arc::new(StubSubRunExecutor)).await;

    let (status, body) = post_json(
        app,
        "/teams/research/execute",
        "test-user",
        json!({ "task": "analyze the codebase" }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "completed");
    assert_eq!(body["agent_count"], 2);
    assert!(!body["delegation_id"].as_str().unwrap().is_empty());
    assert!(!body["parent_run_id"].as_str().unwrap().is_empty());
}

#[tokio::test]
async fn http_execute_returns_503_without_delegation_engine() {
    let store = Arc::new(InMemoryTeamStore::with_builtins("test-user"));
    let app = build_app_team_only(store);

    let (status, body) = post_json(
        app,
        "/teams/research/execute",
        "test-user",
        json!({ "task": "x" }),
    )
    .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error_code"], "delegation_not_configured");
}

#[tokio::test]
async fn http_execute_unknown_team_404() {
    let store = Arc::new(InMemoryTeamStore::with_builtins("test-user"));
    let app = build_app_with_delegation(store, Arc::new(StubSubRunExecutor)).await;

    let (status, body) = post_json(
        app,
        "/teams/nonexistent-team/execute",
        "test-user",
        json!({ "task": "t" }),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["detail"].as_str().unwrap().contains("not found"));
}

#[tokio::test]
async fn http_execute_validation_failure_400_empty_members() {
    let store = Arc::new(InMemoryTeamStore::new());
    let bad = TeamDefinition {
        team_id: "bad-id".into(),
        user_id: "test-user".into(),
        name: "bad-empty".into(),
        description: "x".into(),
        coordination: TeamCoordination::Pipeline,
        members: vec![],
        context: HashMap::new(),
        worktree_mode: WorktreeMode::Shared,
        budget: None,
        max_parallel: 0,
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
    };
    store.save_team(&bad).await.unwrap();

    let app = build_app_with_delegation(store, Arc::new(StubSubRunExecutor)).await;

    let (status, body) = post_json(
        app,
        "/teams/bad-empty/execute",
        "test-user",
        json!({ "task": "t" }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["detail"].as_str().unwrap().contains("validation"));
}

#[tokio::test]
async fn http_execute_token_budget_exceeded_body_status() {
    let store = Arc::new(InMemoryTeamStore::new());
    let team = TeamDefinition {
        team_id: "tb".into(),
        user_id: "test-user".into(),
        name: "budget-team".into(),
        description: "d".into(),
        coordination: TeamCoordination::Pipeline,
        members: vec![TeamMemberDef {
            role: "worker".into(),
            agent_id: None,
            system_prompt: Some("work".into()),
            skills: vec![],
            model_override: None,
            mcp_servers: vec![],
            can_delegate: false,
            max_delegation_depth: 0,
        }],
        context: HashMap::new(),
        worktree_mode: WorktreeMode::Shared,
        budget: Some(astra_services::team_persistence::TeamBudget {
            max_cost_usd: 0.0,
            max_tokens: 100,
            max_duration_secs: 0,
        }),
        max_parallel: 0,
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
    };
    store.save_team(&team).await.unwrap();

    let app = build_app_with_delegation(store, Arc::new(HighTokenExecutor)).await;

    let (status, body) = post_json(
        app,
        "/teams/budget-team/execute",
        "test-user",
        json!({ "task": "task" }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "completed_over_budget");
    assert!(body["error"].as_str().unwrap().contains("budget"));
}

#[tokio::test]
async fn http_execute_unauthorized_without_bearer() {
    let store = Arc::new(InMemoryTeamStore::with_builtins("test-user"));
    let app = build_app_with_delegation(store, Arc::new(StubSubRunExecutor)).await;

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/teams/research/execute")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"task":"x"}"#.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ─── Failure-matrix: sub-run patterns (HTTP surfaces same as CLI team run) ──

/// Pipeline fails on 2nd sub-run vs adversarial fails after 4 successes — both should surface
/// `failed` or `partial` over HTTP (same executors as `team_delegation_integration` mid-run cases).
#[tokio::test]
async fn http_fail_after_n_hard_error_pipeline_and_adversarial() {
    struct Case {
        path: &'static str,
        task: &'static str,
        fail_after: usize,
    }
    let cases = [
        Case {
            path: "/teams/research/execute",
            task: "survey the repo",
            fail_after: 1,
        },
        Case {
            path: "/teams/review/execute",
            task: "review the latest commit",
            fail_after: 4,
        },
    ];
    for case in cases {
        let store = Arc::new(InMemoryTeamStore::with_builtins("test-user"));
        let app = build_app_with_delegation(
            store,
            Arc::new(FailAfterSuccessExecutor::new(case.fail_after)),
        )
        .await;

        let (status, body) =
            post_json(app, case.path, "test-user", json!({ "task": case.task })).await;

        assert_eq!(status, StatusCode::OK, "case path={}", case.path);
        let st = body["status"].as_str().unwrap();
        assert!(
            st == "failed" || st == "partial",
            "case path={}: expected failed or partial, got {st}",
            case.path
        );
        let err = body["error"].as_str().unwrap();
        assert!(
            !err.is_empty(),
            "case path={}: expected error summary",
            case.path
        );
    }
}

#[tokio::test]
async fn http_review_only_reviewer_steps_return_err() {
    let store = Arc::new(InMemoryTeamStore::with_builtins("test-user"));
    let app = build_app_with_delegation(
        store,
        Arc::new(ErrWhenAgentIdContains { needle: "reviewer" }),
    )
    .await;

    let (status, body) = post_json(
        app,
        "/teams/review/execute",
        "test-user",
        json!({ "task": "review the latest commit" }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let st = body["status"].as_str().unwrap();
    assert!(
        st == "failed" || st == "partial",
        "expected failed or partial when reviewer steps error, got {st}"
    );
    assert!(body["error"].as_str().is_some());
}

#[tokio::test]
async fn http_pipeline_ok_response_but_agent_status_failed() {
    let store = Arc::new(InMemoryTeamStore::new());
    let team = TeamDefinition {
        team_id: "single-fail".into(),
        user_id: "test-user".into(),
        name: "single-fail".into(),
        description: "d".into(),
        coordination: TeamCoordination::Pipeline,
        members: vec![TeamMemberDef {
            role: "solo".into(),
            agent_id: None,
            system_prompt: Some("work".into()),
            skills: vec![],
            model_override: None,
            mcp_servers: vec![],
            can_delegate: false,
            max_delegation_depth: 0,
        }],
        context: HashMap::new(),
        worktree_mode: WorktreeMode::Shared,
        budget: None,
        max_parallel: 0,
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
    };
    store.save_team(&team).await.unwrap();

    let app = build_app_with_delegation(store, Arc::new(OkButFailedStatusExecutor)).await;

    let (status, body) = post_json(
        app,
        "/teams/single-fail/execute",
        "test-user",
        json!({ "task": "do one thing" }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(
        body["status"] == "failed" || body["status"] == "partial",
        "unexpected status: {:?}",
        body["status"]
    );
}

#[tokio::test]
async fn http_execute_invalid_json_or_missing_task() {
    let store = Arc::new(InMemoryTeamStore::with_builtins("test-user"));
    let app = build_app_with_delegation(store, Arc::new(StubSubRunExecutor)).await;

    let (status, _) = post_raw_body(
        app.clone(),
        "/teams/research/execute",
        Some("test-user"),
        r#"{"session_id":"only"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let (status, _) = post_raw_body(
        app,
        "/teams/research/execute",
        Some("test-user"),
        "{not json",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn http_execute_team_for_different_user_returns_404() {
    let store = Arc::new(InMemoryTeamStore::with_builtins("test-user"));
    let app = build_app_with_delegation(store, Arc::new(StubSubRunExecutor)).await;

    let (status, body) = post_json(
        app,
        "/teams/research/execute",
        "other-user",
        json!({ "task": "t" }),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["detail"].as_str().unwrap().contains("not found"));
}
