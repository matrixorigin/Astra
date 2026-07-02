use super::super::*;
use astra_services::runs::transform_run_event_for_client;

pub(crate) fn transform_stream_run_events_for_client(
    run_id: &str,
    events: Vec<serde_json::Value>,
) -> Vec<serde_json::Value> {
    let mut pending_run_error: Option<String> = None;
    transform_stream_run_events_for_client_with_pending(run_id, events, &mut pending_run_error)
}

pub(crate) fn transform_stream_run_events_for_client_with_pending(
    run_id: &str,
    events: Vec<serde_json::Value>,
    pending_run_error: &mut Option<String>,
) -> Vec<serde_json::Value> {
    let mut transformed_events = Vec::with_capacity(events.len());

    for event in events {
        let index = event.get("index").cloned();
        let event_type = event
            .get("event_type")
            .or_else(|| event.get("type"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let run_finished_cancelled = event
            .get("data")
            .and_then(serde_json::Value::as_object)
            .and_then(|data| data.get("cancelled"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let run_finished_interrupted = event
            .get("data")
            .and_then(serde_json::Value::as_object)
            .and_then(|data| data.get("interrupted"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        if event_type == "run_error" {
            *pending_run_error = event
                .get("data")
                .and_then(serde_json::Value::as_object)
                .and_then(|data| data.get("error"))
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned);
        }

        if event_type == "run_finished" {
            let data = event
                .get("data")
                .and_then(serde_json::Value::as_object)
                .cloned()
                .unwrap_or_default();

            let mut usage = serde_json::Map::new();
            usage.insert(
                "type".to_string(),
                serde_json::Value::String("usage".to_string()),
            );
            let mut has_usage = false;

            for key in [
                "prompt_tokens",
                "completion_tokens",
                "cache_read_tokens",
                "cache_creation_tokens",
                "tool_call_count",
            ] {
                if let Some(value) = data.get(key).cloned() {
                    usage.insert(key.to_string(), value);
                    has_usage = true;
                }
            }

            if has_usage {
                if let Some(index) = index.clone() {
                    usage.insert("index".to_string(), index);
                }
                transformed_events.push(serde_json::Value::Object(usage));
            }
        }

        let mut transformed = transform_run_event_for_client(event);
        // wip-7 allowlist: the transform returns `Value::Null` for
        // events outside the external client allowlist (e.g.
        // `injection_freshness`). Skip them entirely rather than
        // pushing a bare `null` into the SSE stream — that would
        // either serialize as `data: null` (confusing) or need
        // downstream null-skipping everywhere.
        if transformed.is_null() {
            continue;
        }
        if should_inject_run_id(event_type.as_str())
            && let Some(obj) = transformed.as_object_mut()
            && !obj.contains_key("run_id")
        {
            obj.insert(
                "run_id".to_string(),
                serde_json::Value::String(run_id.to_string()),
            );
        }
        if event_type == "run_finished"
            && let Some(obj) = transformed.as_object_mut()
        {
            let status = if obj.get("status").is_some() {
                None
            } else if run_finished_cancelled {
                Some("cancelled")
            } else if run_finished_interrupted {
                Some("paused")
            } else if pending_run_error.is_some() {
                Some("failed")
            } else {
                Some("completed")
            };

            if let Some(status) = status {
                obj.insert(
                    "status".to_string(),
                    serde_json::Value::String(status.to_string()),
                );
            }
            if let Some(error) = pending_run_error.take() {
                obj.insert("error".to_string(), serde_json::Value::String(error));
            }
        }

        if let Some(index) = index
            && let Some(obj) = transformed.as_object_mut()
        {
            obj.insert("index".to_string(), index);
        }
        transformed_events.push(transformed);
    }

    transformed_events
}

fn should_inject_run_id(event_type: &str) -> bool {
    matches!(
        event_type,
        "run_started"
            | "run_error"
            | "run_interrupted"
            | "run_waiting"
            | "run_paused"
            | "run_resumed"
            | "run_input_queued"
            | "run_finished"
    ) || event_type == "run_blocked"
}

pub(crate) async fn get_run_status_handler(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<RunStatusResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let run = state
        .execution
        .run_lifecycle_service
        .get_run_status(run_id, user.user_id)
        .await?;
    Ok(Json(RunStatusResponse::from(run)))
}

#[derive(serde::Deserialize)]
pub(crate) struct RunProjectionQuery {
    #[serde(default = "default_projection_recent_limit")]
    pub recent_limit: u32,
}

fn default_projection_recent_limit() -> u32 {
    20
}

#[derive(serde::Serialize)]
pub(crate) struct RunProjectionCheckpointResponse {
    pub checkpoint_id: String,
    pub checkpoint_kind: String,
    pub checkpoint_version: String,
    pub node_seq: i64,
    pub created_at: String,
}

#[derive(serde::Serialize)]
pub(crate) struct RunProjectionResponse {
    pub run_id: String,
    pub session_id: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_for: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executor: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_policy: Option<String>,
    pub run_event_high_watermark: i64,
    pub projection_event_idx: i64,
    pub projection_updated_at: String,
    pub projection_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_event_type: Option<String>,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    pub total_tool_calls: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_checkpoint: Option<RunProjectionCheckpointResponse>,
    pub observability: RunProjectionObservabilityResponse,
    pub recent_events: Vec<serde_json::Value>,
}

#[derive(serde::Serialize)]
pub(crate) struct RunProjectionRepairResponse {
    pub repaired: bool,
    pub projection: RunProjectionResponse,
}

#[derive(serde::Serialize)]
pub(crate) struct PromptRequestObservabilityResponse {
    pub request_id: String,
    pub request_hash: String,
    pub message_count: u32,
    pub tool_count: u32,
    pub delta_counts: astra_services::PromptDeltaCounts,
}

#[derive(serde::Serialize)]
pub(crate) struct RunProjectionObservabilityResponse {
    pub has_durable_projection: bool,
    pub observability_available: bool,
    pub projection_lag_events: i64,
    pub prompt_request_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_prompt_request: Option<PromptRequestObservabilityResponse>,
}

impl RunProjectionResponse {
    fn new(
        value: astra_services::runs::RunProjectionRecord,
        observability: RunProjectionObservabilityResponse,
    ) -> Self {
        Self {
            run_id: value.run_id,
            session_id: value.session_id,
            status: value.status,
            waiting_for: value.waiting_for,
            error_message: value.error_message,
            workspace: value.workspace,
            executor: value.executor,
            transport: value.transport,
            fallback_policy: value.fallback_policy,
            run_event_high_watermark: value.run_event_high_watermark,
            projection_event_idx: value.projection_event_idx,
            projection_updated_at: value.projection_updated_at,
            projection_hash: value.projection_hash,
            latest_event_type: value.latest_event_type,
            total_prompt_tokens: value.total_prompt_tokens,
            total_completion_tokens: value.total_completion_tokens,
            total_tool_calls: value.total_tool_calls,
            latest_checkpoint: value.latest_checkpoint.map(|checkpoint| {
                RunProjectionCheckpointResponse {
                    checkpoint_id: checkpoint.checkpoint_id,
                    checkpoint_kind: checkpoint.checkpoint_kind,
                    checkpoint_version: checkpoint.checkpoint_version,
                    node_seq: checkpoint.node_seq,
                    created_at: checkpoint.created_at,
                }
            }),
            observability,
            recent_events: value.recent_events,
        }
    }
}

pub(crate) async fn get_run_projection_handler(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<RunProjectionQuery>,
) -> Result<Json<RunProjectionResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let user_id = user.user_id.clone();
    let projection = state
        .execution
        .run_lifecycle_service
        .get_run_projection(run_id.clone(), user_id.clone(), query.recent_limit)
        .await?;
    Ok(Json(
        build_run_projection_response(&state, &user_id, &run_id, projection).await?,
    ))
}

pub(crate) async fn repair_run_projection_handler(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<RunProjectionQuery>,
) -> Result<Json<RunProjectionRepairResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let user_id = user.user_id.clone();
    let projection = state
        .execution
        .run_lifecycle_service
        .repair_run_projection(run_id.clone(), user_id.clone(), query.recent_limit)
        .await?;
    Ok(Json(RunProjectionRepairResponse {
        repaired: true,
        projection: build_run_projection_response(&state, &user_id, &run_id, projection).await?,
    }))
}

async fn build_run_projection_response(
    state: &AppState,
    user_id: &str,
    run_id: &str,
    mut projection: astra_services::runs::RunProjectionRecord,
) -> Result<RunProjectionResponse, (StatusCode, Json<ErrorResponse>)> {
    projection.recent_events =
        transform_stream_run_events_for_client(run_id, projection.recent_events);
    let projection_lag_events =
        (projection.run_event_high_watermark - projection.projection_event_idx).max(0);
    let (observability_available, prompt_request_count, latest_prompt_request) =
        load_run_prompt_observability(state, user_id, run_id).await?;
    let has_durable_projection = projection.run_event_high_watermark
        == projection.projection_event_idx
        || projection.projection_event_idx >= 0;
    Ok(RunProjectionResponse::new(
        projection,
        RunProjectionObservabilityResponse {
            has_durable_projection,
            observability_available,
            projection_lag_events,
            prompt_request_count,
            latest_prompt_request,
        },
    ))
}

async fn load_run_prompt_observability(
    state: &AppState,
    user_id: &str,
    run_id: &str,
) -> Result<
    (bool, u32, Option<PromptRequestObservabilityResponse>),
    (StatusCode, Json<ErrorResponse>),
> {
    let Some(shared_pool) = state.shared_pool.as_ref() else {
        return Ok((false, 0, None));
    };
    let prompt_request_count =
        astra_services::count_prompt_requests_for_run(shared_pool, user_id, run_id)
            .await
            .map_err(|error| {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(ErrorResponse::new(format!(
                        "Failed to load run prompt request observability: {error}"
                    ))),
                )
            })?;
    let latest_prompt_request =
        astra_services::load_latest_prompt_observability_for_run(shared_pool, user_id, run_id)
            .await
            .map_err(|error| {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(ErrorResponse::new(format!(
                        "Failed to load latest run prompt request: {error}"
                    ))),
                )
            })?
            .map(|request| PromptRequestObservabilityResponse {
                request_id: request.request_id,
                request_hash: request.request_hash,
                message_count: request.message_count,
                tool_count: request.tool_count,
                delta_counts: request.delta_counts,
            });
    Ok((true, prompt_request_count, latest_prompt_request))
}

pub(crate) async fn stream_run_handler(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<RunStreamQuery>,
) -> Response {
    let user = match state.auth_service.current_user(&headers).await {
        Ok(user) => user,
        Err((status, error)) => return sse_error_response(status, error.0.detail),
    };

    match state
        .execution
        .run_lifecycle_service
        .stream_run_live(run_id.clone(), user.user_id, query.last_index)
        .await
    {
        Ok(mut stream) => {
            if let Some(event_rx) = stream.event_rx.take() {
                sse_streaming_response(stream.session_id, stream.run_id, event_rx)
            } else {
                sse_json_response(transform_stream_run_events_for_client(
                    &run_id,
                    stream.events,
                ))
            }
        }
        Err((status, error)) => sse_error_response(status, error.0.detail),
    }
}

pub(crate) async fn cancel_run_handler(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<CancelRunResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let result = state
        .execution
        .run_lifecycle_service
        .cancel_run(run_id, user.user_id)
        .await?;
    Ok(Json(CancelRunResponse::from(result)))
}

pub(crate) async fn list_runs_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<RunListQuery>,
) -> Result<Json<RunListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let runs = state
        .execution
        .run_lifecycle_service
        .list_runs(user.user_id, query.limit, query.offset)
        .await?;
    Ok(Json(RunListResponse::from(runs)))
}

