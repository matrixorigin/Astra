use super::*;

pub(super) fn create_tool_selector(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
) -> (Box<dyn tool_selector::ToolSelector>, PipelineModules) {
    create_tool_selector_with_quality(api, profile, None, None)
}

pub(super) fn create_tool_selector_quiet(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
) -> (Box<dyn tool_selector::ToolSelector>, PipelineModules) {
    create_tool_selector_with_quality_internal(api, profile, None, None, false)
}

/// Shared pipeline learning modules — kept accessible for cross-session persistence.
pub(super) struct PipelineModules {
    pub entity_graph:
        std::sync::Arc<std::sync::Mutex<astra_runtime::pipeline::entity::EntityGraph>>,
    pub pattern_library:
        std::sync::Arc<std::sync::Mutex<astra_runtime::pipeline::pattern::PatternLibrary>>,
    pub calibrator: std::sync::Arc<
        std::sync::Mutex<astra_runtime::pipeline::calibration::ProgressiveCalibrator>,
    >,
    /// Unified skill registry (single source of truth for all skill resolution).
    pub unified_skill_registry: std::sync::Arc<astra_runtime::skills::UnifiedSkillRegistry>,
    /// MCP client manager for external tool servers.
    pub mcp_manager: std::sync::Arc<tokio::sync::RwLock<mcp_client::McpClientManager>>,
    /// File-system watcher for skill hot-reload (kept alive while REPL runs).
    pub _skill_watcher: Option<astra_runtime::skills::watcher::SkillWatcherHandle>,
}

pub(super) fn create_tool_selector_with_quality(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    quality_tracker: Option<std::sync::Arc<std::sync::Mutex<tool_registry::ToolQualityTracker>>>,
    confidence_calibrator: Option<
        std::sync::Arc<astra_runtime::turn::routing_metrics::ConfidenceCalibrator>,
    >,
) -> (Box<dyn tool_selector::ToolSelector>, PipelineModules) {
    create_tool_selector_with_quality_internal(
        api,
        profile,
        quality_tracker,
        confidence_calibrator,
        true,
    )
}

