use super::*;
use crate::server::run::lifecycle::{AgenticRunLifecycleService, ServerSubRunExecutor};

pub(super) async fn build_runtime_wiring(
    settings: &AppSettings,
    shared_pool: &SharedPool,
    lease_hold_cache: &Arc<TaskLeaseHoldCache>,
    run_encryptor: &Arc<FernetTokenEncryptor>,
    state: &AppState,
) -> Result<RuntimeWiring, Box<dyn std::error::Error>> {
    let run_store = Arc::new(astra_services::runs::DatabaseRunStateStore::new(
        shared_pool.clone(),
    ));
    let state_projection_store = Arc::new(astra_services::DatabaseStateProjectionStore::new(
        shared_pool.clone(),
    ));
    let run_engine = crate::server::run::engine::RunEngine::new(run_store)
        .with_projection_store(Arc::clone(&state_projection_store));
    recover_active_runs(&run_engine).await;

    let profile_registry = Arc::new(default_agent_profile_registry());
    let progress_broadcaster = Arc::new(crate::orchestration::ProgressBroadcaster::default());
    let delegation_tracker = Arc::new(
        crate::server::delegation::engine::DelegationTracker::new()
            .with_progress_broadcaster(Arc::clone(&progress_broadcaster)),
    );
    let user_id = "local";
    let matrix_rt = Arc::new(
        crate::matrix_cloud_runtime::MatrixCloudRuntime::attach(
            shared_pool.clone(),
            "default",
            user_id,
            Arc::clone(lease_hold_cache),
        )
        .with_encryptor(Arc::clone(run_encryptor)),
    );
    let memory_extraction_service = matrix_rt.clone_memory_extraction_service();
    let runner_pool = Arc::new(state.runner_pool.clone());
    let runner_rpc_transport: Arc<dyn crate::server::tool_transport::RunnerRpcTransport> =
        runner_pool.clone();
    let runner_scheduler: Arc<dyn crate::server::runner_pool::ServerRunnerScheduler> = runner_pool;
    let workspace_record_store = Arc::new(astra_services::DatabaseWorkspaceRecordStore::new(
        shared_pool.clone(),
    ));
    workspace_record_store.ensure_tables().await?;
    let workspace_record_store: Arc<dyn astra_services::WorkspaceStateStore> =
        workspace_record_store;

    let sub_run_executor: Arc<dyn crate::server::delegation::engine::SubRunExecutor> = {
        let mut exec = ServerSubRunExecutor::new(
            settings.matrixone.clone(),
            Arc::clone(run_encryptor),
            state.edge_callback_ledger.clone(),
        )
        .with_pool(shared_pool.clone())
        .with_edge_connection_pool(state.edge_connection_pool.clone())
        .with_runner_rpc_transport(Arc::clone(&runner_rpc_transport))
        .with_skill_service(state.skill_service.clone());
        if let Some(svc) = memory_extraction_service.as_ref() {
            exec = exec.with_memory_extraction_service(Arc::clone(svc));
        }
        Arc::new(exec)
    };
    let delegation_engine = Arc::new(
        crate::server::delegation::engine::DelegationEngine::with_executor(
            Arc::new(tokio::sync::RwLock::new((*profile_registry).clone())),
            Arc::new(run_engine.clone()),
            delegation_tracker,
            sub_run_executor,
        )
        .with_projection_store(Arc::clone(&state_projection_store)),
    );

    let resource_governor = initialize_resource_governor(shared_pool).await;
    let run_concurrency_limit = std::env::var("ASTRA_RUN_CONCURRENCY_LIMIT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);
    let mut run_lifecycle = AgenticRunLifecycleService::new(
        settings.matrixone.clone(),
        Arc::clone(run_encryptor),
        state.edge_callback_ledger.clone(),
        run_engine,
    )
    .with_pool(shared_pool.clone())
    .with_delegation_engine(Arc::clone(&delegation_engine))
    .with_edge_connection_pool(state.edge_connection_pool.clone())
    .with_edge_dispatch_service(state.execution.edge_dispatch_service.clone())
    .with_edge_registry_service(state.execution.edge_registry_service.clone())
    .with_runner_rpc_transport(runner_rpc_transport)
    .with_runner_scheduler(runner_scheduler)
    .with_workspace_record_store(workspace_record_store)
    .with_resource_governor(resource_governor.clone())
    .with_skill_service(state.skill_service.clone())
    .with_mcp_registry_service(state.mcp_registry_service.clone())
    .with_hook_db_writer(state.turn_persistence.hook_db_writer.clone())
    .with_observer_worker(state.turn_persistence.observer_worker.clone())
    .with_tool_event_writer(state.turn_persistence.tool_event_writer.clone())
    .with_auxiliary_event_writer(state.turn_persistence.auxiliary_event_writer.clone())
    .with_run_concurrency_limit(run_concurrency_limit)
    .with_tool_execution_service(state.tool_execution_service.clone());
    if let Some(svc) = memory_extraction_service.as_ref() {
        run_lifecycle = run_lifecycle.with_memory_extraction_service(Arc::clone(svc));
    }

    #[cfg(feature = "harness")]
    let run_lifecycle = run_lifecycle.with_harness_registry(state.harness_registry.clone());

    let team_store = initialize_team_store(shared_pool, user_id).await;
    Ok(RuntimeWiring {
        matrix_rt,
        run_lifecycle,
        profile_registry,
        delegation_engine,
        team_store,
        resource_governor,
    })
}

async fn recover_active_runs(run_engine: &crate::server::run::engine::RunEngine) {
    match run_engine.recover_active_runs().await {
        Ok(recovered_runs) => {
            if !recovered_runs.is_empty() {
                let waiting = recovered_runs
                    .iter()
                    .filter(|run| run.status == astra_core::STATUS_WAITING)
                    .count();
                let failed = recovered_runs
                    .iter()
                    .filter(|run| run.status == astra_core::STATUS_FAILED)
                    .count();
                tracing::warn!(
                    target: "astra_runtime::state_builder",
                    recovered_total = recovered_runs.len(),
                    recovered_waiting = waiting,
                    recovered_failed = failed,
                    "recovered durable active runs during startup"
                );
            }
        }
        Err(error) => {
            tracing::error!(
                target: "astra_runtime::state_builder",
                error = %error,
                "failed to recover durable active runs during startup"
            );
        }
    }
}

async fn initialize_resource_governor(
    shared_pool: &SharedPool,
) -> std::sync::Arc<dyn astra_services::resource_governor::ResourceGovernor> {
    let resource_governor =
        astra_services::resource_governor::DatabaseResourceGovernor::new(shared_pool.clone());
    if let Err(error) = resource_governor.ensure_tables().await {
        tracing::warn!(
            target: "astra_runtime::state_builder",
            error = %error,
            "resource_governor table init failed"
        );
    }
    std::sync::Arc::new(resource_governor)
}

async fn initialize_team_store(
    shared_pool: &SharedPool,
    user_id: &str,
) -> Arc<dyn astra_services::team_persistence::TeamPersistenceService> {
    let team_store =
        astra_services::team_persistence::MatrixOneTeamStore::new(shared_pool.get().clone());
    if let Err(error) = team_store.ensure_builtins(user_id).await {
        tracing::warn!(
            target: "astra_runtime::state_builder",
            error = %error,
            user_id = %user_id,
            "team builtins seed failed"
        );
    }
    Arc::new(team_store)
}

pub(super) fn default_agent_profile_registry() -> astra_services::AgentProfileRegistry {
    use astra_services::coordination::{AgentProfile, AgentTier};

    let mut profile_registry = astra_services::AgentProfileRegistry::new();

    let mut orch = AgentProfile::new("orchestrator", "Orchestrator", AgentTier::Orchestrator);
    orch.system_prompt = Some(
        "You are the orchestrator agent. Coordinate sub-agents to complete complex tasks."
            .to_string(),
    );
    let _ = profile_registry.register(orch);

    let mut coder = AgentProfile::new("coder", "Coder", AgentTier::System);
    coder.system_prompt =
        Some("You are a coding agent. Write, edit, and debug code to complete tasks.".to_string());
    coder.skill_filter = vec![
        "bash".into(),
        "read_file".into(),
        "write_file".into(),
        "str_replace".into(),
        "git".into(),
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

    profile_registry
}
