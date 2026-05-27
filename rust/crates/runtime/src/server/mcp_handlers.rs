use super::*;

pub(super) async fn register_or_update_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<McpRegisterRequestData>,
) -> Result<Json<McpRegisterRecord>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let binding = state
        .mcp_registry_service
        .upsert_binding(user.user_id.clone(), request.clone())
        .await?;
    let discovered_tools =
        runtime_mcp::discover_binding_tools(binding.binding_id, &request).await?;
    let record = state
        .mcp_registry_service
        .replace_binding_tools(user.user_id, binding.binding_id, discovered_tools)
        .await?;
    Ok(Json(record))
}
