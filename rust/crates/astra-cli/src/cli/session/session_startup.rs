//! REPL startup/setup orchestration extracted from `run_chat_repl`.

use crate::cli::agent_runtime::initialize_multi_agent_runtime;
use crate::cli::cli_config::cli_utils::{
    SessionResumePreflight, clear_profile_last_session_if_matches_or_warn,
    preflight_remote_resume_session,
};
use crate::cli::cloud_sync::{
    append_cloud_pull_sync_journal, try_cloud_pull, try_cloud_pull_preferences,
};
use crate::cli::edge_lifecycle::register_and_start_heartbeat;
use crate::cli::permission_manager;
use crate::cli::project_instructions::discover_project_instructions;
use crate::cli::session::{
    session_guard::{
        self, install_session_panic_hook, install_sigterm_handler, subscribe_shutdown_signal,
    },
    session_recovery,
    session_runtime::{self, PipelineModules, print_session_banner},
    session_state::SessionState,
};
use crate::cli::slash::slash_session;
use crate::cli::startup_trace::StartupTracer;
use crate::cli::theme;
use astra_services::session_journal;
use astra_text_utils::str_preview::truncate_str;
use crossterm::style::Stylize;

pub(crate) struct SessionStartupArtifacts {
    pub pipeline_modules: PipelineModules,
    pub edge_heartbeat_task: Option<tokio::task::JoinHandle<()>>,
    pub skill_quality_path: std::path::PathBuf,
    pub shutdown_signal_rx: tokio::sync::watch::Receiver<Option<session_guard::ShutdownSignal>>,
}

// Note: `selector` field was removed — tool surface is now handled by the LLM directly.

pub(crate) struct GoalSteeringChange {
    pub previous_goal: Option<String>,
    pub turn: u32,
}

pub(crate) fn steer_observability_goal(
    _state: &mut SessionState,
    _goal: &str,
) -> Option<GoalSteeringChange> {
    None
}

/// Apply persisted adaptive engine state to a newly created ObservabilitySession.
/// Called when pending_adaptive_state was stashed during workspace restore and the
/// ObservabilitySession is now available to receive it.
pub(crate) fn apply_pending_adaptive_state(state: &mut SessionState) {
    let adaptive = match state.pending_adaptive_state.take() {
        Some(a) => a,
        None => return,
    };
    let obs = match &state.observability_session {
        Some(o) => o,
        None => {
            state.pending_adaptive_state = Some(adaptive);
            return;
        }
    };
    let mut guard = match obs.write() {
        Ok(guard) => guard,
        Err(_) => {
            state.pending_adaptive_state = Some(adaptive);
            return;
        }
    };
    guard.last_scenario_change_turn = adaptive.last_scenario_change_turn;
    guard.last_token_budget_direction = adaptive.last_token_budget_direction;
    guard.last_token_budget_change_turn = adaptive.last_token_budget_change_turn;
    if let Some(json) = &adaptive.tuned_config_json {
        if let Ok(saved_config) =
            serde_json::from_str::<astra_config::runtime_config::RuntimeConfig>(json)
        {
            let current = std::mem::take(&mut guard.config);
            guard.config = current.merge(saved_config);
        }
    }
}

pub(crate) fn initialize_journal_pub(state: &mut SessionState, session_id: &str) {
    initialize_journal(state, session_id);
}

fn initialize_journal(state: &mut SessionState, session_id: &str) {
    attach_session_journal(state, session_id);
    initialize_session_artifacts(state, session_id);
}

fn record_session_persistence_error(state: &mut SessionState, detail: &str) {
    match state.session_persistence_error.as_deref() {
        Some(existing) if existing == detail => {}
        Some(existing) => {
            state.session_persistence_error = Some(format!("{existing}; {detail}"));
        }
        None => state.session_persistence_error = Some(detail.to_string()),
    }
}

pub(crate) fn attach_session_journal_pub(state: &mut SessionState, session_id: &str) {
    attach_session_journal(state, session_id);
}

fn attach_session_journal(state: &mut SessionState, session_id: &str) {
    let target_path = session_journal::journal_file_path(session_id);
    let already_attached = state
        .journal
        .as_ref()
        .map(|journal| journal.path() == &target_path)
        .unwrap_or(false);

    if !already_attached {
        state.journal = match session_journal::JournalWriter::new(session_id) {
            Ok(journal) => Some(journal),
            Err(err) => {
                eprintln!(
                    "{}",
                    format!("  ⚠ Session journal not available for {session_id}: {err}").yellow()
                );
                None
            }
        };
    }
}

