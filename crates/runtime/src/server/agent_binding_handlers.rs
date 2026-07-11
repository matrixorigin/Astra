use super::*;

#[derive(serde::Serialize)]
pub(super) struct AgentBindingCreateResponse {
    pub agent_binding_id: String,
    pub binding_name: String,
    pub status: astra_services::AgentBindingStatus,
}

#[derive(serde::Serialize)]
pub(super) struct AgentBindingResponse {
    pub agent_binding_id: String,
    pub binding_name: String,
    pub status: astra_services::AgentBindingStatus,
    pub agent_md: String,
    pub capability_servers: Vec<astra_services::CapabilityServerEndpoint>,
    pub runtime_policy: astra_services::RuntimePolicy,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    pub binding_schema_version: String,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_at: Option<String>,
}

impl From<&astra_services::AgentBindingRecord> for AgentBindingCreateResponse {
    fn from(record: &astra_services::AgentBindingRecord) -> Self {
        Self {
            agent_binding_id: record.id.clone(),
            binding_name: record.binding_name.clone(),
            status: record.status.clone(),
        }
    }
}

impl From<astra_services::AgentBindingRecord> for AgentBindingResponse {
    fn from(record: astra_services::AgentBindingRecord) -> Self {
        Self {
            agent_binding_id: record.id,
            binding_name: record.binding_name,
            status: record.status,
            agent_md: record.agent_md,
            capability_servers: record.capability_servers,
            runtime_policy: record.runtime_policy,
            metadata: record.metadata,
            binding_schema_version: record.binding_schema_version,
            created_at: record.created_at,
            disabled_at: record.disabled_at,
        }
    }
}

pub(super) async fn create_agent_binding_handler(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<AgentBindingCreateResponse>, (StatusCode, Json<ErrorResponse>)> {
    let _principal = state
        .auth_service
        .current_principal_for_request(
            &headers,
            external_request_descriptor(&method, &uri, &headers, "/agent-bindings", &body),
        )
        .await?;
    let request = serde_json::from_slice::<astra_services::AgentBindingCreateRequestData>(&body)
        .map_err(|error| agent_binding_json_error_from_body_text(&error.to_string()))?;
    let record = state.agent_binding_service.create_binding(request).await?;
    Ok(Json((&record).into()))
}

fn agent_binding_json_error_from_body_text(detail: &str) -> (StatusCode, Json<ErrorResponse>) {
    astra_core::error_response_coded(
        StatusCode::BAD_REQUEST,
        format!("agent binding request payload is invalid: {detail}"),
        "agent_binding_invalid",
    )
}

pub(super) async fn get_agent_binding_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<AgentBindingResponse>, (StatusCode, Json<ErrorResponse>)> {
    let _user = state.auth_service.current_user(&headers).await?;
    let record = state.agent_binding_service.get_binding(id).await?;
    Ok(Json(record.into()))
}

pub(super) async fn disable_agent_binding_handler(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Json<AgentBindingResponse>, (StatusCode, Json<ErrorResponse>)> {
    let _principal = state
        .auth_service
        .current_principal_for_request(
            &headers,
            external_request_descriptor(
                &method,
                &uri,
                &headers,
                "/agent-bindings/{id}/disable",
                &body,
            ),
        )
        .await?;
    let record = state.agent_binding_service.disable_binding(id).await?;
    Ok(Json(record.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_binding_json_error_maps_to_contract_code() {
        let err = agent_binding_json_error_from_body_text("unknown variant `model`");
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(err.1.error_code.as_deref(), Some("agent_binding_invalid"));
    }
}
