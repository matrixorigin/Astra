//! Multi-agent runtime initialization for the interactive REPL.

use super::{ReplState, agent_loader, delegate_subrun, spawn_subrun};
use std::path::PathBuf;

async fn build_turn_skill_resolver(
    unified_skill_registry: std::sync::Arc<astra_runtime::skills::UnifiedSkillRegistry>,
) -> Option<std::sync::Arc<dyn astra_runtime::turn::skill_tool::SkillResolver>> {
    if unified_skill_registry.is_empty() {
        let _ = unified_skill_registry.discover_all().await;
    }

    let resolver = std::sync::Arc::new(astra_runtime::skills::UnifiedSkillResolver::new(
        unified_skill_registry,
    ));
    let skills = astra_runtime::turn::skill_tool::SkillResolver::available_skills(&*resolver);
    if skills.is_empty() {
        None
    } else {
        Some(resolver as std::sync::Arc<dyn astra_runtime::turn::skill_tool::SkillResolver>)
    }
}

pub(crate) async fn initialize_multi_agent_runtime(
    state: &mut ReplState,
    api: &astra_thin_client::ThinClient,
    token: String,
) {
    let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let skill_resolver = build_turn_skill_resolver(state.unified_skill_registry.clone()).await;

    let mut registry = astra_services::AgentProfileRegistry::new();
    delegate_subrun::register_default_agents(&mut registry);
    let custom_count = agent_loader::load_and_merge(&project_root, &mut registry);
    if custom_count > 0 {
        eprintln!("  loaded {custom_count} custom agent(s) from .astra/agents/");
    }
    let registry = std::sync::Arc::new(tokio::sync::RwLock::new(registry));

    let run_store = std::sync::Arc::new(astra_services::runs::InMemoryRunStateStore::default());
    let tracker =
        std::sync::Arc::new(astra_runtime::server::delegation_engine::DelegationTracker::new());
    let transport = std::sync::Arc::new(astra_messaging::InProcessTransport::new());
    let mailbox_router = std::sync::Arc::new(astra_messaging::AgentMailboxRouter::new(
        transport,
        tracker.clone(),
    ));

    let progress_broadcaster =
        std::sync::Arc::new(astra_runtime::orchestration::ProgressBroadcaster::default());

    let delegate_executor = delegate_subrun::CliDelegateSubRunExecutor::new(
        api.clone(),
        token.clone(),
        state.model.clone(),
        project_root.clone(),
        state.perm_manager.mode(),
        None,
    )
    .with_skill_resolver(skill_resolver.clone())
    .with_skill_search(state.skill_search.clone())
    .with_progress_broadcaster(progress_broadcaster.clone());

    let engine = astra_runtime::server::delegation_engine::DelegationEngine::with_executor(
        registry,
        std::sync::Arc::new(astra_runtime::server::run_engine::RunEngine::new(run_store)),
        tracker,
        std::sync::Arc::new(delegate_executor),
    )
    .with_mailbox_router(mailbox_router.clone());
    state.delegation_engine = Some(std::sync::Arc::new(engine));

    let mut spawn_executor = spawn_subrun::CliSpawnAgentExecutor::new(
        api.clone(),
        token,
        project_root,
        state.perm_manager.mode(),
        None,
    )
    .with_skill_resolver(skill_resolver)
    .with_skill_search(state.skill_search.clone());
    if let Some(session_id) = state.session_id.clone() {
        spawn_executor = spawn_executor.with_active_session_id(session_id);
    }

    // Fork-prefix pipeline: driven entirely by RuntimeConfig (which
    // already layers defaults → user TOML → project TOML → env
    // override `ASTRA_FORK_INHERIT_PREFIX`). No separate observability
    // flag — when the pipeline is on, the operator's chosen sink
    // (Noop / Stderr) is installed. When off, the sink is never
    // attached and the capture helper early-returns on every call.
    //
    // Keep the prefix store attached unconditionally: cheap to own,
    // and the capture helper's flag check is the real gate — storing
    // is a no-op when flag is off, so there's no behavior change for
    // operators who haven't opted in.
    let runtime_cfg = astra_config::runtime_config::RuntimeConfig::load();
    let fork_cfg = &runtime_cfg.fork_prefix;
    // Sync the process-global flag turn-core reads on the hot path
    // with the config value. Must happen before any turn runs, and
    // before any capture attempt on this process — so the startup
    // path is the right place to call it exactly once.
    astra_turn_core::fork_capture::set_fork_inherit_prefix_enabled(fork_cfg.enabled);
    if fork_cfg.enabled {
        let sink: std::sync::Arc<
            dyn astra_turn_core::fork_cache_event::ForkCacheEventSink,
        > = match fork_cfg.sink {
            astra_config::runtime_config::ForkCacheSinkKind::Noop => {
                std::sync::Arc::new(astra_turn_core::fork_cache_event::NoopForkCacheSink)
            }
            astra_config::runtime_config::ForkCacheSinkKind::Stderr => {
                std::sync::Arc::new(astra_turn_core::fork_cache_event::StderrForkCacheSink)
            }
        };
        spawn_executor = spawn_executor.with_fork_cache_sink(sink);
    }

    let prefix_store: std::sync::Arc<
        dyn astra_turn_core::fork_prefix_store::PrefixCaptureSink,
    > = std::sync::Arc::new(
        astra_turn_core::fork_prefix_store::InMemoryPrefixStore::new(),
    );

    state.agent_spawner = Some(std::sync::Arc::new(
        astra_runtime::orchestration::DynamicAgentSpawner::with_broadcaster(
            mailbox_router,
            progress_broadcaster,
        )
        .with_executor(std::sync::Arc::new(spawn_executor))
        .with_prefix_store(prefix_store),
    ));
}
