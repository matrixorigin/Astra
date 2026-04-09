//! Shared context cache for cross-agent knowledge sharing.
//!
//! Provides three tiers of caching:
//! 1. File content cache — avoid redundant disk reads across agents
//! 2. Knowledge fragments — semantic key-value store for agent discoveries
//! 3. Agent findings — structured results from spawned agents

use dashmap::mapref::multiple::RefMulti;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

// ─── Types ──────────────────────────────────────────────────────────────────

/// Shared context cache for cross-agent knowledge sharing.
///
/// Thread-safe cache that allows multiple agents to share:
/// - File contents (with mtime validation and TTL)
/// - Knowledge fragments (semantic key-value pairs)
/// - Agent findings (structured results from completed agents)
#[derive(Debug)]
pub struct SharedContextCache {
    /// file_path → CachedFile
    files: DashMap<PathBuf, CachedFile>,
    /// semantic_key → Knowledge
    knowledge: DashMap<String, Knowledge>,
    /// agent_id → AgentFindings
    agent_results: DashMap<String, AgentFindings>,
    /// TTL for file cache
    file_ttl: Duration,
    /// Max file cache size in bytes
    max_file_cache_bytes: usize,
    /// Current file cache size in bytes
    current_file_cache_bytes: AtomicUsize,
}

/// A cached file with metadata.
#[derive(Debug, Clone)]
pub struct CachedFile {
    /// File content.
    pub content: String,
    /// Size in bytes.
    pub size_bytes: usize,
    /// Last modified time from filesystem.
    pub mtime: SystemTime,
    /// When this entry was cached.
    pub cached_at: SystemTime,
    /// Which agents have read this file.
    pub readers: Vec<String>,
}

/// A knowledge fragment shared between agents.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Knowledge {
    /// Semantic key (e.g., "auth/jwt-config").
    pub key: String,
    /// The knowledge value.
    pub value: serde_json::Value,
    /// Which agent created this.
    pub source_agent: String,
    /// When this was created.
    #[serde(with = "system_time_serde")]
    pub created_at: SystemTime,
    /// How many times this has been accessed.
    pub access_count: u32,
}

/// Findings from a completed agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentFindings {
    /// Agent ID.
    pub agent_id: String,
    /// Agent type (explore, code-review, etc.).
    pub agent_type: String,
    /// Brief summary of what the agent found.
    pub summary: String,
    /// Detailed findings.
    pub findings: Vec<Finding>,
    /// When the agent completed.
    #[serde(with = "system_time_serde")]
    pub completed_at: SystemTime,
}

/// A single finding from an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    /// Category of this finding.
    pub category: FindingCategory,
    /// Short title.
    pub title: String,
    /// Detailed description.
    pub detail: String,
    /// Confidence score (0.0 to 1.0).
    pub confidence: f32,
    /// Related files.
    pub related_files: Vec<PathBuf>,
}

/// Categories for agent findings.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingCategory {
    /// Found code pattern or anti-pattern.
    CodePattern,
    /// Discovered dependency info.
    Dependency,
    /// Architecture insight.
    Architecture,
    /// Security observation.
    Security,
    /// Performance finding.
    Performance,
    /// Documentation gap or update.
    Documentation,
    /// User-defined category.
    Custom(String),
}

// ─── Implementation ─────────────────────────────────────────────────────────

impl Default for SharedContextCache {
    fn default() -> Self {
        Self {
            files: DashMap::new(),
            knowledge: DashMap::new(),
            agent_results: DashMap::new(),
            file_ttl: Duration::from_secs(30),
            max_file_cache_bytes: 100 * 1024 * 1024, // 100MB
            current_file_cache_bytes: AtomicUsize::new(0),
        }
    }
}

impl SharedContextCache {
    /// Create a new cache with custom settings.
    pub fn new(file_ttl: Duration, max_file_cache_bytes: usize) -> Self {
        Self {
            files: DashMap::new(),
            knowledge: DashMap::new(),
            agent_results: DashMap::new(),
            file_ttl,
            max_file_cache_bytes,
            current_file_cache_bytes: AtomicUsize::new(0),
        }
    }

