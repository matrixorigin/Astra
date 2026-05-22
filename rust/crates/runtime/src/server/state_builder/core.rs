use super::*;

pub(super) fn build_auth_service(
    settings: &AppSettings,
    shared_pool: &SharedPool,
) -> Result<Arc<dyn AuthService>, Box<dyn std::error::Error>> {
    let auth_mode = std::env::var("ASTRA_AUTH_MODE")
        .unwrap_or_else(|_| "local_jwt".to_string())
        .trim()
        .to_ascii_lowercase();
    let auth_service: Arc<dyn AuthService> = match auth_mode.as_str() {
        "" | "local_jwt" | "local" | "database" => Arc::new(
            DatabaseAuthService::new(settings.matrixone.clone(), settings.jwt.clone())
                .with_pool(shared_pool.clone()),
        ),
        "trusted_moi" => Arc::new(
            astra_services::auth::TrustedMoiAuthService::from_env()
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?,
        ),
        other => {
            return Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("unsupported ASTRA_AUTH_MODE={other}; expected local_jwt or trusted_moi"),
            )));
        }
    };
    Ok(auth_service)
}

pub(super) fn build_core_state(
    settings: &AppSettings,
    shared_pool: &SharedPool,
    shared_encryptor: &Arc<FernetTokenEncryptor>,
    auth_service: Arc<dyn AuthService>,
) -> AppState {
    AppState::new(
        ServiceInfo::default(),
        Arc::new(
            MatrixOneHealthChecker::new(settings.matrixone.clone()).with_pool(shared_pool.clone()),
        ),
    )
    .with_cors_origins(settings.api.cors_origins.clone())
    .with_shared_pool(shared_pool.clone())
    .with_plan_repository(Arc::new(astra_plan::CloudPlanRepository::new(
        shared_pool.get().clone(),
    )))
    .with_auth_service(auth_service)
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
    .with_model_service(Arc::new(
        DatabaseModelService::new(settings.matrixone.clone(), Arc::clone(shared_encryptor))
            .with_pool(shared_pool.clone()),
    ))
    .with_job_service(Arc::new(InMemoryJobService::new()))
    .with_trigger_service(Arc::new(
        DatabaseTriggerService::new(settings.matrixone.clone()).with_pool(shared_pool.clone()),
    ))
    .with_workflow_service(Arc::new(UnconfiguredWorkflowService))
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
    .with_skill_service(Arc::new(
        DatabaseSkillService::new(settings.matrixone.clone()).with_pool(shared_pool.clone()),
    ))
    .with_skill_config_service(Arc::new(
        DatabaseSkillConfigService::new(settings.matrixone.clone()).with_pool(shared_pool.clone()),
    ))
    .with_llm_trusted_domain_service(Arc::new(
        astra_services::DatabaseLlmTrustedDomainService::new(settings.matrixone.clone())
            .with_pool(shared_pool.clone()),
    ))
    .with_fernet_encryptor(shared_encryptor.as_ref().clone())
    .with_evaluation_service(Arc::new(
        DatabaseEvaluationService::new(settings.matrixone.clone())
            .with_pool(shared_pool.clone())
            .with_memoria_config(
                settings.memoria.base_url.clone(),
                settings.memoria.master_key.clone(),
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
}

pub(super) fn install_turn_persistence_services(
    state: AppState,
    settings: &AppSettings,
    shared_pool: &SharedPool,
) -> AppState {
    state
        .with_turn_core_event_writer(Arc::new(
            DatabaseTurnCoreEventWriter::new(settings.matrixone.clone())
                .with_pool(shared_pool.clone()),
        ))
        .with_turn_tool_event_writer(Arc::new(
            DatabaseTurnToolEventWriter::new(settings.matrixone.clone())
                .with_pool(shared_pool.clone()),
        ))
        .with_turn_hook_db_writer(Arc::new(
            DatabaseTurnHookDbWriter::new(settings.matrixone.clone())
                .with_pool(shared_pool.clone()),
        ))
        .with_turn_reflection_lesson_writer(Arc::new(DatabaseTurnReflectionLessonWriter::new(
            settings.memoria.base_url.clone(),
            settings.memoria.master_key.clone(),
        )))
        .with_turn_observer_worker(Arc::new(DatabaseTurnObserverWorker::new(
            settings.memoria.base_url.clone(),
            settings.memoria.master_key.clone(),
        )))
        .with_turn_auxiliary_event_writer(Arc::new(
            DatabaseTurnAuxiliaryEventWriter::new(settings.matrixone.clone())
                .with_pool(shared_pool.clone()),
        ))
        .with_turn_session_activity_writer(Arc::new(
            DatabaseTurnSessionActivityWriter::new(settings.matrixone.clone())
                .with_pool(shared_pool.clone()),
        ))
}

pub(super) fn install_admin_services(
    state: AppState,
    settings: &AppSettings,
    shared_pool: &SharedPool,
) -> Result<AppState, Box<dyn std::error::Error>> {
    Ok(state
        .with_admin_authorizer(Arc::new(
            DatabaseAdminAuthorizer::new(settings.matrixone.clone(), settings.jwt.clone())
                .with_pool(shared_pool.clone()),
        ))
        .with_admin_initializer(Arc::new(
            DatabaseAdminInitializer::new(settings.matrixone.clone())
                .with_pool(shared_pool.clone()),
        ))
        .with_admin_token_writer(Arc::new(
            DatabaseAdminTokenWriter::from_env(settings.matrixone.clone())
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?
                .with_pool(shared_pool.clone()),
        ))
        .with_admin_token_reader(Arc::new(
            DatabaseAdminTokenReader::new(settings.matrixone.clone())
                .with_pool(shared_pool.clone()),
        ))
        .with_admin_audit_reader(Arc::new(
            DatabaseAdminAuditReader::new(settings.matrixone.clone())
                .with_pool(shared_pool.clone()),
        ))
        .with_admin_feedback_stats_reader(Arc::new(
            DatabaseAdminFeedbackStatsReader::new(settings.matrixone.clone())
                .with_pool(shared_pool.clone()),
        ))
        .with_admin_user_role_manager(Arc::new(
            DatabaseAdminUserRoleManager::new(settings.matrixone.clone())
                .with_pool(shared_pool.clone()),
        ))
        .with_admin_config_service(Arc::new(
            astra_services::DatabaseAdminConfigService::new(settings.matrixone.clone())
                .with_pool(shared_pool.clone()),
        )))
}

pub(super) fn install_execution_services(
    state: AppState,
    shared_pool: &SharedPool,
    lease_hold_cache: &Arc<TaskLeaseHoldCache>,
) -> AppState {
    state
        .with_task_service(Arc::new(MatrixOneTaskService::from_shared(shared_pool)))
        .with_edge_registry_service(Arc::new(DatabaseEdgeRegistryService::from_shared(
            shared_pool,
        )))
        .with_task_lease_service(Arc::new(DatabaseTaskLeaseService::from_shared(
            shared_pool,
            Arc::clone(lease_hold_cache),
        )))
}
