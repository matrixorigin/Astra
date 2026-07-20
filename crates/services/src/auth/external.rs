use std::sync::Arc;
use std::time::Duration;

use astra_core::{ErrorResponse, error_response, error_response_coded, internal_error};

#[derive(Clone, Debug)]
pub struct ExternalAuthProviderConfig {
    pub id: String,
    pub display_name: String,
    pub external_auth_endpoint: String,
}
use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::models::ModelListItem;
use crate::runs::{
    ResolvedModelSelection, RuntimeAuthRequest, RuntimeCapabilityDescriptorRequest,
    RuntimeCapabilityDescriptorsRequest, RuntimeMcpBindingRequest,
    RuntimeSemanticReadCapabilityRequest, RuntimeSkillBindingRequest,
};
use astra_turn_types::ModelSelection;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternalProviderPublicRecord {
    pub id: String,
    pub display_name: String,
    pub credential_type: String,
}

impl From<&ExternalAuthProviderConfig> for ExternalProviderPublicRecord {
    fn from(value: &ExternalAuthProviderConfig) -> Self {
        Self {
            id: value.id.clone(),
            display_name: value.display_name.clone(),
            credential_type: "password".to_string(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternalLoginRequestData {
    pub provider_id: String,
    pub username: String,
    pub password: String,
    pub scope_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternalAuthorizeRequestData {
    pub provider_id: String,
    pub token: String,
    pub request: ExternalRequestDescriptor,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalRequestDescriptor {
    pub method: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_digest: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternalRuntimeContextRequestData {
    pub requested_model_id: String,
    pub requested_tool_ids: Vec<String>,
    pub requested_skill_ids: Vec<String>,
    pub requested_knowledge_base_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternalSessionRecord {
    pub external_session_id: String,
    pub provider_id: String,
    pub astra_user_id: String,
    pub external_subject: String,
    pub provider_scope_id: String,
    pub provider_scope_display_name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalProviderScope {
    pub id: String,
    pub workspace_id: String,
    pub name: String,
    #[serde(default)]
    pub account_name: Option<String>,
    #[serde(default)]
    pub is_default: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalProviderAuthResponse {
    pub external_subject: ExternalSubject,
    pub display_info: ExternalDisplayInfo,
    pub workspace_scopes: Vec<ExternalProviderScope>,
    pub selected_scope: ExternalProviderScope,
    pub default_scope: ExternalProviderScope,
    pub provider_session_handle: String,
    pub expires_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalSubject {
    pub id: String,
    pub provider: String,
    pub kind: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalDisplayInfo {
    pub username: String,
    #[serde(default)]
    pub nickname: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalCatalogModel {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub display_name: Option<String>,
    pub model_name: String,
    #[serde(default)]
    pub provider_ref: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub parameters: Map<String, Value>,
    #[serde(default)]
    pub limits: Map<String, Value>,
    #[serde(default)]
    pub metadata: Map<String, Value>,
}

impl ExternalCatalogModel {
    fn into_model_list_item(self) -> ModelListItem {
        ModelListItem {
            offering_id: self.id,
            access_id: "this-device".to_string(),
            access_kind: crate::models::ModelAccessKind::ThisDevice,
            access_label: "This device".to_string(),
            execution_placement: crate::models::ModelExecutionPlacement::Edge,
            name: self.display_name.unwrap_or(self.name),
            provider: self.provider_ref.unwrap_or_default(),
            description: string_value(&self.metadata, "description"),
            is_active: true,
            context_window: i32_value(&self.limits, "context_window").unwrap_or_default(),
            max_completion_tokens: i32_value(&self.limits, "max_completion_tokens"),
            architecture: string_value(&self.metadata, "architecture"),
            thinking_capability: None,
        }
    }
}

fn string_value(values: &Map<String, Value>, key: &str) -> Option<String> {
    values
        .get(key)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn i32_value(values: &Map<String, Value>, key: &str) -> Option<i32> {
    let value = values.get(key)?;
    if let Some(raw) = value.as_i64() {
        return i32::try_from(raw).ok();
    }
    value.as_u64().and_then(|raw| i32::try_from(raw).ok())
}

impl From<ExternalCatalogModel> for ModelListItem {
    fn from(value: ExternalCatalogModel) -> Self {
        value.into_model_list_item()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalCatalogTool {
    pub id: String,
    pub name: String,
    pub kind: String,
    #[serde(default)]
    pub description: Option<String>,
    pub side_effect_class: String,
    #[serde(default)]
    pub input_schema: Map<String, Value>,
    #[serde(default)]
    pub output_schema: Map<String, Value>,
    #[serde(default)]
    pub metadata: Map<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalCatalogSkill {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub version: i32,
    #[serde(default)]
    pub routing_summary: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub requirements: Map<String, Value>,
    #[serde(default)]
    pub parameters_schema: Map<String, Value>,
    #[serde(default)]
    pub output_contract: Map<String, Value>,
    #[serde(default)]
    pub metadata: Map<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalCatalogKnowledgeBase {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub visibility: String,
    pub index_status: String,
    #[serde(default)]
    pub catalog_asset_refs: Vec<Map<String, Value>>,
    #[serde(default)]
    pub metadata: Map<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalCatalogResponse {
    #[serde(default)]
    pub models: Vec<ExternalCatalogModel>,
    #[serde(default)]
    pub tools: Vec<ExternalCatalogTool>,
    #[serde(default)]
    pub skills: Vec<ExternalCatalogSkill>,
    #[serde(default)]
    pub knowledge_bases: Vec<ExternalCatalogKnowledgeBase>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalAuthorizeRequestResponse {
    pub ok: bool,
    #[serde(default)]
    pub external_subject: Option<String>,
    #[serde(default)]
    pub provider_scope_id: Option<String>,
    #[serde(default)]
    pub request_authorization_id: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    /// Populated by the provider when the verified token is an edge-registration
    /// token. Astra edge auth verifies the self-reported edge id against this.
    #[serde(default)]
    pub edge_agent_id: Option<String>,
}

impl ExternalAuthorizeRequestResponse {
    pub fn into_authorization(
        self,
        provider_id: &str,
    ) -> Result<ExternalAuthorizedRequest, (StatusCode, Json<ErrorResponse>)> {
        if !self.ok {
            return Err(error_response_coded(
                StatusCode::FORBIDDEN,
                self.reason
                    .unwrap_or_else(|| "external provider denied request".to_string()),
                "external_request_denied",
            ));
        }
        let external_subject = self.external_subject.ok_or_else(|| {
            error_response_coded(
                StatusCode::BAD_GATEWAY,
                format!(
                    "external provider '{}' authorize_request omitted external_subject",
                    provider_id
                ),
                "external_provider_response_invalid",
            )
        })?;
        let provider_scope_id = self.provider_scope_id.ok_or_else(|| {
            error_response_coded(
                StatusCode::BAD_GATEWAY,
                format!(
                    "external provider '{}' authorize_request omitted provider_scope_id",
                    provider_id
                ),
                "external_provider_response_invalid",
            )
        })?;
        let request_authorization_id = self.request_authorization_id.ok_or_else(|| {
            error_response_coded(
                StatusCode::BAD_GATEWAY,
                format!(
                    "external provider '{}' authorize_request omitted request_authorization_id",
                    provider_id
                ),
                "external_provider_response_invalid",
            )
        })?;
        Ok(ExternalAuthorizedRequest {
            provider_id: provider_id.to_string(),
            external_subject,
            provider_scope_id,
            request_authorization_id,
            edge_agent_id: self.edge_agent_id,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternalAuthorizedRequest {
    pub provider_id: String,
    pub external_subject: String,
    pub provider_scope_id: String,
    pub request_authorization_id: String,
    /// Present when the token is an edge-registration token; the bound edge
    /// agent id that Astra must match against the self-reported value.
    pub edge_agent_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalRuntimeCapabilityDescriptor {
    pub id: String,
    #[serde(rename = "type")]
    pub descriptor_type: String,
    pub transport: String,
    pub endpoint_url: String,
    pub protocol: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic_read: Option<RuntimeSemanticReadCapabilityRequest>,
    #[serde(default)]
    pub metadata: Map<String, Value>,
}

impl ExternalRuntimeCapabilityDescriptor {
    pub fn to_request_descriptor(&self) -> RuntimeCapabilityDescriptorRequest {
        RuntimeCapabilityDescriptorRequest {
            id: self.id.clone(),
            descriptor_type: self.descriptor_type.clone(),
            transport: self.transport.clone(),
            endpoint_url: self.endpoint_url.clone(),
            protocol: self.protocol.clone(),
            semantic_read: self.semantic_read.clone(),
            metadata: self.metadata.clone(),
        }
    }

    pub fn into_runtime_mcp_binding(self, authorization: String) -> RuntimeMcpBindingRequest {
        RuntimeMcpBindingRequest {
            id: self.id,
            transport: self.transport,
            url: self.endpoint_url,
            auth_token: Some(authorization),
            headers: std::collections::HashMap::new(),
        }
    }

    pub fn into_runtime_skill_binding(self, authorization: String) -> RuntimeSkillBindingRequest {
        RuntimeSkillBindingRequest {
            id: self.id,
            url: self.endpoint_url,
            authorization,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalRuntimeCapabilityDescriptors {
    #[serde(default)]
    pub model_gateway: Option<ExternalRuntimeCapabilityDescriptor>,
    #[serde(default)]
    pub mcp: Option<ExternalRuntimeCapabilityDescriptor>,
    #[serde(default)]
    pub skills: Option<ExternalRuntimeCapabilityDescriptor>,
}

impl ExternalRuntimeCapabilityDescriptors {
    pub fn to_request_descriptors(&self) -> RuntimeCapabilityDescriptorsRequest {
        RuntimeCapabilityDescriptorsRequest {
            model_gateway: self
                .model_gateway
                .as_ref()
                .map(ExternalRuntimeCapabilityDescriptor::to_request_descriptor),
            mcp: self
                .mcp
                .as_ref()
                .map(ExternalRuntimeCapabilityDescriptor::to_request_descriptor),
            skills: self
                .skills
                .as_ref()
                .map(ExternalRuntimeCapabilityDescriptor::to_request_descriptor),
            edge_agent: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalRuntimeContextResponse {
    pub selected_model: ExternalSelectedModelResponse,
    pub runtime_auth: ExternalRuntimeAuthResponse,
    #[serde(default)]
    pub capability_descriptors: ExternalRuntimeCapabilityDescriptors,
    pub runtime_scope: ExternalRuntimeScopeResponse,
    #[serde(default)]
    pub runtime_system_prompt: Option<String>,
    pub task_id: String,
    pub manifest_id: String,
    pub provider_scope_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalSelectedModelResponse {
    pub id: String,
    pub model: String,
}

impl ExternalSelectedModelResponse {
    pub fn to_model_selection(&self) -> ModelSelection {
        ModelSelection {
            offering_id: self.id.clone(),
        }
    }

    pub fn to_resolved_model_selection(&self) -> ResolvedModelSelection {
        ResolvedModelSelection {
            offering_id: self.id.clone(),
            model_name: self.model.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalRuntimeAuthResponse {
    #[serde(rename = "type")]
    pub auth_type: String,
    pub authorization: String,
    pub expires_at: String,
}

impl ExternalRuntimeAuthResponse {
    pub fn to_runtime_auth_request(&self) -> RuntimeAuthRequest {
        RuntimeAuthRequest {
            authorization: self.authorization.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalRuntimeScopeResponse {
    pub allowed_model_id: String,
    #[serde(default)]
    pub allowed_tools: Vec<ExternalCatalogTool>,
    #[serde(default)]
    pub allowed_skills: Vec<ExternalCatalogSkill>,
    #[serde(default)]
    pub allowed_knowledge_bases: Vec<ExternalCatalogKnowledgeBase>,
}

#[derive(Clone, Debug)]
pub struct ExternalProviderSessionHandle {
    pub provider_session_handle: String,
    pub provider_scope_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalRefreshSessionResponse {
    pub provider_session_handle: String,
    pub provider_scope_id: String,
    pub expires_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalLogoutResponse {
    pub provider_session_handle: String,
    pub logged_out: bool,
}

#[async_trait]
pub trait ExternalProviderClient: Send + Sync {
    async fn authorize_request(
        &self,
        provider: &ExternalAuthProviderConfig,
        request: ExternalAuthorizeRequestData,
    ) -> Result<ExternalAuthorizedRequest, (StatusCode, Json<ErrorResponse>)>;

    async fn authenticate(
        &self,
        provider: &ExternalAuthProviderConfig,
        request: ExternalLoginRequestData,
    ) -> Result<ExternalProviderAuthResponse, (StatusCode, Json<ErrorResponse>)>;

    async fn list_catalog(
        &self,
        provider: &ExternalAuthProviderConfig,
        session: ExternalProviderSessionHandle,
    ) -> Result<ExternalCatalogResponse, (StatusCode, Json<ErrorResponse>)>;

    async fn issue_runtime_context(
        &self,
        provider: &ExternalAuthProviderConfig,
        session: ExternalProviderSessionHandle,
        request: ExternalRuntimeContextRequestData,
    ) -> Result<ExternalRuntimeContextResponse, (StatusCode, Json<ErrorResponse>)>;

    async fn refresh_session(
        &self,
        provider: &ExternalAuthProviderConfig,
        session: ExternalProviderSessionHandle,
    ) -> Result<ExternalRefreshSessionResponse, (StatusCode, Json<ErrorResponse>)>;

    async fn logout(
        &self,
        provider: &ExternalAuthProviderConfig,
        session: ExternalProviderSessionHandle,
    ) -> Result<ExternalLogoutResponse, (StatusCode, Json<ErrorResponse>)>;

    /// List the catalog for an authorized-request principal using provider_scope_id and
    /// external_subject directly, without a session handle. Used for edge-JWT principals.
    async fn list_catalog_by_scope(
        &self,
        provider: &ExternalAuthProviderConfig,
        provider_scope_id: String,
        external_subject: String,
    ) -> Result<ExternalCatalogResponse, (StatusCode, Json<ErrorResponse>)>;

    /// Issue a runtime context for an authorized-request principal using provider_scope_id
    /// and external_subject directly, without a session handle. Used for edge-JWT principals.
    async fn issue_runtime_context_by_scope(
        &self,
        provider: &ExternalAuthProviderConfig,
        provider_scope_id: String,
        external_subject: String,
        request: ExternalRuntimeContextRequestData,
    ) -> Result<ExternalRuntimeContextResponse, (StatusCode, Json<ErrorResponse>)>;
}

#[derive(Clone)]
pub struct HttpExternalProviderClient {
    client: reqwest::Client,
}

impl Default for HttpExternalProviderClient {
    fn default() -> Self {
        Self {
            client: reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("external provider HTTP client configuration is static"),
        }
    }
}

impl HttpExternalProviderClient {
    pub fn shared() -> Arc<dyn ExternalProviderClient> {
        Arc::new(Self::default())
    }

    async fn post_action<T, R>(
        &self,
        provider: &ExternalAuthProviderConfig,
        action: &'static str,
        payload: T,
    ) -> Result<R, (StatusCode, Json<ErrorResponse>)>
    where
        T: Serialize + Send,
        R: for<'de> Deserialize<'de>,
    {
        let request = ExternalProviderActionRequest {
            action,
            provider_id: &provider.id,
            payload,
        };
        let response = self
            .client
            .post(&provider.external_auth_endpoint)
            .json(&request)
            .send()
            .await
            .map_err(|error| {
                error_response_coded(
                    StatusCode::BAD_GATEWAY,
                    format!(
                        "external provider '{}' action '{}' request failed: {error}",
                        provider.id, action
                    ),
                    "external_provider_request_failed",
                )
            })?;
        let status = response.status();
        let body = response.text().await.map_err(|error| {
            error_response_coded(
                StatusCode::BAD_GATEWAY,
                format!(
                    "external provider '{}' action '{}' response read failed: {error}",
                    provider.id, action
                ),
                "external_provider_response_invalid",
            )
        })?;
        if !status.is_success() {
            return Err(map_provider_error_status(
                &provider.id,
                action,
                status,
                &body,
            ));
        }
        let envelope = serde_json::from_str::<ProviderSuccessEnvelope>(&body).map_err(|error| {
            error_response_coded(
                StatusCode::BAD_GATEWAY,
                format!(
                    "external provider '{}' action '{}' response envelope is invalid: {error}",
                    provider.id, action
                ),
                "external_provider_response_invalid",
            )
        })?;
        if envelope.code != 0 {
            return Err(error_response_coded(
                StatusCode::BAD_GATEWAY,
                format!(
                    "external provider '{}' action '{}' returned provider error: {}",
                    provider.id,
                    action,
                    envelope
                        .message
                        .unwrap_or_else(|| format!("code {}", envelope.code))
                ),
                format!("external_provider_code_{}", envelope.code),
            ));
        }
        serde_json::from_value::<R>(envelope.data).map_err(|error| {
            error_response_coded(
                StatusCode::BAD_GATEWAY,
                format!(
                    "external provider '{}' action '{}' response data is invalid: {error}",
                    provider.id, action
                ),
                "external_provider_response_invalid",
            )
        })
    }
}

#[async_trait]
impl ExternalProviderClient for HttpExternalProviderClient {
    async fn authorize_request(
        &self,
        provider: &ExternalAuthProviderConfig,
        request: ExternalAuthorizeRequestData,
    ) -> Result<ExternalAuthorizedRequest, (StatusCode, Json<ErrorResponse>)> {
        let response: ExternalAuthorizeRequestResponse = self
            .post_action(
                provider,
                "authorize_request",
                AuthorizeRequestActionPayload {
                    token: request.token,
                    request: request.request,
                },
            )
            .await?;
        response.into_authorization(&provider.id)
    }

    async fn authenticate(
        &self,
        provider: &ExternalAuthProviderConfig,
        request: ExternalLoginRequestData,
    ) -> Result<ExternalProviderAuthResponse, (StatusCode, Json<ErrorResponse>)> {
        self.post_action(
            provider,
            "authenticate",
            AuthenticateActionPayload {
                username: request.username,
                password: request.password,
                provider_scope_id: request.scope_id,
            },
        )
        .await
    }

    async fn list_catalog(
        &self,
        provider: &ExternalAuthProviderConfig,
        session: ExternalProviderSessionHandle,
    ) -> Result<ExternalCatalogResponse, (StatusCode, Json<ErrorResponse>)> {
        self.post_action(
            provider,
            "list_catalog",
            SessionActionPayload::from(session),
        )
        .await
    }

    async fn issue_runtime_context(
        &self,
        provider: &ExternalAuthProviderConfig,
        session: ExternalProviderSessionHandle,
        request: ExternalRuntimeContextRequestData,
    ) -> Result<ExternalRuntimeContextResponse, (StatusCode, Json<ErrorResponse>)> {
        self.post_action(
            provider,
            "issue_runtime_context",
            IssueRuntimeContextActionPayload {
                session: SessionActionPayload::from(session),
                requested_model_id: request.requested_model_id,
                requested_tool_ids: request.requested_tool_ids,
                requested_skill_ids: request.requested_skill_ids,
                requested_knowledge_base_ids: request.requested_knowledge_base_ids,
            },
        )
        .await
    }

    async fn refresh_session(
        &self,
        provider: &ExternalAuthProviderConfig,
        session: ExternalProviderSessionHandle,
    ) -> Result<ExternalRefreshSessionResponse, (StatusCode, Json<ErrorResponse>)> {
        self.post_action(
            provider,
            "refresh_session",
            SessionActionPayload::from(session),
        )
        .await
    }

    async fn logout(
        &self,
        provider: &ExternalAuthProviderConfig,
        session: ExternalProviderSessionHandle,
    ) -> Result<ExternalLogoutResponse, (StatusCode, Json<ErrorResponse>)> {
        self.post_action(provider, "logout", SessionActionPayload::from(session))
            .await
    }

    async fn list_catalog_by_scope(
        &self,
        provider: &ExternalAuthProviderConfig,
        provider_scope_id: String,
        external_subject: String,
    ) -> Result<ExternalCatalogResponse, (StatusCode, Json<ErrorResponse>)> {
        self.post_action(
            provider,
            "list_catalog_by_scope",
            ScopeActionPayload {
                provider_scope_id,
                external_subject,
            },
        )
        .await
    }

    async fn issue_runtime_context_by_scope(
        &self,
        provider: &ExternalAuthProviderConfig,
        provider_scope_id: String,
        external_subject: String,
        request: ExternalRuntimeContextRequestData,
    ) -> Result<ExternalRuntimeContextResponse, (StatusCode, Json<ErrorResponse>)> {
        self.post_action(
            provider,
            "issue_runtime_context_by_scope",
            ScopeRuntimeContextActionPayload {
                scope: ScopeActionPayload {
                    provider_scope_id,
                    external_subject,
                },
                requested_model_id: request.requested_model_id,
                requested_tool_ids: request.requested_tool_ids,
                requested_skill_ids: request.requested_skill_ids,
                requested_knowledge_base_ids: request.requested_knowledge_base_ids,
            },
        )
        .await
    }
}

#[derive(Serialize)]
struct ExternalProviderActionRequest<'a, T> {
    action: &'static str,
    provider_id: &'a str,
    #[serde(flatten)]
    payload: T,
}

#[derive(Serialize)]
struct AuthorizeRequestActionPayload {
    token: String,
    request: ExternalRequestDescriptor,
}

#[derive(Serialize)]
struct AuthenticateActionPayload {
    username: String,
    password: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider_scope_id: Option<String>,
}

#[derive(Serialize)]
struct SessionActionPayload {
    provider_session_handle: String,
    provider_scope_id: String,
}

impl From<ExternalProviderSessionHandle> for SessionActionPayload {
    fn from(value: ExternalProviderSessionHandle) -> Self {
        Self {
            provider_session_handle: value.provider_session_handle,
            provider_scope_id: value.provider_scope_id,
        }
    }
}

#[derive(Serialize)]
struct IssueRuntimeContextActionPayload {
    #[serde(flatten)]
    session: SessionActionPayload,
    requested_model_id: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    requested_tool_ids: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    requested_skill_ids: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    requested_knowledge_base_ids: Vec<String>,
}

#[derive(Serialize)]
struct ScopeActionPayload {
    provider_scope_id: String,
    external_subject: String,
}

#[derive(Serialize)]
struct ScopeRuntimeContextActionPayload {
    #[serde(flatten)]
    scope: ScopeActionPayload,
    requested_model_id: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    requested_tool_ids: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    requested_skill_ids: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    requested_knowledge_base_ids: Vec<String>,
}

#[derive(Deserialize)]
struct ProviderSuccessEnvelope {
    code: i32,
    #[serde(default)]
    message: Option<String>,
    data: Value,
}

#[derive(Deserialize)]
struct ProviderErrorEnvelope {
    error: ProviderErrorBody,
}

#[derive(Deserialize)]
struct ProviderErrorBody {
    code: String,
    message: String,
}

#[derive(Deserialize)]
struct ProviderApiErrorEnvelope {
    code: i32,
    #[serde(default)]
    message: Option<String>,
}

fn map_provider_error_status(
    provider_id: &str,
    action: &str,
    status: reqwest::StatusCode,
    body: &str,
) -> (StatusCode, Json<ErrorResponse>) {
    let mapped_status = match status {
        reqwest::StatusCode::UNAUTHORIZED => StatusCode::UNAUTHORIZED,
        reqwest::StatusCode::FORBIDDEN => StatusCode::FORBIDDEN,
        reqwest::StatusCode::CONFLICT => StatusCode::CONFLICT,
        reqwest::StatusCode::BAD_REQUEST => StatusCode::BAD_REQUEST,
        _ => StatusCode::BAD_GATEWAY,
    };
    if let Ok(envelope) = serde_json::from_str::<ProviderApiErrorEnvelope>(body)
        && envelope.code != 0
    {
        let message = envelope
            .message
            .unwrap_or_else(|| format!("provider error code {}", envelope.code));
        return error_response_coded(
            mapped_status,
            format!(
                "external provider '{}' action '{}' failed: {}",
                provider_id, action, message
            ),
            format!("external_provider_code_{}", envelope.code),
        );
    }
    if let Ok(envelope) = serde_json::from_str::<ProviderErrorEnvelope>(body) {
        return error_response_coded(
            mapped_status,
            format!(
                "external provider '{}' action '{}' failed: {}",
                provider_id, action, envelope.error.message
            ),
            envelope.error.code,
        );
    }
    error_response_coded(
        mapped_status,
        format!(
            "external provider '{}' action '{}' failed with HTTP {}",
            provider_id, action, status
        ),
        "external_provider_action_failed",
    )
}

pub fn validate_provider_runtime_context(
    provider: &ExternalAuthProviderConfig,
    requested_model_id: &str,
    context: &ExternalRuntimeContextResponse,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    if context.selected_model.id != requested_model_id {
        return Err(error_response_coded(
            StatusCode::FORBIDDEN,
            "requested model is not visible in the selected external scope",
            "external_model_not_visible",
        ));
    }
    if context.runtime_scope.allowed_model_id != requested_model_id {
        return Err(provider_context_conflict(format!(
            "external provider '{}' returned runtime scope for model '{}' while '{}' was requested",
            provider.id, context.runtime_scope.allowed_model_id, requested_model_id
        )));
    }
    validate_bearer_authorization(&context.runtime_auth.authorization)?;
    if context.runtime_auth.auth_type != "moi_runtime_grant" {
        return Err(provider_context_invalid(
            "runtime_auth.type must be moi_runtime_grant",
        ));
    }
    let model_gateway = context
        .capability_descriptors
        .model_gateway
        .as_ref()
        .ok_or_else(|| {
            provider_context_invalid("capability_descriptors.model_gateway is required")
        })?;
    validate_capability_descriptor(provider, model_gateway, "model_gateway")?;
    if let Some(mcp) = context.capability_descriptors.mcp.as_ref() {
        validate_capability_descriptor(provider, mcp, "mcp")?;
    }
    if let Some(skills) = context.capability_descriptors.skills.as_ref() {
        validate_capability_descriptor(provider, skills, "skills")?;
    }
    Ok(())
}

fn validate_bearer_authorization(
    authorization: &str,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    let mut parts = authorization.split_ascii_whitespace();
    let scheme = parts.next();
    let token = parts.next();
    if scheme != Some("Bearer") || token.is_none() || parts.next().is_some() {
        return Err(provider_context_invalid(
            "runtime_auth.authorization must contain exactly one Bearer token",
        ));
    }
    Ok(())
}

pub fn validate_runtime_capability_descriptor(
    descriptor: &RuntimeCapabilityDescriptorRequest,
    expected_type: &str,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    validate_descriptor_strings(
        &descriptor.id,
        &descriptor.descriptor_type,
        &descriptor.transport,
        &descriptor.endpoint_url,
        &descriptor.protocol,
        expected_type,
    )
}

fn validate_capability_descriptor(
    provider: &ExternalAuthProviderConfig,
    descriptor: &ExternalRuntimeCapabilityDescriptor,
    expected_type: &str,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    validate_descriptor_strings(
        &descriptor.id,
        &descriptor.descriptor_type,
        &descriptor.transport,
        &descriptor.endpoint_url,
        &descriptor.protocol,
        expected_type,
    )
    .map_err(|(status, error)| {
        error_response_coded(
            status,
            format!(
                "external provider '{}' returned invalid capability descriptor '{}': {}",
                provider.id, descriptor.id, error.detail
            ),
            error
                .error_code
                .clone()
                .unwrap_or_else(|| "external_provider_runtime_context_invalid".to_string()),
        )
    })
}

fn validate_descriptor_strings(
    id: &str,
    descriptor_type: &str,
    transport: &str,
    endpoint_url: &str,
    protocol: &str,
    expected_type: &str,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    validate_exact_descriptor_string("capability_descriptors[].id", id)?;
    validate_exact_descriptor_string("capability_descriptors[].type", descriptor_type)?;
    validate_exact_descriptor_string("capability_descriptors[].transport", transport)?;
    validate_exact_descriptor_string("capability_descriptors[].protocol", protocol)?;
    if descriptor_type != expected_type {
        return Err(provider_context_invalid(format!(
            "capability descriptor type must be {expected_type}"
        )));
    }
    validate_endpoint_url(endpoint_url)
}

fn validate_exact_descriptor_string(
    field: &str,
    value: &str,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    if value.is_empty() || value.trim() != value || value.chars().any(char::is_control) {
        return Err(provider_context_invalid(format!(
            "{field} must be a non-empty exact string"
        )));
    }
    Ok(())
}

fn validate_endpoint_url(url: &str) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    validate_exact_descriptor_string("capability_descriptors[].endpoint_url", url)?;
    let parsed = reqwest::Url::parse(url).map_err(|error| {
        provider_context_invalid(format!(
            "capability descriptor endpoint_url is invalid: {error}"
        ))
    })?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(provider_context_invalid(
            "capability descriptor endpoint_url must be an absolute http or https URL",
        ));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() || parsed.fragment().is_some() {
        return Err(provider_context_invalid(
            "capability descriptor endpoint_url must not contain userinfo or fragment",
        ));
    }
    Ok(())
}

fn provider_context_invalid(detail: impl Into<String>) -> (StatusCode, Json<ErrorResponse>) {
    error_response_coded(
        StatusCode::BAD_GATEWAY,
        detail,
        "external_provider_runtime_context_invalid",
    )
}

fn provider_context_conflict(detail: impl Into<String>) -> (StatusCode, Json<ErrorResponse>) {
    error_response_coded(
        StatusCode::CONFLICT,
        detail,
        "external_provider_runtime_context_disallowed",
    )
}

pub fn resolve_selected_scope(
    requested_scope_id: Option<&str>,
    response: &ExternalProviderAuthResponse,
) -> Result<ExternalProviderScope, (StatusCode, Json<ErrorResponse>)> {
    let visible_selected = response
        .workspace_scopes
        .iter()
        .find(|scope| scope.id == response.selected_scope.id)
        .cloned()
        .ok_or_else(|| {
            error_response_coded(
                StatusCode::FORBIDDEN,
                "provider selected scope is not visible",
                "external_scope_not_visible",
            )
        })?;

    if let Some(scope_id) = requested_scope_id {
        let visible_requested = response
            .workspace_scopes
            .iter()
            .find(|scope| scope.id == scope_id)
            .cloned()
            .ok_or_else(|| {
                error_response_coded(
                    StatusCode::FORBIDDEN,
                    "external scope is missing or no longer visible",
                    "external_scope_not_visible",
                )
            })?;
        if visible_selected.id != visible_requested.id {
            return Err(error_response_coded(
                StatusCode::FORBIDDEN,
                "provider selected scope does not match requested external scope",
                "external_scope_not_visible",
            ));
        }
        return Ok(visible_selected);
    }

    let default_scopes = response
        .workspace_scopes
        .iter()
        .filter(|scope| scope.is_default)
        .cloned()
        .collect::<Vec<_>>();
    match default_scopes.as_slice() {
        [scope] if scope.id == visible_selected.id => Ok(visible_selected),
        [scope] => Err(error_response_coded(
            StatusCode::FORBIDDEN,
            format!(
                "provider selected scope '{}' does not match default scope '{}'",
                visible_selected.id, scope.id
            ),
            "external_scope_not_visible",
        )),
        [] => Err(error_response_coded(
            StatusCode::FORBIDDEN,
            "external scope must be selected",
            "external_scope_required",
        )),
        _ => Err(error_response_coded(
            StatusCode::FORBIDDEN,
            "external provider returned multiple default scopes",
            "external_scope_ambiguous",
        )),
    }
}

pub fn decrypt_provider_session_handle(
    encryptor: &super::FernetTokenEncryptor,
    encrypted: &str,
) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
    encryptor.decrypt(encrypted).map_err(internal_error)
}

pub fn encrypt_provider_session_handle(
    encryptor: &super::FernetTokenEncryptor,
    handle: &str,
) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
    if handle.is_empty() {
        return Err(error_response(
            StatusCode::BAD_GATEWAY,
            "external provider session handle must not be empty",
        ));
    }
    encryptor.encrypt(handle).map_err(internal_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, extract::State, routing::post};
    use serde_json::{Value, json};
    use std::sync::Arc;
    use tokio::sync::Mutex;

    fn provider(endpoint: String) -> ExternalAuthProviderConfig {
        ExternalAuthProviderConfig {
            id: "moi".to_string(),
            display_name: "MOI".to_string(),
            external_auth_endpoint: endpoint,
        }
    }

    #[tokio::test]
    async fn http_provider_client_uses_moi_external_auth_contract() {
        let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
        async fn handler(
            State(captured): State<Arc<Mutex<Vec<Value>>>>,
            Json(body): Json<Value>,
        ) -> Json<Value> {
            captured.lock().await.push(body.clone());
            match body["action"].as_str() {
                Some("authorize_request") => Json(authorize_request_fixture()),
                Some("authenticate") => Json(authenticate_fixture()),
                Some("list_catalog") => Json(list_catalog_fixture()),
                Some("issue_runtime_context") => Json(issue_runtime_context_fixture()),
                Some("refresh_session") => Json(refresh_session_fixture()),
                Some("logout") => Json(logout_fixture()),
                action => Json(json!({
                    "code": 3,
                    "message": format!("unexpected action {action:?}")
                })),
            }
        }
        let app = Router::new()
            .route("/external-auth", post(handler))
            .with_state(captured.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let endpoint = format!("http://{}/external-auth", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve test provider");
        });

        let client = HttpExternalProviderClient::default();
        let authorized = client
            .authorize_request(
                &provider(endpoint.clone()),
                ExternalAuthorizeRequestData {
                    provider_id: "moi".to_string(),
                    token: "Bearer provider-token".to_string(),
                    request: ExternalRequestDescriptor {
                        method: "POST".to_string(),
                        path: "/chat/stream".to_string(),
                        route: Some("/chat/stream".to_string()),
                        request_id: Some("trace-1".to_string()),
                        body_digest: None,
                    },
                },
            )
            .await
            .expect("authorize_request should succeed");
        assert_eq!(authorized.provider_id, "moi");
        assert_eq!(authorized.external_subject, "moi-user-1");

        let response = client
            .authenticate(
                &provider(endpoint.clone()),
                ExternalLoginRequestData {
                    provider_id: "moi".to_string(),
                    username: "admin".to_string(),
                    password: "secret".to_string(),
                    scope_id: None,
                },
            )
            .await
            .expect("authenticate should succeed");

        assert_eq!(response.external_subject.id, "moi-user-1");
        assert_eq!(response.display_info.username, "admin");
        assert_eq!(response.selected_scope.id, "ws-1");
        assert_eq!(response.provider_session_handle, "provider-session-secret");

        let catalog = client
            .list_catalog(
                &provider(endpoint.clone()),
                ExternalProviderSessionHandle {
                    provider_session_handle: "provider-session-secret".to_string(),
                    provider_scope_id: "ws-1".to_string(),
                },
            )
            .await
            .expect("list_catalog should parse MOI catalog");
        assert_eq!(catalog.models[0].id, "model-qwen");
        let model = ModelListItem::from(catalog.models.into_iter().next().unwrap());
        assert_eq!(model.offering_id, "model-qwen");
        assert_eq!(
            model.access_kind,
            crate::models::ModelAccessKind::ThisDevice
        );
        assert_eq!(
            model.execution_placement,
            crate::models::ModelExecutionPlacement::Edge
        );
        assert_eq!(model.name, "Qwen 2.5");

        let runtime_context = client
            .issue_runtime_context(
                &provider(endpoint.clone()),
                ExternalProviderSessionHandle {
                    provider_session_handle: "provider-session-secret".to_string(),
                    provider_scope_id: "ws-1".to_string(),
                },
                ExternalRuntimeContextRequestData {
                    requested_model_id: "model-qwen".to_string(),
                    requested_tool_ids: vec!["tool-search".to_string()],
                    requested_skill_ids: Vec::new(),
                    requested_knowledge_base_ids: Vec::new(),
                },
            )
            .await
            .expect("issue_runtime_context should parse MOI runtime context");
        assert_eq!(runtime_context.selected_model.id, "model-qwen");
        assert_eq!(
            runtime_context
                .capability_descriptors
                .mcp
                .as_ref()
                .expect("mcp server")
                .transport,
            "streamable_http"
        );
        assert_eq!(runtime_context.runtime_scope.allowed_model_id, "model-qwen");

        let refresh = client
            .refresh_session(
                &provider(endpoint.clone()),
                ExternalProviderSessionHandle {
                    provider_session_handle: "provider-session-secret".to_string(),
                    provider_scope_id: "ws-1".to_string(),
                },
            )
            .await
            .expect("refresh_session should parse MOI response");
        assert_eq!(refresh.provider_session_handle, "provider-session-secret-2");
        assert_eq!(refresh.provider_scope_id, "ws-1");

        let logout = client
            .logout(
                &provider(endpoint),
                ExternalProviderSessionHandle {
                    provider_session_handle: "provider-session-secret-2".to_string(),
                    provider_scope_id: "ws-1".to_string(),
                },
            )
            .await
            .expect("logout should parse MOI response");
        assert!(logout.logged_out);

        let bodies = captured.lock().await.clone();
        assert_eq!(bodies[0]["action"], "authorize_request");
        assert_eq!(bodies[0]["provider_id"], "moi");
        assert_eq!(bodies[0]["token"], "Bearer provider-token");
        assert_eq!(bodies[0]["request"]["method"], "POST");
        assert_eq!(bodies[0]["request"]["path"], "/chat/stream");
        assert_eq!(bodies[0]["request"]["route"], "/chat/stream");
        assert_eq!(bodies[0]["request"]["request_id"], "trace-1");
        assert!(bodies[0]["request"].get("summary").is_none());
        assert!(bodies[0].get("has_agent_binding").is_none());
        assert!(bodies[0].get("has_runtime_auth").is_none());
        assert!(bodies[0].get("selected_model_gateway").is_none());
        assert_eq!(bodies[1]["action"], "authenticate");
        assert_eq!(bodies[1]["provider_id"], "moi");
        assert_eq!(bodies[1]["username"], "admin");
        assert_eq!(bodies[1]["password"], "secret");
        assert!(bodies[1].get("provider_session_handle").is_none());
        assert_eq!(bodies[2]["action"], "list_catalog");
        assert_eq!(
            bodies[2]["provider_session_handle"],
            "provider-session-secret"
        );
        assert_eq!(bodies[2]["provider_scope_id"], "ws-1");
        assert!(bodies[2].get("external_session_handle").is_none());
        assert_eq!(bodies[3]["action"], "issue_runtime_context");
        assert_eq!(bodies[3]["requested_model_id"], "model-qwen");
        assert_eq!(bodies[3]["requested_tool_ids"], json!(["tool-search"]));
        assert!(bodies[3].get("requested_model").is_none());
        assert_eq!(bodies[4]["action"], "refresh_session");
        assert_eq!(
            bodies[4]["provider_session_handle"],
            "provider-session-secret"
        );
        assert_eq!(bodies[4]["provider_scope_id"], "ws-1");
        assert_eq!(bodies[5]["action"], "logout");
        assert_eq!(
            bodies[5]["provider_session_handle"],
            "provider-session-secret-2"
        );
        assert_eq!(bodies[5]["provider_scope_id"], "ws-1");
        server.abort();
    }

    #[tokio::test]
    async fn http_provider_client_logout_failure_returns_error() {
        async fn handler(Json(_body): Json<Value>) -> (StatusCode, Json<Value>) {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "code": 14,
                    "message": "provider logout failed"
                })),
            )
        }
        let app = Router::new().route("/external-auth", post(handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let endpoint = format!("http://{}/external-auth", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve test provider");
        });

        let client = HttpExternalProviderClient::default();
        let err = client
            .logout(
                &provider(endpoint),
                ExternalProviderSessionHandle {
                    provider_session_handle: "provider-session-secret".to_string(),
                    provider_scope_id: "ws-1".to_string(),
                },
            )
            .await
            .expect_err("provider logout failure must be explicit");

        assert_eq!(err.0, StatusCode::BAD_GATEWAY);
        assert_eq!(
            err.1.error_code.as_deref(),
            Some("external_provider_code_14")
        );
        server.abort();
    }

    #[test]
    fn runtime_context_requires_model_gateway_descriptor() {
        let context = ExternalRuntimeContextResponse {
            selected_model: ExternalSelectedModelResponse {
                id: "model-qwen".to_string(),
                model: "qwen".to_string(),
            },
            runtime_auth: ExternalRuntimeAuthResponse {
                auth_type: "moi_runtime_grant".to_string(),
                authorization: "Bearer runtime-grant".to_string(),
                expires_at: "2026-01-01T00:00:00Z".to_string(),
            },
            capability_descriptors: ExternalRuntimeCapabilityDescriptors::default(),
            runtime_scope: ExternalRuntimeScopeResponse {
                allowed_model_id: "model-qwen".to_string(),
                allowed_tools: Vec::new(),
                allowed_skills: Vec::new(),
                allowed_knowledge_bases: Vec::new(),
            },
            runtime_system_prompt: None,
            task_id: "task-1".to_string(),
            manifest_id: "manifest-1".to_string(),
            provider_scope_id: "ws-1".to_string(),
        };

        let err = validate_provider_runtime_context(
            &provider("http://127.0.0.1/external-auth".to_string()),
            "model-qwen",
            &context,
        )
        .expect_err("model gateway descriptor is required");

        assert_eq!(err.0, StatusCode::BAD_GATEWAY);
        assert_eq!(
            err.1.error_code.as_deref(),
            Some("external_provider_runtime_context_invalid")
        );
    }

    #[test]
    fn resolve_selected_scope_requires_visible_scope() {
        let scope = ExternalProviderScope {
            id: "ws-1".to_string(),
            workspace_id: "workspace-1".to_string(),
            name: "Workspace".to_string(),
            account_name: None,
            is_default: true,
        };
        let response = ExternalProviderAuthResponse {
            external_subject: ExternalSubject {
                id: "subject".to_string(),
                provider: "moi".to_string(),
                kind: "user".to_string(),
            },
            display_info: ExternalDisplayInfo {
                username: "user".to_string(),
                nickname: None,
                email: None,
            },
            workspace_scopes: vec![scope.clone()],
            selected_scope: scope.clone(),
            default_scope: scope,
            provider_session_handle: "handle".to_string(),
            expires_at: "2026-01-01T00:00:00Z".to_string(),
        };

        let err = resolve_selected_scope(Some("missing"), &response)
            .expect_err("requested scope must be visible");
        assert_eq!(err.0, StatusCode::FORBIDDEN);
        assert_eq!(
            err.1.error_code.as_deref(),
            Some("external_scope_not_visible")
        );
    }

    #[test]
    fn runtime_context_accepts_complete_capability_descriptors() {
        let value = json!({
            "selected_model": {
                "id": "model-qwen",
                "model": "qwen2.5"
            },
            "runtime_auth": {
                "type": "moi_runtime_grant",
                "authorization": "Bearer runtime-grant",
                "expires_at": "2026-01-01T00:00:00Z"
            },
            "capability_descriptors": {
                "model_gateway": {
                    "id": "moi-model-gateway",
                    "type": "model_gateway",
                    "transport": "http",
                    "endpoint_url": "http://127.0.0.1/api/v1/models/openai/chat/completions",
                    "protocol": "openai_chat_completions",
                    "metadata": {}
                }
            },
            "runtime_scope": {
                "allowed_model_id": "model-qwen",
                "allowed_tools": [],
                "allowed_skills": [],
                "allowed_knowledge_bases": []
            },
            "task_id": "task-1",
            "manifest_id": "manifest-1",
            "provider_scope_id": "ws-1"
        });

        let context: ExternalRuntimeContextResponse =
            serde_json::from_value(value).expect("MOI runtime context should parse");
        let model_gateway = context
            .capability_descriptors
            .model_gateway
            .expect("model gateway");
        assert_eq!(model_gateway.protocol, "openai_chat_completions");
        assert_eq!(
            model_gateway.endpoint_url,
            "http://127.0.0.1/api/v1/models/openai/chat/completions"
        );
    }

    fn authenticate_fixture() -> Value {
        JsonFixture(json!({
            "external_subject": {"id": "moi-user-1", "provider": "moi", "kind": "user"},
            "display_info": {"username": "admin", "nickname": "Admin", "email": "admin@example.com"},
            "workspace_scopes": [{
                "id": "ws-1",
                "workspace_id": "workspace-1",
                "name": "Default Workspace",
                "account_name": "default",
                "is_default": true
            }],
            "selected_scope": {
                "id": "ws-1",
                "workspace_id": "workspace-1",
                "name": "Default Workspace",
                "account_name": "default",
                "is_default": true
            },
            "default_scope": {
                "id": "ws-1",
                "workspace_id": "workspace-1",
                "name": "Default Workspace",
                "account_name": "default",
                "is_default": true
            },
            "provider_session_handle": "provider-session-secret",
            "expires_at": "2026-01-01T00:00:00Z"
        }))
        .success()
    }

    fn authorize_request_fixture() -> Value {
        JsonFixture(json!({
            "ok": true,
            "external_subject": "moi-user-1",
            "provider_scope_id": "ws-1",
            "request_authorization_id": "authz-1"
        }))
        .success()
    }

    fn list_catalog_fixture() -> Value {
        JsonFixture(json!({
            "models": [{
                "id": "model-qwen",
                "name": "qwen",
                "display_name": "Qwen 2.5",
                "model_name": "qwen2.5",
                "provider_ref": "moi",
                "capabilities": ["chat"],
                "parameters": {"temperature": true},
                "limits": {"context_window": 32768, "max_completion_tokens": 4096},
                "metadata": {"description": "MOI model", "architecture": "transformer"}
            }],
            "tools": [{
                "id": "tool-search",
                "name": "search",
                "kind": "mcp",
                "description": "Search",
                "side_effect_class": "read",
                "input_schema": {},
                "output_schema": {},
                "metadata": {}
            }],
            "skills": [],
            "knowledge_bases": []
        }))
        .success()
    }

    fn issue_runtime_context_fixture() -> Value {
        JsonFixture(json!({
            "selected_model": {
                "id": "model-qwen",
                "model": "qwen2.5"
            },
            "runtime_auth": {
                "type": "moi_runtime_grant",
                "authorization": "Bearer runtime-grant",
                "expires_at": "2026-01-01T00:00:00Z"
            },
            "capability_descriptors": {
                "model_gateway": {
                    "id": "moi-model-gateway",
                    "type": "model_gateway",
                    "transport": "http",
                    "endpoint_url": "http://127.0.0.1/api/v1/models/openai/chat/completions",
                    "protocol": "openai_chat_completions",
                    "metadata": {}
                },
                "mcp": {
                    "id": "moi-tools",
                    "type": "mcp",
                    "transport": "streamable_http",
                    "endpoint_url": "http://127.0.0.1/api/v1/mcp/http",
                    "protocol": "mcp",
                    "metadata": {}
                }
            },
            "runtime_scope": {
                "allowed_model_id": "model-qwen",
                "allowed_tools": [{
                    "id": "tool-search",
                    "name": "search",
                    "kind": "mcp",
                    "description": "Search",
                    "side_effect_class": "read",
                    "input_schema": {},
                    "output_schema": {},
                    "metadata": {}
                }],
                "allowed_skills": [],
                "allowed_knowledge_bases": []
            },
            "runtime_system_prompt": "Use MOI runtime grant.",
            "task_id": "task-1",
            "manifest_id": "manifest-1",
            "provider_scope_id": "ws-1"
        }))
        .success()
    }

    fn refresh_session_fixture() -> Value {
        JsonFixture(json!({
            "provider_session_handle": "provider-session-secret-2",
            "provider_scope_id": "ws-1",
            "expires_at": "2026-01-01T01:00:00Z"
        }))
        .success()
    }

    fn logout_fixture() -> Value {
        JsonFixture(json!({
            "provider_session_handle": "provider-session-secret-2",
            "logged_out": true
        }))
        .success()
    }

    struct JsonFixture(Value);

    impl JsonFixture {
        fn success(self) -> Value {
            json!({"code": 0, "data": self.0})
        }
    }
}
