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
//! and CLI tool selector from growing separate capability policies.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use astra_services::skills::{SkillListRecord, SkillRecord, SkillService};
use astra_skills::manifest::{
    ExecutionContext, LoadedSkill, SkillManifest, SkillSourceKind, TrustTier,
};
use astra_skills::traits::{SkillError, SkillProvider};

use crate::skills::{BundledSkillProvider, LocalSkillProvider, UnifiedSkillRegistry};

const REMOTE_SKILL_PAGE_SIZE: u32 = 500;
const REMOTE_SKILL_MAX_ROWS: u32 = 5_000;

pub use astra_turn_core::tool_surface::Surface as CapabilitySurface;

/// Source bucket used for deterministic tool visibility and tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ToolCapabilitySource {
    ServerBuiltin,
    ServerMcp,
    ClientBuiltin,
    ClientMcp,
}

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

#[derive(Clone, Debug)]
pub struct ToolSchemaSource {
    pub source: ToolCapabilitySource,
    pub schemas: Vec<Value>,
}

impl ToolSchemaSource {
    pub fn new(source: ToolCapabilitySource, schemas: Vec<Value>) -> Self {
        Self { source, schemas }
    }
}

/// Input for resolving tool schemas for one turn.
#[derive(Clone, Debug)]
pub struct ToolCatalogRequest {
    pub surface: CapabilitySurface,
    pub sources: Vec<ToolSchemaSource>,
}

impl ToolCatalogRequest {
    pub fn new(surface: CapabilitySurface) -> Self {
        Self {
            surface,
            sources: Vec::new(),
        }
    }

    pub fn with_source(mut self, source: ToolCapabilitySource, schemas: Vec<Value>) -> Self {
        self.sources.push(ToolSchemaSource::new(source, schemas));
        self
    }
}

/// Resolve visible tool schemas for an execution surface.
///
/// Dedupe is by function name with first source winning, which makes source
/// precedence explicit and stable. This is not a permissive fallback: a source
/// must be supplied by the caller to participate in resolution.
pub fn resolve_tool_schemas(request: ToolCatalogRequest) -> Vec<Value> {
    let allowed_sources: HashSet<ToolCapabilitySource> = match request.surface {
        CapabilitySurface::Web | CapabilitySurface::CliRemote => [
            ToolCapabilitySource::ServerBuiltin,
            ToolCapabilitySource::ServerMcp,
        ]
        .into_iter()
        .collect(),
        CapabilitySurface::CliLocal => [
            ToolCapabilitySource::ClientBuiltin,
            ToolCapabilitySource::ClientMcp,
        ]
        .into_iter()
        .collect(),
    };

    let mut seen = HashSet::new();
    let mut resolved = Vec::new();
    for source in request.sources {
        if !allowed_sources.contains(&source.source) {
            continue;
        }
        for schema in source.schemas {
            let Some(name) = tool_schema_name(&schema) else {
                continue;
            };
            if seen.insert(name.to_string()) {
                resolved.push(schema);
            }
        }
    }
    resolved
}

/// Full capability set for tests that need the complete catalog. Production
/// callers must derive capabilities from actual service wiring instead.
pub fn full_server_capabilities_for_tests() -> astra_turn_core::capability::CapabilitySet {
    astra_turn_core::capability::CapabilitySet::all()
}

/// Production-truth `CapabilitySet` for an agentic-run lifecycle.
pub fn lifecycle_server_capabilities(
    database_pool_present: bool,
) -> astra_turn_core::capability::CapabilitySet {
    use astra_turn_core::capability::{Capability, CapabilitySet};
    CapabilitySet::empty()
        .with(Capability::AgentSpawner)
        .with(Capability::MemoryService)
        .with_if(database_pool_present, Capability::Database)
        .with(Capability::SkillsCatalog)
        .with(Capability::GitHubAuth)
        .with(Capability::PlanLifecycle)
}

/// Production-truth server `CapabilitySet` derived from [`crate::app_state::AppState`].
pub fn server_capabilities_from(
    state: &crate::app_state::AppState,
) -> astra_turn_core::capability::CapabilitySet {
    lifecycle_server_capabilities(state.shared_pool.is_some())
}

