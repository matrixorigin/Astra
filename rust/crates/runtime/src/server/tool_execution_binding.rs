use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

// Re-export canonical workspace/environment types from astra-runtime-env.
pub use astra_runtime_env::{ExecutorStatus, WorkspaceAuthority, WorkspaceBindingKind};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FallbackPolicy {
    /// Never route a tool call away from the selected executor.
    Disabled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceBinding {
    pub kind: WorkspaceBindingKind,
    pub display_name: String,
    pub cwd: Option<String>,
    pub authority: WorkspaceAuthority,
    pub fallback_policy: FallbackPolicy,
}

impl WorkspaceBinding {
    pub fn server_sandbox(root: impl AsRef<Path>) -> Self {
        Self {
            kind: WorkspaceBindingKind::ServerSandbox,
            display_name: "Server sandbox".to_string(),
            cwd: Some(root.as_ref().display().to_string()),
            authority: WorkspaceAuthority::ReadWrite,
            fallback_policy: FallbackPolicy::Disabled,
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
            fallback_policy: FallbackPolicy::Disabled,
        }
    }

    pub fn cloud_workspace(root: impl Into<String>, authority: WorkspaceAuthority) -> Self {
        Self {
            kind: WorkspaceBindingKind::CloudWorkspace,
            display_name: "Cloud workspace".to_string(),
            cwd: Some(root.into()),
            authority,
            fallback_policy: FallbackPolicy::Disabled,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolExecutionRequest {
    pub user_id: String,
    pub run_id: String,
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
    pub policy: ToolPolicySnapshot,
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
        ToolExecutionRequest {
            user_id: user_id.to_string(),
            run_id: string_arg(args, "_run_id").unwrap_or_default().to_string(),
            session_id: session_id.to_string(),
            tool_call_id: tool_call_id(args).unwrap_or_default().to_string(),
            tool_name: name.to_string(),
            args: args.clone(),
            workspace: self.workspace.clone(),
            workspace_record: self.workspace_record.clone(),
            executor: self.executor.clone(),
            runtime: self.runtime.clone(),
            policy: ToolPolicySnapshot::default(),
        }
    }
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
                "_tool_call_id": "call-1",
                "command": "pwd",
            }),
        );

        assert_eq!(request.user_id, "user-1");
        assert_eq!(request.session_id, "session-1");
        assert_eq!(request.run_id, "run-1");
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
    fn execution_binding_state_snapshot_preserves_runtime() {
        let mut state = ExecutionBindingState::server_sandbox("/tmp/astra-workspace");
        state.set_snapshot(ExecutionBindingSnapshot::new(
            WorkspaceBinding {
                kind: WorkspaceBindingKind::CloudWorkspace,
                display_name: "Cloud workspace".to_string(),
                cwd: Some("/workspace".to_string()),
                authority: WorkspaceAuthority::ReadWrite,
                fallback_policy: FallbackPolicy::Disabled,
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
