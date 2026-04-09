//! Realistic team HTTP API integration tests — complex lifecycle scenarios.
//!
//! Exercises the full team management surface through HTTP requests:
//! CRUD with budget/max_parallel, validation edge-cases, execution history,
//! concurrent team mutations, upsert semantics, and large-team handling.
//!
//! Uses Tower oneshot (no network), InMemoryTeamStore (no DB required).

use std::sync::Arc;

use async_trait::async_trait;
use axum::{
    Router,
    body::{self, Body},
    http::{HeaderMap, Request, StatusCode},
};
use serde_json::{Value, json};
use tower::util::ServiceExt;

use astra_runtime::{
    AppState, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
    AuthTokenRecord, AuthUserRecord, ErrorResponse, HealthChecker, ServiceInfo, build_app,
};
use astra_services::team_persistence::{InMemoryTeamStore, TeamPersistenceService};

// ─── Stubs ──────────────────────────────────────────────────────────────────

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
                axum::Json(ErrorResponse {
                    detail: "Not authenticated".into(),
                }),
            )),
        }
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────────

fn build_test_app() -> Router {
    let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealth))
        .with_auth_service(Arc::new(StubAuth))
        .with_team_store(Arc::new(InMemoryTeamStore::new()));
    build_app(state)
}

fn build_test_app_without_team_store() -> Router {
    let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealth))
        .with_auth_service(Arc::new(StubAuth));
    build_app(state)
}

fn build_test_app_with_store(store: Arc<InMemoryTeamStore>) -> Router {
    let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealth))
        .with_auth_service(Arc::new(StubAuth))
        .with_team_store(store);
    build_app(state)
}

fn auth(user: &str) -> Vec<(&str, String)> {
    vec![("authorization", format!("Bearer {user}"))]
}

