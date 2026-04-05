//! Unified skill registry — aggregates skills from multiple providers.
//!
//! Skills are merged from all providers with priority-ordered resolution:
//! local > bundled > database > mcp > plugin.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use super::activation::ConditionalSkillTracker;
use super::manifest::{LoadedSkill, SkillManifest, SkillSourceKind};
use super::providers::mcp::McpSkillProvider;
use super::traits::{ResolvedSkill, SkillError, SkillProvider, SkillToolInfo};

// ── Cached skill entry ───────────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct CachedSkill {
    manifest: SkillManifest,
    loaded: Option<LoadedSkill>,
}

// ── UnifiedSkillRegistry ─────────────────────────────────────────────────────

/// Aggregates skills from multiple providers with priority-ordered resolution.
pub struct UnifiedSkillRegistry {
    providers: Vec<Box<dyn SkillProvider>>,
    cache: RwLock<HashMap<String, CachedSkill>>,
    conditional_skills: RwLock<Vec<SkillManifest>>,
    conditional_tracker: RwLock<ConditionalSkillTracker>,
    /// Token budget for metadata.
    metadata_budget: u32,
    /// Shared MCP provider for dynamic skill registration from MCP connections.
    mcp_provider: Arc<McpSkillProvider>,
}

impl UnifiedSkillRegistry {
    pub fn new() -> Self {
        let mcp = Arc::new(McpSkillProvider::new());
        Self {
            providers: Vec::new(),
            cache: RwLock::new(HashMap::new()),
            conditional_skills: RwLock::new(Vec::new()),
            conditional_tracker: RwLock::new(ConditionalSkillTracker::new()),
            metadata_budget: 10_000,
            mcp_provider: mcp,
        }
    }

    pub fn with_budget(mut self, budget: u32) -> Self {
        self.metadata_budget = budget;
        self
    }

    /// Add a skill provider.
    pub fn add_provider(&mut self, provider: Box<dyn SkillProvider>) {
        self.providers.push(provider);
    }

    /// Priority order for source kinds (lower = higher priority).
    fn source_priority(kind: &SkillSourceKind) -> u8 {
        match kind {
            SkillSourceKind::Local => 0,
            SkillSourceKind::Bundled => 1,
            SkillSourceKind::Database => 2,
            SkillSourceKind::Mcp => 3,
            SkillSourceKind::Plugin => 4,
        }
    }

    /// Discover and cache skills from all providers (including MCP).
    ///
    /// Clears existing cache and conditional state before re-populating so that
    /// deleted/renamed skills don't linger across refreshes.
    pub async fn discover_all(&self) -> Result<Vec<String>, SkillError> {
        let mut all_manifests: Vec<SkillManifest> = Vec::new();

        for provider in &self.providers {
            match provider.discover().await {
                Ok(manifests) => all_manifests.extend(manifests),
                Err(e) => {
                    eprintln!(
                        "  ⚠ Failed to discover skills from {:?}: {}",
                        provider.source_kind(),
                        e
                    );
                }
            }
        }

        // Include MCP skills from the shared provider.
        match self.mcp_provider.discover().await {
            Ok(manifests) => all_manifests.extend(manifests),
            Err(e) => {
                eprintln!("  ⚠ Failed to discover MCP skills: {e}");
            }
        }

        // Sort by source priority so higher-priority sources win on name collisions
        all_manifests.sort_by_key(|m| Self::source_priority(&m.source));

        let mut cache = self
            .cache
            .write()
            .map_err(|e| SkillError::Internal(format!("cache lock poisoned: {e}")))?;
        let mut conditional = self
            .conditional_skills
            .write()
            .map_err(|e| SkillError::Internal(format!("conditional lock poisoned: {e}")))?;

        // Clear stale state before re-populating.
        cache.clear();
        conditional.clear();

        // Reset conditional activation tracker so deleted/renamed skills
        // don't linger as "activated" after re-discovery.
        if let Ok(mut tracker) = self.conditional_tracker.write() {
            tracker.reset();
        }

        let mut registered = Vec::new();
        let mut total_tokens: u32 = 0;
        // Track alias→owner for collision detection.
        let mut alias_owners: HashMap<String, String> = HashMap::new();

        for manifest in all_manifests {
            let mut manifest = manifest;
            let tokens = manifest.metadata_tokens();
            if total_tokens + tokens > self.metadata_budget {
                eprintln!(
                    "  ⚠ Metadata budget exceeded, skipping skill '{}'",
                    manifest.name
                );
                continue;
            }

            // First occurrence wins (higher priority sources sorted first)
            if cache.contains_key(&manifest.name) {
                continue;
            }

            // Check for alias collisions with other skills' names or aliases.
            // Remove conflicting aliases so they can't be resolved.
            let mut clean_aliases = Vec::new();
            for alias in &manifest.aliases {
                if cache.contains_key(alias) {
                    eprintln!(
                        "  ⚠ Skill '{}': alias '{}' conflicts with existing skill name — ignored",
                        manifest.name, alias
                    );
                } else if let Some(owner) = alias_owners.get(alias) {
                    eprintln!(
                        "  ⚠ Skill '{}': alias '{}' conflicts with skill '{}' — ignored",
                        manifest.name, alias, owner
                    );
                } else {
                    alias_owners.insert(alias.clone(), manifest.name.clone());
                    clean_aliases.push(alias.clone());
                }
            }
            manifest.aliases = clean_aliases;

            if manifest.is_conditional() {
                conditional.push(manifest.clone());
            }

            registered.push(manifest.name.clone());
            total_tokens += tokens;
            cache.insert(
                manifest.name.clone(),
                CachedSkill {
                    manifest,
                    loaded: None,
                },
            );
        }

        Ok(registered)
    }

