use crate::cli::cli_config::cli_utils::{
    credential_store, load_credentials, normalize_model_override, profile_name,
};
use crate::cli::permission_manager::{PermissionManager, PermissionMode};
use crate::cli::session::session_state::SessionState;
use crate::cli::theme;
use crate::{manifest_loader, mcp_client};
use astra_services::session_journal;
#[cfg(test)]
use astra_text_utils::str_preview::prefix_chars;
use crossterm::style::Stylize;

pub(crate) fn create_pipeline_modules(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
) -> PipelineModules {
    create_pipeline_modules_inner(api, profile, true)
}

pub(crate) fn create_pipeline_modules_quiet(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
) -> PipelineModules {
    create_pipeline_modules_inner(api, profile, false)
}

pub(crate) fn local_task_service() -> std::sync::Arc<dyn astra_services::TaskService> {
    let tasks_dir = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".astra")
        .join("tasks");
    std::sync::Arc::new(astra_services::LocalTaskService::new(tasks_dir))
}

/// Resolve a `TaskService` impl for this CLI invocation.
///
/// Edge-cloud contract: the CLI never connects to MatrixOne
/// directly. When `cloud_base` is configured (via env or the
/// authenticated session), we return [`crate::cli::http_task_service::HttpTaskService`]
/// which proxies trait calls through `POST /tasks:rpc`. Otherwise
/// we fall back to the local on-disk store so offline / one-shot
/// CLI and headless tests stay functional.
///
/// `profile` is forwarded to the access-token lookup so the same
/// cloud session can be used for both the SSE stream and task RPC.
pub(crate) async fn resolve_task_service(
    profile: Option<&str>,
) -> std::sync::Arc<dyn astra_services::TaskService> {
    if let Some(cloud_base) = resolve_cloud_base() {
        let token = current_access_token(profile);
        return std::sync::Arc::new(crate::cli::http_task_service::HttpTaskService::new(
            cloud_base, token,
        ));
    }
    local_task_service()
}

