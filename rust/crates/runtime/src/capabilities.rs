//! Unified capability catalog for Web and CLI agent turns.
//!
//! A capability is anything the model can intentionally choose during a turn:
//! tool schemas and skill manifests.  The important boundary is not "web vs
//! cli" by itself, but **where execution happens**.
//!
//! - Web and remote/thin CLI turns execute on the API server. They may only see
//!   server-executable tools and the server-visible skill catalog.
//! - Local CLI turns execute edge tools in the CLI process. They may see local
//!   CLI/project tools, local MCP tools, project-local skills, and the same
//!   authenticated server catalog that Web sees.
//!
//! Keeping those rules here prevents the Web picker, runtime prompt assembly,
//! and CLI tool-surface assembly from growing separate capability policies.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use astra_runtime_env::CapacityProvider;
use astra_services::skills::{SkillListCursor, SkillListRecord, SkillRecord, SkillService};
use astra_skills::manifest::{
    ExecutionContext, LoadedSkill, SkillManifest, SkillSourceKind, TrustTier,
};
use astra_skills::traits::{SkillError, SkillProvider};
use astra_turn_core::tool::schema::tool_schema_name;

use crate::skills::{BundledSkillProvider, LocalSkillProvider, UnifiedSkillRegistry};

const REMOTE_SKILL_PAGE_SIZE: u32 = 500;
const REMOTE_SKILL_MAX_ROWS: u32 = 5_000;

pub use astra_turn_core::tool_surface::Surface as CapabilitySurface;

/// Skill source buckets used by the capability policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SkillCapabilitySource {
    /// Skills installed in the API server process HOME.
    ServerHome,
    /// Skills in MatrixOne visible through `created_by = user OR is_public = 1`.
    ServerDatabaseVisible,
    /// Skills discovered by the local CLI from cwd walk-up and HOME paths.
    CliFilesystem,
    /// Skills compiled into the CLI/runtime binary.
    CliBundled,
}

/// Full capability set for tests that need the complete catalog. Production
/// callers must derive capabilities from actual service wiring instead.
pub fn full_server_capabilities_for_tests() -> astra_turn_core::capability::CapabilitySet {
    astra_turn_core::capability::CapabilitySet::all()
}

/// Production-truth `CapabilitySet` for an agentic-run lifecycle.
pub fn lifecycle_server_capabilities(
    _database_pool_present: bool,
    reflect_service_configured: bool,
) -> astra_turn_core::capability::CapabilitySet {
    use astra_turn_core::capability::{Capability, CapabilitySet};
    CapabilitySet::empty()
        .with(Capability::AgentSpawner)
        .with(Capability::MemoryService)
        .with(Capability::SkillsCatalog)
        .with(Capability::GitHubAuth)
        .with(Capability::PlanLifecycle)
        .with_if(reflect_service_configured, Capability::ReflectService)
}

/// Production-truth server `CapabilitySet` derived from [`crate::app_state::AppState`].
pub fn server_capabilities_from(
    state: &crate::app_state::AppState,
) -> astra_turn_core::capability::CapabilitySet {
    lifecycle_server_capabilities(
        state.shared_pool.is_some(),
        state.reflect_service.is_configured(),
    )
}

/// Server-service/control-plane schemas declared by the server builtin provider.
///
/// This is the provider-owned source pool for Web / remote server execution.
/// Workspace and process schemas are intentionally absent here; they are added
/// only by an explicit runtime/workspace provider binding in
/// `tool_binding_projection`.
pub fn server_builtin_tool_schemas(
    capabilities: &astra_turn_core::capability::CapabilitySet,
) -> Vec<Value> {
    let registry = astra_runtime_env::ToolRegistry::builtins();
    let providers = vec![
        astra_runtime_env::server_service_provider("server-builtin", &registry),
        astra_runtime_env::control_plane_provider("server-control-plane", &registry),
    ];
    let pool = schema_pool_declared_by_providers(&providers);
    let mut schemas =
        astra_turn_core::tool_surface::resolve(CapabilitySurface::Web, capabilities, &pool);
    if !capabilities.has(astra_turn_core::capability::Capability::ReflectService) {
        schemas.retain(|s| tool_schema_name(s) != Some("reflect"));
    }
    retain_server_executable_schemas(&mut schemas);
    #[cfg(unix)]
    {
        astra_tools::schemas::narrow_run_script_for_server(&mut schemas);
    }
    schemas
}

