use astra_core::{
    ErrorResponse, STATUS_CANCELLED, STATUS_COMPLETED, STATUS_DELEGATED, STATUS_FAILED,
    STATUS_PAUSED, STATUS_RUNNING, STATUS_WAITING, SharedPool, SubRunState, error_response,
    error_response_coded,
};
use astra_turn_types::{
    TOOL_INVOCATION_RESULT_ARTIFACT_METADATA_KEY, ToolInvocationContractError,
    ToolInvocationResultPayload, UserIntentDelivery, UserIntentStatus,
};
use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    time::Duration,
};
use thiserror::Error;
use uuid::Uuid;

use crate::db_row::RowExt as RunStateDbRow;
use crate::models::AdmittedModelExecution;
use crate::pagination::MAX_API_LIST_LIMIT;

pub const RUN_LIFECYCLE_UNCONFIGURED_ERROR_CODE: &str = "run_lifecycle_unconfigured";
pub const SSE_HEARTBEAT_INTERVAL_SECS: u64 = 15;

pub fn is_run_lifecycle_unconfigured_error(status: StatusCode, error: &ErrorResponse) -> bool {
    status == StatusCode::NOT_IMPLEMENTED
        && error.error_code.as_deref() == Some(RUN_LIFECYCLE_UNCONFIGURED_ERROR_CODE)
}

#[async_trait]
pub trait RunLifecycleService: Send + Sync {
    async fn create_run(
        &self,
        user_id: String,
        request: ChatRequestData,
    ) -> Result<ChatRunRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn stream_chat(
        &self,
        user_id: String,
        request: ChatRequestData,
    ) -> Result<ChatStreamRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn get_run_status(
        &self,
        run_id: String,
        user_id: String,
    ) -> Result<RunStatusRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn get_run_projection(
        &self,
        _run_id: String,
        _user_id: String,
        _recent_limit: u32,
    ) -> Result<RunProjectionRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Run projection not supported",
        ))
    }

    async fn repair_run_projection(
        &self,
        _run_id: String,
        _user_id: String,
        _recent_limit: u32,
    ) -> Result<RunProjectionRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Run projection repair not supported",
        ))
    }

    async fn stream_run(
        &self,
        run_id: String,
        user_id: String,
        last_index: u32,
    ) -> Result<Vec<serde_json::Value>, (StatusCode, Json<ErrorResponse>)>;

    async fn stream_run_live(
        &self,
        run_id: String,
        user_id: String,
        last_index: u32,
    ) -> Result<ChatStreamRecord, (StatusCode, Json<ErrorResponse>)> {
        let events = self.stream_run(run_id.clone(), user_id, last_index).await?;
        Ok(ChatStreamRecord {
            session_id: String::new(),
            run_id,
            events,
            event_rx: None,
        })
    }

    async fn cancel_run(
        &self,
        run_id: String,
        user_id: String,
    ) -> Result<CancelRunRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn cancel_session_runs(
        &self,
        _session_id: String,
        _user_id: String,
    ) -> Result<Vec<CancelRunRecord>, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Session run cancellation not supported",
        ))
    }

    async fn list_runs_cursor(
        &self,
        user_id: String,
        limit: u32,
        cursor: Option<RunListCursor>,
    ) -> Result<RunListRecord, (StatusCode, Json<ErrorResponse>)>;

    /// Return a bounded authoritative snapshot of one session's durable runs.
    /// Active work is retained ahead of terminal history when the snapshot is
    /// truncated. The route layer is responsible for projecting wire types.
    async fn list_session_runs(
        &self,
        _user_id: String,
        _session_id: String,
        _limit: u32,
    ) -> Result<DurableSessionRunPage, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Session run tree not supported",
        ))
    }

    /// Pause an active run. Default: NOT_IMPLEMENTED.
    async fn pause_run(
        &self,
        _run_id: String,
        _user_id: String,
    ) -> Result<RunMutationRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Pause not supported",
        ))
    }

    /// Resume a paused run. Default: NOT_IMPLEMENTED.
    async fn resume_run(
        &self,
        _run_id: String,
        _user_id: String,
    ) -> Result<RunMutationRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Resume not supported",
        ))
    }

    async fn submit_run_user_intent(
        &self,
        _run_id: String,
        _user_id: String,
        _input: RunUserIntentData,
    ) -> Result<RunUserIntentRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Active-run user intents are not supported",
        ))
    }

    /// Drain pending tool approval requests for a run.
    ///
    /// Returns JSON objects with `request_id`, `tool`, `args` fields.
    /// The WS handler calls this during its polling loop to forward
    /// approval requests to the client.
    ///
    /// Default: no-op (returns empty vec).
    async fn drain_approval_requests(&self, _run_id: &str) -> Vec<serde_json::Value> {
        vec![]
    }

    /// Drain pending ask_user prompt requests for a run.
    ///
    /// Returns JSON objects with `request_id`, `question`, `choices`, `default`,
    /// and `context` fields. The WS handler calls this during its polling loop to
    /// forward prompts to the client.
    ///
    /// Default: no-op (returns empty vec).
    async fn drain_user_prompt_requests(&self, _run_id: &str) -> Vec<serde_json::Value> {
        vec![]
    }

    async fn get_run_interaction_event(
        &self,
        _run_id: String,
        _user_id: String,
        _request_id: String,
        _event_type: String,
    ) -> Result<Option<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Durable run interactions are not supported",
        ))
    }

    async fn resolve_run_interaction(
        &self,
        _run_id: String,
        _user_id: String,
        _request_id: String,
        _kind: DurableRunInteractionKind,
        _response_data: serde_json::Value,
    ) -> Result<DurableRunInteractionResolveOutcome, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Durable run interactions are not supported",
        ))
    }

    /// Drain pending tool progress events for a run.
    ///
    /// Returns JSON objects with `kind` field (`started`, `delta`, `completed`).
    /// The WS handler calls this during its polling loop to forward
    /// progress events to the client.
    ///
    /// Default: no-op (returns empty vec).
    async fn drain_progress_events(&self, _run_id: &str) -> Vec<serde_json::Value> {
        vec![]
    }

    /// Wait for in-flight background tasks to finish during graceful shutdown.
    /// Returns `true` if all tasks drained within the timeout.
    /// Default: no-op (returns true immediately).
    async fn drain_background_tasks(&self, _timeout: std::time::Duration) -> bool {
        true
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionBudget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_turns: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hard_turn_limit: Option<u32>,
}

/// Request-scoped execution controls. The default preserves Astra's native
/// behavior; callers must opt in explicitly to change a policy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionPolicyRequest {
    #[serde(default)]
    pub turn_intent: TurnIntentExecutionPolicy,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnIntentExecutionPolicy {
    #[default]
    Auto,
    /// Do not call Astra's auxiliary TurnIntent LLM. The request keeps the
    /// deterministic baseline profile selected when its loop state is built.
    FixedDefault,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestedTurnInteractionMode {
    NonInteractive,
    Prompt,
    Auto,
    Deny,
    Headless,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceBindingRequestKind {
    ServerSandbox,
    EdgeWorkspace,
    CloudWorkspace,
    None,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum WorkspaceSourceRequest {
    EdgePath {
        path: String,
    },
    UploadedSnapshot {
        artifact_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        root: Option<String>,
    },
    GitCheckout {
        repository: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reference: Option<String>,
    },
    Template {
        template_id: String,
    },
    DatasetBundle {
        dataset_id: String,
    },
    ArtifactBundle {
        artifact_id: String,
    },
    Scratch,
    PersistentVolume {
        volume_id: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceAuthorityRequest {
    ReadOnly,
    ReadWrite,
    None,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceBindingRequest {
    pub kind: WorkspaceBindingRequestKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, alias = "cwd", skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<WorkspaceSourceRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authority: Option<WorkspaceAuthorityRequest>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutorBindingRequestKind {
    ServerLocal,
    EdgeAgent,
    OrchestratorManaged,
    ThinClient,
    Mcp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolTransportKindRequest {
    ServerLocal,
    EdgeWs,
    EdgeLedger,
    GatewayRelay,
    SandboxResidentAgent,
    McpHttp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutorStatusRequest {
    Online,
    Offline,
    Degraded,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutorBindingRequest {
    pub kind: ExecutorBindingRequestKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executor_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<ToolTransportKindRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ExecutorStatusRequest>,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeMcpBindingRequest {
    pub id: String,
    pub transport: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<String>,
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub headers: std::collections::HashMap<String, String>,
}

fn runtime_mcp_debug_url(raw: &str) -> String {
    let Ok(mut url) = reqwest::Url::parse(raw) else {
        return "[invalid-url]".to_string();
    };
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    url.to_string()
}

impl std::fmt::Debug for RuntimeMcpBindingRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut header_names = self.headers.keys().map(String::as_str).collect::<Vec<_>>();
        header_names.sort_unstable();
        let redacted_url = runtime_mcp_debug_url(&self.url);
        f.debug_struct("RuntimeMcpBindingRequest")
            .field("id", &self.id)
            .field("transport", &self.transport)
            .field("url", &redacted_url)
            .field("auth_token_present", &self.auth_token.is_some())
            .field("header_names", &header_names)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSelectionRequest {
    pub offering_id: String,
}

/// Server-resolved model identity for one admitted Offering selection.
///
/// This is internal runtime context, not a client wire shape. The model name
/// is derived from the catalog row selected by `offering_id` or from an
/// authenticated external provider context.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedModelSelection {
    pub offering_id: String,
    pub model_name: String,
}

pub const RUNTIME_SEMANTIC_READ_MCP_CONTRACT_VERSION: &str = "astra-semantic-read-mcp-v1";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSemanticReadCapabilityRequest {
    pub contract_version: String,
    pub tools: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeCapabilityDescriptorRequest {
    pub id: String,
    #[serde(rename = "type")]
    pub descriptor_type: String,
    pub transport: String,
    pub endpoint_url: String,
    pub protocol: String,
    /// Provider-authorized, host-owned semantic read contract. Tool names are
    /// exact provider-native identities, never model-facing aliases.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic_read: Option<RuntimeSemanticReadCapabilityRequest>,
    #[serde(default)]
    pub metadata: serde_json::Map<String, serde_json::Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeCapabilityDescriptorsRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_gateway: Option<RuntimeCapabilityDescriptorRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<RuntimeCapabilityDescriptorRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<RuntimeCapabilityDescriptorRequest>,
    // edge_agent: astra-edge executor descriptor injected by moi-core catalog when
    // a sandbox or runner is selected. The astra-server routes tool callbacks to
    // the edge agent identified by id via the existing edge WebSocket registry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edge_agent: Option<RuntimeCapabilityDescriptorRequest>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityServerRefs {
    pub mcp: String,
    pub skills: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentBindingRuntimeRequest {
    pub id: String,
    pub capability_server_refs: CapabilityServerRefs,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeAuthRequest {
    pub authorization: String,
}

impl std::fmt::Debug for RuntimeAuthRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeAuthRequest")
            .field("authorization_present", &!self.authorization.is_empty())
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct RuntimeSkillBindingRequest {
    pub id: String,
    pub url: String,
    pub authorization: String,
}

impl std::fmt::Debug for RuntimeSkillBindingRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let redacted_url = runtime_mcp_debug_url(&self.url);
        f.debug_struct("RuntimeSkillBindingRequest")
            .field("id", &self.id)
            .field("url", &redacted_url)
            .field("authorization_present", &!self.authorization.is_empty())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeProfileRequest {
    RequestScopedRuntimeMcp,
    AgentBindingRegistry,
}

#[derive(Clone, PartialEq)]
pub struct ChatRequestData {
    pub message: String,
    pub user_intent: Option<String>,
    pub parts: Vec<serde_json::Value>,
    pub attachments: Vec<serde_json::Value>,
    pub runtime_system_prompt: Option<String>,
    pub session_id: Option<String>,
    pub full_llm_capture: bool,
    pub agent_id: Option<String>,
    pub model: Option<String>,
    pub model_selection: Option<ModelSelectionRequest>,
    pub resolved_model_selection: Option<ResolvedModelSelection>,
    /// Short-lived execution material for the admitted Offering.
    /// This value is never client supplied, serialized, persisted, or logged.
    pub admitted_model_execution: Option<AdmittedModelExecution>,
    pub capability_descriptors: Option<RuntimeCapabilityDescriptorsRequest>,
    pub provider_runtime_authorized: bool,
    pub agent_binding: Option<AgentBindingRuntimeRequest>,
    pub runtime_auth: Option<RuntimeAuthRequest>,
    pub runtime_skill_binding: Option<RuntimeSkillBindingRequest>,
    pub runtime_profile: Option<RuntimeProfileRequest>,
    pub skill_search: Option<astra_core::SkillSearchSettings>,
    pub allow_skills: Option<Vec<String>>,
    pub allow_skill_sources: Option<Vec<String>>,
    pub allow_tools: Option<Vec<String>>,
    pub enabled_tools: Option<Vec<String>>,
    pub workspace_binding: Option<WorkspaceBindingRequest>,
    pub executor_binding: Option<ExecutorBindingRequest>,
    pub runtime_mcp_bindings: Vec<RuntimeMcpBindingRequest>,
    pub mcp_binding_ids: Option<Vec<String>>,
    pub context: Option<serde_json::Map<String, serde_json::Value>>,
    pub edge_executor_id: Option<String>,
    pub capabilities: Vec<String>,
    pub forward_headers: std::collections::HashMap<String, String>,
    /// Owning workspace from the provider-authorized turn's edge-registration
    /// token (`provider_scope_id`).  Injected at the request-injection layer
    /// and propagated into `ToolExecutionRequest.workspace_record` by the run
    /// lifecycle so that edge workspace isolation checks work correctly on the
    /// MOI provider-authorized turn path.
    pub provider_workspace_id: Option<String>,
    pub execution_budget: Option<ExecutionBudget>,
    pub execution_policy: ExecutionPolicyRequest,
    pub explain: bool,
    pub interaction_mode: Option<RequestedTurnInteractionMode>,
    pub interactive_client: bool,
}

fn redacted_forward_header_names(headers: &std::collections::HashMap<String, String>) -> Vec<&str> {
    let mut names = headers
        .keys()
        .filter(|name| !name.starts_with("__astra_"))
        .map(String::as_str)
        .collect::<Vec<_>>();
    names.sort_unstable();
    names
}

struct RedactedForwardHeadersDebug<'a>(&'a std::collections::HashMap<String, String>);

impl std::fmt::Debug for RedactedForwardHeadersDebug<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names = redacted_forward_header_names(self.0);
        f.debug_struct("RedactedForwardHeaders")
            .field("count", &names.len())
            .field("names", &names)
            .finish()
    }
}

impl std::fmt::Debug for ChatRequestData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatRequestData")
            .field("message", &self.message)
            .field("user_intent", &self.user_intent)
            .field("parts", &self.parts)
            .field("attachments", &self.attachments)
            .field("runtime_system_prompt", &self.runtime_system_prompt)
            .field("session_id", &self.session_id)
            .field("agent_id", &self.agent_id)
            .field("model", &self.model)
            .field("model_selection", &self.model_selection)
            .field("resolved_model_selection", &self.resolved_model_selection)
            .field(
                "admitted_model_execution_present",
                &self.admitted_model_execution.is_some(),
            )
            .field("capability_descriptors", &self.capability_descriptors)
            .field(
                "provider_runtime_authorized",
                &self.provider_runtime_authorized,
            )
            .field("agent_binding", &self.agent_binding)
            .field("runtime_auth", &self.runtime_auth)
            .field("runtime_skill_binding", &self.runtime_skill_binding)
            .field("runtime_profile", &self.runtime_profile)
            .field("skill_search", &self.skill_search)
            .field("allow_skills", &self.allow_skills)
            .field("allow_skill_sources", &self.allow_skill_sources)
            .field("allow_tools", &self.allow_tools)
            .field("workspace_binding", &self.workspace_binding)
            .field("executor_binding", &self.executor_binding)
            .field("runtime_mcp_bindings", &self.runtime_mcp_bindings)
            .field("deprecated_mcp_binding_ids", &self.mcp_binding_ids)
            .field("context", &self.context)
            .field("edge_executor_id", &self.edge_executor_id)
            .field("capabilities", &self.capabilities)
            .field(
                "forward_headers",
                &RedactedForwardHeadersDebug(&self.forward_headers),
            )
            .field("execution_budget", &self.execution_budget)
            .field("execution_policy", &self.execution_policy)
            .field("explain", &self.explain)
            .field("interaction_mode", &self.interaction_mode)
            .field("interactive_client", &self.interactive_client)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChatRunRecord {
    pub session_id: String,
    pub run_id: String,
    pub status: String,
    pub explain: Option<serde_json::Value>,
}

#[derive(Debug)]
pub struct ChatStreamRecord {
    pub session_id: String,
    pub run_id: String,
    /// Batch events (populated after loop completes for persistence).
    pub events: Vec<serde_json::Value>,
    /// When present, SSE events are streamed incrementally through this
    /// channel. The HTTP handler converts this into a streaming response.
    pub event_rx: Option<tokio::sync::mpsc::Receiver<serde_json::Value>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RunStatusRecord {
    pub run_id: String,
    pub session_id: String,
    /// Durable run-tree identity. A missing parent identifies the root
    /// conversation run; clients must not infer lineage from event timing.
    pub parent_run_id: Option<String>,
    pub root_run_id: Option<String>,
    pub depth: u32,
    pub status: String,
    pub waiting_for: Option<String>,
    pub events_count: i64,
    pub workspace: Option<serde_json::Value>,
    pub executor: Option<serde_json::Value>,
    pub transport: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunProjectionCheckpointRecord {
    pub checkpoint_id: String,
    pub checkpoint_kind: String,
    pub checkpoint_version: String,
    pub node_seq: i64,
    pub created_at: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RunProjectionRecord {
    pub run_id: String,
    pub session_id: String,
    pub status: String,
    pub waiting_for: Option<String>,
    pub error_message: Option<String>,
    pub workspace: Option<serde_json::Value>,
    pub executor: Option<serde_json::Value>,
    pub transport: Option<String>,
    pub run_event_high_watermark: i64,
    pub projection_event_idx: i64,
    pub projection_updated_at: String,
    pub projection_hash: String,
    pub latest_event_type: Option<String>,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    pub total_tool_calls: u32,
    pub latest_checkpoint: Option<RunProjectionCheckpointRecord>,
    pub has_durable_projection: bool,
    pub recent_events: Vec<serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CancelRunRecord {
    pub run_id: String,
    pub status: String,
}

/// Generic record for run mutations (pause, resume, etc.).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunMutationDisposition {
    Applied,
    SessionContinuationRequired,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunContinuationRecord {
    pub strategy: String,
    pub session_id: String,
    pub source_run_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunMutationRecord {
    pub run_id: String,
    pub status: String,
    pub previous_status: String,
    pub disposition: RunMutationDisposition,
    pub continuation: Option<RunContinuationRecord>,
}

impl RunMutationRecord {
    pub fn applied(
        run_id: impl Into<String>,
        status: impl Into<String>,
        previous_status: impl Into<String>,
    ) -> Self {
        Self {
            run_id: run_id.into(),
            status: status.into(),
            previous_status: previous_status.into(),
            disposition: RunMutationDisposition::Applied,
            continuation: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunUserIntentData {
    /// Stable idempotency identity supplied by the caller. Delivery and input
    /// remain separate typed fields; identity must never be reconstructed by
    /// concatenating display text with a magic separator.
    pub intent_id: String,
    pub delivery: UserIntentDelivery,
    pub input: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunUserIntentRecord {
    pub run_id: String,
    pub intent_id: String,
    pub status: UserIntentStatus,
    pub duplicate: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RunListRecord {
    pub runs: Vec<RunStatusRecord>,
    pub total: Option<i64>,
    pub limit: u32,
    pub next_cursor: Option<RunListCursor>,
}

/// Cursor for stable run list pagination.
///
/// `updated_at` is the ordering key, paired with `run_id` as the deterministic
/// tie-breaker for rows updated in the same database timestamp.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunListCursor {
    pub updated_at: String,
    pub run_id: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DurableRunListPage {
    pub runs: Vec<DurableRunRecord>,
    pub total: Option<i64>,
    pub next_cursor: Option<RunListCursor>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DurableSessionRunPage {
    pub runs: Vec<DurableRunRecord>,
    pub limit: u32,
    pub truncated: bool,
}

#[must_use]
pub fn validate_run_list_limit(limit: u32) -> u32 {
    limit.clamp(1, MAX_API_LIST_LIMIT)
}

#[must_use]
fn run_list_query_limit(limit: u32) -> i64 {
    i64::from(validate_run_list_limit(limit)) + 1
}

#[must_use]
fn session_run_query_limit(limit: u32) -> i64 {
    i64::from(validate_run_list_limit(limit)) + 1
}

fn sort_session_run_candidates(runs: &mut [DurableRunRecord]) {
    runs.sort_by(|a, b| {
        durable_run_status_is_terminal(&a.status)
            .cmp(&durable_run_status_is_terminal(&b.status))
            .then_with(|| b.updated_at.cmp(&a.updated_at))
            .then_with(|| b.created_at.cmp(&a.created_at))
            .then_with(|| b.run_id.cmp(&a.run_id))
    });
}

fn sort_session_run_tree(runs: &mut [DurableRunRecord]) {
    runs.sort_by(|a, b| {
        a.depth
            .cmp(&b.depth)
            .then_with(|| a.created_at.cmp(&b.created_at))
            .then_with(|| a.run_id.cmp(&b.run_id))
    });
}

/// A bounded working set must remain a valid tree. Selection limits apply to
/// work/history candidates; the ancestors required to interpret those
/// candidates are structural context and are added after selection.
fn include_session_run_ancestors(
    selected: &mut Vec<DurableRunRecord>,
    candidates_by_id: &HashMap<String, DurableRunRecord>,
) {
    let mut included = selected
        .iter()
        .map(|run| run.run_id.clone())
        .collect::<HashSet<_>>();
    let mut frontier = selected
        .iter()
        .filter_map(|run| run.parent_run_id.clone())
        .collect::<Vec<_>>();

    while let Some(parent_id) = frontier.pop() {
        if !included.insert(parent_id.clone()) {
            continue;
        }
        let Some(parent) = candidates_by_id.get(&parent_id) else {
            continue;
        };
        if let Some(grandparent_id) = parent.parent_run_id.clone() {
            frontier.push(grandparent_id);
        }
        selected.push(parent.clone());
    }
}

pub fn run_list_cursor_db_updated_at(cursor: &RunListCursor) -> Result<String, String> {
    let updated_at = cursor.updated_at.trim();
    if updated_at.is_empty() {
        return Err("invalid run list cursor: updated_at is required".to_string());
    }
    let mut db_updated_at = updated_at.replace('T', " ");
    if let Some(stripped) = db_updated_at.strip_suffix('Z') {
        db_updated_at = stripped.to_string();
    }
    if chrono::NaiveDateTime::parse_from_str(&db_updated_at, "%Y-%m-%d %H:%M:%S%.f").is_err() {
        return Err(format!("invalid run list cursor timestamp: {updated_at}"));
    }
    Ok(db_updated_at)
}

pub fn run_list_cursor_run_id(cursor: &RunListCursor) -> Result<String, String> {
    let run_id = cursor.run_id.trim();
    if run_id.is_empty() {
        return Err("invalid run list cursor: run_id is required".to_string());
    }
    Ok(run_id.to_string())
}

fn durable_run_list_next_cursor(run: &DurableRunRecord) -> RunListCursor {
    RunListCursor {
        updated_at: run.updated_at.clone(),
        run_id: run.run_id.clone(),
    }
}

fn durable_run_after_cursor(run: &DurableRunRecord, cursor: &RunListCursor) -> bool {
    run.updated_at < cursor.updated_at
        || (run.updated_at == cursor.updated_at && run.run_id < cursor.run_id)
}

// ─── Durable Run State Store ─────────────────────────────────────────────────

/// Persistent record for a durable agent run.
#[derive(Clone, Debug, PartialEq)]
pub struct DurableRunRecord {
    pub run_id: String,
    pub user_id: String,
    pub session_id: String,
    /// Parent run ID for delegation sub-runs.
    pub parent_run_id: Option<String>,
    /// Root run ID for a delegated run tree.
    pub root_run_id: Option<String>,
    /// Slash-delimited ancestor path, for deterministic subtree queries.
    pub ancestor_path: Option<String>,
    /// Depth from root run. Root run depth is 0.
    pub depth: u32,
    /// Delegation ID this run belongs to.
    pub delegation_id: Option<String>,
    /// Agent profile ID executing this run.
    pub agent_id: Option<String>,
    /// If this run is a verification-gate retry, links to the original run.
    pub retry_of: Option<String>,
    /// Retry blast radius: node, subtree, or siblings.
    pub retry_scope: Option<String>,
    pub status: String,
    pub waiting_for: Option<String>,
    pub owner_pod_id: Option<String>,
    pub owner_lease_expires_at: Option<String>,
    pub run_generation: u64,
    pub last_event_idx: i64,
    pub checkpoint_version: Option<String>,
    pub checkpoint_json: Option<String>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    /// Number of verification-gate retry attempts.
    pub retry_count: u32,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    pub total_tool_calls: u32,
    pub agent_binding_id: Option<String>,
    pub agent_binding_name: Option<String>,
    pub agent_binding_schema_version: Option<String>,
    /// Effective Offering selected for this run. This is an authorization
    /// identity, not a display model name or provider route.
    pub model_offering_id: Option<String>,
    /// Concrete model identity resolved when the run was admitted.
    pub resolved_model_name: Option<String>,
    pub capability_server_refs_json: Option<String>,
    pub runtime_profile: Option<String>,
    pub events: Vec<serde_json::Value>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DurableRunInteractionKind {
    Approval,
    AskUser,
}

impl DurableRunInteractionKind {
    pub fn required_event_type(self) -> &'static str {
        match self {
            Self::Approval => "approval_required",
            Self::AskUser => "ask_user_prompted",
        }
    }

    pub fn resolved_event_type(self) -> &'static str {
        match self {
            Self::Approval => "approval_resolved",
            Self::AskUser => "ask_user_resolved",
        }
    }

    pub fn waiting_for(self) -> &'static str {
        match self {
            Self::Approval => "tool_approval",
            Self::AskUser => "user_input",
        }
    }

    fn idempotency_namespace(self) -> &'static str {
        match self {
            Self::Approval => "approval",
            Self::AskUser => "ask_user",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum DurableRunInteractionResolveOutcome {
    Resolved(serde_json::Value),
    Idempotent(serde_json::Value),
    Conflict(serde_json::Value),
    MissingRequest,
    NoLongerWaiting,
}

/// Narrow control-plane projection. Frequent cross-pod pause/cancel polling
/// must not hydrate event payloads, checkpoints, model bindings, or transcript
/// metadata from the full durable run record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DurableRunControlRecord {
    pub run_id: String,
    pub status: String,
    pub waiting_for: Option<String>,
    pub parent_run_id: Option<String>,
    pub ancestor_path: Option<String>,
}

impl From<&DurableRunRecord> for DurableRunControlRecord {
    fn from(run: &DurableRunRecord) -> Self {
        Self {
            run_id: run.run_id.clone(),
            status: run.status.clone(),
            waiting_for: run.waiting_for.clone(),
            parent_run_id: run.parent_run_id.clone(),
            ancestor_path: run.ancestor_path.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DurableRunStatusKind {
    Running,
    Waiting,
    Paused,
    Completed,
    Delegated,
    Failed,
    Cancelled,
    Other,
}

pub fn durable_run_status_kind(status: &str) -> DurableRunStatusKind {
    match status {
        STATUS_RUNNING => DurableRunStatusKind::Running,
        STATUS_WAITING => DurableRunStatusKind::Waiting,
        STATUS_PAUSED => DurableRunStatusKind::Paused,
        STATUS_COMPLETED => DurableRunStatusKind::Completed,
        STATUS_DELEGATED => DurableRunStatusKind::Delegated,
        STATUS_FAILED => DurableRunStatusKind::Failed,
        STATUS_CANCELLED => DurableRunStatusKind::Cancelled,
        _ => DurableRunStatusKind::Other,
    }
}

pub fn durable_run_status_is_terminal(status: &str) -> bool {
    matches!(
        durable_run_status_kind(status),
        DurableRunStatusKind::Completed
            | DurableRunStatusKind::Delegated
            | DurableRunStatusKind::Failed
            | DurableRunStatusKind::Cancelled
    )
}

/// Whether a root/session run owns the session execution slot.
///
/// `paused` is split intentionally:
/// - `paused(waiting_for = Some(_))` is a manual/user-held pause and keeps the
///   slot so resume cannot race with a different root turn in the same session.
/// - `paused(waiting_for = None)` is non-interactive/budget-exhausted or
///   terminal-buffered state and releases the slot; resuming it either performs
///   completion promotion or reacquires the slot before executing.
pub fn durable_run_status_blocks_session(status: &str, waiting_for: Option<&str>) -> bool {
    matches!(
        durable_run_status_kind(status),
        DurableRunStatusKind::Running | DurableRunStatusKind::Waiting
    ) || (durable_run_status_kind(status) == DurableRunStatusKind::Paused && waiting_for.is_some())
}

pub fn durable_run_status_to_subrun_state(status: &str) -> SubRunState {
    match durable_run_status_kind(status) {
        DurableRunStatusKind::Running => SubRunState::Running,
        DurableRunStatusKind::Waiting => SubRunState::Waiting,
        DurableRunStatusKind::Paused => SubRunState::Paused,
        DurableRunStatusKind::Completed | DurableRunStatusKind::Delegated => SubRunState::Completed,
        DurableRunStatusKind::Failed | DurableRunStatusKind::Other => SubRunState::Failed,
        DurableRunStatusKind::Cancelled => SubRunState::Cancelled,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DurableRunCheckpointRecord {
    pub checkpoint_id: String,
    pub run_id: String,
    pub user_id: String,
    pub session_id: String,
    pub node_seq: i64,
    pub checkpoint_kind: String,
    pub checkpoint_version: String,
    pub idempotency_key: String,
    pub checkpoint_json: String,
    pub created_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DurableRunDisplayProjectionRecord {
    pub run_id: String,
    pub user_id: String,
    pub session_id: String,
    pub status: String,
    pub waiting_for: Option<String>,
    pub error_message: Option<String>,
    pub projection_event_idx: i64,
    pub latest_event_type: Option<String>,
    pub latest_checkpoint_id: Option<String>,
    pub latest_checkpoint_kind: Option<String>,
    pub latest_checkpoint_version: Option<String>,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    pub total_tool_calls: u32,
    pub projection_hash: String,
    pub updated_at: String,
}

const AGENT_RUN_COLUMNS: &str = "run_id, user_id, session_id, parent_run_id, root_run_id, \
     ancestor_path, depth, delegation_id, agent_id, retry_of, retry_scope, status, waiting_for, \
     owner_pod_id, owner_lease_expires_at, run_generation, last_event_idx, checkpoint_version, \
     checkpoint_json, error_code, error_message, retry_count, total_prompt_tokens, \
     total_completion_tokens, total_tool_calls, agent_binding_id, agent_binding_name, \
     agent_binding_schema_version, model_offering_id, resolved_model_name, \
     capability_server_refs_json, runtime_profile, created_at, updated_at";
pub const RUN_RECOVERY_CLAIM_BATCH_SIZE: u32 = 64;
const MAX_RUN_RECOVERY_CLAIM_BATCH_SIZE: u32 = 256;
const RUN_RECOVERY_CLAIM_COLLISION_RETRIES: usize = 4;
const RUN_LIST_CURSOR_SELECT_SQL: &str =
    "DATE_FORMAT(updated_at, '%Y-%m-%dT%H:%i:%s.%f') AS cursor_updated_at";
// Keep the seek predicate whole. Splitting its parentheses across several
// format fragments made a valid query look malformed in review and makes
// future edits unnecessarily risky. Bind order is updated_at, updated_at,
// run_id, matching the lexicographic DESC cursor used by the in-memory store.
const RUN_LIST_CURSOR_PREDICATE_SQL: &str =
    " AND (updated_at < ? OR (updated_at = ? AND run_id < ?))";
const RUN_LIST_ORDER_SQL: &str = " ORDER BY updated_at DESC, run_id DESC";

const RUN_DISPLAY_PROJECTION_COLUMNS: &str = "run_id, user_id, session_id, status, waiting_for, \
     error_message, projection_event_idx, latest_event_type, latest_checkpoint_id, \
     latest_checkpoint_kind, latest_checkpoint_version, total_prompt_tokens, \
     total_completion_tokens, total_tool_calls, projection_hash, updated_at";

/// Abstraction for durable run persistence.
///
/// Implementations:
/// - `InMemoryRunStateStore` — deterministic durable fake for tests
/// - `DatabaseRunStateStore` — MatrixOne-backed persistence
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardedRunStatusTransition {
    Updated,
    StatusConflict,
    SessionBlocked,
}

#[derive(Debug)]
pub struct GuardedRunStatusTransitionRequest<'a> {
    pub user_id: &'a str,
    pub run_id: &'a str,
    pub session_id: &'a str,
    pub expected_statuses: &'a [&'a str],
    pub status: &'a str,
    pub waiting_for: Option<&'a str>,
    pub error_message: Option<&'a str>,
    pub event: serde_json::Value,
}

#[async_trait]
pub trait RunStateStore: Send + Sync {
    /// Insert a new run record.
    async fn insert_run(&self, record: DurableRunRecord) -> Result<(), String>;

    /// Load a run owned by a user.
    async fn load_run(
        &self,
        user_id: &str,
        run_id: &str,
    ) -> Result<Option<DurableRunRecord>, String>;

    async fn load_run_control(
        &self,
        user_id: &str,
        run_id: &str,
    ) -> Result<Option<DurableRunControlRecord>, String> {
        Ok(self
            .load_run(user_id, run_id)
            .await?
            .as_ref()
            .map(DurableRunControlRecord::from))
    }

    async fn load_run_controls(
        &self,
        user_id: &str,
        run_ids: &[String],
    ) -> Result<Vec<DurableRunControlRecord>, String> {
        let mut controls = Vec::with_capacity(run_ids.len());
        for run_id in run_ids {
            if let Some(run) = self.load_run_control(user_id, run_id).await? {
                controls.push(run);
            }
        }
        Ok(controls)
    }

    /// Update run status and optional fields.
    async fn update_run_status(
        &self,
        user_id: &str,
        run_id: &str,
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
    ) -> Result<bool, String>;

    /// Update run status only if the current status is one of `expected_statuses`.
    ///
    /// This is the compare-and-set primitive used by control-plane races where
    /// a stale load must not overwrite a newer pause/cancel/terminal status.
    async fn update_run_status_if_current(
        &self,
        user_id: &str,
        run_id: &str,
        expected_statuses: &[&str],
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
    ) -> Result<bool, String>;

    /// Atomically update run status and append one durable event if the current
    /// status is one of `expected_statuses`.
    ///
    /// This is the control-plane transition primitive for pause/resume/cancel:
    /// status and its audit event must commit together or not at all.
    #[allow(clippy::too_many_arguments)]
    async fn update_run_status_with_event_if_current(
        &self,
        user_id: &str,
        run_id: &str,
        expected_statuses: &[&str],
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
        event: serde_json::Value,
    ) -> Result<bool, String>;

    /// Atomically update run status and append one durable event only when no
    /// other run in the same user/session currently blocks execution.
    ///
    /// The current run is excluded from the session-blocking check so a
    /// manual-paused run can resume itself. Store implementations should make
    /// the status CAS and session guard one durable operation. The default
    /// fails closed so new store implementations cannot silently inherit a
    /// non-atomic check-then-update fallback.
    async fn update_run_status_with_event_if_current_unless_session_blocked(
        &self,
        request: GuardedRunStatusTransitionRequest<'_>,
    ) -> Result<GuardedRunStatusTransition, String> {
        let _ = request;
        Err(
            "session-guarded run status transition is not implemented for this run store"
                .to_string(),
        )
    }

    /// Atomically update run status and append a durable event batch if the
    /// current status is one of `expected_statuses`.
    ///
    /// Empty `events` is allowed and behaves as a status-only CAS transition.
    #[allow(clippy::too_many_arguments)]
    async fn update_run_status_with_events_if_current(
        &self,
        user_id: &str,
        run_id: &str,
        expected_statuses: &[&str],
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
        events: &[serde_json::Value],
    ) -> Result<bool, String>;

    /// Update token/tool counts.
    async fn update_run_usage(
        &self,
        user_id: &str,
        run_id: &str,
        prompt_tokens: u64,
        completion_tokens: u64,
        tool_calls: u32,
    ) -> Result<bool, String>;

    /// Save checkpoint JSON for crash recovery.
    async fn save_checkpoint(
        &self,
        user_id: &str,
        run_id: &str,
        checkpoint_json: &str,
    ) -> Result<bool, String>;

    /// Load the newest checkpoint for a run, optionally filtered by kind.
    async fn load_latest_checkpoint(
        &self,
        user_id: &str,
        run_id: &str,
        checkpoint_kind: Option<&str>,
    ) -> Result<Option<DurableRunCheckpointRecord>, String>;

    /// Load the current typed display projection for a durable run.
    async fn load_run_projection(
        &self,
        user_id: &str,
        run_id: &str,
    ) -> Result<Option<DurableRunDisplayProjectionRecord>, String>;

    /// Rebuild the typed display projection from durable run facts.
    ///
    /// This is an explicit repair path for projection writes that failed after
    /// the authoritative run status/event/checkpoint facts already committed.
    async fn rebuild_run_projection(
        &self,
        user_id: &str,
        run_id: &str,
    ) -> Result<Option<DurableRunDisplayProjectionRecord>, String>;

    /// Append multiple events in a single batch. This is the canonical write
    /// path. Single-event callers should use `append_event` which delegates here.
    async fn append_events_batch(
        &self,
        user_id: &str,
        run_id: &str,
        events: &[serde_json::Value],
    ) -> Result<(), String>;

    /// Load one canonical interaction fact without hydrating the run's full
    /// event history. Shared stores must use the normalized request identity.
    async fn load_run_interaction_event(
        &self,
        user_id: &str,
        run_id: &str,
        request_id: &str,
        event_type: &str,
    ) -> Result<Option<serde_json::Value>, String> {
        let run = self.load_run(user_id, run_id).await?;
        Ok(run.and_then(|run| {
            run.events.into_iter().rev().find(|event| {
                extract_event_type(event) == event_type
                    && extract_interaction_request_id(event).as_deref() == Some(request_id)
            })
        }))
    }

    /// Resolve a durable interaction and release its run wait in one atomic
    /// transition. The response and `run_resumed` facts commit together.
    async fn resolve_run_interaction(
        &self,
        _user_id: &str,
        _run_id: &str,
        _request_id: &str,
        _kind: DurableRunInteractionKind,
        _response_data: serde_json::Value,
    ) -> Result<DurableRunInteractionResolveOutcome, String> {
        Err("durable run interaction resolution is not supported by this store".to_string())
    }

    /// Append a single event. Default implementation delegates to
    /// `append_events_batch`.
    async fn append_event(
        &self,
        user_id: &str,
        run_id: &str,
        event: serde_json::Value,
    ) -> Result<(), String> {
        self.append_events_batch(user_id, run_id, &[event]).await
    }

    /// List runs with seek pagination. This path intentionally does not compute
    /// an exact total; callers should use `next_cursor`/short page semantics.
    async fn list_user_runs_cursor(
        &self,
        user_id: &str,
        limit: u32,
        cursor: Option<RunListCursor>,
    ) -> Result<DurableRunListPage, String> {
        let _ = (user_id, limit, cursor);
        Err("run list cursor pagination is not supported by this store".to_string())
    }

    /// List a bounded working set for one session. Unknown lifecycle values
    /// intentionally rank with active work so the API projection can surface
    /// the invalid durable value instead of silently hiding it in old history.
    async fn list_session_runs(
        &self,
        user_id: &str,
        session_id: &str,
        limit: u32,
    ) -> Result<DurableSessionRunPage, String> {
        let _ = (user_id, session_id, limit);
        Err("session run listing is not supported by this store".to_string())
    }

    /// Seek-page through only active runs in one session. Mutation workflows
    /// use this instead of repeatedly reading the bounded tree projection,
    /// where unrelated active rows can permanently hide descendants beyond
    /// the first page.
    async fn list_active_session_runs_cursor(
        &self,
        user_id: &str,
        session_id: &str,
        limit: u32,
        cursor: Option<RunListCursor>,
    ) -> Result<DurableRunListPage, String> {
        let _ = (user_id, session_id, limit, cursor);
        Err("active session run cursor pagination is not supported by this store".to_string())
    }

    /// Load the same bounded session working set plus only the lifecycle
    /// events required to reconstruct read-only agent/fanout results. The
    /// database implementation overrides this with two batch queries; this
    /// fallback is for deterministic test stores.
    async fn load_session_agent_recovery(
        &self,
        user_id: &str,
        session_id: &str,
        limit: u32,
    ) -> Result<DurableSessionRunPage, String> {
        let mut page = self.list_session_runs(user_id, session_id, limit).await?;
        for run in &mut page.runs {
            if let Some(full) = self.load_run(user_id, &run.run_id).await? {
                run.events = full.events;
            }
        }
        Ok(page)
    }

    /// Find runs in WAITING status (for resume engine).
    async fn find_waiting_runs(&self) -> Result<Vec<DurableRunRecord>, String>;

    /// Find runs in RUNNING status.
    async fn find_running_runs(&self) -> Result<Vec<DurableRunRecord>, String>;

    /// Find paused runs that still hold a blocking execution wait.
    async fn find_blocking_paused_runs(&self) -> Result<Vec<DurableRunRecord>, String> {
        Ok(Vec::new())
    }

    /// Atomically claim a bounded batch of active runs for restart recovery.
    ///
    /// This includes waiting, running, and blocking paused runs
    /// that need recovery classification. Shared durable stores must override
    /// this operation so concurrent pods claim disjoint work and never return
    /// rows protected by another live owner's lease. The fallback is intended
    /// for process-local deterministic stores.
    async fn claim_recoverable_active_runs(
        &self,
        limit: u32,
    ) -> Result<Vec<DurableRunRecord>, String> {
        let limit = limit.clamp(1, MAX_RUN_RECOVERY_CLAIM_BATCH_SIZE) as usize;
        let mut active = self.find_waiting_runs().await?;
        active.extend(self.find_running_runs().await?);
        active.extend(self.find_blocking_paused_runs().await?);
        active.retain(|run| {
            matches!(run.status.as_str(), STATUS_WAITING | STATUS_RUNNING)
                || (run.status == STATUS_PAUSED && run.waiting_for.is_some())
        });
        active.sort_by(|left, right| {
            left.updated_at
                .cmp(&right.updated_at)
                .then_with(|| left.user_id.cmp(&right.user_id))
                .then_with(|| left.run_id.cmp(&right.run_id))
        });
        active.truncate(limit);
        Ok(active)
    }

    /// Return the interval at which the runtime should renew this store
    /// owner's active run leases. Stores without shared lease state can return
    /// `None`.
    fn owner_lease_renewal_interval(&self) -> Option<Duration> {
        None
    }

    /// Renew the current store owner's lease for a live run.
    ///
    /// Shared durable stores should only renew rows still owned by this store
    /// instance and still in one of the expected active statuses. Returning
    /// `Ok(false)` tells the runtime to stop heartbeating that run.
    async fn renew_owner_lease(
        &self,
        _user_id: &str,
        _run_id: &str,
        _expected_statuses: &[&str],
    ) -> Result<bool, String> {
        Ok(false)
    }

    /// Release this store owner's lease when the process-local executor exits.
    ///
    /// A graceful task exit should not make other runtimes wait for the lease
    /// TTL before they can distinguish durable history from a live executor.
    /// Shared stores must fence this update by their own owner identity.
    async fn release_owner_lease(&self, _user_id: &str, _run_id: &str) -> Result<bool, String> {
        Ok(false)
    }

    /// Find the newest run that blocks starting another run in the same session.
    async fn find_blocking_session_run(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> Result<Option<DurableRunRecord>, String>;

    /// Find all sub-runs belonging to a delegation.
    async fn find_sub_runs(
        &self,
        user_id: &str,
        delegation_id: &str,
    ) -> Result<Vec<DurableRunRecord>, String>;

    /// Update the retry count for a run (verification gate retries).
    async fn update_retry_count(
        &self,
        user_id: &str,
        run_id: &str,
        retry_count: u32,
    ) -> Result<bool, String>;
}

/// In-memory run state store for tests and single-process deployments.
pub struct InMemoryRunStateStore {
    // Lock-order invariant for operations that need both maps:
    // `execution_slots` must be acquired before `runs`. Single-map operations
    // must release their guard before acquiring the other map. Keeping the
    // order adjacent to the fields makes future transitions auditable.
    execution_slots: tokio::sync::RwLock<std::collections::HashMap<(String, String), String>>,
    runs: tokio::sync::RwLock<std::collections::HashMap<String, DurableRunRecord>>,
    checkpoints:
        tokio::sync::RwLock<std::collections::HashMap<String, Vec<DurableRunCheckpointRecord>>>,
    projections:
        tokio::sync::RwLock<std::collections::HashMap<String, DurableRunDisplayProjectionRecord>>,
}

impl InMemoryRunStateStore {
    /// Maximum number of runs kept in memory. When exceeded, the oldest
    /// completed/failed runs are evicted on insert.
    pub const MAX_RUNS: usize = 10_000;

    pub fn new() -> Self {
        Self {
            execution_slots: tokio::sync::RwLock::new(std::collections::HashMap::new()),
            runs: tokio::sync::RwLock::new(std::collections::HashMap::new()),
            checkpoints: tokio::sync::RwLock::new(std::collections::HashMap::new()),
            projections: tokio::sync::RwLock::new(std::collections::HashMap::new()),
        }
    }

    async fn sync_projection(
        &self,
        run: &DurableRunRecord,
        latest_event_type: Option<String>,
        latest_checkpoint: Option<&DurableRunCheckpointRecord>,
    ) {
        let mut projections = self.projections.write().await;
        let existing = projections.get(&run.run_id).cloned();
        let projection = build_run_display_projection(
            run,
            latest_event_type.or_else(|| {
                existing
                    .as_ref()
                    .and_then(|entry| entry.latest_event_type.clone())
            }),
            latest_checkpoint.map(checkpoint_summary_tuple).or_else(|| {
                existing.as_ref().and_then(|entry| {
                    Some((
                        entry.latest_checkpoint_id.clone()?,
                        entry.latest_checkpoint_kind.clone()?,
                        entry.latest_checkpoint_version.clone()?,
                    ))
                })
            }),
        );
        projections.insert(run.run_id.clone(), projection);
    }
}

impl Default for InMemoryRunStateStore {
    fn default() -> Self {
        Self::new()
    }
}

fn run_requires_session_execution_slot(record: &DurableRunRecord) -> bool {
    record.parent_run_id.is_none()
        && record.retry_of.is_none()
        && record.delegation_id.is_none()
        && record.agent_id.is_none()
}

fn sync_in_memory_execution_slot(
    slots: &mut std::collections::HashMap<(String, String), String>,
    run: &DurableRunRecord,
    status: &str,
    waiting_for: Option<&str>,
) -> Result<(), String> {
    if !run_requires_session_execution_slot(run) {
        return Ok(());
    }
    let key = (run.user_id.clone(), run.session_id.clone());
    if durable_run_status_blocks_session(status, waiting_for) {
        match slots.get(&key) {
            Some(owner) if owner != &run.run_id => Err("session already has an active run".into()),
            _ => {
                slots.insert(key, run.run_id.clone());
                Ok(())
            }
        }
    } else {
        if slots.get(&key).is_some_and(|owner| owner == &run.run_id) {
            slots.remove(&key);
        }
        Ok(())
    }
}

fn reconcile_in_memory_execution_slot_for_session(
    slots: &mut std::collections::HashMap<(String, String), String>,
    runs: &std::collections::HashMap<String, DurableRunRecord>,
    user_id: &str,
    session_id: &str,
    scan_if_missing: bool,
) -> Result<(), String> {
    let key = (user_id.to_string(), session_id.to_string());

    // `execution_slots` is maintained under the same lock order as `runs`, so
    // an absent slot is authoritative for non-blocking mutations. A blocking
    // acquisition may request a defensive scan, while a present invalid owner
    // always triggers repair. Scanning on every completed-history insert made
    // bounded in-memory retention O(n²).
    match slots.get(&key) {
        None if !scan_if_missing => return Ok(()),
        None => {}
        Some(owner)
            if runs.get(owner).is_some_and(|run| {
                run.user_id == user_id
                    && run.session_id == session_id
                    && run_requires_session_execution_slot(run)
                    && durable_run_status_blocks_session(&run.status, run.waiting_for.as_deref())
            }) =>
        {
            return Ok(());
        }
        Some(_) => {}
    }

    let mut owner: Option<&str> = None;
    for run in runs.values().filter(|run| {
        run.user_id == user_id
            && run.session_id == session_id
            && run_requires_session_execution_slot(run)
            && durable_run_status_blocks_session(&run.status, run.waiting_for.as_deref())
    }) {
        if let Some(existing) = owner {
            return Err(format!(
                "in-memory session execution invariant violated: session {session_id} has multiple blocking root runs ({existing}, {})",
                run.run_id
            ));
        }
        owner = Some(run.run_id.as_str());
    }

    if let Some(owner) = owner {
        slots.insert(key, owner.to_string());
    } else {
        slots.remove(&key);
    }
    Ok(())
}

fn apply_in_memory_status_transition(
    slots: &mut std::collections::HashMap<(String, String), String>,
    run: &mut DurableRunRecord,
    status: &str,
    waiting_for: Option<&str>,
    error_message: Option<&str>,
    terminal_error_code: Option<&str>,
) -> Result<(), String> {
    ensure_terminal_status_immutable(run, status)?;

    sync_in_memory_execution_slot(slots, run, status, waiting_for)?;
    run.status = status.to_string();
    run.waiting_for = waiting_for.map(ToString::to_string);
    if let Some(msg) = error_message {
        run.error_message = Some(msg.to_string());
    }
    if let Some(code) = terminal_error_code {
        run.error_code = Some(code.to_string());
    }
    run.updated_at = chrono::Utc::now().to_rfc3339();
    Ok(())
}

/// Enforce one terminal-state contract across in-memory and database stores.
/// Same-status replay remains idempotent; changing a terminal fact does not.
fn ensure_terminal_status_immutable(
    run: &DurableRunRecord,
    requested_status: &str,
) -> Result<(), String> {
    let current_status = run.status.as_str();
    if durable_run_status_is_terminal(current_status) && requested_status != current_status {
        return Err(format!(
            "terminal state immutability violated: run {} is already {}, cannot transition to {}",
            run.run_id, current_status, requested_status
        ));
    }
    Ok(())
}

fn new_idempotent_events(
    existing: &[serde_json::Value],
    incoming: &[serde_json::Value],
) -> Vec<serde_json::Value> {
    let mut idempotency_keys = existing
        .iter()
        .filter_map(|event| extract_optional_string(event, "idempotency_key"))
        .collect::<std::collections::HashSet<_>>();
    incoming
        .iter()
        .filter(|event| {
            extract_optional_string(event, "idempotency_key")
                .is_none_or(|key| idempotency_keys.insert(key))
        })
        .cloned()
        .collect()
}

fn in_memory_transition_changes_state(
    run: &DurableRunRecord,
    status: &str,
    waiting_for: Option<&str>,
    error_message: Option<&str>,
) -> bool {
    run.status != status
        || run.waiting_for.as_deref() != waiting_for
        || error_message.is_some_and(|message| run.error_message.as_deref() != Some(message))
}

fn session_execution_slot_owner_reclaimable(
    status: &str,
    waiting_for: Option<&str>,
    owner_lease_expired: bool,
    slot_is_stale: bool,
) -> bool {
    let blocks = durable_run_status_blocks_session(status, waiting_for);
    if !blocks {
        return true;
    }
    matches!(
        durable_run_status_kind(status),
        DurableRunStatusKind::Running | DurableRunStatusKind::Waiting
    ) && owner_lease_expired
        && slot_is_stale
}

fn checkpoint_metadata(
    run_id: &str,
    checkpoint_json: &str,
) -> Result<(String, String, String), String> {
    let value: serde_json::Value =
        serde_json::from_str(checkpoint_json).map_err(|error| error.to_string())?;
    let object = value
        .as_object()
        .ok_or_else(|| "checkpoint payload must be a JSON object".to_string())?;
    let checkpoint_kind = if object.contains_key("phase") {
        "phase".to_string()
    } else {
        validate_checkpoint_payload(object)?;
        if object
            .get("graceful")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            "resume".to_string()
        } else {
            "checkpoint".to_string()
        }
    };
    let checkpoint_version = object
        .get("version")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("phase_checkpoint_v1")
        .to_string();
    let idempotency_key = object
        .get("last_batch_id")
        .and_then(serde_json::Value::as_str)
        .map(|batch_id| format!("checkpoint:{run_id}:{checkpoint_kind}:{batch_id}"))
        .unwrap_or_else(|| {
            let hash = sha256_hex(checkpoint_json.as_bytes());
            format!("checkpoint:{run_id}:{checkpoint_kind}:{hash}")
        });
    Ok((checkpoint_kind, checkpoint_version, idempotency_key))
}

fn validate_checkpoint_payload(
    object: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), String> {
    let Some(version) = object.get("version").and_then(serde_json::Value::as_str) else {
        return Err("version must be checkpoint_vN".to_string());
    };
    if !is_checkpoint_version(version) {
        return Err("version must be checkpoint_vN".to_string());
    }
    if object.contains_key("graceful")
        && !matches!(object.get("graceful"), Some(serde_json::Value::Bool(_)))
    {
        return Err("graceful must be boolean".to_string());
    }
    if object.contains_key("last_batch_id")
        && object
            .get("last_batch_id")
            .and_then(serde_json::Value::as_str)
            .is_none()
    {
        return Err("last_batch_id must be string".to_string());
    }
    let extra = match object.get("extra") {
        Some(serde_json::Value::Object(extra)) => Some(extra),
        Some(_) => return Err("extra must be object".to_string()),
        None => None,
    };
    if let Some(partial) = extra.and_then(|extra| {
        extra
            .get("partial_progress")
            .and_then(serde_json::Value::as_object)
    }) {
        for key in ["step_index", "total_steps", "resumable_marker"] {
            if !partial.contains_key(key) {
                return Err(format!("extra.partial_progress.{key} is required"));
            }
        }
    }
    Ok(())
}

fn is_checkpoint_version(version: &str) -> bool {
    version
        .strip_prefix("checkpoint_v")
        .is_some_and(|suffix| !suffix.is_empty() && suffix.chars().all(|ch| ch.is_ascii_digit()))
}

fn checkpoint_summary_tuple(checkpoint: &DurableRunCheckpointRecord) -> (String, String, String) {
    (
        checkpoint.checkpoint_id.clone(),
        checkpoint.checkpoint_kind.clone(),
        checkpoint.checkpoint_version.clone(),
    )
}

fn build_run_display_projection(
    run: &DurableRunRecord,
    latest_event_type: Option<String>,
    latest_checkpoint: Option<(String, String, String)>,
) -> DurableRunDisplayProjectionRecord {
    let payload = serde_json::json!({
        "run_id": run.run_id,
        "status": run.status,
        "waiting_for": run.waiting_for,
        "error_message": run.error_message,
        "projection_event_idx": run.last_event_idx,
        "latest_event_type": latest_event_type.clone(),
        "latest_checkpoint_id": latest_checkpoint.as_ref().map(|value| value.0.as_str()),
        "latest_checkpoint_kind": latest_checkpoint.as_ref().map(|value| value.1.as_str()),
        "latest_checkpoint_version": latest_checkpoint.as_ref().map(|value| value.2.as_str()),
        "total_prompt_tokens": run.total_prompt_tokens,
        "total_completion_tokens": run.total_completion_tokens,
        "total_tool_calls": run.total_tool_calls,
    });
    DurableRunDisplayProjectionRecord {
        run_id: run.run_id.clone(),
        user_id: run.user_id.clone(),
        session_id: run.session_id.clone(),
        status: run.status.clone(),
        waiting_for: run.waiting_for.clone(),
        error_message: run.error_message.clone(),
        projection_event_idx: run.last_event_idx,
        latest_event_type,
        latest_checkpoint_id: latest_checkpoint.as_ref().map(|value| value.0.clone()),
        latest_checkpoint_kind: latest_checkpoint.as_ref().map(|value| value.1.clone()),
        latest_checkpoint_version: latest_checkpoint.as_ref().map(|value| value.2.clone()),
        total_prompt_tokens: run.total_prompt_tokens,
        total_completion_tokens: run.total_completion_tokens,
        total_tool_calls: run.total_tool_calls,
        projection_hash: sha256_hex(payload.to_string().as_bytes()),
        updated_at: run.updated_at.clone(),
    }
}

fn usage_projection_patch_hash(
    run_id: &str,
    prompt_tokens: u64,
    completion_tokens: u64,
    tool_calls: u32,
) -> String {
    let payload = serde_json::json!({
        "run_id": run_id,
        "usage_patch": {
            "total_prompt_tokens": prompt_tokens,
            "total_completion_tokens": completion_tokens,
            "total_tool_calls": tool_calls,
        },
    });
    sha256_hex(payload.to_string().as_bytes())
}

fn status_projection_patch_hash(
    run_id: &str,
    status: &str,
    waiting_for: Option<&str>,
    error_message: Option<&str>,
    projection_event_idx: i64,
    latest_event_type: Option<&str>,
) -> String {
    let payload = serde_json::json!({
        "run_id": run_id,
        "status_patch": {
            "status": status,
            "waiting_for": waiting_for,
            "error_message": error_message,
            "projection_event_idx": projection_event_idx,
            "latest_event_type": latest_event_type,
        },
    });
    sha256_hex(payload.to_string().as_bytes())
}

#[async_trait]
impl RunStateStore for InMemoryRunStateStore {
    async fn insert_run(&self, record: DurableRunRecord) -> Result<(), String> {
        let mut slots = self.execution_slots.write().await;
        let mut runs = self.runs.write().await;
        let run_id = record.run_id.clone();
        reconcile_in_memory_execution_slot_for_session(
            &mut slots,
            &runs,
            &record.user_id,
            &record.session_id,
            run_requires_session_execution_slot(&record)
                && durable_run_status_blocks_session(&record.status, record.waiting_for.as_deref()),
        )?;
        sync_in_memory_execution_slot(
            &mut slots,
            &record,
            &record.status,
            record.waiting_for.as_deref(),
        )?;
        runs.insert(run_id.clone(), record);
        let Some(inserted) = runs.get(run_id.as_str()).cloned() else {
            return Err(format!(
                "inserted run disappeared before projection sync: {run_id}"
            ));
        };

        // Evict oldest completed/failed runs when over capacity
        let mut evicted_ids = Vec::new();
        if runs.len() > Self::MAX_RUNS {
            let mut evictable: Vec<_> = runs
                .iter()
                .filter(|(_, r)| durable_run_status_is_terminal(&r.status))
                .map(|(id, r)| (id.clone(), r.updated_at.clone()))
                .collect();
            evictable.sort_by(|a, b| a.1.cmp(&b.1));
            let to_remove = runs.len() - Self::MAX_RUNS;
            for (id, _) in evictable.into_iter().take(to_remove) {
                runs.remove(&id);
                evicted_ids.push(id);
            }
        }
        for id in &evicted_ids {
            slots.retain(|_, owner| owner != id);
        }
        drop(runs);
        drop(slots);
        if !evicted_ids.is_empty() {
            {
                let mut projections = self.projections.write().await;
                for id in &evicted_ids {
                    projections.remove(id);
                }
            }
            {
                let mut checkpoints = self.checkpoints.write().await;
                for id in &evicted_ids {
                    checkpoints.remove(id);
                }
            }
        }
        self.sync_projection(&inserted, Some("run_started".to_string()), None)
            .await;
        Ok(())
    }

    async fn load_run(
        &self,
        user_id: &str,
        run_id: &str,
    ) -> Result<Option<DurableRunRecord>, String> {
        let runs = self.runs.read().await;
        Ok(runs
            .get(run_id)
            .filter(|run| run.user_id == user_id)
            .cloned())
    }

    async fn update_run_status(
        &self,
        user_id: &str,
        run_id: &str,
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
    ) -> Result<bool, String> {
        let terminal_error_code = terminal_error_code_from_message(status, error_message);
        let updated = {
            let mut slots = self.execution_slots.write().await;
            let mut runs = self.runs.write().await;
            if let Some(run) = runs.get(run_id)
                && run.user_id == user_id
            {
                reconcile_in_memory_execution_slot_for_session(
                    &mut slots,
                    &runs,
                    user_id,
                    &run.session_id,
                    durable_run_status_blocks_session(status, waiting_for),
                )?;
            }
            if let Some(run) = runs.get_mut(run_id) {
                if run.user_id != user_id {
                    None
                } else {
                    apply_in_memory_status_transition(
                        &mut slots,
                        run,
                        status,
                        waiting_for,
                        error_message,
                        terminal_error_code.as_deref(),
                    )?;
                    Some(run.clone())
                }
            } else {
                None
            }
        };
        if let Some(run) = updated {
            self.sync_projection(&run, None, None).await;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn update_run_status_if_current(
        &self,
        user_id: &str,
        run_id: &str,
        expected_statuses: &[&str],
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
    ) -> Result<bool, String> {
        if expected_statuses.is_empty() {
            return Ok(false);
        }
        let terminal_error_code = terminal_error_code_from_message(status, error_message);
        let updated = {
            let mut slots = self.execution_slots.write().await;
            let mut runs = self.runs.write().await;
            if let Some(run) = runs.get(run_id)
                && run.user_id == user_id
            {
                reconcile_in_memory_execution_slot_for_session(
                    &mut slots,
                    &runs,
                    user_id,
                    &run.session_id,
                    durable_run_status_blocks_session(status, waiting_for),
                )?;
            }
            if let Some(run) = runs.get_mut(run_id) {
                if run.user_id != user_id || !expected_statuses.contains(&run.status.as_str()) {
                    None
                } else {
                    apply_in_memory_status_transition(
                        &mut slots,
                        run,
                        status,
                        waiting_for,
                        error_message,
                        terminal_error_code.as_deref(),
                    )?;
                    Some(run.clone())
                }
            } else {
                None
            }
        };
        if let Some(run) = updated {
            self.sync_projection(&run, None, None).await;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn update_run_status_with_event_if_current(
        &self,
        user_id: &str,
        run_id: &str,
        expected_statuses: &[&str],
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
        event: serde_json::Value,
    ) -> Result<bool, String> {
        if expected_statuses.is_empty() {
            return Ok(false);
        }
        let terminal_error_code = terminal_error_code_from_transition(
            status,
            error_message,
            std::slice::from_ref(&event),
        );
        let updated = {
            let mut slots = self.execution_slots.write().await;
            let mut runs = self.runs.write().await;
            if let Some(run) = runs.get(run_id)
                && run.user_id == user_id
            {
                reconcile_in_memory_execution_slot_for_session(
                    &mut slots,
                    &runs,
                    user_id,
                    &run.session_id,
                    durable_run_status_blocks_session(status, waiting_for),
                )?;
            }
            if let Some(run) = runs.get_mut(run_id) {
                if run.user_id != user_id || !expected_statuses.contains(&run.status.as_str()) {
                    None
                } else {
                    let new_events =
                        new_idempotent_events(&run.events, std::slice::from_ref(&event));
                    if new_events.is_empty()
                        && !in_memory_transition_changes_state(
                            run,
                            status,
                            waiting_for,
                            error_message,
                        )
                    {
                        return Ok(false);
                    }
                    let latest_event_type = new_events.last().map(extract_event_type);
                    apply_in_memory_status_transition(
                        &mut slots,
                        run,
                        status,
                        waiting_for,
                        error_message,
                        terminal_error_code.as_deref(),
                    )?;
                    if !new_events.is_empty() {
                        run.events.extend(new_events);
                        run.last_event_idx = run.events.len() as i64 - 1;
                    }
                    Some((run.clone(), latest_event_type))
                }
            } else {
                None
            }
        };
        if let Some((run, latest_event_type)) = updated {
            self.sync_projection(&run, latest_event_type, None).await;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn update_run_status_with_event_if_current_unless_session_blocked(
        &self,
        request: GuardedRunStatusTransitionRequest<'_>,
    ) -> Result<GuardedRunStatusTransition, String> {
        let GuardedRunStatusTransitionRequest {
            user_id,
            run_id,
            session_id,
            expected_statuses,
            status,
            waiting_for,
            error_message,
            event,
        } = request;
        if expected_statuses.is_empty() {
            return Ok(GuardedRunStatusTransition::StatusConflict);
        }
        let latest_event_type = extract_event_type(&event);
        let terminal_error_code = terminal_error_code_from_transition(
            status,
            error_message,
            std::slice::from_ref(&event),
        );
        let updated = {
            let mut slots = self.execution_slots.write().await;
            let mut runs = self.runs.write().await;
            reconcile_in_memory_execution_slot_for_session(
                &mut slots,
                &runs,
                user_id,
                session_id,
                durable_run_status_blocks_session(status, waiting_for),
            )?;
            let slot_key = (user_id.to_string(), session_id.to_string());
            if slots
                .get(&slot_key)
                .is_some_and(|owner| owner.as_str() != run_id)
            {
                return Ok(GuardedRunStatusTransition::SessionBlocked);
            }
            if let Some(run) = runs.get_mut(run_id) {
                if run.user_id != user_id
                    || run.session_id != session_id
                    || !expected_statuses.contains(&run.status.as_str())
                {
                    None
                } else {
                    apply_in_memory_status_transition(
                        &mut slots,
                        run,
                        status,
                        waiting_for,
                        error_message,
                        terminal_error_code.as_deref(),
                    )?;
                    run.events.push(event);
                    run.last_event_idx = run.events.len() as i64 - 1;
                    Some(run.clone())
                }
            } else {
                None
            }
        };
        if let Some(run) = updated {
            self.sync_projection(&run, Some(latest_event_type), None)
                .await;
            Ok(GuardedRunStatusTransition::Updated)
        } else {
            Ok(GuardedRunStatusTransition::StatusConflict)
        }
    }

    async fn update_run_status_with_events_if_current(
        &self,
        user_id: &str,
        run_id: &str,
        expected_statuses: &[&str],
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
        events: &[serde_json::Value],
    ) -> Result<bool, String> {
        if expected_statuses.is_empty() {
            return Ok(false);
        }
        let terminal_error_code =
            terminal_error_code_from_transition(status, error_message, events);
        let updated = {
            let mut slots = self.execution_slots.write().await;
            let mut runs = self.runs.write().await;
            if let Some(run) = runs.get(run_id)
                && run.user_id == user_id
            {
                reconcile_in_memory_execution_slot_for_session(
                    &mut slots,
                    &runs,
                    user_id,
                    &run.session_id,
                    durable_run_status_blocks_session(status, waiting_for),
                )?;
            }
            if let Some(run) = runs.get_mut(run_id) {
                if run.user_id != user_id || !expected_statuses.contains(&run.status.as_str()) {
                    None
                } else {
                    let new_events = new_idempotent_events(&run.events, events);
                    if !events.is_empty()
                        && new_events.is_empty()
                        && !in_memory_transition_changes_state(
                            run,
                            status,
                            waiting_for,
                            error_message,
                        )
                    {
                        return Ok(false);
                    }
                    let latest_event_type = new_events.last().map(extract_event_type);
                    apply_in_memory_status_transition(
                        &mut slots,
                        run,
                        status,
                        waiting_for,
                        error_message,
                        terminal_error_code.as_deref(),
                    )?;
                    if !new_events.is_empty() {
                        run.events.extend(new_events);
                        run.last_event_idx = run.events.len() as i64 - 1;
                    }
                    Some((run.clone(), latest_event_type))
                }
            } else {
                None
            }
        };
        if let Some((run, latest_event_type)) = updated {
            self.sync_projection(&run, latest_event_type, None).await;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn update_run_usage(
        &self,
        user_id: &str,
        run_id: &str,
        prompt_tokens: u64,
        completion_tokens: u64,
        tool_calls: u32,
    ) -> Result<bool, String> {
        let updated = {
            let mut runs = self.runs.write().await;
            if let Some(run) = runs.get_mut(run_id) {
                if run.user_id != user_id {
                    None
                } else {
                    run.total_prompt_tokens = prompt_tokens;
                    run.total_completion_tokens = completion_tokens;
                    run.total_tool_calls = tool_calls;
                    run.updated_at = chrono::Utc::now().to_rfc3339();
                    Some(run.clone())
                }
            } else {
                None
            }
        };
        if let Some(run) = updated {
            self.sync_projection(&run, None, None).await;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn save_checkpoint(
        &self,
        user_id: &str,
        run_id: &str,
        checkpoint_json: &str,
    ) -> Result<bool, String> {
        let (run, checkpoint) = {
            let mut runs = self.runs.write().await;
            let Some(run) = runs.get_mut(run_id) else {
                return Ok(false);
            };
            if run.user_id != user_id {
                return Ok(false);
            }
            if durable_run_status_is_terminal(&run.status) {
                return Ok(false);
            }
            let (checkpoint_kind, checkpoint_version, idempotency_key) =
                checkpoint_metadata(run_id, checkpoint_json)?;
            run.checkpoint_json = Some(checkpoint_json.to_string());
            run.checkpoint_version = Some(checkpoint_version.clone());
            run.updated_at = chrono::Utc::now().to_rfc3339();
            let checkpoint = DurableRunCheckpointRecord {
                checkpoint_id: uuid::Uuid::now_v7().to_string(),
                run_id: run.run_id.clone(),
                user_id: run.user_id.clone(),
                session_id: run.session_id.clone(),
                node_seq: run.last_event_idx.max(0),
                checkpoint_kind,
                checkpoint_version,
                idempotency_key,
                checkpoint_json: checkpoint_json.to_string(),
                created_at: chrono::Utc::now().to_rfc3339(),
            };
            (run.clone(), checkpoint)
        };
        let mut checkpoints = self.checkpoints.write().await;
        let entries = checkpoints.entry(run_id.to_string()).or_default();
        if let Some(existing) = entries
            .iter_mut()
            .find(|entry| entry.idempotency_key == checkpoint.idempotency_key)
        {
            *existing = checkpoint.clone();
        } else {
            entries.push(checkpoint.clone());
        }
        drop(checkpoints);
        self.sync_projection(&run, None, Some(&checkpoint)).await;
        Ok(true)
    }

    async fn load_latest_checkpoint(
        &self,
        user_id: &str,
        run_id: &str,
        checkpoint_kind: Option<&str>,
    ) -> Result<Option<DurableRunCheckpointRecord>, String> {
        let runs = self.runs.read().await;
        let Some(run) = runs.get(run_id) else {
            return Ok(None);
        };
        if run.user_id != user_id {
            return Ok(None);
        }
        drop(runs);
        let checkpoints = self.checkpoints.read().await;
        let mut matches = checkpoints
            .get(run_id)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|checkpoint| {
                checkpoint_kind.is_none_or(|kind| checkpoint.checkpoint_kind == kind)
            })
            .collect::<Vec<_>>();
        matches.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| b.checkpoint_id.cmp(&a.checkpoint_id))
        });
        Ok(matches.into_iter().next())
    }

    async fn load_run_projection(
        &self,
        user_id: &str,
        run_id: &str,
    ) -> Result<Option<DurableRunDisplayProjectionRecord>, String> {
        let projections = self.projections.read().await;
        Ok(projections
            .get(run_id)
            .filter(|projection| projection.user_id == user_id)
            .cloned())
    }

    async fn rebuild_run_projection(
        &self,
        user_id: &str,
        run_id: &str,
    ) -> Result<Option<DurableRunDisplayProjectionRecord>, String> {
        let Some(run) = self.load_run(user_id, run_id).await? else {
            return Ok(None);
        };
        let latest_event_type = run.events.last().map(extract_event_type);
        let latest_checkpoint = self.load_latest_checkpoint(user_id, run_id, None).await?;
        let projection = build_run_display_projection(
            &run,
            latest_event_type,
            latest_checkpoint.as_ref().map(checkpoint_summary_tuple),
        );
        self.projections
            .write()
            .await
            .insert(run_id.to_string(), projection.clone());
        Ok(Some(projection))
    }

    async fn append_events_batch(
        &self,
        user_id: &str,
        run_id: &str,
        events: &[serde_json::Value],
    ) -> Result<(), String> {
        if events.is_empty() {
            return Ok(());
        }
        let updated = {
            let mut runs = self.runs.write().await;
            let Some(run) = runs.get_mut(run_id) else {
                return Err(format!("run not found while appending events: {run_id}"));
            };
            if run.user_id != user_id {
                return Err(format!("run not found while appending events: {run_id}"));
            }
            let events = new_idempotent_events(&run.events, events);
            if events.is_empty() {
                return Ok(());
            }
            let latest_event_type = extract_event_type(events.last().unwrap());
            let start_idx = run.events.len() as i64;
            let event_count = events.len() as i64;
            run.events.extend(events);
            run.last_event_idx = start_idx + event_count - 1;
            run.updated_at = chrono::Utc::now().to_rfc3339();
            Some((run.clone(), latest_event_type))
        };
        if let Some((run, latest_event_type)) = updated {
            self.sync_projection(&run, Some(latest_event_type), None)
                .await;
        }
        Ok(())
    }

    async fn resolve_run_interaction(
        &self,
        user_id: &str,
        run_id: &str,
        request_id: &str,
        kind: DurableRunInteractionKind,
        response_data: serde_json::Value,
    ) -> Result<DurableRunInteractionResolveOutcome, String> {
        let events = interaction_resolution_events(kind, request_id, response_data.clone());
        let updated = {
            let mut slots = self.execution_slots.write().await;
            let mut runs = self.runs.write().await;
            let Some(run) = runs.get_mut(run_id).filter(|run| run.user_id == user_id) else {
                return Ok(DurableRunInteractionResolveOutcome::MissingRequest);
            };
            let required = run.events.iter().any(|event| {
                extract_event_type(event) == kind.required_event_type()
                    && extract_interaction_request_id(event).as_deref() == Some(request_id)
            });
            if !required {
                return Ok(DurableRunInteractionResolveOutcome::MissingRequest);
            }
            if let Some(existing) = run.events.iter().rev().find(|event| {
                extract_event_type(event) == kind.resolved_event_type()
                    && extract_interaction_request_id(event).as_deref() == Some(request_id)
            }) {
                return Ok(if interaction_response_matches(existing, &response_data) {
                    DurableRunInteractionResolveOutcome::Idempotent(existing.clone())
                } else {
                    DurableRunInteractionResolveOutcome::Conflict(existing.clone())
                });
            }
            if run.status != STATUS_WAITING
                || run.waiting_for.as_deref() != Some(kind.waiting_for())
            {
                return Ok(DurableRunInteractionResolveOutcome::NoLongerWaiting);
            }
            apply_in_memory_status_transition(&mut slots, run, STATUS_RUNNING, None, None, None)?;
            let first_event_idx = run.last_event_idx + 1;
            run.events.extend(events.clone());
            run.last_event_idx = first_event_idx + events.len() as i64 - 1;
            Some(run.clone())
        };
        let Some(run) = updated else {
            return Ok(DurableRunInteractionResolveOutcome::NoLongerWaiting);
        };
        self.sync_projection(&run, Some("run_resumed".to_string()), None)
            .await;
        Ok(DurableRunInteractionResolveOutcome::Resolved(
            events[0].clone(),
        ))
    }

    async fn list_user_runs_cursor(
        &self,
        user_id: &str,
        limit: u32,
        cursor: Option<RunListCursor>,
    ) -> Result<DurableRunListPage, String> {
        if let Some(cursor) = &cursor {
            run_list_cursor_run_id(cursor)?;
        }
        let limit = validate_run_list_limit(limit);
        let runs = self.runs.read().await;
        let mut user_runs: Vec<_> = runs
            .values()
            .filter(|r| r.user_id == user_id)
            .cloned()
            .collect();
        user_runs.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| b.created_at.cmp(&a.created_at))
                .then_with(|| b.run_id.cmp(&a.run_id))
        });
        if let Some(cursor) = &cursor {
            user_runs.retain(|run| durable_run_after_cursor(run, cursor));
        }
        let has_more = user_runs.len() > limit as usize;
        if has_more {
            user_runs.truncate(limit as usize);
        }
        let next_cursor = if has_more {
            user_runs.last().map(durable_run_list_next_cursor)
        } else {
            None
        };
        Ok(DurableRunListPage {
            runs: user_runs,
            total: None,
            next_cursor,
        })
    }

    async fn list_session_runs(
        &self,
        user_id: &str,
        session_id: &str,
        limit: u32,
    ) -> Result<DurableSessionRunPage, String> {
        let limit = validate_run_list_limit(limit);
        let runs = self.runs.read().await;
        let all_session_runs = runs
            .values()
            .filter(|run| run.user_id == user_id && run.session_id == session_id)
            .cloned()
            .collect::<Vec<_>>();
        let candidates_by_id = all_session_runs
            .iter()
            .map(|run| (run.run_id.clone(), run.clone()))
            .collect::<HashMap<_, _>>();
        let mut session_runs = all_session_runs;
        sort_session_run_candidates(&mut session_runs);
        let truncated = session_runs.len() > limit as usize;
        session_runs.truncate(limit as usize);
        include_session_run_ancestors(&mut session_runs, &candidates_by_id);
        sort_session_run_tree(&mut session_runs);
        Ok(DurableSessionRunPage {
            runs: session_runs,
            limit,
            truncated,
        })
    }

    async fn list_active_session_runs_cursor(
        &self,
        user_id: &str,
        session_id: &str,
        limit: u32,
        cursor: Option<RunListCursor>,
    ) -> Result<DurableRunListPage, String> {
        if let Some(cursor) = &cursor {
            run_list_cursor_run_id(cursor)?;
        }
        let limit = validate_run_list_limit(limit);
        let runs = self.runs.read().await;
        let mut active_runs = runs
            .values()
            .filter(|run| {
                run.user_id == user_id
                    && run.session_id == session_id
                    && matches!(
                        run.status.as_str(),
                        STATUS_RUNNING | STATUS_WAITING | STATUS_PAUSED
                    )
            })
            .cloned()
            .collect::<Vec<_>>();
        active_runs.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| b.run_id.cmp(&a.run_id))
        });
        if let Some(cursor) = &cursor {
            active_runs.retain(|run| durable_run_after_cursor(run, cursor));
        }
        let has_more = active_runs.len() > limit as usize;
        if has_more {
            active_runs.truncate(limit as usize);
        }
        let next_cursor = has_more
            .then(|| active_runs.last().map(durable_run_list_next_cursor))
            .flatten();
        Ok(DurableRunListPage {
            runs: active_runs,
            total: None,
            next_cursor,
        })
    }

    async fn find_waiting_runs(&self) -> Result<Vec<DurableRunRecord>, String> {
        let runs = self.runs.read().await;
        Ok(runs
            .values()
            .filter(|r| durable_run_status_kind(&r.status) == DurableRunStatusKind::Waiting)
            .cloned()
            .collect())
    }

    async fn find_running_runs(&self) -> Result<Vec<DurableRunRecord>, String> {
        let runs = self.runs.read().await;
        Ok(runs
            .values()
            .filter(|r| durable_run_status_kind(&r.status) == DurableRunStatusKind::Running)
            .cloned()
            .collect())
    }

    async fn find_blocking_paused_runs(&self) -> Result<Vec<DurableRunRecord>, String> {
        let runs = self.runs.read().await;
        Ok(runs
            .values()
            .filter(|run| {
                durable_run_status_kind(&run.status) == DurableRunStatusKind::Paused
                    && run.waiting_for.is_some()
            })
            .cloned()
            .collect())
    }

    async fn find_blocking_session_run(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> Result<Option<DurableRunRecord>, String> {
        let runs = self.runs.read().await;
        let mut matches = runs
            .values()
            .filter(|run| {
                run.user_id == user_id
                    && run.session_id == session_id
                    && durable_run_status_blocks_session(&run.status, run.waiting_for.as_deref())
            })
            .cloned()
            .collect::<Vec<_>>();
        matches.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(matches.into_iter().next())
    }

    async fn find_sub_runs(
        &self,
        user_id: &str,
        delegation_id: &str,
    ) -> Result<Vec<DurableRunRecord>, String> {
        let runs = self.runs.read().await;
        Ok(runs
            .values()
            .filter(|r| r.user_id == user_id && r.delegation_id.as_deref() == Some(delegation_id))
            .cloned()
            .collect())
    }

    async fn update_retry_count(
        &self,
        user_id: &str,
        run_id: &str,
        retry_count: u32,
    ) -> Result<bool, String> {
        let mut runs = self.runs.write().await;
        if let Some(run) = runs.get_mut(run_id) {
            if run.user_id != user_id {
                return Ok(false);
            }
            run.retry_count = retry_count;
            run.updated_at = chrono::Utc::now().to_rfc3339();
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

// ─── MatrixOne-backed run state store ───────────────────────────────────────

const DEFAULT_RETRY_SCOPE: &str = "node";
const MAX_TOOL_OUTPUT_BATCH_ROWS: usize = 500;
const MAX_TOOL_OUTPUT_BATCH_BYTES: usize = 16 * 1024 * 1024;
const FALLBACK_PREVIEW_BYTES: usize = 400;

#[derive(Clone, Debug)]
struct ToolPreviewContract {
    max_preview_bytes: usize,
    normalize_version: String,
    found: bool,
}

#[derive(Clone, Debug)]
struct ToolOutputPreviewRow {
    payload: String,
    preview_text: String,
    preview_status: String,
    artifact_ref: Option<String>,
    content_hash: String,
    normalize_version: String,
    parent_output_id: Option<String>,
}

#[derive(Debug, Error)]
pub enum DatabaseRunStateStoreError {
    #[error("database operation failed: operation={operation}, entity={entity}, source={source}")]
    Database {
        operation: &'static str,
        entity: String,
        #[source]
        source: sqlx::Error,
    },
    #[error("invalid retry_scope for run {run_id}: {retry_scope}")]
    InvalidRetryScope { run_id: String, retry_scope: String },
    #[error("tool output batch too large: run_id={run_id}, rows={rows}, bytes={bytes}")]
    ToolOutputBatchTooLarge {
        run_id: String,
        rows: usize,
        bytes: usize,
    },
    #[error("JSON serialization failed: operation={operation}, entity={entity}, source={source}")]
    Json {
        operation: &'static str,
        entity: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("invalid durable tool output: output_id={output_id}, source={source}")]
    InvalidToolOutput {
        output_id: String,
        #[source]
        source: ToolInvocationContractError,
    },
}

type DbStoreResult<T> = Result<T, DatabaseRunStateStoreError>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutputBatchItem {
    pub output_id: String,
    pub tool_call_id: Option<String>,
    pub tool_name: String,
    pub result: ToolInvocationResultPayload,
}

/// MatrixOne durable run store.
///
/// Events are append-only in `agent_run_events`; `agent_runs` owns event_idx counter via CAS
/// allocation so reconnect and replay never scan `MAX(event_idx)`.
#[derive(Clone)]
pub struct DatabaseRunStateStore {
    pool: SharedPool,
    owner_pod_id: String,
    lease_ttl: Duration,
    session_execution_slot_stale_after: Duration,
}

impl DatabaseRunStateStore {
    pub const DEFAULT_LEASE_TTL: Duration = Duration::from_secs(45);
    pub const DEFAULT_SESSION_EXECUTION_SLOT_STALE_AFTER: Duration = Duration::from_secs(120);

    pub fn new(pool: SharedPool) -> Self {
        Self {
            pool,
            owner_pod_id: default_owner_pod_id(),
            lease_ttl: Self::DEFAULT_LEASE_TTL,
            session_execution_slot_stale_after: Self::DEFAULT_SESSION_EXECUTION_SLOT_STALE_AFTER,
        }
    }

    pub fn with_owner_pod_id(mut self, owner_pod_id: impl Into<String>) -> Self {
        self.owner_pod_id = owner_pod_id.into();
        self
    }

    pub fn with_lease_ttl(mut self, lease_ttl: Duration) -> Self {
        self.lease_ttl = lease_ttl;
        self
    }

    pub fn with_session_execution_slot_stale_after(mut self, stale_after: Duration) -> Self {
        self.session_execution_slot_stale_after = stale_after;
        self
    }

    pub fn owner_pod_id(&self) -> &str {
        &self.owner_pod_id
    }

    fn lease_expires_at(&self) -> chrono::NaiveDateTime {
        let lease_expires_at = chrono::Utc::now()
            + chrono::Duration::from_std(self.lease_ttl)
                .unwrap_or_else(|_| chrono::Duration::seconds(45));
        lease_expires_at.naive_utc()
    }

    async fn acquire_session_execution_slot_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
        user_id: &str,
        session_id: &str,
        run_id: &str,
    ) -> DbStoreResult<bool> {
        let insert = sqlx::query(
            "INSERT IGNORE INTO agent_session_execution_slots
             (user_id, session_id, run_id, acquired_at, updated_at)
             VALUES (?, ?, ?, NOW(6), NOW(6))",
        )
        .bind(user_id)
        .bind(session_id)
        .bind(run_id)
        .execute(&mut **tx)
        .await
        .map_err(|source| db_error("acquire_session_execution_slot", session_id, source))?;
        if insert.rows_affected() > 0 {
            return Ok(true);
        }

        let slot = sqlx::query(
            "SELECT run_id, updated_at FROM agent_session_execution_slots
             WHERE user_id = ? AND session_id = ?",
        )
        .bind(user_id)
        .bind(session_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(|source| db_error("load_session_execution_slot", session_id, source))?
        .map(|row| {
            Ok::<_, sqlx::Error>((
                row.try_get::<String, _>("run_id")?,
                row.try_get::<chrono::NaiveDateTime, _>("updated_at")?,
            ))
        })
        .transpose()
        .map_err(|source| db_error("decode_session_execution_slot", session_id, source))?;

        let Some((owner, slot_updated_at)) = slot else {
            let retry = sqlx::query(
                "INSERT IGNORE INTO agent_session_execution_slots
                 (user_id, session_id, run_id, acquired_at, updated_at)
                 VALUES (?, ?, ?, NOW(6), NOW(6))",
            )
            .bind(user_id)
            .bind(session_id)
            .bind(run_id)
            .execute(&mut **tx)
            .await
            .map_err(|source| {
                db_error(
                    "retry_acquire_missing_session_execution_slot",
                    session_id,
                    source,
                )
            })?;
            return Ok(retry.rows_affected() > 0);
        };

        if owner != run_id {
            let slot_age = chrono::Utc::now()
                .naive_utc()
                .signed_duration_since(slot_updated_at)
                .to_std()
                .unwrap_or_default();
            let slot_is_stale = slot_age >= self.session_execution_slot_stale_after;
            {
                let owner_state = sqlx::query(
                    "SELECT status, waiting_for, owner_lease_expires_at FROM agent_runs
                     WHERE user_id = ? AND run_id = ?",
                )
                .bind(user_id)
                .bind(&owner)
                .fetch_optional(&mut **tx)
                .await
                .map_err(|source| {
                    db_error("load_session_execution_slot_owner", session_id, source)
                })?;
                let owner_reclaimable = owner_state
                    .as_ref()
                    .map(|row| {
                        let status = row.try_get::<String, _>("status")?;
                        let waiting_for = row.try_get::<Option<String>, _>("waiting_for")?;
                        let owner_lease_expires_at = row
                            .try_get::<Option<chrono::NaiveDateTime>, _>(
                                "owner_lease_expires_at",
                            )?;
                        // A missing lease is stronger evidence of no live
                        // executor than an expired one. The runtime clears
                        // ownership on graceful task exit; after the slot's
                        // stale window that abandoned status must be
                        // reclaimable even if terminal persistence failed.
                        let owner_lease_expired = owner_lease_expires_at
                            .is_none_or(|expires_at| expires_at < chrono::Utc::now().naive_utc());
                        Ok::<_, sqlx::Error>(session_execution_slot_owner_reclaimable(
                            &status,
                            waiting_for.as_deref(),
                            owner_lease_expired,
                            slot_is_stale,
                        ))
                    })
                    .transpose()
                    .map_err(|source| {
                        db_error("decode_session_execution_slot_owner", session_id, source)
                    })?
                    .unwrap_or(true);
                if owner_reclaimable {
                    sqlx::query(
                        "DELETE FROM agent_session_execution_slots
                         WHERE user_id = ? AND session_id = ? AND run_id = ?",
                    )
                    .bind(user_id)
                    .bind(session_id)
                    .bind(&owner)
                    .execute(&mut **tx)
                    .await
                    .map_err(|source| {
                        db_error("cleanup_stale_session_execution_slot", session_id, source)
                    })?;
                    let retry = sqlx::query(
                        "INSERT IGNORE INTO agent_session_execution_slots
                         (user_id, session_id, run_id, acquired_at, updated_at)
                         VALUES (?, ?, ?, NOW(6), NOW(6))",
                    )
                    .bind(user_id)
                    .bind(session_id)
                    .bind(run_id)
                    .execute(&mut **tx)
                    .await
                    .map_err(|source| {
                        db_error("retry_acquire_session_execution_slot", session_id, source)
                    })?;
                    if retry.rows_affected() > 0 {
                        return Ok(true);
                    }
                }
            }
            return Ok(false);
        }

        sqlx::query(
            "UPDATE agent_session_execution_slots
             SET updated_at = NOW(6)
             WHERE user_id = ? AND session_id = ? AND run_id = ?",
        )
        .bind(user_id)
        .bind(session_id)
        .bind(run_id)
        .execute(&mut **tx)
        .await
        .map_err(|source| db_error("refresh_session_execution_slot", session_id, source))?;
        Ok(true)
    }

    async fn release_session_execution_slot_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
        user_id: &str,
        session_id: &str,
        run_id: &str,
    ) -> DbStoreResult<()> {
        sqlx::query(
            "DELETE FROM agent_session_execution_slots
             WHERE user_id = ? AND session_id = ? AND run_id = ?",
        )
        .bind(user_id)
        .bind(session_id)
        .bind(run_id)
        .execute(&mut **tx)
        .await
        .map_err(|source| db_error("release_session_execution_slot", session_id, source))?;
        Ok(())
    }

    async fn sync_session_execution_slot_after_status_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
        run: &DurableRunRecord,
        status: &str,
        waiting_for: Option<&str>,
    ) -> DbStoreResult<bool> {
        if !run_requires_session_execution_slot(run) {
            return Ok(true);
        }
        if durable_run_status_blocks_session(status, waiting_for) {
            debug_assert!(
                durable_run_status_kind(status) != DurableRunStatusKind::Paused
                    || waiting_for.is_some(),
                "paused without waiting_for must release the session execution slot"
            );
            self.acquire_session_execution_slot_tx(tx, &run.user_id, &run.session_id, &run.run_id)
                .await
        } else {
            Self::release_session_execution_slot_tx(tx, &run.user_id, &run.session_id, &run.run_id)
                .await?;
            Ok(true)
        }
    }

    async fn load_tool_preview_contracts(
        &self,
        items: &[ToolOutputBatchItem],
    ) -> DbStoreResult<HashMap<String, ToolPreviewContract>> {
        let tool_names = items
            .iter()
            .map(|item| item.tool_name.clone())
            .collect::<HashSet<_>>();
        let mut contracts = tool_names
            .iter()
            .map(|tool_name| {
                (
                    tool_name.clone(),
                    ToolPreviewContract {
                        max_preview_bytes: FALLBACK_PREVIEW_BYTES,
                        normalize_version: "raw_v1".to_string(),
                        found: false,
                    },
                )
            })
            .collect::<HashMap<_, _>>();
        if tool_names.is_empty() {
            return Ok(contracts);
        }

        let mut builder = sqlx::QueryBuilder::<sqlx::MySql>::new(
            "SELECT tool_name, max_preview_bytes, normalize_version
             FROM preview_template_registry
             WHERE status = 'active' AND tool_name IN (",
        );
        let mut separated = builder.separated(", ");
        for tool_name in &tool_names {
            separated.push_bind(tool_name);
        }
        separated.push_unseparated(")");
        let rows = builder
            .build()
            .fetch_all(self.pool.get())
            .await
            .map_err(|source| {
                db_error("load_tool_preview_contracts", "preview_templates", source)
            })?;
        for row in rows {
            let (tool_name, contract) = decode_tool_preview_contract_row(&row)?;
            contracts.insert(tool_name, contract);
        }
        Ok(contracts)
    }

    async fn record_preview_template_missing_for_tools(
        &self,
        session_id: &str,
        run_id: &str,
        user_id: &str,
        contracts: &HashMap<String, ToolPreviewContract>,
    ) -> DbStoreResult<()> {
        let missing = contracts
            .iter()
            .filter_map(|(tool_name, contract)| (!contract.found).then_some(tool_name.clone()))
            .collect::<Vec<_>>();
        if missing.is_empty() {
            return Ok(());
        }
        let missing_events = missing
            .into_iter()
            .map(|tool_name| (tool_name, Uuid::new_v4().to_string()))
            .collect::<Vec<_>>();
        let last_event_id = missing_events.last().map(|(_, event_id)| event_id.as_str());
        let mut tx = self.pool.get().begin().await.map_err(|source| {
            db_error(
                "begin_record_preview_template_missing_for_tools",
                run_id,
                source,
            )
        })?;
        let mut builder = sqlx::QueryBuilder::<sqlx::MySql>::new(
            "INSERT INTO agent_events
             (event_id, session_id, user_id, event_type, content, metadata, meta_tool_name, created_at) ",
        );
        builder.push_values(missing_events.iter(), |mut row, (tool_name, event_id)| {
            row.push_bind(event_id)
                .push_bind(session_id)
                .push_bind(user_id)
                .push_bind("preview_template_missing")
                .push_bind(tool_name)
                .push_bind(
                    serde_json::json!({
                        "run_id": run_id,
                        "tool_name": tool_name,
                        "fallback_max_preview_bytes": FALLBACK_PREVIEW_BYTES,
                    })
                    .to_string(),
                )
                .push_bind(tool_name)
                .push("NOW(6)");
        });
        let insert_result = builder.build().execute(&mut *tx).await.map_err(|source| {
            db_error("record_preview_template_missing_for_tools", run_id, source)
        })?;
        let inserted_events = crate::storage::rows_affected_to_i64(
            insert_result.rows_affected(),
            "record_preview_template_missing_for_tools",
        )
        .map_err(|source| db_error("record_preview_template_missing_for_tools", run_id, source))?;
        if inserted_events > 0 {
            crate::storage::add_agent_session_event_count_or_create(
                &mut *tx,
                session_id,
                user_id,
                inserted_events,
                last_event_id,
            )
            .await
            .map_err(|source| {
                db_error(
                    "record_preview_template_missing_event_count_delta",
                    run_id,
                    source,
                )
            })?;
        }
        tx.commit()
            .await
            .map_err(|source| db_error("commit_preview_template_missing_events", run_id, source))?;
        Ok(())
    }

    pub async fn acquire_owner_lease(
        &self,
        user_id: &str,
        run_id: &str,
        owner_pod_id: &str,
        ttl: Duration,
    ) -> DbStoreResult<bool> {
        let lease_expires_at = chrono::Utc::now()
            + chrono::Duration::from_std(ttl).unwrap_or_else(|_| chrono::Duration::seconds(45));
        let result = sqlx::query(
            "UPDATE agent_runs
             SET owner_pod_id = ?,
                 owner_lease_expires_at = ?,
                 run_generation = run_generation + 1,
                 updated_at = NOW(6)
             WHERE user_id = ?
               AND run_id = ?
               AND (owner_pod_id IS NULL OR owner_pod_id = ? OR owner_lease_expires_at < NOW(6))",
        )
        .bind(owner_pod_id)
        .bind(lease_expires_at.naive_utc())
        .bind(user_id)
        .bind(run_id)
        .bind(owner_pod_id)
        .execute(self.pool.get())
        .await
        .map_err(|source| db_error("acquire_owner_lease", run_id, source))?;

        Ok(result.rows_affected() > 0)
    }

    pub async fn insert_tool_output_batch(
        &self,
        batch_id: &str,
        session_id: &str,
        run_id: &str,
        user_id: &str,
        items: &[ToolOutputBatchItem],
    ) -> DbStoreResult<()> {
        let payloads = serialize_tool_output_payloads(items)?;
        let payload_bytes = payloads.iter().map(String::len).sum::<usize>();
        if items.len() > MAX_TOOL_OUTPUT_BATCH_ROWS || payload_bytes > MAX_TOOL_OUTPUT_BATCH_BYTES {
            return Err(DatabaseRunStateStoreError::ToolOutputBatchTooLarge {
                run_id: run_id.to_string(),
                rows: items.len(),
                bytes: payload_bytes,
            });
        }
        let preview_contracts = self.load_tool_preview_contracts(items).await?;
        self.record_preview_template_missing_for_tools(
            session_id,
            run_id,
            user_id,
            &preview_contracts,
        )
        .await?;
        let preview_rows = items
            .iter()
            .zip(payloads.iter())
            .map(|(item, payload)| {
                let contract = preview_contracts.get(&item.tool_name).cloned().unwrap_or(
                    ToolPreviewContract {
                        max_preview_bytes: FALLBACK_PREVIEW_BYTES,
                        normalize_version: "raw_v1".to_string(),
                        found: false,
                    },
                );
                build_tool_output_preview_row(session_id, item, payload, &contract)
            })
            .collect::<Vec<_>>();

        let mut tx = self
            .pool
            .get()
            .begin()
            .await
            .map_err(|source| db_error("begin_tool_output_batch", run_id, source))?;

        sqlx::query(
            "INSERT INTO session_tool_output_batches
             (batch_id, session_id, run_id, user_id, output_count, payload_bytes, status, created_at)
             VALUES (?, ?, ?, ?, ?, ?, 'committed', NOW(6))
             ON DUPLICATE KEY UPDATE output_count = output_count",
        )
        .bind(batch_id)
        .bind(session_id)
        .bind(run_id)
        .bind(user_id)
        .bind(items.len() as i64)
        .bind(payload_bytes as i64)
        .execute(&mut *tx)
        .await
        .map_err(|source| db_error("insert_tool_output_batch", batch_id, source))?;

        if !items.is_empty() {
            let mut builder = sqlx::QueryBuilder::<sqlx::MySql>::new(
                "INSERT INTO session_tool_outputs
                 (output_id, batch_id, session_id, run_id, user_id, output_idx,
                  parent_output_id, tool_call_id, tool_name, output_json, payload_bytes,
                  preview_text, preview_status, artifact_ref, content_hash, normalize_version,
                  created_at) ",
            );
            builder.push_values(
                items.iter().zip(preview_rows.iter()).enumerate(),
                |mut row, (idx, (item, preview))| {
                    row.push_bind(&item.output_id)
                        .push_bind(batch_id)
                        .push_bind(session_id)
                        .push_bind(run_id)
                        .push_bind(user_id)
                        .push_bind(idx as i64)
                        .push_bind(&preview.parent_output_id)
                        .push_bind(&item.tool_call_id)
                        .push_bind(&item.tool_name)
                        .push_bind(&preview.payload)
                        .push_bind(preview.payload.len() as i64)
                        .push_bind(&preview.preview_text)
                        .push_bind(&preview.preview_status)
                        .push_bind(&preview.artifact_ref)
                        .push_bind(&preview.content_hash)
                        .push_bind(&preview.normalize_version)
                        .push("NOW(6)");
                },
            );
            builder.push(" ON DUPLICATE KEY UPDATE payload_bytes = payload_bytes");
            builder
                .build()
                .execute(&mut *tx)
                .await
                .map_err(|source| db_error("insert_tool_outputs_batch_rows", batch_id, source))?;
        }

        tx.commit()
            .await
            .map_err(|source| db_error("commit_tool_output_batch", batch_id, source))?;
        Ok(())
    }

    async fn load_run_metadata_for_user(
        &self,
        user_id: &str,
        run_id: &str,
    ) -> DbStoreResult<Option<DurableRunRecord>> {
        let sql =
            format!("SELECT {AGENT_RUN_COLUMNS} FROM agent_runs WHERE user_id = ? AND run_id = ?");
        let row = sqlx::query(&sql)
            .bind(user_id)
            .bind(run_id)
            .fetch_optional(self.pool.get())
            .await
            .map_err(|source| db_error("load_run_metadata_for_user", run_id, source))?;
        row.map(run_record_from_row).transpose()
    }

    /// Transaction-scoped variant of [`load_run_metadata_for_user`].
    ///
    /// Loads run metadata inside an already-open transaction so the slot
    /// ownership check in [`sync_session_execution_slot_after_status_tx`]
    /// sees the same `agent_id` row version that the status UPDATE acts on.
    /// This closes the TOCTOU window where a concurrent agent_id flip could
    /// cause the slot logic to act on stale ownership.
    async fn load_run_metadata_for_user_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
        user_id: &str,
        run_id: &str,
    ) -> DbStoreResult<Option<DurableRunRecord>> {
        let sql = format!(
            "SELECT {AGENT_RUN_COLUMNS} FROM agent_runs WHERE user_id = ? AND run_id = ? FOR UPDATE"
        );
        let row = sqlx::query(&sql)
            .bind(user_id)
            .bind(run_id)
            .fetch_optional(&mut **tx)
            .await
            .map_err(|source| db_error("load_run_metadata_for_user_tx", run_id, source))?;
        row.map(run_record_from_row).transpose()
    }

    async fn load_run_projection_metadata_for_user(
        &self,
        user_id: &str,
        run_id: &str,
    ) -> DbStoreResult<Option<DurableRunDisplayProjectionRecord>> {
        let sql = format!(
            "SELECT {RUN_DISPLAY_PROJECTION_COLUMNS} FROM run_display_projections WHERE user_id = ? AND run_id = ?"
        );
        let row = sqlx::query(&sql)
            .bind(user_id)
            .bind(run_id)
            .fetch_optional(self.pool.get())
            .await
            .map_err(|source| db_error("load_run_projection_for_user", run_id, source))?;
        row.map(run_projection_record_from_row).transpose()
    }

    async fn load_latest_event_type_for_user(
        &self,
        user_id: &str,
        run_id: &str,
    ) -> DbStoreResult<Option<String>> {
        let row = sqlx::query(
            "SELECT event_type
             FROM agent_run_events
             WHERE user_id = ? AND run_id = ?
             ORDER BY event_idx DESC
             LIMIT 1",
        )
        .bind(user_id)
        .bind(run_id)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| db_error("load_latest_event_type_for_user", run_id, source))?;
        row.map(|row| {
            row.try_get::<String, _>("event_type")
                .map_err(|source| db_error("decode_latest_event_type", run_id, source))
        })
        .transpose()
    }

    async fn upsert_run_projection(
        &self,
        projection: &DurableRunDisplayProjectionRecord,
    ) -> DbStoreResult<()> {
        sqlx::query(
            "INSERT INTO run_display_projections
             (run_id, user_id, session_id, status, waiting_for, error_message,
              projection_event_idx, latest_event_type, latest_checkpoint_id,
              latest_checkpoint_kind, latest_checkpoint_version, total_prompt_tokens,
              total_completion_tokens, total_tool_calls, projection_hash, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW(6))
             ON DUPLICATE KEY UPDATE
               status = VALUES(status),
               waiting_for = VALUES(waiting_for),
               error_message = VALUES(error_message),
               projection_event_idx = VALUES(projection_event_idx),
               latest_event_type = VALUES(latest_event_type),
               latest_checkpoint_id = VALUES(latest_checkpoint_id),
               latest_checkpoint_kind = VALUES(latest_checkpoint_kind),
               latest_checkpoint_version = VALUES(latest_checkpoint_version),
               total_prompt_tokens = VALUES(total_prompt_tokens),
               total_completion_tokens = VALUES(total_completion_tokens),
               total_tool_calls = VALUES(total_tool_calls),
               projection_hash = VALUES(projection_hash),
               updated_at = NOW(6)",
        )
        .bind(&projection.run_id)
        .bind(&projection.user_id)
        .bind(&projection.session_id)
        .bind(&projection.status)
        .bind(&projection.waiting_for)
        .bind(&projection.error_message)
        .bind(projection.projection_event_idx)
        .bind(&projection.latest_event_type)
        .bind(&projection.latest_checkpoint_id)
        .bind(&projection.latest_checkpoint_kind)
        .bind(&projection.latest_checkpoint_version)
        .bind(projection.total_prompt_tokens as i64)
        .bind(projection.total_completion_tokens as i64)
        .bind(projection.total_tool_calls as i64)
        .bind(&projection.projection_hash)
        .execute(self.pool.get())
        .await
        .map_err(|source| db_error("upsert_run_projection", &projection.run_id, source))?;
        Ok(())
    }

    async fn patch_run_projection_usage_for_user(
        &self,
        user_id: &str,
        run_id: &str,
        prompt_tokens: u64,
        completion_tokens: u64,
        tool_calls: u32,
    ) -> DbStoreResult<u64> {
        let projection_hash =
            usage_projection_patch_hash(run_id, prompt_tokens, completion_tokens, tool_calls);
        let result = sqlx::query(
            "UPDATE run_display_projections
             SET total_prompt_tokens = ?,
                 total_completion_tokens = ?,
                 total_tool_calls = ?,
                 projection_hash = ?,
                 updated_at = NOW(6)
             WHERE user_id = ? AND run_id = ?",
        )
        .bind(prompt_tokens as i64)
        .bind(completion_tokens as i64)
        .bind(tool_calls as i64)
        .bind(&projection_hash)
        .bind(user_id)
        .bind(run_id)
        .execute(self.pool.get())
        .await
        .map_err(|source| db_error("patch_run_projection_usage", run_id, source))?;
        Ok(result.rows_affected())
    }

    #[allow(clippy::too_many_arguments)]
    async fn patch_run_projection_status_for_user(
        &self,
        user_id: &str,
        run_id: &str,
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
        projection_event_idx: i64,
        latest_event_type: Option<&str>,
    ) -> DbStoreResult<u64> {
        let projection_hash = status_projection_patch_hash(
            run_id,
            status,
            waiting_for,
            error_message,
            projection_event_idx,
            latest_event_type,
        );
        let result = sqlx::query(
            "UPDATE run_display_projections
             SET status = ?,
                 waiting_for = ?,
                 error_message = ?,
                 projection_event_idx = ?,
                 latest_event_type = COALESCE(?, latest_event_type),
                 projection_hash = ?,
                 updated_at = NOW(6)
             WHERE user_id = ? AND run_id = ? AND projection_event_idx <= ?",
        )
        .bind(status)
        .bind(waiting_for)
        .bind(error_message)
        .bind(projection_event_idx)
        .bind(latest_event_type)
        .bind(&projection_hash)
        .bind(user_id)
        .bind(run_id)
        .bind(projection_event_idx)
        .execute(self.pool.get())
        .await
        .map_err(|source| db_error("patch_run_projection_status", run_id, source))?;
        Ok(result.rows_affected())
    }

    #[allow(clippy::too_many_arguments)]
    async fn patch_or_repair_run_projection_status_for_user(
        &self,
        user_id: &str,
        run_id: &str,
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
        projection_event_idx: i64,
        latest_event_type: Option<&str>,
    ) {
        match self
            .patch_run_projection_status_for_user(
                user_id,
                run_id,
                status,
                waiting_for,
                error_message,
                projection_event_idx,
                latest_event_type,
            )
            .await
        {
            Ok(0) => {
                match self
                    .load_run_projection_metadata_for_user(user_id, run_id)
                    .await
                {
                    Ok(None) => {
                        if let Err(error) = self
                            .sync_projection_for_user(user_id, run_id, None, None)
                            .await
                        {
                            tracing::warn!(
                                user_id,
                                run_id,
                                error = %error,
                                "run transition committed but missing display projection repair failed"
                            );
                        }
                    }
                    Ok(Some(existing)) if existing.projection_event_idx > projection_event_idx => {
                        tracing::debug!(
                            user_id,
                            run_id,
                            attempted_projection_event_idx = projection_event_idx,
                            current_projection_event_idx = existing.projection_event_idx,
                            "ignored stale run display projection status patch"
                        );
                    }
                    Ok(Some(_)) => {}
                    Err(error) => {
                        tracing::warn!(
                            user_id,
                            run_id,
                            error = %error,
                            "run transition committed but display projection state check failed"
                        );
                    }
                }
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(
                    user_id,
                    run_id,
                    error = %error,
                    "run transition committed but display projection status patch failed"
                );
            }
        }
    }

    async fn sync_projection_for_user(
        &self,
        user_id: &str,
        run_id: &str,
        latest_event_type: Option<&str>,
        latest_checkpoint: Option<&DurableRunCheckpointRecord>,
    ) -> DbStoreResult<()> {
        let Some(run) = self.load_run_metadata_for_user(user_id, run_id).await? else {
            return Ok(());
        };
        let existing = self
            .load_run_projection_metadata_for_user(user_id, run_id)
            .await?;
        let latest_event_type = if let Some(latest_event_type) = latest_event_type {
            Some(latest_event_type.to_owned())
        } else {
            let existing_event_type = existing
                .as_ref()
                .and_then(|entry| entry.latest_event_type.clone());
            if existing
                .as_ref()
                .is_some_and(|entry| entry.projection_event_idx >= run.last_event_idx)
            {
                existing_event_type
            } else {
                self.load_latest_event_type_for_user(user_id, run_id)
                    .await?
                    .or(existing_event_type)
            }
        };
        let projection = build_run_display_projection(
            &run,
            latest_event_type,
            latest_checkpoint.map(checkpoint_summary_tuple).or_else(|| {
                existing.as_ref().and_then(|entry| {
                    Some((
                        entry.latest_checkpoint_id.clone()?,
                        entry.latest_checkpoint_kind.clone()?,
                        entry.latest_checkpoint_version.clone()?,
                    ))
                })
            }),
        );
        self.upsert_run_projection(&projection).await
    }

    /// Allocate a contiguous block of `count` event indices in one CAS operation.
    /// Returns the starting index. The caller owns [start, start+count).
    async fn allocate_event_indices_batch_for_user(
        &self,
        user_id: &str,
        run_id: &str,
        count: i64,
    ) -> DbStoreResult<i64> {
        if count <= 0 {
            return Ok(0);
        }
        for attempt in 0u32..64 {
            let row = sqlx::query(
                "SELECT last_event_idx FROM agent_runs WHERE user_id = ? AND run_id = ?",
            )
            .bind(user_id)
            .bind(run_id)
            .fetch_one(self.pool.get())
            .await
            .map_err(|source| db_error("select_last_event_idx_for_user", run_id, source))?;
            let current: i64 = row.get(0);
            let result = sqlx::query(
                "UPDATE agent_runs
                 SET last_event_idx = last_event_idx + ?
                 WHERE user_id = ? AND run_id = ? AND last_event_idx = ?",
            )
            .bind(count)
            .bind(user_id)
            .bind(run_id)
            .bind(current)
            .execute(self.pool.get())
            .await
            .map_err(|source| db_error("cas_increment_last_event_idx_for_user", run_id, source))?;
            if result.rows_affected() == 1 {
                return Ok(current + 1);
            }
            // Exponential backoff with jitter: 1ms → 2ms → 4ms → … capping at 128ms.
            // `fastrand` jitter prevents thundering herd across pods.
            let base_ms = 1u64 << attempt.min(7);
            let jitter_ms = fastrand::u64(0..base_ms.min(64));
            tokio::time::sleep(std::time::Duration::from_millis(base_ms + jitter_ms)).await;
        }
        Err(db_error(
            "allocate_event_indices_batch_for_user",
            run_id,
            sqlx::Error::Protocol("run counter CAS exhausted".to_string()),
        ))
    }

    /// Append multiple events in a single batch, minimizing DB round-trips.
    /// Loads run metadata once, allocates all indices in one CAS, does one
    /// bulk INSERT, one last_event_idx UPDATE, and one projection sync.
    async fn append_events_batch_for_user(
        &self,
        user_id: &str,
        run_id: &str,
        events: &[serde_json::Value],
    ) -> DbStoreResult<()> {
        if events.is_empty() {
            return Ok(());
        }

        let run = self
            .load_run_metadata_for_user(user_id, run_id)
            .await?
            .ok_or_else(|| {
                db_error(
                    "load_run_for_batch_append",
                    run_id,
                    sqlx::Error::RowNotFound,
                )
            })?;

        // ── Idempotency dedup ──
        // Pre-filter events whose idempotency_key already exists (optimization).
        // INSERT IGNORE acts as the safety net: if another pod concurrently writes
        // the same key between the SELECT and INSERT, the duplicate row is
        // silently skipped instead of failing the entire batch.
        let idem_keys: Vec<String> = events
            .iter()
            .filter_map(|e| extract_optional_string(e, "idempotency_key"))
            .collect();

        let existing: std::collections::HashSet<String> = if !idem_keys.is_empty() {
            let mut builder = sqlx::QueryBuilder::<sqlx::MySql>::new(
                "SELECT idempotency_key FROM agent_run_events WHERE user_id = ",
            );
            builder.push_bind(user_id);
            builder.push(" AND run_id = ");
            builder.push_bind(run_id);
            builder.push(" AND idempotency_key IN (");
            let mut separated = builder.separated(", ");
            for k in &idem_keys {
                separated.push_bind(k);
            }
            separated.push_unseparated(")");
            let rows = builder
                .build()
                .fetch_all(self.pool.get())
                .await
                .map_err(|source| db_error("lookup_batch_idempotency", run_id, source))?;
            let mut existing = std::collections::HashSet::with_capacity(rows.len());
            for row in rows {
                let key: String = row
                    .try_get("idempotency_key")
                    .map_err(|source| db_error("decode_batch_idempotency_key", run_id, source))?;
                existing.insert(key);
            }
            existing
        } else {
            std::collections::HashSet::new()
        };

        let events: Vec<&serde_json::Value> = events
            .iter()
            .filter(|e| {
                extract_optional_string(e, "idempotency_key")
                    .map(|k| !existing.contains(k.as_str()))
                    .unwrap_or(true)
            })
            .collect();

        if events.is_empty() {
            return Ok(());
        }

        // Allocate all indices in one CAS.
        let start_idx = self
            .allocate_event_indices_batch_for_user(user_id, run_id, events.len() as i64)
            .await?;

        let mut rows: Vec<RunEventInsertRow> = Vec::with_capacity(events.len());

        for (i, event) in events.iter().enumerate() {
            rows.push(build_run_event_insert_row(
                &run.user_id,
                run_id,
                &run.session_id,
                run.agent_id.as_deref(),
                start_idx + i as i64,
                &self.owner_pod_id,
                event,
            )?);
        }

        let mut builder = sqlx::QueryBuilder::<sqlx::MySql>::new(
            "INSERT IGNORE INTO agent_run_events \
             (id, run_id, event_idx, user_id, session_id, event_type, event_id, agent_id, \
              subject_run_id, interaction_request_id, idempotency_key, event_hash, producer_pod_id, payload_json, created_at) ",
        );
        builder.push_values(rows.iter(), |mut row, event| {
            row.push_bind(&event.id)
                .push_bind(&event.run_id)
                .push_bind(event.event_idx)
                .push_bind(&event.user_id)
                .push_bind(&event.session_id)
                .push_bind(&event.event_type)
                .push_bind(&event.event_id)
                .push_bind(&event.agent_id)
                .push_bind(&event.subject_run_id)
                .push_bind(&event.interaction_request_id)
                .push_bind(&event.idempotency_key)
                .push_bind(&event.event_hash)
                .push_bind(&event.producer_pod_id)
                .push_bind(&event.payload_json)
                .push("NOW(6)");
        });

        match builder.build().execute(self.pool.get()).await {
            Ok(_) => {}
            Err(source) => {
                return Err(db_error("insert_run_events_batch", run_id, source));
            }
        };

        // Update last_event_idx to the highest allocated index.
        // Use events.len() (allocated range), not actually_inserted, because
        // INSERT IGNORE may skip duplicate keys in the middle of the batch,
        // creating gaps. The allocated range is monotonic and correct.
        let last_idx = start_idx + events.len() as i64 - 1;
        sqlx::query(
            "UPDATE agent_runs
             SET last_event_idx = CASE WHEN ? > last_event_idx THEN ? ELSE last_event_idx END,
                 updated_at = NOW(6)
             WHERE user_id = ? AND run_id = ?",
        )
        .bind(last_idx)
        .bind(last_idx)
        .bind(user_id)
        .bind(run_id)
        .execute(self.pool.get())
        .await
        .map_err(|source| db_error("update_run_last_event_idx_batch", run_id, source))?;

        self.sync_projection_for_user(user_id, run_id, None, None)
            .await?;

        Ok(())
    }
}

fn serialize_tool_output_payloads(items: &[ToolOutputBatchItem]) -> DbStoreResult<Vec<String>> {
    items
        .iter()
        .map(|item| {
            item.result.validate().map_err(|source| {
                DatabaseRunStateStoreError::InvalidToolOutput {
                    output_id: item.output_id.clone(),
                    source,
                }
            })?;
            serde_json::to_string(&item.result).map_err(|source| DatabaseRunStateStoreError::Json {
                operation: "serialize_tool_output",
                entity: item.output_id.clone(),
                source,
            })
        })
        .collect()
}

fn run_owner_lease_renewal_interval(lease_ttl: Duration) -> Duration {
    let interval_ms = (lease_ttl.as_millis().max(1) / 3).clamp(1, 15_000);
    Duration::from_millis(u64::try_from(interval_ms).unwrap_or(u64::MAX))
}

#[async_trait]
impl RunStateStore for DatabaseRunStateStore {
    async fn insert_run(&self, mut record: DurableRunRecord) -> Result<(), String> {
        if (record.root_run_id.is_none() || record.ancestor_path.is_none())
            && let Some(parent_run_id) = record.parent_run_id.as_deref()
            && let Some(parent) = self
                .load_run_metadata_for_user(&record.user_id, parent_run_id)
                .await
                .map_err(|e| e.to_string())?
        {
            let parent_root = parent.root_run_id.unwrap_or(parent.run_id.clone());
            let parent_path = parent.ancestor_path.unwrap_or(parent.run_id);
            record.root_run_id.get_or_insert(parent_root);
            record
                .ancestor_path
                .get_or_insert_with(|| format!("{parent_path}/{}", record.run_id));
            if record.depth == 0 {
                record.depth = parent.depth.saturating_add(1);
            }
        }
        record
            .root_run_id
            .get_or_insert_with(|| record.run_id.clone());
        record
            .ancestor_path
            .get_or_insert_with(|| record.run_id.clone());
        let retry_scope = record
            .retry_scope
            .as_deref()
            .unwrap_or(DEFAULT_RETRY_SCOPE)
            .to_string();
        validate_retry_scope(&record.run_id, &retry_scope).map_err(|e| e.to_string())?;

        let lease_expires_at = chrono::Utc::now()
            + chrono::Duration::from_std(self.lease_ttl)
                .unwrap_or_else(|_| chrono::Duration::seconds(45));
        let events = std::mem::take(&mut record.events);

        // New run: last_event_idx must be -1 (no events written yet) so the
        // first batch append allocates indices starting at 0.
        if !events.is_empty() {
            record.last_event_idx = -1;
        }

        let insert_result = if run_requires_session_execution_slot(&record)
            && durable_run_status_blocks_session(&record.status, record.waiting_for.as_deref())
        {
            let mut tx = self.pool.get().begin().await.map_err(|source| {
                db_error("insert_run_begin", &record.run_id, source).to_string()
            })?;
            if !self
                .acquire_session_execution_slot_tx(
                    &mut tx,
                    &record.user_id,
                    &record.session_id,
                    &record.run_id,
                )
                .await
                .map_err(|e| e.to_string())?
            {
                tx.rollback().await.map_err(|source| {
                    db_error("insert_run_rollback_slot_blocked", &record.run_id, source).to_string()
                })?;
                return Err("session already has an active run".to_string());
            }
            let result = sqlx::query(
                "INSERT INTO agent_runs
                 (run_id, user_id, session_id, parent_run_id, root_run_id, ancestor_path, depth,
                  delegation_id, agent_id, retry_of, retry_scope, status, waiting_for,
                  owner_pod_id, owner_lease_expires_at, run_generation, last_event_idx,
                  checkpoint_version, checkpoint_json, error_code, error_message, retry_count,
                  total_prompt_tokens, total_completion_tokens, total_tool_calls,
                  agent_binding_id, agent_binding_name, agent_binding_schema_version,
                  model_offering_id, resolved_model_name,
                  capability_server_refs_json, runtime_profile, created_at, updated_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW(6), NOW(6))",
            )
            .bind(&record.run_id)
            .bind(&record.user_id)
            .bind(&record.session_id)
            .bind(&record.parent_run_id)
            .bind(record.root_run_id.as_deref().unwrap_or(&record.run_id))
            .bind(record.ancestor_path.as_deref().unwrap_or(&record.run_id))
            .bind(record.depth as i64)
            .bind(&record.delegation_id)
            .bind(&record.agent_id)
            .bind(&record.retry_of)
            .bind(&retry_scope)
            .bind(&record.status)
            .bind(&record.waiting_for)
            .bind(&self.owner_pod_id)
            .bind(lease_expires_at)
            .bind(record.run_generation as i64)
            .bind(record.last_event_idx)
            .bind(&record.checkpoint_version)
            .bind(&record.checkpoint_json)
            .bind(&record.error_code)
            .bind(&record.error_message)
            .bind(record.retry_count as i64)
            .bind(record.total_prompt_tokens as i64)
            .bind(record.total_completion_tokens as i64)
            .bind(record.total_tool_calls as i64)
            .bind(&record.agent_binding_id)
            .bind(&record.agent_binding_name)
            .bind(&record.agent_binding_schema_version)
            .bind(&record.model_offering_id)
            .bind(&record.resolved_model_name)
            .bind(&record.capability_server_refs_json)
            .bind(&record.runtime_profile)
            .execute(&mut *tx)
            .await
            .map_err(|source| db_error("insert_run", &record.run_id, source).to_string())?;
            tx.commit().await.map_err(|source| {
                db_error("insert_run_commit", &record.run_id, source).to_string()
            })?;
            result
        } else {
            sqlx::query(
                "INSERT INTO agent_runs
                 (run_id, user_id, session_id, parent_run_id, root_run_id, ancestor_path, depth,
                  delegation_id, agent_id, retry_of, retry_scope, status, waiting_for,
                  owner_pod_id, owner_lease_expires_at, run_generation, last_event_idx,
                  checkpoint_version, checkpoint_json, error_code, error_message, retry_count,
                  total_prompt_tokens, total_completion_tokens, total_tool_calls,
                  agent_binding_id, agent_binding_name, agent_binding_schema_version,
                  model_offering_id, resolved_model_name,
                  capability_server_refs_json, runtime_profile, created_at, updated_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW(6), NOW(6))
                 ON DUPLICATE KEY UPDATE updated_at = NOW(6)",
            )
            .bind(&record.run_id)
            .bind(&record.user_id)
            .bind(&record.session_id)
            .bind(&record.parent_run_id)
            .bind(record.root_run_id.as_deref().unwrap_or(&record.run_id))
            .bind(record.ancestor_path.as_deref().unwrap_or(&record.run_id))
            .bind(record.depth as i64)
            .bind(&record.delegation_id)
            .bind(&record.agent_id)
            .bind(&record.retry_of)
            .bind(&retry_scope)
            .bind(&record.status)
            .bind(&record.waiting_for)
            .bind(&self.owner_pod_id)
            .bind(lease_expires_at)
            .bind(record.run_generation as i64)
            .bind(record.last_event_idx)
            .bind(&record.checkpoint_version)
            .bind(&record.checkpoint_json)
            .bind(&record.error_code)
            .bind(&record.error_message)
            .bind(record.retry_count as i64)
            .bind(record.total_prompt_tokens as i64)
            .bind(record.total_completion_tokens as i64)
            .bind(record.total_tool_calls as i64)
            .bind(&record.agent_binding_id)
            .bind(&record.agent_binding_name)
            .bind(&record.agent_binding_schema_version)
            .bind(&record.model_offering_id)
            .bind(&record.resolved_model_name)
            .bind(&record.capability_server_refs_json)
            .bind(&record.runtime_profile)
            .execute(self.pool.get())
            .await
            .map_err(|source| db_error("insert_run", &record.run_id, source).to_string())?
        };
        if insert_result.rows_affected() == 0 {
            return Err("session already has an active run".to_string());
        }

        if !events.is_empty() {
            self.append_events_batch_for_user(&record.user_id, &record.run_id, &events)
                .await
                .map_err(|e| e.to_string())?;
        }
        self.sync_projection_for_user(
            &record.user_id,
            &record.run_id,
            record.events.last().map(extract_event_type).as_deref(),
            None,
        )
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn load_run(
        &self,
        user_id: &str,
        run_id: &str,
    ) -> Result<Option<DurableRunRecord>, String> {
        let Some(mut run) = self
            .load_run_metadata_for_user(user_id, run_id)
            .await
            .map_err(|e| e.to_string())?
        else {
            return Ok(None);
        };

        let rows = sqlx::query(
            "SELECT payload_json, event_idx FROM agent_run_events
             WHERE user_id = ? AND run_id = ?
             ORDER BY event_idx ASC",
        )
        .bind(&run.user_id)
        .bind(run_id)
        .fetch_all(self.pool.get())
        .await
        .map_err(|source| db_error("load_run_events", run_id, source).to_string())?;

        run.events = rows
            .into_iter()
            .map(|row| decode_run_event_payload(&row, run_id))
            .collect::<DbStoreResult<Vec<_>>>()
            .map_err(|e| e.to_string())?;
        Ok(Some(run))
    }

    async fn load_run_control(
        &self,
        user_id: &str,
        run_id: &str,
    ) -> Result<Option<DurableRunControlRecord>, String> {
        let row = sqlx::query(
            "SELECT run_id, status, waiting_for, parent_run_id, ancestor_path
             FROM agent_runs WHERE user_id = ? AND run_id = ?",
        )
        .bind(user_id)
        .bind(run_id)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| db_error("load_run_control", run_id, source).to_string())?;
        row.map(|row| decode_run_control_row(&row, run_id))
            .transpose()
    }

    async fn load_run_controls(
        &self,
        user_id: &str,
        run_ids: &[String],
    ) -> Result<Vec<DurableRunControlRecord>, String> {
        if run_ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut query = sqlx::QueryBuilder::<sqlx::MySql>::new(
            "SELECT run_id, status, waiting_for, parent_run_id, ancestor_path
             FROM agent_runs WHERE user_id = ",
        );
        query.push_bind(user_id).push(" AND run_id IN (");
        {
            let mut ids = query.separated(",");
            for run_id in run_ids {
                ids.push_bind(run_id);
            }
        }
        query.push(")");
        query
            .build()
            .fetch_all(self.pool.get())
            .await
            .map_err(|source| db_error("load_run_controls", "lineage", source).to_string())?
            .into_iter()
            .map(|row| decode_run_control_row(&row, "lineage"))
            .collect()
    }

    async fn update_run_status(
        &self,
        user_id: &str,
        run_id: &str,
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
    ) -> Result<bool, String> {
        let terminal_error_code = terminal_error_code_from_message(status, error_message);
        let mut tx =
            self.pool.get().begin().await.map_err(|source| {
                db_error("update_run_status_begin", run_id, source).to_string()
            })?;
        // Load run metadata inside the transaction so the slot ownership
        // check sees the same row version as the UPDATE, closing the TOCTOU
        // window where a concurrent agent_id flip could misattribute the slot.
        let Some(run) = self
            .load_run_metadata_for_user_tx(&mut tx, user_id, run_id)
            .await
            .map_err(|e| e.to_string())?
        else {
            tx.rollback().await.map_err(|source| {
                db_error("update_run_status_rollback_missing", run_id, source).to_string()
            })?;
            return Ok(false);
        };
        if let Err(error) = ensure_terminal_status_immutable(&run, status) {
            tx.rollback().await.map_err(|source| {
                db_error(
                    "update_run_status_rollback_terminal_conflict",
                    run_id,
                    source,
                )
                .to_string()
            })?;
            return Err(error);
        }
        let mut query = sqlx::QueryBuilder::<sqlx::MySql>::new("UPDATE agent_runs SET status = ");
        query.push_bind(status);
        query.push(", waiting_for = ");
        query.push_bind(waiting_for);
        if let Some(error_message) = error_message {
            query.push(", error_message = ");
            query.push_bind(error_message);
        }
        if let Some(error_code) = terminal_error_code.as_deref() {
            query.push(", error_code = ");
            query.push_bind(error_code);
        }
        query.push(", updated_at = NOW(6) WHERE user_id = ");
        query.push_bind(user_id);
        query.push(" AND run_id = ");
        query.push_bind(run_id);
        let result = query
            .build()
            .execute(&mut *tx)
            .await
            .map_err(|source| db_error("update_run_status", run_id, source).to_string())?;
        if result.rows_affected() == 0 {
            tx.rollback().await.map_err(|source| {
                db_error("update_run_status_rollback_conflict", run_id, source).to_string()
            })?;
            return Ok(false);
        }
        if !self
            .sync_session_execution_slot_after_status_tx(&mut tx, &run, status, waiting_for)
            .await
            .map_err(|e| e.to_string())?
        {
            tx.rollback().await.map_err(|source| {
                db_error("update_run_status_rollback_slot_blocked", run_id, source).to_string()
            })?;
            return Ok(false);
        }
        tx.commit()
            .await
            .map_err(|source| db_error("update_run_status_commit", run_id, source).to_string())?;
        if result.rows_affected() > 0
            && let Err(error) = self
                .sync_projection_for_user(user_id, run_id, None, None)
                .await
        {
            tracing::warn!(
                user_id,
                run_id,
                error = %error,
                "run status committed but display projection refresh failed"
            );
        }
        Ok(true)
    }

    async fn update_run_status_if_current(
        &self,
        user_id: &str,
        run_id: &str,
        expected_statuses: &[&str],
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
    ) -> Result<bool, String> {
        if expected_statuses.is_empty() {
            return Ok(false);
        }
        let mut tx = self.pool.get().begin().await.map_err(|source| {
            db_error("update_run_status_if_current_begin", run_id, source).to_string()
        })?;
        // Load run metadata inside the transaction so the slot ownership
        // check sees the same row version as the UPDATE, closing the TOCTOU
        // window where a concurrent agent_id flip could misattribute the slot.
        let Some(run) = self
            .load_run_metadata_for_user_tx(&mut tx, user_id, run_id)
            .await
            .map_err(|e| e.to_string())?
        else {
            tx.rollback().await.map_err(|source| {
                db_error(
                    "update_run_status_if_current_rollback_missing",
                    run_id,
                    source,
                )
                .to_string()
            })?;
            return Ok(false);
        };
        if expected_statuses.contains(&run.status.as_str())
            && let Err(error) = ensure_terminal_status_immutable(&run, status)
        {
            tx.rollback().await.map_err(|source| {
                db_error(
                    "update_run_status_if_current_rollback_terminal_conflict",
                    run_id,
                    source,
                )
                .to_string()
            })?;
            return Err(error);
        }
        let mut query = sqlx::QueryBuilder::<sqlx::MySql>::new("UPDATE agent_runs SET status = ");
        query.push_bind(status);
        query.push(", waiting_for = ");
        query.push_bind(waiting_for);
        if let Some(error_message) = error_message {
            query.push(", error_message = ");
            query.push_bind(error_message);
        }
        let terminal_error_code = terminal_error_code_from_message(status, error_message);
        if let Some(error_code) = terminal_error_code.as_deref() {
            query.push(", error_code = ");
            query.push_bind(error_code);
        }
        query.push(", updated_at = NOW(6) WHERE user_id = ");
        query.push_bind(user_id);
        query.push(" AND run_id = ");
        query.push_bind(run_id);
        query.push(" AND status IN (");
        let mut separated = query.separated(", ");
        for expected in expected_statuses {
            separated.push_bind(*expected);
        }
        separated.push_unseparated(")");
        let result = query.build().execute(&mut *tx).await.map_err(|source| {
            db_error("update_run_status_if_current", run_id, source).to_string()
        })?;
        if result.rows_affected() == 0 {
            tx.rollback().await.map_err(|source| {
                db_error(
                    "update_run_status_if_current_rollback_conflict",
                    run_id,
                    source,
                )
                .to_string()
            })?;
            return Ok(false);
        }
        if !self
            .sync_session_execution_slot_after_status_tx(&mut tx, &run, status, waiting_for)
            .await
            .map_err(|e| e.to_string())?
        {
            tx.rollback().await.map_err(|source| {
                db_error(
                    "update_run_status_if_current_rollback_slot_blocked",
                    run_id,
                    source,
                )
                .to_string()
            })?;
            return Ok(false);
        }
        tx.commit().await.map_err(|source| {
            db_error("update_run_status_if_current_commit", run_id, source).to_string()
        })?;
        if result.rows_affected() > 0
            && let Err(error) = self
                .sync_projection_for_user(user_id, run_id, None, None)
                .await
        {
            tracing::warn!(
                user_id,
                run_id,
                error = %error,
                "run status CAS committed but display projection refresh failed"
            );
        }
        Ok(true)
    }

    async fn update_run_status_with_event_if_current(
        &self,
        user_id: &str,
        run_id: &str,
        expected_statuses: &[&str],
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
        event: serde_json::Value,
    ) -> Result<bool, String> {
        if expected_statuses.is_empty() {
            return Ok(false);
        }
        let terminal_error_code = terminal_error_code_from_transition(
            status,
            error_message,
            std::slice::from_ref(&event),
        );

        let mut tx = self.pool.get().begin().await.map_err(|source| {
            db_error("transition_run_status_with_event_begin", run_id, source).to_string()
        })?;

        let Some(run) = self
            .load_run_metadata_for_user_tx(&mut tx, user_id, run_id)
            .await
            .map_err(|e| e.to_string())?
        else {
            tx.rollback().await.map_err(|source| {
                db_error(
                    "transition_run_status_with_event_rollback_missing",
                    run_id,
                    source,
                )
                .to_string()
            })?;
            return Ok(false);
        };
        if expected_statuses.contains(&run.status.as_str())
            && let Err(error) = ensure_terminal_status_immutable(&run, status)
        {
            tx.rollback().await.map_err(|source| {
                db_error(
                    "transition_run_status_with_event_rollback_terminal_conflict",
                    run_id,
                    source,
                )
                .to_string()
            })?;
            return Err(error);
        }
        let session_id = run.session_id.clone();
        let agent_id = run.agent_id.clone();
        let last_event_idx = run.last_event_idx;
        let event_idx = last_event_idx + 1;

        let event_row = match build_run_event_insert_row(
            user_id,
            run_id,
            &session_id,
            agent_id.as_deref(),
            event_idx,
            &self.owner_pod_id,
            &event,
        ) {
            Ok(row) => row,
            Err(error) => {
                tx.rollback().await.map_err(|source| {
                    db_error(
                        "transition_run_status_with_event_rollback_prepare_event",
                        run_id,
                        source,
                    )
                    .to_string()
                })?;
                return Err(error.to_string());
            }
        };

        let mut update = sqlx::QueryBuilder::<sqlx::MySql>::new("UPDATE agent_runs SET status = ");
        update.push_bind(status);
        update.push(", waiting_for = ");
        update.push_bind(waiting_for);
        if let Some(error_message) = error_message {
            update.push(", error_message = ");
            update.push_bind(error_message);
        }
        if let Some(error_code) = terminal_error_code.as_deref() {
            update.push(", error_code = ");
            update.push_bind(error_code);
        }
        update.push(", last_event_idx = ");
        update.push_bind(event_idx);
        update.push(", updated_at = NOW(6) WHERE user_id = ");
        update.push_bind(user_id);
        update.push(" AND run_id = ");
        update.push_bind(run_id);
        update.push(" AND last_event_idx = ");
        update.push_bind(last_event_idx);
        update.push(" AND status IN (");
        let mut separated = update.separated(", ");
        for expected in expected_statuses {
            separated.push_bind(*expected);
        }
        separated.push_unseparated(")");

        let update_result = update.build().execute(&mut *tx).await.map_err(|source| {
            db_error("transition_run_status_with_event_update", run_id, source).to_string()
        })?;
        if update_result.rows_affected() == 0 {
            tx.rollback().await.map_err(|source| {
                db_error(
                    "transition_run_status_with_event_rollback_conflict",
                    run_id,
                    source,
                )
                .to_string()
            })?;
            return Ok(false);
        }
        if !self
            .sync_session_execution_slot_after_status_tx(&mut tx, &run, status, waiting_for)
            .await
            .map_err(|e| e.to_string())?
        {
            tx.rollback().await.map_err(|source| {
                db_error(
                    "transition_run_status_with_event_rollback_slot_blocked",
                    run_id,
                    source,
                )
                .to_string()
            })?;
            return Ok(false);
        }

        let insert_result = sqlx::query(
            "INSERT INTO agent_run_events
             (id, run_id, event_idx, user_id, session_id, event_type, event_id, agent_id,
              subject_run_id, interaction_request_id, idempotency_key, event_hash, producer_pod_id, payload_json, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW(6))",
        )
        .bind(&event_row.id)
        .bind(&event_row.run_id)
        .bind(event_row.event_idx)
        .bind(&event_row.user_id)
        .bind(&event_row.session_id)
        .bind(&event_row.event_type)
        .bind(&event_row.event_id)
        .bind(&event_row.agent_id)
        .bind(&event_row.subject_run_id)
        .bind(&event_row.interaction_request_id)
        .bind(&event_row.idempotency_key)
        .bind(&event_row.event_hash)
        .bind(&event_row.producer_pod_id)
        .bind(&event_row.payload_json)
        .execute(&mut *tx)
        .await;
        if let Err(source) = insert_result {
            let rollback_error = tx.rollback().await.err();
            let mut detail = db_error(
                "transition_run_status_with_event_insert_event",
                run_id,
                source,
            )
            .to_string();
            if let Some(rollback_error) = rollback_error {
                detail.push_str(&format!(
                    "; rollback after insert failure also failed: {rollback_error}"
                ));
            }
            return Err(detail);
        }

        tx.commit().await.map_err(|source| {
            db_error("transition_run_status_with_event_commit", run_id, source).to_string()
        })?;

        self.patch_or_repair_run_projection_status_for_user(
            user_id,
            run_id,
            status,
            waiting_for,
            error_message,
            event_row.event_idx,
            Some(&event_row.event_type),
        )
        .await;
        Ok(true)
    }

    async fn update_run_status_with_event_if_current_unless_session_blocked(
        &self,
        request: GuardedRunStatusTransitionRequest<'_>,
    ) -> Result<GuardedRunStatusTransition, String> {
        let GuardedRunStatusTransitionRequest {
            user_id,
            run_id,
            session_id,
            expected_statuses,
            status,
            waiting_for,
            error_message,
            event,
        } = request;
        if expected_statuses.is_empty() {
            return Ok(GuardedRunStatusTransition::StatusConflict);
        }
        let terminal_error_code = terminal_error_code_from_transition(
            status,
            error_message,
            std::slice::from_ref(&event),
        );

        let mut tx = self.pool.get().begin().await.map_err(|source| {
            db_error(
                "guarded_transition_run_status_with_event_begin",
                run_id,
                source,
            )
            .to_string()
        })?;

        let load_sql = format!(
            "SELECT {AGENT_RUN_COLUMNS} FROM agent_runs WHERE user_id = ? AND run_id = ? FOR UPDATE"
        );
        let Some(row) = sqlx::query(&load_sql)
            .bind(user_id)
            .bind(run_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|source| {
                db_error(
                    "guarded_transition_run_status_with_event_load_run",
                    run_id,
                    source,
                )
                .to_string()
            })?
        else {
            tx.rollback().await.map_err(|source| {
                db_error(
                    "guarded_transition_run_status_with_event_rollback_missing",
                    run_id,
                    source,
                )
                .to_string()
            })?;
            return Ok(GuardedRunStatusTransition::StatusConflict);
        };

        let run = run_record_from_row(row).map_err(|error| error.to_string())?;
        if run.session_id != session_id {
            tx.rollback().await.map_err(|source| {
                db_error(
                    "guarded_transition_run_status_with_event_rollback_session_mismatch",
                    run_id,
                    source,
                )
                .to_string()
            })?;
            return Ok(GuardedRunStatusTransition::StatusConflict);
        }
        if expected_statuses.contains(&run.status.as_str())
            && let Err(error) = ensure_terminal_status_immutable(&run, status)
        {
            tx.rollback().await.map_err(|source| {
                db_error(
                    "guarded_transition_run_status_with_event_rollback_terminal_conflict",
                    run_id,
                    source,
                )
                .to_string()
            })?;
            return Err(error);
        }
        let last_event_idx = run.last_event_idx;
        let event_idx = last_event_idx + 1;

        let event_row = match build_run_event_insert_row(
            user_id,
            run_id,
            session_id,
            run.agent_id.as_deref(),
            event_idx,
            &self.owner_pod_id,
            &event,
        ) {
            Ok(row) => row,
            Err(error) => {
                tx.rollback().await.map_err(|source| {
                    db_error(
                        "guarded_transition_run_status_with_event_rollback_prepare_event",
                        run_id,
                        source,
                    )
                    .to_string()
                })?;
                return Err(error.to_string());
            }
        };

        let mut update = sqlx::QueryBuilder::<sqlx::MySql>::new("UPDATE agent_runs SET status = ");
        update.push_bind(status);
        update.push(", waiting_for = ");
        update.push_bind(waiting_for);
        if let Some(error_message) = error_message {
            update.push(", error_message = ");
            update.push_bind(error_message);
        }
        if let Some(error_code) = terminal_error_code.as_deref() {
            update.push(", error_code = ");
            update.push_bind(error_code);
        }
        update.push(", last_event_idx = ");
        update.push_bind(event_idx);
        update.push(", updated_at = NOW(6) WHERE user_id = ");
        update.push_bind(user_id);
        update.push(" AND run_id = ");
        update.push_bind(run_id);
        update.push(" AND session_id = ");
        update.push_bind(session_id);
        update.push(" AND last_event_idx = ");
        update.push_bind(last_event_idx);
        update.push(" AND status IN (");
        {
            let mut separated = update.separated(", ");
            for expected in expected_statuses {
                separated.push_bind(*expected);
            }
            separated.push_unseparated(")");
        }

        let update_result = update.build().execute(&mut *tx).await.map_err(|source| {
            db_error(
                "guarded_transition_run_status_with_event_update",
                run_id,
                source,
            )
            .to_string()
        })?;
        if update_result.rows_affected() == 0 {
            tx.rollback().await.map_err(|source| {
                db_error(
                    "guarded_transition_run_status_with_event_rollback_conflict",
                    run_id,
                    source,
                )
                .to_string()
            })?;
            return Ok(GuardedRunStatusTransition::StatusConflict);
        }
        if !self
            .sync_session_execution_slot_after_status_tx(&mut tx, &run, status, waiting_for)
            .await
            .map_err(|e| e.to_string())?
        {
            tx.rollback().await.map_err(|source| {
                db_error(
                    "guarded_transition_run_status_with_event_rollback_slot_blocked",
                    run_id,
                    source,
                )
                .to_string()
            })?;
            return Ok(GuardedRunStatusTransition::SessionBlocked);
        }

        let insert_result = sqlx::query(
            "INSERT INTO agent_run_events
             (id, run_id, event_idx, user_id, session_id, event_type, event_id, agent_id,
              subject_run_id, interaction_request_id, idempotency_key, event_hash, producer_pod_id, payload_json, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW(6))",
        )
        .bind(&event_row.id)
        .bind(&event_row.run_id)
        .bind(event_row.event_idx)
        .bind(&event_row.user_id)
        .bind(&event_row.session_id)
        .bind(&event_row.event_type)
        .bind(&event_row.event_id)
        .bind(&event_row.agent_id)
        .bind(&event_row.subject_run_id)
        .bind(&event_row.interaction_request_id)
        .bind(&event_row.idempotency_key)
        .bind(&event_row.event_hash)
        .bind(&event_row.producer_pod_id)
        .bind(&event_row.payload_json)
        .execute(&mut *tx)
        .await;
        if let Err(source) = insert_result {
            let rollback_error = tx.rollback().await.err();
            let mut detail = db_error(
                "guarded_transition_run_status_with_event_insert_event",
                run_id,
                source,
            )
            .to_string();
            if let Some(rollback_error) = rollback_error {
                detail.push_str(&format!(
                    "; rollback after insert failure also failed: {rollback_error}"
                ));
            }
            return Err(detail);
        }

        tx.commit().await.map_err(|source| {
            db_error(
                "guarded_transition_run_status_with_event_commit",
                run_id,
                source,
            )
            .to_string()
        })?;

        self.patch_or_repair_run_projection_status_for_user(
            user_id,
            run_id,
            status,
            waiting_for,
            error_message,
            event_row.event_idx,
            Some(&event_row.event_type),
        )
        .await;
        Ok(GuardedRunStatusTransition::Updated)
    }

    async fn update_run_status_with_events_if_current(
        &self,
        user_id: &str,
        run_id: &str,
        expected_statuses: &[&str],
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
        events: &[serde_json::Value],
    ) -> Result<bool, String> {
        if expected_statuses.is_empty() {
            return Ok(false);
        }
        let terminal_error_code =
            terminal_error_code_from_transition(status, error_message, events);

        let mut tx = self.pool.get().begin().await.map_err(|source| {
            db_error("transition_run_status_with_events_begin", run_id, source).to_string()
        })?;

        let Some(run) = self
            .load_run_metadata_for_user_tx(&mut tx, user_id, run_id)
            .await
            .map_err(|e| e.to_string())?
        else {
            tx.rollback().await.map_err(|source| {
                db_error(
                    "transition_run_status_with_events_rollback_missing",
                    run_id,
                    source,
                )
                .to_string()
            })?;
            return Ok(false);
        };
        if expected_statuses.contains(&run.status.as_str())
            && let Err(error) = ensure_terminal_status_immutable(&run, status)
        {
            tx.rollback().await.map_err(|source| {
                db_error(
                    "transition_run_status_with_events_rollback_terminal_conflict",
                    run_id,
                    source,
                )
                .to_string()
            })?;
            return Err(error);
        }
        let session_id = run.session_id.clone();
        let agent_id = run.agent_id.clone();
        let last_event_idx = run.last_event_idx;

        let mut event_rows = Vec::with_capacity(events.len());
        for (offset, event) in events.iter().enumerate() {
            match build_run_event_insert_row(
                user_id,
                run_id,
                &session_id,
                agent_id.as_deref(),
                last_event_idx + 1 + offset as i64,
                &self.owner_pod_id,
                event,
            ) {
                Ok(row) => event_rows.push(row),
                Err(error) => {
                    tx.rollback().await.map_err(|source| {
                        db_error(
                            "transition_run_status_with_events_rollback_prepare_event",
                            run_id,
                            source,
                        )
                        .to_string()
                    })?;
                    return Err(error.to_string());
                }
            }
        }

        let next_last_event_idx = last_event_idx + events.len() as i64;
        let mut update = sqlx::QueryBuilder::<sqlx::MySql>::new("UPDATE agent_runs SET status = ");
        update.push_bind(status);
        update.push(", waiting_for = ");
        update.push_bind(waiting_for);
        if let Some(error_message) = error_message {
            update.push(", error_message = ");
            update.push_bind(error_message);
        }
        if let Some(error_code) = terminal_error_code.as_deref() {
            update.push(", error_code = ");
            update.push_bind(error_code);
        }
        update.push(", last_event_idx = ");
        update.push_bind(next_last_event_idx);
        update.push(", updated_at = NOW(6) WHERE user_id = ");
        update.push_bind(user_id);
        update.push(" AND run_id = ");
        update.push_bind(run_id);
        update.push(" AND last_event_idx = ");
        update.push_bind(last_event_idx);
        update.push(" AND status IN (");
        let mut separated = update.separated(", ");
        for expected in expected_statuses {
            separated.push_bind(*expected);
        }
        separated.push_unseparated(")");

        let update_result = update.build().execute(&mut *tx).await.map_err(|source| {
            db_error("transition_run_status_with_events_update", run_id, source).to_string()
        })?;
        if update_result.rows_affected() == 0 {
            tx.rollback().await.map_err(|source| {
                db_error(
                    "transition_run_status_with_events_rollback_conflict",
                    run_id,
                    source,
                )
                .to_string()
            })?;
            return Ok(false);
        }
        if !self
            .sync_session_execution_slot_after_status_tx(&mut tx, &run, status, waiting_for)
            .await
            .map_err(|e| e.to_string())?
        {
            tx.rollback().await.map_err(|source| {
                db_error(
                    "transition_run_status_with_events_rollback_slot_blocked",
                    run_id,
                    source,
                )
                .to_string()
            })?;
            return Ok(false);
        }

        if !event_rows.is_empty() {
            let mut insert = sqlx::QueryBuilder::<sqlx::MySql>::new(
                "INSERT INTO agent_run_events
                 (id, run_id, event_idx, user_id, session_id, event_type, event_id, agent_id,
                  subject_run_id, interaction_request_id, idempotency_key, event_hash, producer_pod_id, payload_json, created_at) ",
            );
            insert.push_values(&event_rows, |mut row, event| {
                row.push_bind(&event.id)
                    .push_bind(&event.run_id)
                    .push_bind(event.event_idx)
                    .push_bind(&event.user_id)
                    .push_bind(&event.session_id)
                    .push_bind(&event.event_type)
                    .push_bind(&event.event_id)
                    .push_bind(&event.agent_id)
                    .push_bind(&event.subject_run_id)
                    .push_bind(&event.interaction_request_id)
                    .push_bind(&event.idempotency_key)
                    .push_bind(&event.event_hash)
                    .push_bind(&event.producer_pod_id)
                    .push_bind(&event.payload_json)
                    .push("NOW(6)");
            });
            let insert_result = insert.build().execute(&mut *tx).await;
            if let Err(source) = insert_result {
                let rollback_error = tx.rollback().await.err();
                let mut detail = db_error(
                    "transition_run_status_with_events_insert_events",
                    run_id,
                    source,
                )
                .to_string();
                if let Some(rollback_error) = rollback_error {
                    detail.push_str(&format!(
                        "; rollback after insert failure also failed: {rollback_error}"
                    ));
                }
                return Err(detail);
            }
        }

        tx.commit().await.map_err(|source| {
            db_error("transition_run_status_with_events_commit", run_id, source).to_string()
        })?;

        self.patch_or_repair_run_projection_status_for_user(
            user_id,
            run_id,
            status,
            waiting_for,
            error_message,
            next_last_event_idx,
            event_rows.last().map(|event| event.event_type.as_str()),
        )
        .await;
        Ok(true)
    }

    async fn update_run_usage(
        &self,
        user_id: &str,
        run_id: &str,
        prompt_tokens: u64,
        completion_tokens: u64,
        tool_calls: u32,
    ) -> Result<bool, String> {
        let result = sqlx::query(
            "UPDATE agent_runs
             SET total_prompt_tokens = ?, total_completion_tokens = ?, total_tool_calls = ?, updated_at = NOW(6)
             WHERE user_id = ? AND run_id = ?",
        )
        .bind(prompt_tokens as i64)
        .bind(completion_tokens as i64)
        .bind(tool_calls as i64)
        .bind(user_id)
        .bind(run_id)
        .execute(self.pool.get())
        .await
        .map_err(|source| db_error("update_run_usage", run_id, source).to_string())?;
        if result.rows_affected() > 0 {
            match self
                .patch_run_projection_usage_for_user(
                    user_id,
                    run_id,
                    prompt_tokens,
                    completion_tokens,
                    tool_calls,
                )
                .await
            {
                Ok(0) => {
                    if let Err(error) = self
                        .sync_projection_for_user(user_id, run_id, None, None)
                        .await
                    {
                        tracing::warn!(
                            user_id,
                            run_id,
                            error = %error,
                            "run usage committed but display projection repair failed"
                        );
                    }
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(
                        user_id,
                        run_id,
                        error = %error,
                        "run usage committed but display projection usage patch failed"
                    );
                }
            }
        }
        Ok(result.rows_affected() > 0)
    }

    async fn save_checkpoint(
        &self,
        user_id: &str,
        run_id: &str,
        checkpoint_json: &str,
    ) -> Result<bool, String> {
        let Some(run) = self
            .load_run_metadata_for_user(user_id, run_id)
            .await
            .map_err(|e| e.to_string())?
        else {
            return Ok(false);
        };
        let (checkpoint_kind, checkpoint_version, idempotency_key) =
            checkpoint_metadata(run_id, checkpoint_json)?;
        let checkpoint_id = format!("ckpt-{}", uuid::Uuid::now_v7());
        let created_at = chrono::Utc::now().naive_utc();
        let mut tx = self
            .pool
            .get()
            .begin()
            .await
            .map_err(|source| db_error("begin_save_checkpoint", run_id, source).to_string())?;
        sqlx::query(
            "INSERT INTO run_checkpoints
             (checkpoint_id, run_id, user_id, session_id, node_seq, checkpoint_kind,
              checkpoint_version, idempotency_key, checkpoint_json, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON DUPLICATE KEY UPDATE
              checkpoint_json = VALUES(checkpoint_json),
              checkpoint_version = VALUES(checkpoint_version),
              node_seq = VALUES(node_seq)",
        )
        .bind(&checkpoint_id)
        .bind(run_id)
        .bind(&run.user_id)
        .bind(&run.session_id)
        .bind(run.last_event_idx.max(0))
        .bind(&checkpoint_kind)
        .bind(&checkpoint_version)
        .bind(&idempotency_key)
        .bind(checkpoint_json)
        .bind(created_at)
        .execute(&mut *tx)
        .await
        .map_err(|source| db_error("insert_run_checkpoint", run_id, source).to_string())?;
        let result = sqlx::query(
            "UPDATE agent_runs
             SET checkpoint_version = ?, checkpoint_json = ?, updated_at = NOW(6)
             WHERE user_id = ? AND run_id = ?
               AND status NOT IN (?, ?, ?, ?)",
        )
        .bind(&checkpoint_version)
        .bind(checkpoint_json)
        .bind(user_id)
        .bind(run_id)
        .bind(STATUS_COMPLETED)
        .bind(STATUS_DELEGATED)
        .bind(STATUS_FAILED)
        .bind(STATUS_CANCELLED)
        .execute(&mut *tx)
        .await
        .map_err(|source| db_error("save_checkpoint", run_id, source).to_string())?;
        if result.rows_affected() == 0 {
            // Run was deleted or already finished — rollback and signal no-op.
            tx.rollback().await.map_err(|source| {
                db_error("rollback_save_checkpoint", run_id, source).to_string()
            })?;
            return Ok(false);
        }
        tx.commit()
            .await
            .map_err(|source| db_error("commit_save_checkpoint", run_id, source).to_string())?;
        self.sync_projection_for_user(
            user_id,
            run_id,
            None,
            Some(&DurableRunCheckpointRecord {
                checkpoint_id,
                run_id: run.run_id,
                user_id: run.user_id,
                session_id: run.session_id,
                node_seq: run.last_event_idx.max(0),
                checkpoint_kind,
                checkpoint_version,
                idempotency_key,
                checkpoint_json: checkpoint_json.to_string(),
                created_at: created_at.to_string(),
            }),
        )
        .await
        .map_err(|e| e.to_string())?;
        Ok(true)
    }

    async fn load_latest_checkpoint(
        &self,
        user_id: &str,
        run_id: &str,
        checkpoint_kind: Option<&str>,
    ) -> Result<Option<DurableRunCheckpointRecord>, String> {
        let Some(_run) = self
            .load_run_metadata_for_user(user_id, run_id)
            .await
            .map_err(|e| e.to_string())?
        else {
            return Ok(None);
        };

        let row = if let Some(checkpoint_kind) = checkpoint_kind {
            sqlx::query(
                "SELECT checkpoint_id, run_id, user_id, session_id, node_seq, checkpoint_kind,
                        checkpoint_version, idempotency_key, checkpoint_json,
                        DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at
                 FROM run_checkpoints
                 WHERE user_id = ? AND run_id = ? AND checkpoint_kind = ?
                 ORDER BY created_at DESC, checkpoint_id DESC
                 LIMIT 1",
            )
            .bind(user_id)
            .bind(run_id)
            .bind(checkpoint_kind)
            .fetch_optional(self.pool.get())
            .await
            .map_err(|source| db_error("load_latest_checkpoint", run_id, source).to_string())?
        } else {
            sqlx::query(
                "SELECT checkpoint_id, run_id, user_id, session_id, node_seq, checkpoint_kind,
                        checkpoint_version, idempotency_key, checkpoint_json,
                        DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s') AS created_at
                 FROM run_checkpoints
                 WHERE user_id = ? AND run_id = ?
                 ORDER BY created_at DESC, checkpoint_id DESC
                 LIMIT 1",
            )
            .bind(user_id)
            .bind(run_id)
            .fetch_optional(self.pool.get())
            .await
            .map_err(|source| db_error("load_latest_checkpoint", run_id, source).to_string())?
        };
        row.map(|row| decode_run_checkpoint_record_from_row(&row))
            .transpose()
            .map_err(|e| e.to_string())
    }

    async fn load_run_projection(
        &self,
        user_id: &str,
        run_id: &str,
    ) -> Result<Option<DurableRunDisplayProjectionRecord>, String> {
        self.load_run_projection_metadata_for_user(user_id, run_id)
            .await
            .map_err(|e| e.to_string())
    }

    async fn rebuild_run_projection(
        &self,
        user_id: &str,
        run_id: &str,
    ) -> Result<Option<DurableRunDisplayProjectionRecord>, String> {
        let Some(run) = self.load_run(user_id, run_id).await? else {
            return Ok(None);
        };
        let latest_event_type = run.events.last().map(extract_event_type);
        let latest_checkpoint = self.load_latest_checkpoint(user_id, run_id, None).await?;
        let projection = build_run_display_projection(
            &run,
            latest_event_type,
            latest_checkpoint.as_ref().map(checkpoint_summary_tuple),
        );
        self.upsert_run_projection(&projection)
            .await
            .map_err(|e| e.to_string())?;
        Ok(Some(projection))
    }

    // `append_event` not overridden — trait default delegates to
    // `append_events_batch` which uses bulk INSERT internally.

    async fn append_events_batch(
        &self,
        user_id: &str,
        run_id: &str,
        events: &[serde_json::Value],
    ) -> Result<(), String> {
        DatabaseRunStateStore::append_events_batch_for_user(self, user_id, run_id, events)
            .await
            .map_err(|e| e.to_string())
    }

    async fn load_run_interaction_event(
        &self,
        user_id: &str,
        run_id: &str,
        request_id: &str,
        event_type: &str,
    ) -> Result<Option<serde_json::Value>, String> {
        let row = sqlx::query(
            "SELECT event_idx, payload_json
             FROM agent_run_events
             WHERE user_id = ? AND run_id = ?
               AND interaction_request_id = ? AND event_type = ?
             ORDER BY event_idx DESC LIMIT 1",
        )
        .bind(user_id)
        .bind(run_id)
        .bind(request_id)
        .bind(event_type)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| db_error("load_run_interaction_event", run_id, source).to_string())?;
        row.map(|row| decode_run_event_payload(&row, run_id))
            .transpose()
            .map_err(|error| error.to_string())
    }

    async fn resolve_run_interaction(
        &self,
        user_id: &str,
        run_id: &str,
        request_id: &str,
        kind: DurableRunInteractionKind,
        response_data: serde_json::Value,
    ) -> Result<DurableRunInteractionResolveOutcome, String> {
        let events = interaction_resolution_events(kind, request_id, response_data.clone());
        for _ in 0..3 {
            let mut tx = self.pool.get().begin().await.map_err(|source| {
                db_error("resolve_run_interaction_begin", run_id, source).to_string()
            })?;
            let Some(run) = self
                .load_run_metadata_for_user_tx(&mut tx, user_id, run_id)
                .await
                .map_err(|error| error.to_string())?
            else {
                tx.rollback().await.map_err(|source| {
                    db_error("resolve_run_interaction_rollback_missing", run_id, source).to_string()
                })?;
                return Ok(DurableRunInteractionResolveOutcome::MissingRequest);
            };
            let rows = sqlx::query(
                "SELECT event_idx, event_type, payload_json
                 FROM agent_run_events
                 WHERE user_id = ? AND run_id = ? AND interaction_request_id = ?
                   AND event_type IN (?, ?)
                 ORDER BY event_idx ASC",
            )
            .bind(user_id)
            .bind(run_id)
            .bind(request_id)
            .bind(kind.required_event_type())
            .bind(kind.resolved_event_type())
            .fetch_all(&mut *tx)
            .await
            .map_err(|source| {
                db_error("resolve_run_interaction_load_facts", run_id, source).to_string()
            })?;
            let mut required_found = false;
            let mut existing_response = None;
            for row in rows {
                let event_type: String = row.try_get("event_type").map_err(|source| {
                    db_error("decode_run_interaction_event_type", run_id, source).to_string()
                })?;
                let event =
                    decode_run_event_payload(&row, run_id).map_err(|error| error.to_string())?;
                if event_type == kind.required_event_type() {
                    required_found = true;
                } else if event_type == kind.resolved_event_type() {
                    existing_response = Some(event);
                }
            }
            if let Some(existing) = existing_response {
                tx.rollback().await.map_err(|source| {
                    db_error("resolve_run_interaction_rollback_existing", run_id, source)
                        .to_string()
                })?;
                return Ok(if interaction_response_matches(&existing, &response_data) {
                    DurableRunInteractionResolveOutcome::Idempotent(existing)
                } else {
                    DurableRunInteractionResolveOutcome::Conflict(existing)
                });
            }
            if !required_found {
                tx.rollback().await.map_err(|source| {
                    db_error(
                        "resolve_run_interaction_rollback_missing_request",
                        run_id,
                        source,
                    )
                    .to_string()
                })?;
                return Ok(DurableRunInteractionResolveOutcome::MissingRequest);
            }
            if run.status != STATUS_WAITING
                || run.waiting_for.as_deref() != Some(kind.waiting_for())
            {
                tx.rollback().await.map_err(|source| {
                    db_error(
                        "resolve_run_interaction_rollback_not_waiting",
                        run_id,
                        source,
                    )
                    .to_string()
                })?;
                return Ok(DurableRunInteractionResolveOutcome::NoLongerWaiting);
            }

            let first_event_idx = run.last_event_idx + 1;
            let event_rows = events
                .iter()
                .enumerate()
                .map(|(offset, event)| {
                    build_run_event_insert_row(
                        user_id,
                        run_id,
                        &run.session_id,
                        run.agent_id.as_deref(),
                        first_event_idx + offset as i64,
                        &self.owner_pod_id,
                        event,
                    )
                })
                .collect::<DbStoreResult<Vec<_>>>()
                .map_err(|error| error.to_string())?;
            let last_event_idx = first_event_idx + event_rows.len() as i64 - 1;
            let owner_lease_expires_at = self.lease_expires_at();
            let updated = sqlx::query(
                "UPDATE agent_runs
                 SET status = ?, waiting_for = NULL, last_event_idx = ?,
                     owner_pod_id = ?, owner_lease_expires_at = ?,
                     run_generation = run_generation + 1, updated_at = NOW(6)
                 WHERE user_id = ? AND run_id = ? AND status = ? AND waiting_for = ?
                   AND last_event_idx = ?",
            )
            .bind(STATUS_RUNNING)
            .bind(last_event_idx)
            .bind(&self.owner_pod_id)
            .bind(owner_lease_expires_at)
            .bind(user_id)
            .bind(run_id)
            .bind(STATUS_WAITING)
            .bind(kind.waiting_for())
            .bind(run.last_event_idx)
            .execute(&mut *tx)
            .await
            .map_err(|source| {
                db_error("resolve_run_interaction_update", run_id, source).to_string()
            })?;
            if updated.rows_affected() == 0 {
                tx.rollback().await.map_err(|source| {
                    db_error("resolve_run_interaction_rollback_conflict", run_id, source)
                        .to_string()
                })?;
                tokio::task::yield_now().await;
                continue;
            }
            if !self
                .sync_session_execution_slot_after_status_tx(&mut tx, &run, STATUS_RUNNING, None)
                .await
                .map_err(|error| error.to_string())?
            {
                tx.rollback().await.map_err(|source| {
                    db_error("resolve_run_interaction_rollback_slot", run_id, source).to_string()
                })?;
                return Ok(DurableRunInteractionResolveOutcome::NoLongerWaiting);
            }
            let mut insert = sqlx::QueryBuilder::<sqlx::MySql>::new(
                "INSERT INTO agent_run_events
                 (id, run_id, event_idx, user_id, session_id, event_type, event_id, agent_id,
                  subject_run_id, interaction_request_id, idempotency_key, event_hash,
                  producer_pod_id, payload_json, created_at) ",
            );
            insert.push_values(&event_rows, |mut row, event| {
                row.push_bind(&event.id)
                    .push_bind(&event.run_id)
                    .push_bind(event.event_idx)
                    .push_bind(&event.user_id)
                    .push_bind(&event.session_id)
                    .push_bind(&event.event_type)
                    .push_bind(&event.event_id)
                    .push_bind(&event.agent_id)
                    .push_bind(&event.subject_run_id)
                    .push_bind(&event.interaction_request_id)
                    .push_bind(&event.idempotency_key)
                    .push_bind(&event.event_hash)
                    .push_bind(&event.producer_pod_id)
                    .push_bind(&event.payload_json)
                    .push("NOW(6)");
            });
            insert.build().execute(&mut *tx).await.map_err(|source| {
                db_error("resolve_run_interaction_insert_events", run_id, source).to_string()
            })?;
            tx.commit().await.map_err(|source| {
                db_error("resolve_run_interaction_commit", run_id, source).to_string()
            })?;
            if let Err(error) = self
                .sync_projection_for_user(user_id, run_id, Some("run_resumed"), None)
                .await
            {
                tracing::warn!(
                    user_id,
                    run_id,
                    error = %error,
                    "interaction resolved but display projection refresh failed"
                );
            }
            return Ok(DurableRunInteractionResolveOutcome::Resolved(
                events[0].clone(),
            ));
        }

        if let Some(existing) = self
            .load_run_interaction_event(user_id, run_id, request_id, kind.resolved_event_type())
            .await?
        {
            return Ok(if interaction_response_matches(&existing, &response_data) {
                DurableRunInteractionResolveOutcome::Idempotent(existing)
            } else {
                DurableRunInteractionResolveOutcome::Conflict(existing)
            });
        }
        Ok(DurableRunInteractionResolveOutcome::NoLongerWaiting)
    }

    async fn list_user_runs_cursor(
        &self,
        user_id: &str,
        limit: u32,
        cursor: Option<RunListCursor>,
    ) -> Result<DurableRunListPage, String> {
        let limit = validate_run_list_limit(limit);
        let query_limit = run_list_query_limit(limit);
        let rows = if let Some(cursor) = cursor {
            let updated_at = run_list_cursor_db_updated_at(&cursor)?;
            let run_id = run_list_cursor_run_id(&cursor)?;
            let sql = format!(
                "SELECT {AGENT_RUN_COLUMNS}, {RUN_LIST_CURSOR_SELECT_SQL} FROM agent_runs \
                 WHERE user_id = ?{RUN_LIST_CURSOR_PREDICATE_SQL}\
                 {RUN_LIST_ORDER_SQL} LIMIT ?"
            );
            sqlx::query(&sql)
                .bind(user_id)
                .bind(updated_at.clone())
                .bind(updated_at)
                .bind(run_id)
                .bind(query_limit)
                .fetch_all(self.pool.get())
                .await
        } else {
            let sql = format!(
                "SELECT {AGENT_RUN_COLUMNS}, {RUN_LIST_CURSOR_SELECT_SQL} FROM agent_runs \
                 WHERE user_id = ?{RUN_LIST_ORDER_SQL} LIMIT ?"
            );
            sqlx::query(&sql)
                .bind(user_id)
                .bind(query_limit)
                .fetch_all(self.pool.get())
                .await
        }
        .map_err(|source| db_error("list_user_runs_cursor", user_id, source).to_string())?;

        let mut entries = Vec::with_capacity(rows.len());
        for row in rows {
            let cursor = run_list_cursor_from_row(&row).map_err(|e| e.to_string())?;
            let run = run_record_from_row(row).map_err(|e| e.to_string())?;
            entries.push((run, cursor));
        }
        let has_more = entries.len() > limit as usize;
        if has_more {
            entries.truncate(limit as usize);
        }
        let next_cursor = if has_more {
            entries.last().map(|(_, cursor)| cursor.clone())
        } else {
            None
        };
        let runs = entries.into_iter().map(|(run, _)| run).collect();
        Ok(DurableRunListPage {
            runs,
            total: None,
            next_cursor,
        })
    }

    async fn list_session_runs(
        &self,
        user_id: &str,
        session_id: &str,
        limit: u32,
    ) -> Result<DurableSessionRunPage, String> {
        let limit = validate_run_list_limit(limit);
        let sql = format!(
            "SELECT {AGENT_RUN_COLUMNS} FROM agent_runs \
             WHERE user_id = ? AND session_id = ? \
             ORDER BY CASE WHEN status IN (?, ?, ?, ?) THEN 1 ELSE 0 END ASC, \
                      updated_at DESC, created_at DESC, run_id DESC \
             LIMIT ?"
        );
        let rows = sqlx::query(&sql)
            .bind(user_id)
            .bind(session_id)
            .bind(STATUS_COMPLETED)
            .bind(STATUS_DELEGATED)
            .bind(STATUS_FAILED)
            .bind(STATUS_CANCELLED)
            .bind(session_run_query_limit(limit))
            .fetch_all(self.pool.get())
            .await
            .map_err(|source| db_error("list_session_runs", session_id, source).to_string())?;
        let mut runs = rows
            .into_iter()
            .map(run_record_from_row)
            .collect::<DbStoreResult<Vec<_>>>()
            .map_err(|error| error.to_string())?;
        let truncated = runs.len() > limit as usize;
        runs.truncate(limit as usize);

        // Preserve the ancestry required to interpret every selected node.
        // Fetch one parent level per round in a single batch; delegation depth
        // is bounded, and the visited set also makes malformed cycles finite.
        let mut included = runs
            .iter()
            .map(|run| run.run_id.clone())
            .collect::<HashSet<_>>();
        let mut frontier = runs
            .iter()
            .filter_map(|run| run.parent_run_id.clone())
            .filter(|run_id| !included.contains(run_id))
            .collect::<HashSet<_>>();
        while !frontier.is_empty() {
            let mut builder = sqlx::QueryBuilder::<sqlx::MySql>::new(format!(
                "SELECT {AGENT_RUN_COLUMNS} FROM agent_runs WHERE user_id = "
            ));
            builder
                .push_bind(user_id)
                .push(" AND session_id = ")
                .push_bind(session_id)
                .push(" AND run_id IN (");
            let mut separated = builder.separated(", ");
            for run_id in &frontier {
                separated.push_bind(run_id);
            }
            separated.push_unseparated(")");
            let parent_rows =
                builder
                    .build()
                    .fetch_all(self.pool.get())
                    .await
                    .map_err(|source| {
                        db_error("list_session_run_ancestors", session_id, source).to_string()
                    })?;
            let parents = parent_rows
                .into_iter()
                .map(run_record_from_row)
                .collect::<DbStoreResult<Vec<_>>>()
                .map_err(|error| error.to_string())?;
            frontier.clear();
            for parent in parents {
                if !included.insert(parent.run_id.clone()) {
                    continue;
                }
                if let Some(grandparent_id) = parent.parent_run_id.clone()
                    && !included.contains(&grandparent_id)
                {
                    frontier.insert(grandparent_id);
                }
                runs.push(parent);
            }
        }
        sort_session_run_tree(&mut runs);
        Ok(DurableSessionRunPage {
            runs,
            limit,
            truncated,
        })
    }

    async fn list_active_session_runs_cursor(
        &self,
        user_id: &str,
        session_id: &str,
        limit: u32,
        cursor: Option<RunListCursor>,
    ) -> Result<DurableRunListPage, String> {
        let limit = validate_run_list_limit(limit);
        let query_limit = run_list_query_limit(limit);
        let rows = if let Some(cursor) = cursor {
            let updated_at = run_list_cursor_db_updated_at(&cursor)?;
            let run_id = run_list_cursor_run_id(&cursor)?;
            let sql = format!(
                "SELECT {AGENT_RUN_COLUMNS}, {RUN_LIST_CURSOR_SELECT_SQL} FROM agent_runs \
                 WHERE user_id = ? AND session_id = ? AND status IN (?, ?, ?)\
                 {RUN_LIST_CURSOR_PREDICATE_SQL}\
                 {RUN_LIST_ORDER_SQL} LIMIT ?"
            );
            sqlx::query(&sql)
                .bind(user_id)
                .bind(session_id)
                .bind(STATUS_RUNNING)
                .bind(STATUS_WAITING)
                .bind(STATUS_PAUSED)
                .bind(updated_at.clone())
                .bind(updated_at)
                .bind(run_id)
                .bind(query_limit)
                .fetch_all(self.pool.get())
                .await
        } else {
            let sql = format!(
                "SELECT {AGENT_RUN_COLUMNS}, {RUN_LIST_CURSOR_SELECT_SQL} FROM agent_runs \
                 WHERE user_id = ? AND session_id = ? AND status IN (?, ?, ?)\
                 {RUN_LIST_ORDER_SQL} LIMIT ?"
            );
            sqlx::query(&sql)
                .bind(user_id)
                .bind(session_id)
                .bind(STATUS_RUNNING)
                .bind(STATUS_WAITING)
                .bind(STATUS_PAUSED)
                .bind(query_limit)
                .fetch_all(self.pool.get())
                .await
        }
        .map_err(|source| {
            db_error("list_active_session_runs_cursor", session_id, source).to_string()
        })?;

        let mut entries = Vec::with_capacity(rows.len());
        for row in rows {
            let cursor = run_list_cursor_from_row(&row).map_err(|error| error.to_string())?;
            let run = run_record_from_row(row).map_err(|error| error.to_string())?;
            entries.push((run, cursor));
        }
        let has_more = entries.len() > limit as usize;
        if has_more {
            entries.truncate(limit as usize);
        }
        let next_cursor = has_more
            .then(|| entries.last().map(|(_, cursor)| cursor.clone()))
            .flatten();
        Ok(DurableRunListPage {
            runs: entries.into_iter().map(|(run, _)| run).collect(),
            total: None,
            next_cursor,
        })
    }

    async fn load_session_agent_recovery(
        &self,
        user_id: &str,
        session_id: &str,
        limit: u32,
    ) -> Result<DurableSessionRunPage, String> {
        let mut page = self.list_session_runs(user_id, session_id, limit).await?;
        if page.runs.is_empty() {
            return Ok(page);
        }
        // Recovery needs one exact spawn envelope per selected child plus the
        // latest terminal facts per selected run. `event_idx` is run-local, so
        // a global ORDER BY/LIMIT lets one noisy run starve every other run.
        // Keep this as one bounded DB round trip, with the child identity
        // normalized in `subject_run_id` rather than parsed from JSON here.
        let mut builder = sqlx::QueryBuilder::<sqlx::MySql>::new(
            "SELECT run_id, event_idx, payload_json FROM (
               SELECT run_id, event_idx, payload_json FROM (
                 SELECT run_id, event_idx, payload_json,
                        ROW_NUMBER() OVER (
                          PARTITION BY subject_run_id ORDER BY event_idx DESC
                        ) AS recovery_rank
                 FROM agent_run_events
                 WHERE user_id = ",
        );
        builder
            .push_bind(user_id)
            .push(" AND session_id = ")
            .push_bind(session_id)
            .push(" AND event_type = 'agent_spawned' AND subject_run_id IN (");
        {
            let mut ids = builder.separated(",");
            for run in &page.runs {
                ids.push_bind(run.run_id.as_str());
            }
        }
        builder
            .push(
                ")
               ) ranked_spawns WHERE recovery_rank = 1
               UNION ALL
               SELECT run_id, event_idx, payload_json FROM (
                 SELECT run_id, event_idx, payload_json,
                        ROW_NUMBER() OVER (
                          PARTITION BY run_id, event_type ORDER BY event_idx DESC
                        ) AS recovery_rank
                 FROM agent_run_events
                 WHERE user_id = ",
            )
            .push_bind(user_id)
            .push(" AND session_id = ")
            .push_bind(session_id)
            .push(" AND event_type IN ('text_done','run_error','run_finished') AND run_id IN (");
        {
            let mut ids = builder.separated(",");
            for run in &page.runs {
                ids.push_bind(run.run_id.as_str());
            }
        }
        builder.push(
            ")
               ) ranked_terminal WHERE recovery_rank = 1
             ) recovery_events
             ORDER BY run_id, event_idx",
        );
        let rows = builder
            .build()
            .fetch_all(self.pool.get())
            .await
            .map_err(|source| {
                db_error("load_session_agent_recovery", session_id, source).to_string()
            })?;
        let mut events_by_run = HashMap::<String, Vec<(i64, serde_json::Value)>>::new();
        for row in rows {
            let run_id: String = row.try_get("run_id").map_err(|source| {
                db_error("decode_session_agent_recovery_run_id", session_id, source).to_string()
            })?;
            let event_idx: i64 = row.try_get("event_idx").map_err(|source| {
                db_error("decode_session_agent_recovery_event_idx", &run_id, source).to_string()
            })?;
            let payload: String = row.try_get("payload_json").map_err(|source| {
                db_error("decode_session_agent_recovery_payload", &run_id, source).to_string()
            })?;
            let payload = serde_json::from_str(&payload).map_err(|source| {
                format!("invalid durable recovery event for run {run_id}: {source}")
            })?;
            events_by_run
                .entry(run_id)
                .or_default()
                .push((event_idx, payload));
        }
        for run in &mut page.runs {
            if let Some(mut events) = events_by_run.remove(&run.run_id) {
                events.sort_by_key(|(event_idx, _)| *event_idx);
                run.events = events.into_iter().map(|(_, event)| event).collect();
            }
        }
        Ok(page)
    }

    async fn find_waiting_runs(&self) -> Result<Vec<DurableRunRecord>, String> {
        self.find_runs_by_status(STATUS_WAITING).await
    }

    async fn find_running_runs(&self) -> Result<Vec<DurableRunRecord>, String> {
        let sql = format!(
            "SELECT {AGENT_RUN_COLUMNS} FROM agent_runs \
             WHERE status = ? ORDER BY updated_at ASC"
        );
        let rows = sqlx::query(&sql)
            .bind(STATUS_RUNNING)
            .fetch_all(self.pool.get())
            .await
            .map_err(|source| db_error("find_running_runs", "active", source).to_string())?;
        rows.into_iter()
            .map(run_record_from_row)
            .collect::<DbStoreResult<Vec<_>>>()
            .map_err(|e| e.to_string())
    }

    async fn claim_recoverable_active_runs(
        &self,
        limit: u32,
    ) -> Result<Vec<DurableRunRecord>, String> {
        let limit = limit.clamp(1, MAX_RUN_RECOVERY_CLAIM_BATCH_SIZE);
        for _ in 0..RUN_RECOVERY_CLAIM_COLLISION_RETRIES {
            let candidate_sql = format!(
                "SELECT {AGENT_RUN_COLUMNS} FROM agent_runs
                 WHERE (status IN (?, ?) OR (status = ? AND waiting_for IS NOT NULL))
                   AND (
                       owner_pod_id IS NULL
                       OR owner_pod_id = ?
                       OR owner_lease_expires_at IS NULL
                       OR owner_lease_expires_at < NOW(6)
                   )
                 ORDER BY updated_at ASC, user_id ASC, run_id ASC
                 LIMIT ?",
            );
            let candidates = sqlx::query(&candidate_sql)
                .bind(STATUS_WAITING)
                .bind(STATUS_RUNNING)
                .bind(STATUS_PAUSED)
                .bind(&self.owner_pod_id)
                .bind(i64::from(limit))
                .fetch_all(self.pool.get())
                .await
                .map_err(|source| {
                    db_error("select_run_recovery_claim_candidates", "active", source).to_string()
                })?
                .into_iter()
                .map(run_record_from_row)
                .collect::<DbStoreResult<Vec<_>>>()
                .map_err(|error| error.to_string())?;
            if candidates.is_empty() {
                return Ok(Vec::new());
            }

            let claim_expires_at = self.lease_expires_at();
            let mut claim = sqlx::QueryBuilder::<sqlx::MySql>::new(
                "UPDATE agent_runs
                 SET owner_pod_id = ",
            );
            claim.push_bind(&self.owner_pod_id);
            claim.push(", owner_lease_expires_at = ");
            claim.push_bind(claim_expires_at);
            claim.push(
                ", run_generation = run_generation + 1, updated_at = NOW(6)
                 WHERE (status IN (",
            );
            claim.push_bind(STATUS_WAITING);
            claim.push(", ");
            claim.push_bind(STATUS_RUNNING);
            claim.push(") OR (status = ");
            claim.push_bind(STATUS_PAUSED);
            claim.push(
                " AND waiting_for IS NOT NULL))
                   AND (
                       owner_pod_id IS NULL
                       OR owner_pod_id = ",
            );
            claim.push_bind(&self.owner_pod_id);
            claim.push(
                " OR owner_lease_expires_at IS NULL
                       OR owner_lease_expires_at < NOW(6)
                   ) AND (",
            );
            for (index, candidate) in candidates.iter().enumerate() {
                if index > 0 {
                    claim.push(" OR ");
                }
                claim.push("(user_id = ");
                claim.push_bind(&candidate.user_id);
                claim.push(" AND run_id = ");
                claim.push_bind(&candidate.run_id);
                claim.push(" AND run_generation = ");
                claim.push_bind(candidate.run_generation as i64);
                claim.push(")");
            }
            claim.push(")");
            let claimed_count = claim
                .build()
                .execute(self.pool.get())
                .await
                .map_err(|source| {
                    db_error("claim_recoverable_active_runs", "active", source).to_string()
                })?
                .rows_affected();
            if claimed_count == 0 {
                tokio::task::yield_now().await;
                continue;
            }

            let mut claimed = sqlx::QueryBuilder::<sqlx::MySql>::new("SELECT ");
            claimed.push(AGENT_RUN_COLUMNS);
            claimed.push(" FROM agent_runs WHERE owner_pod_id = ");
            claimed.push_bind(&self.owner_pod_id);
            claimed.push(" AND owner_lease_expires_at = ");
            claimed.push_bind(claim_expires_at);
            claimed.push(" AND (");
            for (index, candidate) in candidates.iter().enumerate() {
                if index > 0 {
                    claimed.push(" OR ");
                }
                claimed.push("(user_id = ");
                claimed.push_bind(&candidate.user_id);
                claimed.push(" AND run_id = ");
                claimed.push_bind(&candidate.run_id);
                claimed.push(" AND run_generation = ");
                claimed.push_bind(candidate.run_generation.saturating_add(1) as i64);
                claimed.push(")");
            }
            claimed.push(") ORDER BY updated_at ASC, user_id ASC, run_id ASC");
            let rows = claimed
                .build()
                .fetch_all(self.pool.get())
                .await
                .map_err(|source| {
                    db_error("load_claimed_recoverable_active_runs", "active", source).to_string()
                })?;
            let records = rows
                .into_iter()
                .map(run_record_from_row)
                .collect::<DbStoreResult<Vec<_>>>()
                .map_err(|error| error.to_string())?;
            if !records.is_empty() {
                return Ok(records);
            }
            tokio::task::yield_now().await;
        }
        Ok(Vec::new())
    }

    fn owner_lease_renewal_interval(&self) -> Option<Duration> {
        Some(run_owner_lease_renewal_interval(self.lease_ttl))
    }

    async fn renew_owner_lease(
        &self,
        user_id: &str,
        run_id: &str,
        expected_statuses: &[&str],
    ) -> Result<bool, String> {
        if expected_statuses.is_empty() {
            return Ok(false);
        }

        let mut query =
            sqlx::QueryBuilder::<sqlx::MySql>::new("UPDATE agent_runs SET owner_pod_id = ");
        query.push_bind(&self.owner_pod_id);
        query.push(", owner_lease_expires_at = ");
        query.push_bind(self.lease_expires_at());
        query.push(" WHERE user_id = ");
        query.push_bind(user_id);
        query.push(" AND run_id = ");
        query.push_bind(run_id);
        query.push(" AND owner_pod_id = ");
        query.push_bind(&self.owner_pod_id);
        query.push(" AND status IN (");
        let mut separated = query.separated(", ");
        for expected in expected_statuses {
            separated.push_bind(*expected);
        }
        separated.push_unseparated(")");

        let result = query
            .build()
            .execute(self.pool.get())
            .await
            .map_err(|source| db_error("renew_owner_lease", run_id, source).to_string())?;
        Ok(result.rows_affected() > 0)
    }

    async fn release_owner_lease(&self, user_id: &str, run_id: &str) -> Result<bool, String> {
        let result = sqlx::query(
            "UPDATE agent_runs
             SET owner_pod_id = NULL, owner_lease_expires_at = NULL
             WHERE user_id = ? AND run_id = ? AND owner_pod_id = ?",
        )
        .bind(user_id)
        .bind(run_id)
        .bind(&self.owner_pod_id)
        .execute(self.pool.get())
        .await
        .map_err(|source| db_error("release_owner_lease", run_id, source).to_string())?;
        Ok(result.rows_affected() > 0)
    }

    async fn find_blocking_session_run(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> Result<Option<DurableRunRecord>, String> {
        let sql = format!(
            "SELECT {AGENT_RUN_COLUMNS} FROM agent_runs \
             WHERE user_id = ? AND session_id = ? \
               AND (status IN (?, ?) OR (status = ? AND waiting_for IS NOT NULL)) \
             ORDER BY updated_at DESC \
             LIMIT 1",
        );
        let row = sqlx::query(&sql)
            .bind(user_id)
            .bind(session_id)
            .bind(STATUS_RUNNING)
            .bind(STATUS_WAITING)
            .bind(STATUS_PAUSED)
            .fetch_optional(self.pool.get())
            .await
            .map_err(|source| {
                db_error("find_blocking_session_run", session_id, source).to_string()
            })?;
        row.map(run_record_from_row)
            .transpose()
            .map_err(|e| e.to_string())
    }

    async fn find_sub_runs(
        &self,
        user_id: &str,
        delegation_id: &str,
    ) -> Result<Vec<DurableRunRecord>, String> {
        let sql = format!(
            "SELECT {AGENT_RUN_COLUMNS} FROM agent_runs \
             WHERE user_id = ? AND delegation_id = ? ORDER BY depth ASC, created_at ASC"
        );
        let rows = sqlx::query(&sql)
            .bind(user_id)
            .bind(delegation_id)
            .fetch_all(self.pool.get())
            .await
            .map_err(|source| db_error("find_sub_runs", delegation_id, source).to_string())?;
        rows.into_iter()
            .map(run_record_from_row)
            .collect::<DbStoreResult<Vec<_>>>()
            .map_err(|e| e.to_string())
    }

    async fn update_retry_count(
        &self,
        user_id: &str,
        run_id: &str,
        retry_count: u32,
    ) -> Result<bool, String> {
        let result = sqlx::query(
            "UPDATE agent_runs
             SET retry_count = ?, updated_at = NOW(6)
             WHERE user_id = ? AND run_id = ?",
        )
        .bind(retry_count as i64)
        .bind(user_id)
        .bind(run_id)
        .execute(self.pool.get())
        .await
        .map_err(|source| db_error("update_retry_count", run_id, source).to_string())?;
        Ok(result.rows_affected() > 0)
    }
}

impl DatabaseRunStateStore {
    async fn find_runs_by_status(&self, status: &str) -> Result<Vec<DurableRunRecord>, String> {
        let sql = format!(
            "SELECT {AGENT_RUN_COLUMNS} FROM agent_runs WHERE status = ? ORDER BY updated_at ASC"
        );
        let rows = sqlx::query(&sql)
            .bind(status)
            .fetch_all(self.pool.get())
            .await
            .map_err(|source| db_error("find_runs_by_status", status, source).to_string())?;
        rows.into_iter()
            .map(run_record_from_row)
            .collect::<DbStoreResult<Vec<_>>>()
            .map_err(|e| e.to_string())
    }
}

fn db_error(
    operation: &'static str,
    entity: impl Into<String>,
    source: sqlx::Error,
) -> DatabaseRunStateStoreError {
    DatabaseRunStateStoreError::Database {
        operation,
        entity: entity.into(),
        source,
    }
}

fn db_decode_error(
    operation: &'static str,
    table: &str,
    column: &str,
    source: sqlx::Error,
) -> DatabaseRunStateStoreError {
    db_error(operation, format!("{table}.{column}"), source)
}

fn invalid_database_value_error(
    operation: &'static str,
    table: &str,
    column: &str,
    message: impl Into<String>,
) -> DatabaseRunStateStoreError {
    let source = sqlx::Error::Decode(Box::new(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        message.into(),
    )));
    db_decode_error(operation, table, column, source)
}

fn run_row_string(
    row: &impl RunStateDbRow,
    operation: &'static str,
    table: &str,
    column: &str,
) -> DbStoreResult<String> {
    row.string_column(column)
        .map_err(|source| db_decode_error(operation, table, column, source))
}

fn run_row_optional_string(
    row: &impl RunStateDbRow,
    operation: &'static str,
    table: &str,
    column: &str,
) -> DbStoreResult<Option<String>> {
    row.optional_string_column(column)
        .map_err(|source| db_decode_error(operation, table, column, source))
}

fn run_row_datetime_string(
    row: &impl RunStateDbRow,
    operation: &'static str,
    table: &str,
    column: &str,
) -> DbStoreResult<String> {
    row.datetime_string_column(column)
        .map_err(|source| db_decode_error(operation, table, column, source))
}

fn run_row_optional_datetime_string(
    row: &impl RunStateDbRow,
    operation: &'static str,
    table: &str,
    column: &str,
) -> DbStoreResult<Option<String>> {
    row.optional_datetime_string_column(column)
        .map_err(|source| db_decode_error(operation, table, column, source))
}

fn run_row_i64(
    row: &impl RunStateDbRow,
    operation: &'static str,
    table: &str,
    column: &str,
) -> DbStoreResult<i64> {
    row.i64_column(column)
        .map_err(|source| db_decode_error(operation, table, column, source))
}

fn run_row_at_least_i64(
    row: &impl RunStateDbRow,
    operation: &'static str,
    table: &str,
    column: &str,
    min: i64,
) -> DbStoreResult<i64> {
    let value = run_row_i64(row, operation, table, column)?;
    if value < min {
        return Err(invalid_database_value_error(
            operation,
            table,
            column,
            format!("invalid {table}.{column}: {value}; expected >= {min}"),
        ));
    }
    Ok(value)
}

fn run_row_non_negative_i64(
    row: &impl RunStateDbRow,
    operation: &'static str,
    table: &str,
    column: &str,
) -> DbStoreResult<i64> {
    run_row_at_least_i64(row, operation, table, column, 0)
}

fn run_row_u64(
    row: &impl RunStateDbRow,
    operation: &'static str,
    table: &str,
    column: &str,
) -> DbStoreResult<u64> {
    Ok(run_row_non_negative_i64(row, operation, table, column)? as u64)
}

fn run_row_u32(
    row: &impl RunStateDbRow,
    operation: &'static str,
    table: &str,
    column: &str,
) -> DbStoreResult<u32> {
    let value = run_row_non_negative_i64(row, operation, table, column)?;
    u32::try_from(value).map_err(|_| {
        invalid_database_value_error(
            operation,
            table,
            column,
            format!(
                "invalid {table}.{column}: {value}; expected <= {}",
                u32::MAX
            ),
        )
    })
}

fn decode_tool_preview_contract_row(
    row: &impl RunStateDbRow,
) -> DbStoreResult<(String, ToolPreviewContract)> {
    let operation = "decode_tool_preview_contract_row";
    let table = "preview_template_registry";
    let tool_name = run_row_string(row, operation, table, "tool_name")?;
    let max_preview_bytes = run_row_at_least_i64(row, operation, table, "max_preview_bytes", 1)?;
    let max_preview_bytes = usize::try_from(max_preview_bytes).map_err(|_| {
        invalid_database_value_error(
            operation,
            table,
            "max_preview_bytes",
            format!(
                "invalid {table}.max_preview_bytes: {max_preview_bytes}; expected <= {}",
                usize::MAX
            ),
        )
    })?;
    let normalize_version = run_row_string(row, operation, table, "normalize_version")?;
    Ok((
        tool_name,
        ToolPreviewContract {
            max_preview_bytes,
            normalize_version,
            found: true,
        },
    ))
}

fn default_owner_pod_id() -> String {
    std::env::var("ASTRA_POD_ID")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| format!("astra-runtime-{}", Uuid::new_v4()))
}

fn validate_retry_scope(run_id: &str, retry_scope: &str) -> DbStoreResult<()> {
    match retry_scope {
        "node" | "subtree" | "siblings" => Ok(()),
        other => Err(DatabaseRunStateStoreError::InvalidRetryScope {
            run_id: run_id.to_string(),
            retry_scope: other.to_string(),
        }),
    }
}

fn build_tool_output_preview_row(
    session_id: &str,
    item: &ToolOutputBatchItem,
    payload: &str,
    contract: &ToolPreviewContract,
) -> ToolOutputPreviewRow {
    let content_hash = format!("sha256:{}", sha256_hex(payload.as_bytes()));
    let preview_source = tool_output_preview_source(&item.result, payload);
    let preview_text = truncate_utf8_bytes(preview_source.as_ref(), contract.max_preview_bytes);
    let explicit_artifact_ref = tool_result_artifact_ref(&item.result);
    let large_payload_ref = (payload.len() > contract.max_preview_bytes).then(|| {
        format!(
            "tool_output://{session_id}/{}@{}",
            item.output_id, content_hash
        )
    });
    let preview_status = if !contract.found {
        "fallback"
    } else if preview_source.len() > contract.max_preview_bytes {
        "truncated"
    } else {
        "template"
    }
    .to_string();
    ToolOutputPreviewRow {
        payload: payload.to_string(),
        preview_text,
        preview_status,
        artifact_ref: explicit_artifact_ref.or(large_payload_ref),
        content_hash,
        normalize_version: contract.normalize_version.clone(),
        parent_output_id: item
            .result
            .metadata
            .get("parent_output_id")
            .and_then(serde_json::Value::as_str)
            .map(ToString::to_string),
    }
}

fn tool_output_preview_source<'a>(
    result: &'a ToolInvocationResultPayload,
    payload: &'a str,
) -> Cow<'a, str> {
    let output = result.output.trim();
    if !output.is_empty() {
        return Cow::Borrowed(output);
    }
    Cow::Borrowed(payload)
}

fn tool_result_artifact_ref(result: &ToolInvocationResultPayload) -> Option<String> {
    result
        .metadata
        .get(TOOL_INVOCATION_RESULT_ARTIFACT_METADATA_KEY)
        .and_then(|reference| {
            reference
                .as_str()
                .map(ToString::to_string)
                .or_else(|| extract_optional_string(reference, "artifactUri"))
                .or_else(|| extract_optional_string(reference, "artifactId"))
        })
        .or_else(|| {
            result
                .metadata
                .get("artifact_ref")
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string)
        })
        .or_else(|| {
            result
                .metadata
                .get("artifact_uri")
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string)
        })
}

fn truncate_utf8_bytes(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes.min(value.len());
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    value[..end].to_string()
}

fn run_record_from_row(row: sqlx::mysql::MySqlRow) -> DbStoreResult<DurableRunRecord> {
    decode_run_record_from_row(&row)
}

fn decode_run_control_row(
    row: &sqlx::mysql::MySqlRow,
    entity: &str,
) -> Result<DurableRunControlRecord, String> {
    Ok(DurableRunControlRecord {
        run_id: row
            .try_get("run_id")
            .map_err(|source| db_error("decode_run_control_run_id", entity, source).to_string())?,
        status: row
            .try_get("status")
            .map_err(|source| db_error("decode_run_control_status", entity, source).to_string())?,
        waiting_for: row.try_get("waiting_for").map_err(|source| {
            db_error("decode_run_control_waiting_for", entity, source).to_string()
        })?,
        parent_run_id: row
            .try_get("parent_run_id")
            .map_err(|source| db_error("decode_run_control_parent", entity, source).to_string())?,
        ancestor_path: row.try_get("ancestor_path").map_err(|source| {
            db_error("decode_run_control_ancestor_path", entity, source).to_string()
        })?,
    })
}

fn run_list_cursor_from_row(row: &impl RunStateDbRow) -> DbStoreResult<RunListCursor> {
    let operation = "list_user_runs_cursor";
    let table = "agent_runs";
    let updated_at = run_row_string(row, operation, table, "cursor_updated_at")?;
    if updated_at.trim().is_empty() {
        return Err(invalid_database_value_error(
            operation,
            table,
            "cursor_updated_at",
            "expected non-empty run list cursor timestamp",
        ));
    }
    let run_id = run_row_string(row, operation, table, "run_id")?;
    if run_id.trim().is_empty() {
        return Err(invalid_database_value_error(
            operation,
            table,
            "run_id",
            "expected non-empty run list cursor run_id",
        ));
    }
    Ok(RunListCursor { updated_at, run_id })
}

fn decode_run_record_from_row(row: &impl RunStateDbRow) -> DbStoreResult<DurableRunRecord> {
    let operation = "decode_run_row";
    let table = "agent_runs";
    let run_id = run_row_string(row, operation, table, "run_id")?;
    Ok(DurableRunRecord {
        user_id: run_row_string(row, operation, table, "user_id")?,
        session_id: run_row_string(row, operation, table, "session_id")?,
        parent_run_id: run_row_optional_string(row, operation, table, "parent_run_id")?,
        root_run_id: run_row_optional_string(row, operation, table, "root_run_id")?,
        ancestor_path: run_row_optional_string(row, operation, table, "ancestor_path")?,
        depth: run_row_u32(row, operation, table, "depth")?,
        delegation_id: run_row_optional_string(row, operation, table, "delegation_id")?,
        agent_id: run_row_optional_string(row, operation, table, "agent_id")?,
        retry_of: run_row_optional_string(row, operation, table, "retry_of")?,
        retry_scope: run_row_optional_string(row, operation, table, "retry_scope")?,
        status: run_row_string(row, operation, table, "status")?,
        waiting_for: run_row_optional_string(row, operation, table, "waiting_for")?,
        owner_pod_id: run_row_optional_string(row, operation, table, "owner_pod_id")?,
        owner_lease_expires_at: run_row_optional_datetime_string(
            row,
            operation,
            table,
            "owner_lease_expires_at",
        )?,
        run_generation: run_row_u64(row, operation, table, "run_generation")?,
        last_event_idx: run_row_at_least_i64(row, operation, table, "last_event_idx", -1)?,
        checkpoint_version: run_row_optional_string(row, operation, table, "checkpoint_version")?,
        checkpoint_json: run_row_optional_string(row, operation, table, "checkpoint_json")?,
        error_code: run_row_optional_string(row, operation, table, "error_code")?,
        error_message: run_row_optional_string(row, operation, table, "error_message")?,
        retry_count: run_row_u32(row, operation, table, "retry_count")?,
        total_prompt_tokens: run_row_u64(row, operation, table, "total_prompt_tokens")?,
        total_completion_tokens: run_row_u64(row, operation, table, "total_completion_tokens")?,
        total_tool_calls: run_row_u32(row, operation, table, "total_tool_calls")?,
        agent_binding_id: run_row_optional_string(row, operation, table, "agent_binding_id")?,
        agent_binding_name: run_row_optional_string(row, operation, table, "agent_binding_name")?,
        agent_binding_schema_version: run_row_optional_string(
            row,
            operation,
            table,
            "agent_binding_schema_version",
        )?,
        model_offering_id: run_row_optional_string(row, operation, table, "model_offering_id")?,
        resolved_model_name: run_row_optional_string(row, operation, table, "resolved_model_name")?,
        capability_server_refs_json: run_row_optional_string(
            row,
            operation,
            table,
            "capability_server_refs_json",
        )?,
        runtime_profile: run_row_optional_string(row, operation, table, "runtime_profile")?,
        events: Vec::new(),
        created_at: run_row_datetime_string(row, operation, table, "created_at")?,
        updated_at: run_row_datetime_string(row, operation, table, "updated_at")?,
        run_id,
    })
}

fn run_projection_record_from_row(
    row: sqlx::mysql::MySqlRow,
) -> DbStoreResult<DurableRunDisplayProjectionRecord> {
    decode_run_display_projection_record_from_row(&row)
}

fn decode_run_display_projection_record_from_row(
    row: &impl RunStateDbRow,
) -> DbStoreResult<DurableRunDisplayProjectionRecord> {
    let operation = "decode_run_projection_row";
    let table = "run_display_projections";
    let run_id = run_row_string(row, operation, table, "run_id")?;
    Ok(DurableRunDisplayProjectionRecord {
        run_id,
        user_id: run_row_string(row, operation, table, "user_id")?,
        session_id: run_row_string(row, operation, table, "session_id")?,
        status: run_row_string(row, operation, table, "status")?,
        waiting_for: run_row_optional_string(row, operation, table, "waiting_for")?,
        error_message: run_row_optional_string(row, operation, table, "error_message")?,
        projection_event_idx: run_row_at_least_i64(
            row,
            operation,
            table,
            "projection_event_idx",
            -1,
        )?,
        latest_event_type: run_row_optional_string(row, operation, table, "latest_event_type")?,
        latest_checkpoint_id: run_row_optional_string(
            row,
            operation,
            table,
            "latest_checkpoint_id",
        )?,
        latest_checkpoint_kind: run_row_optional_string(
            row,
            operation,
            table,
            "latest_checkpoint_kind",
        )?,
        latest_checkpoint_version: run_row_optional_string(
            row,
            operation,
            table,
            "latest_checkpoint_version",
        )?,
        total_prompt_tokens: run_row_u64(row, operation, table, "total_prompt_tokens")?,
        total_completion_tokens: run_row_u64(row, operation, table, "total_completion_tokens")?,
        total_tool_calls: run_row_u32(row, operation, table, "total_tool_calls")?,
        projection_hash: run_row_string(row, operation, table, "projection_hash")?,
        updated_at: run_row_datetime_string(row, operation, table, "updated_at")?,
    })
}

fn decode_run_checkpoint_record_from_row(
    row: &impl RunStateDbRow,
) -> DbStoreResult<DurableRunCheckpointRecord> {
    let operation = "decode_run_checkpoint_row";
    let table = "run_checkpoints";
    Ok(DurableRunCheckpointRecord {
        checkpoint_id: run_row_string(row, operation, table, "checkpoint_id")?,
        run_id: run_row_string(row, operation, table, "run_id")?,
        user_id: run_row_string(row, operation, table, "user_id")?,
        session_id: run_row_string(row, operation, table, "session_id")?,
        node_seq: run_row_non_negative_i64(row, operation, table, "node_seq")?,
        checkpoint_kind: run_row_string(row, operation, table, "checkpoint_kind")?,
        checkpoint_version: run_row_string(row, operation, table, "checkpoint_version")?,
        idempotency_key: run_row_string(row, operation, table, "idempotency_key")?,
        checkpoint_json: run_row_string(row, operation, table, "checkpoint_json")?,
        created_at: run_row_datetime_string(row, operation, table, "created_at")?,
    })
}

fn decode_run_event_payload(
    row: &impl RunStateDbRow,
    run_id: &str,
) -> DbStoreResult<serde_json::Value> {
    let operation = "decode_run_event_row";
    let table = "agent_run_events";
    let payload = run_row_string(row, operation, table, "payload_json")?;
    let event_idx = run_row_non_negative_i64(row, operation, table, "event_idx")?;
    let mut value = serde_json::from_str::<serde_json::Value>(&payload).map_err(|source| {
        DatabaseRunStateStoreError::Json {
            operation: "decode_run_event_payload",
            entity: format!("{table}.payload_json:{run_id}:{event_idx}"),
            source,
        }
    })?;
    if let Some(obj) = value.as_object_mut() {
        // The row key is authoritative. Payload-provided indices are stream
        // metadata and must never override durable cursor/ack identity.
        obj.insert("index".to_string(), serde_json::json!(event_idx));
    }
    Ok(value)
}

pub fn extract_event_type(event: &serde_json::Value) -> String {
    extract_optional_string(event, "event_type")
        .or_else(|| extract_optional_string(event, "type"))
        .unwrap_or_else(|| "unknown".to_string())
}

fn extract_optional_string(event: &serde_json::Value, key: &str) -> Option<String> {
    event
        .get(key)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
}

fn extract_interaction_request_id(event: &serde_json::Value) -> Option<String> {
    extract_optional_string(event, "request_id").or_else(|| {
        event
            .get("data")
            .and_then(|data| extract_optional_string(data, "request_id"))
    })
}

fn interaction_idempotency_key(
    kind: DurableRunInteractionKind,
    request_id: &str,
    suffix: &str,
) -> String {
    let identity = sha256_hex(request_id.as_bytes());
    format!(
        "interaction:{}:{identity}:{suffix}",
        kind.idempotency_namespace()
    )
}

fn interaction_resolution_events(
    kind: DurableRunInteractionKind,
    request_id: &str,
    response_data: serde_json::Value,
) -> [serde_json::Value; 2] {
    let outcome = response_data
        .get("outcome")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("resolved");
    [
        serde_json::json!({
            "event_type": kind.resolved_event_type(),
            "idempotency_key": interaction_idempotency_key(kind, request_id, "terminal"),
            "data": response_data,
        }),
        serde_json::json!({
            "event_type": "run_resumed",
            "idempotency_key": interaction_idempotency_key(kind, request_id, "resume"),
            "data": {
                "reason": kind.waiting_for(),
                "request_id": request_id,
                "interaction_outcome": outcome,
            }
        }),
    ]
}

fn interaction_response_matches(
    existing: &serde_json::Value,
    response_data: &serde_json::Value,
) -> bool {
    existing.get("data") == Some(response_data)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("{digest:x}")
}

#[derive(Debug)]
struct RunEventInsertRow {
    id: String,
    run_id: String,
    event_idx: i64,
    user_id: String,
    session_id: String,
    event_type: String,
    event_id: String,
    agent_id: String,
    subject_run_id: Option<String>,
    interaction_request_id: Option<String>,
    idempotency_key: Option<String>,
    event_hash: String,
    producer_pod_id: String,
    payload_json: String,
}

fn build_run_event_insert_row(
    user_id: &str,
    run_id: &str,
    session_id: &str,
    agent_id: Option<&str>,
    event_idx: i64,
    owner_pod_id: &str,
    event: &serde_json::Value,
) -> DbStoreResult<RunEventInsertRow> {
    let payload_json =
        serde_json::to_string(event).map_err(|source| DatabaseRunStateStoreError::Json {
            operation: "serialize_run_event",
            entity: run_id.to_string(),
            source,
        })?;
    let event_type = extract_event_type(event);
    let subject_run_id = (event_type == "agent_spawned")
        .then(|| extract_optional_string(event, "run_id"))
        .flatten();
    let interaction_request_id = extract_interaction_request_id(event);
    let event_id = extract_optional_string(event, "event_id")
        .or_else(|| extract_optional_string(event, "id"))
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let event_hash = sha256_hex(payload_json.as_bytes());
    let idempotency_key = extract_optional_string(event, "idempotency_key");

    Ok(RunEventInsertRow {
        id: Uuid::new_v4().to_string(),
        run_id: run_id.to_string(),
        event_idx,
        user_id: user_id.to_string(),
        session_id: session_id.to_string(),
        event_type,
        event_id,
        agent_id: agent_id.unwrap_or_default().to_string(),
        subject_run_id,
        interaction_request_id,
        idempotency_key,
        event_hash,
        producer_pod_id: owner_pod_id.to_string(),
        payload_json,
    })
}

/// Known client-facing event `type` values that are safe to surface
/// on the external `/chat/turn` SSE stream. Anything outside this
/// allowlist is dropped — it's either an internal diagnostic event
/// (e.g. `injection_freshness`) whose stability no external
/// consumer should depend on, or a future event that hasn't been
/// explicitly opted into the public surface.
///
/// wip-7 motivation: the pre-allowlist code path pass-through'd any
/// `{"type": ...}`-shaped event unchanged. wip-5's
/// `injection_freshness` event carried raw channel text for
/// observation purposes — that text leaked to any authenticated
/// `/chat/turn` client. The wip-7 bridge emits fingerprints only,
/// but the allowlist is the defence-in-depth so a future diagnostic
/// event can't accidentally leak either.
///
/// Adding a new public event: add its `type` string here AND, if the
/// event also arrives in `{"event_type": ..., "data": ...}` shape
/// from the journal, add a dedicated `match` arm below so the
/// transform shapes it consistently.
const EXTERNAL_CLIENT_ALLOWLIST: &[&str] = &[
    // Streaming assistant content.
    "text_delta",
    "text_done",
    "thinking_delta",
    "thinking_done",
    "reasoning_delta",
    "reasoning_done",
    "reasoning_message_content",
    // Tool-call lifecycle.
    "tool_call",
    "tool_call_start",
    "tool_call_end",
    "tool_request",
    "approval_required",
    "approval_batch_required",
    "user_prompt_required",
    // Run lifecycle + framing.
    "run_started",
    "run_error",
    "run_interrupted",
    "run_finished",
    "run_waiting",
    "run_blocked",
    "run_paused",
    "run_resumed",
    "runtime.control.handoff.requested",
    "runtime.control.handoff.rejected",
    "user_intent_accepted",
    "user_intent_applied",
    "context_meta",
    "session_info",
    "turn_complete",
    "turn_done",
    "user_input",
    "usage",
    "explain",
    "error",
    "ping",
    // Work-surface execution binding and transport lifecycle.
    "workspace_bound",
    "executor_bound",
    "executor_status_changed",
    "tool_routing_decision",
    "tool_transport_started",
    "tool_transport_completed",
    "tool_transport_failed",
    // Plan / delegation surface (public admin features).
    "plan_created",
    "plan_step_start",
    "plan_step_done",
    "plan_revised",
    "agent_delegated",
    "agent_communication",
    "agent_spawned",
    "agent_live_event",
    "agent_progress",
    "agent_completed",
    "agent_failed",
    "agent_waiting",
    "agent_cancelled",
    "agent_interrupted",
    "task_board_snapshot",
];

fn insert_if_present(
    out: &mut serde_json::Map<String, serde_json::Value>,
    data: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) {
    if let Some(value) = data.get(key).cloned() {
        out.insert(key.to_string(), value);
    }
}

fn copy_execution_boundary_fields(
    out: &mut serde_json::Map<String, serde_json::Value>,
    data: &serde_json::Map<String, serde_json::Value>,
) {
    for key in [
        "workspace",
        "executor",
        "transport",
        "route",
        "success",
        "duration_ms",
        "error_kind",
        "reason",
        "blocked",
    ] {
        insert_if_present(out, data, key);
    }
}

fn is_external_client_event_type(event_type: &str) -> bool {
    EXTERNAL_CLIENT_ALLOWLIST.contains(&event_type)
}

fn run_error_client_surface(error_kind: &str) -> Option<(&'static str, bool, Option<u64>)> {
    match error_kind {
        "rate_limit" => Some(("LLM_RATE_LIMIT", true, Some(5_000))),
        "server_error" => Some(("SERVER_ERROR", true, Some(2_000))),
        "stream_idle" | "tool_timeout" => Some(("LLM_TIMEOUT", true, Some(0))),
        "stream_transport" => Some(("LLM_TRANSPORT_ERROR", true, Some(1_000))),
        "network" => Some(("LLM_TRANSPORT_ERROR", true, Some(3_000))),
        "budget_exhausted" => Some(("BUDGET_EXCEEDED", false, None)),
        "auth" => Some(("AUTH_ERROR", false, None)),
        "context_window" => Some(("CONTEXT_WINDOW_EXCEEDED", false, None)),
        "invalid_request" => Some(("LLM_INVALID_REQUEST", false, None)),
        "cancelled" => Some(("CANCELLED", false, None)),
        _ => None,
    }
}

fn data_string(data: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<String> {
    data.get(key)
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
}

fn run_error_code_from_data(data: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    data_string(data, "error_code").or_else(|| data_string(data, "error_kind"))
}

fn run_error_code_from_event(event: &serde_json::Value) -> Option<String> {
    let event_type = event
        .get("event_type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if event_type != "run_error" && event_type != "run_finished" {
        return None;
    }
    event
        .get("data")
        .and_then(serde_json::Value::as_object)
        .or_else(|| event.as_object())
        .and_then(run_error_code_from_data)
}

fn terminal_error_code_from_events(status: &str, events: &[serde_json::Value]) -> Option<String> {
    if status != STATUS_FAILED {
        return None;
    }
    events.iter().rev().find_map(run_error_code_from_event)
}

fn terminal_error_code_from_message(status: &str, error_message: Option<&str>) -> Option<String> {
    if status != STATUS_FAILED {
        return None;
    }
    let message = error_message?.trim();
    if message.is_empty() {
        return None;
    }
    Some(
        astra_core::ClassifiedError::from(message.to_string())
            .kind
            .as_str()
            .to_string(),
    )
}

fn terminal_error_code_from_transition(
    status: &str,
    error_message: Option<&str>,
    events: &[serde_json::Value],
) -> Option<String> {
    terminal_error_code_from_events(status, events)
        .or_else(|| terminal_error_code_from_message(status, error_message))
}

pub fn transform_run_event_for_client(event: serde_json::Value) -> serde_json::Value {
    // Already-client-ready shape (`{"type": ..., ...}`): allowlist the
    // `type` value. Drop anything not explicitly safe for external
    // consumption.
    if event
        .get("event_type")
        .and_then(serde_json::Value::as_str)
        .is_none()
        && let Some(client_type) = event.get("type").and_then(serde_json::Value::as_str)
    {
        if is_external_client_event_type(client_type) {
            return event;
        }
        // Unknown / internal event — drop.
        return serde_json::Value::Null;
    }

    let event_type = event
        .get("event_type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let data = event
        .get("data")
        .and_then(serde_json::Value::as_object)
        .cloned()
        .unwrap_or_default();

    match event_type {
        "text_delta" => serde_json::json!({
            "type": "text_delta",
            "content": data.get("chunk").cloned().unwrap_or(serde_json::Value::String(String::new())),
        }),
        "assistant_delta" => serde_json::json!({
            "type": "text_delta",
            "content": data
                .get("text")
                .or_else(|| data.get("chunk"))
                .cloned()
                .unwrap_or(serde_json::Value::String(String::new())),
        }),
        "text_done" => serde_json::json!({
            "type": "text_done",
            "full_text": data.get("full_text").cloned().unwrap_or(serde_json::Value::String(String::new())),
        }),
        "reasoning_message_content" => serde_json::json!({
            "type": "reasoning_message_content",
            "content": data.get("content").cloned().unwrap_or(serde_json::Value::String(String::new())),
        }),
        "thinking_delta" => serde_json::json!({
            "type": "thinking_delta",
            "content": data.get("chunk").cloned().unwrap_or(serde_json::Value::String(String::new())),
        }),
        "thinking_done" | "reasoning_done" => serde_json::json!({ "type": event_type }),
        "tool_call_start" => {
            let mut out = serde_json::Map::from_iter([(
                "type".to_string(),
                serde_json::Value::String("tool_call_start".to_string()),
            )]);
            out.insert(
                "tool".to_string(),
                data.get("tool")
                    .or_else(|| data.get("name"))
                    .cloned()
                    .unwrap_or(serde_json::Value::String(String::new())),
            );
            out.insert(
                "call_id".to_string(),
                data.get("call_id")
                    .or_else(|| data.get("tool_call_id"))
                    .cloned()
                    .unwrap_or(serde_json::Value::String(String::new())),
            );
            out.insert(
                "arguments".to_string(),
                data.get("arguments")
                    .or_else(|| data.get("args"))
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
            );
            copy_execution_boundary_fields(&mut out, &data);
            serde_json::Value::Object(out)
        }
        "tool_result" => {
            let mut out = serde_json::Map::from_iter([(
                "type".to_string(),
                serde_json::Value::String("tool_call_end".to_string()),
            )]);
            out.insert(
                "call_id".to_string(),
                data.get("call_id")
                    .or_else(|| data.get("tool_call_id"))
                    .cloned()
                    .unwrap_or(serde_json::Value::String(String::new())),
            );
            if let Some(tool) = data.get("tool").or_else(|| data.get("name")).cloned() {
                out.insert("tool".to_string(), tool);
            }
            out.insert(
                "result".to_string(),
                data.get("result")
                    .or_else(|| data.get("output"))
                    .cloned()
                    .unwrap_or(serde_json::Value::String(String::new())),
            );
            copy_execution_boundary_fields(&mut out, &data);
            serde_json::Value::Object(out)
        }
        "run_started" => {
            let mut out = serde_json::json!({ "type": "run_started" });
            if let Some(obj) = out.as_object_mut() {
                if let Some(run_id) = data.get("run_id").cloned() {
                    obj.insert("run_id".to_string(), run_id);
                }
                if let Some(session_id) = data.get("session_id").cloned() {
                    obj.insert("session_id".to_string(), session_id);
                }
                if let Some(interaction_mode) = data.get("interaction_mode").cloned() {
                    obj.insert("interaction_mode".to_string(), interaction_mode);
                }
                if let Some(interactive_client) = data.get("interactive_client").cloned() {
                    obj.insert("interactive_client".to_string(), interactive_client);
                }
                if let Some(turn_intent_policy) = data.get("turn_intent_policy").cloned() {
                    obj.insert("turn_intent_policy".to_string(), turn_intent_policy);
                }
                if let Some(workspace) = data.get("workspace").cloned() {
                    obj.insert("workspace".to_string(), workspace);
                }
                if let Some(executor) = data.get("executor").cloned() {
                    obj.insert("executor".to_string(), executor);
                }
                if let Some(transport) = data.get("transport").cloned() {
                    obj.insert("transport".to_string(), transport);
                }
            }
            out
        }
        "run_finished" => {
            let mut out = serde_json::json!({ "type": "run_finished" });
            if let Some(obj) = out.as_object_mut() {
                for key in [
                    "run_id",
                    "status",
                    "outcome",
                    "finish_reason",
                    "error",
                    "error_code",
                    "error_kind",
                    "interrupted",
                    "interruption_kind",
                    "resumable",
                    "waiting_for",
                ] {
                    insert_if_present(obj, &data, key);
                }
            }
            out
        }
        "run_error" => {
            let message = data
                .get("error")
                .or_else(|| data.get("message"))
                .cloned()
                .unwrap_or(serde_json::Value::String("Unknown error".to_string()));
            let surface = data
                .get("error_kind")
                .and_then(serde_json::Value::as_str)
                .and_then(run_error_client_surface);
            let code = surface.map_or("RUN_ERROR", |(code, _, _)| code);
            let error_code = run_error_code_from_data(&data);
            let mut out = serde_json::Map::from_iter([
                (
                    "type".to_string(),
                    serde_json::Value::String("run_error".to_string()),
                ),
                ("message".to_string(), message.clone()),
                ("error".to_string(), message),
                (
                    "code".to_string(),
                    serde_json::Value::String(code.to_string()),
                ),
            ]);
            if let Some(error_code) = error_code {
                out.insert(
                    "error_code".to_string(),
                    serde_json::Value::String(error_code),
                );
            }
            if let Some((_, retryable, retry_after_ms)) = surface {
                out.insert("retryable".to_string(), serde_json::Value::Bool(retryable));
                if let Some(retry_after_ms) = retry_after_ms {
                    out.insert(
                        "retry_after_ms".to_string(),
                        serde_json::Value::from(retry_after_ms),
                    );
                }
            }
            for key in ["run_id", "error_kind", "reason", "blocked"] {
                insert_if_present(&mut out, &data, key);
            }
            serde_json::Value::Object(out)
        }
        "run_interrupted" => {
            let mut out = serde_json::Map::from_iter([(
                "type".to_string(),
                serde_json::Value::String("run_interrupted".to_string()),
            )]);
            for (k, v) in &data {
                out.insert(k.clone(), v.clone());
            }
            if !out.contains_key("message")
                && let Some(user_message) = data.get("user_message").cloned()
            {
                out.insert("message".to_string(), user_message);
            }
            serde_json::Value::Object(out)
        }
        "approval_request" | "approval_required" => {
            let mut out = serde_json::json!({ "type": "approval_required" });
            if let Some(obj) = out.as_object_mut() {
                for (k, v) in &data {
                    obj.insert(k.clone(), v.clone());
                }
            }
            out
        }
        "tool_request" => {
            let mut out = serde_json::json!({ "type": "tool_request" });
            if let Some(obj) = out.as_object_mut() {
                for (k, v) in &data {
                    obj.insert(k.clone(), v.clone());
                }
            }
            out
        }
        "ask_user_prompted" | "user_prompt_required" => {
            let mut out = serde_json::json!({ "type": "user_prompt_required" });
            if let Some(obj) = out.as_object_mut() {
                for (k, v) in &data {
                    obj.insert(k.clone(), v.clone());
                }
            }
            out
        }
        "user_input" => {
            let mut out = serde_json::json!({ "type": "user_input" });
            if let Some(obj) = out.as_object_mut() {
                for (k, v) in &data {
                    obj.insert(k.clone(), v.clone());
                }
            }
            out
        }
        // Turn-completion events carry the authoritative assistant
        // text for client reconciliation (the streaming deltas may be
        // stale if the server recovered mid-turn). Pass through with
        // the full data payload merged into the client-shaped event.
        "turn_complete" | "turn_done" => {
            let mut out = serde_json::json!({ "type": event_type });
            if let Some(obj) = out.as_object_mut() {
                for (k, v) in &data {
                    obj.insert(k.clone(), v.clone());
                }
            }
            out
        }
        "context_meta" => {
            let mut out = serde_json::json!({ "type": "context_meta" });
            if let Some(obj) = out.as_object_mut() {
                for (k, v) in &data {
                    obj.insert(k.clone(), v.clone());
                }
            }
            out
        }
        "run_paused" => {
            // Pause/resume lifecycle events had been falling through
            // the `_` catch-all in pre-wip-7 code, which relied on
            // passthrough. wip-7's allowlist drops the catch-all, so
            // pause/resume need explicit arms — `run_handlers` also
            // injects `run_id` for these.
            let mut out = serde_json::json!({ "type": "run_paused" });
            if let Some(obj) = out.as_object_mut() {
                for (k, v) in &data {
                    obj.insert(k.clone(), v.clone());
                }
            }
            out
        }
        "run_waiting" => {
            let mut out = serde_json::json!({ "type": "run_waiting" });
            if let Some(obj) = out.as_object_mut() {
                for (k, v) in &data {
                    obj.insert(k.clone(), v.clone());
                }
            }
            out
        }
        "run_resumed" => {
            let mut out = serde_json::json!({ "type": "run_resumed" });
            if let Some(obj) = out.as_object_mut() {
                for (k, v) in &data {
                    obj.insert(k.clone(), v.clone());
                }
            }
            out
        }
        "user_intent" => {
            let mut out = serde_json::json!({
                "type": "user_intent_accepted",
                "status": UserIntentStatus::AcceptedRemote,
            });
            if let Some(obj) = out.as_object_mut() {
                for key in ["intent_id", "delivery"] {
                    insert_if_present(obj, &data, key);
                }
            }
            out
        }
        "user_intent_applied" => {
            let mut out = serde_json::json!({ "type": "user_intent_applied" });
            if let Some(obj) = out.as_object_mut() {
                for key in ["intent_id", "delivery", "status", "event_index"] {
                    insert_if_present(obj, &data, key);
                }
            }
            out
        }
        "run_blocked" => {
            let mut out = serde_json::json!({ "type": "run_blocked" });
            if let Some(obj) = out.as_object_mut() {
                for (k, v) in &data {
                    obj.insert(k.clone(), v.clone());
                }
            }
            out
        }
        "plan_created" => serde_json::json!({
            "type": "plan_created",
            "plan": data.get("plan").cloned().unwrap_or(serde_json::json!({})),
        }),
        "plan_step_start" => serde_json::json!({
            "type": "plan_step_start",
            "step": data.get("step").cloned().unwrap_or(serde_json::Value::String(String::new())),
        }),
        "plan_step_done" => serde_json::json!({
            "type": "plan_step_done",
            "step": data.get("step").cloned().unwrap_or(serde_json::Value::String(String::new())),
            "result": data.get("result").cloned().unwrap_or(serde_json::Value::String(String::new())),
        }),
        "plan_revised" => serde_json::json!({
            "type": "plan_revised",
            "plan": data.get("plan").cloned().unwrap_or(serde_json::json!({})),
        }),
        "agent_delegated" => serde_json::json!({
            "type": "agent_delegated",
            "agent_id": data.get("agent_id").cloned().unwrap_or(serde_json::Value::String(String::new())),
            "task": data.get("task").cloned().unwrap_or(serde_json::Value::String(String::new())),
        }),
        "agent_communication"
        | "agent_spawned"
        | "agent_progress"
        | "agent_completed"
        | "agent_failed"
        | "agent_waiting"
        | "agent_cancelled"
        | "agent_interrupted" => {
            let mut out = serde_json::json!({ "type": event_type });
            if let Some(obj) = out.as_object_mut() {
                for (k, v) in &data {
                    obj.insert(k.clone(), v.clone());
                }
            }
            out
        }
        "keepalive" => serde_json::json!({ "type": "ping" }),
        _ => {
            // wip-7 allowlist: unknown internal event_types are
            // dropped. Adding a new event type means adding it
            // explicitly above (and, for client-facing events,
            // adding its `type` to `EXTERNAL_CLIENT_ALLOWLIST`).
            serde_json::Value::Null
        }
    }
}

#[derive(Clone, Debug)]
pub struct UnconfiguredRunLifecycleService;

#[async_trait]
impl RunLifecycleService for UnconfiguredRunLifecycleService {
    async fn create_run(
        &self,
        _user_id: String,
        _request: ChatRequestData,
    ) -> Result<ChatRunRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response_coded(
            StatusCode::NOT_IMPLEMENTED,
            "Run lifecycle service not configured",
            RUN_LIFECYCLE_UNCONFIGURED_ERROR_CODE,
        ))
    }

    async fn stream_chat(
        &self,
        _user_id: String,
        _request: ChatRequestData,
    ) -> Result<ChatStreamRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response_coded(
            StatusCode::NOT_IMPLEMENTED,
            "Run lifecycle service not configured",
            RUN_LIFECYCLE_UNCONFIGURED_ERROR_CODE,
        ))
    }

    async fn get_run_status(
        &self,
        _run_id: String,
        _user_id: String,
    ) -> Result<RunStatusRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response_coded(
            StatusCode::NOT_IMPLEMENTED,
            "Run lifecycle service not configured",
            RUN_LIFECYCLE_UNCONFIGURED_ERROR_CODE,
        ))
    }

    async fn stream_run(
        &self,
        _run_id: String,
        _user_id: String,
        _last_index: u32,
    ) -> Result<Vec<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response_coded(
            StatusCode::NOT_IMPLEMENTED,
            "Run lifecycle service not configured",
            RUN_LIFECYCLE_UNCONFIGURED_ERROR_CODE,
        ))
    }

    async fn cancel_run(
        &self,
        _run_id: String,
        _user_id: String,
    ) -> Result<CancelRunRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response_coded(
            StatusCode::NOT_IMPLEMENTED,
            "Run lifecycle service not configured",
            RUN_LIFECYCLE_UNCONFIGURED_ERROR_CODE,
        ))
    }

    async fn list_runs_cursor(
        &self,
        _user_id: String,
        _limit: u32,
        _cursor: Option<RunListCursor>,
    ) -> Result<RunListRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response_coded(
            StatusCode::NOT_IMPLEMENTED,
            "Run lifecycle service not configured",
            RUN_LIFECYCLE_UNCONFIGURED_ERROR_CODE,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use uuid::Uuid;

    static MATRIXONE_RUN_STORE_BOOTSTRAP: tokio::sync::OnceCell<astra_core::MatrixOneSettings> =
        tokio::sync::OnceCell::const_new();

    fn durable_run_record(run_id: &str) -> DurableRunRecord {
        DurableRunRecord {
            run_id: run_id.to_string(),
            session_id: "s1".into(),
            user_id: "u1".into(),
            status: "running".into(),
            parent_run_id: None,
            root_run_id: Some(run_id.to_string()),
            ancestor_path: Some(run_id.to_string()),
            depth: 0,
            delegation_id: None,
            agent_id: None,
            retry_of: None,
            retry_scope: None,
            waiting_for: None,
            owner_pod_id: None,
            owner_lease_expires_at: None,
            run_generation: 0,
            last_event_idx: -1,
            checkpoint_version: None,
            checkpoint_json: None,
            error_code: None,
            error_message: None,
            retry_count: 0,
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
            total_tool_calls: 0,
            agent_binding_id: None,
            agent_binding_name: None,
            agent_binding_schema_version: None,
            model_offering_id: None,
            resolved_model_name: None,
            capability_server_refs_json: None,
            runtime_profile: None,
            events: vec![],
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
        }
    }

    #[test]
    fn run_event_rows_normalize_spawn_subject_without_inventing_terminal_subjects() {
        let spawned = build_run_event_insert_row(
            "user-1",
            "parent-run",
            "session-1",
            Some("root"),
            4,
            "pod-a",
            &json!({
                "type": "agent_spawned",
                "run_id": "child-run",
                "agent_id": "reviewer"
            }),
        )
        .unwrap();
        assert_eq!(spawned.subject_run_id.as_deref(), Some("child-run"));

        let terminal = build_run_event_insert_row(
            "user-1",
            "child-run",
            "session-1",
            Some("reviewer"),
            9,
            "pod-a",
            &json!({"event_type": "run_finished", "data": {"status": "completed"}}),
        )
        .unwrap();
        assert_eq!(terminal.subject_run_id, None);
    }

    #[derive(Clone)]
    struct FakeRunStateRow {
        failed_column: Option<&'static str>,
        i64_overrides: Vec<(&'static str, i64)>,
        payload_json: &'static str,
    }

    impl FakeRunStateRow {
        fn complete() -> Self {
            Self {
                failed_column: None,
                i64_overrides: Vec::new(),
                payload_json: r#"{"event_type":"text_delta","content":"hi"}"#,
            }
        }

        fn fail_on(column: &'static str) -> Self {
            Self {
                failed_column: Some(column),
                ..Self::complete()
            }
        }

        fn with_i64(column: &'static str, value: i64) -> Self {
            Self {
                i64_overrides: vec![(column, value)],
                ..Self::complete()
            }
        }

        fn with_payload_json(payload_json: &'static str) -> Self {
            Self {
                payload_json,
                ..Self::complete()
            }
        }

        fn fail_if_needed(&self, column: &str) -> Result<(), sqlx::Error> {
            if self.failed_column == Some(column) {
                Err(sqlx::Error::ColumnNotFound(column.to_string()))
            } else {
                Ok(())
            }
        }
    }

    impl RunStateDbRow for FakeRunStateRow {
        fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
            self.fail_if_needed(column)?;
            Ok(match column {
                "run_id" => "run-1",
                "user_id" => "user-1",
                "session_id" => "session-1",
                "status" => STATUS_RUNNING,
                "checkpoint_id" => "checkpoint-1",
                "checkpoint_kind" => "resume",
                "checkpoint_version" => "checkpoint_v2",
                "idempotency_key" => "checkpoint:run-1:resume:batch-1",
                "checkpoint_json" => r#"{"version":"checkpoint_v2"}"#,
                "projection_hash" => "hash-1",
                "payload_json" => self.payload_json,
                "tool_name" => "bash",
                "normalize_version" => "raw_v2",
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
            .to_string())
        }

        fn optional_string_column(&self, column: &str) -> Result<Option<String>, sqlx::Error> {
            self.fail_if_needed(column)?;
            Ok(match column {
                "parent_run_id" => Some("parent-run".to_string()),
                "root_run_id" => Some("root-run".to_string()),
                "ancestor_path" => Some("root-run/run-1".to_string()),
                "delegation_id" => Some("delegation-1".to_string()),
                "agent_id" => Some("agent-1".to_string()),
                "retry_of" => Some("retry-source".to_string()),
                "retry_scope" => Some("node".to_string()),
                "waiting_for" => None,
                "owner_pod_id" => Some("pod-1".to_string()),
                "checkpoint_version" => Some("checkpoint_v2".to_string()),
                "checkpoint_json" => Some(r#"{"version":"checkpoint_v2"}"#.to_string()),
                "error_code" => None,
                "error_message" => None,
                "agent_binding_id" => Some("binding-1".to_string()),
                "agent_binding_name" => Some("binding".to_string()),
                "agent_binding_schema_version" => Some("v1".to_string()),
                "model_offering_id" => Some("offer-model".to_string()),
                "resolved_model_name" => Some("model".to_string()),
                "capability_server_refs_json" => Some("[]".to_string()),
                "runtime_profile" => Some("default".to_string()),
                "latest_event_type" => Some("text_delta".to_string()),
                "latest_checkpoint_id" => Some("checkpoint-1".to_string()),
                "latest_checkpoint_kind" => Some("resume".to_string()),
                "latest_checkpoint_version" => Some("checkpoint_v2".to_string()),
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            })
        }

        fn i64_column(&self, column: &str) -> Result<i64, sqlx::Error> {
            self.fail_if_needed(column)?;
            if let Some((_, value)) = self
                .i64_overrides
                .iter()
                .find(|(candidate, _)| *candidate == column)
            {
                return Ok(*value);
            }
            Ok(match column {
                "depth" => 2,
                "run_generation" => 3,
                "last_event_idx" => 4,
                "retry_count" => 1,
                "total_prompt_tokens" => 100,
                "total_completion_tokens" => 25,
                "total_tool_calls" => 6,
                "node_seq" => 4,
                "projection_event_idx" => 4,
                "total" => 11,
                "event_idx" => 9,
                "max_preview_bytes" => 1024,
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            })
        }

        fn datetime_string_column(&self, column: &str) -> Result<String, sqlx::Error> {
            self.fail_if_needed(column)?;
            Ok(match column {
                "created_at" => "2026-06-26 12:00:00",
                "updated_at" => "2026-06-26 12:01:00",
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
            .to_string())
        }

        fn optional_datetime_string_column(
            &self,
            column: &str,
        ) -> Result<Option<String>, sqlx::Error> {
            self.fail_if_needed(column)?;
            Ok(match column {
                "owner_lease_expires_at" => Some("2026-06-26 12:02:00".to_string()),
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            })
        }
    }

    fn assert_run_db_error_mentions(
        result: Result<impl std::fmt::Debug, DatabaseRunStateStoreError>,
        needle: &str,
    ) {
        let error = result.expect_err("decode should fail");
        match error {
            DatabaseRunStateStoreError::Database { entity, source, .. } => {
                assert!(
                    entity.contains(needle) || source.to_string().contains(needle),
                    "error should identify `{needle}`, got entity={entity}, source={source}"
                );
            }
            other => panic!("expected database decode error, got {other:?}"),
        }
    }

    fn assert_run_json_error(result: Result<impl std::fmt::Debug, DatabaseRunStateStoreError>) {
        let error = result.expect_err("decode should fail");
        assert!(
            matches!(
                error,
                DatabaseRunStateStoreError::Json {
                    operation: "decode_run_event_payload",
                    ..
                }
            ),
            "expected run event payload JSON decode error, got {error:?}"
        );
    }

    fn make_event(event_type: &str, data: serde_json::Value) -> serde_json::Value {
        json!({"event_type": event_type, "data": data})
    }

    async fn setup_database_run_state_store_it() -> (DatabaseRunStateStore, astra_core::SharedPool)
    {
        assert_eq!(
            std::env::var("ASTRA_TEST_DB_IT").as_deref(),
            Ok("1"),
            "set ASTRA_TEST_DB_IT=1 for ignored integration tests"
        );
        let settings = MATRIXONE_RUN_STORE_BOOTSTRAP
            .get_or_init(|| async {
                let settings = astra_core::MatrixOneSettings::from_env();
                let catalog = std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG")
                    .unwrap_or_else(|_| "mysql".to_string());
                crate::storage::ensure_core_schema(&settings, &catalog)
                    .await
                    .expect("ensure_core_schema");
                settings
            })
            .await
            .clone();
        let pool = astra_core::SharedPool::new(&settings)
            .await
            .expect("SharedPool::new");
        (
            DatabaseRunStateStore::new(pool.clone()).with_owner_pod_id("runs-it-pod"),
            pool,
        )
    }

    async fn cleanup_database_run_fixture(
        pool: &astra_core::SharedPool,
        user_id: &str,
        run_id: &str,
    ) {
        for sql in [
            "DELETE FROM run_display_projections WHERE user_id = ? AND run_id = ?",
            "DELETE FROM run_checkpoints WHERE user_id = ? AND run_id = ?",
            "DELETE FROM agent_run_events WHERE user_id = ? AND run_id = ?",
            "DELETE FROM agent_runs WHERE user_id = ? AND run_id = ?",
        ] {
            sqlx::query(sql)
                .bind(user_id)
                .bind(run_id)
                .execute(pool.get())
                .await
                .unwrap_or_else(|error| {
                    panic!(
                        "database run fixture cleanup failed for user={user_id} run={run_id} sql={sql}: {error}"
                    )
                });
        }
    }

    #[test]
    fn session_execution_slot_applies_only_to_user_root_runs() {
        let root = durable_run_record("root");
        assert!(run_requires_session_execution_slot(&root));

        let mut retry = durable_run_record("retry");
        retry.retry_of = Some("root".into());
        assert!(!run_requires_session_execution_slot(&retry));

        let mut child = durable_run_record("child");
        child.parent_run_id = Some("root".into());
        assert!(!run_requires_session_execution_slot(&child));

        let mut delegated = durable_run_record("delegated");
        delegated.delegation_id = Some("delegation-1".into());
        assert!(!run_requires_session_execution_slot(&delegated));

        let mut team_parent = durable_run_record("team-parent");
        team_parent.agent_id = Some("orchestrator".into());
        assert!(!run_requires_session_execution_slot(&team_parent));
    }

    #[test]
    fn durable_run_status_helpers_keep_terminal_and_blocking_semantics_distinct() {
        assert_eq!(
            durable_run_status_kind(STATUS_RUNNING),
            DurableRunStatusKind::Running
        );
        assert_eq!(
            durable_run_status_kind(STATUS_WAITING),
            DurableRunStatusKind::Waiting
        );
        assert_eq!(
            durable_run_status_kind(STATUS_PAUSED),
            DurableRunStatusKind::Paused
        );
        assert_eq!(
            durable_run_status_kind(STATUS_COMPLETED),
            DurableRunStatusKind::Completed
        );
        assert_eq!(
            durable_run_status_kind(STATUS_DELEGATED),
            DurableRunStatusKind::Delegated
        );
        assert_eq!(
            durable_run_status_kind(STATUS_FAILED),
            DurableRunStatusKind::Failed
        );
        assert_eq!(
            durable_run_status_kind(STATUS_CANCELLED),
            DurableRunStatusKind::Cancelled
        );
        assert_eq!(
            durable_run_status_kind("mystery"),
            DurableRunStatusKind::Other
        );

        assert!(durable_run_status_is_terminal(STATUS_COMPLETED));
        assert!(durable_run_status_is_terminal(STATUS_DELEGATED));
        assert!(durable_run_status_is_terminal(STATUS_FAILED));
        assert!(durable_run_status_is_terminal(STATUS_CANCELLED));
        assert!(!durable_run_status_is_terminal(STATUS_RUNNING));
        assert!(!durable_run_status_is_terminal(STATUS_WAITING));
        assert!(!durable_run_status_is_terminal(STATUS_PAUSED));

        assert!(durable_run_status_blocks_session(STATUS_RUNNING, None));
        assert!(durable_run_status_blocks_session(STATUS_WAITING, None));
        assert!(durable_run_status_blocks_session(
            STATUS_PAUSED,
            Some("tool_approval")
        ));
        assert!(!durable_run_status_blocks_session(STATUS_PAUSED, None));
        assert!(!durable_run_status_blocks_session(STATUS_COMPLETED, None));
        assert!(session_execution_slot_owner_reclaimable(
            STATUS_COMPLETED,
            None,
            false,
            false
        ));
        assert!(session_execution_slot_owner_reclaimable(
            STATUS_PAUSED,
            None,
            false,
            false
        ));
        assert!(!session_execution_slot_owner_reclaimable(
            STATUS_RUNNING,
            None,
            false,
            true
        ));
        assert!(!session_execution_slot_owner_reclaimable(
            STATUS_RUNNING,
            None,
            true,
            false
        ));
        assert!(session_execution_slot_owner_reclaimable(
            STATUS_RUNNING,
            None,
            true,
            true
        ));
        assert!(!session_execution_slot_owner_reclaimable(
            STATUS_PAUSED,
            Some("user_resume"),
            true,
            true
        ));
        assert!(!durable_run_status_blocks_session(STATUS_DELEGATED, None));
        assert_eq!(
            durable_run_status_to_subrun_state(STATUS_WAITING),
            SubRunState::Waiting
        );
        assert_eq!(
            durable_run_status_to_subrun_state(STATUS_RUNNING),
            SubRunState::Running
        );
        assert_eq!(
            durable_run_status_to_subrun_state(STATUS_DELEGATED),
            SubRunState::Completed
        );
        assert_eq!(
            durable_run_status_to_subrun_state("mystery"),
            SubRunState::Failed
        );
    }

    #[test]
    fn run_record_row_decode_preserves_database_values_and_fails_loudly() {
        let record = decode_run_record_from_row(&FakeRunStateRow::complete()).unwrap();
        assert_eq!(record.run_id, "run-1");
        assert_eq!(record.user_id, "user-1");
        assert_eq!(record.session_id, "session-1");
        assert_eq!(record.parent_run_id.as_deref(), Some("parent-run"));
        assert_eq!(record.root_run_id.as_deref(), Some("root-run"));
        assert_eq!(record.ancestor_path.as_deref(), Some("root-run/run-1"));
        assert_eq!(record.depth, 2);
        assert_eq!(record.status, STATUS_RUNNING);
        assert_eq!(record.run_generation, 3);
        assert_eq!(record.last_event_idx, 4);
        assert_eq!(record.retry_count, 1);
        assert_eq!(record.total_prompt_tokens, 100);
        assert_eq!(record.total_completion_tokens, 25);
        assert_eq!(record.total_tool_calls, 6);
        assert_eq!(
            record.owner_lease_expires_at.as_deref(),
            Some("2026-06-26 12:02:00")
        );
        assert_eq!(record.created_at, "2026-06-26 12:00:00");
        assert_eq!(record.updated_at, "2026-06-26 12:01:00");

        for column in [
            "run_id",
            "user_id",
            "session_id",
            "parent_run_id",
            "root_run_id",
            "ancestor_path",
            "depth",
            "delegation_id",
            "agent_id",
            "retry_of",
            "retry_scope",
            "status",
            "waiting_for",
            "owner_pod_id",
            "owner_lease_expires_at",
            "run_generation",
            "last_event_idx",
            "checkpoint_version",
            "checkpoint_json",
            "error_code",
            "error_message",
            "retry_count",
            "total_prompt_tokens",
            "total_completion_tokens",
            "total_tool_calls",
            "agent_binding_id",
            "agent_binding_name",
            "agent_binding_schema_version",
            "model_offering_id",
            "resolved_model_name",
            "capability_server_refs_json",
            "runtime_profile",
            "created_at",
            "updated_at",
        ] {
            assert_run_db_error_mentions(
                decode_run_record_from_row(&FakeRunStateRow::fail_on(column)),
                column,
            );
        }
    }

    #[test]
    fn run_record_row_decode_rejects_invalid_numeric_database_values() {
        for column in [
            "depth",
            "run_generation",
            "retry_count",
            "total_prompt_tokens",
            "total_completion_tokens",
            "total_tool_calls",
        ] {
            assert_run_db_error_mentions(
                decode_run_record_from_row(&FakeRunStateRow::with_i64(column, -1)),
                column,
            );
        }

        assert_run_db_error_mentions(
            decode_run_record_from_row(&FakeRunStateRow::with_i64("last_event_idx", -2)),
            "last_event_idx",
        );
        assert_eq!(
            decode_run_record_from_row(&FakeRunStateRow::with_i64("last_event_idx", -1))
                .unwrap()
                .last_event_idx,
            -1
        );
        assert_run_db_error_mentions(
            decode_run_record_from_row(&FakeRunStateRow::with_i64(
                "depth",
                i64::from(u32::MAX) + 1,
            )),
            "depth",
        );
    }

    #[test]
    fn run_checkpoint_row_decode_preserves_values_and_fails_loudly() {
        let checkpoint = decode_run_checkpoint_record_from_row(&FakeRunStateRow::complete())
            .expect("checkpoint row decodes");
        assert_eq!(checkpoint.checkpoint_id, "checkpoint-1");
        assert_eq!(checkpoint.run_id, "run-1");
        assert_eq!(checkpoint.user_id, "user-1");
        assert_eq!(checkpoint.session_id, "session-1");
        assert_eq!(checkpoint.node_seq, 4);
        assert_eq!(checkpoint.checkpoint_kind, "resume");
        assert_eq!(checkpoint.checkpoint_version, "checkpoint_v2");
        assert_eq!(checkpoint.created_at, "2026-06-26 12:00:00");

        for column in [
            "checkpoint_id",
            "run_id",
            "user_id",
            "session_id",
            "node_seq",
            "checkpoint_kind",
            "checkpoint_version",
            "idempotency_key",
            "checkpoint_json",
            "created_at",
        ] {
            assert_run_db_error_mentions(
                decode_run_checkpoint_record_from_row(&FakeRunStateRow::fail_on(column)),
                column,
            );
        }
        assert_run_db_error_mentions(
            decode_run_checkpoint_record_from_row(&FakeRunStateRow::with_i64("node_seq", -1)),
            "node_seq",
        );
    }

    #[test]
    fn run_projection_row_decode_preserves_values_and_fails_loudly() {
        let projection =
            decode_run_display_projection_record_from_row(&FakeRunStateRow::complete())
                .expect("projection row decodes");
        assert_eq!(projection.run_id, "run-1");
        assert_eq!(projection.user_id, "user-1");
        assert_eq!(projection.session_id, "session-1");
        assert_eq!(projection.status, STATUS_RUNNING);
        assert_eq!(projection.projection_event_idx, 4);
        assert_eq!(projection.latest_event_type.as_deref(), Some("text_delta"));
        assert_eq!(projection.total_prompt_tokens, 100);
        assert_eq!(projection.total_completion_tokens, 25);
        assert_eq!(projection.total_tool_calls, 6);
        assert_eq!(projection.projection_hash, "hash-1");
        assert_eq!(projection.updated_at, "2026-06-26 12:01:00");

        for column in [
            "run_id",
            "user_id",
            "session_id",
            "status",
            "waiting_for",
            "error_message",
            "projection_event_idx",
            "latest_event_type",
            "latest_checkpoint_id",
            "latest_checkpoint_kind",
            "latest_checkpoint_version",
            "total_prompt_tokens",
            "total_completion_tokens",
            "total_tool_calls",
            "projection_hash",
            "updated_at",
        ] {
            assert_run_db_error_mentions(
                decode_run_display_projection_record_from_row(&FakeRunStateRow::fail_on(column)),
                column,
            );
        }

        assert_run_db_error_mentions(
            decode_run_display_projection_record_from_row(&FakeRunStateRow::with_i64(
                "projection_event_idx",
                -2,
            )),
            "projection_event_idx",
        );
        assert_eq!(
            decode_run_display_projection_record_from_row(&FakeRunStateRow::with_i64(
                "projection_event_idx",
                -1,
            ))
            .unwrap()
            .projection_event_idx,
            -1
        );
    }

    #[test]
    fn usage_projection_patch_hash_changes_with_usage_totals() {
        let base = usage_projection_patch_hash("run-1", 10, 4, 2);
        assert_eq!(base, usage_projection_patch_hash("run-1", 10, 4, 2));
        assert_ne!(base, usage_projection_patch_hash("run-1", 11, 4, 2));
        assert_ne!(base, usage_projection_patch_hash("run-1", 10, 5, 2));
        assert_ne!(base, usage_projection_patch_hash("run-1", 10, 4, 3));
        assert_ne!(base, usage_projection_patch_hash("run-2", 10, 4, 2));
    }

    #[test]
    fn status_projection_patch_hash_changes_with_transition_fields() {
        let base = status_projection_patch_hash(
            "run-1",
            STATUS_FAILED,
            None,
            Some("boom"),
            2,
            Some("run_finished"),
        );
        assert_eq!(
            base,
            status_projection_patch_hash(
                "run-1",
                STATUS_FAILED,
                None,
                Some("boom"),
                2,
                Some("run_finished")
            )
        );
        assert_ne!(
            base,
            status_projection_patch_hash(
                "run-1",
                STATUS_COMPLETED,
                None,
                Some("boom"),
                2,
                Some("run_finished")
            )
        );
        assert_ne!(
            base,
            status_projection_patch_hash(
                "run-1",
                STATUS_FAILED,
                Some("tool_approval"),
                Some("boom"),
                2,
                Some("run_finished")
            )
        );
        assert_ne!(
            base,
            status_projection_patch_hash(
                "run-1",
                STATUS_FAILED,
                None,
                Some("boom"),
                3,
                Some("run_finished")
            )
        );
        assert_ne!(
            base,
            status_projection_patch_hash("run-1", STATUS_FAILED, None, Some("boom"), 2, None)
        );
    }

    #[test]
    fn tool_preview_contract_row_decode_preserves_values_and_fails_loudly() {
        let (tool_name, contract) = decode_tool_preview_contract_row(&FakeRunStateRow::complete())
            .expect("preview contract decodes");
        assert_eq!(tool_name, "bash");
        assert_eq!(contract.max_preview_bytes, 1024);
        assert_eq!(contract.normalize_version, "raw_v2");
        assert!(contract.found);

        for column in ["tool_name", "max_preview_bytes", "normalize_version"] {
            assert_run_db_error_mentions(
                decode_tool_preview_contract_row(&FakeRunStateRow::fail_on(column)),
                column,
            );
        }
        assert_run_db_error_mentions(
            decode_tool_preview_contract_row(&FakeRunStateRow::with_i64("max_preview_bytes", 0)),
            "max_preview_bytes",
        );
    }

    fn preview_contract(max_preview_bytes: usize, found: bool) -> ToolPreviewContract {
        ToolPreviewContract {
            max_preview_bytes,
            normalize_version: "raw_v1".to_string(),
            found,
        }
    }

    fn preview_item(tool_name: &str, output_json: serde_json::Value) -> ToolOutputBatchItem {
        let mut object = output_json.as_object().cloned().unwrap_or_default();
        let output = [
            "result", "output", "content", "text", "message", "error", "stderr", "stdout",
        ]
        .into_iter()
        .find_map(|key| object.remove(key))
        .map(|value| match value {
            serde_json::Value::String(text) => text,
            other => other.to_string(),
        })
        .unwrap_or_default();
        ToolOutputBatchItem {
            output_id: "output-1".to_string(),
            tool_call_id: Some("call-1".to_string()),
            tool_name: tool_name.to_string(),
            result: ToolInvocationResultPayload::bounded_projection(
                output,
                object.into_iter().collect(),
                None,
            ),
        }
    }

    #[test]
    fn tool_output_serialization_rejects_forged_unbounded_payload_before_database_work() {
        let item = ToolOutputBatchItem {
            output_id: "forged-output".to_string(),
            tool_call_id: Some("call-1".to_string()),
            tool_name: "provider_read".to_string(),
            result: ToolInvocationResultPayload {
                output: "🦀".repeat(astra_turn_types::TOOL_INVOCATION_RESULT_OUTPUT_MAX_BYTES),
                metadata: Default::default(),
                exit_semantics: None,
            },
        };

        let error = serialize_tool_output_payloads(&[item]).unwrap_err();
        assert!(matches!(
            error,
            DatabaseRunStateStoreError::InvalidToolOutput { ref output_id, .. }
                if output_id == "forged-output"
        ));
    }

    #[test]
    fn tool_output_preview_prefers_semantic_result_over_transport_metadata() {
        let output_json = json!({
            "capacity_provider_coverage": [
                {
                    "provider_id": "server-control-plane",
                    "capabilities": ["session", "task_board", "introspect"]
                },
                {
                    "provider_id": "edge-macpro.local",
                    "capabilities": ["workspace_read", "workspace_write", "shell"]
                }
            ],
            "executor": {
                "display_name": "macpro.local",
                "status": "online",
            },
            "policy": {
                "filesystem": "read_write_workspace",
                "network": "open",
            },
            "result": ".agent/\n.github/\nrust/\nweb/\n",
        });
        let item = preview_item("list_dir", output_json);
        let payload = serde_json::to_string(&item.result).expect("payload serializes");

        let row = build_tool_output_preview_row(
            "session-1",
            &item,
            &payload,
            &preview_contract(120, true),
        );

        assert_eq!(row.payload, payload, "audit payload must remain lossless");
        assert_eq!(row.preview_text, ".agent/\n.github/\nrust/\nweb/");
        assert_eq!(row.preview_status, "template");
        assert!(
            !row.preview_text.contains("capacity_provider_coverage"),
            "preview should show the tool result, not transport metadata"
        );
        assert!(
            row.artifact_ref.is_some(),
            "large wrapper payload should still be addressable even when the preview is compact"
        );
    }

    #[test]
    fn tool_output_preview_large_result_is_borrowed_then_bounded() {
        let large_result = format!("{}{}", "x".repeat(256 * 1024), "tail");
        let item = preview_item(
            "read_file",
            json!({
                "capacity_provider_coverage": [{"provider_id": "edge-1"}],
                "result": large_result,
            }),
        );
        let payload = serde_json::to_string(&item.result).expect("payload serializes");

        let source = tool_output_preview_source(&item.result, &payload);
        assert!(
            matches!(source, Cow::Borrowed(_)),
            "large string result must be borrowed before the bounded preview allocation"
        );

        let row = build_tool_output_preview_row(
            "session-1",
            &item,
            &payload,
            &preview_contract(128, true),
        );

        assert_eq!(row.preview_text.len(), 128);
        assert!(row.preview_text.chars().all(|ch| ch == 'x'));
        assert_eq!(row.preview_status, "truncated");
        assert!(row.payload.len() <= astra_turn_types::TOOL_INVOCATION_RESULT_MAX_BYTES);
        assert!(row.payload.contains("astraResultProjection"));
        assert!(row.payload.contains("capacity_provider_coverage"));
    }

    #[test]
    fn tool_output_preview_uses_canonical_json_text_for_structured_result() {
        let item = preview_item(
            "custom_tool",
            json!({
                "result": {"nested": "not a scalar preview"},
                "data": ["also", "not", "scalar"],
            }),
        );
        let payload = serde_json::to_string(&item.result).expect("payload serializes");

        let row = build_tool_output_preview_row(
            "session-1",
            &item,
            &payload,
            &preview_contract(64, false),
        );

        assert_eq!(
            row.preview_text,
            truncate_utf8_bytes(&item.result.output, 64)
        );
        assert_eq!(row.preview_status, "fallback");
    }

    #[test]
    fn run_counter_and_event_payload_decoders_fail_loudly() {
        assert_eq!(
            run_row_non_negative_i64(
                &FakeRunStateRow::complete(),
                "count_user_runs",
                "agent_runs",
                "total",
            )
            .unwrap(),
            11
        );
        assert_run_db_error_mentions(
            run_row_non_negative_i64(
                &FakeRunStateRow::fail_on("total"),
                "count_user_runs",
                "agent_runs",
                "total",
            ),
            "total",
        );
        assert_run_db_error_mentions(
            run_row_non_negative_i64(
                &FakeRunStateRow::with_i64("total", -1),
                "count_user_runs",
                "agent_runs",
                "total",
            ),
            "total",
        );

        let event = decode_run_event_payload(&FakeRunStateRow::complete(), "run-1").unwrap();
        assert_eq!(event["event_type"], "text_delta");
        assert_eq!(event["index"], 9);

        assert_run_db_error_mentions(
            decode_run_event_payload(&FakeRunStateRow::fail_on("payload_json"), "run-1"),
            "payload_json",
        );
        assert_run_db_error_mentions(
            decode_run_event_payload(&FakeRunStateRow::fail_on("event_idx"), "run-1"),
            "event_idx",
        );
        assert_run_db_error_mentions(
            decode_run_event_payload(&FakeRunStateRow::with_i64("event_idx", -1), "run-1"),
            "event_idx",
        );
        assert_run_json_error(decode_run_event_payload(
            &FakeRunStateRow::with_payload_json("{not-json"),
            "run-1",
        ));
    }

    #[test]
    fn owner_lease_renewal_interval_is_derived_from_ttl() {
        assert_eq!(
            run_owner_lease_renewal_interval(Duration::from_secs(45)),
            Duration::from_secs(15)
        );
        assert_eq!(
            run_owner_lease_renewal_interval(Duration::from_millis(30)),
            Duration::from_millis(10)
        );
        assert_eq!(
            run_owner_lease_renewal_interval(Duration::from_secs(300)),
            Duration::from_secs(15),
            "very long leases should not make active-run heartbeat too sparse"
        );
    }

    #[test]
    fn in_memory_terminal_status_is_immutable_including_delegated() {
        for terminal in [
            STATUS_COMPLETED,
            STATUS_DELEGATED,
            STATUS_FAILED,
            STATUS_CANCELLED,
        ] {
            let mut slots = std::collections::HashMap::new();
            let mut run = durable_run_record(&format!("{terminal}-terminal"));
            run.status = terminal.into();

            apply_in_memory_status_transition(&mut slots, &mut run, terminal, None, None, None)
                .expect("idempotent terminal replay must succeed");
            let error = apply_in_memory_status_transition(
                &mut slots,
                &mut run,
                STATUS_RUNNING,
                None,
                None,
                None,
            )
            .expect_err("terminal run must not resurrect");
            assert!(error.contains("terminal state immutability violated"));
            assert_eq!(run.status, terminal);
        }

        let mut active = durable_run_record("active");
        ensure_terminal_status_immutable(&active, STATUS_COMPLETED)
            .expect("non-terminal runs remain transitionable");
        active.status = STATUS_WAITING.into();
        ensure_terminal_status_immutable(&active, STATUS_FAILED)
            .expect("waiting recovery can still settle terminally");
    }

    #[tokio::test]
    async fn in_memory_store_allows_new_root_run_when_existing_run_is_paused_without_waiting() {
        let store = InMemoryRunStateStore::new();

        let mut paused = durable_run_record("paused");
        paused.status = STATUS_PAUSED.into();
        paused.waiting_for = None;
        store.insert_run(paused).await.unwrap();

        let fresh = durable_run_record("fresh");
        store
            .insert_run(fresh)
            .await
            .expect("resumable paused run should not block a fresh root run in the same session");
    }

    #[tokio::test]
    async fn in_memory_store_blocks_new_root_run_when_existing_run_waits_for_input() {
        let store = InMemoryRunStateStore::new();

        let mut paused = durable_run_record("paused");
        paused.status = STATUS_PAUSED.into();
        paused.waiting_for = Some("tool_approval".into());
        store.insert_run(paused).await.unwrap();

        let fresh = durable_run_record("fresh");
        let error = store
            .insert_run(fresh)
            .await
            .expect_err("approval-waiting paused run must still block the session");
        assert_eq!(error, "session already has an active run");
    }

    #[tokio::test]
    async fn in_memory_session_execution_slot_releases_on_nonblocking_status() {
        let store = InMemoryRunStateStore::new();

        store
            .insert_run(durable_run_record("slot-owner"))
            .await
            .unwrap();
        let blocked = store
            .insert_run(durable_run_record("blocked-while-running"))
            .await
            .expect_err("running root run must own the session slot");
        assert_eq!(blocked, "session already has an active run");

        assert!(
            store
                .update_run_status_if_current(
                    "u1",
                    "slot-owner",
                    &[STATUS_RUNNING],
                    STATUS_PAUSED,
                    None,
                    None
                )
                .await
                .unwrap()
        );
        store
            .insert_run(durable_run_record("fresh-after-paused-none"))
            .await
            .expect("paused without waiting_for must release the session slot");

        assert!(
            store
                .update_run_status_if_current(
                    "u1",
                    "fresh-after-paused-none",
                    &[STATUS_RUNNING],
                    STATUS_COMPLETED,
                    None,
                    None,
                )
                .await
                .unwrap()
        );
        store
            .insert_run(durable_run_record("fresh-after-terminal"))
            .await
            .expect("terminal root run must release the session slot");
    }

    #[tokio::test]
    async fn guarded_status_transition_excludes_current_paused_run_from_session_blocker() {
        let store = InMemoryRunStateStore::new();

        let mut paused = durable_run_record("paused-self");
        paused.status = STATUS_PAUSED.into();
        paused.waiting_for = Some("user_resume".into());
        store.insert_run(paused).await.unwrap();

        let outcome = store
            .update_run_status_with_event_if_current_unless_session_blocked(
                GuardedRunStatusTransitionRequest {
                    user_id: "u1",
                    run_id: "paused-self",
                    session_id: "s1",
                    expected_statuses: &[STATUS_PAUSED],
                    status: STATUS_RUNNING,
                    waiting_for: None,
                    error_message: None,
                    event: serde_json::json!({"event_type": "run_resumed", "data": {}}),
                },
            )
            .await
            .unwrap();

        assert_eq!(outcome, GuardedRunStatusTransition::Updated);
        let run = store.load_run("u1", "paused-self").await.unwrap().unwrap();
        assert_eq!(run.status, STATUS_RUNNING);
        assert_eq!(run.events.last().unwrap()["event_type"], "run_resumed");
    }

    #[tokio::test]
    async fn guarded_status_transition_rejects_other_durable_session_blocker() {
        let store = InMemoryRunStateStore::new();

        let mut paused = durable_run_record("paused-target");
        paused.status = STATUS_PAUSED.into();
        paused.waiting_for = None;
        store.insert_run(paused).await.unwrap();

        store
            .insert_run(durable_run_record("blocking-root"))
            .await
            .unwrap();

        let outcome = store
            .update_run_status_with_event_if_current_unless_session_blocked(
                GuardedRunStatusTransitionRequest {
                    user_id: "u1",
                    run_id: "paused-target",
                    session_id: "s1",
                    expected_statuses: &[STATUS_PAUSED],
                    status: STATUS_RUNNING,
                    waiting_for: None,
                    error_message: None,
                    event: serde_json::json!({"event_type": "run_resumed", "data": {}}),
                },
            )
            .await
            .unwrap();

        assert_eq!(outcome, GuardedRunStatusTransition::SessionBlocked);
        let target = store
            .load_run("u1", "paused-target")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(target.status, STATUS_PAUSED);
        assert!(
            target
                .events
                .iter()
                .all(|event| event["event_type"] != "run_resumed")
        );
    }

    #[tokio::test]
    async fn guarded_status_transition_status_mismatch_does_not_acquire_slot_or_event() {
        let store = InMemoryRunStateStore::new();

        let mut paused = durable_run_record("paused-mismatch");
        paused.status = STATUS_PAUSED.into();
        paused.waiting_for = None;
        store.insert_run(paused).await.unwrap();

        let outcome = store
            .update_run_status_with_event_if_current_unless_session_blocked(
                GuardedRunStatusTransitionRequest {
                    user_id: "u1",
                    run_id: "paused-mismatch",
                    session_id: "s1",
                    expected_statuses: &[STATUS_RUNNING],
                    status: STATUS_RUNNING,
                    waiting_for: None,
                    error_message: None,
                    event: serde_json::json!({"event_type": "run_resumed", "data": {}}),
                },
            )
            .await
            .unwrap();

        assert_eq!(outcome, GuardedRunStatusTransition::StatusConflict);
        let run = store
            .load_run("u1", "paused-mismatch")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.status, STATUS_PAUSED);
        assert!(
            run.events
                .iter()
                .all(|event| event["event_type"] != "run_resumed")
        );
        store
            .insert_run(durable_run_record("fresh-after-cas-miss"))
            .await
            .expect("CAS miss must not acquire the session slot");
    }

    #[tokio::test]
    async fn in_memory_guarded_transition_reconciles_stale_slot_cache_from_run_truth() {
        let store = InMemoryRunStateStore::new();

        let mut paused = durable_run_record("paused-stale-slot");
        paused.status = STATUS_PAUSED.into();
        paused.waiting_for = None;
        store.insert_run(paused).await.unwrap();
        {
            let mut slots = store.execution_slots.write().await;
            slots.insert(
                ("u1".to_string(), "s1".to_string()),
                "ghost-run".to_string(),
            );
        }

        let outcome = store
            .update_run_status_with_event_if_current_unless_session_blocked(
                GuardedRunStatusTransitionRequest {
                    user_id: "u1",
                    run_id: "paused-stale-slot",
                    session_id: "s1",
                    expected_statuses: &[STATUS_PAUSED],
                    status: STATUS_RUNNING,
                    waiting_for: None,
                    error_message: None,
                    event: serde_json::json!({"event_type": "run_resumed", "data": {}}),
                },
            )
            .await
            .unwrap();

        assert_eq!(outcome, GuardedRunStatusTransition::Updated);
        let slots = store.execution_slots.read().await;
        assert_eq!(
            slots
                .get(&("u1".to_string(), "s1".to_string()))
                .map(String::as_str),
            Some("paused-stale-slot")
        );
    }

    #[tokio::test]
    async fn in_memory_blocking_start_repairs_missing_slot_before_admission() {
        let store = InMemoryRunStateStore::new();
        store
            .insert_run(durable_run_record("live-owner"))
            .await
            .unwrap();
        store
            .execution_slots
            .write()
            .await
            .remove(&("u1".to_string(), "s1".to_string()));

        store
            .insert_run(durable_run_record("must-not-start"))
            .await
            .expect_err("run truth must prevent admission when its slot index is missing");

        assert_eq!(
            store
                .execution_slots
                .read()
                .await
                .get(&("u1".to_string(), "s1".to_string()))
                .map(String::as_str),
            Some("live-owner")
        );
    }

    #[tokio::test]
    async fn concurrent_guarded_resume_allows_only_one_root_run_per_session() {
        let store = std::sync::Arc::new(InMemoryRunStateStore::new());
        for run_id in ["paused-a", "paused-b"] {
            let mut run = durable_run_record(run_id);
            run.status = STATUS_PAUSED.into();
            run.waiting_for = None;
            store.insert_run(run).await.unwrap();
        }

        let resume_a = {
            let store = store.clone();
            tokio::spawn(async move {
                store
                    .update_run_status_with_event_if_current_unless_session_blocked(
                        GuardedRunStatusTransitionRequest {
                            user_id: "u1",
                            run_id: "paused-a",
                            session_id: "s1",
                            expected_statuses: &[STATUS_PAUSED],
                            status: STATUS_RUNNING,
                            waiting_for: None,
                            error_message: None,
                            event: serde_json::json!({"event_type": "run_resumed", "data": {}}),
                        },
                    )
                    .await
                    .unwrap()
            })
        };
        let resume_b = {
            let store = store.clone();
            tokio::spawn(async move {
                store
                    .update_run_status_with_event_if_current_unless_session_blocked(
                        GuardedRunStatusTransitionRequest {
                            user_id: "u1",
                            run_id: "paused-b",
                            session_id: "s1",
                            expected_statuses: &[STATUS_PAUSED],
                            status: STATUS_RUNNING,
                            waiting_for: None,
                            error_message: None,
                            event: serde_json::json!({"event_type": "run_resumed", "data": {}}),
                        },
                    )
                    .await
                    .unwrap()
            })
        };

        let outcomes = [resume_a.await.unwrap(), resume_b.await.unwrap()];
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| **outcome == GuardedRunStatusTransition::Updated)
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| **outcome == GuardedRunStatusTransition::SessionBlocked)
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn in_memory_list_user_runs_cursor_seek_paginates_without_count() {
        let store = InMemoryRunStateStore::new();
        for i in 0..5 {
            let mut run = durable_run_record(&format!("run-{i}"));
            run.status = STATUS_COMPLETED.into();
            run.session_id = format!("session-{i}");
            run.created_at = "2026-07-03T10:00:00.000000Z".into();
            run.updated_at = "2026-07-03T10:00:00.000000Z".into();
            store.insert_run(run).await.unwrap();
        }

        let first = store.list_user_runs_cursor("u1", 2, None).await.unwrap();
        assert_eq!(first.total, None);
        assert_eq!(
            first
                .runs
                .iter()
                .map(|run| run.run_id.as_str())
                .collect::<Vec<_>>(),
            vec!["run-4", "run-3"]
        );
        assert_eq!(
            first
                .next_cursor
                .as_ref()
                .map(|cursor| cursor.run_id.as_str()),
            Some("run-3")
        );

        let second = store
            .list_user_runs_cursor("u1", 2, first.next_cursor)
            .await
            .unwrap();
        assert_eq!(
            second
                .runs
                .iter()
                .map(|run| run.run_id.as_str())
                .collect::<Vec<_>>(),
            vec!["run-2", "run-1"]
        );

        let third = store
            .list_user_runs_cursor("u1", 2, second.next_cursor)
            .await
            .unwrap();
        assert_eq!(
            third
                .runs
                .iter()
                .map(|run| run.run_id.as_str())
                .collect::<Vec<_>>(),
            vec!["run-0"]
        );
        assert!(third.next_cursor.is_none());

        let without_total = store.list_user_runs_cursor("u1", 2, None).await.unwrap();
        assert_eq!(without_total.total, None);
    }

    #[tokio::test]
    async fn in_memory_active_session_cursor_pages_605_runs_after_prior_page_mutation() {
        let store = InMemoryRunStateStore::new();
        for index in 0..605 {
            let mut run = durable_run_record(&format!("active-{index:03}"));
            run.parent_run_id = Some("root".into());
            run.root_run_id = Some("root".into());
            run.ancestor_path = Some("root".into());
            run.depth = 1;
            run.status = STATUS_RUNNING.into();
            run.updated_at = format!("2026-07-03T10:{:02}:00.000000Z", index / 60);
            store.runs.write().await.insert(run.run_id.clone(), run);
        }
        let mut unrelated = durable_run_record("unrelated-session");
        unrelated.session_id = "s2".into();
        store
            .runs
            .write()
            .await
            .insert(unrelated.run_id.clone(), unrelated);

        let first = store
            .list_active_session_runs_cursor("u1", "s1", 200, None)
            .await
            .unwrap();
        assert_eq!(first.runs.len(), 200);
        let first_cursor = first.next_cursor.expect("more active runs");
        {
            let mut runs = store.runs.write().await;
            for run in &first.runs {
                let stored = runs.get_mut(&run.run_id).unwrap();
                stored.status = STATUS_CANCELLED.into();
                stored.updated_at = "2026-07-18T12:00:00.000000Z".into();
            }
        }

        let second = store
            .list_active_session_runs_cursor("u1", "s1", 200, Some(first_cursor))
            .await
            .unwrap();
        assert_eq!(second.runs.len(), 200);
        let third = store
            .list_active_session_runs_cursor(
                "u1",
                "s1",
                200,
                Some(second.next_cursor.expect("third page cursor")),
            )
            .await
            .unwrap();
        assert_eq!(third.runs.len(), 200);
        let fourth = store
            .list_active_session_runs_cursor(
                "u1",
                "s1",
                200,
                Some(third.next_cursor.expect("fourth page cursor")),
            )
            .await
            .unwrap();
        assert_eq!(fourth.runs.len(), 5);
        assert!(fourth.next_cursor.is_none());
    }

    #[tokio::test]
    async fn in_memory_session_run_snapshot_is_scoped_bounded_and_keeps_active_work() {
        let store = InMemoryRunStateStore::new();

        let mut root = durable_run_record("root");
        root.status = STATUS_COMPLETED.into();
        root.created_at = "2026-07-11T00:00:00Z".into();
        root.updated_at = "2026-07-11T00:01:00Z".into();
        store.insert_run(root).await.unwrap();

        let mut old_terminal = durable_run_record("old-terminal");
        old_terminal.status = STATUS_FAILED.into();
        old_terminal.parent_run_id = Some("root".into());
        old_terminal.root_run_id = Some("root".into());
        old_terminal.depth = 1;
        old_terminal.created_at = "2026-07-11T00:02:00Z".into();
        old_terminal.updated_at = "2026-07-11T00:03:00Z".into();
        store.insert_run(old_terminal).await.unwrap();

        let mut active = durable_run_record("active-child");
        active.status = STATUS_WAITING.into();
        active.parent_run_id = Some("root".into());
        active.root_run_id = Some("root".into());
        active.depth = 1;
        active.created_at = "2026-07-11T00:04:00Z".into();
        active.updated_at = "2026-07-11T00:05:00Z".into();
        store.insert_run(active).await.unwrap();

        let mut other_session = durable_run_record("other-session");
        other_session.session_id = "s2".into();
        store.insert_run(other_session).await.unwrap();

        let mut other_user = durable_run_record("other-user");
        other_user.user_id = "u2".into();
        store.insert_run(other_user).await.unwrap();

        let page = store.list_session_runs("u1", "s1", 2).await.unwrap();
        assert!(page.truncated);
        assert_eq!(page.limit, 2);
        assert_eq!(
            page.runs
                .iter()
                .map(|run| run.run_id.as_str())
                .collect::<Vec<_>>(),
            vec!["root", "old-terminal", "active-child"]
        );
        assert!(page.runs.iter().any(|run| run.run_id == "active-child"));
        assert!(page.runs.iter().all(|run| run.user_id == "u1"));
        assert!(page.runs.iter().all(|run| run.session_id == "s1"));
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
    async fn database_list_user_runs_cursor_seek_paginates_tied_updated_at_on_matrixone() {
        let (store, pool) = setup_database_run_state_store_it().await;
        let user_id = format!("runs-it-cursor-user-{}", Uuid::new_v4());
        let prefix = format!("runs-it-cursor-run-{}", Uuid::new_v4());
        let run_ids = (0..5)
            .map(|idx| format!("{prefix}-{idx}"))
            .collect::<Vec<_>>();
        let tied_ts = "2026-07-03 10:00:00.123456";

        for (idx, run_id) in run_ids.iter().enumerate() {
            cleanup_database_run_fixture(&pool, &user_id, run_id).await;
            let mut run = durable_run_record(run_id);
            run.user_id = user_id.clone();
            run.session_id = format!("runs-it-cursor-session-{idx}");
            run.status = STATUS_COMPLETED.into();
            store.insert_run(run).await.expect("insert cursor run");
            sqlx::query(
                "UPDATE agent_runs SET created_at = ?, updated_at = ? \
                 WHERE user_id = ? AND run_id = ?",
            )
            .bind(tied_ts)
            .bind(tied_ts)
            .bind(&user_id)
            .bind(run_id)
            .execute(pool.get())
            .await
            .expect("force tied run timestamp");
        }

        let first = store
            .list_user_runs_cursor(&user_id, 2, None)
            .await
            .expect("first cursor page");
        assert_eq!(
            first
                .runs
                .iter()
                .map(|run| run.run_id.as_str())
                .collect::<Vec<_>>(),
            vec![run_ids[4].as_str(), run_ids[3].as_str()]
        );
        let first_cursor = first.next_cursor.expect("first page cursor");
        assert_eq!(first_cursor.run_id, run_ids[3]);

        let second = store
            .list_user_runs_cursor(&user_id, 2, Some(first_cursor))
            .await
            .expect("second cursor page");
        assert_eq!(
            second
                .runs
                .iter()
                .map(|run| run.run_id.as_str())
                .collect::<Vec<_>>(),
            vec![run_ids[2].as_str(), run_ids[1].as_str()]
        );
        let second_cursor = second.next_cursor.expect("second page cursor");
        assert_eq!(second_cursor.run_id, run_ids[1]);

        let third = store
            .list_user_runs_cursor(&user_id, 2, Some(second_cursor))
            .await
            .expect("third cursor page");
        assert_eq!(
            third
                .runs
                .iter()
                .map(|run| run.run_id.as_str())
                .collect::<Vec<_>>(),
            vec![run_ids[0].as_str()]
        );
        assert!(third.next_cursor.is_none());

        for run_id in &run_ids {
            cleanup_database_run_fixture(&pool, &user_id, run_id).await;
        }
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
    async fn database_active_session_cursor_pages_605_runs_after_first_page_mutation() {
        let (store, pool) = setup_database_run_state_store_it().await;
        // Production identity columns are VARCHAR(64); keep the real-DB
        // fixture inside the same wire contract instead of relying on an
        // in-memory-only oversized identifier.
        let user_id = format!("runs-ac-u-{}", Uuid::new_v4());
        let session_id = format!("runs-ac-s-{}", Uuid::new_v4());
        let root_run_id = format!("runs-ac-r-{}", Uuid::new_v4());

        let mut insert = sqlx::QueryBuilder::<sqlx::MySql>::new(
            "INSERT INTO agent_runs \
             (run_id, user_id, session_id, parent_run_id, root_run_id, ancestor_path, depth, status) ",
        );
        insert.push_values(0..605, |mut row, index| {
            row.push_bind(format!("active-{index:03}-{}", Uuid::new_v4()))
                .push_bind(&user_id)
                .push_bind(&session_id)
                .push_bind(&root_run_id)
                .push_bind(&root_run_id)
                .push_bind(&root_run_id)
                .push_bind(1_i32)
                .push_bind(STATUS_RUNNING);
        });
        insert
            .build()
            .execute(pool.get())
            .await
            .expect("insert 605 active descendants");

        let first = store
            .list_active_session_runs_cursor(&user_id, &session_id, 200, None)
            .await
            .expect("first active page");
        assert_eq!(first.runs.len(), 200);
        let first_cursor = first.next_cursor.expect("second page cursor");

        let mut cancel_first =
            sqlx::QueryBuilder::<sqlx::MySql>::new("UPDATE agent_runs SET status = ");
        cancel_first
            .push_bind(STATUS_CANCELLED)
            .push(", updated_at = NOW(6) WHERE user_id = ")
            .push_bind(&user_id)
            .push(" AND run_id IN (");
        {
            let mut ids = cancel_first.separated(",");
            for run in &first.runs {
                ids.push_bind(&run.run_id);
            }
        }
        cancel_first.push(")");
        cancel_first
            .build()
            .execute(pool.get())
            .await
            .expect("mutate first page");

        let second = store
            .list_active_session_runs_cursor(&user_id, &session_id, 200, Some(first_cursor))
            .await
            .expect("second active page");
        assert_eq!(second.runs.len(), 200);
        let third = store
            .list_active_session_runs_cursor(&user_id, &session_id, 200, second.next_cursor)
            .await
            .expect("third active page");
        assert_eq!(third.runs.len(), 200);
        let fourth = store
            .list_active_session_runs_cursor(&user_id, &session_id, 200, third.next_cursor)
            .await
            .expect("fourth active page");
        assert_eq!(fourth.runs.len(), 5);
        assert!(fourth.next_cursor.is_none());

        sqlx::query("DELETE FROM agent_runs WHERE user_id = ?")
            .bind(&user_id)
            .execute(pool.get())
            .await
            .expect("cleanup active cursor fixtures");
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
    async fn database_session_run_snapshot_keeps_active_work_ahead_of_terminal_history() {
        let (store, pool) = setup_database_run_state_store_it().await;
        let user_id = format!("runs-it-tree-user-{}", Uuid::new_v4());
        let session_id = format!("runs-it-tree-session-{}", Uuid::new_v4());
        let active_id = format!("runs-it-tree-active-{}", Uuid::new_v4());
        let newest_terminal_id = format!("runs-it-tree-terminal-new-{}", Uuid::new_v4());
        let older_terminal_id = format!("runs-it-tree-terminal-old-{}", Uuid::new_v4());
        let run_ids = [&active_id, &newest_terminal_id, &older_terminal_id];

        for run_id in run_ids {
            cleanup_database_run_fixture(&pool, &user_id, run_id).await;
        }
        let fixtures = [
            (&active_id, STATUS_WAITING, "2026-07-11 00:00:01.000000"),
            (
                &newest_terminal_id,
                STATUS_COMPLETED,
                "2026-07-11 00:00:05.000000",
            ),
            (
                &older_terminal_id,
                STATUS_FAILED,
                "2026-07-11 00:00:04.000000",
            ),
        ];
        for (run_id, status, updated_at) in fixtures {
            let mut run = durable_run_record(run_id);
            run.user_id = user_id.clone();
            run.session_id = session_id.clone();
            run.status = status.into();
            store.insert_run(run).await.unwrap();
            sqlx::query("UPDATE agent_runs SET updated_at = ? WHERE user_id = ? AND run_id = ?")
                .bind(updated_at)
                .bind(&user_id)
                .bind(run_id)
                .execute(pool.get())
                .await
                .unwrap();
        }

        let page = store
            .list_session_runs(&user_id, &session_id, 2)
            .await
            .unwrap();
        assert!(page.truncated);
        assert!(page.runs.iter().any(|run| run.run_id == active_id));
        assert!(page.runs.iter().any(|run| run.run_id == newest_terminal_id));
        assert!(!page.runs.iter().any(|run| run.run_id == older_terminal_id));

        for run_id in run_ids {
            cleanup_database_run_fixture(&pool, &user_id, run_id).await;
        }
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
    async fn database_session_run_snapshot_ranks_delegated_as_terminal() {
        let (store, pool) = setup_database_run_state_store_it().await;
        let user_id = format!("runs-it-delegated-user-{}", Uuid::new_v4());
        let session_id = format!("runs-it-delegated-session-{}", Uuid::new_v4());
        let active_id = format!("runs-it-delegated-active-{}", Uuid::new_v4());
        let delegated_id = format!("runs-it-delegated-terminal-{}", Uuid::new_v4());
        let completed_id = format!("runs-it-completed-terminal-{}", Uuid::new_v4());

        for (run_id, status, updated_at) in [
            (&active_id, STATUS_WAITING, "2026-07-11 00:00:01.000000"),
            (
                &delegated_id,
                STATUS_DELEGATED,
                "2026-07-11 00:00:05.000000",
            ),
            (
                &completed_id,
                STATUS_COMPLETED,
                "2026-07-11 00:00:04.000000",
            ),
        ] {
            cleanup_database_run_fixture(&pool, &user_id, run_id).await;
            let mut run = durable_run_record(run_id);
            run.user_id = user_id.clone();
            run.session_id = session_id.clone();
            run.status = status.into();
            store.insert_run(run).await.unwrap();
            sqlx::query("UPDATE agent_runs SET updated_at = ? WHERE user_id = ? AND run_id = ?")
                .bind(updated_at)
                .bind(&user_id)
                .bind(run_id)
                .execute(pool.get())
                .await
                .unwrap();
        }

        let page = store
            .list_session_runs(&user_id, &session_id, 2)
            .await
            .unwrap();
        assert!(page.runs.iter().any(|run| run.run_id == active_id));
        assert!(page.runs.iter().any(|run| run.run_id == delegated_id));
        assert!(!page.runs.iter().any(|run| run.run_id == completed_id));

        for run_id in [&active_id, &delegated_id, &completed_id] {
            cleanup_database_run_fixture(&pool, &user_id, run_id).await;
        }
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
    async fn database_terminal_state_is_immutable_across_all_transition_paths() {
        let (store, pool) = setup_database_run_state_store_it().await;
        let user_id = format!("runs-it-terminal-user-{}", Uuid::new_v4());
        let session_id = format!("runs-it-terminal-session-{}", Uuid::new_v4());
        let run_id = format!("runs-it-terminal-run-{}", Uuid::new_v4());
        cleanup_database_run_fixture(&pool, &user_id, &run_id).await;

        let mut run = durable_run_record(&run_id);
        run.user_id = user_id.clone();
        run.session_id = session_id.clone();
        store
            .insert_run(run)
            .await
            .expect("insert terminal fixture");
        assert!(
            store
                .update_run_status_if_current(
                    &user_id,
                    &run_id,
                    &[STATUS_RUNNING],
                    STATUS_DELEGATED,
                    None,
                    None,
                )
                .await
                .expect("settle fixture as delegated")
        );

        let assert_terminal_conflict = |error: String| {
            assert!(
                error.contains("terminal state immutability violated"),
                "unexpected transition error: {error}"
            );
        };
        assert_terminal_conflict(
            store
                .update_run_status(&user_id, &run_id, STATUS_RUNNING, None, None)
                .await
                .expect_err("unguarded transition must not resurrect a terminal run"),
        );
        assert_terminal_conflict(
            store
                .update_run_status_if_current(
                    &user_id,
                    &run_id,
                    &[STATUS_DELEGATED],
                    STATUS_RUNNING,
                    None,
                    None,
                )
                .await
                .expect_err("CAS transition must not resurrect a terminal run"),
        );
        assert_terminal_conflict(
            store
                .update_run_status_with_event_if_current(
                    &user_id,
                    &run_id,
                    &[STATUS_DELEGATED],
                    STATUS_RUNNING,
                    None,
                    None,
                    make_event("run_resumed", json!({})),
                )
                .await
                .expect_err("event transition must not resurrect a terminal run"),
        );
        assert_terminal_conflict(
            store
                .update_run_status_with_events_if_current(
                    &user_id,
                    &run_id,
                    &[STATUS_DELEGATED],
                    STATUS_RUNNING,
                    None,
                    None,
                    &[make_event("run_resumed", json!({}))],
                )
                .await
                .expect_err("event batch must not resurrect a terminal run"),
        );
        assert_terminal_conflict(
            store
                .update_run_status_with_event_if_current_unless_session_blocked(
                    GuardedRunStatusTransitionRequest {
                        user_id: &user_id,
                        run_id: &run_id,
                        session_id: &session_id,
                        expected_statuses: &[STATUS_DELEGATED],
                        status: STATUS_RUNNING,
                        waiting_for: None,
                        error_message: None,
                        event: make_event("run_resumed", json!({})),
                    },
                )
                .await
                .expect_err("guarded transition must not resurrect a terminal run"),
        );

        assert!(
            !store
                .save_checkpoint(
                    &user_id,
                    &run_id,
                    r#"{"version":"checkpoint_v2","graceful":true,"last_batch_id":"terminal"}"#,
                )
                .await
                .expect("terminal checkpoint attempt should be a visible no-op")
        );
        let loaded = store
            .load_run(&user_id, &run_id)
            .await
            .expect("load terminal fixture")
            .expect("terminal fixture exists");
        assert_eq!(loaded.status, STATUS_DELEGATED);
        assert!(loaded.events.is_empty());
        assert!(loaded.checkpoint_json.is_none());
        assert!(loaded.checkpoint_version.is_none());
        assert!(
            store
                .load_latest_checkpoint(&user_id, &run_id, None)
                .await
                .expect("load checkpoint projection")
                .is_none()
        );

        cleanup_database_run_fixture(&pool, &user_id, &run_id).await;
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
    async fn database_agent_recovery_batches_selected_events_on_matrixone() {
        let (store, pool) = setup_database_run_state_store_it().await;
        let user_id = format!("runs-it-recovery-user-{}", Uuid::new_v4());
        let session_id = format!("runs-it-recovery-session-{}", Uuid::new_v4());
        let root_id = format!("runs-it-recovery-root-{}", Uuid::new_v4());
        let child_id = format!("runs-it-recovery-child-{}", Uuid::new_v4());
        for run_id in [&root_id, &child_id] {
            cleanup_database_run_fixture(&pool, &user_id, run_id).await;
        }
        let mut root = durable_run_record(&root_id);
        root.user_id = user_id.clone();
        root.session_id = session_id.clone();
        store.insert_run(root).await.unwrap();
        let mut child = durable_run_record(&child_id);
        child.user_id = user_id.clone();
        child.session_id = session_id.clone();
        child.parent_run_id = Some(root_id.clone());
        child.root_run_id = Some(root_id.clone());
        child.depth = 1;
        child.agent_id = Some("reviewer".into());
        store.insert_run(child).await.unwrap();
        store
            .append_events_batch(
                &user_id,
                &root_id,
                &[
                    serde_json::json!({
                        "type": "agent_spawned",
                        "run_id": child_id,
                        "agent_id": "reviewer",
                        "agent_type": "code-review",
                        "description": "review storage",
                        "fanout_slot": {"group_id":"review","target_count":1,"slot_index":0,"slot_id":"correctness"}
                    }),
                    serde_json::json!({"type":"agent_progress","status":"noise"}),
                ],
            )
            .await
            .unwrap();
        let noisy_terminal_events = (0..40)
            .map(|attempt| {
                serde_json::json!({
                    "event_type": "run_finished",
                    "data": {"attempt": attempt, "status": "still-reconciling"}
                })
            })
            .collect::<Vec<_>>();
        store
            .append_events_batch(&user_id, &child_id, &noisy_terminal_events)
            .await
            .unwrap();
        let active_page = store
            .load_session_agent_recovery(&user_id, &session_id, 2)
            .await
            .expect("MatrixOne recovery batch query");
        let root = active_page
            .runs
            .iter()
            .find(|run| run.run_id == root_id)
            .unwrap();
        let child = active_page
            .runs
            .iter()
            .find(|run| run.run_id == child_id)
            .unwrap();
        assert_eq!(
            root.events.len(),
            1,
            "unneeded progress events stay out of recovery"
        );
        assert_eq!(root.events[0]["type"], "agent_spawned");
        assert_eq!(child.status, STATUS_RUNNING);
        assert_eq!(
            child.events.len(),
            1,
            "recovery keeps only the latest fact for each terminal event type"
        );
        assert_eq!(child.events[0]["data"]["attempt"], 39);

        assert!(
            store
                .update_run_status_with_events_if_current(
                    &user_id,
                    &child_id,
                    &[STATUS_RUNNING],
                    STATUS_COMPLETED,
                    None,
                    None,
                    &[
                        serde_json::json!({"event_type":"text_done","data":{"full_text":"durable finding"}}),
                        serde_json::json!({"event_type":"run_finished","data":{"tool_call_count":3}}),
                    ],
                )
                .await
                .unwrap()
        );
        let completed_page = store
            .load_session_agent_recovery(&user_id, &session_id, 200)
            .await
            .expect("MatrixOne recovery refresh query");
        let child = completed_page
            .runs
            .iter()
            .find(|run| run.run_id == child_id)
            .unwrap();
        assert_eq!(child.status, STATUS_COMPLETED);
        assert_eq!(child.events.len(), 2);
        assert_eq!(child.events[0]["event_type"], "text_done");
        assert_eq!(child.events[1]["event_type"], "run_finished");

        for run_id in [&root_id, &child_id] {
            cleanup_database_run_fixture(&pool, &user_id, run_id).await;
        }
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
    async fn database_recovery_claims_are_bounded_disjoint_and_lease_safe_on_matrixone() {
        let (_, pool) = setup_database_run_state_store_it().await;
        let user_id = format!("rcu-{}", Uuid::new_v4());
        let prefix = format!("rc-{}", Uuid::new_v4());
        let recoverable_ids = (0..8)
            .map(|index| format!("{prefix}-r{index}"))
            .collect::<Vec<_>>();
        let live_id = format!("{prefix}-live");
        let all_ids = recoverable_ids
            .iter()
            .chain(std::iter::once(&live_id))
            .cloned()
            .collect::<Vec<_>>();

        let fixture_store =
            DatabaseRunStateStore::new(pool.clone()).with_owner_pod_id("runs-it-claim-fixture");
        for (index, run_id) in all_ids.iter().enumerate() {
            cleanup_database_run_fixture(&pool, &user_id, run_id).await;
            let mut run = durable_run_record(run_id);
            run.user_id = user_id.clone();
            run.session_id = format!("{prefix}-s{index}");
            fixture_store
                .insert_run(run)
                .await
                .expect("insert claim run");
        }
        for (index, run_id) in recoverable_ids.iter().enumerate() {
            sqlx::query(
                "UPDATE agent_runs
                 SET owner_pod_id = 'expired-owner',
                     owner_lease_expires_at = NOW(6) - INTERVAL 1 SECOND,
                     updated_at = NOW(6) - INTERVAL ? SECOND
                 WHERE user_id = ? AND run_id = ?",
            )
            .bind(100_i64 - index as i64)
            .bind(&user_id)
            .bind(run_id)
            .execute(pool.get())
            .await
            .expect("expire recovery fixture lease");
        }
        sqlx::query(
            "UPDATE agent_runs
             SET owner_pod_id = 'live-owner',
                 owner_lease_expires_at = NOW(6) + INTERVAL 60 SECOND
             WHERE user_id = ? AND run_id = ?",
        )
        .bind(&user_id)
        .bind(&live_id)
        .execute(pool.get())
        .await
        .expect("protect live fixture lease");

        let owner_a = DatabaseRunStateStore::new(pool.clone()).with_owner_pod_id("claim-pod-a");
        let owner_b = DatabaseRunStateStore::new(pool.clone()).with_owner_pod_id("claim-pod-b");
        let (claimed_a, claimed_b) = tokio::join!(
            owner_a.claim_recoverable_active_runs(4),
            owner_b.claim_recoverable_active_runs(4)
        );
        let claimed_a = claimed_a.expect("pod A recovery claim");
        let claimed_b = claimed_b.expect("pod B recovery claim");
        assert!(claimed_a.len() <= 4);
        assert!(claimed_b.len() <= 4);
        let ids_a = claimed_a
            .iter()
            .map(|run| run.run_id.as_str())
            .collect::<HashSet<_>>();
        let ids_b = claimed_b
            .iter()
            .map(|run| run.run_id.as_str())
            .collect::<HashSet<_>>();
        assert!(ids_a.is_disjoint(&ids_b), "pods must claim disjoint runs");
        assert_eq!(
            ids_a.len() + ids_b.len(),
            recoverable_ids.len(),
            "claim collision retries should distribute the complete bounded working set"
        );
        assert!(!ids_a.contains(live_id.as_str()));
        assert!(!ids_b.contains(live_id.as_str()));
        assert!(claimed_a.iter().all(|run| {
            run.owner_pod_id.as_deref() == Some("claim-pod-a") && run.run_generation == 1
        }));
        assert!(claimed_b.iter().all(|run| {
            run.owner_pod_id.as_deref() == Some("claim-pod-b") && run.run_generation == 1
        }));

        for run_id in &all_ids {
            cleanup_database_run_fixture(&pool, &user_id, run_id).await;
        }
        sqlx::query("DELETE FROM agent_session_execution_slots WHERE user_id = ?")
            .bind(&user_id)
            .execute(pool.get())
            .await
            .expect("cleanup recovery claim slots");
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
    async fn database_interaction_resolution_converges_across_pods_on_matrixone() {
        let (_, pool) = setup_database_run_state_store_it().await;
        let user_id = format!("riu-{}", Uuid::new_v4());
        let run_id = format!("rir-{}", Uuid::new_v4());
        let session_id = format!("ris-{}", Uuid::new_v4());
        cleanup_database_run_fixture(&pool, &user_id, &run_id).await;

        let fixture =
            DatabaseRunStateStore::new(pool.clone()).with_owner_pod_id("interaction-owner");
        let mut run = durable_run_record(&run_id);
        run.user_id = user_id.clone();
        run.session_id = session_id;
        fixture
            .insert_run(run)
            .await
            .expect("insert interaction run");
        assert!(
            fixture
                .update_run_status_with_event_if_current(
                    &user_id,
                    &run_id,
                    &[STATUS_RUNNING],
                    STATUS_WAITING,
                    Some("tool_approval"),
                    None,
                    json!({
                        "event_type": "approval_required",
                        "data": {
                            "request_id": "cross-pod-approval",
                            "tool": "bash",
                            "approval_kind": "standard"
                        }
                    }),
                )
                .await
                .expect("persist approval wait")
        );

        let pod_a = DatabaseRunStateStore::new(pool.clone()).with_owner_pod_id("interaction-pod-a");
        let pod_b = DatabaseRunStateStore::new(pool.clone()).with_owner_pod_id("interaction-pod-b");
        let allow = json!({
            "request_id": "cross-pod-approval",
            "outcome": "approved",
            "decision": "allow",
            "tool": "bash",
            "approval_kind": "standard"
        });
        let deny = json!({
            "request_id": "cross-pod-approval",
            "outcome": "denied",
            "decision": "deny",
            "reason": "review rejected",
            "tool": "bash",
            "approval_kind": "standard"
        });
        let (outcome_a, outcome_b) = tokio::join!(
            pod_a.resolve_run_interaction(
                &user_id,
                &run_id,
                "cross-pod-approval",
                DurableRunInteractionKind::Approval,
                allow,
            ),
            pod_b.resolve_run_interaction(
                &user_id,
                &run_id,
                "cross-pod-approval",
                DurableRunInteractionKind::Approval,
                deny,
            )
        );
        let outcomes = [outcome_a.unwrap(), outcome_b.unwrap()];
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(
                    outcome,
                    DurableRunInteractionResolveOutcome::Resolved(_)
                ))
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(
                    outcome,
                    DurableRunInteractionResolveOutcome::Conflict(_)
                ))
                .count(),
            1
        );
        let durable = fixture.load_run(&user_id, &run_id).await.unwrap().unwrap();
        assert_eq!(durable.status, STATUS_RUNNING);
        assert_eq!(durable.waiting_for, None);
        assert!(matches!(
            durable.owner_pod_id.as_deref(),
            Some("interaction-pod-a" | "interaction-pod-b")
        ));
        assert!(durable.owner_lease_expires_at.is_some());
        assert_eq!(durable.run_generation, 1);
        assert_eq!(
            durable
                .events
                .iter()
                .filter(|event| extract_event_type(event) == "approval_resolved")
                .count(),
            1
        );
        assert!(
            fixture
                .load_run_interaction_event(
                    &user_id,
                    &run_id,
                    "cross-pod-approval",
                    "approval_resolved",
                )
                .await
                .unwrap()
                .is_some(),
            "a fresh pod must find the canonical terminal interaction by normalized identity"
        );

        cleanup_database_run_fixture(&pool, &user_id, &run_id).await;
        sqlx::query("DELETE FROM agent_session_execution_slots WHERE user_id = ?")
            .bind(&user_id)
            .execute(pool.get())
            .await
            .expect("cleanup interaction slot");
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
    async fn database_run_control_projection_batches_deep_lineage_on_matrixone() {
        let (store, pool) = setup_database_run_state_store_it().await;
        let user_id = format!("runs-it-control-user-{}", Uuid::new_v4());
        let session_id = format!("runs-it-control-session-{}", Uuid::new_v4());
        let root_id = format!("runs-it-control-root-{}", Uuid::new_v4());
        let child_id = format!("runs-it-control-child-{}", Uuid::new_v4());
        let grandchild_id = format!("runs-it-control-grandchild-{}", Uuid::new_v4());
        for run_id in [&root_id, &child_id, &grandchild_id] {
            cleanup_database_run_fixture(&pool, &user_id, run_id).await;
        }

        let mut root = durable_run_record(&root_id);
        root.user_id = user_id.clone();
        root.session_id = session_id.clone();
        store.insert_run(root).await.unwrap();
        let mut child = durable_run_record(&child_id);
        child.user_id = user_id.clone();
        child.session_id = session_id.clone();
        child.parent_run_id = Some(root_id.clone());
        child.root_run_id = Some(root_id.clone());
        child.ancestor_path = Some(format!("{root_id}/{child_id}"));
        child.depth = 1;
        store.insert_run(child).await.unwrap();
        let mut grandchild = durable_run_record(&grandchild_id);
        grandchild.user_id = user_id.clone();
        grandchild.session_id = session_id;
        grandchild.parent_run_id = Some(child_id.clone());
        grandchild.root_run_id = Some(root_id.clone());
        grandchild.ancestor_path = Some(format!("{root_id}/{child_id}/{grandchild_id}"));
        grandchild.depth = 2;
        store.insert_run(grandchild).await.unwrap();
        store
            .append_events_batch(
                &user_id,
                &grandchild_id,
                &(0..32)
                    .map(
                        |idx| serde_json::json!({"event_type":"agent_progress","data":{"idx":idx}}),
                    )
                    .collect::<Vec<_>>(),
            )
            .await
            .unwrap();

        store
            .update_run_status(&user_id, &root_id, STATUS_PAUSED, Some("user_resume"), None)
            .await
            .unwrap();
        let target = store
            .load_run_control(&user_id, &grandchild_id)
            .await
            .unwrap()
            .expect("grandchild control projection");
        assert_eq!(target.parent_run_id.as_deref(), Some(child_id.as_str()));
        let expected_path = format!("{root_id}/{child_id}/{grandchild_id}");
        assert_eq!(
            target.ancestor_path.as_deref(),
            Some(expected_path.as_str())
        );
        let ancestors = store
            .load_run_controls(&user_id, &[root_id.clone(), child_id.clone()])
            .await
            .unwrap();
        assert_eq!(ancestors.len(), 2);
        assert!(
            ancestors
                .iter()
                .any(|run| { run.run_id == root_id && run.status == STATUS_PAUSED })
        );
        assert!(
            ancestors
                .iter()
                .any(|run| { run.run_id == child_id && run.status == STATUS_RUNNING })
        );

        for run_id in [&root_id, &child_id, &grandchild_id] {
            cleanup_database_run_fixture(&pool, &user_id, run_id).await;
        }
    }

    #[test]
    fn run_list_sql_contract_uses_seek_cursor_not_offset() {
        let order_sql = RUN_LIST_ORDER_SQL.to_ascii_uppercase();
        let cursor_sql = RUN_LIST_CURSOR_PREDICATE_SQL.to_ascii_uppercase();
        assert!(!order_sql.contains(" OFFSET "));
        assert!(!cursor_sql.contains(" OFFSET "));
        assert!(order_sql.contains("UPDATED_AT DESC"));
        assert!(order_sql.contains("RUN_ID DESC"));
        assert!(cursor_sql.contains("RUN_ID < ?"));
        assert_eq!(
            RUN_LIST_CURSOR_PREDICATE_SQL,
            " AND (updated_at < ? OR (updated_at = ? AND run_id < ?))"
        );
    }

    #[test]
    fn checkpoint_metadata_validates_versioned_checkpoints() {
        assert!(checkpoint_metadata("run", r#"{"version":"bad"}"#).is_err());

        let (kind, version, idempotency_key) = checkpoint_metadata(
            "run",
            r#"{"version":"checkpoint_v2","graceful":true,"last_batch_id":"batch-1"}"#,
        )
        .expect("valid checkpoint");
        assert_eq!(kind, "resume");
        assert_eq!(version, "checkpoint_v2");
        assert_eq!(idempotency_key, "checkpoint:run:resume:batch-1");

        let (kind, version, _) =
            checkpoint_metadata("run", r#"{"phase":"shutdown"}"#).expect("phase checkpoint");
        assert_eq!(kind, "phase");
        assert_eq!(version, "phase_checkpoint_v1");
    }

    /// Covers all event types that reach the client via transform_run_event_for_client:
    /// type mapping, field preservation, pass-through, drop, and error-surface semantics.
    #[test]
    fn event_transform_to_client_surface_covers_all_event_types() {
        type EventTransformCase<'a> = (&'a str, serde_json::Value, &'a dyn Fn(&serde_json::Value));
        let cases: Vec<EventTransformCase<'_>> = vec![
            // ── text / thinking: durable journal → client SSE shape ──
            (
                "text_delta",
                make_event("text_delta", json!({"chunk": "hi"})),
                &|o| {
                    assert_eq!(o["type"], "text_delta");
                    assert_eq!(o["content"], "hi");
                },
            ),
            (
                "text_delta (no chunk)",
                make_event("text_delta", json!({})),
                &|o| {
                    assert_eq!(o["content"], "");
                },
            ),
            (
                "assistant_delta→text_delta",
                make_event("assistant_delta", json!({"text": "hi"})),
                &|o| {
                    assert_eq!(o["type"], "text_delta");
                    assert_eq!(o["content"], "hi");
                },
            ),
            (
                "text_done",
                make_event("text_done", json!({"full_text": "all"})),
                &|o| {
                    assert_eq!(o["type"], "text_done");
                    assert_eq!(o["full_text"], "all");
                },
            ),
            (
                "reasoning_message_content",
                make_event("reasoning_message_content", json!({"content": "think"})),
                &|o| {
                    assert_eq!(o["type"], "reasoning_message_content");
                    assert_eq!(o["content"], "think");
                },
            ),
            (
                "thinking_delta",
                make_event("thinking_delta", json!({"chunk": "t"})),
                &|o| {
                    assert_eq!(o["type"], "thinking_delta");
                    assert_eq!(o["content"], "t");
                },
            ),
            (
                "thinking_done",
                make_event("thinking_done", json!({"full_text": "all think"})),
                &|o| {
                    assert_eq!(o["type"], "thinking_done");
                },
            ),
            (
                "reasoning_done",
                make_event("reasoning_done", json!({"full_text": "reason"})),
                &|o| {
                    assert_eq!(o["type"], "reasoning_done");
                },
            ),
            // ── tool events ──
            (
                "tool_call_start",
                make_event(
                    "tool_call_start",
                    json!({
                        "name": "bash", "tool_call_id": "c1", "args": {"command": "ls"},
                        "workspace": {"kind": "server_sandbox", "cwd": "/tmp/astra-workspaces/run-1"},
                        "executor": {"kind": "server_local", "transport": "server_local"},
                        "transport": "server_local"
                    }),
                ),
                &|o| {
                    assert_eq!(o["type"], "tool_call_start");
                    assert_eq!(o["tool"], "bash");
                    assert_eq!(o["call_id"], "c1");
                    assert_eq!(o["arguments"]["command"], "ls");
                    assert_eq!(o["workspace"]["kind"], "server_sandbox");
                },
            ),
            (
                "tool_result→tool_call_end",
                make_event(
                    "tool_result",
                    json!({
                        "tool_call_id": "c1", "name": "bash", "output": "ok", "success": true,
                        "duration_ms": 42,
                        "workspace": {"kind": "edge_workspace", "cwd": "/Users/xupeng/github/astra"},
                        "executor": {"kind": "edge_agent", "executor_id": "edge-1", "transport": "edge_ws"},
                        "transport": "edge_ws"
                    }),
                ),
                &|o| {
                    assert_eq!(o["type"], "tool_call_end");
                    assert_eq!(o["call_id"], "c1");
                    assert_eq!(o["result"], "ok");
                    assert_eq!(o["success"], true);
                    assert_eq!(o["duration_ms"], 42);
                },
            ),
            // ── run lifecycle ──
            (
                "run_started",
                make_event(
                    "run_started",
                    json!({
                        "run_id": "run-1", "session_id": "sess-1", "interaction_mode": "auto",
                        "interactive_client": true,
                        "turn_intent_policy": "fixed_default",
                        "workspace": {"kind": "server_sandbox", "cwd": "/tmp/astra-workspaces/run-1"},
                        "executor": {"kind": "server_local", "status": "online"},
                        "transport": "server_local"
                    }),
                ),
                &|o| {
                    assert_eq!(o["type"], "run_started");
                    assert_eq!(o["run_id"], "run-1");
                    assert_eq!(o["interaction_mode"], "auto");
                    assert_eq!(o["turn_intent_policy"], "fixed_default");
                },
            ),
            (
                "run_finished",
                make_event(
                    "run_finished",
                    json!({
                        "run_id": "run-1", "status": "paused", "error": "boom",
                        "error_code": "network", "waiting_for": "task_board_intervention"
                    }),
                ),
                &|o| {
                    assert_eq!(o["type"], "run_finished");
                    assert_eq!(o["status"], "paused");
                    assert_eq!(o["error_code"], "network");
                },
            ),
            (
                "run_waiting",
                make_event(
                    "run_waiting",
                    json!({"reason": "waiting: executor_offline"}),
                ),
                &|o| {
                    assert_eq!(o["type"], "run_waiting");
                },
            ),
            (
                "run_interrupted",
                make_event(
                    "run_interrupted",
                    json!({
                        "kind": "budget_exhausted", "resumable": true,
                        "user_message": "You can continue in the next message."
                    }),
                ),
                &|o| {
                    assert_eq!(o["type"], "run_interrupted");
                    assert_eq!(o["resumable"], true);
                },
            ),
            // ── run_error surface ──
            (
                "run_error basic",
                make_event("run_error", json!({"error": "boom"})),
                &|o| {
                    assert_eq!(o["type"], "run_error");
                    assert_eq!(o["code"], "RUN_ERROR");
                },
            ),
            (
                "run_error rate_limit",
                make_event(
                    "run_error",
                    json!({"error": "slow down", "error_kind": "rate_limit"}),
                ),
                &|o| {
                    assert_eq!(o["code"], "LLM_RATE_LIMIT");
                    assert_eq!(o["retryable"], true);
                    assert_eq!(o["retry_after_ms"], 5_000);
                },
            ),
            (
                "run_error server_error",
                make_event(
                    "run_error",
                    json!({"error": "provider 500", "error_kind": "server_error"}),
                ),
                &|o| {
                    assert_eq!(o["code"], "SERVER_ERROR");
                    assert_eq!(o["retryable"], true);
                    assert_eq!(o["retry_after_ms"], 2_000);
                },
            ),
            (
                "run_error empty→default msg",
                make_event("run_error", json!({})),
                &|o| {
                    assert_eq!(o["message"], "Unknown error");
                },
            ),
            // ── approval / user_input ──
            (
                "approval_request→approval_required",
                make_event("approval_request", json!({"approval_id": "a-1"})),
                &|o| {
                    assert_eq!(o["type"], "approval_required");
                    assert_eq!(o["approval_id"], "a-1");
                },
            ),
            (
                "approval_required canonical",
                make_event("approval_required", json!({"approval_id": "a-2"})),
                &|o| {
                    assert_eq!(o["type"], "approval_required");
                    assert_eq!(o["approval_id"], "a-2");
                },
            ),
            (
                "tool_request canonical",
                make_event(
                    "tool_request",
                    json!({"request_id": "call-1", "tool": "bash", "args": {"cmd": "pwd"}}),
                ),
                &|o| {
                    assert_eq!(o["type"], "tool_request");
                    assert_eq!(o["request_id"], "call-1");
                    assert_eq!(o["tool"], "bash");
                },
            ),
            (
                "user_input",
                make_event("user_input", json!({"text": "approved"})),
                &|o| {
                    assert_eq!(o["type"], "user_input");
                    assert_eq!(o["text"], "approved");
                },
            ),
            // ── plan events ──
            (
                "plan_created",
                make_event("plan_created", json!({"plan": {"steps": []}})),
                &|o| {
                    assert_eq!(o["type"], "plan_created");
                },
            ),
            (
                "plan_step_start",
                make_event("plan_step_start", json!({"step": "s1"})),
                &|o| {
                    assert_eq!(o["type"], "plan_step_start");
                },
            ),
            (
                "plan_step_done",
                make_event("plan_step_done", json!({"step": "s1", "result": "ok"})),
                &|o| {
                    assert_eq!(o["type"], "plan_step_done");
                },
            ),
            (
                "plan_revised",
                make_event("plan_revised", json!({"plan": {}})),
                &|o| {
                    assert_eq!(o["type"], "plan_revised");
                },
            ),
            // ── agent subrun events ──
            (
                "agent_delegated",
                make_event("agent_delegated", json!({"agent_id": "a1", "task": "t"})),
                &|o| {
                    assert_eq!(o["type"], "agent_delegated");
                },
            ),
            (
                "agent_progress",
                make_event(
                    "agent_progress",
                    json!({"agent_id": "a1", "progress": "50%"}),
                ),
                &|o| {
                    assert_eq!(o["type"], "agent_progress");
                },
            ),
            (
                "agent_completed",
                make_event(
                    "agent_completed",
                    json!({"agent_id": "a1", "result": "done"}),
                ),
                &|o| {
                    assert_eq!(o["type"], "agent_completed");
                },
            ),
            (
                "agent_failed",
                make_event("agent_failed", json!({"agent_id": "a1", "error": "boom"})),
                &|o| {
                    assert_eq!(o["type"], "agent_failed");
                },
            ),
            (
                "agent_communication",
                make_event(
                    "agent_communication",
                    json!({
                        "schema_version": "astra.agent_communication.v1",
                        "observed_by": {"run_id": "run-review", "agent_id": "reviewer"},
                        "direction": "received",
                        "message_id": "msg-1",
                        "from": {"run_id": "run-code", "agent_id": "coder"},
                        "to": {"kind": "direct", "address": {"run_id": "run-review", "agent_id": "reviewer"}},
                        "payload_kind": "text",
                        "summary": "review this",
                        "timestamp_ms": 42,
                        "requires_ack": false
                    }),
                ),
                &|o| {
                    assert_eq!(o["type"], "agent_communication");
                    assert_eq!(o["observed_by"]["run_id"], "run-review");
                    assert_eq!(o["from"]["agent_id"], "coder");
                    assert_eq!(o["summary"], "review this");
                },
            ),
            (
                "agent_waiting",
                make_event(
                    "agent_waiting",
                    json!({
                        "agent_id": "a1", "reason": "executor_offline",
                        "workspace": {"kind": "edge_workspace", "cwd": "/repo"},
                        "executor": {"kind": "edge_agent", "status": "offline"},
                        "transport": "edge_ws"
                    }),
                ),
                &|o| {
                    assert_eq!(o["type"], "agent_waiting");
                    assert_eq!(o["reason"], "executor_offline");
                },
            ),
            (
                "agent_cancelled",
                make_event(
                    "agent_cancelled",
                    json!({"agent_id": "a1", "reason": "user request"}),
                ),
                &|o| {
                    assert_eq!(o["type"], "agent_cancelled");
                },
            ),
            (
                "agent_interrupted",
                make_event(
                    "agent_interrupted",
                    json!({"agent_id": "a1", "reason": "budget_exhausted"}),
                ),
                &|o| {
                    assert_eq!(o["type"], "agent_interrupted");
                },
            ),
            // ── keepalive → ping ──
            (
                "keepalive→ping",
                make_event("keepalive", json!({})),
                &|o| {
                    assert_eq!(o["type"], "ping");
                },
            ),
            // ── drop cases ──
            (
                "unknown→dropped",
                make_event("custom_event", json!({})),
                &|o| assert!(o.is_null()),
            ),
            (
                "team_prepare→dropped",
                make_event(
                    "team_prepare",
                    json!({"delegation_id": "d1", "phase": "prepare"}),
                ),
                &|o| assert!(o.is_null()),
            ),
            ("no type→dropped", json!({"data": {}}), &|o| {
                assert!(o.is_null())
            }),
            (
                "text_delta no data",
                json!({"event_type": "text_delta"}),
                &|o| {
                    assert_eq!(o["type"], "text_delta");
                    assert_eq!(o["content"], "");
                },
            ),
            // ── already-shaped (allowlist / pass-through) ──
            (
                "internal injection_freshness→dropped",
                json!({"type": "injection_freshness", "channels": [{"tag":"self_awareness","hash":0u64,"bytes":0u64,"is_empty":true}]}),
                &|o| {
                    assert!(o.is_null(), "client-shaped internal event must be dropped");
                },
            ),
            (
                "shaped text_delta pass-through",
                json!({"type": "text_delta", "content": "hello", "index": 3}),
                &|o| {
                    assert_eq!(o["type"], "text_delta");
                    assert_eq!(o["content"], "hello");
                },
            ),
            (
                "shaped user_intent_applied pass-through",
                json!({
                    "type": "user_intent_applied",
                    "intent_id": "input-7",
                    "delivery": "guide_current_run",
                    "status": "applied",
                    "event_index": 7,
                    "content": "change course",
                }),
                &|o| {
                    assert_eq!(o["type"], "user_intent_applied");
                    assert_eq!(o["intent_id"], "input-7");
                    assert_eq!(o["delivery"], "guide_current_run");
                    assert_eq!(o["status"], "applied");
                    assert_eq!(o["event_index"], 7);
                    assert_eq!(o["content"], "change course");
                },
            ),
            (
                "shaped tool_call pass-through",
                json!({"type": "tool_call", "tool_call": {"id": "c1"}}),
                &|o| {
                    assert_eq!(o["type"], "tool_call");
                },
            ),
            (
                "shaped tool_call_end pass-through",
                json!({"type": "tool_call_end", "call_id": "c1", "result": "ok"}),
                &|o| {
                    assert_eq!(o["type"], "tool_call_end");
                },
            ),
            (
                "agent_live_event pass-through",
                json!({"type": "agent_live_event", "agent_id": "agent-1", "event_kind": "output_delta", "content": "child output"}),
                &|o| {
                    assert_eq!(o["type"], "agent_live_event");
                },
            ),
            (
                "shaped agent_waiting pass-through",
                json!({"type": "agent_waiting", "agent_id": "agent-1", "reason": "executor_offline", "workspace": {"kind": "edge_workspace", "cwd": "/repo"}, "executor": {"kind": "edge_agent", "status": "offline"}, "transport": "edge_ws"}),
                &|o| {
                    assert_eq!(o["type"], "agent_waiting");
                },
            ),
            // ── work-surface / transport pass-through ──
            (
                "workspace_bound pass-through",
                json!({"type": "workspace_bound", "workspace": {"kind": "server_sandbox"}, "executor": {"kind": "server_local"}}),
                &|o| {
                    assert_eq!(o["type"], "workspace_bound");
                },
            ),
            (
                "tool_transport_started pass-through",
                json!({"type": "tool_transport_started", "call_id": "c1", "tool": "bash"}),
                &|o| {
                    assert_eq!(o["type"], "tool_transport_started");
                },
            ),
            (
                "run_blocked pass-through",
                json!({"type": "run_blocked", "call_id": "c1", "tool": "bash", "reason": "executor_offline"}),
                &|o| {
                    assert_eq!(o["type"], "run_blocked");
                },
            ),
            // ── durable-prefix → client shape (strip event_type/data envelope) ──
            (
                "durable run_blocked→client",
                json!({
                    "event_type": "run_blocked", "index": 4,
                    "data": {"call_id": "c1", "tool": "bash", "reason": "workspace_executor_unavailable",
                        "message": "Workspace is not routed to an available executor.",
                        "workspace": {"kind": "cloud_workspace"},
                        "executor": {"kind": "orchestrator_managed", "status": "degraded"},
                        "transport": "sandbox_resident_agent"}
                }),
                &|o| {
                    assert_eq!(o["type"], "run_blocked");
                    assert_eq!(o["reason"], "workspace_executor_unavailable");
                    assert_eq!(o["transport"], "sandbox_resident_agent");
                    assert!(o["index"].is_null(), "durable index stripped");
                },
            ),
        ];
        for (_label, input, check) in &cases {
            let out = transform_run_event_for_client(input.clone());
            check(&out);
        }
    }

    #[test]
    fn public_sse_event_wire_contract_is_exact_for_core_lifecycle() {
        let cases = vec![
            (
                make_event("text_delta", json!({"chunk": "hi"})),
                json!({"type": "text_delta", "content": "hi"}),
            ),
            (
                make_event(
                    "tool_call_start",
                    json!({
                        "name": "bash",
                        "tool_call_id": "call-1",
                        "args": {"command": "pwd"},
                        "workspace": {"kind": "edge_workspace", "cwd": "/repo"},
                        "executor": {"kind": "edge_agent", "executor_id": "edge-1"},
                        "transport": "edge_ws"
                    }),
                ),
                json!({
                    "type": "tool_call_start",
                    "tool": "bash",
                    "call_id": "call-1",
                    "arguments": {"command": "pwd"},
                    "workspace": {"kind": "edge_workspace", "cwd": "/repo"},
                    "executor": {"kind": "edge_agent", "executor_id": "edge-1"},
                    "transport": "edge_ws"
                }),
            ),
            (
                make_event(
                    "tool_result",
                    json!({
                        "name": "bash",
                        "tool_call_id": "call-1",
                        "output": "ok",
                        "success": true,
                        "duration_ms": 12,
                        "workspace": {"kind": "edge_workspace", "cwd": "/repo"},
                        "executor": {"kind": "edge_agent", "executor_id": "edge-1"},
                        "transport": "edge_ws"
                    }),
                ),
                json!({
                    "type": "tool_call_end",
                    "tool": "bash",
                    "call_id": "call-1",
                    "result": "ok",
                    "success": true,
                    "duration_ms": 12,
                    "workspace": {"kind": "edge_workspace", "cwd": "/repo"},
                    "executor": {"kind": "edge_agent", "executor_id": "edge-1"},
                    "transport": "edge_ws"
                }),
            ),
            (
                make_event(
                    "run_finished",
                    json!({
                        "run_id": "run-1",
                        "status": "paused",
                        "error_code": "network",
                        "interrupted": true,
                        "interruption_kind": "executor_offline",
                        "resumable": true,
                        "waiting_for": "executor"
                    }),
                ),
                json!({
                    "type": "run_finished",
                    "run_id": "run-1",
                    "status": "paused",
                    "error_code": "network",
                    "interrupted": true,
                    "interruption_kind": "executor_offline",
                    "resumable": true,
                    "waiting_for": "executor"
                }),
            ),
            (
                make_event(
                    "run_error",
                    json!({
                        "run_id": "run-1",
                        "error": "slow down",
                        "error_kind": "rate_limit"
                    }),
                ),
                json!({
                    "type": "run_error",
                    "run_id": "run-1",
                    "message": "slow down",
                    "error": "slow down",
                    "error_kind": "rate_limit",
                    "error_code": "rate_limit",
                    "code": "LLM_RATE_LIMIT",
                    "retryable": true,
                    "retry_after_ms": 5_000
                }),
            ),
        ];

        for (input, expected) in cases {
            assert_eq!(transform_run_event_for_client(input), expected);
        }
    }

    #[test]
    fn chat_request_data_debug_redacts_forward_header_values() {
        let mut forward_headers = std::collections::HashMap::new();
        forward_headers.insert(
            "authorization".to_string(),
            "Bearer secret-token".to_string(),
        );
        forward_headers.insert("x-workspace-id".to_string(), "ws-123".to_string());
        forward_headers.insert("__astra_connection_tokens".to_string(), "x-hop".to_string());

        let request = ChatRequestData {
            message: "hi".to_string(),
            user_intent: None,
            parts: Vec::new(),
            attachments: Vec::new(),
            runtime_system_prompt: None,
            session_id: Some("sess-1".to_string()),
            agent_id: None,
            model: None,
            model_selection: None,
            resolved_model_selection: None,
            admitted_model_execution: Some(AdmittedModelExecution {
                offering_id: "offer-gpt-4".to_string(),
                model_name: "gpt-4".to_string(),
                wire_model_name: None,
                api_key: "provider-api-secret".to_string(),
                base_url: "https://models.example.com/v1".to_string(),
                provider: "openai".to_string(),
                cache_capability: None,
                request_body_overrides: None,
                context_window: Some(128_000),
                header_overrides: HashMap::new(),
                completions_url_override: None,
                request_timeout_ms: None,
            }),
            capability_descriptors: None,
            provider_runtime_authorized: false,
            agent_binding: None,
            runtime_auth: None,
            runtime_skill_binding: None,
            runtime_profile: None,
            skill_search: None,
            allow_skills: None,
            allow_skill_sources: None,
            allow_tools: None,
            enabled_tools: None,
            workspace_binding: None,
            executor_binding: None,
            runtime_mcp_bindings: Vec::new(),
            mcp_binding_ids: None,
            context: None,
            edge_executor_id: None,
            capabilities: Vec::new(),
            forward_headers,
            execution_budget: Some(ExecutionBudget {
                initial_turns: Some(10),
                hard_turn_limit: Some(18),
            }),
            execution_policy: Default::default(),
            full_llm_capture: false,
            explain: false,
            interaction_mode: None,
            interactive_client: false,
            provider_workspace_id: None,
        };

        let rendered = format!("{request:?}");
        assert!(rendered.contains("authorization"));
        assert!(rendered.contains("x-workspace-id"));
        assert!(!rendered.contains("Bearer secret-token"));
        assert!(!rendered.contains("ws-123"));
        assert!(!rendered.contains("__astra_connection_tokens"));
        assert!(!rendered.contains("provider-api-secret"));
        assert!(rendered.contains("admitted_model_execution_present: true"));
    }

    #[test]
    fn runtime_auth_request_debug_redacts_authorization_value() {
        let runtime_auth = RuntimeAuthRequest {
            authorization: "Bearer secret-runtime-token".to_string(),
        };

        let rendered = format!("{runtime_auth:?}");

        assert!(rendered.contains("authorization_present"));
        assert!(!rendered.contains("secret-runtime-token"));
        assert!(!rendered.contains("Bearer"));
    }

    #[test]
    fn chat_request_data_debug_redacts_runtime_auth_value() {
        let request = ChatRequestData {
            message: "hi".to_string(),
            user_intent: None,
            parts: Vec::new(),
            attachments: Vec::new(),
            runtime_system_prompt: None,
            session_id: Some("sess-1".to_string()),
            agent_id: None,
            model: Some("gpt-4".to_string()),
            model_selection: Some(ModelSelectionRequest {
                offering_id: "offer-gpt-4".to_string(),
            }),
            resolved_model_selection: Some(ResolvedModelSelection {
                offering_id: "offer-gpt-4".to_string(),
                model_name: "gpt-4".to_string(),
            }),
            admitted_model_execution: None,
            capability_descriptors: None,
            provider_runtime_authorized: false,
            agent_binding: None,
            runtime_auth: Some(RuntimeAuthRequest {
                authorization: "Bearer secret-runtime-token".to_string(),
            }),
            runtime_skill_binding: None,
            runtime_profile: None,
            skill_search: None,
            allow_skills: None,
            allow_skill_sources: None,
            allow_tools: None,
            enabled_tools: None,
            workspace_binding: None,
            executor_binding: None,
            runtime_mcp_bindings: Vec::new(),
            mcp_binding_ids: None,
            context: None,
            edge_executor_id: None,
            capabilities: Vec::new(),
            forward_headers: std::collections::HashMap::new(),
            execution_budget: None,
            execution_policy: Default::default(),
            full_llm_capture: false,
            explain: false,
            interaction_mode: None,
            interactive_client: false,
            provider_workspace_id: None,
        };

        let rendered = format!("{request:?}");

        assert!(rendered.contains("RuntimeAuthRequest"));
        assert!(rendered.contains("authorization_present"));
        assert!(!rendered.contains("secret-runtime-token"));
        assert!(!rendered.contains("Bearer secret-runtime-token"));
    }

    #[test]
    fn runtime_mcp_binding_request_debug_redacts_credentials() {
        let binding = RuntimeMcpBindingRequest {
            id: "external_nl2sql".to_string(),
            transport: "streamable_http".to_string(),
            url: "http://user:url-secret@tool-server/mcp/http?token=query-secret#frag".to_string(),
            auth_token: Some("secret-auth-token".to_string()),
            headers: std::collections::HashMap::from([
                (
                    "Authorization".to_string(),
                    "Bearer secret-runtime-grant".to_string(),
                ),
                ("X-External-Workspace".to_string(), "ws-secret".to_string()),
            ]),
        };

        let rendered = format!("{binding:?}");

        assert!(rendered.contains("external_nl2sql"));
        assert!(rendered.contains("auth_token_present"));
        assert!(rendered.contains("http://tool-server/mcp/http"));
        assert!(rendered.contains("Authorization"));
        assert!(rendered.contains("X-External-Workspace"));
        assert!(!rendered.contains("url-secret"));
        assert!(!rendered.contains("query-secret"));
        assert!(!rendered.contains("#frag"));
        assert!(!rendered.contains("secret-auth-token"));
        assert!(!rendered.contains("secret-runtime-grant"));
        assert!(!rendered.contains("ws-secret"));
    }

    #[tokio::test]
    async fn unconfigured_service_uses_stable_error_code() {
        let service = UnconfiguredRunLifecycleService;
        let err = service
            .create_run(
                "u1".to_string(),
                ChatRequestData {
                    message: "hi".to_string(),
                    user_intent: None,
                    parts: Vec::new(),
                    attachments: Vec::new(),
                    runtime_system_prompt: None,
                    session_id: None,
                    agent_id: None,
                    model: None,
                    model_selection: None,
                    resolved_model_selection: None,
                    admitted_model_execution: None,
                    capability_descriptors: None,
                    provider_runtime_authorized: false,
                    agent_binding: None,
                    runtime_auth: None,
                    runtime_skill_binding: None,
                    runtime_profile: None,
                    skill_search: None,
                    allow_skills: None,
                    allow_skill_sources: None,
                    allow_tools: None,
                    enabled_tools: None,
                    workspace_binding: None,
                    executor_binding: None,
                    runtime_mcp_bindings: Vec::new(),
                    mcp_binding_ids: None,
                    context: None,
                    edge_executor_id: None,
                    capabilities: Vec::new(),
                    forward_headers: std::collections::HashMap::new(),
                    execution_budget: Some(ExecutionBudget {
                        initial_turns: Some(25),
                        hard_turn_limit: Some(40),
                    }),
                    execution_policy: Default::default(),
                    full_llm_capture: false,
                    explain: false,
                    interaction_mode: None,
                    interactive_client: false,
                    provider_workspace_id: None,
                },
            )
            .await
            .expect_err("service should be unconfigured");
        assert!(is_run_lifecycle_unconfigured_error(err.0, &err.1.0));
        assert_eq!(
            err.1.0.error_code.as_deref(),
            Some(RUN_LIFECYCLE_UNCONFIGURED_ERROR_CODE)
        );
    }

    /// U2: InMemoryRunStateStore must evict old completed runs when the
    /// store exceeds its capacity, preventing unbounded memory growth.
    #[tokio::test]
    async fn in_memory_run_store_evicts_completed_runs() {
        let store = InMemoryRunStateStore::new();
        let max = InMemoryRunStateStore::MAX_RUNS;

        // Fill to capacity + 10 with completed runs. The timeout guards the
        // lock/liveness contract: eviction must not recursively acquire the
        // execution-slot write lock or regress to per-insert history scans.
        tokio::time::timeout(Duration::from_secs(10), async {
            for i in 0..max + 10 {
                let record = DurableRunRecord {
                    run_id: format!("run-{i}"),
                    session_id: "s1".into(),
                    user_id: "u1".into(),
                    status: "completed".into(),
                    parent_run_id: None,
                    root_run_id: None,
                    ancestor_path: None,
                    depth: 0,
                    delegation_id: None,
                    agent_id: None,
                    retry_of: None,
                    retry_scope: None,
                    waiting_for: None,
                    owner_pod_id: None,
                    owner_lease_expires_at: None,
                    run_generation: 0,
                    last_event_idx: -1,
                    checkpoint_version: None,
                    checkpoint_json: None,
                    error_code: None,
                    error_message: None,
                    retry_count: 0,
                    total_prompt_tokens: 0,
                    total_completion_tokens: 0,
                    total_tool_calls: 0,
                    agent_binding_id: None,
                    agent_binding_name: None,
                    agent_binding_schema_version: None,
                    model_offering_id: None,
                    resolved_model_name: None,
                    capability_server_refs_json: None,
                    runtime_profile: None,
                    events: vec![],
                    created_at: chrono::Utc::now().to_rfc3339(),
                    updated_at: chrono::Utc::now().to_rfc3339(),
                };
                store.insert_run(record).await.unwrap();
            }
        })
        .await
        .expect("bounded in-memory retention must complete without lock starvation");

        // Store must not exceed max capacity
        let runs = store.runs.read().await;
        assert!(
            runs.len() <= max,
            "store has {} runs, expected ≤ {max}",
            runs.len()
        );
    }

    // ── Batch event append tests ───────────────────────────────────────

    #[tokio::test]
    async fn append_events_batch_stores_events_in_order() {
        let store = InMemoryRunStateStore::new();
        let run = durable_run_record("batch-order");
        store.insert_run(run).await.unwrap();

        let events = vec![
            make_event("tool_call", json!({"tool": "read_file"})),
            make_event("tool_result", json!({"output": "hello"})),
            make_event("text_delta", json!({"chunk": "done"})),
        ];
        store
            .append_events_batch("u1", "batch-order", &events)
            .await
            .unwrap();

        let loaded = store.load_run("u1", "batch-order").await.unwrap().unwrap();
        assert_eq!(loaded.events.len(), 3);
        assert_eq!(loaded.events[0]["event_type"], "tool_call");
        assert_eq!(loaded.events[1]["event_type"], "tool_result");
        assert_eq!(loaded.events[2]["event_type"], "text_delta");
        assert_eq!(loaded.last_event_idx, 2);
    }

    #[tokio::test]
    async fn run_interaction_resolution_is_atomic_idempotent_and_conflict_safe() {
        let store = InMemoryRunStateStore::new();
        store
            .insert_run(durable_run_record("interaction-resolution"))
            .await
            .unwrap();
        assert!(
            store
                .update_run_status_with_event_if_current(
                    "u1",
                    "interaction-resolution",
                    &[STATUS_RUNNING],
                    STATUS_WAITING,
                    Some("tool_approval"),
                    None,
                    json!({
                        "event_type": "approval_required",
                        "data": {
                            "request_id": "approval-1",
                            "tool": "bash",
                            "approval_kind": "standard"
                        }
                    }),
                )
                .await
                .unwrap()
        );
        let approved = json!({
            "request_id": "approval-1",
            "outcome": "approved",
            "decision": "allow",
            "tool": "bash",
            "approval_kind": "standard"
        });
        assert!(matches!(
            store
                .resolve_run_interaction(
                    "u1",
                    "interaction-resolution",
                    "approval-1",
                    DurableRunInteractionKind::Approval,
                    approved.clone(),
                )
                .await
                .unwrap(),
            DurableRunInteractionResolveOutcome::Resolved(_)
        ));
        assert!(matches!(
            store
                .resolve_run_interaction(
                    "u1",
                    "interaction-resolution",
                    "approval-1",
                    DurableRunInteractionKind::Approval,
                    approved,
                )
                .await
                .unwrap(),
            DurableRunInteractionResolveOutcome::Idempotent(_)
        ));
        assert!(matches!(
            store
                .resolve_run_interaction(
                    "u1",
                    "interaction-resolution",
                    "approval-1",
                    DurableRunInteractionKind::Approval,
                    json!({
                        "request_id": "approval-1",
                        "outcome": "denied",
                        "decision": "deny",
                        "tool": "bash",
                        "approval_kind": "standard"
                    }),
                )
                .await
                .unwrap(),
            DurableRunInteractionResolveOutcome::Conflict(_)
        ));
        let run = store
            .load_run("u1", "interaction-resolution")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.status, STATUS_RUNNING);
        assert_eq!(run.waiting_for, None);
        assert_eq!(
            run.events
                .iter()
                .filter(|event| extract_event_type(event) == "approval_resolved")
                .count(),
            1
        );
        assert_eq!(run.events.last().unwrap()["event_type"], "run_resumed");
    }

    #[tokio::test]
    async fn append_events_batch_empty_is_noop() {
        let store = InMemoryRunStateStore::new();
        let run = durable_run_record("batch-empty");
        store.insert_run(run).await.unwrap();

        store
            .append_events_batch("u1", "batch-empty", &[])
            .await
            .unwrap();

        let loaded = store.load_run("u1", "batch-empty").await.unwrap().unwrap();
        assert_eq!(loaded.events.len(), 0);
        assert_eq!(loaded.last_event_idx, -1); // unchanged
    }

    #[tokio::test]
    async fn append_events_batch_unknown_run_returns_error() {
        let store = InMemoryRunStateStore::new();
        let event = make_event("tool_result", json!({"output": "orphan"}));

        let batch_error = store
            .append_events_batch("u1", "missing-run", std::slice::from_ref(&event))
            .await
            .expect_err("non-empty batch append to unknown run must fail");
        assert!(batch_error.contains("run not found"));

        let single_error = store
            .append_event("u1", "missing-run", event)
            .await
            .expect_err("single append delegates to batch and must also fail");
        assert!(single_error.contains("run not found"));
    }

    #[tokio::test]
    async fn status_with_event_transition_commits_status_and_event() {
        let store = InMemoryRunStateStore::new();
        store
            .insert_run(durable_run_record("transition-commit"))
            .await
            .unwrap();

        let updated = store
            .update_run_status_with_event_if_current(
                "u1",
                "transition-commit",
                &[STATUS_RUNNING],
                STATUS_PAUSED,
                Some("user_resume"),
                None,
                make_event("run_paused", json!({})),
            )
            .await
            .unwrap();

        assert!(updated);
        let loaded = store
            .load_run("u1", "transition-commit")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.status, STATUS_PAUSED);
        assert_eq!(loaded.waiting_for.as_deref(), Some("user_resume"));
        assert_eq!(loaded.last_event_idx, 0);
        assert_eq!(loaded.events.len(), 1);
        assert_eq!(loaded.events[0]["event_type"], "run_paused");

        let projection = store
            .load_run_projection("u1", "transition-commit")
            .await
            .unwrap()
            .expect("transition should refresh projection");
        assert_eq!(projection.status, STATUS_PAUSED);
        assert_eq!(projection.latest_event_type.as_deref(), Some("run_paused"));
    }

    #[tokio::test]
    async fn status_with_event_transition_rejects_unexpected_status_without_event() {
        let store = InMemoryRunStateStore::new();
        store
            .insert_run(durable_run_record("transition-conflict"))
            .await
            .unwrap();

        let updated = store
            .update_run_status_with_event_if_current(
                "u1",
                "transition-conflict",
                &[STATUS_PAUSED],
                STATUS_CANCELLED,
                None,
                None,
                make_event("run_finished", json!({"cancelled": true})),
            )
            .await
            .unwrap();

        assert!(!updated);
        let loaded = store
            .load_run("u1", "transition-conflict")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.status, STATUS_RUNNING);
        assert!(loaded.waiting_for.is_none());
        assert_eq!(loaded.last_event_idx, -1);
        assert!(loaded.events.is_empty());
    }

    #[tokio::test]
    async fn status_with_event_transition_rejects_wrong_owner_without_event() {
        let store = InMemoryRunStateStore::new();
        store
            .insert_run(durable_run_record("transition-owner"))
            .await
            .unwrap();

        let updated = store
            .update_run_status_with_event_if_current(
                "u2",
                "transition-owner",
                &[STATUS_RUNNING],
                STATUS_CANCELLED,
                None,
                None,
                make_event("run_finished", json!({"cancelled": true})),
            )
            .await
            .unwrap();

        assert!(!updated);
        let loaded = store
            .load_run("u1", "transition-owner")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.status, STATUS_RUNNING);
        assert_eq!(loaded.last_event_idx, -1);
        assert!(loaded.events.is_empty());
    }

    #[tokio::test]
    async fn save_checkpoint_rejects_terminal_run_without_mutation() {
        let store = InMemoryRunStateStore::new();
        store
            .insert_run(durable_run_record("terminal-checkpoint"))
            .await
            .unwrap();
        store
            .update_run_status_with_event_if_current(
                "u1",
                "terminal-checkpoint",
                &[STATUS_RUNNING],
                STATUS_FAILED,
                None,
                Some("boom"),
                make_event("run_error", json!({"error": "boom"})),
            )
            .await
            .unwrap();

        let saved = store
            .save_checkpoint(
                "u1",
                "terminal-checkpoint",
                r#"{"version":"checkpoint_v2","graceful":true,"last_batch_id":"terminal"}"#,
            )
            .await
            .unwrap();

        assert!(!saved);
        let loaded = store
            .load_run("u1", "terminal-checkpoint")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.status, STATUS_FAILED);
        assert!(loaded.checkpoint_json.is_none());
        assert!(loaded.checkpoint_version.is_none());
        assert!(
            store
                .load_latest_checkpoint("u1", "terminal-checkpoint", None)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn status_with_events_transition_commits_event_batch() {
        let store = InMemoryRunStateStore::new();
        store
            .insert_run(durable_run_record("transition-batch"))
            .await
            .unwrap();

        let events = vec![
            make_event(
                "run_error",
                json!({"error": "boom", "error_code": "network", "error_kind": "network"}),
            ),
            make_event(
                "run_finished",
                json!({"status": "failed", "error_code": "network"}),
            ),
        ];
        let updated = store
            .update_run_status_with_events_if_current(
                "u1",
                "transition-batch",
                &[STATUS_RUNNING],
                STATUS_FAILED,
                None,
                Some("boom"),
                &events,
            )
            .await
            .unwrap();

        assert!(updated);
        let loaded = store
            .load_run("u1", "transition-batch")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.status, STATUS_FAILED);
        assert_eq!(loaded.error_code.as_deref(), Some("network"));
        assert_eq!(loaded.error_message.as_deref(), Some("boom"));
        assert_eq!(loaded.last_event_idx, 1);
        assert_eq!(loaded.events.len(), 2);
        assert_eq!(loaded.events[0]["event_type"], "run_error");
        assert_eq!(loaded.events[1]["event_type"], "run_finished");

        let projection = store
            .load_run_projection("u1", "transition-batch")
            .await
            .unwrap()
            .expect("transition should refresh projection");
        assert_eq!(projection.status, STATUS_FAILED);
        assert_eq!(
            projection.latest_event_type.as_deref(),
            Some("run_finished")
        );
    }

    #[tokio::test]
    async fn status_with_events_transition_allows_empty_batch() {
        let store = InMemoryRunStateStore::new();
        store
            .insert_run(durable_run_record("transition-empty-batch"))
            .await
            .unwrap();

        let updated = store
            .update_run_status_with_events_if_current(
                "u1",
                "transition-empty-batch",
                &[STATUS_RUNNING],
                STATUS_WAITING,
                Some("tool_approval"),
                None,
                &[],
            )
            .await
            .unwrap();

        assert!(updated);
        let loaded = store
            .load_run("u1", "transition-empty-batch")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.status, STATUS_WAITING);
        assert_eq!(loaded.waiting_for.as_deref(), Some("tool_approval"));
        assert_eq!(loaded.last_event_idx, -1);
        assert!(loaded.events.is_empty());
    }

    #[tokio::test]
    async fn failed_status_transition_classifies_error_message_without_events() {
        let store = InMemoryRunStateStore::new();
        store
            .insert_run(durable_run_record("transition-message-code"))
            .await
            .unwrap();

        let updated = store
            .update_run_status_if_current(
                "u1",
                "transition-message-code",
                &[STATUS_RUNNING],
                STATUS_FAILED,
                None,
                Some(
                    "database operation failed: error communicating with database: unexpected EOF",
                ),
            )
            .await
            .unwrap();

        assert!(updated);
        let loaded = store
            .load_run("u1", "transition-message-code")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.status, STATUS_FAILED);
        assert_eq!(loaded.error_code.as_deref(), Some("database_error"));
        assert_eq!(
            loaded.error_message.as_deref(),
            Some("database operation failed: error communicating with database: unexpected EOF")
        );
        assert_eq!(loaded.last_event_idx, -1);
        assert!(loaded.events.is_empty());
    }

    #[tokio::test]
    async fn failed_direct_status_update_classifies_error_message_without_events() {
        let store = InMemoryRunStateStore::new();
        store
            .insert_run(durable_run_record("direct-message-code"))
            .await
            .unwrap();

        let updated = store
            .update_run_status(
                "u1",
                "direct-message-code",
                STATUS_FAILED,
                None,
                Some("[stream_transport] stream body closed"),
            )
            .await
            .unwrap();

        assert!(updated);
        let loaded = store
            .load_run("u1", "direct-message-code")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.status, STATUS_FAILED);
        assert_eq!(loaded.error_code.as_deref(), Some("stream_transport"));
        assert_eq!(
            loaded.error_message.as_deref(),
            Some("[stream_transport] stream body closed")
        );
        assert_eq!(loaded.last_event_idx, -1);
        assert!(loaded.events.is_empty());
    }

    #[tokio::test]
    async fn failed_empty_event_batch_classifies_error_message() {
        let store = InMemoryRunStateStore::new();
        store
            .insert_run(durable_run_record("transition-empty-failed-batch"))
            .await
            .unwrap();

        let updated = store
            .update_run_status_with_events_if_current(
                "u1",
                "transition-empty-failed-batch",
                &[STATUS_RUNNING],
                STATUS_FAILED,
                None,
                Some("[network] LLM request failed: connection reset"),
                &[],
            )
            .await
            .unwrap();

        assert!(updated);
        let loaded = store
            .load_run("u1", "transition-empty-failed-batch")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.status, STATUS_FAILED);
        assert_eq!(loaded.error_code.as_deref(), Some("network"));
        assert_eq!(loaded.last_event_idx, -1);
        assert!(loaded.events.is_empty());
    }

    #[tokio::test]
    async fn status_with_events_transition_rejects_unexpected_status_without_batch() {
        let store = InMemoryRunStateStore::new();
        store
            .insert_run(durable_run_record("transition-batch-conflict"))
            .await
            .unwrap();

        let events = vec![make_event("run_finished", json!({"status": "cancelled"}))];
        let updated = store
            .update_run_status_with_events_if_current(
                "u1",
                "transition-batch-conflict",
                &[STATUS_PAUSED],
                STATUS_CANCELLED,
                None,
                None,
                &events,
            )
            .await
            .unwrap();

        assert!(!updated);
        let loaded = store
            .load_run("u1", "transition-batch-conflict")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.status, STATUS_RUNNING);
        assert_eq!(loaded.last_event_idx, -1);
        assert!(loaded.events.is_empty());
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
    async fn database_status_event_batch_transition_and_projection_repair_hold_on_matrixone() {
        let (store, pool) = setup_database_run_state_store_it().await;
        let user_id = format!("runs-it-user-{}", Uuid::new_v4());
        let run_id = format!("runs-it-run-{}", Uuid::new_v4());
        let session_id = format!("runs-it-session-{}", Uuid::new_v4());
        cleanup_database_run_fixture(&pool, &user_id, &run_id).await;

        let mut record = durable_run_record(&run_id);
        record.user_id = user_id.clone();
        record.session_id = session_id;
        record.root_run_id = Some(run_id.clone());
        record.ancestor_path = Some(run_id.clone());
        store.insert_run(record).await.expect("insert run");

        let saved_checkpoint = store
            .save_checkpoint(
                &user_id,
                &run_id,
                r#"{"version":"checkpoint_v2","graceful":true,"last_batch_id":"db-it"}"#,
            )
            .await
            .expect("save checkpoint before terminal transition");
        assert!(saved_checkpoint);

        let events = vec![
            make_event("run_error", json!({"error": "boom"})),
            make_event("run_finished", json!({"status": "failed"})),
        ];
        let updated = store
            .update_run_status_with_events_if_current(
                &user_id,
                &run_id,
                &[STATUS_RUNNING],
                STATUS_FAILED,
                None,
                Some("boom"),
                &events,
            )
            .await
            .expect("status+events transition");
        assert!(updated);

        let stale_update = store
            .update_run_status_with_events_if_current(
                &user_id,
                &run_id,
                &[STATUS_RUNNING],
                STATUS_CANCELLED,
                None,
                None,
                &[make_event("run_finished", json!({"cancelled": true}))],
            )
            .await
            .expect("stale status+events transition");
        assert!(!stale_update);

        store
            .update_run_usage(&user_id, &run_id, 10, 4, 2)
            .await
            .expect("update usage");
        let usage_projection = store
            .load_run_projection(&user_id, &run_id)
            .await
            .expect("load projection after usage patch")
            .expect("projection should exist after usage patch");
        assert_eq!(usage_projection.status, STATUS_FAILED);
        assert_eq!(usage_projection.error_message.as_deref(), Some("boom"));
        assert_eq!(usage_projection.projection_event_idx, 1);
        assert_eq!(
            usage_projection.latest_event_type.as_deref(),
            Some("run_finished")
        );
        assert_eq!(
            usage_projection.latest_checkpoint_version.as_deref(),
            Some("checkpoint_v2")
        );
        assert_eq!(usage_projection.total_prompt_tokens, 10);
        assert_eq!(usage_projection.total_completion_tokens, 4);
        assert_eq!(usage_projection.total_tool_calls, 2);

        let loaded = store
            .load_run(&user_id, &run_id)
            .await
            .expect("load run")
            .expect("run exists");
        assert_eq!(loaded.status, STATUS_FAILED);
        assert_eq!(loaded.error_message.as_deref(), Some("boom"));
        assert_eq!(loaded.error_code.as_deref(), Some("unknown"));
        assert_eq!(loaded.last_event_idx, 1);
        assert_eq!(loaded.events.len(), 2);
        assert_eq!(loaded.events[0]["event_type"], "run_error");
        assert_eq!(loaded.events[1]["event_type"], "run_finished");

        sqlx::query(
            "UPDATE run_display_projections
             SET status = 'running',
                 error_message = NULL,
                 projection_event_idx = -1,
                 latest_event_type = 'stale_event',
                 latest_checkpoint_kind = NULL,
                 latest_checkpoint_version = NULL
             WHERE user_id = ? AND run_id = ?",
        )
        .bind(&user_id)
        .bind(&run_id)
        .execute(pool.get())
        .await
        .expect("corrupt projection");

        let repaired = store
            .rebuild_run_projection(&user_id, &run_id)
            .await
            .expect("repair projection")
            .expect("projection repaired");
        assert_eq!(repaired.status, STATUS_FAILED);
        assert_eq!(repaired.error_message.as_deref(), Some("boom"));
        assert_eq!(repaired.projection_event_idx, 1);
        assert_eq!(repaired.latest_event_type.as_deref(), Some("run_finished"));
        assert_eq!(
            repaired.latest_checkpoint_version.as_deref(),
            Some("checkpoint_v2")
        );

        cleanup_database_run_fixture(&pool, &user_id, &run_id).await;
    }

    #[tokio::test]
    async fn rebuild_run_projection_repairs_stale_projection_from_facts() {
        let store = InMemoryRunStateStore::new();
        store
            .insert_run(durable_run_record("projection-repair"))
            .await
            .unwrap();
        let saved_checkpoint = store
            .save_checkpoint(
                "u1",
                "projection-repair",
                r#"{"version":"checkpoint_v2","graceful":true,"last_batch_id":"repair"}"#,
            )
            .await
            .unwrap();
        assert!(saved_checkpoint);

        let events = vec![
            make_event("run_error", json!({"error": "boom"})),
            make_event("run_finished", json!({"status": "failed"})),
        ];
        store
            .update_run_status_with_events_if_current(
                "u1",
                "projection-repair",
                &[STATUS_RUNNING],
                STATUS_FAILED,
                None,
                Some("boom"),
                &events,
            )
            .await
            .unwrap();
        store
            .update_run_usage("u1", "projection-repair", 10, 4, 2)
            .await
            .unwrap();

        let mut stale = store
            .load_run_projection("u1", "projection-repair")
            .await
            .unwrap()
            .expect("projection should exist before corruption");
        stale.status = STATUS_RUNNING.to_string();
        stale.error_message = None;
        stale.projection_event_idx = -1;
        stale.latest_event_type = Some("stale_event".to_string());
        stale.latest_checkpoint_kind = None;
        stale.latest_checkpoint_version = None;
        stale.total_prompt_tokens = 0;
        stale.total_completion_tokens = 0;
        stale.total_tool_calls = 0;
        store
            .projections
            .write()
            .await
            .insert("projection-repair".to_string(), stale);

        let repaired = store
            .rebuild_run_projection("u1", "projection-repair")
            .await
            .unwrap()
            .expect("repair should rebuild projection");
        assert_eq!(repaired.status, STATUS_FAILED);
        assert_eq!(repaired.error_message.as_deref(), Some("boom"));
        assert_eq!(repaired.projection_event_idx, 1);
        assert_eq!(repaired.latest_event_type.as_deref(), Some("run_finished"));
        assert_eq!(repaired.latest_checkpoint_kind.as_deref(), Some("resume"));
        assert_eq!(
            repaired.latest_checkpoint_version.as_deref(),
            Some("checkpoint_v2")
        );
        assert_eq!(repaired.total_prompt_tokens, 10);
        assert_eq!(repaired.total_completion_tokens, 4);
        assert_eq!(repaired.total_tool_calls, 2);

        let loaded = store
            .load_run_projection("u1", "projection-repair")
            .await
            .unwrap()
            .expect("repaired projection should be stored");
        assert_eq!(loaded.projection_hash, repaired.projection_hash);
    }

    #[tokio::test]
    async fn in_memory_run_store_rejects_wrong_owner_operations() {
        let store = InMemoryRunStateStore::new();
        let mut run = durable_run_record("owner-bound");
        run.delegation_id = Some("delegation-1".to_string());
        store.insert_run(run).await.unwrap();

        assert!(store.load_run("u2", "owner-bound").await.unwrap().is_none());
        assert!(
            store
                .load_run_projection("u2", "owner-bound")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .load_latest_checkpoint("u2", "owner-bound", None)
                .await
                .unwrap()
                .is_none()
        );

        assert!(
            !store
                .update_run_status("u2", "owner-bound", "completed", None, None)
                .await
                .unwrap()
        );
        assert!(
            !store
                .update_run_status_if_current(
                    "u2",
                    "owner-bound",
                    &["running"],
                    "completed",
                    None,
                    None
                )
                .await
                .unwrap()
        );
        assert!(
            !store
                .update_run_usage("u2", "owner-bound", 10, 5, 1)
                .await
                .unwrap()
        );
        assert!(
            !store
                .save_checkpoint(
                    "u2",
                    "owner-bound",
                    r#"{"version":"checkpoint_v1","graceful":true,"last_batch_id":"wrong-owner"}"#,
                )
                .await
                .unwrap()
        );
        assert!(
            !store
                .update_retry_count("u2", "owner-bound", 3)
                .await
                .unwrap()
        );
        assert!(
            store
                .find_sub_runs("u2", "delegation-1")
                .await
                .unwrap()
                .is_empty()
        );

        let append_error = store
            .append_event(
                "u2",
                "owner-bound",
                make_event("tool_result", json!({"output": "wrong owner"})),
            )
            .await
            .expect_err("wrong-owner append must not mutate the run");
        assert!(append_error.contains("run not found"));

        let loaded = store.load_run("u1", "owner-bound").await.unwrap().unwrap();
        assert_eq!(loaded.status, "running");
        assert_eq!(loaded.total_prompt_tokens, 0);
        assert_eq!(loaded.total_completion_tokens, 0);
        assert_eq!(loaded.total_tool_calls, 0);
        assert_eq!(loaded.retry_count, 0);
        assert_eq!(loaded.last_event_idx, -1);
        assert!(loaded.events.is_empty());
        assert!(loaded.checkpoint_json.is_none());
        assert_eq!(
            store
                .find_sub_runs("u1", "delegation-1")
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn append_events_batch_single_event() {
        let store = InMemoryRunStateStore::new();
        let run = durable_run_record("batch-single");
        store.insert_run(run).await.unwrap();

        store
            .append_events_batch(
                "u1",
                "batch-single",
                &[make_event("run_started", json!({}))],
            )
            .await
            .unwrap();

        let loaded = store.load_run("u1", "batch-single").await.unwrap().unwrap();
        assert_eq!(loaded.events.len(), 1);
        assert_eq!(loaded.last_event_idx, 0);
    }

    #[tokio::test]
    async fn append_events_batch_preserves_sequential_semantics() {
        // Batch and sequential append must produce identical state.
        let store_batch = InMemoryRunStateStore::new();
        let store_seq = InMemoryRunStateStore::new();

        let events: Vec<_> = (0..5)
            .map(|i| make_event("text_delta", json!({"chunk": i.to_string()})))
            .collect();

        store_batch
            .insert_run(durable_run_record("r-batch"))
            .await
            .unwrap();
        store_seq
            .insert_run(durable_run_record("r-seq"))
            .await
            .unwrap();

        store_batch
            .append_events_batch("u1", "r-batch", &events)
            .await
            .unwrap();
        for e in &events {
            store_seq
                .append_event("u1", "r-seq", e.clone())
                .await
                .unwrap();
        }

        let batch = store_batch
            .load_run("u1", "r-batch")
            .await
            .unwrap()
            .unwrap();
        let seq = store_seq.load_run("u1", "r-seq").await.unwrap().unwrap();
        assert_eq!(batch.events.len(), seq.events.len());
        assert_eq!(batch.last_event_idx, seq.last_event_idx);
        for (i, (be, se)) in batch.events.iter().zip(seq.events.iter()).enumerate() {
            assert_eq!(be, se, "event {i} differs between batch and sequential");
        }
    }

    #[tokio::test]
    async fn in_memory_event_append_enforces_idempotency_keys() {
        let store = InMemoryRunStateStore::new();
        store
            .insert_run(durable_run_record("idempotent-append"))
            .await
            .unwrap();
        let event = json!({
            "event_type": "user_intent_applied",
            "idempotency_key": "user_intent_applied:intent-1",
            "data": {"intent_id": "intent-1", "event_index": 1}
        });

        let (first, second) = tokio::join!(
            store.append_event("u1", "idempotent-append", event.clone()),
            store.append_event("u1", "idempotent-append", event),
        );
        first.unwrap();
        second.unwrap();

        let run = store
            .load_run("u1", "idempotent-append")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.events.len(), 1);
        assert_eq!(run.last_event_idx, 0);
    }

    #[tokio::test]
    async fn in_memory_same_state_transition_reports_duplicate_event_conflict() {
        let store = InMemoryRunStateStore::new();
        store
            .insert_run(durable_run_record("idempotent-transition"))
            .await
            .unwrap();
        let event = json!({
            "event_type": "user_intent",
            "idempotency_key": "user_intent:intent-1",
            "data": {"intent_id": "intent-1"}
        });

        let (first, second) = tokio::join!(
            store.update_run_status_with_events_if_current(
                "u1",
                "idempotent-transition",
                &["running"],
                "running",
                None,
                None,
                std::slice::from_ref(&event),
            ),
            store.update_run_status_with_events_if_current(
                "u1",
                "idempotent-transition",
                &["running"],
                "running",
                None,
                None,
                std::slice::from_ref(&event),
            ),
        );
        let mut outcomes = [first.unwrap(), second.unwrap()];
        outcomes.sort_unstable();
        assert_eq!(outcomes, [false, true]);

        let run = store
            .load_run("u1", "idempotent-transition")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.events.len(), 1);
    }
}

#[cfg(test)]
mod context_meta_transform_contract_tests {
    use super::transform_run_event_for_client;
    use serde_json::json;

    #[test]
    fn context_meta_journal_event_preserves_manifest_trace_for_clients() {
        let trace = json!({
            "source": "llm_context",
            "wire": {"total_cache_control_count": 2}
        });
        let out = transform_run_event_for_client(json!({
            "event_type": "context_meta",
            "data": {
                "system_prompt_tokens": 99,
                "context_manifest_trace": trace
            }
        }));

        assert_eq!(out["type"], "context_meta");
        assert_eq!(out["system_prompt_tokens"], 99);
        assert_eq!(out["context_manifest_trace"], trace);
    }

    #[test]
    fn already_shaped_context_meta_preserves_manifest_trace_for_clients() {
        let event = json!({
            "type": "context_meta",
            "system_prompt_tokens": 88,
            "context_manifest_trace": {
                "source": "llm_context_bridge",
                "wire": {"message_cache_control_count": 1}
            }
        });

        let out = transform_run_event_for_client(event.clone());

        assert_eq!(out, event);
    }
}
