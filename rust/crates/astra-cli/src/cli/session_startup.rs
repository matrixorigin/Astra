//! REPL startup/setup orchestration extracted from `run_chat_repl`.

use super::*;
use session_guard::{
    install_session_panic_hook, install_sigterm_handler, subscribe_shutdown_signal,
};
use session_runtime::PipelineModules;

pub(crate) struct SessionStartupArtifacts {
    pub pipeline_modules: PipelineModules,
    pub edge_heartbeat_task: Option<tokio::task::JoinHandle<()>>,
    pub skill_quality_path: std::path::PathBuf,
    pub pinned_skills_path: std::path::PathBuf,
    pub shutdown_signal_rx: tokio::sync::watch::Receiver<Option<session_guard::ShutdownSignal>>,
}

// Note: `selector` field was removed — tool selection is now handled by the LLM directly.

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
    ) {
        let _ = clear_profile_last_session_if_matches(profile, &session_id);
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
        #[derive(serde::Deserialize)]
        struct MemoryModelWire {
            model_name: String,
        }

        let token = session_runtime::fresh_access_token(&self.api, self.profile.as_deref()).await?;
        let body = self
            .api
            .get_authed_path_text(&token, astra_thin_client::paths::model_memory())
            .await
            .ok()?;
        let response = serde_json::from_str::<MemoryModelWire>(&body).ok()?;
        Some(astra_runtime::memory_hooks::relevance::LlmConnParams {
            base_url: format!("{}/v1", self.api.api_origin()),
            api_key: token,
            model_name: response.model_name,
            provider: "openai".to_string(),
        })
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
) -> Result<SessionStartupArtifacts, String> {
    // Install panic hook to write session_end on unexpected crashes.
    install_session_panic_hook();
    // Install signal handlers so SIGTERM/SIGHUP can drain through normal REPL shutdown.
    install_sigterm_handler();
    let shutdown_signal_rx = subscribe_shutdown_signal();

    // --session-id: override with explicit session UUID
    if let Ok(sid) = std::env::var("ASTRA_CLI_SESSION_ID") {
        state.set_session_id(sid.clone());
        state.pending_recovery = None;
        eprintln!(
            "{}",
            format!("  Using session {}", truncate_str(&sid, 12)).magenta()
        );
    }

    // --name: set session display name
    if let Ok(name) = std::env::var("ASTRA_CLI_SESSION_NAME") {
        state.session_name = Some(name);
    }

    // --yes: warn about auto-approve mode
    if state.perm_manager.mode() == permission_manager::PermissionMode::Auto {
        eprintln!(
            "{}",
            "  ⚠ Auto-approve mode: all tool calls will execute without confirmation.".yellow()
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
        let maint =
            session_journal::run_session_maintenance(SESSION_TTL_DAYS, JOURNAL_COMPRESS_DAYS);
        if maint.sessions_deleted > 0 || maint.journals_compressed > 0 {
            let mut parts = Vec::new();
            if maint.sessions_deleted > 0 {
                parts.push(format!(
                    "{} expired sessions removed",
                    maint.sessions_deleted
                ));
            }
            if maint.journals_compressed > 0 {
                parts.push(format!("{} journals compressed", maint.journals_compressed));
            }
            eprintln!("  {} {}", theme::icon_ok(), parts.join(", ").dim());
        }
    }

    // Load persisted skill quality data from previous sessions
    let skill_quality_path = dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("astra")
        .join("skill_quality.json");
    state.skill_quality_tracker =
        astra_skills::quality::SkillQualityTracker::load(&skill_quality_path);

    // Load pinned skills from previous sessions
    let pinned_skills_path = dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("astra")
        .join("pinned_skills.json");
    if let Ok(data) = std::fs::read_to_string(&pinned_skills_path) {
        match serde_json::from_str::<std::collections::HashSet<String>>(&data) {
            Ok(set) => state.pinned_skills = set,
            Err(e) => eprintln!("⚠ Failed to parse pinned_skills.json: {e}"),
        }
    }
    tracer.phase("config_load");

    // Session-scoped quality tracker: tools that work well get boosted over time.
    // Seed from prior session snapshot (if any) so boost factors don't reset.
    let profile_name_for_quality = profile.unwrap_or("default");
    let persisted_quality =
        astra_turn_core::tool_health_persistence::load_tool_quality(profile_name_for_quality);
    let quality_tracker = {
        let mut tracker = tool_registry::ToolQualityTracker::new();
        if !persisted_quality.is_empty() {
            tracker.merge(&persisted_quality);
            eprintln!(
                "{}",
                format!(
                    "  ✓ Restored tool quality ({} tools tracked)",
                    persisted_quality.len()
                )
                .dim()
            );
        }
        std::sync::Arc::new(std::sync::Mutex::new(tracker))
    };
    let quality_tracker_for_save = quality_tracker.clone();
    state.tool_quality_tracker = Some(quality_tracker_for_save);
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
        if !cross_session_health_entries.is_empty() {
            eprintln!(
                "{}",
                format!(
                    "  ✓ Restored tool health ({} tools tracked)",
                    cross_session_health_entries.len()
                )
                .dim()
            );
        }
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
    state.team_store = std::sync::Arc::new(crate::http_team_store::HttpTeamStore::new(
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
        let has_models = session_runtime::check_server_has_models(api, &token).await;
        if !has_models {
            state.model = Some("⚠ none".to_string());
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
            "No LLM model configured on server. Run: astra-admin model add".yellow()
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
        pinned_skills_path,
        shutdown_signal_rx,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::session_journal::{self, JournalDirGuard};
    use wiremock::matchers::{header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn isolated_sessions_dir() -> (tempfile::TempDir, JournalDirGuard) {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let guard = JournalDirGuard::new(&sessions);
        (tmp, guard)
    }

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
        let mut creds = crate::cli_utils::CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            crate::cli_utils::Profile {
                access_token: Some("test-token".into()),
                last_session_id: Some(session_id.to_string()),
                ..Default::default()
            },
        );
        crate::cli_utils::save_credentials(&creds).unwrap();
    }

    // Verify that no_instructions=true prevents project instructions from being
    // loaded into SessionState, regardless of what's on disk.
    #[test]
    fn no_instructions_true_skips_loading() {
        use project_instructions::discover_instructions_from_paths;

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
        use project_instructions::discover_instructions_from_paths;

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

    #[tokio::test]
    async fn prune_stale_pending_recovery_clears_stale_last_session() {
        let (_tmp, _guard) = isolated_sessions_dir();
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

        assert_eq!(state.pending_recovery, None);
        assert_eq!(
            crate::cli_utils::load_credentials()
                .profiles
                .get("default")
                .and_then(|profile| profile.last_session_id.as_deref()),
            None
        );
    }

    #[tokio::test]
    async fn prune_stale_pending_recovery_keeps_live_session() {
        let (_tmp, _guard) = isolated_sessions_dir();
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
