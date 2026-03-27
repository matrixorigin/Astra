use serde_json::Value;

use super::meta::{TOOL_CATALOG, ToolMeta};
use super::report::{SelectionReport, ToolQualityTracker};
use super::scoring::{
    DEFAULT_TOOL_BUDGET_TOKENS, pre_filter_dynamic, pre_filter_dynamic_calibrated,
    pre_filter_dynamic_with_memory, pre_filter_dynamic_with_pressure,
    pre_filter_dynamic_with_quality,
};
use super::state::ConversationState;
use crate::pipeline::routing::{RoutingDecision, ToolFilter};
use crate::turn::routing_metrics::ConfidenceCalibrator;

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
        let state = ConversationState::from_message_with_context(query, turn_count, recent_tools);

        if state.is_conversational
            && !state.is_fetch
            && !state.is_mutate
            && !state.is_analytical
            && !state.references_history
        {
            let schemas = self.pinned_only();
            let names = Self::selected_names(&schemas);
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
        let state = ConversationState::from_message_with_context(query, turn_count, recent_tools);

        if state.is_conversational
            && !state.is_fetch
            && !state.is_mutate
            && !state.is_analytical
            && !state.references_history
        {
            let schemas = self.pinned_only();
            let names = Self::selected_names(&schemas);
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

        let report = SelectionReport {
            selected_count: schemas.len() as u32,
            tools_selected: names,
            budget_used,
            budget_total: budget,
        };

        (schemas, report)
    }

    /// Full-featured selection with quality tracking AND confidence calibration.
    #[allow(clippy::too_many_arguments)]
    pub fn select_calibrated(
        &self,
        query: &str,
        turn_count: u32,
        budget: u32,
        recent_tools: &[String],
        quality_tracker: Option<&ToolQualityTracker>,
        calibrator: Option<&ConfidenceCalibrator>,
        boost_terms: &[String],
    ) -> (Vec<Value>, SelectionReport) {
        // Build the effective query: original + memory-derived boost terms
        let effective_query = if boost_terms.is_empty() {
            query.to_string()
        } else {
            format!("{} {}", query, boost_terms.join(" "))
        };
        let state = ConversationState::from_message_with_context(
            &effective_query,
            turn_count,
            recent_tools,
        );

        if state.is_conversational
            && !state.is_fetch
            && !state.is_mutate
            && !state.is_analytical
            && !state.references_history
        {
            let schemas = self.pinned_only();
            let names = Self::selected_names(&schemas);
            let report = SelectionReport {
                tools_selected: names,
                selected_count: schemas.len() as u32,
                budget_used: 0,
                budget_total: budget,
            };
            return (schemas, report);
        }

        let ranked =
            pre_filter_dynamic_calibrated(&state, &effective_query, quality_tracker, calibrator);
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
    ) -> (Vec<Value>, SelectionReport) {
        // Use tool_filter for early conversational detection
        if routing.tool_filter == ToolFilter::Minimal {
            let schemas = self.pinned_only();
            let names = Self::selected_names(&schemas);
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

        // Use the routing's embedded ConversationState for backward-compatible scoring.
        // Pass memory domain hints for gate softening in tool relevance scoring.
        // When budget_pressure > 0, apply pressure-aware filtering to exclude
        // marginally relevant tools — saving schema tokens.
        let ranked = if budget_pressure > 0.01 {
            pre_filter_dynamic_with_pressure(
                &routing.conversation_state,
                &effective_query,
                quality_tracker,
                calibrator,
                memory_domain_hints,
                budget_pressure,
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
        for tool in TOOL_CATALOG.iter() {
            if tool.pinned
                && let Some(schema) = self.find_schema(tool.name)
            {
                included_names.insert(tool.name.to_string());
                result.push(schema);
            }
        }

        // Add dynamic tools greedily from ranked list using measured costs
        for &(idx, _score) in ranked_dynamic {
            let tool = &TOOL_CATALOG[idx];
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
        TOOL_CATALOG
            .iter()
            .filter(|t| t.pinned)
            .filter_map(|tool| self.find_schema(tool.name))
            .collect()
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

    /// Total tool count (built-in + registered plugins).
    pub fn total_tool_count(&self) -> usize {
        self.all_schemas.len()
    }
}
