use super::*;
use astra_services::runs::{
    ExecutionBudget, ExecutionPolicyRequest, ExecutionTimeBudget, SkillAutoRouteExecutionPolicy,
    TurnIntentExecutionPolicy,
};
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
    assert!(req.execution_time_budget.is_none());
    assert!(!req.explain);
    assert!(req.session_id.is_none());
    assert!(req.agent_id.is_none());
    assert_eq!(req.execution_policy, ExecutionPolicyRequest::default());

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

#[test]
fn reauthentication_request_uses_closed_structured_purposes() {
    let trust: AuthReauthenticateRequest = serde_json::from_value(json!({
        "password": "correct horse battery staple",
        "purpose": "device_trust"
    }))
    .unwrap();
    assert_eq!(trust.purpose, ReauthenticationPurpose::DeviceTrust);

    let reenroll: AuthReauthenticateRequest = serde_json::from_value(json!({
        "password": "correct horse battery staple",
        "purpose": "device_reenroll"
    }))
    .unwrap();
    assert_eq!(reenroll.purpose, ReauthenticationPurpose::DeviceReenroll);

    for invalid in [
        json!({"password": "secret", "purpose": "trust this laptop"}),
        json!({"password": "secret", "purpose": true}),
        json!({"password": "secret", "purpose": "device_trust", "confirmed": true}),
    ] {
        assert!(
            serde_json::from_value::<AuthReauthenticateRequest>(invalid).is_err(),
            "reauthentication authority must not accept free-form intent or boolean self-attestation"
        );
    }
}

// ── deserialization with all fields ─────────────────────────────

