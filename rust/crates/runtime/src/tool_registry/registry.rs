use serde_json::Value;

use super::scoring::{
    DEFAULT_TOOL_BUDGET_TOKENS, pre_filter_dynamic, pre_filter_dynamic_with_memory,
    pre_filter_dynamic_with_outcome_bias, pre_filter_dynamic_with_quality,
};
use crate::pipeline::routing::{RoutingDecision, ToolFilter};
use astra_turn_core::routing_metrics::ConfidenceCalibrator;
use astra_turn_core::tool_registry_meta::{TOOL_CATALOG, ToolMeta};
use astra_turn_core::tool_registry_report::{SelectionReport, ToolQualityTracker};
use astra_turn_core::tool_registry_state::ConversationState;

use super::tool_pool::sort_schemas_by_name;

/// The main tool selection interface.
///
/// ```text
/// let registry = ToolRegistry::new(all_tool_schemas());
/// let selected = registry.select("matrixorigin memoria最新的pr?", 1);
/// // selected contains: 7 pinned + relevant dynamic tools within budget
/// ```
pub struct ToolRegistry {
    all_schemas: Vec<Value>,
    budget_tokens: u32,
    /// Real token costs measured from actual schemas (schema JSON bytes / 4).
    /// Maps tool name → measured token cost.
    measured_costs: std::collections::HashMap<String, u32>,
    /// O(1) lookup: tool name → index into all_schemas.
    schema_index: std::collections::HashMap<String, usize>,
    /// Pre-resolved pinned tool schemas (cloned once at construction).
    pinned_schemas: Vec<(String, Value)>,
    /// Plugin tool names (registered dynamically, always included in selection).
    plugin_tool_names: Vec<String>,
}

impl ToolRegistry {
    pub fn new(all_schemas: Vec<Value>) -> Self {
        let measured_costs = Self::measure_all_schemas(&all_schemas);
        let schema_index = Self::build_schema_index(&all_schemas);
        let pinned_schemas = Self::resolve_pinned(&all_schemas, &schema_index);
        Self {
            all_schemas,
            budget_tokens: DEFAULT_TOOL_BUDGET_TOKENS,
            measured_costs,
            schema_index,
            pinned_schemas,
            plugin_tool_names: Vec::new(),
        }
    }

    pub fn with_budget(mut self, budget: u32) -> Self {
        self.budget_tokens = budget;
        self
    }

