//! Multi-agent runtime initialization for the interactive REPL.

use super::{agent_loader, delegate_subrun, spawn_subrun};
use crate::SessionState;
use std::path::PathBuf;

fn attach_session_to_spawner(
    spawner: astra_runtime::orchestration::DynamicAgentSpawner,
    session_id: Option<&str>,
) -> astra_runtime::orchestration::DynamicAgentSpawner {
    match session_id {
        Some(session_id) => spawner.with_session(session_id.to_string()),
        None => spawner,
    }
}

/// Build a fully-wired [`DynamicAgentSpawner`] without mutating a
/// SessionState. Extracted from [`initialize_multi_agent_runtime`] so
/// the one-shot `chat -m` code path can wire dynamic sub-agent support
/// into a `BasicCliChatContext` without constructing a full REPL
/// state.
///
/// Applies the fork-prefix pipeline configuration from
/// `RuntimeConfig.fork_prefix` in the same way the REPL path does:
/// - syncs the process-global flag
/// - installs the configured sink (Noop/Stderr) when enabled
/// - always attaches an `InMemoryPrefixStore` (cheap; capture
///   no-ops when the flag is off)
///
/// Returns only the spawner + mailbox_router the caller needs to
/// hand to `AgentActionContext`. The delegation engine is NOT
/// created here — REPL builds its own via
/// `initialize_multi_agent_runtime` because the engine also wires
/// parent agent routing. one-shot chat needs only `agent(action='spawn', ...)`.
pub(crate) async fn build_one_shot_spawner(
    api: &astra_thin_client::ThinClient,
    token: String,
    unified_skill_registry: std::sync::Arc<astra_runtime::skills::UnifiedSkillRegistry>,
    perm_mode: super::permission_manager::PermissionMode,
    skill_search: astra_core::SkillSearchSettings,
    session_id: Option<String>,
    default_model: Option<String>,
) -> std::sync::Arc<astra_runtime::orchestration::DynamicAgentSpawner> {
    let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let skill_resolver = build_turn_skill_resolver(unified_skill_registry).await;

    let tracker =
        std::sync::Arc::new(astra_runtime::server::delegation::engine::DelegationTracker::new());
    let transport = std::sync::Arc::new(astra_messaging::InProcessTransport::new());
    let mailbox_router = std::sync::Arc::new(astra_messaging::AgentMailboxRouter::new(
        transport,
        tracker.clone(),
    ));
    let progress_broadcaster =
        std::sync::Arc::new(astra_runtime::orchestration::ProgressBroadcaster::default());

    let mut spawn_executor =
        spawn_subrun::CliSpawnAgentExecutor::new(api.clone(), token, project_root, perm_mode, None)
            .with_default_model(default_model)
            .with_skill_resolver(skill_resolver)
            .with_skill_search(skill_search);
    // One-shot `chat -m` uses the captured token only — no profile to
    // re-read from. The token-provider wiring lives on the REPL path
    // (`initialize_multi_agent_runtime`) where token rotation is real.
    if let Some(sid) = session_id.as_deref() {
        spawn_executor = spawn_executor.with_active_session_id(sid.to_string());
    }

    let runtime_cfg = astra_config::runtime_config::RuntimeConfig::load();
    let fork_cfg = &runtime_cfg.fork_prefix;
    if fork_cfg.enabled {
        let sink: std::sync::Arc<dyn astra_turn_core::fork_cache_event::ForkCacheEventSink> =
            match fork_cfg.sink {
                astra_config::runtime_config::ForkCacheSinkKind::Noop => {
                    std::sync::Arc::new(astra_turn_core::fork_cache_event::NoopForkCacheSink)
                }
                astra_config::runtime_config::ForkCacheSinkKind::Stderr => {
                    std::sync::Arc::new(astra_turn_core::fork_cache_event::StderrForkCacheSink)
                }
            };
        spawn_executor = spawn_executor.with_fork_cache_sink(sink);
    }

    let prefix_store: std::sync::Arc<dyn astra_turn_core::fork_prefix_store::PrefixCaptureSink> =
        std::sync::Arc::new(astra_turn_core::fork_prefix_store::InMemoryPrefixStore::new());

    std::sync::Arc::new(attach_session_to_spawner(
        astra_runtime::orchestration::DynamicAgentSpawner::with_broadcaster(
            mailbox_router,
            progress_broadcaster,
        )
        .with_executor(std::sync::Arc::new(spawn_executor))
        .with_prefix_store(prefix_store)
        .with_max_concurrent_agents(resolved_spawn_concurrency_cap()),
        session_id.as_deref(),
    ))
}

