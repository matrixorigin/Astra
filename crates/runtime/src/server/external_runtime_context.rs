use super::*;

pub(crate) async fn inject_effective_runtime_context(
    state: &AppState,
    principal: &AuthPrincipal,
    request: &mut astra_services::runs::ChatRequestData,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    if principal.is_external_authorized_request() {
        request.provider_runtime_authorized = true;
        return apply_provider_supplied_runtime_context(request);
    }
    if !principal.is_external_user_session() {
        return reject_unauthorized_capability_descriptors(request);
    }
    if request_has_explicit_runtime(request) {
        return Err(external_client_runtime_forbidden());
    }
    if !request.runtime_mcp_bindings.is_empty() {
        return Err(error_response_coded(
            StatusCode::BAD_REQUEST,
            "external direct runtime_mcp_bindings must be issued by the external provider",
            "external_runtime_context_required",
        ));
    }
    let requested_model_id = request
        .selected_model
        .as_ref()
        .and_then(|selected| selected.id.clone())
        .ok_or_else(external_requested_model_required)?;
    let requested_skill_ids = request.allow_skills.clone().unwrap_or_default();
    let context = state
        .auth_service
        .external_runtime_context(
            principal,
            ExternalRuntimeContextRequestData {
                requested_model_id,
                requested_tool_ids: request.allow_tools.clone().unwrap_or_default(),
                requested_skill_ids: requested_skill_ids.clone(),
                requested_knowledge_base_ids: Vec::new(),
            },
        )
        .await?;
    apply_provider_runtime_context(request, context, !requested_skill_ids.is_empty())
}

