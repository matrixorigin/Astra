use std::sync::Arc;

use serde_json::Value;

use astra_config::ToolSurfaceConfig;

use super::DEFAULT_TOOL_SCHEMA_BUDGET_TOKENS;
use astra_turn_core::tool::schema::tool_schema_name;
use astra_turn_core::tool_registry_meta::{TOOL_CATALOG, ToolMeta};
use astra_turn_core::tool_registry_report::ToolSelectionReport;

fn sort_schemas_by_name(schemas: &mut [Value]) {
    schemas.sort_by(|a, b| {
        let a_name = tool_schema_name(a).unwrap_or("");
        let b_name = tool_schema_name(b).unwrap_or("");
        a_name.cmp(b_name)
    });
}

fn split_ascii_words(text: &str) -> Vec<&str> {
    text.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|word| !word.is_empty())
        .collect()
}

fn is_pure_conversational_query(query: &str) -> bool {
    const CONVERSATIONAL_THRESHOLD_CHARS: usize = 20;
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return true;
    }

    let lower = trimmed.to_lowercase();
    let has_content = lower
        .chars()
        .any(|c| c.is_alphanumeric() || ('\u{4e00}'..='\u{9fff}').contains(&c));
    if !has_content {
        return true;
    }
    if lower.chars().count() > CONVERSATIONAL_THRESHOLD_CHARS {
        return false;
    }

    const CONVERSATIONAL_CN: &[&str] = &["你好", "谢谢", "再见", "好的", "是的", "不是", "嗯"];
    let compact_cjk: String = lower
        .chars()
        .filter(|c| c.is_alphanumeric() || ('\u{4e00}'..='\u{9fff}').contains(c))
        .collect();
    if CONVERSATIONAL_CN
        .iter()
        .any(|phrase| compact_cjk == *phrase)
    {
        return true;
    }

    const CONVERSATIONAL_EN: &[&str] = &[
        "hello",
        "hi",
        "hey",
        "thanks",
        "thank you",
        "bye",
        "goodbye",
        "yes",
        "no",
        "ok",
        "okay",
        "sure",
        "yep",
        "nope",
    ];
    let words = split_ascii_words(&lower);
    CONVERSATIONAL_EN.iter().any(|phrase| {
        let phrase_words = split_ascii_words(phrase);
        words == phrase_words
            || (phrase_words.len() == 1
                && matches!(phrase_words[0], "hello" | "hi" | "hey")
                && words == [phrase_words[0], "there"])
    })
}

/// The main tool surface interface.
///
/// ```text
/// let registry = ToolRegistry::new(all_tool_schemas());
/// let surface = registry.build_initial_surface("matrixorigin memoria最新的pr?");
/// // surface contains the stable always_load tools; deferred tools are activated explicitly
/// ```
pub struct ToolRegistry {
    all_schemas: Vec<Value>,
    schema_budget_tokens: u32,
    /// Real token costs measured from actual schemas (schema JSON bytes / 4).
    /// Maps tool name → measured token cost.
    measured_costs: std::collections::HashMap<String, u32>,
    /// O(1) lookup: tool name → index into all_schemas.
    schema_index: std::collections::HashMap<String, usize>,
    /// Pre-resolved always_load tool schemas (cloned once at construction).
    always_load_schemas: Vec<(String, Value)>,
    /// Pre-sorted always_load schemas for `always_load_only()` — shared via Arc
    /// to avoid ~5-8KB clone per conversational turn. Atomically replaced when
    /// runtime-injected schemas change.
    always_load_sorted: Arc<Vec<Value>>,
    /// Cached set of always_load tool names, rebuilt alongside `always_load_sorted`.
    /// Avoids reconstructing a `HashSet<String>` (cloning ~14 names) on every
    /// selection path — 2-3 calls per turn previously.
    always_load_name_cache: std::collections::HashSet<String>,
}

impl ToolRegistry {
    pub fn new(all_schemas: Vec<Value>) -> Self {
        Self::new_with_surface_config(all_schemas, None)
    }

