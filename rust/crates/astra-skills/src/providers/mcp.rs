//! MCP skill provider — discovers skills from connected MCP servers.
//!
//! MCP servers can expose skills via `skill://` resource URIs. This provider
//! queries connected servers for skill resources and parses them as SKILL.md.

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::RwLock;

use crate::loader;
use crate::manifest::{LoadedSkill, SkillManifest, SkillSourceKind};
use crate::traits::{SkillError, SkillProvider};

/// A skill discovered from an MCP server.
#[derive(Clone, Debug)]
struct McpSkillEntry {
    server_name: String,
    manifest: SkillManifest,
    instructions: String,
}

/// Composite key for MCP skill cache: `(server_name, skill_name)`.
///
/// Prevents same-name skills from different servers from overwriting
/// each other. Removing one server's skills leaves the other intact.
type McpCacheKey = (String, String);

/// Provides skills from connected MCP servers via `skill://` resource URIs.
///
/// MCP servers advertise skills as resources. This provider:
/// 1. Queries each connected server for resources matching `skill://`
/// 2. Fetches the resource content (expected to be SKILL.md format)
/// 3. Parses and caches the results
///
/// The cache is keyed by `(server_name, skill_name)` to prevent collisions
/// when multiple servers expose skills with the same name.
pub struct McpSkillProvider {
    cache: RwLock<HashMap<McpCacheKey, McpSkillEntry>>,
}

impl McpSkillProvider {
    pub fn new() -> Self {
        Self {
            cache: RwLock::new(HashMap::new()),
        }
    }

    /// Register a skill from an MCP server.
    ///
    /// Called when an MCP server connection provides skill resources.
    pub fn register_mcp_skill(
        &self,
        server_name: &str,
        skill_md_content: &str,
    ) -> Result<String, SkillError> {
        let (mut manifest, instructions) = loader::parse_skill_md(skill_md_content)?;
        manifest.source = SkillSourceKind::Mcp;

        let name = manifest.name.clone();
        let key = (server_name.to_string(), name.clone());

        let mut cache = self
            .cache
            .write()
            .map_err(|e| SkillError::Internal(format!("lock poisoned: {e}")))?;
        cache.insert(
            key,
            McpSkillEntry {
                server_name: server_name.to_string(),
                manifest,
                instructions,
            },
        );

        Ok(name)
    }

    /// Remove all skills from a specific MCP server (called on disconnect).
    ///
    /// Only removes entries belonging to the disconnected server; skills
    /// with the same name from other servers remain available.
    pub fn remove_server_skills(&self, server_name: &str) {
        if let Ok(mut cache) = self.cache.write() {
            cache.retain(|_, entry| entry.server_name != server_name);
        }
    }
}

impl Default for McpSkillProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SkillProvider for McpSkillProvider {
    fn source_kind(&self) -> SkillSourceKind {
        SkillSourceKind::Mcp
    }

    async fn discover(&self) -> Result<Vec<SkillManifest>, SkillError> {
        let cache = self
            .cache
            .read()
            .map_err(|e| SkillError::Internal(format!("lock poisoned: {e}")))?;

        // Sort entries by server name for deterministic deduplication:
        // alphabetically-first server wins when multiple servers expose
        // the same skill name.
        let mut entries: Vec<&McpSkillEntry> = cache.values().collect();
        entries.sort_by(|a, b| a.server_name.cmp(&b.server_name));

        let mut seen = std::collections::HashSet::new();
        let mut manifests = Vec::new();
        for entry in entries {
            if seen.insert(entry.manifest.name.clone()) {
                manifests.push(entry.manifest.clone());
            } else {
                eprintln!(
                    "  ⚠ MCP skill '{}' from '{}' shadowed by earlier server",
                    entry.manifest.name, entry.server_name
                );
            }
        }
        Ok(manifests)
    }

