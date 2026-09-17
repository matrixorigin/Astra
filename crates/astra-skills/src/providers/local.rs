//! Local filesystem skill provider — discovers and loads skills from SKILL.md files.
//!
//! Scans filesystem directories for skill directories containing a `SKILL.md`
//! file. Agent Skills-compatible paths are discovered automatically.

use async_trait::async_trait;
use std::path::PathBuf;

use crate::loader;
use crate::manifest::{LoadedSkill, SkillManifest, SkillSourceKind};
use crate::traits::{SkillError, SkillProvider};

/// Provides skills from local filesystem directories.
pub struct LocalSkillProvider {
    /// Directories to scan for skills.
    search_paths: Vec<PathBuf>,
}

impl LocalSkillProvider {
    /// Create with standard CLI search paths.
    ///
    /// This includes project walk-up paths (`.astra/skills/`, `.agent/skills/`,
    /// `.claude/skills/`) plus user-level HOME paths. Use this for standalone CLI
    /// execution, where project-local skills are part of the local workspace.
    pub fn standard() -> Self {
        Self {
            search_paths: loader::skill_search_paths(),
        }
    }

    /// Create with walk-up search paths anchored at an explicit project root.
    ///
    /// Unlike [`Self::standard`], discovery is anchored at `project_root` instead
    /// of the process current directory. Tool execution may run in a workspace
    /// that differs from the process cwd (e.g. a CLI process launched from a
    /// monorepo while the session workspace is a nested repository); anchoring
    /// at the explicit root keeps that workspace's project skills visible.
    pub fn with_project_root(project_root: &std::path::Path) -> Self {
        Self {
            search_paths: loader::skill_search_paths_from_root(project_root),
        }
    }

    /// Create with only user-level HOME search paths.
    ///
    /// This is the provider API servers should use for their deployment-local
    /// catalog. It deliberately does not walk the current working directory, so
    /// a server launched from a repository cannot accidentally expose that
    /// repository's project-local skills to every web user.
    pub fn home_global() -> Self {
        Self {
            search_paths: loader::home_skill_search_paths(),
        }
    }

    /// Create with custom search paths.
    pub fn with_paths(paths: Vec<PathBuf>) -> Self {
        Self {
            search_paths: paths,
        }
    }
}

#[async_trait]
impl SkillProvider for LocalSkillProvider {
    fn source_kind(&self) -> SkillSourceKind {
        SkillSourceKind::Local
    }

    async fn discover(&self) -> Result<Vec<SkillManifest>, SkillError> {
        let mut manifests = Vec::new();
        let mut seen_names = std::collections::HashSet::new();
        let mut seen_paths = std::collections::HashSet::new();

        for search_dir in &self.search_paths {
            if !search_dir.exists() {
                continue;
            }

            let found = loader::discover_skills_in_dir(search_dir);
            for (name, skill_md_path) in found {
                if seen_names.contains(&name) {
                    continue; // Earlier paths have higher priority
                }

                // Resolve symlinks to prevent duplicate loading of the same
                // physical skill directory via different symlink paths.
                let canonical = skill_md_path
                    .canonicalize()
                    .unwrap_or_else(|_| skill_md_path.clone());
                if !seen_paths.insert(canonical) {
                    continue;
                }

                match loader::load_skill_from_path_confined(&skill_md_path, search_dir) {
                    Ok(loaded) => {
                        seen_names.insert(name);
                        manifests.push(loaded.manifest);
                    }
                    Err(e) => {
                        eprintln!("  ⚠ Failed to parse {}: {}", skill_md_path.display(), e);
                    }
                }
            }
        }

        Ok(manifests)
    }