#[test]
fn chat_request_all_fields() {
    let input = json!({
        "message": "hello",
        "session_id": "s1",
        "agent_id": "a1",
        "model_selection": {"offering_id": "model-gpt-4"},
        "capability_descriptors": {
            "model_gateway": {
                "id": "moi-model-gateway",
                "type": "model_gateway",
                "transport": "http",
                "endpoint_url": "http://catalog:8081/api/v1/model-gateway",
                "protocol": "openai_chat_completions",
                "model_context_window": 128000,
                "metadata": {}
            }
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
        "stable_runtime_system_prompt": "Extension instructions take precedence on semantic overlap.",
        "agent_bindings": [
            {
                "id": "binding-foundation",
                "capability_server_refs": {"mcp": "tools", "skills": "skills"}
            },
            {
                "id": "binding-extension",
                "capability_server_refs": {"mcp": "tools", "skills": "skills"}
            }
        ],
        "execution_budget": {"initial_turns": 10, "hard_turn_limit": 18},
        "execution_policy": {
            "turn_intent": "fixed_default",
            "skill_auto_route": "disabled"
        },
        "explain": true
    });
    let req: ChatRequest = serde_json::from_value(input).unwrap();
    assert_eq!(req.message, "hello");
    assert_eq!(req.session_id.as_deref(), Some("s1"));
    assert_eq!(req.agent_id.as_deref(), Some("a1"));
    assert_eq!(
        req.model_selection
            .as_ref()
            .map(|selection| selection.offering_id.as_str()),
        Some("model-gpt-4")
    );
    assert_eq!(
        req.capability_descriptors
            .as_ref()
            .and_then(|descriptors| descriptors.model_gateway.as_ref())
            .map(|descriptor| descriptor.endpoint_url.as_str()),
        Some("http://catalog:8081/api/v1/model-gateway")
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
        req.stable_runtime_system_prompt.as_deref(),
        Some("Extension instructions take precedence on semantic overlap.")
    );
    assert_eq!(req.agent_bindings.len(), 2);
    assert_eq!(req.agent_bindings[0].id, "binding-foundation");
    assert_eq!(req.agent_bindings[0].capability_server_refs.mcp, "tools");
    assert_eq!(
        req.agent_bindings[0].capability_server_refs.skills,
        "skills"
    );
    assert_eq!(req.agent_bindings[1].id, "binding-extension");
    assert_eq!(
        req.execution_budget,
        Some(ExecutionBudget {
            initial_turns: Some(10),
            hard_turn_limit: Some(18),
        })
    );
    assert!(req.explain);
    assert_eq!(
        req.execution_policy.turn_intent,
        TurnIntentExecutionPolicy::FixedDefault
    );
    assert_eq!(
        req.execution_policy.skill_auto_route,
        SkillAutoRouteExecutionPolicy::Disabled
    );
    let ctx = req.context.unwrap();
    assert_eq!(ctx.get("key").unwrap(), "value");
}

#[test]
fn chat_request_rejects_legacy_top_level_model_field() {
    let result = serde_json::from_str::<ChatRequest>(
        r#"{"message":"hello","model_selection":{"offering_id":"offer-gpt-4"},"model":"gpt-4"}"#,
    );
    assert!(result.is_err(), "legacy top-level model must be rejected");
    let err = result.err().unwrap();
    assert!(
        err.to_string().contains("unknown field `model`"),
        "unexpected error: {err}"
    );
}

#[test]
fn chat_request_rejects_model_selection_string_form() {
    let result =
        serde_json::from_str::<ChatRequest>(r#"{"message":"hello","model_selection":"gpt-4"}"#);
    assert!(
        result.is_err(),
        "model_selection must be an object, not a string"
    );
}

#[test]
fn chat_request_rejects_model_selection_missing_offering_id() {
    let result = serde_json::from_str::<ChatRequest>(r#"{"message":"hello","model_selection":{}}"#);
    assert!(result.is_err(), "model_selection.offering_id is required");
    let err = result.err().unwrap();
    assert!(
        err.to_string().contains("missing field `offering_id`"),
        "unexpected error: {err}"
    );
}

#[test]
fn chat_request_rejects_model_selection_route_fields() {
    let result = serde_json::from_str::<ChatRequest>(
        r#"{"message":"hello","model_selection":{"offering_id":"offer-gpt-4","gateway":"primary"}}"#,
    );
    assert!(
        result.is_err(),
        "model_selection must reject route authority"
    );
    let err = result.err().unwrap();
    assert!(
        err.to_string().contains("unknown field `gateway`"),
        "unexpected error: {err}"
    );
}

#[test]
fn chat_request_rejects_runtime_auth_credentials_map() {
    let result = serde_json::from_str::<ChatRequest>(
        r#"{"message":"hello","model_selection":{"offering_id":"offer-gpt-4"},"runtime_auth":{"credentials":{"token":"secret"}}}"#,
    );
    assert!(
        result.is_err(),
        "runtime_auth must carry authorization, not credentials"
    );
}

#[test]
fn chat_request_agent_binding_capability_refs_are_strict() {
    let result = serde_json::from_str::<ChatRequest>(
        r#"{"message":"hello","model_selection":{"offering_id":"offer-gpt-4"},"agent_binding":{"id":"ab_018f05f5-c7dd-7f43-83e6-93d56d9d7391","capability_server_refs":{"mcp":"tools","skills":"skills","models":"models"}}}"#,
    );
    assert!(
        result.is_err(),
        "agent_binding.capability_server_refs must reject undeclared server kinds"
    );
    let err = result.err().unwrap();
    assert!(
        err.to_string().contains("unknown field `models`"),
        "unexpected error: {err}"
    );

    let request = serde_json::from_str::<ChatRequest>(
        r#"{"message":"hello","model_selection":{"offering_id":"offer-gpt-4"},"agent_binding":{"id":"ab_018f05f5-c7dd-7f43-83e6-93d56d9d7391","capability_server_refs":{"mcp":"tools","skills":"skills"}}}"#,
    )
    .expect("logical MCP and skill server refs are the canonical runtime binding shape");
    let binding = request.agent_binding.expect("agent binding");
    assert_eq!(binding.capability_server_refs.mcp, "tools");
    assert_eq!(binding.capability_server_refs.skills, "skills");
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
fn chat_request_execution_time_budget_roundtrip_is_independent_from_round_budget() {
    let req: ChatRequest = serde_json::from_str(
        r#"{"message":"budget","execution_time_budget":{"remaining_seconds":37}}"#,
    )
    .unwrap();
    assert_eq!(
        req.execution_time_budget,
        Some(ExecutionTimeBudget {
            remaining_seconds: 37,
        })
    );
    assert!(
        req.execution_budget.is_none(),
        "wall time must not opt into or expand the round budget"
    );
}

#[test]
fn chat_request_execution_policy_rejects_unknown_fields() {
    let result = serde_json::from_str::<ChatRequest>(
        r#"{"message":"hello","execution_policy":{"turn_intent":"fixed_default","extra":true}}"#,
    );
    assert!(
        result.is_err(),
        "execution_policy must remain a closed contract"
    );
    assert!(
        result
            .err()
            .unwrap()
            .to_string()
            .contains("unknown field `extra`"),
        "unexpected error"
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
        memoria: "connected".into(),
        persist_ok: 42,
        persist_fail: 1,
        interaction_api_major: AGENT_INTERACTION_API_MAJOR.to_string(),
        build_git_sha: "a".repeat(40),
    };
    let v = serde_json::to_value(&resp).unwrap();
    assert_eq!(v["status"], "ok");
    assert_eq!(v["database"], "connected");
    assert_eq!(v["memoria"], "connected");
    assert_eq!(v["persist_ok"], 42);
    assert_eq!(v["persist_fail"], 1);
    assert_eq!(v["interaction_api_major"], AGENT_INTERACTION_API_MAJOR);
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
        parent_run_id: None,
        root_run_id: Some("r1".into()),
        depth: 0,
        status: "waiting".into(),
        waiting_for: Some("tool_call".into()),
        events_count: 3,
        workspace: Some(json!({"kind": "server_sandbox"})),
        executor: Some(json!({"kind": "server_local"})),
        transport: Some("server_local".into()),
        accounting: None,
    };
    let v = serde_json::to_value(&resp).unwrap();
    assert_eq!(v["waiting_for"], "tool_call");
    assert!(v["parent_run_id"].is_null());
    assert_eq!(v["root_run_id"], "r1");
    assert_eq!(v["depth"], 0);
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
        execution_settled: true,
    };
    let v = serde_json::to_value(&resp).unwrap();
    assert_eq!(v["run_id"], "r1");
    assert_eq!(v["status"], "cancelled");
    assert_eq!(v["execution_settled"], true);
}

#[test]
fn auth_token_response_serializes() {
    let resp = AuthTokenResponse {
        user_id: "u1".into(),
        access_token: "at".into(),
        refresh_token: "rt".into(),
        token_type: "Bearer".into(),
        expires_in: 3600,
    };
    let v = serde_json::to_value(&resp).unwrap();
    assert_eq!(v["user_id"], "u1");
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
            parent_run_id: None,
            root_run_id: Some("run-1".into()),
            depth: 0,
            status: "running".into(),
            waiting_for: None,
            events_count: 3,
            workspace: None,
            executor: None,
            transport: None,
            accounting: None,
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
        parent_run_id: Some("root".into()),
        root_run_id: Some("root".into()),
        depth: 1,
        status: "waiting".into(),
        waiting_for: Some("tool_call".into()),
        events_count: 7,
        workspace: None,
        executor: None,
        transport: None,
        accounting: None,
    };
    let resp: RunStatusResponse = record.into();
    assert_eq!(resp.status, "waiting");
    assert_eq!(resp.waiting_for.as_deref(), Some("tool_call"));
    assert_eq!(resp.events_count, 7);
    assert_eq!(resp.parent_run_id.as_deref(), Some("root"));
    assert_eq!(resp.root_run_id.as_deref(), Some("root"));
    assert_eq!(resp.depth, 1);

    // without waiting_for
    let record = RunStatusRecord {
        run_id: "r2".into(),
        session_id: "s2".into(),
        parent_run_id: None,
        root_run_id: Some("r2".into()),
        depth: 0,
        status: "completed".into(),
        waiting_for: None,
        events_count: 10,
        workspace: None,
        executor: None,
        transport: None,
        accounting: None,
    };
    let resp: RunStatusResponse = record.into();
    assert!(resp.waiting_for.is_none());
}

#[test]
fn cancel_run_record_to_response() {
    let record = CancelRunRecord {
        run_id: "r1".into(),
        status: "cancelled".into(),
        execution_settled: true,
    };
    let resp: CancelRunResponse = record.into();
    assert_eq!(resp.run_id, "r1");
    assert_eq!(resp.status, "cancelled");
}

#[test]
fn run_mutation_record_to_response() {
    let record = RunMutationRecord::applied("r1", "paused", "running");
    let resp: RunMutationResponse = record.into();
    assert_eq!(resp.run_id, "r1");
    assert_eq!(resp.status, "paused");
    assert_eq!(resp.previous_status, "running");
    assert_eq!(resp.disposition, RunMutationDisposition::Applied);
    assert!(resp.continuation.is_none());
}

#[test]
fn run_mutation_continuation_to_response() {
    let record = RunMutationRecord {
        run_id: "child-run".into(),
        status: "paused".into(),
        previous_status: "paused".into(),
        disposition: RunMutationDisposition::SessionContinuationRequired,
        continuation: Some(RunContinuationRecord {
            strategy: "session_continuation".into(),
            session_id: "session-1".into(),
            source_run_id: "child-run".into(),
        }),
    };
    let response: RunMutationResponse = record.into();
    assert_eq!(
        response.disposition,
        RunMutationDisposition::SessionContinuationRequired
    );
    let continuation = response.continuation.expect("continuation response");
    assert_eq!(continuation.session_id, "session-1");
    assert_eq!(continuation.source_run_id, "child-run");
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
        user_id: "u1".into(),
        access_token: "access123".into(),
        refresh_token: "refresh456".into(),
        token_type: "Bearer".into(),
        expires_in: 7200,
    };
    let resp: AuthTokenResponse = record.into();
    assert_eq!(resp.user_id, "u1");
    assert_eq!(resp.access_token, "access123");
    assert_eq!(resp.refresh_token, "refresh456");
    assert_eq!(resp.token_type, "Bearer");
    assert_eq!(resp.expires_in, 7200);
}