/// Runtime/workspace executor schemas that require an explicit execution
/// provider such as edge, server sandbox, or orchestrator-managed runtime.
pub fn runtime_executor_tool_schemas(
    capabilities: &astra_turn_core::capability::CapabilitySet,
) -> Vec<Value> {
    let registry = astra_runtime_env::ToolRegistry::builtins();
    let provider = astra_runtime_env::runtime_workspace_provider(
        astra_runtime_env::CapacityProviderType::Sandbox,
        "runtime-workspace",
        &registry,
        astra_runtime_env::RuntimePlatform::Unknown,
    );
    let pool = schema_pool_declared_by_providers(&[provider]);
    let mut schemas =
        astra_turn_core::tool_surface::resolve(CapabilitySurface::Web, capabilities, &pool);
    #[cfg(unix)]
    {
        astra_tools::schemas::narrow_run_script_for_server(&mut schemas);
    }
    schemas
}

/// Tool schemas for local CLI turns.
pub fn cli_local_tool_schemas(
    client_builtin: Vec<Value>,
    client_mcp: Vec<Value>,
    capabilities: &astra_turn_core::capability::CapabilitySet,
) -> Vec<Value> {
    let registry = astra_runtime_env::ToolRegistry::builtins();
    let providers = vec![
        astra_runtime_env::server_service_provider("cli-server-service", &registry),
        astra_runtime_env::control_plane_provider("cli-control-plane", &registry),
        astra_runtime_env::cli_local_provider("cli-local", &registry),
    ];
    let mut pool = client_builtin
        .into_iter()
        .filter(|schema| {
            tool_schema_name(schema).is_some_and(|name| {
                providers
                    .iter()
                    .any(|provider| provider.declares_tool(name))
            })
        })
        .collect::<Vec<_>>();
    pool.extend(mcp_provider_tool_schemas(
        "cli-request-scoped-mcp",
        client_mcp,
    ));
    astra_turn_core::tool_surface::resolve(CapabilitySurface::CliLocal, capabilities, &pool)
}

/// Tool schemas for remote/thin CLI turns. Mirrors Web because execution
/// happens on the API server.
pub fn cli_remote_tool_schemas(
    server_mcp: Vec<Value>,
    capabilities: &astra_turn_core::capability::CapabilitySet,
) -> Vec<Value> {
    let mut pool = server_builtin_tool_schemas(capabilities);
    pool.extend(mcp_provider_tool_schemas("server-mcp", server_mcp));
    let mut schemas =
        astra_turn_core::tool_surface::resolve(CapabilitySurface::CliRemote, capabilities, &pool);
    if !capabilities.has(astra_turn_core::capability::Capability::ReflectService) {
        schemas.retain(|s| tool_schema_name(s) != Some("reflect"));
    }
    retain_server_executable_schemas(&mut schemas);
    #[cfg(unix)]
    {
        astra_tools::schemas::narrow_run_script_for_server(&mut schemas);
    }
    schemas
}

fn mcp_provider_tool_schemas(provider_id: &str, schemas: Vec<Value>) -> Vec<Value> {
    let registry = astra_runtime_env::ToolRegistry::builtins();
    let provider = astra_runtime_env::request_scoped_mcp_provider_from_schemas(
        provider_id,
        schemas.as_slice(),
    );
    astra_runtime_env::CapabilityResolver.filter_tool_schemas_for_providers(
        &registry,
        schemas,
        &request_scoped_mcp_capabilities(),
        &[provider],
    )
}

fn request_scoped_mcp_capabilities() -> astra_runtime_env::EffectiveCapabilitySet {
    astra_runtime_env::EffectiveCapabilitySet {
        workspace: astra_runtime_env::WorkspaceCapabilities {
            present: false,
            readable: false,
            writable: false,
            persistent: false,
        },
        executor: astra_runtime_env::ExecutorCapabilities {
            reachable: true,
            control_plane: false,
            server_service: false,
            runtime_executor: false,
            mcp_executor: true,
        },
        runtime: astra_runtime_env::RuntimeCapabilities {
            runtime_has_process: false,
            runtime_has_shell: false,
            runtime_has_git: false,
            runtime_has_lsp: false,
            runtime_has_network: false,
            runtime_has_credentials: false,
        },
        policy: astra_runtime_env::PolicyCapabilities {
            filesystem_read: false,
            filesystem_write: false,
            network: astra_runtime_env::NetworkCapability::AllowList,
            credentials: false,
            approvals_required: false,
            audit_required: true,
        },
    }
}

fn retain_server_executable_schemas(schemas: &mut Vec<Value>) {
    schemas.retain(|schema| !matches!(tool_schema_name(schema), Some("lsp" | "powershell")));
}

fn schema_pool_declared_by_providers(
    providers: &[astra_runtime_env::CapacityProviderDeclaration],
) -> Vec<Value> {
    let registry = astra_runtime_env::ToolRegistry::builtins();
    astra_tools::schemas::all_tool_schemas()
        .into_iter()
        .filter(|schema| {
            tool_schema_name(schema).is_some_and(|name| {
                registry
                    .get(name)
                    .is_some_and(|spec| spec.load_policy.is_public_schema_policy())
                    && providers
                        .iter()
                        .any(|provider| provider.declares_tool(name))
            })
        })
        .collect()
}