fn create_tool_selector_with_quality_internal(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    quality_tracker: Option<std::sync::Arc<std::sync::Mutex<tool_registry::ToolQualityTracker>>>,
    confidence_calibrator: Option<
        std::sync::Arc<astra_runtime::turn::routing_metrics::ConfidenceCalibrator>,
    >,
    _announce_skills: bool,
) -> (Box<dyn tool_selector::ToolSelector>, PipelineModules) {
    use astra_runtime::pipeline::{
        calibration::ProgressiveCalibrator, entity::EntityGraph, pattern::PatternLibrary,
    };

    let all_schemas = edge_tools::all_tool_schemas();
    let mut registry = tool_registry::ToolRegistry::new(all_schemas);

    // Load skill manifests from skills/ directory and register plugin tools
    let mut plugin_registry = tool_registry::PluginRegistry::new();
    manifest_loader::load_skills_directory(&mut plugin_registry);
    registry.register_plugins(&plugin_registry);

    let mut tfidf = tool_selector::TfIdfSelector::new(registry);
    if let Some(qt) = quality_tracker {
        tfidf = tfidf.with_quality_tracker(qt);
    }
    if let Some(cal) = confidence_calibrator {
        tfidf = tfidf.with_confidence_calibrator(cal);
    }

    // Wire pipeline learning modules for progressive improvement
    let entity_graph = std::sync::Arc::new(std::sync::Mutex::new(EntityGraph::new()));
    let pattern_library = std::sync::Arc::new(std::sync::Mutex::new(PatternLibrary::new()));
    let calibrator = std::sync::Arc::new(std::sync::Mutex::new(ProgressiveCalibrator::new(0.15)));
    tfidf = tfidf
        .with_entity_graph(entity_graph.clone())
        .with_pattern_library(pattern_library.clone())
        .with_progressive_calibrator(calibrator.clone());

    // Initialize unified skill registry with providers (priority: Local > Bundled)
    let mut unified_skill_registry = astra_runtime::skills::UnifiedSkillRegistry::new();
    unified_skill_registry.add_provider(Box::new(
        astra_runtime::skills::LocalSkillProvider::standard(),
    ));
    unified_skill_registry.add_provider(Box::new(
        astra_runtime::skills::BundledSkillProvider::with_defaults(),
    ));
    let unified_skill_registry = std::sync::Arc::new(unified_skill_registry);
    // Discover skills eagerly so the `skill` tool schema is populated from the first turn.
    // This is a sync context, so bridge to async via a scoped thread.
    let handle = tokio::runtime::Handle::current();
    let _ = std::thread::scope(|s| {
        s.spawn(|| handle.block_on(unified_skill_registry.discover_all()))
            .join()
            .expect("skill discover thread panicked")
    });

    // Initialize MCP client manager and connect any MCP servers declared in
    // skill manifests. This registers `skill://` resources from connected
    // servers into the unified skill registry.
    let mcp_manager =
        std::sync::Arc::new(tokio::sync::RwLock::new(mcp_client::McpClientManager::new()));

    // Configure sampling so MCP servers can request LLM completions.
    {
        if let Some(token) = current_access_token(profile) {
            let sampling = mcp_client::SamplingConfig {
                api: std::sync::Arc::new(
                    astra_thin_client::ThinClient::new(api.api_origin().as_str(), None)
                        .expect("valid API origin for sampling"),
                ),
                token,
                model: "default".to_string(),
                max_tokens_cap: mcp_client::DEFAULT_SAMPLING_MAX_TOKENS_CAP,
            };
            tokio::task::block_in_place(|| {
                handle.block_on(async {
                    mcp_manager
                        .write()
                        .await
                        .set_sampling_config(Some(sampling));
                })
            });
        }
    }

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
                        for config in mcp_configs {
                            let name = config.name.clone();
                            match manager.connect_and_discover_skills(config, &reg).await {
                                Ok(n) if n > 0 => {
                                    eprintln!(
                                        "  {} Connected '{}' ({n} tool{})",
                                        crossterm::style::Stylize::cyan("✓"),
                                        name,
                                        if n == 1 { "" } else { "s" }
                                    );
                                }
                                Ok(_) => {}
                                Err(e) => {
                                    eprintln!(
                                        "  {} MCP server '{}' failed to connect: {}",
                                        theme::icon_warn(),
                                        name,
                                        e
                                    );
                                }
                            }
                        }
                    })
                })
                .join()
                .unwrap_or_else(|e| eprintln!("  ⚠ MCP connection thread panicked: {e:?}"))
            });
        }
    }

    // Start file-system watcher for skill hot-reload
    let skill_watcher = astra_runtime::skills::watcher::start_watching(
        unified_skill_registry.clone(),
        astra_runtime::skills::loader::skill_search_paths(),
    );

    let creds = load_credentials();
    let name = profile_name(profile, &creds);
    let _token = creds
        .profiles
        .get(&name)
        .and_then(|p| p.access_token.as_ref())
        .cloned();

    let modules = PipelineModules {
        entity_graph,
        pattern_library,
        calibrator,
        unified_skill_registry,
        mcp_manager,
        _skill_watcher: skill_watcher,
    };

    // TF-IDF selector only — no LLM-based tool selection.
    // The previous FallbackSelector(LLM, TfIdf) added an extra LLM API call per turn
    // just to pick tools, adding 10-80s latency depending on the model. TF-IDF is
    // fast (<1ms), deterministic, and works well for all common query patterns.
    // Skill activation is handled by the `skill` tool in the agentic loop.
    let selector: Box<dyn tool_selector::ToolSelector> = Box::new(tfidf);

    (selector, modules)
}

/// Tool selector for background plan execution.
///
/// Uses TF-IDF selector only — no LLM-based tool selection (same as foreground REPL).
/// Attaches entity_graph, pattern_library, and calibrator `Arc`s when available so
/// subtasks read the same learned state as the REPL.
pub(crate) fn create_background_plan_selector(
    ctx: &crate::plan_executor::BackgroundPlanContext,
) -> Box<dyn tool_selector::ToolSelector> {
    let all_schemas = edge_tools::all_tool_schemas();
    let mut registry = tool_registry::ToolRegistry::new(all_schemas);
    let mut plugin_registry = tool_registry::PluginRegistry::new();
    manifest_loader::load_skills_directory(&mut plugin_registry);
    registry.register_plugins(&plugin_registry);

    let mut tfidf = tool_selector::TfIdfSelector::new(registry);
    if let (Some(eg), Some(pl), Some(cal)) = (
        ctx.entity_graph.as_ref(),
        ctx.pattern_library.as_ref(),
        ctx.calibrator.as_ref(),
    ) {
        tfidf = tfidf
            .with_entity_graph(eg.clone())
            .with_pattern_library(pl.clone())
            .with_progressive_calibrator(cal.clone());
    }

    Box::new(tfidf)
}

