use crate::cli::auth_flow::{parse_auth_tokens, save_refreshed_profile_tokens};
use crate::cli::cli_config::cli_utils::{load_credentials, normalize_model_override, profile_name};
use crate::cli::permission_manager::{PermissionManager, PermissionMode};
use crate::cli::session::session_state::SessionState;
use crate::cli::theme;
use crate::{manifest_loader, mcp_client};
use astra_services::{
    ModelAccessProjectionResponse, ModelDefaultInvalidReason, ModelDefaultResolution,
    ModelListCursor, ModelListItemResponse, ModelListPageResponse, session_journal,
};
#[cfg(test)]
use astra_text_utils::str_preview::prefix_chars;
use crossterm::style::Stylize;
use std::collections::HashSet;

pub(crate) fn create_pipeline_modules(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
) -> PipelineModules {
    create_pipeline_modules_inner(api, profile, true, true, None)
}

pub(crate) fn create_pipeline_modules_quiet(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
) -> PipelineModules {
    create_pipeline_modules_inner(api, profile, false, true, None)
}

/// Build the local interactive baseline and defer every external provider.
/// The TUI event loop owns and supervises convergence through
/// [`discover_external_pipeline_capabilities`].
///
/// `project_root` anchors the local skill registry's project walk-up. Pass the
/// session's resolved workspace root (git root preferred) so skills stay
/// visible even when the process cwd differs from the tool execution workdir.
/// `None` falls back to the process current directory.
pub(crate) fn create_tui_pipeline_modules(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    project_root: Option<&std::path::Path>,
) -> PipelineModules {
    create_pipeline_modules_inner(api, profile, false, false, project_root)
}

/// Resolve the astra server base URL. Returns `None` when no server
/// is configured (offline mode).
pub(crate) fn resolve_cloud_base() -> Option<String> {
    std::env::var("ASTRA_API_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim_end_matches('/').to_string())
}

/// Replace the task manager's store (used once at startup when we upgrade
/// from the synchronous in-memory fallback to an API-backed durable store).
/// The new manager inherits the current session_id.
/// Shared runtime modules created during pipeline construction.
///
/// Holds the unified skill registry, MCP client manager, and skill-watcher
/// handle so the REPL can wire them into its shared state after startup.
pub(crate) struct PipelineModules {
    /// Unified skill registry (single source of truth for all skill resolution).
    pub unified_skill_registry: std::sync::Arc<astra_runtime::skills::UnifiedSkillRegistry>,
    /// MCP client manager for external tool servers.
    pub mcp_manager: std::sync::Arc<tokio::sync::RwLock<mcp_client::McpClientManager>>,
    /// File-system watcher for skill hot-reload (kept alive while REPL runs).
    pub _skill_watcher: Option<astra_runtime::skills::watcher::SkillWatcherHandle>,
}

#[derive(Debug)]
pub(crate) struct McpConnectionFailure {
    pub name: String,
    pub error: String,
}

#[derive(Debug)]
pub(crate) struct ExternalPipelineDiscoveryReport {
    pub skills:
        Result<astra_runtime::skills::SkillDiscoveryReport, astra_runtime::skills::SkillError>,
    pub mcp_failures: Vec<McpConnectionFailure>,
}

/// Format an [`McpError`] as a concise user-facing message without redundant
/// server-name prefixes (we print the server name separately).
fn format_mcp_error(error: &crate::mcp_client::McpError) -> String {
    match error {
        crate::mcp_client::McpError::InvalidConfig(msg) => format!("invalid configuration — {msg}"),
        crate::mcp_client::McpError::Spawn(msg) => format!("failed to start — {msg}"),
        crate::mcp_client::McpError::Initialize(msg) => {
            // Initialize errors embed the server name ("xxx: <reason>");
            // strip it since we already print the name in the output line.
            if let Some((_name, rest)) = msg.split_once(": ") {
                rest.to_string()
            } else {
                msg.clone()
            }
        }
        crate::mcp_client::McpError::Service(e) => format!("service error — {e}"),
        crate::mcp_client::McpError::ToolNotFound(tool) => format!("tool '{tool}' not found"),
        crate::mcp_client::McpError::ServerNotConnected(name) => {
            format!("server '{name}' is not connected")
        }
        crate::mcp_client::McpError::ConnectionLost(_name, reason) => {
            format!("connection lost — {reason}")
        }
        crate::mcp_client::McpError::ReconnectionFailed(_name, attempts) => {
            format!("all {attempts} reconnection attempts failed")
        }
    }
}

async fn connect_mcp_configs(
    configs: Vec<crate::mcp_client::McpServerConfig>,
    manager: &std::sync::Arc<tokio::sync::RwLock<mcp_client::McpClientManager>>,
    registry: &std::sync::Arc<astra_runtime::skills::UnifiedSkillRegistry>,
) -> Vec<(String, Result<(), String>)> {
    let mut results = Vec::with_capacity(configs.len());
    for config in configs {
        let name = config.name.clone();
        let result = mcp_client::connect_and_discover_skills(manager, config, registry)
            .await
            .map(|_| ())
            .map_err(|error| format_mcp_error(&error));
        results.push((name, result));
    }
    results
}

/// Converge server skills and configured MCP providers without blocking the
/// interactive event loop. The caller owns the task lifecycle and presents the
/// typed report on its own UI surface.
pub(crate) async fn discover_external_pipeline_capabilities(
    registry: std::sync::Arc<astra_runtime::skills::UnifiedSkillRegistry>,
    manager: std::sync::Arc<tokio::sync::RwLock<mcp_client::McpClientManager>>,
) -> ExternalPipelineDiscoveryReport {
    let config_result =
        tokio::task::spawn_blocking(manifest_loader::collect_mcp_server_configs).await;
    let mut mcp_failures = Vec::new();
    match config_result {
        Ok(configs) => {
            for (name, result) in connect_mcp_configs(configs, &manager, &registry).await {
                if let Err(error) = result {
                    mcp_failures.push(McpConnectionFailure { name, error });
                }
            }
        }
        Err(error) => mcp_failures.push(McpConnectionFailure {
            name: "configuration".into(),
            error: format!("MCP configuration discovery task stopped: {error}"),
        }),
    }

    ExternalPipelineDiscoveryReport {
        skills: registry.discover_all_report().await,
        mcp_failures,
    }
}

fn create_pipeline_modules_inner(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    announce_skills: bool,
    connect_external: bool,
    project_root: Option<&std::path::Path>,
) -> PipelineModules {
    // Selector removed — the runtime now builds the turn-specific tool surface
    // directly from the local CLI catalog plus any mounted server/MCP schemas.

    // Initialize the local CLI capability catalog.
    //
    // Local project/home skills remain CLI-local. If the user is authenticated
    // against an API server, we also mount that server's visible catalog
    // (server HOME skills + database skills visible to this user). That keeps
    // CLI and Web aligned for shared server capabilities without pretending
    // that project-local CLI skills are available to Web sessions.
    // Always install the remote catalog provider. It reads tokens at
    // call time, so sessions can recover after env-token expiry/login
    // without requiring a full CLI restart to rebuild pipeline modules.
    let profile_owned = profile.map(str::to_string);
    let token_provider: astra_runtime::capabilities::TokenProvider =
        std::sync::Arc::new(move || current_access_token(profile_owned.as_deref()));
    let remote_catalog = Some(
        astra_runtime::capabilities::RemoteSkillCatalogProvider::new(api.clone(), token_provider),
    );
    let unified_skill_registry = if connect_external {
        astra_runtime::capabilities::build_cli_local_skill_registry(remote_catalog, project_root)
    } else {
        astra_runtime::capabilities::build_cli_local_skill_registry_bootstrap(
            remote_catalog,
            project_root,
        )
    };
    let handle = tokio::runtime::Handle::current();

    // Initialize MCP client manager and connect any MCP servers declared in
    // skill manifests. This registers `skill://` resources from connected
    // servers into the unified skill registry.
    let mcp_manager =
        std::sync::Arc::new(tokio::sync::RwLock::new(mcp_client::McpClientManager::new()));

    let workspace_root = project_root
        .map(std::path::Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok());

    // MCP roots, skill discovery, and the watcher must describe the same
    // workspace.  Deriving them independently from process cwd lets a session
    // discover one catalog at startup and watch a different one afterwards.
    {
        if let Some(root_path) = workspace_root.as_deref() {
            let uri = format!("file://{}", root_path.display());
            let root = rmcp::model::Root::new(uri).with_name("workspace");
            tokio::task::block_in_place(|| {
                handle.block_on(async {
                    let roots = mcp_manager.read().await.roots().clone();
                    roots.write().await.push(root);
                })
            });
        }
    }

    if connect_external {
        let mcp_configs = manifest_loader::collect_mcp_server_configs();
        if !mcp_configs.is_empty() {
            let mgr = mcp_manager.clone();
            let reg = unified_skill_registry.clone();
            let _ = std::thread::scope(|s| {
                s.spawn(|| {
                    handle.block_on(async {
                        let results = connect_mcp_configs(mcp_configs, &mgr, &reg).await;

                        if announce_skills {
                            // Count MCP tools per server (0 is fine — e.g. memoria).
                            let manager = mgr.read().await;
                            let mut tool_counts: std::collections::HashMap<String, usize> =
                                std::collections::HashMap::new();
                            for (server, _) in manager.all_tools() {
                                *tool_counts.entry(server.to_string()).or_insert(0) += 1;
                            }

                            let ok: Vec<&str> = results
                                .iter()
                                .filter_map(|(n, r)| r.is_ok().then_some(n.as_str()))
                                .collect();
                            let failures: Vec<(&str, &str)> = results
                                .iter()
                                .filter_map(|(n, r)| {
                                    r.as_ref().err().map(|e| (n.as_str(), e.as_str()))
                                })
                                .collect();

                            // ✓ N MCP server(s) connected: name1 (12)  ·  name2  ·  name3 (5)
                            if !ok.is_empty() {
                                let list = ok
                                    .iter()
                                    .map(|name| {
                                        match tool_counts.get(*name).copied().unwrap_or(0) {
                                            0 => name.to_string(),
                                            n => format!("{name} ({n})"),
                                        }
                                    })
                                    .collect::<Vec<_>>()
                                    .join("  ·  ");
                                eprintln!(
                                    "  {} {} MCP server{} connected: {}",
                                    theme::icon_ok(),
                                    ok.len(),
                                    if ok.len() == 1 { "" } else { "s" },
                                    list,
                                );
                            }

                            // ✗ bad-server: <reason>  (one line per failure)
                            for (name, err) in &failures {
                                eprintln!("  {} MCP '{}': {}", theme::icon_err(), name, err,);
                            }
                        }
                    })
                })
                .join()
                .unwrap_or_else(|e| {
                    if announce_skills {
                        eprintln!(
                            "  {} MCP connection thread panicked: {e:?}",
                            theme::icon_err()
                        )
                    }
                })
            });
        }
    }

    // Start file-system watcher for skill hot-reload
    let skill_watcher = astra_runtime::skills::watcher::start_watching(
        unified_skill_registry.clone(),
        workspace_root.as_deref().map_or_else(
            astra_skills::loader::skill_search_paths,
            astra_skills::loader::skill_search_paths_from_root,
        ),
    );

    let creds = load_credentials();
    let name = profile_name(profile, &creds);
    let _token = creds
        .profiles
        .get(&name)
        .and_then(|p| p.access_token.as_ref())
        .cloned();

    PipelineModules {
        unified_skill_registry,
        mcp_manager,
        _skill_watcher: skill_watcher,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServerModelSelection {
    pub name: String,
    pub context_window: Option<u32>,
    pub offering_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ServerDefaultModel {
    Selected(ServerModelSelection),
    NoModels,
    Unavailable,
}

fn model_list_entry_is_active(entry: &ModelListItemResponse) -> bool {
    entry.is_active
}

fn model_list_entry_name(entry: &ModelListItemResponse) -> Option<&str> {
    let name = entry.name.trim();
    (!name.is_empty()).then_some(name)
}

pub(crate) fn model_list_entry_context_window(entry: &ModelListItemResponse) -> Option<u32> {
    u32::try_from(entry.context_window)
        .ok()
        .filter(|value| *value > 0)
}

fn model_selection_from_list_entry(entry: &ModelListItemResponse) -> Option<ServerModelSelection> {
    let offering_id = entry.offering_id.as_str();
    if offering_id.is_empty() || offering_id.trim() != offering_id {
        return None;
    }
    Some(ServerModelSelection {
        name: model_list_entry_name(entry)?.to_string(),
        context_window: model_list_entry_context_window(entry),
        offering_id: offering_id.to_string(),
    })
}

pub(crate) async fn load_server_model_catalog(
    api: &astra_thin_client::ThinClient,
    token: &str,
) -> Result<(Vec<ModelListItemResponse>, String), String> {
    let mut cursor: Option<ModelListCursor> = None;
    let mut items = Vec::new();
    let mut total = None;
    let mut revision = None;
    let mut seen_cursors = HashSet::new();
    loop {
        if let Some(current) = &cursor {
            if !seen_cursors.insert(current.clone()) {
                return Err("server model registry cycled its continuation cursor".to_string());
            }
        }
        let cursor_tuple = cursor.as_ref().map(|value| {
            (
                value.provider.as_str(),
                value.model_name.as_str(),
                value.model_id.as_str(),
            )
        });
        let response = api
            .get_models_page_response_timeout(
                token,
                std::time::Duration::from_secs(3),
                cursor_tuple,
            )
            .await
            .map_err(|error| format!("failed to load server model registry: {error}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "server model registry request failed with status {}",
                response.status()
            ));
        }
        let page: ModelListPageResponse = response.json().await.map_err(|error| {
            format!("server model registry response was not valid JSON: {error}")
        })?;
        if page.limit == 0 || page.limit > 200 {
            return Err("server model registry returned an invalid page limit".to_string());
        }
        if total.get_or_insert(page.total) != &page.total {
            return Err("server model registry changed total during pagination".to_string());
        }
        if revision.get_or_insert_with(|| page.catalog_revision.clone()) != &page.catalog_revision {
            return Err("server model registry changed revision during pagination".to_string());
        }
        let page_had_items = !page.items.is_empty();
        items.extend(page.items);
        let Some(next) = page.next_cursor else {
            if items.len() != page.total as usize {
                return Err(format!(
                    "server model registry ended with {} items but advertised {}",
                    items.len(),
                    page.total
                ));
            }
            return Ok((items, revision.unwrap_or_default()));
        };
        if !page_had_items {
            return Err("server model registry returned a cursor without items".to_string());
        }
        if cursor.as_ref() == Some(&next) {
            return Err("server model registry repeated its continuation cursor".to_string());
        }
        cursor = Some(next);
    }
}

pub(crate) async fn load_server_model_catalog_json(
    api: &astra_thin_client::ThinClient,
    token: &str,
) -> Result<String, String> {
    let (items, revision) = load_server_model_catalog(api, token).await?;
    serde_json::to_string_pretty(&ModelListPageResponse {
        total: items.len() as u32,
        limit: 200,
        items,
        next_cursor: None,
        catalog_revision: revision,
    })
    .map_err(|error| format!("failed to render complete model catalog: {error}"))
}

async fn load_server_model_access(
    api: &astra_thin_client::ThinClient,
    token: &str,
) -> Result<ModelAccessProjectionResponse, String> {
    let mut cursor: Option<ModelListCursor> = None;
    let mut first: Option<ModelAccessProjectionResponse> = None;
    let mut expected_revision = None;
    let mut expected_total = None;
    let mut seen_cursors = HashSet::new();
    loop {
        if let Some(current) = &cursor {
            if !seen_cursors.insert(current.clone()) {
                return Err("Model Access projection cycled its continuation cursor".to_string());
            }
        }
        let cursor_tuple = cursor.as_ref().map(|value| {
            (
                value.provider.as_str(),
                value.model_name.as_str(),
                value.model_id.as_str(),
            )
        });
        let response = api
            .get_model_access_page_response_timeout(
                token,
                std::time::Duration::from_secs(3),
                cursor_tuple,
            )
            .await
            .map_err(|error| format!("failed to load Model Access projection: {error}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "Model Access projection request failed with status {}",
                response.status()
            ));
        }
        let page: ModelAccessProjectionResponse = response
            .json()
            .await
            .map_err(|error| format!("Model Access projection was not valid JSON: {error}"))?;
        if page.limit == 0 || page.limit > 200 {
            return Err("Model Access projection returned an invalid page limit".to_string());
        }
        if expected_total.get_or_insert(page.total) != &page.total {
            return Err("Model Access projection changed total during pagination".to_string());
        }
        if expected_revision.get_or_insert_with(|| page.catalog_revision.clone())
            != &page.catalog_revision
        {
            return Err(
                "Model Access projection changed catalog revision during pagination".to_string(),
            );
        }
        let page_had_offerings = !page.offerings.is_empty();
        if page.next_cursor.is_some() && !page_had_offerings {
            return Err("Model Access projection returned a cursor without offerings".to_string());
        }
        if let Some(existing) = &mut first {
            if existing.accesses != page.accesses {
                return Err(
                    "Model Access projection changed access declarations during pagination"
                        .to_string(),
                );
            }
            existing.offerings.extend(page.offerings);
            existing.next_cursor = page.next_cursor.clone();
        } else {
            first = Some(page);
        }
        let next = first.as_ref().and_then(|page| page.next_cursor.clone());
        let Some(next) = next else {
            let projection =
                first.ok_or_else(|| "Model Access projection returned no page".to_string())?;
            if projection.offerings.len() != projection.total as usize {
                return Err(format!(
                    "Model Access projection ended with {} offerings but advertised {}",
                    projection.offerings.len(),
                    projection.total
                ));
            }
            for access in &projection.accesses {
                let count = projection
                    .offerings
                    .iter()
                    .filter(|offering| offering.access_id == access.id)
                    .count() as u32;
                if count != access.available_model_count {
                    return Err(format!(
                        "Model Access count for '{}' advertised {} but drained {}",
                        access.id, access.available_model_count, count
                    ));
                }
            }
            return Ok(projection);
        };
        if cursor.as_ref() == Some(&next) {
            return Err("Model Access projection repeated its continuation cursor".to_string());
        }
        cursor = Some(next);
    }
}

pub(crate) fn default_model_selection_from_access(
    projection: &ModelAccessProjectionResponse,
) -> Result<Option<ServerModelSelection>, String> {
    match projection.default_resolution.as_ref() {
        Some(ModelDefaultResolution::Invalid { reason }) => {
            return Err(format!(
                "Model Access rejected the provider default because {}; choose an available model explicitly",
                model_default_invalid_reason_message(*reason)
            ));
        }
        Some(ModelDefaultResolution::Missing) => {
            return if projection.offerings.is_empty() {
                Ok(None)
            } else {
                Err(
                    "Model Access has no resolved default for a non-empty effective catalog"
                        .to_string(),
                )
            };
        }
        Some(ModelDefaultResolution::Selected { offering_id, .. })
            if projection.default_offering_id.as_deref() != Some(offering_id) =>
        {
            return Err(
                "Model Access default resolution disagrees with default_offering_id".to_string(),
            );
        }
        Some(ModelDefaultResolution::Selected { .. }) => {}
        None => {
            return Err("Model Access omitted required default_resolution".to_string());
        }
    }
    let Some(default_offering_id) = projection.default_offering_id.as_deref() else {
        return if projection.offerings.is_empty() {
            Ok(None)
        } else {
            Err("Model Access omitted its default for a non-empty effective catalog".to_string())
        };
    };
    let entry = projection
        .offerings
        .iter()
        .find(|entry| entry.offering_id == default_offering_id)
        .ok_or_else(|| {
            "Model Access default does not reference an effective Offering".to_string()
        })?;
    if !model_list_entry_is_active(entry) {
        return Err("Model Access default references an inactive Offering".to_string());
    }
    model_selection_from_list_entry(entry)
        .map(Some)
        .ok_or_else(|| "Model Access default has invalid selection metadata".to_string())
}

fn model_default_invalid_reason_message(reason: ModelDefaultInvalidReason) -> &'static str {
    match reason {
        ModelDefaultInvalidReason::InvalidOfferingId => "its offering identity is invalid",
        ModelDefaultInvalidReason::NotEffectiveOffering => {
            "it is not part of the effective model catalog"
        }
    }
}

pub(crate) fn model_selection_for_name_from_catalog(
    models: &[ModelListItemResponse],
    model: &str,
) -> Option<ServerModelSelection> {
    let registry_model = astra_turn_core::thinking_config::resolve_model_thinking(model).0;
    models
        .iter()
        .filter(|entry| model_list_entry_is_active(entry))
        .find(|entry| {
            model_list_entry_name(entry)
                .is_some_and(|name| name.eq_ignore_ascii_case(registry_model))
        })
        .and_then(model_selection_from_list_entry)
}

pub(crate) fn model_selection_for_offering_from_catalog(
    models: &[ModelListItemResponse],
    offering_id: &str,
) -> Option<ServerModelSelection> {
    models
        .iter()
        .filter(|entry| model_list_entry_is_active(entry))
        .find(|entry| entry.offering_id == offering_id)
        .and_then(model_selection_from_list_entry)
}

pub(crate) async fn resolve_server_model_selection(
    api: &astra_thin_client::ThinClient,
    token: &str,
    model: &str,
) -> Result<ServerModelSelection, String> {
    let (catalog, _) = load_server_model_catalog(api, token).await?;
    resolve_server_model_selection_from_catalog(api, token, model, &catalog).await
}

/// Resolve an explicit selector against the complete authoritative catalog.
/// The server catalog is paginated but this function only receives the fully
/// drained projection, so a missing entry is a definitive admission failure.
pub(crate) async fn resolve_server_model_selection_from_catalog(
    api: &astra_thin_client::ThinClient,
    token: &str,
    model: &str,
    catalog: &[ModelListItemResponse],
) -> Result<ServerModelSelection, String> {
    if let Some(selection) = model_selection_for_name_from_catalog(catalog, model) {
        return Ok(selection);
    }
    let _ = (api, token);
    Err(format!(
        "model '{model}' is not an active Server Offering in the authoritative catalog"
    ))
}

fn model_selection_from_exact_response(
    requested_model: &str,
    body: &str,
) -> Result<ServerModelSelection, String> {
    let response: serde_json::Value = serde_json::from_str(body)
        .map_err(|error| format!("exact model response was not valid JSON: {error}"))?;
    let name = response
        .get("name")
        .and_then(serde_json::Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| "exact model response omitted name".to_string())?;
    if name != requested_model {
        return Err(format!(
            "exact model response name mismatch: requested '{requested_model}', got '{name}'"
        ));
    }
    if !response
        .get("is_active")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        return Err(format!(
            "model '{requested_model}' is not an active Server Offering"
        ));
    }
    let offering_id = response
        .get("model_id")
        .and_then(serde_json::Value::as_str)
        .filter(|offering_id| !offering_id.trim().is_empty())
        .ok_or_else(|| "exact model response omitted model_id".to_string())?;
    let context_window = response
        .get("context_window")
        .and_then(serde_json::Value::as_i64)
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value > 0);

    Ok(ServerModelSelection {
        name: name.to_string(),
        context_window,
        offering_id: offering_id.to_string(),
    })
}