// ── chat_request_into_data ──────────────────────────────────────

#[test]
fn chat_request_parses_provider_resolved_model_selection() {
    let request: ChatRequest = serde_json::from_value(json!({
        "message": "hello",
        "model_selection": {"offering_id": "offer-provider"},
        "resolved_model_selection": {
            "offering_id": "offer-provider",
            "model_name": "provider-model"
        }
    }))
    .expect("provider wire request should parse");

    let resolved = request
        .resolved_model_selection
        .expect("trusted resolution should remain present on the wire request");
    assert_eq!(resolved.offering_id, "offer-provider");
    assert_eq!(resolved.model_name, "provider-model");
}

#[test]
fn chat_request_work_binding_is_explicit_and_strict() {
    let request: ChatRequest = serde_json::from_value(json!({
        "message": "continue",
        "session_id": "session-1",
        "work_binding": {"work_id": "work-1", "branch_id": "branch-1"}
    }))
    .expect("typed Work binding");
    let binding = request.work_binding.expect("Work binding");
    assert_eq!(binding.work_id, "work-1");
    assert_eq!(binding.branch_id, "branch-1");
    assert!(binding.item.is_none());
    let request: ChatRequest = serde_json::from_value(json!({
        "message": "continue",
        "session_id": "session-1",
        "work_binding": {
            "work_id": "work-1",
            "branch_id": "branch-1",
            "item": {
                "item_id": "root",
                "item_revision": 1,
                "attempt_id": "run-1"
            }
        }
    }))
    .expect("typed WorkItem attempt binding");
    assert_eq!(
        request
            .work_binding
            .expect("Work binding")
            .item
            .expect("WorkItem binding")
            .attempt_id,
        "run-1"
    );
    assert!(
        serde_json::from_value::<ChatRequest>(json!({
            "message": "continue",
            "session_id": "session-1",
            "work_binding": {
                "work_id": "work-1",
                "branch_id": "branch-1",
                "item": {"item_id": "root", "item_revision": 1}
            }
        }))
        .is_err(),
        "partial WorkItem bindings must fail closed"
    );
    assert!(
        serde_json::from_value::<ChatRequest>(json!({
            "message": "continue",
            "session_id": "session-1",
            "work_binding": {
                "work_id": "work-1",
                "branch_id": "branch-1",
                "intent": "guess"
            }
        }))
        .is_err(),
        "untyped/unknown Work binding fields must fail closed"
    );
}

