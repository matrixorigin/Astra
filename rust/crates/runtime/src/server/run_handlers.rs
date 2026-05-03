use super::*;
use astra_services::runs::transform_run_event_for_client;

pub(super) fn transform_stream_run_events_for_client(
    run_id: &str,
    events: Vec<serde_json::Value>,
) -> Vec<serde_json::Value> {
    let mut pending_run_error: Option<String> = None;
    transform_stream_run_events_for_client_with_pending(run_id, events, &mut pending_run_error)
}

pub(super) fn transform_stream_run_events_for_client_with_pending(
    run_id: &str,
    events: Vec<serde_json::Value>,
    pending_run_error: &mut Option<String>,
) -> Vec<serde_json::Value> {
    let mut transformed_events = Vec::with_capacity(events.len());

    for event in events {
        let index = event.get("index").cloned();
        let event_type = event
            .get("event_type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let run_finished_cancelled = event
            .get("data")
            .and_then(serde_json::Value::as_object)
            .and_then(|data| data.get("cancelled"))
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
        if matches!(
            event_type.as_str(),
            "run_started" | "run_paused" | "run_resumed" | "run_finished"
        ) && let Some(obj) = transformed.as_object_mut()
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

pub(super) async fn get_run_status_handler(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<RunStatusResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let run = state
        .run_lifecycle_service
        .get_run_status(run_id, user.user_id)
        .await?;
    Ok(Json(RunStatusResponse::from(run)))
}

pub(super) async fn stream_run_handler(
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
        .run_lifecycle_service
        .stream_run(run_id.clone(), user.user_id, query.last_index)
        .await
    {
        Ok(events) => sse_json_response(transform_stream_run_events_for_client(&run_id, events)),
        Err((status, error)) => sse_error_response(status, error.0.detail),
    }
}

pub(super) async fn cancel_run_handler(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<CancelRunResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let result = state
        .run_lifecycle_service
        .cancel_run(run_id, user.user_id)
        .await?;
    Ok(Json(CancelRunResponse::from(result)))
}

pub(super) async fn list_runs_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<RunListQuery>,
) -> Result<Json<RunListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let runs = state
        .run_lifecycle_service
        .list_runs(user.user_id, query.limit, query.offset)
        .await?;
    Ok(Json(RunListResponse::from(runs)))
}

pub(super) async fn pause_run_handler(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<RunMutationResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let result = state
        .run_lifecycle_service
        .pause_run(run_id, user.user_id)
        .await?;
    Ok(Json(RunMutationResponse::from(result)))
}

pub(super) async fn resume_run_handler(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<RunMutationResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let result = state
        .run_lifecycle_service
        .resume_run(run_id, user.user_id)
        .await?;
    Ok(Json(RunMutationResponse::from(result)))
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
            json!({"type": "error", "message": "boom", "code": "RUN_ERROR", "index": 8})
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
            json!({"type": "error", "message": "boom", "code": "RUN_ERROR", "index": 10})
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
                    "event_type": "run_resumed",
                    "data": {},
                    "index": 3
                }),
            ],
        );

        assert_eq!(
            transformed[0],
            json!({"type": "run_paused", "run_id": "run-123", "index": 2})
        );
        assert_eq!(
            transformed[1],
            json!({"type": "run_resumed", "run_id": "run-123", "index": 3})
        );
    }

    #[tokio::test]
    async fn stream_run_http_replays_durable_text_done_after_cache_eviction() {
        use crate::server::run_engine::RunEngine;
        use crate::server::run_lifecycle::AgenticRunLifecycleService;
        use astra_services::runs::InMemoryRunStateStore;

        let engine = RunEngine::new(Arc::new(InMemoryRunStateStore::new()));
        engine
            .start_run("run-durable-http", "u1", "session-http")
            .await
            .expect("start durable run");
        engine
            .append_event(
                "run-durable-http",
                json!({"event_type": "text_done", "data": {"full_text": "durable final answer"}}),
            )
            .await
            .expect("persist text_done");
        engine
            .append_event(
                "run-durable-http",
                json!({"event_type": "run_finished", "data": {"prompt_tokens": 2, "completion_tokens": 1}}),
            )
            .await
            .expect("persist run_finished");
        engine
            .persist_status("run-durable-http", astra_core::STATUS_COMPLETED, None, None)
            .await
            .expect("mark completed");

        let lifecycle = AgenticRunLifecycleService::new(
            test_matrixone(),
            Arc::new(
                crate::FernetTokenEncryptor::new("0123456789abcdef")
                    .expect("test encryptor should initialize"),
            ),
            Arc::new(TokioMutex::new(HashMap::new())),
        )
        .with_run_engine(engine);

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
}