/// Return the skill source policy for a surface.
pub fn skill_sources_for_surface(surface: CapabilitySurface) -> &'static [SkillCapabilitySource] {
    match surface {
        CapabilitySurface::Web | CapabilitySurface::CliRemote => &[
            SkillCapabilitySource::ServerHome,
            SkillCapabilitySource::ServerDatabaseVisible,
        ],
        CapabilitySurface::CliLocal => &[
            SkillCapabilitySource::CliFilesystem,
            SkillCapabilitySource::CliBundled,
            SkillCapabilitySource::ServerHome,
            SkillCapabilitySource::ServerDatabaseVisible,
        ],
    }
}

pub fn surface_uses_server_skill_catalog(surface: CapabilitySurface) -> bool {
    skill_sources_for_surface(surface).iter().any(|source| {
        matches!(
            source,
            SkillCapabilitySource::ServerHome | SkillCapabilitySource::ServerDatabaseVisible
        )
    })
}

/// Build the server-visible skill registry for Web / remote CLI execution.
pub fn build_server_skill_registry(
    skill_service: Option<Arc<dyn SkillService>>,
    user_id: &str,
) -> Option<Arc<UnifiedSkillRegistry>> {
    crate::skills::catalog::build_server_visible_skill_registry(skill_service, user_id)
}

/// Build the local CLI skill registry.
///
/// Provider order is intentional:
/// 1. Project/home filesystem skills from the CLI process.
/// 2. Bundled dynamic skills compiled into the CLI.
/// 3. Authenticated remote server catalog, which adds DB skills and API-server
///    HOME skills without overriding project-local skills of the same name.
pub fn build_cli_local_skill_registry(
    remote_catalog: Option<RemoteSkillCatalogProvider>,
) -> Arc<UnifiedSkillRegistry> {
    let mut registry = UnifiedSkillRegistry::new();
    registry.add_provider(Box::new(LocalSkillProvider::standard()));
    registry.add_provider(Box::new(BundledSkillProvider::with_defaults()));
    if let Some(provider) = remote_catalog {
        registry.add_provider(Box::new(provider));
    }
    let registry = Arc::new(registry);
    crate::skills::catalog::discover_registry_now(&registry);
    registry
}

/// HTTP-backed skill provider used by the local CLI to read the same
/// authenticated server catalog that Web uses.
///
/// This provider is read-only. Publishing/importing still goes through the
/// existing `/skill publish` flow so visibility changes stay explicit.
/// Reads the current access token at call time. The token may have
/// been refreshed since the provider was constructed (login, 401-retry,
/// background refresh). Returning `None` means "not authenticated" —
/// the provider will fail requests with a clear credentials error
/// rather than using a stale token that always 401s.
pub type TokenProvider = std::sync::Arc<dyn Fn() -> Option<String> + Send + Sync>;

#[derive(Clone)]
pub struct RemoteSkillCatalogProvider {
    api: astra_thin_client::ThinClient,
    token_provider: TokenProvider,
}

impl RemoteSkillCatalogProvider {
    pub fn new(api: astra_thin_client::ThinClient, token_provider: TokenProvider) -> Self {
        Self {
            api,
            token_provider,
        }
    }

    fn current_token(&self) -> Result<String, SkillError> {
        (self.token_provider)().ok_or_else(|| {
            SkillError::LoadFailed(
                "no valid access token available; run /login to authenticate".into(),
            )
        })
    }

    async fn list_page(
        &self,
        limit: u32,
        cursor: Option<SkillListCursor>,
    ) -> Result<SkillListRecord, SkillError> {
        let token = self.current_token()?;
        let mut params = vec![("limit", limit.to_string())];
        if let Some(cursor) = &cursor {
            params.push(("after_skill_name", cursor.skill_name.clone()));
            params.push(("after_version", cursor.version.clone()));
            params.push(("after_skill_id", cursor.skill_id.clone()));
        }
        let body = self
            .api
            .get_skills_query_text(&token, &params)
            .await
            .map_err(|source| {
                SkillError::LoadFailed(format!(
                    "failed to list remote skill catalog page: {source}"
                ))
            })?;
        serde_json::from_str(&body).map_err(|source| {
            SkillError::ParseFailed(format!(
                "failed to parse remote skill catalog page: {source}"
            ))
        })
    }

    async fn load_record(&self, name: &str) -> Result<SkillRecord, SkillError> {
        let token = self.current_token()?;
        let body = self
            .api
            .get_skill_query_text(&token, name, &[])
            .await
            .map_err(|source| {
                SkillError::LoadFailed(format!("failed to load remote skill '{name}': {source}"))
            })?;
        serde_json::from_str(&body).map_err(|source| {
            SkillError::ParseFailed(format!(
                "failed to parse remote skill '{name}' response: {source}"
            ))
        })
    }
}

