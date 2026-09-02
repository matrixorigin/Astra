use super::*;
use crate::server::run::lifecycle::AgenticRunLifecycleService;

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

/// Build the same [`AppState`] as production `astra-server` (MatrixOne, auth, durable runs).
///
/// Intended for **ignored** integration tests (`ASTRA_TEST_DB_IT=1`) that hit real HTTP
/// routes and assert database rows. Load `.env` / secrets the same way as local server startup.
pub async fn build_server_state(
    settings: AppSettings,
) -> Result<AppState, Box<dyn std::error::Error>> {
    ensure_core_schema(&settings.matrixone, &settings.database_bootstrap_catalog).await?;
    // Keep a small, bounded control-plane reservation inside the configured
    // total. Long-running runs and background persistence use the general pool;
    // auth/health retain a lane even when that pool is temporarily saturated.
    let control_max = control_pool_max_connections(settings.matrixone.db_pool_max_connections);
    let shared_max = settings
        .matrixone
        .db_pool_max_connections
        .saturating_sub(control_max)
        .max(1);
    let shared_settings = pool_settings_with_limit(&settings.matrixone, shared_max);
    let shared_pool = SharedPool::new(&shared_settings).await?;
    let control_pool = if control_max == 0 {
        None
    } else {
        Some(SharedPool::new(&pool_settings_with_limit(&settings.matrixone, control_max)).await?)
    };

    let shared_encryptor = Arc::new(
        FernetTokenEncryptor::from_key(settings.token_encryption_key.as_deref())
            .map_err(Box::<dyn std::error::Error>::from)?,
    );
    let auth_service = core::build_auth_service(
        &settings,
        &shared_pool,
        control_pool.as_ref(),
        &shared_encryptor,
    )?;

    let state = core::build_core_state(
        &settings,
        &shared_pool,
        control_pool.as_ref(),
        &shared_encryptor,
        auth_service,
    );
    let state = core::install_turn_persistence_services(state, &settings, &shared_pool);
    let state =
        core::install_admin_services(state, &settings, &shared_pool, control_pool.as_ref())?;
    let state = core::install_execution_services(state, &shared_pool).with_memoria_config(
        settings.memoria.base_url.clone(),
        settings.memoria.master_key.clone(),
    );
    let state = install_skillify_harness_service(state, &settings, &shared_pool, &shared_encryptor);

    let wiring =
        runtime::build_runtime_wiring(&settings, &shared_pool, &shared_encryptor, &state).await?;
    let matrix_rt = Arc::clone(&wiring.matrix_rt);

    let artifact_signing_secret =
        core::derive_runtime_subkey(&settings.runtime_root_secret, b"artifact-signing")
            .iter()
            .fold(String::with_capacity(64), |mut encoded, byte| {
                use std::fmt::Write;
                write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
                encoded
            });

    let state = state
        .with_artifact_signing_secret(artifact_signing_secret)
        .with_run_lifecycle_service(Arc::new(wiring.run_lifecycle))
        .with_agent_profile_registry(Arc::clone(&wiring.profile_registry))
        .with_delegation_engine(Arc::clone(&wiring.delegation_engine))
        .with_team_store(Arc::clone(&wiring.team_store))
        .with_resource_governor(Arc::clone(&wiring.resource_governor))
        .with_auxiliary_pools(control_pool.into_iter().collect());
    let state = state.with_matrix_cloud_runtime(Some(matrix_rt));
    Ok(state)
}

/// Reserve at most four connections, or roughly five percent of a smaller
/// configured pool, for control-plane work. A pool below four connections is
/// left intact rather than creating a second pool that would exceed the
/// operator's explicit total.
fn control_pool_max_connections(total: u32) -> u32 {
    (total / 20).clamp(0, 4)
}

fn pool_settings_with_limit(
    base: &astra_core::MatrixOneSettings,
    max_connections: u32,
) -> astra_core::MatrixOneSettings {
    let mut settings = base.clone();
    settings.db_pool_max_connections = max_connections.max(1);
    settings.db_pool_min_connections = settings
        .db_pool_min_connections
        .min(settings.db_pool_max_connections);
    settings
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
    use super::{control_pool_max_connections, pool_settings_with_limit};
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

    #[test]
    fn control_pool_reservation_stays_inside_configured_total() {
        assert_eq!(control_pool_max_connections(1), 0);
        assert_eq!(control_pool_max_connections(19), 0);
        assert_eq!(control_pool_max_connections(20), 1);
        assert_eq!(control_pool_max_connections(80), 4);
        assert_eq!(control_pool_max_connections(u32::MAX), 4);
    }

    #[test]
    fn pool_settings_limit_never_leaves_min_above_max() {
        let mut base = astra_core::MatrixOneSettings::default();
        base.db_pool_min_connections = 10;
        let limited = pool_settings_with_limit(&base, 4);
        assert_eq!(limited.db_pool_max_connections, 4);
        assert_eq!(limited.db_pool_min_connections, 4);
    }
}