pub(crate) async fn inject_effective_runtime_context_body(
    state: &AppState,
    principal: &AuthPrincipal,
    body: Bytes,
) -> Result<Bytes, (StatusCode, Json<ErrorResponse>)> {
    if principal.is_external_authorized_request() {
        return Ok(body);
    }
    if !principal.is_external_user_session() {
        if body_has_capability_descriptors(&body)? {
            return Err(external_runtime_authorization_required());
        }
        return Ok(body);
    }
    let mut value: serde_json::Value = serde_json::from_slice(&body).map_err(|error| {
        error_response(
            StatusCode::BAD_REQUEST,
            format!("chat turn request body must be JSON for external runtime context: {error}"),
        )
    })?;
    let object = value.as_object_mut().ok_or_else(|| {
        error_response(
            StatusCode::BAD_REQUEST,
            "chat turn request body must be a JSON object for external runtime context",
        )
    })?;
    if body_has_explicit_runtime(object) {
        return Err(external_client_runtime_forbidden());
    }
    if object
        .get("runtime_mcp_bindings")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|bindings| !bindings.is_empty())
    {
        return Err(error_response_coded(
            StatusCode::BAD_REQUEST,
            "external direct runtime_mcp_bindings must be issued by the external provider",
            "external_runtime_context_required",
        ));
    }
    let requested_model_id = object
        .get("selected_model")
        .and_then(|selected| selected.get("id"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(external_requested_model_required)?;
    let requested_tool_ids = string_array_field(object, "allow_tools")?;
    let requested_skill_ids = string_array_field(object, "allow_skills")?;
    let context = state
        .auth_service
        .external_runtime_context(
            principal,
            ExternalRuntimeContextRequestData {
                requested_model_id,
                requested_tool_ids,
                requested_skill_ids: requested_skill_ids.clone(),
                requested_knowledge_base_ids: Vec::new(),
            },
        )
        .await?;
    let authorization = context.runtime_auth.authorization.clone();
    let selected_model = context.selected_model.to_selected_model_request();
    let runtime_auth = context.runtime_auth.to_runtime_auth_request();
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
    if !requested_skill_ids.is_empty()
        && let Some(skills) = context.capability_descriptors.skills
    {
        object.insert(
            "runtime_skill_binding".to_string(),
            serde_json::json!({
                "id": skills.id,
                "url": skills.endpoint_url,
                "authorization": authorization,
            }),
        );
    }
    serde_json::to_vec(&value)
        .map(Bytes::from)
        .map_err(internal_error)
}

fn request_has_explicit_runtime(request: &astra_services::runs::ChatRequestData) -> bool {
    request.agent_binding.is_some()
        || request.runtime_auth.is_some()
        || request.runtime_skill_binding.is_some()
        || request.capability_descriptors.is_some()
        || request
            .selected_model
            .as_ref()
            .and_then(|selected| selected.gateway.as_ref())
            .is_some()
}

fn body_has_explicit_runtime(object: &serde_json::Map<String, serde_json::Value>) -> bool {
    object
        .get("agent_binding")
        .is_some_and(|value| !value.is_null())
        || object
            .get("runtime_auth")
            .is_some_and(|value| !value.is_null())
        || object
            .get("runtime_skill_binding")
            .is_some_and(|value| !value.is_null())
        || object
            .get("capability_descriptors")
            .is_some_and(|value| !value.is_null())
        || object
            .get("selected_model")
            .and_then(|selected| selected.get("gateway"))
            .is_some_and(|gateway| !gateway.is_null())
}

fn apply_provider_runtime_context(
    request: &mut astra_services::runs::ChatRequestData,
    context: ExternalRuntimeContextResponse,
    requested_skills: bool,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    let authorization = context.runtime_auth.authorization.clone();
    let selected_model = context.selected_model.to_selected_model_request();
    request.model = Some(selected_model.model.clone());
    request.selected_model = Some(selected_model);
    request.runtime_auth = Some(context.runtime_auth.to_runtime_auth_request());
    request.capability_descriptors = Some(context.capability_descriptors.to_request_descriptors());
    request.provider_runtime_authorized = true;
    request.runtime_system_prompt = context.runtime_system_prompt;
    if let Some(mcp) = context.capability_descriptors.mcp {
        request.runtime_profile =
            Some(astra_services::runs::RuntimeProfileRequest::RequestScopedRuntimeMcp);
        request.runtime_mcp_bindings = vec![mcp.into_runtime_mcp_binding(authorization.clone())];
    }
    if requested_skills && let Some(skills) = context.capability_descriptors.skills {
        request
            .forward_headers
            .insert("authorization".to_string(), authorization.clone());
        request.runtime_skill_binding = Some(skills.into_runtime_skill_binding(authorization));
    }
    Ok(())
}

fn apply_provider_supplied_runtime_context(
    request: &mut astra_services::runs::ChatRequestData,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    let Some(descriptors) = request.capability_descriptors.as_ref() else {
        if request.runtime_auth.is_some() {
            return Err(error_response_coded(
                StatusCode::BAD_REQUEST,
                "provider-authorized runtime_auth requires capability_descriptors",
                "external_runtime_context_required",
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
        astra_services::auth::external::validate_runtime_capability_descriptor(mcp, "mcp")?;
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
        astra_services::auth::external::validate_runtime_capability_descriptor(skills, "skills")?;
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
        astra_services::auth::external::validate_runtime_capability_descriptor(
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
        return Err(external_runtime_authorization_required());
    }
    Ok(())
}

fn external_runtime_authorization_required() -> (StatusCode, Json<ErrorResponse>) {
    error_response_coded(
        StatusCode::BAD_REQUEST,
        "capability_descriptors require provider-authorized request authentication",
        "external_runtime_context_required",
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

fn external_client_runtime_forbidden() -> (StatusCode, Json<ErrorResponse>) {
    error_response_coded(
        StatusCode::BAD_REQUEST,
        "external clients must not submit provider-issued runtime context fields",
        "external_runtime_context_forbidden",
    )
}

fn external_requested_model_required() -> (StatusCode, Json<ErrorResponse>) {
    error_response_coded(
        StatusCode::BAD_REQUEST,
        "external chat requires selected_model.id returned by /models",
        "external_requested_model_required",
    )
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

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::auth::external::{
        ExternalRuntimeAuthResponse, ExternalRuntimeCapabilityDescriptor,
        ExternalRuntimeCapabilityDescriptors, ExternalRuntimeScopeResponse,
        ExternalSelectedModelResponse,
    };
    use astra_services::runs::{
        AgentBindingRuntimeRequest, CapabilityServerRefs, ChatRequestData, RuntimeAuthRequest,
        RuntimeCapabilityDescriptorRequest, RuntimeCapabilityDescriptorsRequest,
        SelectedModelRequest,
    };
    use astra_services::{
        AuthRefreshRequestData, AuthRegisterRequestData, AuthTokenRecord, AuthUserRecord,
        ExternalRuntimeContextRequestData, ExternalRuntimeContextResponse,
    };
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    struct TestHealthChecker;

    #[async_trait]
    impl HealthChecker for TestHealthChecker {
        async fn database_healthy(&self) -> bool {
            true
        }
    }

    #[derive(Default)]
    struct RuntimeContextAuthService {
        requested_model_ids: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl AuthService for RuntimeContextAuthService {
        async fn register(
            &self,
            _request: AuthRegisterRequestData,
        ) -> Result<AuthUserRecord, (StatusCode, Json<ErrorResponse>)> {
            unimplemented!("not used")
        }

        async fn login(
            &self,
            _request: astra_services::AuthLoginRequestData,
        ) -> Result<AuthTokenRecord, (StatusCode, Json<ErrorResponse>)> {
            unimplemented!("not used")
        }

        async fn refresh(
            &self,
            _request: AuthRefreshRequestData,
        ) -> Result<AuthTokenRecord, (StatusCode, Json<ErrorResponse>)> {
            unimplemented!("not used")
        }

        async fn logout(
            &self,
            _request: AuthRefreshRequestData,
        ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
            unimplemented!("not used")
        }

        async fn current_user(
            &self,
            _headers: &HeaderMap,
        ) -> Result<AuthUserRecord, (StatusCode, Json<ErrorResponse>)> {
            unimplemented!("not used")
        }

        async fn external_runtime_context(
            &self,
            _principal: &AuthPrincipal,
            request: ExternalRuntimeContextRequestData,
        ) -> Result<ExternalRuntimeContextResponse, (StatusCode, Json<ErrorResponse>)> {
            self.requested_model_ids
                .lock()
                .expect("requested ids")
                .push(request.requested_model_id.clone());
            Ok(ExternalRuntimeContextResponse {
                selected_model: ExternalSelectedModelResponse {
                    id: request.requested_model_id,
                    model: "qwen3.7-max".to_string(),
                },
                runtime_auth: ExternalRuntimeAuthResponse {
                    auth_type: "moi_runtime_grant".to_string(),
                    authorization: "Bearer runtime-grant".to_string(),
                    expires_at: "2026-01-01T00:00:00Z".to_string(),
                },
                capability_descriptors: ExternalRuntimeCapabilityDescriptors {
                    model_gateway: Some(external_descriptor(
                        "moi-model-gateway",
                        "model_gateway",
                        "http://127.0.0.1/model",
                    )),
                    mcp: None,
                    skills: None,
                },
                runtime_scope: ExternalRuntimeScopeResponse {
                    allowed_model_id: "model-qwen".to_string(),
                    allowed_tools: Vec::new(),
                    allowed_skills: Vec::new(),
                    allowed_knowledge_bases: Vec::new(),
                },
                runtime_system_prompt: None,
                task_id: "task-1".to_string(),
                manifest_id: "manifest-1".to_string(),
                provider_scope_id: "workspace-1".to_string(),
            })
        }
    }

    fn base_request() -> ChatRequestData {
        ChatRequestData {
            message: "hello".to_string(),
            parts: Vec::new(),
            attachments: Vec::new(),
            runtime_system_prompt: None,
            session_id: None,
            full_llm_capture: false,
            agent_id: None,
            model: None,
            selected_model: Some(SelectedModelRequest {
                id: Some("model-qwen".to_string()),
                model: "qwen3.7-max".to_string(),
                gateway: None,
            }),
            capability_descriptors: None,
            provider_runtime_authorized: false,
            agent_binding: None,
            runtime_auth: None,
            runtime_skill_binding: None,
            runtime_profile: None,
            llm_token_service: None,
            skill_search: None,
            allow_skills: None,
            allow_skill_sources: None,
            allow_tools: None,
            workspace_binding: None,
            executor_binding: None,
            runtime_mcp_bindings: Vec::new(),
            mcp_binding_ids: None,
            context: None,
            edge_executor_id: None,
            capabilities: Vec::new(),
            forward_headers: std::collections::HashMap::new(),
            execution_budget: None,
            explain: false,
            interaction_mode: None,
            interactive_client: false,
        }
    }

    fn descriptor(
        id: &str,
        descriptor_type: &str,
        endpoint_url: &str,
    ) -> RuntimeCapabilityDescriptorRequest {
        RuntimeCapabilityDescriptorRequest {
            id: id.to_string(),
            descriptor_type: descriptor_type.to_string(),
            transport: "streamable_http".to_string(),
            endpoint_url: endpoint_url.to_string(),
            protocol: descriptor_type.to_string(),
            metadata: serde_json::Map::new(),
        }
    }

    fn external_descriptor(
        id: &str,
        descriptor_type: &str,
        endpoint_url: &str,
    ) -> ExternalRuntimeCapabilityDescriptor {
        ExternalRuntimeCapabilityDescriptor {
            id: id.to_string(),
            descriptor_type: descriptor_type.to_string(),
            transport: "streamable_http".to_string(),
            endpoint_url: endpoint_url.to_string(),
            protocol: descriptor_type.to_string(),
            metadata: serde_json::Map::new(),
        }
    }

    #[tokio::test]
    async fn external_user_session_with_selected_model_id_injects_provider_runtime_context() {
        let auth = Arc::new(RuntimeContextAuthService::default());
        let state = AppState::new(ServiceInfo::default(), Arc::new(TestHealthChecker))
            .with_auth_service(auth.clone());
        let principal = AuthPrincipal {
            user: AuthUserRecord {
                user_id: "external:moi:user-1".to_string(),
                username: "user-1".to_string(),
                email: String::new(),
                display_name: None,
            },
            session_id: Some("external-session-1".to_string()),
            origin: AuthPrincipalOrigin::External(AuthExternalSessionContext {
                provider_id: "moi".to_string(),
                external_subject: "user-1".to_string(),
                external_session_id: "external-session-1".to_string(),
                provider_scope_id: "workspace-1".to_string(),
                provider_scope_display_name: Some("Workspace 1".to_string()),
            }),
        };
        let mut request = base_request();

        inject_effective_runtime_context(&state, &principal, &mut request)
            .await
            .expect("external user context should be injected");

        assert_eq!(
            auth.requested_model_ids
                .lock()
                .expect("requested ids")
                .as_slice(),
            ["model-qwen"]
        );
        assert_eq!(
            request
                .selected_model
                .as_ref()
                .and_then(|selected| selected.id.as_deref()),
            Some("model-qwen")
        );
        assert_eq!(
            request
                .runtime_auth
                .as_ref()
                .map(|auth| auth.authorization.as_str()),
            Some("Bearer runtime-grant")
        );
        assert!(request.capability_descriptors.is_some());
    }

    #[tokio::test]
    async fn external_user_session_body_injects_llm_token_service_from_model_gateway() {
        let auth = Arc::new(RuntimeContextAuthService::default());
        let state = AppState::new(ServiceInfo::default(), Arc::new(TestHealthChecker))
            .with_auth_service(auth.clone());
        let principal = AuthPrincipal {
            user: AuthUserRecord {
                user_id: "external:moi:user-1".to_string(),
                username: "user-1".to_string(),
                email: String::new(),
                display_name: None,
            },
            session_id: Some("external-session-1".to_string()),
            origin: AuthPrincipalOrigin::External(AuthExternalSessionContext {
                provider_id: "moi".to_string(),
                external_subject: "user-1".to_string(),
                external_session_id: "external-session-1".to_string(),
                provider_scope_id: "workspace-1".to_string(),
                provider_scope_display_name: Some("Workspace 1".to_string()),
            }),
        };
        let body = Bytes::from(
            serde_json::to_vec(&serde_json::json!({
                "selected_model": {"id": "model-qwen"},
                "messages": [{"role": "user", "content": "hello"}]
            }))
            .expect("body json"),
        );

        let injected = inject_effective_runtime_context_body(&state, &principal, body)
            .await
            .expect("external user context body should be injected");
        let value: serde_json::Value =
            serde_json::from_slice(&injected).expect("injected body should be json");

        assert!(value.get("model").is_none());
        assert_eq!(value["selected_model"]["model"], "qwen3.7-max");
        assert_eq!(
            value["runtime_auth"]["authorization"],
            "Bearer runtime-grant"
        );
        assert_eq!(value["llm_token_service"]["url"], "http://127.0.0.1/model");
    }

    #[test]
    fn explicit_runtime_detection_includes_capability_descriptors() {
        let mut request = base_request();
        request.capability_descriptors = Some(RuntimeCapabilityDescriptorsRequest {
            model_gateway: Some(descriptor(
                "moi-model-gateway",
                "model_gateway",
                "http://127.0.0.1/model",
            )),
            mcp: None,
            skills: None,
        });

        assert!(request_has_explicit_runtime(&request));
    }

    #[test]
    fn provider_authorized_request_applies_complete_descriptors() {
        let mut request = base_request();
        request.provider_runtime_authorized = true;
        request.runtime_auth = Some(RuntimeAuthRequest {
            authorization: "Bearer runtime-grant".to_string(),
        });
        request.capability_descriptors = Some(RuntimeCapabilityDescriptorsRequest {
            model_gateway: Some(RuntimeCapabilityDescriptorRequest {
                transport: "http".to_string(),
                protocol: "openai_responses".to_string(),
                ..descriptor(
                    "moi-model-gateway",
                    "model_gateway",
                    "http://127.0.0.1/model",
                )
            }),
            mcp: Some(descriptor("moi-tools", "mcp", "http://127.0.0.1/mcp")),
            skills: Some(descriptor(
                "moi-skills",
                "skills",
                "http://127.0.0.1/skills",
            )),
        });

        apply_provider_supplied_runtime_context(&mut request)
            .expect("provider descriptors should be accepted");

        assert_eq!(request.runtime_mcp_bindings.len(), 1);
        assert_eq!(request.runtime_mcp_bindings[0].id, "moi-tools");
        assert_eq!(
            request.runtime_mcp_bindings[0].auth_token.as_deref(),
            Some("Bearer runtime-grant")
        );
        let skill = request
            .runtime_skill_binding
            .as_ref()
            .expect("skill binding");
        assert_eq!(skill.id, "moi-skills");
        assert_eq!(skill.authorization, "Bearer runtime-grant");
    }

    #[test]
    fn provider_authorized_agent_binding_request_does_not_create_request_scoped_bindings() {
        let mut request = base_request();
        request.provider_runtime_authorized = true;
        request.agent_binding = Some(AgentBindingRuntimeRequest {
            id: "binding-1".to_string(),
            capability_server_refs: CapabilityServerRefs {
                mcp: "moi-tools".to_string(),
                skills: "moi-skills".to_string(),
            },
        });
        request.runtime_auth = Some(RuntimeAuthRequest {
            authorization: "Bearer runtime-grant".to_string(),
        });
        request.capability_descriptors = Some(RuntimeCapabilityDescriptorsRequest {
            model_gateway: Some(RuntimeCapabilityDescriptorRequest {
                transport: "http".to_string(),
                protocol: "openai_responses".to_string(),
                ..descriptor(
                    "moi-model-gateway",
                    "model_gateway",
                    "http://127.0.0.1/model",
                )
            }),
            mcp: Some(descriptor("moi-tools", "mcp", "http://127.0.0.1/mcp")),
            skills: Some(descriptor(
                "moi-skills",
                "skills",
                "http://127.0.0.1/skills",
            )),
        });

        apply_provider_supplied_runtime_context(&mut request)
            .expect("provider descriptors should be accepted");

        assert!(request.runtime_mcp_bindings.is_empty());
        assert!(request.runtime_skill_binding.is_none());
        assert!(request.runtime_profile.is_none());
    }
}
