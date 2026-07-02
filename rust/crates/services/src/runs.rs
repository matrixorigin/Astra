use astra_core::{
    ErrorResponse, STATUS_CANCELLED, STATUS_COMPLETED, STATUS_FAILED, STATUS_INPUT_QUEUED,
    STATUS_PAUSED, STATUS_RUNNING, STATUS_WAITING, SharedPool, SubRunState, error_response,
    error_response_coded,
};
use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};
use thiserror::Error;
use uuid::Uuid;

use crate::db_row::RowExt as RunStateDbRow;

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

    async fn list_runs(
        &self,
        user_id: String,
        limit: u32,
        offset: u32,
    ) -> Result<RunListRecord, (StatusCode, Json<ErrorResponse>)>;

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

    async fn submit_run_input(
        &self,
        _run_id: String,
        _user_id: String,
        _input: RunInputData,
    ) -> Result<RunInputRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Run input not supported",
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmTokenServiceConfig {
    pub url: String,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmTokenServiceRequest {
    pub url: String,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

impl From<LlmTokenServiceRequest> for LlmTokenServiceConfig {
    fn from(value: LlmTokenServiceRequest) -> Self {
        Self {
            url: value.url,
            timeout_ms: value.timeout_ms,
        }
    }
}

impl From<LlmTokenServiceConfig> for LlmTokenServiceRequest {
    fn from(value: LlmTokenServiceConfig) -> Self {
        Self {
            url: value.url,
            timeout_ms: value.timeout_ms,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionBudget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_turns: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hard_turn_limit: Option<u32>,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FallbackPolicyRequest {
    /// Never route a tool call away from the selected executor.
    Disabled,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_policy: Option<FallbackPolicyRequest>,
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
pub struct SelectedModelRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway: Option<String>,
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
    pub parts: Vec<serde_json::Value>,
    pub attachments: Vec<serde_json::Value>,
    pub runtime_system_prompt: Option<String>,
    pub session_id: Option<String>,
    pub full_llm_capture: bool,
    pub agent_id: Option<String>,
    pub model: Option<String>,
    pub selected_model: Option<SelectedModelRequest>,
    pub capability_descriptors: Option<RuntimeCapabilityDescriptorsRequest>,
    pub provider_runtime_authorized: bool,
    pub agent_binding: Option<AgentBindingRuntimeRequest>,
    pub runtime_auth: Option<RuntimeAuthRequest>,
    pub runtime_skill_binding: Option<RuntimeSkillBindingRequest>,
    pub runtime_profile: Option<RuntimeProfileRequest>,
    pub llm_token_service: Option<LlmTokenServiceConfig>,
    pub skill_search: Option<astra_core::SkillSearchSettings>,
    pub allow_skills: Option<Vec<String>>,
    pub allow_skill_sources: Option<Vec<String>>,
    pub allow_tools: Option<Vec<String>>,
    pub workspace_binding: Option<WorkspaceBindingRequest>,
    pub executor_binding: Option<ExecutorBindingRequest>,
    pub runtime_mcp_bindings: Vec<RuntimeMcpBindingRequest>,
    pub mcp_binding_ids: Option<Vec<String>>,
    pub context: Option<serde_json::Map<String, serde_json::Value>>,
    pub edge_executor_id: Option<String>,
    pub capabilities: Vec<String>,
    pub forward_headers: std::collections::HashMap<String, String>,
    pub execution_budget: Option<ExecutionBudget>,
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
            .field("parts", &self.parts)
            .field("attachments", &self.attachments)
            .field("runtime_system_prompt", &self.runtime_system_prompt)
            .field("session_id", &self.session_id)
            .field("agent_id", &self.agent_id)
            .field("model", &self.model)
            .field("selected_model", &self.selected_model)
            .field("capability_descriptors", &self.capability_descriptors)
            .field(
                "provider_runtime_authorized",
                &self.provider_runtime_authorized,
            )
            .field("agent_binding", &self.agent_binding)
            .field("runtime_auth", &self.runtime_auth)
            .field("runtime_skill_binding", &self.runtime_skill_binding)
            .field("runtime_profile", &self.runtime_profile)
            .field("llm_token_service", &self.llm_token_service)
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
    pub status: String,
    pub waiting_for: Option<String>,
    pub events_count: i64,
    pub workspace: Option<serde_json::Value>,
    pub executor: Option<serde_json::Value>,
    pub transport: Option<String>,
    pub fallback_policy: Option<String>,
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
    pub fallback_policy: Option<String>,
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunMutationRecord {
    pub run_id: String,
    pub status: String,
    pub previous_status: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunInputData {
    pub idempotency_key: String,
    pub input: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunInputRecord {
    pub run_id: String,
    pub accepted: bool,
    pub duplicate: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RunListRecord {
    pub runs: Vec<RunStatusRecord>,
    pub total: i64,
    pub limit: u32,
    pub offset: u32,
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
    pub selected_model_json: Option<String>,
    pub selected_model_name: Option<String>,
    pub selected_model_gateway: Option<String>,
    pub capability_server_refs_json: Option<String>,
    pub runtime_profile: Option<String>,
    pub events: Vec<serde_json::Value>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DurableRunStatusKind {
    Running,
    InputQueued,
    Waiting,
    Paused,
    Completed,
    Failed,
    Cancelled,
    Other,
}

pub fn durable_run_status_kind(status: &str) -> DurableRunStatusKind {
    match status {
        STATUS_RUNNING => DurableRunStatusKind::Running,
        STATUS_INPUT_QUEUED => DurableRunStatusKind::InputQueued,
        STATUS_WAITING => DurableRunStatusKind::Waiting,
        STATUS_PAUSED => DurableRunStatusKind::Paused,
        STATUS_COMPLETED => DurableRunStatusKind::Completed,
        STATUS_FAILED => DurableRunStatusKind::Failed,
        STATUS_CANCELLED => DurableRunStatusKind::Cancelled,
        _ => DurableRunStatusKind::Other,
    }
}

pub fn durable_run_status_is_terminal(status: &str) -> bool {
    matches!(
        durable_run_status_kind(status),
        DurableRunStatusKind::Completed
            | DurableRunStatusKind::Failed
            | DurableRunStatusKind::Cancelled
    )
}

pub fn durable_run_status_blocks_session(status: &str, waiting_for: Option<&str>) -> bool {
    matches!(
        durable_run_status_kind(status),
        DurableRunStatusKind::Running
            | DurableRunStatusKind::InputQueued
            | DurableRunStatusKind::Waiting
    ) || (durable_run_status_kind(status) == DurableRunStatusKind::Paused && waiting_for.is_some())
}

pub fn durable_run_status_to_subrun_state(status: &str) -> SubRunState {
    match durable_run_status_kind(status) {
        DurableRunStatusKind::Running | DurableRunStatusKind::InputQueued => SubRunState::Running,
        DurableRunStatusKind::Waiting | DurableRunStatusKind::Paused => SubRunState::Paused,
        DurableRunStatusKind::Completed => SubRunState::Completed,
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
     agent_binding_schema_version, selected_model_json, selected_model_name, \
     selected_model_gateway, capability_server_refs_json, runtime_profile, created_at, updated_at";

const RUN_DISPLAY_PROJECTION_COLUMNS: &str = "run_id, user_id, session_id, status, waiting_for, \
     error_message, projection_event_idx, latest_event_type, latest_checkpoint_id, \
     latest_checkpoint_kind, latest_checkpoint_version, total_prompt_tokens, \
     total_completion_tokens, total_tool_calls, projection_hash, updated_at";

/// Abstraction for durable run persistence.
///
/// Implementations:
/// - `InMemoryRunStateStore` — deterministic durable fake for tests
/// - `DatabaseRunStateStore` — MatrixOne-backed persistence
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

    /// List runs for a user with pagination.
    async fn list_user_runs(
        &self,
        user_id: &str,
        limit: u32,
        offset: u32,
    ) -> Result<(Vec<DurableRunRecord>, i64), String>;

    /// Find runs in WAITING status (for resume engine).
    async fn find_waiting_runs(&self) -> Result<Vec<DurableRunRecord>, String>;

    /// Find runs in RUNNING status.
    async fn find_running_runs(&self) -> Result<Vec<DurableRunRecord>, String>;

    /// Find active runs this store owner may recover after startup.
    ///
    /// Implementations backed by shared durable storage must not return rows
    /// owned by a different live pod with an unexpired lease. Otherwise one
    /// app instance can falsely mark another instance's live run as crashed.
    async fn find_recoverable_running_runs(&self) -> Result<Vec<DurableRunRecord>, String> {
        self.find_running_runs().await
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

fn run_requires_session_exclusive_start(record: &DurableRunRecord) -> bool {
    record.parent_run_id.is_none()
        && record.retry_of.is_none()
        && record.delegation_id.is_none()
        && record.agent_id.is_none()
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

#[async_trait]
impl RunStateStore for InMemoryRunStateStore {
    async fn insert_run(&self, record: DurableRunRecord) -> Result<(), String> {
        let mut runs = self.runs.write().await;
        let run_id = record.run_id.clone();
        if run_requires_session_exclusive_start(&record)
            && runs.values().any(|run| {
                run.user_id == record.user_id
                    && run.session_id == record.session_id
                    && durable_run_status_blocks_session(&run.status, run.waiting_for.as_deref())
            })
        {
            return Err("session already has an active run".to_string());
        }
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
        drop(runs);
        if !evicted_ids.is_empty() {
            let mut projections = self.projections.write().await;
            let mut checkpoints = self.checkpoints.write().await;
            for id in evicted_ids {
                projections.remove(&id);
                checkpoints.remove(&id);
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
        let updated = {
            let mut runs = self.runs.write().await;
            if let Some(run) = runs.get_mut(run_id) {
                if run.user_id != user_id {
                    None
                } else {
                    run.status = status.to_string();
                    run.waiting_for = waiting_for.map(ToString::to_string);
                    if let Some(msg) = error_message {
                        run.error_message = Some(msg.to_string());
                    }
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
        let updated = {
            let mut runs = self.runs.write().await;
            if let Some(run) = runs.get_mut(run_id) {
                if run.user_id != user_id || !expected_statuses.contains(&run.status.as_str()) {
                    None
                } else {
                    run.status = status.to_string();
                    run.waiting_for = waiting_for.map(ToString::to_string);
                    if let Some(msg) = error_message {
                        run.error_message = Some(msg.to_string());
                    }
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
        let latest_event_type = extract_event_type(&event);
        let terminal_error_code =
            terminal_error_code_from_events(status, std::slice::from_ref(&event));
        let updated = {
            let mut runs = self.runs.write().await;
            if let Some(run) = runs.get_mut(run_id) {
                if run.user_id != user_id || !expected_statuses.contains(&run.status.as_str()) {
                    None
                } else {
                    run.status = status.to_string();
                    run.waiting_for = waiting_for.map(ToString::to_string);
                    if let Some(msg) = error_message {
                        run.error_message = Some(msg.to_string());
                    }
                    if let Some(code) = terminal_error_code.as_ref() {
                        run.error_code = Some(code.clone());
                    }
                    run.events.push(event);
                    run.last_event_idx = run.events.len() as i64 - 1;
                    run.updated_at = chrono::Utc::now().to_rfc3339();
                    Some(run.clone())
                }
            } else {
                None
            }
        };
        if let Some(run) = updated {
            self.sync_projection(&run, Some(latest_event_type), None)
                .await;
            Ok(true)
        } else {
            Ok(false)
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
        let latest_event_type = events.last().map(extract_event_type);
        let terminal_error_code = terminal_error_code_from_events(status, events);
        let updated = {
            let mut runs = self.runs.write().await;
            if let Some(run) = runs.get_mut(run_id) {
                if run.user_id != user_id || !expected_statuses.contains(&run.status.as_str()) {
                    None
                } else {
                    run.status = status.to_string();
                    run.waiting_for = waiting_for.map(ToString::to_string);
                    if let Some(msg) = error_message {
                        run.error_message = Some(msg.to_string());
                    }
                    if let Some(code) = terminal_error_code.as_ref() {
                        run.error_code = Some(code.clone());
                    }
                    if !events.is_empty() {
                        run.events.extend(events.iter().cloned());
                        run.last_event_idx = run.events.len() as i64 - 1;
                    }
                    run.updated_at = chrono::Utc::now().to_rfc3339();
                    Some(run.clone())
                }
            } else {
                None
            }
        };
        if let Some(run) = updated {
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
        let latest_event_type = extract_event_type(events.last().unwrap());
        let updated = {
            let mut runs = self.runs.write().await;
            let Some(run) = runs.get_mut(run_id) else {
                return Err(format!("run not found while appending events: {run_id}"));
            };
            if run.user_id != user_id {
                return Err(format!("run not found while appending events: {run_id}"));
            }
            let start_idx = run.events.len() as i64;
            run.events.extend(events.iter().cloned());
            run.last_event_idx = start_idx + events.len() as i64 - 1;
            run.updated_at = chrono::Utc::now().to_rfc3339();
            Some(run.clone())
        };
        if let Some(run) = updated {
            self.sync_projection(&run, Some(latest_event_type), None)
                .await;
        }
        Ok(())
    }

    async fn list_user_runs(
        &self,
        user_id: &str,
        limit: u32,
        offset: u32,
    ) -> Result<(Vec<DurableRunRecord>, i64), String> {
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
        let total = user_runs.len() as i64;
        let page = user_runs
            .into_iter()
            .skip(offset as usize)
            .take(limit as usize)
            .collect();
        Ok((page, total))
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
            .filter(|r| {
                matches!(
                    durable_run_status_kind(&r.status),
                    DurableRunStatusKind::Running | DurableRunStatusKind::InputQueued
                )
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
}

type DbStoreResult<T> = Result<T, DatabaseRunStateStoreError>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutputBatchItem {
    pub output_id: String,
    pub tool_call_id: Option<String>,
    pub tool_name: String,
    pub output_json: serde_json::Value,
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
}

impl DatabaseRunStateStore {
    pub const DEFAULT_LEASE_TTL: Duration = Duration::from_secs(45);

    pub fn new(pool: SharedPool) -> Self {
        Self {
            pool,
            owner_pod_id: default_owner_pod_id(),
            lease_ttl: Self::DEFAULT_LEASE_TTL,
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

    pub fn owner_pod_id(&self) -> &str {
        &self.owner_pod_id
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
        let payloads = items
            .iter()
            .map(|item| {
                serde_json::to_string(&item.output_json).map_err(|source| {
                    DatabaseRunStateStoreError::Json {
                        operation: "serialize_tool_output",
                        entity: item.output_id.clone(),
                        source,
                    }
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
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
        let projection = build_run_display_projection(
            &run,
            latest_event_type.map(ToOwned::to_owned).or_else(|| {
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
              idempotency_key, event_hash, producer_pod_id, payload_json, created_at) ",
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
                .push_bind(&event.idempotency_key)
                .push_bind(&event.event_hash)
                .push_bind(&event.producer_pod_id)
                .push_bind(&event.payload_json)
                .push("NOW(6)");
        });

        match builder.build().execute(self.pool.get()).await {
            Ok(_) => {}
            Err(sqlx::Error::Database(db_err)) => {
                // MySQL error 1062 / SQLSTATE 23000 = duplicate key.
                // INSERT IGNORE should prevent this, but if a UNIQUE constraint
                // fires anyway (e.g. on a non-idempotency-key column), propagate.
                if db_err.code() == Some(std::borrow::Cow::Borrowed("23000"))
                    && db_err.message().contains("idempotency_key")
                {
                    tracing::warn!(
                        target: "astra_services::runs",
                        run_id = %run_id,
                        "Idempotency key conflict in batch insert, skipping"
                    );
                } else {
                    return Err(db_error(
                        "insert_run_events_batch",
                        run_id,
                        sqlx::Error::Database(db_err),
                    ));
                }
            }
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

        let insert_result = if run_requires_session_exclusive_start(&record) {
            sqlx::query(
                "INSERT INTO agent_runs
                 (run_id, user_id, session_id, parent_run_id, root_run_id, ancestor_path, depth,
                  delegation_id, agent_id, retry_of, retry_scope, status, waiting_for,
                  owner_pod_id, owner_lease_expires_at, run_generation, last_event_idx,
                  checkpoint_version, checkpoint_json, error_code, error_message, retry_count,
                  total_prompt_tokens, total_completion_tokens, total_tool_calls,
                  agent_binding_id, agent_binding_name, agent_binding_schema_version,
                  selected_model_json, selected_model_name, selected_model_gateway,
                  capability_server_refs_json, runtime_profile, created_at, updated_at)
                 SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW(6), NOW(6)
                 FROM DUAL
                 WHERE NOT EXISTS (
                     SELECT 1
                     FROM agent_runs
                     WHERE user_id = ?
                       AND session_id = ?
                       AND (status IN ('running', 'waiting') OR (status = 'paused' AND waiting_for IS NOT NULL))
                     LIMIT 1
                 )",
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
            .bind(lease_expires_at.naive_utc())
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
            .bind(&record.selected_model_json)
            .bind(&record.selected_model_name)
            .bind(&record.selected_model_gateway)
            .bind(&record.capability_server_refs_json)
            .bind(&record.runtime_profile)
            .bind(&record.user_id)
            .bind(&record.session_id)
            .execute(self.pool.get())
            .await
            .map_err(|source| db_error("insert_run", &record.run_id, source).to_string())?
        } else {
            sqlx::query(
                "INSERT INTO agent_runs
                 (run_id, user_id, session_id, parent_run_id, root_run_id, ancestor_path, depth,
                  delegation_id, agent_id, retry_of, retry_scope, status, waiting_for,
                  owner_pod_id, owner_lease_expires_at, run_generation, last_event_idx,
                  checkpoint_version, checkpoint_json, error_code, error_message, retry_count,
                  total_prompt_tokens, total_completion_tokens, total_tool_calls,
                  agent_binding_id, agent_binding_name, agent_binding_schema_version,
                  selected_model_json, selected_model_name, selected_model_gateway,
                  capability_server_refs_json, runtime_profile, created_at, updated_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW(6), NOW(6))
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
            .bind(lease_expires_at.naive_utc())
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
            .bind(&record.selected_model_json)
            .bind(&record.selected_model_name)
            .bind(&record.selected_model_gateway)
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

    async fn update_run_status(
        &self,
        user_id: &str,
        run_id: &str,
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
    ) -> Result<bool, String> {
        let result = if let Some(error_message) = error_message {
            sqlx::query(
                "UPDATE agent_runs
                 SET status = ?, waiting_for = ?, error_message = ?, updated_at = NOW(6)
                 WHERE user_id = ? AND run_id = ?",
            )
            .bind(status)
            .bind(waiting_for)
            .bind(error_message)
            .bind(user_id)
            .bind(run_id)
            .execute(self.pool.get())
            .await
            .map_err(|source| db_error("update_run_status", run_id, source).to_string())?
        } else {
            sqlx::query(
                "UPDATE agent_runs
                 SET status = ?, waiting_for = ?, updated_at = NOW(6)
                 WHERE user_id = ? AND run_id = ?",
            )
            .bind(status)
            .bind(waiting_for)
            .bind(user_id)
            .bind(run_id)
            .execute(self.pool.get())
            .await
            .map_err(|source| db_error("update_run_status", run_id, source).to_string())?
        };
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
        Ok(result.rows_affected() > 0)
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
        let mut query = sqlx::QueryBuilder::<sqlx::MySql>::new("UPDATE agent_runs SET status = ");
        query.push_bind(status);
        query.push(", waiting_for = ");
        query.push_bind(waiting_for);
        if let Some(error_message) = error_message {
            query.push(", error_message = ");
            query.push_bind(error_message);
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
        let result = query
            .build()
            .execute(self.pool.get())
            .await
            .map_err(|source| {
                db_error("update_run_status_if_current", run_id, source).to_string()
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
        Ok(result.rows_affected() > 0)
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
        let terminal_error_code =
            terminal_error_code_from_events(status, std::slice::from_ref(&event));

        let mut tx = self.pool.get().begin().await.map_err(|source| {
            db_error("transition_run_status_with_event_begin", run_id, source).to_string()
        })?;

        let Some(row) = sqlx::query(
            "SELECT session_id, agent_id, last_event_idx
             FROM agent_runs WHERE user_id = ? AND run_id = ?",
        )
        .bind(user_id)
        .bind(run_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|source| {
            db_error("transition_run_status_with_event_load_run", run_id, source).to_string()
        })?
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

        let session_id: String = row.try_get("session_id").map_err(|source| {
            db_error(
                "transition_run_status_with_event_decode_session",
                run_id,
                source,
            )
            .to_string()
        })?;
        let agent_id: Option<String> = row.try_get("agent_id").map_err(|source| {
            db_error(
                "transition_run_status_with_event_decode_agent",
                run_id,
                source,
            )
            .to_string()
        })?;
        let last_event_idx: i64 = row.try_get("last_event_idx").map_err(|source| {
            db_error(
                "transition_run_status_with_event_decode_last_event_idx",
                run_id,
                source,
            )
            .to_string()
        })?;
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

        let insert_result = sqlx::query(
            "INSERT INTO agent_run_events
             (id, run_id, event_idx, user_id, session_id, event_type, event_id, agent_id,
              idempotency_key, event_hash, producer_pod_id, payload_json, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW(6))",
        )
        .bind(&event_row.id)
        .bind(&event_row.run_id)
        .bind(event_row.event_idx)
        .bind(&event_row.user_id)
        .bind(&event_row.session_id)
        .bind(&event_row.event_type)
        .bind(&event_row.event_id)
        .bind(&event_row.agent_id)
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

        if let Err(error) = self
            .sync_projection_for_user(user_id, run_id, Some(&event_row.event_type), None)
            .await
        {
            tracing::warn!(
                user_id,
                run_id,
                error = %error,
                "run transition committed but display projection refresh failed"
            );
        }
        Ok(true)
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
        let terminal_error_code = terminal_error_code_from_events(status, events);

        let mut tx = self.pool.get().begin().await.map_err(|source| {
            db_error("transition_run_status_with_events_begin", run_id, source).to_string()
        })?;

        let Some(row) = sqlx::query(
            "SELECT session_id, agent_id, last_event_idx
             FROM agent_runs WHERE user_id = ? AND run_id = ?",
        )
        .bind(user_id)
        .bind(run_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|source| {
            db_error("transition_run_status_with_events_load_run", run_id, source).to_string()
        })?
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

        let session_id: String = row.try_get("session_id").map_err(|source| {
            db_error(
                "transition_run_status_with_events_decode_session",
                run_id,
                source,
            )
            .to_string()
        })?;
        let agent_id: Option<String> = row.try_get("agent_id").map_err(|source| {
            db_error(
                "transition_run_status_with_events_decode_agent",
                run_id,
                source,
            )
            .to_string()
        })?;
        let last_event_idx: i64 = row.try_get("last_event_idx").map_err(|source| {
            db_error(
                "transition_run_status_with_events_decode_last_event_idx",
                run_id,
                source,
            )
            .to_string()
        })?;

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

        if !event_rows.is_empty() {
            let mut insert = sqlx::QueryBuilder::<sqlx::MySql>::new(
                "INSERT INTO agent_run_events
                 (id, run_id, event_idx, user_id, session_id, event_type, event_id, agent_id,
                  idempotency_key, event_hash, producer_pod_id, payload_json, created_at) ",
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

        if let Err(error) = self
            .sync_projection_for_user(
                user_id,
                run_id,
                event_rows.last().map(|event| event.event_type.as_str()),
                None,
            )
            .await
        {
            tracing::warn!(
                user_id,
                run_id,
                error = %error,
                "run transition committed but display projection refresh failed"
            );
        }
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
            self.sync_projection_for_user(user_id, run_id, None, None)
                .await
                .map_err(|e| e.to_string())?;
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
             WHERE user_id = ? AND run_id = ? AND status NOT IN ('completed', 'failed')",
        )
        .bind(&checkpoint_version)
        .bind(checkpoint_json)
        .bind(user_id)
        .bind(run_id)
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

    async fn list_user_runs(
        &self,
        user_id: &str,
        limit: u32,
        offset: u32,
    ) -> Result<(Vec<DurableRunRecord>, i64), String> {
        let total_row = sqlx::query("SELECT COUNT(*) AS total FROM agent_runs WHERE user_id = ?")
            .bind(user_id)
            .fetch_one(self.pool.get())
            .await
            .map_err(|source| db_error("count_user_runs", user_id, source).to_string())?;
        let total = run_row_non_negative_i64(&total_row, "count_user_runs", "agent_runs", "total")
            .map_err(|e| e.to_string())?;
        let sql = format!(
            "SELECT {AGENT_RUN_COLUMNS} FROM agent_runs \
             WHERE user_id = ? ORDER BY updated_at DESC LIMIT ? OFFSET ?"
        );
        let rows = sqlx::query(&sql)
            .bind(user_id)
            .bind(limit as i64)
            .bind(offset as i64)
            .fetch_all(self.pool.get())
            .await
            .map_err(|source| db_error("list_user_runs", user_id, source).to_string())?;
        let runs = rows
            .into_iter()
            .map(run_record_from_row)
            .collect::<DbStoreResult<Vec<_>>>()
            .map_err(|e| e.to_string())?;
        Ok((runs, total))
    }

    async fn find_waiting_runs(&self) -> Result<Vec<DurableRunRecord>, String> {
        self.find_runs_by_status(STATUS_WAITING).await
    }

    async fn find_running_runs(&self) -> Result<Vec<DurableRunRecord>, String> {
        let sql = format!(
            "SELECT {AGENT_RUN_COLUMNS} FROM agent_runs \
             WHERE status IN (?, ?) ORDER BY updated_at ASC"
        );
        let rows = sqlx::query(&sql)
            .bind(STATUS_RUNNING)
            .bind(STATUS_INPUT_QUEUED)
            .fetch_all(self.pool.get())
            .await
            .map_err(|source| db_error("find_running_runs", "active", source).to_string())?;
        rows.into_iter()
            .map(run_record_from_row)
            .collect::<DbStoreResult<Vec<_>>>()
            .map_err(|e| e.to_string())
    }

    async fn find_recoverable_running_runs(&self) -> Result<Vec<DurableRunRecord>, String> {
        let sql = format!(
            "SELECT {AGENT_RUN_COLUMNS} FROM agent_runs
             WHERE status IN (?, ?)
               AND (
                   owner_pod_id IS NULL
                   OR owner_pod_id = ?
                   OR owner_lease_expires_at IS NULL
                   OR owner_lease_expires_at < NOW(6)
               )
             ORDER BY updated_at ASC",
        );
        let rows = sqlx::query(&sql)
            .bind(STATUS_RUNNING)
            .bind(STATUS_INPUT_QUEUED)
            .bind(&self.owner_pod_id)
            .fetch_all(self.pool.get())
            .await
            .map_err(|source| {
                db_error("find_recoverable_running_runs", "active", source).to_string()
            })?;
        rows.into_iter()
            .map(run_record_from_row)
            .collect::<DbStoreResult<Vec<_>>>()
            .map_err(|e| e.to_string())
    }

    async fn find_blocking_session_run(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> Result<Option<DurableRunRecord>, String> {
        let sql = format!(
            "SELECT {AGENT_RUN_COLUMNS} FROM agent_runs \
             WHERE user_id = ? AND session_id = ? \
               AND (status IN (?, ?, ?) OR (status = ? AND waiting_for IS NOT NULL)) \
             ORDER BY updated_at DESC \
             LIMIT 1",
        );
        let row = sqlx::query(&sql)
            .bind(user_id)
            .bind(session_id)
            .bind(STATUS_RUNNING)
            .bind(STATUS_INPUT_QUEUED)
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
    let preview_text = truncate_utf8_bytes(payload, contract.max_preview_bytes);
    let explicit_artifact_ref = extract_optional_string(&item.output_json, "artifact_ref")
        .or_else(|| extract_optional_string(&item.output_json, "artifact_uri"));
    let large_payload_ref = (payload.len() > contract.max_preview_bytes).then(|| {
        format!(
            "tool_output://{session_id}/{}@{}",
            item.output_id, content_hash
        )
    });
    let preview_status = if !contract.found {
        "fallback"
    } else if payload.len() > contract.max_preview_bytes {
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
        parent_output_id: extract_optional_string(&item.output_json, "parent_output_id"),
    }
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
        selected_model_json: run_row_optional_string(row, operation, table, "selected_model_json")?,
        selected_model_name: run_row_optional_string(row, operation, table, "selected_model_name")?,
        selected_model_gateway: run_row_optional_string(
            row,
            operation,
            table,
            "selected_model_gateway",
        )?,
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
    if let Some(obj) = value.as_object_mut()
        && !obj.contains_key("index")
    {
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
    // Run lifecycle + framing.
    "run_started",
    "run_error",
    "run_interrupted",
    "run_finished",
    "run_waiting",
    "run_blocked",
    "run_paused",
    "run_resumed",
    "run_input_queued",
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
        "fallback_policy",
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
                if let Some(suppressed_loop_nudges) = data.get("suppressed_loop_nudges").cloned() {
                    obj.insert("suppressed_loop_nudges".to_string(), suppressed_loop_nudges);
                }
                if let Some(interactive_client) = data.get("interactive_client").cloned() {
                    obj.insert("interactive_client".to_string(), interactive_client);
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
                if let Some(fallback_policy) = data.get("fallback_policy").cloned() {
                    obj.insert("fallback_policy".to_string(), fallback_policy);
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
                    "error",
                    "error_code",
                    "error_kind",
                    "interrupted",
                    "interruption_kind",
                    "resumable",
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
        "run_input_queued" => {
            let mut out = serde_json::json!({ "type": "run_input_queued" });
            if let Some(obj) = out.as_object_mut() {
                for (k, v) in &data {
                    obj.insert(k.clone(), v.clone());
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
        "agent_spawned" | "agent_progress" | "agent_completed" | "agent_failed"
        | "agent_waiting" | "agent_cancelled" | "agent_interrupted" => {
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

    async fn list_runs(
        &self,
        _user_id: String,
        _limit: u32,
        _offset: u32,
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
            selected_model_json: None,
            selected_model_name: None,
            selected_model_gateway: None,
            capability_server_refs_json: None,
            runtime_profile: None,
            events: vec![],
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
        }
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
                "selected_model_json" => Some(r#"{"name":"model"}"#.to_string()),
                "selected_model_name" => Some("model".to_string()),
                "selected_model_gateway" => Some("gateway".to_string()),
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
            let _ = sqlx::query(sql)
                .bind(user_id)
                .bind(run_id)
                .execute(pool.get())
                .await;
        }
    }

    #[test]
    fn session_exclusive_start_applies_only_to_user_root_runs() {
        let root = durable_run_record("root");
        assert!(run_requires_session_exclusive_start(&root));

        let mut retry = durable_run_record("retry");
        retry.retry_of = Some("root".into());
        assert!(!run_requires_session_exclusive_start(&retry));

        let mut child = durable_run_record("child");
        child.parent_run_id = Some("root".into());
        assert!(!run_requires_session_exclusive_start(&child));

        let mut delegated = durable_run_record("delegated");
        delegated.delegation_id = Some("delegation-1".into());
        assert!(!run_requires_session_exclusive_start(&delegated));

        let mut team_parent = durable_run_record("team-parent");
        team_parent.agent_id = Some("orchestrator".into());
        assert!(!run_requires_session_exclusive_start(&team_parent));
    }

    #[test]
    fn durable_run_status_helpers_keep_terminal_and_blocking_semantics_distinct() {
        assert_eq!(
            durable_run_status_kind(STATUS_RUNNING),
            DurableRunStatusKind::Running
        );
        assert_eq!(
            durable_run_status_kind(STATUS_INPUT_QUEUED),
            DurableRunStatusKind::InputQueued
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
        assert!(durable_run_status_is_terminal(STATUS_FAILED));
        assert!(durable_run_status_is_terminal(STATUS_CANCELLED));
        assert!(!durable_run_status_is_terminal(STATUS_RUNNING));
        assert!(!durable_run_status_is_terminal(STATUS_INPUT_QUEUED));
        assert!(!durable_run_status_is_terminal(STATUS_WAITING));
        assert!(!durable_run_status_is_terminal(STATUS_PAUSED));

        assert!(durable_run_status_blocks_session(STATUS_RUNNING, None));
        assert!(durable_run_status_blocks_session(STATUS_INPUT_QUEUED, None));
        assert!(durable_run_status_blocks_session(STATUS_WAITING, None));
        assert!(durable_run_status_blocks_session(
            STATUS_PAUSED,
            Some("tool_approval")
        ));
        assert!(!durable_run_status_blocks_session(STATUS_PAUSED, None));
        assert!(!durable_run_status_blocks_session(STATUS_COMPLETED, None));
        assert_eq!(
            durable_run_status_to_subrun_state(STATUS_WAITING),
            SubRunState::Paused
        );
        assert_eq!(
            durable_run_status_to_subrun_state(STATUS_RUNNING),
            SubRunState::Running
        );
        assert_eq!(
            durable_run_status_to_subrun_state(STATUS_INPUT_QUEUED),
            SubRunState::Running
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
            "selected_model_json",
            "selected_model_name",
            "selected_model_gateway",
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
    fn acquire_owner_lease_update_is_owner_bound() {
        let source = include_str!("runs.rs");
        let body = source
            .split("pub async fn acquire_owner_lease(")
            .nth(1)
            .and_then(|rest| rest.split("pub async fn insert_tool_output_batch").next())
            .expect("acquire_owner_lease body");

        assert!(
            body.contains("user_id: &str"),
            "lease acquisition must take the owner boundary explicitly"
        );
        assert!(
            body.contains("WHERE user_id = ?") && body.contains("AND run_id = ?"),
            "lease acquisition must not update agent_runs by bare run_id"
        );
        assert!(
            !body.contains("WHERE run_id = ?"),
            "lease acquisition must not retain the old ownerless predicate"
        );
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

    #[test]
    fn sse_text_events() {
        // text_delta
        let out = transform_run_event_for_client(make_event("text_delta", json!({"chunk": "hi"})));
        assert_eq!(out["type"], "text_delta");
        assert_eq!(out["content"], "hi");
        // text_delta missing chunk
        let out = transform_run_event_for_client(make_event("text_delta", json!({})));
        assert_eq!(out["content"], "");
        // assistant_delta maps to text_delta
        let out =
            transform_run_event_for_client(make_event("assistant_delta", json!({"text": "hi"})));
        assert_eq!(out["type"], "text_delta");
        assert_eq!(out["content"], "hi");
        // text_done
        let out =
            transform_run_event_for_client(make_event("text_done", json!({"full_text": "all"})));
        assert_eq!(out["type"], "text_done");
        assert_eq!(out["full_text"], "all");
    }

    #[test]
    fn sse_thinking_events() {
        // reasoning_message_content
        let out = transform_run_event_for_client(make_event(
            "reasoning_message_content",
            json!({"content": "think"}),
        ));
        assert_eq!(out["type"], "reasoning_message_content");
        assert_eq!(out["content"], "think");
        // thinking_delta
        let out =
            transform_run_event_for_client(make_event("thinking_delta", json!({"chunk": "t"})));
        assert_eq!(out["type"], "thinking_delta");
        assert_eq!(out["content"], "t");
        // thinking_done
        let out = transform_run_event_for_client(make_event(
            "thinking_done",
            json!({"full_text": "all think"}),
        ));
        assert_eq!(out["type"], "thinking_done");
        // reasoning_done
        let out = transform_run_event_for_client(make_event(
            "reasoning_done",
            json!({"full_text": "reason"}),
        ));
        assert_eq!(out["type"], "reasoning_done");
    }

    #[test]
    fn tool_call_start() {
        let out = transform_run_event_for_client(make_event(
            "tool_call_start",
            json!({
                "name": "bash",
                "tool_call_id": "c1",
                "args": {"command": "ls"},
                "workspace": {"kind": "server_sandbox", "cwd": "/tmp/astra-workspaces/run-1"},
                "executor": {"kind": "server_local", "transport": "server_local"},
                "transport": "server_local",
                "fallback_policy": "disabled"
            }),
        ));
        assert_eq!(out["type"], "tool_call_start");
        assert_eq!(out["tool"], "bash");
        assert_eq!(out["call_id"], "c1");
        assert_eq!(out["arguments"]["command"], "ls");
        assert_eq!(out["workspace"]["kind"], "server_sandbox");
        assert_eq!(out["executor"]["kind"], "server_local");
        assert_eq!(out["transport"], "server_local");
        assert_eq!(out["fallback_policy"], "disabled");
    }

    #[test]
    fn tool_result() {
        let out = transform_run_event_for_client(make_event(
            "tool_result",
            json!({
                "tool_call_id": "c1",
                "name": "bash",
                "output": "ok",
                "success": true,
                "duration_ms": 42,
                "workspace": {"kind": "edge_workspace", "cwd": "/Users/xupeng/github/astra"},
                "executor": {"kind": "edge_agent", "executor_id": "edge-1", "transport": "edge_ws"},
                "transport": "edge_ws",
                "fallback_policy": "disabled"
            }),
        ));
        assert_eq!(out["type"], "tool_call_end");
        assert_eq!(out["call_id"], "c1");
        assert_eq!(out["tool"], "bash");
        assert_eq!(out["result"], "ok");
        assert_eq!(out["success"], true);
        assert_eq!(out["duration_ms"], 42);
        assert_eq!(out["workspace"]["kind"], "edge_workspace");
        assert_eq!(out["executor"]["executor_id"], "edge-1");
        assert_eq!(out["transport"], "edge_ws");
        assert_eq!(out["fallback_policy"], "disabled");
    }

    #[test]
    fn run_started_and_finished() {
        let started = transform_run_event_for_client(make_event(
            "run_started",
            json!({
                "run_id": "run-1",
                "session_id": "sess-1",
                "interaction_mode": "auto",
                "suppressed_loop_nudges": true,
                "interactive_client": true,
                "workspace": {"kind": "server_sandbox", "cwd": "/tmp/astra-workspaces/run-1"},
                "executor": {"kind": "server_local", "status": "online"},
                "transport": "server_local",
                "fallback_policy": "disabled"
            }),
        ));
        assert_eq!(started["type"], "run_started");
        assert_eq!(started["run_id"], "run-1");
        assert_eq!(started["session_id"], "sess-1");
        assert_eq!(started["interaction_mode"], "auto");
        assert_eq!(started["suppressed_loop_nudges"], true);
        assert_eq!(started["interactive_client"], true);
        assert_eq!(started["workspace"]["kind"], "server_sandbox");
        assert_eq!(started["workspace"]["cwd"], "/tmp/astra-workspaces/run-1");
        assert_eq!(started["executor"]["kind"], "server_local");
        assert_eq!(started["executor"]["status"], "online");
        assert_eq!(started["transport"], "server_local");
        assert_eq!(started["fallback_policy"], "disabled");

        let finished = transform_run_event_for_client(make_event(
            "run_finished",
            json!({"run_id": "run-1", "status": "failed", "error": "boom", "error_code": "network"}),
        ));
        assert_eq!(finished["type"], "run_finished");
        assert_eq!(finished["run_id"], "run-1");
        assert_eq!(finished["status"], "failed");
        assert_eq!(finished["error"], "boom");
        assert_eq!(finished["error_code"], "network");

        let waiting = transform_run_event_for_client(make_event(
            "run_waiting",
            json!({"reason": "waiting: executor_offline"}),
        ));
        assert_eq!(waiting["type"], "run_waiting");
        assert_eq!(waiting["reason"], "waiting: executor_offline");
    }

    #[test]
    fn run_error_maps_to_run_lifecycle_type() {
        let out = transform_run_event_for_client(make_event("run_error", json!({"error": "boom"})));
        assert_eq!(out["type"], "run_error");
        assert_eq!(out["message"], "boom");
        assert_eq!(out["error"], "boom");
        assert_eq!(out["code"], "RUN_ERROR");
    }

    #[test]
    fn run_error_uses_semantic_client_code_when_error_kind_is_known() {
        let out = transform_run_event_for_client(make_event(
            "run_error",
            json!({"error": "slow down", "error_kind": "rate_limit"}),
        ));
        assert_eq!(out["type"], "run_error");
        assert_eq!(out["message"], "slow down");
        assert_eq!(out["error_kind"], "rate_limit");
        assert_eq!(out["error_code"], "rate_limit");
        assert_eq!(out["code"], "LLM_RATE_LIMIT");
        assert_eq!(out["retryable"], true);
        assert_eq!(out["retry_after_ms"], 5_000);

        let out = transform_run_event_for_client(make_event(
            "run_error",
            json!({"error": "provider 500", "error_kind": "server_error"}),
        ));
        assert_eq!(out["code"], "SERVER_ERROR");
        assert_eq!(out["error_code"], "server_error");
        assert_eq!(out["retryable"], true);
        assert_eq!(out["retry_after_ms"], 2_000);
    }

    #[test]
    fn run_error_default_message() {
        let out = transform_run_event_for_client(make_event("run_error", json!({})));
        assert_eq!(out["message"], "Unknown error");
    }

    #[test]
    fn run_interrupted_is_client_visible() {
        let out = transform_run_event_for_client(make_event(
            "run_interrupted",
            json!({
                "kind": "budget_exhausted",
                "resumable": true,
                "user_message": "You can continue in the next message."
            }),
        ));
        assert_eq!(out["type"], "run_interrupted");
        assert_eq!(out["kind"], "budget_exhausted");
        assert_eq!(out["resumable"], true);
        assert_eq!(out["message"], "You can continue in the next message.");
    }

    #[test]
    fn approval_and_user_input_events_are_client_visible_for_replay() {
        let approval = transform_run_event_for_client(make_event(
            "approval_request",
            json!({"approval_id": "approval-1"}),
        ));
        assert_eq!(approval["type"], "approval_required");
        assert_eq!(approval["approval_id"], "approval-1");
        let canonical_approval = transform_run_event_for_client(make_event(
            "approval_required",
            json!({"approval_id": "approval-2"}),
        ));
        assert_eq!(canonical_approval["type"], "approval_required");
        assert_eq!(canonical_approval["approval_id"], "approval-2");

        let input =
            transform_run_event_for_client(make_event("user_input", json!({"text": "approved"})));
        assert_eq!(input["type"], "user_input");
        assert_eq!(input["text"], "approved");
    }

    #[test]
    fn plan_events() {
        let created = transform_run_event_for_client(make_event(
            "plan_created",
            json!({"plan": {"steps": []}}),
        ));
        assert_eq!(created["type"], "plan_created");
        let step_start =
            transform_run_event_for_client(make_event("plan_step_start", json!({"step": "s1"})));
        assert_eq!(step_start["type"], "plan_step_start");
        let step_done = transform_run_event_for_client(make_event(
            "plan_step_done",
            json!({"step": "s1", "result": "ok"}),
        ));
        assert_eq!(step_done["type"], "plan_step_done");
        let revised =
            transform_run_event_for_client(make_event("plan_revised", json!({"plan": {}})));
        assert_eq!(revised["type"], "plan_revised");
    }

    #[test]
    fn agent_events() {
        let delegated = transform_run_event_for_client(make_event(
            "agent_delegated",
            json!({"agent_id": "a1", "task": "t"}),
        ));
        assert_eq!(delegated["type"], "agent_delegated");
        let progress = transform_run_event_for_client(make_event(
            "agent_progress",
            json!({"agent_id": "a1", "progress": "50%"}),
        ));
        assert_eq!(progress["type"], "agent_progress");
        let completed = transform_run_event_for_client(make_event(
            "agent_completed",
            json!({"agent_id": "a1", "result": "done"}),
        ));
        assert_eq!(completed["type"], "agent_completed");
        let failed = transform_run_event_for_client(make_event(
            "agent_failed",
            json!({"agent_id": "a1", "error": "boom"}),
        ));
        assert_eq!(failed["type"], "agent_failed");
        let waiting = transform_run_event_for_client(make_event(
            "agent_waiting",
            json!({
                "agent_id": "a1",
                "reason": "executor_offline",
                "workspace": {"kind": "edge_workspace", "cwd": "/repo"},
                "executor": {"kind": "edge_agent", "status": "offline"},
                "transport": "edge_ws",
            }),
        ));
        assert_eq!(waiting["type"], "agent_waiting");
        assert_eq!(waiting["agent_id"], "a1");
        assert_eq!(waiting["reason"], "executor_offline");
        assert_eq!(waiting["workspace"]["kind"], "edge_workspace");
        assert_eq!(waiting["executor"]["kind"], "edge_agent");
        assert_eq!(waiting["transport"], "edge_ws");
        let cancelled = transform_run_event_for_client(make_event(
            "agent_cancelled",
            json!({"agent_id": "a1", "reason": "user request"}),
        ));
        assert_eq!(cancelled["type"], "agent_cancelled");
        let interrupted = transform_run_event_for_client(make_event(
            "agent_interrupted",
            json!({"agent_id": "a1", "reason": "budget_exhausted"}),
        ));
        assert_eq!(interrupted["type"], "agent_interrupted");
    }

    #[test]
    fn keepalive_maps_to_ping() {
        let out = transform_run_event_for_client(make_event("keepalive", json!({})));
        assert_eq!(out["type"], "ping");
    }

    #[test]
    fn unknown_and_missing_events_are_dropped() {
        // unknown event_type (with and without data)
        assert!(transform_run_event_for_client(make_event("custom_event", json!({}))).is_null());
        assert!(
            transform_run_event_for_client(make_event(
                "team_prepare",
                json!({"delegation_id":"d1","phase":"prepare"})
            ))
            .is_null()
        );

        // missing event_type AND type
        assert!(transform_run_event_for_client(json!({"data": {}})).is_null());

        // missing data object but event_type present
        let out = transform_run_event_for_client(json!({"event_type": "text_delta"}));
        assert_eq!(out["type"], "text_delta");
        assert_eq!(out["content"], "");
    }

    #[test]
    fn already_shaped_client_events_allowlist() {
        // not in allowlist → dropped
        let event = json!({"type": "injection_freshness", "channels": [{"tag":"self_awareness","hash":0u64,"bytes":0u64,"is_empty":true}]});
        assert!(
            transform_run_event_for_client(event).is_null(),
            "client-shaped internal event must be dropped"
        );

        // in allowlist → pass through
        let event = json!({"type": "text_delta", "content": "hello", "index": 3});
        assert_eq!(transform_run_event_for_client(event.clone()), event);

        // work surface tool events → pass through
        let start = json!({"type": "tool_call", "tool_call": {"id": "c1"}});
        let end = json!({"type": "tool_call_end", "call_id": "c1", "result": "ok"});
        assert_eq!(transform_run_event_for_client(start.clone()), start);
        assert_eq!(transform_run_event_for_client(end.clone()), end);
    }

    #[test]
    fn already_shaped_agent_live_events_pass_through() {
        let event = json!({
            "type": "agent_live_event",
            "agent_id": "agent-1",
            "event_kind": "output_delta",
            "content": "child output",
        });
        assert_eq!(transform_run_event_for_client(event.clone()), event);

        let waiting = json!({
            "type": "agent_waiting",
            "agent_id": "agent-1",
            "reason": "executor_offline",
            "workspace": {"kind": "edge_workspace", "cwd": "/repo"},
            "executor": {"kind": "edge_agent", "status": "offline"},
            "transport": "edge_ws",
        });
        assert_eq!(transform_run_event_for_client(waiting.clone()), waiting);
    }

    #[test]
    fn already_shaped_work_surface_binding_and_transport_events_pass_through() {
        for event in [
            json!({
                "type": "workspace_bound",
                "workspace": {"kind": "server_sandbox"},
                "executor": {"kind": "server_local"},
            }),
            json!({
                "type": "executor_bound",
                "workspace": {"kind": "server_sandbox"},
                "executor": {"kind": "server_local"},
            }),
            json!({
                "type": "tool_routing_decision",
                "call_id": "c1",
                "tool": "bash",
                "route": "server_sandbox",
            }),
            json!({
                "type": "tool_transport_started",
                "call_id": "c1",
                "tool": "bash",
            }),
            json!({
                "type": "tool_transport_completed",
                "call_id": "c1",
                "duration_ms": 12,
            }),
            json!({
                "type": "tool_transport_failed",
                "call_id": "c1",
                "error": "offline",
            }),
            json!({
                "type": "run_blocked",
                "call_id": "c1",
                "tool": "bash",
                "reason": "executor_offline",
            }),
            json!({
                "type": "run_blocked",
                "call_id": "c1",
                "tool": "bash",
                "reason": "transport_disconnected",
            }),
            json!({
                "type": "run_blocked",
                "call_id": "c1",
                "tool": "bash",
                "reason": "fallback_disabled",
            }),
            json!({
                "type": "run_blocked",
                "call_id": "c1",
                "tool": "bash",
                "reason": "workspace_executor_unavailable",
                "workspace": {"kind": "cloud_workspace"},
                "executor": {"kind": "orchestrator_managed", "status": "degraded"},
            }),
        ] {
            assert_eq!(transform_run_event_for_client(event.clone()), event);
        }
    }

    #[test]
    fn durable_blocked_run_event_transform_for_client() {
        let out = transform_run_event_for_client(json!({
            "event_type": "run_blocked",
            "data": {
                "call_id": "c1",
                "tool": "bash",
                "reason": "workspace_executor_unavailable",
                "message": "Workspace is not routed to an available executor.",
                "workspace": {"kind": "cloud_workspace"},
                "executor": {"kind": "orchestrator_managed", "status": "degraded"},
                "transport": "sandbox_resident_agent",
                "fallback_policy": "disabled"
            },
            "index": 4
        }));

        assert_eq!(
            out,
            json!({
                "type": "run_blocked",
                "call_id": "c1",
                "tool": "bash",
                "reason": "workspace_executor_unavailable",
                "message": "Workspace is not routed to an available executor.",
                "workspace": {"kind": "cloud_workspace"},
                "executor": {"kind": "orchestrator_managed", "status": "degraded"},
                "transport": "sandbox_resident_agent",
                "fallback_policy": "disabled"
            })
        );
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
            parts: Vec::new(),
            attachments: Vec::new(),
            runtime_system_prompt: None,
            session_id: Some("sess-1".to_string()),
            agent_id: None,
            model: None,
            selected_model: None,
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
            forward_headers,
            execution_budget: Some(ExecutionBudget {
                initial_turns: Some(10),
                hard_turn_limit: Some(18),
            }),
            full_llm_capture: false,
            explain: false,
            interaction_mode: None,
            interactive_client: false,
        };

        let rendered = format!("{request:?}");
        assert!(rendered.contains("authorization"));
        assert!(rendered.contains("x-workspace-id"));
        assert!(!rendered.contains("Bearer secret-token"));
        assert!(!rendered.contains("ws-123"));
        assert!(!rendered.contains("__astra_connection_tokens"));
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
            parts: Vec::new(),
            attachments: Vec::new(),
            runtime_system_prompt: None,
            session_id: Some("sess-1".to_string()),
            agent_id: None,
            model: None,
            selected_model: Some(SelectedModelRequest {
                id: None,
                model: "gpt-4".to_string(),
                gateway: Some("primary-gateway".to_string()),
            }),
            capability_descriptors: None,
            provider_runtime_authorized: false,
            agent_binding: None,
            runtime_auth: Some(RuntimeAuthRequest {
                authorization: "Bearer secret-runtime-token".to_string(),
            }),
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
            full_llm_capture: false,
            explain: false,
            interaction_mode: None,
            interactive_client: false,
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
                    parts: Vec::new(),
                    attachments: Vec::new(),
                    runtime_system_prompt: None,
                    session_id: None,
                    agent_id: None,
                    model: None,
                    selected_model: None,
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
                    execution_budget: Some(ExecutionBudget {
                        initial_turns: Some(25),
                        hard_turn_limit: Some(40),
                    }),
                    full_llm_capture: false,
                    explain: false,
                    interaction_mode: None,
                    interactive_client: false,
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

        // Fill to capacity + 10 with completed runs
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
                selected_model_json: None,
                selected_model_name: None,
                selected_model_gateway: None,
                capability_server_refs_json: None,
                runtime_profile: None,
                events: vec![],
                created_at: chrono::Utc::now().to_rfc3339(),
                updated_at: chrono::Utc::now().to_rfc3339(),
            };
            store.insert_run(record).await.unwrap();
        }

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

        let loaded = store
            .load_run(&user_id, &run_id)
            .await
            .expect("load run")
            .expect("run exists");
        assert_eq!(loaded.status, STATUS_FAILED);
        assert_eq!(loaded.error_message.as_deref(), Some("boom"));
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
