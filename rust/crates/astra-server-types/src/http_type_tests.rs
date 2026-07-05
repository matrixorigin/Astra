use super::*;
use astra_services::runs::ExecutionBudget;
use axum::http::StatusCode;
use serde_json::{Map, Value, json};

// ── default functions ───────────────────────────────────────────

type DefaultCase<'a> = (&'a str, Box<dyn Fn() -> String>);

#[test]
fn default_functions_return_expected_values() {
    let cases: &[DefaultCase] = &[
        ("days", Box::new(|| default_days().to_string())),
        (
            "admin_scope",
            Box::new(|| default_admin_scope().to_string()),
        ),
        (
            "session_limit",
            Box::new(|| default_session_limit().to_string()),
        ),
        (
            "prompt_optimization_type",
            Box::new(|| default_prompt_optimization_type().to_string()),
        ),
        (
            "feedback_export_format",
            Box::new(|| default_feedback_export_format().to_string()),
        ),
        (
            "admin_audit_limit",
            Box::new(|| default_admin_audit_limit().to_string()),
        ),
        (
            "signal_types",
            Box::new(|| format!("{:?}", default_signal_types())),
        ),
    ];
    let expected = &[
        "7",
        "global",
        "50",
        "compression",
        "jsonl",
        "100",
        r#"["wrong_skill"]"#,
    ];
    for ((label, f), exp) in cases.iter().zip(expected) {
        assert_eq!(f(), *exp, "default_{label}");
    }
}

// ── deserialization with defaults ───────────────────────────────

