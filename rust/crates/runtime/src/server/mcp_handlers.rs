use super::*;

pub(super) async fn register_or_update_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut request): Json<McpRegisterRequestData>,
) -> Result<Json<McpRegisterRecord>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let sanitized_alias = astra_mcp::sanitize_tool_name(request.binding.alias.trim());
    if sanitized_alias.is_empty() {
        return Err(error_response_coded(
            StatusCode::BAD_REQUEST,
            "binding.alias must not be empty after sanitization",
            "mcp_binding_invalid",
        ));
    }
    request.binding.alias = sanitized_alias;

    let discovered_tools = runtime_mcp::discover_binding_tools(&request).await?;
    let record = state
        .mcp_registry_service
        .upsert_discovered_binding(user.user_id, request, discovered_tools)
        .await?;
    Ok(Json(record))
}