    /// Get the configured default token budget.
    pub fn default_budget(&self) -> u32 {
        self.budget_tokens
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
            if let Some(name) = schema
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
            {
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
            .filter_map(|(i, s)| {
                s.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .map(|name| (name.to_string(), i))
            })
            .collect()
    }

    /// Pre-resolve pinned tool schemas once at construction.
    fn resolve_pinned(
        schemas: &[Value],
        index: &std::collections::HashMap<String, usize>,
    ) -> Vec<(String, Value)> {
        TOOL_CATALOG
            .iter()
            .filter(|t| t.pinned)
            .filter_map(|t| {
                index
                    .get(t.name)
                    .map(|&i| (t.name.to_string(), schemas[i].clone()))
            })
            .collect()
    }

    /// O(1) schema lookup by tool name.
    pub fn schema_by_name(&self, name: &str) -> Option<&Value> {
        self.schema_index.get(name).map(|&i| &self.all_schemas[i])
    }

    /// Pre-resolved pinned schemas (name, schema) — cloned once at construction.
    pub fn pinned_schemas(&self) -> &[(String, Value)] {
        &self.pinned_schemas
    }

    /// Total measured token cost of all pinned tool schemas.
    /// Used for accurate overhead estimation in budget pressure calculation.
    pub fn total_pinned_token_cost(&self) -> u32 {
        self.pinned_schemas
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

    /// Select tools for a given user query and conversation turn.
    ///
    /// Returns tool schemas to include in the LLM request.
    /// Pinned tools are always included. Dynamic tools are ranked by
    /// relevance and added within the token budget.
    pub fn select(&self, query: &str, turn_count: u32) -> Vec<Value> {
        self.select_with_budget(query, turn_count, self.budget_tokens)
    }

    /// Select tools with a custom token budget, returning both schemas and a report.
    pub fn select_with_report(
        &self,
        query: &str,
        turn_count: u32,
        budget: u32,
    ) -> (Vec<Value>, SelectionReport) {
        self.select_with_report_ctx(query, turn_count, budget, &[])
    }

    /// Select tools with context from recent turns (for recency boost).
    pub fn select_with_report_ctx(
        &self,
        query: &str,
        turn_count: u32,
        budget: u32,
        recent_tools: &[String],
    ) -> (Vec<Value>, SelectionReport) {
        use astra_turn_core::selector_observability as obs;

        let state = ConversationState::from_message_with_context(query, turn_count, recent_tools);

        // Conversational short-circuit: skip dynamic ranking for greetings/acks.
        // BUT: if recent_tools is non-empty, the session has active tool context
        // and the next turn likely needs related tools (e.g., memory_retrieve
        // after memory_store). Don't short-circuit in that case.
        if state.is_conversational
            && !state.is_fetch
            && !state.is_mutate
            && !state.is_analytical
            && !state.references_history
            && state.recent_tools.is_empty()
        {
            let schemas = self.pinned_only();
            let names = Self::selected_names(&schemas);
            obs::emit_selector_trace(&obs::SelectionTrace {
                query,
                mode: "conversational",
                top: None,
                r#final: &names,
                budget: None,
                reason: Some("conversational query — pinned-only (no active tool context)"),
            });
            let report = SelectionReport {
                tools_selected: names,
                selected_count: schemas.len() as u32,
                budget_used: 0,
                budget_total: budget,
            };
            return (schemas, report);
        }

        let ranked = pre_filter_dynamic(&state, query);
        let schemas = self.budget_select_measured(&ranked, budget);
        let names = Self::selected_names(&schemas);

        let budget_used: u32 = names
            .iter()
            .filter(|n| {
                !TOOL_CATALOG
                    .iter()
                    .any(|t| t.pinned && t.name == n.as_str())
            })
            .map(|n| self.token_cost(n))
            .sum();

        // Observability: dynamic path — record the top-ranked
        // candidates before budget trimming so a reviewer can see
        // WHY a tool was excluded (scored too low vs. priced out).
        // Cap the top list at 10 to keep lines bounded.
        if obs::is_selector_observability_enabled() {
            let top: Vec<(&str, f64)> = ranked
                .iter()
                .take(10)
                .map(|(idx, score)| (TOOL_CATALOG[*idx].name, *score))
                .collect();
            obs::emit_selector_trace(&obs::SelectionTrace {
                query,
                mode: "dynamic",
                top: Some(top),
                r#final: &names,
                budget: Some(obs::SelectionBudget {
                    used: budget_used,
                    total: budget,
                }),
                reason: None,
            });
        }

        let report = SelectionReport {
            selected_count: schemas.len() as u32,
            tools_selected: names,
            budget_used,
            budget_total: budget,
        };

        (schemas, report)
    }

    /// Select tools with a custom token budget.
    pub fn select_with_budget(&self, query: &str, turn_count: u32, budget: u32) -> Vec<Value> {
        let (schemas, _report) = self.select_with_report(query, turn_count, budget);
        schemas
    }

    /// Select tools with quality-aware scoring: uses historical effectiveness
    /// data from the tracker to boost/penalize tool rankings.
    pub fn select_with_quality(
        &self,
        query: &str,
        turn_count: u32,
        budget: u32,
        recent_tools: &[String],
        quality_tracker: Option<&ToolQualityTracker>,
    ) -> (Vec<Value>, SelectionReport) {
        use astra_turn_core::selector_observability as obs;

        let state = ConversationState::from_message_with_context(query, turn_count, recent_tools);

        if state.is_conversational
            && !state.is_fetch
            && !state.is_mutate
            && !state.is_analytical
            && !state.references_history
            && state.recent_tools.is_empty()
        {
            let schemas = self.pinned_only();
            let names = Self::selected_names(&schemas);
            obs::emit_selector_trace(&obs::SelectionTrace {
                query,
                mode: "quality_conversational",
                top: None,
                r#final: &names,
                budget: None,
                reason: Some("quality path — conversational, pinned-only (no active tool context)"),
            });
            let report = SelectionReport {
                tools_selected: names,
                selected_count: schemas.len() as u32,
                budget_used: 0,
                budget_total: budget,
            };
            return (schemas, report);
        }

        let ranked = pre_filter_dynamic_with_quality(&state, query, quality_tracker);
        let schemas = self.budget_select_measured(&ranked, budget);
        let names = Self::selected_names(&schemas);

        let budget_used: u32 = names
            .iter()
            .filter(|n| {
                !TOOL_CATALOG
                    .iter()
                    .any(|t| t.pinned && t.name == n.as_str())
            })
            .map(|n| self.token_cost(n))
            .sum();

        if obs::is_selector_observability_enabled() {
            let top: Vec<(&str, f64)> = ranked
                .iter()
                .take(10)
                .map(|(idx, score)| (TOOL_CATALOG[*idx].name, *score))
                .collect();
            obs::emit_selector_trace(&obs::SelectionTrace {
                query,
                mode: "quality",
                top: Some(top),
                r#final: &names,
                budget: Some(obs::SelectionBudget {
                    used: budget_used,
                    total: budget,
                }),
                reason: None,
            });
        }

        let report = SelectionReport {
            selected_count: schemas.len() as u32,
            tools_selected: names,
            budget_used,
            budget_total: budget,
        };

        (schemas, report)
    }
    /// Pipeline-integrated selection using a pre-computed RoutingDecision.
    ///
    /// This is the new preferred entry point for tool selection. It uses
    /// the RoutingDecision's embedded ConversationState for backward-compatible
    /// TF-IDF scoring, while leveraging the enriched routing intelligence
    /// (task type, domain, confidence, tool filter) for better decisions.
    ///
    /// ```text
    /// let routing = RoutingEngine::analyze(query, turn, &recent, &hints, boost);
    /// let (schemas, report) = registry.select_routed(query, &routing, 800, &[], None, None);
    /// ```
    pub fn select_routed(
        &self,
        query: &str,
        routing: &RoutingDecision,
        budget: u32,
        extra_boost_terms: &[String],
        quality_tracker: Option<&ToolQualityTracker>,
        calibrator: Option<&ConfidenceCalibrator>,
    ) -> (Vec<Value>, SelectionReport) {
        self.select_routed_with_memory(
            query,
            routing,
            budget,
            extra_boost_terms,
            quality_tracker,
            calibrator,
            &[],
        )
    }

    /// Like [`select_routed`] but also accepts memory domain hints for gate softening.
    #[allow(clippy::too_many_arguments)]
    pub fn select_routed_with_memory(
        &self,
        query: &str,
        routing: &RoutingDecision,
        budget: u32,
        extra_boost_terms: &[String],
        quality_tracker: Option<&ToolQualityTracker>,
        calibrator: Option<&ConfidenceCalibrator>,
        memory_domain_hints: &[crate::pipeline::routing::DomainHint],
    ) -> (Vec<Value>, SelectionReport) {
        self.select_routed_with_pressure(
            query,
            routing,
            budget,
            extra_boost_terms,
            quality_tracker,
            calibrator,
            memory_domain_hints,
            0.0,
            &std::collections::HashMap::new(),
            &[],
            &std::collections::HashMap::new(),
        )
    }

    /// Pressure-aware tool selection.  When `budget_pressure` > 0, the scoring
    /// pipeline applies a rising minimum-score floor that excludes marginally
    /// relevant tools, saving schema tokens under token pressure.
    #[allow(clippy::too_many_arguments)]
    pub fn select_routed_with_pressure(
        &self,
        query: &str,
        routing: &RoutingDecision,
        budget: u32,
        extra_boost_terms: &[String],
        quality_tracker: Option<&ToolQualityTracker>,
        calibrator: Option<&ConfidenceCalibrator>,
        memory_domain_hints: &[crate::pipeline::routing::DomainHint],
        budget_pressure: f64,
        co_occurrence: &std::collections::HashMap<String, f64>,
        file_context: &[String],
        outcome_bias: &std::collections::HashMap<String, f64>,
    ) -> (Vec<Value>, SelectionReport) {
        use astra_turn_core::selector_observability as obs;

        // Use tool_filter for early conversational detection
        if routing.tool_filter == ToolFilter::Minimal {
            let schemas = self.pinned_only();
            let names = Self::selected_names(&schemas);
            // Observability: minimal-filter early return. A G3-class
            // bug where a tool is missing from the routed-minimal
            // path shows up as `final` not containing the expected
            // tool — visible without reading the routing engine.
            obs::emit_selector_trace(&obs::SelectionTrace {
                query,
                mode: "routed_minimal",
                top: None,
                r#final: &names,
                budget: None,
                reason: Some("routing.tool_filter == Minimal — pinned-only"),
            });
            return (
                schemas,
                SelectionReport {
                    tools_selected: names.clone(),
                    selected_count: names.len() as u32,
                    budget_used: 0,
                    budget_total: budget,
                },
            );
        }

        // Build effective query: original + routing boost terms + extra boost
        let effective_query = if routing.boost_terms.is_empty() && extra_boost_terms.is_empty() {
            query.to_string()
        } else {
            let mut parts = vec![query.to_string()];
            parts.extend(routing.boost_terms.iter().cloned());
            parts.extend(extra_boost_terms.iter().cloned());
            parts.join(" ")
        };

        // Use the routing's ConversationState for scoring.
        // Pass memory domain hints for gate softening in tool relevance scoring.
        // When budget_pressure > 0 or co-occurrence data exists, apply the full
        // co-occurrence-aware scoring pipeline for maximum selection quality.
        let has_co_occurrence = !co_occurrence.is_empty();
        let has_file_context = !file_context.is_empty();
        let has_outcome_bias = !outcome_bias.is_empty();
        let ranked = if budget_pressure > 0.01
            || has_co_occurrence
            || has_file_context
            || has_outcome_bias
        {
            pre_filter_dynamic_with_outcome_bias(
                &routing.conversation_state,
                &effective_query,
                quality_tracker,
                calibrator,
                memory_domain_hints,
                budget_pressure,
                co_occurrence,
                file_context,
                outcome_bias,
            )
        } else {
            pre_filter_dynamic_with_memory(
                &routing.conversation_state,
                &effective_query,
                quality_tracker,
                calibrator,
                memory_domain_hints,
            )
        };
        let schemas = self.budget_select_measured(&ranked, budget);
        let names = Self::selected_names(&schemas);
        let selected_count = schemas.len() as u32;

        let budget_used: u32 = names
            .iter()
            .filter(|n| {
                !TOOL_CATALOG
                    .iter()
                    .any(|t| t.pinned && t.name == n.as_str())
            })
            .map(|n| self.token_cost(n))
            .sum();

        // Observability: routed + pressure dynamic path — this is
        // the production selector entry point (tool_selector.rs).
        // G3 would show up here as a final list missing the tool
        // the caller expected, paired with a top list that shows
        // either (a) the tool absent entirely, or (b) a low score.
        if obs::is_selector_observability_enabled() {
            let top: Vec<(&str, f64)> = ranked
                .iter()
                .take(10)
                .map(|(idx, score)| (TOOL_CATALOG[*idx].name, *score))
                .collect();
            // Mode distinguishes pressure path from plain routed so
            // a reviewer can tell which pipeline ran. Label stays
            // short — grep-friendly.
            let mode = if budget_pressure > 0.01
                || has_co_occurrence
                || has_file_context
                || has_outcome_bias
            {
                "routed_pressure"
            } else {
                "routed"
            };
            obs::emit_selector_trace(&obs::SelectionTrace {
                query,
                mode,
                top: Some(top),
                r#final: &names,
                budget: Some(obs::SelectionBudget {
                    used: budget_used,
                    total: budget,
                }),
                reason: None,
            });
        }

        (
            schemas,
            SelectionReport {
                selected_count,
                tools_selected: names,
                budget_used,
                budget_total: budget,
            },
        )
    }

    /// Budget selection using measured token costs from actual schemas.
    fn budget_select_measured(
        &self,
        ranked_dynamic: &[(usize, f64)],
        budget_tokens: u32,
    ) -> Vec<Value> {
        let mut result = Vec::new();
        let mut used_tokens: u32 = 0;
        let mut included_names = std::collections::HashSet::new();

        // Always include pinned tools first (budget-exempt)
        for (name, schema) in &self.pinned_schemas {
            included_names.insert(name.clone());
            result.push(schema.clone());
        }

        // Add dynamic tools greedily from ranked list using measured costs
        for &(idx, _score) in ranked_dynamic {
            let tool = &TOOL_CATALOG[idx];
            if included_names.contains(tool.name) {
                continue;
            }
            let cost = self.token_cost(tool.name);
            if used_tokens + cost > budget_tokens {
                continue;
            }
            if let Some(schema) = self.find_schema(tool.name) {
                included_names.insert(tool.name.to_string());
                result.push(schema);
                used_tokens += cost;
            }
        }

        // Include registered plugin tools if budget permits
        for name in &self.plugin_tool_names {
            if included_names.contains(name) {
                continue;
            }
            let cost = self.token_cost(name);
            if used_tokens + cost > budget_tokens {
                continue;
            }
            if let Some(schema) = self.find_schema(name) {
                result.push(schema);
                used_tokens += cost;
            }
        }

        // Sort alphabetically for prompt-cache stability (same rationale as select_two_phase)
        sort_schemas_by_name(&mut result);

        result
    }

    fn find_schema(&self, name: &str) -> Option<Value> {
        self.all_schemas
            .iter()
            .find(|s| {
                s.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    == Some(name)
            })
            .cloned()
    }

    /// Return only pinned tools (for conversational queries).
    pub fn pinned_only(&self) -> Vec<Value> {
        let mut schemas: Vec<Value> = self.pinned_schemas.iter().map(|(_, s)| s.clone()).collect();
        sort_schemas_by_name(&mut schemas);
        schemas
    }

    /// Return ALL tool schemas (bypass selection — used for tool execution).
    pub fn all_schemas(&self) -> &[Value] {
        &self.all_schemas
    }

    /// Return all tool names from the catalog.
    pub fn all_tool_names() -> Vec<&'static str> {
        TOOL_CATALOG.iter().map(|t| t.name).collect()
    }

    /// Return names of currently selected tools.
    pub fn selected_names(schemas: &[Value]) -> Vec<String> {
        schemas
            .iter()
            .filter_map(|s| {
                s.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .map(String::from)
            })
            .collect()
    }

    /// Get metadata for a tool by name.
    pub fn get_meta(name: &str) -> Option<&'static ToolMeta> {
        TOOL_CATALOG.iter().find(|t| t.name == name)
    }

