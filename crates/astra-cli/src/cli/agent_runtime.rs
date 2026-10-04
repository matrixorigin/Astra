//! Multi-agent runtime initialization for the interactive REPL.

use crate::cli::session::session_state::SessionState;

fn attach_session_to_spawner(
    spawner: astra_runtime::orchestration::DynamicAgentSpawner,
    session_id: Option<&str>,
) -> astra_runtime::orchestration::DynamicAgentSpawner {
    match session_id {
        Some(session_id) => spawner.with_session(session_id.to_string()),
        None => spawner,
    }
}

pub(crate) fn bind_skill_resolver(
    unified_skill_registry: std::sync::Arc<astra_runtime::skills::UnifiedSkillRegistry>,
) -> Option<std::sync::Arc<dyn astra_runtime::turn::skill_tool::SkillResolver>> {
    // Resolver construction is a binding operation, not a discovery lifecycle.
    // In particular, the interactive TUI intentionally starts from a local
    // bootstrap registry and converges remote providers in a supervised
    // background task. Triggering `discover_all()` here would pull provider
    // retry/backoff into the first-frame critical path. The resolver keeps the
    // shared registry alive and observes skills added by later convergence.
    Some(std::sync::Arc::new(
        astra_runtime::skills::UnifiedSkillResolver::new(unified_skill_registry),
    ))
}

/// Install session-local recovery projections. Server admission owns child execution.
pub(crate) fn initialize_agent_projection(state: &mut SessionState) {
    let tracker =
        std::sync::Arc::new(astra_runtime::server::delegation::engine::DelegationTracker::new());
    let transport = std::sync::Arc::new(astra_messaging::InProcessTransport::new());
    let mailbox_router =
        std::sync::Arc::new(astra_messaging::AgentMailboxRouter::new(transport, tracker));
    state.agent_spawner = Some(std::sync::Arc::new(attach_session_to_spawner(
        astra_runtime::orchestration::DynamicAgentSpawner::new(mailbox_router)
            .with_active_work_registry(state.active_work_registry.clone()),
        state.session_id.as_deref(),
    )));
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use astra_runtime::skills::UnifiedSkillRegistry;
    use astra_skills::manifest::{LoadedSkill, SkillManifest, SkillSourceKind};
    use astra_skills::traits::{SkillError, SkillProvider};
    use async_trait::async_trait;

    use super::bind_skill_resolver;

    struct DeferredProvider {
        discoveries: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl SkillProvider for DeferredProvider {
        fn source_kind(&self) -> SkillSourceKind {
            SkillSourceKind::Database
        }

        async fn discover(&self) -> Result<Vec<SkillManifest>, SkillError> {
            self.discoveries.fetch_add(1, Ordering::SeqCst);
            Ok(vec![SkillManifest {
                name: "late_remote_skill".into(),
                description: "arrives after interactive startup".into(),
                source: SkillSourceKind::Database,
                user_invocable: true,
                ..Default::default()
            }])
        }

        async fn load(&self, name: &str) -> Result<LoadedSkill, SkillError> {
            Err(SkillError::NotFound(name.to_string()))
        }

        async fn refresh(&self) -> Result<(), SkillError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn resolver_binding_does_not_own_discovery_and_observes_later_convergence() {
        let discoveries = Arc::new(AtomicUsize::new(0));
        let mut registry = UnifiedSkillRegistry::new();
        registry.add_provider(Box::new(DeferredProvider {
            discoveries: discoveries.clone(),
        }));
        let registry = Arc::new(registry);

        let resolver = bind_skill_resolver(registry.clone()).expect("resolver is always available");
        assert_eq!(discoveries.load(Ordering::SeqCst), 0);
        assert!(resolver.available_skills().is_empty());

        registry
            .discover_all()
            .await
            .expect("explicit lifecycle convergence");

        assert_eq!(discoveries.load(Ordering::SeqCst), 1);
        assert_eq!(
            resolver
                .available_skills()
                .into_iter()
                .map(|skill| skill.name)
                .collect::<Vec<_>>(),
            vec!["late_remote_skill"]
        );
    }
}