pub(crate) async fn resolve_server_offering_selection(
    api: &astra_thin_client::ThinClient,
    token: &str,
    offering_id: &str,
) -> Result<ServerModelSelection, String> {
    let (catalog, _) = load_server_model_catalog(api, token).await?;
    model_selection_for_offering_from_catalog(&catalog, offering_id)
        .ok_or_else(|| format!("Offering '{offering_id}' is not active in the Server catalog"))
}

/// Resolve the Server-governed default Offering when the user did not choose
/// one. Catalog failure remains distinct from a valid empty catalog.
pub(crate) async fn resolve_server_default_model(
    api: &astra_thin_client::ThinClient,
    token: &str,
) -> ServerDefaultModel {
    tracing::debug!(
        target: "astra_cli::model_selection",
        "resolve_server_default_model: calling GET /model-access"
    );
    let projection = match load_server_model_access(api, token).await {
        Ok(projection) => projection,
        Err(error) => {
            tracing::warn!(
                target: "astra_cli::model_selection",
                %error,
                "resolve_server_default_model: failed to load complete Model Access projection → Unavailable"
            );
            return ServerDefaultModel::Unavailable;
        }
    };
    let result = match default_model_selection_from_access(&projection) {
        Ok(Some(selection)) => ServerDefaultModel::Selected(selection),
        Ok(None) => ServerDefaultModel::NoModels,
        Err(error) => {
            tracing::warn!(
                target: "astra_cli::model_selection",
                %error,
                catalog_revision = %projection.catalog_revision,
                "resolve_server_default_model: invalid Model Access projection"
            );
            ServerDefaultModel::Unavailable
        }
    };
    tracing::debug!(
        target: "astra_cli::model_selection",
        result = ?result,
        "resolve_server_default_model: completed"
    );
    result
}

pub(crate) async fn ensure_state_default_model(
    api: &astra_thin_client::ThinClient,
    token: &str,
    state: &mut SessionState,
) -> Option<String> {
    if let Some(model) = normalize_model_override(state.model.as_deref()).map(str::to_string) {
        match resolve_server_model_selection(api, token, &model).await {
            Ok(selection) => {
                crate::cli::slash::slash_config::set_active_offering_id_for_request(Some(
                    selection.offering_id,
                ));
                if let Some(context_window) = selection.context_window {
                    state.context_budget = astra_runtime::prompts::ContextBudget::from_runtime_config_with_context_window(
                        &state.runtime_config,
                        Some(&model),
                        Some(context_window),
                    );
                }
            }
            Err(error) => {
                tracing::warn!(
                    target: "astra_cli::model_selection",
                    model,
                    error = %error,
                    "could not resolve explicit model context_window from server registry"
                );
                eprintln!("warning: {error}; keeping current context budget");
            }
        }
        return Some(model);
    }
    match resolve_server_default_model(api, token).await {
        ServerDefaultModel::Selected(selection) => {
            state.model = Some(selection.name.clone());
            crate::cli::slash::slash_config::set_active_offering_id_for_request(Some(
                selection.offering_id.clone(),
            ));
            if let Some(context_window) = selection.context_window {
                state.context_budget =
                    astra_runtime::prompts::ContextBudget::from_runtime_config_with_context_window(
                        &state.runtime_config,
                        Some(&selection.name),
                        Some(context_window),
                    );
            } else {
                tracing::warn!(
                    target: "astra_cli::model_selection",
                    model = %selection.name,
                    "server default model is missing positive context_window metadata"
                );
                eprintln!(
                    "warning: model '{}' is missing positive context_window metadata; keeping current context budget",
                    selection.name
                );
            }
            Some(selection.name)
        }
        ServerDefaultModel::NoModels | ServerDefaultModel::Unavailable => None,
    }
}

/// Outcome of `try_refresh_token` for deciding whether on-disk credentials may still be valid.
#[derive(Debug)]
enum SilentRefreshError {
    Thin(astra_thin_client::ThinClientError),
    /// HTTP 200 body was not usable; keep existing tokens.
    BadResponse(String),
    /// New tokens could not be written; do not clear the file.
    SaveFailed(String),
}

impl SilentRefreshError {
    fn keep_credentials(&self) -> bool {
        match self {
            SilentRefreshError::Thin(e) => should_keep_credentials_on_refresh_error(e),
            SilentRefreshError::BadResponse(_) | SilentRefreshError::SaveFailed(_) => true,
        }
    }
}