    /// Count of pinned tools.
    pub fn pinned_count() -> usize {
        TOOL_CATALOG.iter().filter(|t| t.pinned).count()
    }

    /// Count of dynamic (selectable) tools.
    pub fn dynamic_count() -> usize {
        TOOL_CATALOG.iter().filter(|t| !t.pinned).count()
    }

    /// Register plugin tools from a PluginRegistry, merging their schemas
    /// into the active tool set. Rebuilds internal indexes.
    ///
    /// This is the bridge between dynamic skill/plugin registration and
    /// the production tool selection pipeline.
    pub fn register_plugins(&mut self, plugins: &super::plugin::PluginRegistry) {
        let plugin_schemas = plugins.schemas();
        if plugin_schemas.is_empty() {
            return;
        }
        // Track plugin tool names for inclusion in selection
        for schema in &plugin_schemas {
            if let Some(name) = schema
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
            {
                self.plugin_tool_names.push(name.to_string());
            }
        }
        self.all_schemas.extend(plugin_schemas);
        // Rebuild indexes to include the new schemas
        self.measured_costs = Self::measure_all_schemas(&self.all_schemas);
        self.schema_index = Self::build_schema_index(&self.all_schemas);
        // Pinned schemas don't change (plugins are never pinned in catalog)
    }

