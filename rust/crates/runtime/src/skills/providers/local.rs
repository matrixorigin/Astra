//! Local filesystem skill provider — discovers and loads skills from SKILL.md files.
//!
//! Scans standard directories (`.astra/skills/`, `skills/`, `~/.astra/skills/`)
//! for skill directories containing a `SKILL.md` file.

use async_trait::async_trait;
use std::path::PathBuf;

use crate::skills::loader;
use crate::skills::manifest::{LoadedSkill, SkillManifest, SkillSourceKind};
use crate::skills::traits::{SkillError, SkillProvider};

/// Provides skills from local filesystem directories.
pub struct LocalSkillProvider {
    /// Directories to scan for skills.
    search_paths: Vec<PathBuf>,
}

impl LocalSkillProvider {
    /// Create with standard search paths (`.astra/skills/`, `skills/`, `~/.astra/skills/`).
    pub fn standard() -> Self {
        Self {
            search_paths: loader::skill_search_paths(),
        }
    }

    /// Create with custom search paths.
    pub fn with_paths(paths: Vec<PathBuf>) -> Self {
        Self {
            search_paths: paths,
        }
    }

    /// Add an additional search path.
    pub fn add_path(&mut self, path: PathBuf) {
        self.search_paths.push(path);
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
                let canonical = skill_md_path.canonicalize().unwrap_or_else(|_| skill_md_path.clone());
                if !seen_paths.insert(canonical) {
                    continue;
                }

                match loader::load_skill_from_path_confined(&skill_md_path, search_dir) {
                    Ok(loaded) => {
                        seen_names.insert(name);
                        manifests.push(loaded.manifest);
                    }
                    Err(e) => {
                        eprintln!(
                            "  ⚠ Failed to parse {}: {}",
                            skill_md_path.display(),
                            e
                        );
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
                let mut loaded =
                    loader::load_skill_from_path_confined(&skill_md, search_dir)?;
                loaded.manifest.source = SkillSourceKind::Local;
                return Ok(loaded);
            }
        }

        Err(SkillError::NotFound(format!("local skill not found: {name}")))
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
            "---\nname: review\ndescription: Code review\ntriggers:\n  - review\n---\nReview the code.",
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
            "---\nname: test-skill\ndescription: A test\nmodel: gpt-4o\n---\nStep 1: Do it.\nStep 2: Done.",
        );

        let provider = LocalSkillProvider::with_paths(vec![dir.path().to_path_buf()]);
        let loaded = provider.load("test-skill").await.unwrap();

        assert_eq!(loaded.manifest.name, "test-skill");
        assert_eq!(loaded.manifest.model.as_deref(), Some("gpt-4o"));
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
}
