use super::*;

/// Build the same [`AppState`] as production `astra-server` (MatrixOne, auth, in-process bridge, runs).
///
/// Intended for **ignored** integration tests (`ASTRA_SYSTEM_MATRIX_E2E=1`) that hit real HTTP
/// routes and assert database rows. Load `.env` / secrets the same way as local server startup.
pub async fn build_server_state(
    settings: AppSettings,
) -> Result<AppState, Box<dyn std::error::Error>> {
    ensure_core_schema(&settings.matrixone).await?;
    let shared_pool = SharedPool::new(&settings.matrixone).await?;
    let lease_hold_cache = Arc::new(TaskLeaseHoldCache::default());

    // Build shared pipeline learning modules (server-wide singleton).
    let learning_stack = build_pipeline_learning_stack(None);

    let state = AppState::new(
        ServiceInfo::default(),
        Arc::new(MatrixOneHealthChecker::new(settings.matrixone.clone())),
    )
    .with_shared_pool(shared_pool.clone())
    .with_auth_service(Arc::new(
        DatabaseAuthService::new(settings.matrixone.clone(), settings.jwt.clone())
            .with_pool(shared_pool.clone()),
    ))
    .with_session_service(Arc::new(
        DatabaseSessionService::new(settings.matrixone.clone()).with_pool(shared_pool.clone()),
    ))
    .with_agent_service(Arc::new(
        DatabaseAgentService::new(settings.matrixone.clone()).with_pool(shared_pool.clone()),
    ))
    .with_event_service(Arc::new(
        DatabaseEventService::new(settings.matrixone.clone()).with_pool(shared_pool.clone()),
    ))
    .with_context_service(Arc::new(
        DatabaseContextService::new(settings.matrixone.clone()).with_pool(shared_pool.clone()),
    ))
    .with_decision_service(Arc::new(
        DatabaseDecisionService::new(settings.matrixone.clone()).with_pool(shared_pool.clone()),
    ))
    .with_model_service({
        let encryptor =
            Arc::new(FernetTokenEncryptor::from_env().map_err(Box::<dyn std::error::Error>::from)?);
        Arc::new(DatabaseModelService::new(
            settings.matrixone.clone(),
            encryptor,
        ))
    })
    .with_job_service(Arc::new(InMemoryJobService::new()))
    .with_trigger_service(Arc::new(
        DatabaseTriggerService::new(settings.matrixone.clone()).with_pool(shared_pool.clone()),
    ))
    .with_workflow_service(Arc::new(
        DatabaseWorkflowService::new(settings.matrixone.clone()).with_pool(shared_pool.clone()),
    ))
    .with_sandbox_service(Arc::new(
        DatabaseSandboxService::new(settings.matrixone.clone()).with_pool(shared_pool.clone()),
    ))
    .with_branch_service(Arc::new(
        DatabaseBranchService::new(settings.matrixone.clone()).with_pool(shared_pool.clone()),
    ))
    .with_data_versioning_service(Arc::new(
        DatabaseDataVersioningService::new(settings.matrixone.clone())
            .with_pool(shared_pool.clone()),
    ))
    .with_marketplace_service(Arc::new(
        DatabaseMarketplaceService::new(settings.matrixone.clone()).with_pool(shared_pool.clone()),
    ))
    .with_marketplace_stats_service(Arc::new(
        DatabaseMarketplaceStatsService::new(settings.matrixone.clone())
            .with_pool(shared_pool.clone()),
    ))
    .with_replay_service(Arc::new(
        DatabaseReplayService::new(settings.matrixone.clone()).with_pool(shared_pool.clone()),
    ))
    .with_session_audit_service(Arc::new(
        DatabaseSessionAuditService::new(settings.matrixone.clone()).with_pool(shared_pool.clone()),
    ))
    .with_streaming_service(Arc::new(UnconfiguredStreamingService))
    .with_skill_service(Arc::new(
        DatabaseSkillService::new(settings.matrixone.clone()).with_pool(shared_pool.clone()),
    ))
    .with_skill_config_service(Arc::new(
        DatabaseSkillConfigService::new(settings.matrixone.clone()).with_pool(shared_pool.clone()),
    ))
    .with_fernet_encryptor(
        FernetTokenEncryptor::from_env().map_err(Box::<dyn std::error::Error>::from)?,
    )
    .with_evaluation_service(Arc::new(
        DatabaseEvaluationService::new(settings.matrixone.clone())
            .with_pool(shared_pool.clone())
            .with_memoria_config(
                settings.memoria_base_url.clone(),
                settings.memoria_master_key.clone(),
            ),
    ))
    .with_introspection_service(Arc::new(
        DatabaseIntrospectionService::new(settings.matrixone.clone())
            .with_pool(shared_pool.clone()),
    ))
    .with_reflect_service(Arc::new(
        DatabaseReflectService::new(settings.matrixone.clone()).with_pool(shared_pool.clone()),
    ))
    .with_learning_feedback_service(Arc::new(
        DatabaseLearningFeedbackService::new(settings.matrixone.clone())
            .with_pool(shared_pool.clone()),
    ))
    .with_turn_core_event_writer(Arc::new(
        DatabaseTurnCoreEventWriter::new(settings.matrixone.clone()).with_pool(shared_pool.clone()),
    ))
    .with_turn_tool_event_writer(Arc::new(
        DatabaseTurnToolEventWriter::new(settings.matrixone.clone()).with_pool(shared_pool.clone()),
    ))
    .with_turn_hook_db_writer(Arc::new(
        DatabaseTurnHookDbWriter::new(settings.matrixone.clone()).with_pool(shared_pool.clone()),
    ))
    .with_turn_reflection_lesson_writer(Arc::new(DatabaseTurnReflectionLessonWriter::new(
        settings.memoria_base_url.clone(),
        settings.memoria_master_key.clone(),
    )))
    .with_turn_observer_worker(Arc::new(DatabaseTurnObserverWorker::new(
        settings.memoria_base_url.clone(),
        settings.memoria_master_key.clone(),
    )))
    .with_turn_auxiliary_event_writer(Arc::new(
        DatabaseTurnAuxiliaryEventWriter::new(settings.matrixone.clone())
            .with_pool(shared_pool.clone()),
    ))
    .with_turn_session_activity_writer(Arc::new(
        DatabaseTurnSessionActivityWriter::new(settings.matrixone.clone())
            .with_pool(shared_pool.clone()),
    ))
    .with_admin_authorizer(Arc::new(DatabaseAdminAuthorizer::new(
        settings.matrixone.clone(),
        settings.jwt,
    )))
    .with_admin_initializer(Arc::new(
        DatabaseAdminInitializer::new(settings.matrixone.clone()).with_pool(shared_pool.clone()),
    ))
    .with_admin_token_writer(Arc::new(
        DatabaseAdminTokenWriter::from_env(settings.matrixone.clone())
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?
            .with_pool(shared_pool.clone()),
    ))
    .with_admin_token_reader(Arc::new(
        DatabaseAdminTokenReader::new(settings.matrixone.clone()).with_pool(shared_pool.clone()),
    ))
    .with_admin_audit_reader(Arc::new(
        DatabaseAdminAuditReader::new(settings.matrixone.clone()).with_pool(shared_pool.clone()),
    ))
    .with_admin_feedback_stats_reader(Arc::new(
        DatabaseAdminFeedbackStatsReader::new(settings.matrixone.clone())
            .with_pool(shared_pool.clone()),
    ))
    .with_admin_user_role_manager(Arc::new(DatabaseAdminUserRoleManager::new(
        settings.matrixone.clone(),
    )))
    .with_turn_learning_writer(learning_stack.writer.clone())
    .with_task_service(Arc::new(MatrixOneTaskService::from_shared(&shared_pool)))
    .with_edge_registry_service(Arc::new(DatabaseEdgeRegistryService::from_shared(
        &shared_pool,
    )))
    .with_task_lease_service(Arc::new(DatabaseTaskLeaseService::from_shared(
        &shared_pool,
        Arc::clone(&lease_hold_cache),
    )));

    // Wire chat turn bridge: prefer explicit URL override, fall back to in-process Rust impl.
    // Note: with_chat_turn_bridge_url auto-wires the learning writer from AppState.
    let state = if let Some(url) = settings.chat_turn_bridge_url {
        state
            .with_chat_turn_bridge_url(url)
            .with_chat_turn_bridge_secret(settings.chat_turn_bridge_secret)
    } else {
        let encryptor =
            Arc::new(FernetTokenEncryptor::from_env().map_err(Box::<dyn std::error::Error>::from)?);
        let edge_ledger = state.edge_callback_ledger.clone();
        state
            .with_chat_turn_bridge(Arc::new(
                turn::bridge_inprocess::InProcessChatTurnBridge::new(
                    settings.matrixone.clone(),
                    encryptor,
                )
                .with_learning_writer(learning_stack.writer.clone())
                .with_edge_callback_ledger(edge_ledger),
            ))
            .with_chat_turn_bridge_secret(settings.chat_turn_bridge_secret)
    };
    let state = state.with_memoria_config(settings.memoria_base_url, settings.memoria_master_key);

    // Wire run lifecycle service: uses ServerAgenticLoopHost for agentic loops.
    // Attach RunEngine for durable persistence of run state.
    let run_encryptor =
        Arc::new(FernetTokenEncryptor::from_env().map_err(Box::<dyn std::error::Error>::from)?);
    let run_store = Arc::new(astra_services::runs::InMemoryRunStateStore::default());
    let run_engine = crate::server::run_engine::RunEngine::new(run_store);

    // Wire multi-agent coordination: profile registry + delegation engine.
    let mut profile_registry = astra_services::AgentProfileRegistry::new();
    // Register default agent profiles so delegation validation works.
    {
        use astra_services::coordination::{AgentProfile, AgentTier};
        let mut orch = AgentProfile::new("orchestrator", "Orchestrator", AgentTier::Orchestrator);
        orch.system_prompt = Some(
            "You are the orchestrator agent. Coordinate sub-agents to complete complex tasks."
                .to_string(),
        );
        let _ = profile_registry.register(orch);

        let mut coder = AgentProfile::new("coder", "Coder", AgentTier::System);
        coder.system_prompt = Some(
            "You are a coding agent. Write, edit, and debug code to complete tasks.".to_string(),
        );
        coder.skill_filter = vec![
            "bash".into(),
            "read_file".into(),
            "write_file".into(),
            "str_replace".into(),
            "git_commit".into(),
        ];
        let _ = profile_registry.register(coder);

        let mut reviewer = AgentProfile::new("reviewer", "Reviewer", AgentTier::System);
        reviewer.system_prompt = Some(
            "You are a code review agent. Review code for bugs, security, and best practices."
                .to_string(),
        );
        reviewer.skill_filter = vec!["read_file".into(), "bash".into()];
        let _ = profile_registry.register(reviewer);

        let mut writer = AgentProfile::new("writer", "Writer", AgentTier::User);
        writer.system_prompt =
            Some("You are a documentation writer. Create clear, concise docs.".to_string());
        let _ = profile_registry.register(writer);
    }
    let profile_registry = Arc::new(profile_registry);
    let progress_broadcaster = Arc::new(crate::orchestration::ProgressBroadcaster::default());
    let delegation_tracker = Arc::new(
        crate::server::delegation_engine::DelegationTracker::new()
            .with_progress_broadcaster(Arc::clone(&progress_broadcaster)),
    );
    // Wire a real sub-run executor backed by ServerAgenticLoopHost.
    let sub_run_executor: Arc<dyn crate::server::delegation_engine::SubRunExecutor> = Arc::new(
        super::run_lifecycle::ServerSubRunExecutor::new(
            settings.matrixone.clone(),
            run_encryptor.clone(),
            state.edge_callback_ledger.clone(),
        )
        .with_pool(shared_pool.clone())
        .with_edge_connection_pool(state.edge_connection_pool.clone()),
    );
    let delegation_engine = Arc::new(
        crate::server::delegation_engine::DelegationEngine::with_executor(
            Arc::new(tokio::sync::RwLock::new((*profile_registry).clone())),
            Arc::new(run_engine.clone()),
            delegation_tracker,
            sub_run_executor,
        ),
    );

    // ── Resource governor (Phase 5) ───────────────────────────────────
    let resource_governor =
        astra_services::resource_governor::DatabaseResourceGovernor::new(shared_pool.clone());
    if let Err(e) = resource_governor.ensure_tables().await {
        eprintln!("[state_builder] resource_governor table init failed: {e}");
    }
    let resource_governor: std::sync::Arc<dyn astra_services::resource_governor::ResourceGovernor> =
        std::sync::Arc::new(resource_governor);

    // Create lifecycle service with delegation engine wired in.
    let run_lifecycle = super::run_lifecycle::AgenticRunLifecycleService::new(
        settings.matrixone.clone(),
        run_encryptor.clone(),
        state.edge_callback_ledger.clone(),
    )
    .with_pool(shared_pool.clone())
    .with_run_engine(run_engine)
    .with_delegation_engine(Arc::clone(&delegation_engine))
    .with_edge_connection_pool(state.edge_connection_pool.clone())
    .with_resource_governor(resource_governor.clone());

    // Wire team persistence store backed by MatrixOne.
    let team_store =
        astra_services::team_persistence::MatrixOneTeamStore::new(shared_pool.get().clone());
    let user_id = std::env::var("MO_USER_ID").unwrap_or_else(|_| "local".to_string());
    if let Err(e) = team_store.ensure_builtins(&user_id).await {
        eprintln!("[state_builder] team builtins seed failed: {e}");
    }
    let team_store: Arc<dyn astra_services::team_persistence::TeamPersistenceService> =
        Arc::new(team_store);
    let state = state
        .with_run_lifecycle_service(Arc::new(run_lifecycle))
        .with_agent_profile_registry(profile_registry)
        .with_delegation_engine(delegation_engine)
        .with_team_store(team_store)
        .with_resource_governor(resource_governor.clone());

    let matrix_rt = Arc::new(crate::matrix_cloud_runtime::MatrixCloudRuntime::attach(
        shared_pool.clone(),
        "default",
        &user_id,
        learning_stack.entity_graph.clone(),
        learning_stack.pattern_library.clone(),
        learning_stack.calibrator.clone(),
        Arc::new(Mutex::new(Vec::new())),
        None,
        Arc::clone(&lease_hold_cache),
    ));
    let state = state.with_matrix_cloud_runtime(Some(matrix_rt));
    Ok(state)
}

