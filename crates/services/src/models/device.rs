//! Device Offerings enter the existing effective catalog/admission owner.
//! MOI source policy is workspace-scoped and independent from account-level
//! Genesis policy. No OpenAI request or provider credential exists here.
use super::*;
use crate::runner_model_bindings::{self, ResolvedRunnerModelBinding};
use astra_turn_types::runner_inference::{
    RunnerInferenceProtocol, RunnerInferenceSource, RunnerNativeContext,
};

const ADMISSION_BUDGET: std::time::Duration = std::time::Duration::from_secs(20);
// Inventory must not hold the independently usable organization catalog behind
// a device source outage. Execution retains its separate authoritative budget.
const CATALOG_ADMISSION_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

fn unavailable() -> (StatusCode, Json<ErrorResponse>) {
    error_response_coded(
        StatusCode::SERVICE_UNAVAILABLE,
        "Device model workspace admission is unavailable. Try again after MOI connectivity is restored.",
        "device_model_admission_unavailable",
    )
}

fn denied() -> (StatusCode, Json<ErrorResponse>) {
    error_response_coded(
        StatusCode::FORBIDDEN,
        "This workspace or Role cannot use device ChatGPT. Check the workspace selection and model source policy.",
        "device_model_source_denied",
    )
}

#[derive(Clone, Copy)]
enum SourceAvailability {
    Allowed,
    Denied,
    Unavailable,
}

impl SourceAvailability {
    fn from_admission(
        result: Result<Result<(), (StatusCode, Json<ErrorResponse>)>, tokio::time::error::Elapsed>,
    ) -> Self {
        match result {
            Ok(Ok(())) => Self::Allowed,
            Ok(Err((StatusCode::FORBIDDEN, _))) => Self::Denied,
            Ok(Err(_)) | Err(_) => Self::Unavailable,
        }
    }
}

fn catalog_item_for_source(
    binding: &runner_model_bindings::RunnerModelCatalogBinding,
    availability: SourceAvailability,
) -> Option<ModelListItem> {
    match availability {
        SourceAvailability::Denied => None,
        SourceAvailability::Allowed => Some(binding.catalog_item()),
        SourceAvailability::Unavailable => {
            let mut item = binding.catalog_item();
            item.is_active = false;
            item.description = Some("Device admission could not be confirmed. Renew MOI sign-in or check workspace source access and connectivity; reconnect this device if it is offline.".into());
            Some(item)
        }
    }
}

impl DatabaseModelService {
    async fn device_policy_with_pat(
        &self,
        pat: &str,
        context: &RunnerNativeContext,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        let provider = self.uc_provider.as_ref().ok_or_else(denied)?;
        let mut url = crate::auth::uc::validate_uc_url(&provider.settings.moi_api_url)
            .map_err(|_| unavailable())?;
        url.path_segments_mut().map_err(|_| unavailable())?.extend([
            "workspaces",
            context.workspace_id.as_str(),
            "model-sources",
            "chatgpt-device",
            "admission",
        ]);
        let mut request = provider
            .client
            .get(url)
            .header("X-API-Key", pat)
            .header("X-Workspace-ID", context.workspace_id.as_str());
        if let Some(role) = &context.role_candidate {
            request = request.header("X-MOI-Role-ID", role.as_str());
        }
        let response = request.send().await.map_err(|_| unavailable())?;
        if response.status() == StatusCode::FORBIDDEN {
            return Err(denied());
        }
        if response.status() != StatusCode::OK {
            return Err(unavailable());
        }
        #[derive(Deserialize)]
        struct Envelope {
            code: String,
            data: Admission,
        }
        #[derive(Deserialize)]
        struct Admission {
            workspace_id: String,
            source: String,
            policy_revision: String,
        }
        let result: Envelope = crate::auth::uc::UcNativeProvider::json_bounded(response, 64 * 1024)
            .await
            .map_err(|_| unavailable())?;
        if result.code != "OK"
            || result.data.workspace_id != context.workspace_id.as_str()
            || result.data.source != "chatgpt_device"
            || !result.data.policy_revision.starts_with("sha256:")
        {
            return Err(unavailable());
        }
        Ok(())
    }

