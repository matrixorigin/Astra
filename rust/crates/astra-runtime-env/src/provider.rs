use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{RequiredExecutor, ToolRegistry, ToolSpec, WorkspaceAuthority};

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum CapacityProviderType {
    ServerService,
    ControlPlane,
    RequestScopedMcp,
    EdgeCapacity,
    Sandbox,
    OrchestratorManagedRuntime,
    CliLocal,
    #[default]
    Unknown,
}

impl CapacityProviderType {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ServerService => "server_service",
            Self::ControlPlane => "control_plane",
            Self::RequestScopedMcp => "request_scoped_mcp",
            Self::EdgeCapacity => "edge_capacity",
            Self::Sandbox => "sandbox",
            Self::OrchestratorManagedRuntime => "orchestrator_managed_runtime",
            Self::CliLocal => "cli_local",
            Self::Unknown => "unknown",
        }
    }
}

impl fmt::Display for CapacityProviderType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl PartialEq<&str> for CapacityProviderType {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<CapacityProviderType> for &str {
    fn eq(&self, other: &CapacityProviderType) -> bool {
        *self == other.as_str()
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum CapacityProviderStatus {
    Ready,
    Unbound,
    Degraded,
    Offline,
    #[default]
    Unknown,
}

impl CapacityProviderStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Unbound => "unbound",
            Self::Degraded => "degraded",
            Self::Offline => "offline",
            Self::Unknown => "unknown",
        }
    }
}

impl fmt::Display for CapacityProviderStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl PartialEq<&str> for CapacityProviderStatus {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<CapacityProviderStatus> for &str {
    fn eq(&self, other: &CapacityProviderStatus) -> bool {
        *self == other.as_str()
    }
}

/// Runtime/provider capacity status surfaced to introspect, UI metadata, and
/// context assembly. Provider types are product/runtime concepts, not
/// implementation class names.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapacityProviderCoverageEntry {
    pub provider_type: CapacityProviderType,
    pub provider_id: String,
    pub status: CapacityProviderStatus,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<String>,
}

/// A concrete provider declaration for one runtime surface.
///
/// Tool visibility starts here: schemas are eligible only when an active
/// provider declares ownership of the tool, then binding/runtime/policy checks
/// decide whether the declared tool is actually usable for this turn.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapacityProviderDeclaration {
    pub provider_type: CapacityProviderType,
    pub provider_id: String,
    #[serde(default)]
    pub tool_names: BTreeSet<String>,
    #[serde(default)]
    pub dynamic_prefixes: Vec<String>,
}

pub trait CapacityProvider {
    fn provider_type(&self) -> CapacityProviderType;
    fn provider_id(&self) -> &str;
    fn declares_tool(&self, tool_name: &str) -> bool;
}

impl CapacityProvider for CapacityProviderDeclaration {
    fn provider_type(&self) -> CapacityProviderType {
        self.provider_type
    }

    fn provider_id(&self) -> &str {
        &self.provider_id
    }

    fn declares_tool(&self, tool_name: &str) -> bool {
        self.tool_names.contains(tool_name)
            || self
                .dynamic_prefixes
                .iter()
                .any(|prefix| tool_name.starts_with(prefix))
    }
}

impl CapacityProviderDeclaration {
    pub fn new(
        provider_type: CapacityProviderType,
        provider_id: impl Into<String>,
        tool_names: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            provider_type,
            provider_id: provider_id.into(),
            tool_names: tool_names.into_iter().collect(),
            dynamic_prefixes: Vec::new(),
        }
    }

    pub fn from_registry(
        provider_type: CapacityProviderType,
        provider_id: impl Into<String>,
        registry: &ToolRegistry,
        mut predicate: impl FnMut(&ToolSpec) -> bool,
    ) -> Self {
        Self::new(
            provider_type,
            provider_id,
            registry
                .iter()
                .filter(|spec| predicate(spec))
                .map(|spec| spec.name.clone()),
        )
    }

    pub fn with_dynamic_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.dynamic_prefixes.push(prefix.into());
        self
    }
}

impl CapacityProviderCoverageEntry {
    pub fn ready(
        provider_type: CapacityProviderType,
        provider_id: impl Into<String>,
        capabilities: Vec<String>,
    ) -> Self {
        Self {
            provider_type,
            provider_id: provider_id.into(),
            status: CapacityProviderStatus::Ready,
            capabilities,
            unavailable_reason: None,
        }
    }

    pub fn unavailable(
        provider_type: CapacityProviderType,
        provider_id: impl Into<String>,
        status: CapacityProviderStatus,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            provider_type,
            provider_id: provider_id.into(),
            status,
            capabilities: Vec::new(),
            unavailable_reason: Some(reason.into()),
        }
    }
}