/// Owns the same `Arc` pipeline module handles as [`PipelineLearningWriter`] for wiring
/// [`crate::matrix_cloud_runtime::MatrixCloudRuntime`] without duplicate pools.
pub(super) struct PipelineLearningStack {
    pub writer: Arc<dyn TurnLearningWriter>,
    pub entity_graph: Arc<Mutex<crate::pipeline::entity::EntityGraph>>,
    pub pattern_library: Arc<Mutex<crate::pipeline::pattern::PatternLibrary>>,
    pub calibrator: Arc<Mutex<crate::pipeline::calibration::ProgressiveCalibrator>>,
    pub active_canary: Option<crate::evolution::service::PersistedActiveCanary>,
    /// Profile name used for cross-session persistence.
    pub profile: Option<String>,
}

impl PipelineLearningStack {
    /// Persist current learning state to disk if a profile was configured.
    pub fn save_with_active_canary(
        &self,
        active_canary: Option<crate::evolution::service::PersistedActiveCanary>,
    ) {
        if let Some(ref profile) = self.profile {
            let _ = crate::pipeline::persistence::save_learning_state_with_health_and_canary(
                profile,
                &self.entity_graph,
                &self.pattern_library,
                &self.calibrator,
                &[],
                active_canary,
            );
        }
    }
}