/// Best-effort silent auth: validate existing token or try refresh.
/// Never blocks or prompts — just ensures credentials are fresh if possible.
/// Clears credentials only when the server definitively rejects auth (after handling
/// refresh-token rotation races — see `recover_credentials_after_refresh_race`).
pub(crate) async fn try_silent_auth(api: &astra_thin_client::ThinClient, profile: Option<&str>) {
    // When gateway provides a pre-validated token via env, skip auth entirely —
    // the gateway owns token lifecycle and the HTTP round-trip is wasteful.
    if std::env::var("ASTRA_ACCESS_TOKEN")
        .map(|t| !t.is_empty())
        .unwrap_or(false)
    {
        return;
    }
    let creds = load_credentials();
    let name = profile_name(profile, &creds);
    let prof = creds.profiles.get(&name);

    // Try existing access_token
    if let Some(token) = prof.and_then(|p| p.access_token.as_ref()) {
        match api
            .get_auth_me_text_timeout(token, std::time::Duration::from_secs(3))
            .await
        {
            Ok(resp) if resp.status().is_success() => return,
            Ok(resp) if resp.status().as_u16() == 401 => {
                // 401 Unauthorized — token is definitely invalid, try refresh
            }
            _ => return, // Network error, proxy error, 5xx, other 4xx: keep cached creds
        }
    } else {
        return;
    }

    let Some(refresh) = prof.and_then(|p| p.refresh_token.as_ref()) else {
        return;
    };
    let refresh_str = refresh.as_str();

    match try_refresh_token(api, profile, refresh_str).await {
        Ok(()) => {
            eprintln!("  {} Token refreshed", theme::icon_ok());
        }
        Err(err) => {
            // Do not wipe local creds on transport failures, 5xx, or malformed JSON —
            // the access/refresh pair on disk may still be valid.
            if err.keep_credentials() {
                return;
            }
            // Another CLI may have won refresh first (server revokes old refresh on success).
            if recover_credentials_after_refresh_race(api, profile, refresh_str).await {}
        }
    }

    // Never wipe credentials in try_silent_auth — it's a best-effort check.
    // If both access_token validation and refresh fail, the user still has
    // their saved credentials for the next explicit login or refresh attempt.
    // Wiping here causes repeated "not logged in" after proxy/network issues.
}

fn should_keep_credentials_on_refresh_error(err: &astra_thin_client::ThinClientError) -> bool {
    match err {
        astra_thin_client::ThinClientError::Http(_) => true,
        astra_thin_client::ThinClientError::Json(_) => true,
        astra_thin_client::ThinClientError::Api { status, .. } => status.is_server_error(),
        _ => false,
    }
}

/// After `POST /auth/refresh` fails with 4xx, re-read credentials: another process may have
/// rotated the refresh token and saved new tokens while we still held the old refresh value.
async fn recover_credentials_after_refresh_race(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    attempted_refresh: &str,
) -> bool {
    let creds = load_credentials();
    let name = profile_name(profile, &creds);
    let Some(prof) = creds.profiles.get(&name) else {
        return false;
    };
    if let Some(tok) = prof.access_token.as_ref() {
        if let Ok(resp) = api
            .get_auth_me_text_timeout(tok, std::time::Duration::from_secs(3))
            .await
            && resp.status().is_success()
        {
            return true;
        }
    }
    if let Some(r) = prof.refresh_token.as_ref()
        && r.as_str() != attempted_refresh
        && try_refresh_token(api, profile, r.as_str()).await.is_ok()
    {
        eprintln!("  {} Token refreshed", theme::icon_ok());
        return true;
    }
    false
}

/// Try to refresh an expired access token using the stored refresh_token.
async fn try_refresh_token(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    refresh_token: &str,
) -> Result<(), SilentRefreshError> {
    let body = api
        .post_auth_refresh_json(&serde_json::json!({ "refresh_token": refresh_token }))
        .await
        .map_err(SilentRefreshError::Thin)?;
    let tokens = parse_auth_tokens(&body)
        .map_err(|error| SilentRefreshError::BadResponse(format!("refresh response: {error}")))?;
    save_refreshed_profile_tokens(profile, &tokens).map_err(SilentRefreshError::SaveFailed)?;
    Ok(())
}

/// Attempt to refresh an expired token mid-session.
///
/// Returns `true` (and persists new credentials) on success.
/// On failure, logs the underlying reason before returning `false`, so users
/// can see why a refresh did not recover their session.
pub(crate) async fn attempt_token_refresh(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
) -> bool {
    let creds = load_credentials();
    let name = profile_name(profile, &creds);
    let Some(refresh) = creds
        .profiles
        .get(&name)
        .and_then(|p| p.refresh_token.as_ref())
    else {
        astra_core::agent_warn!(
            "auth",
            "token refresh skipped: no refresh_token stored for profile '{name}'"
        );
        return false;
    };
    let refresh_str = refresh.clone();
    drop(creds);
    match try_refresh_token(api, profile, &refresh_str).await {
        Ok(()) => true,
        Err(err) => {
            if err.keep_credentials() {
                astra_core::agent_warn!(
                    "auth",
                    "token refresh failed (credentials preserved): {err:?}"
                );
                return false;
            }
            astra_core::agent_warn!(
                "auth",
                "token refresh failed, attempting race-recovery path: {err:?}"
            );
            let recovered =
                recover_credentials_after_refresh_race(api, profile, &refresh_str).await;
            if !recovered {
                astra_core::agent_warn!(
                    "auth",
                    "token refresh race-recovery also failed for profile '{name}'"
                );
            }
            recovered
        }
    }
}

const ACCESS_TOKEN_REFRESH_SKEW_SECS: i64 = 60;

fn jwt_expiry_epoch(token: &str) -> Option<i64> {
    use base64::Engine;

    let payload = token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
        .ok()?;
    let value: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    value.get("exp").and_then(|v| {
        v.as_i64()
            .or_else(|| v.as_u64().and_then(|n| i64::try_from(n).ok()))
    })
}

fn access_token_needs_refresh(token: &str, now_epoch: i64) -> bool {
    jwt_expiry_epoch(token)
        .map(|exp| exp <= now_epoch + ACCESS_TOKEN_REFRESH_SKEW_SECS)
        .unwrap_or(false)
}

fn active_env_access_token(now_epoch: i64) -> Option<String> {
    let token = std::env::var("ASTRA_ACCESS_TOKEN").ok()?;
    if token.is_empty() || access_token_needs_refresh(&token, now_epoch) {
        return None;
    }
    Some(token)
}

pub(crate) async fn fresh_access_token(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
) -> Option<String> {
    let now = chrono::Utc::now().timestamp();
    if let Some(token) = active_env_access_token(now) {
        tracing::debug!(
            target: "astra_cli::auth",
            "fresh_access_token: using ASTRA_ACCESS_TOKEN env var (valid, not expired)"
        );
        return Some(token);
    }
    tracing::debug!(
        target: "astra_cli::auth",
        "fresh_access_token: ASTRA_ACCESS_TOKEN env var absent/expired, falling back to profile credentials"
    );

    let creds = load_credentials();
    let name = profile_name(profile, &creds);
    let profile_entry = creds.profiles.get(&name)?;
    let access = profile_entry.access_token.clone();
    let has_refresh = profile_entry.refresh_token.is_some();
    drop(creds);

    if let Some(token) = access {
        if !access_token_needs_refresh(&token, now) {
            return Some(token);
        }
        if !has_refresh {
            return None;
        }
        if attempt_token_refresh(api, profile).await {
            return current_access_token(profile).filter(|fresh| {
                !access_token_needs_refresh(fresh, chrono::Utc::now().timestamp())
            });
        }
        return None;
    }

    if has_refresh && attempt_token_refresh(api, profile).await {
        return current_access_token(profile)
            .filter(|fresh| !access_token_needs_refresh(fresh, chrono::Utc::now().timestamp()));
    }

    None
}

pub(crate) fn initialize_session_state(
    profile: Option<&str>,
    initial_model: Option<&str>,
    cli_context: &crate::cli::cli_config::cli_context::CliContext,
) -> SessionState {
    let mut state = SessionState::default();
    state.cli_context = cli_context.clone();
    let project_root = std::env::current_dir().unwrap_or_default();
    state.perm_manager = match cli_context.permission_mode.as_deref() {
        Some(mode) => PermissionManager::with_workspace_trust_mode(
            mode.parse::<PermissionMode>()
                .unwrap_or_else(|err| panic!("invalid permission mode in CliContext: {err}")),
            &project_root,
        ),
        None => PermissionManager::with_workspace_trust(cli_context.auto_approve, &project_root),
    };
    if let Some(session_id) = &cli_context.session_id {
        state.set_session_id(session_id.clone());
    }
    state.session_name = cli_context.session_name.clone();
    state.pending_recovery = detect_pending_recovery_session(profile);
    if state.cli_context.session_id.is_some() {
        state.pending_recovery = None;
    }
    if let Some(m) = normalize_model_override(initial_model) {
        state.model = Some(m.to_string());
    }

    // Initialize observability hub for M1-M6 integration
    // Use persistent storage under ~/.astra/observability for user profiles
    let obs_path = astra_runtime_env::local_state_root().join("observability");
    state.observability_hub = Some(std::sync::Arc::new(
        astra_runtime::observability::ObservabilityHub::with_storage(obs_path),
    ));

    // Create an early observability session so the very first turn can record
    // context assembly traces.  The session keeps this "pending" ID for its
    // lifetime — handle_turn_result skips re-creation since is_none() is false.
    if let Some(ref hub) = state.observability_hub {
        let user_id = state
            .ingestion_user_id
            .clone()
            .unwrap_or_else(|| "anonymous".to_string());
        state.observability_session = Some(hub.start_session(&user_id, "pending"));
        // Apply any adaptive state stashed during workspace restore.
        super::session_startup::apply_pending_adaptive_state(&mut state);
    }

    state
}

fn detect_pending_recovery_session(cli_profile: Option<&str>) -> Option<String> {
    let session_id = crate::cli::cli_config::cli_utils::stored_last_session_id(cli_profile)?;
    match astra_services::session_workspace::read_workspace_optional(&session_id) {
        Ok(Some(workspace)) => {
            if workspace.status.eq_ignore_ascii_case("completed") {
                return None;
            }
            workspace_matches_current_project(&workspace).then_some(session_id)
        }
        Ok(None) => crate::cli::cli_config::cli_utils::local_session_is_resumable(&session_id)
            .then_some(session_id),
        Err(error) => {
            eprintln!(
                "  ⚠ workspace read failed while checking pending recovery for {session_id}: {error}"
            );
            Some(session_id)
        }
    }
}

fn pending_recovery_status_line(state: &SessionState) -> Option<String> {
    state.pending_recovery.as_ref().map(|session_id| {
        let mut line = "previous session available via /resume".to_string();
        match astra_services::session_workspace::read_workspace_optional(session_id) {
            Ok(Some(workspace)) => {
                if let Some(error) = workspace
                    .last_persistence_error
                    .as_deref()
                    .map(str::trim)
                    .filter(|error| !error.is_empty())
                {
                    line.push_str(&format!(" [persistence degraded: {error}]"));
                }
            }
            Ok(None) => {}
            Err(error) => {
                line.push_str(&format!(
                    " [workspace metadata unreadable: {}]",
                    truncate_label(&error.to_string(), 96)
                ));
            }
        }
        line
    })
}

use crate::cli::effects::truncate_label;

fn workspace_matches_current_project(
    workspace: &astra_services::session_workspace::WorkspaceMetadata,
) -> bool {
    let current_cwd = match std::env::current_dir() {
        Ok(path) => path,
        Err(_) => return false,
    };

    if let (Some(current_root), Some(workspace_root)) =
        (current_git_root(), workspace.git_root.as_deref())
    {
        return same_path(&current_root, std::path::Path::new(workspace_root));
    }

    path_contains_or_matches(&current_cwd, std::path::Path::new(&workspace.cwd))
}

fn git_root_from(cwd: &std::path::Path) -> Option<std::path::PathBuf> {
    std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| {
            let root = String::from_utf8(output.stdout).ok()?;
            let trimmed = root.trim();
            (!trimmed.is_empty()).then(|| std::path::PathBuf::from(trimmed))
        })
}

fn current_git_root() -> Option<std::path::PathBuf> {
    git_root_from(&std::env::current_dir().ok()?)
}

/// Resolve the session's project root for skill registry anchoring.
///
/// Preferred: the git repository root of the process cwd (the canonical
/// project root for tool execution). Fallback: the process current directory.
/// This keeps project skill discovery anchored at the tool execution
/// workspace instead of a possibly-unrelated process cwd.
pub(crate) fn resolved_session_project_root() -> Option<std::path::PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    Some(git_root_from(&cwd).unwrap_or(cwd))
}

fn canonical_or_original(path: &std::path::Path) -> std::path::PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn same_path(left: &std::path::Path, right: &std::path::Path) -> bool {
    canonical_or_original(left) == canonical_or_original(right)
}

fn path_contains_or_matches(left: &std::path::Path, right: &std::path::Path) -> bool {
    let left = canonical_or_original(left);
    let right = canonical_or_original(right);
    left == right || left.starts_with(&right) || right.starts_with(&left)
}

