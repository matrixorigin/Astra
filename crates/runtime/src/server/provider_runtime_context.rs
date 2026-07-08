use super::*;

pub(crate) async fn inject_effective_runtime_context(
    _state: &AppState,
    principal: &AuthPrincipal,
    request: &mut astra_services::runs::ChatRequestData,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    if let Some(provider_context) = principal.provider_authorized_request_context() {
        request.provider_runtime_authorized = true;
        return apply_provider_supplied_runtime_context(
            request,
            &provider_context.allowed_capability_descriptors,
        );
    }
    reject_unauthorized_capability_descriptors(request)
}

pub(crate) async fn inject_effective_runtime_context_body(
    _state: &AppState,
    principal: &AuthPrincipal,
    body: Bytes,
) -> Result<Bytes, (StatusCode, Json<ErrorResponse>)> {
    if let Some(provider_context) = principal.provider_authorized_request_context() {
        validate_provider_supplied_runtime_context_body(
            &body,
            &provider_context.allowed_capability_descriptors,
        )?;
        return Ok(body);
    }
    if body_has_capability_descriptors(&body)? {
        return Err(provider_runtime_authorization_required());
    }
    Ok(body)
}

fn validate_provider_supplied_runtime_context_body(
    body: &Bytes,
    allowed_capability_descriptors: &[astra_core::ProviderCapabilityDescriptorConfig],
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    let value: serde_json::Value = serde_json::from_slice(body).map_err(|error| {
        error_response_coded(
            StatusCode::BAD_REQUEST,
            format!("provider-authorized request body is invalid JSON: {error}"),
            "provider_runtime_context_invalid",
        )
    })?;
    let Some(raw_descriptors) = value
        .as_object()
        .and_then(|object| object.get("capability_descriptors"))
    else {
        return Ok(());
    };
    if value
        .as_object()
        .and_then(|object| object.get("runtime_auth"))
        .and_then(|runtime_auth| runtime_auth.get("authorization"))
        .and_then(serde_json::Value::as_str)
        .is_none_or(|authorization| authorization.trim().is_empty())
    {
        return Err(error_response_coded(
            StatusCode::BAD_REQUEST,
            "runtime_auth.authorization is required with capability_descriptors",
            "agent_binding_runtime_auth_missing",
        ));
    }
    let descriptors: astra_services::runs::RuntimeCapabilityDescriptorsRequest =
        serde_json::from_value(raw_descriptors.clone()).map_err(|error| {
            error_response_coded(
                StatusCode::BAD_REQUEST,
                format!("capability_descriptors are invalid: {error}"),
                "provider_runtime_context_invalid",
            )
        })?;
    validate_provider_capability_descriptors(&descriptors, allowed_capability_descriptors)
}

fn apply_provider_supplied_runtime_context(
    request: &mut astra_services::runs::ChatRequestData,
    allowed_capability_descriptors: &[astra_core::ProviderCapabilityDescriptorConfig],
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
    validate_provider_capability_descriptors(descriptors, allowed_capability_descriptors)?;
    if let Some(mcp) = descriptors.mcp.as_ref() {
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
    Ok(())
}

fn validate_provider_capability_descriptors(
    descriptors: &astra_services::runs::RuntimeCapabilityDescriptorsRequest,
    allowed_capability_descriptors: &[astra_core::ProviderCapabilityDescriptorConfig],
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    if let Some(mcp) = descriptors.mcp.as_ref() {
        astra_services::auth::provider_request::validate_runtime_capability_descriptor_allowed(
            mcp,
            "mcp",
            allowed_capability_descriptors,
        )?;
    }
    if let Some(skills) = descriptors.skills.as_ref() {
        astra_services::auth::provider_request::validate_runtime_capability_descriptor_allowed(
            skills,
            "skills",
            allowed_capability_descriptors,
        )?;
    }
    if let Some(model_gateway) = descriptors.model_gateway.as_ref() {
        astra_services::auth::provider_request::validate_runtime_capability_descriptor_allowed(
            model_gateway,
            "model_gateway",
            allowed_capability_descriptors,
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn allowance(endpoint_url: &str) -> astra_core::ProviderCapabilityDescriptorConfig {
        astra_core::ProviderCapabilityDescriptorConfig {
            id: "mcp-1".to_string(),
            descriptor_type: "mcp".to_string(),
            transport: "http".to_string(),
            endpoint_url: endpoint_url.to_string(),
            protocol: "mcp.v1".to_string(),
        }
    }

    fn provider_body(endpoint_url: &str) -> Bytes {
        Bytes::from(
            serde_json::to_vec(&json!({
                "capability_descriptors": {
                    "mcp": {
                        "id": "mcp-1",
                        "type": "mcp",
                        "transport": "http",
                        "endpoint_url": endpoint_url,
                        "protocol": "mcp.v1"
                    }
                },
                "runtime_auth": {
                    "authorization": "Bearer runtime-token"
                }
            }))
            .expect("json"),
        )
    }

    #[test]
    fn provider_runtime_body_accepts_exact_allowed_descriptor() {
        validate_provider_supplied_runtime_context_body(
            &provider_body("https://provider.example/mcp"),
            &[allowance("https://provider.example/mcp")],
        )
        .expect("exactly allowed descriptor should pass");
    }

    #[test]
    fn provider_runtime_body_rejects_descriptor_endpoint_not_in_allowlist() {
        let (status, body) = validate_provider_supplied_runtime_context_body(
            &provider_body("https://attacker.example/mcp"),
            &[allowance("https://provider.example/mcp")],
        )
        .expect_err("endpoint not in provider allowlist must be rejected");

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body.0.error_code.as_deref(),
            Some("provider_runtime_context_invalid")
        );
        assert!(body.0.detail.contains("not allowed"));
    }

    #[test]
    fn provider_runtime_body_rejects_descriptors_without_runtime_auth() {
        let body = Bytes::from(
            serde_json::to_vec(&json!({
                "capability_descriptors": {
                    "mcp": {
                        "id": "mcp-1",
                        "type": "mcp",
                        "transport": "http",
                        "endpoint_url": "https://provider.example/mcp",
                        "protocol": "mcp.v1"
                    }
                }
            }))
            .expect("json"),
        );

        let (status, body) = validate_provider_supplied_runtime_context_body(
            &body,
            &[allowance("https://provider.example/mcp")],
        )
        .expect_err("runtime_auth is required with descriptors");

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body.0.error_code.as_deref(),
            Some("agent_binding_runtime_auth_missing")
        );
    }
}
