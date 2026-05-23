use super::*;

/// Response for `GET /edges/status`.
#[derive(Serialize)]
pub(crate) struct EdgeStatusResponse {
    pub(crate) edges: Vec<EdgeInfo>,
}

#[derive(Serialize)]
pub(crate) struct EdgeInfo {
    pub(crate) edge_agent_id: String,
    pub(crate) hostname: Option<String>,
    pub(crate) workspace_dir: Option<String>,
    pub(crate) connected_secs: u64,
}

pub(crate) async fn edge_status_handler(
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
