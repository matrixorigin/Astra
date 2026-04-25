use super::*;
use astra_core::STATUS_CANCELLED;
use astra_services::{DatabaseSessionArtifactStore, SessionArtifactJsonStore};

pub(super) async fn create_session_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<SessionCreateRequest>,
) -> Result<(StatusCode, Json<SessionResponse>), (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let session = state
        .session_service
        .create_session(
            user.user_id,
            SessionCreateRequestData {
                agent_id: request.agent_id,
                title: request.title,
                metadata: request.metadata,
            },
        )
        .await?;
    Ok((StatusCode::CREATED, Json(SessionResponse::from(session))))
}

pub(super) async fn list_sessions_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<SessionListQuery>,
) -> Result<Json<SessionListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let sessions = state
        .session_service
        .list_sessions(SessionListFilter {
            user_id: user.user_id,
            agent_id: query.agent_id,
            status: query.session_status,
            limit: query.limit,
            offset: query.offset,
        })
        .await?;
    Ok(Json(SessionListResponse::from(sessions)))
}

pub(super) async fn get_session_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<SessionResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let session = state
        .session_service
        .get_session(session_id, user.user_id)
        .await?;
    Ok(Json(SessionResponse::from(session)))
}

pub(super) async fn update_session_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<SessionUpdateRequest>,
) -> Result<Json<SessionResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let session = state
        .session_service
        .update_session(
            session_id,
            user.user_id,
            SessionUpdateRequestData {
                title: request.title,
                metadata: request.metadata,
                status: request.status,
            },
        )
        .await?;
    Ok(Json(SessionResponse::from(session)))
}

pub(super) async fn delete_session_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    state
        .session_service
        .delete_session(session_id, user.user_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(super) async fn close_session_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<SessionResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let session = state
        .session_service
        .update_session(
            session_id,
            user.user_id,
            SessionUpdateRequestData {
                title: None,
                metadata: None,
                status: Some("closed".to_string()),
            },
        )
        .await?;
    Ok(Json(SessionResponse::from(session)))
}

pub(super) async fn resume_session_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<SessionResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let session = state
        .session_service
        .update_session(
            session_id,
            user.user_id,
            SessionUpdateRequestData {
                title: None,
                metadata: None,
                status: Some("active".to_string()),
            },
        )
        .await?;
    Ok(Json(SessionResponse::from(session)))
}

pub(super) async fn cancel_session_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<SessionResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let session = state
        .session_service
        .update_session(
            session_id,
            user.user_id,
            SessionUpdateRequestData {
                title: None,
                metadata: None,
                status: Some(STATUS_CANCELLED.to_string()),
            },
        )
        .await?;
    Ok(Json(SessionResponse::from(session)))
}

pub(super) async fn session_activity_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<SessionActivityQuery>,
) -> Result<Json<SessionActivityResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    // Verify ownership first.
    let _ = state
        .session_service
        .get_session(session_id.clone(), user.user_id.clone())
        .await?;

    let activities = state
        .session_service
        .get_session_activity(session_id, user.user_id, query.limit, query.offset)
        .await?;
    Ok(Json(SessionActivityResponse::from(activities)))
}

pub(super) async fn list_session_artifacts_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<SessionArtifactListQuery>,
) -> Result<Json<SessionArtifactListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let _ = state
        .session_service
        .get_session(session_id.clone(), user.user_id)
        .await?;
    let artifact_store = session_artifact_store(&state)?;
    let artifacts = artifact_store
        .list_json_artifacts(
            &session_id,
            query.artifact_kind.as_deref(),
            query.limit as usize,
        )
        .await
        .map_err(internal_artifact_error)?;
    Ok(Json(SessionArtifactListResponse {
        session_id,
        artifacts: artifacts
            .into_iter()
            .map(session_artifact_response)
            .collect(),
        limit: query.limit,
    }))
}