/// Quick check whether the server has at least one LLM model configured.
/// Returns `true` on network errors (optimistic — don't block startup).
pub(super) async fn check_server_has_models(
    api: &astra_thin_client::ThinClient,
    token: &str,
) -> bool {
    let resp = match api
        .get_models_response_timeout(token, std::time::Duration::from_secs(3))
        .await
    {
        Ok(r) if r.status().is_success() => r,
        _ => return true,
    };
    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(_) => return true,
    };
    if let Some(arr) = body.as_array() {
        return !arr.is_empty();
    }
    if let Some(arr) = body.get("models").and_then(|v| v.as_array()) {
        return !arr.is_empty();
    }
    true
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
pub(super) async fn try_silent_auth(api: &astra_thin_client::ThinClient, profile: Option<&str>) {
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
            Ok(resp) if resp.status().is_client_error() => {
                // 4xx (401 expired, 404 user gone, etc.) — token is invalid
            }
            _ => return, // Network error or 5xx: proceed with cached creds
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
            return;
        }
        Err(err) => {
            // Do not wipe local creds on transport failures, 5xx, or malformed JSON —
            // the access/refresh pair on disk may still be valid.
            if err.keep_credentials() {
                return;
            }
            // Another CLI may have won refresh first (server revokes old refresh on success).
            if recover_credentials_after_refresh_race(api, profile, refresh_str).await {
                return;
            }
        }
    }

    let mut creds = load_credentials();
    let name = profile_name(profile, &creds);
    if let Some(p) = creds.profiles.get_mut(&name) {
        p.access_token = None;
        // Keep refresh_token for next login attempt - it may still be valid
        // even if this refresh attempt failed (e.g., due to temporary network issues).
        // If refresh_token is truly expired, the next refresh will also fail and
        // user will be prompted to re-login then.
    }
    if let Err(e) = save_credentials(&creds) {
        tracing::warn!(
            target: "astra_cli::repl_runtime",
            error = %e,
            "failed to save credentials after token refresh"
        );
    }
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
    let mut creds = load_credentials();
    let name = profile_name(profile, &creds);
    let entry = creds.profiles.entry(name).or_default();
    entry.access_token = Some(new_access.to_string());
    entry.refresh_token = Some(new_refresh.to_string());
    save_credentials(&creds).map_err(SilentRefreshError::SaveFailed)?;
    Ok(())
}

/// Attempt to refresh an expired token mid-session.
///
/// Returns `true` (and persists new credentials) on success.
/// On failure, logs the underlying reason before returning `false`, so users
/// can see why a refresh did not recover their session.
pub(super) async fn attempt_token_refresh(
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

pub(super) fn build_repl_editor() -> Result<(Editor<ReplHelper, FileHistory>, PathBuf), String> {
    let hist_path = history_path();
    if let Some(parent) = hist_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let config = Config::builder()
        .completion_type(CompletionType::List)
        .build();
    let mut editor: Editor<ReplHelper, FileHistory> =
        Editor::with_config(config).map_err(|e| e.to_string())?;
    editor.set_helper(Some(ReplHelper));
    editor.bind_sequence(
        RlEvent::Any,
        RlEventHandler::Conditional(Box::new(SlashStartCompleteHandler)),
    );
    let _ = editor.load_history(&hist_path);
    Ok((editor, hist_path))
}

pub(crate) fn initialize_repl_state(
    profile: Option<&str>,
    initial_model: Option<&str>,
) -> ReplState {
    let mut state = ReplState::default();
    state.pending_recovery = detect_pending_recovery_session(profile);
    state.pending_plan_resume_digest = detect_pending_plan_resume_digest();
    if let Some(m) = initial_model {
        state.model = Some(m.to_string());
    }

    // Initialize local task service
    let tasks_dir = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".astra")
        .join("tasks");
    state.task_service = Some(std::sync::Arc::new(astra_services::LocalTaskService::new(
        tasks_dir,
    )));

    // Initialize observability hub for M1-M6 integration
    // Use persistent storage under ~/.astra/observability for user profiles
    let obs_path = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".astra")
        .join("observability");
    state.observability_hub = Some(std::sync::Arc::new(
        astra_runtime::observability_integration::ObservabilityHub::with_storage(obs_path),
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
        super::repl_turn::apply_pending_adaptive_state(&mut state);
        super::repl_turn::apply_pending_goal_progress_state(&mut state);
    }

    // Restore persisted feedback aggregator state (if any)
    if let Some(ref hub) = state.observability_hub {
        if let Err(e) = astra_runtime::auto_tuning::load_feedback("default", hub.tuning()) {
            eprintln!("[auto-tuning] failed to load feedback: {e}");
        }
    }

    state
}

