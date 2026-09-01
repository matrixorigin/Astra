use super::super::*;
use crate::server::header_utils::collect_forward_headers;
use astra_services::{DelegationRequest, DelegationResult};

/// Client-authored delegation intent. Owner, parent run, and parent session
/// are derived from the authenticated URL resource and are never accepted
/// from the body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DelegationHttpRequest {
    delegation_id: String,
    task: String,
    pattern: astra_services::CoordinationPattern,
    depth: u32,
    #[serde(default)]
    delegation_chain: Vec<String>,
    context: std::collections::HashMap<String, serde_json::Value>,
    execution_metadata: Option<serde_json::Value>,
}

/// POST /chat/runs/{run_id}/delegate
///
/// Delegates a run to one or more sub-agents according to a coordination
/// pattern (fan-out, pipeline, adversarial review, sequential).
pub(crate) async fn delegate_run_handler(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<DelegationHttpRequest>,
) -> Result<Json<DelegationResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let user_id = user.user_id;
    // Verify the authenticated user owns this run (IDOR prevention).
    let parent = state
        .execution
        .run_lifecycle_service
        .get_run_status(run_id.clone(), user_id.clone())
        .await?;
    let mut request = DelegationRequest {
        delegation_id: body.delegation_id,
        session_id: parent.session_id,
        parent_run_id: run_id.clone(),
        task: body.task,
        pattern: body.pattern,
        user_id,
        depth: body.depth,
        delegation_chain: body.delegation_chain,
        context: body.context,
        execution_metadata: body.execution_metadata,
    };
    let forward_headers = collect_forward_headers(&headers);
    request
        .context
        .remove(crate::turn::agentic::delegate_interception::FORWARD_HEADERS_CONTEXT_KEY);

    let engine = state.delegation_engine.as_ref().ok_or_else(|| {
        error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "delegation engine not configured",
        )
    })?;

    // Resolve the source agent identity from the tracker.
    // Top-level runs (not sub-runs) default to "main", which matches
    // the root orchestrator profile registered by register_default_agents().
    let source_agent_id = engine
        .tracker()
        .get_agent_id(&run_id)
        .await
        .unwrap_or_else(|| "main".to_string());

    // Validate the delegation request against the profile registry.
    engine
        .validate(&request, &source_agent_id)
        .await
        .map_err(|e| error_response(StatusCode::BAD_REQUEST, e))?;

    // Execute the delegation.
    let result = engine
        .execute_with_forward_headers(request, &source_agent_id, None, forward_headers, None)
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;

    Ok(Json(DelegationResponse::from(result)))
}

/// GET /chat/runs/{run_id}/delegations
///
/// Returns sub-run IDs spawned by delegations from this parent run.
pub(crate) async fn list_delegations_handler(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<DelegationListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let user_id = user.user_id;
    state
        .execution
        .run_lifecycle_service
        .get_run_status(run_id.clone(), user_id.clone())
        .await?;

    let engine = state.delegation_engine.as_ref().ok_or_else(|| {
        error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "delegation engine not configured",
        )
    })?;

    let sub_runs = engine.tracker().get_children(&run_id).await;
    Ok(Json(DelegationListResponse {
        parent_run_id: run_id,
        sub_run_ids: sub_runs,
    }))
}

// ─── Response types ─────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub(crate) struct DelegationResponse {
    pub delegation_id: String,
    pub status: String,
    pub agent_results: Vec<DelegationAgentResult>,
    pub aggregated_output: Option<String>,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    pub total_tool_calls: u32,
}

#[derive(Debug, Serialize)]
pub(crate) struct DelegationAgentResult {
    pub agent_id: String,
    pub status: String,
    pub output: Option<String>,
    pub error: Option<String>,
}

impl From<DelegationResult> for DelegationResponse {
    fn from(r: DelegationResult) -> Self {
        Self {
            delegation_id: r.delegation_id,
            status: r.status,
            agent_results: r
                .agent_results
                .into_iter()
                .map(|ar| DelegationAgentResult {
                    agent_id: ar.agent_id,
                    status: ar.status,
                    output: ar.output,
                    error: ar.error,
                })
                .collect(),
            aggregated_output: r.aggregated_output,
            total_prompt_tokens: r.total_prompt_tokens,
            total_completion_tokens: r.total_completion_tokens,
            total_tool_calls: r.total_tool_calls,
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct DelegationListResponse {
    pub parent_run_id: String,
    pub sub_run_ids: Vec<String>,
}

// ─── Delegation pause / resume ──────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub(crate) struct DelegationMutationResponse {
    pub parent_run_id: String,
    pub affected: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DelegationMutationRequest {
    expected_session_id: String,
}

fn validated_expected_session_id(
    body: &DelegationMutationRequest,
) -> Result<&str, (StatusCode, Json<ErrorResponse>)> {
    if body.expected_session_id.trim().is_empty() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "expected_session_id must not be empty",
        ));
    }
    Ok(&body.expected_session_id)
}

