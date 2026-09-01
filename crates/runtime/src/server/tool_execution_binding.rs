use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::tool_route_selection::ToolExecutionRouteKind;

// Re-export canonical workspace/environment types from astra-runtime-env.
pub use astra_runtime_env::{ExecutorStatus, WorkspaceAuthority, WorkspaceBindingKind};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceBinding {
    pub kind: WorkspaceBindingKind,
    pub display_name: String,
    pub cwd: Option<String>,
    pub authority: WorkspaceAuthority,
}

impl WorkspaceBinding {
    pub fn none() -> Self {
        Self {
            kind: WorkspaceBindingKind::None,
            display_name: "No file environment".to_string(),
            cwd: None,
            authority: WorkspaceAuthority::None,
        }
    }

    pub fn server_sandbox(root: impl AsRef<Path>) -> Self {
        Self {
            kind: WorkspaceBindingKind::ServerSandbox,
            display_name: "Server sandbox".to_string(),
            cwd: Some(root.as_ref().display().to_string()),
            authority: WorkspaceAuthority::ReadWrite,
        }
    }

    pub fn edge_workspace(
        display_name: impl Into<String>,
        cwd: impl Into<String>,
        authority: WorkspaceAuthority,
    ) -> Self {
        Self {
            kind: WorkspaceBindingKind::EdgeWorkspace,
            display_name: display_name.into(),
            cwd: Some(cwd.into()),
            authority,
        }
    }