    // ─── File Cache ─────────────────────────────────────────────────────

    /// Get cached file content if valid (not expired, mtime unchanged).
    pub fn get_file(&self, path: &PathBuf) -> Option<String> {
        let entry = self.files.get(path)?;
        let now = SystemTime::now();

        // Check TTL
        if now.duration_since(entry.cached_at).unwrap_or_default() >= self.file_ttl {
            return None;
        }

        // Check mtime hasn't changed (if file still exists)
        if let Ok(meta) = std::fs::metadata(path) {
            if let Ok(mtime) = meta.modified() {
                if mtime != entry.mtime {
                    return None; // File modified, cache invalid
                }
            }
        }

        Some(entry.content.clone())
    }

    /// Cache file content with mtime tracking.
    pub fn put_file(&self, path: PathBuf, content: String, mtime: SystemTime, agent_id: &str) {
        let size = content.len();

        // Evict if over limit
        self.maybe_evict(size);

        // Preserve existing readers
        let mut readers = self
            .files
            .get(&path)
            .map(|e| e.readers.clone())
            .unwrap_or_default();

        if !readers.contains(&agent_id.to_string()) {
            readers.push(agent_id.to_string());
        }

        self.files.insert(
            path,
            CachedFile {
                content,
                size_bytes: size,
                mtime,
                cached_at: SystemTime::now(),
                readers,
            },
        );

        self.current_file_cache_bytes
            .fetch_add(size, Ordering::Relaxed);
    }

    /// Record that an agent read a file (for tracking which agents saw what).
    pub fn record_file_read(&self, path: &PathBuf, agent_id: &str) {
        if let Some(mut entry) = self.files.get_mut(path) {
            if !entry.readers.contains(&agent_id.to_string()) {
                entry.readers.push(agent_id.to_string());
            }
        }
    }