    /// Inject a single tool schema dynamically (e.g. the `skill` or `delegate` tool).
    ///
    /// When `pinned` is true the tool is budget-exempt (always included like
    /// core tools such as `bash` and `read_file`).  When false it behaves as a
    /// plugin tool that is only included when budget allows.
    pub fn inject_schema(&mut self, schema: Value) {
        self.inject_schema_pinned(schema, true);
    }

    /// Inject with explicit pinning control.
    pub fn inject_schema_pinned(&mut self, schema: Value, pinned: bool) {
        if let Some(name) = schema
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
        {
            if self.schema_index.contains_key(name) {
                return;
            }
            let name_owned = name.to_string();
            let idx = self.all_schemas.len();
            let json_bytes = serde_json::to_string(&schema).map(|s| s.len()).unwrap_or(0);
            self.measured_costs
                .insert(name_owned.clone(), (json_bytes / 4) as u32);
            self.schema_index.insert(name_owned.clone(), idx);
            if pinned {
                self.pinned_schemas.push((name_owned, schema.clone()));
            } else {
                self.plugin_tool_names.push(name_owned);
            }
            self.all_schemas.push(schema);
        }
    }

    /// Insert a new schema or replace an existing schema with the same tool name.
    pub fn upsert_schema_pinned(&mut self, schema: Value, pinned: bool) {
        let Some(name) = schema
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            return;
        };

