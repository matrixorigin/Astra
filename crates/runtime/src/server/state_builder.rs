use super::*;
use crate::server::run::lifecycle::AgenticRunLifecycleService;

pub(crate) mod bridge;
mod core;
mod runtime;

struct RuntimeWiring {
    matrix_rt: Arc<crate::matrix_cloud_runtime::MatrixCloudRuntime>,
    run_lifecycle: AgenticRunLifecycleService,
    profile_registry: Arc<astra_services::AgentProfileRegistry>,
    delegation_engine: Arc<crate::server::delegation::engine::DelegationEngine>,
    team_store: Arc<dyn astra_services::team_persistence::TeamPersistenceService>,
    resource_governor: std::sync::Arc<dyn astra_services::resource_governor::ResourceGovernor>,
}

/// Build the same [`AppState`] as production `astra-server` (MatrixOne, auth, in-process bridge, runs).
///
/// Intended for **ignored** integration tests (`ASTRA_TEST_DB_IT=1`) that hit real HTTP
/// routes and assert database rows. Load `.env` / secrets the same way as local server startup.
pub async fn build_server_state(
    settings: AppSettings,
) -> Result<AppState, Box<dyn std::error::Error>> {
    ensure_core_schema(&settings.matrixone, &settings.database_bootstrap_catalog).await?;
    let shared_pool = SharedPool::new(&settings.matrixone).await?;
    let lease_hold_cache = Arc::new(TaskLeaseHoldCache::default());
    let shared_encryptor = Arc::new(
        FernetTokenEncryptor::from_key(settings.token_encryption_key.as_deref())
            .map_err(Box::<dyn std::error::Error>::from)?,
    );
    let auth_service = core::build_auth_service(&settings, &shared_pool, &shared_encryptor)?;

    let state = core::build_core_state(&settings, &shared_pool, &shared_encryptor, auth_service);
    let state = core::install_turn_persistence_services(state, &settings, &shared_pool);
    let state = core::install_admin_services(state, &settings, &shared_pool)?;
    let state = core::install_execution_services(state, &shared_pool, &lease_hold_cache)
        .with_memoria_config(
            settings.memoria.base_url.clone(),
            settings.memoria.master_key.clone(),
        );
    let state = install_skillify_harness_service(state, &settings, &shared_pool, &shared_encryptor);

    let wiring = runtime::build_runtime_wiring(
        &settings,
        &shared_pool,
        &lease_hold_cache,
        &shared_encryptor,
        &state,
    )
    .await?;
    let matrix_rt = Arc::clone(&wiring.matrix_rt);

    let state = state
        .with_run_lifecycle_service(Arc::new(wiring.run_lifecycle))
        .with_agent_profile_registry(Arc::clone(&wiring.profile_registry))
        .with_delegation_engine(Arc::clone(&wiring.delegation_engine))
        .with_team_store(Arc::clone(&wiring.team_store))
        .with_resource_governor(Arc::clone(&wiring.resource_governor));
    let state = bridge::attach_chat_turn_bridge(
        state,
        &settings,
        &shared_pool,
        &shared_encryptor,
        &matrix_rt,
    );

    let state = state.with_matrix_cloud_runtime(Some(matrix_rt));
    Ok(state)
}

fn install_skillify_harness_service(
    state: AppState,
    settings: &AppSettings,
    shared_pool: &SharedPool,
    shared_encryptor: &Arc<FernetTokenEncryptor>,
) -> AppState {
    let skillify_agent_executor = Arc::new(
        super::skillify_agent_executor::RuntimeSkillifyAgentExecutor::new(
            settings.matrixone.clone(),
            Arc::clone(shared_encryptor),
            state.admin.config_service.clone(),
            shared_pool.clone(),
        ),
    );
    state.with_harness_service(Arc::new(
        DatabaseHarnessService::new(shared_pool.clone())
            .with_skillify_agent_executor(skillify_agent_executor),
    ))
}

#[cfg(test)]
mod tests {
    use super::runtime::default_agent_profile_registry;
    use astra_services::coordination::AgentTier;

    #[test]
    fn default_agent_profile_registry_registers_core_profiles() {
        let registry = default_agent_profile_registry();

        assert_eq!(registry.list().len(), 4);

        let orchestrator = registry.get("orchestrator").expect("orchestrator profile");
        assert_eq!(orchestrator.name, "Orchestrator");
        assert_eq!(orchestrator.tier, AgentTier::Orchestrator);
        assert!(orchestrator.can_delegate);

        let coder = registry.get("coder").expect("coder profile");
        assert_eq!(coder.tier, AgentTier::System);
        assert!(coder.skill_filter.iter().any(|tool| tool == "write_file"));

        let reviewer = registry.get("reviewer").expect("reviewer profile");
        assert_eq!(reviewer.skill_filter, vec!["read_file", "bash"]);

        let writer = registry.get("writer").expect("writer profile");
        assert_eq!(writer.tier, AgentTier::User);
        assert!(!writer.can_delegate);
    }
}