/// Resolve the astra server base URL. Returns `None` when no server
/// is configured (offline mode).
pub(crate) fn resolve_cloud_base() -> Option<String> {
    std::env::var("ASTRA_API_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim_end_matches('/').to_string())
}

/// Task store for the durable per-session task board (`session_todos`).
///
/// When cloud is configured, returns an [`crate::cli::session::session_todo_client::HttpTaskStore`]
/// that polls the server's `GET /sessions/{sid}/todos` endpoint and
/// receives broadcast notifications from `route_task_action` after
/// every successful mutation. The observer sees tasks within one poll
/// cycle (~50ms after the dirty flag fires).
///
/// Offline / headless falls back to the in-memory store (no server).
///
/// Returns `(store, Option<notify_tx>)`. Callers wire `notify_tx`
/// into the tool executor so `route_task_action` can signal the
/// observer after each cloud write.
///
/// `cloud_base` selection: prefers the explicit caller-supplied URL
/// (typically `ThinClient::api_origin()` — the same source the tool
/// executor uses), falling back to the `ASTRA_API_URL` env var so
/// scripted callers without a thin client still hit the right server.
/// Never reads the env var when an explicit base is provided — having
/// two sources of truth was the root cause of the "TUI dashboard
/// never appears" bug: the executor used `api_origin()` while the
/// task store used the env var, so the in-memory store handed to
/// the observer never saw the cloud writes.
pub(crate) async fn resolve_task_store(
    profile: Option<&str>,
    cloud_base_override: Option<&str>,
) -> (
    std::sync::Arc<dyn astra_tools::task_mgmt::TaskStore>,
    Option<tokio::sync::broadcast::Sender<String>>,
) {
    let cloud_base = cloud_base_override
        .map(|s| s.trim_end_matches('/').to_string())
        .or_else(resolve_cloud_base);
    if let Some(cloud_base) = cloud_base {
        let token = current_access_token(profile);
        let (store, notify_tx) =
            crate::cli::session::session_todo_client::HttpTaskStore::new(cloud_base, token);
        return (store, Some(notify_tx));
    }
    (
        std::sync::Arc::new(astra_tools::task_mgmt::InMemoryTaskStore::new().with_validation()),
        None,
    )
}

pub(crate) fn install_task_service(
    state: &mut SessionState,
    task_service: std::sync::Arc<dyn astra_services::TaskService>,
) {
    state.task_service = Some(task_service);
}

/// Replace the task manager's store (used once at startup when we upgrade
/// from the synchronous in-memory fallback to an API-backed durable store).
/// The new manager inherits the current session_id.
pub(crate) fn install_task_store(
    state: &mut SessionState,
    store: std::sync::Arc<dyn astra_tools::task_mgmt::TaskStore>,
) {
    let session_id = state
        .session_id
        .clone()
        .unwrap_or_else(|| "no-session".to_string());
    state.task_manager =
        std::sync::Arc::new(astra_tools::task_mgmt::TaskManager::new(session_id, store));
}

/// Resolve the durable cloud background-task runtime (TaskService + lease).
///
/// Edge-cloud contract: no direct MO connection from the CLI. Both
/// services proxy through their REST surfaces:
/// - TaskService → `POST /tasks:rpc`
/// - TaskLeaseService → `/tasks/{id}/lease/*`
///
/// `profile` is forwarded to the access-token resolver so a logged-in
/// CLI invocation gets bearer auth.
pub(crate) async fn resolve_cloud_task_runtime(
    profile: Option<&str>,
) -> Result<
    (
        std::sync::Arc<dyn astra_services::TaskService>,
        std::sync::Arc<dyn astra_services::TaskLeaseService>,
    ),
    String,
> {
    let cloud_base = resolve_cloud_base().ok_or_else(|| {
        "Cloud task runtime requires ASTRA_API_URL; CLI does not connect to MatrixOne directly"
            .to_string()
    })?;
    let token = current_access_token(profile);
    let task_service: std::sync::Arc<dyn astra_services::TaskService> = std::sync::Arc::new(
        crate::cli::http_task_service::HttpTaskService::new(cloud_base.clone(), token.clone()),
    );
    let lease_service: std::sync::Arc<dyn astra_services::TaskLeaseService> = std::sync::Arc::new(
        crate::cli::http_task_service::HttpTaskLeaseService::new(cloud_base, token),
    );
    Ok((task_service, lease_service))
}

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

fn create_pipeline_modules_inner(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    announce_skills: bool,
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
    let unified_skill_registry =
        astra_runtime::capabilities::build_cli_local_skill_registry(remote_catalog);
    let handle = tokio::runtime::Handle::current();

    // Initialize MCP client manager and connect any MCP servers declared in
    // skill manifests. This registers `skill://` resources from connected
    // servers into the unified skill registry.
    let mcp_manager =
        std::sync::Arc::new(tokio::sync::RwLock::new(mcp_client::McpClientManager::new()));

    // Set initial roots to the current working directory.
    {
        if let Ok(cwd) = std::env::current_dir() {
            let uri = format!("file://{}", cwd.display());
            let root = rmcp::model::Root::new(uri).with_name("workspace");
            tokio::task::block_in_place(|| {
                handle.block_on(async {
                    let roots = mcp_manager.read().await.roots().clone();
                    roots.write().await.push(root);
                })
            });
        }
    }

    {
        let mcp_configs = manifest_loader::collect_mcp_server_configs();
        if !mcp_configs.is_empty() {
            let mgr = mcp_manager.clone();
            let reg = unified_skill_registry.clone();
            let _ = std::thread::scope(|s| {
                s.spawn(|| {
                    handle.block_on(async {
                        let mut manager = mgr.write().await;

                        // Collect (name, Ok(()) | Err(msg)) for every server.
                        let mut results: Vec<(String, Result<(), String>)> = Vec::new();
                        for config in mcp_configs {
                            let name = config.name.clone();
                            match mcp_client::connect_and_discover_skills(
                                &mut manager,
                                config,
                                &reg,
                            )
                            .await
                            {
                                Ok(_) => results.push((name, Ok(()))),
                                Err(e) => results.push((name, Err(format_mcp_error(&e)))),
                            }
                        }

                        if announce_skills {
                            // Count MCP tools per server (0 is fine — e.g. memoria).
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
        astra_skills::loader::skill_search_paths(),
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
pub(crate) enum ServerDefaultModel {
    Selected(String),
    NoModels,
    Unavailable,
}

fn model_list_entry_is_active(entry: &serde_json::Value) -> bool {
    if let Some(value) = entry.get("is_active") {
        if let Some(active) = value.as_bool() {
            return active;
        }
        if let Some(active) = value.as_i64() {
            return active != 0;
        }
        if let Some(active) = value.as_u64() {
            return active != 0;
        }
    }
    if let Some(value) = entry.get("active") {
        if let Some(active) = value.as_bool() {
            return active;
        }
        if let Some(active) = value.as_i64() {
            return active != 0;
        }
        if let Some(active) = value.as_u64() {
            return active != 0;
        }
    }
    true
}

fn model_list_entry_name(entry: &serde_json::Value) -> Option<&str> {
    entry
        .get("name")
        .or_else(|| entry.get("model_name"))
        .or_else(|| entry.get("model_id"))
        .or_else(|| entry.get("id"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|name| !name.is_empty())
}

pub(crate) fn default_model_from_models_response(body: &serde_json::Value) -> Option<String> {
    let models = body.as_array().or_else(|| {
        body.get("models")
            .or_else(|| body.get("items"))
            .and_then(|value| value.as_array())
    })?;
    models
        .iter()
        .filter(|entry| model_list_entry_is_active(entry))
        .find_map(|entry| model_list_entry_name(entry).map(str::to_string))
}

/// Resolve the model the CLI should preselect when the user did not explicitly
/// choose one. `Unavailable` preserves the old optimistic startup behavior for
/// transient API/model-list failures.
pub(crate) async fn resolve_server_default_model(
    api: &astra_thin_client::ThinClient,
    token: &str,
) -> ServerDefaultModel {
    let resp = match api
        .get_models_response_timeout(token, std::time::Duration::from_secs(3))
        .await
    {
        Ok(r) if r.status().is_success() => r,
        _ => return ServerDefaultModel::Unavailable,
    };
    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(_) => return ServerDefaultModel::Unavailable,
    };
    match default_model_from_models_response(&body) {
        Some(model) => ServerDefaultModel::Selected(model),
        None => ServerDefaultModel::NoModels,
    }
}

pub(crate) async fn ensure_state_default_model(
    api: &astra_thin_client::ThinClient,
    token: &str,
    state: &mut SessionState,
) -> Option<String> {
    if let Some(model) = normalize_model_override(state.model.as_deref()) {
        return Some(model.to_string());
    }
    match resolve_server_default_model(api, token).await {
        ServerDefaultModel::Selected(model) => {
            state.model = Some(model.clone());
            Some(model)
        }
        ServerDefaultModel::NoModels | ServerDefaultModel::Unavailable => None,
    }
}

/// Outcome of `try_refresh_token` for deciding whether on-disk credentials may still be valid.
#[derive(Debug)]
enum SilentRefreshError {
    Thin(astra_thin_client::ThinClientError),
    /// HTTP 200 body was not usable; keep existing tokens.
    BadResponse(&'static str),
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
    let value: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| SilentRefreshError::Thin(e.into()))?;
    let new_access = value.get("access_token").and_then(|v| v.as_str()).ok_or(
        SilentRefreshError::BadResponse("refresh response: missing access_token"),
    )?;
    let new_refresh = value.get("refresh_token").and_then(|v| v.as_str()).ok_or(
        SilentRefreshError::BadResponse("refresh response: missing refresh_token"),
    )?;
    let new_access = new_access.to_string();
    let new_refresh = new_refresh.to_string();
    credential_store()
        .mutate(|creds| {
            let name = profile_name(profile, creds);
            let entry = creds.profiles.entry(name).or_default();
            entry.access_token = Some(new_access.clone());
            entry.refresh_token = Some(new_refresh.clone());
        })
        .map_err(|e| SilentRefreshError::SaveFailed(e.to_string()))?;
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
        return Some(token);
    }

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
                .unwrap_or(PermissionMode::Prompt),
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

    // Initialize a durable task service synchronously; startup paths that can
    // Startup may upgrade this to an API-backed store via `resolve_task_store`.
    install_task_service(&mut state, local_task_service());

    // Initialize observability hub for M1-M6 integration
    // Use persistent storage under ~/.astra/observability for user profiles
    let obs_path = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".astra")
        .join("observability");
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

fn current_git_root() -> Option<std::path::PathBuf> {
    std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| {
            let root = String::from_utf8(output.stdout).ok()?;
            let trimmed = root.trim();
            (!trimmed.is_empty()).then(|| std::path::PathBuf::from(trimmed))
        })
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

/// Full session counters + history from local JSONL (used after `/session fork`).
pub fn session_state_from_journal(session_id: &str) -> Result<RestoredSessionState, String> {
    Ok(restore_session_state_from_journal(session_id)?.session)
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
    let events = match session_journal::read_journal(session_id) {
        Ok(events) => events,
        Err(error) => {
            return Err(format!(
                "failed to read session journal for {session_id}: {error}"
            ));
        }
    };

    let mut restored = RestoredSessionState::default();
    let start_idx = events
        .iter()
        .rposition(|event| event.event_type == session_journal::JournalEventType::SessionStart)
        .map(|idx| idx + 1)
        .unwrap_or(0);
    let last_turn_event = events[start_idx..]
        .iter()
        .rev()
        .find(|event| event.event_type == session_journal::JournalEventType::Turn)
        .cloned();

    for event in events.into_iter().skip(start_idx) {
        if event.event_type != session_journal::JournalEventType::Turn {
            continue;
        }
        restored.history.push((
            event.user_input.unwrap_or_default(),
            event.assistant_output.unwrap_or_default(),
        ));
        restored.turn = restored
            .turn
            .max(event.turn.unwrap_or(restored.turn.saturating_add(1)));
        restored.total_prompt_tokens += event.tokens_in.unwrap_or(0);
        restored.total_completion_tokens += event.tokens_out.unwrap_or(0);
        restored.total_cache_read_tokens += event.cache_read_tokens.unwrap_or(0);
        restored.total_cache_creation_tokens += event.cache_creation_tokens.unwrap_or(0);
        if let Some(tools_used) = event.tools_used {
            restored.recent_tools = tools_used;
        }
    }

    Ok(RestoredJournalState {
        exists: journal_exists,
        session: restored,
        last_turn_event,
    })
}

pub(crate) fn print_session_banner(profile: Option<&str>, state: &SessionState) {
    let creds = load_credentials();
    let pname = profile_name(profile, &creds);
    let p = creds.profiles.get(&pname);
    let logged_in = p.and_then(|p| p.access_token.as_ref()).is_some();
    let model_display = state.model.as_deref().unwrap_or("auto");
    let version = env!("CARGO_PKG_VERSION");
    let skills_count = state.unified_skill_registry.len();

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
        model_display.bold().yellow(),
        "·".white().bold(),
        format!("v{version} · {pname}").white().bold()
    );

    // Build left column first so we can measure its actual width
    let mut left: Vec<String> = Vec::new();
    for line in logo_plain {
        if line.is_empty() {
            left.push(String::new());
        } else if *line == "    astra" {
            left.push(format!("{}", line.magenta().bold()));
        } else {
            left.push(format!("{}", line.magenta()));
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
    right.push(trunc_vis("Tips", right_col_w).white().bold().to_string());
    right.push(
        trunc_vis("/help for all commands", right_col_w)
            .white()
            .bold()
            .to_string(),
    );
    right.push(
        trunc_vis("Ctrl+K command picker", right_col_w)
            .white()
            .bold()
            .to_string(),
    );
    right.push(
        trunc_vis("Alt+Enter multi-line input", right_col_w)
            .white()
            .bold()
            .to_string(),
    );
    right.push(sep_line.as_str().white().bold().to_string());
    right.push(trunc_vis("Status", right_col_w).white().bold().to_string());
    right.push(
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
        )
        .white()
        .bold()
        .to_string(),
    );
    if let Some(line) = pending_recovery_status_line(state) {
        let truncated = trunc_vis(&line, right_col_w);
        right.push(truncated.yellow().bold().to_string());
    }
    if let Ok(proxy) = std::env::var("http_proxy").or_else(|_| std::env::var("HTTP_PROXY")) {
        if !proxy.is_empty() {
            let max_proxy = right_col_w.saturating_sub(8);
            let short = if proxy.len() > max_proxy {
                format!("{}…", &proxy[..max_proxy.saturating_sub(1)])
            } else {
                proxy
            };
            right.push(format!("proxy: {short}").white().bold().to_string());
        }
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

    fn render_banner_frame(layout: &BannerLayout<'_>, with_stars: bool, mut rng_seed: u64) {
        use crossterm::style::Stylize;
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
                    out.push_str(&format!("{}", s.dark_grey()));
                } else {
                    out.push(' ');
                }
            }
            out
        };

        // Header — title is embedded inline; brighter so it stands out.
        eprint!("{}", "╭".white().bold());
        eprint!("{}", "─".repeat(*lead_dash).white().bold());
        eprint!("{}", title_padded.bold().cyan());
        eprint!("{}", "─".repeat(*trail_dash).white().bold());
        eprintln!("{}", "╮".white().bold());
        // Body
        for row in 0..*total_rows {
            let l_pad = starfield_pad(*left_col_w, vis_w(&left[row]), &mut rng_seed, 12);
            let r_pad = starfield_pad(*right_col_w, vis_w(&right[row]), &mut rng_seed, 8);
            eprintln!(
                "{} {}{} {} {}{} {}",
                "│".white().bold(),
                left[row],
                l_pad,
                "│".white().bold(),
                right[row],
                r_pad,
                "│".white().bold(),
            );
        }
        // Footer
        eprintln!(
            "{}{}{}",
            "╰".white().bold(),
            h_bar.white().bold(),
            "╯".white().bold()
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
            render_banner_frame(&layout, true, seed);
            std::thread::sleep(Duration::from_millis(100));
        }
        eprint!("\x1b[{}A\r", card_lines);
        render_banner_frame(&layout, false, 0);
    } else {
        render_banner_frame(&layout, false, 0);
    }

    eprintln!();
    let welcome = banner_welcome_text(&pname, p, logged_in);
    let model_hint = if model_display == "auto" {
        format!("{} {}", "auto".yellow(), "mode".grey())
    } else {
        format!("{} {}", model_display.cyan(), "mode".grey())
    };
    eprintln!(
        "  {} {} {}",
        welcome.cyan(),
        model_hint,
        "· /model to change".grey()
    );
    eprintln!();
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
        ACCESS_TOKEN_REFRESH_SKEW_SECS, RestoredSessionState, SilentRefreshError,
        access_token_needs_refresh, banner_session_display, banner_welcome_text,
        current_access_token, current_git_root, default_model_from_models_response,
        fresh_access_token, initialize_session_state, pending_recovery_status_line,
        restore_history_from_journal, restore_session_state_from_journal, restored_journal_state,
        should_keep_credentials_on_refresh_error,
    };
    use crate::cli::cli_config::cli_utils::{
        CredentialsFile, Profile, load_credentials, save_credentials,
    };
    use crate::cli::session::session_state::SessionState;
    use crate::tests::isolate_credentials;
    use astra_services::session_journal;
    use tempfile::tempdir;
    use wiremock::matchers::{method, path};
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

    #[test]
    fn default_model_from_models_response_uses_first_active_name() {
        let body = serde_json::json!([
            {"name": "inactive-model", "is_active": false},
            {"name": "  deepseek-v4-flash-anthropic  ", "is_active": true},
            {"name": "deepseek-v4-pro-official", "is_active": true}
        ]);

        assert_eq!(
            default_model_from_models_response(&body).as_deref(),
            Some("deepseek-v4-flash-anthropic")
        );
    }

    #[test]
    fn default_model_from_models_response_accepts_wrapped_legacy_shape() {
        let body = serde_json::json!({
            "models": [
                {"model_name": "legacy-inactive", "active": 0},
                {"model_name": "legacy-active", "active": 1}
            ]
        });

        assert_eq!(
            default_model_from_models_response(&body).as_deref(),
            Some("legacy-active")
        );
    }

    #[test]
    fn default_model_from_models_response_accepts_items_shape() {
        let body = serde_json::json!({
            "items": [
                {"id": "row-without-name", "is_active": true},
                {"name": "items-active-model", "is_active": true}
            ]
        });

        assert_eq!(
            default_model_from_models_response(&body).as_deref(),
            Some("row-without-name")
        );
    }

    #[test]
    fn default_model_from_models_response_uses_model_id_when_name_is_missing() {
        let body = serde_json::json!({
            "models": [
                {"name": "inactive", "is_active": false},
                {"model_id": "row-without-name", "is_active": true}
            ]
        });

        assert_eq!(
            default_model_from_models_response(&body).as_deref(),
            Some("row-without-name")
        );
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
            None,
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
}