#[test]
fn work_create_request_is_a_strict_typed_command() {
    let request: WorkCreateRequestV1 = serde_json::from_value(json!({
        "request_id": "start-work-1",
        "goal": "Ship the canonical Start Work boundary.",
        "criteria": [{
            "criterion_id": "tests-pass",
            "kind": "test_check",
            "statement": "Relevant tests pass.",
            "command": "cargo test -p astra-runtime work_handlers"
        }]
    }))
    .expect("typed Work create request");
    assert_eq!(request.request_id, "start-work-1");
    assert_eq!(request.goal, "Ship the canonical Start Work boundary.");
    assert!(
        serde_json::from_value::<WorkCreateRequestV1>(json!({
            "request_id": "start-work-1",
            "goal": "Ship the canonical Start Work boundary.",
            "criteria": [],
            "session_id": "client-controlled-session"
        }))
        .is_err(),
        "public Work creation must never accept an internal session identity"
    );
    assert!(
        serde_json::from_value::<WorkCreateRequestV1>(json!({
            "request_id": "start-work-1",
            "goal": "Missing criteria is not an implicit inference request."
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<WorkCreateRequestV1>(json!({
            "request_id": "start-work-1",
            "goal": "Do not accept an unverifiable model label.",
            "criteria": [{
                "criterion_id": "looks-good",
                "kind": "model_assessment",
                "statement": "The model thinks this is done."
            }]
        }))
        .is_err(),
        "reserved criterion labels must not become constructible verification contracts"
    );
}

#[test]
fn work_turn_request_cannot_smuggle_runtime_or_session_authority() {
    let request: WorkTurnRequestV1 = serde_json::from_value(json!({
        "request_id": "continue-1",
        "attachment_id": "attachment-1",
        "message": "Continue from the current Work facts."
    }))
    .expect("typed Work turn request");
    assert_eq!(request.request_id, "continue-1");
    assert_eq!(request.message, "Continue from the current Work facts.");
    for forbidden in [
        json!({
            "request_id": "continue-1",
            "attachment_id": "attachment-1",
            "message": "Continue.",
            "session_id": "client-session"
        }),
        json!({
            "request_id": "continue-1",
            "attachment_id": "attachment-1",
            "message": "Continue.",
            "model_selection": {"offering_id": "client-route"}
        }),
        json!({
            "request_id": "continue-1",
            "attachment_id": "attachment-1",
            "message": "Continue.",
            "work_binding": {"work_id": "other", "branch_id": "other"}
        }),
    ] {
        assert!(
            serde_json::from_value::<WorkTurnRequestV1>(forbidden).is_err(),
            "Work continuation authority is entirely path/server owned"
        );
    }
}

#[test]
fn work_control_release_request_is_strict_and_carries_exact_basis() {
    let request: WorkBranchControlOperationRequestV1 = serde_json::from_value(json!({
        "request_id": "release-1",
        "expected_branch_revision": 2,
        "expected_writer_epoch": 7,
        "expected_canonical_root_hash": "sha256:root",
        "command": {
            "kind": "release_branch_control",
            "attachment_id": "attachment-1"
        }
    }))
    .expect("typed Work controller release");
    assert_eq!(request.expected_branch_revision, 2);
    assert_eq!(request.expected_writer_epoch, 7);
    assert!(matches!(
        request.command,
        WorkBranchControlCommandV1::ReleaseBranchControl { ref attachment_id }
            if attachment_id == "attachment-1"
    ));
    assert!(
        serde_json::from_value::<WorkBranchControlOperationRequestV1>(json!({
            "request_id": "release-1",
            "expected_branch_revision": 2,
            "expected_writer_epoch": 7,
            "expected_canonical_root_hash": "sha256:root",
            "command": {
                "kind": "release_branch_control",
                "attachment_id": "attachment-1"
            },
            "session_id": "client-controlled-session"
        }))
        .is_err(),
        "controller release must not accept internal session authority"
    );
}

#[test]
fn work_task_graph_query_is_an_exact_pinned_pagination_contract() {
    let query: WorkTaskGraphQueryV1 = serde_json::from_value(json!({
        "graph_revision": 7,
        "item_offset": 8,
        "item_limit": 8,
        "dependency_offset": 128,
        "dependency_limit": 128
    }))
    .expect("typed Task Graph query");
    assert_eq!(query.graph_revision, Some(7));
    assert_eq!(query.item_offset, Some(8));
    assert!(
        serde_json::from_value::<WorkTaskGraphQueryV1>(json!({
            "graph_revision": 7,
            "item_offset": 8,
            "session_id": "internal-session"
        }))
        .is_err(),
        "Task Graph reads must not accept internal identity or unknown controls"
    );
}

#[test]
fn chat_request_into_data_maps_all_fields() {
    let mut ctx = Map::new();
    ctx.insert("tool".into(), json!("calc"));
    let req = ChatRequest {
        message: "hello".into(),
        conversation_authority: None,
        user_intent: Some("pure hello".into()),
        parts: vec![json!({"type": "text", "text": "hello"})],
        attachments: vec![json!({"id": "att-1", "kind": "file"})],
        stable_runtime_system_prompt: Some("Prefer extension skills on semantic overlap.".into()),
        runtime_system_prompt: Some("Runtime SQL scope db_name: retail.".into()),
        session_id: Some("s1".into()),
        work_binding: Some(astra_services::runs::WorkRuntimeBindingRequest {
            work_id: "work-1".into(),
            branch_id: "branch-1".into(),
            item: None,
        }),
        agent_id: Some("a1".into()),
        model_selection: Some(astra_turn_types::ModelSelection {
            offering_id: "offer-gpt-4".into(),
        }),
        resolved_model_selection: Some(astra_services::runs::ResolvedModelSelection {
            offering_id: "offer-gpt-4".into(),
            model_name: "gpt-4".into(),
        }),
        capability_descriptors: None,
        agent_bindings: Vec::new(),
        agent_binding: None,
        runtime_auth: None,
        runtime_profile: None,
        workspace_binding: None,
        executor_binding: None,
        skill_search: Some(astra_core::SkillSearchSettings::default()),
        allow_skills: None,
        allow_skill_sources: None,
        allow_tools: None,
        enabled_tools: Some(vec!["github".to_string()]),
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
        context: Some(ctx.clone()),
        edge_executor_id: Some("edge-1".into()),
        capabilities: vec!["bash".into(), "fs".into()],
        execution_budget: Some(ExecutionBudget {
            initial_turns: Some(3),
            hard_turn_limit: Some(7),
        }),
        execution_time_budget: Some(ExecutionTimeBudget {
            remaining_seconds: 37,
        }),
        execution_policy: ExecutionPolicyRequest {
            turn_intent: TurnIntentExecutionPolicy::FixedDefault,
            skill_auto_route: SkillAutoRouteExecutionPolicy::Disabled,
        },
        explain: true,
        interaction_mode: Some(astra_services::runs::RequestedTurnInteractionMode::Auto),
        interactive_client: true,
        plan_subtask_id: None,
        is_plan_subtask: None,
    };
    let data = chat_request_into_data(req);
    assert_eq!(data.message, "hello");
    assert_eq!(data.user_intent.as_deref(), Some("pure hello"));
    assert_eq!(data.parts, vec![json!({"type": "text", "text": "hello"})]);
    assert_eq!(
        data.attachments,
        vec![json!({"id": "att-1", "kind": "file"})]
    );
    assert_eq!(
        data.stable_runtime_system_prompt.as_deref(),
        Some("Prefer extension skills on semantic overlap.")
    );
    assert_eq!(
        data.runtime_system_prompt.as_deref(),
        Some("Runtime SQL scope db_name: retail.")
    );
    assert_eq!(data.session_id.as_deref(), Some("s1"));
    assert_eq!(
        data.work_binding
            .as_ref()
            .map(|binding| (binding.work_id.as_str(), binding.branch_id.as_str())),
        Some(("work-1", "branch-1"))
    );
    assert_eq!(data.agent_id.as_deref(), Some("a1"));
    assert!(
        data.model.is_none(),
        "wire conversion must not resolve routes"
    );
    assert_eq!(
        data.model_selection
            .as_ref()
            .map(|selection| selection.offering_id.as_str()),
        Some("offer-gpt-4")
    );
    assert_eq!(
        data.resolved_model_selection.as_ref().map(|selection| (
            selection.offering_id.as_str(),
            selection.model_name.as_str()
        )),
        Some(("offer-gpt-4", "gpt-4"))
    );
    assert!(data.admitted_model_execution.is_none());
    assert_eq!(
        data.skill_search,
        Some(astra_core::SkillSearchSettings::default())
    );
    assert_eq!(data.runtime_mcp_bindings.len(), 1);
    assert_eq!(data.runtime_mcp_bindings[0].id, "external_nl2sql");
    assert_eq!(data.context, Some(ctx));
    assert_eq!(data.edge_executor_id.as_deref(), Some("edge-1"));
    assert_eq!(data.capabilities, vec!["bash", "fs"]);
    assert_eq!(data.enabled_tools, Some(vec!["github".to_string()]));
    assert_eq!(
        data.execution_budget,
        Some(ExecutionBudget {
            initial_turns: Some(3),
            hard_turn_limit: Some(7),
        })
    );
    assert_eq!(
        data.execution_time_budget,
        Some(ExecutionTimeBudget {
            remaining_seconds: 37,
        })
    );
    assert!(data.explain);
    assert_eq!(
        data.execution_policy.turn_intent,
        TurnIntentExecutionPolicy::FixedDefault
    );
    assert_eq!(
        data.execution_policy.skill_auto_route,
        SkillAutoRouteExecutionPolicy::Disabled
    );
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
    assert!(data.work_binding.is_none());
    assert!(data.agent_id.is_none());
    assert!(data.model.is_none());
    assert!(data.resolved_model_selection.is_none());
    assert_eq!(
        data.model_selection_mode,
        astra_services::runs::ModelSelectionMode::ExplicitOffering
    );
    assert!(data.admitted_model_execution.is_none());
    assert!(data.runtime_mcp_bindings.is_empty());
    assert!(data.context.is_none());
    assert!(data.edge_executor_id.is_none());
    assert!(data.capabilities.is_empty());
    assert!(data.execution_budget.is_none());
    assert!(data.execution_time_budget.is_none());
    assert_eq!(data.execution_policy, ExecutionPolicyRequest::default());
    assert!(!data.explain);
    assert!(data.interaction_mode.is_none());
    assert!(!data.interactive_client);
}

#[test]
fn chat_request_rejects_removed_mcp_binding_ids_field() {
    let error = match serde_json::from_str::<ChatRequest>(
        r#"{"message":"test","mcp_binding_ids":["retired-binding"]}"#,
    ) {
        Ok(_) => panic!("removed mcp_binding_ids must be rejected at the wire boundary"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("unknown field `mcp_binding_ids`")
    );
}

#[test]
fn chat_request_into_data_merges_plan_subtask_into_context() {
    let req = ChatRequest {
        message: "do step".into(),
        conversation_authority: None,
        user_intent: None,
        parts: Vec::new(),
        attachments: Vec::new(),
        stable_runtime_system_prompt: None,
        runtime_system_prompt: None,
        session_id: None,
        work_binding: None,
        agent_id: None,
        model_selection: None,
        resolved_model_selection: None,
        capability_descriptors: None,
        agent_bindings: Vec::new(),
        agent_binding: None,
        runtime_auth: None,
        runtime_profile: None,
        workspace_binding: None,
        executor_binding: None,
        skill_search: None,
        allow_skills: None,
        allow_skill_sources: None,
        allow_tools: None,
        enabled_tools: None,
        runtime_mcp_bindings: Vec::new(),
        context: None,
        edge_executor_id: None,
        capabilities: Vec::new(),
        execution_budget: None,
        execution_time_budget: None,
        execution_policy: ExecutionPolicyRequest::default(),
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

#[test]
fn patch_materialization_request_is_closed_and_revision_pinned() {
    let value = json!({
        "request_id": "request-1",
        "patch_artifact_id": "patch-1",
        "expected_target_branch_revision": 4,
        "expected_target_graph_revision": 3
    });
    serde_json::from_value::<WorkPatchMaterializationRequestV1>(value.clone())
        .expect("complete materialization request");
    let mut widened = value.clone();
    widened
        .as_object_mut()
        .expect("request object")
        .insert("provider_ref".into(), json!("must-stay-server-owned"));
    assert!(serde_json::from_value::<WorkPatchMaterializationRequestV1>(widened).is_err());
    let mut internal_basis = value;
    internal_basis
        .as_object_mut()
        .expect("request object")
        .insert(
            "expected_target_subject_ref".into(),
            json!("internal-workspace"),
        );
    assert!(serde_json::from_value::<WorkPatchMaterializationRequestV1>(internal_basis).is_err());
}

#[test]
fn patch_commit_request_is_closed_revision_pinned_and_cannot_assert_identity() {
    let value = json!({
        "request_id": "commit-1",
        "patch_artifact_id": "patch-1",
        "expected_target_branch_revision": 4,
        "expected_target_graph_revision": 3,
        "message": "Commit reviewed changes"
    });
    serde_json::from_value::<WorkPatchCommitRequestV1>(value.clone())
        .expect("complete patch commit request");
    for internal in ["provider_ref", "policy_decision_ref", "author_email"] {
        let mut widened = value.clone();
        widened
            .as_object_mut()
            .expect("request object")
            .insert(internal.into(), json!("caller-controlled"));
        assert!(
            serde_json::from_value::<WorkPatchCommitRequestV1>(widened).is_err(),
            "{internal} must remain server-owned"
        );
    }
}

#[test]
fn patch_export_request_is_closed_and_cannot_assert_provider_authority() {
    let value = json!({
        "request_id": "export-1",
        "expected_branch_revision": 4,
        "expected_graph_revision": 3
    });
    serde_json::from_value::<WorkPatchArtifactExportRequestV1>(value.clone())
        .expect("complete export request");
    let mut widened = value.clone();
    widened
        .as_object_mut()
        .expect("request object")
        .insert("provider_ref".into(), json!("caller-provider"));
    assert!(serde_json::from_value::<WorkPatchArtifactExportRequestV1>(widened).is_err());
    let mut internal_basis = value;
    internal_basis
        .as_object_mut()
        .expect("request object")
        .insert("expected_subject_ref".into(), json!("internal-workspace"));
    assert!(serde_json::from_value::<WorkPatchArtifactExportRequestV1>(internal_basis).is_err());
}

#[test]
fn patch_artifact_page_query_is_closed() {
    let value = json!({
        "before_created_at": "2026-08-02T12:00:00.000001Z",
        "before_patch_artifact_id": "patch-1",
        "limit": 20
    });
    serde_json::from_value::<WorkPatchArtifactsQueryV1>(value.clone())
        .expect("complete patch artifact page query");
    let mut widened = value;
    widened
        .as_object_mut()
        .expect("query object")
        .insert("include_content".into(), json!(true));
    assert!(serde_json::from_value::<WorkPatchArtifactsQueryV1>(widened).is_err());
}

#[test]
fn patch_materialization_page_query_is_closed() {
    let value = json!({
        "source_branch_id": "branch-alternative-1",
        "before_created_at": "2026-08-02T12:00:00.000001Z",
        "before_operation_id": "materialization-1",
        "limit": 20
    });
    serde_json::from_value::<WorkPatchMaterializationsQueryV1>(value.clone())
        .expect("complete materialization page query");
    let mut widened = value;
    widened
        .as_object_mut()
        .expect("query object")
        .insert("include_executor_lease".into(), json!(true));
    assert!(serde_json::from_value::<WorkPatchMaterializationsQueryV1>(widened).is_err());
}