    pub fn new_runtime_surface(all_schemas: Vec<Value>) -> Self {
        let cfg = astra_config::runtime_config::RuntimeConfig::cached()
            .tool_surface
            .clone();
        Self::new_with_surface_config(all_schemas, Some(&cfg))
    }

    pub fn new_with_tool_surface(all_schemas: Vec<Value>, surface_cfg: &ToolSurfaceConfig) -> Self {
        Self::new_with_surface_config(all_schemas, Some(surface_cfg))
    }

    fn new_with_surface_config(
        all_schemas: Vec<Value>,
        surface_cfg: Option<&ToolSurfaceConfig>,
    ) -> Self {
        let all_schemas: Vec<Value> = all_schemas
            .into_iter()
            .filter(|schema| tool_schema_name(schema).is_some())
            .collect();
        let measured_costs = Self::measure_all_schemas(&all_schemas);
        let schema_index = Self::build_schema_index(&all_schemas);
        let always_load_schemas =
            Self::resolve_always_load(&all_schemas, &schema_index, surface_cfg);
        let mut always_load_sorted: Vec<Value> =
            always_load_schemas.iter().map(|(_, s)| s.clone()).collect();
        sort_schemas_by_name(&mut always_load_sorted);
        let always_load_name_cache: std::collections::HashSet<String> = always_load_schemas
            .iter()
            .map(|(name, _)| name.clone())
            .collect();
        Self {
            all_schemas,
            schema_budget_tokens: DEFAULT_TOOL_SCHEMA_BUDGET_TOKENS,
            measured_costs,
            schema_index,
            always_load_schemas,
            always_load_sorted: Arc::new(always_load_sorted),
            always_load_name_cache,
        }
    }

    pub fn with_schema_budget(mut self, budget: u32) -> Self {
        self.schema_budget_tokens = budget;
        self
    }

    /// Get the configured default schema-token budget used for surface telemetry.
    pub fn default_schema_budget(&self) -> u32 {
        self.schema_budget_tokens
    }

    /// Access the full list of tool schemas.
    pub fn all_tool_schemas(&self) -> &[Value] {
        &self.all_schemas
    }

    /// Measure actual token cost of each tool schema from real JSON.
    /// Uses JSON bytes / 4 as the token approximation.
    fn measure_all_schemas(schemas: &[Value]) -> std::collections::HashMap<String, u32> {
        let mut costs = std::collections::HashMap::new();
        for schema in schemas {
            if let Some(name) = tool_schema_name(schema) {
                let json_bytes = serde_json::to_string(schema).map(|s| s.len()).unwrap_or(0);
                costs.insert(name.to_string(), (json_bytes / 4) as u32);
            }
        }
        costs
    }

    /// Build name → index map for O(1) schema lookup.
    fn build_schema_index(schemas: &[Value]) -> std::collections::HashMap<String, usize> {
        schemas
            .iter()
            .enumerate()
            .filter_map(|(i, s)| tool_schema_name(s).map(|name| (name.to_string(), i)))
            .collect()
    }

    /// Pre-resolve always_load tool schemas once at construction.
    fn resolve_always_load(
        schemas: &[Value],
        index: &std::collections::HashMap<String, usize>,
        surface_cfg: Option<&ToolSurfaceConfig>,
    ) -> Vec<(String, Value)> {
        let default_cfg;
        let cfg = match surface_cfg {
            Some(cfg) => cfg,
            None => {
                default_cfg = ToolSurfaceConfig::default();
                &default_cfg
            }
        };
        super::surface::ToolSurface::build(schemas.to_vec(), cfg, &[])
            .always_load_schemas()
            .into_iter()
            .filter_map(|schema| {
                let name = tool_schema_name(&schema).map(str::to_string);
                name.and_then(|name| {
                    index.get(&name)?;
                    Some((name, schema))
                })
            })
            .collect()
    }

