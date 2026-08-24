use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{
    RequiredExecutor, RuntimePlatform, ToolRegistry, ToolSpec, WorkspaceAuthority, tool_schema_name,
};

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum CapacityProviderType {
    ServerService,
    ControlPlane,
    RequestScopedMcp,
    McpProvider,
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
            Self::McpProvider => "mcp_provider",
            Self::EdgeCapacity => "edge_capacity",
            Self::Sandbox => "sandbox",
            Self::OrchestratorManagedRuntime => "orchestrator_managed_runtime",
            Self::CliLocal => "cli_local",
            Self::Unknown => "unknown",
        }
    }

    pub const fn is_runtime_executor(self) -> bool {
        matches!(
            self,
            Self::EdgeCapacity | Self::Sandbox | Self::OrchestratorManagedRuntime | Self::CliLocal
        )
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
#[serde(try_from = "CapacityProviderDeclarationWire")]
pub struct CapacityProviderDeclaration {
    pub provider_type: CapacityProviderType,
    pub provider_id: String,
    #[serde(default)]
    pub tool_names: BTreeSet<String>,
    #[serde(default)]
    pub tool_schema_digests: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CapacityProviderDeclarationWire {
    provider_type: CapacityProviderType,
    provider_id: String,
    #[serde(default)]
    tool_names: BTreeSet<String>,
    #[serde(default)]
    tool_schema_digests: BTreeMap<String, String>,
}

impl TryFrom<CapacityProviderDeclarationWire> for CapacityProviderDeclaration {
    type Error = String;

    fn try_from(raw: CapacityProviderDeclarationWire) -> Result<Self, Self::Error> {
        let provider_id = validate_provider_id(raw.provider_id)?;
        let tool_names: BTreeSet<String> = raw
            .tool_names
            .into_iter()
            .map(|tool_name| validate_declared_tool_name_for_provider(raw.provider_type, tool_name))
            .collect::<Result<_, _>>()?;

        for tool_name in raw.tool_schema_digests.keys() {
            validate_declared_tool_name_for_provider(raw.provider_type, tool_name.clone())?;
            if !tool_names.contains(tool_name) {
                return Err(format!(
                    "provider schema digest key must reference a declared tool name: {tool_name}"
                ));
            }
        }

        let tool_schema_digests = tool_names
            .iter()
            .map(|tool_name| {
                let digest = raw
                    .tool_schema_digests
                    .get(tool_name)
                    .cloned()
                    .unwrap_or_else(|| canonical_tool_name_digest(tool_name));
                (tool_name.clone(), digest)
            })
            .collect();

        Ok(Self {
            provider_type: raw.provider_type,
            provider_id,
            tool_names,
            tool_schema_digests,
        })
    }
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
    }
}

impl CapacityProviderDeclaration {
    /// Construct a declaration. Panics via [`canonical_provider_id`] /
    /// [`canonical_declared_tool_name_for_provider`] if the provider id or any
    /// tool name is invalid — those helpers are the single source of truth for
    /// what a well-formed identifier looks like.
    pub fn new(
        provider_type: CapacityProviderType,
        provider_id: impl Into<String>,
        tool_names: impl IntoIterator<Item = String>,
    ) -> Self {
        let provider_id = canonical_provider_id(provider_id);
        let tool_names: BTreeSet<String> = tool_names
            .into_iter()
            .map(|tool_name| canonical_declared_tool_name_for_provider(provider_type, tool_name))
            .collect();
        let tool_schema_digests = tool_names
            .iter()
            .map(|name| (name.clone(), canonical_tool_name_digest(name)))
            .collect();
        Self {
            provider_type,
            provider_id,
            tool_names,
            tool_schema_digests,
        }
    }

    /// Construct a declaration from a ToolRegistry, filtering by predicate.
    /// Same panic contract as [`Self::new`].
    pub fn from_registry(
        provider_type: CapacityProviderType,
        provider_id: impl Into<String>,
        registry: &ToolRegistry,
        mut predicate: impl FnMut(&ToolSpec) -> bool,
    ) -> Self {
        let provider_id = canonical_provider_id(provider_id);
        let mut declaration = Self {
            provider_type,
            provider_id,
            tool_names: BTreeSet::new(),
            tool_schema_digests: BTreeMap::new(),
        };
        for spec in registry.iter().filter(|spec| predicate(spec)) {
            let tool_name =
                canonical_declared_tool_name_for_provider(provider_type, spec.name.clone());
            declaration.tool_names.insert(tool_name.clone());
            declaration
                .tool_schema_digests
                .insert(tool_name, canonical_tool_spec_digest(spec));
        }
        declaration
    }

    pub fn with_tool_schema_digest(
        mut self,
        tool_name: impl Into<String>,
        schema_digest: impl Into<String>,
    ) -> Self {
        let tool_name = canonical_declared_tool_name_for_provider(self.provider_type, tool_name);
        self.tool_names.insert(tool_name.clone());
        self.tool_schema_digests
            .insert(tool_name, schema_digest.into());
        self
    }

    pub fn schema_digest_for_tool(&self, tool_name: &str) -> Option<&str> {
        if self.tool_names.contains(tool_name) {
            return self.tool_schema_digests.get(tool_name).map(String::as_str);
        }
        None
    }
}

pub fn canonical_tool_spec_digest(spec: &ToolSpec) -> String {
    let value = serde_json::to_value(spec).expect("ToolSpec must serialize");
    canonical_sha256_digest("tool_spec", &value)
}

pub fn canonical_tool_schema_digest(schema: &Value) -> String {
    canonical_sha256_digest("tool_schema", schema)
}

pub fn tool_offer_id(tool_name: &str, provider_id: &str) -> String {
    astra_core::tool_offer::tool_offer_id(tool_name, provider_id)
}

pub fn is_valid_tool_offer_id(value: &str) -> bool {
    astra_core::tool_offer::is_valid_tool_offer_id(value)
}

pub fn is_valid_provider_id(value: &str) -> bool {
    astra_core::tool_offer::is_valid_provider_id(value)
}

pub fn is_mcp_namespaced_tool_name(value: &str) -> bool {
    astra_core::tool_offer::is_mcp_namespaced_tool_name(value)
}

pub fn is_valid_tool_offer_tool_name(value: &str) -> bool {
    astra_core::tool_offer::is_valid_tool_offer_tool_name(value)
}

fn canonical_tool_name_digest(tool_name: &str) -> String {
    canonical_sha256_digest("tool_name", &serde_json::json!({ "name": tool_name }))
}

fn canonical_sha256_digest(kind: &str, value: &Value) -> String {
    let canonical = canonical_json_value(value);
    let bytes = serde_json::to_vec(&canonical).expect("canonical JSON must serialize");
    let mut hasher = Sha256::new();
    hasher.update(kind.as_bytes());
    hasher.update(b"\0");
    hasher.update(bytes);
    let digest = hasher.finalize();
    format!("sha256:{digest:x}")
}

fn canonical_json_value(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(canonical_json_value).collect()),
        Value::Object(map) => {
            let mut sorted = serde_json::Map::new();
            let mut keys: Vec<_> = map.keys().collect();
            keys.sort();
            for key in keys {
                sorted.insert(key.clone(), canonical_json_value(&map[key]));
            }
            Value::Object(sorted)
        }
        other => other.clone(),
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
            provider_id: canonical_provider_id(provider_id),
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
            provider_id: canonical_provider_id(provider_id),
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
pub const CAP_TASK_BOARD: &str = "task_board";
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
    labels([CAP_WEB_FETCH, CAP_WEB_SEARCH, CAP_MEMORY])
}