    /// Get files that multiple agents have read (potential shared context).
    pub fn get_commonly_read_files(&self, min_readers: usize) -> Vec<PathBuf> {
        self.files
            .iter()
            .filter(|e: &RefMulti<'_, PathBuf, CachedFile>| e.readers.len() >= min_readers)
            .map(|e: RefMulti<'_, PathBuf, CachedFile>| e.key().clone())
            .collect()
    }

    /// Get number of cached files.
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Get current file cache size in bytes.
    pub fn file_cache_bytes(&self) -> usize {
        self.current_file_cache_bytes.load(Ordering::Relaxed)
    }

    // ─── Knowledge Store ────────────────────────────────────────────────

    /// Store a knowledge fragment.
    pub fn share_knowledge(&self, key: String, value: serde_json::Value, source_agent: &str) {
        self.knowledge.insert(
            key.clone(),
            Knowledge {
                key,
                value,
                source_agent: source_agent.to_string(),
                created_at: SystemTime::now(),
                access_count: 0,
            },
        );
    }

    /// Get a knowledge fragment by exact key.
    pub fn get_knowledge(&self, key: &str) -> Option<serde_json::Value> {
        if let Some(mut entry) = self.knowledge.get_mut(key) {
            entry.access_count += 1;
            Some(entry.value.clone())
        } else {
            None
        }
    }

    /// Search knowledge by key prefix.
    pub fn search_knowledge(&self, prefix: &str) -> Vec<Knowledge> {
        self.knowledge
            .iter()
            .filter(|e: &RefMulti<'_, String, Knowledge>| e.key.starts_with(prefix))
            .map(|e: RefMulti<'_, String, Knowledge>| e.value().clone())
            .collect()
    }

    /// Get all knowledge keys.
    pub fn list_knowledge_keys(&self) -> Vec<String> {
        self.knowledge
            .iter()
            .map(|e: RefMulti<'_, String, Knowledge>| e.key().clone())
            .collect()
    }

    /// Get number of knowledge entries.
    pub fn knowledge_count(&self) -> usize {
        self.knowledge.len()
    }

    // ─── Agent Findings ─────────────────────────────────────────────────

    /// Store findings from a completed agent.
    pub fn store_agent_findings(&self, findings: AgentFindings) {
        self.agent_results
            .insert(findings.agent_id.clone(), findings);
    }

    /// Get findings from a specific agent.
    pub fn get_agent_findings(&self, agent_id: &str) -> Option<AgentFindings> {
        self.agent_results
            .get(agent_id)
            .map(|e: dashmap::mapref::one::Ref<'_, String, AgentFindings>| e.value().clone())
    }

    /// Get all findings of a specific category.
    pub fn get_findings_by_category(&self, category: &FindingCategory) -> Vec<Finding> {
        self.agent_results
            .iter()
            .flat_map(|e: RefMulti<'_, String, AgentFindings>| {
                e.value().findings.iter().cloned().collect::<Vec<_>>()
            })
            .filter(|f| &f.category == category)
            .collect()
    }

    /// Get all agent IDs that have stored findings.
    pub fn list_agent_findings(&self) -> Vec<String> {
        self.agent_results
            .iter()
            .map(|e: RefMulti<'_, String, AgentFindings>| e.value().agent_id.clone())
            .collect()
    }

    /// Get summary of all agent findings for prompt injection.
    pub fn summarize_findings(&self) -> String {
        let mut lines = Vec::new();
        for entry in self.agent_results.iter() {
            let entry: RefMulti<'_, String, AgentFindings> = entry;
            let val = entry.value();
            lines.push(format!(
                "## Agent: {} ({})",
                val.agent_id, val.agent_type
            ));
            lines.push(val.summary.clone());
            for finding in &val.findings {
                lines.push(format!(
                    "- [{:?}] {}: {}",
                    finding.category, finding.title, finding.detail
                ));
            }
            lines.push(String::new());
        }
        lines.join("\n")
    }

    // ─── Eviction ───────────────────────────────────────────────────────

    fn maybe_evict(&self, needed_bytes: usize) {
        let current = self.current_file_cache_bytes.load(Ordering::Relaxed);
        if current + needed_bytes <= self.max_file_cache_bytes {
            return;
        }

        // Collect entries sorted by cached_at (oldest first)
        let mut entries: Vec<_> = self
            .files
            .iter()
            .map(|e: RefMulti<'_, PathBuf, CachedFile>| {
                (e.key().clone(), e.value().cached_at, e.value().size_bytes)
            })
            .collect();
        entries.sort_by_key(|(_, cached_at, _)| *cached_at);

        let mut freed = 0;
        for (path, _, size) in entries {
            if freed >= needed_bytes {
                break;
            }
            self.files.remove(&path);
            freed += size;
            self.current_file_cache_bytes
                .fetch_sub(size, Ordering::Relaxed);
        }
    }

    /// Clear all caches.
    pub fn clear(&self) {
        self.files.clear();
        self.knowledge.clear();
        self.agent_results.clear();
        self.current_file_cache_bytes.store(0, Ordering::Relaxed);
    }

    /// Get cache statistics.
    pub fn stats(&self) -> CacheStats {
        CacheStats {
            file_count: self.files.len(),
            file_cache_bytes: self.current_file_cache_bytes.load(Ordering::Relaxed),
            knowledge_count: self.knowledge.len(),
            agent_findings_count: self.agent_results.len(),
        }
    }
}

/// Cache statistics.
#[derive(Debug, Clone)]
pub struct CacheStats {
    pub file_count: usize,
    pub file_cache_bytes: usize,
    pub knowledge_count: usize,
    pub agent_findings_count: usize,
}

// ─── Tool Schemas ───────────────────────────────────────────────────────────

/// Tool schema for sharing knowledge between agents.
pub fn share_context_schema() -> serde_json::Value {
    serde_json::json!({
        "name": "share_context",
        "description": "Share a knowledge fragment with other agents. Use this to communicate findings, patterns, or insights that other agents might need.",
        "input_schema": {
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "Semantic key for this knowledge (e.g., 'auth/jwt-config', 'db/schema-version')"
                },
                "value": {
                    "description": "The knowledge to share (JSON object or primitive)"
                },
                "category": {
                    "type": "string",
                    "enum": ["code_pattern", "dependency", "architecture", "security", "performance", "documentation"],
                    "description": "Category of this knowledge (optional)"
                }
            },
            "required": ["key", "value"]
        }
    })
}

/// Tool schema for querying shared knowledge.
pub fn query_context_schema() -> serde_json::Value {
    serde_json::json!({
        "name": "query_context",
        "description": "Query knowledge shared by other agents. Returns matching knowledge fragments.",
        "input_schema": {
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "Exact key to lookup"
                },
                "prefix": {
                    "type": "string",
                    "description": "Key prefix to search (e.g., 'auth/' returns all auth-related knowledge)"
                },
                "category": {
                    "type": "string",
                    "enum": ["code_pattern", "dependency", "architecture", "security", "performance", "documentation"],
                    "description": "Filter findings by category"
                }
            }
        }
    })
}

// ─── Serde helpers ──────────────────────────────────────────────────────────

mod system_time_serde {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    pub fn serialize<S>(time: &SystemTime, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let duration = time.duration_since(UNIX_EPOCH).unwrap_or_default();
        duration.as_millis().serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<SystemTime, D::Error>
    where
        D: Deserializer<'de>,
    {
        let millis = u128::deserialize(deserializer)?;
        Ok(UNIX_EPOCH + Duration::from_millis(millis as u64))
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;
    use std::time::Duration;

    #[test]
    fn test_file_cache_basic() {
        let cache = SharedContextCache::default();
        let path = PathBuf::from("/tmp/test_file.txt");
        let content = "Hello, World!".to_string();
        let mtime = SystemTime::now();

        // Put and get
        cache.put_file(path.clone(), content.clone(), mtime, "agent-1");
        assert_eq!(cache.get_file(&path), Some(content.clone()));

        // Record another reader
        cache.record_file_read(&path, "agent-2");
        let entry = cache.files.get(&path).unwrap();
        assert_eq!(entry.readers.len(), 2);
        assert!(entry.readers.contains(&"agent-1".to_string()));
        assert!(entry.readers.contains(&"agent-2".to_string()));
    }

    #[test]
    fn test_file_cache_ttl() {
        let cache = SharedContextCache::new(Duration::from_millis(50), 100 * 1024 * 1024);
        let path = PathBuf::from("/tmp/test_ttl.txt");
        let content = "Expires soon".to_string();
        let mtime = SystemTime::now();

        cache.put_file(path.clone(), content.clone(), mtime, "agent-1");
        assert!(cache.get_file(&path).is_some());

        // Wait for TTL to expire
        sleep(Duration::from_millis(60));
        assert!(cache.get_file(&path).is_none());
    }

    #[test]
    fn test_knowledge_store() {
        let cache = SharedContextCache::default();

        // Share knowledge
        cache.share_knowledge(
            "auth/jwt-secret".to_string(),
            serde_json::json!({"algorithm": "HS256"}),
            "explore-agent",
        );
        cache.share_knowledge(
            "auth/session-ttl".to_string(),
            serde_json::json!(3600),
            "explore-agent",
        );
        cache.share_knowledge(
            "db/version".to_string(),
            serde_json::json!("14.2"),
            "db-agent",
        );

        // Get by exact key
        let jwt = cache.get_knowledge("auth/jwt-secret").unwrap();
        assert_eq!(jwt["algorithm"], "HS256");

        // Search by prefix
        let auth_knowledge = cache.search_knowledge("auth/");
        assert_eq!(auth_knowledge.len(), 2);

        // Check access count increased
        let _ = cache.get_knowledge("auth/jwt-secret");
        let entry = cache.knowledge.get("auth/jwt-secret").unwrap();
        assert_eq!(entry.access_count, 2);
    }

    #[test]
    fn test_agent_findings() {
        let cache = SharedContextCache::default();

        let findings = AgentFindings {
            agent_id: "explore-1".to_string(),
            agent_type: "explore".to_string(),
            summary: "Found authentication patterns".to_string(),
            findings: vec![
                Finding {
                    category: FindingCategory::Security,
                    title: "JWT implementation".to_string(),
                    detail: "Uses HS256 with env-based secret".to_string(),
                    confidence: 0.95,
                    related_files: vec![PathBuf::from("src/auth.rs")],
                },
                Finding {
                    category: FindingCategory::CodePattern,
                    title: "Error handling".to_string(),
                    detail: "Uses Result with custom Error type".to_string(),
                    confidence: 0.9,
                    related_files: vec![PathBuf::from("src/error.rs")],
                },
            ],
            completed_at: SystemTime::now(),
        };

        cache.store_agent_findings(findings);

        // Get by agent
        let retrieved = cache.get_agent_findings("explore-1").unwrap();
        assert_eq!(retrieved.findings.len(), 2);

        // Get by category
        let security_findings = cache.get_findings_by_category(&FindingCategory::Security);
        assert_eq!(security_findings.len(), 1);
        assert_eq!(security_findings[0].title, "JWT implementation");
    }

    #[test]
    fn test_commonly_read_files() {
        let cache = SharedContextCache::default();
        let mtime = SystemTime::now();

        // File read by multiple agents
        cache.put_file(
            PathBuf::from("src/main.rs"),
            "fn main()".to_string(),
            mtime,
            "agent-1",
        );
        cache.record_file_read(&PathBuf::from("src/main.rs"), "agent-2");
        cache.record_file_read(&PathBuf::from("src/main.rs"), "agent-3");

        // File read by one agent
        cache.put_file(
            PathBuf::from("src/lib.rs"),
            "mod lib".to_string(),
            mtime,
            "agent-1",
        );

        let common = cache.get_commonly_read_files(2);
        assert_eq!(common.len(), 1);
        assert_eq!(common[0], PathBuf::from("src/main.rs"));
    }

    #[test]
    fn test_cache_eviction() {
        // Small cache limit
        let cache = SharedContextCache::new(Duration::from_secs(30), 100);
        let mtime = SystemTime::now();

        // Add first file (50 bytes)
        cache.put_file(
            PathBuf::from("file1.txt"),
            "x".repeat(50),
            mtime,
            "agent-1",
        );
        assert_eq!(cache.file_count(), 1);

        // Add second file (50 bytes) - should fit
        cache.put_file(
            PathBuf::from("file2.txt"),
            "y".repeat(50),
            mtime,
            "agent-1",
        );
        assert_eq!(cache.file_count(), 2);

        // Add third file (60 bytes) - should evict oldest
        cache.put_file(
            PathBuf::from("file3.txt"),
            "z".repeat(60),
            mtime,
            "agent-1",
        );
        // file1 should be evicted
        assert!(cache.get_file(&PathBuf::from("file1.txt")).is_none());
    }

    #[test]
    fn test_summarize_findings() {
        let cache = SharedContextCache::default();

        cache.store_agent_findings(AgentFindings {
            agent_id: "explore-1".to_string(),
            agent_type: "explore".to_string(),
            summary: "Found auth patterns".to_string(),
            findings: vec![Finding {
                category: FindingCategory::Security,
                title: "JWT".to_string(),
                detail: "Uses JWT".to_string(),
                confidence: 0.9,
                related_files: vec![],
            }],
            completed_at: SystemTime::now(),
        });

        let summary = cache.summarize_findings();
        assert!(summary.contains("explore-1"));
        assert!(summary.contains("JWT"));
        assert!(summary.contains("Security"));
    }
}
