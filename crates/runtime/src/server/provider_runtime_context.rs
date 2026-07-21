use super::*;

pub(crate) async fn inject_effective_runtime_context(
    _state: &AppState,
    principal: &AuthPrincipal,
    request: &mut astra_services::runs::ChatRequestData,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    if principal.is_provider_authorized_request() {
        request.provider_runtime_authorized = true;
        // Thread provider_scope_id into the request so the run lifecycle can
        // propagate it into ToolExecutionRequest.workspace_record, enabling
        // workspace isolation checks on the edge transport path even when the
        // turn carries no full WorkspaceRecord (MOI provider-authorized turns).
        if let AuthPrincipalOrigin::ProviderAuthorizedRequest(ctx) = &principal.origin {
            request.provider_workspace_id = Some(ctx.provider_scope_id.clone());
        }
        return apply_provider_supplied_runtime_context(request);
    }
    reject_unauthorized_capability_descriptors(request)
}

pub(crate) async fn inject_effective_runtime_context_body(
    state: &AppState,
    principal: &AuthPrincipal,
    body: Bytes,
) -> Result<Bytes, (StatusCode, Json<ErrorResponse>)> {
    if principal.is_provider_authorized_request() {
        if principal.is_edge_registration() {
            return inject_edge_registration_runtime_context_body(state, principal, body).await;
        }
        return Ok(body);
    }
    if body_has_capability_descriptors(&body)? {
        return Err(provider_runtime_authorization_required());
    }
    Ok(body)
}

async fn inject_edge_registration_runtime_context_body(
    state: &AppState,
    principal: &AuthPrincipal,
    body: Bytes,
) -> Result<Bytes, (StatusCode, Json<ErrorResponse>)> {
    let mut value: serde_json::Value = serde_json::from_slice(&body).map_err(|error| {
        error_response(
            StatusCode::BAD_REQUEST,
            format!("chat turn request body must be JSON for edge runtime context: {error}"),
        )
    })?;
    let object = value.as_object_mut().ok_or_else(|| {
        error_response(
            StatusCode::BAD_REQUEST,
            "chat turn request body must be a JSON object for edge runtime context",
        )
    })?;
    // Strip any caller-supplied runtime context fields. Edge-registration tokens
    // are not allowed to provide runtime auth, capability descriptors, or
    // runtime bindings directly; the provider issues all of these below.
    object.remove("runtime_auth");
    object.remove("capability_descriptors");
    object.remove("llm_token_service");
    object.remove("runtime_profile");
    object.remove("runtime_mcp_bindings");
    object.remove("runtime_skill_binding");
    object.remove("runtime_system_prompt");

    let requested_model_id = match object
        .get("selected_model")
        .and_then(|s| s.get("id"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
    {
        Some(id) => id,
        None => {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                "edge chat turn request must specify selected_model.id",
            ));
        }
    };
    // Replace caller-supplied selected_model wholesale with the provider-issued one below.
    object.remove("selected_model");
    let requested_tool_ids = string_array_field(object, "allow_tools")?;
    let requested_skill_ids = string_array_field(object, "allow_skills")?;
    let t0 = std::time::Instant::now();
    tracing::info!("astra_timing: issue_runtime_context started (edge-jwt path)");
    let context = state
        .auth_service
        .external_runtime_context_by_scope(
            principal,
            astra_services::ExternalRuntimeContextRequestData {
                requested_model_id,
                requested_tool_ids: requested_tool_ids.clone(),
                requested_skill_ids: requested_skill_ids.clone(),
                requested_knowledge_base_ids: Vec::new(),
            },
        )
        .await?;
    tracing::info!(
        elapsed_ms = t0.elapsed().as_millis(),
        "astra_timing: issue_runtime_context completed (edge-jwt path)"
    );
    let authorization = context.runtime_auth.authorization.clone();
    let selected_model = context.selected_model.to_selected_model_request();
    let runtime_auth = context.runtime_auth.to_runtime_auth_request();
    let allowed_tool_ids = effective_allowed_tool_ids(
        &requested_tool_ids,
        context
            .runtime_scope
            .allowed_tools
            .iter()
            .map(|tool| tool.id.as_str()),
    );
    // The provider grant may narrow the caller's request but must never broaden
    // it. Persist the effective intersection into the request consumed by the
    // rest of Astra's tool admission pipeline.
    object.insert(
        "allow_tools".to_string(),
        serde_json::to_value(allowed_tool_ids).map_err(internal_error)?,
    );
    if let Some(runtime_system_prompt) = context.runtime_system_prompt {
        object.insert(
            "runtime_system_prompt".to_string(),
            serde_json::Value::String(runtime_system_prompt),
        );
    }
    object.insert(
        "selected_model".to_string(),
        serde_json::to_value(&selected_model).map_err(internal_error)?,
    );
    object.insert(
        "runtime_auth".to_string(),
        serde_json::to_value(&runtime_auth).map_err(internal_error)?,
    );
    object.insert(
        "capability_descriptors".to_string(),
        serde_json::to_value(context.capability_descriptors.to_request_descriptors())
            .map_err(internal_error)?,
    );
    if let Some(model_gateway) = context.capability_descriptors.model_gateway.as_ref() {
        object.insert(
            "llm_token_service".to_string(),
            serde_json::json!({
                "url": model_gateway.endpoint_url,
            }),
        );
    }
    if let Some(mcp) = context.capability_descriptors.mcp {
        object.insert(
            "runtime_profile".to_string(),
            serde_json::Value::String("request_scoped_runtime_mcp".to_string()),
        );
        object.insert(
            "runtime_mcp_bindings".to_string(),
            serde_json::to_value(vec![mcp.into_runtime_mcp_binding(authorization.clone())])
                .map_err(internal_error)?,
        );
    }
    if !requested_skill_ids.is_empty() {
        if let Some(skills) = context.capability_descriptors.skills {
            object.insert(
                "runtime_skill_binding".to_string(),
                serde_json::json!({
                    "id": skills.id,
                    "url": skills.endpoint_url,
                    "authorization": authorization,
                }),
            );
        }
    }
    serde_json::to_vec(&value)
        .map(Bytes::from)
        .map_err(internal_error)
}

