//! Dynamic tool registration for plugins and skill manifests.
//!
//! Complements the static `TOOL_CATALOG` with runtime-registerable tools.
//! Plugin tools participate in TF-IDF scoring alongside built-in tools,
//! can be enabled/disabled per session, and are loaded from skill manifests.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::tool_registry_meta::{IntentType, Scope, TOOL_CATALOG};
use astra_text_utils::text_tokenize;

// ─── Plugin Tool Entry ──────────────────────────────────────────────────────

/// Owned tool metadata for dynamically registered tools.
/// Unlike static `ToolMeta` (which uses `&'static str`), this supports runtime creation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginToolEntry {
    pub name: String,
    pub description: String,
    pub triggers: Vec<String>,
    pub pinned: bool,
    pub intents: Vec<IntentType>,
    pub scope: Scope,
    pub schema: Value,
    pub schema_tokens: u32,
    /// Plugin source (e.g., "skills/kubernetes", "user-defined")
    pub source: String,
    /// Whether this tool is currently enabled
    pub enabled: bool,
}

// ─── Plugin Registry ────────────────────────────────────────────────────────

/// Registry for dynamically registered plugin tools.
///
/// Design principles:
/// - Additive: doesn't modify the static TOOL_CATALOG
/// - Safe: rejects name conflicts with built-in tools
/// - Scorable: pre-tokenizes triggers for TF-IDF matching
/// - Toggleable: tools can be enabled/disabled per session
#[derive(Debug, Default)]
pub struct PluginRegistry {
    tools: Vec<PluginToolEntry>,
    /// Pre-tokenized (triggers + description + name) for TF-IDF scoring.
    /// Parallel to `tools` — index i of token_cache corresponds to tools[i].
    token_cache: Vec<Vec<String>>,
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new plugin tool.
    ///
    /// Returns `Err` if:
    /// - Name conflicts with a built-in tool in TOOL_CATALOG
    /// - Name is already registered as a plugin tool
    pub fn register(&mut self, entry: PluginToolEntry) -> Result<(), String> {
        if TOOL_CATALOG.iter().any(|t| t.name == entry.name) {
            return Err(format!(
                "Tool '{}' conflicts with built-in tool",
                entry.name
            ));
        }
        if self.tools.iter().any(|t| t.name == entry.name) {
            return Err(format!("Plugin tool '{}' already registered", entry.name));
        }

        let tokens = tokenize_entry(&entry);
        self.token_cache.push(tokens);
        self.tools.push(entry);
        Ok(())
    }

    /// Unregister a plugin tool by name. Returns true if found and removed.
    pub fn unregister(&mut self, name: &str) -> bool {
        if let Some(idx) = self.tools.iter().position(|t| t.name == name) {
            self.tools.remove(idx);
            self.token_cache.remove(idx);
            true
        } else {
            false
        }
    }

    /// Enable or disable a tool. Returns true if the tool was found.
    pub fn set_enabled(&mut self, name: &str, enabled: bool) -> bool {
        if let Some(tool) = self.tools.iter_mut().find(|t| t.name == name) {
            tool.enabled = enabled;
            true
        } else {
            false
        }
    }

    /// Iterator over all enabled plugin tools.
    pub fn enabled_tools(&self) -> impl Iterator<Item = &PluginToolEntry> {
        self.tools.iter().filter(|t| t.enabled)
    }

    /// Collect JSON schemas for all enabled plugin tools.
    pub fn schemas(&self) -> Vec<Value> {
        self.tools
            .iter()
            .filter(|t| t.enabled)
            .map(|t| t.schema.clone())
            .collect()
    }