        let json_bytes = serde_json::to_string(&schema).map(|s| s.len()).unwrap_or(0);
        self.measured_costs
            .insert(name.clone(), (json_bytes / 4) as u32);

        if let Some(&idx) = self.schema_index.get(&name) {
            self.all_schemas[idx] = schema.clone();
            if let Some((_, existing)) = self.pinned_schemas.iter_mut().find(|(n, _)| n == &name) {
                *existing = schema;
            } else if pinned {
                self.pinned_schemas.push((name, schema));
            }
            return;
        }

        self.inject_schema_pinned(schema, pinned);
    }

    /// Insert or replace a pinned schema by tool name.
    pub fn upsert_schema(&mut self, schema: Value) {
        self.upsert_schema_pinned(schema, true);
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
    fn inject_schema_ignores_malformed() {
        let mut reg = ToolRegistry::new(vec![sample_schema("bash")]);
        reg.inject_schema(json!({"broken": true}));
        assert_eq!(reg.total_tool_count(), 1);
    }

    #[test]
    fn injected_schema_is_pinned_by_default() {
        let mut reg = ToolRegistry::new(vec![sample_schema("bash")]);
        reg.inject_schema(sample_schema("skill"));
        // Default inject is pinned (budget-exempt)
        assert!(reg.pinned_schemas.iter().any(|(n, _)| n == "skill"));
        assert!(!reg.plugin_tool_names.contains(&"skill".to_string()));
    }

    #[test]
    fn injected_schema_unpinned_is_plugin() {
        let mut reg = ToolRegistry::new(vec![sample_schema("bash")]);
        reg.inject_schema_pinned(sample_schema("skill"), false);
        assert!(reg.plugin_tool_names.contains(&"skill".to_string()));
        assert!(!reg.pinned_schemas.iter().any(|(n, _)| n == "skill"));
    }

    #[test]
    fn pinned_only_includes_injected_pinned() {
        let mut reg = ToolRegistry::new(vec![sample_schema("bash")]);
        reg.inject_schema(sample_schema("skill"));
        let pinned = reg.pinned_only();
        let names: Vec<&str> = pinned
            .iter()
            .filter_map(|s| {
                s.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
            })
            .collect();
        assert!(
            names.contains(&"skill"),
            "pinned_only should include injected pinned tools"
        );
    }

    // ── Selector observability integration (Pass B) ──
    //
    // These tests verify the `[selector]` stderr hook plumbs through
    // the real `select_with_report_ctx` path. They exercise both the
    // conversational short-circuit and the dynamic ranking branch.
    // Stderr content is not captured (no stable API in tokio tests),
    // but the flag guard ensures the hot path runs without panic
    // when observability is on — which is what we'd regress if the
    // obs module grew a lifetime bug or serde panic.

    #[test]
    fn select_with_report_ctx_conversational_path_does_not_panic_with_obs_on() {
        use astra_turn_core::selector_observability::{
            SELECTOR_OBS_TEST_MUTEX, restore_selector_observability_for_tests,
            set_selector_observability_for_tests,
        };
        let _lock = SELECTOR_OBS_TEST_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev = set_selector_observability_for_tests(true);

        let mut schemas = vec![sample_schema("bash"), sample_schema("read_file")];
        // Ensure a pinned + non-pinned mix so both branches of
        // pinned_only + filter are exercised under observability.
        schemas.push(sample_schema("github_list_prs"));
        let registry = ToolRegistry::new(schemas);
        // "hello" is the conversational short-circuit case.
        let (out_schemas, report) = registry.select_with_report_ctx("hello", 0, 800, &[]);
        assert!(!out_schemas.is_empty());
        assert_eq!(report.selected_count as usize, out_schemas.len());

        restore_selector_observability_for_tests(prev);
    }

    #[test]
    fn conversational_with_recent_tools_runs_dynamic_path() {
        // Verify: the `recent_tools.is_empty()` guard prevents the
        // conversational short-circuit. We test this by checking that
        // "好的" (conversational) WITH recent_tools=["github_ci_status"]
        // includes github_list_prs (same-category recency boost), while
        // WITHOUT recent_tools it only returns pinned tools.
        let schemas: Vec<Value> = TOOL_CATALOG.iter().map(|t| sample_schema(t.name)).collect();
        let registry = ToolRegistry::new(schemas);

        // Without recent tools: conversational short-circuit → pinned only
        let (_, report_bare) = registry.select_with_report_ctx("好的", 2, 800, &[]);
        let bare_count = report_bare.selected_count;

        // With recent tools: dynamic path runs → recency boost can add tools
        let recent = vec!["github_ci_status".to_string()];
        let (_, report_ctx) = registry.select_with_report_ctx("好的", 2, 800, &recent);
        let ctx_count = report_ctx.selected_count;

        assert!(
            ctx_count > bare_count,
            "with recent_tools, dynamic path should select MORE than pinned-only: bare={bare_count} ctx={ctx_count}"
        );
    }

    #[test]
    fn quality_path_conversational_with_recent_tools_runs_dynamic() {
        // Symmetric test: select_with_quality must also respect recent_tools guard.
        let schemas: Vec<Value> = TOOL_CATALOG.iter().map(|t| sample_schema(t.name)).collect();
        let registry = ToolRegistry::new(schemas);

        // Without recent tools: quality conversational short-circuit → pinned only
        let ((_, report_bare), _) =
            with_obs_capture(|| registry.select_with_quality("好的", 2, 800, &[], None));
        let bare_count = report_bare.selected_count;

        // With recent tools: dynamic path
        let recent = vec!["github_ci_status".to_string()];
        let ((_, report_ctx), _) =
            with_obs_capture(|| registry.select_with_quality("好的", 2, 800, &recent, None));
        let ctx_count = report_ctx.selected_count;

        assert!(
            ctx_count > bare_count,
            "quality path with recent_tools should select MORE than pinned-only: bare={bare_count} ctx={ctx_count}"
        );
    }

    #[test]
    fn conversational_without_recent_tools_shortcircuits() {
        let schemas: Vec<Value> = TOOL_CATALOG.iter().map(|t| sample_schema(t.name)).collect();
        let registry = ToolRegistry::new(schemas);
        // "谢谢" with no recent_tools → should short-circuit to pinned-only.
        let (out, report) = registry.select_with_report_ctx("谢谢", 1, 800, &[]);
        assert_eq!(
            report.selected_count,
            ToolRegistry::pinned_count() as u32,
            "conversational + no recent_tools should return only pinned"
        );
        let _ = out;
    }

    #[test]
    fn select_with_report_ctx_dynamic_path_does_not_panic_with_obs_on() {
        use astra_turn_core::selector_observability::{
            SELECTOR_OBS_TEST_MUTEX, restore_selector_observability_for_tests,
            set_selector_observability_for_tests,
        };
        let _lock = SELECTOR_OBS_TEST_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev = set_selector_observability_for_tests(true);

        let schemas = vec![
            sample_schema("bash"),
            sample_schema("read_file"),
            sample_schema("grep"),
            sample_schema("list_dir"),
        ];
        let registry = ToolRegistry::new(schemas);
        // Analytical-ish query — forces the dynamic branch.
        let (out, report) =
            registry.select_with_report_ctx("search for TODO in source files", 0, 800, &[]);
        assert!(!out.is_empty());
        assert!(report.selected_count > 0);

        restore_selector_observability_for_tests(prev);
    }

    #[test]
    fn select_path_is_unaffected_when_obs_flag_off() {
        // Without the flag set, emit_selector_trace is a noop and
        // the selection result must be identical to the pre-obs
        // behavior (byte-for-byte on tools_selected).
        use astra_turn_core::selector_observability::{
            SELECTOR_OBS_TEST_MUTEX, restore_selector_observability_for_tests,
            set_selector_observability_for_tests,
        };
        let _lock = SELECTOR_OBS_TEST_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev = set_selector_observability_for_tests(false);

        let schemas = vec![sample_schema("bash"), sample_schema("read_file")];
        let registry = ToolRegistry::new(schemas);
        let (_out, report) = registry.select_with_report_ctx("search for needles", 0, 800, &[]);
        assert!(report.selected_count >= 1);

        restore_selector_observability_for_tests(prev);
    }

    // ── Production-path coverage (G3 repro surface) ──
    //
    // `select_routed_with_pressure` is the ONLY registry selection
    // entry point called from production code (tool_selector.rs:700).
    // If observability only fires on `select_with_report_ctx` then
    // turning the flag on yields zero lines in production and the
    // G3-class bug it was built to catch goes undetected.
    //
    // These tests capture emitted trace JSON in-process so we can
    // assert each production path emits exactly one `[selector]`
    // record with the right shape. They would FAIL on the original
    // PR (no emit on routed / quality paths) — that's the TDD red
    // before the emit-site additions below.

    fn with_obs_capture<F, R>(f: F) -> (R, Vec<String>)
    where
        F: FnOnce() -> R,
    {
        use astra_turn_core::selector_observability::{
            SELECTOR_OBS_TEST_MUTEX, drain_captured_traces_for_tests,
            restore_selector_observability_for_tests, set_capture_to_buffer_for_tests,
            set_selector_observability_for_tests,
        };
        let _lock = SELECTOR_OBS_TEST_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev_flag = set_selector_observability_for_tests(true);
        let prev_capture = set_capture_to_buffer_for_tests(true);
        // Drain anything a prior test left behind so we only see this
        // test's writes.
        let _ = drain_captured_traces_for_tests();

        let out = f();
        let captured = drain_captured_traces_for_tests();

        set_capture_to_buffer_for_tests(prev_capture);
        restore_selector_observability_for_tests(prev_flag);
        (out, captured)
    }

    #[test]
    fn routed_pressure_dynamic_path_emits_selector_trace() {
        use crate::pipeline::routing::RoutingEngine;
        let schemas = vec![
            sample_schema("bash"),
            sample_schema("read_file"),
            sample_schema("grep"),
            sample_schema("list_dir"),
        ];
        let registry = ToolRegistry::new(schemas);
        let query = "search for TODO in source files";
        let ((_out, report), captured) = with_obs_capture(|| {
            let routing = RoutingEngine::analyze(query, 0, &[], &[], vec![]);
            registry.select_routed_with_pressure(
                query,
                &routing,
                800,
                &[],
                None,
                None,
                &[],
                0.0,
                &std::collections::HashMap::new(),
                &[],
                &std::collections::HashMap::new(),
            )
        });
        assert!(report.selected_count > 0);
        let matching: Vec<_> = captured
            .iter()
            .filter(|t| t.contains("\"query\":\"search for TODO in source files\""))
            .collect();
        assert_eq!(
            matching.len(),
            1,
            "routed_with_pressure must emit exactly one selector trace for its query; got {captured:?}"
        );
        let json = matching[0];
        assert!(
            json.contains("\"mode\":\"routed\"") || json.contains("\"mode\":\"routed_pressure\""),
            "expected routed mode label in trace; got {json}"
        );
        assert!(
            json.contains("\"query\":"),
            "trace must include query; got {json}"
        );
        assert!(
            json.contains("\"final\":"),
            "trace must include final list; got {json}"
        );
    }

    #[test]
    fn routed_pressure_minimal_filter_emits_selector_trace() {
        use crate::pipeline::routing::{RoutingEngine, ToolFilter};
        let schemas = vec![sample_schema("bash"), sample_schema("read_file")];
        let registry = ToolRegistry::new(schemas);
        // A short greeting triggers the Minimal ToolFilter branch.
        // Verify the early-return path ALSO emits — otherwise the
        // "tool missing from conversational path" case stays silent.
        let query = "hello";
        let ((_out, _report), captured) = with_obs_capture(|| {
            let mut routing = RoutingEngine::analyze(query, 0, &[], &[], vec![]);
            // Force Minimal so the test doesn't depend on the routing
            // engine's classification heuristics.
            routing.tool_filter = ToolFilter::Minimal;
            registry.select_routed_with_pressure(
                query,
                &routing,
                800,
                &[],
                None,
                None,
                &[],
                0.0,
                &std::collections::HashMap::new(),
                &[],
                &std::collections::HashMap::new(),
            )
        });
        assert_eq!(
            captured.len(),
            1,
            "minimal-filter early return must also emit a trace; got {captured:?}"
        );
        assert!(
            captured[0].contains("\"mode\":\"routed_minimal\"")
                || captured[0].contains("\"mode\":\"routed\""),
            "expected minimal-filter mode label; got {}",
            captured[0]
        );
    }

    #[test]
    fn quality_path_emits_selector_trace_on_dynamic_branch() {
        let schemas = vec![
            sample_schema("bash"),
            sample_schema("read_file"),
            sample_schema("grep"),
        ];
        let registry = ToolRegistry::new(schemas);
        let query = "search for TODO comments";
        let ((_out, report), captured) =
            with_obs_capture(|| registry.select_with_quality(query, 0, 800, &[], None));
        assert!(report.selected_count > 0);
        let matching: Vec<_> = captured
            .iter()
            .filter(|t| t.contains("\"query\":\"search for TODO comments\""))
            .collect();
        assert_eq!(
            matching.len(),
            1,
            "select_with_quality dynamic branch must emit exactly one trace for its query; got {captured:?}"
        );
        assert!(
            matching[0].contains("\"mode\":\"quality\""),
            "expected quality mode label; got {}",
            matching[0]
        );
    }

    #[test]
    fn routed_pressure_off_flag_emits_nothing() {
        // Sanity guard: we must not emit when the flag is off, even
        // on production paths. Would catch a regression where the
        // flag check gets skipped inside the new emit sites.
        use astra_turn_core::selector_observability::{
            SELECTOR_OBS_TEST_MUTEX, drain_captured_traces_for_tests,
            restore_selector_observability_for_tests, set_capture_to_buffer_for_tests,
            set_selector_observability_for_tests,
        };
        let _lock = SELECTOR_OBS_TEST_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev_flag = set_selector_observability_for_tests(false);
        let prev_capture = set_capture_to_buffer_for_tests(true);
        let _ = drain_captured_traces_for_tests();

        use crate::pipeline::routing::RoutingEngine;
        let schemas = vec![sample_schema("bash"), sample_schema("read_file")];
        let registry = ToolRegistry::new(schemas);
        let query = "anything";
        let routing = RoutingEngine::analyze(query, 0, &[], &[], vec![]);
        let _ = registry.select_routed_with_pressure(
            query,
            &routing,
            800,
            &[],
            None,
            None,
            &[],
            0.0,
            &std::collections::HashMap::new(),
            &[],
            &std::collections::HashMap::new(),
        );

        let captured = drain_captured_traces_for_tests();
        set_capture_to_buffer_for_tests(prev_capture);
        restore_selector_observability_for_tests(prev_flag);
        assert!(
            captured.is_empty(),
            "flag off must produce zero traces; got {captured:?}"
        );
    }
}

