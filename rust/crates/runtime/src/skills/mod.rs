//! Skill framework — trait-based, extensible skill system.
//!
//! # Architecture
//!
//! The skill framework is organized into layers:
//!
//! - **Core types** ([`manifest`], [`traits`], [`version`]): Universal skill descriptors,
//!   trait definitions, and semantic versioning.
//! - **Loading** ([`loader`]): SKILL.md parser for YAML frontmatter + Markdown body.
//! - **Activation** ([`activation`]): Conditional skill activation via path globs
//!   and keyword trigger detection.
//! - **Arguments** ([`arguments`]): Argument substitution (`$ARGUMENTS`, `${SKILL_DIR}`).
//! - **Hooks** ([`hooks`]): Pre/post invocation lifecycle hooks.
//! - **Execution** ([`executor`]): Inline and isolated (sub-agent) execution modes.
//! - **Registry** ([`registry`]): Unified skill registry aggregating multiple providers.
//! - **Handlers** ([`handlers`]): HTTP handlers for the skill REST API.
//!
//! # Extensibility
//!
//! New skill sources implement [`traits::SkillProvider`].
//! New execution modes implement [`traits::SkillExecutor`].
//! The registry and resolver are trait-based to allow composition.
//!
//! # Backward Compatibility
//!
//! The existing `turn::skill_tool::SkillResolver` trait is bridged via
//! [`registry::LegacySkillResolverAdapter`].

pub mod activation;
pub mod arguments;
pub mod composition;
pub mod executor;
pub mod handlers;
pub mod hooks;
pub mod loader;
pub mod manifest;
pub mod providers;
pub mod quality;
pub mod registry;
pub mod traits;
pub mod verify;
pub mod version;
pub mod watcher;

// Re-export HTTP handlers at the module root for backward compatibility
// with `crate::skills::register_skill_handler` etc. used in router_builder.rs.
pub use handlers::*;

// Re-export key framework types for convenience.
pub use manifest::{ExecutionContext, LoadedSkill, SkillManifest, SkillSourceKind};
pub use registry::{SharedSkillRegistry, UnifiedSkillRegistry, UnifiedSkillResolver};
pub use traits::{SkillError, SkillExecutor, SkillProvider, SkillResolver};
pub use version::{Dependency, DependencyResolver, Version, VersionConstraint};
pub use providers::{BundledSkillProvider, DatabaseSkillProvider, LocalSkillProvider, McpSkillProvider};
pub use quality::{SkillQualityTracker, SkillQualityEntry, SkillOutcome};
pub use verify::SkillVerifier;
pub use composition::{CompositionContext, CompositionError};

/// Returns a shared reference to a static empty `UnifiedSkillRegistry`.
/// Useful in tests and server contexts where no local skill providers apply.
pub fn empty_unified_registry() -> &'static std::sync::Arc<UnifiedSkillRegistry> {
    use std::sync::OnceLock;
    static EMPTY: OnceLock<std::sync::Arc<UnifiedSkillRegistry>> = OnceLock::new();
    EMPTY.get_or_init(|| std::sync::Arc::new(UnifiedSkillRegistry::new()))
}

/// Returns a shared reference to a default `UnifiedSkillRegistry` populated
/// with Local and Bundled providers. Eagerly discovers skills on first call.
///
/// Use this for all CLI entry points (one-shot message, exec, /review, etc.)
/// so they see the same skills as the interactive REPL.
pub fn default_unified_registry() -> &'static std::sync::Arc<UnifiedSkillRegistry> {
    use std::sync::OnceLock;
    static DEFAULT: OnceLock<std::sync::Arc<UnifiedSkillRegistry>> = OnceLock::new();
    DEFAULT.get_or_init(|| {
        let mut registry = UnifiedSkillRegistry::new();
        registry.add_provider(Box::new(LocalSkillProvider::standard()));
        registry.add_provider(Box::new(BundledSkillProvider::with_defaults()));
        let registry = std::sync::Arc::new(registry);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let r = registry.clone();
            let _ = std::thread::scope(|s| {
                s.spawn(|| handle.block_on(r.discover_all()))
                    .join()
                    .ok()
            });
        }
        registry
    })
}

/// Detect inline shell command lines (`! ...`) in skill instructions.
///
/// Returns `true` if any line, after stripping leading whitespace, starts
/// with `!` followed by any whitespace character (space, tab, NBSP, etc.)
/// or is exactly `!`. Used to sandbox MCP skills.
pub fn has_inline_shell(instructions: &str) -> bool {
    instructions.lines().any(|line| {
        let t = line.trim_start();
        if t == "!" {
            return true;
        }
        if let Some(rest) = t.strip_prefix('!') {
            // Any whitespace after `!` indicates a shell command.
            rest.starts_with(char::is_whitespace)
        } else {
            false
        }
    })
}
