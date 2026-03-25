use super::*;

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
        Ok(events) => sse_json_response(events),
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
