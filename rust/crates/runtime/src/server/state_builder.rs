use super::*;

pub(super) async fn build_server_state(
    settings: AppSettings,
) -> Result<AppState, Box<dyn std::error::Error>> {
    ensure_core_schema(&settings.matrixone).await?;
    let shared_pool = SharedPool::new(&settings.matrixone).await?;
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
    .with_streaming_service(Arc::new(DatabaseStreamingService))
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
    )));

    // Wire chat turn bridge: prefer explicit URL override, fall back to in-process Rust impl.
    let state = if let Some(url) = settings.chat_turn_bridge_url {
        state
            .with_chat_turn_bridge_url(url)
            .with_chat_turn_bridge_secret(settings.chat_turn_bridge_secret)
    } else {
        let encryptor =
            Arc::new(FernetTokenEncryptor::from_env().map_err(Box::<dyn std::error::Error>::from)?);
        state
            .with_chat_turn_bridge(Arc::new(
                turn::bridge_inprocess::InProcessChatTurnBridge::new(
                    settings.matrixone.clone(),
                    encryptor,
                ),
            ))
            .with_chat_turn_bridge_secret(settings.chat_turn_bridge_secret)
    };
    let state = state.with_memoria_config(settings.memoria_base_url, settings.memoria_master_key);
    Ok(state)
}