    async fn load(&self, name: &str) -> Result<LoadedSkill, SkillError> {
        for search_dir in &self.search_paths {
            let skill_md = search_dir.join(name).join("SKILL.md");
            if skill_md.exists() {
                let mut loaded = loader::load_skill_from_path_confined(&skill_md, search_dir)?;
                loaded.manifest.source = SkillSourceKind::Local;
                return Ok(loaded);
            }
        }

        // Directory name often uses snake_case (`review_changes`) while SKILL.md frontmatter
        // uses kebab-case (`review-changes`). Direct path join misses; resolve by scanning.
        for search_dir in &self.search_paths {
            if !search_dir.exists() {
                continue;
            }
            let found = loader::discover_skills_in_dir(search_dir);
            for (_dir_name, skill_md_path) in found {
                let mut loaded =
                    match loader::load_skill_from_path_confined(&skill_md_path, search_dir) {
                        Ok(l) => l,
                        Err(_) => continue,
                    };
                if loaded.manifest.name == name || loaded.manifest.aliases.iter().any(|a| a == name)
                {
                    loaded.manifest.source = SkillSourceKind::Local;
                    return Ok(loaded);
                }
            }
        }

        Err(SkillError::NotFound(format!(
            "local skill not found: {name}"
        )))
    }

    async fn refresh(&self) -> Result<(), SkillError> {
        // Filesystem provider re-scans on next discover() call; nothing to cache-bust.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_test_skill(dir: &std::path::Path, name: &str, content: &str) {
        let skill_dir = dir.join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), content).unwrap();
    }

    #[tokio::test]
    async fn discover_finds_local_skills() {
        let dir = TempDir::new().unwrap();
        create_test_skill(
            dir.path(),
            "review",
            "---\nname: review\ndescription: Code review\n---\nReview the code.",
        );
        create_test_skill(
            dir.path(),
            "debug",
            "---\nname: debug\ndescription: Debug helper\n---\nDebug the issue.",
        );

        let provider = LocalSkillProvider::with_paths(vec![dir.path().to_path_buf()]);
        let manifests = provider.discover().await.unwrap();
        assert_eq!(manifests.len(), 2);

        let names: Vec<&str> = manifests.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"review"));
        assert!(names.contains(&"debug"));
    }

    #[tokio::test]
    async fn load_returns_full_skill() {
        let dir = TempDir::new().unwrap();
        create_test_skill(
            dir.path(),
            "test-skill",
            "---\nname: test-skill\ndescription: A test\n---\nStep 1: Do it.\nStep 2: Done.",
        );

        let provider = LocalSkillProvider::with_paths(vec![dir.path().to_path_buf()]);
        let loaded = provider.load("test-skill").await.unwrap();

        assert_eq!(loaded.manifest.name, "test-skill");
        assert!(loaded.instructions.contains("Step 1"));
        assert_eq!(loaded.manifest.source, SkillSourceKind::Local);
    }

    #[tokio::test]
    async fn load_not_found() {
        let dir = TempDir::new().unwrap();
        let provider = LocalSkillProvider::with_paths(vec![dir.path().to_path_buf()]);
        let result = provider.load("nonexistent").await;
        assert!(matches!(result, Err(SkillError::NotFound(_))));
    }

    /// Kebab-case manifest `name` with snake_case directory.
    #[tokio::test]
    async fn load_resolves_kebab_name_from_snake_case_dir() {
        let dir = TempDir::new().unwrap();
        create_test_skill(
            dir.path(),
            "review_changes",
            "---\nname: review-changes\ndescription: Review\n---\nDo the review.",
        );

        let provider = LocalSkillProvider::with_paths(vec![dir.path().to_path_buf()]);
        let loaded = provider.load("review-changes").await.unwrap();
        assert_eq!(loaded.manifest.name, "review-changes");
        assert!(loaded.instructions.contains("Do the review."));
    }

    #[tokio::test]
    async fn load_resolves_by_alias_when_dir_name_differs() {
        let dir = TempDir::new().unwrap();
        create_test_skill(
            dir.path(),
            "my_skill_dir",
            "---\nname: canonical\ndescription: X\naliases:\n  - review-changes\n---\nBody.",
        );

        let provider = LocalSkillProvider::with_paths(vec![dir.path().to_path_buf()]);
        let loaded = provider.load("review-changes").await.unwrap();
        assert_eq!(loaded.manifest.name, "canonical");
    }

    #[tokio::test]
    async fn priority_order_respected() {
        let high = TempDir::new().unwrap();
        let low = TempDir::new().unwrap();

        create_test_skill(
            high.path(),
            "shared",
            "---\nname: shared\ndescription: High priority\n---\nHigh.",
        );
        create_test_skill(
            low.path(),
            "shared",
            "---\nname: shared\ndescription: Low priority\n---\nLow.",
        );

        let provider = LocalSkillProvider::with_paths(vec![
            high.path().to_path_buf(),
            low.path().to_path_buf(),
        ]);

        let manifests = provider.discover().await.unwrap();
        assert_eq!(manifests.len(), 1);
        assert_eq!(manifests[0].description, "High priority");
    }

    #[tokio::test]
    async fn missing_dir_is_silently_skipped() {
        let provider =
            LocalSkillProvider::with_paths(vec![PathBuf::from("/nonexistent/path/skills")]);
        let manifests = provider.discover().await.unwrap();
        assert!(manifests.is_empty());
    }

    #[tokio::test]
    async fn with_project_root_anchors_discovery_at_explicit_root() {
        let root = TempDir::new().unwrap();
        // Project-local skill dirs under the explicit root.
        std::fs::create_dir_all(root.path().join(".astra").join("skills")).unwrap();
        std::fs::create_dir_all(root.path().join(".agent").join("skills")).unwrap();
        create_test_skill(
            &root.path().join(".astra").join("skills"),
            "rooted-skill",
            "---\nname: rooted-skill\ndescription: From project root\n---\nRooted.",
        );

        let provider = LocalSkillProvider::with_project_root(root.path());
        let manifests = provider.discover().await.unwrap();
        assert!(
            manifests.iter().any(|m| m.name == "rooted-skill"),
            "project-root-anchored discovery must find the project skill, got: {manifests:?}"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn symlink_dedup_prevents_double_loading() {
        let real_dir = TempDir::new().unwrap();
        let link_dir = TempDir::new().unwrap();

        create_test_skill(
            real_dir.path(),
            "review",
            "---\nname: review\ndescription: Real\n---\nInstructions.",
        );

        // Create a symlink pointing to the real skill directory
        let link_target = link_dir.path().join("review");
        std::os::unix::fs::symlink(real_dir.path().join("review"), &link_target).unwrap();

        // Both real and symlink dirs in search paths
        let provider = LocalSkillProvider::with_paths(vec![
            real_dir.path().to_path_buf(),
            link_dir.path().to_path_buf(),
        ]);

        let manifests = provider.discover().await.unwrap();
        // Should only find the skill once despite the symlink
        assert_eq!(manifests.len(), 1);
    }

    #[tokio::test]
    async fn review_changes_skill_exposes_fixed_fanout_for_parallel_reviews() {
        let repo_skills = astra_core::test_paths::workspace_path(".agent/skills")
            .canonicalize()
            .expect("repo skills dir should resolve in workspace tests");
        let provider = LocalSkillProvider::with_paths(vec![repo_skills]);
        let loaded = provider.load("review-changes").await.unwrap();
        assert!(
            loaded
                .manifest
                .allowed_tools
                .iter()
                .any(|tool| tool == "agent_fanout"),
            "review-changes must expose fixed fanout without a discovery round"
        );
        assert!(
            !loaded
                .manifest
                .allowed_tools
                .iter()
                .any(|tool| tool == "agent"),
            "review-changes must not expose a competing per-agent lifecycle"
        );
    }

    #[tokio::test]
    async fn standard_astra_priority_prefers_agent_contract_over_claude_compatibility() {
        let root = TempDir::new().unwrap();
        std::fs::create_dir_all(root.path().join(".git")).unwrap();
        let agent_skills = root.path().join(".agent/skills");
        let claude_skills = root.path().join(".claude/skills");

        create_test_skill(
            &agent_skills,
            "review_changes",
            "---\nname: review-changes\ndescription: Astra review contract\nallowed_tools:\n  - agent_fanout\n---\nVerify every child finding before synthesis.",
        );
        create_test_skill(
            &claude_skills,
            "review_changes",
            "---\nname: review-changes\ndescription: Compatibility review contract\nallowed_tools:\n  - read_file\n---\nCompatibility instructions.",
        );

        let provider =
            LocalSkillProvider::with_paths(loader::skill_search_paths_from(root.path(), None));
        let loaded = provider.load("review-changes").await.unwrap();

        assert_eq!(loaded.manifest.description, "Astra review contract");
        assert!(
            loaded
                .manifest
                .allowed_tools
                .iter()
                .any(|tool| tool == "agent_fanout")
        );
        assert!(
            loaded
                .instructions
                .contains("Verify every child finding before synthesis")
        );
    }
}