fn detect_pending_recovery_session(cli_profile: Option<&str>) -> Option<String> {
    let session_id = resumable_last_session_id(cli_profile)?;
    let workspace = astra_services::session_workspace::read_workspace(&session_id).ok()?;
    workspace_matches_current_project(&workspace).then_some(session_id)
}

/// P3.3 — if a persisted plan_state.json exists for the active user, load it
/// and compute a compact resume digest. The digest is a one-shot hint: it is
/// only injected into the next turn's system prompt when the user message
/// signals resume intent, and cleared thereafter.
fn detect_pending_plan_resume_digest() -> Option<String> {
    let path = astra_runtime::plan_decompose::PlanModeState::state_path();
    if !path.exists() {
        return None;
    }
    match astra_runtime::plan_decompose::PlanModeState::load_with_recovery(&path) {
        Ok(state) => {
            // B8: Don't surface a resume hint for a plan from a different
            // workspace — it would mis-direct @resume-plan.
            let cwd = std::env::current_dir().ok()?;
            if !state.matches_workspace(&cwd) {
                return None;
            }
            astra_runtime::plan::plan_resume::plan_resume_digest(&state)
        }
        Err(err) => {
            eprintln!("  ⚠ plan_state.json present but unreadable: {err}");
            None
        }
    }
}

const PLAN_RESUME_STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(30 * 60);

fn plan_context_is_stale(
    modified: std::time::SystemTime,
    now: std::time::SystemTime,
    stale_after: std::time::Duration,
) -> bool {
    now.duration_since(modified)
        .map(|elapsed| elapsed >= stale_after)
        .unwrap_or(false)
}

fn should_refresh_pending_plan_context(path: &std::path::Path) -> bool {
    path.metadata()
        .ok()
        .and_then(|meta| meta.modified().ok())
        .is_some_and(|modified| {
            plan_context_is_stale(
                modified,
                std::time::SystemTime::now(),
                PLAN_RESUME_STALE_AFTER,
            )
        })
}

pub(crate) fn maybe_restore_pending_plan_mode(line: &str, state: &mut ReplState) -> bool {
    if state.plan_mode.is_some()
        || state.executing_plan.is_some()
        || state.plan_handle.is_some()
        || state.pending_plan_resume_digest.is_none()
        || !line.contains("@resume-plan")
    {
        return false;
    }

    let path = astra_runtime::plan_decompose::PlanModeState::state_path();
    if !path.exists() {
        state.pending_plan_resume_digest = None;
        return false;
    }

    match astra_runtime::plan_decompose::PlanModeState::load_with_recovery(&path) {
        Ok(mut plan_state) => {
            // B8: Refuse to restore a plan from a different workspace —
            // continuing it against the wrong project would corrupt
            // both contexts.
            let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            if !plan_state.matches_workspace(&cwd) {
                eprintln!(
                    "  ⚠ Saved plan belongs to a different workspace ({}) — \
                     ignoring @resume-plan in this directory.",
                    plan_state.context.root
                );
                state.pending_plan_resume_digest = None;
                return false;
            }

            if should_refresh_pending_plan_context(&path) {
                let project_root =
                    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
                plan_state.context = astra_runtime::plan_decompose::analyze_project(&project_root);
                let _ = plan_state.save_with_backup(&path);
            }

            state.pending_plan_resume_digest = None;
            state.chat_plan_only = false;
            state.plan_mode = Some(plan_state);
            true
        }
        Err(err) => {
            eprintln!("  ⚠ failed to restore saved plan mode: {err}");
            state.pending_plan_resume_digest = None;
            false
        }
    }
}

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

#[derive(Debug, Default, PartialEq)]
pub struct RestoredSessionState {
    pub history: Vec<(String, String)>,
    pub turn: u32,
    pub recent_tools: Vec<String>,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
}

/// Rebuild `(user_msg, assistant_msg)` history from the session journal.
/// Only `Turn` events with both user_input and assistant_output are included.
pub(super) fn restore_history_from_journal(session_id: &str) -> Vec<(String, String)> {
    restore_session_state_from_journal(session_id).history
}

/// Full session counters + history from local JSONL (used after `/session fork`).
pub fn session_state_from_journal(session_id: &str) -> RestoredSessionState {
    restore_session_state_from_journal(session_id)
}