#[derive(Debug, Default, PartialEq, Clone)]
pub struct RestoredSessionState {
    pub history: Vec<(String, String)>,
    pub turn: u32,
    pub recent_tools: Vec<String>,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    pub total_cache_read_tokens: u64,
    pub total_cache_creation_tokens: u64,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct RestoredJournalState {
    pub exists: bool,
    pub session: RestoredSessionState,
    pub last_turn_event: Option<session_journal::JournalEvent>,
}

/// Rebuild `(user_msg, assistant_msg)` history from the session journal.
/// Only `Turn` events with both user_input and assistant_output are included.
pub(crate) fn restore_history_from_journal(
    session_id: &str,
) -> Result<Vec<(String, String)>, String> {
    Ok(restore_session_state_from_journal(session_id)?
        .session
        .history)
}

/// Full session counters and history materialized from the local JSONL projection.
pub fn session_state_from_journal(session_id: &str) -> Result<RestoredSessionState, String> {
    Ok(restore_session_state_from_journal(session_id)?.session)
}

/// Return the newest durable context-assembly trace for a local session.
///
/// Context traces are derived sidecar evidence, but the journal is still the
/// recovery authority when a process exits between the primary turn commit
/// and its workspace projection. Callers use the event turn as the monotonic
/// boundary; the trace's internal `turn_id` may identify a model round rather
/// than a user turn.
pub(crate) fn latest_context_assembly_trace_from_journal(
    session_id: &str,
) -> Result<
    Option<(
        u32,
        astra_turn_core::context_assembly_trace::ContextAssemblyTrace,
    )>,
    String,
> {
    session_journal::validate_session_id(session_id)
        .map_err(|error| format!("failed to read session journal for {session_id}: {error}"))?;
    if !session_journal::journal_file_path(session_id).exists() {
        return Ok(None);
    }
    let events = session_journal::read_journal(session_id)
        .map_err(|error| format!("failed to read session journal for {session_id}: {error}"))?;
    for event in events.into_iter().rev() {
        let Some(turn) = event.turn else {
            continue;
        };
        if event.event_type != session_journal::JournalEventType::ContextAssemblyRecorded {
            continue;
        }
        let Some(trace_json) = event.context_assembly_trace else {
            continue;
        };
        match serde_json::from_value::<astra_turn_core::context_assembly_trace::ContextAssemblyTrace>(
            trace_json,
        ) {
            Ok(trace) => return Ok(Some((turn, trace))),
            Err(error) => tracing::warn!(
                session_id,
                turn,
                %error,
                "ignored malformed durable context-assembly trace during recovery"
            ),
        }
    }
    Ok(None)
}

pub(crate) fn restored_journal_state(session_id: &str) -> Result<RestoredJournalState, String> {
    restore_session_state_from_journal(session_id)
}

fn restore_session_state_from_journal(session_id: &str) -> Result<RestoredJournalState, String> {
    session_journal::validate_session_id(session_id)
        .map_err(|error| format!("failed to read session journal for {session_id}: {error}"))?;
    let journal_exists = session_journal::journal_file_path(session_id).exists();
    if !journal_exists {
        return Ok(RestoredJournalState::default());
    }
    let events = match session_journal::read_journal_append_order(session_id) {
        Ok(events) => events,
        Err(error) => {
            return Err(format!(
                "failed to read session journal for {session_id}: {error}"
            ));
        }
    };

    Ok(restored_journal_state_from_append_order_events(
        journal_exists,
        &events,
    ))
}

/// Rebuild interactive turn authority from one already-fenced physical
/// append-order snapshot. Callers holding a session execution lease use this
/// to avoid mixing a continuation cursor from one journal generation with
/// counters/history from another.
pub(crate) fn restored_journal_state_from_append_order_events(
    journal_exists: bool,
    events: &[session_journal::JournalEvent],
) -> RestoredJournalState {
    let mut restored = RestoredSessionState::default();
    let start_idx = events
        .iter()
        .rposition(|event| event.event_type == session_journal::JournalEventType::SessionStart)
        .map(|idx| idx + 1)
        .unwrap_or(0);
    let last_turn_event = events[start_idx..]
        .iter()
        .rev()
        .find(|event| {
            matches!(
                event.event_type,
                session_journal::JournalEventType::Turn
                    | session_journal::JournalEventType::TurnError
            )
        })
        .cloned();

    for event in events.iter().skip(start_idx) {
        if !matches!(
            event.event_type,
            session_journal::JournalEventType::Turn | session_journal::JournalEventType::TurnError
        ) {
            continue;
        }
        if event.event_type == session_journal::JournalEventType::Turn {
            restored.history.extend(restored_turn_history_pairs(&event));
        } else {
            restored.history.extend(turn_error_history_pairs(&event));
        }
        restored.turn = restored
            .turn
            .max(event.turn.unwrap_or(restored.turn.saturating_add(1)));
        restored.total_prompt_tokens += event.tokens_in.unwrap_or(0);
        restored.total_completion_tokens += event.tokens_out.unwrap_or(0);
        restored.total_cache_read_tokens += event.cache_read_tokens.unwrap_or(0);
        restored.total_cache_creation_tokens += event.cache_creation_tokens.unwrap_or(0);
        if let Some(tools_used) = &event.tools_used {
            restored.recent_tools = tools_used.clone();
        }
    }
    crate::cli::history_work::record_pair_history(
        astra_core::history_work::HistoryWorkSite::CliJournalHistoryHydration,
        &restored.history,
    );

    RestoredJournalState {
        exists: journal_exists,
        session: restored,
        last_turn_event,
    }
}

pub(crate) fn turn_error_history_pairs(
    event: &session_journal::JournalEvent,
) -> Vec<(String, String)> {
    let mut inputs = Vec::new();
    if let Some(input) = event
        .user_input
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        inputs.push(input.to_string());
    }
    inputs.extend(applied_user_intents_from_turn_metadata(
        event.metadata.as_ref(),
    ));
    let failure = event.error.as_deref().unwrap_or("turn failed");
    inputs
        .into_iter()
        .map(|input| (input, format!("[Previous turn failed: {failure}]")))
        .collect()
}

fn restored_turn_history_pairs(event: &session_journal::JournalEvent) -> Vec<(String, String)> {
    let base = event.user_input.clone().unwrap_or_default();
    let assistant = event.assistant_output.clone().unwrap_or_default();
    let applied_intents = applied_user_intents_from_turn_metadata(event.metadata.as_ref());

    let mut inputs = Vec::with_capacity(1 + applied_intents.len());
    if !base.trim().is_empty() {
        inputs.push(base);
    }
    inputs.extend(applied_intents);

    if inputs.is_empty() {
        return vec![(String::new(), assistant)];
    }

    let mut pairs = Vec::with_capacity(inputs.len());
    let last_idx = inputs.len() - 1;
    for (idx, input) in inputs.into_iter().enumerate() {
        let output = if idx == last_idx {
            assistant.clone()
        } else {
            String::new()
        };
        pairs.push((input, output));
    }
    pairs
}

fn applied_user_intents_from_turn_metadata(metadata: Option<&serde_json::Value>) -> Vec<String> {
    #[derive(serde::Deserialize)]
    struct StoredUserIntent {
        intent_id: String,
        delivery: astra_turn_types::UserIntentDelivery,
        status: astra_turn_types::UserIntentStatus,
        event_index: usize,
        content: String,
    }

    let mut intents = metadata
        .and_then(|metadata| metadata.get("user_intents"))
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(
            |entry| match serde_json::from_value::<StoredUserIntent>(entry.clone()) {
                Ok(intent)
                    if !intent.intent_id.trim().is_empty()
                        && !intent.content.trim().is_empty()
                        && intent.delivery
                            == astra_turn_types::UserIntentDelivery::GuideCurrentRun
                        && intent.status == astra_turn_types::UserIntentStatus::Applied =>
                {
                    Some(intent)
                }
                Ok(_) => None,
                Err(error) => {
                    tracing::warn!(error = %error, "ignored malformed session user intent");
                    None
                }
            },
        )
        .collect::<Vec<_>>();
    intents.sort_by_key(|intent| intent.event_index);
    let mut seen = std::collections::HashSet::new();
    intents
        .into_iter()
        .filter(|intent| seen.insert(intent.intent_id.clone()))
        .map(|intent| intent.content.trim().to_string())
        .collect()
}

pub(crate) fn print_session_banner(profile: Option<&str>, state: &SessionState) {
    let creds = load_credentials();
    let pname = profile_name(profile, &creds);
    let p = creds.profiles.get(&pname);
    let logged_in = p.and_then(|p| p.access_token.as_ref()).is_some()
        || active_env_access_token(chrono::Utc::now().timestamp()).is_some();
    let model_display = state.model.as_deref().unwrap_or("auto");
    let version = env!("CARGO_PKG_VERSION");
    let skills_count = state.unified_skill_registry.len();
    let colors_enabled = std::env::var_os("NO_COLOR").is_none();

    // ── Two-column card layout ─────────────────────────────────────────
    let term_w = crossterm::terminal::size()
        .map(|(c, _)| c as usize)
        .unwrap_or(80)
        .clamp(60, 120);

    // Layout: │ <left_col> │ <right_col> │
    // outer border = 2 chars (│...│), divider = 3 chars ( │ ), padding = 2 (spaces inside)

    let rng = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(42);

    // Logo lines (plain text, will be styled per-row)
    let logo_plain: &[&str] = &[
        "",
        "    ▟██▙",
        "   ▟█▀▀█▙",
        "  ▟█▘  ▝█▙",
        "  ▀▀████▀▀",
        "    astra",
        "",
    ];
    let left_footer = format!(
        " {} {} {}",
        style_banner_text(model_display, BannerTextStyle::YellowBold, colors_enabled),
        style_banner_text("·", BannerTextStyle::WhiteBold, colors_enabled),
        style_banner_text(
            format!("v{version} · {pname}"),
            BannerTextStyle::WhiteBold,
            colors_enabled,
        )
    );

    // Build left column first so we can measure its actual width
    let mut left: Vec<String> = Vec::new();
    for line in logo_plain {
        if line.is_empty() {
            left.push(String::new());
        } else if *line == "    astra" {
            left.push(style_banner_text(
                *line,
                BannerTextStyle::MagentaBold,
                colors_enabled,
            ));
        } else {
            left.push(style_banner_text(
                *line,
                BannerTextStyle::Magenta,
                colors_enabled,
            ));
        }
    }
    left.push(left_footer);

    // Derive left column width from content (was hardcoded 24)
    let left_col_w = left
        .iter()
        .map(|l| crate::cli::terminal_region::visible_char_width(l))
        .max()
        .unwrap_or(20)
        .max(10);
    // 7 = │ + sp + │ + sp + │ + sp + │
    let right_col_w = term_w.saturating_sub(left_col_w + 7);
    let total_inner = left_col_w + right_col_w + 3; // 3 = " │ " between columns

    // Truncation helper: ensure visible text fits within max_vis columns.
    // Operates on plain text before ANSI styling; appends "…" when truncated.
    let trunc_vis = |text: &str, max_vis: usize| -> String {
        let w = crate::cli::terminal_region::visible_char_width(text);
        if w <= max_vis {
            return text.to_string();
        }
        let mut out = String::new();
        let mut used = 0usize;
        for ch in text.chars() {
            let ch_w = if ch.is_ascii() { 1 } else { 2 };
            if used + ch_w + 1 > max_vis {
                break;
            }
            out.push(ch);
            used += ch_w;
        }
        out.push('…');
        out
    };

    // Right column: build with truncation safety on every line
    let sep_line = "─".repeat(right_col_w);
    let mut right: Vec<String> = Vec::new();
    right.push(style_banner_text(
        trunc_vis("Tips", right_col_w),
        BannerTextStyle::WhiteBold,
        colors_enabled,
    ));
    right.push(style_banner_text(
        trunc_vis("/help for all commands", right_col_w),
        BannerTextStyle::WhiteBold,
        colors_enabled,
    ));
    right.push(style_banner_text(
        trunc_vis("Ctrl+K command picker", right_col_w),
        BannerTextStyle::WhiteBold,
        colors_enabled,
    ));
    right.push(style_banner_text(
        trunc_vis("Alt+Enter multi-line input", right_col_w),
        BannerTextStyle::WhiteBold,
        colors_enabled,
    ));
    right.push(style_banner_text(
        sep_line,
        BannerTextStyle::WhiteBold,
        colors_enabled,
    ));
    right.push(style_banner_text(
        trunc_vis("Status", right_col_w),
        BannerTextStyle::WhiteBold,
        colors_enabled,
    ));
    right.push(style_banner_text(
        trunc_vis(
            &format!(
                "{skills_count} skills · {}",
                if logged_in {
                    "logged in"
                } else {
                    "not logged in"
                }
            ),
            right_col_w,
        ),
        BannerTextStyle::WhiteBold,
        colors_enabled,
    ));
    if let Some(line) = pending_recovery_status_line(state) {
        let truncated = trunc_vis(&line, right_col_w);
        right.push(style_banner_text(
            truncated,
            BannerTextStyle::YellowBold,
            colors_enabled,
        ));
    }

    // Equalize heights
    let total_rows = left.len().max(right.len());
    while left.len() < total_rows {
        left.push(String::new());
    }
    while right.len() < total_rows {
        right.push(String::new());
    }

    // ── Render helper ────────────────────────────────────────────────────
    // Body row width (between the two outer │):
    //   " " + left_col_w + " " + "│" + " " + right_col_w + " "
    //   = 1 + left_col_w + 1 + 1 + 1 + right_col_w + 1 = total_inner + 4
    // …but the divider counts as part of total_inner already (total_inner = left + right + 3),
    // so the inner span between the two outer │ is `total_inner + 2`.
    let inner_w = total_inner + 2;
    let h_bar = "─".repeat(inner_w);

    // Title embedded in the top edge: ╭─ astra v0.1.0 ─────────╮
    let title_text = format!("astra v{version}");
    // " astra v0.1.0 " — leading and trailing space so the title breathes.
    let title_padded = format!(" {} ", title_text);
    let title_w = crate::cli::terminal_region::visible_char_width(&title_padded);
    // Layout: ╭ ─ <title> ── … ── ╮  (1 leading dash before the title)
    let lead_dash = 1usize;
    let trail_dash = inner_w.saturating_sub(lead_dash + title_w);

    // Total lines in the card (header + body + footer)
    let card_lines = 1 + total_rows + 1;

    // Render one frame of the card
    struct BannerLayout<'a> {
        left: &'a [String],
        right: &'a [String],
        title_padded: &'a str,
        lead_dash: usize,
        trail_dash: usize,
        h_bar: &'a str,
        total_rows: usize,
        left_col_w: usize,
        right_col_w: usize,
    }

    fn render_banner_frame(
        layout: &BannerLayout<'_>,
        with_stars: bool,
        mut rng_seed: u64,
        colors_enabled: bool,
    ) {
        use std::io::Write;
        let vis_w = crate::cli::terminal_region::visible_char_width;
        let next = |r: &mut u64| -> u64 {
            *r ^= *r << 13;
            *r ^= *r >> 7;
            *r ^= *r << 17;
            *r
        };
        let BannerLayout {
            left,
            right,
            title_padded,
            lead_dash,
            trail_dash,
            h_bar,
            total_rows,
            left_col_w,
            right_col_w,
        } = layout;

        const STARS: &[&str] = &["·", "✦", "✧", "⋆", "˙"];
        let starfield_pad = |target: usize, vis: usize, rng: &mut u64, density: u64| -> String {
            let pad = target.saturating_sub(vis);
            let mut out = String::new();
            for i in 0..pad {
                if with_stars && next(rng) % 100 < density && i > 0 && i < pad.saturating_sub(1) {
                    let s = STARS[(next(rng) % STARS.len() as u64) as usize];
                    out.push_str(&style_banner_text(
                        s,
                        BannerTextStyle::DarkGrey,
                        colors_enabled,
                    ));
                } else {
                    out.push(' ');
                }
            }
            out
        };

        // Header — title is embedded inline; brighter so it stands out.
        eprint!(
            "{}",
            style_banner_text("╭", BannerTextStyle::WhiteBold, colors_enabled)
        );
        eprint!(
            "{}",
            style_banner_text(
                "─".repeat(*lead_dash),
                BannerTextStyle::WhiteBold,
                colors_enabled,
            )
        );
        eprint!(
            "{}",
            style_banner_text(*title_padded, BannerTextStyle::BoldCyan, colors_enabled,)
        );
        eprint!(
            "{}",
            style_banner_text(
                "─".repeat(*trail_dash),
                BannerTextStyle::WhiteBold,
                colors_enabled,
            )
        );
        eprintln!(
            "{}",
            style_banner_text("╮", BannerTextStyle::WhiteBold, colors_enabled)
        );
        // Body
        for row in 0..*total_rows {
            let l_pad = starfield_pad(*left_col_w, vis_w(&left[row]), &mut rng_seed, 12);
            let r_pad = starfield_pad(*right_col_w, vis_w(&right[row]), &mut rng_seed, 8);
            eprintln!(
                "{} {}{} {} {}{} {}",
                style_banner_text("│", BannerTextStyle::WhiteBold, colors_enabled),
                left[row],
                l_pad,
                style_banner_text("│", BannerTextStyle::WhiteBold, colors_enabled),
                right[row],
                r_pad,
                style_banner_text("│", BannerTextStyle::WhiteBold, colors_enabled),
            );
        }
        // Footer
        eprintln!(
            "{}{}{}",
            style_banner_text("╰", BannerTextStyle::WhiteBold, colors_enabled),
            style_banner_text(*h_bar, BannerTextStyle::WhiteBold, colors_enabled),
            style_banner_text("╯", BannerTextStyle::WhiteBold, colors_enabled)
        );
        let _ = std::io::stderr().flush();
    }

    use std::io::IsTerminal;
    let animated = crossterm::terminal::size().is_ok()
        && std::env::var("NO_COLOR").is_err()
        && std::env::var("CI").is_err()
        && std::io::stderr().is_terminal();

    eprintln!();

    let layout = BannerLayout {
        left: &left,
        right: &right,
        title_padded: &title_padded,
        lead_dash,
        trail_dash,
        h_bar: &h_bar,
        total_rows,
        left_col_w,
        right_col_w,
    };

    if animated {
        use std::time::Duration;

        let seeds = [
            rng,
            rng.wrapping_add(7919),
            rng.wrapping_add(104729),
            rng.wrapping_add(999983),
        ];
        for (frame, &seed) in seeds.iter().enumerate() {
            if frame > 0 {
                eprint!("\x1b[{}A\r", card_lines);
            }
            render_banner_frame(&layout, true, seed, colors_enabled);
            std::thread::sleep(Duration::from_millis(100));
        }
        eprint!("\x1b[{}A\r", card_lines);
        render_banner_frame(&layout, false, 0, colors_enabled);
    } else {
        render_banner_frame(&layout, false, 0, colors_enabled);
    }

    eprintln!();
    let welcome = banner_welcome_text(&pname, p, logged_in);
    let model_hint = if model_display == "auto" {
        format!(
            "{} {}",
            style_banner_text("auto", BannerTextStyle::Yellow, colors_enabled),
            style_banner_text("mode", BannerTextStyle::Grey, colors_enabled)
        )
    } else {
        format!(
            "{} {}",
            style_banner_text(model_display, BannerTextStyle::Cyan, colors_enabled),
            style_banner_text("mode", BannerTextStyle::Grey, colors_enabled)
        )
    };
    eprintln!(
        "  {} {} {}",
        style_banner_text(welcome, BannerTextStyle::Cyan, colors_enabled),
        model_hint,
        style_banner_text("· /model to change", BannerTextStyle::Grey, colors_enabled)
    );
    eprintln!();
}

