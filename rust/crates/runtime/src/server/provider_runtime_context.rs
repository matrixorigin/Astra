use super::*;

pub(crate) async fn inject_effective_runtime_context(
    _state: &AppState,
    principal: &AuthPrincipal,
    request: &mut astra_services::runs::ChatRequestData,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    if principal.is_provider_authorized_request() {
        request.provider_runtime_authorized = true;
        return apply_provider_supplied_runtime_context(request);
    }
    reject_unauthorized_capability_descriptors(request)
}

pub(crate) async fn inject_effective_runtime_context_body(
    _state: &AppState,
    principal: &AuthPrincipal,
    body: Bytes,
) -> Result<Bytes, (StatusCode, Json<ErrorResponse>)> {
    if principal.is_provider_authorized_request() {
        return Ok(body);
    }
    if body_has_capability_descriptors(&body)? {
        return Err(provider_runtime_authorization_required());
    }
    Ok(body)
}

fn apply_provider_supplied_runtime_context(
    request: &mut astra_services::runs::ChatRequestData,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    let Some(descriptors) = request.capability_descriptors.as_ref() else {
        if request.runtime_auth.is_some() {
            return Err(error_response_coded(
                StatusCode::BAD_REQUEST,
                "provider-authorized runtime_auth requires capability_descriptors",
                "provider_runtime_context_required",
            ));
        }
        return Ok(());
    };
    if request
        .selected_model
        .as_ref()
        .and_then(|selected| selected.gateway.as_ref())
        .is_some()
    {
        return Err(error_response_coded(
            StatusCode::BAD_REQUEST,
            "provider-issued selected_model must not include gateway",
            "selected_model_invalid",
        ));
    }
    let authorization = request
        .runtime_auth
        .as_ref()
        .map(|runtime_auth| runtime_auth.authorization.clone())
        .ok_or_else(|| {
            error_response_coded(
                StatusCode::BAD_REQUEST,
                "runtime_auth.authorization is required with capability_descriptors",
                "agent_binding_runtime_auth_missing",
            )
        })?;
    let agent_binding_mode = request.agent_binding.is_some();
    if let Some(mcp) = descriptors.mcp.as_ref() {
        astra_services::auth::provider_request::validate_runtime_capability_descriptor(mcp, "mcp")?;
        if !agent_binding_mode {
            request.runtime_profile =
                Some(astra_services::runs::RuntimeProfileRequest::RequestScopedRuntimeMcp);
            request.runtime_mcp_bindings = vec![astra_services::runs::RuntimeMcpBindingRequest {
                id: mcp.id.clone(),
                transport: mcp.transport.clone(),
                url: mcp.endpoint_url.clone(),
                auth_token: Some(authorization.clone()),
                headers: std::collections::HashMap::new(),
            }];
        }
    }
    if let Some(skills) = descriptors.skills.as_ref() {
        astra_services::auth::provider_request::validate_runtime_capability_descriptor(
            skills, "skills",
        )?;
        if !agent_binding_mode {
            request
                .forward_headers
                .insert("authorization".to_string(), authorization.clone());
            request.runtime_skill_binding =
                Some(astra_services::runs::RuntimeSkillBindingRequest {
                    id: skills.id.clone(),
                    url: skills.endpoint_url.clone(),
                    authorization,
                });
        }
    }
    if let Some(model_gateway) = descriptors.model_gateway.as_ref() {
        astra_services::auth::provider_request::validate_runtime_capability_descriptor(
            model_gateway,
            "model_gateway",
        )?;
    }
    Ok(())
}

fn reject_unauthorized_capability_descriptors(
    request: &astra_services::runs::ChatRequestData,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    if request.capability_descriptors.is_some() {
        return Err(provider_runtime_authorization_required());
    }
    Ok(())
}

fn provider_runtime_authorization_required() -> (StatusCode, Json<ErrorResponse>) {
    error_response_coded(
        StatusCode::BAD_REQUEST,
        "capability_descriptors require provider request authentication",
        "provider_runtime_context_required",
    )
}

fn body_has_capability_descriptors(
    body: &Bytes,
) -> Result<bool, (StatusCode, Json<ErrorResponse>)> {
    let value: serde_json::Value = serde_json::from_slice(body).map_err(|error| {
        error_response(
            StatusCode::BAD_REQUEST,
            format!("chat turn request body must be JSON: {error}"),
        )
    })?;
    Ok(value
        .as_object()
        .and_then(|object| object.get("capability_descriptors"))
        .is_some_and(|value| !value.is_null()))
}