async fn get(app: Router, path: &str, user: &str) -> (StatusCode, Value) {
    let mut builder = Request::builder().method("GET").uri(path);
    for (k, v) in auth(user) {
        builder = builder.header(k, v);
    }
    let response = app
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

async fn post(app: Router, path: &str, user: &str, payload: Value) -> (StatusCode, Value) {
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

async fn post_raw(app: Router, path: &str, user: &str, body: &str) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json");
    for (k, v) in auth(user) {
        builder = builder.header(k, v);
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

async fn delete(app: Router, path: &str, user: &str) -> (StatusCode, Value) {
    let mut builder = Request::builder().method("DELETE").uri(path);
    for (k, v) in auth(user) {
        builder = builder.header(k, v);
    }
    let response = app
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

// ─── Payloads ───────────────────────────────────────────────────────────────

fn dev_team_payload() -> Value {
    json!({
        "name": "dev-cycle",
        "description": "Full dev cycle: plan, implement, test, review",
        "coordination": { "type": "pipeline" },
        "members": [
            {
                "role": "planner",
                "system_prompt": "Decompose the task into subtasks with acceptance criteria.",
                "skills": ["plan-decompose"],
                "mcp_servers": [],
                "can_delegate": false,
                "max_delegation_depth": 0
            },
            {
                "role": "implementer",
                "agent_id": "fast-coder",
                "system_prompt": "Implement code changes following the plan.",
                "skills": ["edit", "shell"],
                "model_override": "claude-sonnet",
                "mcp_servers": ["filesystem"],
                "can_delegate": true,
                "max_delegation_depth": 2
            },
            {
                "role": "tester",
                "system_prompt": "Write and run tests, verifying acceptance criteria.",
                "skills": ["verify-task", "shell"],
                "mcp_servers": [],
                "can_delegate": false,
                "max_delegation_depth": 0
            },
            {
                "role": "reviewer",
                "system_prompt": "Review code changes for correctness, style, and security.",
                "skills": ["review-changes"],
                "model_override": "claude-opus",
                "mcp_servers": ["github"],
                "can_delegate": false,
                "max_delegation_depth": 0
            }
        ],
        "context": {
            "repo": "mo-dev-agent",
            "language": "rust",
            "test_cmd": "cargo test --workspace"
        },
        "worktree_mode": "isolated",
        "budget": {
            "max_cost_usd": 25.0,
            "max_tokens": 2000000,
            "max_duration_secs": 1800
        },
        "max_parallel": 2
    })
}

fn adversarial_review_payload() -> Value {
    json!({
        "name": "adversarial-review",
        "description": "Producer writes code, reviewer challenges it for 5 rounds",
        "coordination": {
            "type": "adversarial",
            "max_rounds": 5,
            "threshold": 0.85
        },
        "members": [
            {
                "role": "producer",
                "system_prompt": "Write high-quality code.",
                "skills": ["edit", "shell"],
                "mcp_servers": [],
                "can_delegate": false,
                "max_delegation_depth": 0
            },
            {
                "role": "reviewer",
                "system_prompt": "Find bugs, security issues, and performance problems.",
                "skills": ["review-changes"],
                "mcp_servers": [],
                "can_delegate": false,
                "max_delegation_depth": 0
            }
        ],
        "budget": {
            "max_cost_usd": 10.0,
            "max_tokens": 500000,
            "max_duration_secs": 600
        }
    })
}

fn fanout_research_payload() -> Value {
    json!({
        "name": "parallel-research",
        "description": "Fan-out: 3 researchers investigate in parallel, results merged",
        "coordination": {
            "type": "fan_out",
            "aggregation": "best_score"
        },
        "members": [
            {
                "role": "researcher-api",
                "system_prompt": "Research REST API best practices.",
                "skills": ["web-search"],
                "mcp_servers": [],
                "can_delegate": false,
                "max_delegation_depth": 0
            },
            {
                "role": "researcher-perf",
                "system_prompt": "Research performance optimization techniques.",
                "skills": ["web-search"],
                "mcp_servers": [],
                "can_delegate": false,
                "max_delegation_depth": 0
            },
            {
                "role": "researcher-sec",
                "system_prompt": "Research security hardening strategies.",
                "skills": ["web-search"],
                "mcp_servers": [],
                "can_delegate": false,
                "max_delegation_depth": 0
            }
        ],
        "max_parallel": 3,
        "budget": {
            "max_cost_usd": 5.0,
            "max_tokens": 300000,
            "max_duration_secs": 300
        }
    })
}

fn sequential_migration_payload() -> Value {
    json!({
        "name": "db-migration",
        "description": "Sequential: analyze schema, write migration, test, deploy",
        "coordination": {
            "type": "sequential",
            "stop_on_success": false
        },
        "members": [
            {
                "role": "schema-analyst",
                "system_prompt": "Analyze current schema and propose migration plan.",
                "skills": [],
                "mcp_servers": ["database"],
                "can_delegate": false,
                "max_delegation_depth": 0
            },
            {
                "role": "migration-writer",
                "system_prompt": "Write backward-compatible SQL migration.",
                "skills": ["edit"],
                "mcp_servers": [],
                "can_delegate": false,
                "max_delegation_depth": 0
            }
        ]
    })
}

// ═══════════════════════════════════════════════════════════════════════════
// Scenario 1: Full lifecycle — create 4 teams, list, get, update, delete
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn scenario_full_team_lifecycle() {
    let app = build_test_app();
    let user = "lifecycle-user";

    // ── Create 4 teams with different coordination patterns ──
    let teams = [
        ("dev-cycle", dev_team_payload()),
        ("adversarial-review", adversarial_review_payload()),
        ("parallel-research", fanout_research_payload()),
        ("db-migration", sequential_migration_payload()),
    ];

    for (name, payload) in &teams {
        let (status, body) = post(app.clone(), "/teams", user, payload.clone()).await;
        assert_eq!(status, StatusCode::OK, "create {name} failed: {body}");
        assert_eq!(body["name"], *name);
        assert!(!body["team_id"].as_str().unwrap().is_empty());
    }

    // ── List: should see all 4 ──
    let (status, body) = get(app.clone(), "/teams", user).await;
    assert_eq!(status, StatusCode::OK);
    let team_list = body["teams"].as_array().unwrap();
    assert_eq!(team_list.len(), 4);
    let names: Vec<&str> = team_list
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"dev-cycle"));
    assert!(names.contains(&"adversarial-review"));
    assert!(names.contains(&"parallel-research"));
    assert!(names.contains(&"db-migration"));

    // Verify budget/max_parallel visible in list summary
    let dev_summary = team_list.iter().find(|t| t["name"] == "dev-cycle").unwrap();
    assert_eq!(dev_summary["max_parallel"], 2);
    assert_eq!(dev_summary["budget"]["max_cost_usd"], 25.0);
    assert_eq!(dev_summary["coordination"]["type"], "pipeline");
    assert_eq!(dev_summary["worktree_mode"], "isolated");
    let migration_summary = team_list
        .iter()
        .find(|t| t["name"] == "db-migration")
        .unwrap();
    assert_eq!(migration_summary["max_parallel"], 0);
    assert!(migration_summary.get("budget").is_none() || migration_summary["budget"].is_null());

    // ── Get detail for dev-cycle: verify budget & max_parallel ──
    let (status, body) = get(app.clone(), "/teams/dev-cycle", user).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "dev-cycle");
    assert_eq!(body["members"].as_array().unwrap().len(), 4);
    assert_eq!(body["max_parallel"], 2);
    let budget = &body["budget"];
    assert_eq!(budget["max_cost_usd"], 25.0);
    assert_eq!(budget["max_tokens"], 2_000_000);
    assert_eq!(budget["max_duration_secs"], 1800);
    assert_eq!(body["worktree_mode"], "isolated");
    assert_eq!(body["coordination"]["type"], "pipeline");

    // ── Get detail for adversarial-review ──
    let (status, body) = get(app.clone(), "/teams/adversarial-review", user).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["coordination"]["type"], "adversarial");
    assert_eq!(body["coordination"]["max_rounds"], 5);
    assert_eq!(body["coordination"]["threshold"], 0.85);
    assert_eq!(body["members"].as_array().unwrap().len(), 2);

    // ── Update dev-cycle: change budget and add max_parallel ──
    let mut updated = dev_team_payload();
    updated["description"] = json!("Updated: full dev cycle v2");
    updated["budget"]["max_cost_usd"] = json!(50.0);
    updated["max_parallel"] = json!(4);
    let (status, body) = post(app.clone(), "/teams", user, updated).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["description"], "Updated: full dev cycle v2");
    assert_eq!(body["budget"]["max_cost_usd"], 50.0);
    assert_eq!(body["max_parallel"], 4);

    // Re-fetch to confirm persistence
    let (status, body) = get(app.clone(), "/teams/dev-cycle", user).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["description"], "Updated: full dev cycle v2");
    assert_eq!(body["budget"]["max_cost_usd"], 50.0);
    assert_eq!(body["max_parallel"], 4);

    // ── Delete db-migration ──
    let (status, body) = delete(app.clone(), "/teams/db-migration", user).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["deleted"].as_bool().unwrap());

    // Confirm gone
    let (status, _) = get(app.clone(), "/teams/db-migration", user).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // List should now have 3
    let (status, body) = get(app.clone(), "/teams", user).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["teams"].as_array().unwrap().len(), 3);
}