pub fn control_plane_capabilities(extra: impl IntoIterator<Item = &'static str>) -> Vec<String> {
    let mut capabilities = labels([
        CAP_SESSION,
        CAP_TASK_BOARD,
        CAP_PLAN,
        CAP_POLICY,
        CAP_AUDIT,
        CAP_REFLECT,
        CAP_INTROSPECT,
        CAP_TOOL_SEARCH,
    ]);
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
            spec.load_policy != crate::ToolLoadPolicy::RequestScoped
                && matches!(
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
        |spec| {
            spec.load_policy != crate::ToolLoadPolicy::RequestScoped
                && matches!(spec.required.executor, RequiredExecutor::ControlPlane)
        },
    )
}

pub fn runtime_workspace_provider(
    provider_type: CapacityProviderType,
    provider_id: impl Into<String>,
    registry: &ToolRegistry,
    platform: RuntimePlatform,
) -> CapacityProviderDeclaration {
    CapacityProviderDeclaration::from_registry(provider_type, provider_id, registry, |spec| {
        spec.load_policy != crate::ToolLoadPolicy::RequestScoped
            && matches!(
                spec.required.executor,
                RequiredExecutor::RuntimeExecutor | RequiredExecutor::ServiceOrRuntimeExecutor
            )
            && runtime_workspace_provider_declares_tool(provider_type, spec.name.as_str(), platform)
    })
}