pub(crate) async fn pause_run_handler(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<RunMutationResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let result = state
        .execution
        .run_lifecycle_service
        .pause_run(run_id, user.user_id)
        .await?;
    Ok(Json(RunMutationResponse::from(result)))
}

pub(crate) async fn resume_run_handler(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<RunMutationResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let result = state
        .execution
        .run_lifecycle_service
        .resume_run(run_id, user.user_id)
        .await?;
    Ok(Json(RunMutationResponse::from(result)))
}

#[derive(serde::Deserialize)]
pub(crate) struct RunInputRequest {
    pub idempotency_key: String,
    #[serde(default)]
    pub input: serde_json::Value,
}

#[derive(serde::Serialize)]
pub(crate) struct RunInputResponse {
    pub run_id: String,
    pub accepted: bool,
    pub duplicate: bool,
}

pub(crate) async fn submit_run_input_handler(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<RunInputRequest>,
) -> Result<Json<RunInputResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let result = state
        .execution
        .run_lifecycle_service
        .submit_run_input(
            run_id,
            user.user_id,
            astra_services::runs::RunInputData {
                idempotency_key: request.idempotency_key,
                input: request.input,
            },
        )
        .await?;
    Ok(Json(RunInputResponse {
        run_id: result.run_id,
        accepted: result.accepted,
        duplicate: result.duplicate,
    }))
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use super::*;
    use async_trait::async_trait;
    use axum::{
        Json,
        body::{self, Body},
        http::{HeaderMap, Request, StatusCode},
    };
    use serde_json::json;
    use tokio::sync::Mutex as TokioMutex;
    use tower::util::ServiceExt;

    use crate::{
        AppState, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData,
        AuthService, AuthTokenRecord, AuthUserRecord, ErrorResponse, HealthChecker, ServiceInfo,
        build_app,
    };

    static HTTP_RUN_DB: tokio::sync::OnceCell<astra_core::SharedPool> =
        tokio::sync::OnceCell::const_new();

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
            _request: AuthRegisterRequestData,
        ) -> Result<AuthUserRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn login(
            &self,
            _request: AuthLoginRequestData,
        ) -> Result<AuthTokenRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn refresh(
            &self,
            _request: AuthRefreshRequestData,
        ) -> Result<AuthTokenRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn logout(
            &self,
            _request: AuthRefreshRequestData,
        ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn current_user(
            &self,
            headers: &HeaderMap,
        ) -> Result<AuthUserRecord, (StatusCode, Json<ErrorResponse>)> {
            if headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                == Some("Bearer good-token")
            {
                Ok(AuthUserRecord {
                    user_id: "u1".to_string(),
                    username: "test-user".to_string(),
                    email: "u1@example.test".to_string(),
                    display_name: None,
                })
            } else {
                Err((
                    StatusCode::UNAUTHORIZED,
                    Json(ErrorResponse::new("Not authenticated".to_string())),
                ))
            }
        }
    }

    fn test_matrixone() -> astra_core::MatrixOneSettings {
        astra_core::MatrixOneSettings::mock()
    }

    async fn setup_http_run_db_it() -> astra_core::SharedPool {
        assert_eq!(
            std::env::var("ASTRA_TEST_DB_IT").as_deref(),
            Ok("1"),
            "set ASTRA_TEST_DB_IT=1 for ignored integration tests"
        );
        HTTP_RUN_DB
            .get_or_init(|| async {
                let settings = astra_core::MatrixOneSettings::from_env();
                let catalog = std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG")
                    .unwrap_or_else(|_| "mysql".to_string());
                astra_services::ensure_core_schema(&settings, &catalog)
                    .await
                    .expect("ensure_core_schema");
                astra_core::SharedPool::new(&settings)
                    .await
                    .expect("SharedPool::new")
            })
            .await
            .clone()
    }

    async fn cleanup_run_http_fixture(pool: &astra_core::SharedPool, user_id: &str, run_id: &str) {
        for sql in [
            "DELETE FROM run_display_projections WHERE user_id = ? AND run_id = ?",
            "DELETE FROM run_checkpoints WHERE user_id = ? AND run_id = ?",
            "DELETE FROM agent_run_events WHERE user_id = ? AND run_id = ?",
            "DELETE FROM agent_runs WHERE user_id = ? AND run_id = ?",
        ] {
            sqlx::query(sql)
                .bind(user_id)
                .bind(run_id)
                .execute(pool.get())
                .await
                .expect("cleanup run HTTP fixture");
        }
    }

    #[test]
    fn transform_stream_run_events_for_client_uses_client_protocol_shape() {
        let transformed = transform_stream_run_events_for_client(
            "run-123",
            vec![
                json!({
                    "event_type": "text_delta",
                    "data": {"chunk": "hello"},
                    "index": 7
                }),
                json!({
                    "event_type": "run_error",
                    "data": {"error": "boom"},
                    "index": 8
                }),
            ],
        );

        assert_eq!(
            transformed[0],
            json!({"type": "text_delta", "content": "hello", "index": 7})
        );
        assert_eq!(
            transformed[1],
            json!({"type": "run_error", "message": "boom", "error": "boom", "code": "RUN_ERROR", "run_id": "run-123", "index": 8})
        );
    }

    #[test]
    fn transform_stream_run_events_for_client_maps_tool_result_to_tool_call_end() {
        let transformed = transform_stream_run_events_for_client(
            "run-123",
            vec![json!({
                "event_type": "tool_result",
                "data": {"call_id": "call-1", "result": "ok"},
                "index": 9
            })],
        );

        assert_eq!(
            transformed[0],
            json!({"type": "tool_call_end", "call_id": "call-1", "result": "ok", "index": 9})
        );
    }

    #[test]
    fn transform_stream_run_events_for_client_emits_usage_and_terminal_status() {
        let transformed = transform_stream_run_events_for_client(
            "run-123",
            vec![
                json!({
                    "event_type": "run_error",
                    "data": {"error": "boom"},
                    "index": 10
                }),
                json!({
                    "event_type": "run_finished",
                    "data": {"prompt_tokens": 7, "completion_tokens": 3, "tool_call_count": 2},
                    "index": 11
                }),
                json!({
                    "event_type": "run_finished",
                    "data": {"cancelled": true, "prompt_tokens": 1, "completion_tokens": 0},
                    "index": 12
                }),
            ],
        );

        assert_eq!(
            transformed[0],
            json!({"type": "run_error", "message": "boom", "error": "boom", "code": "RUN_ERROR", "run_id": "run-123", "index": 10})
        );
        assert_eq!(
            transformed[1],
            json!({"type": "usage", "prompt_tokens": 7, "completion_tokens": 3, "tool_call_count": 2, "index": 11})
        );
        assert_eq!(
            transformed[2],
            json!({"type": "run_finished", "run_id": "run-123", "status": "failed", "error": "boom", "index": 11})
        );
        assert_eq!(
            transformed[3],
            json!({"type": "usage", "prompt_tokens": 1, "completion_tokens": 0, "index": 12})
        );
        assert_eq!(
            transformed[4],
            json!({"type": "run_finished", "run_id": "run-123", "status": "cancelled", "index": 12})
        );
    }

    #[test]
    fn transform_stream_run_events_for_client_emits_interrupted_pause_status() {
        let transformed = transform_stream_run_events_for_client(
            "run-123",
            vec![
                json!({
                    "event_type": "run_interrupted",
                    "data": {
                        "kind": "budget_exhausted",
                        "resumable": true,
                        "user_message": "You can continue in the next message."
                    },
                    "index": 10
                }),
                json!({
                    "event_type": "run_finished",
                    "data": {
                        "interrupted": true,
                        "interruption_kind": "budget_exhausted",
                        "resumable": true,
                        "prompt_tokens": 7,
                        "completion_tokens": 3
                    },
                    "index": 11
                }),
            ],
        );

        assert_eq!(
            transformed[0],
            json!({
                "type": "run_interrupted",
                "run_id": "run-123",
                "kind": "budget_exhausted",
                "resumable": true,
                "user_message": "You can continue in the next message.",
                "message": "You can continue in the next message.",
                "index": 10
            })
        );
        assert_eq!(
            transformed[1],
            json!({"type": "usage", "prompt_tokens": 7, "completion_tokens": 3, "index": 11})
        );
        assert_eq!(
            transformed[2],
            json!({
                "type": "run_finished",
                "run_id": "run-123",
                "status": "paused",
                "interrupted": true,
                "interruption_kind": "budget_exhausted",
                "resumable": true,
                "index": 11
            })
        );
    }

    #[test]
    fn transform_stream_run_events_for_client_injects_run_id_into_run_started() {
        let transformed = transform_stream_run_events_for_client(
            "run-123",
            vec![json!({
                "event_type": "run_started",
                "data": {},
                "index": 1
            })],
        );

        assert_eq!(
            transformed[0],
            json!({"type": "run_started", "run_id": "run-123", "index": 1})
        );
    }

    #[test]
    fn transform_stream_run_events_for_client_injects_run_id_into_pause_resume_events() {
        let transformed = transform_stream_run_events_for_client(
            "run-123",
            vec![
                json!({
                    "event_type": "run_paused",
                    "data": {},
                    "index": 2
                }),
                json!({
                    "event_type": "run_waiting",
                    "data": {"reason": "waiting: executor_offline"},
                    "index": 3
                }),
                json!({
                    "event_type": "run_resumed",
                    "data": {},
                    "index": 4
                }),
            ],
        );

        assert_eq!(
            transformed[0],
            json!({"type": "run_paused", "run_id": "run-123", "index": 2})
        );
        assert_eq!(
            transformed[1],
            json!({
                "type": "run_waiting",
                "run_id": "run-123",
                "reason": "waiting: executor_offline",
                "index": 3
            })
        );
        assert_eq!(
            transformed[2],
            json!({"type": "run_resumed", "run_id": "run-123", "index": 4})
        );
    }

    #[test]
    fn transform_stream_run_events_for_client_emits_input_queued_with_run_id() {
        let transformed = transform_stream_run_events_for_client(
            "run-123",
            vec![json!({
                "event_type": "run_input_queued",
                "data": {"waiting_for": "user_input"},
                "index": 5
            })],
        );

        assert_eq!(
            transformed,
            vec![json!({
                "type": "run_input_queued",
                "run_id": "run-123",
                "waiting_for": "user_input",
                "index": 5
            })]
        );
    }

    #[test]
    fn transform_stream_run_events_for_client_injects_run_id_into_blocked_events() {
        let transformed = transform_stream_run_events_for_client(
            "run-123",
            vec![
                json!({
                    "event_type": "run_blocked", "reason": "fallback_disabled",
                    "data": {
                        "message": "Server fallback is disabled.",
                        "reason": "fallback_disabled"
                    },
                    "index": 4
                }),
                json!({
                    "type": "run_blocked", "reason": "transport_disconnected",
                    "reason": "transport_disconnected",
                    "index": 5
                }),
                json!({
                    "event_type": "run_blocked", "reason": "route_mismatch",
                    "data": {
                        "reason": "route_mismatch",
                        "workspace": {"kind": "cloud_workspace"},
                        "executor": {"kind": "orchestrator_managed", "status": "degraded"}
                    },
                    "index": 6
                }),
            ],
        );

        assert_eq!(
            transformed[0],
            json!({
                "type": "run_blocked", "reason": "fallback_disabled",
                "run_id": "run-123",
                "message": "Server fallback is disabled.",
                "reason": "fallback_disabled",
                "index": 4
            })
        );
        assert_eq!(
            transformed[1],
            json!({
                "type": "run_blocked", "reason": "transport_disconnected",
                "run_id": "run-123",
                "reason": "transport_disconnected",
                "index": 5
            })
        );
        assert_eq!(
            transformed[2],
            json!({
                "type": "run_blocked", "reason": "route_mismatch",
                "run_id": "run-123",
                "reason": "route_mismatch",
                "workspace": {"kind": "cloud_workspace"},
                "executor": {"kind": "orchestrator_managed", "status": "degraded"},
                "index": 6
            })
        );
    }

    #[tokio::test]
    async fn stream_run_http_replays_durable_text_done_after_cache_eviction() {
        use crate::server::run::engine::RunEngine;
        use crate::server::run::lifecycle::AgenticRunLifecycleService;
        use astra_services::runs::InMemoryRunStateStore;

        let engine = RunEngine::new(Arc::new(InMemoryRunStateStore::new()));
        engine
            .start_run("run-durable-http", "u1", "session-http")
            .await
            .expect("start durable run");
        engine
            .append_event(
                "u1",
                "run-durable-http",
                json!({"event_type": "text_done", "data": {"full_text": "durable final answer"}}),
            )
            .await
            .expect("persist text_done");
        engine
            .append_event(
                "u1",
                "run-durable-http",
                json!({"event_type": "run_finished", "data": {"prompt_tokens": 2, "completion_tokens": 1}}),
            )
            .await
            .expect("persist run_finished");
        engine
            .persist_status(
                "u1",
                "run-durable-http",
                astra_core::STATUS_COMPLETED,
                None,
                None,
            )
            .await
            .expect("mark completed");

        let lifecycle = AgenticRunLifecycleService::new(
            test_matrixone(),
            Arc::new(
                crate::FernetTokenEncryptor::new("0123456789abcdef")
                    .expect("test encryptor should initialize"),
            ),
            Arc::new(TokioMutex::new(HashMap::new())),
            engine,
        );

        let app = build_app(
            AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
                .with_auth_service(Arc::new(StubAuthService))
                .with_run_lifecycle_service(Arc::new(lifecycle)),
        );

        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/chat/runs/run-durable-http/stream?last_index=1")
                    .header("authorization", "Bearer good-token")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("response should be returned");

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some("text/event-stream")
        );
        let body = body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .expect("body should be readable");
        let text = String::from_utf8(body.to_vec()).expect("sse should be utf8");
        assert!(
            text.contains("\"type\":\"text_done\""),
            "durable replay should keep final answer event: {text}"
        );
        assert!(
            text.contains("\"full_text\":\"durable final answer\""),
            "durable replay should expose final answer text to the client: {text}"
        );
        assert!(
            text.contains("\"type\":\"usage\""),
            "run_finished usage should still be transformed for the client: {text}"
        );
        assert!(
            text.contains("\"type\":\"run_finished\""),
            "terminal lifecycle event should still reach the client: {text}"
        );
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
    async fn stream_run_http_replays_matrixone_cache_miss_durable_events() {
        use crate::server::run::engine::RunEngine;
        use crate::server::run::lifecycle::AgenticRunLifecycleService;
        use astra_services::runs::{DatabaseRunStateStore, RunStateStore};
        use sqlx::Row;
        use uuid::Uuid;

        let shared_pool = setup_http_run_db_it().await;
        let settings = shared_pool.settings().clone();
        let user_id = "u1";
        let run_id = format!("run-replay-http-it-{}", Uuid::new_v4());
        let session_id = format!("session-replay-http-it-{}", Uuid::new_v4());
        cleanup_run_http_fixture(&shared_pool, user_id, &run_id).await;

        let store: Arc<dyn RunStateStore> = Arc::new(
            DatabaseRunStateStore::new(shared_pool.clone())
                .with_owner_pod_id("stream-replay-http-it-pod"),
        );
        let engine = RunEngine::new(store);
        engine
            .start_run(&run_id, user_id, &session_id)
            .await
            .expect("start durable DB run");
        let durable_events = vec![
            json!({
                "type": "tool_call",
                "tool_call": {"id": "call-1", "name": "bash", "arguments": {"cmd": "printf ok"}}
            }),
            json!({
                "type": "tool_call_end",
                "call_id": "call-1",
                "tool": "bash",
                "result": "ok"
            }),
            json!({
                "event_type": "text_done",
                "data": {"full_text": "durable final answer from matrixone"}
            }),
            json!({
                "event_type": "run_finished",
                "data": {"prompt_tokens": 2, "completion_tokens": 1, "tool_call_count": 1}
            }),
        ];
        let transitioned = engine
            .transition_status_with_events_if_current(
                user_id,
                &run_id,
                &[astra_core::STATUS_RUNNING],
                astra_core::STATUS_COMPLETED,
                None,
                None,
                &durable_events,
            )
            .await
            .expect("commit terminal durable DB events");
        assert!(transitioned);

        let persisted_event_types = sqlx::query(
            "SELECT event_type FROM agent_run_events
             WHERE user_id = ? AND run_id = ?
             ORDER BY event_idx ASC",
        )
        .bind(user_id)
        .bind(&run_id)
        .fetch_all(shared_pool.get())
        .await
        .expect("load persisted event types")
        .into_iter()
        .map(|row| row.try_get::<String, _>("event_type").expect("event_type"))
        .collect::<Vec<_>>();
        assert_eq!(
            persisted_event_types,
            vec![
                "run_started",
                "tool_call",
                "tool_call_end",
                "text_done",
                "run_finished",
            ],
            "cache-miss replay must be backed by semantic durable facts only"
        );

        let lifecycle = AgenticRunLifecycleService::new(
            settings,
            Arc::new(
                crate::FernetTokenEncryptor::new("0123456789abcdef")
                    .expect("test encryptor should initialize"),
            ),
            Arc::new(TokioMutex::new(HashMap::new())),
            engine,
        );
        let app = build_app(
            AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
                .with_auth_service(Arc::new(StubAuthService))
                .with_run_lifecycle_service(Arc::new(lifecycle)),
        );

        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(format!("/chat/runs/{run_id}/stream?last_index=1"))
                    .header("authorization", "Bearer good-token")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("response should be returned");

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some("text/event-stream")
        );
        let body = body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .expect("body should be readable");
        let text = String::from_utf8(body.to_vec()).expect("sse should be utf8");
        assert!(
            text.contains("\"type\":\"tool_call\""),
            "durable replay should expose tool start boundary: {text}"
        );
        assert!(
            text.contains("\"type\":\"tool_call_end\""),
            "durable replay should expose tool end boundary: {text}"
        );
        assert!(
            text.contains("\"full_text\":\"durable final answer from matrixone\""),
            "durable replay should expose final answer text: {text}"
        );
        assert!(
            text.contains("\"type\":\"usage\""),
            "run_finished usage should still be transformed for the client: {text}"
        );
        assert!(
            text.contains("\"type\":\"run_finished\"") && text.contains("\"status\":\"completed\""),
            "terminal lifecycle event should still reach the client: {text}"
        );
        assert!(
            !text.contains("text_delta") && !text.contains("agent_live_event"),
            "cache-miss replay should not require transport-only live deltas: {text}"
        );

        cleanup_run_http_fixture(&shared_pool, user_id, &run_id).await;
    }

    #[tokio::test]
    async fn get_run_projection_http_returns_bounded_projection_and_filters_internal_events() {
        use crate::server::run::engine::RunEngine;
        use crate::server::run::lifecycle::AgenticRunLifecycleService;
        use astra_services::runs::InMemoryRunStateStore;

        let engine = RunEngine::new(Arc::new(InMemoryRunStateStore::new()));
        engine
            .start_run("run-projection-http", "u1", "session-projection")
            .await
            .expect("start durable run");
        engine
            .append_event(
                "u1",
                "run-projection-http",
                json!({
                    "event_type": "workspace_bound",
                    "data": {
                        "workspace": {
                            "kind": "edge_workspace",
                            "display_name": "MacBook Pro",
                            "cwd": "/Users/xupeng/github/astra",
                            "authority": "read_write",
                            "fallback_policy": "disabled"
                        },
                        "executor": {
                            "kind": "edge_agent",
                            "executor_id": "edge-macbook-1",
                            "display_name": "MacBook Pro",
                            "transport": "edge_ws",
                            "status": "online"
                        },
                        "transport": "edge_ws",
                        "fallback_policy": "disabled"
                    }
                }),
            )
            .await
            .expect("persist binding event");
        engine
            .append_event(
                "u1",
                "run-projection-http",
                json!({"event_type": "injection_freshness", "data": {"fingerprint": "secret"}}),
            )
            .await
            .expect("persist internal event");
        engine
            .append_event(
                "u1",
                "run-projection-http",
                json!({"event_type": "text_done", "data": {"full_text": "durable answer"}}),
            )
            .await
            .expect("persist final answer");
        engine
            .append_event(
                "u1",
                "run-projection-http",
                json!({"event_type": "run_error", "data": {"error": "boom"}}),
            )
            .await
            .expect("persist error");
        engine
            .append_event(
                "u1",
                "run-projection-http",
                json!({"event_type": "run_finished", "data": {"prompt_tokens": 5, "completion_tokens": 2}}),
            )
            .await
            .expect("persist run finished");
        engine
            .persist_usage("u1", "run-projection-http", 5, 2, 0)
            .await
            .expect("persist usage");
        engine
            .persist_checkpoint(
                "u1",
                "run-projection-http",
                r#"{"version":"checkpoint_v3","graceful":true,"last_batch_id":"batch-run-projection"}"#,
            )
            .await
            .expect("persist checkpoint");
        engine
            .persist_status(
                "u1",
                "run-projection-http",
                astra_core::STATUS_FAILED,
                None,
                Some("boom"),
            )
            .await
            .expect("mark failed");

        let lifecycle = AgenticRunLifecycleService::new(
            test_matrixone(),
            Arc::new(
                crate::FernetTokenEncryptor::new("0123456789abcdef")
                    .expect("test encryptor should initialize"),
            ),
            Arc::new(TokioMutex::new(HashMap::new())),
            engine,
        );

        let app = build_app(
            AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
                .with_auth_service(Arc::new(StubAuthService))
                .with_run_lifecycle_service(Arc::new(lifecycle)),
        );

        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/chat/runs/run-projection-http/projection?recent_limit=4")
                    .header("authorization", "Bearer good-token")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("response should be returned");

        assert_eq!(resp.status(), StatusCode::OK);
        let body = body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .expect("body should be readable");
        let json: serde_json::Value =
            serde_json::from_slice(&body).expect("projection response should be valid json");
        assert_eq!(json.get("status"), Some(&json!("failed")));
        assert_eq!(json.get("latest_event_type"), Some(&json!("run_finished")));
        assert_eq!(json.get("run_event_high_watermark"), Some(&json!(5)));
        assert_eq!(
            json.pointer("/workspace/kind"),
            Some(&json!("edge_workspace"))
        );
        assert_eq!(
            json.pointer("/workspace/cwd"),
            Some(&json!("/Users/xupeng/github/astra"))
        );
        assert_eq!(json.pointer("/executor/kind"), Some(&json!("edge_agent")));
        assert_eq!(
            json.pointer("/executor/executor_id"),
            Some(&json!("edge-macbook-1"))
        );
        assert_eq!(json.get("transport"), Some(&json!("edge_ws")));
        assert_eq!(json.get("fallback_policy"), Some(&json!("disabled")));
        assert_eq!(
            json.pointer("/latest_checkpoint/checkpoint_version"),
            Some(&json!("checkpoint_v3"))
        );
        assert_eq!(
            json.pointer("/observability/projection_lag_events"),
            Some(&json!(0))
        );
        assert_eq!(
            json.pointer("/observability/observability_available"),
            Some(&json!(false))
        );
        assert_eq!(
            json.pointer("/observability/prompt_request_count"),
            Some(&json!(0))
        );
        let recent_events = json
            .get("recent_events")
            .and_then(serde_json::Value::as_array)
            .expect("recent_events should be an array");
        assert_eq!(recent_events.len(), 4);
        assert!(
            recent_events.iter().all(|event| {
                event.get("type").and_then(serde_json::Value::as_str) != Some("workspace_bound")
            }),
            "projection top-level binding must not depend on the bounded recent event window"
        );
        assert!(
            recent_events.iter().all(|event| {
                event.get("type").and_then(serde_json::Value::as_str) != Some("injection_freshness")
            }),
            "internal events must stay out of the public projection payload"
        );
        assert!(
            recent_events
                .iter()
                .any(|event| event.get("type") == Some(&json!("usage"))),
            "run projection should expose transformed usage data"
        );
        assert!(
            recent_events
                .iter()
                .any(|event| event.get("full_text") == Some(&json!("durable answer"))),
            "run projection should keep bounded durable transcript data"
        );
    }

    #[tokio::test]
    async fn repair_run_projection_http_rebuilds_and_returns_projection() {
        use crate::server::run::engine::RunEngine;
        use crate::server::run::lifecycle::AgenticRunLifecycleService;
        use astra_services::runs::InMemoryRunStateStore;

        let engine = RunEngine::new(Arc::new(InMemoryRunStateStore::new()));
        engine
            .start_run("run-projection-repair-http", "u1", "session-projection")
            .await
            .expect("start durable run");
        engine
            .transition_status_with_events_if_current(
                "u1",
                "run-projection-repair-http",
                &[astra_core::STATUS_RUNNING],
                astra_core::STATUS_FAILED,
                None,
                Some("boom"),
                &[
                    json!({"event_type": "run_error", "data": {"error": "boom"}}),
                    json!({"event_type": "run_finished", "data": {"status": "failed"}}),
                ],
            )
            .await
            .expect("persist terminal transition");

        let lifecycle = AgenticRunLifecycleService::new(
            test_matrixone(),
            Arc::new(
                crate::FernetTokenEncryptor::new("0123456789abcdef")
                    .expect("test encryptor should initialize"),
            ),
            Arc::new(TokioMutex::new(HashMap::new())),
            engine,
        );

        let app = build_app(
            AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
                .with_auth_service(Arc::new(StubAuthService))
                .with_run_lifecycle_service(Arc::new(lifecycle)),
        );

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/chat/runs/run-projection-repair-http/projection/repair?recent_limit=2")
                    .header("authorization", "Bearer good-token")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("response should be returned");

        assert_eq!(resp.status(), StatusCode::OK);
        let body = body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .expect("body should be readable");
        let json: serde_json::Value =
            serde_json::from_slice(&body).expect("repair response should be valid json");
        assert_eq!(json.get("repaired"), Some(&json!(true)));
        assert_eq!(json.pointer("/projection/status"), Some(&json!("failed")));
        assert_eq!(
            json.pointer("/projection/latest_event_type"),
            Some(&json!("run_finished"))
        );
        assert_eq!(
            json.pointer("/projection/observability/projection_lag_events"),
            Some(&json!(0))
        );
        let recent_events = json
            .pointer("/projection/recent_events")
            .and_then(serde_json::Value::as_array)
            .expect("recent events should be present");
        assert_eq!(recent_events.len(), 2);
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
    async fn repair_run_projection_http_repairs_real_database_projection() {
        use crate::server::run::engine::RunEngine;
        use crate::server::run::lifecycle::AgenticRunLifecycleService;
        use astra_services::runs::{DatabaseRunStateStore, RunStateStore};
        use sqlx::Row;
        use uuid::Uuid;

        let shared_pool = setup_http_run_db_it().await;
        let settings = shared_pool.settings().clone();
        let user_id = "u1";
        let run_id = format!("run-http-it-{}", Uuid::new_v4());
        let session_id = format!("session-http-it-{}", Uuid::new_v4());
        cleanup_run_http_fixture(&shared_pool, user_id, &run_id).await;

        let store: Arc<dyn RunStateStore> = Arc::new(
            DatabaseRunStateStore::new(shared_pool.clone())
                .with_owner_pod_id("projection-repair-http-it-pod"),
        );
        let engine = RunEngine::new(store);
        engine
            .start_run(&run_id, user_id, &session_id)
            .await
            .expect("start durable DB run");
        let checkpoint_saved = engine
            .persist_checkpoint(
                user_id,
                &run_id,
                r#"{"version":"checkpoint_v2","graceful":true,"last_batch_id":"http-repair-it"}"#,
            )
            .await
            .expect("save checkpoint before terminal transition");
        assert!(checkpoint_saved);
        let transitioned = engine
            .transition_status_with_events_if_current(
                user_id,
                &run_id,
                &[astra_core::STATUS_RUNNING],
                astra_core::STATUS_FAILED,
                None,
                Some("boom"),
                &[
                    json!({"event_type": "run_error", "data": {"error": "boom"}}),
                    json!({"event_type": "run_finished", "data": {"status": "failed"}}),
                ],
            )
            .await
            .expect("persist terminal transition");
        assert!(transitioned);

        sqlx::query(
            "UPDATE run_display_projections
             SET status = 'running',
                 error_message = NULL,
                 projection_event_idx = -1,
                 latest_event_type = 'stale_event',
                 latest_checkpoint_id = NULL,
                 latest_checkpoint_kind = NULL,
                 latest_checkpoint_version = NULL
             WHERE user_id = ? AND run_id = ?",
        )
        .bind(user_id)
        .bind(&run_id)
        .execute(shared_pool.get())
        .await
        .expect("corrupt projection");

        let lifecycle = AgenticRunLifecycleService::new(
            settings,
            Arc::new(
                crate::FernetTokenEncryptor::new("0123456789abcdef")
                    .expect("test encryptor should initialize"),
            ),
            Arc::new(TokioMutex::new(HashMap::new())),
            engine,
        );

        let app = build_app(
            AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
                .with_auth_service(Arc::new(StubAuthService))
                .with_shared_pool(shared_pool.clone())
                .with_run_lifecycle_service(Arc::new(lifecycle)),
        );

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!(
                        "/chat/runs/{run_id}/projection/repair?recent_limit=2"
                    ))
                    .header("authorization", "Bearer good-token")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("response should be returned");

        assert_eq!(resp.status(), StatusCode::OK);
        let body = body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .expect("body should be readable");
        let json: serde_json::Value =
            serde_json::from_slice(&body).expect("repair response should be valid json");
        assert_eq!(json.get("repaired"), Some(&json!(true)));
        assert_eq!(json.pointer("/projection/status"), Some(&json!("failed")));
        assert_eq!(
            json.pointer("/projection/error_message"),
            Some(&json!("boom"))
        );
        assert_eq!(
            json.pointer("/projection/latest_event_type"),
            Some(&json!("run_finished"))
        );
        assert_eq!(
            json.pointer("/projection/latest_checkpoint/checkpoint_version"),
            Some(&json!("checkpoint_v2"))
        );
        assert_eq!(
            json.pointer("/projection/observability/projection_lag_events"),
            Some(&json!(0))
        );
        let recent_events = json
            .pointer("/projection/recent_events")
            .and_then(serde_json::Value::as_array)
            .expect("recent events should be present");
        assert_eq!(recent_events.len(), 2);

        let row = sqlx::query(
            "SELECT status, error_message, projection_event_idx, latest_event_type,
                    latest_checkpoint_version
             FROM run_display_projections
             WHERE user_id = ? AND run_id = ?",
        )
        .bind(user_id)
        .bind(&run_id)
        .fetch_one(shared_pool.get())
        .await
        .expect("load repaired projection row");
        let db_status: String = row.try_get("status").expect("status");
        let db_error: Option<String> = row.try_get("error_message").expect("error_message");
        let db_event_idx: i64 = row
            .try_get("projection_event_idx")
            .expect("projection_event_idx");
        let db_latest_event: Option<String> =
            row.try_get("latest_event_type").expect("latest_event_type");
        let db_checkpoint_version: Option<String> = row
            .try_get("latest_checkpoint_version")
            .expect("latest_checkpoint_version");
        assert_eq!(db_status, astra_core::STATUS_FAILED);
        assert_eq!(db_error.as_deref(), Some("boom"));
        // `RunEngine::start_run` persists `run_started` at index 0; the
        // terminal batch then writes `run_error` and `run_finished`.
        assert_eq!(db_event_idx, 2);
        assert_eq!(db_latest_event.as_deref(), Some("run_finished"));
        assert_eq!(db_checkpoint_version.as_deref(), Some("checkpoint_v2"));

        cleanup_run_http_fixture(&shared_pool, user_id, &run_id).await;
    }

    #[tokio::test]
    async fn get_run_projection_http_hides_foreign_run() {
        use crate::server::run::engine::RunEngine;
        use crate::server::run::lifecycle::AgenticRunLifecycleService;
        use astra_services::runs::InMemoryRunStateStore;

        let engine = RunEngine::new(Arc::new(InMemoryRunStateStore::new()));
        engine
            .start_run("run-foreign", "u2", "session-foreign")
            .await
            .expect("start durable run");
        let lifecycle = AgenticRunLifecycleService::new(
            test_matrixone(),
            Arc::new(
                crate::FernetTokenEncryptor::new("0123456789abcdef")
                    .expect("test encryptor should initialize"),
            ),
            Arc::new(TokioMutex::new(HashMap::new())),
            engine,
        );

        let app = build_app(
            AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
                .with_auth_service(Arc::new(StubAuthService))
                .with_run_lifecycle_service(Arc::new(lifecycle)),
        );

        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/chat/runs/run-foreign/projection")
                    .header("authorization", "Bearer good-token")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("response should be returned");

        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn repair_run_projection_http_hides_foreign_run() {
        use crate::server::run::engine::RunEngine;
        use crate::server::run::lifecycle::AgenticRunLifecycleService;
        use astra_services::runs::InMemoryRunStateStore;

        let engine = RunEngine::new(Arc::new(InMemoryRunStateStore::new()));
        engine
            .start_run("run-foreign-repair", "u2", "session-foreign")
            .await
            .expect("start durable run");
        let lifecycle = AgenticRunLifecycleService::new(
            test_matrixone(),
            Arc::new(
                crate::FernetTokenEncryptor::new("0123456789abcdef")
                    .expect("test encryptor should initialize"),
            ),
            Arc::new(TokioMutex::new(HashMap::new())),
            engine,
        );

        let app = build_app(
            AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
                .with_auth_service(Arc::new(StubAuthService))
                .with_run_lifecycle_service(Arc::new(lifecycle)),
        );

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/chat/runs/run-foreign-repair/projection/repair")
                    .header("authorization", "Bearer good-token")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("response should be returned");

        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