// ═══════════════════════════════════════════════════════════════════════════
// Scenario 2: Multi-user isolation — teams are scoped per user
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn scenario_multi_user_isolation() {
    let app = build_test_app();

    // User A creates a team
    let (s, _) = post(app.clone(), "/teams", "alice", dev_team_payload()).await;
    assert_eq!(s, StatusCode::OK);

    // User B creates a team with the same name
    let (s, _) = post(app.clone(), "/teams", "bob", dev_team_payload()).await;
    assert_eq!(s, StatusCode::OK);

    // Each user sees only their own
    let (_, body_a) = get(app.clone(), "/teams", "alice").await;
    let (_, body_b) = get(app.clone(), "/teams", "bob").await;
    assert_eq!(body_a["teams"].as_array().unwrap().len(), 1);
    assert_eq!(body_b["teams"].as_array().unwrap().len(), 1);

    // Different team_ids
    let id_a = body_a["teams"][0]["team_id"].as_str().unwrap();
    let id_b = body_b["teams"][0]["team_id"].as_str().unwrap();
    assert_ne!(id_a, id_b);

    // User A cannot see user B's team by name (scoped)
    let (s, _) = get(app.clone(), "/teams/dev-cycle", "charlie").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

// ═══════════════════════════════════════════════════════════════════════════
// Scenario 3: Validation edge cases
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn scenario_validation_rejects_bad_teams() {
    let app = build_test_app();
    let user = "validator";

    // Empty members
    let (status, body) = post(
        app.clone(),
        "/teams",
        user,
        json!({
            "name": "empty-team",
            "description": "no members",
            "coordination": { "type": "pipeline" },
            "members": []
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "empty members: {body}");

    // Adversarial with wrong member count (needs exactly 2)
    let (status, body) = post(
        app.clone(),
        "/teams",
        user,
        json!({
            "name": "bad-adversarial",
            "description": "3 members for adversarial",
            "coordination": { "type": "adversarial", "max_rounds": 3, "threshold": 0.8 },
            "members": [
                { "role": "a", "skills": [], "mcp_servers": [] },
                { "role": "b", "skills": [], "mcp_servers": [] },
                { "role": "c", "skills": [], "mcp_servers": [] }
            ]
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "adversarial 3 members: {body}"
    );

    // Duplicate roles
    let (status, body) = post(
        app.clone(),
        "/teams",
        user,
        json!({
            "name": "dup-roles",
            "description": "duplicate role names",
            "coordination": { "type": "pipeline" },
            "members": [
                { "role": "coder", "skills": [], "mcp_servers": [] },
                { "role": "coder", "skills": [], "mcp_servers": [] }
            ]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "duplicate roles: {body}");

    // Negative budget
    let (status, body) = post(
        app.clone(),
        "/teams",
        user,
        json!({
            "name": "neg-budget",
            "description": "negative cost",
            "coordination": { "type": "pipeline" },
            "members": [
                { "role": "coder", "skills": [], "mcp_servers": [] }
            ],
            "budget": { "max_cost_usd": -5.0 }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "negative budget: {body}");
}

// ═══════════════════════════════════════════════════════════════════════════
// Scenario 4: Upsert semantics — same name preserves team_id
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn scenario_upsert_preserves_team_id() {
    let app = build_test_app();
    let user = "upsert-user";

    // Create
    let (_, body1) = post(app.clone(), "/teams", user, sequential_migration_payload()).await;
    let team_id = body1["team_id"].as_str().unwrap().to_string();
    assert!(!team_id.is_empty());

    // Upsert with changed description
    let mut updated = sequential_migration_payload();
    updated["description"] = json!("Updated migration workflow v2");
    let (_, body2) = post(app.clone(), "/teams", user, updated).await;
    assert_eq!(
        body2["team_id"].as_str().unwrap(),
        team_id,
        "team_id must be stable across upserts"
    );
    assert_eq!(body2["description"], "Updated migration workflow v2");
}

// ═══════════════════════════════════════════════════════════════════════════
// Scenario 5: Execution history through orchestrator-level store
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn scenario_execution_history_via_api() {
    let app = build_test_app();
    let user = "exec-user";

    // Create team
    let (s, body) = post(app.clone(), "/teams", user, fanout_research_payload()).await;
    assert_eq!(s, StatusCode::OK);
    let team_name = body["name"].as_str().unwrap();

    // No executions yet
    let (s, body) = get(app.clone(), &format!("/teams/{team_name}/executions"), user).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["executions"].as_array().unwrap().len(), 0);
    assert_eq!(body["team_name"], team_name);
}

// ═══════════════════════════════════════════════════════════════════════════
// Scenario 6: Delete non-existent → 404
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn scenario_delete_nonexistent_returns_404() {
    let app = build_test_app();
    let (status, _) = delete(app, "/teams/ghost-team", "anyone").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ═══════════════════════════════════════════════════════════════════════════
// Scenario 7: No auth → 401
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn scenario_no_auth_returns_401() {
    let app = build_test_app();
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/teams")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ═══════════════════════════════════════════════════════════════════════════
// Scenario 8: Complex team with all fields populated — round-trip fidelity
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn scenario_complex_team_full_roundtrip() {
    let app = build_test_app();
    let user = "complex-user";

    let payload = json!({
        "name": "mega-team",
        "description": "Complex team exercising every field",
        "coordination": {
            "type": "fan_out",
            "aggregation": "merge"
        },
        "members": [
            {
                "role": "lead",
                "agent_id": "agent-lead-001",
                "system_prompt": "You are the tech lead. Coordinate and review.",
                "skills": ["review-changes", "plan-decompose"],
                "model_override": "claude-opus",
                "mcp_servers": ["github", "jira"],
                "can_delegate": true,
                "max_delegation_depth": 3
            },
            {
                "role": "backend",
                "system_prompt": "Implement backend features in Rust.",
                "skills": ["edit", "shell", "review-changes"],
                "model_override": "claude-sonnet",
                "mcp_servers": ["filesystem", "database"],
                "can_delegate": true,
                "max_delegation_depth": 1
            },
            {
                "role": "frontend",
                "agent_id": "agent-frontend-react",
                "system_prompt": "Implement React components with TypeScript.",
                "skills": ["edit", "shell"],
                "mcp_servers": ["filesystem", "browser"],
                "can_delegate": false,
                "max_delegation_depth": 0
            },
            {
                "role": "devops",
                "system_prompt": "Handle CI/CD, Docker, and deployment.",
                "skills": ["shell"],
                "model_override": "claude-haiku",
                "mcp_servers": ["kubernetes", "docker"],
                "can_delegate": false,
                "max_delegation_depth": 0
            },
            {
                "role": "security",
                "system_prompt": "Audit for vulnerabilities and compliance.",
                "skills": ["review-changes", "web-search"],
                "mcp_servers": [],
                "can_delegate": false,
                "max_delegation_depth": 0
            }
        ],
        "context": {
            "repo": "mo-dev-agent",
            "language": "rust,typescript",
            "ci": "github-actions",
            "deploy_target": "kubernetes",
            "branch": "feature/team-system"
        },
        "worktree_mode": "isolated",
        "budget": {
            "max_cost_usd": 100.0,
            "max_tokens": 5000000,
            "max_duration_secs": 3600
        },
        "max_parallel": 3
    });

    // Create
    let (status, body) = post(app.clone(), "/teams", user, payload.clone()).await;
    assert_eq!(status, StatusCode::OK, "create mega-team: {body}");

    // Fetch and verify every field
    let (status, team) = get(app.clone(), "/teams/mega-team", user).await;
    assert_eq!(status, StatusCode::OK);

    // Top-level
    assert_eq!(team["name"], "mega-team");
    assert_eq!(team["description"], "Complex team exercising every field");
    assert_eq!(team["user_id"], user);
    assert_eq!(team["worktree_mode"], "isolated");
    assert_eq!(team["max_parallel"], 3);

    // Coordination
    assert_eq!(team["coordination"]["type"], "fan_out");
    assert_eq!(team["coordination"]["aggregation"], "merge");

    // Budget
    let b = &team["budget"];
    assert_eq!(b["max_cost_usd"], 100.0);
    assert_eq!(b["max_tokens"], 5_000_000);
    assert_eq!(b["max_duration_secs"], 3600);

    // Members
    let members = team["members"].as_array().unwrap();
    assert_eq!(members.len(), 5);

    // Verify lead member
    let lead = &members[0];
    assert_eq!(lead["role"], "lead");
    assert_eq!(lead["agent_id"], "agent-lead-001");
    assert_eq!(lead["model_override"], "claude-opus");
    assert!(lead["can_delegate"].as_bool().unwrap());
    assert_eq!(lead["max_delegation_depth"], 3);
    let lead_skills: Vec<&str> = lead["skills"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect();
    assert!(lead_skills.contains(&"review-changes"));
    assert!(lead_skills.contains(&"plan-decompose"));
    let lead_mcp: Vec<&str> = lead["mcp_servers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect();
    assert!(lead_mcp.contains(&"github"));
    assert!(lead_mcp.contains(&"jira"));

    // Verify frontend member has explicit agent_id
    let frontend = &members[2];
    assert_eq!(frontend["role"], "frontend");
    assert_eq!(frontend["agent_id"], "agent-frontend-react");
    assert!(!frontend["can_delegate"].as_bool().unwrap());

    // Context map
    assert_eq!(team["context"]["repo"], "mo-dev-agent");
    assert_eq!(team["context"]["language"], "rust,typescript");
    assert_eq!(team["context"]["ci"], "github-actions");
    assert_eq!(team["context"]["deploy_target"], "kubernetes");
    assert_eq!(team["context"]["branch"], "feature/team-system");

    // ── Upsert: reduce budget, change member ──
    let mut updated = payload.clone();
    updated["budget"]["max_cost_usd"] = json!(50.0);
    updated["max_parallel"] = json!(5);
    let created_at_first = team["created_at"].as_str().unwrap().to_string();

    let (status, body) = post(app.clone(), "/teams", user, updated).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["team_id"], team["team_id"],
        "team_id stable after upsert"
    );
    assert_eq!(body["budget"]["max_cost_usd"], 50.0);
    assert_eq!(body["max_parallel"], 5);
    assert_eq!(
        body["created_at"].as_str(),
        Some(created_at_first.as_str()),
        "created_at must be preserved on upsert"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// Scenario 9: Team without optional fields — budget=null, max_parallel=0
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn scenario_minimal_team_defaults() {
    let app = build_test_app();
    let user = "minimal-user";

    let (status, body) = post(
        app.clone(),
        "/teams",
        user,
        json!({
            "name": "bare-minimum",
            "description": "No budget, no max_parallel, shared worktree",
            "coordination": { "type": "pipeline" },
            "members": [
                { "role": "worker", "skills": [], "mcp_servers": [] }
            ]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "minimal team: {body}");

    let (_, team) = get(app, "/teams/bare-minimum", user).await;
    assert!(team.get("budget").is_none() || team["budget"].is_null());
    assert_eq!(team["max_parallel"], 0);
    assert_eq!(team["worktree_mode"], "shared");
}

// ═══════════════════════════════════════════════════════════════════════════
// Scenario 10: Execution history for non-existent team → 404
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn scenario_executions_nonexistent_team_404() {
    let app = build_test_app();
    let (status, _) = get(app, "/teams/ghost/executions", "anyone").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ═══════════════════════════════════════════════════════════════════════════
// Scenario 11: Team store not configured → 503
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn scenario_team_routes_without_store_return_503() {
    let app = build_test_app_without_team_store();
    let (status, body) = get(app.clone(), "/teams", "u1").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        body["detail"]
            .as_str()
            .unwrap()
            .contains("team service not configured"),
        "body={body}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// Scenario 12: Malformed JSON on POST /teams
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn scenario_post_teams_invalid_json_is_4xx() {
    let app = build_test_app();
    let (status, _) = post_raw(app, "/teams", "u1", "{not valid json").await;
    assert!(
        status.is_client_error(),
        "expected 4xx for invalid JSON, got {status}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// Scenario 13: Execution history reflects store records (limit clamp)
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn scenario_execution_history_and_limit_clamp() {
    let store = Arc::new(InMemoryTeamStore::new());
    let app = build_test_app_with_store(store.clone());
    let user = "exec-history-user";

    let (status, created) = post(
        app.clone(),
        "/teams",
        user,
        json!({
            "name": "exec-history-team",
            "description": "for execution listing",
            "coordination": { "type": "pipeline" },
            "members": [{ "role": "solo", "skills": [], "mcp_servers": [] }]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create team: {created}");

    let team_id = created["team_id"].as_str().unwrap();

    for i in 0..5 {
        let eid = format!("exec-{i}");
        store
            .record_execution_start(&eid, team_id, user, &format!("task {i}"))
            .await
            .unwrap();
    }

    let (status, body) = get(
        app.clone(),
        "/teams/exec-history-team/executions?limit=2",
        user,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["executions"].as_array().unwrap().len(), 2);

    // limit=0 → handler uses default window (50); we have 5 rows
    let (status, body) = get(
        app.clone(),
        "/teams/exec-history-team/executions?limit=0",
        user,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["executions"].as_array().unwrap().len(), 5);

    store
        .record_execution_complete("exec-0", "completed", Some(r#"{"ok":true}"#))
        .await
        .unwrap();

    let (status, body) = get(app, "/teams/exec-history-team/executions?limit=10", user).await;
    assert_eq!(status, StatusCode::OK);
    let execs = body["executions"].as_array().unwrap();
    let done = execs
        .iter()
        .find(|e| e["execution_id"] == "exec-0")
        .unwrap();
    assert_eq!(done["status"], "completed");
}

// ═══════════════════════════════════════════════════════════════════════════
// Scenario 14: Snapshot CRUD via API
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn scenario_snapshot_crud() {
    let app = build_test_app();
    let user = "snap-user";

    // Create a team first
    let (s, _) = post(app.clone(), "/teams", user, sequential_migration_payload()).await;
    assert_eq!(s, StatusCode::OK);

    // Create snapshot
    let (s, snap) = post(
        app.clone(),
        "/teams/db-migration/snapshots",
        user,
        json!({ "label": "before refactor", "git_commit": "abc123" }),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "create snapshot: {snap}");
    assert!(!snap["snapshot_id"].as_str().unwrap().is_empty());
    assert_eq!(snap["team_name"], "db-migration");
    assert_eq!(snap["label"], "before refactor");
    assert_eq!(snap["git_commit"], "abc123");
    assert!(snap["team_definition_json"].as_str().is_some());

    let snap_id = snap["snapshot_id"].as_str().unwrap().to_string();

    // List snapshots
    let (s, body) = get(app.clone(), "/teams/db-migration/snapshots", user).await;
    assert_eq!(s, StatusCode::OK);
    let snaps = body["snapshots"].as_array().unwrap();
    assert_eq!(snaps.len(), 1);
    assert_eq!(snaps[0]["snapshot_id"], snap_id);

    // Delete snapshot
    let (s, body) = delete(app.clone(), &format!("/teams/snapshots/{snap_id}"), user).await;
    assert_eq!(s, StatusCode::OK);
    assert!(body["deleted"].as_bool().unwrap());

    // Confirm gone
    let (s, body) = get(app.clone(), "/teams/db-migration/snapshots", user).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["snapshots"].as_array().unwrap().len(), 0);
}

// ═══════════════════════════════════════════════════════════════════════════
// Scenario 15: Snapshot user isolation
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn scenario_snapshot_user_isolation() {
    let app = build_test_app();

    // Alice creates a team and snapshot
    let (s, _) = post(
        app.clone(),
        "/teams",
        "alice",
        sequential_migration_payload(),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, snap) = post(
        app.clone(),
        "/teams/db-migration/snapshots",
        "alice",
        json!({ "label": "alice snap" }),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let snap_id = snap["snapshot_id"].as_str().unwrap().to_string();

    // Bob creates same-named team
    let (s, _) = post(app.clone(), "/teams", "bob", sequential_migration_payload()).await;
    assert_eq!(s, StatusCode::OK);

    // Bob sees no snapshots for his team
    let (s, body) = get(app.clone(), "/teams/db-migration/snapshots", "bob").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["snapshots"].as_array().unwrap().len(), 0);

    // Bob cannot delete Alice's snapshot
    let (s, _) = delete(app.clone(), &format!("/teams/snapshots/{snap_id}"), "bob").await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // Alice still sees her snapshot
    let (s, body) = get(app.clone(), "/teams/db-migration/snapshots", "alice").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["snapshots"].as_array().unwrap().len(), 1);
}

// ═══════════════════════════════════════════════════════════════════════════
// Scenario 16: Snapshot for non-existent team → 404
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn scenario_snapshot_nonexistent_team_404() {
    let app = build_test_app();
    let (s, _) = get(app.clone(), "/teams/ghost/snapshots", "u1").await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    let (s, _) = post(
        app,
        "/teams/ghost/snapshots",
        "u1",
        json!({ "label": "nope" }),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

// ═══════════════════════════════════════════════════════════════════════════
// Scenario 17: Delete non-existent snapshot → 404
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn scenario_delete_nonexistent_snapshot_404() {
    let app = build_test_app();
    let (s, _) = delete(app, "/teams/snapshots/no-such-snap", "u1").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}
