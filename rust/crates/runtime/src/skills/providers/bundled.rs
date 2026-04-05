//! Bundled skill provider — serves skills compiled into the binary.
//!
//! Skills are registered at startup (e.g. from embedded SKILL.md content)
//! and remain available for the lifetime of the process.

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::RwLock;

use crate::skills::manifest::{LoadedSkill, SkillManifest, SkillSourceKind};
use crate::skills::traits::{SkillError, SkillProvider};

/// A skill bundled into the binary.
#[derive(Clone, Debug)]
struct BundledEntry {
    manifest: SkillManifest,
    instructions: String,
}

/// Provides skills compiled into the binary.
///
/// Bundled skills are registered via [`register`](Self::register) at startup.
/// They have lower priority than local skills but higher than database skills.
pub struct BundledSkillProvider {
    skills: RwLock<HashMap<String, BundledEntry>>,
}

impl BundledSkillProvider {
    pub fn new() -> Self {
        Self {
            skills: RwLock::new(HashMap::new()),
        }
    }

    /// Register a bundled skill from a raw SKILL.md content string.
    pub fn register_from_skill_md(&self, content: &str) -> Result<String, SkillError> {
        let (mut manifest, instructions) = crate::skills::loader::parse_skill_md(content)?;
        manifest.source = SkillSourceKind::Bundled;
        let name = manifest.name.clone();

        let mut skills = self
            .skills
            .write()
            .map_err(|e| SkillError::Internal(format!("lock poisoned: {e}")))?;
        skills.insert(
            name.clone(),
            BundledEntry {
                manifest,
                instructions,
            },
        );

        Ok(name)
    }

    /// Register a bundled skill from manifest + instructions directly.
    pub fn register(
        &self,
        mut manifest: SkillManifest,
        instructions: String,
    ) -> Result<(), SkillError> {
        manifest.source = SkillSourceKind::Bundled;
        manifest.trust_tier = crate::skills::manifest::TrustTier::Bundled;
        let name = manifest.name.clone();

        let mut skills = self
            .skills
            .write()
            .map_err(|e| SkillError::Internal(format!("lock poisoned: {e}")))?;
        skills.insert(
            name,
            BundledEntry {
                manifest,
                instructions,
            },
        );

        Ok(())
    }
}

impl Default for BundledSkillProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl BundledSkillProvider {
    /// Create a provider with all built-in skills pre-registered.
    pub fn with_defaults() -> Self {
        let provider = Self::new();
        for content in super::dynamic_skills::all_dynamic_skills() {
            if let Err(e) = provider.register_from_skill_md(&content) {
                eprintln!("  ⚠ Failed to register bundled skill: {e}");
            }
        }
        provider
    }
}

#[async_trait]
impl SkillProvider for BundledSkillProvider {
    fn source_kind(&self) -> SkillSourceKind {
        SkillSourceKind::Bundled
    }

    async fn discover(&self) -> Result<Vec<SkillManifest>, SkillError> {
        let skills = self
            .skills
            .read()
            .map_err(|e| SkillError::Internal(format!("lock poisoned: {e}")))?;
        Ok(skills.values().map(|e| e.manifest.clone()).collect())
    }

    async fn load(&self, name: &str) -> Result<LoadedSkill, SkillError> {
        let skills = self
            .skills
            .read()
            .map_err(|e| SkillError::Internal(format!("lock poisoned: {e}")))?;

        let entry = skills
            .get(name)
            .ok_or_else(|| SkillError::NotFound(format!("bundled skill not found: {name}")))?;

        Ok(LoadedSkill {
            manifest: entry.manifest.clone(),
            instructions: entry.instructions.clone(),
            instruction_tokens: (entry.instructions.len() as u32) / 4,
            resources: None,
            skill_dir: None,
        })
    }