fn restore_session_state_from_journal(session_id: &str) -> RestoredSessionState {
    let Ok(events) = session_journal::read_journal(session_id) else {
        return RestoredSessionState::default();
    };

    let mut restored = RestoredSessionState::default();
    let start_idx = events
        .iter()
        .rposition(|event| event.event_type == session_journal::JournalEventType::SessionStart)
        .map(|idx| idx + 1)
        .unwrap_or(0);

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
        if let Some(tools_used) = event.tools_used {
            restored.recent_tools = tools_used;
        }
    }

    restored
}

fn random_tips(logged_in: bool) -> [&'static str; 2] {
    use std::time::SystemTime;

    const TIPS: &[&str] = &[
        "Type / to browse all commands",
        "Ctrl+K opens the command picker on an empty line",
        "Ctrl+R to search command history",
        "Alt+Enter for multi-line input",
        "/explain toggles reasoning visibility",
        "/diff for colored unified git diff; /plan on for plan-only chat (no tools)",
        "/stats shows session token usage",
        "/tools shows tool call performance",
        "/health for tool health dashboard",
        "/resume to continue a previous session",
        "/session to see current session info",
        "/learn to see learning insights",
        "End a line with \\ to continue on next line",
        "/diagnostics runs API/binary/auth checks if something feels off",
        "/sync shows cloud sync status",
    ];

    const TIPS_NOT_LOGGED_IN: &[&str] = &[
        "/login to authenticate with existing account",
        "/register to create a new account",
        "Most features require login to work",
    ];

    let pool = if logged_in { TIPS } else { TIPS_NOT_LOGGED_IN };
    let seed = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as usize)
        .unwrap_or(0);
    let i = seed % pool.len();
    let j = (i + 1 + seed / pool.len()) % pool.len();
    // Ensure two different tips
    let j = if j == i { (i + 1) % pool.len() } else { j };
    [pool[i], pool[j]]
}

pub(super) fn print_repl_banner(profile: Option<&str>, state: &ReplState) {
    let creds = load_credentials();
    let pname = profile_name(profile, &creds);
    let p = creds.profiles.get(&pname);
    let logged_in = p.and_then(|p| p.access_token.as_ref()).is_some();
    let user_display = match (p.and_then(|p| p.username.as_deref()), logged_in) {
        (Some(name), true) => name.to_string(),
        (Some(name), false) => format!("{name} (not logged in)"),
        (None, _) => "not logged in".to_string(),
    };
    let session_display = banner_session_display(state);
    let model_display = state.model.as_deref().unwrap_or("auto");
    let version = env!("CARGO_PKG_VERSION");

    let [tip1, tip2] = random_tips(logged_in);
    let tip1_plain = format!("  💡 {tip1}");
    let tip2_plain = format!("  💡 {tip2}");

    let lines_plain = [
        format!("  astra  v{version}  powered by MatrixOne"),
        format!(
            "  profile: {}  user: {}  model: {}  session: {}",
            pname, user_display, model_display, session_display
        ),
        tip1_plain,
        tip2_plain,
    ];
    let w = lines_plain
        .iter()
        .map(|l| crate::terminal_region::visible_char_width(l))
        .max()
        .unwrap_or(60)
        + 2;

    let lines_colored = [
        format!(
            "  {}  {}  {}",
            "astra".cyan().bold(),
            format!("v{version}").dim(),
            "powered by MatrixOne".dim()
        ),
        format!(
            "  profile: {}  user: {}  model: {}  session: {}",
            pname.cyan(),
            if logged_in {
                user_display.dim().to_string()
            } else {
                user_display.yellow().to_string()
            },
            model_display.cyan(),
            session_display.as_str().dim(),
        ),
        format!("  💡 {}", tip1.dim()),
        format!("  💡 {}", tip2.dim()),
    ];

    let row = |colored: &str, plain_width: usize| {
        let pad = w.saturating_sub(plain_width);
        format!("{} {colored}{} {}", "│".cyan(), " ".repeat(pad), "│".cyan())
    };

    let hr = "─".repeat(w + 2);

    eprintln!();
    print_startup_logo();
    eprintln!("{}", format!("╭{hr}╮").cyan());
    eprintln!(
        "{}",
        row(
            &lines_colored[0],
            crate::terminal_region::visible_char_width(&lines_plain[0]),
        )
    );
    eprintln!(
        "{}",
        row(
            &lines_colored[1],
            crate::terminal_region::visible_char_width(&lines_plain[1]),
        )
    );
    eprintln!("{}", format!("├{hr}┤").cyan().dim());
    eprintln!(
        "{}",
        row(
            &lines_colored[2],
            crate::terminal_region::visible_char_width(&lines_plain[2]),
        )
    );
    eprintln!(
        "{}",
        row(
            &lines_colored[3],
            crate::terminal_region::visible_char_width(&lines_plain[3]),
        )
    );
    eprintln!("{}", format!("╰{hr}╯").cyan());

    // Show active limits (system prompt, max-budget, permission mode)
    let mut limits = Vec::new();
    if let Ok(max_turns) = std::env::var("MO_MAX_TURNS") {
        limits.push(format!("max-turns: {}", max_turns));
    }
    if state.max_budget_limit > 0.0 {
        limits.push(format!("max-budget: ${:.2}", state.max_budget_limit));
    }
    if state.perm_manager.mode() == crate::permission_manager::PermissionMode::Auto {
        limits.push("permission: auto".to_string());
    }
    if !limits.is_empty() {
        eprintln!(
            "{}",
            format!("  ⚙ Active limits: {}", limits.join(" │ "))
                .yellow()
                .dim()
        );
    }

    eprintln!(
        "{}",
        "  Ctrl-C to cancel │ Ctrl+K command picker │ /help for commands │ /quit to exit".dim()
    );
    eprintln!();
}