    pub(super) async fn device_catalog(
        &self,
        user_id: &str,
        subject: &str,
    ) -> Result<Vec<ModelListItem>, (StatusCode, Json<ErrorResponse>)> {
        let deadline = tokio::time::Instant::now() + CATALOG_ADMISSION_BUDGET;
        let pool = self.pool.as_ref().ok_or_else(unavailable)?;
        let bindings = tokio::time::timeout_at(
            deadline,
            runner_model_bindings::list_runner_model_catalog_bindings(pool, user_id),
        )
        .await
        .map_err(|_| unavailable())?
        .map_err(|_| unavailable())?;
        if bindings.is_empty() {
            return Ok(Vec::new());
        }
        let pat = tokio::time::timeout_at(deadline, self.runtime_pat(subject)).await;
        let mut decisions = BTreeMap::new();
        let mut items = Vec::new();
        for binding in bindings {
            let RunnerInferenceSource::Chatgpt { native_context, .. } = &binding.definition.source
            else {
                continue;
            };
            let key = (
                native_context.workspace_id.as_str().to_owned(),
                native_context
                    .role_candidate
                    .as_ref()
                    .map(|id| id.as_str().to_owned()),
            );
            let decision = if let Some(decision) = decisions.get(&key) {
                *decision
            } else {
                let decision = if decisions.len() >= 32 {
                    SourceAvailability::Unavailable
                } else if let Ok(Ok(pat)) = &pat {
                    SourceAvailability::from_admission(
                        tokio::time::timeout_at(
                            deadline,
                            self.device_policy_with_pat(pat, native_context),
                        )
                        .await,
                    )
                } else {
                    SourceAvailability::Unavailable
                };
                if decisions.len() < 32 {
                    decisions.insert(key, decision);
                }
                decision
            };
            // This is the user-owned inventory, not execution authority. A
            // device-policy outage must fail this source closed without making
            // independently admitted organizational models disappear.
            if let Some(item) = catalog_item_for_source(&binding, decision) {
                items.push(item);
            }
        }
        Ok(items)
    }

    pub(super) async fn admit_device(
        &self,
        user_id: &str,
        offering_id: &str,
    ) -> Result<AdmittedModelExecution, (StatusCode, Json<ErrorResponse>)> {
        tokio::time::timeout(ADMISSION_BUDGET, async {
            let subject = self.uc_subject(user_id).await?.ok_or_else(denied)?;
            let pool = self.pool.as_ref().ok_or_else(unavailable)?;
            let binding =
                runner_model_bindings::resolve_runner_offering(pool, user_id, offering_id)
                    .await
                    .map_err(binding_error)?;
            let RunnerInferenceSource::Chatgpt { native_context, .. } = &binding.definition.source
            else {
                return Err(denied());
            };
            let pat = self.runtime_pat(&subject).await?;
            self.device_policy_with_pat(&pat, native_context).await?;
            admitted(binding)
        })
        .await
        .map_err(|_| unavailable())?
    }
}

fn binding_error(error: crate::ServiceError) -> (StatusCode, Json<ErrorResponse>) {
    use crate::ServiceErrorKind;
    match error.kind {
        ServiceErrorKind::NotFound
        | ServiceErrorKind::Conflict
        | ServiceErrorKind::Verification => error_response_coded(
            StatusCode::CONFLICT,
            "The device model is unavailable. Reconnect this device and select the model again.",
            "device_model_unavailable",
        ),
        _ => unavailable(),
    }
}