/// POST /chat/runs/{run_id}/delegations/pause
///
/// Pause all sub-runs delegated from this parent run.
pub(crate) async fn pause_delegations_handler(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<DelegationMutationRequest>,
) -> Result<Json<DelegationMutationResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let user_id = user.user_id;
    let expected_session_id = validated_expected_session_id(&body)?;

    let engine = state.delegation_engine.as_ref().ok_or_else(|| {
        error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "delegation engine not configured",
        )
    })?;

    let affected = engine
        .pause_children_of(&user_id, expected_session_id, &run_id)
        .await;
    Ok(Json(DelegationMutationResponse {
        parent_run_id: run_id,
        affected,
    }))
}

/// POST /chat/runs/{run_id}/delegations/resume
///
/// Resume all sub-runs delegated from this parent run.
pub(crate) async fn resume_delegations_handler(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<DelegationMutationRequest>,
) -> Result<Json<DelegationMutationResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let user_id = user.user_id;
    let expected_session_id = validated_expected_session_id(&body)?;

    let engine = state.delegation_engine.as_ref().ok_or_else(|| {
        error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "delegation engine not configured",
        )
    })?;

    let affected = engine
        .resume_children_of(&user_id, expected_session_id, &run_id)
        .await;
    Ok(Json(DelegationMutationResponse {
        parent_run_id: run_id,
        affected,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::HashMap,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use astra_core::{ErrorResponse, error_response};
    use astra_services::{
        AggregationStrategy, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData,
        AuthService, AuthTokenRecord, AuthUserRecord, CancelRunRecord, ChatRequestData,
        ChatRunRecord, ChatStreamRecord, CoordinationPattern, RunLifecycleService, RunListRecord,
        RunStatusRecord,
    };
    use async_trait::async_trait;
    use axum::{
        Json,
        extract::{Path, State},
        http::{HeaderMap, HeaderValue, StatusCode},
    };
    use tokio::sync::Mutex;

    use crate::{AppState, HealthChecker, ServiceInfo};

    #[derive(Clone)]
    struct AlwaysHealthy;

    #[async_trait]
    impl HealthChecker for AlwaysHealthy {
        async fn database_healthy(&self) -> bool {
            true
        }
    }

    #[derive(Default)]
    struct RecordingAuthService {
        current_user_calls: AtomicUsize,
    }

    #[async_trait]
    impl AuthService for RecordingAuthService {
        async fn register(
            &self,
            _request: AuthRegisterRequestData,
        ) -> Result<AuthUserRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!("register is not used in delegation handler tests")
        }

        async fn login(
            &self,
            _request: AuthLoginRequestData,
        ) -> Result<AuthTokenRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!("login is not used in delegation handler tests")
        }

        async fn refresh(
            &self,
            _request: AuthRefreshRequestData,
        ) -> Result<AuthTokenRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!("refresh is not used in delegation handler tests")
        }

        async fn logout(
            &self,
            _request: AuthRefreshRequestData,
        ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
            unreachable!("logout is not used in delegation handler tests")
        }

        async fn current_user(
            &self,
            headers: &HeaderMap,
        ) -> Result<AuthUserRecord, (StatusCode, Json<ErrorResponse>)> {
            self.current_user_calls.fetch_add(1, Ordering::SeqCst);
            if headers.get("authorization").is_none() {
                return Err(error_response(
                    StatusCode::UNAUTHORIZED,
                    "missing authorization",
                ));
            }
            Ok(AuthUserRecord {
                user_id: "delegation-owner".into(),
                username: "delegation-owner".into(),
                email: "delegation@example.com".into(),
                display_name: None,
            })
        }
    }

    #[derive(Default)]
    struct RecordingRunLifecycleService {
        run_status_requests: Mutex<Vec<(String, String)>>,
    }

    #[async_trait]
    impl RunLifecycleService for RecordingRunLifecycleService {
        async fn create_run(
            &self,
            _user_id: String,
            _request: ChatRequestData,
        ) -> Result<ChatRunRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!("create_run is not used in delegation handler tests")
        }

        async fn stream_chat(
            &self,
            _user_id: String,
            _request: ChatRequestData,
        ) -> Result<ChatStreamRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!("stream_chat is not used in delegation handler tests")
        }

        async fn get_run_status(
            &self,
            run_id: String,
            user_id: String,
        ) -> Result<RunStatusRecord, (StatusCode, Json<ErrorResponse>)> {
            self.run_status_requests
                .lock()
                .await
                .push((run_id, user_id));
            Err(error_response(StatusCode::FORBIDDEN, "run access denied"))
        }

        async fn stream_run(
            &self,
            _run_id: String,
            _user_id: String,
            _last_index: u32,
        ) -> Result<Vec<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
            unreachable!("stream_run is not used in delegation handler tests")
        }

        async fn cancel_run(
            &self,
            _run_id: String,
            _user_id: String,
        ) -> Result<CancelRunRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!("cancel_run is not used in delegation handler tests")
        }

        async fn list_runs_cursor(
            &self,
            _user_id: String,
            _limit: u32,
            _cursor: Option<astra_services::runs::RunListCursor>,
        ) -> Result<RunListRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!("list_runs is not used in delegation handler tests")
        }
    }

    fn build_state(
        auth_service: Arc<dyn AuthService>,
        run_lifecycle_service: Arc<dyn RunLifecycleService>,
    ) -> AppState {
        AppState::new(ServiceInfo::default(), Arc::new(AlwaysHealthy))
            .with_auth_service(auth_service)
            .with_run_lifecycle_service(run_lifecycle_service)
    }

    fn auth_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_static("Bearer delegation-test-token"),
        );
        headers
    }

    fn fan_out_request() -> DelegationHttpRequest {
        DelegationHttpRequest {
            delegation_id: "delegation-1".into(),
            task: "delegate this run".into(),
            pattern: CoordinationPattern::FanOut {
                agent_ids: vec!["agent-a".into()],
                aggregation: AggregationStrategy::AllResults,
                timeout_sec: 30,
            },
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        }
    }

    /// Read handlers verify ownership via status lookup. Mutations carry an
    /// immutable session precondition into the atomic store write instead of
    /// discovering session authority from run_id.
    #[tokio::test]
    async fn delegation_handlers_verify_run_ownership() {
        let auth = Arc::new(RecordingAuthService::default());
        let lifecycle = Arc::new(RecordingRunLifecycleService::default());
        let state = build_state(auth.clone(), lifecycle.clone());
        let run_id = "run-123".to_string();

        let delegate_err = delegate_run_handler(
            State(state.clone()),
            Path(run_id.clone()),
            auth_headers(),
            Json(fan_out_request()),
        )
        .await
        .expect_err("delegate should stop at ownership check");
        assert_eq!(delegate_err.0, StatusCode::FORBIDDEN);

        let list_err =
            list_delegations_handler(State(state.clone()), Path(run_id.clone()), auth_headers())
                .await
                .expect_err("list should stop at ownership check");
        assert_eq!(list_err.0, StatusCode::FORBIDDEN);

        let pause_err = pause_delegations_handler(
            State(state.clone()),
            Path(run_id.clone()),
            auth_headers(),
            Json(DelegationMutationRequest {
                expected_session_id: "session-1".to_string(),
            }),
        )
        .await
        .expect_err("pause should require a configured delegation engine");
        assert_eq!(pause_err.0, StatusCode::SERVICE_UNAVAILABLE);

        let resume_err = resume_delegations_handler(
            State(state),
            Path(run_id.clone()),
            auth_headers(),
            Json(DelegationMutationRequest {
                expected_session_id: "session-1".to_string(),
            }),
        )
        .await
        .expect_err("resume should require a configured delegation engine");
        assert_eq!(resume_err.0, StatusCode::SERVICE_UNAVAILABLE);

        let calls = lifecycle.run_status_requests.lock().await.clone();
        assert!(
            calls
                == vec![
                    (run_id.clone(), "delegation-owner".to_string()),
                    (run_id, "delegation-owner".to_string()),
                ],
            "mutation handlers must not discover session authority via status lookup: {calls:?}"
        );
        assert_eq!(
            auth.current_user_calls.load(Ordering::SeqCst),
            4,
            "all delegation handlers should authenticate before checking ownership"
        );
    }

    #[test]
    fn delegation_http_body_rejects_client_authored_authority() {
        for field in ["user_id", "parent_run_id", "session_id"] {
            let mut body = serde_json::json!({
                "delegation_id": "delegation-1",
                "task": "delegate",
                "pattern": {
                    "pattern": "fan_out",
                    "agent_ids": ["agent-a"],
                    "aggregation": "all_results",
                    "timeout_sec": 30
                },
                "depth": 0,
                "delegation_chain": [],
                "context": {},
                "execution_metadata": null
            });
            body.as_object_mut()
                .unwrap()
                .insert(field.to_string(), serde_json::json!("foreign-authority"));
            assert!(
                serde_json::from_value::<DelegationHttpRequest>(body).is_err(),
                "{field} must not be client-authored"
            );
        }
    }
}