#[derive(Clone, Copy)]
enum BannerTextStyle {
    WhiteBold,
    Magenta,
    MagentaBold,
    Yellow,
    YellowBold,
    BoldCyan,
    Cyan,
    Grey,
    DarkGrey,
}

fn style_banner_text(
    text: impl Into<String>,
    style: BannerTextStyle,
    colors_enabled: bool,
) -> String {
    let text = text.into();
    if !colors_enabled {
        return text;
    }
    match style {
        BannerTextStyle::WhiteBold => text.white().bold().to_string(),
        BannerTextStyle::Magenta => text.magenta().to_string(),
        BannerTextStyle::MagentaBold => text.magenta().bold().to_string(),
        BannerTextStyle::Yellow => text.yellow().to_string(),
        BannerTextStyle::YellowBold => text.bold().yellow().to_string(),
        BannerTextStyle::BoldCyan => text.bold().cyan().to_string(),
        BannerTextStyle::Cyan => text.cyan().to_string(),
        BannerTextStyle::Grey => text.grey().to_string(),
        BannerTextStyle::DarkGrey => text.dark_grey().to_string(),
    }
}

fn banner_welcome_text(
    profile_name: &str,
    profile: Option<&astra_credentials::Profile>,
    logged_in: bool,
) -> String {
    if !logged_in {
        return "Welcome to astra".to_string();
    }
    let user = profile
        .and_then(|profile| profile.username.as_deref())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(profile_name);
    format!("Welcome back, {user}")
}

#[cfg(test)]
fn banner_session_display(state: &SessionState) -> String {
    match state.session_id.as_deref() {
        Some(s) => {
            let short = prefix_chars(s, 8);
            if state.turn > 0 {
                format!("{short} (resumed)")
            } else {
                short
            }
        }
        None => "new".to_string(),
    }
}

pub(crate) fn current_access_token(profile: Option<&str>) -> Option<String> {
    // Gateway-injected env tokens still win, but only while locally usable.
    if let Some(token) = active_env_access_token(chrono::Utc::now().timestamp()) {
        return Some(token);
    }
    let creds = load_credentials();
    let name = profile_name(profile, &creds);
    creds
        .profiles
        .get(&name)
        .and_then(|p| p.access_token.clone())
}

