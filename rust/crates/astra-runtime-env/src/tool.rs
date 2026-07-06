use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::{
    CapacityProvider, CapacityProviderDeclaration, CapacityProviderType, EffectiveCapabilitySet,
    NetworkCapability,
};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ToolLoadPolicy {
    /// Visible in the default `tools[]` surface when the schema exists.
    AlwaysLoad,
    /// Advertised through the deferred catalog and activated with `tool_search`.
    Deferred,
    /// Runtime implementation detail. The model should not see this as a
    /// standalone public schema.
    Internal,
}

impl ToolLoadPolicy {
    pub const fn is_public_schema_policy(self) -> bool {
        matches!(self, Self::AlwaysLoad | Self::Deferred)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RequiredExecutor {
    None,
    ControlPlane,
    ServiceExecutor,
    ServiceOrRuntimeExecutor,
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
            executor: RequiredExecutor::ControlPlane,
            ..Self::none()
        }
    }

    pub const fn service_executor() -> Self {
        Self {
            executor: RequiredExecutor::ServiceExecutor,
            ..Self::none()
        }
    }

    pub const fn shared_network() -> Self {
        Self {
            executor: RequiredExecutor::ServiceOrRuntimeExecutor,
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
    pub load_policy: ToolLoadPolicy,
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
        Self::new(builtin_tool_specs())
    }

    pub fn get(&self, name: &str) -> Option<&ToolSpec> {
        self.tools.get(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = &ToolSpec> {
        self.tools.values()
    }
}

fn builtin_tool_specs() -> Vec<ToolSpec> {
    vec![
        // Blocking clarification is part of the default safety loop: when the
        // model needs a user decision, ask_user must already be callable.
        control_plane("ask_user", ToolLoadPolicy::AlwaysLoad),
        control_plane("agent", ToolLoadPolicy::Deferred),
        control_plane("agent_fanout", ToolLoadPolicy::Deferred),
        control_plane("enter_plan_mode", ToolLoadPolicy::Deferred),
        control_plane("exit_plan_mode", ToolLoadPolicy::Deferred),
        control_plane("get_agent_info", ToolLoadPolicy::Deferred),
        // Self-observation and session reflection are control-plane recovery
        // entrypoints. They must be callable without a discovery round-trip.
        control_plane("introspect", ToolLoadPolicy::AlwaysLoad),
        control_plane("reflect", ToolLoadPolicy::AlwaysLoad),
        // Non-blocking status updates are still part of the user communication
        // path, so keep notify available with ask_user instead of requiring a
        // discovery round-trip.
        control_plane("notify", ToolLoadPolicy::AlwaysLoad),
        control_plane("compress_context", ToolLoadPolicy::Deferred),
        control_plane("rollback_session_state", ToolLoadPolicy::Deferred),
        control_plane("session", ToolLoadPolicy::Deferred),
        control_plane("skill", ToolLoadPolicy::AlwaysLoad),
        control_plane("task", ToolLoadPolicy::AlwaysLoad),
        control_plane("task_output", ToolLoadPolicy::Deferred),
        control_plane("task_stop", ToolLoadPolicy::Deferred),
        control_plane("task_list", ToolLoadPolicy::Deferred),
        server_service("memory", ToolLoadPolicy::AlwaysLoad),
        server_service("mo_query", ToolLoadPolicy::Deferred),
        server_service("rollback_database_snapshots", ToolLoadPolicy::Deferred),
        control_plane("tool_search", ToolLoadPolicy::AlwaysLoad),
        shared_network("web_search", ToolLoadPolicy::Deferred),
        shared_network("web_fetch", ToolLoadPolicy::Deferred),
        server_network_credentials("github", ToolLoadPolicy::Deferred),
        project_write("publish_artifact", ToolLoadPolicy::Deferred),
        project_read("read_file", ToolLoadPolicy::AlwaysLoad),
        project_read("list_dir", ToolLoadPolicy::AlwaysLoad),
        project_read("grep", ToolLoadPolicy::AlwaysLoad),
        project_read("glob", ToolLoadPolicy::AlwaysLoad),
        project_read("symbols", ToolLoadPolicy::Deferred),
        // Reads an image file from the workspace and renders it to the terminal
        // via img2sixel. Opt-in — an agent calls it after producing an image.
        project_read("display_sixel", ToolLoadPolicy::Deferred),
        project_write("write_file", ToolLoadPolicy::AlwaysLoad),
        project_write("str_replace", ToolLoadPolicy::AlwaysLoad),
        project_write("delete_file", ToolLoadPolicy::Internal),
        project_write("multi_edit", ToolLoadPolicy::Internal),
        project_write("rollback_file_edits", ToolLoadPolicy::Deferred),
        shell("bash", ToolLoadPolicy::AlwaysLoad),
        shell("powershell", ToolLoadPolicy::Deferred),
        shell("run_script", ToolLoadPolicy::Deferred),
        background_shell("background_shell", ToolLoadPolicy::Internal),
        git_read("git", ToolLoadPolicy::AlwaysLoad),
        git_clone("git_clone", ToolLoadPolicy::Internal),
        lsp("lsp", ToolLoadPolicy::Deferred),
        lsp("find_definition", ToolLoadPolicy::Internal),
        lsp("find_references", ToolLoadPolicy::Internal),
    ]
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolOfferCandidateReason {
    Selected,
    LowerPriority,
    ProviderTypeMismatch,
    CapabilityUnavailable,
    SchemaConflict,
    ToolNameConflict,
    AmbiguousProvider,
    ProviderIdConflict,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolOfferCandidate {
    pub offer_id: String,
    pub provider_type: CapacityProviderType,
    pub provider_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_digest: Option<String>,
    pub selected: bool,
    pub reason: ToolOfferCandidateReason,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolSurfaceAdmission {
    pub tool_name: String,
    pub visible: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_offer_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_provider_type: Option<CapacityProviderType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_provider_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hidden_reason: Option<ToolUnavailableReason>,
    #[serde(default)]
    pub candidates: Vec<ToolOfferCandidate>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AvailableToolSurface {
    pub tool_names: Vec<String>,
    pub denials: Vec<ToolDenial>,
    #[serde(default, skip_serializing)]
    pub admissions: Vec<ToolSurfaceAdmission>,
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

    pub fn admission_for(&self, tool_name: &str) -> Option<&ToolSurfaceAdmission> {
        self.admissions
            .iter()
            .find(|admission| admission.tool_name == tool_name)
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
    pub fn available_tool_surface_for_providers(
        &self,
        registry: &ToolRegistry,
        capabilities: &EffectiveCapabilitySet,
        providers: &[CapacityProviderDeclaration],
    ) -> AvailableToolSurface {
        self.available_tool_surface_impl(registry, capabilities, providers)
    }

    fn available_tool_surface_impl(
        &self,
        registry: &ToolRegistry,
        capabilities: &EffectiveCapabilitySet,
        providers: &[CapacityProviderDeclaration],
    ) -> AvailableToolSurface {
        let provider_conflicts = provider_conflicting_tools(registry, capabilities, providers);
        let mut tool_names = Vec::new();
        let mut denials = Vec::new();
        let mut admissions = Vec::new();
        let mut specs: Vec<ToolSpec> = registry.iter().cloned().collect();
        let mut seen: HashSet<String> = specs.iter().map(|spec| spec.name.clone()).collect();
        for provider in providers {
            for tool_name in &provider.tool_names {
                if seen.contains(tool_name) {
                    continue;
                }
                let Some(spec) = dynamic_tool_spec(tool_name) else {
                    continue;
                };
                seen.insert(tool_name.clone());
                specs.push(spec);
            }
        }
        for spec in &specs {
            if !providers
                .iter()
                .any(|provider| provider.declares_tool(&spec.name))
            {
                continue;
            }
            let provider_conflict = provider_conflicts.get(&spec.name).copied();
            let mut admission =
                self.admission_for_spec(spec, capabilities, providers, provider_conflict);
            if let Some(conflict) = provider_conflict {
                let reason =
                    ToolUnavailableReason::PolicyDenied(conflict.denial_code().to_string());
                admission.visible = false;
                admission.selected_offer_id = None;
                admission.selected_provider_type = None;
                admission.selected_provider_id = None;
                admission.hidden_reason = Some(reason.clone());
                denials.push(ToolDenial {
                    tool_name: spec.name.clone(),
                    reason,
                });
                admissions.push(admission);
                continue;
            }
            match self.check(spec, capabilities) {
                Ok(()) if admission.visible => tool_names.push(spec.name.clone()),
                Ok(()) => {
                    if let Some(reason) = admission.hidden_reason.clone() {
                        denials.push(ToolDenial {
                            tool_name: spec.name.clone(),
                            reason,
                        });
                    }
                }
                Err(reason) => {
                    admission.visible = false;
                    admission.hidden_reason = Some(reason.clone());
                    denials.push(ToolDenial {
                        tool_name: spec.name.clone(),
                        reason,
                    });
                }
            }
            admissions.push(admission);
        }
        // Sort for deterministic serialization (HashMap iteration is non-deterministic).
        tool_names.sort();
        denials.sort_by(|a, b| a.tool_name.cmp(&b.tool_name));
        admissions.sort_by(|a, b| a.tool_name.cmp(&b.tool_name));
        AvailableToolSurface {
            tool_names,
            denials,
            admissions,
        }
    }

    fn admission_for_spec(
        &self,
        spec: &ToolSpec,
        capabilities: &EffectiveCapabilitySet,
        providers: &[CapacityProviderDeclaration],
        provider_conflict: Option<ProviderToolConflictKind>,
    ) -> ToolSurfaceAdmission {
        let declared_candidates: Vec<&CapacityProviderDeclaration> = providers
            .iter()
            .filter(|provider| provider.declares_tool(&spec.name))
            .collect();
        let selected_provider = if provider_conflict.is_some() {
            None
        } else {
            selected_provider_for_tool(spec, capabilities, &declared_candidates)
        };
        let selected_offer_id =
            selected_provider.map(|provider| offer_id(spec.name.as_str(), provider));
        let selected_provider_type = selected_provider.map(|provider| provider.provider_type);
        let selected_provider_id = selected_provider.map(|provider| provider.provider_id.clone());
        let hidden_reason = if selected_provider.is_none() && provider_conflict.is_none() {
            Some(ToolUnavailableReason::ExecutorUnavailable(
                "no_matching_provider_offer".to_string(),
            ))
        } else {
            None
        };
        let mut candidates: Vec<ToolOfferCandidate> = declared_candidates
            .into_iter()
            .map(|provider| {
                let offer_id = offer_id(spec.name.as_str(), provider);
                let selected = selected_offer_id.as_deref() == Some(offer_id.as_str());
                let reason = if let Some(conflict) = provider_conflict {
                    conflict.candidate_reason()
                } else if selected {
                    ToolOfferCandidateReason::Selected
                } else if provider_type_matches_requirement(
                    spec.required.executor,
                    provider.provider_type,
                ) && provider_capability_ready(
                    spec.required.executor,
                    capabilities,
                    provider.provider_type,
                ) {
                    ToolOfferCandidateReason::LowerPriority
                } else if provider_type_matches_requirement(
                    spec.required.executor,
                    provider.provider_type,
                ) {
                    ToolOfferCandidateReason::CapabilityUnavailable
                } else {
                    ToolOfferCandidateReason::ProviderTypeMismatch
                };
                ToolOfferCandidate {
                    offer_id,
                    provider_type: provider.provider_type,
                    provider_id: provider.provider_id.clone(),
                    schema_digest: provider
                        .schema_digest_for_tool(&spec.name)
                        .map(str::to_string),
                    selected,
                    reason,
                }
            })
            .collect();
        candidates.sort_by(|a, b| {
            a.provider_type
                .as_str()
                .cmp(b.provider_type.as_str())
                .then_with(|| a.provider_id.cmp(&b.provider_id))
                .then_with(|| a.offer_id.cmp(&b.offer_id))
        });

        ToolSurfaceAdmission {
            tool_name: spec.name.clone(),
            visible: selected_provider.is_some() && self.check(spec, capabilities).is_ok(),
            selected_offer_id,
            selected_provider_type,
            selected_provider_id,
            hidden_reason,
            candidates,
        }
    }

    pub fn filter_tool_schemas_for_providers(
        &self,
        registry: &ToolRegistry,
        schemas: Vec<Value>,
        capabilities: &EffectiveCapabilitySet,
        providers: &[CapacityProviderDeclaration],
    ) -> Vec<Value> {
        let provider_conflicts = provider_conflicting_tools(registry, capabilities, providers);
        let prompt_schema_conflicts =
            astra_core::tool_schema::prompt_schema_conflicting_tool_names(&schemas);
        self.filter_tool_schemas_impl(registry, schemas, capabilities, |tool_name| {
            if provider_conflicts.contains_key(tool_name)
                || prompt_schema_conflicts.contains(tool_name)
            {
                return false;
            }
            providers
                .iter()
                .any(|provider| provider.declares_tool(tool_name))
        })
    }

    pub fn filter_tool_schemas_for_binding(
        &self,
        registry: &ToolRegistry,
        schemas: Vec<Value>,
        binding: &crate::RunBinding,
    ) -> Vec<Value> {
        let prompt_schema_conflicts =
            astra_core::tool_schema::prompt_schema_conflicting_tool_names(&schemas);
        self.filter_tool_schemas_impl(registry, schemas, &binding.capabilities, |tool_name| {
            !prompt_schema_conflicts.contains(tool_name) && binding.tool_surface.contains(tool_name)
        })
    }

    fn filter_tool_schemas_impl(
        &self,
        registry: &ToolRegistry,
        schemas: Vec<Value>,
        capabilities: &EffectiveCapabilitySet,
        mut provider_declares: impl FnMut(&str) -> bool,
    ) -> Vec<Value> {
        let mut seen = HashSet::new();
        let filtered: Vec<Value> = schemas
            .into_iter()
            .filter(|schema| {
                let Some(tool_name) = tool_schema_name(schema) else {
                    return false;
                };
                if !seen.insert(tool_name.to_string()) {
                    return false;
                }
                if !provider_declares(tool_name) {
                    return false;
                }
                let dynamic_spec = dynamic_tool_spec(tool_name);
                let Some(spec) = registry.get(tool_name).or(dynamic_spec.as_ref()) else {
                    return false;
                };
                if !spec.load_policy.is_public_schema_policy() {
                    return false;
                }
                self.check_tool_call(registry, tool_name, &Value::Null, capabilities)
                    .is_ok()
            })
            .collect();
        let mut catalog_schemas = Vec::new();
        let mut dynamic_schemas = Vec::new();
        for schema in filtered {
            if tool_schema_name(&schema).is_some_and(|name| registry.get(name).is_some()) {
                catalog_schemas.push(schema);
            } else {
                dynamic_schemas.push(schema);
            }
        }
        astra_core::tool_schema::sort_tool_schemas_by_name(&mut dynamic_schemas);
        catalog_schemas.extend(dynamic_schemas);
        catalog_schemas
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

    pub fn check_tool_for_surface(
        &self,
        registry: &ToolRegistry,
        tool_name: &str,
        capabilities: &EffectiveCapabilitySet,
        surface: &AvailableToolSurface,
    ) -> Result<(), ToolUnavailableReason> {
        check_surface_admits_tool(tool_name, surface)?;
        self.check_tool(registry, tool_name, capabilities)
    }

    pub fn check_tool_call(
        &self,
        registry: &ToolRegistry,
        tool_name: &str,
        args: &Value,
        capabilities: &EffectiveCapabilitySet,
    ) -> Result<(), ToolUnavailableReason> {
        if tool_name == "git" && git_action_requires_write(args) {
            let spec = git_write(tool_name, ToolLoadPolicy::AlwaysLoad);
            return self.check(&spec, capabilities);
        }
        if tool_name == "lsp" && lsp_action_requires_write(args) {
            let spec = project_write(tool_name, ToolLoadPolicy::Deferred);
            return self.check(&spec, capabilities);
        }
        let dynamic_spec = dynamic_tool_spec(tool_name);
        let spec = registry
            .get(tool_name)
            .or(dynamic_spec.as_ref())
            .ok_or(ToolUnavailableReason::UnknownTool)?;
        self.check(spec, capabilities)
    }

    pub fn check_tool_call_for_surface(
        &self,
        registry: &ToolRegistry,
        tool_name: &str,
        args: &Value,
        capabilities: &EffectiveCapabilitySet,
        surface: &AvailableToolSurface,
    ) -> Result<(), ToolUnavailableReason> {
        let dynamic_spec = dynamic_tool_spec(tool_name);
        if registry.get(tool_name).or(dynamic_spec.as_ref()).is_none() {
            return Err(ToolUnavailableReason::UnknownTool);
        }
        check_surface_admits_tool(tool_name, surface)?;
        self.check_tool_call(registry, tool_name, args, capabilities)
    }

    pub fn check(
        &self,
        spec: &ToolSpec,
        capabilities: &EffectiveCapabilitySet,
    ) -> Result<(), ToolUnavailableReason> {
        match spec.required.executor {
            RequiredExecutor::None => {}
            RequiredExecutor::ControlPlane if !capabilities.executor.control_plane => {
                return Err(ToolUnavailableReason::ExecutorUnavailable(
                    "control_plane_required".to_string(),
                ));
            }
            RequiredExecutor::ServiceExecutor if !capabilities.executor.server_service => {
                return Err(ToolUnavailableReason::ExecutorUnavailable(
                    "service_executor_required".to_string(),
                ));
            }
            RequiredExecutor::ServiceOrRuntimeExecutor
                if !capabilities.executor.server_service
                    && !capabilities.executor.runtime_executor =>
            {
                return Err(ToolUnavailableReason::ExecutorUnavailable(
                    "service_or_runtime_executor_required".to_string(),
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
            | RequiredExecutor::ServiceOrRuntimeExecutor
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

fn check_surface_admits_tool(
    tool_name: &str,
    surface: &AvailableToolSurface,
) -> Result<(), ToolUnavailableReason> {
    if surface.contains(tool_name) {
        return Ok(());
    }
    Err(surface
        .denial_for(tool_name)
        .cloned()
        .or_else(|| {
            surface
                .admission_for(tool_name)
                .and_then(|admission| admission.hidden_reason.clone())
        })
        .unwrap_or_else(|| {
            ToolUnavailableReason::ExecutorUnavailable(
                "tool_not_selected_by_current_provider_surface".to_string(),
            )
        }))
}

fn selected_provider_for_tool<'a>(
    spec: &ToolSpec,
    capabilities: &EffectiveCapabilitySet,
    providers: &[&'a CapacityProviderDeclaration],
) -> Option<&'a CapacityProviderDeclaration> {
    let mut candidates: Vec<&CapacityProviderDeclaration> = providers
        .iter()
        .copied()
        .filter(|provider| {
            provider_type_matches_requirement(spec.required.executor, provider.provider_type)
                && provider_capability_ready(
                    spec.required.executor,
                    capabilities,
                    provider.provider_type,
                )
        })
        .collect();
    candidates.sort_by(|a, b| {
        provider_selection_rank(spec.required.executor, capabilities, a.provider_type)
            .cmp(&provider_selection_rank(
                spec.required.executor,
                capabilities,
                b.provider_type,
            ))
            .then_with(|| a.provider_type.as_str().cmp(b.provider_type.as_str()))
            .then_with(|| a.provider_id.cmp(&b.provider_id))
    });
    candidates.into_iter().next()
}

fn provider_capability_ready(
    required: RequiredExecutor,
    capabilities: &EffectiveCapabilitySet,
    provider_type: CapacityProviderType,
) -> bool {
    match required {
        RequiredExecutor::None => true,
        RequiredExecutor::ControlPlane => capabilities.executor.control_plane,
        RequiredExecutor::ServiceExecutor => capabilities.executor.server_service,
        RequiredExecutor::ServiceOrRuntimeExecutor => {
            (provider_type == CapacityProviderType::ServerService
                && capabilities.executor.server_service)
                || (provider_type.is_runtime_executor() && capabilities.executor.runtime_executor)
        }
        RequiredExecutor::RuntimeExecutor => capabilities.executor.runtime_executor,
        RequiredExecutor::McpExecutor => capabilities.executor.mcp_executor,
    }
}

fn provider_type_matches_requirement(
    required: RequiredExecutor,
    provider_type: CapacityProviderType,
) -> bool {
    match required {
        RequiredExecutor::None => true,
        RequiredExecutor::ControlPlane => provider_type == CapacityProviderType::ControlPlane,
        RequiredExecutor::ServiceExecutor => provider_type == CapacityProviderType::ServerService,
        RequiredExecutor::ServiceOrRuntimeExecutor => {
            provider_type == CapacityProviderType::ServerService
                || provider_type.is_runtime_executor()
        }
        RequiredExecutor::RuntimeExecutor => provider_type.is_runtime_executor(),
        RequiredExecutor::McpExecutor => matches!(
            provider_type,
            CapacityProviderType::RequestScopedMcp | CapacityProviderType::McpProvider
        ),
    }
}

fn provider_selection_rank(
    required: RequiredExecutor,
    capabilities: &EffectiveCapabilitySet,
    provider_type: CapacityProviderType,
) -> u8 {
    match required {
        RequiredExecutor::ServiceOrRuntimeExecutor
            if capabilities.executor.runtime_executor && provider_type.is_runtime_executor() =>
        {
            0
        }
        RequiredExecutor::ServiceOrRuntimeExecutor
            if provider_type == CapacityProviderType::ServerService =>
        {
            1
        }
        RequiredExecutor::RuntimeExecutor if provider_type == CapacityProviderType::CliLocal => 0,
        RequiredExecutor::RuntimeExecutor
            if provider_type == CapacityProviderType::EdgeCapacity =>
        {
            1
        }
        RequiredExecutor::RuntimeExecutor if provider_type == CapacityProviderType::Sandbox => 2,
        RequiredExecutor::RuntimeExecutor
            if provider_type == CapacityProviderType::OrchestratorManagedRuntime =>
        {
            3
        }
        _ => 0,
    }
}

fn offer_id(tool_name: &str, provider: &CapacityProviderDeclaration) -> String {
    crate::tool_offer_id(tool_name, &provider.provider_id)
}

pub use astra_core::tool_schema::tool_schema_name;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderToolConflictKind {
    SchemaContract,
    DynamicToolName,
    AmbiguousProvider,
    ProviderIdCollision,
}

impl ProviderToolConflictKind {
    const fn denial_code(self) -> &'static str {
        match self {
            Self::SchemaContract => "provider_schema_conflict",
            Self::DynamicToolName => "provider_tool_name_conflict",
            Self::AmbiguousProvider => "provider_owner_ambiguous",
            Self::ProviderIdCollision => "provider_id_conflict",
        }
    }

    const fn candidate_reason(self) -> ToolOfferCandidateReason {
        match self {
            Self::SchemaContract => ToolOfferCandidateReason::SchemaConflict,
            Self::DynamicToolName => ToolOfferCandidateReason::ToolNameConflict,
            Self::AmbiguousProvider => ToolOfferCandidateReason::AmbiguousProvider,
            Self::ProviderIdCollision => ToolOfferCandidateReason::ProviderIdConflict,
        }
    }
}

fn provider_conflicting_tools(
    registry: &ToolRegistry,
    capabilities: &EffectiveCapabilitySet,
    providers: &[CapacityProviderDeclaration],
) -> HashMap<String, ProviderToolConflictKind> {
    let duplicate_provider_ids = duplicate_provider_ids(providers);
    let mut providers_by_tool: HashMap<String, Vec<&CapacityProviderDeclaration>> = HashMap::new();
    for provider in providers {
        for tool_name in &provider.tool_names {
            providers_by_tool
                .entry(tool_name.clone())
                .or_default()
                .push(provider);
        }
    }

    providers_by_tool
        .into_iter()
        .filter_map(|(tool_name, declaring_providers)| {
            if declaring_providers
                .iter()
                .any(|provider| duplicate_provider_ids.contains(provider.provider_id.as_str()))
            {
                return Some((tool_name, ProviderToolConflictKind::ProviderIdCollision));
            }
            if declaring_providers.len() <= 1 {
                return None;
            }
            if provider_schema_digest_conflicts(&tool_name, &declaring_providers) {
                return Some((tool_name, ProviderToolConflictKind::SchemaContract));
            }

            let dynamic_spec = dynamic_tool_spec(&tool_name);
            let registry_spec = registry.get(&tool_name);
            let spec = registry_spec.or(dynamic_spec.as_ref())?;
            if registry_spec.is_none() {
                return Some((tool_name, ProviderToolConflictKind::DynamicToolName));
            }
            (!provider_selection_is_unambiguous(spec, capabilities, &declaring_providers))
                .then_some((tool_name, ProviderToolConflictKind::AmbiguousProvider))
        })
        .collect()
}

fn duplicate_provider_ids(providers: &[CapacityProviderDeclaration]) -> HashSet<&str> {
    let mut seen = HashSet::new();
    let mut duplicates = HashSet::new();
    for provider in providers {
        let provider_id = provider.provider_id.as_str();
        if !seen.insert(provider_id) {
            duplicates.insert(provider_id);
        }
    }
    duplicates
}

fn provider_schema_digest_conflicts(
    tool_name: &str,
    providers: &[&CapacityProviderDeclaration],
) -> bool {
    let digests: HashSet<&str> = providers
        .iter()
        .filter_map(|provider| provider.schema_digest_for_tool(tool_name))
        .collect();
    digests.len() > 1
}

fn provider_selection_is_unambiguous(
    spec: &ToolSpec,
    capabilities: &EffectiveCapabilitySet,
    providers: &[&CapacityProviderDeclaration],
) -> bool {
    let mut best_rank = None;
    let mut best_count = 0usize;
    for provider in providers {
        if !provider_type_matches_requirement(spec.required.executor, provider.provider_type)
            || !provider_capability_ready(
                spec.required.executor,
                capabilities,
                provider.provider_type,
            )
        {
            continue;
        }
        let rank =
            provider_selection_rank(spec.required.executor, capabilities, provider.provider_type);
        match best_rank {
            None => {
                best_rank = Some(rank);
                best_count = 1;
            }
            Some(current) if rank < current => {
                best_rank = Some(rank);
                best_count = 1;
            }
            Some(current) if rank == current => {
                best_count += 1;
            }
            Some(_) => {}
        }
    }
    best_count == 1
}

fn control_plane(name: &str, load_policy: ToolLoadPolicy) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        load_policy,
        effect: ToolEffect::none(),
        required: ToolRequirements::control_plane(),
    }
}

fn server_service(name: &str, load_policy: ToolLoadPolicy) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        load_policy,
        effect: ToolEffect::none(),
        required: ToolRequirements::service_executor(),
    }
}

fn shared_network(name: &str, load_policy: ToolLoadPolicy) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        load_policy,
        effect: ToolEffect {
            uses_network: true,
            ..ToolEffect::none()
        },
        required: ToolRequirements::shared_network(),
    }
}

fn server_network_credentials(name: &str, load_policy: ToolLoadPolicy) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        load_policy,
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
        load_policy: ToolLoadPolicy::Deferred,
        effect: ToolEffect::none(),
        required: ToolRequirements::mcp(),
    }
}

fn dynamic_tool_spec(name: &str) -> Option<ToolSpec> {
    if crate::is_mcp_namespaced_tool_name(name) {
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

fn project_read(name: &str, load_policy: ToolLoadPolicy) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        load_policy,
        effect: ToolEffect {
            reads_workspace: true,
            ..ToolEffect::none()
        },
        required: ToolRequirements::project_read(),
    }
}

fn project_write(name: &str, load_policy: ToolLoadPolicy) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        load_policy,
        effect: ToolEffect {
            reads_workspace: true,
            writes_workspace: true,
            ..ToolEffect::none()
        },
        required: ToolRequirements::project_write(),
    }
}

fn shell(name: &str, load_policy: ToolLoadPolicy) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        load_policy,
        effect: ToolEffect {
            reads_workspace: true,
            spawns_process: true,
            ..ToolEffect::none()
        },
        required: ToolRequirements::shell(),
    }
}

fn background_shell(name: &str, load_policy: ToolLoadPolicy) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        load_policy,
        effect: ToolEffect {
            reads_workspace: true,
            spawns_process: true,
            ..ToolEffect::none()
        },
        required: ToolRequirements::background_shell(),
    }
}