#[async_trait]
impl SkillProvider for RemoteSkillCatalogProvider {
    fn source_kind(&self) -> SkillSourceKind {
        SkillSourceKind::Database
    }

    async fn discover(&self) -> Result<Vec<SkillManifest>, SkillError> {
        let mut cursor = None;
        let mut manifests = Vec::new();
        loop {
            let remaining = REMOTE_SKILL_MAX_ROWS.saturating_sub(manifests.len() as u32);
            if remaining == 0 {
                break;
            }
            let limit = remaining.min(REMOTE_SKILL_PAGE_SIZE);
            let page = self.list_page(limit, cursor).await?;
            let page_len = page.skills.len() as u32;
            cursor = page.next_cursor.clone();
            manifests.extend(page.skills.into_iter().map(|item| SkillManifest {
                name: item.skill_name,
                version: item.version.parse().unwrap_or_default(),
                description: item.description.unwrap_or_default(),
                source: SkillSourceKind::Database,
                category: item.category,
                ..Default::default()
            }));
            if page_len < limit || cursor.is_none() {
                break;
            }
        }
        Ok(manifests)
    }

    async fn load(&self, name: &str) -> Result<LoadedSkill, SkillError> {
        loaded_skill_from_record(self.load_record(name).await?)
    }

    async fn refresh(&self) -> Result<(), SkillError> {
        Ok(())
    }
}

pub fn loaded_skill_from_record(record: SkillRecord) -> Result<LoadedSkill, SkillError> {
    let metadata = record.metadata.unwrap_or_else(|| serde_json::json!({}));
    let metadata_obj = metadata.as_object().cloned().unwrap_or_default();
    let instructions = metadata_obj
        .get("instructions")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            SkillError::LoadFailed(format!(
                "remote skill '{}' did not include metadata.instructions",
                record.skill_name
            ))
        })?
        .to_string();
    let instruction_tokens = (instructions.len() as u32) / 4;
    let version = record.version.parse().unwrap_or_default();

    let execution_context = match metadata_obj
        .get("execution_context")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "fork" => ExecutionContext::Fork,
        _ => ExecutionContext::Inline,
    };
    let trust_tier = match metadata_obj
        .get("trust_tier")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "bundled" => TrustTier::Bundled,
        "verified" => TrustTier::Verified,
        "community" => TrustTier::Community,
        _ => TrustTier::Unverified,
    };
    let user_invocable = metadata_obj
        .get("user_invocable")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let category = metadata_obj
        .get("category")
        .and_then(Value::as_str)
        .map(str::to_string);
    let tags = string_array_field(&metadata_obj, "tags");
    let allowed_tools = string_array_field(&metadata_obj, "allowed_tools");
    let metadata_map: HashMap<String, Value> = metadata_obj.into_iter().collect();

    Ok(LoadedSkill {
        manifest: SkillManifest {
            name: record.skill_name,
            version,
            description: record.description.unwrap_or_default(),
            source: SkillSourceKind::Database,
            execution_context,
            user_invocable,
            allowed_tools,
            category,
            tags,
            metadata: metadata_map,
            trust_tier,
            ..Default::default()
        },
        instructions,
        instruction_tokens,
        resources: None,
        skill_dir: None,
    })
}