pub fn cli_local_provider(
    provider_id: impl Into<String>,
    registry: &ToolRegistry,
) -> CapacityProviderDeclaration {
    runtime_workspace_provider(
        CapacityProviderType::CliLocal,
        provider_id,
        registry,
        RuntimePlatform::current(),
    )
}

fn runtime_workspace_provider_declares_tool(
    provider_type: CapacityProviderType,
    tool_name: &str,
    platform: RuntimePlatform,
) -> bool {
    match tool_name {
        // Edge publication is implemented by the request-scoped managed file
        // transfer interceptor, not by the generic Edge tool executor. The
        // server-local and CLI implementations remain ordinary runtime tools.
        "publish_artifact" => !matches!(provider_type, CapacityProviderType::EdgeCapacity),
        // Terminal rendering is an access-surface affordance, not generic
        // workspace executor capacity.
        "display_sixel" => matches!(provider_type, CapacityProviderType::CliLocal),
        // PowerShell is platform capacity. User text such as "Windows" must not
        // expose it; only a provider/runtime that advertises Windows may.
        "powershell" => {
            platform.supports_powershell()
                && matches!(
                    provider_type,
                    CapacityProviderType::CliLocal
                        | CapacityProviderType::EdgeCapacity
                        | CapacityProviderType::Sandbox
                        | CapacityProviderType::OrchestratorManagedRuntime
                )
        }
        _ => true,
    }
}

pub fn request_scoped_mcp_provider(
    provider_id: impl Into<String>,
    tool_names: impl IntoIterator<Item = String>,
) -> CapacityProviderDeclaration {
    let tool_names = tool_names
        .into_iter()
        .map(canonical_request_scoped_mcp_tool_name)
        .collect::<Vec<_>>();
    CapacityProviderDeclaration::new(
        CapacityProviderType::RequestScopedMcp,
        provider_id,
        tool_names,
    )
}

pub fn request_scoped_mcp_provider_from_schemas(
    provider_id: impl Into<String>,
    schemas: &[Value],
) -> CapacityProviderDeclaration {
    mcp_provider_from_schemas_for_type(CapacityProviderType::RequestScopedMcp, provider_id, schemas)
}

pub fn mcp_provider(
    provider_id: impl Into<String>,
    tool_names: impl IntoIterator<Item = String>,
) -> CapacityProviderDeclaration {
    let tool_names = tool_names
        .into_iter()
        .map(canonical_mcp_tool_name)
        .collect::<Vec<_>>();
    CapacityProviderDeclaration::new(CapacityProviderType::McpProvider, provider_id, tool_names)
}

pub fn mcp_provider_from_schemas(
    provider_id: impl Into<String>,
    schemas: &[Value],
) -> CapacityProviderDeclaration {
    mcp_provider_from_schemas_for_type(CapacityProviderType::McpProvider, provider_id, schemas)
}

fn mcp_provider_from_schemas_for_type(
    provider_type: CapacityProviderType,
    provider_id: impl Into<String>,
    schemas: &[Value],
) -> CapacityProviderDeclaration {
    let mut declaration =
        CapacityProviderDeclaration::new(provider_type, provider_id, Vec::<String>::new());
    for schema in schemas {
        let Some(tool_name) = tool_schema_name(schema) else {
            continue;
        };
        if !is_mcp_namespaced_tool_name(tool_name) {
            continue;
        }
        declaration.tool_names.insert(tool_name.to_string());
        declaration
            .tool_schema_digests
            .insert(tool_name.to_string(), canonical_tool_schema_digest(schema));
    }
    declaration
}