fn git_read(name: &str, load_policy: ToolLoadPolicy) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        load_policy,
        effect: ToolEffect {
            reads_workspace: true,
            ..ToolEffect::none()
        },
        required: ToolRequirements::git_read(),
    }
}

fn git_write(name: &str, load_policy: ToolLoadPolicy) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        load_policy,
        effect: ToolEffect {
            reads_workspace: true,
            writes_workspace: true,
            mutates_external_state: name == "git_push",
            ..ToolEffect::none()
        },
        required: ToolRequirements::git_write(),
    }
}

fn git_clone(name: &str, load_policy: ToolLoadPolicy) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        load_policy,
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

fn lsp(name: &str, load_policy: ToolLoadPolicy) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        load_policy,
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

    fn local_cli_providers(registry: &ToolRegistry) -> Vec<crate::CapacityProviderDeclaration> {
        vec![
            crate::server_service_provider("server", registry),
            crate::control_plane_provider("control", registry),
            crate::cli_local_provider("cli", registry),
        ]
    }

    fn server_providers(registry: &ToolRegistry) -> Vec<crate::CapacityProviderDeclaration> {
        vec![
            crate::server_service_provider("server", registry),
            crate::control_plane_provider("control", registry),
        ]
    }

    #[test]
    fn builtin_tool_specs_have_unique_names_before_hashmap_projection() {
        let mut seen = std::collections::BTreeSet::new();
        for spec in builtin_tool_specs() {
            assert!(
                seen.insert(spec.name.clone()),
                "duplicate builtin ToolSpec: {}",
                spec.name
            );
        }
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
            "web_fetch",
            "web_search",
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
    fn runtime_executor_binding_does_not_imply_server_owned_capacity() {
        let registry = registry();
        let binding = RunBinding::edge_developer("/repo", &registry);

        assert!(
            !binding.capabilities.executor.control_plane,
            "edge workspace execution capacity must not declare control-plane ownership"
        );
        assert!(binding.capabilities.executor.runtime_executor);
        assert!(
            !binding.capabilities.executor.server_service,
            "edge workspace execution capacity must not declare server-service ownership"
        );

        assert!(binding.tool_surface.contains("read_file"));
        assert!(binding.tool_surface.contains("web_fetch"));
        assert!(binding.tool_surface.contains("web_search"));
        assert!(!binding.tool_surface.contains("ask_user"));
        assert!(!binding.tool_surface.contains("tool_search"));
        assert!(!binding.tool_surface.contains("memory"));
        assert_eq!(
            CapabilityResolver.check_tool(&registry, "web_fetch", &binding.capabilities),
            Ok(())
        );
        assert_eq!(
            CapabilityResolver.check_tool(&registry, "ask_user", &binding.capabilities),
            Err(ToolUnavailableReason::ExecutorUnavailable(
                "control_plane_required".to_string()
            ))
        );
        assert_eq!(
            CapabilityResolver.check_tool(&registry, "tool_search", &binding.capabilities),
            Err(ToolUnavailableReason::ExecutorUnavailable(
                "control_plane_required".to_string()
            ))
        );
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
        for tool in ["bash", "write_file"] {
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
            let admission = binding
                .tool_surface
                .admission_for(tool)
                .unwrap_or_else(|| panic!("{tool} admission"));
            assert!(!admission.visible);
            assert_eq!(
                admission.hidden_reason,
                Some(ToolUnavailableReason::PolicyDenied(
                    PolicyIntent::disallowed_tool_reason(tool)
                ))
            );
        }
        assert!(!binding.tool_surface.contains("ask_user"));
        assert_eq!(binding.tool_surface.denial_for("ask_user"), None);
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
        let tools = git_actions
            .into_iter()
            .map(|action| format!("git_{action}"))
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
    fn control_plane_user_communication_tools_are_always_load() {
        let registry = registry();
        let ask_user = registry.get("ask_user").expect("ask_user registered");
        let notify = registry.get("notify").expect("notify registered");

        assert_eq!(ask_user.required.executor, RequiredExecutor::ControlPlane);
        assert_eq!(notify.required.executor, RequiredExecutor::ControlPlane);
        assert_eq!(ask_user.load_policy, ToolLoadPolicy::AlwaysLoad);
        assert_eq!(notify.load_policy, ToolLoadPolicy::AlwaysLoad);
    }

    #[test]
    fn observation_control_plane_tools_are_always_load() {
        let registry = registry();
        for name in ["introspect", "reflect", "tool_search"] {
            let spec = registry
                .get(name)
                .unwrap_or_else(|| panic!("{name} registered"));
            assert_eq!(
                spec.required.executor,
                RequiredExecutor::ControlPlane,
                "{name} must remain a control-plane observation entrypoint"
            );
            assert_eq!(
                spec.load_policy,
                ToolLoadPolicy::AlwaysLoad,
                "{name} must not require deferred discovery"
            );
        }
    }

    #[test]
    fn dynamic_mcp_tools_are_request_scoped_deferred_not_builtin_always_load() {
        let registry = registry();
        let name = "mcp__filesystem__read_file";

        assert!(
            registry.get(name).is_none(),
            "MCP tools are dynamic request schemas, not builtin ToolSpecs"
        );

        let spec = dynamic_tool_spec(name).expect("mcp-prefixed names resolve dynamically");
        assert_eq!(spec.load_policy, ToolLoadPolicy::Deferred);
        assert_eq!(spec.required.executor, RequiredExecutor::McpExecutor);

        let binding = RunBinding::cloud_control_plane(&registry);
        assert!(!binding.tool_surface.contains(name));
        assert_eq!(
            CapabilityResolver.check_tool(&registry, name, &binding.capabilities),
            Err(ToolUnavailableReason::ExecutorUnavailable(
                "mcp_executor_required".to_string()
            ))
        );
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
            None,
            "unknown workspace ownership must make workspace tools invisible, not visible with a wall"
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
                platform: crate::RuntimePlatform::Unknown,
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
            None,
            "missing executor transport must make workspace tools invisible, not visible with a wall"
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
        assert!(!binding.tool_surface.contains("web_fetch"));
        assert!(!binding.tool_surface.contains("web_search"));
        assert_eq!(
            binding.tool_surface.denial_for("git_clone"),
            Some(&ToolUnavailableReason::PolicyDenied(
                "network_allow_list".to_string()
            ))
        );
        assert_eq!(
            binding.tool_surface.denial_for("web_fetch"),
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
    fn request_scoped_mcp_tools_require_provider_offer_in_surface() {
        let registry = registry();
        let unbound = RunBinding::resolve(
            WorkspaceBinding::none(),
            ExecutorBinding {
                kind: crate::ExecutorBindingKind::Mcp,
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
            CapabilityResolver.check_tool_call_for_surface(
                &registry,
                "mcp__github__create_issue",
                &serde_json::json!({}),
                &unbound.capabilities,
                &unbound.tool_surface,
            ),
            Err(ToolUnavailableReason::ExecutorUnavailable(
                "tool_not_selected_by_current_provider_surface".to_string()
            )),
            "MCP executor capability alone must not make an undiscovered MCP tool executable"
        );

        let bound = RunBinding::resolve_with_provider_declarations(
            WorkspaceBinding::none(),
            ExecutorBinding {
                kind: crate::ExecutorBindingKind::Mcp,
                executor_id: "mcp".to_string(),
                display_name: "MCP server".to_string(),
                transport: crate::ToolTransportKind::McpHttp,
                status: crate::ExecutorStatus::Online,
            },
            RuntimeBinding::none(),
            PolicyIntent::cloud_control_plane(),
            &registry,
            &[crate::request_scoped_mcp_provider(
                "mcp",
                ["mcp__github__create_issue".to_string()],
            )],
        );

        assert_eq!(
            CapabilityResolver.check_tool_call_for_surface(
                &registry,
                "mcp__github__create_issue",
                &serde_json::json!({}),
                &bound.capabilities,
                &bound.tool_surface,
            ),
            Ok(())
        );
    }

    #[test]
    fn tool_call_validation_uses_frozen_surface_before_live_capabilities() {
        let registry = registry();
        let binding = RunBinding::local_developer("/repo", &registry);
        let server_only_surface = CapabilityResolver.available_tool_surface_for_providers(
            &registry,
            &binding.capabilities,
            &server_providers(&registry),
        );

        assert_eq!(
            CapabilityResolver.check_tool_call_for_surface(
                &registry,
                "bash",
                &serde_json::json!({"command": "pwd"}),
                &binding.capabilities,
                &server_only_surface,
            ),
            Err(ToolUnavailableReason::ExecutorUnavailable(
                "tool_not_selected_by_current_provider_surface".to_string()
            )),
            "runtime capability alone must not route a tool that was absent from the selected provider surface"
        );
    }

    #[test]
    fn selected_runtime_offer_is_rechecked_against_execution_time_capabilities() {
        let registry = registry();
        let binding = RunBinding::local_developer("/repo", &registry);
        assert!(binding.tool_surface.contains("bash"));

        let mut offline_capabilities = binding.capabilities;
        offline_capabilities.executor.reachable = false;
        offline_capabilities.executor.runtime_executor = false;

        assert_eq!(
            CapabilityResolver.check_tool_call_for_surface(
                &registry,
                "bash",
                &serde_json::json!({"command": "pwd"}),
                &offline_capabilities,
                &binding.tool_surface,
            ),
            Err(ToolUnavailableReason::ExecutorUnavailable(
                "runtime_executor_required".to_string()
            )),
            "a selected provider offer must still fail closed if its executor goes offline before execution"
        );
    }

    #[test]
    fn schema_filter_keeps_local_service_and_project_tools_for_local_cli() {
        let registry = registry();
        let binding = RunBinding::local_developer("/repo", &registry);
        let schemas = vec![
            serde_json::json!({"type": "function", "function": {"name": "tool_search"}}),
            serde_json::json!({"type": "function", "function": {"name": "memory"}}),
            serde_json::json!({"type": "function", "function": {"name": "web_fetch"}}),
            serde_json::json!({"type": "function", "function": {"name": "web_search"}}),
            serde_json::json!({"type": "function", "function": {"name": "bash"}}),
            serde_json::json!({"type": "function", "function": {"name": "read_file"}}),
            serde_json::json!({"type": "function", "function": {"name": "not_registered"}}),
        ];

        let names: Vec<String> = CapabilityResolver
            .filter_tool_schemas_for_providers(
                &registry,
                schemas,
                &binding.capabilities,
                &local_cli_providers(&registry),
            )
            .into_iter()
            .filter_map(|schema| tool_schema_name(&schema).map(str::to_string))
            .collect();

        for expected in [
            "tool_search",
            "memory",
            "web_fetch",
            "web_search",
            "bash",
            "read_file",
        ] {
            assert!(
                names.iter().any(|name| name == expected),
                "{expected} should be visible for a local CLI runtime: {names:?}"
            );
        }
        assert!(!names.iter().any(|name| name == "not_registered"));
    }

    #[test]
    fn schema_filter_hides_internal_runtime_tool_schemas_even_when_capable() {
        let registry = registry();
        let binding = RunBinding::local_developer("/repo", &registry);
        let schemas = vec![
            serde_json::json!({"type": "function", "function": {"name": "read_file"}}),
            serde_json::json!({"type": "function", "function": {"name": "delete_file"}}),
            serde_json::json!({"type": "function", "function": {"name": "multi_edit"}}),
            serde_json::json!({"type": "function", "function": {"name": "git_clone"}}),
            serde_json::json!({"type": "function", "function": {"name": "find_definition"}}),
        ];

        let names: Vec<String> = CapabilityResolver
            .filter_tool_schemas_for_providers(
                &registry,
                schemas,
                &binding.capabilities,
                &local_cli_providers(&registry),
            )
            .into_iter()
            .filter_map(|schema| tool_schema_name(&schema).map(str::to_string))
            .collect();

        assert_eq!(names, vec!["read_file".to_string()]);
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
            .filter_tool_schemas_for_providers(
                &registry,
                schemas,
                &binding.capabilities,
                &server_providers(&registry),
            )
            .into_iter()
            .filter_map(|schema| tool_schema_name(&schema).map(str::to_string))
            .collect();

        assert!(names.iter().any(|name| name == "tool_search"));
        assert!(!names.iter().any(|name| name == "bash"));
        assert!(!names.iter().any(|name| name == "read_file"));
    }

    #[test]
    fn binding_schema_filter_hides_control_plane_tools_without_control_plane_binding() {
        let registry = registry();
        let binding = RunBinding::edge_developer("/repo", &registry);
        let schemas = vec![
            serde_json::json!({"type": "function", "function": {"name": "task"}}),
            serde_json::json!({"type": "function", "function": {"name": "introspect"}}),
            serde_json::json!({"type": "function", "function": {"name": "reflect"}}),
            serde_json::json!({"type": "function", "function": {"name": "agent_fanout"}}),
            serde_json::json!({"type": "function", "function": {"name": "memory"}}),
            serde_json::json!({"type": "function", "function": {"name": "read_file"}}),
            serde_json::json!({"type": "function", "function": {"name": "bash"}}),
        ];

        let names: Vec<String> = CapabilityResolver
            .filter_tool_schemas_for_binding(&registry, schemas, &binding)
            .into_iter()
            .filter_map(|schema| tool_schema_name(&schema).map(str::to_string))
            .collect();

        assert!(names.iter().any(|name| name == "read_file"));
        assert!(names.iter().any(|name| name == "bash"));
        for hidden in ["task", "introspect", "reflect", "agent_fanout", "memory"] {
            assert!(
                !names.iter().any(|name| name == hidden),
                "{hidden} must not be prompt-visible without a matching provider binding: {names:?}"
            );
        }
    }

    #[test]
    fn provider_aware_schema_filter_requires_provider_declaration() {
        let registry = registry();
        let binding = RunBinding::local_developer("/repo", &registry);
        let schemas = vec![
            serde_json::json!({"type": "function", "function": {"name": "tool_search"}}),
            serde_json::json!({"type": "function", "function": {"name": "bash"}}),
            serde_json::json!({"type": "function", "function": {"name": "read_file"}}),
        ];
        let server_only = vec![
            crate::server_service_provider("server", &registry),
            crate::control_plane_provider("control", &registry),
        ];

        let names: Vec<String> = CapabilityResolver
            .filter_tool_schemas_for_providers(
                &registry,
                schemas.clone(),
                &binding.capabilities,
                &server_only,
            )
            .into_iter()
            .filter_map(|schema| tool_schema_name(&schema).map(str::to_string))
            .collect();

        assert!(names.iter().any(|name| name == "tool_search"));
        assert!(!names.iter().any(|name| name == "bash"));
        assert!(!names.iter().any(|name| name == "read_file"));

        let with_cli = vec![
            crate::server_service_provider("server", &registry),
            crate::control_plane_provider("control", &registry),
            crate::cli_local_provider("cli", &registry),
        ];
        let names: Vec<String> = CapabilityResolver
            .filter_tool_schemas_for_providers(&registry, schemas, &binding.capabilities, &with_cli)
            .into_iter()
            .filter_map(|schema| tool_schema_name(&schema).map(str::to_string))
            .collect();

        assert!(names.iter().any(|name| name == "tool_search"));
        assert!(names.iter().any(|name| name == "bash"));
        assert!(names.iter().any(|name| name == "read_file"));
    }

    #[test]
    fn provider_aware_tool_surface_requires_provider_declaration() {
        let registry = registry();
        let binding = RunBinding::local_developer("/repo", &registry);
        let server_only = vec![
            crate::server_service_provider("server", &registry),
            crate::control_plane_provider("control", &registry),
        ];

        let surface = CapabilityResolver.available_tool_surface_for_providers(
            &registry,
            &binding.capabilities,
            &server_only,
        );

        assert!(surface.contains("tool_search"));
        assert!(surface.contains("ask_user"));
        assert!(!surface.contains("bash"));
        assert!(!surface.contains("read_file"));
        assert_eq!(surface.denial_for("bash"), None);
        assert_eq!(surface.denial_for("read_file"), None);
    }

    #[test]
    fn local_cli_shared_network_offer_overrides_server_without_prompt_schema_churn() {
        let registry = registry();
        let binding = RunBinding::local_developer("/repo", &registry);
        let admission = binding
            .tool_surface
            .admission_for("web_fetch")
            .expect("web_fetch admission");

        assert!(admission.visible);
        assert_eq!(
            admission.selected_provider_type,
            Some(crate::CapacityProviderType::CliLocal)
        );
        assert_eq!(admission.selected_provider_id.as_deref(), Some("local-cli"));
        assert_eq!(
            admission.selected_offer_id.as_deref(),
            Some("web_fetch@local-cli")
        );
        assert!(admission.candidates.iter().any(|candidate| {
            candidate.provider_type == crate::CapacityProviderType::ServerService
                && candidate.reason == ToolOfferCandidateReason::LowerPriority
        }));

        let schema = serde_json::json!({
            "type": "function",
            "function": {
                "name": "web_fetch",
                "description": "Fetch a URL"
            }
        });
        let providers = local_cli_providers(&registry);
        let mut reversed_providers = providers.clone();
        reversed_providers.reverse();
        let filtered = CapabilityResolver.filter_tool_schemas_for_providers(
            &registry,
            vec![schema.clone()],
            &binding.capabilities,
            &providers,
        );
        let filtered_reversed = CapabilityResolver.filter_tool_schemas_for_providers(
            &registry,
            vec![schema.clone()],
            &binding.capabilities,
            &reversed_providers,
        );

        assert_eq!(filtered, vec![schema.clone()]);
        assert_eq!(
            filtered_reversed,
            vec![schema],
            "provider route selection must not rewrite prompt-visible schemas"
        );
    }

    #[test]
    fn cloud_control_plane_shared_network_offer_uses_server_service() {
        let registry = registry();
        let binding = RunBinding::cloud_control_plane(&registry);
        let admission = binding
            .tool_surface
            .admission_for("web_fetch")
            .expect("web_fetch admission");

        assert!(admission.visible);
        assert_eq!(
            admission.selected_provider_type,
            Some(crate::CapacityProviderType::ServerService)
        );
        assert_eq!(
            admission.selected_offer_id.as_deref(),
            Some("web_fetch@server-service")
        );
    }

    #[test]
    fn edge_shared_network_offer_uses_edge_not_server_fallback() {
        let registry = registry();
        let binding = RunBinding::edge_developer("/repo", &registry);
        let admission = binding
            .tool_surface
            .admission_for("web_fetch")
            .expect("web_fetch admission");

        assert!(admission.visible);
        assert_eq!(
            admission.selected_provider_type,
            Some(crate::CapacityProviderType::EdgeCapacity)
        );
        assert_eq!(
            admission.selected_provider_id.as_deref(),
            Some("edge-agent")
        );
        assert_eq!(admission.candidates.len(), 1);
    }

    #[test]
    fn provider_schema_conflict_hides_surface() {
        let registry = registry();
        let binding = RunBinding::local_developer("/repo", &registry);
        let providers = vec![
            crate::server_service_provider("server", &registry)
                .with_tool_schema_digest("web_fetch", "sha256:server-contract"),
            crate::cli_local_provider("cli", &registry)
                .with_tool_schema_digest("web_fetch", "sha256:cli-contract"),
        ];

        let surface = CapabilityResolver.available_tool_surface_for_providers(
            &registry,
            &binding.capabilities,
            &providers,
        );
        let admission = surface
            .admission_for("web_fetch")
            .expect("web_fetch admission");

        assert!(!surface.contains("web_fetch"));
        assert!(!admission.visible);
        assert_eq!(
            surface.denial_for("web_fetch"),
            Some(&ToolUnavailableReason::PolicyDenied(
                "provider_schema_conflict".to_string()
            ))
        );
        assert!(admission.selected_offer_id.is_none());
        assert!(admission.candidates.iter().all(|candidate| candidate.reason
            == ToolOfferCandidateReason::SchemaConflict
            && candidate.schema_digest.is_some()));
    }

    #[test]
    fn duplicate_builtin_provider_at_same_rank_is_ambiguous() {
        let registry = registry();
        let binding = RunBinding::cloud_control_plane(&registry);
        let providers = vec![
            crate::server_service_provider("server-a", &registry),
            crate::server_service_provider("server-b", &registry),
        ];

        let surface = CapabilityResolver.available_tool_surface_for_providers(
            &registry,
            &binding.capabilities,
            &providers,
        );
        let admission = surface
            .admission_for("web_fetch")
            .expect("web_fetch admission");

        assert!(!surface.contains("web_fetch"));
        assert!(!admission.visible);
        assert_eq!(
            surface.denial_for("web_fetch"),
            Some(&ToolUnavailableReason::PolicyDenied(
                "provider_owner_ambiguous".to_string()
            ))
        );
        assert!(
            admission
                .candidates
                .iter()
                .all(|candidate| candidate.reason == ToolOfferCandidateReason::AmbiguousProvider)
        );
    }

    #[test]
    fn duplicate_provider_id_hides_every_offer_from_that_identity() {
        let registry = registry();
        let providers = vec![
            crate::mcp_provider("weather", ["mcp__weather__query".to_string()]),
            crate::mcp_provider("weather", ["mcp__weather__forecast".to_string()]),
        ];
        let binding = RunBinding::resolve_with_provider_declarations(
            WorkspaceBinding::none(),
            ExecutorBinding {
                kind: crate::ExecutorBindingKind::Mcp,
                executor_id: "weather".to_string(),
                display_name: "Weather MCP".to_string(),
                transport: crate::ToolTransportKind::McpHttp,
                status: crate::ExecutorStatus::Online,
            },
            RuntimeBinding::none(),
            PolicyIntent::cloud_control_plane(),
            &registry,
            &providers,
        );

        for tool_name in ["mcp__weather__query", "mcp__weather__forecast"] {
            let admission = binding
                .tool_surface
                .admission_for(tool_name)
                .expect("duplicate-id MCP admission");
            assert!(!binding.tool_surface.contains(tool_name));
            assert!(!admission.visible);
            assert!(admission.selected_offer_id.is_none());
            assert_eq!(
                binding.tool_surface.denial_for(tool_name),
                Some(&ToolUnavailableReason::PolicyDenied(
                    "provider_id_conflict".to_string()
                ))
            );
            assert!(
                admission.candidates.iter().all(|candidate| {
                    !candidate.selected
                        && candidate.reason == ToolOfferCandidateReason::ProviderIdConflict
                }),
                "all candidates from a colliding provider id must be non-selected"
            );
        }
    }

    #[test]
    fn duplicate_provider_id_filters_prompt_schemas() {
        let registry = registry();
        let binding = RunBinding::resolve(
            WorkspaceBinding::none(),
            ExecutorBinding {
                kind: crate::ExecutorBindingKind::Mcp,
                executor_id: "weather".to_string(),
                display_name: "Weather MCP".to_string(),
                transport: crate::ToolTransportKind::McpHttp,
                status: crate::ExecutorStatus::Online,
            },
            RuntimeBinding::none(),
            PolicyIntent::cloud_control_plane(),
            &registry,
        );
        let schemas = vec![
            serde_json::json!({"type": "function", "function": {"name": "mcp__weather__query"}}),
            serde_json::json!({"type": "function", "function": {"name": "mcp__weather__forecast"}}),
        ];
        let providers = vec![
            crate::mcp_provider("weather", ["mcp__weather__query".to_string()]),
            crate::mcp_provider("weather", ["mcp__weather__forecast".to_string()]),
        ];

        let filtered = CapabilityResolver.filter_tool_schemas_for_providers(
            &registry,
            schemas,
            &binding.capabilities,
            &providers,
        );

        assert!(
            filtered.is_empty(),
            "prompt-visible schemas must not keep tools whose offer ids collide"
        );
    }

    #[test]
    fn provider_type_mismatch_is_a_diagnosed_hidden_offer_not_silent_visibility() {
        let registry = registry();
        let binding = RunBinding::local_developer("/repo", &registry);
        let providers = vec![crate::CapacityProviderDeclaration::new(
            crate::CapacityProviderType::ServerService,
            "wrong-owner",
            ["bash".to_string()],
        )];

        let surface = CapabilityResolver.available_tool_surface_for_providers(
            &registry,
            &binding.capabilities,
            &providers,
        );
        let admission = surface.admission_for("bash").expect("bash admission");

        assert!(!surface.contains("bash"));
        assert_eq!(
            surface.denial_for("bash"),
            Some(&ToolUnavailableReason::ExecutorUnavailable(
                "no_matching_provider_offer".to_string()
            ))
        );
        assert!(!admission.visible);
        assert_eq!(
            admission.hidden_reason,
            Some(ToolUnavailableReason::ExecutorUnavailable(
                "no_matching_provider_offer".to_string()
            ))
        );
        assert_eq!(
            admission.candidates[0].reason,
            ToolOfferCandidateReason::ProviderTypeMismatch
        );
    }

    #[test]
    fn request_scoped_mcp_schema_filter_requires_exact_provider_binding() {
        let registry = registry();
        let binding = RunBinding::resolve(
            WorkspaceBinding::none(),
            ExecutorBinding {
                kind: crate::ExecutorBindingKind::Mcp,
                executor_id: "mcp".to_string(),
                display_name: "MCP".to_string(),
                transport: crate::ToolTransportKind::McpHttp,
                status: crate::ExecutorStatus::Online,
            },
            RuntimeBinding::none(),
            PolicyIntent::cloud_control_plane(),
            &registry,
        );
        let schemas = vec![
            serde_json::json!({"type": "function", "function": {"name": "mcp__weather"}}),
            serde_json::json!({"type": "function", "function": {"name": "mcp__calculator"}}),
        ];
        let providers = vec![crate::request_scoped_mcp_provider(
            "mcp",
            ["mcp__weather".to_string()],
        )];

        let names: Vec<String> = CapabilityResolver
            .filter_tool_schemas_for_providers(
                &registry,
                schemas,
                &binding.capabilities,
                &providers,
            )
            .into_iter()
            .filter_map(|schema| tool_schema_name(&schema).map(str::to_string))
            .collect();

        assert_eq!(names, vec!["mcp__weather".to_string()]);
    }

    #[test]
    fn request_scoped_mcp_surface_includes_provider_bound_dynamic_tool() {
        let registry = registry();
        let providers = vec![crate::request_scoped_mcp_provider(
            "mcp",
            ["mcp__weather".to_string()],
        )];
        let binding = RunBinding::resolve_with_provider_declarations(
            WorkspaceBinding::none(),
            ExecutorBinding {
                kind: crate::ExecutorBindingKind::Mcp,
                executor_id: "mcp".to_string(),
                display_name: "MCP".to_string(),
                transport: crate::ToolTransportKind::McpHttp,
                status: crate::ExecutorStatus::Online,
            },
            RuntimeBinding::none(),
            PolicyIntent::cloud_control_plane(),
            &registry,
            &providers,
        );
        let admission = binding
            .tool_surface
            .admission_for("mcp__weather")
            .expect("dynamic MCP admission");

        assert!(binding.tool_surface.contains("mcp__weather"));
        assert!(admission.visible);
        assert_eq!(
            admission.selected_provider_type,
            Some(crate::CapacityProviderType::RequestScopedMcp)
        );
        assert_eq!(
            admission.selected_offer_id.as_deref(),
            Some("mcp__weather@mcp")
        );
    }

    #[test]
    fn generic_mcp_provider_surface_includes_provider_bound_dynamic_tool() {
        let registry = registry();
        let providers = vec![crate::mcp_provider(
            "server-mcp",
            ["mcp__github__search".to_string()],
        )];
        let binding = RunBinding::resolve_with_provider_declarations(
            WorkspaceBinding::none(),
            ExecutorBinding {
                kind: crate::ExecutorBindingKind::Mcp,
                executor_id: "server-mcp".to_string(),
                display_name: "Server MCP".to_string(),
                transport: crate::ToolTransportKind::McpHttp,
                status: crate::ExecutorStatus::Online,
            },
            RuntimeBinding::none(),
            PolicyIntent::cloud_control_plane(),
            &registry,
            &providers,
        );
        let admission = binding
            .tool_surface
            .admission_for("mcp__github__search")
            .expect("dynamic MCP admission");

        assert!(binding.tool_surface.contains("mcp__github__search"));
        assert!(admission.visible);
        assert_eq!(
            admission.selected_provider_type,
            Some(crate::CapacityProviderType::McpProvider)
        );
        assert_eq!(
            admission.selected_offer_id.as_deref(),
            Some("mcp__github__search@server-mcp")
        );
    }

    #[test]
    fn duplicate_mcp_provider_tool_with_different_schema_digest_is_hidden() {
        let registry = registry();
        let providers = vec![
            crate::mcp_provider("weather-a", ["mcp__weather".to_string()])
                .with_tool_schema_digest("mcp__weather", "sha256:a"),
            crate::mcp_provider("weather-b", ["mcp__weather".to_string()])
                .with_tool_schema_digest("mcp__weather", "sha256:b"),
        ];
        let binding = RunBinding::resolve_with_provider_declarations(
            WorkspaceBinding::none(),
            ExecutorBinding {
                kind: crate::ExecutorBindingKind::Mcp,
                executor_id: "weather-a".to_string(),
                display_name: "Weather MCP".to_string(),
                transport: crate::ToolTransportKind::McpHttp,
                status: crate::ExecutorStatus::Online,
            },
            RuntimeBinding::none(),
            PolicyIntent::cloud_control_plane(),
            &registry,
            &providers,
        );
        let admission = binding
            .tool_surface
            .admission_for("mcp__weather")
            .expect("dynamic MCP admission");

        assert!(!binding.tool_surface.contains("mcp__weather"));
        assert!(!admission.visible);
        assert_eq!(
            binding.tool_surface.denial_for("mcp__weather"),
            Some(&ToolUnavailableReason::PolicyDenied(
                "provider_schema_conflict".to_string()
            ))
        );
        assert!(
            admission
                .candidates
                .iter()
                .all(|candidate| candidate.reason == ToolOfferCandidateReason::SchemaConflict)
        );
    }

    #[test]
    fn duplicate_mcp_provider_tool_without_digest_is_hidden_from_surface() {
        let registry = registry();
        let providers = vec![
            crate::mcp_provider("weather-a", ["mcp__weather".to_string()]),
            crate::mcp_provider("weather-b", ["mcp__weather".to_string()]),
        ];
        let binding = RunBinding::resolve_with_provider_declarations(
            WorkspaceBinding::none(),
            ExecutorBinding {
                kind: crate::ExecutorBindingKind::Mcp,
                executor_id: "weather-a".to_string(),
                display_name: "Weather MCP".to_string(),
                transport: crate::ToolTransportKind::McpHttp,
                status: crate::ExecutorStatus::Online,
            },
            RuntimeBinding::none(),
            PolicyIntent::cloud_control_plane(),
            &registry,
            &providers,
        );
        let admission = binding
            .tool_surface
            .admission_for("mcp__weather")
            .expect("dynamic MCP admission");

        assert!(!binding.tool_surface.contains("mcp__weather"));
        assert!(!admission.visible);
        assert_eq!(
            binding.tool_surface.denial_for("mcp__weather"),
            Some(&ToolUnavailableReason::PolicyDenied(
                "provider_tool_name_conflict".to_string()
            ))
        );
        assert!(
            admission
                .candidates
                .iter()
                .all(|candidate| candidate.reason == ToolOfferCandidateReason::ToolNameConflict)
        );
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
            .filter_tool_schemas_for_providers(
                &registry,
                schemas,
                &binding.capabilities,
                &local_cli_providers(&registry),
            )
            .into_iter()
            .filter_map(|schema| tool_schema_name(&schema).map(str::to_string))
            .collect();

        // Only the single valid `bash` schema should remain — no duplicates.
        assert_eq!(names, vec!["bash".to_string()]);
    }

    #[test]
    fn filter_tool_schemas_fails_closed_for_same_name_schema_conflict() {
        let registry = registry();
        let binding = RunBinding::local_developer("/repo", &registry);
        let schemas = vec![
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": "bash",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "cmd": { "type": "string" }
                        }
                    }
                }
            }),
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": "bash",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "script": { "type": "string" }
                        }
                    }
                }
            }),
            serde_json::json!({"type": "function", "function": {"name": "read_file"}}),
        ];

        let names: Vec<String> = CapabilityResolver
            .filter_tool_schemas_for_providers(
                &registry,
                schemas,
                &binding.capabilities,
                &local_cli_providers(&registry),
            )
            .into_iter()
            .filter_map(|schema| tool_schema_name(&schema).map(str::to_string))
            .collect();

        assert_eq!(
            names,
            vec!["read_file".to_string()],
            "same-name schemas with different prompt contracts must fail closed"
        );
    }

    #[test]
    fn filter_tool_schemas_dedupes_same_name_equivalent_schema_without_prompt_churn() {
        let registry = registry();
        let binding = RunBinding::local_developer("/repo", &registry);
        let schemas = vec![
            serde_json::json!({"function": {"name": "bash"}}),
            serde_json::json!({"type": "function", "function": {"name": "bash"}}),
        ];

        let filtered = CapabilityResolver.filter_tool_schemas_for_providers(
            &registry,
            schemas.clone(),
            &binding.capabilities,
            &local_cli_providers(&registry),
        );
        let names: Vec<String> = filtered
            .iter()
            .filter_map(|schema| tool_schema_name(schema).map(str::to_string))
            .collect();

        assert_eq!(names, vec!["bash".to_string()]);
        assert_eq!(
            filtered,
            vec![schemas[0].clone()],
            "dedupe must keep the first canonical schema bytes stable"
        );
    }

    #[test]
    fn filter_tool_schemas_for_providers_is_byte_stable_for_permuted_dynamic_schemas() {
        let registry = registry();
        let binding = RunBinding::resolve(
            WorkspaceBinding::none(),
            ExecutorBinding {
                kind: crate::ExecutorBindingKind::Mcp,
                executor_id: "mcp".to_string(),
                display_name: "MCP".to_string(),
                transport: crate::ToolTransportKind::McpHttp,
                status: crate::ExecutorStatus::Online,
            },
            RuntimeBinding::none(),
            PolicyIntent::cloud_control_plane(),
            &registry,
        );
        let provider = crate::request_scoped_mcp_provider(
            "mcp",
            [
                "mcp__alpha__query".to_string(),
                "mcp__zeta__query".to_string(),
            ],
        );
        let alpha = serde_json::json!({
            "type": "function",
            "function": {
                "name": "mcp__alpha__query",
                "description": "Alpha query"
            }
        });
        let zeta = serde_json::json!({
            "type": "function",
            "function": {
                "name": "mcp__zeta__query",
                "description": "Zeta query"
            }
        });

        let first = CapabilityResolver.filter_tool_schemas_for_providers(
            &registry,
            vec![zeta.clone(), alpha.clone()],
            &binding.capabilities,
            std::slice::from_ref(&provider),
        );
        let second = CapabilityResolver.filter_tool_schemas_for_providers(
            &registry,
            vec![alpha.clone(), zeta.clone()],
            &binding.capabilities,
            &[provider],
        );

        assert_eq!(
            first,
            vec![alpha, zeta],
            "provider-aware dynamic schemas must be ordered by canonical tool name"
        );
        assert_eq!(
            serde_json::to_vec(&first).expect("serialize first schemas"),
            serde_json::to_vec(&second).expect("serialize second schemas"),
            "provider-aware schema filtering must not depend on MCP discovery order"
        );
    }

    #[test]
    fn filter_tool_schemas_fails_closed_for_same_tool_provider_digest_conflict() {
        let registry = registry();
        let binding = RunBinding::resolve(
            WorkspaceBinding::none(),
            ExecutorBinding {
                kind: crate::ExecutorBindingKind::Mcp,
                executor_id: "mcp".to_string(),
                display_name: "MCP".to_string(),
                transport: crate::ToolTransportKind::McpHttp,
                status: crate::ExecutorStatus::Online,
            },
            RuntimeBinding::none(),
            PolicyIntent::cloud_control_plane(),
            &registry,
        );
        let schemas = vec![serde_json::json!({
            "type": "function",
            "function": {"name": "mcp__weather__query"}
        })];
        let providers = vec![
            crate::request_scoped_mcp_provider("mcp-a", ["mcp__weather__query".to_string()]),
            crate::request_scoped_mcp_provider("mcp-b", ["mcp__weather__query".to_string()]),
        ];

        let names: Vec<String> = CapabilityResolver
            .filter_tool_schemas_for_providers(
                &registry,
                schemas,
                &binding.capabilities,
                &providers,
            )
            .into_iter()
            .filter_map(|schema| tool_schema_name(&schema).map(str::to_string))
            .collect();

        // Same tool from different MCP providers with different schemas should be HIDDEN (not deduplicated)
        // to prevent malicious provider from replacing a legitimate tool path.
        assert_eq!(
            names,
            Vec::<String>::new(),
            "same canonical tool from different MCP providers should be hidden"
        );
    }

    #[test]
    fn filter_tool_schemas_accepts_missing_type_when_not_duplicate() {
        let registry = registry();
        let binding = RunBinding::local_developer("/repo", &registry);
        let schemas = vec![serde_json::json!({"function": {"name": "read_file"}})];

        let names: Vec<String> = CapabilityResolver
            .filter_tool_schemas_for_providers(
                &registry,
                schemas,
                &binding.capabilities,
                &local_cli_providers(&registry),
            )
            .into_iter()
            .filter_map(|schema| tool_schema_name(&schema).map(str::to_string))
            .collect();

        assert_eq!(names, vec!["read_file".to_string()]);
    }
}