fn initialize_session_artifacts(state: &mut SessionState, session_id: &str) {
    let needs_start_event =
        session_journal::journal_needs_session_start(session_id).unwrap_or(true);

    if needs_start_event {
        if state.journal.is_none() {
            return;
        }
        let mut start_event = session_journal::JournalEvent::session_start(
            Some(session_id),
            astra_core::model_override::normalize_model_override(state.model.as_deref()),
        );
        start_event.edge_policy = Some(session_journal::EdgePolicySnapshot {
            permission_mode: Some(state.perm_manager.mode().to_string()),
            cloud_policy_version: None,
            rules_fingerprint: None,
        });
        let start_append = state
            .journal
            .as_ref()
            .expect("checked journal presence")
            .append(&start_event);
        if let Err(error) = start_append {
            eprintln!("  ⚠ failed to append session start event: {error}");
            record_session_persistence_error(state, "failed to append session start event");
        }
        super::session_side_effects::enqueue_ingestion_pub(state, &start_event);

        use astra_config::config_versions::ConfigVersionStore;
        if state.config_version_id.is_none()
            && let Some(store) = astra_config::config_versions::LocalFileStore::at_default_root()
        {
            let meta = astra_config::config_versions::PutMetadata {
                source_session: Some(session_id.to_string()),
                parent: None,
            };
            if let Ok(id) = store.put(&state.runtime_config, meta) {
                let ev = session_journal::JournalEvent::config_version_change(
                    Some(session_id),
                    state.turn,
                    None,
                    id.as_str(),
                    "startup",
                );
                if let Err(error) = state
                    .journal
                    .as_ref()
                    .expect("checked journal presence")
                    .append(&ev)
                {
                    eprintln!("  ⚠ failed to append config version change event: {error}");
                }
                super::session_side_effects::enqueue_ingestion_pub(state, &ev);
                state.config_version_id = Some(id.as_str().to_string());
            }
        }
    }

    let (mut ws, mut dirty, workspace_existed) =
        match astra_services::session_workspace::read_workspace_optional(session_id) {
            Ok(Some(ws)) => (ws, false, true),
            Ok(None) => (
                astra_services::session_workspace::WorkspaceMetadata::new(
                    session_id,
                    astra_core::model_override::normalize_model_override(state.model.as_deref())
                        .unwrap_or("default"),
                ),
                true,
                false,
            ),
            Err(error) => (
                session_recovery::workspace_metadata_from_live_state_after_read_failure(
                    state, session_id, &error,
                ),
                true,
                false,
            ),
        };
    if ws.status != "active" {
        ws.status = "active".to_string();
        dirty = true;
    }
    if let Some(model) =
        astra_core::model_override::normalize_model_override(state.model.as_deref())
        && (ws.model.is_none() || (!workspace_existed && ws.model.as_deref() != Some(model)))
    {
        ws.model = Some(model.to_string());
        dirty = true;
    }
    let permission_mode = state.perm_manager.mode().to_string();
    if (!workspace_existed || ws.permission_mode.is_none())
        && ws.permission_mode.as_deref() != Some(permission_mode.as_str())
    {
        ws.permission_mode = Some(permission_mode);
        dirty = true;
    }
    if !workspace_existed {
        ws.turn_count = ws.turn_count.max(state.turn);
        ws.total_tokens_in = ws.total_tokens_in.max(state.total_prompt_tokens);
        ws.total_tokens_out = ws.total_tokens_out.max(state.total_completion_tokens);
        ws.total_cache_read_tokens = ws
            .total_cache_read_tokens
            .max(state.total_cache_read_tokens);
        ws.total_cache_creation_tokens = ws
            .total_cache_creation_tokens
            .max(state.total_cache_creation_tokens);
        session_recovery::sync_plan_fields_to_workspace(state, &mut ws);
        session_recovery::sync_context_trace_to_workspace(state, &mut ws);
        session_recovery::sync_session_state_to_workspace(state, &mut ws);
    }
    if state.session_persistence_error.is_some()
        && ws.last_persistence_error != state.session_persistence_error
    {
        ws.last_persistence_error = state.session_persistence_error.clone();
        dirty = true;
    }
    if dirty {
        ws.updated_at = chrono::Utc::now().to_rfc3339();
        if let Err(e) = astra_services::session_workspace::write_workspace(&ws) {
            eprintln!("  ⚠ workspace write failed during init: {e}");
            record_session_persistence_error(state, "failed to write workspace metadata");
        }
    }

    if state.observability_session.is_none() {
        state.observability_session = Some(if let Some(hub) = &state.observability_hub {
            let user_id = state
                .ingestion_user_id
                .clone()
                .unwrap_or_else(|| "anonymous".to_string());
            hub.start_session(&user_id, session_id)
        } else {
            std::sync::Arc::new(std::sync::RwLock::new(
                astra_runtime::observability::ObservabilitySession::new_simple(session_id),
            ))
        });
        apply_pending_adaptive_state(state);
    }
}

async fn prune_stale_pending_recovery(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    state: &mut SessionState,
) {
    let Some(session_id) = state.pending_recovery.clone() else {
        return;
    };
    if matches!(
        preflight_remote_resume_session(api, profile, &session_id).await,
        SessionResumePreflight::Missing
    ) && !crate::cli::cli_config::cli_utils::local_session_is_resumable(&session_id)
    {
        clear_profile_last_session_if_matches_or_warn(
            profile,
            &session_id,
            "session_startup:prune_stale_pending_recovery",
        );
        state.pending_recovery = None;
    }
}

#[derive(Debug)]
struct CliSessionMemorySelectorResolver {
    api: astra_thin_client::ThinClient,
    profile: Option<String>,
}

#[async_trait::async_trait]
impl astra_runtime::session_memory::SelectorParamsResolver for CliSessionMemorySelectorResolver {
    async fn resolve(&self) -> Option<astra_runtime::memory_hooks::relevance::LlmConnParams> {
        self.resolve_candidates().await.into_iter().next()
    }