fn string_array_field(map: &serde_json::Map<String, Value>, key: &str) -> Vec<String> {
    map.get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema(name: &str) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": name,
                "description": format!("{name} tool"),
                "parameters": {"type": "object"}
            }
        })
    }

    fn names(schemas: Vec<Value>) -> Vec<String> {
        schemas
            .iter()
            .filter_map(tool_schema_name)
            .map(str::to_string)
            .collect()
    }

    fn contains_object_key(value: &Value, needle: &str) -> bool {
        match value {
            Value::Object(map) => {
                map.contains_key(needle)
                    || map.values().any(|child| contains_object_key(child, needle))
            }
            Value::Array(items) => items.iter().any(|child| contains_object_key(child, needle)),
            _ => false,
        }
    }

    // ── remote/server tool schema assembly ──

    #[test]
    fn cli_remote_catalog_uses_server_builtin_and_namespaced_server_mcp_only() {
        let caps = full_server_capabilities_for_tests();
        let tool_names = names(cli_remote_tool_schemas(
            vec![
                schema("mcp__server_docs__query"),
                schema("mcp__"),
                schema("mcp__bad/name"),
                schema("read_file"),
                schema("powershell"),
                json!({"type": "custom", "function": {"name": "mcp__custom__bad"}}),
            ],
            &caps,
        ));

        assert!(tool_names.contains(&"tool_search".to_string()));
        assert!(tool_names.contains(&"memory".to_string()));
        assert!(tool_names.contains(&"mcp__server_docs__query".to_string()));
        for hidden in [
            "read_file",
            "powershell",
            "mcp__custom__bad",
            "mcp__",
            "mcp__bad/name",
        ] {
            assert!(
                !tool_names.contains(&hidden.to_string()),
                "{hidden} must not enter remote CLI server-executed schema surface: {tool_names:?}"
            );
        }
    }

    #[test]
    fn cli_remote_catalog_fails_closed_for_conflicting_server_mcp_schema() {
        let caps = full_server_capabilities_for_tests();
        let tool_names = names(cli_remote_tool_schemas(
            vec![
                json!({
                    "type": "function",
                    "function": {
                        "name": "mcp__docs__query",
                        "parameters": {
                            "type": "object",
                            "properties": {
                                "query": { "type": "string" }
                            }
                        }
                    }
                }),
                json!({
                    "type": "function",
                    "function": {
                        "name": "mcp__docs__query",
                        "parameters": {
                            "type": "object",
                            "properties": {
                                "q": { "type": "string" }
                            }
                        }
                    }
                }),
            ],
            &caps,
        ));

        assert!(
            !tool_names.contains(&"mcp__docs__query".to_string()),
            "conflicting remote MCP schemas must be hidden instead of first-wins"
        );
    }

    #[test]
    fn mcp_schema_pool_requires_provider_admitted_function_schema() {
        let tool_names = names(mcp_provider_tool_schemas(
            "cli-request-scoped-mcp",
            vec![
                schema("mcp__zeta__query"),
                schema("custom_plugin_tool"),
                schema("mcp__bad/name"),
                json!({"type": "custom", "function": {"name": "mcp__custom__bad"}}),
                schema("mcp__alpha__query"),
            ],
        ));

        assert_eq!(
            tool_names,
            vec![
                "mcp__alpha__query".to_string(),
                "mcp__zeta__query".to_string()
            ],
            "MCP schemas must be admitted as provider offers, fail closed for invalid/non-MCP schemas, and keep stable prompt order"
        );
    }

    #[test]
    fn skill_source_policy_matches_execution_surface() {
        assert_eq!(
            skill_sources_for_surface(CapabilitySurface::Web),
            &[
                SkillCapabilitySource::ServerHome,
                SkillCapabilitySource::ServerDatabaseVisible,
            ],
            "web skill visibility must be server-owned only"
        );
        assert_eq!(
            skill_sources_for_surface(CapabilitySurface::CliRemote),
            skill_sources_for_surface(CapabilitySurface::Web),
            "remote/thin CLI skill visibility must match web"
        );
        assert_eq!(
            skill_sources_for_surface(CapabilitySurface::CliLocal),
            &[
                SkillCapabilitySource::CliFilesystem,
                SkillCapabilitySource::CliBundled,
                SkillCapabilitySource::ServerHome,
                SkillCapabilitySource::ServerDatabaseVisible,
            ],
            "local CLI must include local filesystem skills before the authenticated server catalog"
        );
    }

    // ── surface-routed backbone and local process tools ──

    #[test]
    fn surface_routed_backbone_and_local_process_tools() {
        use astra_turn_core::capability::{Capability, CapabilitySet};
        use astra_turn_core::tool_surface::{Surface, resolve};

        let pool = astra_tools::schemas::all_tool_schemas();

        // ── Plan lifecycle: server-owned in web/remote execution ──
        let base_caps = CapabilitySet::empty()
            .with(Capability::AgentSpawner)
            .with(Capability::MemoryService)
            .with(Capability::Database)
            .with(Capability::SkillsCatalog)
            .with(Capability::GitHubAuth)
            .with(Capability::LSPServer);
        let plan_caps = base_caps.clone().with(Capability::PlanLifecycle);

        let local_plan = names(resolve(Surface::CliLocal, &base_caps, &pool));
        let web_plan = names(resolve(Surface::Web, &plan_caps, &pool));
        let remote_plan = names(resolve(Surface::CliRemote, &plan_caps, &pool));

        assert!(
            !local_plan.contains(&"enter_plan_mode".to_string()),
            "this capability pass models server-owned plan lifecycle; CLI local plan mode is injected by the CLI adapter"
        );
        assert!(
            web_plan.contains(&"enter_plan_mode".to_string()),
            "web must expose plan lifecycle"
        );
        assert_eq!(
            remote_plan, web_plan,
            "remote CLI plan visibility must match web"
        );

        let server_caps = lifecycle_server_capabilities(true, true);
        let local_caps = CapabilitySet::empty()
            .with(Capability::MemoryService)
            .with(Capability::SkillsCatalog)
            .with(Capability::PlanLifecycle)
            .with(Capability::LocalBackgroundTasks);

        // ── Durable task board: runtime backbone, not CLI-local ──
        for (surface, names) in [
            ("web", names(resolve(Surface::Web, &server_caps, &pool))),
            (
                "remote CLI",
                names(resolve(Surface::CliRemote, &server_caps, &pool)),
            ),
            (
                "local CLI",
                names(resolve(Surface::CliLocal, &local_caps, &pool)),
            ),
        ] {
            assert!(
                names.contains(&"task".to_string()),
                "{surface} must expose the durable task-board backbone: {names:?}"
            );
        }

        // ── Typed background process tools: local executor only ──
        let web_bg = names(resolve(Surface::Web, &server_caps, &pool));
        let remote_bg = names(resolve(Surface::CliRemote, &server_caps, &pool));
        let local_bg = names(resolve(Surface::CliLocal, &local_caps, &pool));

        for bg_tool in &["task_output", "task_stop", "task_list"] {
            assert!(
                !web_bg.contains(&bg_tool.to_string()),
                "web must not advertise {bg_tool}"
            );
            assert!(
                !remote_bg.contains(&bg_tool.to_string()),
                "remote CLI must not advertise {bg_tool}"
            );
            assert!(
                local_bg.contains(&bg_tool.to_string()),
                "local CLI must advertise {bg_tool}"
            );
        }
        assert!(
            !local_bg.contains(&"job".to_string()),
            "removed job tool must not appear"
        );
    }

    #[test]
    fn server_executed_tool_descriptions_do_not_reference_unavailable_job_tool() {
        let caps = lifecycle_server_capabilities(true, true);
        let server_tools = server_builtin_tool_schemas(&caps);
        let remote_tools = cli_remote_tool_schemas(Vec::new(), &caps);

        for (surface, schemas) in [("web", server_tools), ("remote", remote_tools)] {
            let names = names(schemas.clone());
            assert!(
                !names.contains(&"job".to_string()),
                "{surface} must not advertise local TUI job tool: {names:?}"
            );
            for schema in schemas {
                let name = tool_schema_name(&schema).unwrap_or("<unknown>");
                let desc = schema
                    .get("function")
                    .and_then(|function| function.get("description"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                assert!(
                    !desc.contains("job(action='") && !desc.contains("use `job`"),
                    "{surface} schema `{name}` must not direct the model to unavailable local job tool: {desc}"
                );
            }
        }
    }

    #[test]
    fn server_builtin_schema_pool_is_provider_declared_only() {
        let caps = full_server_capabilities_for_tests();
        let tool_names: std::collections::BTreeSet<String> =
            names(server_builtin_tool_schemas(&caps))
                .into_iter()
                .collect();

        for visible in [
            "ask_user",
            "agent",
            "task",
            "session",
            "tool_search",
            "web_fetch",
            "memory",
        ] {
            assert!(
                tool_names.contains(visible),
                "{visible} should be declared by the server builtin provider"
            );
        }
        for hidden in [
            "bash",
            "read_file",
            "write_file",
            "str_replace",
            "git",
            "run_script",
            "symbols",
            "delete_file",
            "multi_edit",
            "background_shell",
            "git_clone",
            "find_definition",
            "find_references",
            "task_output",
            "task_stop",
            "task_list",
        ] {
            assert!(
                !tool_names.contains(hidden),
                "{hidden} must require an explicit runtime/workspace provider"
            );
        }
    }

    #[test]
    fn runtime_executor_schema_pool_is_not_server_builtin_capacity() {
        let caps = full_server_capabilities_for_tests();
        let tool_names: std::collections::BTreeSet<String> =
            names(runtime_executor_tool_schemas(&caps))
                .into_iter()
                .collect();

        for visible in [
            "bash",
            "web_fetch",
            "web_search",
            "read_file",
            "write_file",
            "git",
            "run_script",
            "lsp",
        ] {
            assert!(
                tool_names.contains(visible),
                "{visible} should be available to explicit runtime/workspace providers"
            );
        }
        for hidden in ["powershell", "display_sixel"] {
            assert!(
                !tool_names.contains(hidden),
                "{hidden} requires an explicit terminal/platform-local provider, not the generic server runtime provider pool"
            );
        }
        for server_owned in ["ask_user", "agent", "tool_search", "memory"] {
            assert!(
                !tool_names.contains(server_owned),
                "{server_owned} must stay in the server builtin provider pool"
            );
        }
        for internal in [
            "delete_file",
            "multi_edit",
            "background_shell",
            "git_clone",
            "find_definition",
            "find_references",
        ] {
            assert!(
                !tool_names.contains(internal),
                "{internal} is an internal execution helper and must not enter model schema pools"
            );
        }
    }

    #[test]
    fn cli_local_catalog_filters_builtin_source_by_provider_ownership() {
        let caps = full_server_capabilities_for_tests();
        let tool_names = names(cli_local_tool_schemas(
            vec![
                schema("read_file"),
                schema("bash"),
                schema("memory"),
                schema("ask_user"),
                schema("custom_local_builtin"),
            ],
            vec![
                schema("mcp__local__query"),
                schema("mcp__"),
                schema("mcp__bad/name"),
            ],
            &caps,
        ));

        for expected in [
            "read_file",
            "bash",
            "memory",
            "ask_user",
            "mcp__local__query",
        ] {
            assert!(
                tool_names.contains(&expected.to_string()),
                "{expected} should remain visible in local CLI catalog: {tool_names:?}"
            );
        }
        assert!(
            !tool_names.contains(&"custom_local_builtin".to_string()),
            "client builtin schemas must still be declared by a CLI/server provider"
        );
        for invalid in ["mcp__", "mcp__bad/name"] {
            assert!(
                !tool_names.contains(&invalid.to_string()),
                "invalid MCP schema name {invalid:?} must not enter local CLI catalog: {tool_names:?}"
            );
        }
    }

    #[test]
    fn cli_local_catalog_fails_closed_for_conflicting_mcp_schemas() {
        let caps = full_server_capabilities_for_tests();
        let tool_names = names(cli_local_tool_schemas(
            vec![schema("read_file")],
            vec![
                json!({
                    "type": "function",
                    "function": {
                        "name": "mcp__docs__query",
                        "parameters": {
                            "type": "object",
                            "properties": {
                                "query": { "type": "string" }
                            }
                        }
                    }
                }),
                json!({
                    "type": "function",
                    "function": {
                        "name": "mcp__docs__query",
                        "parameters": {
                            "type": "object",
                            "properties": {
                                "q": { "type": "string" }
                            }
                        }
                    }
                }),
            ],
            &caps,
        ));

        assert!(tool_names.contains(&"read_file".to_string()));
        assert!(
            !tool_names.contains(&"mcp__docs__query".to_string()),
            "conflicting MCP offers must be hidden instead of first-wins"
        );
    }

    #[test]
    fn cli_local_catalog_is_byte_stable_for_identical_inputs() {
        let caps = full_server_capabilities_for_tests();
        let client_mcp = vec![
            json!({
                "type": "function",
                "function": {
                    "name": "mcp__docs__query",
                    "description": "Query local docs.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "query": { "type": "string" }
                        },
                        "required": ["query"]
                    }
                }
            }),
            json!({
                "type": "function",
                "function": {
                    "name": "mcp__tickets__search",
                    "description": "Search local tickets.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "q": { "type": "string" }
                        },
                        "required": ["q"]
                    }
                }
            }),
        ];

        let first = cli_local_tool_schemas(
            astra_tools::schemas::all_tool_schemas(),
            client_mcp.clone(),
            &caps,
        );
        let second =
            cli_local_tool_schemas(astra_tools::schemas::all_tool_schemas(), client_mcp, &caps);

        assert_eq!(
            serde_json::to_vec(&first).expect("serialize first tool schema list"),
            serde_json::to_vec(&second).expect("serialize second tool schema list"),
            "CLI local tool schema list must be byte-stable for identical provider inputs"
        );
    }

    #[test]
    fn cli_local_catalog_is_byte_stable_for_permuted_mcp_inputs() {
        let caps = full_server_capabilities_for_tests();
        let alpha = schema("mcp__alpha__query");
        let zeta = schema("mcp__zeta__query");

        let first = cli_local_tool_schemas(
            astra_tools::schemas::all_tool_schemas(),
            vec![zeta.clone(), alpha.clone()],
            &caps,
        );
        let second = cli_local_tool_schemas(
            astra_tools::schemas::all_tool_schemas(),
            vec![alpha, zeta],
            &caps,
        );

        assert_eq!(
            serde_json::to_vec(&first).expect("serialize first tool schema list"),
            serde_json::to_vec(&second).expect("serialize second tool schema list"),
            "CLI local prompt schemas must not depend on MCP list_tools order"
        );
    }

    #[test]
    fn cli_remote_catalog_is_byte_stable_for_permuted_mcp_inputs() {
        let caps = full_server_capabilities_for_tests();
        let alpha = schema("mcp__server_alpha__query");
        let zeta = schema("mcp__server_zeta__query");

        let first = cli_remote_tool_schemas(vec![zeta.clone(), alpha.clone()], &caps);
        let second = cli_remote_tool_schemas(vec![alpha, zeta], &caps);

        assert_eq!(
            serde_json::to_vec(&first).expect("serialize first tool schema list"),
            serde_json::to_vec(&second).expect("serialize second tool schema list"),
            "CLI remote prompt schemas must not depend on MCP list_tools order"
        );
    }

    #[test]
    fn cli_local_prompt_schemas_do_not_embed_provider_or_route_metadata() {
        let caps = full_server_capabilities_for_tests();
        let schemas = cli_local_tool_schemas(
            astra_tools::schemas::all_tool_schemas(),
            vec![schema("mcp__docs__query")],
            &caps,
        );

        for forbidden_key in [
            "provider_id",
            "provider_type",
            "capacity_provider",
            "offer_id",
            "selected_offer",
            "selected_route",
            "executor_id",
        ] {
            assert!(
                !schemas
                    .iter()
                    .any(|schema| contains_object_key(schema, forbidden_key)),
                "provider/executor routing metadata key `{forbidden_key}` must not enter prompt-visible tool schemas"
            );
        }
    }

    #[test]
    fn server_executed_surfaces_hide_client_only_runtime_tools() {
        let caps = full_server_capabilities_for_tests();
        let web = names(server_builtin_tool_schemas(&caps));
        let remote = names(cli_remote_tool_schemas(Vec::new(), &caps));
        let local = names(cli_local_tool_schemas(
            astra_tools::schemas::all_tool_schemas(),
            Vec::new(),
            &caps,
        ));

        for tool in ["lsp", "powershell"] {
            assert!(
                !web.contains(&tool.to_string()),
                "web/server surface must not advertise client-only tool {tool}: {web:?}"
            );
            assert!(
                !remote.contains(&tool.to_string()),
                "remote CLI executes on the server and must not advertise client-only tool {tool}: {remote:?}"
            );
        }
        assert!(
            local.contains(&"lsp".to_string()),
            "local CLI should retain client-owned LSP tools"
        );
        assert!(
            !local.contains(&"powershell".to_string()),
            "powershell requires an explicit Windows provider/platform fact, not the default local catalog: {local:?}"
        );
    }

    #[test]
    fn server_executed_surfaces_advertise_server_reflect_tool() {
        let caps = full_server_capabilities_for_tests();
        let web = names(server_builtin_tool_schemas(&caps));
        let remote = names(cli_remote_tool_schemas(Vec::new(), &caps));
        let local = names(cli_local_tool_schemas(
            astra_tools::schemas::all_tool_schemas(),
            Vec::new(),
            &caps,
        ));

        for (surface, names) in [("web", web), ("remote", remote)] {
            assert!(
                names.contains(&"reflect".to_string()),
                "{surface} must expose server-side reflect over persisted/cloud observation data: {names:?}"
            );
        }
        assert!(
            local.contains(&"reflect".to_string()),
            "local CLI must also retain reflect so it can use local/session artifacts: {local:?}"
        );
    }

    #[test]
    fn server_executed_surfaces_hide_reflect_without_reflect_service_capability() {
        let caps = lifecycle_server_capabilities(true, false);
        let web = names(server_builtin_tool_schemas(&caps));
        let remote = names(cli_remote_tool_schemas(Vec::new(), &caps));

        for (surface, names) in [("web", web), ("remote", remote)] {
            assert!(
                !names.contains(&"reflect".to_string()),
                "{surface} must not advertise reflect until the reflect service is configured: {names:?}"
            );
        }
    }

    #[test]
    fn server_lifecycle_capabilities() {
        use astra_turn_core::capability::Capability;

        // AgentSpawner is always included
        let caps = lifecycle_server_capabilities(true, true);
        assert!(
            caps.has(Capability::AgentSpawner),
            "server lifecycle must include AgentSpawner"
        );

        // A database pool is an internal runtime dependency, not a default
        // model-facing SQL capability. SQL/debug tools need an explicit
        // admin/debug provider or policy gate instead of appearing in ordinary
        // web/server agent surfaces.
        assert!(!lifecycle_server_capabilities(false, true).has(Capability::Database));
        assert!(!caps.has(Capability::Database));
        assert!(!lifecycle_server_capabilities(true, false).has(Capability::ReflectService));
        assert!(caps.has(Capability::ReflectService));

        // Web resolve with lifecycle caps advertises agent tool
        let tool_names = names(server_builtin_tool_schemas(&caps));
        assert!(
            tool_names.contains(&"agent".to_string()),
            "production lifecycle must advertise agent tool"
        );

        // full_server_capabilities_for_tests also includes AgentSpawner
        assert!(full_server_capabilities_for_tests().has(Capability::AgentSpawner));
    }

    // ── remote skill validation ──

    #[test]
    fn remote_skill_validation() {
        // Missing instructions → LoadFailed
        let record = SkillRecord {
            skill_id: "empty@1.0.0".to_string(),
            skill_name: "empty".to_string(),
            version: "1.0.0".to_string(),
            description: None,
            metadata: Some(json!({})),
            created_at: None,
        };
        let err = loaded_skill_from_record(record).expect_err("missing instructions must fail");
        assert!(matches!(err, SkillError::LoadFailed(m) if m.contains("metadata.instructions")));

        // Missing token → LoadFailed
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:1", None)
            .expect("test api origin");
        let provider = RemoteSkillCatalogProvider::new(api, std::sync::Arc::new(|| None));
        let err = provider
            .current_token()
            .expect_err("missing token must fail");
        assert!(matches!(err, SkillError::LoadFailed(m) if m.contains("no valid access token")));
    }
}