#[test]
fn deserialization_applies_defaults() {
    // ChatRequest
    let req: ChatRequest = serde_json::from_str(r#"{"message":"hi"}"#).unwrap();
    assert_eq!(req.message, "hi");
    assert!(req.execution_budget.is_none());
    assert!(!req.explain);
    assert!(req.session_id.is_none());
    assert!(req.agent_id.is_none());

    // SessionListQuery
    let q: SessionListQuery = serde_json::from_str("{}").unwrap();
    assert_eq!(q.limit, 50);
    assert!(q.after_updated_at.is_none());
    assert!(q.after_session_id.is_none());

    // RunStreamQuery
    let q: RunStreamQuery = serde_json::from_str("{}").unwrap();
    assert_eq!(q.last_index, 0);

    // RunListQuery
    let q: RunListQuery = serde_json::from_str("{}").unwrap();
    assert_eq!(q.limit, 50);
    assert!(q.after_updated_at.is_none());
    assert!(q.after_run_id.is_none());

    // ChatRouteRequest
    let q: ChatRouteRequest = serde_json::from_str("{}").unwrap();
    assert_eq!(q.query, "");

    // LearningTriggerRequest
    let req: LearningTriggerRequest = serde_json::from_str("{}").unwrap();
    assert_eq!(req.days, 7);
    assert!(!req.force);

    // AdminTokenCreateRequest
    let req: AdminTokenCreateRequest = serde_json::from_str(r#"{"token_type":"api_key"}"#).unwrap();
    assert_eq!(req.scope, "global");

    // AdminAuditListQuery
    let q: AdminAuditListQuery = serde_json::from_str("{}").unwrap();
    assert_eq!(q.limit, 100);

    // PromptOptimizeRequest
    let req: PromptOptimizeRequest = serde_json::from_str(r#"{"agent_id":"a1"}"#).unwrap();
    assert_eq!(req.optimization_type, "compression");

    // FeedbackExportRequest
    let req: FeedbackExportRequest = serde_json::from_str("{}").unwrap();
    assert_eq!(req.format, "jsonl");
}

// ── deserialization with all fields ─────────────────────────────

#[test]
fn chat_request_all_fields() {
    let input = json!({
        "message": "hello",
        "session_id": "s1",
        "agent_id": "a1",
        "selected_model": {"id": "model-gpt-4", "model": "gpt-4"},
        "capability_descriptors": {
            "model_gateway": {
                "id": "moi-model-gateway",
                "type": "model_gateway",
                "transport": "http",
                "endpoint_url": "http://catalog:8081/api/v1/model-gateway",
                "protocol": "openai_responses",
                "metadata": {}
            }
        },
        "llm_token_service": {
            "url": "http://catalog:8081/api/v1/llm-token",
            "timeout_ms": 2500
        },
        "runtime_mcp_bindings": [
            {
                "id": "external_nl2sql",
                "transport": "streamable_http",
                "url": "http://tool-server/api/v1/workspaces/ws-1/mcp/http",
                "headers": {"Authorization": "Bearer runtime-token"}
            }
        ],
        "context": {"key": "value"},
        "execution_budget": {"initial_turns": 10, "hard_turn_limit": 18},
        "explain": true
    });
    let req: ChatRequest = serde_json::from_value(input).unwrap();
    assert_eq!(req.message, "hello");
    assert_eq!(req.session_id.as_deref(), Some("s1"));
    assert_eq!(req.agent_id.as_deref(), Some("a1"));
    assert_eq!(
        req.selected_model
            .as_ref()
            .map(|selected| selected.model.as_str()),
        Some("gpt-4")
    );
    assert_eq!(
        req.selected_model
            .as_ref()
            .and_then(|selected| selected.id.as_deref()),
        Some("model-gpt-4")
    );
    assert_eq!(
        req.capability_descriptors
            .as_ref()
            .and_then(|descriptors| descriptors.model_gateway.as_ref())
            .map(|descriptor| descriptor.endpoint_url.as_str()),
        Some("http://catalog:8081/api/v1/model-gateway")
    );
    assert_eq!(
        req.llm_token_service.as_ref().map(|v| v.url.as_str()),
        Some("http://catalog:8081/api/v1/llm-token")
    );
    assert_eq!(
        req.llm_token_service.as_ref().and_then(|v| v.timeout_ms),
        Some(2500)
    );
    assert_eq!(req.runtime_mcp_bindings.len(), 1);
    assert_eq!(req.runtime_mcp_bindings[0].id, "external_nl2sql");
    assert_eq!(
        req.runtime_mcp_bindings[0]
            .headers
            .get("Authorization")
            .map(String::as_str),
        Some("Bearer runtime-token")
    );
    assert_eq!(
        req.execution_budget,
        Some(ExecutionBudget {
            initial_turns: Some(10),
            hard_turn_limit: Some(18),
        })
    );
    assert!(req.explain);
    let ctx = req.context.unwrap();
    assert_eq!(ctx.get("key").unwrap(), "value");
}

#[test]
fn chat_request_rejects_legacy_top_level_model_field() {
    let result = serde_json::from_str::<ChatRequest>(
        r#"{"message":"hello","selected_model":{"model":"gpt-4"},"model":"gpt-4"}"#,
    );
    assert!(result.is_err(), "legacy top-level model must be rejected");
    let err = result.err().unwrap();
    assert!(
        err.to_string().contains("unknown field `model`"),
        "unexpected error: {err}"
    );
}

#[test]
fn chat_request_rejects_external_auth_body_envelope() {
    let result = serde_json::from_str::<ChatRequest>(
        r#"{"message":"hello","selected_model":{"model":"gpt-4"},"external_auth":{"provider_id":"moi","action":"authorize_request"}}"#,
    );
    assert!(result.is_err(), "external auth must not be in chat body");
    let err = result.err().unwrap();
    assert!(
        err.to_string().contains("unknown field `external_auth`"),
        "unexpected error: {err}"
    );
}

#[test]
fn chat_request_rejects_selected_model_string_form() {
    let result =
        serde_json::from_str::<ChatRequest>(r#"{"message":"hello","selected_model":"gpt-4"}"#);
    assert!(
        result.is_err(),
        "selected_model must be an object, not a string"
    );
}

#[test]
fn chat_request_rejects_selected_model_missing_model() {
    let result = serde_json::from_str::<ChatRequest>(r#"{"message":"hello","selected_model":{}}"#);
    assert!(result.is_err(), "selected_model.model is required");
    let err = result.err().unwrap();
    assert!(
        err.to_string().contains("missing field `model`"),
        "unexpected error: {err}"
    );
}

#[test]
fn chat_request_rejects_selected_model_unknown_field() {
    let result = serde_json::from_str::<ChatRequest>(
        r#"{"message":"hello","selected_model":{"model":"gpt-4","provider":"openai"}}"#,
    );
    assert!(result.is_err(), "selected_model must reject unknown fields");
    let err = result.err().unwrap();
    assert!(
        err.to_string().contains("unknown field `provider`"),
        "unexpected error: {err}"
    );
}

#[test]
fn chat_request_rejects_runtime_auth_credentials_map() {
    let result = serde_json::from_str::<ChatRequest>(
        r#"{"message":"hello","selected_model":{"model":"gpt-4"},"runtime_auth":{"credentials":{"token":"secret"}}}"#,
    );
    assert!(
        result.is_err(),
        "runtime_auth must carry authorization, not credentials"
    );
}

#[test]
fn chat_request_rejects_agent_binding_model_capability_ref() {
    let result = serde_json::from_str::<ChatRequest>(
        r#"{"message":"hello","selected_model":{"model":"gpt-4"},"agent_binding":{"id":"ab_018f05f5-c7dd-7f43-83e6-93d56d9d7391","capability_server_refs":{"mcp":"tools","skills":"skills","models":"models"}}}"#,
    );
    assert!(
        result.is_err(),
        "agent_binding.capability_server_refs.models must be rejected"
    );
    let err = result.err().unwrap();
    assert!(
        err.to_string().contains("unknown field `models`"),
        "unexpected error: {err}"
    );
}

#[test]
fn chat_request_execution_budget_roundtrip() {
    let req: ChatRequest = serde_json::from_str(
        r#"{"message":"budget","execution_budget":{"initial_turns":4,"hard_turn_limit":9}}"#,
    )
    .unwrap();
    assert_eq!(
        req.execution_budget,
        Some(ExecutionBudget {
            initial_turns: Some(4),
            hard_turn_limit: Some(9),
        })
    );
}

#[test]
fn session_create_request_with_metadata() {
    let input = json!({
        "agent_id": "a1",
        "title": "My Session",
        "metadata": {"foo": 42}
    });
    let req: SessionCreateRequest = serde_json::from_value(input).unwrap();
    assert_eq!(req.agent_id.as_deref(), Some("a1"));
    assert_eq!(req.title.as_deref(), Some("My Session"));
    let meta = req.metadata.unwrap();
    assert_eq!(meta.get("foo").unwrap(), 42);
}

#[test]
fn auth_register_request_with_display_name() {
    let input = json!({
        "username": "alice",
        "email": "alice@example.com",
        "password": "secret",
        "display_name": "Alice W."
    });
    let req: AuthRegisterRequest = serde_json::from_value(input).unwrap();
    assert_eq!(req.username, "alice");
    assert_eq!(req.email, "alice@example.com");
    assert_eq!(req.password, "secret");
    assert_eq!(req.display_name.as_deref(), Some("Alice W."));
}

#[test]
fn auth_register_request_without_display_name() {
    let input = json!({
        "username": "bob",
        "email": "bob@example.com",
        "password": "pass"
    });
    let req: AuthRegisterRequest = serde_json::from_value(input).unwrap();
    assert!(req.display_name.is_none());
}

#[test]
fn learning_trigger_request_all_fields() {
    let input = json!({
        "days": 14,
        "force": true,
        "signal_types": ["slow_execution", "high_cost"],
        "weights": {"alpha": 0.5}
    });
    let req: LearningTriggerRequest = serde_json::from_value(input).unwrap();
    assert_eq!(req.days, 14);
    assert!(req.force);
    assert_eq!(req.signal_types, vec!["slow_execution", "high_cost"]);
    let w = req.weights.unwrap();
    assert_eq!(w.get("alpha").unwrap(), &json!(0.5));
}

#[test]
fn session_list_query_all_fields() {
    let input = json!({
        "agent_id": "a1",
        "session_status": "active",
        "limit": 20,
        "after_updated_at": "2024-01-02T00:00:00Z",
        "after_session_id": "s5"
    });
    let q: SessionListQuery = serde_json::from_value(input).unwrap();
    assert_eq!(q.agent_id.as_deref(), Some("a1"));
    assert_eq!(q.session_status.as_deref(), Some("active"));
    assert_eq!(q.limit, 20);
    assert_eq!(q.after_updated_at.as_deref(), Some("2024-01-02T00:00:00Z"));
    assert_eq!(q.after_session_id.as_deref(), Some("s5"));
}

#[test]
fn session_artifact_list_query_cursor_requires_complete_seek_key() {
    let q: SessionArtifactListQuery = serde_json::from_value(json!({
        "artifact_kind": "llm_capture",
        "limit": 10,
        "after_created_at": "2026-10-01T12:34:56.123456",
        "after_artifact_id": "artifact-5"
    }))
    .unwrap();
    let cursor = q.cursor().unwrap().unwrap();
    assert_eq!(cursor.created_at, "2026-10-01T12:34:56.123456");
    assert_eq!(cursor.artifact_id, "artifact-5");

    let missing_artifact_id: SessionArtifactListQuery = serde_json::from_value(json!({
        "after_created_at": "2026-10-01T12:34:56.123456"
    }))
    .unwrap();
    assert_eq!(
        missing_artifact_id.cursor().unwrap_err().0,
        StatusCode::BAD_REQUEST
    );

    let invalid_timestamp: SessionArtifactListQuery = serde_json::from_value(json!({
        "after_created_at": "2026-10-01T12:34:56",
        "after_artifact_id": "artifact-5"
    }))
    .unwrap();
    assert_eq!(
        invalid_timestamp.cursor().unwrap_err().0,
        StatusCode::BAD_REQUEST
    );
}

#[test]
fn run_list_query_cursor_requires_complete_seek_key() {
    let q: RunListQuery = serde_json::from_value(json!({
        "limit": 20,
        "after_updated_at": "2024-01-02T00:00:00.000000",
        "after_run_id": "run-5"
    }))
    .unwrap();
    let cursor = q.cursor().unwrap().unwrap();
    assert_eq!(cursor.updated_at, "2024-01-02T00:00:00.000000");
    assert_eq!(cursor.run_id, "run-5");

    let missing_run_id: RunListQuery = serde_json::from_value(json!({
        "after_updated_at": "2024-01-02T00:00:00.000000"
    }))
    .unwrap();
    assert_eq!(
        missing_run_id.cursor().unwrap_err().0,
        StatusCode::BAD_REQUEST
    );

    let offset_query = serde_json::from_value::<RunListQuery>(json!({
        "offset": 10,
        "after_updated_at": "2024-01-02T00:00:00.000000",
        "after_run_id": "run-5"
    }));
    assert!(offset_query.is_err(), "legacy offset must not deserialize");
}

#[test]
fn admin_token_create_request_all_fields() {
    let input = json!({
        "token_type": "api_key",
        "provider": "openai",
        "scope": "user",
        "scope_id": "u123",
        "token_value": "sk-xxx"
    });
    let req: AdminTokenCreateRequest = serde_json::from_value(input).unwrap();
    assert_eq!(req.scope, "user");
    assert_eq!(req.provider.as_deref(), Some("openai"));
    assert_eq!(req.scope_id.as_deref(), Some("u123"));
    assert_eq!(req.token_value.as_deref(), Some("sk-xxx"));
}

// ── deserialization missing required fields ─────────────────────

#[test]
fn deserialization_errors_on_missing_required_fields() {
    let cases: &[(&str, &str)] = &[
        (
            "ChatRequest/message",
            r#"{"execution_budget":{"initial_turns":3}}"#,
        ),
        ("AuthLoginRequest/username", r#"{"password":"x"}"#),
        ("AuthLoginRequest/password", r#"{"username":"x"}"#),
        (
            "AuthRegisterRequest/email",
            r#"{"username":"u","password":"p"}"#,
        ),
        (
            "AuthRegisterRequest/username",
            r#"{"email":"e@e.com","password":"p"}"#,
        ),
        ("PromptOptimizeRequest/agent_id", "{}"),
        ("AdminUserRoleRequest/role", r#"{"username":"u"}"#),
    ];
    for (_label, json) in cases {
        let err = serde_json::from_str::<serde_json::Value>(json).unwrap();
        // Each type has a different schema; just verify the raw parse succeeds
        // and the targeted type fails.
        let _ = err;
    }
    // Typed deserialization failures — each type requires specific missing field
    assert!(
        serde_json::from_str::<ChatRequest>(
            r#"{"execution_budget":{"initial_turns":3,"hard_turn_limit":7}}"#
        )
        .is_err()
    );
    assert!(serde_json::from_str::<AuthLoginRequest>(r#"{"password":"x"}"#).is_err());
    assert!(serde_json::from_str::<AuthLoginRequest>(r#"{"username":"x"}"#).is_err());
    assert!(
        serde_json::from_str::<AuthRegisterRequest>(r#"{"username":"u","password":"p"}"#).is_err()
    );
    assert!(
        serde_json::from_str::<AuthRegisterRequest>(r#"{"email":"e@e.com","password":"p"}"#)
            .is_err()
    );
    assert!(serde_json::from_str::<PromptOptimizeRequest>("{}").is_err());
    assert!(serde_json::from_str::<AdminUserRoleRequest>(r#"{"username":"u"}"#).is_err());
}

// ── serialization of response types ─────────────────────────────

#[test]
fn root_response_serializes() {
    let resp = RootResponse {
        name: "astra".into(),
        version: "1.0.0".into(),
        docs: "https://docs.example.com".into(),
    };
    let v = serde_json::to_value(&resp).unwrap();
    assert_eq!(v["name"], "astra");
    assert_eq!(v["version"], "1.0.0");
    assert_eq!(v["docs"], "https://docs.example.com");
}

#[test]
fn health_response_serializes() {
    let resp = HealthResponse {
        status: "ok".into(),
        database: "connected".into(),
        persist_ok: 42,
        persist_fail: 1,
    };
    let v = serde_json::to_value(&resp).unwrap();
    assert_eq!(v["status"], "ok");
    assert_eq!(v["database"], "connected");
    assert_eq!(v["persist_ok"], 42);
    assert_eq!(v["persist_fail"], 1);
}

#[test]
fn auth_user_response_serializes_with_none_display_name() {
    let resp = AuthUserResponse {
        user_id: "u1".into(),
        username: "alice".into(),
        email: "a@b.com".into(),
        display_name: None,
    };
    let v = serde_json::to_value(&resp).unwrap();
    assert_eq!(v["user_id"], "u1");
    assert_eq!(v["display_name"], Value::Null);
}

#[test]
fn auth_user_response_serializes_with_some_display_name() {
    let resp = AuthUserResponse {
        user_id: "u1".into(),
        username: "alice".into(),
        email: "a@b.com".into(),
        display_name: Some("Alice".into()),
    };
    let v = serde_json::to_value(&resp).unwrap();
    assert_eq!(v["display_name"], "Alice");
}

#[test]
fn session_response_serializes_all_fields() {
    let mut meta = Map::new();
    meta.insert("k".into(), json!("v"));
    let resp = SessionResponse {
        session_id: "s1".into(),
        user_id: "u1".into(),
        agent_id: Some("a1".into()),
        title: Some("T".into()),
        metadata: meta,
        status: "active".into(),
        event_count: 5,
        created_at: "2024-01-01T00:00:00Z".into(),
        updated_at: Some("2024-01-02T00:00:00Z".into()),
        ended_at: None,
    };
    let v = serde_json::to_value(&resp).unwrap();
    assert_eq!(v["session_id"], "s1");
    assert_eq!(v["agent_id"], "a1");
    assert_eq!(v["metadata"]["k"], "v");
    assert_eq!(v["event_count"], 5);
    assert_eq!(v["ended_at"], Value::Null);
}

#[test]
fn chat_response_serializes_with_explain() {
    let resp = ChatResponse {
        session_id: "s1".into(),
        run_id: "r1".into(),
        status: "running".into(),
        explain: Some(json!({"routing": "skill_a"})),
    };
    let v = serde_json::to_value(&resp).unwrap();
    assert_eq!(v["explain"]["routing"], "skill_a");
}

#[test]
fn run_status_response_serializes() {
    let resp = RunStatusResponse {
        run_id: "r1".into(),
        session_id: "s1".into(),
        status: "waiting".into(),
        waiting_for: Some("tool_call".into()),
        events_count: 3,
        workspace: Some(json!({"kind": "server_sandbox"})),
        executor: Some(json!({"kind": "server_local"})),
        transport: Some("server_local".into()),
    };
    let v = serde_json::to_value(&resp).unwrap();
    assert_eq!(v["waiting_for"], "tool_call");
    assert_eq!(v["events_count"], 3);
    assert_eq!(v["workspace"]["kind"], "server_sandbox");
    assert_eq!(v["executor"]["kind"], "server_local");
    assert_eq!(v["transport"], "server_local");
}

#[test]
fn cancel_run_response_serializes() {
    let resp = CancelRunResponse {
        run_id: "r1".into(),
        status: "cancelled".into(),
    };
    let v = serde_json::to_value(&resp).unwrap();
    assert_eq!(v["run_id"], "r1");
    assert_eq!(v["status"], "cancelled");
}

#[test]
fn auth_token_response_serializes() {
    let resp = AuthTokenResponse {
        access_token: "at".into(),
        refresh_token: "rt".into(),
        token_type: "Bearer".into(),
        expires_in: 3600,
    };
    let v = serde_json::to_value(&resp).unwrap();
    assert_eq!(v["access_token"], "at");
    assert_eq!(v["token_type"], "Bearer");
    assert_eq!(v["expires_in"], 3600);
}

#[test]
fn auth_register_response_serializes() {
    let resp = AuthRegisterResponse {
        user_id: "u1".into(),
        username: "alice".into(),
        email: "a@b.com".into(),
        display_name: Some("Alice".into()),
        roles: vec!["user".into()],
        is_admin: false,
        access_token: "at".into(),
        refresh_token: "rt".into(),
        token_type: "Bearer".into(),
        expires_in: 3600,
    };
    let v = serde_json::to_value(&resp).unwrap();
    assert_eq!(v["user_id"], "u1");
    assert_eq!(v["access_token"], "at");
    assert_eq!(v["display_name"], "Alice");
}

#[test]
fn auth_logout_response_serializes() {
    let resp = AuthLogoutResponse {
        message: "bye".into(),
    };
    let v = serde_json::to_value(&resp).unwrap();
    assert_eq!(v["message"], "bye");
}

#[test]
fn session_list_response_serializes() {
    let resp = SessionListResponse {
        sessions: vec![],
        total: Some(0),
        limit: 50,
        next_cursor: None,
    };
    let v = serde_json::to_value(&resp).unwrap();
    assert_eq!(v["sessions"], json!([]));
    assert_eq!(v["total"], 0);
    assert!(v["next_cursor"].is_null());

    let resp = SessionListResponse {
        sessions: vec![],
        total: None,
        limit: 50,
        next_cursor: None,
    };
    let v = serde_json::to_value(&resp).unwrap();
    assert!(v["total"].is_null());
}

#[test]
fn admin_token_response_serializes() {
    let resp = AdminTokenResponse {
        token_id: "t1".into(),
        token_type: "api_key".into(),
        provider: Some("openai".into()),
        scope: "global".into(),
        scope_id: None,
        created_at: "2024-01-01T00:00:00Z".into(),
    };
    let v = serde_json::to_value(&resp).unwrap();
    assert_eq!(v["token_id"], "t1");
    assert_eq!(v["provider"], "openai");
    assert_eq!(v["scope_id"], Value::Null);
}

#[test]
fn admin_audit_response_serializes() {
    let resp = AdminAuditResponse {
        log_id: "l1".into(),
        user_id: "u1".into(),
        action: "login".into(),
        resource_type: "session".into(),
        resource_id: Some("s1".into()),
        timestamp: "2024-01-01T00:00:00Z".into(),
        details: Some(json!({"ip": "127.0.0.1"})),
    };
    let v = serde_json::to_value(&resp).unwrap();
    assert_eq!(v["details"]["ip"], "127.0.0.1");
}

#[test]
fn admin_init_response_serializes() {
    let resp = AdminInitResponse {
        message: "done".into(),
        tables_created: 5,
    };
    let v = serde_json::to_value(&resp).unwrap();
    assert_eq!(v["tables_created"], 5);
}

#[test]
fn admin_feedback_stats_response_serializes() {
    let mut by_type = Map::new();
    by_type.insert("thumbs_up".into(), json!(10));
    let resp = AdminFeedbackStatsResponse {
        total_feedback: 20,
        positive_feedback: 15,
        negative_feedback: 5,
        avg_rating: Some(4.2),
        feedback_by_type: by_type,
    };
    let v = serde_json::to_value(&resp).unwrap();
    assert_eq!(v["total_feedback"], 20);
    assert_eq!(v["avg_rating"], 4.2);
    assert_eq!(v["feedback_by_type"]["thumbs_up"], 10);
}

#[test]
fn admin_user_role_response_serializes() {
    let resp = AdminUserRoleResponse {
        username: "alice".into(),
        role_name: "admin".into(),
        message: "ok".into(),
    };
    let v = serde_json::to_value(&resp).unwrap();
    assert_eq!(v["role_name"], "admin");
}

#[test]
fn prompt_optimize_response_serializes() {
    let resp = PromptOptimizeResponse {
        job_id: "j1".into(),
        status: "queued",
        message: "started".into(),
    };
    let v = serde_json::to_value(&resp).unwrap();
    assert_eq!(v["status"], "queued");
}

#[test]
fn feedback_export_response_serializes() {
    let resp = FeedbackExportResponse {
        job_id: "j1".into(),
        status: "ready",
        download_url: Some("https://example.com/f.jsonl".into()),
    };
    let v = serde_json::to_value(&resp).unwrap();
    assert_eq!(v["download_url"], "https://example.com/f.jsonl");
}

// ── From trait implementations ──────────────────────────────────

#[test]
fn admin_token_record_to_response() {
    let record = AdminTokenRecord {
        token_id: "t1".into(),
        token_type: "api_key".into(),
        provider: Some("openai".into()),
        scope: "global".into(),
        scope_id: None,
        created_at: "2024-01-01T00:00:00Z".into(),
    };
    let resp: AdminTokenResponse = record.into();
    assert_eq!(resp.token_id, "t1");
    assert_eq!(resp.token_type, "api_key");
    assert_eq!(resp.provider.as_deref(), Some("openai"));
    assert_eq!(resp.scope, "global");
    assert!(resp.scope_id.is_none());
    assert_eq!(resp.created_at, "2024-01-01T00:00:00Z");
}

#[test]
fn admin_audit_record_to_response() {
    // with details
    let details = json!({"reason": "suspicious"});
    let record = AdminAuditRecord {
        log_id: "l1".into(),
        user_id: "u1".into(),
        action: "delete".into(),
        resource_type: "token".into(),
        resource_id: Some("t1".into()),
        timestamp: "2024-06-01T12:00:00Z".into(),
        details: Some(details.clone()),
    };
    let resp: AdminAuditResponse = record.into();
    assert_eq!(resp.log_id, "l1");
    assert_eq!(resp.resource_id.as_deref(), Some("t1"));
    assert_eq!(resp.details, Some(details));

    // without details
    let record = AdminAuditRecord {
        log_id: "l2".into(),
        user_id: "u2".into(),
        action: "login".into(),
        resource_type: "user".into(),
        resource_id: None,
        timestamp: "2024-06-01T12:00:00Z".into(),
        details: None,
    };
    let resp: AdminAuditResponse = record.into();
    assert!(resp.resource_id.is_none());
    assert!(resp.details.is_none());
}

#[test]
fn admin_feedback_stats_record_to_response() {
    // with avg_rating
    let mut by_type = Map::new();
    by_type.insert("rating".into(), json!(10));
    let record = AdminFeedbackStatsRecord {
        total_feedback: 100,
        positive_feedback: 80,
        negative_feedback: 20,
        avg_rating: Some(4.5),
        feedback_by_type: by_type.clone(),
    };
    let resp: AdminFeedbackStatsResponse = record.into();
    assert_eq!(resp.total_feedback, 100);
    assert_eq!(resp.avg_rating, Some(4.5));

    // none avg_rating
    let record = AdminFeedbackStatsRecord {
        total_feedback: 0,
        positive_feedback: 0,
        negative_feedback: 0,
        avg_rating: None,
        feedback_by_type: Map::new(),
    };
    let resp: AdminFeedbackStatsResponse = record.into();
    assert!(resp.avg_rating.is_none());
}

#[test]
fn admin_init_record_to_response() {
    let record = AdminInitRecord {
        message: "Initialized".into(),
        tables_created: 12,
    };
    let resp: AdminInitResponse = record.into();
    assert_eq!(resp.message, "Initialized");
    assert_eq!(resp.tables_created, 12);
}

#[test]
fn admin_user_role_record_to_response() {
    let record = AdminUserRoleRecord {
        username: "bob".into(),
        role_name: "editor".into(),
        message: "Role assigned".into(),
    };
    let resp: AdminUserRoleResponse = record.into();
    assert_eq!(resp.username, "bob");
    assert_eq!(resp.role_name, "editor");
    assert_eq!(resp.message, "Role assigned");
}

#[test]
fn session_record_to_response() {
    // all fields present
    let mut meta = Map::new();
    meta.insert("env".into(), json!("prod"));
    let record = SessionRecord {
        session_id: "s1".into(),
        user_id: "u1".into(),
        agent_id: Some("a1".into()),
        title: Some("Test Session".into()),
        metadata: meta.clone(),
        status: "active".into(),
        event_count: 42,
        created_at: "2024-01-01T00:00:00Z".into(),
        updated_at: Some("2024-01-02T00:00:00Z".into()),
        ended_at: Some("2024-01-03T00:00:00Z".into()),
    };
    let resp: SessionResponse = record.into();
    assert_eq!(resp.session_id, "s1");
    assert_eq!(resp.agent_id.as_deref(), Some("a1"));
    assert_eq!(resp.title.as_deref(), Some("Test Session"));
    assert_eq!(resp.event_count, 42);
    assert_eq!(resp.updated_at.as_deref(), Some("2024-01-02T00:00:00Z"));
    assert_eq!(resp.ended_at.as_deref(), Some("2024-01-03T00:00:00Z"));

    // optional fields None
    let record = SessionRecord {
        session_id: "s2".into(),
        user_id: "u2".into(),
        agent_id: None,
        title: None,
        metadata: Map::new(),
        status: "ended".into(),
        event_count: 0,
        created_at: "2024-01-01T00:00:00Z".into(),
        updated_at: None,
        ended_at: None,
    };
    let resp: SessionResponse = record.into();
    assert!(resp.agent_id.is_none());
    assert!(resp.title.is_none());
    assert!(resp.updated_at.is_none());
    assert!(resp.ended_at.is_none());
}

#[test]
fn session_list_record_to_response() {
    // non-empty
    let record = SessionListRecord {
        sessions: vec![SessionRecord {
            session_id: "s1".into(),
            user_id: "u1".into(),
            agent_id: None,
            title: None,
            metadata: Map::new(),
            status: "active".into(),
            event_count: 1,
            created_at: "2024-01-01T00:00:00Z".into(),
            updated_at: None,
            ended_at: None,
        }],
        total: Some(1),
        limit: 50,
        next_cursor: Some(SessionListCursor {
            updated_at: "2024-01-01T00:00:00Z".into(),
            session_id: "s1".into(),
        }),
    };
    let resp: SessionListResponse = record.into();
    assert_eq!(resp.sessions.len(), 1);
    assert_eq!(resp.total, Some(1));
    assert_eq!(resp.limit, 50);
    assert_eq!(
        resp.next_cursor
            .as_ref()
            .map(|cursor| cursor.session_id.as_str()),
        Some("s1")
    );

    // empty
    let record = SessionListRecord {
        sessions: vec![],
        total: None,
        limit: 20,
        next_cursor: None,
    };
    let resp: SessionListResponse = record.into();
    assert!(resp.sessions.is_empty());
    assert_eq!(resp.total, None);
    assert_eq!(resp.limit, 20);
    assert!(resp.next_cursor.is_none());
}

#[test]
fn run_list_record_to_response_preserves_optional_total_and_cursor() {
    let record = RunListRecord {
        runs: vec![RunStatusRecord {
            run_id: "run-1".into(),
            session_id: "session-1".into(),
            status: "running".into(),
            waiting_for: None,
            events_count: 3,
            workspace: None,
            executor: None,
            transport: None,
        }],
        total: None,
        limit: 20,
        next_cursor: Some(RunListCursor {
            updated_at: "2024-01-02T00:00:00.000000".into(),
            run_id: "run-1".into(),
        }),
    };

    let resp: RunListResponse = record.into();
    assert_eq!(resp.runs.len(), 1);
    assert_eq!(resp.total, None);
    assert_eq!(resp.limit, 20);
    assert_eq!(
        resp.next_cursor
            .as_ref()
            .map(|cursor| cursor.run_id.as_str()),
        Some("run-1")
    );

    let v = serde_json::to_value(&resp).unwrap();
    assert!(v["total"].is_null());
    assert!(v.get("offset").is_none());
    assert_eq!(v["next_cursor"]["run_id"], "run-1");
}

#[test]
fn chat_run_record_to_response() {
    // with explain
    let explain = json!({"candidates": [{"skill": "math", "score": 0.9}]});
    let record = ChatRunRecord {
        session_id: "s1".into(),
        run_id: "r1".into(),
        status: "completed".into(),
        explain: Some(explain.clone()),
    };
    let resp: ChatResponse = record.into();
    assert_eq!(resp.session_id, "s1");
    assert_eq!(resp.status, "completed");
    assert_eq!(resp.explain, Some(explain));

    // without explain
    let record = ChatRunRecord {
        session_id: "s1".into(),
        run_id: "r1".into(),
        status: "running".into(),
        explain: None,
    };
    let resp: ChatResponse = record.into();
    assert!(resp.explain.is_none());
}

#[test]
fn run_status_record_to_response() {
    // with waiting_for
    let record = RunStatusRecord {
        run_id: "r1".into(),
        session_id: "s1".into(),
        status: "waiting".into(),
        waiting_for: Some("tool_call".into()),
        events_count: 7,
        workspace: None,
        executor: None,
        transport: None,
    };
    let resp: RunStatusResponse = record.into();
    assert_eq!(resp.status, "waiting");
    assert_eq!(resp.waiting_for.as_deref(), Some("tool_call"));
    assert_eq!(resp.events_count, 7);

    // without waiting_for
    let record = RunStatusRecord {
        run_id: "r2".into(),
        session_id: "s2".into(),
        status: "completed".into(),
        waiting_for: None,
        events_count: 10,
        workspace: None,
        executor: None,
        transport: None,
    };
    let resp: RunStatusResponse = record.into();
    assert!(resp.waiting_for.is_none());
}

#[test]
fn cancel_run_record_to_response() {
    let record = CancelRunRecord {
        run_id: "r1".into(),
        status: "cancelled".into(),
    };
    let resp: CancelRunResponse = record.into();
    assert_eq!(resp.run_id, "r1");
    assert_eq!(resp.status, "cancelled");
}

#[test]
fn run_mutation_record_to_response() {
    let record = RunMutationRecord {
        run_id: "r1".into(),
        status: "paused".into(),
        previous_status: "running".into(),
    };
    let resp: RunMutationResponse = record.into();
    assert_eq!(resp.run_id, "r1");
    assert_eq!(resp.status, "paused");
    assert_eq!(resp.previous_status, "running");
}

#[test]
fn auth_user_record_to_response() {
    // with display_name
    let record = AuthUserRecord {
        user_id: "u1".into(),
        username: "alice".into(),
        email: "alice@example.com".into(),
        display_name: Some("Alice W.".into()),
    };
    let resp: AuthUserResponse = record.into();
    assert_eq!(resp.username, "alice");
    assert_eq!(resp.display_name.as_deref(), Some("Alice W."));

    // without display_name
    let record = AuthUserRecord {
        user_id: "u2".into(),
        username: "bob".into(),
        email: "bob@example.com".into(),
        display_name: None,
    };
    let resp: AuthUserResponse = record.into();
    assert!(resp.display_name.is_none());
}

#[test]
fn auth_token_record_to_response() {
    let record = AuthTokenRecord {
        access_token: "access123".into(),
        refresh_token: "refresh456".into(),
        token_type: "Bearer".into(),
        expires_in: 7200,
    };
    let resp: AuthTokenResponse = record.into();
    assert_eq!(resp.access_token, "access123");
    assert_eq!(resp.refresh_token, "refresh456");
    assert_eq!(resp.token_type, "Bearer");
    assert_eq!(resp.expires_in, 7200);
}

// ── chat_request_into_data ──────────────────────────────────────

#[test]
fn chat_request_into_data_maps_all_fields() {
    let mut ctx = Map::new();
    ctx.insert("tool".into(), json!("calc"));
    let req = ChatRequest {
        message: "hello".into(),
        parts: vec![json!({"type": "text", "text": "hello"})],
        attachments: vec![json!({"id": "att-1", "kind": "file"})],
        runtime_system_prompt: Some("Runtime SQL scope db_name: retail.".into()),
        session_id: Some("s1".into()),
        agent_id: Some("a1".into()),
        selected_model: Some(astra_services::runs::SelectedModelRequest {
            id: None,
            model: "gpt-4".into(),
            gateway: None,
        }),
        capability_descriptors: None,
        agent_binding: None,
        runtime_auth: None,
        runtime_profile: None,
        workspace_binding: None,
        executor_binding: None,
        llm_token_service: Some(astra_services::LlmTokenServiceRequest {
            url: "http://catalog:8081/api/v1/llm-token".into(),
            timeout_ms: Some(2500),
        }),
        skill_search: Some(astra_core::SkillSearchSettings::default()),
        allow_skills: None,
        allow_skill_sources: None,
        allow_tools: None,
        runtime_mcp_bindings: vec![astra_services::runs::RuntimeMcpBindingRequest {
            id: "external_nl2sql".into(),
            transport: "streamable_http".into(),
            url: "http://tool-server/api/v1/workspaces/ws-1/mcp/http".into(),
            auth_token: None,
            headers: std::collections::HashMap::from([(
                "Authorization".to_string(),
                "Bearer runtime-token".to_string(),
            )]),
        }],
        mcp_binding_ids: None,
        context: Some(ctx.clone()),
        edge_executor_id: Some("edge-1".into()),
        capabilities: vec!["bash".into(), "fs".into()],
        execution_budget: Some(ExecutionBudget {
            initial_turns: Some(3),
            hard_turn_limit: Some(7),
        }),
        explain: true,
        interaction_mode: Some(astra_services::runs::RequestedTurnInteractionMode::Auto),
        interactive_client: true,
        plan_subtask_id: None,
        is_plan_subtask: None,
    };
    let data = chat_request_into_data(req);
    assert_eq!(data.message, "hello");
    assert_eq!(data.parts, vec![json!({"type": "text", "text": "hello"})]);
    assert_eq!(
        data.attachments,
        vec![json!({"id": "att-1", "kind": "file"})]
    );
    assert_eq!(
        data.runtime_system_prompt.as_deref(),
        Some("Runtime SQL scope db_name: retail.")
    );
    assert_eq!(data.session_id.as_deref(), Some("s1"));
    assert_eq!(data.agent_id.as_deref(), Some("a1"));
    assert_eq!(data.model.as_deref(), Some("gpt-4"));
    assert_eq!(
        data.llm_token_service.as_ref().map(|v| v.url.as_str()),
        Some("http://catalog:8081/api/v1/llm-token")
    );
    assert_eq!(
        data.llm_token_service.as_ref().and_then(|v| v.timeout_ms),
        Some(2500)
    );
    assert_eq!(
        data.skill_search,
        Some(astra_core::SkillSearchSettings::default())
    );
    assert_eq!(data.runtime_mcp_bindings.len(), 1);
    assert_eq!(data.runtime_mcp_bindings[0].id, "external_nl2sql");
    assert!(data.mcp_binding_ids.is_none());
    assert_eq!(data.context, Some(ctx));
    assert_eq!(data.edge_executor_id.as_deref(), Some("edge-1"));
    assert_eq!(data.capabilities, vec!["bash", "fs"]);
    assert_eq!(
        data.execution_budget,
        Some(ExecutionBudget {
            initial_turns: Some(3),
            hard_turn_limit: Some(7),
        })
    );
    assert!(data.explain);
    assert_eq!(
        data.interaction_mode,
        Some(astra_services::runs::RequestedTurnInteractionMode::Auto)
    );
    assert!(data.interactive_client);
}

#[test]
fn chat_request_into_data_maps_defaults() {
    let req: ChatRequest = serde_json::from_str(r#"{"message":"test"}"#).unwrap();
    let data = chat_request_into_data(req);
    assert_eq!(data.message, "test");
    assert!(data.parts.is_empty());
    assert!(data.attachments.is_empty());
    assert!(data.runtime_system_prompt.is_none());
    assert!(data.session_id.is_none());
    assert!(data.agent_id.is_none());
    assert!(data.model.is_none());
    assert!(data.llm_token_service.is_none());
    assert!(data.runtime_mcp_bindings.is_empty());
    assert!(data.mcp_binding_ids.is_none());
    assert!(data.context.is_none());
    assert!(data.edge_executor_id.is_none());
    assert!(data.capabilities.is_empty());
    assert!(data.execution_budget.is_none());
    assert!(!data.explain);
    assert!(data.interaction_mode.is_none());
    assert!(!data.interactive_client);
}

#[test]
fn chat_request_into_data_merges_plan_subtask_into_context() {
    let req = ChatRequest {
        message: "do step".into(),
        parts: Vec::new(),
        attachments: Vec::new(),
        runtime_system_prompt: None,
        session_id: None,
        agent_id: None,
        selected_model: None,
        capability_descriptors: None,
        agent_binding: None,
        runtime_auth: None,
        runtime_profile: None,
        workspace_binding: None,
        executor_binding: None,
        llm_token_service: None,
        skill_search: None,
        allow_skills: None,
        allow_skill_sources: None,
        allow_tools: None,
        runtime_mcp_bindings: Vec::new(),
        mcp_binding_ids: None,
        context: None,
        edge_executor_id: None,
        capabilities: Vec::new(),
        execution_budget: None,
        explain: false,
        interaction_mode: None,
        interactive_client: false,
        plan_subtask_id: Some("sub-42".into()),
        is_plan_subtask: Some(true),
    };
    let data = chat_request_into_data(req);
    let ctx = data.context.unwrap();
    assert_eq!(
        ctx.get("plan_subtask_id").and_then(|v| v.as_str()),
        Some("sub-42")
    );
    assert_eq!(
        ctx.get("is_plan_subtask").and_then(|v| v.as_bool()),
        Some(true)
    );
}
