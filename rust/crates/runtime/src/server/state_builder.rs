use super::*;

pub(super) async fn build_server_state(
    settings: AppSettings,
) -> Result<AppState, Box<dyn std::error::Error>> {
    ensure_core_schema(&settings.matrixone).await?;
    let shared_pool = SharedPool::new(&settings.matrixone).await?;
    let lease_hold_cache = Arc::new(TaskLeaseHoldCache::default());

    // Build shared pipeline learning modules (server-wide singleton).
    let learning_stack = build_pipeline_learning_stack();

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
    .with_replay_service(Arc::new(
        DatabaseReplayService::new(settings.matrixone.clone()).with_pool(shared_pool.clone()),
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
        DatabaseEvaluationService::new(settings.matrixone.clone()).with_pool(shared_pool.clone()),
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
    let run_store = Arc::new(mo_agent_services::runs::InMemoryRunStateStore::default());
    let run_engine = crate::server::run_engine::RunEngine::new(run_store);
    let run_lifecycle = super::run_lifecycle::AgenticRunLifecycleService::new(
        settings.matrixone.clone(),
        run_encryptor,
        state.edge_callback_ledger.clone(),
    )
    .with_pool(shared_pool.clone())
    .with_run_engine(run_engine.clone());
    let state = state.with_run_lifecycle_service(Arc::new(run_lifecycle));

    // Wire multi-agent coordination: profile registry + delegation engine.
    let profile_registry = Arc::new(mo_agent_services::AgentProfileRegistry::new());
    let delegation_tracker = Arc::new(
        crate::server::delegation_engine::DelegationTracker::new(),
    );
    let delegation_engine = Arc::new(
        crate::server::delegation_engine::DelegationEngine::new(
            Arc::new(tokio::sync::RwLock::new(
                (*profile_registry).clone(),
            )),
            Arc::new(run_engine),
            delegation_tracker,
        ),
    );
    let state = state
        .with_agent_profile_registry(profile_registry)
        .with_delegation_engine(delegation_engine);

    let user_id = std::env::var("MO_USER_ID").unwrap_or_else(|_| "local".to_string());
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
}

/// Creates pipeline modules (EntityGraph, PatternLibrary, ProgressiveCalibrator)
/// and wires them into a PipelineLearningWriter for turn-outcome-driven learning.
fn build_pipeline_learning_stack() -> PipelineLearningStack {
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

    if let Ok(mut eg) = entity_graph.lock() {
        eg.merge(&default_entities());
    }
    if let Ok(mut pl) = pattern_library.lock() {
        pl.merge(&default_patterns());
    }
    if let Ok(mut cal) = calibrator.lock() {
        cal.merge(&default_calibration());
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn::contracts::TurnLearningOutcome;

    #[tokio::test]
    async fn build_pipeline_learning_writer_creates_functional_writer() {
        let writer = build_pipeline_learning_stack().writer;
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
        };
        let _ = writer.record_outcome(outcome).await;
    }

    #[tokio::test]
    async fn build_pipeline_learning_writer_learns_across_calls() {
        let writer = build_pipeline_learning_stack().writer;
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
            };
            let _ = writer.record_outcome(outcome).await;
        }
        // No panics = writer accumulates state correctly
    }

    #[test]
    fn pipeline_learning_stack_shares_arcs_between_writer_and_fields() {
        let s = build_pipeline_learning_stack();
        assert_eq!(Arc::strong_count(&s.entity_graph), 2);
        assert_eq!(Arc::strong_count(&s.pattern_library), 2);
        assert_eq!(Arc::strong_count(&s.calibrator), 2);
    }
}