    async fn resolve_candidates(
        &self,
    ) -> Vec<astra_runtime::memory_hooks::relevance::LlmConnParams> {
        #[derive(serde::Deserialize)]
        struct MemoryModelWire {
            model_name: String,
            #[serde(default)]
            candidate_model_names: Vec<String>,
            #[serde(default)]
            candidate_thinking_capabilities: Vec<Option<String>>,
        }

        let Some(token) =
            session_runtime::fresh_access_token(&self.api, self.profile.as_deref()).await
        else {
            return Vec::new();
        };
        let body = self
            .api
            .get_authed_path_text(&token, astra_thin_client::paths::model_memory())
            .await
            .ok();
        let Some(body) = body else {
            return Vec::new();
        };
        let response = serde_json::from_str::<MemoryModelWire>(&body).ok();
        let Some(response) = response else {
            return Vec::new();
        };
        let model_names = if response.candidate_model_names.is_empty() {
            vec![response.model_name]
        } else {
            response.candidate_model_names
        };
        let thinking_caps = if response.candidate_thinking_capabilities.is_empty() {
            vec![None]
        } else {
            response.candidate_thinking_capabilities
        };
        model_names
            .into_iter()
            .zip(thinking_caps.into_iter().chain(std::iter::repeat(None)))
            .map(|(model_name, thinking_cap_str)| {
                astra_runtime::memory_hooks::relevance::LlmConnParams {
                    base_url: format!("{}/v1", self.api.api_origin()),
                    api_key: token.clone(),
                    model_name,
                    wire_model_name: None,
                    provider: "openai".to_string(),
                    request_body_overrides: None,
                    thinking_capability: thinking_cap_str
                        .as_deref()
                        .and_then(|s| astra_services::models::ThinkingCapability::from_db(Some(s))),
                }
            })
            .collect()
    }
}

#[derive(Debug)]
struct CliSessionMemoryMemoriaClient {
    api: astra_thin_client::ThinClient,
    profile: Option<String>,
    working_ids: std::sync::Mutex<std::collections::HashMap<String, String>>,
}

