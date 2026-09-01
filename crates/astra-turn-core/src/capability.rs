//! Runtime capabilities for capability-driven tool surface resolution.
//!
//! Tool visibility is intentionally derived from metadata:
//!
//! ```text
//! visible(tool) = surface_admits(tool.scope, surface)
//!               && capabilities.has_all(tool.requires)
//! ```

use std::collections::BTreeSet;

/// Runtime service/capability required by one or more tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Capability {
    /// Sub-agent fan-out and result collection via `agent`.
    AgentSpawner,
    /// Memoria-backed cross-session memory via `memory`.
    MemoryService,
    /// MatrixOne/database operations via `mo`.
    Database,
    /// Skill registry/tool support.
    SkillsCatalog,
    /// GitHub auth / API access via `github`.
    GitHubAuth,
    /// Language-server-backed code intelligence via `lsp`.
    LSPServer,
    /// Server-owned plan lifecycle tools.
    PlanLifecycle,
    /// Local/edge background task registry and projection control.
    LocalBackgroundTasks,
    /// Persisted session reflection service used by `reflect`.
    ReflectService,
    /// Canonical Work planning bound to the current owner and session branch.
    WorkPlanning,
    /// Ability to establish canonical Work around the current durable run.
    WorkLifecycle,
}

impl Capability {
    pub fn as_str(self) -> &'static str {
        match self {
            Capability::AgentSpawner => "agent_spawner",
            Capability::MemoryService => "memory_service",
            Capability::Database => "database",
            Capability::SkillsCatalog => "skills_catalog",
            Capability::GitHubAuth => "github_auth",
            Capability::LSPServer => "lsp_server",
            Capability::PlanLifecycle => "plan_lifecycle",
            Capability::LocalBackgroundTasks => "local_background_tasks",
            Capability::ReflectService => "reflect_service",
            Capability::WorkPlanning => "work_planning",
            Capability::WorkLifecycle => "work_lifecycle",
        }
    }

    /// Whether this capability requires a runtime executor binding.
    ///
    /// An executor-gated capability cannot be satisfied by a service or static
    /// feature flag; the runtime must have an active executor handle (e.g., a
    /// spawn-context for AgentSpawner or an owner-scoped credential provider)
    /// for tools requiring this capability to pass admission.
    pub fn is_executor_gated(self) -> bool {
        matches!(
            self,
            Capability::AgentSpawner | Capability::GitHubAuth | Capability::LocalBackgroundTasks
        )
    }
}

/// Session-invariant set of runtime capabilities.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CapabilitySet {
    capabilities: BTreeSet<Capability>,
}

impl CapabilitySet {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn all() -> Self {
        Self::empty()
            .with(Capability::AgentSpawner)
            .with(Capability::MemoryService)
            .with(Capability::Database)
            .with(Capability::SkillsCatalog)
            .with(Capability::GitHubAuth)
            .with(Capability::LSPServer)
            .with(Capability::PlanLifecycle)
            .with(Capability::ReflectService)
            .with(Capability::WorkPlanning)
            .with(Capability::WorkLifecycle)
        // NOTE: LocalBackgroundTasks is intentionally excluded — it is an
        // edge-only capability (typed background tasks like bg-shell).
        // Server-side ToolEngine has no handlers for task_output / task_stop
        // / task_list; those tools only execute on edge agents.
    }

    pub fn with(mut self, capability: Capability) -> Self {
        self.capabilities.insert(capability);
        self
    }

    pub fn with_if(self, condition: bool, capability: Capability) -> Self {
        if condition {
            self.with(capability)
        } else {
            self
        }
    }

    pub fn without(mut self, capability: Capability) -> Self {
        self.capabilities.remove(&capability);
        self
    }

    pub fn has(&self, capability: Capability) -> bool {
        self.capabilities.contains(&capability)
    }

    pub fn has_all(&self, capabilities: &[Capability]) -> bool {
        capabilities.iter().all(|capability| self.has(*capability))
    }

    pub fn active(&self) -> impl Iterator<Item = Capability> + '_ {
        self.capabilities.iter().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_has_no_capabilities() {
        let caps = CapabilitySet::empty();
        assert!(!caps.has(Capability::AgentSpawner));
        assert!(!caps.has_all(&[Capability::AgentSpawner]));
        assert!(caps.has_all(&[]));
    }

    #[test]
    fn builder_methods_are_chainable() {
        let caps = CapabilitySet::empty()
            .with(Capability::Database)
            .with_if(false, Capability::AgentSpawner)
            .with_if(true, Capability::MemoryService);

        assert!(caps.has(Capability::Database));
        assert!(caps.has(Capability::MemoryService));
        assert!(!caps.has(Capability::AgentSpawner));
    }

    #[test]
    fn all_contains_every_server_side_capability() {
        let caps = CapabilitySet::all();
        for capability in [
            Capability::AgentSpawner,
            Capability::MemoryService,
            Capability::Database,
            Capability::SkillsCatalog,
            Capability::GitHubAuth,
            Capability::LSPServer,
            Capability::PlanLifecycle,
            Capability::ReflectService,
            Capability::WorkPlanning,
        ] {
            assert!(caps.has(capability), "missing {capability:?}");
        }
    }
}