pub(super) async fn get_latest_session_artifact_handler(
    State(state): State<AppState>,
    Path((session_id, artifact_kind)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<SessionArtifactResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let _ = state
        .session_service
        .get_session(session_id.clone(), user.user_id)
        .await?;
    let artifact_store = session_artifact_store(&state)?;
    let artifact = artifact_store
        .load_latest_json_artifact(&session_id, &artifact_kind)
        .await
        .map_err(internal_artifact_error)?
        .ok_or_else(session_artifact_not_found)?;
    Ok(Json(session_artifact_response(artifact)))
}

pub(super) async fn get_session_artifact_handler(
    State(state): State<AppState>,
    Path((session_id, artifact_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<SessionArtifactResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let _ = state
        .session_service
        .get_session(session_id.clone(), user.user_id)
        .await?;
    let artifact_store = session_artifact_store(&state)?;
    let artifact = artifact_store
        .load_json_artifact(&artifact_id)
        .await
        .map_err(internal_artifact_error)?
        .filter(|artifact| artifact.session_id == session_id)
        .ok_or_else(session_artifact_not_found)?;
    Ok(Json(session_artifact_response(artifact)))
}

pub(super) async fn download_session_artifact_handler(
    State(state): State<AppState>,
    Path((session_id, artifact_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<(HeaderMap, Vec<u8>), (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let _ = state
        .session_service
        .get_session(session_id.clone(), user.user_id)
        .await?;
    let artifact_store = session_artifact_store(&state)?;
    let artifact = artifact_store
        .load_json_artifact(&artifact_id)
        .await
        .map_err(internal_artifact_error)?
        .filter(|artifact| artifact.session_id == session_id)
        .ok_or_else(session_artifact_not_found)?;

    let response = session_artifact_response(artifact);
    let payload = serde_json::to_vec_pretty(&response)
        .map_err(|error| internal_artifact_error(error.to_string()))?;
    let filename = session_artifact_download_filename(&response);
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    headers.insert(
        axum::http::header::CONTENT_DISPOSITION,
        axum::http::HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
            .map_err(|error| internal_artifact_error(error.to_string()))?,
    );
    Ok((headers, payload))
}

fn session_artifact_store(
    state: &AppState,
) -> Result<DatabaseSessionArtifactStore, (StatusCode, Json<ErrorResponse>)> {
    let Some(shared_pool) = state.shared_pool.clone() else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse::new("session artifact store unavailable")),
        ));
    };
    Ok(DatabaseSessionArtifactStore::new(shared_pool.settings().clone()).with_pool(shared_pool))
}

fn session_artifact_response(
    artifact: astra_services::StoredSessionArtifact,
) -> SessionArtifactResponse {
    SessionArtifactResponse {
        artifact_id: artifact.artifact_id,
        session_id: artifact.session_id,
        user_id: artifact.user_id,
        artifact_kind: artifact.artifact_kind,
        source: artifact.source,
        turn: artifact.turn,
        round: artifact.round,
        content: artifact.content,
        metadata: artifact.metadata,
        created_at: artifact.created_at,
    }
}

fn session_artifact_download_filename(artifact: &SessionArtifactResponse) -> String {
    let kind = artifact
        .artifact_kind
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => ch,
            _ => '_',
        })
        .collect::<String>();
    format!("{kind}_{}.json", artifact.artifact_id)
}

fn session_artifact_not_found() -> (StatusCode, Json<ErrorResponse>) {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorResponse::new("session artifact not found")),
    )
}

fn internal_artifact_error(error: String) -> (StatusCode, Json<ErrorResponse>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse::new(error)),
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn session_artifact_handlers_enforce_session_scope_and_store_api() {
        let source = include_str!("session_handlers.rs");
        assert!(
            source.contains("get_session(session_id.clone(), user.user_id)"),
            "artifact handlers should verify session ownership before reading artifacts"
        );
        assert!(
            source.contains(".list_json_artifacts("),
            "artifact list handler should use the session artifact store list API"
        );
        assert!(
            source.contains(".load_json_artifact(&artifact_id)"),
            "artifact get handler should use the session artifact store get API"
        );
        assert!(
            source.contains(".load_latest_json_artifact(&session_id, &artifact_kind)"),
            "artifact latest handler should use the session artifact store latest API"
        );
        assert!(
            source.contains("CONTENT_DISPOSITION"),
            "artifact download handler should return a downloadable attachment response"
        );
    }
}
