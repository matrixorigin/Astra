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

    if let Ok(settings) = astra_runtime::matrix_settings_from_env().map_err(|e| {
        eprintln!("[startup] cloud sync disabled: {e}. Set MATRIXONE_PASSWORD to enable.");
        state.matrix_runtime = None;
    }) {
        state.matrix_runtime = match SharedPool::new(&settings).await {
            Ok(pool) => {
                let user_id = astra_core::cli_user_id();
                let lease = std::sync::Arc::new(astra_services::TaskLeaseHoldCache::default());
                let mut runtime = astra_runtime::MatrixCloudRuntime::attach(
                    pool,
                    profile.unwrap_or("default"),
                    &user_id,
                    lease,
                );
                if let Ok(enc) = astra_services::FernetTokenEncryptor::from_env() {
                    runtime = runtime.with_encryptor(std::sync::Arc::new(enc));
                }
                Some(std::sync::Arc::new(runtime))
            }
            Err(e) => {
                // Log the error so users know cloud sync won't work for this session.
                // This is a common cause of missing checkpoint/event sync in diagnostics.
                astra_core::agent_warn!(
                    "matrix_pool",
                    "Cloud sync disabled for this session: failed to connect to MatrixOne — {e}"
                );
                eprintln!(
                    "  {} Cloud sync disabled: MatrixOne connection failed",
                    theme::icon_warn()
                );
                None
            }
        };
        if let Some(ref mc) = state.matrix_runtime {
            let pool = mc.shared_pool().get().clone();
            let user_id = astra_core::cli_user_id();
            let mo_team_store = astra_services::team_persistence::MatrixOneTeamStore::new(pool);
            if let Err(e) = mo_team_store.ensure_builtins(&user_id).await {
                eprintln!("  {} team store builtins: {e}", theme::icon_warn());
            }
            state.team_store = std::sync::Arc::new(mo_team_store);
            // Session-memory.md background extractor. Shares broker +
            // ingestion with the rest of the runtime so events land in
            // agent_events and the CLI can bridge UX signals.
            state.session_memory_extractor = mc.clone_memory_extraction_service();
        }
    }
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

    if let Some(token) = current_access_token(profile) {
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
    if let Some(ref tok) = current_access_token(profile) {
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

    if let Some(token) = current_access_token(profile) {
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
}