/// Resolve the user-facing cap on concurrent live subagents.
///
/// Default 10 — matches the rough Claude Code parity used elsewhere
/// (see workflow agent fan-out cap). A long-lived chat session that
/// keeps spawning replacement agents on every transient failure can
/// otherwise accumulate dozens of in-flight runs, all burning tokens.
/// Override via `ASTRA_MAX_CONCURRENT_AGENTS=N` for users who genuinely
/// need a different ceiling. Invalid values fall back to the default
/// rather than panicking — this is a UX limit, not a security one.
fn resolved_spawn_concurrency_cap() -> usize {
    const DEFAULT_CAP: usize = 10;
    match std::env::var("ASTRA_MAX_CONCURRENT_AGENTS") {
        Ok(raw) => match raw.trim().parse::<usize>() {
            Ok(n) if n > 0 => n,
            _ => {
                tracing::warn!(
                    target: "astra::spawner",
                    raw = %raw,
                    default = DEFAULT_CAP,
                    "ASTRA_MAX_CONCURRENT_AGENTS unparseable; using default"
                );
                DEFAULT_CAP
            }
        },
        Err(_) => DEFAULT_CAP,
    }
}

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
    state: &mut SessionState,
    api: &astra_thin_client::ThinClient,
    token: String,
    profile: Option<&str>,
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
        std::sync::Arc::new(astra_runtime::server::delegation::engine::DelegationTracker::new());
    let transport = std::sync::Arc::new(astra_messaging::InProcessTransport::new());
    let mailbox_router = std::sync::Arc::new(astra_messaging::AgentMailboxRouter::new(
        transport,
        tracker.clone(),
    ));

    let progress_broadcaster =
        std::sync::Arc::new(astra_runtime::orchestration::ProgressBroadcaster::default());

    // Read fork-prefix config once for both executors so the
    // delegate and spawn paths stay in lockstep on observability.
    let runtime_cfg_for_delegate = astra_config::runtime_config::RuntimeConfig::load();
    let fork_cfg_for_delegate = &runtime_cfg_for_delegate.fork_prefix;
    let delegate_fork_cache_sink: Option<
        std::sync::Arc<dyn astra_turn_core::fork_cache_event::ForkCacheEventSink>,
    > = if fork_cfg_for_delegate.enabled {
        Some(match fork_cfg_for_delegate.sink {
            astra_config::runtime_config::ForkCacheSinkKind::Noop => {
                std::sync::Arc::new(astra_turn_core::fork_cache_event::NoopForkCacheSink)
            }
            astra_config::runtime_config::ForkCacheSinkKind::Stderr => {
                std::sync::Arc::new(astra_turn_core::fork_cache_event::StderrForkCacheSink)
            }
        })
    } else {
        None
    };

    let mut delegate_executor = delegate_subrun::CliDelegateSubRunExecutor::new(
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
    if let Some(sink) = delegate_fork_cache_sink.clone() {
        delegate_executor = delegate_executor.with_fork_cache_sink(sink);
    }

    // Build the shared fork-prefix store once up-front so both the
    // spawner and the delegation engine hold the same Arc. Bug B
    // step 2: without shared state, a prefix captured on a parent
    // turn (recorded into the spawner's store) would be invisible
    // to the delegate path — defeating the purpose.
    let prefix_store: std::sync::Arc<dyn astra_turn_core::fork_prefix_store::PrefixCaptureSink> =
        std::sync::Arc::new(astra_turn_core::fork_prefix_store::InMemoryPrefixStore::new());

    let engine = astra_runtime::server::delegation::engine::DelegationEngine::with_executor(
        registry,
        std::sync::Arc::new(astra_runtime::server::run::engine::RunEngine::new(
            run_store,
        )),
        tracker,
        std::sync::Arc::new(delegate_executor),
    )
    .with_mailbox_router(mailbox_router.clone())
    .with_prefix_store(prefix_store.clone());
    state.delegation_engine = Some(std::sync::Arc::new(engine));

    let mut spawn_executor = spawn_subrun::CliSpawnAgentExecutor::new(
        api.clone(),
        token,
        project_root,
        state.perm_manager.mode(),
        None,
    )
    .with_default_model(state.model.clone())
    .with_skill_resolver(skill_resolver)
    .with_skill_search(state.skill_search.clone())
    .with_bg_task_commands(state.bg_task_commands.clone());
    // Wire a token provider so each spawn reads the freshest access
    // token at execution time. Without this, sub-agents fail with 401
    // in long-running sessions after the parent's auth refresh
    // rotates the token (session 82ff91e5 reproduced this — 4
    // sub-agents all returned "Could not validate credentials"
    // because the executor froze the token at startup).
    //
    // We can't borrow `state` into the closure (state lives on the
    // REPL stack and gets mutated each turn), but `current_access_token`
    // takes only `Option<&str>` for the profile and reads the
    // credentials file fresh on each call. So we capture an owned
    // profile string and the closure becomes Send + Sync + 'static.
    {
        let profile_owned = profile.map(str::to_string);
        let provider: spawn_subrun::TokenProvider = std::sync::Arc::new(move || {
            super::session_runtime::current_access_token(profile_owned.as_deref())
        });
        spawn_executor = spawn_executor.with_token_provider(provider);
    }
    if let Some(session_id) = state.session_id.clone() {
        spawn_executor = spawn_executor.with_active_session_id(session_id);
    }

    // Fork-prefix pipeline: driven entirely by RuntimeConfig (which
    // already layers defaults → user TOML → project TOML).
    // When the pipeline is on, the operator's chosen sink
    // (Noop / Stderr) is installed. When off, the sink is never
    // attached.
    //
    // Keep the prefix store attached unconditionally: cheap to own.
    let runtime_cfg = astra_config::runtime_config::RuntimeConfig::load();
    let fork_cfg = &runtime_cfg.fork_prefix;
    if fork_cfg.enabled {
        let sink: std::sync::Arc<dyn astra_turn_core::fork_cache_event::ForkCacheEventSink> =
            match fork_cfg.sink {
                astra_config::runtime_config::ForkCacheSinkKind::Noop => {
                    std::sync::Arc::new(astra_turn_core::fork_cache_event::NoopForkCacheSink)
                }
                astra_config::runtime_config::ForkCacheSinkKind::Stderr => {
                    std::sync::Arc::new(astra_turn_core::fork_cache_event::StderrForkCacheSink)
                }
            };
        spawn_executor = spawn_executor.with_fork_cache_sink(sink);
    }

    state.agent_spawner = Some(std::sync::Arc::new(attach_session_to_spawner(
        astra_runtime::orchestration::DynamicAgentSpawner::with_broadcaster(
            mailbox_router,
            progress_broadcaster,
        )
        .with_executor(std::sync::Arc::new(spawn_executor))
        .with_prefix_store(prefix_store)
        .with_max_concurrent_agents(resolved_spawn_concurrency_cap()),
        state.session_id.as_deref(),
    )));
}