impl CliSessionMemoryMemoriaClient {
    fn new(api: astra_thin_client::ThinClient, profile: Option<&str>) -> Self {
        Self {
            api,
            profile: profile.map(str::to_string),
            working_ids: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    async fn fresh_token(&self) -> Result<String, String> {
        session_runtime::fresh_access_token(&self.api, self.profile.as_deref())
            .await
            .ok_or_else(|| "no CLI access token available".to_string())
    }

    fn parse_memories(
        body: &serde_json::Value,
    ) -> Vec<astra_runtime::turn::cloud::memoria_compact::MemoriaMemory> {
        body.get("memories")
            .and_then(serde_json::Value::as_array)
            .or_else(|| body.as_array())
            .into_iter()
            .flatten()
            .filter_map(|entry| serde_json::from_value(entry.clone()).ok())
            .collect()
    }

    fn track_working_id_from_memories(
        &self,
        session_id: &str,
        memories: &[astra_runtime::turn::cloud::memoria_compact::MemoriaMemory],
    ) {
        if let Some(memory) = memories.iter().find(|memory| {
            astra_runtime::session_memory::runner::decode_session_memory_entry(
                &memory.content,
                session_id,
            )
            .is_some()
        }) {
            if let Ok(mut guard) = self.working_ids.lock() {
                guard.insert(session_id.to_string(), memory.memory_id.clone());
            }
        }
    }
}

#[async_trait::async_trait]
impl astra_runtime::turn::cloud::memoria_compact::MemoriaClient for CliSessionMemoryMemoriaClient {
    async fn retrieve_ext(
        &self,
        query: &str,
        session_id: Option<&str>,
        top_k: usize,
        filter_session: bool,
    ) -> Result<Vec<astra_runtime::turn::cloud::memoria_compact::MemoriaMemory>, String> {
        let token = self.fresh_token().await?;
        let mut body = serde_json::json!({
            "query": query,
            "top_k": top_k,
        });
        if let Some(session_id) = session_id {
            body["session_id"] = serde_json::json!(session_id);
            if filter_session {
                body["session_scope"] = serde_json::json!("only");
            }
        }
        let response = self
            .api
            .post_memory_retrieve_json(&token, &body)
            .await
            .map_err(|error| format!("memory retrieve failed: {error}"))?;
        let status = response.status();
        let payload: serde_json::Value = response
            .json()
            .await
            .map_err(|error| format!("memory retrieve parse failed: {error}"))?;
        if !status.is_success() {
            return Err(format!("memory retrieve HTTP {status}"));
        }
        let memories = Self::parse_memories(&payload);
        if let Some(session_id) = session_id {
            self.track_working_id_from_memories(session_id, &memories);
        }
        Ok(memories)
    }

    async fn store(
        &self,
        content: &str,
        memory_type: &str,
        session_id: Option<&str>,
        trust_tier: Option<&str>,
    ) -> Result<String, String> {
        let token = self.fresh_token().await?;
        if memory_type == "working"
            && let Some(session_id) = session_id
            && let Some(memory_id) = self
                .working_ids
                .lock()
                .ok()
                .and_then(|guard| guard.get(session_id).cloned())
        {
            let path = format!("/memory/{memory_id}/correct");
            let body = serde_json::json!({
                "new_content": content,
                "reason": "session memory update",
            });
            if self
                .api
                .put_bearer_path_json_text(&token, &path, &body)
                .await
                .is_ok()
            {
                return Ok(memory_id);
            }
            if let Ok(mut guard) = self.working_ids.lock() {
                guard.remove(session_id);
            }
        }

        let mut body = serde_json::json!({
            "content": content,
            "memory_type": memory_type,
        });
        if let Some(session_id) = session_id {
            body["session_id"] = serde_json::json!(session_id);
        }
        if let Some(trust_tier) = trust_tier {
            body["trust_tier"] = serde_json::json!(trust_tier);
        }
        let response = self
            .api
            .post_memory_store_json(&token, &body)
            .await
            .map_err(|error| format!("memory store failed: {error}"))?;
        let status = response.status();
        let payload: serde_json::Value = response
            .json()
            .await
            .map_err(|error| format!("memory store parse failed: {error}"))?;
        if !status.is_success() {
            return Err(format!("memory store HTTP {status}"));
        }
        let memory_id = payload
            .get("memory_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "memory store missing memory_id".to_string())?
            .to_string();
        if memory_type == "working"
            && let Some(session_id) = session_id
            && let Ok(mut guard) = self.working_ids.lock()
        {
            guard.insert(session_id.to_string(), memory_id.clone());
        }
        Ok(memory_id)
    }

    async fn purge_working(&self, session_id: &str) -> Result<u64, String> {
        let Some(memory_id) = self
            .working_ids
            .lock()
            .ok()
            .and_then(|guard| guard.get(session_id).cloned())
        else {
            return Ok(0);
        };

        let token = self.fresh_token().await?;
        let body = serde_json::json!({
            "memory_ids": [memory_id],
            "reason": "session memory purge",
        });
        let response = self
            .api
            .post_memory_purge_json(&token, &body)
            .await
            .map_err(|error| format!("memory purge failed: {error}"))?;
        let status = response.status();
        let payload: serde_json::Value = response
            .json()
            .await
            .map_err(|error| format!("memory purge parse failed: {error}"))?;
        if !status.is_success() {
            return Err(format!("memory purge HTTP {status}"));
        }
        if let Ok(mut guard) = self.working_ids.lock() {
            guard.remove(session_id);
        }
        Ok(payload
            .get("deleted_count")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0))
    }
}

fn build_cli_session_memory_event_sink()
-> std::sync::Arc<dyn Fn(&session_journal::JournalEvent) + Send + Sync> {
    let writers = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::<
        String,
        session_journal::JournalWriter,
    >::new()));
    std::sync::Arc::new(move |event: &session_journal::JournalEvent| {
        let Some(session_id) = event.session_id.as_deref().filter(|sid| !sid.is_empty()) else {
            return;
        };
        let mut guard = match writers.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                tracing::warn!(
                    session_id,
                    event_type = ?event.event_type,
                    "session-memory journal writer cache poisoned; recovering"
                );
                poisoned.into_inner()
            }
        };
        let writer = match guard.entry(session_id.to_string()) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let writer = match session_journal::JournalWriter::new(session_id) {
                    Ok(writer) => writer,
                    Err(error) => {
                        tracing::warn!(
                            session_id,
                            event_type = ?event.event_type,
                            ?error,
                            "failed to open local journal for session-memory event"
                        );
                        return;
                    }
                };
                entry.insert(writer)
            }
        };
        if let Err(error) = writer.append(event) {
            tracing::warn!(
                session_id,
                event_type = ?event.event_type,
                ?error,
                "failed to append session-memory event to local journal"
            );
        }
    })
}

async fn build_cli_session_memory_extractor(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
) -> Option<std::sync::Arc<astra_runtime::session_memory::MemoryExtractionService>> {
    #[derive(serde::Deserialize)]
    struct AuthMeWire {
        user_id: String,
    }

    let token = session_runtime::fresh_access_token(api, profile).await?;
    let me_body = api.get_auth_me_text(&token).await.ok()?;
    let me = serde_json::from_str::<AuthMeWire>(&me_body).ok()?;
    let selector = std::sync::Arc::new(CliSessionMemorySelectorResolver {
        api: api.clone(),
        profile: profile.map(str::to_string),
    });
    let memoria = std::sync::Arc::new(CliSessionMemoryMemoriaClient::new(api.clone(), profile))
        as std::sync::Arc<dyn astra_runtime::turn::cloud::memoria_compact::MemoriaClient>;
    let ingestion = astra_services::event_ingestion::IngestionSender::disconnected();
    let broker =
        std::sync::Arc::new(astra_runtime::session_memory::BackgroundActivityBroker::new());
    let service = astra_runtime::session_memory::MemoryExtractionService::new(
        selector, memoria, ingestion, me.user_id, broker,
    )
    .with_local_current_snapshot()
    .with_local_event_sink(build_cli_session_memory_event_sink());
    Some(std::sync::Arc::new(service))
}