fn banner_session_display(state: &ReplState) -> String {
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

fn startup_logo_lines() -> &'static [&'static str] {
    &[
        " █████╗ ███████╗████████╗██████╗  █████╗",
        "██╔══██╗██╔════╝╚══██╔══╝██╔══██╗██╔══██╗",
        "███████║███████╗   ██║   ██████╔╝███████║",
        "██╔══██║╚════██║   ██║   ██╔══██╗██╔══██║",
        "██║  ██║███████║   ██║   ██║  ██║██║  ██║",
        "╚═╝  ╚═╝╚══════╝   ╚═╝   ╚═╝  ╚═╝╚═╝  ╚═╝",
    ]
}

#[cfg(test)]
fn startup_logo_frames() -> Vec<String> {
    let lines = startup_logo_lines();
    (0..lines.len())
        .map(|end| lines[..=end].join("\n"))
        .collect()
}

fn print_startup_logo() {
    use std::io::Write;
    use std::time::Duration;

    let logo_lines = startup_logo_lines();
    // Skip animation if ASTRA_STARTUP_TRACE or ASTRA_FAST_STARTUP is set
    let animated = crossterm::terminal::size().is_ok()
        && std::env::var("NO_COLOR").is_err()
        && std::env::var("CI").is_err()
        && std::env::var("ASTRA_STARTUP_TRACE").is_err()
        && std::env::var("ASTRA_FAST_STARTUP").is_err();

    if animated {
        let delay = Duration::from_millis(28);
        for line in logo_lines {
            eprintln!("  {}", line.cyan().bold());
            let _ = std::io::stderr().flush();
            std::thread::sleep(delay);
        }
        std::thread::sleep(Duration::from_millis(70));
    } else {
        for line in logo_lines {
            eprintln!("  {}", line.cyan().bold());
        }
    }
    eprintln!();
}