    /// O(1) schema lookup by tool name.
    pub fn schema_by_name(&self, name: &str) -> Option<&Value> {
        self.schema_index.get(name).map(|&i| &self.all_schemas[i])
    }

    /// Pre-resolved always_load schemas (name, schema) — cloned once at construction.
    pub fn always_load_schemas(&self) -> &[(String, Value)] {
        &self.always_load_schemas
    }

    /// Return the resolved always_load tool names in stable order for cross-crate
    /// wire metadata. The internal cache remains a set because hot-path
    /// membership checks should stay O(1).
    pub fn always_load_tool_names_sorted(&self) -> Vec<String> {
        let mut names: Vec<String> = self.always_load_name_cache.iter().cloned().collect();
        names.sort();
        names
    }

    /// Total measured token cost of all always_load tool schemas.
    /// Used for accurate overhead estimation in budget pressure calculation.
    pub fn total_always_load_token_cost(&self) -> u32 {
        self.always_load_schemas
            .iter()
            .map(|(name, _)| self.token_cost(name))
            .sum()
    }

    /// Get measured token cost for a tool, falling back to catalog estimate.
    pub fn token_cost(&self, name: &str) -> u32 {
        self.measured_costs.get(name).copied().unwrap_or_else(|| {
            ToolRegistry::get_meta(name)
                .map(|m| m.schema_tokens)
                .unwrap_or(40)
        })
    }

    /// Build the visible tool surface for a given user query and conversation turn.
    ///
    /// Returns tool schemas to include in the LLM request.
    /// AlwaysLoad tools are included deterministically. Non-always_load built-ins are
    /// deferred and must be activated explicitly through `tool_search`.
    pub fn build_initial_surface(&self, query: &str) -> Vec<Value> {
        let (schemas, _report) =
            self.build_initial_surface_with_report(query, self.schema_budget_tokens);
        schemas
    }

    /// Build a tool surface with a custom schema-token budget, returning both schemas and a report.
    pub fn build_initial_surface_with_report(
        &self,
        query: &str,
        schema_budget: u32,
    ) -> (Vec<Value>, ToolSelectionReport) {
        self.build_initial_surface_with_report_ctx(query, schema_budget, &[])
    }

    /// Build a tool surface with context from recent turns.
    pub fn build_initial_surface_with_report_ctx(
        &self,
        query: &str,
        schema_budget: u32,
        recent_tools: &[String],
    ) -> (Vec<Value>, ToolSelectionReport) {
        // Conversational short-circuit: pure greetings/acks need no tools. If
        // recent tools exist, preserve tool continuity for follow-up turns.
        if recent_tools.is_empty() && is_pure_conversational_query(query) {
            let report = ToolSelectionReport {
                visible_tools: Vec::new(),
                visible_count: 0,
                schema_budget_used: 0,
                schema_budget_total: schema_budget,
            };
            return (Vec::new(), report);
        }

        let schemas = self.always_load_only();
        let names = Self::visible_names(schemas.as_ref());

        let report = ToolSelectionReport {
            visible_count: schemas.as_ref().len() as u32,
            visible_tools: names,
            schema_budget_used: 0,
            schema_budget_total: schema_budget,
        };

        (schemas.as_ref().clone(), report)
    }

    /// Pipeline-integrated tool surface using a pre-computed RoutingDecision.
    ///
    /// Routing decides whether this is a tool-bearing turn. The built-in
    /// surface is deterministic: only always_load schemas are returned. Deferred
    /// tools stay deferred until explicitly activated via `tool_search`.
    pub fn build_routed_surface(&self, schema_budget: u32) -> (Vec<Value>, ToolSelectionReport) {
        let schemas = self.always_load_only();
        let names = Self::visible_names(schemas.as_ref());
        (
            schemas.as_ref().clone(),
            ToolSelectionReport {
                visible_count: names.len() as u32,
                visible_tools: names,
                schema_budget_used: 0,
                schema_budget_total: schema_budget,
            },
        )
    }

    /// Return only always_load tools.
    pub fn always_load_only(&self) -> Arc<Vec<Value>> {
        Arc::clone(&self.always_load_sorted)
    }

