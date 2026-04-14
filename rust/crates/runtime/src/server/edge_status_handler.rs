use super::*;

/// Response for `GET /edges/status`.
#[derive(Serialize)]
pub(super) struct EdgeStatusResponse {
    pub(super) edges: Vec<EdgeInfo>,
}

#[derive(Serialize)]
pub(super) struct EdgeInfo {
    pub(super) edge_agent_id: String,
    pub(super) hostname: Option<String>,
    pub(super) workspace_dir: Option<String>,
    pub(super) connected_secs: u64,
}

pub(super) async fn edge_status_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<EdgeStatusResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;

    let infos = state.edge_connection_pool.get_user_edges(&user.user_id);
    let edges = infos
        .into_iter()
        .map(|info| EdgeInfo {
            edge_agent_id: info.edge_agent_id,
            hostname: info.hostname,
            workspace_dir: info.workspace_dir,
            connected_secs: info.connected_at.elapsed().as_secs(),
        })
        .collect();

    Ok(Json(EdgeStatusResponse { edges }))
}