pub const CAP_WEB_FETCH: &str = "web_fetch";
pub const CAP_WEB_SEARCH: &str = "web_search";
pub const CAP_MEMORY: &str = "memory";
pub const CAP_REFLECT: &str = "reflect";
pub const CAP_INTROSPECT: &str = "introspect";
pub const CAP_TOOL_SEARCH: &str = "tool_search";
pub const CAP_MO_QUERY: &str = "mo_query";
pub const CAP_SESSION: &str = "session";
pub const CAP_TASK: &str = "task";
pub const CAP_PLAN: &str = "plan";
pub const CAP_MULTI_AGENT: &str = "multi_agent";
pub const CAP_POLICY: &str = "policy";
pub const CAP_AUDIT: &str = "audit";
pub const CAP_WORKSPACE_READ: &str = "workspace_read";
pub const CAP_WORKSPACE_WRITE: &str = "workspace_write";
pub const CAP_SHELL: &str = "shell";
pub const CAP_GIT: &str = "git";
pub const CAP_GIT_READ: &str = "git_read";
pub const CAP_RUN_SCRIPT: &str = "run_script";
pub const CAP_SYMBOLS: &str = "symbols";
pub const CAP_LOCAL_BACKGROUND_TASKS: &str = "local_background_tasks";

pub fn server_service_capabilities() -> Vec<String> {
    labels([
        CAP_WEB_FETCH,
        CAP_WEB_SEARCH,
        CAP_MEMORY,
        CAP_REFLECT,
        CAP_INTROSPECT,
        CAP_TOOL_SEARCH,
    ])
}

pub fn control_plane_capabilities(extra: impl IntoIterator<Item = &'static str>) -> Vec<String> {
    let mut capabilities = labels([CAP_SESSION, CAP_TASK, CAP_PLAN, CAP_POLICY, CAP_AUDIT]);
    capabilities.extend(extra.into_iter().map(str::to_string));
    capabilities
}

pub fn read_write_workspace_capabilities() -> Vec<String> {
    labels([
        CAP_WEB_FETCH,
        CAP_WEB_SEARCH,
        CAP_WORKSPACE_READ,
        CAP_WORKSPACE_WRITE,
        CAP_SHELL,
        CAP_GIT,
        CAP_RUN_SCRIPT,
        CAP_SYMBOLS,
    ])
}

pub fn workspace_runtime_capabilities(authority: WorkspaceAuthority) -> Vec<String> {
    match authority {
        WorkspaceAuthority::ReadOnly => labels([
            CAP_WEB_FETCH,
            CAP_WEB_SEARCH,
            CAP_WORKSPACE_READ,
            CAP_GIT_READ,
            CAP_SYMBOLS,
        ]),
        WorkspaceAuthority::ReadWrite => read_write_workspace_capabilities(),
        WorkspaceAuthority::None | WorkspaceAuthority::Unknown => Vec::new(),
    }
}

pub fn request_scoped_mcp_coverage(
    provider_id: impl Into<String>,
    schemas_present: bool,
    ready_tool_names: Vec<String>,
) -> CapacityProviderCoverageEntry {
    let provider_id = provider_id.into();
    if !ready_tool_names.is_empty() {
        return CapacityProviderCoverageEntry::ready(
            CapacityProviderType::RequestScopedMcp,
            provider_id,
            ready_tool_names,
        );
    }

    let reason = if schemas_present {
        "no_request_scoped_mcp_runtime_binding"
    } else {
        "no_request_scoped_mcp_provider_bound"
    };
    CapacityProviderCoverageEntry::unavailable(
        CapacityProviderType::RequestScopedMcp,
        provider_id,
        CapacityProviderStatus::Unbound,
        reason,
    )
}

pub fn server_service_provider(
    provider_id: impl Into<String>,
    registry: &ToolRegistry,
) -> CapacityProviderDeclaration {
    CapacityProviderDeclaration::from_registry(
        CapacityProviderType::ServerService,
        provider_id,
        registry,
        |spec| {
            matches!(
                spec.required.executor,
                RequiredExecutor::ServiceExecutor | RequiredExecutor::ServiceOrRuntimeExecutor
            )
        },
    )
}

pub fn control_plane_provider(
    provider_id: impl Into<String>,
    registry: &ToolRegistry,
) -> CapacityProviderDeclaration {
    CapacityProviderDeclaration::from_registry(
        CapacityProviderType::ControlPlane,
        provider_id,
        registry,
        |spec| matches!(spec.required.executor, RequiredExecutor::ControlPlane),
    )
}

pub fn runtime_workspace_provider(
    provider_type: CapacityProviderType,
    provider_id: impl Into<String>,
    registry: &ToolRegistry,
) -> CapacityProviderDeclaration {
    CapacityProviderDeclaration::from_registry(provider_type, provider_id, registry, |spec| {
        matches!(
            spec.required.executor,
            RequiredExecutor::RuntimeExecutor | RequiredExecutor::ServiceOrRuntimeExecutor
        ) && runtime_workspace_provider_declares_tool(provider_type, spec.name.as_str())
    })
}

pub fn cli_local_provider(
    provider_id: impl Into<String>,
    registry: &ToolRegistry,
) -> CapacityProviderDeclaration {
    runtime_workspace_provider(CapacityProviderType::CliLocal, provider_id, registry)
}