#[cfg(test)]
mod pinned_budget_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pinned_tools_included_even_with_tiny_budget() {
        // Use bash (pinned) + git_log / mo_branch (still dynamic after the
        // pinned-list expansion) to prove budget gating only affects dynamic.
        let schemas = vec![
            json!({"function": {"name": "bash", "description": "Execute shell commands", "parameters": {"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"]}}}),
            json!({"function": {"name": "read_file", "description": "Read file contents", "parameters": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}}}),
            json!({"function": {"name": "str_replace", "description": "Replace text in files", "parameters": {"type": "object", "properties": {"path": {"type": "string"}, "old_str": {"type": "string"}, "new_str": {"type": "string"}}, "required": ["path", "old_str", "new_str"]}}}),
            json!({"function": {"name": "git_log", "description": "Show git log", "parameters": {"type": "object", "properties": {}}}}),
            json!({"function": {"name": "mo_branch", "description": "Matrixone branch ops", "parameters": {"type": "object", "properties": {}}}}),
        ];

        let registry = ToolRegistry::new(schemas);

        let pinned = registry.pinned_schemas();
        let pinned_names: Vec<&str> = pinned.iter().map(|(n, _)| n.as_str()).collect();
        assert!(
            pinned_names.contains(&"bash"),
            "bash should be pinned, got: {:?}",
            pinned_names
        );
        assert!(
            pinned_names.contains(&"read_file"),
            "read_file should be pinned, got: {:?}",
            pinned_names
        );
        assert!(
            pinned_names.contains(&"str_replace"),
            "str_replace should be pinned, got: {:?}",
            pinned_names
        );

        // budget_select_measured with budget=0: pinned survive, dynamic excluded.
        let git_log_idx = TOOL_CATALOG
            .iter()
            .position(|t| t.name == "git_log")
            .expect("git_log must exist in TOOL_CATALOG");
        let mo_branch_idx = TOOL_CATALOG
            .iter()
            .position(|t| t.name == "mo_branch")
            .expect("mo_branch must exist in TOOL_CATALOG");

        let ranked = vec![(git_log_idx, 0.8), (mo_branch_idx, 0.5)];
        let result = registry.budget_select_measured(&ranked, 0);

        let result_names: Vec<&str> = result
            .iter()
            .filter_map(|s| {
                s.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
            })
            .collect();

        // Pinned tools present (budget-exempt).
        assert!(
            result_names.contains(&"bash"),
            "bash must survive zero budget, got: {:?}",
            result_names
        );
        assert!(
            result_names.contains(&"read_file"),
            "read_file must survive zero budget, got: {:?}",
            result_names
        );
        assert!(
            result_names.contains(&"str_replace"),
            "str_replace must survive zero budget, got: {:?}",
            result_names
        );
        // Dynamic tools excluded — proves budget is actually enforced.
        assert!(
            !result_names.contains(&"git_log"),
            "git_log should be excluded at zero budget"
        );
        assert!(
            !result_names.contains(&"mo_branch"),
            "mo_branch should be excluded at zero budget"
        );
        assert_eq!(
            result_names.len(),
            3,
            "only pinned tools should survive zero budget"
        );
    }
}