fn labels<const N: usize>(values: [&'static str; N]) -> Vec<String> {
    values.into_iter().map(str::to_string).collect()
}

fn canonical_provider_id(provider_id: impl Into<String>) -> String {
    validate_provider_id(provider_id).unwrap_or_else(|message| panic!("{message}"))
}

fn canonical_declared_tool_name_for_provider(
    provider_type: CapacityProviderType,
    tool_name: impl Into<String>,
) -> String {
    validate_declared_tool_name_for_provider(provider_type, tool_name)
        .unwrap_or_else(|message| panic!("{message}"))
}

fn canonical_request_scoped_mcp_tool_name(tool_name: impl Into<String>) -> String {
    canonical_declared_tool_name_for_provider(CapacityProviderType::RequestScopedMcp, tool_name)
}

fn canonical_mcp_tool_name(tool_name: impl Into<String>) -> String {
    canonical_declared_tool_name_for_provider(CapacityProviderType::McpProvider, tool_name)
}

fn validate_provider_id(provider_id: impl Into<String>) -> Result<String, String> {
    let provider_id = provider_id.into();
    if !is_valid_provider_id(&provider_id) {
        return Err(format!("invalid capacity provider id: {provider_id}"));
    }
    Ok(provider_id)
}

fn validate_declared_tool_name_for_provider(
    provider_type: CapacityProviderType,
    tool_name: impl Into<String>,
) -> Result<String, String> {
    let tool_name = tool_name.into();
    if !is_valid_tool_offer_tool_name(&tool_name) {
        return Err(format!("invalid provider-declared tool name: {tool_name}"));
    }
    if matches!(
        provider_type,
        CapacityProviderType::RequestScopedMcp | CapacityProviderType::McpProvider
    ) && !is_mcp_namespaced_tool_name(&tool_name)
    {
        return Err(format!(
            "MCP provider tool name must be namespaced: {tool_name}"
        ));
    }
    Ok(tool_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_type_serializes_as_stable_product_label() {
        let value = serde_json::to_value(CapacityProviderType::OrchestratorManagedRuntime)
            .expect("serialize provider type");
        assert_eq!(value, "orchestrator_managed_runtime");
        let mcp = serde_json::to_value(CapacityProviderType::McpProvider)
            .expect("serialize MCP provider type");
        assert_eq!(mcp, "mcp_provider");
    }

    #[test]
    fn tool_offer_id_uses_provider_identity_without_prompt_route_metadata() {
        assert_eq!(
            tool_offer_id("web_fetch", "server-builtin"),
            "web_fetch@server-builtin"
        );
        assert_eq!(
            tool_offer_id("web_fetch", "edge-macpro"),
            "web_fetch@edge-macpro"
        );
        assert_eq!(
            tool_offer_id("mcp__github__search", "request-scoped-mcp"),
            "mcp__github__search@request-scoped-mcp"
        );
    }

    #[test]
    fn tool_offer_identifiers_reject_ambiguous_or_path_like_values() {
        assert!(is_valid_provider_id("server-builtin"));
        assert!(is_valid_provider_id("edge:macpro.local"));
        assert!(is_valid_tool_offer_id(
            "mcp__github__search@request-scoped-mcp"
        ));
        assert!(is_valid_tool_offer_id("bash@edge:macpro.local"));

        assert!(!is_valid_provider_id("edge@macpro"));
        assert!(!is_valid_provider_id("edge macpro"));
        assert!(!is_valid_provider_id("../edge"));
        assert!(!is_valid_provider_id("..."));
        assert!(!is_valid_tool_offer_id("web_fetch"));
        assert!(!is_valid_tool_offer_id("web_fetch@edge@macpro"));
        assert!(!is_valid_tool_offer_id("web.fetch@server-builtin"));
        assert!(!is_valid_tool_offer_id("web_fetch@edge/macpro"));
    }

    #[test]
    #[should_panic(expected = "invalid capacity provider id")]
    fn provider_declaration_rejects_invalid_provider_id_at_construction() {
        let _ = CapacityProviderDeclaration::new(
            CapacityProviderType::EdgeCapacity,
            "edge@macpro",
            ["bash".to_string()],
        );
    }

    #[test]
    #[should_panic(expected = "invalid provider-declared tool name")]
    fn provider_declaration_rejects_invalid_tool_name_at_construction() {
        let _ = CapacityProviderDeclaration::new(
            CapacityProviderType::ServerService,
            "server-builtin",
            ["web.fetch".to_string()],
        );
    }

    #[test]
    #[should_panic(expected = "invalid capacity provider id")]
    fn provider_coverage_rejects_invalid_provider_id_at_construction() {
        let _ = CapacityProviderCoverageEntry::ready(
            CapacityProviderType::RequestScopedMcp,
            "mcp@github",
            vec!["mcp__github__search".to_string()],
        );
    }

    #[test]
    fn provider_declaration_deserialization_enforces_canonical_ids() {
        let provider: CapacityProviderDeclaration = serde_json::from_value(serde_json::json!({
            "provider_type": "edge_capacity",
            "provider_id": "edge:macpro.local",
            "tool_names": ["bash"]
        }))
        .expect("valid provider declaration should deserialize");

        assert!(provider.declares_tool("bash"));
        assert_eq!(provider.provider_id, "edge:macpro.local");

        let invalid_provider =
            serde_json::from_value::<CapacityProviderDeclaration>(serde_json::json!({
                "provider_type": "edge_capacity",
                "provider_id": "edge@macpro",
                "tool_names": ["bash"]
            }))
            .expect_err("invalid provider id must fail deserialization");
        assert!(
            invalid_provider
                .to_string()
                .contains("invalid capacity provider id")
        );

        let invalid_tool =
            serde_json::from_value::<CapacityProviderDeclaration>(serde_json::json!({
                "provider_type": "server_service",
                "provider_id": "server-builtin",
                "tool_names": ["web.fetch"]
            }))
            .expect_err("invalid tool name must fail deserialization");
        assert!(
            invalid_tool
                .to_string()
                .contains("invalid provider-declared tool name")
        );
    }

    #[test]
    fn provider_declaration_deserialization_rejects_legacy_dynamic_prefixes() {
        let error = serde_json::from_value::<CapacityProviderDeclaration>(serde_json::json!({
            "provider_type": "request_scoped_mcp",
            "provider_id": "mcp",
            "tool_names": [],
            "dynamic_prefixes": ["mcp__"]
        }))
        .expect_err("legacy dynamic_prefixes must fail instead of being silently ignored");

        assert!(
            error
                .to_string()
                .contains("unknown field `dynamic_prefixes`"),
            "legacy dynamic_prefixes must be rejected explicitly: {error}"
        );
    }

    #[test]
    fn request_scoped_mcp_deserialization_rejects_non_namespaced_tools() {
        let error = serde_json::from_value::<CapacityProviderDeclaration>(serde_json::json!({
            "provider_type": "request_scoped_mcp",
            "provider_id": "mcp",
            "tool_names": ["web_fetch"]
        }))
        .expect_err("request-scoped MCP must not advertise canonical non-MCP tools implicitly");

        assert!(
            error
                .to_string()
                .contains("MCP provider tool name must be namespaced")
        );
    }

    #[test]
    fn generic_edge_provider_does_not_declare_request_scoped_publish_artifact() {
        let registry = ToolRegistry::builtins();
        let provider = runtime_workspace_provider(
            CapacityProviderType::EdgeCapacity,
            "edge",
            &registry,
            RuntimePlatform::Linux,
        );

        assert!(!provider.declares_tool("materialize_attachment"));
        assert!(!provider.declares_tool("publish_artifact"));
        assert!(provider.declares_tool("read_file"));

        let server_sandbox = runtime_workspace_provider(
            CapacityProviderType::Sandbox,
            "server-sandbox",
            &registry,
            RuntimePlatform::Linux,
        );
        assert!(server_sandbox.declares_tool("publish_artifact"));
    }

    #[test]
    fn provider_declaration_deserialization_rejects_orphan_schema_digest_keys() {
        let error = serde_json::from_value::<CapacityProviderDeclaration>(serde_json::json!({
            "provider_type": "server_service",
            "provider_id": "server-builtin",
            "tool_names": ["web_fetch"],
            "tool_schema_digests": {
                "web_search": "sha256:unexpected"
            }
        }))
        .expect_err("schema digest keys must be scoped to declared tools");

        assert!(
            error
                .to_string()
                .contains("provider schema digest key must reference a declared tool name")
        );
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
        assert!(control.declares_tool("task_board"));
        assert!(!control.declares_tool("task"));
        assert!(!control.declares_tool("web_fetch"));

        assert!(cli.declares_tool("bash"));
        assert!(cli.declares_tool("web_fetch"));
        assert!(cli.declares_tool("web_search"));
        assert!(cli.declares_tool("read_file"));
        assert_eq!(
            cli.declares_tool("powershell"),
            RuntimePlatform::current().supports_powershell()
        );
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
            let provider = runtime_workspace_provider(
                provider_type,
                "runtime",
                &registry,
                RuntimePlatform::Unknown,
            );
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
    fn windows_runtime_workspace_providers_advertise_powershell() {
        let registry = ToolRegistry::builtins();
        for provider_type in [
            CapacityProviderType::CliLocal,
            CapacityProviderType::EdgeCapacity,
            CapacityProviderType::Sandbox,
            CapacityProviderType::OrchestratorManagedRuntime,
        ] {
            let provider = runtime_workspace_provider(
                provider_type,
                "runtime",
                &registry,
                RuntimePlatform::Windows,
            );
            assert!(
                provider.declares_tool("powershell"),
                "{provider_type} should advertise powershell when platform facts say Windows"
            );
        }
    }

    #[test]
    fn request_scoped_mcp_provider_declares_only_bound_tools() {
        let provider = request_scoped_mcp_provider("mcp", ["mcp__weather".to_string()]);

        assert!(provider.declares_tool("mcp__weather"));
        assert!(!provider.declares_tool("mcp__calculator"));
    }

    #[test]
    #[should_panic(expected = "MCP provider tool name must be namespaced")]
    fn request_scoped_mcp_provider_rejects_non_mcp_tool_names_without_compatibility_offer() {
        let _ = request_scoped_mcp_provider("mcp", ["web_fetch".to_string()]);
    }

    #[test]
    #[should_panic(expected = "invalid provider-declared tool name")]
    fn provider_schema_digest_rejects_invalid_tool_name() {
        let _ = server_service_provider("server", &ToolRegistry::builtins())
            .with_tool_schema_digest("web.fetch", "sha256:invalid");
    }

    #[test]
    fn mcp_schema_digest_is_canonical_across_json_key_order() {
        let first = serde_json::json!({
            "type": "function",
            "function": {
                "name": "mcp__weather__query",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "city": { "type": "string" },
                        "unit": { "type": "string" }
                    }
                }
            }
        });
        let second = serde_json::json!({
            "function": {
                "parameters": {
                    "properties": {
                        "unit": { "type": "string" },
                        "city": { "type": "string" }
                    },
                    "type": "object"
                },
                "name": "mcp__weather__query"
            },
            "type": "function"
        });

        assert_eq!(
            canonical_tool_schema_digest(&first),
            canonical_tool_schema_digest(&second)
        );
    }

    #[test]
    fn request_scoped_mcp_provider_from_schemas_uses_schema_digest() {
        let schema = serde_json::json!({
            "type": "function",
            "function": { "name": "mcp__weather__query" }
        });
        let provider =
            request_scoped_mcp_provider_from_schemas("mcp", std::slice::from_ref(&schema));

        assert!(provider.declares_tool("mcp__weather__query"));
        assert_eq!(
            provider.schema_digest_for_tool("mcp__weather__query"),
            Some(canonical_tool_schema_digest(&schema).as_str())
        );
    }

    #[test]
    fn mcp_provider_from_schemas_uses_generic_mcp_provider_type() {
        let schema = serde_json::json!({
            "type": "function",
            "function": { "name": "mcp__github__search" }
        });
        let provider = mcp_provider_from_schemas("server-mcp", std::slice::from_ref(&schema));

        assert_eq!(provider.provider_type, CapacityProviderType::McpProvider);
        assert_eq!(provider.provider_id, "server-mcp");
        assert!(provider.declares_tool("mcp__github__search"));
        assert_eq!(
            provider.schema_digest_for_tool("mcp__github__search"),
            Some(canonical_tool_schema_digest(&schema).as_str())
        );
    }

    #[test]
    #[should_panic(expected = "invalid capacity provider id")]
    fn mcp_provider_from_schemas_rejects_user_label_delimiter_injection() {
        let schema = serde_json::json!({
            "type": "function",
            "function": { "name": "mcp__github__search" }
        });
        let _ = mcp_provider_from_schemas("mcp@github", &[schema]);
    }
}