fn admitted(
    binding: ResolvedRunnerModelBinding,
) -> Result<AdmittedModelExecution, (StatusCode, Json<ErrorResponse>)> {
    let definition = &binding.definition;
    let RunnerInferenceSource::Chatgpt {
        connection_ref,
        authorization_revision,
        ..
    } = &definition.source
    else {
        return Err(denied());
    };
    definition.validate().map_err(|_| unavailable())?;
    if definition.protocol != RunnerInferenceProtocol::OpenAiResponses {
        return Err(unavailable());
    }
    // Stable across token rotation, terminal detach and socket reconnect.
    // Explicit reauthorization or model/account change produces a new key.
    let scope = serde_json::to_vec(&(
        "astra-device-chatgpt-response-scope-v1",
        &binding.user_id,
        connection_ref,
        authorization_revision,
        &definition.model_name,
    ))
    .map_err(|_| unavailable())?;
    Ok(AdmittedModelExecution {
        offering_id: runner_model_bindings::runner_offering_id(
            &binding.user_id,
            &definition.identity,
        ),
        response_scope: Some(format!("{:x}", Sha256::digest(scope))),
        device_binding: Some(binding.clone()),
        inference_protocol: Some(astra_core::model_wire::LlmProviderProtocol::OpenAiResponses),
        responses_reasoning_history: Some(
            astra_core::model_wire::responses::ReasoningHistory::AllTurns,
        ),
        subscription: None,
        admitted_at: Instant::now(),
        price_snapshot: None,
        source_identity: Some(ResolvedModelSourceIdentity {
            provider: "chatgpt".into(),
            access_label: "My ChatGPT · this device".into(),
        }),
        access_kind: ModelAccessKind::ThisDevice,
        execution_placement: ModelExecutionPlacement::Edge,
        model_name: definition.display_name.as_str().into(),
        wire_model_name: Some(definition.model_name.as_str().into()),
        api_key: String::new(),
        base_url: astra_credentials::chatgpt::API_BASE.into(),
        provider: "chatgpt".into(),
        cache_capability: None,
        thinking_capability: None,
        fixed_temperature: None,
        supported_reasoning_efforts: (!definition.supported_reasoning_efforts.is_empty())
            .then(|| definition.supported_reasoning_efforts.clone()),
        thinking_protocol: Some(
            astra_core::model_wire::thinking::ThinkingProtocol::ResponsesReasoning,
        ),
        request_body_overrides: None,
        context_window: Some(definition.context_window.get()),
        max_completion_tokens: Some(definition.max_output_tokens.get()),
        header_overrides: HashMap::new(),
        completions_url_override: None,
        request_timeout_ms: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_turn_types::runner_inference::*;
    use std::num::{NonZeroU32, NonZeroU64};

    fn binding() -> runner_model_bindings::RunnerModelCatalogBinding {
        let id = |value| RunnerInferenceId::new(value).unwrap();
        runner_model_bindings::RunnerModelCatalogBinding {
            user_id: "synthetic-owner".into(),
            online: true,
            definition: RunnerInferenceBindingDefinition {
                identity: RunnerInferenceBindingIdentity {
                    runner_id: id("runner"),
                    journal_id: id("journal"),
                    binding_id: id("binding"),
                    binding_revision: NonZeroU64::MIN,
                    profile_revision: NonZeroU64::MIN,
                },
                display_name: RunnerInferenceModelName::new("My device model").unwrap(),
                model_name: RunnerInferenceModelName::new("synthetic-provider-model").unwrap(),
                protocol: RunnerInferenceProtocol::OpenAiResponses,
                source: RunnerInferenceSource::Chatgpt {
                    connection_ref: id("connection"),
                    authorization_revision: NonZeroU64::MIN,
                    native_context: RunnerNativeContext {
                        session_id: id("login"),
                        workspace_id: id("workspace"),
                        role_candidate: None,
                    },
                },
                supported_reasoning_efforts: vec![],
                context_window: NonZeroU32::new(1024).unwrap(),
                max_output_tokens: NonZeroU32::new(64).unwrap(),
            },
        }
    }

    #[tokio::test]
    async fn uncertain_source_admission_is_inactive_inventory_never_execution_permission() {
        let binding = binding();
        let allowed =
            catalog_item_for_source(&binding, SourceAvailability::from_admission(Ok(Ok(()))))
                .unwrap();
        assert!(allowed.is_active);
        for status in [
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::UNAUTHORIZED,
            StatusCode::NOT_FOUND,
        ] {
            let availability = SourceAvailability::from_admission(Ok(Err((
                status,
                Json(ErrorResponse::new("synthetic source failure")),
            ))));
            let item = catalog_item_for_source(&binding, availability).unwrap();
            assert!(!item.is_active);
            assert_eq!(item.offering_id, allowed.offering_id);
            assert!(item.description.unwrap().contains("could not be confirmed"));
        }
        let denied = SourceAvailability::from_admission(Ok(Err((
            StatusCode::FORBIDDEN,
            Json(ErrorResponse::new("source disabled")),
        ))));
        assert!(catalog_item_for_source(&binding, denied).is_none());
        let timeout = tokio::time::timeout(std::time::Duration::ZERO, std::future::pending()).await;
        assert!(
            !catalog_item_for_source(&binding, SourceAvailability::from_admission(timeout))
                .unwrap()
                .is_active
        );
    }
}