    fn rebuild_always_load_sorted(&mut self) {
        let mut sorted: Vec<Value> = self
            .always_load_schemas
            .iter()
            .map(|(_, s)| s.clone())
            .collect();
        sort_schemas_by_name(&mut sorted);
        self.always_load_sorted = Arc::new(sorted);
        self.always_load_name_cache = self
            .always_load_schemas
            .iter()
            .map(|(name, _)| name.clone())
            .collect();
    }

    /// Return ALL tool schemas (bypass selection — used for tool execution).
    pub fn all_schemas(&self) -> &[Value] {
        &self.all_schemas
    }

    /// Return all tool names from the catalog.
    pub fn all_tool_names() -> Vec<&'static str> {
        TOOL_CATALOG.iter().map(|t| t.name).collect()
    }

    /// Return all tool names from THIS registry instance (includes
    /// runtime-surface-only schemas like skill, etc.).
    pub fn all_schema_names(&self) -> Vec<String> {
        self.all_schemas
            .iter()
            .filter_map(|s| tool_schema_name(s).map(String::from))
            .collect()
    }

    /// Return names of currently visible tools.
    pub fn visible_names(schemas: &[Value]) -> Vec<String> {
        schemas
            .iter()
            .filter_map(|s| tool_schema_name(s).map(String::from))
            .collect()
    }

    /// Get metadata for a tool by name.
    pub fn get_meta(name: &str) -> Option<&'static ToolMeta> {
        TOOL_CATALOG.iter().find(|t| t.name == name)
    }

    /// Register plugin tools from a PluginRegistry.
    ///
    /// Plugin schemas become **lookup-able** (the executor can dispatch them
    /// and `tool_search(select:NAME)` can return them), but lookup alone does
    /// not place them in the visible `tools[]` surface. Plugins default to the
    /// deferred listing. Callers that need a plugin in `tools[]` must build the
    /// visible surface with that plugin explicitly always_load.
    ///
    /// This keeps the Anthropic prompt-cache prefix byte-stable across
    /// plugin registration: schemas are added to lookup/execution indexes,
    /// but the selected `tools[]` surface remains always_load-only.
    pub fn register_plugins(
        &mut self,
        plugins: &astra_turn_core::tool_registry_plugin::PluginRegistry,
    ) {
        let plugin_schemas = plugins.schemas();
        if plugin_schemas.is_empty() {
            return;
        }
        // Plugins are looked up by name for executor dispatch and
        // `tool_search(select:NAME)`. They live in the deferred listing unless
        // the caller builds a always_load visible surface for them.
        self.all_schemas.extend(
            plugin_schemas
                .into_iter()
                .filter(|schema| tool_schema_name(schema).is_some()),
        );
        // Rebuild indexes to include the new schemas
        self.measured_costs = Self::measure_all_schemas(&self.all_schemas);
        self.schema_index = Self::build_schema_index(&self.all_schemas);
    }

    /// Inject a single tool schema dynamically (e.g. a session-local plugin tool).
    ///
    /// When `always_load` is true the tool is budget-exempt (always included like
    /// core tools such as `bash` and `read_file`). When false it is lookupable
    /// by name but does not enter `tools[]` unless explicitly reinserted as always_load.
    pub fn inject_schema(&mut self, schema: Value) {
        self.inject_schema_always_load(schema, true);
    }

    /// Inject with explicit load policy control.
    pub fn inject_schema_always_load(&mut self, schema: Value, always_load: bool) {
        if let Some(name) = tool_schema_name(&schema) {
            if self.schema_index.contains_key(name) {
                return;
            }
            let name_owned = name.to_string();
            let idx = self.all_schemas.len();
            let json_bytes = serde_json::to_string(&schema).map(|s| s.len()).unwrap_or(0);
            self.measured_costs
                .insert(name_owned.clone(), (json_bytes / 4) as u32);
            self.schema_index.insert(name_owned.clone(), idx);
            if always_load {
                self.always_load_schemas.push((name_owned, schema.clone()));
                self.rebuild_always_load_sorted();
            }
            self.all_schemas.push(schema);
        }
    }

    /// Insert a new schema or replace an existing schema with the same tool name.
    pub fn upsert_schema_always_load(&mut self, schema: Value, always_load: bool) {
        let Some(name) = tool_schema_name(&schema).map(str::to_string) else {
            return;
        };

        let json_bytes = serde_json::to_string(&schema).map(|s| s.len()).unwrap_or(0);
        self.measured_costs
            .insert(name.clone(), (json_bytes / 4) as u32);

        if let Some(&idx) = self.schema_index.get(&name) {
            self.all_schemas[idx] = schema.clone();
            if let Some((_, existing)) = self
                .always_load_schemas
                .iter_mut()
                .find(|(n, _)| n == &name)
            {
                *existing = schema;
            } else if always_load {
                self.always_load_schemas.push((name, schema));
            }
            self.rebuild_always_load_sorted();
            return;
        }

        self.inject_schema_always_load(schema, always_load);
    }

    /// Insert or replace a always_load schema by tool name.
    pub fn upsert_schema(&mut self, schema: Value) {
        self.upsert_schema_always_load(schema, true);
    }

    /// Total tool count (built-in + registered plugins).
    pub fn total_tool_count(&self) -> usize {
        self.all_schemas.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_schema(name: &str) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": name,
                "description": format!("Tool {name}"),
                "parameters": { "type": "object", "properties": {} }
            }
        })
    }

    #[test]
    fn inject_schema_adds_to_registry() {
        let mut reg = ToolRegistry::new(vec![sample_schema("bash")]);
        assert_eq!(reg.total_tool_count(), 1);
        assert!(reg.schema_by_name("bash").is_some());
        assert!(reg.schema_by_name("skill").is_none());

        reg.inject_schema(sample_schema("skill"));

        assert_eq!(reg.total_tool_count(), 2);
        assert!(reg.schema_by_name("skill").is_some());
        assert!(reg.token_cost("skill") > 0);
    }

    #[test]
    fn inject_schema_is_idempotent() {
        let mut reg = ToolRegistry::new(vec![sample_schema("bash")]);
        reg.inject_schema(sample_schema("skill"));
        reg.inject_schema(sample_schema("skill"));
        assert_eq!(reg.total_tool_count(), 2);
    }

    #[test]
    fn registry_drops_custom_type_but_accepts_function_shorthand_at_construction() {
        let reg = ToolRegistry::new(vec![
            sample_schema("bash"),
            json!({"type": "custom", "function": {"name": "custom_shape"}}),
            json!({"function": {"name": "missing_type"}}),
        ]);

        assert_eq!(reg.total_tool_count(), 2);
        assert_eq!(
            reg.all_schema_names(),
            vec!["bash".to_string(), "missing_type".to_string()]
        );
        assert!(reg.schema_by_name("custom_shape").is_none());
        assert!(reg.schema_by_name("missing_type").is_some());
    }

    #[test]
    fn inject_schema_ignores_malformed_and_accepts_function_shorthand() {
        let mut reg = ToolRegistry::new(vec![sample_schema("bash")]);
        reg.inject_schema(json!({"broken": true}));
        reg.inject_schema(json!({"type": "custom", "function": {"name": "custom_shape"}}));
        reg.inject_schema(json!({"function": {"name": "missing_type"}}));
        assert_eq!(reg.total_tool_count(), 2);
        assert!(reg.schema_by_name("custom_shape").is_none());
        assert!(reg.schema_by_name("missing_type").is_some());
    }

    #[test]
    fn upsert_schema_ignores_custom_type_and_accepts_function_shorthand() {
        let mut reg = ToolRegistry::new(vec![sample_schema("bash")]);
        reg.upsert_schema(json!({"type": "custom", "function": {"name": "custom_shape"}}));
        reg.upsert_schema(json!({"function": {"name": "missing_type"}}));

        assert_eq!(reg.total_tool_count(), 2);
        assert!(reg.schema_by_name("custom_shape").is_none());
        assert!(reg.schema_by_name("missing_type").is_some());
    }

    #[test]
    fn injected_schema_is_always_load_by_default() {
        let mut reg = ToolRegistry::new(vec![sample_schema("bash")]);
        reg.inject_schema(sample_schema("skill"));
        // Default inject is always_load (budget-exempt)
        assert!(reg.always_load_schemas.iter().any(|(n, _)| n == "skill"));
    }

    #[test]
    fn injected_schema_deferred_is_lookupable_but_not_selected() {
        let mut reg = ToolRegistry::new(vec![sample_schema("bash")]);
        reg.inject_schema_always_load(sample_schema("skill"), false);
        assert!(reg.schema_by_name("skill").is_some());
        assert!(!reg.always_load_schemas.iter().any(|(n, _)| n == "skill"));

        let (selected, report) = reg.build_initial_surface_with_report("use the skill tool", 800);
        let names = ToolRegistry::visible_names(&selected);
        assert!(
            !names.contains(&"skill".to_string()),
            "deferred injected tools must not enter the visible surface by lookup alone"
        );
        assert_eq!(report.schema_budget_used, 0);
    }

    #[test]
    fn always_load_only_includes_injected_always_load_tool() {
        let mut reg = ToolRegistry::new(vec![sample_schema("bash")]);
        reg.inject_schema(sample_schema("skill"));
        let always_load = reg.always_load_only();
        let names: Vec<&str> = always_load
            .iter()
            .filter_map(|s| {
                s.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
            })
            .collect();
        assert!(
            names.contains(&"skill"),
            "always_load_only should include injected always_load tools"
        );
    }

    #[test]
    fn new_with_tool_surface_honors_runtime_always_load_additions() {
        let schemas = vec![
            sample_schema("bash"),
            sample_schema("grep"),
            sample_schema("github"),
            sample_schema("tool_search"),
            sample_schema("skill"),
        ];
        let cfg = ToolSurfaceConfig {
            pinned_tools: vec!["github".into()],
        };

        let reg = ToolRegistry::new_with_tool_surface(schemas, &cfg);
        let always_load_names: Vec<String> = reg
            .always_load_schemas
            .iter()
            .map(|(name, _)| name.clone())
            .collect();

        assert!(always_load_names.iter().any(|name| name == "github"));
        assert!(
            always_load_names.iter().any(|name| name == "grep"),
            "default always_load declarations must stay in the resolved runtime surface"
        );
        assert_eq!(
            reg.always_load_tool_names_sorted(),
            {
                let mut names = always_load_names.clone();
                names.sort();
                names
            },
            "resolved always_load names must be stable and reflect runtime additions"
        );
    }

    #[test]
    fn custom_always_load_tool_is_budget_exempt_in_surface_report() {
        let schemas = vec![
            sample_schema("bash"),
            sample_schema("tool_search"),
            sample_schema("web_fetch"),
        ];
        let cfg = ToolSurfaceConfig {
            pinned_tools: vec!["web_fetch".into()],
        };
        let reg = ToolRegistry::new_with_tool_surface(schemas, &cfg);

        let (selected, report) = reg.build_initial_surface_with_report("fetch this web page", 0);
        let names = ToolRegistry::visible_names(&selected);

        assert!(names.contains(&"web_fetch".to_string()));
        assert_eq!(
            report.schema_budget_used, 0,
            "user-always_load tools are part of this registry's always_load surface and must not consume deferred-tool budget"
        );
    }

    #[test]
    fn unknown_always_load_config_does_not_remove_default_tool() {
        let schemas = vec![
            sample_schema("bash"),
            sample_schema("grep"),
            sample_schema("tool_search"),
        ];
        let cfg = ToolSurfaceConfig {
            pinned_tools: vec!["not_a_real_tool".into()],
        };
        let reg = ToolRegistry::new_with_tool_surface(schemas, &cfg);

        let (selected, report) =
            reg.build_initial_surface_with_report("grep for UserSession in the code", 0);
        let names = ToolRegistry::visible_names(&selected);

        assert!(
            reg.always_load_schemas
                .iter()
                .any(|(name, _)| name == "grep"),
            "unknown always_load_tools entries must not remove default always_load tools"
        );
        assert!(
            names.contains(&"grep".to_string()),
            "default always_load tools stay visible even with zero deferred budget: {names:?}"
        );
        assert_eq!(
            report.schema_budget_used, 0,
            "always_load tools must not consume deferred-tool budget"
        );
    }

    // ── Tool surface observability integration (Pass B) ──
    //
    // These tests verify the real `build_initial_surface_with_report_ctx` path. They exercise
    // both the conversational short-circuit and the non-conversational
    // always_load-only branch.
    // Stderr content is not captured (no stable API in tokio tests),
    // but the flag guard ensures the hot path runs without panic
    // when observability is on — which is what we'd regress if the
    // obs module grew a lifetime bug or serde panic.

    #[test]
    fn build_initial_surface_with_report_ctx_conversational_path_does_not_panic_with_obs_on() {
        let mut schemas = vec![sample_schema("bash"), sample_schema("read_file")];
        // Ensure a always_load + non-always_load mix so construction keeps the deferred
        // schema lookupable while the conversational path returns no tools.
        schemas.push(sample_schema("github"));
        let registry = ToolRegistry::new(schemas);
        // "hello" is the conversational short-circuit case.
        let (out_schemas, report) =
            registry.build_initial_surface_with_report_ctx("hello", 800, &[]);
        assert!(out_schemas.is_empty());
        assert_eq!(report.visible_count as usize, out_schemas.len());
    }

    #[test]
    fn conversational_without_recent_tools_shortcircuits() {
        let schemas: Vec<Value> = TOOL_CATALOG.iter().map(|t| sample_schema(t.name)).collect();
        let registry = ToolRegistry::new(schemas);
        // "谢谢" with no recent_tools → should short-circuit to no tools.
        let (out, report) = registry.build_initial_surface_with_report_ctx("谢谢", 800, &[]);
        assert_eq!(
            report.visible_count, 0,
            "conversational + no recent_tools should return no tools"
        );
        assert!(
            out.is_empty(),
            "pure conversational turns should be tool-free"
        );
    }

    #[test]
    fn conversational_shortcut_requires_pure_ack_or_greeting() {
        assert!(is_pure_conversational_query("hello there"));
        assert!(is_pure_conversational_query("谢谢"));
        assert!(!is_pure_conversational_query("hi fix the tests"));
        assert!(!is_pure_conversational_query("你好请修复测试"));
    }

    #[test]
    fn conversational_with_recent_tools_preserves_tool_surface() {
        let schemas: Vec<Value> = TOOL_CATALOG.iter().map(|t| sample_schema(t.name)).collect();
        let registry = ToolRegistry::new(schemas);
        let recent_tools = vec!["read_file".to_string()];

        let (out, report) =
            registry.build_initial_surface_with_report_ctx("hello", 800, &recent_tools);

        assert!(!out.is_empty());
        assert_eq!(report.visible_count as usize, out.len());
    }

    #[test]
    fn build_initial_surface_with_report_ctx_non_conversational_path_returns_always_load_tools() {
        let schemas = vec![
            sample_schema("bash"),
            sample_schema("read_file"),
            sample_schema("grep"),
            sample_schema("list_dir"),
        ];
        let registry = ToolRegistry::new(schemas);
        // Analytical-ish query forces the non-conversational always_load-only path.
        let (out, report) = registry.build_initial_surface_with_report_ctx(
            "search for TODO in source files",
            800,
            &[],
        );
        assert!(!out.is_empty());
        assert!(report.visible_count > 0);
    }
}

#[cfg(test)]
mod always_load_budget_tests {
    use super::*;
    use serde_json::json;
}