    /// Load a skill's full content by name.
    pub async fn load(&self, name: &str) -> Result<LoadedSkill, SkillError> {
        // Check if already loaded in cache
        {
            let cache = self
                .cache
                .read()
                .map_err(|e| SkillError::Internal(format!("cache lock poisoned: {e}")))?;
            if let Some(entry) = cache.get(name) {
                if let Some(ref loaded) = entry.loaded {
                    return Ok(loaded.clone());
                }
            }
        }

        // Try each provider, then the shared MCP provider.
        let all_sources: Vec<&dyn SkillProvider> = self
            .providers
            .iter()
            .map(|p| p.as_ref())
            .chain(std::iter::once(
                self.mcp_provider.as_ref() as &dyn SkillProvider
            ))
            .collect();

        for provider in all_sources {
            match provider.load(name).await {
                Ok(loaded) => {
                    let mut cache = self
                        .cache
                        .write()
                        .map_err(|e| SkillError::Internal(format!("cache lock poisoned: {e}")))?;
                    cache.insert(
                        name.to_string(),
                        CachedSkill {
                            manifest: loaded.manifest.clone(),
                            loaded: Some(loaded.clone()),
                        },
                    );
                    return Ok(loaded);
                }
                Err(SkillError::NotFound(_)) => continue,
                Err(e) => return Err(e),
            }
        }

        Err(SkillError::NotFound(format!("unknown skill: {name}")))
    }

    /// Get the manifest for a skill (metadata only, no instructions).
    pub fn get_manifest(&self, name: &str) -> Option<SkillManifest> {
        self.cache
            .read()
            .ok()?
            .get(name)
            .map(|e| e.manifest.clone())
    }

    /// Get the fully loaded skill (manifest + instructions + resources).
    pub fn get_loaded_skill(&self, name: &str) -> Option<LoadedSkill> {
        self.cache
            .read()
            .ok()?
            .get(name)
            .and_then(|e| e.loaded.clone())
    }

    /// List all available skill manifests.
    pub fn all_manifests(&self) -> Vec<SkillManifest> {
        self.cache
            .read()
            .map(|c| c.values().map(|e| e.manifest.clone()).collect())
            .unwrap_or_default()
    }