    pub fn cloud_workspace(root: impl Into<String>, authority: WorkspaceAuthority) -> Self {
        Self {
            kind: WorkspaceBindingKind::CloudWorkspace,
            display_name: "Cloud workspace".to_string(),
            cwd: Some(root.into()),
            authority,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutorBindingKind {
    ServerLocal,
    EdgeAgent,
    OrchestratorManaged,
    ThinClient,
    Mcp,
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolTransportKind {
    ServerLocal,
    EdgeWs,
    EdgeLedger,
    McpHttp,
    GatewayRelay,
    SandboxResidentAgent,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutorBinding {
    pub kind: ExecutorBindingKind,
    pub executor_id: String,
    pub display_name: String,
    pub transport: ToolTransportKind,
    pub status: ExecutorStatus,
}

impl ExecutorBinding {
    pub fn server_local() -> Self {
        Self {
            kind: ExecutorBindingKind::ServerLocal,
            executor_id: "server-local".to_string(),
            display_name: "Server sandbox".to_string(),
            transport: ToolTransportKind::ServerLocal,
            status: ExecutorStatus::Online,
        }
    }

    pub fn server_control_plane() -> Self {
        Self {
            kind: ExecutorBindingKind::ServerLocal,
            executor_id: "server-control-plane".to_string(),
            display_name: "Server control plane".to_string(),
            transport: ToolTransportKind::ServerLocal,
            status: ExecutorStatus::Online,
        }
    }

    pub fn edge_agent(
        executor_id: impl Into<String>,
        display_name: impl Into<String>,
        transport: ToolTransportKind,
        status: ExecutorStatus,
    ) -> Self {
        Self {
            kind: ExecutorBindingKind::EdgeAgent,
            executor_id: executor_id.into(),
            display_name: display_name.into(),
            transport,
            status,
        }
    }

    pub fn request_scoped_mcp() -> Self {
        Self {
            kind: ExecutorBindingKind::Mcp,
            executor_id: "request-scoped-mcp".to_string(),
            display_name: "Request-scoped MCP".to_string(),
            transport: ToolTransportKind::McpHttp,
            status: ExecutorStatus::Online,
        }
    }

    pub fn orchestrator_managed(
        executor_id: impl Into<String>,
        display_name: impl Into<String>,
        status: ExecutorStatus,
    ) -> Self {
        Self {
            kind: ExecutorBindingKind::OrchestratorManaged,
            executor_id: executor_id.into(),
            display_name: display_name.into(),
            transport: ToolTransportKind::SandboxResidentAgent,
            status,
        }
    }
}

pub(crate) fn runtime_env_executor_kind_for_provider(
    kind: ExecutorBindingKind,
) -> astra_runtime_env::ExecutorBindingKind {
    match kind {
        ExecutorBindingKind::ServerLocal => astra_runtime_env::ExecutorBindingKind::ServerRuntime,
        ExecutorBindingKind::EdgeAgent => astra_runtime_env::ExecutorBindingKind::EdgeAgent,
        ExecutorBindingKind::OrchestratorManaged => {
            astra_runtime_env::ExecutorBindingKind::OrchestratorManaged
        }
        ExecutorBindingKind::ThinClient
        | ExecutorBindingKind::Mcp
        | ExecutorBindingKind::Unknown => astra_runtime_env::ExecutorBindingKind::Unknown,
    }
}

pub(crate) fn capacity_provider_type_for_workspace_executor(
    workspace_kind: WorkspaceBindingKind,
    executor_kind: ExecutorBindingKind,
) -> astra_runtime_env::CapacityProviderType {
    astra_runtime_env::runtime_execution_provider_type(
        workspace_kind,
        runtime_env_executor_kind_for_provider(executor_kind),
    )
}

pub(crate) fn runtime_execution_provider_id_for_executor(executor: &ExecutorBinding) -> String {
    match executor.kind {
        ExecutorBindingKind::ServerLocal => "server-sandbox".to_string(),
        ExecutorBindingKind::EdgeAgent | ExecutorBindingKind::OrchestratorManaged => {
            executor.executor_id.clone()
        }
        ExecutorBindingKind::ThinClient
        | ExecutorBindingKind::Mcp
        | ExecutorBindingKind::Unknown => "workspace-executor".to_string(),
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ToolPolicySnapshot {
    pub allowed_tools: Vec<String>,
    pub approval_policy: Option<String>,
    pub network_policy: Option<String>,
    pub secret_policy: Option<String>,
    pub sandbox_policy: Option<String>,
    pub max_execution_secs: Option<f64>,
    pub max_output_bytes: Option<usize>,
    pub max_background_session_secs: Option<f64>,
    /// Opaque conditional-read instruction for the exact native provider
    /// request. Unlike admission-only fields below, this is intentionally
    /// serialized across transport boundaries. Transports that do not
    /// understand the declared protocol must not fabricate an acknowledgement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic_read_condition: Option<astra_turn_types::SemanticReadCondition>,
    /// Exact provider policy already used by the permission gate. This is
    /// internal durable-decision input and is never forwarded to providers.
    #[serde(skip)]
    pub resolved_provider_policy:
        Option<astra_turn_core::provider_resolution::ResolvedInvocationPolicy>,
    /// Acknowledged grant that allowed this invocation to reach dispatch.
    #[serde(skip)]
    pub permission_grant: Option<ToolPermissionGrantSnapshot>,
    /// Frozen administrator admission facts used by both decision hashing and
    /// route execution, preventing policy TOCTOU within one invocation.
    #[serde(skip)]
    pub admission_snapshot: Option<ToolExecutionAdmissionSnapshot>,
    /// Trusted control epoch returned by durable action admission for this
    /// exact dispatch. It is installed only after the ledger grants Execute,
    /// never serialized to an external executor or accepted from tool args.
    #[serde(skip)]
    pub expected_control_epoch: Option<i64>,
    /// Concrete, trusted revision facts for one semantic pure-read decision.
    /// Eligibility in the provider descriptor is insufficient without this.
    #[serde(skip)]
    pub semantic_read_freshness: Option<astra_turn_types::SemanticReadFreshnessResolution>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolExecutionAdmissionSnapshot {
    /// Whether the exact selected offer was disabled at decision time.
    pub selected_offer_disabled: bool,
    /// Exact allowlist for the selected provider. `None` means that provider
    /// had no configured allowlist; unrelated providers are intentionally not
    /// part of this invocation's decision identity.
    pub selected_provider_allowed_tools: Option<BTreeSet<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolPermissionGrantSource {
    Policy,
    ImplicitPolicy,
    ParentApproval,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolPermissionGrantSnapshot {
    pub source: ToolPermissionGrantSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updates_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolExecutionRequest {
    pub user_id: String,
    pub run_id: String,
    pub turn_chain_id: String,
    pub session_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub args: Value,
    pub workspace: WorkspaceBinding,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_record: Option<astra_runtime_env::WorkspaceRecord>,
    pub executor: ExecutorBinding,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<astra_runtime_env::RuntimeBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_offer: Option<SelectedToolOfferSnapshot>,
    pub policy: ToolPolicySnapshot,
    /// Provider authorization injected only into bash on the provider-selected
    /// Edge executor. The credential is never serialized into durable snapshots.
    #[serde(skip)]
    pub runtime_process_authorization:
        Option<Arc<astra_services::runs::RuntimeProcessAuthorizationContext>>,
    /// Replay fence for the skipped process authorization.
    #[serde(default)]
    pub runtime_process_authorization_required: bool,
    /// Request-scoped dispatch authorization callback. Like file transfer
    /// credentials, this is never serialized into durable tool snapshots.
    #[serde(skip)]
    pub runtime_edge_dispatch_authorization:
        Option<Arc<astra_services::runs::RuntimeEdgeDispatchAuthorizationContext>>,
    /// Non-secret durable marker. If a snapshot is replayed without the
    /// skipped callback context, routing must fail closed rather than degrade
    /// to ordinary Edge dispatch.
    #[serde(default)]
    pub runtime_edge_dispatch_authorization_required: bool,
}

struct ToolExecutionIdentityParts<'a> {
    user_id: &'a str,
    session_id: &'a str,
    run_id: &'a str,
    turn_chain_id: &'a str,
    invocation_id: &'a str,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SelectedToolOfferSnapshot {
    pub offer_id: String,
    pub provider_id: String,
    #[serde(default = "default_selected_offer_route")]
    pub route: ToolExecutionRouteKind,
}

impl SelectedToolOfferSnapshot {
    pub fn new(tool_name: impl AsRef<str>, provider_id: impl Into<String>) -> Self {
        let provider_id = provider_id.into();
        Self {
            offer_id: astra_runtime_env::tool_offer_id(tool_name.as_ref(), &provider_id),
            provider_id,
            route: default_selected_offer_route(),
        }
    }

    pub fn new_with_route(
        tool_name: impl AsRef<str>,
        provider_id: impl Into<String>,
        route: ToolExecutionRouteKind,
    ) -> Self {
        let provider_id = provider_id.into();
        Self {
            offer_id: astra_runtime_env::tool_offer_id(tool_name.as_ref(), &provider_id),
            provider_id,
            route,
        }
    }
}

fn default_selected_offer_route() -> ToolExecutionRouteKind {
    ToolExecutionRouteKind::Unsupported
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExecutionBindingSnapshot {
    pub workspace: WorkspaceBinding,
    pub executor: ExecutorBinding,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<astra_runtime_env::RuntimeBinding>,
}

impl ExecutionBindingSnapshot {
    pub fn new(
        workspace: WorkspaceBinding,
        executor: ExecutorBinding,
        runtime: astra_runtime_env::RuntimeBinding,
    ) -> Self {
        Self {
            workspace,
            executor,
            runtime: Some(runtime),
        }
    }

    pub fn inferred(workspace: WorkspaceBinding, executor: ExecutorBinding) -> Self {
        Self {
            workspace,
            executor,
            runtime: None,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ExecutionBindingState {
    workspace: WorkspaceBinding,
    workspace_record: Option<astra_runtime_env::WorkspaceRecord>,
    executor: ExecutorBinding,
    runtime: Option<astra_runtime_env::RuntimeBinding>,
}

impl ExecutionBindingState {
    pub(crate) fn none() -> Self {
        Self {
            workspace: WorkspaceBinding::none(),
            workspace_record: None,
            executor: ExecutorBinding::server_control_plane(),
            runtime: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn server_sandbox(root: impl AsRef<Path>) -> Self {
        Self {
            workspace: WorkspaceBinding::server_sandbox(root),
            workspace_record: None,
            executor: ExecutorBinding::server_local(),
            runtime: None,
        }
    }

    pub(crate) fn workspace(&self) -> &WorkspaceBinding {
        &self.workspace
    }

    pub(crate) fn executor(&self) -> &ExecutorBinding {
        &self.executor
    }

    pub(crate) fn runtime(&self) -> Option<&astra_runtime_env::RuntimeBinding> {
        self.runtime.as_ref()
    }

    pub(crate) fn set_bindings(&mut self, workspace: WorkspaceBinding, executor: ExecutorBinding) {
        self.workspace = workspace;
        self.executor = executor;
        self.runtime = None;
    }

    pub(crate) fn set_snapshot(&mut self, snapshot: ExecutionBindingSnapshot) {
        self.workspace = snapshot.workspace;
        self.executor = snapshot.executor;
        self.runtime = snapshot.runtime;
    }

    pub(crate) fn set_workspace_record(
        &mut self,
        workspace_record: Option<astra_runtime_env::WorkspaceRecord>,
    ) {
        self.workspace_record = workspace_record;
    }

    pub(crate) fn set_edge_workspace_binding(
        &mut self,
        executor_id: impl Into<String>,
        display_name: impl Into<String>,
        cwd: impl Into<String>,
        authority: WorkspaceAuthority,
    ) {
        let executor_id = executor_id.into();
        let display_name = display_name.into();
        self.workspace = WorkspaceBinding::edge_workspace(display_name.clone(), cwd, authority);
        self.executor = ExecutorBinding::edge_agent(
            executor_id,
            display_name,
            ToolTransportKind::EdgeWs,
            ExecutorStatus::Online,
        );
        self.runtime = None;
    }

    pub(crate) fn tool_execution_request(
        &self,
        user_id: &str,
        session_id: &str,
        name: &str,
        args: &Value,
    ) -> ToolExecutionRequest {
        let run_id = string_arg(args, "_run_id").unwrap_or_default().to_string();
        let turn_chain_id = string_arg(args, "_turn_chain_id")
            .map(str::to_string)
            .unwrap_or_else(|| run_id.clone());
        self.build_tool_execution_request(
            ToolExecutionIdentityParts {
                user_id,
                session_id,
                run_id: &run_id,
                turn_chain_id: &turn_chain_id,
                invocation_id: tool_call_id(args).unwrap_or_default(),
            },
            name,
            args,
        )
    }

    /// Build a route request from a validated durable identity. Invocation
    /// identity stays out of provider-authored arguments.
    pub(crate) fn tool_execution_request_for_invocation(
        &self,
        identity: &astra_turn_types::ToolInvocationIdentity,
        name: &str,
        args: &Value,
    ) -> ToolExecutionRequest {
        self.build_tool_execution_request(
            ToolExecutionIdentityParts {
                user_id: &identity.user_id,
                session_id: &identity.session_id,
                run_id: &identity.run_id,
                turn_chain_id: &identity.turn_chain_id,
                invocation_id: &identity.invocation_id,
            },
            name,
            args,
        )
    }

    fn build_tool_execution_request(
        &self,
        identity: ToolExecutionIdentityParts<'_>,
        name: &str,
        args: &Value,
    ) -> ToolExecutionRequest {
        ToolExecutionRequest {
            user_id: identity.user_id.to_string(),
            run_id: identity.run_id.to_string(),
            turn_chain_id: identity.turn_chain_id.to_string(),
            session_id: identity.session_id.to_string(),
            tool_call_id: identity.invocation_id.to_string(),
            tool_name: name.to_string(),
            args: args.clone(),
            workspace: self.workspace.clone(),
            workspace_record: self.workspace_record.clone(),
            executor: self.executor.clone(),
            runtime: self.runtime.clone(),
            selected_offer: None,
            policy: ToolPolicySnapshot::default(),
            runtime_process_authorization: None,
            runtime_process_authorization_required: false,
            runtime_edge_dispatch_authorization: None,
            runtime_edge_dispatch_authorization_required: false,
        }
    }
}

impl ToolExecutionRequest {
    pub(crate) fn with_selected_offer(mut self, offer: SelectedToolOfferSnapshot) -> Self {
        self.selected_offer = Some(offer);
        self
    }

    pub(crate) fn with_transport_arguments(&self) -> Self {
        let mut request = self.clone();
        request.args = transport_tool_arguments(&request.args);
        request
    }
}

pub(crate) fn transport_tool_arguments(args: &Value) -> Value {
    let Some(map) = args.as_object() else {
        return args.clone();
    };
    let mut public = map.clone();
    public.remove("_tool_call_id");
    public.remove("_run_id");
    public.remove("_turn_chain_id");
    Value::Object(public)
}

fn tool_call_id(args: &Value) -> Option<&str> {
    args.get("_tool_call_id").and_then(Value::as_str)
}

fn string_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn durable_control_epoch_is_not_a_provider_wire_field() {
        let mut policy = ToolPolicySnapshot {
            expected_control_epoch: Some(7),
            ..ToolPolicySnapshot::default()
        };
        let mut wire = serde_json::to_value(&policy).expect("serialize policy wire");
        assert!(wire.get("expected_control_epoch").is_none());

        wire["expected_control_epoch"] = json!(99);
        policy = serde_json::from_value(wire).expect("decode provider-shaped policy");
        assert_eq!(
            policy.expected_control_epoch, None,
            "external/provider bytes cannot manufacture action-admission authority"
        );
    }

    fn workspace_record() -> astra_runtime_env::WorkspaceRecord {
        astra_runtime_env::WorkspaceRecord {
            workspace_id: "workspace-1".to_string(),
            owner_scope: astra_runtime_env::WorkspaceOwnerScope::Tenant,
            kind: astra_runtime_env::WorkspaceBindingKind::CloudWorkspace,
            authority: astra_runtime_env::WorkspaceAuthority::ReadWrite,
            root_or_volume_ref: "/cloud/workspaces/workspace-1".to_string(),
            source: astra_runtime_env::WorkspaceSource::Scratch,
            persistence: astra_runtime_env::WorkspacePersistence::Session,
            revision: "rev-1".to_string(),
            display_name: "Workspace 1".to_string(),
        }
    }

    #[test]
    fn execution_binding_state_builds_request_with_workspace_record() {
        let mut state = ExecutionBindingState::server_sandbox("/tmp/astra-workspace");
        state.set_workspace_record(Some(workspace_record()));

        let request = state.tool_execution_request(
            "user-1",
            "session-1",
            "bash",
            &json!({
                "_run_id": " run-1 ",
                "_turn_chain_id": " turn-7 ",
                "_tool_call_id": "call-1",
                "command": "pwd",
            }),
        );

        assert_eq!(request.user_id, "user-1");
        assert_eq!(request.session_id, "session-1");
        assert_eq!(request.run_id, "run-1");
        assert_eq!(request.turn_chain_id, "turn-7");
        assert_eq!(request.tool_call_id, "call-1");
        assert_eq!(request.tool_name, "bash");
        assert_eq!(request.args["command"], "pwd");
        assert_eq!(
            request
                .workspace_record
                .expect("workspace record")
                .workspace_id,
            "workspace-1"
        );
    }

    #[test]
    fn execution_binding_state_uses_run_as_explicit_legacy_turn_fallback() {
        let state = ExecutionBindingState::server_sandbox("/tmp/astra-workspace");

        let request = state.tool_execution_request(
            "user-1",
            "session-1",
            "bash",
            &json!({
                "_run_id": "run-1",
                "_tool_call_id": "call-1",
                "command": "pwd",
            }),
        );

        assert_eq!(request.run_id, "run-1");
        assert_eq!(request.turn_chain_id, "run-1");
    }

    #[test]
    fn typed_invocation_request_keeps_identity_out_of_provider_arguments() {
        let state = ExecutionBindingState::server_sandbox("/tmp/astra-workspace");
        let identity = astra_turn_types::ToolInvocationIdentity::new(
            "user-1",
            "session-1",
            "run-1",
            "turn-1",
            "call-1",
        )
        .unwrap();
        let args = json!({"_provider_cursor": "cursor-7", "query": "select 1"});

        let request = state.tool_execution_request_for_invocation(&identity, "query", &args);

        assert_eq!(request.user_id, "user-1");
        assert_eq!(request.session_id, "session-1");
        assert_eq!(request.run_id, "run-1");
        assert_eq!(request.turn_chain_id, "turn-1");
        assert_eq!(request.tool_call_id, "call-1");
        assert_eq!(request.args, args);
        assert!(request.args.get("_run_id").is_none());
        assert!(request.args.get("_turn_chain_id").is_none());
        assert!(request.args.get("_tool_call_id").is_none());
    }

    #[test]
    fn execution_binding_state_snapshot_preserves_runtime() {
        let mut state = ExecutionBindingState::server_sandbox("/tmp/astra-workspace");
        state.set_snapshot(ExecutionBindingSnapshot::new(
            WorkspaceBinding {
                kind: WorkspaceBindingKind::CloudWorkspace,
                display_name: "Cloud workspace".to_string(),
                cwd: Some("/workspace".to_string()),
                authority: WorkspaceAuthority::ReadWrite,
            },
            ExecutorBinding {
                kind: ExecutorBindingKind::EdgeAgent,
                executor_id: "edge-1".to_string(),
                display_name: "Edge agent".to_string(),
                transport: ToolTransportKind::EdgeWs,
                status: ExecutorStatus::Online,
            },
            astra_runtime_env::RuntimeBinding::gvisor("runtime-1"),
        ));

        assert_eq!(state.workspace().kind, WorkspaceBindingKind::CloudWorkspace);
        assert_eq!(state.executor().kind, ExecutorBindingKind::EdgeAgent);
        assert_eq!(
            state.runtime().expect("runtime").isolation_backend,
            astra_runtime_env::RuntimeIsolationBackend::GVisorRunsc
        );
    }

    #[test]
    fn execution_binding_state_explicit_bindings_clear_runtime() {
        let mut state = ExecutionBindingState::server_sandbox("/tmp/astra-workspace");
        state.set_snapshot(ExecutionBindingSnapshot::new(
            WorkspaceBinding::server_sandbox("/tmp/astra-workspace"),
            ExecutorBinding::server_local(),
            astra_runtime_env::RuntimeBinding::gvisor("runtime-1"),
        ));

        state.set_bindings(
            WorkspaceBinding::server_sandbox("/tmp/astra-workspace"),
            ExecutorBinding::server_local(),
        );

        assert!(state.runtime().is_none());
    }

    #[test]
    fn transport_tool_arguments_remove_only_astra_internal_metadata() {
        let args = json!({
            "query": "select 1",
            "_tool_call_id": "call-1",
            "_run_id": "run-1",
            "_turn_chain_id": "turn-1",
            "_domain_parameter": "must remain"
        });

        let projected = transport_tool_arguments(&args);

        assert_eq!(projected["query"], "select 1");
        assert_eq!(projected["_domain_parameter"], "must remain");
        assert!(projected.get("_tool_call_id").is_none(), "{projected:?}");
        assert!(projected.get("_run_id").is_none(), "{projected:?}");
        assert!(projected.get("_turn_chain_id").is_none(), "{projected:?}");
    }

    #[test]
    fn semantic_freshness_is_internal_decision_input_not_provider_transport() {
        let state = ExecutionBindingState::server_sandbox("/tmp/astra-workspace");
        let mut request = state.tool_execution_request(
            "user-1",
            "session-1",
            "provider_read",
            &json!({"query": "status"}),
        );
        request.policy.semantic_read_freshness = Some(
            astra_turn_types::SemanticReadFreshnessResolution::Available(
                astra_turn_types::SemanticReadFreshnessContext::new(
                    "tenant:user-1",
                    vec![
                        astra_turn_types::SemanticFreshnessFact::new(
                            astra_turn_types::SemanticFreshnessScope::Provider,
                            "provider-1",
                            "revision-7",
                        )
                        .unwrap(),
                    ],
                )
                .unwrap(),
            ),
        );

        let encoded = serde_json::to_value(&request).unwrap();
        assert!(
            encoded["policy"].get("semantic_read_freshness").is_none(),
            "internal cache authority must not be sent to providers: {encoded}"
        );
        assert!(!encoded.to_string().contains("revision-7"));
    }

    #[test]
    fn conditional_read_instruction_crosses_the_transport_boundary_unchanged() {
        let state = ExecutionBindingState::server_sandbox("/tmp/astra-workspace");
        let mut request = state.tool_execution_request(
            "user-1",
            "session-1",
            "provider_read",
            &json!({"query": "status", "_run_id": "run-secret"}),
        );
        let freshness = astra_turn_types::SemanticReadFreshnessContext::new(
            "tenant:user-1",
            vec![
                astra_turn_types::SemanticFreshnessFact::new(
                    astra_turn_types::SemanticFreshnessScope::Provider,
                    "provider-1",
                    "revision-7",
                )
                .unwrap(),
            ],
        )
        .unwrap();
        let condition =
            astra_turn_types::SemanticReadCondition::new("if-match", "etag-7", &freshness).unwrap();
        request.policy.semantic_read_condition = Some(condition.clone());

        let transport_request = request.with_transport_arguments();
        assert_eq!(
            transport_request.policy.semantic_read_condition,
            Some(condition.clone())
        );
        assert!(transport_request.args.get("_run_id").is_none());
        let restored: ToolExecutionRequest =
            serde_json::from_value(serde_json::to_value(transport_request).unwrap()).unwrap();
        assert_eq!(restored.policy.semantic_read_condition, Some(condition));
    }

    #[test]
    fn execution_binding_state_edge_binding_sets_transport() {
        let mut state = ExecutionBindingState::server_sandbox("/tmp/astra-workspace");

        state.set_edge_workspace_binding(
            "edge-1",
            "MacBook Pro",
            "/Users/test/project",
            WorkspaceAuthority::ReadWrite,
        );

        assert_eq!(state.workspace().kind, WorkspaceBindingKind::EdgeWorkspace);
        assert_eq!(state.executor().kind, ExecutorBindingKind::EdgeAgent);
        assert_eq!(state.executor().transport, ToolTransportKind::EdgeWs);
        assert_eq!(state.executor().status, ExecutorStatus::Online);
        assert!(state.runtime().is_none());
    }

    #[test]
    fn workspace_binding_kind_re_export_is_same_type() {
        // The re-exported WorkspaceBindingKind must be the same type as
        // astra_runtime_env::WorkspaceBindingKind (not a different enum).
        let a: WorkspaceBindingKind = astra_runtime_env::WorkspaceBindingKind::ServerSandbox;
        assert_eq!(a, WorkspaceBindingKind::ServerSandbox);

        // LocalFilesystem exists in the unified type (was not in the old
        // server-only enum).
        assert_eq!(
            WorkspaceBindingKind::LocalFilesystem,
            astra_runtime_env::WorkspaceBindingKind::LocalFilesystem
        );
    }
}