fn runtime_workspace_provider_declares_tool(
    provider_type: CapacityProviderType,
    tool_name: &str,
) -> bool {
    // These are access-surface/platform-local affordances, not generic
    // workspace executor capabilities. CLI-local may expose them because the
    // terminal/platform is the access surface; web/server+edge must wait for an
    // explicit terminal or platform-specific provider instead of showing tools
    // that can only fail.
    if !matches!(provider_type, CapacityProviderType::CliLocal)
        && matches!(tool_name, "display_sixel" | "powershell")
    {
        return false;
    }
    true
}

pub fn request_scoped_mcp_provider(
    provider_id: impl Into<String>,
    tool_names: impl IntoIterator<Item = String>,
) -> CapacityProviderDeclaration {
    CapacityProviderDeclaration::new(
        CapacityProviderType::RequestScopedMcp,
        provider_id,
        tool_names,
    )
}

fn labels<const N: usize>(values: [&'static str; N]) -> Vec<String> {
    values.into_iter().map(str::to_string).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_type_serializes_as_stable_product_label() {
        let value = serde_json::to_value(CapacityProviderType::OrchestratorManagedRuntime)
            .expect("serialize provider type");
        assert_eq!(value, "orchestrator_managed_runtime");
    }

    #[test]
    fn request_scoped_mcp_coverage_distinguishes_missing_binding_from_missing_provider() {
        let absent = request_scoped_mcp_coverage("mcp", false, Vec::new());
        assert_eq!(absent.provider_type, CapacityProviderType::RequestScopedMcp);
        assert_eq!(absent.status, CapacityProviderStatus::Unbound);
        assert_eq!(
            absent.unavailable_reason.as_deref(),
            Some("no_request_scoped_mcp_provider_bound")
        );

        let stale_schema = request_scoped_mcp_coverage("mcp", true, Vec::new());
        assert_eq!(
            stale_schema.unavailable_reason.as_deref(),
            Some("no_request_scoped_mcp_runtime_binding")
        );

        let ready = request_scoped_mcp_coverage("mcp", true, vec!["mcp__weather".to_string()]);
        assert_eq!(ready.status, CapacityProviderStatus::Ready);
        assert_eq!(ready.capabilities, vec!["mcp__weather".to_string()]);
    }

    #[test]
    fn workspace_runtime_capability_labels_follow_authority() {
        assert!(workspace_runtime_capabilities(WorkspaceAuthority::None).is_empty());
        assert_eq!(
            workspace_runtime_capabilities(WorkspaceAuthority::ReadOnly),
            vec![
                CAP_WEB_FETCH,
                CAP_WEB_SEARCH,
                CAP_WORKSPACE_READ,
                CAP_GIT_READ,
                CAP_SYMBOLS
            ]
        );
        assert!(
            workspace_runtime_capabilities(WorkspaceAuthority::ReadWrite)
                .iter()
                .any(|capability| capability == CAP_SHELL)
        );
    }

    #[test]
    fn provider_declarations_partition_builtin_tool_ownership() {
        let registry = ToolRegistry::builtins();
        let server = server_service_provider("server", &registry);
        let control = control_plane_provider("control", &registry);
        let cli = cli_local_provider("cli", &registry);

        assert!(server.declares_tool("web_fetch"));
        assert!(server.declares_tool("web_search"));
        assert!(server.declares_tool("memory"));
        assert!(!server.declares_tool("bash"));

        assert!(control.declares_tool("ask_user"));
        assert!(control.declares_tool("task"));
        assert!(!control.declares_tool("web_fetch"));

        assert!(cli.declares_tool("bash"));
        assert!(cli.declares_tool("web_fetch"));
        assert!(cli.declares_tool("web_search"));
        assert!(cli.declares_tool("read_file"));
        assert!(cli.declares_tool("powershell"));
        assert!(cli.declares_tool("display_sixel"));
        assert!(!cli.declares_tool("memory"));
    }

    #[test]
    fn non_cli_runtime_workspace_providers_hide_terminal_and_platform_local_tools() {
        let registry = ToolRegistry::builtins();
        for provider_type in [
            CapacityProviderType::EdgeCapacity,
            CapacityProviderType::Sandbox,
            CapacityProviderType::OrchestratorManagedRuntime,
        ] {
            let provider = runtime_workspace_provider(provider_type, "runtime", &registry);
            assert!(provider.declares_tool("bash"));
            assert!(provider.declares_tool("web_fetch"));
            assert!(provider.declares_tool("web_search"));
            assert!(provider.declares_tool("read_file"));
            assert!(
                !provider.declares_tool("powershell"),
                "{provider_type} must not advertise powershell without platform readiness"
            );
            assert!(
                !provider.declares_tool("display_sixel"),
                "{provider_type} must not advertise terminal sixel rendering to web/server surfaces"
            );
        }
    }

    #[test]
    fn request_scoped_mcp_provider_declares_only_bound_tools() {
        let provider = request_scoped_mcp_provider("mcp", ["mcp__weather".to_string()]);

        assert!(provider.declares_tool("mcp__weather"));
        assert!(!provider.declares_tool("mcp__calculator"));
    }
}
