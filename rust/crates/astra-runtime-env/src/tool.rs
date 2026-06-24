use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::{EffectiveCapabilitySet, NetworkCapability};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ToolClass {
    ControlPlane,
    ServerService,
    ProjectRead,
    ProjectWrite,
    Process,
    Git,
    CodeIntel,
    Device,
    Mcp,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RequiredExecutor {
    None,
    ServiceExecutor,
    ControlPlane,
    RuntimeExecutor,
    McpExecutor,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RequiredWorkspace {
    None,
    Present,
    ReadOnly,
    ReadWrite,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RequiredNetwork {
    None,
    AllowList,
    Open,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolEffect {
    pub reads_workspace: bool,
    pub writes_workspace: bool,
    pub spawns_process: bool,
    pub uses_network: bool,
    pub uses_credentials: bool,
    pub mutates_external_state: bool,
}

impl ToolEffect {
    pub const fn none() -> Self {
        Self {
            reads_workspace: false,
            writes_workspace: false,
            spawns_process: false,
            uses_network: false,
            uses_credentials: false,
            mutates_external_state: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolRequirements {
    pub executor: RequiredExecutor,
    pub workspace: RequiredWorkspace,
    pub filesystem_read: bool,
    pub filesystem_write: bool,
    pub process_spawn: bool,
    pub shell: bool,
    pub git: bool,
    pub lsp: bool,
    pub background_session: bool,
    pub network: RequiredNetwork,
    pub credentials: bool,
}

impl ToolRequirements {
    pub const fn none() -> Self {
        Self {
            executor: RequiredExecutor::None,
            workspace: RequiredWorkspace::None,
            filesystem_read: false,
            filesystem_write: false,
            process_spawn: false,
            shell: false,
            git: false,
            lsp: false,
            background_session: false,
            network: RequiredNetwork::None,
            credentials: false,
        }
    }

    pub const fn control_plane() -> Self {
        Self {
            executor: RequiredExecutor::ServiceExecutor,
            ..Self::none()
        }
    }

    pub const fn server_network() -> Self {
        Self {
            executor: RequiredExecutor::ServiceExecutor,
            network: RequiredNetwork::AllowList,
            ..Self::none()
        }
    }

    pub const fn server_network_credentials() -> Self {
        Self {
            executor: RequiredExecutor::ServiceExecutor,
            network: RequiredNetwork::AllowList,
            credentials: true,
            ..Self::none()
        }
    }

    pub const fn mcp() -> Self {
        Self {
            executor: RequiredExecutor::McpExecutor,
            ..Self::none()
        }
    }

    pub const fn project_read() -> Self {
        Self {
            executor: RequiredExecutor::RuntimeExecutor,
            workspace: RequiredWorkspace::ReadOnly,
            filesystem_read: true,
            ..Self::none()
        }
    }

    pub const fn project_write() -> Self {
        Self {
            executor: RequiredExecutor::RuntimeExecutor,
            workspace: RequiredWorkspace::ReadWrite,
            filesystem_read: true,
            filesystem_write: true,
            ..Self::none()
        }
    }

    pub const fn shell() -> Self {
        Self {
            executor: RequiredExecutor::RuntimeExecutor,
            workspace: RequiredWorkspace::ReadWrite,
            process_spawn: true,
            shell: true,
            filesystem_read: true,
            filesystem_write: true,
            ..Self::none()
        }
    }

    pub const fn background_shell() -> Self {
        Self {
            background_session: true,
            ..Self::shell()
        }
    }

    pub const fn git_read() -> Self {
        Self {
            git: true,
            ..Self::project_read()
        }
    }

    pub const fn git_write() -> Self {
        Self {
            git: true,
            ..Self::project_write()
        }
    }

    pub const fn git_clone() -> Self {
        Self {
            network: RequiredNetwork::AllowList,
            ..Self::git_write()
        }
    }

    pub const fn lsp() -> Self {
        Self {
            lsp: true,
            ..Self::project_read()
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolSpec {
    pub name: String,
    pub class: ToolClass,
    pub effect: ToolEffect,
    pub required: ToolRequirements,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolRegistry {
    tools: HashMap<String, ToolSpec>,
}

impl ToolRegistry {
    pub fn new(tools: Vec<ToolSpec>) -> Self {
        let tools = tools.into_iter().map(|t| (t.name.clone(), t)).collect();
        Self { tools }
    }

    pub fn builtins() -> Self {
        Self::new(vec![
            control_plane("ask_user"),
            control_plane("agent"),
            control_plane("agent_fanout"),
            control_plane("enter_plan_mode"),
            control_plane("exit_plan_mode"),
            control_plane("get_agent_info"),
            control_plane("introspect"),
            control_plane("notify"),
            control_plane("prioritize_tool"),
            control_plane("deprioritize_tool"),
            control_plane("compress_context"),
            control_plane("rollback_session_state"),
            control_plane("session"),
            control_plane("skill"),
            control_plane("task"),
            control_plane("task_output"),
            control_plane("task_stop"),
            control_plane("task_list"),
            server_service("memory"),
            server_service("mo_query"),
            server_service("rollback_database_snapshots"),
            server_service("tool_search"),
            server_network("web_search"),
            server_network("web_fetch"),
            server_network_credentials("github"),
            project_read("publish_artifact"),
            project_read("read_file"),
            project_read("list_dir"),
            project_read("grep"),
            project_read("glob"),
            project_read("symbols"),
            project_write("write_file"),
            project_write("str_replace"),
            project_write("delete_file"),
            project_write("multi_edit"),
            project_write("rollback_file_edits"),
            shell("bash"),
            shell("powershell"),
            shell("run_script"),
            background_shell("background_shell"),
            git_read("git"),
            git_clone("git_clone"),
            lsp("lsp"),
            lsp("find_definition"),
            lsp("find_references"),
        ])
    }

    pub fn get(&self, name: &str) -> Option<&ToolSpec> {
        self.tools.get(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = &ToolSpec> {
        self.tools.values()
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::builtins()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolDenial {
    pub tool_name: String,
    pub reason: ToolUnavailableReason,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AvailableToolSurface {
    pub tool_names: Vec<String>,
    pub denials: Vec<ToolDenial>,
}

impl AvailableToolSurface {
    pub fn contains(&self, tool_name: &str) -> bool {
        self.tool_names.iter().any(|name| name == tool_name)
    }

    pub fn denial_for(&self, tool_name: &str) -> Option<&ToolUnavailableReason> {
        self.denials
            .iter()
            .find(|denial| denial.tool_name == tool_name)
            .map(|denial| &denial.reason)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Error)]
pub enum ToolUnavailableReason {
    #[error("tool is not registered")]
    UnknownTool,
    #[error("executor is unavailable: {0}")]
    ExecutorUnavailable(String),
    #[error("workspace is unavailable: {0}")]
    WorkspaceUnavailable(String),
    #[error("runtime capability is missing: {0}")]
    RuntimeCapabilityMissing(String),
    #[error("policy denied: {0}")]
    PolicyDenied(String),
}

#[derive(Debug, Clone, Default)]
pub struct CapabilityResolver;

impl CapabilityResolver {
    pub fn available_tool_surface(
        &self,
        registry: &ToolRegistry,
        capabilities: &EffectiveCapabilitySet,
    ) -> AvailableToolSurface {
        let mut tool_names = Vec::new();
        let mut denials = Vec::new();
        for spec in registry.iter() {
            match self.check(spec, capabilities) {
                Ok(()) => tool_names.push(spec.name.clone()),
                Err(reason) => denials.push(ToolDenial {
                    tool_name: spec.name.clone(),
                    reason,
                }),
            }
        }
        // Sort for deterministic serialization (HashMap iteration is non-deterministic).
        tool_names.sort();
        denials.sort_by(|a, b| a.tool_name.cmp(&b.tool_name));
        AvailableToolSurface {
            tool_names,
            denials,
        }
    }

    pub fn filter_tool_schemas(
        &self,
        registry: &ToolRegistry,
        schemas: Vec<Value>,
        capabilities: &EffectiveCapabilitySet,
    ) -> Vec<Value> {
        let mut seen = HashSet::new();
        schemas
            .into_iter()
            .filter(|schema| {
                let Some(tool_name) = tool_schema_name(schema) else {
                    return false;
                };
                if !seen.insert(tool_name.to_string()) {
                    return false;
                }
                self.check_tool_call(registry, tool_name, &Value::Null, capabilities)
                    .is_ok()
            })
            .collect()
    }

    pub fn check_tool(
        &self,
        registry: &ToolRegistry,
        tool_name: &str,
        capabilities: &EffectiveCapabilitySet,
    ) -> Result<(), ToolUnavailableReason> {
        let dynamic_spec = dynamic_tool_spec(tool_name);
        let spec = registry
            .get(tool_name)
            .or(dynamic_spec.as_ref())
            .ok_or(ToolUnavailableReason::UnknownTool)?;
        self.check(spec, capabilities)
    }

    pub fn check_tool_call(
        &self,
        registry: &ToolRegistry,
        tool_name: &str,
        args: &Value,
        capabilities: &EffectiveCapabilitySet,
    ) -> Result<(), ToolUnavailableReason> {
        if tool_name == "git" && git_action_requires_write(args) {
            let spec = git_write(tool_name);
            return self.check(&spec, capabilities);
        }
        if tool_name == "lsp" && lsp_action_requires_write(args) {
            let spec = project_write(tool_name);
            return self.check(&spec, capabilities);
        }
        let dynamic_spec = dynamic_tool_spec(tool_name);
        let spec = registry
            .get(tool_name)
            .or(dynamic_spec.as_ref())
            .ok_or(ToolUnavailableReason::UnknownTool)?;
        self.check(spec, capabilities)
    }

    pub fn check(
        &self,
        spec: &ToolSpec,
        capabilities: &EffectiveCapabilitySet,
    ) -> Result<(), ToolUnavailableReason> {
        match spec.required.executor {
            RequiredExecutor::None => {}
            RequiredExecutor::ServiceExecutor
                if !(capabilities.executor.control_plane
                    || capabilities.executor.runtime_executor) =>
            {
                return Err(ToolUnavailableReason::ExecutorUnavailable(
                    "service_executor_required".to_string(),
                ));
            }
            RequiredExecutor::ControlPlane if !capabilities.executor.control_plane => {
                return Err(ToolUnavailableReason::ExecutorUnavailable(
                    "control_plane_executor_required".to_string(),
                ));
            }
            RequiredExecutor::RuntimeExecutor if !capabilities.executor.runtime_executor => {
                return Err(ToolUnavailableReason::ExecutorUnavailable(
                    "runtime_executor_required".to_string(),
                ));
            }
            RequiredExecutor::McpExecutor if !capabilities.executor.mcp_executor => {
                return Err(ToolUnavailableReason::ExecutorUnavailable(
                    "mcp_executor_required".to_string(),
                ));
            }
            RequiredExecutor::ControlPlane
            | RequiredExecutor::ServiceExecutor
            | RequiredExecutor::RuntimeExecutor
            | RequiredExecutor::McpExecutor => {}
        }

        match spec.required.workspace {
            RequiredWorkspace::None => {}
            RequiredWorkspace::Present if !capabilities.workspace.present => {
                return Err(ToolUnavailableReason::WorkspaceUnavailable(
                    "workspace_required".to_string(),
                ));
            }
            RequiredWorkspace::ReadOnly if !capabilities.workspace.readable => {
                if !capabilities.policy.filesystem_read {
                    return Err(ToolUnavailableReason::PolicyDenied(
                        "filesystem_read".to_string(),
                    ));
                }
                return Err(ToolUnavailableReason::WorkspaceUnavailable(
                    "readable_workspace_required".to_string(),
                ));
            }
            RequiredWorkspace::ReadWrite if !capabilities.workspace.writable => {
                if !capabilities.policy.filesystem_write {
                    return Err(ToolUnavailableReason::PolicyDenied(
                        "filesystem_write".to_string(),
                    ));
                }
                return Err(ToolUnavailableReason::WorkspaceUnavailable(
                    "writable_workspace_required".to_string(),
                ));
            }
            RequiredWorkspace::Present
            | RequiredWorkspace::ReadOnly
            | RequiredWorkspace::ReadWrite => {}
        }

        // NOTE: filesystem_read/write policy checks are defense-in-depth.
        // The workspace capability (workspace.readable / workspace.writable) computed in
        // EffectiveCapabilitySet::from_bindings already encodes the policy_filesystem checks.
        // Re-checking here ensures each requirement check is independently correct even if
        // the workspace capability derivation changes.
        if spec.required.filesystem_read {
            // Policy-level check: workspace must be readable per policy
            if !capabilities.policy.filesystem_read {
                return Err(ToolUnavailableReason::PolicyDenied(
                    "filesystem_read".to_string(),
                ));
            }
            // Runtime-level check: must have process capability
            if !capabilities.runtime.runtime_has_process {
                return Err(ToolUnavailableReason::RuntimeCapabilityMissing(
                    "process".to_string(),
                ));
            }
        }
        if spec.required.filesystem_write {
            if !capabilities.policy.filesystem_write {
                return Err(ToolUnavailableReason::PolicyDenied(
                    "filesystem_write".to_string(),
                ));
            }
            if !capabilities.runtime.runtime_has_process {
                return Err(ToolUnavailableReason::RuntimeCapabilityMissing(
                    "process".to_string(),
                ));
            }
        }
        if spec.required.process_spawn && !capabilities.runtime.runtime_has_process {
            return Err(ToolUnavailableReason::RuntimeCapabilityMissing(
                "process".to_string(),
            ));
        }
        if spec.required.shell && !capabilities.runtime.runtime_has_shell {
            return Err(ToolUnavailableReason::RuntimeCapabilityMissing(
                "shell".to_string(),
            ));
        }
        if spec.required.git && !capabilities.runtime.runtime_has_git {
            return Err(ToolUnavailableReason::RuntimeCapabilityMissing(
                "git".to_string(),
            ));
        }
        if spec.required.lsp && !capabilities.runtime.runtime_has_lsp {
            return Err(ToolUnavailableReason::RuntimeCapabilityMissing(
                "lsp".to_string(),
            ));
        }
        if spec.required.background_session && !capabilities.runtime.runtime_has_process {
            return Err(ToolUnavailableReason::RuntimeCapabilityMissing(
                "process".to_string(),
            ));
        }
        match spec.required.network {
            RequiredNetwork::None => {}
            RequiredNetwork::AllowList
                if capabilities.policy.network < NetworkCapability::AllowList =>
            {
                return Err(ToolUnavailableReason::PolicyDenied(
                    "network_allow_list".to_string(),
                ));
            }
            RequiredNetwork::Open if capabilities.policy.network < NetworkCapability::Open => {
                return Err(ToolUnavailableReason::PolicyDenied(
                    "network_open".to_string(),
                ));
            }
            RequiredNetwork::AllowList | RequiredNetwork::Open => {}
        }
        if spec.required.credentials && !capabilities.policy.credentials {
            return Err(ToolUnavailableReason::PolicyDenied(
                "credentials".to_string(),
            ));
        }

        Ok(())
    }
}

pub use astra_core::tool_schema::tool_schema_name;

fn control_plane(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        class: ToolClass::ControlPlane,
        effect: ToolEffect::none(),
        required: ToolRequirements::control_plane(),
    }
}

fn server_service(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        class: ToolClass::ServerService,
        effect: ToolEffect::none(),
        required: ToolRequirements::control_plane(),
    }
}

fn server_network(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        class: ToolClass::ServerService,
        effect: ToolEffect {
            uses_network: true,
            ..ToolEffect::none()
        },
        required: ToolRequirements::server_network(),
    }
}

fn server_network_credentials(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        class: ToolClass::ServerService,
        effect: ToolEffect {
            uses_network: true,
            uses_credentials: true,
            ..ToolEffect::none()
        },
        required: ToolRequirements::server_network_credentials(),
    }
}

fn request_scoped_mcp(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        class: ToolClass::Mcp,
        effect: ToolEffect::none(),
        required: ToolRequirements::mcp(),
    }
}

fn dynamic_tool_spec(name: &str) -> Option<ToolSpec> {
    if name.starts_with("mcp__") {
        return Some(request_scoped_mcp(name));
    }
    None
}

fn git_action_requires_write(args: &Value) -> bool {
    matches!(
        args.get("action").and_then(Value::as_str),
        Some("commit" | "stash" | "revert_commit" | "push" | "clone")
    )
}

fn lsp_action_requires_write(args: &Value) -> bool {
    matches!(args.get("dry_run").and_then(Value::as_bool), Some(false))
}

fn project_read(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        class: ToolClass::ProjectRead,
        effect: ToolEffect {
            reads_workspace: true,
            ..ToolEffect::none()
        },
        required: ToolRequirements::project_read(),
    }
}

fn project_write(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        class: ToolClass::ProjectWrite,
        effect: ToolEffect {
            reads_workspace: true,
            writes_workspace: true,
            ..ToolEffect::none()
        },
        required: ToolRequirements::project_write(),
    }
}

fn shell(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        class: ToolClass::Process,
        effect: ToolEffect {
            reads_workspace: true,
            spawns_process: true,
            ..ToolEffect::none()
        },
        required: ToolRequirements::shell(),
    }
}

fn background_shell(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        class: ToolClass::Process,
        effect: ToolEffect {
            reads_workspace: true,
            spawns_process: true,
            ..ToolEffect::none()
        },
        required: ToolRequirements::background_shell(),
    }
}

fn git_read(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        class: ToolClass::Git,
        effect: ToolEffect {
            reads_workspace: true,
            ..ToolEffect::none()
        },
        required: ToolRequirements::git_read(),
    }
}

fn git_write(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        class: ToolClass::Git,
        effect: ToolEffect {
            reads_workspace: true,
            writes_workspace: true,
            mutates_external_state: name == "git_push",
            ..ToolEffect::none()
        },
        required: ToolRequirements::git_write(),
    }
}

fn git_clone(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        class: ToolClass::Git,
        effect: ToolEffect {
            reads_workspace: true,
            writes_workspace: true,
            uses_network: true,
            mutates_external_state: false,
            ..ToolEffect::none()
        },
        required: ToolRequirements::git_clone(),
    }
}

fn lsp(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        class: ToolClass::Device,
        effect: ToolEffect {
            reads_workspace: true,
            ..ToolEffect::none()
        },
        required: ToolRequirements::lsp(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ExecutorBinding, PolicyIntent, RunBinding, RuntimeBinding, WorkspaceAuthority,
        WorkspaceBinding,
    };

    fn registry() -> ToolRegistry {
        ToolRegistry::builtins()
    }

    #[test]
    fn cloud_control_plane_without_runtime_hides_project_tools() {
        let registry = registry();
        let binding = RunBinding::cloud_control_plane(&registry);

        assert!(binding.tool_surface.contains("ask_user"));
        assert!(binding.tool_surface.contains("skill"));
        assert!(binding.tool_surface.contains("tool_search"));
        assert!(binding.tool_surface.contains("web_search"));
        for tool in [
            "bash",
            "run_script",
            "background_shell",
            "read_file",
            "write_file",
            "git",
            "git_clone",
            "find_definition",
        ] {
            assert!(
                !binding.tool_surface.contains(tool),
                "{tool} must not be visible without a workspace runtime"
            );
        }

        assert_eq!(
            CapabilityResolver.check_tool(&registry, "bash", &binding.capabilities),
            Err(ToolUnavailableReason::ExecutorUnavailable(
                "runtime_executor_required".to_string()
            ))
        );
    }

    #[test]
    fn edge_cloud_workspace_uses_edge_capabilities_for_project_tools() {
        let registry = registry();
        let binding = RunBinding::edge_developer("/repo", &registry);

        for tool in [
            "bash",
            "read_file",
            "write_file",
            "git",
            "git_clone",
            "find_definition",
            "background_shell",
        ] {
            assert!(
                binding.tool_surface.contains(tool),
                "{tool} should be visible"
            );
        }
    }

    #[test]
    fn policy_allowed_tools_restricts_visible_tool_surface() {
        let registry = registry();
        let binding = RunBinding::resolve(
            WorkspaceBinding::edge_workspace("/repo", WorkspaceAuthority::ReadWrite),
            ExecutorBinding::edge_agent("edge-agent"),
            RuntimeBinding::host_process("edge-host"),
            PolicyIntent::local_developer().with_allowed_tools(["read_file", "git"]),
            &registry,
        );

        assert!(binding.tool_surface.contains("read_file"));
        assert!(binding.tool_surface.contains("git"));
        for tool in ["bash", "write_file", "ask_user"] {
            assert!(
                !binding.tool_surface.contains(tool),
                "{tool} must be hidden by allowed_tools"
            );
            assert_eq!(
                binding.tool_surface.denial_for(tool),
                Some(&ToolUnavailableReason::PolicyDenied(
                    PolicyIntent::disallowed_tool_reason(tool)
                ))
            );
        }
    }

    #[test]
    fn policy_allowed_tools_wildcard_keeps_capability_surface() {
        let registry = registry();
        let binding = RunBinding::resolve(
            WorkspaceBinding::edge_workspace("/repo", WorkspaceAuthority::ReadWrite),
            ExecutorBinding::edge_agent("edge-agent"),
            RuntimeBinding::host_process("edge-host"),
            PolicyIntent::local_developer().with_allowed_tools(["*"]),
            &registry,
        );

        assert!(binding.tool_surface.contains("bash"));
        assert!(binding.tool_surface.contains("write_file"));
        assert!(binding.tool_surface.contains("git"));
    }

    #[test]
    fn git_and_github_helper_style_names_are_unknown_tools() {
        let registry = registry();
        let binding = RunBinding::edge_developer("/repo", &registry);

        let git_actions = [
            "status",
            "diff",
            "log",
            "show",
            "blame",
            "file_history",
            "log_search",
            "contributors",
            "commit",
            "stash",
            "revert_commit",
            "push",
        ];
        let github_actions = [
            "list_prs",
            "get_pr",
            "ci_status",
            "list_issues",
            "get_issue",
            "repo_stats",
            "create_issue",
        ];
        let tools = std::iter::once("delegate".to_string())
            .chain(
                git_actions
                    .into_iter()
                    .map(|action| format!("git_{action}")),
            )
            .chain(
                github_actions
                    .into_iter()
                    .map(|action| format!("github_{action}")),
            );

        for tool in tools {
            assert_eq!(
                CapabilityResolver.check_tool_call(
                    &registry,
                    tool.as_str(),
                    &serde_json::json!({}),
                    &binding.capabilities,
                ),
                Err(ToolUnavailableReason::UnknownTool),
                "{tool} must not resolve through static or dynamic registry entries"
            );
        }
    }

    #[test]
    fn read_only_workspace_allows_reads_and_blocks_mutations() {
        let registry = registry();
        let binding = RunBinding::resolve(
            WorkspaceBinding::cloud_workspace("/repo", WorkspaceAuthority::ReadOnly),
            ExecutorBinding::orchestrator_managed("orchestrator:review"),
            RuntimeBinding::host_process("review-runtime"),
            PolicyIntent::read_only_review(),
            &registry,
        );

        assert!(binding.tool_surface.contains("read_file"));
        assert!(binding.tool_surface.contains("git"));
        assert!(!binding.tool_surface.contains("write_file"));
        assert_eq!(
            CapabilityResolver.check_tool_call(
                &registry,
                "git",
                &serde_json::json!({"action": "commit"}),
                &binding.capabilities,
            ),
            Err(ToolUnavailableReason::PolicyDenied(
                "filesystem_write".to_string()
            ))
        );

        assert_eq!(
            binding.tool_surface.denial_for("write_file"),
            Some(&ToolUnavailableReason::PolicyDenied(
                "filesystem_write".to_string()
            ))
        );
    }

    #[test]
    fn filesystem_policy_denials_are_reported_as_policy_denied() {
        let registry = registry();
        let mut no_access = PolicyIntent::local_developer();
        no_access.filesystem = crate::FilesystemPolicy::NoAccess;
        let binding = RunBinding::resolve(
            WorkspaceBinding::cloud_workspace("/repo", WorkspaceAuthority::ReadWrite),
            ExecutorBinding::orchestrator_managed("orchestrator:repo"),
            RuntimeBinding::host_process("repo-runtime"),
            no_access,
            &registry,
        );

        assert_eq!(
            binding.tool_surface.denial_for("read_file"),
            Some(&ToolUnavailableReason::PolicyDenied(
                "filesystem_read".to_string()
            ))
        );
        assert_eq!(
            CapabilityResolver.check_tool_call(
                &registry,
                "read_file",
                &serde_json::json!({"path": "README.md"}),
                &binding.capabilities,
            ),
            Err(ToolUnavailableReason::PolicyDenied(
                "filesystem_read".to_string()
            ))
        );

        let mut read_only_policy = PolicyIntent::local_developer();
        read_only_policy.filesystem = crate::FilesystemPolicy::ReadOnlyWorkspace;
        let binding = RunBinding::resolve(
            WorkspaceBinding::cloud_workspace("/repo", WorkspaceAuthority::ReadWrite),
            ExecutorBinding::orchestrator_managed("orchestrator:repo"),
            RuntimeBinding::host_process("repo-runtime"),
            read_only_policy,
            &registry,
        );

        assert_eq!(
            binding.tool_surface.denial_for("write_file"),
            Some(&ToolUnavailableReason::PolicyDenied(
                "filesystem_write".to_string()
            ))
        );
    }

    #[test]
    fn workspace_authority_denials_are_not_reported_as_policy_denied() {
        let registry = registry();
        let mut write_policy = PolicyIntent::local_developer();
        write_policy.filesystem = crate::FilesystemPolicy::ReadWriteWorkspace;
        let binding = RunBinding::resolve(
            WorkspaceBinding::cloud_workspace("/repo", WorkspaceAuthority::ReadOnly),
            ExecutorBinding::orchestrator_managed("orchestrator:repo"),
            RuntimeBinding::host_process("repo-runtime"),
            write_policy,
            &registry,
        );

        assert_eq!(
            binding.tool_surface.denial_for("write_file"),
            Some(&ToolUnavailableReason::WorkspaceUnavailable(
                "writable_workspace_required".to_string()
            ))
        );
    }

    #[test]
    fn runtime_missing_blocks_reads_even_when_workspace_metadata_is_present() {
        let registry = registry();
        let binding = RunBinding::resolve(
            WorkspaceBinding::cloud_workspace("/snapshot", WorkspaceAuthority::ReadOnly),
            ExecutorBinding::orchestrator_managed("orchestrator:snapshot"),
            RuntimeBinding::none(),
            PolicyIntent::read_only_review(),
            &registry,
        );

        assert!(!binding.tool_surface.contains("read_file"));
        assert_eq!(
            binding.tool_surface.denial_for("read_file"),
            Some(&ToolUnavailableReason::RuntimeCapabilityMissing(
                "process".to_string()
            ))
        );
    }

    #[test]
    fn unknown_workspace_kind_does_not_grant_project_authority() {
        let registry = registry();
        let binding = RunBinding::resolve(
            WorkspaceBinding {
                kind: crate::WorkspaceBindingKind::Unknown,
                display_name: "Unknown workspace".to_string(),
                cwd: Some("/workspace".to_string()),
                authority: WorkspaceAuthority::ReadWrite,
                persistent: true,
            },
            ExecutorBinding::orchestrator_managed("orchestrator:workspace"),
            RuntimeBinding::oci_container("runtime"),
            PolicyIntent::strict_orchestrator(),
            &registry,
        );

        assert!(!binding.tool_surface.contains("read_file"));
        assert!(!binding.tool_surface.contains("bash"));
        assert_eq!(
            binding.tool_surface.denial_for("read_file"),
            Some(&ToolUnavailableReason::WorkspaceUnavailable(
                "readable_workspace_required".to_string()
            ))
        );
    }

    #[test]
    fn unknown_runtime_topology_does_not_grant_project_capabilities() {
        let registry = registry();
        let binding = RunBinding::resolve(
            WorkspaceBinding::cloud_workspace("/workspace", WorkspaceAuthority::ReadWrite),
            ExecutorBinding::orchestrator_managed("orchestrator:workspace"),
            RuntimeBinding {
                session_manager: crate::RuntimeSessionManager::Unknown,
                isolation_backend: crate::RuntimeIsolationBackend::OciRuntime,
                launch_driver: crate::RuntimeLaunchDriver::Containerd,
                runtime_id: "runtime".to_string(),
                display_name: "Unknown runtime".to_string(),
                status: crate::RuntimeStatus::Ready,
                ephemeral: true,
                supports_long_sessions: true,
                interaction_channels: Vec::new(),
            },
            PolicyIntent::strict_orchestrator(),
            &registry,
        );

        assert!(!binding.capabilities.runtime.runtime_has_process);
        assert!(!binding.tool_surface.contains("read_file"));
        assert_eq!(
            binding.tool_surface.denial_for("read_file"),
            Some(&ToolUnavailableReason::RuntimeCapabilityMissing(
                "process".to_string()
            ))
        );
    }

    #[test]
    fn unknown_executor_transport_does_not_grant_runtime_executor_authority() {
        let registry = registry();
        let binding = RunBinding::resolve(
            WorkspaceBinding::cloud_workspace("/workspace", WorkspaceAuthority::ReadWrite),
            ExecutorBinding {
                kind: crate::ExecutorBindingKind::EdgeAgent,
                executor_id: "edge".to_string(),
                display_name: "Edge".to_string(),
                transport: crate::ToolTransportKind::Unknown,
                status: crate::ExecutorStatus::Online,
            },
            RuntimeBinding::oci_container("runtime"),
            PolicyIntent::strict_orchestrator(),
            &registry,
        );

        assert!(!binding.capabilities.executor.runtime_executor);
        assert!(!binding.tool_surface.contains("read_file"));
        assert_eq!(
            binding.tool_surface.denial_for("read_file"),
            Some(&ToolUnavailableReason::ExecutorUnavailable(
                "runtime_executor_required".to_string()
            ))
        );
    }

    #[test]
    fn read_only_snapshot_helper_exposes_reads_through_orchestrator_runtime() {
        let registry = registry();
        let binding = RunBinding::read_only_snapshot("/snapshot", &registry);

        assert_eq!(
            binding.runtime.session_manager,
            crate::RuntimeSessionManager::ProviderManaged
        );
        assert_eq!(
            binding.runtime.isolation_backend,
            crate::RuntimeIsolationBackend::ProviderManaged
        );
        assert_eq!(
            binding.runtime.launch_driver,
            crate::RuntimeLaunchDriver::Kubernetes
        );
        for tool in ["read_file", "list_dir", "grep", "glob", "git"] {
            assert!(
                binding.tool_surface.contains(tool),
                "{tool} should be visible for read-only snapshot with runtime"
            );
        }
        for tool in ["write_file", "str_replace", "bash", "run_script"] {
            assert!(
                !binding.tool_surface.contains(tool),
                "{tool} must stay hidden for read-only snapshot"
            );
        }
    }

    #[test]
    fn openshell_runtime_does_not_imply_gpu_tools_without_runtime_evidence() {
        let registry = registry();
        let binding = RunBinding::resolve(
            WorkspaceBinding::cloud_workspace("/repo", WorkspaceAuthority::ReadWrite),
            ExecutorBinding::orchestrator_managed("orchestrator:gpu"),
            RuntimeBinding::nvidia_openshell("openshell"),
            PolicyIntent::strict_orchestrator(),
            &registry,
        );

        assert!(binding.tool_surface.contains("bash"));
    }

    #[test]
    fn network_denial_hides_networked_git_clone() {
        let registry = registry();
        let mut policy = PolicyIntent::local_developer();
        policy.network = crate::NetworkPolicy::Disabled;
        let binding = RunBinding::resolve(
            WorkspaceBinding::local_filesystem("/repo", WorkspaceAuthority::ReadWrite),
            ExecutorBinding::local_cli(),
            RuntimeBinding::host_process("local-host"),
            policy,
            &registry,
        );

        assert!(binding.tool_surface.contains("git"));
        assert!(!binding.tool_surface.contains("git_clone"));
        assert_eq!(
            binding.tool_surface.denial_for("git_clone"),
            Some(&ToolUnavailableReason::PolicyDenied(
                "network_allow_list".to_string()
            ))
        );
    }

    #[test]
    fn argument_sensitive_git_mutation_requires_writable_workspace() {
        let registry = registry();
        let binding = RunBinding::resolve(
            WorkspaceBinding::cloud_workspace("/repo", WorkspaceAuthority::ReadOnly),
            ExecutorBinding::orchestrator_managed("orchestrator:review"),
            RuntimeBinding::host_process("review-runtime"),
            PolicyIntent::read_only_review(),
            &registry,
        );

        assert_eq!(
            CapabilityResolver.check_tool_call(
                &registry,
                "git",
                &serde_json::json!({"action": "commit"}),
                &binding.capabilities,
            ),
            Err(ToolUnavailableReason::PolicyDenied(
                "filesystem_write".to_string()
            ))
        );
        assert_eq!(
            CapabilityResolver.check_tool_call(
                &registry,
                "git",
                &serde_json::json!({"action": "status"}),
                &binding.capabilities,
            ),
            Ok(())
        );
    }

    #[test]
    fn request_scoped_mcp_tools_use_mcp_executor_capability() {
        let registry = registry();
        let binding = RunBinding::resolve(
            WorkspaceBinding::none(),
            ExecutorBinding {
                kind: crate::ExecutorBindingKind::RequestScopedMcp,
                executor_id: "mcp".to_string(),
                display_name: "MCP server".to_string(),
                transport: crate::ToolTransportKind::McpHttp,
                status: crate::ExecutorStatus::Online,
            },
            RuntimeBinding::none(),
            PolicyIntent::cloud_control_plane(),
            &registry,
        );

        assert_eq!(
            CapabilityResolver.check_tool_call(
                &registry,
                "mcp__github__create_issue",
                &serde_json::json!({}),
                &binding.capabilities,
            ),
            Ok(())
        );
    }

    #[test]
    fn schema_filter_keeps_local_service_and_project_tools_for_local_cli() {
        let registry = registry();
        let binding = RunBinding::local_developer("/repo", &registry);
        let schemas = vec![
            serde_json::json!({"type": "function", "function": {"name": "tool_search"}}),
            serde_json::json!({"type": "function", "function": {"name": "memory"}}),
            serde_json::json!({"type": "function", "function": {"name": "bash"}}),
            serde_json::json!({"type": "function", "function": {"name": "read_file"}}),
            serde_json::json!({"type": "function", "function": {"name": "not_registered"}}),
        ];

        let names: Vec<String> = CapabilityResolver
            .filter_tool_schemas(&registry, schemas, &binding.capabilities)
            .into_iter()
            .filter_map(|schema| tool_schema_name(&schema).map(str::to_string))
            .collect();

        for expected in ["tool_search", "memory", "bash", "read_file"] {
            assert!(
                names.iter().any(|name| name == expected),
                "{expected} should be visible for a local CLI runtime: {names:?}"
            );
        }
        assert!(!names.iter().any(|name| name == "not_registered"));
    }

    #[test]
    fn schema_filter_hides_project_tools_without_workspace_runtime() {
        let registry = registry();
        let binding = RunBinding::cloud_control_plane(&registry);
        let schemas = vec![
            serde_json::json!({"type": "function", "function": {"name": "tool_search"}}),
            serde_json::json!({"type": "function", "function": {"name": "bash"}}),
            serde_json::json!({"type": "function", "function": {"name": "read_file"}}),
        ];

        let names: Vec<String> = CapabilityResolver
            .filter_tool_schemas(&registry, schemas, &binding.capabilities)
            .into_iter()
            .filter_map(|schema| tool_schema_name(&schema).map(str::to_string))
            .collect();

        assert!(names.iter().any(|name| name == "tool_search"));
        assert!(!names.iter().any(|name| name == "bash"));
        assert!(!names.iter().any(|name| name == "read_file"));
    }

    #[test]
    fn tool_schema_name_rejects_non_function_type() {
        // Non-function schemas must not leak a name — fail closed.
        let custom = serde_json::json!({"type": "custom", "function": {"name": "leaked"}});
        assert!(
            tool_schema_name(&custom).is_none(),
            "non-function tool type must not expose a schema name"
        );
    }

    #[test]
    fn tool_schema_name_accepts_missing_type_function_schema() {
        let no_type = serde_json::json!({"function": {"name": "read_file"}});
        assert_eq!(
            tool_schema_name(&no_type),
            Some("read_file"),
            "provider shorthand without `type` should still expose a function schema name"
        );
    }

    #[test]
    fn tool_schema_name_rejects_empty_or_whitespace_names() {
        let empty = serde_json::json!({"type": "function", "function": {"name": ""}});
        let whitespace = serde_json::json!({"type": "function", "function": {"name": "   "}});
        assert!(
            tool_schema_name(&empty).is_none(),
            "empty name must be rejected"
        );
        assert!(
            tool_schema_name(&whitespace).is_none(),
            "whitespace-only name must be rejected"
        );
    }

    #[test]
    fn tool_schema_name_rejects_missing_function_or_name() {
        let no_function = serde_json::json!({"type": "function"});
        let no_name = serde_json::json!({"type": "function", "function": {}});
        assert!(tool_schema_name(&no_function).is_none());
        assert!(tool_schema_name(&no_name).is_none());
    }

    #[test]
    fn tool_schema_name_accepts_valid_function_schema() {
        let valid = serde_json::json!({"type": "function", "function": {"name": "bash"}});
        assert_eq!(tool_schema_name(&valid), Some("bash"));
    }

    #[test]
    fn filter_tool_schemas_fails_closed_for_non_function_empty_and_duplicate_names() {
        // Non-function and empty-named schemas fail closed. Missing top-level
        // `type` is valid provider shorthand, but a tool may only appear once
        // in the runtime surface.
        let registry = registry();
        let binding = RunBinding::local_developer("/repo", &registry);
        let schemas = vec![
            serde_json::json!({"type": "function", "function": {"name": "bash"}}),
            serde_json::json!({"type": "custom", "function": {"name": "bash"}}),
            serde_json::json!({"type": "function", "function": {"name": ""}}),
            serde_json::json!({"function": {"name": "bash"}}),
        ];

        let names: Vec<String> = CapabilityResolver
            .filter_tool_schemas(&registry, schemas, &binding.capabilities)
            .into_iter()
            .filter_map(|schema| tool_schema_name(&schema).map(str::to_string))
            .collect();

        // Only the single valid `bash` schema should remain — no duplicates.
        assert_eq!(names, vec!["bash".to_string()]);
    }

    #[test]
    fn filter_tool_schemas_accepts_missing_type_when_not_duplicate() {
        let registry = registry();
        let binding = RunBinding::local_developer("/repo", &registry);
        let schemas = vec![serde_json::json!({"function": {"name": "read_file"}})];

        let names: Vec<String> = CapabilityResolver
            .filter_tool_schemas(&registry, schemas, &binding.capabilities)
            .into_iter()
            .filter_map(|schema| tool_schema_name(&schema).map(str::to_string))
            .collect();

        assert_eq!(names, vec!["read_file".to_string()]);
    }
}
