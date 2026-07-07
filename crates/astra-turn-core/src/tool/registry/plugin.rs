//! Dynamic tool registration for plugins and skill manifests.
//!
//! Complements the static `TOOL_CATALOG` with runtime-registerable tools.
//! Plugin tools can be enabled/disabled per session and are loaded from skill
//! manifests.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::tool::registry::meta::{IntentType, Scope, TOOL_CATALOG};

// ─── Plugin Tool Entry ──────────────────────────────────────────────────────

/// Owned tool metadata for dynamically registered tools.
/// Unlike static `ToolMeta` (which uses `&'static str`), this supports runtime creation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginToolEntry {
    pub name: String,
    pub description: String,
    pub triggers: Vec<String>,
    pub always_load: bool,
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
/// - Toggleable: tools can be enabled/disabled per session
#[derive(Debug, Default)]
pub struct PluginRegistry {
    tools: Vec<PluginToolEntry>,
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

        self.tools.push(entry);
        Ok(())
    }

    /// Unregister a plugin tool by name. Returns true if found and removed.
    pub fn unregister(&mut self, name: &str) -> bool {
        if let Some(idx) = self.tools.iter().position(|t| t.name == name) {
            self.tools.remove(idx);
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
            always_load: false,
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
    fn set_enabled_unknown_tool_returns_false() {
        let mut reg = PluginRegistry::new();
        assert!(!reg.set_enabled("nonexistent", true));
    }
}