    /// List all skill names.
    pub fn skill_names(&self) -> Vec<String> {
        self.cache
            .read()
            .map(|c| c.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Number of registered skills.
    pub fn len(&self) -> usize {
        self.cache.read().map(|c| c.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Record a file path for conditional skill activation.
    /// Returns newly activated skill names.
    pub fn record_file_path(&self, file_path: &str) -> Vec<String> {
        let conditional = match self.conditional_skills.read() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("  ⚠ conditional_skills lock poisoned: {e}");
                return Vec::new();
            }
        };
        if conditional.is_empty() {
            return Vec::new();
        }

        let mut tracker = match self.conditional_tracker.write() {
            Ok(t) => t,
            Err(e) => {
                eprintln!("  ⚠ conditional_tracker lock poisoned: {e}");
                return Vec::new();
            }
        };

        tracker.record_path(file_path, &conditional)
    }

    /// Check if a conditional skill has been activated.
    pub fn is_skill_activated(&self, name: &str) -> bool {
        // Unconditional skills are always "activated"
        let cache = match self.cache.read() {
            Ok(c) => c,
            Err(_) => return false,
        };
        if let Some(entry) = cache.get(name) {
            if !entry.manifest.is_conditional() {
                return true;
            }
        }

        let tracker = match self.conditional_tracker.read() {
            Ok(t) => t,
            Err(_) => return false,
        };
        tracker.is_activated(name)
    }

    // ── MCP skill lifecycle ──────────────────────────────────────────────

    /// Register a skill from an MCP server connection.
    ///
    /// Call this when an MCP server advertises skill resources. Triggers
    /// re-discovery so the new skill appears in the registry cache.
    pub async fn register_mcp_skill(
        &self,
        server_name: &str,
        skill_md_content: &str,
    ) -> Result<String, SkillError> {
        let name = self
            .mcp_provider
            .register_mcp_skill(server_name, skill_md_content)?;
        self.discover_all().await?;
        Ok(name)
    }

    /// Remove all skills belonging to a disconnected MCP server.
    ///
    /// Triggers re-discovery so removed skills are purged from the cache.
    pub async fn remove_mcp_server_skills(&self, server_name: &str) -> Result<(), SkillError> {
        self.mcp_provider.remove_server_skills(server_name);
        self.discover_all().await?;
        Ok(())
    }

    /// Get a reference to the shared MCP provider (for direct inspection).
    pub fn mcp_provider(&self) -> &Arc<McpSkillProvider> {
        &self.mcp_provider
    }

    /// Refresh all providers — clears stale state and re-discovers.
    pub async fn refresh(&self) -> Result<(), SkillError> {
        for provider in &self.providers {
            provider.refresh().await?;
        }
        // discover_all already clears cache, conditional_skills, and tracker.
        self.discover_all().await?;
        Ok(())
    }
}

impl Default for UnifiedSkillRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ── UnifiedSkillResolver ─────────────────────────────────────────────────────

/// Adapter that implements the `turn::skill_tool::SkillResolver` trait
/// using the `UnifiedSkillRegistry`.
///
/// This bridges the new framework with the existing agentic loop.
pub struct UnifiedSkillResolver {
    registry: Arc<UnifiedSkillRegistry>,
}

impl UnifiedSkillResolver {
    pub fn new(registry: Arc<UnifiedSkillRegistry>) -> Self {
        Self { registry }
    }

    fn loaded_to_resolved(loaded: &LoadedSkill) -> ResolvedSkill {
        ResolvedSkill {
            name: loaded.manifest.name.clone(),
            instructions: loaded.instructions.clone(),
            model: loaded.manifest.model.clone(),
            max_tokens: loaded.manifest.max_tokens,
            allowed_tools: loaded.manifest.allowed_tools.clone(),
            execution_context: loaded.manifest.execution_context.clone(),
            hooks: loaded.manifest.hooks.clone().unwrap_or_default(),
            skill_dir: loaded.skill_dir.as_ref().map(|p| p.display().to_string()),
            source: loaded.manifest.source.clone(),
            success_criteria: loaded.manifest.success_criteria.clone(),
            composition: loaded.manifest.composition.clone(),
            input_schema: loaded.manifest.input_schema.clone(),
            aliases: loaded.manifest.aliases.clone(),
            effort: loaded.manifest.effort.clone(),
            agent_type: loaded.manifest.agent_type.clone(),
        }
    }
}

impl super::traits::SkillResolver for UnifiedSkillResolver {
    fn resolve(&self, name: &str) -> Result<ResolvedSkill, SkillError> {
        let registry = self.registry.clone();
        let name = name.to_string();

        // Try cache first (synchronous — no runtime needed)
        let mut canonical_name = name.clone();
        if let Ok(cache) = registry.cache.read() {
            // Direct name match
            if let Some(entry) = cache.get(&name) {
                if let Some(ref loaded) = entry.loaded {
                    return Ok(Self::loaded_to_resolved(loaded));
                }
            }
            // Alias match — check all cached entries
            for entry in cache.values() {
                if entry.manifest.aliases.iter().any(|a| a == &name) {
                    if let Some(ref loaded) = entry.loaded {
                        return Ok(Self::loaded_to_resolved(loaded));
                    }
                    // Found the canonical name for this alias — use it for provider load
                    canonical_name = entry.manifest.name.clone();
                }
            }
        }

        // Cache miss — load from providers using canonical name (not alias).
        let handle = tokio::runtime::Handle::current();
        let result = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    handle.block_on(async {
                        for provider in &registry.providers {
                            match provider.load(&canonical_name).await {
                                Ok(loaded) => {
                                    if let Ok(mut cache) = registry.cache.write() {
                                        cache.insert(
                                            canonical_name.clone(),
                                            CachedSkill {
                                                manifest: loaded.manifest.clone(),
                                                loaded: Some(loaded.clone()),
                                            },
                                        );
                                    }
                                    return Ok(loaded);
                                }
                                Err(SkillError::NotFound(_)) => continue,
                                Err(e) => return Err(e),
                            }
                        }
                        Err(SkillError::NotFound(format!(
                            "unknown skill: {canonical_name}"
                        )))
                    })
                })
                .join()
                .map_err(|_| SkillError::Internal("provider load thread panicked".into()))?
        });

        result.map(|loaded| Self::loaded_to_resolved(&loaded))
    }

    fn available_skills(&self) -> Vec<SkillToolInfo> {
        self.registry
            .all_manifests()
            .into_iter()
            .filter(|m| m.user_invocable)
            .filter(|m| {
                // Exclude conditional skills that haven't been activated
                if m.is_conditional() {
                    self.registry.is_skill_activated(&m.name)
                } else {
                    true
                }
            })
            .map(|m| SkillToolInfo {
                name: m.name,
                description: m.description,
                when_to_use: m.when_to_use,
                source: m.source,
                aliases: m.aliases,
            })
            .collect()
    }
}

/// Adapter that implements `turn::skill_tool::SkillResolver` (the original trait)
/// by delegating to a `traits::SkillResolver`.
pub struct LegacySkillResolverAdapter {
    inner: Arc<dyn super::traits::SkillResolver>,
}

impl LegacySkillResolverAdapter {
    pub fn new(inner: Arc<dyn super::traits::SkillResolver>) -> Self {
        Self { inner }
    }
}

impl crate::turn::skill_tool::SkillResolver for LegacySkillResolverAdapter {
    fn resolve(&self, name: &str) -> Result<crate::turn::skill_tool::ResolvedSkill, String> {
        match self.inner.resolve(name) {
            Ok(resolved) => Ok(crate::turn::skill_tool::ResolvedSkill {
                name: resolved.name,
                instructions: resolved.instructions,
                model: resolved.model,
                max_tokens: resolved.max_tokens,
                allowed_tools: resolved.allowed_tools,
                execution_context: resolved.execution_context,
                hooks: resolved.hooks,
                skill_dir: resolved.skill_dir,
                source: resolved.source,
                success_criteria: resolved.success_criteria,
                composition: resolved.composition,
                input_schema: resolved.input_schema,
                aliases: resolved.aliases,
                effort: resolved.effort,
                agent_type: resolved.agent_type,
            }),
            Err(e) => Err(e.to_string()),
        }
    }