    async fn load(&self, name: &str) -> Result<LoadedSkill, SkillError> {
        let cache = self
            .cache
            .read()
            .map_err(|e| SkillError::Internal(format!("lock poisoned: {e}")))?;

        // Deterministic: pick alphabetically-first server when collisions exist.
        let entry = cache
            .values()
            .filter(|e| e.manifest.name == name)
            .min_by(|a, b| a.server_name.cmp(&b.server_name))
            .ok_or_else(|| SkillError::NotFound(format!("MCP skill not found: {name}")))?;

        Ok(LoadedSkill {
            manifest: entry.manifest.clone(),
            instructions: entry.instructions.clone(),
            instruction_tokens: (entry.instructions.len() as u32) / 4,
            resources: None,
            skill_dir: None,
        })
    }

    async fn refresh(&self) -> Result<(), SkillError> {
        // MCP skills are re-discovered on reconnect; nothing to refresh here.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MCP_SKILL: &str = r#"---
name: mcp-analyze
description: "Analyze data via MCP server"
triggers:
  - analyze
---
# Analysis

Use the MCP server's analysis tool to process the data.
"#;

    #[tokio::test]
    async fn register_and_discover() {
        let provider = McpSkillProvider::new();
        let name = provider
            .register_mcp_skill("test-server", MCP_SKILL)
            .unwrap();
        assert_eq!(name, "mcp-analyze");

        let manifests = provider.discover().await.unwrap();
        assert_eq!(manifests.len(), 1);
        assert_eq!(manifests[0].source, SkillSourceKind::Mcp);
    }

    #[tokio::test]
    async fn load_mcp_skill() {
        let provider = McpSkillProvider::new();
        provider
            .register_mcp_skill("test-server", MCP_SKILL)
            .unwrap();

        let loaded = provider.load("mcp-analyze").await.unwrap();
        assert!(loaded.instructions.contains("Analysis"));
        assert_eq!(loaded.manifest.source, SkillSourceKind::Mcp);
    }

    #[tokio::test]
    async fn remove_server_skills() {
        let provider = McpSkillProvider::new();
        provider.register_mcp_skill("server-a", MCP_SKILL).unwrap();

        let skill_b = r#"---
name: mcp-query
description: "Query via MCP"
---
Query instructions.
"#;
        provider.register_mcp_skill("server-b", skill_b).unwrap();

        assert_eq!(provider.discover().await.unwrap().len(), 2);

        provider.remove_server_skills("server-a");
        let remaining = provider.discover().await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].name, "mcp-query");
    }

    #[tokio::test]
    async fn load_not_found() {
        let provider = McpSkillProvider::new();
        let result = provider.load("nonexistent").await;
        assert!(matches!(result, Err(SkillError::NotFound(_))));
    }

    #[tokio::test]
    async fn same_name_different_servers_no_overwrite() {
        let provider = McpSkillProvider::new();

        let skill_a = r#"---
name: analyze
description: "Server A analysis"
---
Server A instructions.
"#;
        let skill_b = r#"---
name: analyze
description: "Server B analysis"
---
Server B instructions.
"#;

        provider.register_mcp_skill("server-a", skill_a).unwrap();
        provider.register_mcp_skill("server-b", skill_b).unwrap();

        // Both are registered (discover deduplicates by name, but both exist internally)
        let cache_len = { provider.cache.read().unwrap().len() };
        assert_eq!(cache_len, 2);

        // discover returns deduplicated manifests (one per unique name)
        // Deterministic: alphabetically-first server ("server-a") wins
        let manifests = provider.discover().await.unwrap();
        assert_eq!(manifests.len(), 1);
        assert_eq!(manifests[0].name, "analyze");
        assert_eq!(manifests[0].description, "Server A analysis");

        // load also picks server-a deterministically
        let loaded = provider.load("analyze").await.unwrap();
        assert!(loaded.instructions.contains("Server A instructions"));
    }

    #[tokio::test]
    async fn remove_server_preserves_other_same_name() {
        let provider = McpSkillProvider::new();

        let skill = r#"---
name: shared-skill
description: "Shared"
---
Instructions.
"#;

        provider.register_mcp_skill("server-a", skill).unwrap();
        provider.register_mcp_skill("server-b", skill).unwrap();

        // Remove server-a; server-b's copy should survive
        provider.remove_server_skills("server-a");

        let loaded = provider.load("shared-skill").await.unwrap();
        assert!(loaded.instructions.contains("Instructions"));

        // Only one entry left
        let cache = provider.cache.read().unwrap();
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.values().next().unwrap().server_name, "server-b");
    }
}
