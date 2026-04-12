use super::*;
use astra_services::runs::transform_run_event_for_client;

fn transform_stream_run_events_for_client(events: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    events
        .into_iter()
        .map(|event| {
            let index = event.get("index").cloned();
            let mut transformed = transform_run_event_for_client(event);
            if let Some(index) = index
                && let Some(obj) = transformed.as_object_mut()
            {
                obj.insert("index".to_string(), index);
            }
            transformed
        })
        .collect()
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
        .stream_run(run_id, user.user_id, query.last_index)
        .await
    {
        Ok(events) => sse_json_response(transform_stream_run_events_for_client(events)),
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
        let transformed = transform_stream_run_events_for_client(vec![
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
        let transformed = transform_stream_run_events_for_client(vec![json!({
            "event_type": "tool_result",
            "data": {"call_id": "call-1", "result": "ok"},
            "index": 9
        })]);

        assert_eq!(
            transformed[0],
            json!({"type": "tool_call_end", "call_id": "call-1", "result": "ok", "index": 9})
        );
    }
}