    /// TF-IDF score for a single plugin tool against query tokens.
    ///
    /// Uses the same tokenization engine as built-in tool scoring (CJK-aware).
    /// IDF is computed over the plugin catalog only (not mixed with built-in).
    pub fn tfidf_score(&self, query_tokens: &[String], plugin_idx: usize) -> f64 {
        if plugin_idx >= self.token_cache.len() {
            return 0.0;
        }
        let tool_tokens = &self.token_cache[plugin_idx];
        if tool_tokens.is_empty() || query_tokens.is_empty() {
            return 0.0;
        }

        let n_docs = self.token_cache.len().max(1) as f64;
        let mut match_score = 0.0_f64;
        let mut total_weight = 0.0_f64;

        for qt in query_tokens {
            let df = self
                .token_cache
                .iter()
                .filter(|tokens| tokens.contains(qt))
                .count();
            let idf = if df > 0 {
                (n_docs / df as f64).ln() + 1.0
            } else {
                0.0
            };

            total_weight += idf;
            if tool_tokens.contains(qt) {
                match_score += idf;
            }
        }

        if total_weight > 0.0 {
            match_score / total_weight
        } else {
            0.0
        }
    }

    /// Score all enabled plugin tools against query tokens.
    /// Returns `(plugin_index, tool_name, score)` sorted descending by score.
    pub fn score_all(&self, query_tokens: &[String]) -> Vec<(usize, String, f64)> {
        let mut results: Vec<_> = self
            .tools
            .iter()
            .enumerate()
            .filter(|(_, t)| t.enabled)
            .map(|(idx, t)| {
                let score = self.tfidf_score(query_tokens, idx);
                (idx, t.name.clone(), score)
            })
            .filter(|(_, _, score)| *score > 0.01)
            .collect();
        results.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
        results
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub fn get(&self, name: &str) -> Option<&PluginToolEntry> {
        self.tools.iter().find(|t| t.name == name)
    }

    /// Total estimated schema tokens for all enabled plugin tools.
    pub fn total_schema_tokens(&self) -> u32 {
        self.tools
            .iter()
            .filter(|t| t.enabled)
            .map(|t| t.schema_tokens)
            .sum()
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Tokenize a plugin entry's name + description + triggers for TF-IDF.
fn tokenize_entry(entry: &PluginToolEntry) -> Vec<String> {
    let mut tokens = Vec::new();
    for trigger in &entry.triggers {
        tokens.extend(text_tokenize::tokenize(trigger));
    }
    tokens.extend(text_tokenize::tokenize(&entry.description));
    tokens.extend(text_tokenize::tokenize(&entry.name));
    tokens
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_entry(name: &str, triggers: &[&str], desc: &str) -> PluginToolEntry {
        PluginToolEntry {
            name: name.to_string(),
            description: desc.to_string(),
            triggers: triggers.iter().map(|s| s.to_string()).collect(),
            pinned: false,
            intents: vec![IntentType::CodeRead],
            scope: Scope::Local,
            schema: json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": desc,
                    "parameters": {"type": "object", "properties": {}}
                }
            }),
            schema_tokens: 20,
            source: "test".to_string(),
            enabled: true,
        }
    }

    // ── Registration tests ──

    #[test]
    fn register_and_retrieve() {
        let mut reg = PluginRegistry::new();
        let entry = make_entry("my_tool", &["custom", "plugin"], "A custom plugin tool");
        assert!(reg.register(entry).is_ok());
        assert_eq!(reg.len(), 1);
        assert!(reg.get("my_tool").is_some());
    }