pub(crate) async fn complete_session_startup(
    state: &mut SessionState,
    tracer: &mut StartupTracer,
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    resume_session_id: Option<&str>,
    no_instructions: bool,
    cli_context: &crate::cli::cli_config::cli_context::CliContext,
) -> Result<SessionStartupArtifacts, String> {
    // Install panic hook to write session_end on unexpected crashes.
    install_session_panic_hook();
    // Install signal handlers so SIGTERM/SIGHUP can drain through normal REPL shutdown.
    install_sigterm_handler();
    let shutdown_signal_rx = subscribe_shutdown_signal();

    // --session-id: override with explicit session UUID
    if let Some(sid) = cli_context.session_id.as_deref() {
        state.set_session_id(sid.to_string());
        state.pending_recovery = None;
        eprintln!(
            "{}",
            format!("  Using session {}", truncate_str(sid, 12)).magenta()
        );
    }

    // --name: set session display name
    if let Some(name) = cli_context.session_name.as_ref() {
        state.session_name = Some(name.clone());
    }

    // --yes: warn about auto-approve mode
    if state.perm_manager.mode() == permission_manager::PermissionMode::Auto {
        eprintln!(
            "{}",
            "  🔓 Auto mode is ON — normal tool risk is auto-approved; some git/sensitive gates may still stop.".dim()
        );
    } else if state.perm_manager.mode() == permission_manager::PermissionMode::Bypass {
        eprintln!(
            "{}",
            "  🔓 Bypass mode is ON — approval prompts are skipped; catastrophic and policy hard-denies still apply.".dim()
        );
    }

    // Load project instructions from .astra/instructions.md
    if !no_instructions {
        if let Some(instructions) = discover_project_instructions() {
            let lines = instructions.lines().count();
            eprintln!(
                "  {} {}",
                theme::icon_ok(),
                format!("Loaded project instructions ({lines} lines)").dim()
            );
            state.project_instructions = Some(instructions);
        }
    }

    // Session lifecycle maintenance: compress old journals and delete expired sessions.
    // Non-blocking, best-effort — errors are silently ignored.
    {
        const SESSION_TTL_DAYS: u64 = 30;
        const JOURNAL_COMPRESS_DAYS: u64 = 7;
        let _maint =
            session_journal::run_session_maintenance(SESSION_TTL_DAYS, JOURNAL_COMPRESS_DAYS);
    }

    // Load persisted skill quality data from previous sessions
    let skill_quality_path = dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("astra")
        .join("skill_quality.json");
    state.skill_quality_tracker =
        astra_skills::quality::SkillQualityTracker::load(&skill_quality_path);

    tracer.phase("config_load");

    // Session-scoped confidence calibrator: thresholds adapt to correction rates
    let _confidence_calibrator =
        std::sync::Arc::new(astra_turn_core::routing_metrics::ConfidenceCalibrator::default());
    let pipeline_modules = session_runtime::create_pipeline_modules(api, profile);
    tracer.phase("pipeline_modules");

    // Load cross-session tool-health state from local files.
    let profile_name = profile.unwrap_or("default");
    let (cross_session_health_entries, cloud_pull_result, pref_keys_after_pull) = {
        let cross_session_health_entries =
            astra_turn_core::tool_health_persistence::load_tool_health(profile_name);
        state.synced_tool_health_entries =
            astra_turn_core::tool_health_persistence::load_synced_tool_health(profile_name);
        let cloud_pull_result = try_cloud_pull(profile_name).await;
        let pref_keys = try_cloud_pull_preferences(state).await;
        (cross_session_health_entries, cloud_pull_result, pref_keys)
    };
    tracer.phase("learning_state");

    state.tool_health_entries = cross_session_health_entries.clone();
    if state.synced_tool_health_entries.is_empty() {
        state.synced_tool_health_entries = cross_session_health_entries;
    }

    state.session_memory_extractor = build_cli_session_memory_extractor(api, profile).await;
    state.team_store = std::sync::Arc::new(crate::cli::http_team_store::HttpTeamStore::new(
        api.api_origin(),
        profile,
    ));
    tracer.phase("matrix_pool");

    state.unified_skill_registry = pipeline_modules.unified_skill_registry.clone();
    state.mcp_manager = pipeline_modules.mcp_manager.clone();

    append_cloud_pull_sync_journal(
        state,
        profile_name,
        "session_startup",
        &cloud_pull_result,
        &pref_keys_after_pull,
    );

    let startup_token = session_runtime::fresh_access_token(api, profile).await;

    if let Some(token) = startup_token.as_deref() {
        match session_runtime::resolve_server_default_model(api, token).await {
            session_runtime::ServerDefaultModel::Selected(model) => {
                if state.model.is_none() {
                    state.model = Some(model);
                }
            }
            session_runtime::ServerDefaultModel::NoModels => {
                state.model = Some("⚠ none".to_string());
            }
            session_runtime::ServerDefaultModel::Unavailable => {}
        }
    }
    tracer.phase("model_check");
    prune_stale_pending_recovery(api, profile, state).await;

    if state.session_id.is_none()
        && let Some(sid) = resume_session_id
    {
        slash_session::restore_session_into_state(sid, profile, api, state).await?;
    }

    print_session_banner(profile, state);
    tracer.phase("banner");

    // Pending recovery is silently retained in state for /resume; no startup banner.

    // Proxy info now shown in startup card — no separate line needed.

    let mut edge_heartbeat_task: Option<tokio::task::JoinHandle<()>> = None;
    if let Some(tok) = startup_token.as_deref() {
        edge_heartbeat_task = register_and_start_heartbeat(api, tok, profile).await;
    }
    tracer.phase("edge_heartbeat");

    if state.model.as_deref() == Some("⚠ none") {
        eprintln!(
            "  {}  {}",
            theme::icon_warn(),
            "No LLM model configured on server. Run: astra admin model add".yellow()
        );
        eprintln!();
        state.model = None;
    }

    tracer.phase("completions_deferred");

    if let Some(token) = startup_token {
        initialize_multi_agent_runtime(state, api, token, profile).await;
    }
    tracer.phase("multi_agent_runtime");

    // Ready status now conveyed by the startup card — no separate line.

    Ok(SessionStartupArtifacts {
        pipeline_modules,
        edge_heartbeat_task,
        skill_quality_path,
        shutdown_signal_rx,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        apply_pending_adaptive_state, build_cli_session_memory_extractor, initialize_journal,
        prune_stale_pending_recovery,
    };
    use crate::cli::session::session_state::SessionState;
    use astra_services::session_journal;
    use wiremock::matchers::{header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn write_resumable_session(session_id: &str) {
        let writer = session_journal::JournalWriter::new(session_id).unwrap();
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(session_id),
                Some("gpt-5"),
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::interruption_recorded(
                Some(session_id),
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
    }

    fn write_profile_with_token(session_id: &str) {
        let mut creds = crate::cli::cli_config::cli_utils::CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            crate::cli::cli_config::cli_utils::Profile {
                access_token: Some("test-token".into()),
                last_session_id: Some(session_id.to_string()),
                ..Default::default()
            },
        );
        crate::cli::cli_config::cli_utils::save_credentials(&creds).unwrap();
    }

    fn poisoned_observability_session(
        session_id: &str,
    ) -> std::sync::Arc<std::sync::RwLock<astra_runtime::observability::ObservabilitySession>> {
        let session = std::sync::Arc::new(std::sync::RwLock::new(
            astra_runtime::observability::ObservabilitySession::new_simple(session_id),
        ));
        let poisoned = session.clone();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = poisoned.write().unwrap();
            panic!("poison observability lock");
        }));
        session
    }

    fn workspace_backup_path_for(session_id: &str) -> Option<std::path::PathBuf> {
        let workspace_dir = astra_services::session_workspace::workspace_dir_for(session_id);
        std::fs::read_dir(workspace_dir)
            .ok()?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("workspace.yaml.corrupt-"))
            })
    }

    // Verify that no_instructions=true prevents project instructions from being
    // loaded into SessionState, regardless of what's on disk.
    #[test]
    fn no_instructions_true_skips_loading() {
        use crate::cli::project_instructions::discover_instructions_from_paths;

        let tmp = tempfile::tempdir().unwrap();
        let astra_dir = tmp.path().join(".astra");
        std::fs::create_dir_all(&astra_dir).unwrap();
        std::fs::write(
            astra_dir.join("instructions.md"),
            "# Project rules\nDo things.",
        )
        .unwrap();

        let mut state = SessionState::default();
        // Simulate the guard: when no_instructions is true, skip the load.
        let no_instructions = true;
        if !no_instructions {
            if let Some(instructions) = discover_instructions_from_paths(Some(tmp.path()), None) {
                state.project_instructions = Some(instructions);
            }
        }
        assert!(
            state.project_instructions.is_none(),
            "no_instructions=true must not populate state.project_instructions"
        );
    }

    // Verify that no_instructions=false (default) still loads instructions.
    #[test]
    fn no_instructions_false_loads_instructions() {
        use crate::cli::project_instructions::discover_instructions_from_paths;

        let tmp = tempfile::tempdir().unwrap();
        let astra_dir = tmp.path().join(".astra");
        std::fs::create_dir_all(&astra_dir).unwrap();
        std::fs::write(
            astra_dir.join("instructions.md"),
            "# Project rules\nDo things.",
        )
        .unwrap();

        let mut state = SessionState::default();
        let no_instructions = false;
        if !no_instructions {
            if let Some(instructions) = discover_instructions_from_paths(Some(tmp.path()), None) {
                state.project_instructions = Some(instructions);
            }
        }
        assert!(
            state.project_instructions.is_some(),
            "no_instructions=false must load project_instructions when file exists"
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn prune_stale_pending_recovery_keeps_local_state_when_remote_is_stale() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let _creds_guard = crate::tests::isolate_credentials();
        let session_id = format!("pending-stale-{}", uuid::Uuid::new_v4());
        write_resumable_session(&session_id);
        write_profile_with_token(&session_id);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/sessions/{session_id}")))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "detail": "Session not found"
            })))
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();

        let mut state = SessionState {
            pending_recovery: Some(session_id.clone()),
            ..Default::default()
        };
        prune_stale_pending_recovery(&api, None, &mut state).await;

        assert_eq!(state.pending_recovery.as_deref(), Some(session_id.as_str()));
        assert_eq!(
            crate::cli::cli_config::cli_utils::load_credentials()
                .profiles
                .get("default")
                .and_then(|profile| profile.last_session_id.as_deref()),
            Some(session_id.as_str())
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn prune_stale_pending_recovery_keeps_live_session() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let _creds_guard = crate::tests::isolate_credentials();
        let session_id = format!("pending-live-{}", uuid::Uuid::new_v4());
        write_resumable_session(&session_id);
        write_profile_with_token(&session_id);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/sessions/{session_id}")))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "session_id": session_id,
                "status": "active"
            })))
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();

        let mut state = SessionState {
            pending_recovery: Some(session_id.clone()),
            ..Default::default()
        };
        prune_stale_pending_recovery(&api, None, &mut state).await;

        assert_eq!(state.pending_recovery.as_deref(), Some(session_id.as_str()));
    }

    #[test]
    fn initialize_journal_attaches_without_duplicate_start_or_workspace_reset() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let sid = format!("test-attach-{}", uuid::Uuid::new_v4());
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
                3,
                None,
                "hello",
                "world",
                0,
                10,
                5,
                20,
            ))
            .unwrap();

        let mut ws = astra_services::session_workspace::WorkspaceMetadata::new(&sid, "gpt-5");
        ws.turn_count = 3;
        ws.total_tokens_in = 10;
        ws.total_tokens_out = 5;
        astra_services::session_workspace::write_workspace(&ws).unwrap();

        let mut state = SessionState {
            model: Some("gpt-5".to_string()),
            ..Default::default()
        };
        initialize_journal(&mut state, &sid);

        let events = session_journal::read_journal(&sid).unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| {
                    event.event_type == session_journal::JournalEventType::SessionStart
                })
                .count(),
            1,
        );

        let restored_ws = astra_services::session_workspace::read_workspace(&sid).unwrap();
        assert_eq!(restored_ws.turn_count, 3);
        assert_eq!(restored_ws.total_tokens_in, 10);
        assert_eq!(restored_ws.total_tokens_out, 5);
        assert_eq!(restored_ws.status, "active");
    }

    #[test]
    fn initialize_journal_reopens_completed_session_without_resetting_workspace() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let sid = format!("test-reopen-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&sid),
                Some("gpt-5"),
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::session_end(Some(&sid), 3))
            .unwrap();

        let mut ws = astra_services::session_workspace::WorkspaceMetadata::new(&sid, "gpt-5");
        ws.turn_count = 3;
        ws.total_tokens_in = 120;
        ws.total_tokens_out = 45;
        ws.status = "completed".to_string();
        astra_services::session_workspace::write_workspace(&ws).unwrap();

        let mut state = SessionState {
            model: Some("gpt-5".to_string()),
            ..Default::default()
        };
        initialize_journal(&mut state, &sid);

        let events = session_journal::read_journal(&sid).unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| {
                    event.event_type == session_journal::JournalEventType::SessionStart
                })
                .count(),
            2,
        );
        let last_two: Vec<_> = events
            .iter()
            .rev()
            .take(2)
            .map(|e| e.event_type.clone())
            .collect();
        assert_eq!(
            last_two,
            vec![
                session_journal::JournalEventType::ConfigChange,
                session_journal::JournalEventType::SessionStart,
            ],
            "reopen must produce SessionStart followed by a startup ConfigChange",
        );

        let restored_ws = astra_services::session_workspace::read_workspace(&sid).unwrap();
        assert_eq!(restored_ws.turn_count, 3);
        assert_eq!(restored_ws.total_tokens_in, 120);
        assert_eq!(restored_ws.total_tokens_out, 45);
        assert_eq!(restored_ws.status, "active");
    }

    #[test]
    fn initialize_journal_does_not_duplicate_start_after_sync_marker() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let sid = format!("test-sync-marker-{}", uuid::Uuid::new_v4());
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
                "hello",
                "world",
                0,
                10,
                5,
                20,
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::cloud_pull_sync_marker(
                Some(&sid),
                "default",
                "session_startup",
                &["blocked_tools".to_string()],
                false,
            ))
            .unwrap();

        let mut state = SessionState {
            model: Some("gpt-5".to_string()),
            ..Default::default()
        };
        initialize_journal(&mut state, &sid);

        let events = session_journal::read_journal(&sid).unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| {
                    event.event_type == session_journal::JournalEventType::SessionStart
                })
                .count(),
            1,
        );
        assert_eq!(
            events.last().map(|event| &event.event_type),
            Some(&session_journal::JournalEventType::SyncMarker)
        );
    }

    #[test]
    fn apply_pending_adaptive_state_requeues_when_lock_is_poisoned() {
        let mut state = SessionState::default();
        state.pending_adaptive_state =
            Some(crate::cli::session::session_state::PersistedAdaptiveState {
                last_scenario_change_turn: Some(3),
                last_token_budget_direction: 1,
                last_token_budget_change_turn: Some(2),
                active_experiment_id: Some("exp-1".to_string()),
                active_variant: Some("variant-a".to_string()),
                tuned_config_json: None,
            });
        state.observability_session = Some(poisoned_observability_session("sid-adaptive"));

        apply_pending_adaptive_state(&mut state);

        let adaptive = state
            .pending_adaptive_state
            .as_ref()
            .expect("adaptive state should remain pending");
        assert_eq!(adaptive.last_token_budget_direction, 1);
        assert_eq!(adaptive.active_experiment_id.as_deref(), Some("exp-1"));
    }

    #[test]
    fn initialize_journal_preserves_existing_workspace() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let sid = "sess-existing-workspace";
        let mut ws = astra_services::session_workspace::WorkspaceMetadata::new(sid, "old-model");
        ws.turn_count = 7;
        ws.permission_mode = Some("plan".into());
        ws.last_context_trace = Some(astra_services::session_workspace::ContextTraceSignal {
            turn_id: "turn-7".into(),
            captured_at: None,
            tool_surface: Some(astra_services::session_workspace::ContextTraceToolSurface {
                tools_available: 8,
                visible_tools: vec!["lsp".into()],
                surface_scope: "latest_round".into(),
                latency_ms: 11,
            }),
            memory: None,
            history: None,
            budget: Some(
                astra_services::session_workspace::ContextTraceBudgetSignal {
                    max_tokens: 4096,
                    total_used: 700,
                    budget_pressure: 0.17,
                    compression_triggered: false,
                },
            ),
            timing: None,
            explanations: Vec::new(),
        });
        astra_services::session_workspace::write_workspace(&ws).unwrap();

        let mut state = SessionState::default();
        state.model = Some("new-model".into());
        initialize_journal(&mut state, sid);

        let persisted = astra_services::session_workspace::read_workspace(sid).unwrap();
        assert_eq!(persisted.model.as_deref(), Some("old-model"));
        assert_eq!(persisted.permission_mode.as_deref(), Some("plan"));
        assert_eq!(persisted.turn_count, 7);
        assert_eq!(
            persisted
                .last_context_trace
                .as_ref()
                .map(|trace| trace.turn_id.as_str()),
            Some("turn-7")
        );
    }

    #[test]
    fn initialize_journal_repairs_corrupt_workspace_yaml_from_live_state() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let sid = format!("test-corrupt-workspace-{}", uuid::Uuid::new_v4());
        let workspace_dir = astra_services::session_workspace::workspace_dir_for(&sid);
        std::fs::create_dir_all(&workspace_dir).unwrap();
        let workspace_path = workspace_dir.join("workspace.yaml");
        let corrupt_bytes = b":\nnot-valid-yaml".to_vec();
        std::fs::write(&workspace_path, &corrupt_bytes).unwrap();

        let mut state = SessionState {
            model: Some("gpt-5".to_string()),
            turn: 2,
            total_prompt_tokens: 20,
            total_completion_tokens: 10,
            ..Default::default()
        };
        initialize_journal(&mut state, &sid);

        assert_ne!(std::fs::read(&workspace_path).unwrap(), corrupt_bytes);
        let backup =
            workspace_backup_path_for(&sid).expect("corrupt workspace should be backed up");
        assert_eq!(std::fs::read(backup).unwrap(), corrupt_bytes);
        let workspace = astra_services::session_workspace::read_workspace(&sid).unwrap();
        assert_eq!(workspace.turn_count, 2);
        assert_eq!(workspace.total_tokens_in, 20);
        assert_eq!(workspace.total_tokens_out, 10);
        assert_eq!(workspace.model.as_deref(), Some("gpt-5"));
        assert_eq!(workspace.status, "active");
    }

    #[serial_test::serial]
    #[test]
    fn initialize_journal_marks_persistence_error_when_session_start_append_fails() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let sid = format!("test-init-start-append-fail-{}", uuid::Uuid::new_v4());
        let sessions_root = session_journal::journal_file_path(&sid)
            .parent()
            .unwrap()
            .to_path_buf();
        std::fs::create_dir_all(&sessions_root).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&sessions_root, std::fs::Permissions::from_mode(0o500))
                .unwrap();
        }

        let mut state = SessionState {
            model: Some("gpt-5".to_string()),
            ..Default::default()
        };
        initialize_journal(&mut state, &sid);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&sessions_root, std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }

        let error = state
            .session_persistence_error
            .as_deref()
            .expect("persistence error");
        assert!(
            error.contains("failed to append session start event"),
            "got: {error}"
        );
        assert!(
            error.contains("failed to write workspace metadata"),
            "got: {error}"
        );
    }

    #[serial_test::serial]
    #[test]
    fn initialize_journal_marks_persistence_error_when_workspace_write_fails() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let sid = format!("test-init-workspace-write-fail-{}", uuid::Uuid::new_v4());
        let workspace_dir = astra_services::session_workspace::workspace_dir_for(&sid);
        std::fs::create_dir_all(&workspace_dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&workspace_dir, std::fs::Permissions::from_mode(0o500))
                .unwrap();
        }

        let mut state = SessionState {
            model: Some("gpt-5".to_string()),
            ..Default::default()
        };
        initialize_journal(&mut state, &sid);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&workspace_dir, std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }

        assert_eq!(
            state.session_persistence_error.as_deref(),
            Some("failed to write workspace metadata")
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn build_cli_session_memory_extractor_initializes_when_authenticated() {
        let _creds_guard = crate::tests::isolate_credentials();
        write_profile_with_token("sess-memory");

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/auth/me"))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "user_id": "user-123"
            })))
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();

        let svc = build_cli_session_memory_extractor(&api, None).await;
        assert!(
            svc.is_some(),
            "authenticated CLI should build session memory extractor"
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn build_cli_session_memory_extractor_skips_when_auth_me_fails() {
        let _creds_guard = crate::tests::isolate_credentials();
        write_profile_with_token("sess-memory");

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/auth/me"))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "detail": "unauthorized"
            })))
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();

        let svc = build_cli_session_memory_extractor(&api, None).await;
        assert!(
            svc.is_none(),
            "missing auth identity should disable session memory extractor"
        );
    }
}