fn effective_allowed_tool_ids<'a>(
    requested_tool_ids: &[String],
    provider_allowed_tool_ids: impl IntoIterator<Item = &'a str>,
) -> Vec<String> {
    let provider_allowed_tool_ids = provider_allowed_tool_ids
        .into_iter()
        .collect::<std::collections::HashSet<_>>();
    requested_tool_ids
        .iter()
        .filter(|tool_id| provider_allowed_tool_ids.contains(tool_id.as_str()))
        .cloned()
        .collect()
}

fn string_array_field(
    object: &serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Result<Vec<String>, (StatusCode, Json<ErrorResponse>)> {
    let Some(value) = object.get(field) else {
        return Ok(Vec::new());
    };
    if value.is_null() {
        return Ok(Vec::new());
    }
    let array = value.as_array().ok_or_else(|| {
        error_response(
            StatusCode::BAD_REQUEST,
            format!("{field} must be an array of string ids"),
        )
    })?;
    array
        .iter()
        .map(|item| {
            item.as_str().map(ToOwned::to_owned).ok_or_else(|| {
                error_response(
                    StatusCode::BAD_REQUEST,
                    format!("{field} must contain only string ids"),
                )
            })
        })
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::auth::{AuthPrincipalOrigin, AuthProviderAuthorizedRequestContext};
    use axum::http::StatusCode;

    struct AlwaysHealthy;
    #[async_trait::async_trait]
    impl HealthChecker for AlwaysHealthy {
        async fn database_healthy(&self) -> bool {
            true
        }
    }

    struct StubEdgeAuth;
    #[async_trait::async_trait]
    impl AuthService for StubEdgeAuth {
        async fn register(
            &self,
            _req: AuthRegisterRequestData,
        ) -> Result<AuthUserRecord, (StatusCode, axum::Json<ErrorResponse>)> {
            unreachable!()
        }
        async fn login(
            &self,
            _req: AuthLoginRequestData,
        ) -> Result<AuthTokenRecord, (StatusCode, axum::Json<ErrorResponse>)> {
            unreachable!()
        }
        async fn refresh(
            &self,
            _req: AuthRefreshRequestData,
        ) -> Result<AuthTokenRecord, (StatusCode, axum::Json<ErrorResponse>)> {
            unreachable!()
        }
        async fn logout(
            &self,
            _req: AuthRefreshRequestData,
        ) -> Result<(), (StatusCode, axum::Json<ErrorResponse>)> {
            unreachable!()
        }
        async fn current_user(
            &self,
            _headers: &axum::http::HeaderMap,
        ) -> Result<AuthUserRecord, (StatusCode, axum::Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn external_runtime_context_by_scope(
            &self,
            _principal: &AuthPrincipal,
            request: astra_services::ExternalRuntimeContextRequestData,
        ) -> Result<
            astra_services::ExternalRuntimeContextResponse,
            (StatusCode, axum::Json<ErrorResponse>),
        > {
            use astra_services::auth::external::{
                ExternalCatalogTool, ExternalRuntimeAuthResponse,
                ExternalRuntimeCapabilityDescriptors, ExternalRuntimeScopeResponse,
                ExternalSelectedModelResponse,
            };

            assert_eq!(request.requested_model_id, "model-requested");
            assert_eq!(request.requested_tool_ids, ["bash", "read_file"]);
            Ok(astra_services::ExternalRuntimeContextResponse {
                selected_model: ExternalSelectedModelResponse {
                    id: "model-requested".to_string(),
                    model: "provider-model".to_string(),
                },
                runtime_auth: ExternalRuntimeAuthResponse {
                    auth_type: "moi_runtime_grant".to_string(),
                    authorization: "Bearer runtime-grant".to_string(),
                    expires_at: "2026-07-21T00:00:00Z".to_string(),
                },
                capability_descriptors: ExternalRuntimeCapabilityDescriptors::default(),
                runtime_scope: ExternalRuntimeScopeResponse {
                    allowed_model_id: "model-requested".to_string(),
                    allowed_tools: vec![ExternalCatalogTool {
                        id: "read_file".to_string(),
                        name: "Read file".to_string(),
                        kind: "tool".to_string(),
                        description: None,
                        side_effect_class: "read".to_string(),
                        input_schema: serde_json::Map::new(),
                        output_schema: serde_json::Map::new(),
                        metadata: serde_json::Map::new(),
                    }],
                    allowed_skills: Vec::new(),
                    allowed_knowledge_bases: Vec::new(),
                },
                runtime_system_prompt: None,
                task_id: "task-1".to_string(),
                manifest_id: "manifest-1".to_string(),
                provider_scope_id: "ws-1".to_string(),
            })
        }
    }

    fn edge_principal() -> AuthPrincipal {
        AuthPrincipal {
            user: AuthUserRecord {
                user_id: "u1".to_string(),
                username: "edge".to_string(),
                email: "edge@test".to_string(),
                display_name: None,
            },
            session_id: None,
            origin: AuthPrincipalOrigin::ProviderAuthorizedRequest(
                AuthProviderAuthorizedRequestContext {
                    provider_id: "p1".to_string(),
                    external_subject: "s1".to_string(),
                    provider_scope_id: "ws-1".to_string(),
                    request_authorization_id: "r1".to_string(),
                    edge_agent_id: Some("edge-test".to_string()),
                },
            ),
        }
    }

    fn test_state() -> AppState {
        AppState::new(
            ServiceInfo::new("prc-test", "0.0.0-test", ""),
            std::sync::Arc::new(AlwaysHealthy),
        )
        .with_auth_service(std::sync::Arc::new(StubEdgeAuth))
    }

    #[tokio::test]
    async fn missing_selected_model_id_returns_400() {
        let state = test_state();
        let principal = edge_principal();
        let body = Bytes::from_static(b"{}");

        let err = inject_edge_registration_runtime_context_body(&state, &principal, body)
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(
            err.1.0.detail.contains("selected_model.id"),
            "error message must mention selected_model.id: {:?}",
            err.1.0.detail
        );
    }

    #[tokio::test]
    async fn injected_runtime_auth_and_capability_descriptors_are_stripped_before_model_id_check() {
        let state = test_state();
        let principal = edge_principal();
        // Attacker injects both reserved fields but omits selected_model.id.
        // The handler must strip the injected fields without error and then
        // fail on the missing model id — not on unexpected field presence.
        let body = Bytes::from(
            serde_json::json!({
                "runtime_auth": {"authorization": "attacker-token"},
                "capability_descriptors": {"mcp": {"id": "evil"}},
                "selected_model": {}
            })
            .to_string(),
        );

        let err = inject_edge_registration_runtime_context_body(&state, &principal, body)
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(
            err.1.0.detail.contains("selected_model.id"),
            "must fail on missing selected_model.id, not on injected fields: {:?}",
            err.1.0.detail
        );
    }

    #[tokio::test]
    async fn injected_body_uses_intersection_of_requested_and_provider_allowed_tools() {
        let state = test_state();
        let principal = edge_principal();
        let body = Bytes::from(
            serde_json::json!({
                "selected_model": {"id": "model-requested", "model": "caller-model"},
                "allow_tools": ["bash", "read_file"]
            })
            .to_string(),
        );

        let body = inject_edge_registration_runtime_context_body(&state, &principal, body)
            .await
            .expect("provider runtime context should be injected");
        let value: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON body");

        assert_eq!(value["allow_tools"], serde_json::json!(["read_file"]));
        assert_eq!(value["selected_model"]["model"], "provider-model");
        assert_eq!(
            value["runtime_auth"]["authorization"],
            "Bearer runtime-grant"
        );
    }

    #[test]
    fn provider_tool_grant_only_narrows_requested_tools() {
        let requested = vec![
            "bash".to_string(),
            "read_file".to_string(),
            "write_file".to_string(),
        ];

        let effective =
            effective_allowed_tool_ids(&requested, ["read_file", "provider_only", "write_file"]);

        assert_eq!(effective, vec!["read_file", "write_file"]);
    }

    #[test]
    fn missing_provider_tool_grants_fail_closed() {
        let requested = vec!["bash".to_string()];

        let effective = effective_allowed_tool_ids(&requested, std::iter::empty());

        assert!(effective.is_empty());
    }
}