#[cfg(test)]
mod tests {
    use super::{
        ACCESS_TOKEN_REFRESH_SKEW_SECS, BannerTextStyle, RestoredSessionState, ServerDefaultModel,
        SilentRefreshError, access_token_needs_refresh, applied_user_intents_from_turn_metadata,
        banner_session_display, banner_welcome_text, current_access_token, current_git_root,
        default_model_selection_from_access, ensure_state_default_model, fresh_access_token,
        git_root_from, initialize_session_state, load_server_model_access,
        model_default_invalid_reason_message, model_selection_for_name_from_catalog,
        model_selection_from_exact_response, pending_recovery_status_line,
        resolve_server_default_model, resolve_server_model_selection, restore_history_from_journal,
        restore_session_state_from_journal, restored_journal_state,
        should_keep_credentials_on_refresh_error, style_banner_text,
    };
    use crate::cli::cli_config::cli_utils::{
        CredentialsFile, Profile, load_credentials, save_credentials,
    };
    use crate::cli::session::session_state::SessionState;
    use crate::tests::isolate_credentials;
    use astra_services::{
        ModelAccessKind, ModelAccessProjectionResponse, ModelDefaultInvalidReason,
        ModelDefaultResolution, ModelDefaultScope, ModelDefaultSource, ModelExecutionPlacement,
        ModelListCursor, ModelListItemResponse, session_journal,
    };
    use tempfile::tempdir;
    use wiremock::matchers::{method, path, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    struct EnvGuard {
        key: &'static str,
        old: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let old = std::env::var(key).ok();
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, old }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.old {
                Some(value) => unsafe {
                    std::env::set_var(self.key, value);
                },
                None => unsafe {
                    std::env::remove_var(self.key);
                },
            }
        }
    }

    fn jwt_with_exp(exp: i64) -> String {
        use base64::Engine;

        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"alg":"none","typ":"JWT"}"#);
        let payload =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp}}}"#));
        format!("{header}.{payload}.sig")
    }

    fn catalog_entry(
        offering_id: &str,
        name: &str,
        is_active: bool,
        context_window: i32,
    ) -> ModelListItemResponse {
        ModelListItemResponse {
            offering_id: offering_id.to_string(),
            access_id: "self-hosted".to_string(),
            access_kind: ModelAccessKind::SelfHosted,
            access_label: "Self-hosted".to_string(),
            execution_placement: ModelExecutionPlacement::Server,
            name: name.to_string(),
            provider: "openai".to_string(),
            description: None,
            is_active,
            context_window,
            max_completion_tokens: None,
            architecture: None,
            thinking_capability: None,
        }
    }

    fn access_projection(
        offerings: Vec<ModelListItemResponse>,
        default_offering_id: Option<&str>,
    ) -> ModelAccessProjectionResponse {
        let default_resolution = match default_offering_id {
            Some(offering_id) => ModelDefaultResolution::Selected {
                offering_id: offering_id.to_string(),
                source: ModelDefaultSource::Astra,
                scope: ModelDefaultScope::EffectiveCatalog,
            },
            None => ModelDefaultResolution::Missing,
        };
        let total = offerings.len() as u32;
        ModelAccessProjectionResponse {
            accesses: Vec::new(),
            offerings,
            default_offering_id: default_offering_id.map(str::to_string),
            default_resolution: Some(default_resolution),
            next_cursor: None,
            limit: 50,
            total,
            catalog_revision: "sha256:test-catalog".to_string(),
            observed_at: "2026-07-20T00:00:00Z".to_string(),
        }
    }

    fn catalog_page(items: Vec<ModelListItemResponse>) -> serde_json::Value {
        let total = items.len();
        serde_json::json!({
            "items": items,
            "next_cursor": null,
            "limit": 50,
            "total": total,
            "catalog_revision": "sha256:test-catalog"
        })
    }

    #[test]
    fn default_model_selection_uses_server_projection_not_catalog_order() {
        let projection = access_projection(
            vec![
                catalog_entry(
                    "offer-flash",
                    "  deepseek-v4-flash-anthropic  ",
                    true,
                    128_000,
                ),
                catalog_entry("offer-pro", "deepseek-v4-pro-official", true, 128_000),
            ],
            Some("offer-pro"),
        );

        let selection = default_model_selection_from_access(&projection)
            .expect("valid projection")
            .expect("default Offering");
        assert_eq!(selection.name, "deepseek-v4-pro-official");
        assert_eq!(selection.offering_id, "offer-pro");
    }

    #[test]
    fn default_model_selection_carries_configured_context_window() {
        let projection = access_projection(
            vec![catalog_entry(
                "offer-claude",
                "claude-sonnet-4",
                true,
                200_000,
            )],
            Some("offer-claude"),
        );

        let selection = default_model_selection_from_access(&projection)
            .expect("valid projection")
            .expect("default model selection");
        assert_eq!(selection.name, "claude-sonnet-4");
        assert_eq!(selection.offering_id, "offer-claude");
        assert_eq!(selection.context_window, Some(200_000));
    }

    #[test]
    fn named_selection_uses_active_model_entry() {
        let catalog = vec![
            catalog_entry("offer-inactive", "custom-model", false, 8_000),
            catalog_entry("offer-active", "custom-model", true, 500_000),
        ];

        let selection = model_selection_for_name_from_catalog(&catalog, "CUSTOM-MODEL")
            .expect("active Offering");
        assert_eq!(selection.offering_id, "offer-active");
        assert_eq!(selection.context_window, Some(500_000));
    }

    #[test]
    fn named_selection_strips_thinking_suffix_for_registry_match() {
        let catalog = vec![catalog_entry(
            "offer-deepseek-pro",
            "deepseek-v4-pro-official",
            true,
            1_000_000,
        )];

        let selection = model_selection_for_name_from_catalog(
            &catalog,
            "deepseek-v4-pro-official(thinking:high)",
        )
        .expect("active Offering");
        assert_eq!(selection.offering_id, "offer-deepseek-pro");
        assert_eq!(selection.context_window, Some(1_000_000));
    }

    #[test]
    fn exact_model_selection_requires_canonical_active_identity() {
        let resolved = model_selection_from_exact_response(
            "overflow-model",
            r#"{"name":"overflow-model","model_id":"offer-201","is_active":true,"context_window":200000}"#,
        )
        .expect("active exact model should resolve");
        assert_eq!(resolved.name, "overflow-model");
        assert_eq!(resolved.offering_id, "offer-201");
        assert_eq!(resolved.context_window, Some(200_000));

        let inactive = model_selection_from_exact_response(
            "overflow-model",
            r#"{"name":"overflow-model","model_id":"offer-201","is_active":false}"#,
        )
        .expect_err("inactive exact model must fail closed");
        assert!(inactive.contains("not an active"), "{inactive}");

        let mismatch = model_selection_from_exact_response(
            "overflow-model",
            r#"{"name":"other-model","model_id":"offer-202","is_active":true}"#,
        )
        .expect_err("path/name mismatch must fail closed");
        assert!(mismatch.contains("name mismatch"), "{mismatch}");
    }

    #[tokio::test]
    async fn server_model_resolution_preserves_offering_identity_and_optional_context() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(catalog_page(vec![
                catalog_entry("offer-small", "small-model", true, 8_192),
                catalog_entry(
                    "offer-deepseek",
                    "deepseek-v4-pro-official",
                    true,
                    1_000_000,
                ),
            ])))
            .mount(&mock)
            .await;
        let api = astra_thin_client::ThinClient::new(&mock.uri(), None).unwrap();

        let small = resolve_server_model_selection(&api, "token", "small-model")
            .await
            .expect("active Offering without optional context metadata");
        assert_eq!(small.offering_id, "offer-small");
        assert_eq!(small.context_window, Some(8_192));

        let deepseek = resolve_server_model_selection(
            &api,
            "token",
            "deepseek-v4-pro-official(thinking:high)",
        )
        .await
        .expect("active Offering selected through thinking suffix");
        assert_eq!(deepseek.offering_id, "offer-deepseek");
        assert_eq!(deepseek.context_window, Some(1_000_000));
    }

    #[tokio::test]
    async fn server_model_resolution_rejects_model_missing_from_authoritative_catalog() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(catalog_page(vec![
                catalog_entry("offer-listed", "listed-model", true, 8_192),
            ])))
            .mount(&mock)
            .await;
        let api = astra_thin_client::ThinClient::new(&mock.uri(), None).expect("client");
        let error = resolve_server_model_selection(&api, "token", "overflow-model(thinking:high)")
            .await
            .expect_err("a missing Offering must fail closed");
        assert!(error.contains("authoritative catalog"), "{error}");
    }

    #[tokio::test]
    async fn server_model_resolution_drains_catalog_before_admission() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .and(query_param_is_missing("after_provider"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "items": [catalog_entry("offer-first", "first-model", true, 8_192)],
                "next_cursor": {
                    "provider": "openai",
                    "model_name": "first-model",
                    "model_id": "offer-first"
                },
                "limit": 200,
                "total": 2,
                "catalog_revision": "sha256:drain"
            })))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .and(query_param("after_provider", "openai"))
            .and(query_param("after_name", "first-model"))
            .and(query_param("after_offering_id", "offer-first"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "items": [catalog_entry("offer-second", "second-model", true, 16_384)],
                "next_cursor": null,
                "limit": 200,
                "total": 2,
                "catalog_revision": "sha256:drain"
            })))
            .mount(&mock)
            .await;

        let api = astra_thin_client::ThinClient::new(&mock.uri(), None).expect("client");
        let selection = resolve_server_model_selection(&api, "token", "second-model")
            .await
            .expect("second-page Offering should resolve");
        assert_eq!(selection.offering_id, "offer-second");
    }

    #[tokio::test]
    async fn server_default_resolution_uses_model_access_default() {
        let mock = MockServer::start().await;
        let projection = access_projection(
            vec![
                catalog_entry("offer-alpha", "alpha-model", true, 8_192),
                catalog_entry("offer-beta", "beta-model", true, 128_000),
            ],
            Some("offer-beta"),
        );
        Mock::given(method("GET"))
            .and(path("/model-access"))
            .respond_with(ResponseTemplate::new(200).set_body_json(projection))
            .mount(&mock)
            .await;
        let api = astra_thin_client::ThinClient::new(&mock.uri(), None).unwrap();

        let resolved = resolve_server_default_model(&api, "token").await;

        assert_eq!(
            resolved,
            ServerDefaultModel::Selected(super::ServerModelSelection {
                name: "beta-model".to_string(),
                context_window: Some(128_000),
                offering_id: "offer-beta".to_string(),
            })
        );
    }

    #[tokio::test]
    async fn model_access_rejects_cursor_without_pagination_progress() {
        let mock = MockServer::start().await;
        let mut projection = access_projection(Vec::new(), None);
        projection.total = 1;
        projection.next_cursor = Some(ModelListCursor {
            provider: "openai".to_string(),
            model_name: "missing".to_string(),
            model_id: "offer-missing".to_string(),
        });
        Mock::given(method("GET"))
            .and(path("/model-access"))
            .respond_with(ResponseTemplate::new(200).set_body_json(projection))
            .mount(&mock)
            .await;
        let api = astra_thin_client::ThinClient::new(&mock.uri(), None).expect("client");

        let error = load_server_model_access(&api, "token")
            .await
            .expect_err("a continuation cursor must accompany progress");

        assert!(error.contains("cursor without offerings"), "{error}");
    }

    #[tokio::test]
    async fn ensure_state_default_model_updates_budget_for_explicit_model() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(catalog_page(vec![
                catalog_entry(
                    "offer-deepseek",
                    "deepseek-v4-pro-official",
                    true,
                    1_000_000,
                ),
            ])))
            .mount(&mock)
            .await;
        let api = astra_thin_client::ThinClient::new(&mock.uri(), None).unwrap();
        let mut state = SessionState {
            model: Some("deepseek-v4-pro-official".to_string()),
            ..SessionState::default()
        };

        let selected = ensure_state_default_model(&api, "token", &mut state).await;

        assert_eq!(
            selected.as_deref(),
            Some("deepseek-v4-pro-official"),
            "explicit model selection should be preserved"
        );
        assert_eq!(
            state.context_budget.model_limit, 1_000_000,
            "state diagnostics must reflect the server model context_window, not the client default"
        );
    }

    #[test]
    fn default_model_selection_rejects_inconsistent_projection() {
        let missing_default = access_projection(
            vec![catalog_entry("offer-active", "active-model", true, 8_192)],
            None,
        );
        assert!(default_model_selection_from_access(&missing_default).is_err());

        let unknown_default = access_projection(
            vec![catalog_entry("offer-active", "active-model", true, 8_192)],
            Some("offer-missing"),
        );
        assert!(default_model_selection_from_access(&unknown_default).is_err());
    }

    #[test]
    fn invalid_provider_default_message_is_human_readable_and_stable() {
        assert_eq!(
            model_default_invalid_reason_message(ModelDefaultInvalidReason::InvalidOfferingId),
            "its offering identity is invalid"
        );
        assert_eq!(
            model_default_invalid_reason_message(ModelDefaultInvalidReason::NotEffectiveOffering),
            "it is not part of the effective model catalog"
        );
    }

    #[tokio::test]
    async fn invalid_provider_default_keeps_manual_model_selection_available() {
        let mock = MockServer::start().await;
        let projection = ModelAccessProjectionResponse {
            accesses: Vec::new(),
            offerings: vec![catalog_entry("offer-valid", "valid-model", true, 8_192)],
            default_offering_id: None,
            default_resolution: Some(ModelDefaultResolution::Invalid {
                reason: ModelDefaultInvalidReason::NotEffectiveOffering,
            }),
            next_cursor: None,
            limit: 50,
            total: 1,
            catalog_revision: "sha256:invalid-provider-default".to_string(),
            observed_at: "2026-08-10T00:00:00Z".to_string(),
        };
        Mock::given(method("GET"))
            .and(path("/model-access"))
            .respond_with(ResponseTemplate::new(200).set_body_json(projection))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(catalog_page(vec![
                catalog_entry("offer-valid", "valid-model", true, 8_192),
            ])))
            .mount(&mock)
            .await;
        let api = astra_thin_client::ThinClient::new(&mock.uri(), None).unwrap();

        assert_eq!(
            resolve_server_default_model(&api, "token").await,
            ServerDefaultModel::Unavailable,
            "an invalid provider default must not silently fall back"
        );
        let manual = resolve_server_model_selection(&api, "token", "valid-model")
            .await
            .expect("valid effective Offering remains selectable manually");
        assert_eq!(manual.offering_id, "offer-valid");
    }

    #[test]
    fn restore_history_empty_for_unknown_session() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let history = restore_history_from_journal("nonexistent-session-xyz-123").unwrap();
        assert!(history.is_empty());
    }

    #[test]
    fn restore_history_from_journal_roundtrip() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let sid = format!("test-restore-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();

        writer
            .append(&session_journal::JournalEvent::turn(
                Some(&sid),
                1,
                None,
                "what is Rust?",
                "Rust is a systems language.",
                0,
                10,
                5,
                100,
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::turn(
                Some(&sid),
                2,
                None,
                "show me an example",
                "fn main() {}",
                0,
                8,
                4,
                80,
            ))
            .unwrap();

        let history = restore_history_from_journal(&sid).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].0, "what is Rust?");
        assert_eq!(history[0].1, "Rust is a systems language.");
        assert_eq!(history[1].0, "show me an example");
    }

    #[test]
    fn restore_history_preserves_applied_user_intents_as_ordered_events() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let sid = format!("test-restore-deferred-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();

        writer
            .append(
                &session_journal::JournalEvent::turn(
                    Some(&sid),
                    1,
                    None,
                    "1",
                    "handled latest input",
                    0,
                    10,
                    5,
                    100,
                )
                .with_applied_user_intents([(
                    "intent-2",
                    astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                    astra_turn_types::UserIntentStatus::Applied,
                    2,
                    "2",
                )]),
            )
            .unwrap();

        let history = restore_history_from_journal(&sid).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0], ("1".to_string(), String::new()));
        assert_eq!(
            history[1],
            ("2".to_string(), "handled latest input".to_string())
        );
    }

    #[test]
    fn restore_history_preserves_duplicate_deferred_user_events() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let sid = format!("test-restore-deferred-dup-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();

        writer
            .append(
                &session_journal::JournalEvent::turn(
                    Some(&sid),
                    1,
                    None,
                    "retry",
                    "handled retry",
                    0,
                    10,
                    5,
                    100,
                )
                .with_applied_user_intents([
                    (
                        "intent-retry-1",
                        astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                        astra_turn_types::UserIntentStatus::Applied,
                        2,
                        "retry",
                    ),
                    (
                        "intent-retry-2",
                        astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                        astra_turn_types::UserIntentStatus::Applied,
                        3,
                        "retry",
                    ),
                ]),
            )
            .unwrap();

        let history = restore_history_from_journal(&sid).unwrap();
        assert_eq!(history.len(), 3);
        assert_eq!(history[0], ("retry".to_string(), String::new()));
        assert_eq!(history[1], ("retry".to_string(), String::new()));
        assert_eq!(
            history[2],
            ("retry".to_string(), "handled retry".to_string())
        );
    }

    #[test]
    fn applied_user_intents_restore_by_event_index_and_ignore_malformed_entries() {
        let metadata = serde_json::json!({
            "user_intents": [
                {"content": "missing-a"},
                {
                    "intent_id": "intent-3",
                    "delivery": "guide_current_run",
                    "status": "applied",
                    "event_index": 3,
                    "content": "third"
                },
                {
                    "intent_id": "intent-2",
                    "delivery": "guide_current_run",
                    "status": "applied",
                    "event_index": 2,
                    "content": "second"
                }
            ]
        });

        assert_eq!(
            applied_user_intents_from_turn_metadata(Some(&metadata)),
            vec!["second".to_string(), "third".to_string()]
        );
    }

    #[test]
    fn restore_history_skips_non_turn_events() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let sid = format!("test-skip-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();

        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&sid),
                Some("gpt-4o"),
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::config_change(
                Some(&sid),
                "model",
                "gpt-4o",
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::turn(
                Some(&sid),
                1,
                None,
                "hello",
                "hi there",
                0,
                5,
                3,
                50,
            ))
            .unwrap();

        let history = restore_history_from_journal(&sid).unwrap();
        assert_eq!(history.len(), 1, "only Turn events should be included");
        assert_eq!(history[0].0, "hello");
    }

    #[test]
    fn restore_session_state_recovers_turn_tools_and_tokens() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let sid = format!("test-state-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();

        writer
            .append(
                &session_journal::JournalEvent::turn(
                    Some(&sid),
                    1,
                    None,
                    "memoria 最新的一个ci?",
                    "ok",
                    1,
                    120,
                    30,
                    100,
                )
                .with_tool_surface(
                    vec!["github".into()],
                    vec![],
                    vec!["github".into()],
                    30,
                ),
            )
            .unwrap();
        writer
            .append(
                &session_journal::JournalEvent::turn(
                    Some(&sid),
                    2,
                    None,
                    "pr呢？",
                    "ok",
                    1,
                    80,
                    20,
                    90,
                )
                .with_cache_tokens(80, 10)
                .with_tool_surface(
                    vec!["github".into()],
                    vec![],
                    vec!["github".into()],
                    35,
                ),
            )
            .unwrap();

        let restored = restore_session_state_from_journal(&sid).unwrap().session;
        assert_eq!(
            restored.turn, 2,
            "turn should reflect restored conversation length"
        );
        assert_eq!(restored.total_prompt_tokens, 200);
        assert_eq!(restored.total_completion_tokens, 50);
        assert_eq!(restored.total_cache_read_tokens, 80);
        assert_eq!(restored.total_cache_creation_tokens, 10);
        assert_eq!(restored.recent_tools, vec!["github".to_string()]);
        assert_eq!(restored.history.len(), 2);
    }

    #[test]
    fn restore_session_state_uses_latest_session_segment() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let sid = format!("test-segment-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();

        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&sid),
                Some("gpt-5"),
            ))
            .unwrap();
        writer
            .append(
                &session_journal::JournalEvent::turn(
                    Some(&sid),
                    1,
                    None,
                    "old question",
                    "old answer",
                    0,
                    500,
                    50,
                    10,
                )
                .with_tool_surface(
                    vec!["git".into()],
                    vec![],
                    vec!["git".into()],
                    10,
                ),
            )
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::session_end(Some(&sid), 1))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&sid),
                Some("gpt-5"),
            ))
            .unwrap();
        writer
            .append(
                &session_journal::JournalEvent::turn(
                    Some(&sid),
                    1,
                    None,
                    "latest question",
                    "latest answer",
                    0,
                    80,
                    20,
                    10,
                )
                .with_tool_surface(
                    vec!["github".into()],
                    vec![],
                    vec!["github".into()],
                    20,
                ),
            )
            .unwrap();

        let restored = restore_session_state_from_journal(&sid).unwrap().session;
        assert_eq!(
            restored.history,
            vec![("latest question".into(), "latest answer".into())]
        );
        assert_eq!(restored.turn, 1);
        assert_eq!(restored.total_prompt_tokens, 80);
        assert_eq!(restored.total_completion_tokens, 20);
        assert_eq!(restored.recent_tools, vec!["github".to_string()]);
    }

    #[test]
    fn restore_session_state_keeps_recorded_turn_after_stray_session_start() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let sid = format!("test-stray-start-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();

        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&sid),
                Some("gpt-5"),
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::turn(
                Some(&sid),
                1,
                None,
                "first",
                "one",
                0,
                10,
                4,
                10,
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::turn(
                Some(&sid),
                2,
                None,
                "second",
                "two",
                0,
                20,
                6,
                10,
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&sid),
                Some("gpt-5"),
            ))
            .unwrap();
        writer
            .append(
                &session_journal::JournalEvent::turn(
                    Some(&sid),
                    3,
                    None,
                    "latest",
                    "three",
                    0,
                    30,
                    8,
                    10,
                )
                .with_tool_surface(
                    vec!["github".into()],
                    vec![],
                    vec!["github".into()],
                    20,
                ),
            )
            .unwrap();

        let restored = restore_session_state_from_journal(&sid).unwrap().session;
        assert_eq!(restored.history, vec![("latest".into(), "three".into())]);
        assert_eq!(restored.turn, 3);
        assert_eq!(restored.total_prompt_tokens, 30);
        assert_eq!(restored.total_completion_tokens, 8);
        assert_eq!(restored.recent_tools, vec!["github".to_string()]);
    }

    #[test]
    fn restore_session_state_from_journal_surfaces_unreadable_journal() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let sid = format!("test-unreadable-{}", uuid::Uuid::new_v4());
        std::fs::create_dir_all(session_journal::journal_file_path(&sid)).unwrap();

        let error = restore_session_state_from_journal(&sid)
            .expect_err("directory journal path should surface an error");

        assert!(error.contains("failed to read session journal"), "{error}");
    }

    #[test]
    fn restored_journal_state_tracks_existence_and_last_turn_event() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let sid = format!("test-restore-journal-state-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();

        let missing = restored_journal_state("missing-session-xyz").unwrap();
        assert!(!missing.exists);
        assert!(missing.last_turn_event.is_none());
        assert_eq!(missing.session, RestoredSessionState::default());

        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&sid),
                Some("gpt-5"),
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::turn(
                Some(&sid),
                1,
                None,
                "first",
                "one",
                0,
                10,
                4,
                10,
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&sid),
                Some("gpt-5"),
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::turn(
                Some(&sid),
                3,
                None,
                "latest",
                "three",
                0,
                30,
                8,
                10,
            ))
            .unwrap();

        let restored = restored_journal_state(&sid).unwrap();
        assert!(restored.exists);
        assert_eq!(restored.session.turn, 3);
        assert_eq!(
            restored.last_turn_event.and_then(|event| event.turn),
            Some(3)
        );
    }

    #[test]
    #[serial_test::serial]
    fn restore_turn_error_advances_cursor_usage_and_guidance_without_partial_success() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = format!("restore-turn-error-{}", uuid::Uuid::new_v4());
        let writer = astra_services::session_journal::JournalWriter::new(&sid).unwrap();
        let mut event = astra_services::session_journal::JournalEvent::turn_error(
            Some(&sid),
            4,
            Some("test-model"),
            "original",
            "approval callback failed",
            10,
        )
        .with_applied_user_intents([(
            "intent-recovered",
            astra_turn_types::UserIntentDelivery::GuideCurrentRun,
            astra_turn_types::UserIntentStatus::Applied,
            17,
            "do not modify",
        )]);
        event.tokens_in = Some(80);
        event.tokens_out = Some(6);
        event.cache_read_tokens = Some(70);
        event.cache_creation_tokens = Some(2);
        writer.append(&event).unwrap();

        let restored = restore_session_state_from_journal(&sid).unwrap().session;
        assert_eq!(restored.turn, 4);
        assert_eq!(restored.total_prompt_tokens, 80);
        assert_eq!(restored.total_completion_tokens, 6);
        assert_eq!(restored.total_cache_read_tokens, 70);
        assert_eq!(restored.total_cache_creation_tokens, 2);
        assert!(restored.history.iter().any(|(input, output)| {
            input == "do not modify"
                && output.contains("Previous turn failed")
                && !output.contains("partial assistant")
        }));
    }

    #[serial_test::serial]
    #[test]
    fn initialize_session_state_skips_cleanly_ended_session() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let _creds_guard = isolate_credentials();

        let sid = format!("test-ended-init-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&sid),
                Some("gpt-5"),
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::turn(
                Some(&sid),
                1,
                None,
                "old question",
                "old answer",
                0,
                20,
                10,
                10,
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::session_end(Some(&sid), 1))
            .unwrap();

        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                last_session_id: Some(sid.clone()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        let state = initialize_session_state(
            None,
            Some("gpt-5"),
            &crate::cli::cli_config::cli_context::CliContext::default(),
        );
        assert_eq!(state.session_id, None);
        assert_eq!(state.pending_recovery, None);
        assert!(state.history.is_empty());
        assert_eq!(state.turn, 0);
    }

    #[serial_test::serial]
    #[test]
    fn initialize_session_state_treats_symbolic_default_as_no_model_selection() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let _creds_guard = isolate_credentials();

        let state = initialize_session_state(
            None,
            Some("default"),
            &crate::cli::cli_config::cli_context::CliContext::default(),
        );

        assert_eq!(state.model, None);
    }

    #[serial_test::serial]
    #[test]
    fn initialize_session_state_records_project_scoped_pending_recovery() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let _creds_guard = isolate_credentials();

        let sid = format!("test-pending-recovery-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&sid),
                Some("gpt-5"),
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::turn(
                Some(&sid),
                1,
                None,
                "continue the refactor",
                "I updated the parser entry point.",
                0,
                20,
                10,
                10,
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::interruption_recorded(
                Some(&sid),
                1,
                serde_json::json!({
                    "kind": "rate_limited",
                    "resumable": true,
                    "has_checkpoint": true,
                    "tool_calls_completed": 1,
                    "turns_completed": 1,
                    "remaining_turns": 4,
                }),
            ))
            .unwrap();

        let current_cwd = std::env::current_dir().unwrap();
        let mut ws = astra_services::session_workspace::WorkspaceMetadata::with_context(
            &sid,
            "gpt-5",
            &current_cwd.display().to_string(),
            Some("main"),
        );
        ws.git_root = current_git_root().map(|path| path.display().to_string());
        astra_services::session_workspace::write_workspace(&ws).unwrap();

        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                last_session_id: Some(sid.clone()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        let state = initialize_session_state(
            None,
            Some("gpt-5"),
            &crate::cli::cli_config::cli_context::CliContext::default(),
        );
        assert_eq!(state.session_id, None);
        assert_eq!(state.pending_recovery.as_deref(), Some(sid.as_str()));
        assert!(state.history.is_empty());
        assert_eq!(state.turn, 0);
    }

    #[serial_test::serial]
    #[test]
    fn initialize_session_state_explicit_session_id_suppresses_pending_recovery() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let _creds_guard = isolate_credentials();

        let sid = uuid::Uuid::new_v4().to_string();
        let writer = session_journal::JournalWriter::new(&sid).unwrap();
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&sid),
                Some("gpt-5"),
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::interruption_recorded(
                Some(&sid),
                0,
                serde_json::json!({
                    "kind": "rate_limited",
                    "resumable": true,
                    "has_checkpoint": true,
                    "tool_calls_completed": 0,
                    "turns_completed": 0,
                    "remaining_turns": 5,
                }),
            ))
            .unwrap();

        let current_cwd = std::env::current_dir().unwrap();
        let mut ws = astra_services::session_workspace::WorkspaceMetadata::with_context(
            &sid,
            "gpt-5",
            &current_cwd.display().to_string(),
            Some("main"),
        );
        ws.git_root = current_git_root().map(|path| path.display().to_string());
        astra_services::session_workspace::write_workspace(&ws).unwrap();

        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                last_session_id: Some(sid.clone()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        let cli_context = crate::cli::cli_config::cli_context::CliContext::from_launch_options(
            false,
            &[],
            &[],
            &[],
            false,
            Some(sid.clone()),
            None,
        )
        .expect("valid cli context");
        let state = initialize_session_state(None, Some("gpt-5"), &cli_context);

        assert_eq!(state.session_id.as_deref(), Some(sid.as_str()));
        assert_eq!(state.pending_recovery, None);
    }

    #[test]
    fn pending_recovery_status_line_describes_explicit_resume_intents() {
        let state = SessionState {
            pending_recovery: Some("sess-123".to_string()),
            ..SessionState::default()
        };
        assert_eq!(
            pending_recovery_status_line(&state).as_deref(),
            Some("previous session available via /resume")
        );
        assert_eq!(pending_recovery_status_line(&SessionState::default()), None);
    }

    #[serial_test::serial]
    #[test]
    fn pending_recovery_status_line_surfaces_persistence_degradation() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let sid = format!("pending-recovery-{}", uuid::Uuid::new_v4());
        let mut workspace =
            astra_services::session_workspace::WorkspaceMetadata::new(&sid, "gpt-5");
        workspace.last_persistence_error = Some("failed to append turn event".to_string());
        astra_services::session_workspace::write_workspace(&workspace).unwrap();

        let state = SessionState {
            pending_recovery: Some(sid),
            ..SessionState::default()
        };

        let line = pending_recovery_status_line(&state).expect("pending recovery line");
        assert!(line.contains("previous session available via /resume"));
        assert!(line.contains("persistence degraded: failed to append turn event"));
    }

    #[serial_test::serial]
    #[test]
    fn initialize_session_state_ignores_pending_recovery_from_other_project() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let _creds_guard = isolate_credentials();

        let sid = format!("test-other-project-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&sid),
                Some("gpt-5"),
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::interruption_recorded(
                Some(&sid),
                0,
                serde_json::json!({
                    "kind": "rate_limited",
                    "resumable": true,
                    "has_checkpoint": true,
                    "tool_calls_completed": 0,
                    "turns_completed": 0,
                    "remaining_turns": 5,
                }),
            ))
            .unwrap();

        let other_project = tempdir().unwrap();
        let ws = astra_services::session_workspace::WorkspaceMetadata::with_context(
            &sid,
            "gpt-5",
            &other_project.path().display().to_string(),
            None,
        );
        astra_services::session_workspace::write_workspace(&ws).unwrap();

        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                last_session_id: Some(sid.clone()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        let state = initialize_session_state(
            None,
            Some("gpt-5"),
            &crate::cli::cli_config::cli_context::CliContext::default(),
        );
        assert_eq!(state.session_id, None);
        assert_eq!(state.pending_recovery, None);
        assert!(state.history.is_empty());
        assert_eq!(state.turn, 0);
    }

    #[serial_test::serial]
    #[test]
    fn initialize_session_state_preserves_pending_recovery_when_workspace_is_corrupt() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let _creds_guard = isolate_credentials();

        let sid = format!("test-corrupt-pending-recovery-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&sid),
                Some("gpt-5"),
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::interruption_recorded(
                Some(&sid),
                0,
                serde_json::json!({
                    "kind": "rate_limited",
                    "resumable": true,
                    "has_checkpoint": true,
                    "tool_calls_completed": 0,
                    "turns_completed": 0,
                    "remaining_turns": 5,
                }),
            ))
            .unwrap();

        let workspace_dir = astra_services::session_workspace::workspace_dir_for(&sid);
        std::fs::create_dir_all(&workspace_dir).unwrap();
        std::fs::write(workspace_dir.join("workspace.yaml"), ":\nnot-valid-yaml").unwrap();

        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                last_session_id: Some(sid.clone()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        let state = initialize_session_state(
            None,
            Some("gpt-5"),
            &crate::cli::cli_config::cli_context::CliContext::default(),
        );
        assert_eq!(state.session_id, None);
        assert_eq!(state.pending_recovery.as_deref(), Some(sid.as_str()));
    }

    #[serial_test::serial]
    #[test]
    fn initialize_session_state_preserves_pending_recovery_for_workspace_only_corrupt_session() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let _creds_guard = isolate_credentials();

        let sid = format!(
            "test-workspace-only-corrupt-recovery-{}",
            uuid::Uuid::new_v4()
        );
        let workspace_dir = astra_services::session_workspace::workspace_dir_for(&sid);
        std::fs::create_dir_all(&workspace_dir).unwrap();
        std::fs::write(workspace_dir.join("workspace.yaml"), ":\nnot-valid-yaml").unwrap();

        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                last_session_id: Some(sid.clone()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        let state = initialize_session_state(
            None,
            Some("gpt-5"),
            &crate::cli::cli_config::cli_context::CliContext::default(),
        );
        assert_eq!(state.session_id, None);
        assert_eq!(state.pending_recovery.as_deref(), Some(sid.as_str()));
    }

    #[serial_test::serial]
    #[test]
    fn initialize_session_state_preserves_pending_recovery_when_workspace_missing() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let _creds_guard = isolate_credentials();

        let sid = format!("test-missing-pending-recovery-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&sid),
                Some("gpt-5"),
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::interruption_recorded(
                Some(&sid),
                0,
                serde_json::json!({
                    "kind": "rate_limited",
                    "resumable": true,
                    "has_checkpoint": true,
                    "tool_calls_completed": 0,
                    "turns_completed": 0,
                    "remaining_turns": 5,
                }),
            ))
            .unwrap();

        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                last_session_id: Some(sid.clone()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        let state = initialize_session_state(
            None,
            Some("gpt-5"),
            &crate::cli::cli_config::cli_context::CliContext::default(),
        );
        assert_eq!(state.session_id, None);
        assert_eq!(state.pending_recovery.as_deref(), Some(sid.as_str()));
    }

    #[serial_test::serial]
    #[test]
    fn initialize_session_state_ignores_stale_pending_recovery_without_local_state() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let _creds_guard = isolate_credentials();

        let sid = format!("test-stale-pending-recovery-{}", uuid::Uuid::new_v4());
        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                last_session_id: Some(sid),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        let state = initialize_session_state(
            None,
            Some("gpt-5"),
            &crate::cli::cli_config::cli_context::CliContext::default(),
        );
        assert_eq!(state.session_id, None);
        assert_eq!(state.pending_recovery, None);
    }

    #[serial_test::serial]
    #[test]
    fn pending_recovery_status_line_surfaces_workspace_unreadable() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let sid = format!("pending-recovery-corrupt-{}", uuid::Uuid::new_v4());
        let workspace_dir = astra_services::session_workspace::workspace_dir_for(&sid);
        std::fs::create_dir_all(&workspace_dir).unwrap();
        std::fs::write(workspace_dir.join("workspace.yaml"), ":\nnot-valid-yaml").unwrap();

        let state = SessionState {
            pending_recovery: Some(sid),
            ..SessionState::default()
        };

        let line = pending_recovery_status_line(&state).expect("pending recovery line");
        assert!(line.contains("previous session available via /resume"));
        assert!(line.contains("workspace metadata unreadable:"), "{line}");
    }

    // ── Session display logic ──────────────────────────────────────────────

    #[test]
    fn session_display_shows_new_for_none() {
        let state = SessionState::default();
        assert_eq!(banner_session_display(&state), "new");
    }

    #[test]
    fn session_display_shows_truncated_id_for_fresh_session() {
        let state = SessionState {
            session_id: Some("abcdef12-3456-7890".to_string()),
            ..Default::default()
        };
        assert_eq!(banner_session_display(&state), "abcdef12");
    }

    #[test]
    fn session_display_shows_resumed_for_restored_session() {
        let state = SessionState {
            session_id: Some("abcdef12-3456-7890".to_string()),
            turn: 3,
            ..Default::default()
        };
        assert_eq!(banner_session_display(&state), "abcdef12 (resumed)");
    }

    #[test]
    fn model_display_shows_auto_when_none() {
        let state = SessionState::default();
        let display = state.model.as_deref().unwrap_or("auto");
        assert_eq!(display, "auto");
    }

    #[test]
    fn model_display_shows_actual_name_when_set() {
        let state = SessionState {
            model: Some("gpt-5".to_string()),
            ..Default::default()
        };
        let display = state.model.as_deref().unwrap_or("auto");
        assert_eq!(display, "gpt-5");
    }

    #[test]
    fn banner_style_omits_ansi_when_colors_are_disabled() {
        let plain = style_banner_text("Message", BannerTextStyle::WhiteBold, false);
        assert_eq!(plain, "Message");
        assert!(!plain.contains('\x1b'));

        let colored = style_banner_text("Message", BannerTextStyle::WhiteBold, true);
        assert!(colored.contains('\x1b'));
    }

    #[test]
    fn banner_welcome_uses_username_when_logged_in() {
        let profile = Profile {
            username: Some("xupeng".to_string()),
            access_token: Some("token".to_string()),
            ..Default::default()
        };
        assert_eq!(
            banner_welcome_text("default", Some(&profile), true),
            "Welcome back, xupeng"
        );
    }

    #[test]
    fn banner_welcome_falls_back_to_profile_name() {
        let profile = Profile {
            access_token: Some("token".to_string()),
            ..Default::default()
        };
        assert_eq!(
            banner_welcome_text("test", Some(&profile), true),
            "Welcome back, test"
        );
    }

    #[test]
    fn banner_welcome_handles_logged_out_state() {
        assert_eq!(
            banner_welcome_text("default", None, false),
            "Welcome to astra"
        );
    }

    // ─── auto-auth regression guards ──────────────────────────
    //
    // During real-world verification the credentials file appeared
    // to have its access_token cleared after a `chat -m` run with
    // a still-valid token — which would be a real bug (forces the
    // user to /login every turn). The underlying behavior is
    // actually correct: `try_silent_auth` only reaches the
    // token-clearing branch when (a) auth_me returns 4xx AND (b)
    // refresh fails with a non-transient error AND (c) no
    // concurrent process has rotated the refresh token. The
    // perceived regression was from a specific 401 during one run
    // that happened to coincide with manual inspection.
    //
    // These tests pin the semantics of
    // `should_keep_credentials_on_refresh_error` so transport /
    // server-side / parse errors don't accidentally start clearing
    // creds — those are all recoverable conditions.

    #[test]
    fn server_5xx_preserves_credentials() {
        // Server is down temporarily — do NOT clobber creds. If we
        // cleared on 5xx, a transient server outage would force
        // every user to /login every time.
        use astra_thin_client::ThinClientError;
        let err = ThinClientError::Api {
            status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            body: "oops".into(),
        };
        assert!(
            should_keep_credentials_on_refresh_error(&err),
            "5xx from server must preserve credentials"
        );
    }

    #[test]
    fn malformed_refresh_response_preserves_credentials() {
        // Server returned 200 but body didn't parse — treat like
        // transient error. If refresh truly failed, next attempt
        // will classify correctly.
        let raw = serde_json::from_str::<serde_json::Value>("not-json").unwrap_err();
        let err = astra_thin_client::ThinClientError::Json(raw);
        assert!(
            should_keep_credentials_on_refresh_error(&err),
            "malformed refresh response must not clear credentials"
        );
    }

    #[test]
    fn server_4xx_clears_credentials() {
        // The one path that SHOULD clear: 4xx from /auth/refresh
        // means the refresh token is permanently invalid (revoked /
        // expired). Leaving it around would cause silent failure
        // on every subsequent refresh. Pin this behavior so a
        // future over-correction toward "never clear" doesn't sneak
        // in and hide real auth failures.
        use astra_thin_client::ThinClientError;
        let err = ThinClientError::Api {
            status: reqwest::StatusCode::UNAUTHORIZED,
            body: "token revoked".into(),
        };
        assert!(
            !should_keep_credentials_on_refresh_error(&err),
            "4xx from /auth/refresh must clear credentials so \
             next login works cleanly"
        );
    }

    #[test]
    fn keep_credentials_helper_enum_matches_silent_refresh_error() {
        // Cross-check: SilentRefreshError::keep_credentials must
        // agree with should_keep_credentials_on_refresh_error for
        // every Thin variant. A mismatch would make one code path
        // clear creds while another preserves them on the same
        // underlying error — confusing and hard to debug. Pin the
        // agreement at compile/test time.
        let thin = astra_thin_client::ThinClientError::Api {
            status: reqwest::StatusCode::UNAUTHORIZED,
            body: "".into(),
        };
        let direct = should_keep_credentials_on_refresh_error(&thin);
        let wrapped = SilentRefreshError::Thin(thin).keep_credentials();
        assert_eq!(
            direct, wrapped,
            "SilentRefreshError::Thin wrapper must defer to \
             should_keep_credentials_on_refresh_error"
        );
    }

    // ── ASTRA_ACCESS_TOKEN env var ──────────────────────────────

    #[serial_test::serial]
    #[test]
    fn current_access_token_prefers_env_var() {
        let _g = isolate_credentials();
        let _env = EnvGuard::set("ASTRA_ACCESS_TOKEN", "env-token-xyz");
        assert_eq!(
            current_access_token(None),
            Some("env-token-xyz".to_string()),
            "should return env var token when ASTRA_ACCESS_TOKEN is set"
        );
    }

    #[serial_test::serial]
    #[test]
    fn current_access_token_ignores_empty_env_var() {
        let _g = isolate_credentials();
        let _env = EnvGuard::set("ASTRA_ACCESS_TOKEN", "");
        // With empty env and no credentials file, should return None
        assert_eq!(
            current_access_token(None),
            None,
            "empty env var should be ignored"
        );
    }

    #[serial_test::serial]
    #[test]
    fn current_access_token_falls_back_to_file_without_env() {
        let _g = isolate_credentials();
        // Make sure ASTRA_ACCESS_TOKEN is not set (empty = ignored)
        let _env_clear = EnvGuard::set("ASTRA_ACCESS_TOKEN", "");
        // Write a credentials file to the isolated path
        let mut creds = CredentialsFile {
            current_profile: Some("default".to_string()),
            ..Default::default()
        };
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                username: Some("user".into()),
                access_token: Some("file-token-abc".into()),
                refresh_token: None,
                ..Default::default()
            },
        );
        let path = crate::cli::cli_config::cli_utils::credentials_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(&path, serde_json::to_string(&creds).unwrap()).unwrap();
        assert_eq!(
            current_access_token(None),
            Some("file-token-abc".to_string()),
            "should fall back to credentials file when env var is empty"
        );
    }

    #[serial_test::serial]
    #[test]
    fn current_access_token_falls_back_to_file_when_env_token_expired() {
        let _g = isolate_credentials();
        let _env = EnvGuard::set("ASTRA_ACCESS_TOKEN", &jwt_with_exp(1));

        let mut creds = CredentialsFile {
            current_profile: Some("default".to_string()),
            ..Default::default()
        };
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                username: Some("user".into()),
                access_token: Some("file-token-abc".into()),
                refresh_token: Some("refresh-token".into()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        assert_eq!(
            current_access_token(None),
            Some("file-token-abc".to_string()),
            "expired env token must not mask usable file credentials"
        );
    }

    #[test]
    fn access_token_needs_refresh_only_when_expired_or_near_expiry() {
        let now = 1_700_000_000_i64;
        assert!(access_token_needs_refresh(
            &jwt_with_exp(now + ACCESS_TOKEN_REFRESH_SKEW_SECS - 1),
            now
        ));
        assert!(access_token_needs_refresh(&jwt_with_exp(now - 1), now));
        assert!(!access_token_needs_refresh(
            &jwt_with_exp(now + ACCESS_TOKEN_REFRESH_SKEW_SECS + 120),
            now
        ));
        assert!(!access_token_needs_refresh("not-a-jwt", now));
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn fresh_access_token_refreshes_expired_saved_token() {
        let _g = isolate_credentials();
        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".into(),
            Profile {
                account_id: Some("user-id-1".to_string()),
                access_token: Some(
                    "eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0.eyJleHAiOjF9.sig".to_string(),
                ),
                refresh_token: Some("refresh-old".to_string()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/auth/refresh"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "user_id": "user-id-1",
                "access_token": "fresh-access",
                "refresh_token": "fresh-refresh"
            })))
            .mount(&mock)
            .await;
        let api = astra_thin_client::ThinClient::new(&mock.uri(), None).unwrap();

        let token = fresh_access_token(&api, None).await;

        assert_eq!(token.as_deref(), Some("fresh-access"));
        let creds = load_credentials();
        let profile = creds.profiles.get("default").unwrap();
        assert_eq!(profile.access_token.as_deref(), Some("fresh-access"));
        assert_eq!(profile.refresh_token.as_deref(), Some("fresh-refresh"));
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn fresh_access_token_keeps_valid_saved_token_without_refresh() {
        let _g = isolate_credentials();
        let token = "eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0.eyJleHAiOjQxMDAwMDAwMDB9.sig".to_string();
        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".into(),
            Profile {
                access_token: Some(token.clone()),
                refresh_token: Some("refresh-old".to_string()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/auth/refresh"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock)
            .await;
        let api = astra_thin_client::ThinClient::new(&mock.uri(), None).unwrap();

        let fresh = fresh_access_token(&api, None).await;

        assert_eq!(fresh.as_deref(), Some(token.as_str()));
        let creds = load_credentials();
        let profile = creds.profiles.get("default").unwrap();
        assert_eq!(profile.access_token.as_deref(), Some(token.as_str()));
        assert_eq!(profile.refresh_token.as_deref(), Some("refresh-old"));
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn fresh_access_token_fails_closed_when_expired_saved_token_refresh_fails() {
        let _g = isolate_credentials();
        let stale = jwt_with_exp(1);
        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".into(),
            Profile {
                access_token: Some(stale.clone()),
                refresh_token: Some("refresh-old".to_string()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/auth/refresh"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock)
            .await;
        let api = astra_thin_client::ThinClient::new(&mock.uri(), None).unwrap();

        let fresh = fresh_access_token(&api, None).await;

        assert_eq!(
            fresh, None,
            "expired credentials must not be reported as fresh when refresh fails"
        );
        let creds = load_credentials();
        let profile = creds.profiles.get("default").unwrap();
        assert_eq!(
            profile.access_token.as_deref(),
            Some(stale.as_str()),
            "transient refresh failure should preserve saved credentials for a later retry"
        );
        assert_eq!(profile.refresh_token.as_deref(), Some("refresh-old"));
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn fresh_access_token_fails_closed_when_expired_saved_token_has_no_refresh_token() {
        let _g = isolate_credentials();
        let stale = jwt_with_exp(1);
        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".into(),
            Profile {
                access_token: Some(stale.clone()),
                refresh_token: None,
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        let mock = MockServer::start().await;
        let api = astra_thin_client::ThinClient::new(&mock.uri(), None).unwrap();

        let fresh = fresh_access_token(&api, None).await;

        assert_eq!(
            fresh, None,
            "an expired token without refresh credentials is not usable auth"
        );
        assert!(mock.received_requests().await.unwrap().is_empty());
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn fresh_access_token_refreshes_profile_when_env_token_is_expired() {
        let _g = isolate_credentials();
        let _env = EnvGuard::set("ASTRA_ACCESS_TOKEN", &jwt_with_exp(1));
        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".into(),
            Profile {
                account_id: Some("user-id-1".to_string()),
                access_token: Some(jwt_with_exp(1)),
                refresh_token: Some("refresh-old".to_string()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/auth/refresh"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "user_id": "user-id-1",
                "access_token": "fresh-access",
                "refresh_token": "fresh-refresh"
            })))
            .mount(&mock)
            .await;
        let api = astra_thin_client::ThinClient::new(&mock.uri(), None).unwrap();

        let token = fresh_access_token(&api, None).await;

        assert_eq!(token.as_deref(), Some("fresh-access"));
        let creds = load_credentials();
        let profile = creds.profiles.get("default").unwrap();
        assert_eq!(profile.access_token.as_deref(), Some("fresh-access"));
        assert_eq!(profile.refresh_token.as_deref(), Some("fresh-refresh"));
    }

    #[test]
    fn git_root_from_prefers_repository_root_without_mutating_process_cwd() {
        let dir = tempfile::TempDir::new().unwrap();
        let nested = dir.path().join("nested");
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::fs::create_dir_all(&nested).unwrap();
        let root = git_root_from(&nested).unwrap();
        assert_eq!(
            std::fs::canonicalize(&root).unwrap(),
            std::fs::canonicalize(dir.path()).unwrap()
        );
    }

    #[test]
    fn git_root_from_returns_none_outside_repository() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(git_root_from(dir.path()).is_none());
    }
}