    #[test]
    fn reject_builtin_name_conflict() {
        let mut reg = PluginRegistry::new();
        let entry = make_entry("bash", &["shell"], "Conflicts with built-in bash");
        let result = reg.register(entry);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("conflicts with built-in"));
    }

    #[test]
    fn reject_duplicate_plugin_name() {
        let mut reg = PluginRegistry::new();
        let e1 = make_entry("my_tool", &["first"], "First");
        let e2 = make_entry("my_tool", &["second"], "Second");
        assert!(reg.register(e1).is_ok());
        let result = reg.register(e2);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("already registered"));
    }

    #[test]
    fn unregister_removes_tool() {
        let mut reg = PluginRegistry::new();
        reg.register(make_entry("my_tool", &["x"], "desc")).unwrap();
        assert_eq!(reg.len(), 1);
        assert!(reg.unregister("my_tool"));
        assert_eq!(reg.len(), 0);
        assert!(!reg.unregister("nonexistent"));
    }

    // ── Enable/disable tests ──

    #[test]
    fn toggle_enabled() {
        let mut reg = PluginRegistry::new();
        reg.register(make_entry("my_tool", &["x"], "desc")).unwrap();
        assert!(reg.get("my_tool").unwrap().enabled);

        reg.set_enabled("my_tool", false);
        assert!(!reg.get("my_tool").unwrap().enabled);
        assert_eq!(reg.enabled_tools().count(), 0);
        assert!(reg.schemas().is_empty());

        reg.set_enabled("my_tool", true);
        assert_eq!(reg.enabled_tools().count(), 1);
    }

    // ── TF-IDF scoring tests ──

    #[test]
    fn tfidf_scores_matching_query() {
        let mut reg = PluginRegistry::new();
        reg.register(make_entry(
            "kubectl_get",
            &["kubernetes", "kubectl", "pods", "k8s", "deployment"],
            "Get Kubernetes resources",
        ))
        .unwrap();
        reg.register(make_entry(
            "docker_ps",
            &["docker", "container", "image"],
            "List Docker containers",
        ))
        .unwrap();

        let query = text_tokenize::tokenize("show kubernetes pods");
        let scores = reg.score_all(&query);

        // kubectl_get should rank first for kubernetes query
        assert!(!scores.is_empty(), "should have results");
        assert_eq!(scores[0].1, "kubectl_get", "kubectl should rank first");
    }

    #[test]
    fn tfidf_returns_zero_for_unrelated() {
        let mut reg = PluginRegistry::new();
        reg.register(make_entry(
            "kubectl_get",
            &["kubernetes", "k8s"],
            "Kubernetes resources",
        ))
        .unwrap();

        let query = text_tokenize::tokenize("read a python file");
        let score = reg.tfidf_score(&query, 0);
        assert!(
            score < 0.01,
            "unrelated query should score near zero: {score:.4}"
        );
    }

    #[test]
    fn tfidf_cjk_triggers_work() {
        let mut reg = PluginRegistry::new();
        reg.register(make_entry(
            "mo_analytics",
            &["数据库", "分析", "统计", "database", "analytics"],
            "Database analytics and statistics",
        ))
        .unwrap();

        let query = text_tokenize::tokenize("数据库分析");
        let score = reg.tfidf_score(&query, 0);
        assert!(score > 0.0, "CJK query should match: {score:.4}");
    }

    // ── Schema collection tests ──

    #[test]
    fn schemas_only_includes_enabled() {
        let mut reg = PluginRegistry::new();
        reg.register(make_entry("a", &["x"], "Tool A")).unwrap();
        reg.register(make_entry("b", &["y"], "Tool B")).unwrap();
        assert_eq!(reg.schemas().len(), 2);

        reg.set_enabled("a", false);
        assert_eq!(reg.schemas().len(), 1);
    }

    #[test]
    fn total_schema_tokens_sums_enabled() {
        let mut reg = PluginRegistry::new();
        let mut e1 = make_entry("a", &["x"], "A");
        e1.schema_tokens = 50;
        let mut e2 = make_entry("b", &["y"], "B");
        e2.schema_tokens = 30;
        reg.register(e1).unwrap();
        reg.register(e2).unwrap();

        assert_eq!(reg.total_schema_tokens(), 80);
        reg.set_enabled("a", false);
        assert_eq!(reg.total_schema_tokens(), 30);
    }

    // ── Edge cases ──

    #[test]
    fn empty_registry_scores_safely() {
        let reg = PluginRegistry::new();
        let query = text_tokenize::tokenize("anything");
        let scores = reg.score_all(&query);
        assert!(scores.is_empty());
    }

    #[test]
    fn tfidf_out_of_bounds_returns_zero() {
        let reg = PluginRegistry::new();
        let query = text_tokenize::tokenize("test");
        assert_eq!(reg.tfidf_score(&query, 999), 0.0);
    }

    #[test]
    fn set_enabled_unknown_tool_returns_false() {
        let mut reg = PluginRegistry::new();
        assert!(!reg.set_enabled("nonexistent", true));
    }
}