/// Tool schemas for Web agent / server-executed turns.
///
/// Capability-driven (see `astra_turn_core::tool_surface`). The catalog,
/// the `requires` metadata, and the active server `CapabilitySet` together
/// decide what the model sees.
pub fn server_runtime_tool_schemas(
    capabilities: &astra_turn_core::capability::CapabilitySet,
) -> Vec<Value> {
    let mut schemas = astra_turn_core::tool_surface::resolve(
        CapabilitySurface::Web,
        capabilities,
        &astra_tools::schemas::all_tool_schemas(),
    );
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
    let mut pool = client_builtin;
    let mut seen: HashSet<String> = pool
        .iter()
        .filter_map(|schema| tool_schema_name(schema).map(str::to_string))
        .collect();
    for schema in client_mcp {
        if let Some(name) = tool_schema_name(&schema)
            && seen.insert(name.to_string())
        {
            pool.push(schema);
        }
    }
    astra_turn_core::tool_surface::resolve(CapabilitySurface::CliLocal, capabilities, &pool)
}

/// Tool schemas for remote/thin CLI turns. Mirrors Web because execution
/// happens on the API server.
pub fn cli_remote_tool_schemas(
    server_mcp: Vec<Value>,
    capabilities: &astra_turn_core::capability::CapabilitySet,
) -> Vec<Value> {
    let mut pool = astra_tools::schemas::all_tool_schemas();
    let mut seen: HashSet<String> = pool
        .iter()
        .filter_map(|schema| tool_schema_name(schema).map(str::to_string))
        .collect();
    for schema in server_mcp {
        if let Some(name) = tool_schema_name(&schema)
            && seen.insert(name.to_string())
        {
            pool.push(schema);
        }
    }
    astra_turn_core::tool_surface::resolve(CapabilitySurface::CliRemote, capabilities, &pool)
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

fn tool_schema_name(schema: &Value) -> Option<&str> {
    schema
        .get("function")
        .and_then(|function| function.get("name"))
        .and_then(Value::as_str)
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

    async fn list_page(&self, limit: u32, offset: u32) -> Result<SkillListRecord, SkillError> {
        let token = self.current_token()?;
        let body = self
            .api
            .get_skills_query_text(
                &token,
                &[("limit", limit.to_string()), ("offset", offset.to_string())],
            )
            .await
            .map_err(|source| {
                SkillError::LoadFailed(format!(
                    "failed to list remote skill catalog at offset {offset}: {source}"
                ))
            })?;
        serde_json::from_str(&body).map_err(|source| {
            SkillError::ParseFailed(format!(
                "failed to parse remote skill catalog page at offset {offset}: {source}"
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
        let mut offset = 0;
        let mut manifests = Vec::new();
        loop {
            let remaining = REMOTE_SKILL_MAX_ROWS.saturating_sub(offset);
            if remaining == 0 {
                break;
            }
            let limit = remaining.min(REMOTE_SKILL_PAGE_SIZE);
            let page = self.list_page(limit, offset).await?;
            let page_len = page.skills.len() as u32;
            manifests.extend(page.skills.into_iter().map(|item| SkillManifest {
                name: item.skill_name,
                version: item.version.parse().unwrap_or_default(),
                description: item.description.unwrap_or_default(),
                source: SkillSourceKind::Database,
                category: item.category,
                ..Default::default()
            }));
            offset = offset.saturating_add(page_len);
            if page_len < limit || offset as i64 >= page.total {
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

    #[test]
    fn web_and_remote_cli_only_see_server_sources() {
        let sources = vec![
            ToolSchemaSource::new(ToolCapabilitySource::ServerBuiltin, vec![schema("server")]),
            ToolSchemaSource::new(ToolCapabilitySource::ServerMcp, vec![schema("server_mcp")]),
            ToolSchemaSource::new(ToolCapabilitySource::ClientBuiltin, vec![schema("client")]),
            ToolSchemaSource::new(ToolCapabilitySource::ClientMcp, vec![schema("client_mcp")]),
        ];

        let web = names(resolve_tool_schemas(ToolCatalogRequest {
            surface: CapabilitySurface::Web,
            sources: sources.clone(),
        }));
        let remote_cli = names(resolve_tool_schemas(ToolCatalogRequest {
            surface: CapabilitySurface::CliRemote,
            sources,
        }));

        assert_eq!(
            web,
            vec!["server".to_string(), "server_mcp".to_string()],
            "web must not expose client-local tools"
        );
        assert_eq!(
            remote_cli, web,
            "remote/thin CLI capability visibility must match web"
        );
    }

    #[test]
    fn local_cli_only_sees_client_sources() {
        let resolved = names(resolve_tool_schemas(
            ToolCatalogRequest::new(CapabilitySurface::CliLocal)
                .with_source(ToolCapabilitySource::ServerBuiltin, vec![schema("server")])
                .with_source(ToolCapabilitySource::ClientBuiltin, vec![schema("client")])
                .with_source(ToolCapabilitySource::ClientMcp, vec![schema("client_mcp")]),
        ));

        assert_eq!(
            resolved,
            vec!["client".to_string(), "client_mcp".to_string()],
            "local CLI must not claim server-only tools as locally executable"
        );
    }

    #[test]
    fn tool_dedupe_keeps_first_source() {
        let resolved = resolve_tool_schemas(
            ToolCatalogRequest::new(CapabilitySurface::CliLocal)
                .with_source(ToolCapabilitySource::ClientBuiltin, vec![schema("shared")])
                .with_source(ToolCapabilitySource::ClientMcp, vec![schema("shared")]),
        );

        assert_eq!(names(resolved), vec!["shared".to_string()]);
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

    #[test]
    fn real_plan_lifecycle_tools_are_server_only() {
        use astra_turn_core::capability::{Capability, CapabilitySet};
        use astra_turn_core::tool_surface::{Surface, resolve};

        let pool = astra_tools::schemas::all_tool_schemas();
        let without_plan = CapabilitySet::empty()
            .with(Capability::AgentSpawner)
            .with(Capability::MemoryService)
            .with(Capability::Database)
            .with(Capability::SkillsCatalog)
            .with(Capability::GitHubAuth)
            .with(Capability::LSPServer);
        let with_plan = without_plan.clone().with(Capability::PlanLifecycle);

        let local = names(resolve(Surface::CliLocal, &without_plan, &pool));
        let web = names(resolve(Surface::Web, &with_plan, &pool));
        let remote = names(resolve(Surface::CliRemote, &with_plan, &pool));

        assert!(
            !local.contains(&"enter_plan_mode".to_string())
                && !local.contains(&"exit_plan_mode".to_string()),
            "local CLI must not expose plan lifecycle tools without PlanLifecycle"
        );
        assert!(
            web.contains(&"enter_plan_mode".to_string())
                && web.contains(&"exit_plan_mode".to_string()),
            "web must expose the server-owned plan lifecycle tools"
        );
        assert_eq!(
            remote, web,
            "remote/thin CLI should expose the same server-owned lifecycle tools as web"
        );
    }

    #[test]
    fn lifecycle_capabilities_include_agent_spawner() {
        let caps = lifecycle_server_capabilities(true);
        assert!(
            caps.has(astra_turn_core::capability::Capability::AgentSpawner),
            "server lifecycle should advertise AgentSpawner because ServerToolExecutor dispatches agent.spawn"
        );
    }

    #[test]
    fn lifecycle_capabilities_gate_database_on_pool() {
        let without_pool = lifecycle_server_capabilities(false);
        let with_pool = lifecycle_server_capabilities(true);
        assert!(!without_pool.has(astra_turn_core::capability::Capability::Database));
        assert!(with_pool.has(astra_turn_core::capability::Capability::Database));
    }

    #[test]
    fn web_resolve_with_lifecycle_caps_advertises_agent() {
        let caps = lifecycle_server_capabilities(true);
        let names = names(server_runtime_tool_schemas(&caps));
        assert!(
            names.contains(&"agent".to_string()),
            "production lifecycle should advertise agent — got {names:?}"
        );
    }

    #[test]
    fn full_server_capabilities_for_tests_includes_agent_spawner() {
        assert!(
            full_server_capabilities_for_tests()
                .has(astra_turn_core::capability::Capability::AgentSpawner)
        );
    }

    #[test]
    fn remote_skill_record_requires_real_instructions() {
        let record = SkillRecord {
            skill_id: "empty@1.0.0".to_string(),
            skill_name: "empty".to_string(),
            version: "1.0.0".to_string(),
            description: None,
            metadata: Some(json!({})),
            created_at: None,
        };

        let err = loaded_skill_from_record(record).expect_err("missing instructions must fail");
        assert!(
            matches!(err, SkillError::LoadFailed(message) if message.contains("metadata.instructions")),
            "remote provider must not synthesize empty skill bodies"
        );
    }

    #[test]
    fn remote_skill_provider_requires_a_live_token() {
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:1", None)
            .expect("test api origin");
        let provider = RemoteSkillCatalogProvider::new(api, std::sync::Arc::new(|| None));

        let err = provider
            .current_token()
            .expect_err("missing token must be surfaced as a load failure");
        assert!(
            matches!(err, SkillError::LoadFailed(message) if message.contains("no valid access token available")),
            "provider should emit an auth-shaped error when the token provider returns None"
        );
    }
}