    async fn refresh(&self) -> Result<(), SkillError> {
        // Bundled skills are static — nothing to refresh.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_SKILL: &str = r#"---
name: builtin-review
description: "Built-in code review skill"
triggers:
  - review
  - audit
---
# Review Process

1. Read the diff
2. Check for bugs
3. Suggest improvements
"#;

    #[tokio::test]
    async fn register_and_discover() {
        let provider = BundledSkillProvider::new();
        let name = provider.register_from_skill_md(SAMPLE_SKILL).unwrap();
        assert_eq!(name, "builtin-review");

        let manifests = provider.discover().await.unwrap();
        assert_eq!(manifests.len(), 1);
        assert_eq!(manifests[0].name, "builtin-review");
        assert_eq!(manifests[0].source, SkillSourceKind::Bundled);
    }

    #[tokio::test]
    async fn load_returns_full_content() {
        let provider = BundledSkillProvider::new();
        provider.register_from_skill_md(SAMPLE_SKILL).unwrap();

        let loaded = provider.load("builtin-review").await.unwrap();
        assert!(loaded.instructions.contains("Review Process"));
        assert_eq!(loaded.manifest.source, SkillSourceKind::Bundled);
        assert!(loaded.skill_dir.is_none());
    }

    #[tokio::test]
    async fn load_not_found() {
        let provider = BundledSkillProvider::new();
        let result = provider.load("nonexistent").await;
        assert!(matches!(result, Err(SkillError::NotFound(_))));
    }

    #[tokio::test]
    async fn register_direct() {
        let provider = BundledSkillProvider::new();
        let manifest = SkillManifest {
            name: "direct-skill".into(),
            description: "Directly registered".into(),
            ..Default::default()
        };
        provider.register(manifest, "Do the thing.".into()).unwrap();

        let loaded = provider.load("direct-skill").await.unwrap();
        assert_eq!(loaded.instructions, "Do the thing.");
        assert_eq!(loaded.manifest.source, SkillSourceKind::Bundled);
    }

    #[tokio::test]
    async fn multiple_bundled_skills() {
        let provider = BundledSkillProvider::new();

        let skill_a = r#"---
name: skill-a
description: "Skill A"
---
Instructions A.
"#;
        let skill_b = r#"---
name: skill-b
description: "Skill B"
---
Instructions B.
"#;

        provider.register_from_skill_md(skill_a).unwrap();
        provider.register_from_skill_md(skill_b).unwrap();

        let manifests = provider.discover().await.unwrap();
        assert_eq!(manifests.len(), 2);
    }

    #[tokio::test]
    async fn with_defaults_registers_bundled_skills() {
        let provider = BundledSkillProvider::with_defaults();
        let manifests = provider.discover().await.unwrap();

        let names: Vec<&str> = manifests.iter().map(|m| m.name.as_str()).collect();
        let expected = [
            "batch", "debug", "reflect", "review", "skillify", "stuck", "verify", "remember",
        ];
        for name in &expected {
            assert!(names.contains(name), "missing bundled skill: {name}");
        }
        assert_eq!(manifests.len(), expected.len());

        for m in &manifests {
            assert_eq!(m.source, SkillSourceKind::Bundled);
        }
    }

    #[tokio::test]
    async fn no_fork_context_by_default() {
        let provider = BundledSkillProvider::with_defaults();
        let manifests = provider.discover().await.unwrap();
        for m in &manifests {
            let loaded = provider.load(&m.name).await.unwrap();
            assert_eq!(
                loaded.manifest.execution_context,
                crate::skills::manifest::ExecutionContext::Inline,
                "{} should be inline (no fork context)",
                m.name
            );
        }
    }

    #[tokio::test]
    async fn with_defaults_all_loadable() {
        let provider = BundledSkillProvider::with_defaults();
        let manifests = provider.discover().await.unwrap();
        for m in &manifests {
            let loaded = provider.load(&m.name).await;
            assert!(loaded.is_ok(), "failed to load bundled skill: {}", m.name);
            let loaded = loaded.unwrap();
            assert!(
                !loaded.instructions.is_empty(),
                "empty instructions for skill: {}",
                m.name
            );
        }
    }
}