    fn available_skills(&self) -> Vec<crate::turn::skill_tool::SkillToolInfo> {
        self.inner
            .available_skills()
            .into_iter()
            .map(|s| crate::turn::skill_tool::SkillToolInfo {
                name: s.name,
                description: s.description,
                when_to_use: s.when_to_use,
                source: s.source,
                aliases: s.aliases,
            })
            .collect()
    }
}

/// Thread-safe shared registry.
pub type SharedSkillRegistry = Arc<UnifiedSkillRegistry>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::traits::SkillResolver as _;
    use async_trait::async_trait;

    struct StubProvider {
        skills: Vec<(SkillManifest, String)>,
    }

    #[async_trait]
    impl SkillProvider for StubProvider {
        fn source_kind(&self) -> SkillSourceKind {
            SkillSourceKind::Bundled
        }

        async fn discover(&self) -> Result<Vec<SkillManifest>, SkillError> {
            Ok(self.skills.iter().map(|(m, _)| m.clone()).collect())
        }

        async fn load(&self, name: &str) -> Result<LoadedSkill, SkillError> {
            self.skills
                .iter()
                .find(|(m, _)| m.name == name)
                .map(|(m, instr)| LoadedSkill {
                    manifest: m.clone(),
                    instructions: instr.clone(),
                    instruction_tokens: (instr.len() as u32) / 4,
                    resources: None,
                    skill_dir: None,
                })
                .ok_or_else(|| SkillError::NotFound(format!("not found: {name}")))
        }

        async fn refresh(&self) -> Result<(), SkillError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn registry_discover_and_load() {
        let mut registry = UnifiedSkillRegistry::new();
        registry.add_provider(Box::new(StubProvider {
            skills: vec![(
                SkillManifest {
                    name: "test-skill".into(),
                    description: "A test skill".into(),
                    source: SkillSourceKind::Bundled,
                    ..Default::default()
                },
                "Do the thing.".into(),
            )],
        }));

        let registered = registry.discover_all().await.unwrap();
        assert_eq!(registered, vec!["test-skill"]);
        assert_eq!(registry.len(), 1);

        let loaded = registry.load("test-skill").await.unwrap();
        assert_eq!(loaded.instructions, "Do the thing.");
    }

    #[tokio::test]
    async fn registry_priority_resolution() {
        let mut registry = UnifiedSkillRegistry::new();

        // Local provider (higher priority)
        registry.add_provider(Box::new(StubProvider {
            skills: vec![(
                SkillManifest {
                    name: "shared".into(),
                    description: "Local version".into(),
                    source: SkillSourceKind::Local,
                    ..Default::default()
                },
                "Local instructions.".into(),
            )],
        }));

        // Database provider (lower priority)
        registry.add_provider(Box::new(StubProvider {
            skills: vec![(
                SkillManifest {
                    name: "shared".into(),
                    description: "DB version".into(),
                    source: SkillSourceKind::Database,
                    ..Default::default()
                },
                "DB instructions.".into(),
            )],
        }));

        registry.discover_all().await.unwrap();

        // Local version should win
        let manifest = registry.get_manifest("shared").unwrap();
        assert_eq!(manifest.description, "Local version");
    }

    #[tokio::test]
    async fn registry_conditional_skills() {
        let mut registry = UnifiedSkillRegistry::new();
        registry.add_provider(Box::new(StubProvider {
            skills: vec![(
                SkillManifest {
                    name: "rust-lint".into(),
                    description: "Rust linter".into(),
                    paths: vec!["src/**/*.rs".into()],
                    source: SkillSourceKind::Local,
                    ..Default::default()
                },
                "Lint Rust code.".into(),
            )],
        }));

        registry.discover_all().await.unwrap();

        // Not activated yet
        assert!(!registry.is_skill_activated("rust-lint"));

        // Activate by touching a matching file
        let activated = registry.record_file_path("src/main.rs");
        assert_eq!(activated, vec!["rust-lint"]);
        assert!(registry.is_skill_activated("rust-lint"));
    }

    #[tokio::test]
    async fn resolver_adapter_works() {
        let mut registry = UnifiedSkillRegistry::new();
        registry.add_provider(Box::new(StubProvider {
            skills: vec![(
                SkillManifest {
                    name: "test".into(),
                    description: "Test skill".into(),
                    source: SkillSourceKind::Bundled,
                    ..Default::default()
                },
                "Instructions here.".into(),
            )],
        }));
        registry.discover_all().await.unwrap();
        // Pre-load so the synchronous resolver can find it
        registry.load("test").await.unwrap();

        let resolver = UnifiedSkillResolver::new(Arc::new(registry));
        let skills = resolver.available_skills();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "test");

        let resolved = resolver.resolve("test").unwrap();
        assert_eq!(resolved.instructions, "Instructions here.");
    }

    #[tokio::test]
    async fn resolver_loads_uncached_skill_from_provider() {
        let mut registry = UnifiedSkillRegistry::new();
        registry.add_provider(Box::new(StubProvider {
            skills: vec![(
                SkillManifest {
                    name: "uncached".into(),
                    description: "Not pre-loaded".into(),
                    source: SkillSourceKind::Bundled,
                    ..Default::default()
                },
                "Lazy instructions.".into(),
            )],
        }));
        // Discover (populates manifest cache) but do NOT call load().
        // This means the loaded field is None — the resolver must call
        // provider.load() via block_in_place + block_on.
        registry.discover_all().await.unwrap();

        let resolver = UnifiedSkillResolver::new(Arc::new(registry));
        let resolved = resolver.resolve("uncached").unwrap();
        assert_eq!(resolved.instructions, "Lazy instructions.");
    }

    /// Provider whose load() uses a genuine async yield point.
    /// Before the fix, now_or_never() would return None and skip this provider.
    struct AsyncYieldProvider {
        skills: Vec<(SkillManifest, String)>,
    }

    #[async_trait]
    impl SkillProvider for AsyncYieldProvider {
        fn source_kind(&self) -> SkillSourceKind {
            SkillSourceKind::Database
        }
        async fn discover(&self) -> Result<Vec<SkillManifest>, SkillError> {
            Ok(self.skills.iter().map(|(m, _)| m.clone()).collect())
        }
        async fn load(&self, name: &str) -> Result<LoadedSkill, SkillError> {
            // Genuine async yield — this would cause now_or_never() to return None
            tokio::task::yield_now().await;
            self.skills
                .iter()
                .find(|(m, _)| m.name == name)
                .map(|(m, instr)| LoadedSkill {
                    manifest: m.clone(),
                    instructions: instr.clone(),
                    instruction_tokens: (instr.len() as u32) / 4,
                    resources: None,
                    skill_dir: None,
                })
                .ok_or_else(|| SkillError::NotFound(format!("not found: {name}")))
        }
        async fn refresh(&self) -> Result<(), SkillError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn resolver_handles_async_provider_load() {
        let mut registry = UnifiedSkillRegistry::new();
        registry.add_provider(Box::new(AsyncYieldProvider {
            skills: vec![(
                SkillManifest {
                    name: "async-skill".into(),
                    description: "Loaded via async yield".into(),
                    source: SkillSourceKind::Database,
                    ..Default::default()
                },
                "Async instructions.".into(),
            )],
        }));
        registry.discover_all().await.unwrap();

        let resolver = UnifiedSkillResolver::new(Arc::new(registry));
        // Before fix: this would fail with NotFound because now_or_never()
        // returned None when the future yielded.
        let resolved = resolver.resolve("async-skill").unwrap();
        assert_eq!(resolved.instructions, "Async instructions.");
    }

    // ── Error path and edge case tests ───────────────────────────────────

    #[tokio::test]
    async fn load_unknown_skill_returns_not_found() {
        let registry = UnifiedSkillRegistry::new();
        let err = registry.load("nonexistent").await.unwrap_err();
        assert!(matches!(err, SkillError::NotFound(_)));
    }

    #[tokio::test]
    async fn empty_registry_has_no_skills() {
        let registry = UnifiedSkillRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        assert!(registry.all_manifests().is_empty());
        assert!(registry.skill_names().is_empty());
        assert!(registry.get_manifest("anything").is_none());
    }

    #[tokio::test]
    async fn discover_all_on_empty_registry_returns_empty() {
        let registry = UnifiedSkillRegistry::new();
        let registered = registry.discover_all().await.unwrap();
        assert!(registered.is_empty());
    }

    #[tokio::test]
    async fn metadata_budget_skips_over_budget_skills() {
        // "small S []" → ~10 chars → 2 tokens; "big <long desc> []" → many tokens.
        // Budget of 5 fits "small" but not both.
        let mut registry = UnifiedSkillRegistry::new().with_budget(5);
        registry.add_provider(Box::new(StubProvider {
            skills: vec![
                (
                    SkillManifest {
                        name: "small".into(),
                        description: "S".into(),
                        source: SkillSourceKind::Local,
                        ..Default::default()
                    },
                    "small instructions".into(),
                ),
                (
                    SkillManifest {
                        name: "big".into(),
                        description: "This is a very big description that uses many many tokens and should exceed budget".into(),
                        source: SkillSourceKind::Local,
                        ..Default::default()
                    },
                    "big instructions".into(),
                ),
            ],
        }));

        let registered = registry.discover_all().await.unwrap();
        assert_eq!(registered.len(), 1);
        assert_eq!(registered[0], "small");
        assert!(registry.get_manifest("big").is_none());
    }

    #[tokio::test]
    async fn get_manifest_for_unknown_returns_none() {
        let mut registry = UnifiedSkillRegistry::new();
        registry.add_provider(Box::new(StubProvider {
            skills: vec![(
                SkillManifest {
                    name: "exists".into(),
                    description: "Yes".into(),
                    source: SkillSourceKind::Local,
                    ..Default::default()
                },
                "inst".into(),
            )],
        }));
        registry.discover_all().await.unwrap();

        assert!(registry.get_manifest("exists").is_some());
        assert!(registry.get_manifest("nope").is_none());
    }

    #[tokio::test]
    async fn load_caches_result_for_subsequent_calls() {
        let mut registry = UnifiedSkillRegistry::new();
        registry.add_provider(Box::new(StubProvider {
            skills: vec![(
                SkillManifest {
                    name: "cached".into(),
                    description: "D".into(),
                    source: SkillSourceKind::Local,
                    ..Default::default()
                },
                "original instructions".into(),
            )],
        }));
        registry.discover_all().await.unwrap();

        let first = registry.load("cached").await.unwrap();
        let second = registry.load("cached").await.unwrap();
        assert_eq!(first.instructions, second.instructions);
    }

    #[tokio::test]
    async fn record_file_path_no_conditional_skills_noop() {
        let mut registry = UnifiedSkillRegistry::new();
        registry.add_provider(Box::new(StubProvider {
            skills: vec![(
                SkillManifest {
                    name: "unconditional".into(),
                    description: "Always active".into(),
                    source: SkillSourceKind::Local,
                    ..Default::default()
                },
                "inst".into(),
            )],
        }));
        registry.discover_all().await.unwrap();

        // Unconditional skills are always activated
        assert!(registry.is_skill_activated("unconditional"));
        // Recording a path returns nothing new
        let activated = registry.record_file_path("any/file.rs");
        assert!(activated.is_empty());
    }

    #[tokio::test]
    async fn conditional_skill_not_activated_by_non_matching_path() {
        let mut registry = UnifiedSkillRegistry::new();
        registry.add_provider(Box::new(StubProvider {
            skills: vec![(
                SkillManifest {
                    name: "ts-lint".into(),
                    description: "TypeScript linter".into(),
                    paths: vec!["**/*.ts".into()],
                    source: SkillSourceKind::Local,
                    ..Default::default()
                },
                "Lint TS.".into(),
            )],
        }));
        registry.discover_all().await.unwrap();

        let activated = registry.record_file_path("src/main.rs");
        assert!(activated.is_empty());
        assert!(!registry.is_skill_activated("ts-lint"));
    }

    #[tokio::test]
    async fn is_skill_activated_unknown_skill_returns_false() {
        let registry = UnifiedSkillRegistry::new();
        assert!(!registry.is_skill_activated("unknown-skill"));
    }

    struct FailingProvider;

    #[async_trait]
    impl SkillProvider for FailingProvider {
        fn source_kind(&self) -> SkillSourceKind {
            SkillSourceKind::Plugin
        }
        async fn discover(&self) -> Result<Vec<SkillManifest>, SkillError> {
            Err(SkillError::Internal("provider crashed".into()))
        }
        async fn load(&self, name: &str) -> Result<LoadedSkill, SkillError> {
            Err(SkillError::Internal(format!("load failed: {name}")))
        }
        async fn refresh(&self) -> Result<(), SkillError> {
            Err(SkillError::Internal("refresh failed".into()))
        }
    }

    #[tokio::test]
    async fn discover_continues_when_provider_fails() {
        let mut registry = UnifiedSkillRegistry::new();
        registry.add_provider(Box::new(FailingProvider));
        registry.add_provider(Box::new(StubProvider {
            skills: vec![(
                SkillManifest {
                    name: "from-good-provider".into(),
                    description: "Works".into(),
                    source: SkillSourceKind::Local,
                    ..Default::default()
                },
                "inst".into(),
            )],
        }));

        let registered = registry.discover_all().await.unwrap();
        assert_eq!(registered, vec!["from-good-provider"]);
    }

    #[tokio::test]
    async fn load_propagates_non_not_found_errors() {
        let mut registry = UnifiedSkillRegistry::new();
        registry.add_provider(Box::new(FailingProvider));

        let err = registry.load("anything").await.unwrap_err();
        assert!(matches!(err, SkillError::Internal(_)));
    }

    #[tokio::test]
    async fn resolver_unknown_skill_returns_not_found() {
        let registry = Arc::new(UnifiedSkillRegistry::new());
        let resolver = UnifiedSkillResolver::new(registry);

        let err = resolver.resolve("nonexistent").unwrap_err();
        assert!(matches!(err, SkillError::NotFound(_)));
    }

    #[tokio::test]
    async fn resolver_excludes_non_invocable_skills() {
        let mut registry = UnifiedSkillRegistry::new();
        registry.add_provider(Box::new(StubProvider {
            skills: vec![
                (
                    SkillManifest {
                        name: "invocable".into(),
                        description: "User can call".into(),
                        user_invocable: true,
                        source: SkillSourceKind::Bundled,
                        ..Default::default()
                    },
                    "inst".into(),
                ),
                (
                    SkillManifest {
                        name: "hidden".into(),
                        description: "Internal only".into(),
                        user_invocable: false,
                        source: SkillSourceKind::Bundled,
                        ..Default::default()
                    },
                    "inst".into(),
                ),
            ],
        }));
        registry.discover_all().await.unwrap();

        let resolver = UnifiedSkillResolver::new(Arc::new(registry));
        let skills = resolver.available_skills();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "invocable");
    }

    #[tokio::test]
    async fn resolver_excludes_unactivated_conditional_skills() {
        let mut registry = UnifiedSkillRegistry::new();
        registry.add_provider(Box::new(StubProvider {
            skills: vec![(
                SkillManifest {
                    name: "conditional".into(),
                    description: "Path-gated".into(),
                    paths: vec!["**/*.py".into()],
                    source: SkillSourceKind::Local,
                    ..Default::default()
                },
                "inst".into(),
            )],
        }));
        registry.discover_all().await.unwrap();

        let resolver = UnifiedSkillResolver::new(Arc::new(registry));
        let skills = resolver.available_skills();
        // Conditional skill not yet activated → excluded
        assert!(skills.is_empty());
    }

    #[tokio::test]
    async fn legacy_adapter_delegates_correctly() {
        let mut registry = UnifiedSkillRegistry::new();
        registry.add_provider(Box::new(StubProvider {
            skills: vec![(
                SkillManifest {
                    name: "adapt-me".into(),
                    description: "Adaptation test".into(),
                    source: SkillSourceKind::Bundled,
                    ..Default::default()
                },
                "Adapted instructions.".into(),
            )],
        }));
        registry.discover_all().await.unwrap();
        registry.load("adapt-me").await.unwrap();

        let unified_resolver = Arc::new(UnifiedSkillResolver::new(Arc::new(registry)));
        let adapter = LegacySkillResolverAdapter::new(unified_resolver);

        use crate::turn::skill_tool::SkillResolver;
        let resolved = adapter.resolve("adapt-me").unwrap();
        assert_eq!(resolved.instructions, "Adapted instructions.");

        let skills = adapter.available_skills();
        assert_eq!(skills.len(), 1);
    }

    #[tokio::test]
    async fn legacy_adapter_maps_error_to_string() {
        let registry = Arc::new(UnifiedSkillRegistry::new());
        let resolver = Arc::new(UnifiedSkillResolver::new(registry));
        let adapter = LegacySkillResolverAdapter::new(resolver);

        use crate::turn::skill_tool::SkillResolver;
        let err = adapter.resolve("missing").unwrap_err();
        assert!(err.contains("unknown skill"));
    }

    #[tokio::test]
    async fn refresh_re_discovers_skills() {
        let mut registry = UnifiedSkillRegistry::new();
        registry.add_provider(Box::new(StubProvider {
            skills: vec![(
                SkillManifest {
                    name: "refreshable".into(),
                    description: "D".into(),
                    source: SkillSourceKind::Local,
                    ..Default::default()
                },
                "inst".into(),
            )],
        }));

        // Calling refresh should discover skills
        registry.refresh().await.unwrap();
        assert_eq!(registry.len(), 1);
    }

    #[tokio::test]
    async fn refresh_propagates_provider_error() {
        let mut registry = UnifiedSkillRegistry::new();
        registry.add_provider(Box::new(FailingProvider));

        let err = registry.refresh().await.unwrap_err();
        assert!(matches!(err, SkillError::Internal(_)));
    }

    #[tokio::test]
    async fn multiple_providers_deduplicate_by_priority() {
        let mut registry = UnifiedSkillRegistry::new();

        // Plugin (lowest priority)
        registry.add_provider(Box::new(StubProvider {
            skills: vec![(
                SkillManifest {
                    name: "dupe".into(),
                    description: "Plugin".into(),
                    source: SkillSourceKind::Plugin,
                    ..Default::default()
                },
                "plugin".into(),
            )],
        }));

        // Bundled (mid priority)
        registry.add_provider(Box::new(StubProvider {
            skills: vec![(
                SkillManifest {
                    name: "dupe".into(),
                    description: "Bundled".into(),
                    source: SkillSourceKind::Bundled,
                    ..Default::default()
                },
                "bundled".into(),
            )],
        }));

        let registered = registry.discover_all().await.unwrap();
        assert_eq!(registered.len(), 1);
        // Bundled (priority 1) beats Plugin (priority 4)
        let manifest = registry.get_manifest("dupe").unwrap();
        assert_eq!(manifest.description, "Bundled");
    }

    #[tokio::test]
    async fn discover_all_clears_stale_cache() {
        let mut registry = UnifiedSkillRegistry::new();
        registry.add_provider(Box::new(StubProvider {
            skills: vec![(
                SkillManifest {
                    name: "skill-a".into(),
                    description: "A".into(),
                    source: SkillSourceKind::Bundled,
                    ..Default::default()
                },
                "A".into(),
            )],
        }));

        registry.discover_all().await.unwrap();
        assert_eq!(registry.len(), 1);
        assert!(registry.get_manifest("skill-a").is_some());

        // Manually inject a stale entry into cache
        {
            let mut cache = registry.cache.write().unwrap();
            cache.insert(
                "stale-skill".into(),
                CachedSkill {
                    manifest: SkillManifest {
                        name: "stale-skill".into(),
                        description: "Stale".into(),
                        source: SkillSourceKind::Local,
                        ..Default::default()
                    },
                    loaded: None,
                },
            );
        }
        assert_eq!(registry.len(), 2);

        // Re-discover should clear the stale entry
        registry.discover_all().await.unwrap();
        assert_eq!(registry.len(), 1);
        assert!(registry.get_manifest("skill-a").is_some());
        assert!(registry.get_manifest("stale-skill").is_none());
    }

    #[tokio::test]
    async fn refresh_resets_conditional_tracker() {
        let mut registry = UnifiedSkillRegistry::new();
        registry.add_provider(Box::new(StubProvider {
            skills: vec![(
                SkillManifest {
                    name: "cond-skill".into(),
                    description: "Conditional".into(),
                    source: SkillSourceKind::Local,
                    paths: vec!["*.rs".into()],
                    ..Default::default()
                },
                "RS".into(),
            )],
        }));

        registry.discover_all().await.unwrap();
        let activated = registry.record_file_path("main.rs");
        assert_eq!(activated, vec!["cond-skill"]);
        assert!(registry.is_skill_activated("cond-skill"));

        // Refresh should reset the conditional tracker
        registry.refresh().await.unwrap();

        // After refresh, the tracker was reset so re-recording the path
        // should re-activate the skill (it's a new activation)
        let tracker = registry.conditional_tracker.read().unwrap();
        assert!(!tracker.is_activated("cond-skill"));
    }

    #[tokio::test]
    async fn register_mcp_skill_adds_to_cache() {
        let registry = UnifiedSkillRegistry::new();

        let skill_md = r#"---
name: mcp-test
description: "MCP test skill"
triggers:
  - test
---
MCP test instructions.
"#;

        let name = registry
            .register_mcp_skill("server-1", skill_md)
            .await
            .unwrap();
        assert_eq!(name, "mcp-test");
        assert!(registry.get_manifest("mcp-test").is_some());

        let loaded = registry.load("mcp-test").await.unwrap();
        assert!(loaded.instructions.contains("MCP test instructions"));
        assert_eq!(loaded.manifest.source, SkillSourceKind::Mcp);
    }

    #[tokio::test]
    async fn remove_mcp_server_skills_clears_from_cache() {
        let registry = UnifiedSkillRegistry::new();

        let skill_md = r#"---
name: mcp-remove
description: "Will be removed"
---
Removable.
"#;

        registry
            .register_mcp_skill("server-x", skill_md)
            .await
            .unwrap();
        assert!(registry.get_manifest("mcp-remove").is_some());

        registry.remove_mcp_server_skills("server-x").await.unwrap();
        assert!(registry.get_manifest("mcp-remove").is_none());
    }

    #[tokio::test]
    async fn mcp_provider_shared_with_registry() {
        let registry = UnifiedSkillRegistry::new();
        let provider = registry.mcp_provider();

        let skill_md = r#"---
name: shared-mcp
description: "Shared"
---
Shared MCP.
"#;
        provider.register_mcp_skill("srv", skill_md).unwrap();

        // Manually trigger discover so the cache picks it up
        registry.discover_all().await.unwrap();
        assert!(registry.get_manifest("shared-mcp").is_some());
    }

    // ── Alias resolution tests ──────────────────────────────────────────

    #[tokio::test]
    async fn resolver_finds_skill_by_alias() {
        let mut registry = UnifiedSkillRegistry::new();
        registry.add_provider(Box::new(StubProvider {
            skills: vec![(
                SkillManifest {
                    name: "code-review".into(),
                    description: "Review code".into(),
                    aliases: vec!["cr".into(), "review".into()],
                    ..Default::default()
                },
                "Review instructions.".into(),
            )],
        }));
        registry.discover_all().await.unwrap();
        // Pre-load so entry.loaded is Some
        registry.load("code-review").await.unwrap();

        let resolver = UnifiedSkillResolver::new(Arc::new(registry));
        // Resolve by alias
        let resolved = resolver.resolve("cr").unwrap();
        assert_eq!(resolved.name, "code-review");
        assert_eq!(resolved.aliases, vec!["cr", "review"]);

        // Also works with second alias
        let resolved2 = resolver.resolve("review").unwrap();
        assert_eq!(resolved2.name, "code-review");
    }

    #[tokio::test]
    async fn resolver_alias_miss_returns_not_found() {
        let mut registry = UnifiedSkillRegistry::new();
        registry.add_provider(Box::new(StubProvider {
            skills: vec![(
                SkillManifest {
                    name: "some-skill".into(),
                    description: "A skill".into(),
                    aliases: vec!["alias-a".into()],
                    ..Default::default()
                },
                "Instructions.".into(),
            )],
        }));
        registry.discover_all().await.unwrap();
        registry.load("some-skill").await.unwrap();

        let resolver = UnifiedSkillResolver::new(Arc::new(registry));
        let result = resolver.resolve("nonexistent-alias");
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn available_skills_includes_aliases() {
        let mut registry = UnifiedSkillRegistry::new();
        registry.add_provider(Box::new(StubProvider {
            skills: vec![(
                SkillManifest {
                    name: "my-skill".into(),
                    description: "Skill with aliases".into(),
                    aliases: vec!["ms".into(), "mine".into()],
                    ..Default::default()
                },
                "Do things.".into(),
            )],
        }));
        registry.discover_all().await.unwrap();

        let resolver = UnifiedSkillResolver::new(Arc::new(registry));
        let skills = resolver.available_skills();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "my-skill");
        assert_eq!(skills[0].aliases, vec!["ms", "mine"]);
    }

    #[tokio::test]
    async fn discover_all_warns_on_alias_collision_with_skill_name() {
        let mut registry = UnifiedSkillRegistry::new();
        registry.add_provider(Box::new(StubProvider {
            skills: vec![
                (
                    SkillManifest {
                        name: "skill-a".into(),
                        description: "First skill".into(),
                        ..Default::default()
                    },
                    "A instructions.".into(),
                ),
                (
                    SkillManifest {
                        name: "skill-b".into(),
                        description: "Second skill".into(),
                        aliases: vec!["skill-a".into()], // collides with skill-a's name
                        ..Default::default()
                    },
                    "B instructions.".into(),
                ),
            ],
        }));
        // Should succeed without panic — collision is warned, not fatal
        let names = registry.discover_all().await.unwrap();
        assert_eq!(names.len(), 2);
    }

    #[tokio::test]
    async fn discover_all_warns_on_alias_vs_alias_collision() {
        let mut registry = UnifiedSkillRegistry::new();
        registry.add_provider(Box::new(StubProvider {
            skills: vec![
                (
                    SkillManifest {
                        name: "alpha".into(),
                        description: "Alpha".into(),
                        aliases: vec!["shared-alias".into()],
                        ..Default::default()
                    },
                    "Alpha.".into(),
                ),
                (
                    SkillManifest {
                        name: "beta".into(),
                        description: "Beta".into(),
                        aliases: vec!["shared-alias".into()], // same alias as alpha
                        ..Default::default()
                    },
                    "Beta.".into(),
                ),
            ],
        }));
        let names = registry.discover_all().await.unwrap();
        assert_eq!(names.len(), 2);
        // First-registered alias wins — alpha owns "shared-alias"
    }
}