pub(super) fn current_access_token(profile: Option<&str>) -> Option<String> {
    let creds = load_credentials();
    let name = profile_name(profile, &creds);
    creds
        .profiles
        .get(&name)
        .and_then(|p| p.access_token.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli_utils::{CredentialsFile, Profile};
    use crate::tests::isolate_credentials;
    use tempfile::tempdir;

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

    fn isolated_sessions_dir() -> (tempfile::TempDir, session_journal::JournalDirGuard) {
        let tmp = tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let guard = session_journal::JournalDirGuard::new(&sessions);
        (tmp, guard)
    }

    #[test]
    fn restore_history_empty_for_unknown_session() {
        let (_tmp, _g) = isolated_sessions_dir();
        let history = restore_history_from_journal("nonexistent-session-xyz-123");
        assert!(history.is_empty());
    }

    #[test]
    fn restore_history_from_journal_roundtrip() {
        let (_tmp, _g) = isolated_sessions_dir();
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

        let history = restore_history_from_journal(&sid);
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].0, "what is Rust?");
        assert_eq!(history[0].1, "Rust is a systems language.");
        assert_eq!(history[1].0, "show me an example");
    }

    #[test]
    fn restore_history_skips_non_turn_events() {
        let (_tmp, _g) = isolated_sessions_dir();
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

        let history = restore_history_from_journal(&sid);
        assert_eq!(history.len(), 1, "only Turn events should be included");
        assert_eq!(history[0].0, "hello");
    }

    #[test]
    fn restore_session_state_recovers_turn_tools_and_tokens() {
        let (_tmp, _g) = isolated_sessions_dir();
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
                .with_tool_selection(
                    vec!["github_ci_status".into()],
                    vec![],
                    vec!["github_ci_status".into()],
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
                .with_tool_selection(
                    vec!["github_list_prs".into()],
                    vec![],
                    vec!["github_list_prs".into()],
                    35,
                ),
            )
            .unwrap();

        let restored = restore_session_state_from_journal(&sid);
        assert_eq!(
            restored.turn, 2,
            "turn should reflect restored conversation length"
        );
        assert_eq!(restored.total_prompt_tokens, 200);
        assert_eq!(restored.total_completion_tokens, 50);
        assert_eq!(restored.recent_tools, vec!["github_list_prs".to_string()]);
        assert_eq!(restored.history.len(), 2);
    }

    #[test]
    fn restore_session_state_uses_latest_session_segment() {
        let (_tmp, _g) = isolated_sessions_dir();
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
                .with_tool_selection(
                    vec!["git_log".into()],
                    vec![],
                    vec!["git_log".into()],
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
                .with_tool_selection(
                    vec!["github_ci_status".into()],
                    vec![],
                    vec!["github_ci_status".into()],
                    20,
                ),
            )
            .unwrap();

        let restored = restore_session_state_from_journal(&sid);
        assert_eq!(
            restored.history,
            vec![("latest question".into(), "latest answer".into())]
        );
        assert_eq!(restored.turn, 1);
        assert_eq!(restored.total_prompt_tokens, 80);
        assert_eq!(restored.total_completion_tokens, 20);
        assert_eq!(restored.recent_tools, vec!["github_ci_status".to_string()]);
    }

    #[test]
    fn restore_session_state_keeps_recorded_turn_after_stray_session_start() {
        let (_tmp, _g) = isolated_sessions_dir();
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
                .with_tool_selection(
                    vec!["github_ci_status".into()],
                    vec![],
                    vec!["github_ci_status".into()],
                    20,
                ),
            )
            .unwrap();

        let restored = restore_session_state_from_journal(&sid);
        assert_eq!(restored.history, vec![("latest".into(), "three".into())]);
        assert_eq!(restored.turn, 3);
        assert_eq!(restored.total_prompt_tokens, 30);
        assert_eq!(restored.total_completion_tokens, 8);
        assert_eq!(restored.recent_tools, vec!["github_ci_status".to_string()]);
    }

    #[test]
    fn initialize_repl_state_skips_cleanly_ended_session() {
        let (_tmp, _g) = isolated_sessions_dir();
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
                last_session_id: Some(sid),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        let state = initialize_repl_state(None, Some("gpt-5"));
        assert_eq!(state.session_id, None);
        assert_eq!(state.pending_recovery, None);
        assert!(state.history.is_empty());
        assert_eq!(state.turn, 0);
    }

    #[test]
    fn initialize_repl_state_records_project_scoped_pending_recovery() {
        let (_tmp, _g) = isolated_sessions_dir();
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

        let state = initialize_repl_state(None, Some("gpt-5"));
        assert_eq!(state.session_id, None);
        assert_eq!(state.pending_recovery.as_deref(), Some(sid.as_str()));
        assert!(state.history.is_empty());
        assert_eq!(state.turn, 0);
    }

    #[test]
    fn initialize_repl_state_ignores_pending_recovery_from_other_project() {
        let (_tmp, _g) = isolated_sessions_dir();
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
                last_session_id: Some(sid),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        let state = initialize_repl_state(None, Some("gpt-5"));
        assert_eq!(state.session_id, None);
        assert_eq!(state.pending_recovery, None);
        assert!(state.history.is_empty());
        assert_eq!(state.turn, 0);
    }

    #[test]
    fn initialize_repl_state_detects_pending_plan_resume_digest() {
        let (_tmp, _g) = isolated_sessions_dir();
        let _creds_guard = isolate_credentials();
        let home = tempdir().unwrap();
        let _home_guard = EnvGuard::set("HOME", home.path().to_str().unwrap());
        std::fs::create_dir_all(home.path().join(".astra")).unwrap();

        let mut plan_state = astra_runtime::plan_decompose::PlanModeState::new(
            "Fix auth middleware".to_string(),
            Default::default(),
        );
        plan_state
            .plan
            .subtasks
            .push(astra_services::task_orchestrator::SubtaskPlan {
                id: "t1".into(),
                title: "Patch token validation".into(),
                status: astra_services::task_orchestrator::TaskStatus::InProgress,
                ..Default::default()
            });
        plan_state
            .save_to_file(&astra_runtime::plan_decompose::PlanModeState::state_path())
            .unwrap();

        let state = initialize_repl_state(None, Some("gpt-5"));
        let digest = state.pending_plan_resume_digest.expect("resume digest");
        assert!(digest.contains("Fix auth middleware"), "{digest}");
        assert!(digest.contains("Patch token validation"), "{digest}");
    }

    #[test]
    fn maybe_restore_pending_plan_mode_activates_saved_plan() {
        let (_tmp, _g) = isolated_sessions_dir();
        let _creds_guard = isolate_credentials();
        let home = tempdir().unwrap();
        let _home_guard = EnvGuard::set("HOME", home.path().to_str().unwrap());
        std::fs::create_dir_all(home.path().join(".astra")).unwrap();

        let mut plan_state = astra_runtime::plan_decompose::PlanModeState::new(
            "Ship plan resume".to_string(),
            Default::default(),
        );
        plan_state
            .plan
            .subtasks
            .push(astra_services::task_orchestrator::SubtaskPlan {
                id: "t1".into(),
                title: "Wire implicit resume".into(),
                status: astra_services::task_orchestrator::TaskStatus::Pending,
                ..Default::default()
            });
        plan_state
            .save_to_file(&astra_runtime::plan_decompose::PlanModeState::state_path())
            .unwrap();

        let mut state = ReplState {
            pending_plan_resume_digest: Some("[plan-resume] goal=\"Ship plan resume\"".into()),
            ..ReplState::default()
        };
        assert!(maybe_restore_pending_plan_mode(
            "please @resume-plan",
            &mut state
        ));
        assert!(state.pending_plan_resume_digest.is_none());
        assert_eq!(
            state.plan_mode.as_ref().map(|ps| ps.goal.as_str()),
            Some("Ship plan resume")
        );
    }

    #[test]
    fn plan_context_is_stale_respects_threshold() {
        let now = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(10_000);
        let fresh = now - std::time::Duration::from_secs(60);
        let stale = now - std::time::Duration::from_secs(31 * 60);
        assert!(!plan_context_is_stale(
            fresh,
            now,
            std::time::Duration::from_secs(30 * 60)
        ));
        assert!(plan_context_is_stale(
            stale,
            now,
            std::time::Duration::from_secs(30 * 60)
        ));
    }

    // ── Session display logic ──────────────────────────────────────────────

    #[test]
    fn session_display_shows_new_for_none() {
        let state = ReplState::default();
        assert_eq!(banner_session_display(&state), "new");
    }

    #[test]
    fn session_display_shows_truncated_id_for_fresh_session() {
        let state = ReplState {
            session_id: Some("abcdef12-3456-7890".to_string()),
            ..Default::default()
        };
        assert_eq!(banner_session_display(&state), "abcdef12");
    }

    #[test]
    fn session_display_shows_resumed_for_restored_session() {
        let state = ReplState {
            session_id: Some("abcdef12-3456-7890".to_string()),
            turn: 3,
            ..Default::default()
        };
        assert_eq!(banner_session_display(&state), "abcdef12 (resumed)");
    }

    #[test]
    fn model_display_shows_auto_when_none() {
        let state = ReplState::default();
        let display = state.model.as_deref().unwrap_or("auto");
        assert_eq!(display, "auto");
    }

    #[test]
    fn model_display_shows_actual_name_when_set() {
        let state = ReplState {
            model: Some("gpt-5".to_string()),
            ..Default::default()
        };
        let display = state.model.as_deref().unwrap_or("auto");
        assert_eq!(display, "gpt-5");
    }

    #[test]
    fn startup_logo_has_multiple_lines_and_brand_shape() {
        let lines = startup_logo_lines();
        assert!(lines.len() >= 5);
        assert!(lines.iter().all(|line| !line.trim().is_empty()));
        assert!(lines[0].contains("███"));
        // Last line should have the bottom corners of the logo
        assert!(lines.iter().any(|line| line.contains("╚═╝")));
    }

    #[test]
    fn startup_logo_frames_progressively_reveal_logo() {
        let lines = startup_logo_lines();
        let frames = startup_logo_frames();
        assert_eq!(frames.len(), lines.len());
        assert_eq!(frames[0], lines[0]);
        assert_eq!(frames.last().unwrap(), &lines.join("\n"));
        for (idx, frame) in frames.iter().enumerate() {
            assert_eq!(frame.lines().count(), idx + 1);
        }
    }
}