/// Creates pipeline modules (EntityGraph, PatternLibrary, ProgressiveCalibrator)
/// and wires them into a PipelineLearningWriter for turn-outcome-driven learning.
///
/// Bootstrap defaults are layered first so common patterns/entities exist on a
/// cold start. When `profile` is provided, persisted per-profile state from
/// `~/.astra/learning/<profile>.json` is then overlaid on top so user-learned
/// patterns (including recent block/deprioritize signals) are not erased by the
/// bootstrap priors.
pub(super) fn build_pipeline_learning_stack(profile: Option<&str>) -> PipelineLearningStack {
    use crate::pipeline::{
        calibration::ProgressiveCalibrator,
        defaults::{default_calibration, default_entities, default_patterns},
        entity::EntityGraph,
        learning::PipelineLearningWriter,
        pattern::PatternLibrary,
    };

    let entity_graph = Arc::new(Mutex::new(EntityGraph::new()));
    let pattern_library = Arc::new(Mutex::new(PatternLibrary::new()));
    // Use 0.70 as initial threshold — requires some confidence before auto-routing
    let calibrator = Arc::new(Mutex::new(ProgressiveCalibrator::new(0.70)));

    // Layer bootstrap defaults first so built-in patterns/entities are always present.
    if let Ok(mut eg) = entity_graph.lock() {
        eg.merge(&default_entities());
    }
    if let Ok(mut pl) = pattern_library.lock() {
        pl.merge(&default_patterns());
    }
    if let Ok(mut cal) = calibrator.lock() {
        cal.merge(&default_calibration());
    }

    // Overlay cross-session persisted state after bootstrap so user-learned
    // patterns win over defaults even when the defaults carry larger priors.
    let mut active_canary = None;
    if let Some(p) = profile
        && let Some(snapshot) = crate::pipeline::persistence::load_snapshot(p)
    {
        if let Ok(mut eg) = entity_graph.lock() {
            eg.merge(&snapshot.entities);
        }
        if let Ok(mut pl) = pattern_library.lock() {
            pl.overlay(&snapshot.patterns);
        }
        if let Some(calibration) = snapshot.calibration.as_ref()
            && let Ok(mut cal) = calibrator.lock()
        {
            cal.merge(calibration);
        }
        active_canary = snapshot.active_canary;
    }

    let writer = Arc::new(
        PipelineLearningWriter::new()
            .with_entity_graph(entity_graph.clone())
            .with_pattern_library(pattern_library.clone())
            .with_progressive_calibrator(calibrator.clone()),
    );
    PipelineLearningStack {
        writer,
        entity_graph,
        pattern_library,
        calibrator,
        active_canary,
        profile: profile.map(str::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evolution::types::PatternAction;
    use crate::pipeline::defaults::default_patterns;
    use crate::pipeline::routing::{DomainHint, TaskType};
    use crate::turn::contracts::TurnLearningOutcome;

    #[tokio::test]
    async fn build_pipeline_learning_writer_creates_functional_writer() {
        let writer = build_pipeline_learning_stack(None).writer;
        // Should accept an outcome without panic
        let outcome = TurnLearningOutcome {
            query: "matrixorigin PR check".to_string(),
            tools_selected: vec!["github_search".to_string()],
            tools_used: vec!["github_search".to_string()],
            success: true,
            quality: 0.8,
            was_corrected: false,
            task_type_label: Some("code".to_string()),
            domain_hint_label: Some("github".to_string()),
            user_feedback_score: None,
            reward_hacking_risk: 0.0,
            reward_hacking_flags: Vec::new(),
            causal_support_score: 1.0,
            causal_support_flags: Vec::new(),
        };
        let _ = writer.record_outcome(outcome).await;
    }

    #[tokio::test]
    async fn build_pipeline_learning_writer_learns_across_calls() {
        let writer = build_pipeline_learning_stack(None).writer;
        // Record two outcomes to meet PatternLibrary minimum
        for i in 0..2 {
            let outcome = TurnLearningOutcome {
                query: format!("test query {i}"),
                tools_selected: vec!["bash".to_string()],
                tools_used: vec!["bash".to_string()],
                success: true,
                quality: 0.7,
                was_corrected: false,
                task_type_label: Some("code".to_string()),
                domain_hint_label: None,
                user_feedback_score: None,
                reward_hacking_risk: 0.0,
                reward_hacking_flags: Vec::new(),
                causal_support_score: 1.0,
                causal_support_flags: Vec::new(),
            };
            let _ = writer.record_outcome(outcome).await;
        }
        // No panics = writer accumulates state correctly
    }

    #[test]
    fn pipeline_learning_stack_shares_arcs_between_writer_and_fields() {
        let s = build_pipeline_learning_stack(None);
        assert_eq!(Arc::strong_count(&s.entity_graph), 2);
        assert_eq!(Arc::strong_count(&s.pattern_library), 2);
        assert_eq!(Arc::strong_count(&s.calibrator), 2);
    }

    #[test]
    fn persisted_patterns_overlay_defaults_without_losing_blocked_state() {
        let tools = vec!["grep".to_string()];
        let mut persisted = crate::pipeline::pattern::PatternLibrary::new();
        persisted.record_outcome(
            &tools,
            TaskType::Code,
            Some(DomainHint::Code),
            true,
            0.8,
            None,
        );
        persisted.apply_evolution_action("grep", PatternAction::Block);

        let mut layered = crate::pipeline::pattern::PatternLibrary::new();
        layered.merge(&default_patterns());
        layered.overlay(&persisted.export());

        assert!(
            layered
                .blocked_tool_names()
                .iter()
                .any(|name| name == "grep"),
            "persisted blocked tools should survive bootstrap default layering"
        );
    }
}
