use super::*;
use astra_services::runs::transform_run_event_for_client;

pub(super) fn transform_stream_run_events_for_client(
    run_id: &str,
    events: Vec<serde_json::Value>,
) -> Vec<serde_json::Value> {
    let mut transformed_events = Vec::with_capacity(events.len());
    let mut pending_run_error: Option<String> = None;

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
            pending_run_error = event
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
        )
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
    use super::*;
    use serde_json::json;

    #[test]
    fn transform_stream_run_events_for_client_uses_client_protocol_shape() {
        let transformed = transform_stream_run_events_for_client("run-123", vec![
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
        ]);

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
        let transformed = transform_stream_run_events_for_client("run-123", vec![json!({
            "event_type": "tool_result",
            "data": {"call_id": "call-1", "result": "ok"},
            "index": 9
        })]);

        assert_eq!(
            transformed[0],
            json!({"type": "tool_call_end", "call_id": "call-1", "result": "ok", "index": 9})
        );
    }

    #[test]
    fn transform_stream_run_events_for_client_emits_usage_and_terminal_status() {
        let transformed = transform_stream_run_events_for_client("run-123", vec![
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
        ]);

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
        let transformed = transform_stream_run_events_for_client("run-123", vec![json!({
            "event_type": "run_started",
            "data": {},
            "index": 1
        })]);

        assert_eq!(
            transformed[0],
            json!({"type": "run_started", "run_id": "run-123", "index": 1})
        );
    }

    #[test]
    fn transform_stream_run_events_for_client_injects_run_id_into_pause_resume_events() {
        let transformed = transform_stream_run_events_for_client("run-123", vec![
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
        ]);

        assert_eq!(
            transformed[0],
            json!({"type": "run_paused", "run_id": "run-123", "index": 2})
        );
        assert_eq!(
            transformed[1],
            json!({"type": "run_resumed", "run_id": "run-123", "index": 3})
        );
    }
}
