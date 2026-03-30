//! Tool selection strategies.
//!
//! # Architecture
//!
//! Tool selection is a **separate concern** from tool execution and LLM chat.
//! This module defines the `ToolSelector` trait and three strategies:
//!
//! - [`TfIdfSelector`]: Fast heuristic fallback using TF-IDF scoring (no LLM call).
//!   `ConversationState` is its private implementation detail — not a public API.
//!   **Do NOT add fields to ConversationState for new edge cases.**
//!
//! - [`LlmToolSelector`]: Asks a model to select tools from a compact catalog summary.
//!   Handles semantic understanding natively — "matrixone呢？" after a PR query
//!   is trivial for an LLM but impossible for heuristics.
//!
//! - [`FallbackSelector`]: Tries LLM first (accurate), falls back to TF-IDF (fast)
//!   if the LLM call fails or times out.
//!
//! # Design rationale
//!
//! ConversationState was a **leaky abstraction**: every new edge case required
//! adding a field (`is_github`, `is_fetch`, `recent_tools`, etc.), effectively
//! simulating a mini language model with struct fields. The correct fix is to
//! let the actual LLM handle semantic understanding, and keep heuristics only
//! as a fallback for when the LLM is unavailable.
//!
//! The next edge case should be handled by **improving the LLM prompt**, not
//! by adding a field to ConversationState.

use crate::pipeline::calibration::ProgressiveCalibrator;
use crate::pipeline::entity::{EntityGraph, extract_entities};
use crate::pipeline::pattern::PatternLibrary;
use crate::pipeline::routing::{DomainHint, RoutingEngine, TaskType};
use crate::tool_registry::{self, TOOL_CATALOG, ToolQualityTracker, ToolRegistry};
use crate::turn::routing_metrics::ConfidenceCalibrator;
use async_trait::async_trait;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const MIN_LEARNED_ENTITY_CONFIDENCE: f64 = 0.30;

// ─── Public types ────────────────────────────────────────────────────────────

/// Context provided to tool selectors. Open for extension without
/// modifying selector implementations (they use what they need).
pub struct SelectionContext<'a> {
    pub query: &'a str,
    pub turn_count: u32,
    pub recent_tools: &'a [String],
    pub budget_tokens: u32,
    /// Memory-derived entity hints that boost tool scoring.
    /// Example: if memory knows "matrixorigin = GitHub org", boost_terms
    /// would contain ["github", "org", "repository"] — extra TF-IDF terms
    /// that improve tool ranking without changing the original query.
    pub boost_terms: Vec<String>,
    /// Budget pressure from token usage ratio (0.0 = normal, 1.0 = critical).
    /// Computed from compaction tier: Normal=0.0, TrimSchemas=0.3,
    /// CompactHistory=0.6, AggressivePrune=0.9.
    /// Reduces effective tool budget: `budget * (1.0 - pressure * 0.5)`.
    pub budget_pressure: f64,
    /// Memory-derived domain hints (e.g., "matrixorigin" → DomainHint::GitHub).
    /// When present, lowers the content relevance gate for tools in matching
    /// domains — enabling selection even when TF-IDF score is near-zero.
    pub memory_domain_hints: Vec<DomainHint>,
    /// Tools that should be excluded from selection (e.g., deprioritized by
    /// TurnGuard health tracker, or stall-restricted). These are filtered out
    /// BEFORE scoring, saving compute and preventing broken tools from being offered.
    pub restricted_tools: Vec<String>,
    /// Project language/framework hints derived from workspace files.
    /// e.g., ["rust"] from Cargo.toml, ["typescript"] from tsconfig.json.
    /// Boosts tools relevant to the detected project type.
    pub file_context: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LearnedContext {
    pub task_archetype: Option<TaskType>,
    pub entity_hints: Vec<String>,
    pub pattern_hints: Vec<String>,
    pub calibration_hints: Vec<String>,
    pub tool_hints: Vec<String>,
}

impl LearnedContext {
    pub fn is_empty(&self) -> bool {
        self.task_archetype.is_none()
            && self.entity_hints.is_empty()
            && self.pattern_hints.is_empty()
            && self.calibration_hints.is_empty()
            && self.tool_hints.is_empty()
    }

    pub fn prompt_fragment(&self) -> String {
        if self.is_empty() {
            return String::new();
        }

        let mut lines = Vec::new();
        if let Some(task_type) = self.task_archetype {
            lines.push(format!("- Learned task archetype: {task_type:?}"));
        }
        for entity in &self.entity_hints {
            lines.push(format!("- {entity}"));
        }
        for pattern in &self.pattern_hints {
            lines.push(format!("- {pattern}"));
        }
        for hint in &self.calibration_hints {
            lines.push(format!("- {hint}"));
        }
        for hint in &self.tool_hints {
            lines.push(format!("- {hint}"));
        }
        format!(
            "Learned runtime context (use as a prior, not a hard requirement):\n{}",
            lines.join("\n")
        )
    }
}

/// Result of tool selection.
#[derive(Debug, Clone)]
pub struct SelectionResult {
    /// Tool names selected (pinned tools always included by ToolRegistry).
    pub tool_names: Vec<String>,
    /// Which strategy produced this result (used in tests and logging).
    pub strategy: &'static str,
    /// Token budget consumed by dynamic (non-pinned) tools.
    pub budget_used: u32,
    /// True if the selector failed (timeout, error, empty) — signals fallback.
    pub failed: bool,
    /// Selection confidence: 0.0 = very uncertain (few signals, low TF-IDF),
    /// 1.0 = highly confident (multiple signals, strong TF-IDF match).
    /// Used to gate system prompt behavior (e.g., "ask for clarification" advisory).
    pub confidence: f64,
    /// LLM tokens consumed by the selector itself (0 for TF-IDF).
    pub selector_tokens_in: u64,
    pub selector_tokens_out: u64,
    /// Skills selected by LLM (skill names that should have instructions loaded).
    /// Empty for TF-IDF fallback (skills require semantic understanding).
    pub selected_skills: Vec<String>,
}

impl Default for SelectionResult {
    fn default() -> Self {
        Self {
            tool_names: vec![],
            strategy: "default",
            budget_used: 0,
            failed: false,
            confidence: 0.0,
            selector_tokens_in: 0,
            selector_tokens_out: 0,
            selected_skills: vec![],
        }
    }
}

/// Skill metadata for tool selection (lightweight, ~50 tokens per skill).
#[derive(Debug, Clone)]
pub struct SkillCatalogEntry {
    pub name: String,
    pub description: String,
}

/// Strategy for selecting tools from the catalog.
#[async_trait]
pub trait ToolSelector: Send + Sync {
    async fn select(&self, ctx: &SelectionContext<'_>) -> SelectionResult;

    async fn select_with_learned_context(
        &self,
        ctx: &SelectionContext<'_>,
        _learned_context: &LearnedContext,
    ) -> SelectionResult {
        self.select(ctx).await
    }

    fn learned_context(&self, _query: &str, _recent_tools: &[String]) -> LearnedContext {
        LearnedContext::default()
    }

    /// Check if this selector has skills registered for selection.
    /// Default is false (empty) — only LlmToolSelector has skills.
    fn selected_skills_empty(&self) -> bool {
        true
    }

    /// Access the underlying tool registry for schema/cost queries.
    /// Returns the default registry if the selector doesn't own one.
    fn registry(&self) -> &ToolRegistry {
        static DEFAULT: std::sync::LazyLock<ToolRegistry> =
            std::sync::LazyLock::new(|| ToolRegistry::new(vec![]));
        &DEFAULT
    }

    /// Record the outcome of a turn for progressive learning.
    /// Default is no-op — only TfIdfSelector (with pipeline modules) learns.
    #[allow(clippy::too_many_arguments)]
    fn record_outcome(
        &self,
        _query: &str,
        _tools_used: &[String],
        _task_type: TaskType,
        _domain: Option<DomainHint>,
        _success: bool,
        _quality: f64,
        _was_corrected: bool,
        _user_feedback_score: Option<i64>,
    ) {
    }
}

// ─── TF-IDF selector (heuristic fallback) ────────────────────────────────────

/// Fast heuristic selector using TF-IDF scoring. No LLM call.
/// Wraps [`ToolRegistry`] — ConversationState is an internal detail.
///
/// When pipeline modules are present (EntityGraph, PatternLibrary,
/// ProgressiveCalibrator), uses RoutingEngine for enriched routing
/// that improves over time.
pub struct TfIdfSelector {
    registry: ToolRegistry,
    /// Session-scoped quality tracker. When present, historical tool effectiveness
    /// boosts/penalizes tool rankings (self-improving feedback loop).
    quality_tracker: Option<Arc<Mutex<ToolQualityTracker>>>,
    /// Session-scoped confidence calibrator. When present, adjusts score thresholds
    /// based on historical correction rates per intent type.
    confidence_calibrator: Option<Arc<ConfidenceCalibrator>>,
    /// Entity knowledge graph. When present, extracts entities from queries and
    /// uses learned domain associations to boost relevant tools.
    entity_graph: Option<Arc<Mutex<EntityGraph>>>,
    /// Tool chain pattern library. When present, suggests tools from historically
    /// successful patterns for the detected task type.
    pattern_library: Option<Arc<Mutex<PatternLibrary>>>,
    /// Progressive 3-axis calibrator. When present, replaces the single-axis
    /// ConfidenceCalibrator with per-intent × per-domain × per-task calibration.
    progressive_calibrator: Option<Arc<Mutex<ProgressiveCalibrator>>>,
}

impl TfIdfSelector {
    pub fn new(registry: ToolRegistry) -> Self {
        Self {
            registry,
            quality_tracker: None,
            confidence_calibrator: None,
            entity_graph: None,
            pattern_library: None,
            progressive_calibrator: None,
        }
    }

    pub fn with_quality_tracker(mut self, tracker: Arc<Mutex<ToolQualityTracker>>) -> Self {
        self.quality_tracker = Some(tracker);
        self
    }

    pub fn with_confidence_calibrator(mut self, calibrator: Arc<ConfidenceCalibrator>) -> Self {
        self.confidence_calibrator = Some(calibrator);
        self
    }

    pub fn with_entity_graph(mut self, graph: Arc<Mutex<EntityGraph>>) -> Self {
        self.entity_graph = Some(graph);
        self
    }

    pub fn with_pattern_library(mut self, library: Arc<Mutex<PatternLibrary>>) -> Self {
        self.pattern_library = Some(library);
        self
    }

    pub fn with_progressive_calibrator(
        mut self,
        calibrator: Arc<Mutex<ProgressiveCalibrator>>,
    ) -> Self {
        self.progressive_calibrator = Some(calibrator);
        self
    }

    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    /// Get a reference to the quality tracker (if set) for recording feedback.
    pub fn quality_tracker(&self) -> Option<&Arc<Mutex<ToolQualityTracker>>> {
        self.quality_tracker.as_ref()
    }

    /// Compute entity boost terms from the EntityGraph.
    fn entity_boost_terms(&self, query: &str) -> Vec<String> {
        let graph = match &self.entity_graph {
            Some(eg) => match eg.lock() {
                Ok(g) => g,
                Err(e) => e.into_inner(),
            },
            None => return Vec::new(),
        };

        let entities = extract_entities(query);
        let mut terms = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for entity in &entities {
            for term in graph.boost_for(entity) {
                if seen.insert(term.clone()) {
                    terms.push(term);
                }
            }
        }
        terms
    }

    /// Compute pattern boost terms from the PatternLibrary.
    fn pattern_boost_terms(&self, task_type: TaskType, domain: Option<DomainHint>) -> Vec<String> {
        let lib = match &self.pattern_library {
            Some(pl) => match pl.lock() {
                Ok(l) => l,
                Err(e) => e.into_inner(),
            },
            None => return Vec::new(),
        };
        lib.boost_terms_for(task_type, domain)
    }

    fn summarize_learned_context(&self, query: &str) -> LearnedContext {
        let entity_boost = self.entity_boost_terms(query);
        let routing = RoutingEngine::analyze(query, 1, &[], &[], entity_boost);
        let mut learned = LearnedContext {
            task_archetype: Some(routing.task_type),
            entity_hints: Vec::new(),
            pattern_hints: Vec::new(),
            calibration_hints: Vec::new(),
            tool_hints: Vec::new(),
        };

        if let Some(graph) = &self.entity_graph {
            let graph = match graph.lock() {
                Ok(g) => g,
                Err(e) => e.into_inner(),
            };
            for entity in extract_entities(query).into_iter().take(3) {
                if let Some(knowledge) = graph.get(&entity) {
                    let confidence = knowledge.decayed_confidence();
                    if confidence < MIN_LEARNED_ENTITY_CONFIDENCE {
                        continue;
                    }
                    let domain = knowledge
                        .domain
                        .map(|d| format!("{d:?}"))
                        .unwrap_or_else(|| "unknown".to_string());
                    let tools = if knowledge.associated_tools.is_empty() {
                        "no learned tools yet".to_string()
                    } else {
                        knowledge
                            .associated_tools
                            .iter()
                            .take(3)
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", ")
                    };
                    learned.entity_hints.push(format!(
                        "Entity '{entity}' is associated with domain {domain} and tools [{tools}] (confidence {:.2})",
                        confidence
                    ));
                }
            }
        }

        if let Some(patterns) = &self.pattern_library {
            let patterns = match patterns.lock() {
                Ok(p) => p,
                Err(e) => e.into_inner(),
            };
            for pattern in patterns.suggest(routing.task_type, routing.domain_hint, 2) {
                learned.pattern_hints.push(format!(
                    "Successful tool chain for {:?}/{:?}: {} (success {:.0}%, quality {:.2})",
                    pattern.task_type,
                    pattern.domain,
                    pattern.tools.join(" -> "),
                    pattern.success_rate() * 100.0,
                    pattern.avg_quality()
                ));
            }
        }

        if let Some(calibrator) = &self.progressive_calibrator {
            let calibrator = match calibrator.lock() {
                Ok(c) => c,
                Err(e) => e.into_inner(),
            };
            let intent = format!("{:?}", routing.task_type).to_lowercase();
            let mut calibration_candidates = Vec::new();

            if let Some(stats) = calibrator.intent_stats(&intent)
                && stats.has_enough_data()
                && stats.correction_rate() >= 0.30
            {
                calibration_candidates.push((
                    stats.correction_rate(),
                    format!(
                        "Calibration risk: intent '{intent}' needed correction {:.0}% of the time across {} observations",
                        stats.correction_rate() * 100.0,
                        stats.total
                    ),
                ));
            }
            if let Some(domain) = routing.domain_hint
                && let Some(stats) = calibrator.domain_stats(domain)
                && stats.has_enough_data()
                && stats.correction_rate() >= 0.30
            {
                calibration_candidates.push((
                    stats.correction_rate(),
                    format!(
                        "Calibration risk: domain {domain:?} needed correction {:.0}% of the time across {} observations",
                        stats.correction_rate() * 100.0,
                        stats.total
                    ),
                ));
            }
            if let Some(stats) = calibrator.task_stats(routing.task_type)
                && stats.has_enough_data()
                && stats.correction_rate() >= 0.30
            {
                calibration_candidates.push((
                    stats.correction_rate(),
                    format!(
                        "Calibration risk: task {:?} needed correction {:.0}% of the time across {} observations",
                        routing.task_type,
                        stats.correction_rate() * 100.0,
                        stats.total
                    ),
                ));
            }
            calibration_candidates
                .sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
            learned.calibration_hints.extend(
                calibration_candidates
                    .into_iter()
                    .take(2)
                    .map(|(_, hint)| hint),
            );
        }

        if let Some(tracker) = &self.quality_tracker {
            let tracker = match tracker.lock() {
                Ok(t) => t,
                Err(e) => e.into_inner(),
            };
            let mut entries: Vec<_> = tracker
                .all_entries()
                .iter()
                .filter(|(_, entry)| entry.selections >= 3)
                .map(|(tool, entry)| (tool.as_str(), entry.boost_factor(), entry))
                .collect();
            entries.sort_by(|a, b| {
                let lhs = (a.1 - 1.0).abs();
                let rhs = (b.1 - 1.0).abs();
                rhs.partial_cmp(&lhs).unwrap_or(std::cmp::Ordering::Equal)
            });

            let mut pushed_positive = false;
            let mut pushed_negative = false;
            for (tool, boost, entry) in entries {
                if !pushed_positive && boost > 1.05 {
                    learned.tool_hints.push(format!(
                        "Tool history: prefer '{tool}' (use-rate {:.0}%, avg quality {:.2})",
                        entry.use_rate() * 100.0,
                        entry.avg_quality()
                    ));
                    pushed_positive = true;
                } else if !pushed_negative && boost < 0.95 {
                    learned.tool_hints.push(format!(
                        "Tool history: be cautious with '{tool}' (use-rate {:.0}%, avg quality {:.2})",
                        entry.use_rate() * 100.0,
                        entry.avg_quality()
                    ));
                    pushed_negative = true;
                }

                if pushed_positive && pushed_negative {
                    break;
                }
            }
        }

        if learned.entity_hints.is_empty()
            && learned.pattern_hints.is_empty()
            && learned.calibration_hints.is_empty()
            && learned.tool_hints.is_empty()
        {
            learned.task_archetype = None;
        }

        learned
    }

    /// Record the outcome of a turn for learning.
    ///
    /// Call this after a turn completes to update:
    /// - EntityGraph: entity → domain → tools associations
    /// - PatternLibrary: tool chain success/failure patterns
    /// - ProgressiveCalibrator: per-intent/domain/task correction rates
    #[allow(clippy::too_many_arguments)]
    pub fn record_turn_outcome(
        &self,
        query: &str,
        tools_used: &[String],
        task_type: TaskType,
        domain: Option<DomainHint>,
        success: bool,
        quality: f64,
        was_corrected: bool,
        user_feedback_score: Option<i64>,
    ) {
        // Learn entity → domain → tools associations
        if success
            && let Some(eg) = &self.entity_graph
            && let Ok(mut graph) = eg.lock()
        {
            let entities = extract_entities(query);
            if let Some(d) = domain {
                for entity in &entities {
                    graph.learn(entity, d, tools_used, user_feedback_score);
                }
            }
        }

        // Record tool chain pattern
        if let Some(pl) = &self.pattern_library
            && let Ok(mut lib) = pl.lock()
        {
            lib.record_outcome(
                tools_used,
                task_type,
                domain,
                success,
                quality,
                user_feedback_score,
            );
        }

        // Record calibration data
        if let Some(pc) = &self.progressive_calibrator
            && let Ok(mut cal) = pc.lock()
        {
            let intent = format!("{task_type:?}").to_lowercase();
            cal.record(
                &intent,
                domain,
                task_type,
                was_corrected,
                user_feedback_score,
            );
        }
    }
}

#[async_trait]
impl ToolSelector for TfIdfSelector {
    fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    fn learned_context(&self, query: &str, _recent_tools: &[String]) -> LearnedContext {
        self.summarize_learned_context(query)
    }

    async fn select(&self, ctx: &SelectionContext<'_>) -> SelectionResult {
        // ── Phase 1: Gather boost terms from pipeline modules ──
        let entity_boost = self.entity_boost_terms(ctx.query);
        let all_boost: Vec<String> = ctx
            .boost_terms
            .iter()
            .chain(entity_boost.iter())
            .cloned()
            .collect();

        // ── Phase 2: Compute unified routing decision ──
        let routing = RoutingEngine::analyze(
            ctx.query,
            ctx.turn_count,
            ctx.recent_tools,
            &[], // memory_hints (populated by caller via boost_terms)
            all_boost.clone(),
        );

        // ── Phase 3: Add pattern boost terms (needs task_type from routing) ──
        let pattern_boost = self.pattern_boost_terms(routing.task_type, routing.domain_hint);

        // ── Phase 3b: Compute co-occurrence scores from learned patterns ──
        let co_occurrence = self
            .pattern_library
            .as_ref()
            .and_then(|pl| pl.lock().ok())
            .map(|lib| {
                lib.co_occurrence_scores(
                    &ctx.recent_tools
                        .iter()
                        .map(|s| s.to_string())
                        .collect::<Vec<_>>(),
                )
            })
            .unwrap_or_default();

        // ── Phase 4: Select tools via RoutingDecision path ──
        // Apply budget pressure: reduce effective budget under token pressure.
        // pressure=0.0 → full budget, pressure=1.0 → 30% budget.
        let effective_budget = if ctx.budget_pressure > 0.0 {
            let scale = 1.0 - ctx.budget_pressure.clamp(0.0, 1.0) * 0.7;
            (ctx.budget_tokens as f64 * scale) as u32
        } else {
            ctx.budget_tokens
        };

        let tracker_guard: Option<std::sync::MutexGuard<'_, ToolQualityTracker>> =
            self.quality_tracker.as_ref().and_then(|t| t.lock().ok());
        let tracker_ref: Option<&ToolQualityTracker> = tracker_guard.as_deref();
        let calibrator_ref = self.confidence_calibrator.as_deref();

        let (_schemas, report) = self.registry.select_routed_with_pressure(
            ctx.query,
            &routing,
            effective_budget,
            &pattern_boost,
            tracker_ref,
            calibrator_ref,
            &ctx.memory_domain_hints,
            ctx.budget_pressure,
            &co_occurrence,
            &ctx.file_context,
        );
        drop(tracker_guard);

        // ── Phase 4b: Filter out restricted tools (deprioritized / stall-blocked) ──
        let filtered_tools: Vec<String> = if ctx.restricted_tools.is_empty() {
            report.tools_selected
        } else {
            report
                .tools_selected
                .into_iter()
                .filter(|name| !ctx.restricted_tools.contains(name))
                .collect()
        };

        // Record selection in quality tracker
        if let Some(qt) = &self.quality_tracker
            && let Ok(mut guard) = qt.lock()
        {
            guard.record_selection(&filtered_tools);
        }

        // ── Phase 5: Use routing confidence (richer than signal-count heuristic) ──
        // Blend routing confidence with dynamic-tools-selected factor
        let dynamic_selected = filtered_tools
            .iter()
            .filter(|n| {
                !TOOL_CATALOG
                    .iter()
                    .any(|t| t.pinned && t.name == n.as_str())
            })
            .count();
        let tool_factor: f64 = match dynamic_selected {
            0 => 0.0,
            1..=2 => 0.15,
            _ => 0.3,
        };
        let confidence = (routing.confidence * 0.7 + tool_factor).min(1.0);

        SelectionResult {
            tool_names: filtered_tools,
            strategy: "tfidf_routed",
            budget_used: report.budget_used,
            failed: false,
            confidence,
            selector_tokens_in: 0,
            selector_tokens_out: 0,
            selected_skills: vec![], // TF-IDF doesn't select skills (requires semantic understanding)
        }
    }

    fn record_outcome(
        &self,
        query: &str,
        tools_used: &[String],
        task_type: TaskType,
        domain: Option<DomainHint>,
        success: bool,
        quality: f64,
        was_corrected: bool,
        user_feedback_score: Option<i64>,
    ) {
        self.record_turn_outcome(
            query,
            tools_used,
            task_type,
            domain,
            success,
            quality,
            was_corrected,
            user_feedback_score,
        );
    }
}

/// Compute selection confidence from signal count and dynamic tools selected.
/// Returns 0.0–1.0. Low confidence triggers advisory in system prompt.
pub fn compute_selection_confidence(signal_count: usize, dynamic_tools_selected: usize) -> f64 {
    let signal_conf: f64 = match signal_count {
        0 => 0.0,
        1 => 0.3,
        2 => 0.6,
        _ => 0.8,
    };
    let tool_conf: f64 = match dynamic_tools_selected {
        0 => 0.0,
        1..=2 => 0.2,
        _ => 0.4,
    };
    (signal_conf * 0.6 + tool_conf * 0.4).min(1.0)
}

// ─── Compact tool catalog for LLM prompt ─────────────────────────────────────

/// One-line-per-tool catalog summary for the LLM tool selector.
/// ~200 tokens — much cheaper than sending full schemas (~3800 tokens).
fn build_catalog_summary() -> String {
    TOOL_CATALOG
        .iter()
        .map(|t| {
            format!(
                "- {}: {}",
                t.name,
                t.description.split('.').next().unwrap_or("")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build skill catalog summary for LLM tool selector.
/// Skills are prefixed with [SKILL] to distinguish from tools.
fn build_skill_catalog_summary(skills: &[SkillCatalogEntry]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    skills
        .iter()
        .map(|s| {
            format!(
                "- [SKILL] {}: {}",
                s.name,
                s.description.split('.').next().unwrap_or(&s.description)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build combined catalog (tools + skills) for LLM.
fn build_combined_catalog(skills: &[SkillCatalogEntry]) -> String {
    let tools = build_catalog_summary();
    let skills_summary = build_skill_catalog_summary(skills);
    if skills_summary.is_empty() {
        tools
    } else {
        format!("{}\n\n# Skills (select when task matches):\n{}", tools, skills_summary)
    }
}

/// System prompt for the tool selection LLM call.
const TOOL_SELECT_SYSTEM: &str = "\
You are a tool selector. Given the user's query and context, decide which tools and skills are needed.
Return ONLY a JSON array of names. Select 1-5 items total. Do not explain.
Pinned tools (bash, read_file, write_file, str_replace, list_dir, grep, glob) are always available — do NOT include them.
Skills are prefixed with [SKILL] in the catalog. Include the skill name (without prefix) if the task matches.
Only select from the dynamic tools and skills listed below.";

fn build_tool_select_prompt(
    query: &str,
    recent_tools: &[String],
    learned_context: &LearnedContext,
    catalog: &str,
) -> Vec<Value> {
    let system = format!("{}\n\nDynamic tools and skills:\n{}", TOOL_SELECT_SYSTEM, catalog);
    let mut user_msg = format!("Query: {}", query);
    if !recent_tools.is_empty() {
        user_msg.push_str(&format!("\nRecently used: {:?}", recent_tools));
    }
    if !learned_context.is_empty() {
        user_msg.push_str(&format!("\n{}", learned_context.prompt_fragment()));
    }
    vec![
        serde_json::json!({"role": "system", "content": system}),
        serde_json::json!({"role": "user", "content": user_msg}),
    ]
}

/// Parse tool names from LLM response text.
/// Handles: `["a", "b"]`, markdown code blocks, trailing text.
fn parse_tool_names_from_llm(text: &str) -> Vec<String> {
    // Find the first JSON array in the text
    let trimmed = text.trim();
    let json_str = if let Some(start) = trimmed.find('[') {
        if let Some(end) = trimmed[start..].find(']') {
            &trimmed[start..start + end + 1]
        } else {
            return vec![];
        }
    } else {
        return vec![];
    };

    serde_json::from_str::<Vec<String>>(json_str).unwrap_or_default()
}

// ─── LLM-based tool selector ────────────────────────────────────────────────

/// LLM-based tool selector. Makes a lightweight chat/turn call to select tools.
///
/// Uses the same `{base}/chat/turn` endpoint as the main chat loop, but with:
/// - A compact system prompt (~200 tokens)
/// - No tool schemas (the LLM just generates text)
/// - A 5-second timeout (tool selection should be fast)
///
/// If the call fails, returns an empty result so the fallback can take over.
pub struct LlmToolSelector {
    client: reqwest::Client,
    base_url: String,
    token: String,
    model: Option<String>,
    catalog_summary: String,
    /// Skill names registered for selection (used to filter LLM response).
    skill_names: std::collections::HashSet<String>,
}

impl LlmToolSelector {
    pub fn new(client: reqwest::Client, base_url: String, token: String) -> Self {
        let catalog_summary = build_catalog_summary();
        Self {
            client,
            base_url,
            token,
            model: None,
            catalog_summary,
            skill_names: std::collections::HashSet::new(),
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Register skills for selection. Call this after skills are discovered.
    pub fn with_skills(mut self, skills: Vec<SkillCatalogEntry>) -> Self {
        self.skill_names = skills.iter().map(|s| s.name.clone()).collect();
        self.catalog_summary = build_combined_catalog(&skills);
        self
    }

    /// Update skill catalog (e.g., when skills are dynamically loaded).
    pub fn update_skills(&mut self, skills: &[SkillCatalogEntry]) {
        self.skill_names = skills.iter().map(|s| s.name.clone()).collect();
        self.catalog_summary = build_combined_catalog(skills);
    }

    /// Make a lightweight SSE call and collect the full response text.
    /// Returns (text, tokens_in, tokens_out).
    async fn call_llm(&self, messages: Vec<Value>) -> Result<(String, u64, u64), String> {
        let mut payload = serde_json::json!({
            "messages": messages,
        });
        if let Some(ref model) = self.model {
            payload["model"] = Value::String(model.clone());
        }

        let resp = self
            .client
            .post(format!("{}/chat/turn", self.base_url))
            .header("authorization", format!("Bearer {}", self.token))
            .header("accept", "text/event-stream")
            .json(&payload)
            .timeout(Duration::from_secs(8))
            .send()
            .await
            .map_err(|e| format!("tool-select LLM call failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("tool-select LLM error {status}: {body}"));
        }

        // Collect SSE text events
        let full_body = resp.text().await.map_err(|e| e.to_string())?;
        let mut text = String::new();
        let mut tin: u64 = 0;
        let mut tout: u64 = 0;
        for line in full_body.lines() {
            if let Some(data) = line.strip_prefix("data: ") {
                if data == "[DONE]" {
                    break;
                }
                if let Ok(chunk) = serde_json::from_str::<Value>(data) {
                    // Standard OpenAI streaming format
                    if let Some(delta) = chunk
                        .get("choices")
                        .and_then(|c| c.get(0))
                        .and_then(|c| c.get("delta"))
                        .and_then(|d| d.get("content"))
                        .and_then(Value::as_str)
                    {
                        text.push_str(delta);
                    }
                    // Usage in final chunk
                    if let Some(usage) = chunk.get("usage") {
                        tin = usage
                            .get("prompt_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        tout = usage
                            .get("completion_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                    }
                    // InProcess bridge format
                    if let Some(t) = chunk.get("type").and_then(Value::as_str) {
                        if t == "text_delta"
                            && let Some(d) = chunk.get("content").and_then(Value::as_str)
                        {
                            text.push_str(d);
                        }
                        if t == "_inprocess_summary"
                            && let Some(ft) = chunk.get("full_text").and_then(Value::as_str)
                        {
                            tin = chunk
                                .get("prompt_tokens")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0);
                            tout = chunk
                                .get("completion_tokens")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0);
                            return Ok((ft.to_string(), tin, tout));
                        }
                    }
                }
            }
        }
        Ok((text, tin, tout))
    }
}

#[async_trait]
impl ToolSelector for LlmToolSelector {
    async fn select(&self, ctx: &SelectionContext<'_>) -> SelectionResult {
        self.select_with_learned_context(ctx, &LearnedContext::default())
            .await
    }

    fn selected_skills_empty(&self) -> bool {
        self.skill_names.is_empty()
    }

    async fn select_with_learned_context(
        &self,
        ctx: &SelectionContext<'_>,
        learned_context: &LearnedContext,
    ) -> SelectionResult {
        // Debug: log skill_names and catalog_summary presence
        if std::env::var("MO_DEBUG_SKILLS").is_ok() {
            eprintln!("[DEBUG] LlmToolSelector skill_names: {:?}", self.skill_names);
            eprintln!("[DEBUG] Catalog includes skills: {}", self.catalog_summary.contains("[SKILL]"));
        }
        
        let messages = build_tool_select_prompt(
            ctx.query,
            ctx.recent_tools,
            learned_context,
            &self.catalog_summary,
        );

        match self.call_llm(messages).await {
            Ok((text, tin, tout)) => {
                let names = parse_tool_names_from_llm(&text);
                
                // Debug: log LLM raw response and parsed names
                if std::env::var("MO_DEBUG_SKILLS").is_ok() {
                    eprintln!("[DEBUG] LLM raw response: {:?}", text);
                    eprintln!("[DEBUG] LLM parsed names: {:?}", names);
                }
                
                let valid_tools: std::collections::HashSet<&str> =
                    TOOL_CATALOG.iter().map(|t| t.name).collect();
                
                // Separate tools from skills
                let mut tool_names = Vec::new();
                let mut selected_skills = Vec::new();
                
                for name in names {
                    if valid_tools.contains(name.as_str()) {
                        tool_names.push(name);
                    } else if self.skill_names.contains(&name) {
                        selected_skills.push(name);
                    }
                    // Ignore unknown names
                }

                if tool_names.is_empty() && selected_skills.is_empty() {
                    return SelectionResult {
                        tool_names: vec![],
                        strategy: "llm_empty",
                        budget_used: 0,
                        failed: true,
                        confidence: 0.0,
                        selector_tokens_in: tin,
                        selector_tokens_out: tout,
                        selected_skills: vec![],
                    };
                }

                SelectionResult {
                    tool_names,
                    strategy: "llm",
                    budget_used: 0,
                    failed: false,
                    confidence: 0.9,
                    selector_tokens_in: tin,
                    selector_tokens_out: tout,
                    selected_skills,
                }
            }
            Err(e) => {
                // LLM call failed — signal fallback
                if std::env::var("MO_DEBUG_SKILLS").is_ok() {
                    eprintln!("[DEBUG] LLM tool selection error: {}", e);
                }
                SelectionResult {
                    tool_names: vec![],
                    strategy: "llm_error",
                    budget_used: 0,
                    failed: true,
                    confidence: 0.0,
                    selector_tokens_in: 0,
                    selector_tokens_out: 0,
                    selected_skills: vec![],
                }
            }
        }
    }
}

// ─── Fallback selector ──────────────────────────────────────────────────────

/// Chained selector: tries `primary`, falls back to `fallback` if primary
/// returns empty or errors.
pub struct FallbackSelector {
    primary: Box<dyn ToolSelector>,
    fallback: Box<dyn ToolSelector>,
}

impl FallbackSelector {
    pub fn new(primary: Box<dyn ToolSelector>, fallback: Box<dyn ToolSelector>) -> Self {
        Self { primary, fallback }
    }
}

#[async_trait]
impl ToolSelector for FallbackSelector {
    fn registry(&self) -> &ToolRegistry {
        self.fallback.registry()
    }

    fn learned_context(&self, query: &str, recent_tools: &[String]) -> LearnedContext {
        self.fallback.learned_context(query, recent_tools)
    }

    async fn select(&self, ctx: &SelectionContext<'_>) -> SelectionResult {
        let learned_context = self.fallback.learned_context(ctx.query, ctx.recent_tools);
        self.select_with_learned_context(ctx, &learned_context)
            .await
    }

    async fn select_with_learned_context(
        &self,
        ctx: &SelectionContext<'_>,
        learned_context: &LearnedContext,
    ) -> SelectionResult {
        // Fast path: if TF-IDF with learned context is confident, skip LLM call.
        let fast_result = self
            .fallback
            .select_with_learned_context(ctx, learned_context)
            .await;

        let has_dynamic_tools = fast_result.tool_names.iter().any(|n| {
            !crate::tool_registry::TOOL_CATALOG
                .iter()
                .any(|t| t.pinned && t.name == n.as_str())
        });

        // High confidence with dynamic tools → trust TF-IDF
        if fast_result.confidence >= 0.7 && has_dynamic_tools {
            return fast_result;
        }

        // Skills require semantic LLM selection. If the primary selector has
        // skills registered, we must still ask it even when TF-IDF only found
        // pinned tools or no dynamic tools.
        let primary_has_skills = !self.primary.selected_skills_empty();

        // No dynamic tools and no skills → no point asking primary.
        if !has_dynamic_tools && !primary_has_skills {
            return fast_result;
        }

        // Low/mid confidence with dynamic tools, or any available skills →
        // ask the primary selector for a better result.
        let result = self
            .primary
            .select_with_learned_context(ctx, learned_context)
            .await;
        if !result.failed && (!result.tool_names.is_empty() || !result.selected_skills.is_empty()) {
            result
        } else {
            fast_result
        }
    }

    fn record_outcome(
        &self,
        query: &str,
        tools_used: &[String],
        task_type: TaskType,
        domain: Option<DomainHint>,
        success: bool,
        quality: f64,
        was_corrected: bool,
        user_feedback_score: Option<i64>,
    ) {
        // Forward to fallback (TfIdfSelector) — it has the pipeline modules.
        self.fallback.record_outcome(
            query,
            tools_used,
            task_type,
            domain,
            success,
            quality,
            was_corrected,
            user_feedback_score,
        );
    }
}

// ─── Helpers for callers ────────────────────────────────────────────────────

/// Given selected tool names from a [`ToolSelector`], resolve them to full
/// JSON schemas from the registry.  Pinned tools are always included, but
/// under extreme token pressure (`budget_pressure >= 0.8`), non-core pinned
/// tools (memory_store, memory_search) are demoted unless they were
/// explicitly selected by the scoring pipeline.
pub fn resolve_schemas(
    registry: &ToolRegistry,
    selected_names: &[String],
) -> (Vec<Value>, tool_registry::SelectionReport) {
    resolve_schemas_with_pressure(registry, selected_names, 0.0)
}

/// Pressure-aware variant of [`resolve_schemas`].
///
/// At increasing pressure levels, schemas are progressively pruned:
/// - `>= 0.3`: Truncate descriptions + remove param descriptions (saves ~40%)
/// - `>= 0.6`: Remove descriptions entirely (saves ~60%)
/// - `>= 0.8`: Also skip deferrable pinned tools + strip optional params (saves ~70%)
pub fn resolve_schemas_with_pressure(
    registry: &ToolRegistry,
    selected_names: &[String],
    budget_pressure: f64,
) -> (Vec<Value>, tool_registry::SelectionReport) {
    // Under high pressure, skip non-core pinned tools unless explicitly selected.
    const DEFERRABLE_PINNED: &[&str] = &["memory_store", "memory_search"];
    let skip_deferrable = budget_pressure >= 0.8
        && !selected_names
            .iter()
            .any(|n| DEFERRABLE_PINNED.contains(&n.as_str()));

    // Determine pruning level based on pressure
    let prune_level = if budget_pressure >= 0.8 {
        PruneLevel::Aggressive
    } else if budget_pressure >= 0.6 {
        PruneLevel::Medium
    } else if budget_pressure >= 0.3 {
        PruneLevel::Light
    } else {
        PruneLevel::None
    };

    let mut schemas = Vec::new();
    let mut names = Vec::new();
    let pinned_names: std::collections::HashSet<&str> = TOOL_CATALOG
        .iter()
        .filter(|t| t.pinned)
        .map(|t| t.name)
        .collect();

    // Use pre-resolved pinned schemas (cached at registry construction)
    for (name, schema) in registry.pinned_schemas() {
        if skip_deferrable && DEFERRABLE_PINNED.contains(&name.as_str()) {
            continue;
        }
        schemas.push(prune_schema(schema.clone(), prune_level));
        names.push(name.clone());
    }

    // Add dynamic tools via O(1) index lookup (replaces linear search)
    for name in selected_names {
        if names.contains(name) {
            continue; // already included as pinned
        }
        if let Some(schema) = registry.schema_by_name(name) {
            schemas.push(prune_schema(schema.clone(), prune_level));
            names.push(name.clone());
        }
    }

    let budget_used: u32 = names
        .iter()
        .filter(|n| !pinned_names.contains(n.as_str()))
        .map(|n| registry.token_cost(n))
        .sum();

    let report = tool_registry::SelectionReport {
        tools_selected: names,
        selected_count: schemas.len() as u32,
        budget_used,
        budget_total: registry.default_budget(),
    };

    (schemas, report)
}

// ── Schema pruning ──────────────────────────────────────────────────────────

/// Pruning intensity level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PruneLevel {
    /// No pruning — full schema.
    None,
    /// Truncate tool description to 80 chars, remove param descriptions.
    Light,
    /// Remove tool description entirely, remove param descriptions.
    Medium,
    /// Remove descriptions, strip optional parameters (keep only required).
    Aggressive,
}

/// Prune a single tool schema to reduce token cost.
pub fn prune_schema(mut schema: Value, level: PruneLevel) -> Value {
    if level == PruneLevel::None {
        return schema;
    }

    let func = match schema.get_mut("function") {
        Some(f) => f,
        None => return schema,
    };

    // Prune function description
    match level {
        PruneLevel::Light => {
            if let Some(Value::String(desc)) = func.get("description")
                && desc.len() > 80
            {
                let truncated = truncate_at_boundary(desc, 80);
                func["description"] = Value::String(truncated);
            }
        }
        PruneLevel::Medium | PruneLevel::Aggressive => {
            func.as_object_mut().map(|m| m.remove("description"));
        }
        PruneLevel::None => {}
    }

    // Prune parameter descriptions
    if let Some(params) = func.get_mut("parameters") {
        // Extract required names first (before mutable borrow of properties)
        let required_names: std::collections::HashSet<String> = if level == PruneLevel::Aggressive {
            params
                .get("required")
                .and_then(|r| r.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default()
        } else {
            std::collections::HashSet::new()
        };

        if let Some(props) = params.get_mut("properties")
            && let Some(obj) = props.as_object_mut()
        {
            // Aggressive: strip optional params (keep only required)
            if level == PruneLevel::Aggressive && !required_names.is_empty() {
                obj.retain(|name, _| required_names.contains(name));
            }

            // Light/Medium/Aggressive: remove param descriptions
            for (_name, prop) in obj.iter_mut() {
                if let Some(p) = prop.as_object_mut() {
                    p.remove("description");
                }
            }
        }
    }

    schema
}

/// Truncate a string at a word boundary, safely handling multi-byte UTF-8.
fn truncate_at_boundary(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    // Walk back to a valid char boundary (handles CJK, emoji, etc.)
    let mut byte_idx = max;
    while byte_idx > 0 && !s.is_char_boundary(byte_idx) {
        byte_idx -= 1;
    }
    // Find last space before the safe boundary for a clean word break
    match s[..byte_idx].rfind(' ') {
        Some(pos) => format!("{}…", &s[..pos]),
        None => format!("{}…", &s[..byte_idx]),
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_registry::{ConversationState, IntentType, TOOL_CATALOG, pre_filter_dynamic};

    fn mock_registry() -> ToolRegistry {
        let schemas: Vec<Value> = TOOL_CATALOG
            .iter()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": {"type": "object", "properties": {}}
                    }
                })
            })
            .collect();
        ToolRegistry::new(schemas)
    }

    // ── Parse LLM response ──

    #[test]
    fn parse_json_array() {
        let names = parse_tool_names_from_llm(r#"["github_list_prs", "memory_search"]"#);
        assert_eq!(names, vec!["github_list_prs", "memory_search"]);
    }

    #[test]
    fn parse_json_in_markdown_block() {
        let text = "```json\n[\"git_log\", \"git_diff\"]\n```";
        let names = parse_tool_names_from_llm(text);
        assert_eq!(names, vec!["git_log", "git_diff"]);
    }

    #[test]
    fn parse_json_with_trailing_text() {
        let text = "Based on the query, I'd select:\n[\"github_list_prs\"]\nThese tools...";
        let names = parse_tool_names_from_llm(text);
        assert_eq!(names, vec!["github_list_prs"]);
    }

    #[test]
    fn parse_no_json_returns_empty() {
        let names = parse_tool_names_from_llm("I don't know which tools to use");
        assert!(names.is_empty());
    }

    #[test]
    fn parse_malformed_json_returns_empty() {
        let names = parse_tool_names_from_llm("[github_list_prs]");
        assert!(names.is_empty());
    }

    // ── Catalog summary ──

    #[test]
    fn catalog_summary_covers_all_dynamic_tools() {
        let summary = build_catalog_summary();
        for tool in TOOL_CATALOG.iter().filter(|t| !t.pinned) {
            assert!(
                summary.contains(tool.name),
                "catalog summary missing tool: {}",
                tool.name
            );
        }
    }

    #[test]
    fn catalog_summary_is_compact() {
        let summary = build_catalog_summary();
        // Should be under 1000 chars (~250 tokens)
        assert!(
            summary.len() < 2500,
            "catalog summary too long: {} chars",
            summary.len()
        );
    }

    // ── Prompt construction ──

    #[test]
    fn prompt_includes_recent_tools() {
        let messages = build_tool_select_prompt(
            "matrixone呢？",
            &["github_list_prs".to_string()],
            &LearnedContext::default(),
            "catalog",
        );
        let user_msg = messages[1]["content"].as_str().unwrap();
        assert!(user_msg.contains("github_list_prs"));
        assert!(user_msg.contains("matrixone"));
    }

    #[test]
    fn prompt_includes_learned_runtime_context() {
        let messages = build_tool_select_prompt(
            "matrixone呢？",
            &[],
            &LearnedContext {
                task_archetype: Some(TaskType::Fetch),
                entity_hints: vec!["Entity 'matrixorigin' is associated with domain GitHub".into()],
                pattern_hints: vec!["Successful tool chain for Fetch/Some(GitHub): github_search -> github_list_prs".into()],
                calibration_hints: vec!["Calibration risk: domain GitHub needed correction 60% of the time".into()],
                tool_hints: vec!["Tool history: prefer 'github_list_prs'".into()],
            },
            "catalog",
        );
        let user_msg = messages[1]["content"].as_str().unwrap();
        assert!(user_msg.contains("Learned runtime context"));
        assert!(user_msg.contains("matrixorigin"));
        assert!(user_msg.contains("github_list_prs"));
        assert!(user_msg.contains("Calibration risk"));
        assert!(user_msg.contains("Tool history"));
    }

    #[test]
    fn tfidf_selector_surfaces_learned_context_from_entity_and_pattern_memory() {
        let mut graph = EntityGraph::new();
        graph.learn(
            "matrixorigin",
            DomainHint::GitHub,
            &["github_search".into(), "github_list_prs".into()],
            None,
        );

        let mut patterns = PatternLibrary::new();
        for _ in 0..2 {
            patterns.record_outcome(
                &["github_search".into(), "github_list_prs".into()],
                TaskType::Fetch,
                Some(DomainHint::GitHub),
                true,
                0.95,
                None,
            );
        }

        let selector = TfIdfSelector::new(mock_registry())
            .with_entity_graph(Arc::new(Mutex::new(graph)))
            .with_pattern_library(Arc::new(Mutex::new(patterns)));

        let learned = selector.learned_context("matrixorigin 最新 pr", &[]);
        assert_eq!(learned.task_archetype, Some(TaskType::Fetch));
        assert!(
            learned
                .entity_hints
                .iter()
                .any(|hint| hint.contains("matrixorigin") && hint.contains("GitHub"))
        );
        assert!(
            learned
                .pattern_hints
                .iter()
                .any(|hint| hint.contains("github_search -> github_list_prs"))
        );
    }

    #[test]
    fn learned_context_filters_low_confidence_entity_hints() {
        let mut graph = EntityGraph::new();
        graph.merge(&[crate::pipeline::entity::EntityKnowledge {
            name: "stale-org".into(),
            aliases: vec![],
            domain: Some(DomainHint::GitHub),
            associated_tools: vec!["github_list_prs".into()],
            confidence: 0.2,
            observation_count: 1,
            last_observed_at: chrono::Utc::now().timestamp() as u64,
        }]);

        let selector =
            TfIdfSelector::new(mock_registry()).with_entity_graph(Arc::new(Mutex::new(graph)));
        let learned = selector.learned_context("stale-org 最新 pr", &[]);
        assert!(
            learned.entity_hints.is_empty(),
            "low-confidence entity should be filtered"
        );
    }

    #[test]
    fn learned_context_keeps_high_confidence_entity_hints() {
        let mut graph = EntityGraph::new();
        graph.learn(
            "matrixorigin",
            DomainHint::GitHub,
            &["github_search".into(), "github_list_prs".into()],
            None,
        );
        graph.learn(
            "matrixorigin",
            DomainHint::GitHub,
            &["github_search".into(), "github_list_prs".into()],
            None,
        );

        let selector =
            TfIdfSelector::new(mock_registry()).with_entity_graph(Arc::new(Mutex::new(graph)));
        let learned = selector.learned_context("matrixorigin 最新 pr", &[]);
        assert!(
            learned
                .entity_hints
                .iter()
                .any(|hint| hint.contains("matrixorigin") && hint.contains("confidence"))
        );
    }

    #[test]
    fn tfidf_selector_surfaces_calibration_and_tool_history_hints() {
        let tracker = Arc::new(Mutex::new(ToolQualityTracker::new()));
        {
            let mut tracker = tracker.lock().unwrap();
            for _ in 0..5 {
                tracker.record_selection(&["github_list_prs".into()]);
                tracker.record_feedback(&crate::tool_registry::SelectionFeedback {
                    tools_used: vec!["github_list_prs".into()],
                    unused_count: 0,
                    precision: 1.0,
                    recall: 1.0,
                });
                tracker.record_quality("github_list_prs", 0.95);
                tracker.record_selection(&["glob".into()]);
            }
        }

        let calibrator = Arc::new(Mutex::new(ProgressiveCalibrator::new(0.7)));
        {
            let mut calibrator = calibrator.lock().unwrap();
            for _ in 0..5 {
                calibrator.record(
                    "fetch",
                    Some(DomainHint::GitHub),
                    TaskType::Fetch,
                    true,
                    None,
                );
            }
        }

        let selector = TfIdfSelector::new(mock_registry())
            .with_quality_tracker(tracker)
            .with_progressive_calibrator(calibrator);

        let learned = selector.learned_context("matrixorigin 最新 pr", &[]);
        assert!(
            learned
                .calibration_hints
                .iter()
                .any(|hint| hint.contains("Calibration risk") && hint.contains("GitHub"))
        );
        assert!(
            learned
                .tool_hints
                .iter()
                .any(|hint| hint.contains("prefer 'github_list_prs'"))
        );
        assert!(
            learned
                .tool_hints
                .iter()
                .any(|hint| hint.contains("cautious with 'glob'"))
        );
    }

    // ── TfIdfSelector ──

    #[tokio::test]
    async fn tfidf_pr_query_includes_github() {
        let selector = TfIdfSelector::new(mock_registry());
        let ctx = SelectionContext {
            query: "matrixorigin memoria 最新的pr?",
            turn_count: 1,
            recent_tools: &[],
            budget_tokens: 800,
            boost_terms: vec![],
            budget_pressure: 0.0,
            memory_domain_hints: vec![],
            restricted_tools: vec![],
            file_context: vec![],
        };
        let result = selector.select(&ctx).await;
        assert!(
            result.tool_names.contains(&"github_list_prs".to_string()),
            "PR query must select github_list_prs, got: {:?}",
            result.tool_names
        );
        assert_eq!(result.strategy, "tfidf_routed");
    }

    #[tokio::test]
    async fn tfidf_conversational_only_pinned() {
        let selector = TfIdfSelector::new(mock_registry());
        let ctx = SelectionContext {
            query: "你好",
            turn_count: 1,
            recent_tools: &[],
            budget_tokens: 800,
            boost_terms: vec![],
            budget_pressure: 0.0,
            memory_domain_hints: vec![],
            restricted_tools: vec![],
            file_context: vec![],
        };
        let result = selector.select(&ctx).await;
        let dynamic_count = result
            .tool_names
            .iter()
            .filter(|n| {
                !TOOL_CATALOG
                    .iter()
                    .any(|t| t.pinned && t.name == n.as_str())
            })
            .count();
        assert_eq!(
            dynamic_count, 0,
            "conversational should have 0 dynamic tools"
        );
    }

    // ── Precision tests (the user's exact scenarios) ──

    #[test]
    fn prefilter_github_query_ranks_github_tools_first() {
        let state = ConversationState::from_message("milvus 的 PR", 1);
        let ranked = pre_filter_dynamic(&state, "milvus 的 PR");

        let top3_names: Vec<_> = ranked
            .iter()
            .take(3)
            .map(|&(idx, _)| TOOL_CATALOG[idx].name)
            .collect();
        assert!(
            top3_names.contains(&"github_list_prs"),
            "github_list_prs should be in top 3 for PR query, got: {:?}",
            top3_names
        );
    }

    #[test]
    fn prefilter_recency_boost_promotes_recent_tool() {
        // "matrixone 最新" triggers is_fetch via "最新" (latest), giving the query
        // a signal that interacts with recency boost for github_list_prs.
        let state = ConversationState::from_message_with_context(
            "matrixone 最新",
            2,
            &["github_list_prs".to_string()],
        );
        let ranked = pre_filter_dynamic(&state, "matrixone 最新");

        let prs_rank = ranked
            .iter()
            .position(|&(idx, _)| TOOL_CATALOG[idx].name == "github_list_prs");
        assert!(
            prs_rank.is_some_and(|r| r < 3),
            "recency boost should promote github_list_prs to top 3, got rank: {:?}",
            prs_rank
        );
    }

    #[test]
    fn prefilter_threshold_filters_irrelevant_tools() {
        // "帮我写个排序算法" = "help me write a sorting algorithm" — pure coding query.
        // With adaptive threshold: 0 signals → threshold=0, but GitHub tools should
        // still rank LOW because TF-IDF for "排序算法" against GitHub tool descriptions
        // is near-zero. They may appear in results but should rank below coding tools.
        let state = ConversationState::from_message("帮我写个排序算法", 1);
        let ranked = pre_filter_dynamic(&state, "帮我写个排序算法");

        // Under adaptive threshold, 0-signal queries get ALL non-zero-score tools.
        // GitHub tools may have tiny scores from generic terms. The key invariant is
        // that they are NOT in the top positions — coding tools should dominate.
        if let Some(github_rank) = ranked
            .iter()
            .position(|&(idx, _)| TOOL_CATALOG[idx].intents.contains(&IntentType::GitHub))
        {
            // If GitHub tools appear at all, they must not be in top 3
            assert!(
                github_rank >= 3 || ranked.len() <= 3,
                "GitHub tools should not rank in top 3 for pure coding query, got rank {}",
                github_rank
            );
        }
        // Either way, the result set should be non-empty (adaptive threshold widens)
        assert!(
            !ranked.is_empty(),
            "0-signal query should still get some dynamic tools"
        );
    }

    #[test]
    fn prefilter_memory_query_includes_memory_tools() {
        let state = ConversationState::from_message("我有哪些记忆？", 1);
        let ranked = pre_filter_dynamic(&state, "我有哪些记忆？");

        let has_memory = ranked
            .iter()
            .any(|&(idx, _)| TOOL_CATALOG[idx].intents.contains(&IntentType::Memory));
        assert!(has_memory, "Memory query should include memory tools");
    }

    #[test]
    fn prefilter_ci_query_includes_github_ci() {
        let state = ConversationState::from_message("memoria最新的ci?", 1);
        let ranked = pre_filter_dynamic(&state, "memoria最新的ci?");

        let top5_names: Vec<_> = ranked
            .iter()
            .take(5)
            .map(|&(idx, _)| TOOL_CATALOG[idx].name)
            .collect();
        assert!(
            top5_names.contains(&"github_ci_status"),
            "CI query should include github_ci_status in top 5, got: {:?}",
            top5_names
        );
    }

    // ── Resolve schemas ──

    #[test]
    fn resolve_schemas_always_includes_pinned() {
        let registry = mock_registry();
        let (schemas, report) = resolve_schemas(&registry, &["github_list_prs".into()]);

        let names = &report.tools_selected;
        assert!(names.contains(&"bash".to_string()), "must include bash");
        assert!(
            names.contains(&"read_file".to_string()),
            "must include read_file"
        );
        assert!(
            names.contains(&"github_list_prs".to_string()),
            "must include requested tool"
        );
        assert_eq!(schemas.len(), report.selected_count as usize);
    }

    #[test]
    fn resolve_schemas_deduplicates_pinned() {
        let registry = mock_registry();
        // Request bash (which is pinned) — should not appear twice
        let (_, report) = resolve_schemas(&registry, &["bash".into(), "github_list_prs".into()]);
        let bash_count = report
            .tools_selected
            .iter()
            .filter(|n| *n == "bash")
            .count();
        assert_eq!(bash_count, 1, "bash should appear exactly once");
    }

    // ── FallbackSelector ──

    fn make_ctx(query: &'static str) -> SelectionContext<'static> {
        SelectionContext {
            query,
            turn_count: 1,
            recent_tools: &[],
            budget_tokens: 800,
            boost_terms: vec![],
            budget_pressure: 0.0,
            memory_domain_hints: vec![],
            restricted_tools: vec![],
            file_context: vec![],
        }
    }

    fn fixed_result(
        tools: Vec<String>,
        strategy: &'static str,
        confidence: f64,
    ) -> SelectionResult {
        SelectionResult {
            tool_names: tools,
            strategy,
            budget_used: 0,
            failed: false,
            confidence,
            selector_tokens_in: 0,
            selector_tokens_out: 0,
            selected_skills: vec![],
        }
    }

    #[tokio::test]
    async fn fallback_skips_primary_when_tfidf_high_confidence_with_dynamic_tools() {
        // High confidence + dynamic tool → TF-IDF result used directly, primary never called
        struct PanicSelector;
        #[async_trait]
        impl ToolSelector for PanicSelector {
            async fn select(&self, _ctx: &SelectionContext<'_>) -> SelectionResult {
                panic!("primary should not be called when TF-IDF is confident");
            }
        }

        struct HighConfSelector;
        #[async_trait]
        impl ToolSelector for HighConfSelector {
            async fn select(&self, _ctx: &SelectionContext<'_>) -> SelectionResult {
                fixed_result(vec!["github_list_prs".into()], "tfidf_high", 0.8)
            }
        }

        let selector = FallbackSelector::new(Box::new(PanicSelector), Box::new(HighConfSelector));
        let result = selector.select(&make_ctx("list prs")).await;
        assert_eq!(result.strategy, "tfidf_high");
    }

    #[tokio::test]
    async fn fallback_skips_primary_for_pinned_only_result() {
        // No dynamic tools (only pinned like bash/memory_search) → skip primary regardless of confidence
        struct PanicSelector;
        #[async_trait]
        impl ToolSelector for PanicSelector {
            async fn select(&self, _ctx: &SelectionContext<'_>) -> SelectionResult {
                panic!("primary should not be called for pinned-only selection");
            }
        }

        struct PinnedOnlySelector;
        #[async_trait]
        impl ToolSelector for PinnedOnlySelector {
            async fn select(&self, _ctx: &SelectionContext<'_>) -> SelectionResult {
                // bash is pinned — no dynamic tools
                fixed_result(vec!["bash".into()], "tfidf_conversational", 0.1)
            }
        }

        let selector = FallbackSelector::new(Box::new(PanicSelector), Box::new(PinnedOnlySelector));
        let result = selector.select(&make_ctx("hi")).await;
        assert_eq!(result.strategy, "tfidf_conversational");
    }

    #[tokio::test]
    async fn fallback_uses_primary_for_skill_only_selection_when_primary_has_skills() {
        struct SkillPrimary;
        #[async_trait]
        impl ToolSelector for SkillPrimary {
            async fn select(&self, _ctx: &SelectionContext<'_>) -> SelectionResult {
                SelectionResult {
                    tool_names: vec![],
                    strategy: "llm_skill",
                    budget_used: 0,
                    failed: false,
                    confidence: 0.9,
                    selector_tokens_in: 0,
                    selector_tokens_out: 0,
                    selected_skills: vec!["tune-performance".into()],
                }
            }

            fn selected_skills_empty(&self) -> bool {
                false
            }
        }

        struct PinnedOnlySelector;
        #[async_trait]
        impl ToolSelector for PinnedOnlySelector {
            async fn select(&self, _ctx: &SelectionContext<'_>) -> SelectionResult {
                fixed_result(vec!["bash".into()], "tfidf_conversational", 0.1)
            }
        }

        let selector = FallbackSelector::new(Box::new(SkillPrimary), Box::new(PinnedOnlySelector));
        let result = selector.select(&make_ctx("tune performance")).await;
        assert_eq!(result.strategy, "llm_skill");
        assert!(result.tool_names.is_empty());
        assert_eq!(result.selected_skills, vec!["tune-performance"]);
    }

    #[tokio::test]
    async fn fallback_uses_primary_when_successful() {
        struct FixedSelector(Vec<String>);
        #[async_trait]
        impl ToolSelector for FixedSelector {
            async fn select(&self, _ctx: &SelectionContext<'_>) -> SelectionResult {
                SelectionResult {
                    tool_names: self.0.clone(),
                    strategy: "fixed",
                    budget_used: 0,
                    failed: false,
                    confidence: 0.9,
                    selector_tokens_in: 0,
                    selector_tokens_out: 0,
                    selected_skills: vec![],
                }
            }
        }

        // Fallback has low confidence with dynamic tool → should escalate to primary (LLM)
        struct LowConfSelector;
        #[async_trait]
        impl ToolSelector for LowConfSelector {
            async fn select(&self, _ctx: &SelectionContext<'_>) -> SelectionResult {
                SelectionResult {
                    tool_names: vec!["github_list_prs".into()],
                    strategy: "tfidf_low",
                    budget_used: 0,
                    failed: false,
                    confidence: 0.3,
                    selector_tokens_in: 0,
                    selector_tokens_out: 0,
                    selected_skills: vec![],
                }
            }
        }

        let primary = Box::new(FixedSelector(vec!["github_list_prs".into()]));
        let fallback = Box::new(LowConfSelector);
        let selector = FallbackSelector::new(primary, fallback);

        let ctx = SelectionContext {
            query: "test",
            turn_count: 1,
            recent_tools: &[],
            budget_tokens: 800,
            boost_terms: vec![],
            budget_pressure: 0.0,
            memory_domain_hints: vec![],
            restricted_tools: vec![],
            file_context: vec![],
        };
        let result = selector.select(&ctx).await;
        // Primary should be called because fallback had dynamic tools with low confidence
        assert_eq!(result.strategy, "fixed");
        assert_eq!(result.tool_names, vec!["github_list_prs"]);
    }

    #[tokio::test]
    async fn fallback_uses_secondary_on_empty() {
        struct EmptySelector;
        #[async_trait]
        impl ToolSelector for EmptySelector {
            async fn select(&self, _ctx: &SelectionContext<'_>) -> SelectionResult {
                SelectionResult {
                    tool_names: vec![],
                    strategy: "llm_error",
                    budget_used: 0,
                    failed: true,
                    confidence: 0.0,
                    selector_tokens_in: 0,
                    selector_tokens_out: 0,
                    selected_skills: vec![],
                }
            }
        }
        struct FixedSelector(Vec<String>);
        #[async_trait]
        impl ToolSelector for FixedSelector {
            async fn select(&self, _ctx: &SelectionContext<'_>) -> SelectionResult {
                SelectionResult {
                    tool_names: self.0.clone(),
                    strategy: "tfidf",
                    budget_used: 100,
                    failed: false,
                    confidence: 0.5,
                    selector_tokens_in: 0,
                    selector_tokens_out: 0,
                    selected_skills: vec![],
                }
            }
        }

        let primary = Box::new(EmptySelector);
        let fallback = Box::new(FixedSelector(vec!["memory_search".into()]));
        let selector = FallbackSelector::new(primary, fallback);

        let ctx = SelectionContext {
            query: "test",
            turn_count: 1,
            recent_tools: &[],
            budget_tokens: 800,
            boost_terms: vec![],
            budget_pressure: 0.0,
            memory_domain_hints: vec![],
            restricted_tools: vec![],
            file_context: vec![],
        };
        let result = selector.select(&ctx).await;
        assert_eq!(result.strategy, "tfidf");
        assert_eq!(result.tool_names, vec!["memory_search"]);
    }

    #[tokio::test]
    async fn fallback_select_with_learned_context_reuses_provided_context() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct EmptyPrimary;
        #[async_trait]
        impl ToolSelector for EmptyPrimary {
            async fn select(&self, _ctx: &SelectionContext<'_>) -> SelectionResult {
                SelectionResult {
                    tool_names: vec![],
                    strategy: "empty_primary",
                    budget_used: 0,
                    failed: true,
                    confidence: 0.0,
                    selector_tokens_in: 0,
                    selector_tokens_out: 0,
                    selected_skills: vec![],
                }
            }
        }

        struct SpyFallback {
            learned_calls: Arc<AtomicUsize>,
        }

        #[async_trait]
        impl ToolSelector for SpyFallback {
            async fn select(&self, _ctx: &SelectionContext<'_>) -> SelectionResult {
                SelectionResult {
                    tool_names: vec![],
                    strategy: "spy_fallback_plain",
                    budget_used: 0,
                    failed: true,
                    confidence: 0.0,
                    selector_tokens_in: 0,
                    selector_tokens_out: 0,
                    selected_skills: vec![],
                }
            }

            fn learned_context(&self, _query: &str, _recent_tools: &[String]) -> LearnedContext {
                self.learned_calls.fetch_add(1, Ordering::SeqCst);
                LearnedContext::default()
            }

            async fn select_with_learned_context(
                &self,
                _ctx: &SelectionContext<'_>,
                learned_context: &LearnedContext,
            ) -> SelectionResult {
                if learned_context
                    .tool_hints
                    .iter()
                    .any(|hint| hint.contains("github_list_prs"))
                {
                    SelectionResult {
                        tool_names: vec!["github_list_prs".into()],
                        strategy: "spy_fallback_learned",
                        budget_used: 0,
                        failed: false,
                        confidence: 0.8,
                        selector_tokens_in: 0,
                        selector_tokens_out: 0,
                        selected_skills: vec![],
                    }
                } else {
                    SelectionResult {
                        tool_names: vec![],
                        strategy: "spy_fallback_plain",
                        budget_used: 0,
                        failed: true,
                        confidence: 0.0,
                        selector_tokens_in: 0,
                        selector_tokens_out: 0,
                        selected_skills: vec![],
                    }
                }
            }
        }

        let learned_calls = Arc::new(AtomicUsize::new(0));
        let selector = FallbackSelector::new(
            Box::new(EmptyPrimary),
            Box::new(SpyFallback {
                learned_calls: learned_calls.clone(),
            }),
        );
        let ctx = SelectionContext {
            query: "matrixorigin 最新 pr",
            turn_count: 1,
            recent_tools: &[],
            budget_tokens: 800,
            boost_terms: vec![],
            budget_pressure: 0.0,
            memory_domain_hints: vec![],
            restricted_tools: vec![],
            file_context: vec![],
        };

        let provided = LearnedContext {
            task_archetype: Some(TaskType::Fetch),
            entity_hints: vec![],
            pattern_hints: vec![],
            calibration_hints: vec![],
            tool_hints: vec!["Tool history: prefer 'github_list_prs'".into()],
        };
        let result = selector.select_with_learned_context(&ctx, &provided).await;
        assert_eq!(result.strategy, "spy_fallback_learned");
        assert_eq!(result.tool_names, vec!["github_list_prs"]);
        assert_eq!(
            learned_calls.load(Ordering::SeqCst),
            0,
            "provided learned context should be reused without recomputing fallback context"
        );
    }

    #[tokio::test]
    async fn fallback_learned_context_can_improve_primary_selection() {
        struct LearnedAwarePrimary;

        #[async_trait]
        impl ToolSelector for LearnedAwarePrimary {
            async fn select(&self, _ctx: &SelectionContext<'_>) -> SelectionResult {
                SelectionResult {
                    tool_names: vec![],
                    strategy: "learned_primary_empty",
                    budget_used: 0,
                    failed: true,
                    confidence: 0.0,
                    selector_tokens_in: 0,
                    selector_tokens_out: 0,
                    selected_skills: vec![],
                }
            }

            async fn select_with_learned_context(
                &self,
                _ctx: &SelectionContext<'_>,
                learned_context: &LearnedContext,
            ) -> SelectionResult {
                let has_github_entity = learned_context
                    .entity_hints
                    .iter()
                    .any(|hint| hint.contains("matrixorigin") && hint.contains("GitHub"));
                let has_pr_pattern = learned_context
                    .pattern_hints
                    .iter()
                    .any(|hint| hint.contains("github_search -> github_list_prs"));
                if has_github_entity && has_pr_pattern {
                    SelectionResult {
                        tool_names: vec!["github_list_prs".into()],
                        strategy: "learned_primary",
                        budget_used: 0,
                        failed: false,
                        confidence: 0.9,
                        selector_tokens_in: 0,
                        selector_tokens_out: 0,
                        selected_skills: vec![],
                    }
                } else {
                    SelectionResult {
                        tool_names: vec![],
                        strategy: "learned_primary_empty",
                        budget_used: 0,
                        failed: true,
                        confidence: 0.0,
                        selector_tokens_in: 0,
                        selector_tokens_out: 0,
                        selected_skills: vec![],
                    }
                }
            }
        }

        let ctx = SelectionContext {
            query: "matrixorigin 最新 pr",
            turn_count: 1,
            recent_tools: &[],
            budget_tokens: 800,
            boost_terms: vec![],
            budget_pressure: 0.0,
            memory_domain_hints: vec![],
            restricted_tools: vec![],
            file_context: vec![],
        };

        let baseline = LearnedAwarePrimary.select(&ctx).await;
        assert!(
            baseline.failed,
            "without learned context, learned-aware primary should fail closed"
        );

        let mut graph = EntityGraph::new();
        graph.learn(
            "matrixorigin",
            DomainHint::GitHub,
            &["github_search".into(), "github_list_prs".into()],
            None,
        );
        graph.learn(
            "matrixorigin",
            DomainHint::GitHub,
            &["github_search".into(), "github_list_prs".into()],
            None,
        );

        let mut patterns = PatternLibrary::new();
        for _ in 0..2 {
            patterns.record_outcome(
                &["github_search".into(), "github_list_prs".into()],
                TaskType::Fetch,
                Some(DomainHint::GitHub),
                true,
                0.95,
                None,
            );
        }

        let fallback = TfIdfSelector::new(mock_registry())
            .with_entity_graph(Arc::new(Mutex::new(graph)))
            .with_pattern_library(Arc::new(Mutex::new(patterns)));

        let selector = FallbackSelector::new(Box::new(LearnedAwarePrimary), Box::new(fallback));
        let result = selector.select(&ctx).await;

        // With learned context, either TF-IDF is confident enough (fast path)
        // or primary gets the learned context and succeeds.
        // Either way, github_list_prs should be selected.
        assert!(
            result.tool_names.contains(&"github_list_prs".to_string()),
            "learned context should help select github_list_prs, got: {:?} (strategy: {})",
            result.tool_names,
            result.strategy
        );
        assert!(
            !result.failed,
            "fallback selector should improve selection with learned context"
        );
    }

    // ── Quality Tracker integration ──

    #[tokio::test]
    async fn tfidf_selector_with_quality_tracker_records_selection() {
        let registry = mock_registry();
        let tracker = Arc::new(Mutex::new(ToolQualityTracker::new()));
        let selector = TfIdfSelector::new(registry).with_quality_tracker(tracker.clone());

        let ctx = SelectionContext {
            query: "show me the github pull requests",
            turn_count: 2,
            recent_tools: &[],
            budget_tokens: 800,
            boost_terms: vec![],
            budget_pressure: 0.0,
            memory_domain_hints: vec![],
            restricted_tools: vec![],
            file_context: vec![],
        };
        let result = selector.select(&ctx).await;
        assert!(!result.tool_names.is_empty());

        // Quality tracker should have recorded the selection
        let qt = tracker.lock().unwrap();
        let entries = qt.all_entries();
        assert!(
            !entries.is_empty(),
            "tracker should have recorded at least one tool"
        );
        // At least one selected tool should have selections > 0
        let any_selected = entries.values().any(|e| e.selections > 0);
        assert!(
            any_selected,
            "at least one tool should have been recorded as selected"
        );
    }

    #[tokio::test]
    async fn tfidf_selector_without_tracker_still_works() {
        let registry = mock_registry();
        let selector = TfIdfSelector::new(registry); // no tracker

        let ctx = SelectionContext {
            query: "show me the git status",
            turn_count: 2,
            recent_tools: &[],
            budget_tokens: 800,
            boost_terms: vec![],
            budget_pressure: 0.0,
            memory_domain_hints: vec![],
            restricted_tools: vec![],
            file_context: vec![],
        };
        let result = selector.select(&ctx).await;
        assert!(!result.tool_names.is_empty());
        assert!(!result.failed);
    }

    // ── 7.5: Memory-Augmented Entity Hints (boost_terms) ──

    #[tokio::test]
    async fn boost_terms_improve_github_tool_selection() {
        let selector = TfIdfSelector::new(mock_registry());

        // Without boost: "matrixorigin" alone has 0 signals, low TF-IDF
        let ctx_no_boost = SelectionContext {
            query: "matrixorigin",
            turn_count: 1,
            recent_tools: &[],
            budget_tokens: 800,
            boost_terms: vec![],
            budget_pressure: 0.0,
            memory_domain_hints: vec![],
            restricted_tools: vec![],
            file_context: vec![],
        };
        let result_no_boost = selector.select(&ctx_no_boost).await;

        // With boost: memory knows "matrixorigin = GitHub org"
        let ctx_boosted = SelectionContext {
            query: "matrixorigin",
            turn_count: 1,
            recent_tools: &[],
            budget_tokens: 800,
            boost_terms: vec!["github".into(), "org".into(), "repository".into()],
            budget_pressure: 0.0,
            memory_domain_hints: vec![],
            restricted_tools: vec![],
            file_context: vec![],
        };
        let result_boosted = selector.select(&ctx_boosted).await;

        // Boosted version should include more GitHub-related tools
        let github_no_boost = result_no_boost
            .tool_names
            .iter()
            .filter(|n| n.contains("github"))
            .count();
        let github_boosted = result_boosted
            .tool_names
            .iter()
            .filter(|n| n.contains("github"))
            .count();
        assert!(
            github_boosted >= github_no_boost,
            "boost_terms should include at least as many github tools: {} vs {}",
            github_boosted,
            github_no_boost
        );
    }

    #[tokio::test]
    async fn boost_terms_do_not_break_conversational() {
        let selector = TfIdfSelector::new(mock_registry());
        // "你好" is conversational — boost_terms shouldn't override that
        let ctx = SelectionContext {
            query: "你好",
            turn_count: 1,
            recent_tools: &[],
            budget_tokens: 800,
            boost_terms: vec!["github".into()],
            budget_pressure: 0.0,
            memory_domain_hints: vec![],
            restricted_tools: vec![],
            file_context: vec![],
        };
        let result = selector.select(&ctx).await;
        // Should still work (conversational may or may not include github depending
        // on whether boost changes signals — but should not crash)
        assert!(!result.failed);
    }

    #[tokio::test]
    async fn boost_terms_empty_identical_to_no_boost() {
        let selector = TfIdfSelector::new(mock_registry());
        let ctx_none = SelectionContext {
            query: "check the status",
            turn_count: 1,
            recent_tools: &[],
            budget_tokens: 800,
            boost_terms: vec![],
            budget_pressure: 0.0,
            memory_domain_hints: vec![],
            restricted_tools: vec![],
            file_context: vec![],
        };
        let ctx_empty = SelectionContext {
            query: "check the status",
            turn_count: 1,
            recent_tools: &[],
            budget_tokens: 800,
            boost_terms: vec![],
            budget_pressure: 0.0,
            memory_domain_hints: vec![],
            restricted_tools: vec![],
            file_context: vec![],
        };
        let r1 = selector.select(&ctx_none).await;
        let r2 = selector.select(&ctx_empty).await;
        assert_eq!(
            r1.tool_names, r2.tool_names,
            "empty boost_terms should be identical"
        );
    }

    #[tokio::test]
    async fn boost_terms_irrelevant_does_not_degrade() {
        let selector = TfIdfSelector::new(mock_registry());
        // "show git status" should always select git_status
        let ctx_no_boost = SelectionContext {
            query: "show git status",
            turn_count: 1,
            recent_tools: &[],
            budget_tokens: 800,
            boost_terms: vec![],
            budget_pressure: 0.0,
            memory_domain_hints: vec![],
            restricted_tools: vec![],
            file_context: vec![],
        };
        let ctx_irrelevant = SelectionContext {
            query: "show git status",
            turn_count: 1,
            recent_tools: &[],
            budget_tokens: 800,
            boost_terms: vec!["quantum".into(), "physics".into(), "entanglement".into()],
            budget_pressure: 0.0,
            memory_domain_hints: vec![],
            restricted_tools: vec![],
            file_context: vec![],
        };
        let r1 = selector.select(&ctx_no_boost).await;
        let r2 = selector.select(&ctx_irrelevant).await;
        // git tools should still be present with irrelevant boost
        let git_count_original = r1.tool_names.iter().filter(|n| n.contains("git")).count();
        let git_count_boosted = r2.tool_names.iter().filter(|n| n.contains("git")).count();
        assert!(
            git_count_boosted >= git_count_original,
            "irrelevant boost should not reduce git tools: {} vs {}",
            git_count_boosted,
            git_count_original
        );
    }

    #[tokio::test]
    async fn boost_terms_git_selects_git_tools() {
        let selector = TfIdfSelector::new(mock_registry());
        // Vague query + git boost terms → should select git tools
        let ctx = SelectionContext {
            query: "what happened recently",
            turn_count: 1,
            recent_tools: &[],
            budget_tokens: 800,
            boost_terms: vec!["git".into(), "commit".into(), "branch".into()],
            budget_pressure: 0.0,
            memory_domain_hints: vec![],
            restricted_tools: vec![],
            file_context: vec![],
        };
        let result = selector.select(&ctx).await;
        let has_git = result.tool_names.iter().any(|n| n.contains("git"));
        assert!(
            has_git,
            "git boost terms should include git tools, got: {:?}",
            result.tool_names
        );
    }

    #[tokio::test]
    async fn boost_terms_memory_selects_memory_tools() {
        let selector = TfIdfSelector::new(mock_registry());
        let ctx = SelectionContext {
            query: "what do I care about",
            turn_count: 1,
            recent_tools: &[],
            budget_tokens: 800,
            boost_terms: vec!["memory".into(), "search".into(), "retrieve".into()],
            budget_pressure: 0.0,
            memory_domain_hints: vec![],
            restricted_tools: vec![],
            file_context: vec![],
        };
        let result = selector.select(&ctx).await;
        let has_memory = result.tool_names.iter().any(|n| n.contains("memory"));
        assert!(
            has_memory,
            "memory boost terms should include memory tools, got: {:?}",
            result.tool_names
        );
    }

    #[tokio::test]
    async fn boost_terms_large_count_does_not_crash() {
        let selector = TfIdfSelector::new(mock_registry());
        let many_terms: Vec<String> = (0..100).map(|i| format!("term_{}", i)).collect();
        let ctx = SelectionContext {
            query: "test",
            turn_count: 1,
            recent_tools: &[],
            budget_tokens: 800,
            boost_terms: many_terms,
            budget_pressure: 0.0,
            memory_domain_hints: vec![],
            restricted_tools: vec![],
            file_context: vec![],
        };
        let result = selector.select(&ctx).await;
        assert!(!result.failed, "100 boost terms should not crash");
    }

    #[tokio::test]
    async fn boost_terms_with_strong_signal_no_override() {
        let selector = TfIdfSelector::new(mock_registry());
        // Strong GitHub signal + memory boost → GitHub tools should still be present
        let ctx = SelectionContext {
            query: "list all GitHub pull requests",
            turn_count: 1,
            recent_tools: &[],
            budget_tokens: 800,
            boost_terms: vec!["memory".into(), "search".into()],
            budget_pressure: 0.0,
            memory_domain_hints: vec![],
            restricted_tools: vec![],
            file_context: vec![],
        };
        let result = selector.select(&ctx).await;
        let has_github = result.tool_names.iter().any(|n| n.contains("github"));
        assert!(
            has_github,
            "strong GitHub signal should survive memory boost: {:?}",
            result.tool_names
        );
    }

    #[tokio::test]
    async fn boost_terms_cjk_terms_work() {
        let selector = TfIdfSelector::new(mock_registry());
        let ctx = SelectionContext {
            query: "matrixorigin",
            turn_count: 1,
            recent_tools: &[],
            budget_tokens: 800,
            boost_terms: vec!["代码".into(), "仓库".into(), "提交".into()],
            budget_pressure: 0.0,
            memory_domain_hints: vec![],
            restricted_tools: vec![],
            file_context: vec![],
        };
        let result = selector.select(&ctx).await;
        assert!(!result.failed, "CJK boost terms should not cause errors");
    }

    #[tokio::test]
    async fn boost_terms_confidence_improvement() {
        let selector = TfIdfSelector::new(mock_registry());
        // "matrixorigin" alone → low confidence (0 signals)
        let ctx_no_boost = SelectionContext {
            query: "matrixorigin",
            turn_count: 1,
            recent_tools: &[],
            budget_tokens: 800,
            boost_terms: vec![],
            budget_pressure: 0.0,
            memory_domain_hints: vec![],
            restricted_tools: vec![],
            file_context: vec![],
        };
        let r1 = selector.select(&ctx_no_boost).await;
        // With boost → might trigger github signal → higher confidence
        let ctx_boosted = SelectionContext {
            query: "matrixorigin",
            turn_count: 1,
            recent_tools: &[],
            budget_tokens: 800,
            boost_terms: vec![
                "github".into(),
                "repository".into(),
                "pull".into(),
                "request".into(),
            ],
            budget_pressure: 0.0,
            memory_domain_hints: vec![],
            restricted_tools: vec![],
            file_context: vec![],
        };
        let r2 = selector.select(&ctx_boosted).await;
        assert!(
            r2.confidence >= r1.confidence,
            "boosted confidence ({}) should be >= unboosted ({})",
            r2.confidence,
            r1.confidence
        );
    }

    // ── Pipeline Wiring Integration Tests ──

    #[tokio::test]
    async fn wiring_entity_graph_boosts_known_entity() {
        let graph = EntityGraph::new();
        let graph = Arc::new(Mutex::new(graph));

        // Teach the entity graph that "matrixorigin" is GitHub domain
        {
            let mut g = graph.lock().unwrap();
            g.learn(
                "matrixorigin",
                DomainHint::GitHub,
                &["github_list_prs".into(), "github_search_repos".into()],
                None,
            );
            g.learn(
                "matrixorigin",
                DomainHint::GitHub,
                &["github_list_prs".into()],
                None,
            );
        }

        let selector = TfIdfSelector::new(mock_registry()).with_entity_graph(graph.clone());

        // Without entity graph knowledge, "matrixorigin" triggers 0 signals
        let baseline = TfIdfSelector::new(mock_registry());
        let ctx = SelectionContext {
            query: "matrixorigin的PR情况",
            turn_count: 1,
            recent_tools: &[],
            budget_tokens: 800,
            boost_terms: vec![],
            budget_pressure: 0.0,
            memory_domain_hints: vec![],
            restricted_tools: vec![],
            file_context: vec![],
        };

        let r_baseline = baseline.select(&ctx).await;
        let r_enriched = selector.select(&ctx).await;

        // Enriched selector should have higher confidence because entity graph
        // adds "github", "repository" etc. as boost terms
        assert!(
            r_enriched.confidence >= r_baseline.confidence,
            "entity-enriched confidence ({}) should be >= baseline ({})",
            r_enriched.confidence,
            r_baseline.confidence
        );
    }

    #[tokio::test]
    async fn wiring_pattern_library_adds_boost_terms() {
        let lib = PatternLibrary::new();
        let lib = Arc::new(Mutex::new(lib));

        // Record successful tool chain patterns for CodeReview tasks
        {
            let mut l = lib.lock().unwrap();
            for _ in 0..3 {
                l.record_outcome(
                    &["github_list_prs".into(), "github_get_pr".into()],
                    TaskType::Fetch,
                    Some(DomainHint::GitHub),
                    true,
                    0.9,
                    None,
                );
            }
        }

        let selector = TfIdfSelector::new(mock_registry()).with_pattern_library(lib.clone());

        let ctx = SelectionContext {
            query: "review the latest PR",
            turn_count: 1,
            recent_tools: &[],
            budget_tokens: 800,
            boost_terms: vec![],
            budget_pressure: 0.0,
            memory_domain_hints: vec![],
            restricted_tools: vec![],
            file_context: vec![],
        };

        let result = selector.select(&ctx).await;

        // Pattern library should inject tool names as boost terms,
        // improving selection for the detected task type
        assert!(
            !result.tool_names.is_empty(),
            "pattern-enriched selection should return tools"
        );
        assert_eq!(result.strategy, "tfidf_routed");
    }

    #[tokio::test]
    async fn wiring_record_turn_outcome_updates_all_modules() {
        let graph = Arc::new(Mutex::new(EntityGraph::new()));
        let lib = Arc::new(Mutex::new(PatternLibrary::new()));
        let cal = Arc::new(Mutex::new(ProgressiveCalibrator::new(0.15)));

        let selector = TfIdfSelector::new(mock_registry())
            .with_entity_graph(graph.clone())
            .with_pattern_library(lib.clone())
            .with_progressive_calibrator(cal.clone());

        // Record a successful turn outcome (twice — PatternLibrary needs ≥2 observations)
        selector.record_turn_outcome(
            "check matrixorigin PRs",
            &["github_list_prs".into(), "github_search_repos".into()],
            TaskType::Fetch,
            Some(DomainHint::GitHub),
            true,  // success
            0.85,  // quality
            false, // not corrected
            None,
        );
        selector.record_turn_outcome(
            "check matrixorigin issues",
            &["github_list_prs".into(), "github_search_repos".into()],
            TaskType::Fetch,
            Some(DomainHint::GitHub),
            true,
            0.9,
            false,
            None,
        );

        // Verify EntityGraph learned the association
        {
            let g = graph.lock().unwrap();
            let boost = g.boost_for("matrixorigin");
            assert!(
                !boost.is_empty(),
                "entity graph should have learned 'matrixorigin' → GitHub"
            );
        }

        // Verify PatternLibrary recorded the outcome
        {
            let l = lib.lock().unwrap();
            let suggestions = l.suggest(TaskType::Fetch, Some(DomainHint::GitHub), 5);
            assert!(
                !suggestions.is_empty(),
                "pattern library should have recorded the outcome"
            );
        }

        // Record correction to verify calibrator
        selector.record_turn_outcome(
            "check matrixorigin issues",
            &["github_list_issues".into()],
            TaskType::Fetch,
            Some(DomainHint::GitHub),
            true,
            0.7,
            true, // was corrected
            None,
        );
    }

    #[tokio::test]
    async fn wiring_backward_compat_no_pipeline_modules() {
        // No pipeline modules → should behave identically to old path
        let selector = TfIdfSelector::new(mock_registry());
        let ctx = SelectionContext {
            query: "show me recent pull requests",
            turn_count: 1,
            recent_tools: &[],
            budget_tokens: 800,
            boost_terms: vec![],
            budget_pressure: 0.0,
            memory_domain_hints: vec![],
            restricted_tools: vec![],
            file_context: vec![],
        };

        let result = selector.select(&ctx).await;
        // Should still work via RoutingEngine (creates ConversationState internally)
        assert!(!result.tool_names.is_empty());
        assert_eq!(result.strategy, "tfidf_routed");
        assert!(!result.failed);
    }

    #[tokio::test]
    async fn wiring_entity_graph_learning_loop_improves_over_time() {
        let graph = Arc::new(Mutex::new(EntityGraph::new()));
        let lib = Arc::new(Mutex::new(PatternLibrary::new()));

        let selector = TfIdfSelector::new(mock_registry())
            .with_entity_graph(graph.clone())
            .with_pattern_library(lib.clone());

        let ctx = SelectionContext {
            query: "matrixorigin的issue有哪些",
            turn_count: 1,
            recent_tools: &[],
            budget_tokens: 800,
            boost_terms: vec![],
            budget_pressure: 0.0,
            memory_domain_hints: vec![],
            restricted_tools: vec![],
            file_context: vec![],
        };

        // First selection: no learned knowledge
        let r1 = selector.select(&ctx).await;

        // Simulate learning from 3 successful turns
        for _ in 0..3 {
            selector.record_turn_outcome(
                "matrixorigin issues",
                &["github_list_issues".into(), "github_search_repos".into()],
                TaskType::Fetch,
                Some(DomainHint::GitHub),
                true,
                0.9,
                false,
                None,
            );
        }

        // Second selection: should benefit from learned entity associations
        let r2 = selector.select(&ctx).await;

        assert!(
            r2.confidence >= r1.confidence,
            "after learning, confidence ({}) should be >= initial ({})",
            r2.confidence,
            r1.confidence
        );
    }

    #[tokio::test]
    async fn wiring_all_modules_composed() {
        let graph = Arc::new(Mutex::new(EntityGraph::new()));
        let lib = Arc::new(Mutex::new(PatternLibrary::new()));
        let cal = Arc::new(Mutex::new(ProgressiveCalibrator::new(0.15)));

        // Pre-populate entity graph
        {
            let mut g = graph.lock().unwrap();
            g.learn(
                "rust",
                DomainHint::Code,
                &["file_read".into(), "bash".into()],
                None,
            );
            g.learn("rust", DomainHint::Code, &["file_read".into()], None);
        }

        // Pre-populate pattern library
        {
            let mut l = lib.lock().unwrap();
            for _ in 0..3 {
                l.record_outcome(
                    &["file_read".into(), "bash".into()],
                    TaskType::Code,
                    Some(DomainHint::Code),
                    true,
                    0.85,
                    None,
                );
            }
        }

        let selector = TfIdfSelector::new(mock_registry())
            .with_entity_graph(graph)
            .with_pattern_library(lib)
            .with_progressive_calibrator(cal);

        let ctx = SelectionContext {
            query: "help me with rust code review",
            turn_count: 3,
            recent_tools: &["file_read".into()],
            budget_tokens: 800,
            boost_terms: vec![],
            budget_pressure: 0.0,
            memory_domain_hints: vec![],
            restricted_tools: vec![],
            file_context: vec![],
        };

        let result = selector.select(&ctx).await;

        // All modules composed: entity boost + routing + pattern boost
        assert!(!result.tool_names.is_empty());
        assert_eq!(result.strategy, "tfidf_routed");
        assert!(
            result.confidence > 0.0,
            "composed selection should have non-zero confidence"
        );
    }

    #[tokio::test]
    async fn wiring_failed_outcome_not_learned_to_entity_graph() {
        let graph = Arc::new(Mutex::new(EntityGraph::new()));

        let selector = TfIdfSelector::new(mock_registry()).with_entity_graph(graph.clone());

        // Record a FAILED turn outcome
        selector.record_turn_outcome(
            "check kubernetes status",
            &["github_list_prs".into()],
            TaskType::Fetch,
            Some(DomainHint::GitHub),
            false, // FAILED
            0.2,
            false,
            None,
        );

        // Entity graph should NOT learn from failures
        {
            let g = graph.lock().unwrap();
            let boost = g.boost_for("kubernetes");
            assert!(
                boost.is_empty(),
                "failed outcomes should not be learned by entity graph, got: {:?}",
                boost
            );
        }
    }

    #[tokio::test]
    async fn wiring_conversational_query_returns_pinned_only() {
        let selector = TfIdfSelector::new(mock_registry())
            .with_entity_graph(Arc::new(Mutex::new(EntityGraph::new())))
            .with_pattern_library(Arc::new(Mutex::new(PatternLibrary::new())));

        let ctx = SelectionContext {
            query: "谢谢你的帮助",
            turn_count: 1,
            recent_tools: &[],
            budget_tokens: 800,
            boost_terms: vec![],
            budget_pressure: 0.0,
            memory_domain_hints: vec![],
            restricted_tools: vec![],
            file_context: vec![],
        };

        let result = selector.select(&ctx).await;
        // Conversational queries should return minimal (pinned) tools
        // Budget used should be 0 for pinned-only
        assert_eq!(result.budget_used, 0);
    }

    // ── Trait-level record_outcome tests ──────────────────────────────────

    #[test]
    fn trait_record_outcome_on_tfidf_updates_entity_graph() {
        let graph = Arc::new(Mutex::new(EntityGraph::new()));
        let selector = TfIdfSelector::new(mock_registry()).with_entity_graph(graph.clone());

        // Before: no boost terms for "matrixorigin"
        let boost_before = graph.lock().unwrap().boost_for("matrixorigin");
        assert!(boost_before.is_empty());

        // Use trait method (not concrete record_turn_outcome)
        let sel: &dyn ToolSelector = &selector;
        sel.record_outcome(
            "matrixorigin PR review",
            &["github_search".to_string()],
            TaskType::Code,
            Some(DomainHint::GitHub),
            true,
            0.8,
            false,
            None,
        );

        // After: entity graph learned the association
        let boost_after = graph.lock().unwrap().boost_for("matrixorigin");
        assert!(
            !boost_after.is_empty(),
            "entity graph should learn from trait-level record_outcome"
        );
    }

    #[tokio::test]
    async fn trait_record_outcome_on_fallback_forwards_to_tfidf() {
        let graph = Arc::new(Mutex::new(EntityGraph::new()));
        let tfidf = TfIdfSelector::new(mock_registry()).with_entity_graph(graph.clone());

        let fallback_selector = FallbackSelector::new(
            Box::new(TfIdfSelector::new(mock_registry())), // primary (no modules)
            Box::new(tfidf),                               // fallback (has modules)
        );

        // Use trait method on FallbackSelector
        let sel: &dyn ToolSelector = &fallback_selector;
        sel.record_outcome(
            "matrixorigin deployment",
            &["bash".to_string()],
            TaskType::Code,
            Some(DomainHint::GitHub),
            true,
            0.7,
            false,
            None,
        );

        // Verify it forwarded to fallback's TfIdfSelector
        let boost = graph.lock().unwrap().boost_for("matrixorigin");
        assert!(
            !boost.is_empty(),
            "FallbackSelector should forward record_outcome to fallback"
        );
    }

    #[test]
    fn trait_record_outcome_noop_by_default() {
        // A selector without pipeline modules should silently do nothing
        let selector = TfIdfSelector::new(mock_registry());
        let sel: &dyn ToolSelector = &selector;
        // Should not panic
        sel.record_outcome(
            "test",
            &["bash".into()],
            TaskType::Code,
            None,
            true,
            0.5,
            false,
            None,
        );
    }

    // ── Budget pressure tests ────────────────────────────────────────────

    #[tokio::test]
    async fn budget_pressure_reduces_tool_count() {
        let selector = TfIdfSelector::new(mock_registry());
        // Normal pressure: full budget
        let ctx_normal = SelectionContext {
            query: "show me the PR status and git log for matrixorigin",
            turn_count: 3,
            recent_tools: &[],
            budget_tokens: 800,
            boost_terms: vec![],
            budget_pressure: 0.0,
            memory_domain_hints: vec![],
            restricted_tools: vec![],
            file_context: vec![],
        };
        let result_normal = selector.select(&ctx_normal).await;

        // High pressure: reduced budget → fewer or equal tools
        let ctx_pressure = SelectionContext {
            query: "show me the PR status and git log for matrixorigin",
            turn_count: 3,
            recent_tools: &[],
            budget_tokens: 800,
            boost_terms: vec![],
            budget_pressure: 0.9,
            memory_domain_hints: vec![],
            restricted_tools: vec![],
            file_context: vec![],
        };
        let result_pressure = selector.select(&ctx_pressure).await;

        assert!(
            result_pressure.tool_names.len() <= result_normal.tool_names.len(),
            "High budget pressure should select ≤ tools vs normal: pressure={}, normal={}",
            result_pressure.tool_names.len(),
            result_normal.tool_names.len()
        );
    }

    #[tokio::test]
    async fn budget_pressure_zero_equals_no_pressure() {
        let selector = TfIdfSelector::new(mock_registry());
        let ctx = SelectionContext {
            query: "list pull requests",
            turn_count: 1,
            recent_tools: &[],
            budget_tokens: 800,
            boost_terms: vec![],
            budget_pressure: 0.0,
            memory_domain_hints: vec![],
            restricted_tools: vec![],
            file_context: vec![],
        };
        let result = selector.select(&ctx).await;
        // budget_pressure=0.0 should not change behavior at all
        assert!(!result.failed);
        assert!(!result.tool_names.is_empty());
    }

    #[tokio::test]
    async fn budget_pressure_clamps_to_valid_range() {
        let selector = TfIdfSelector::new(mock_registry());
        // Negative pressure should be clamped to 0
        let ctx_neg = SelectionContext {
            query: "list pull requests",
            turn_count: 1,
            recent_tools: &[],
            budget_tokens: 800,
            boost_terms: vec![],
            budget_pressure: -0.5,
            memory_domain_hints: vec![],
            restricted_tools: vec![],
            file_context: vec![],
        };
        let result_neg = selector.select(&ctx_neg).await;

        // Overshoot pressure clamped to 1.0
        let ctx_over = SelectionContext {
            query: "list pull requests",
            turn_count: 1,
            recent_tools: &[],
            budget_tokens: 800,
            boost_terms: vec![],
            budget_pressure: 2.0,
            memory_domain_hints: vec![],
            restricted_tools: vec![],
            file_context: vec![],
        };
        let result_over = selector.select(&ctx_over).await;

        // Neither should panic or fail
        assert!(!result_neg.failed);
        assert!(!result_over.failed);
    }

    // ── Memory domain hint tests ─────────────────────────────────────────

    #[tokio::test]
    async fn memory_domain_hint_boosts_github_tools() {
        let selector = TfIdfSelector::new(mock_registry());
        // Query with entity name that has NO keyword overlap with GitHub tools
        let ctx_no_hint = SelectionContext {
            query: "matrixorigin最新的状态",
            turn_count: 3,
            recent_tools: &[],
            budget_tokens: 800,
            boost_terms: vec![],
            budget_pressure: 0.0,
            memory_domain_hints: vec![],
            restricted_tools: vec![],
            file_context: vec![],
        };
        let result_no_hint = selector.select(&ctx_no_hint).await;

        // Same query but with GitHub domain hint from memory
        let ctx_hint = SelectionContext {
            query: "matrixorigin最新的状态",
            turn_count: 3,
            recent_tools: &[],
            budget_tokens: 800,
            boost_terms: vec![],
            budget_pressure: 0.0,
            memory_domain_hints: vec![DomainHint::GitHub],
            restricted_tools: vec![],
            file_context: vec![],
        };
        let result_hint = selector.select(&ctx_hint).await;

        // With domain hint, confidence should be >= no-hint case
        // (the hint adds score to GitHub tools, improving overall confidence)
        let gh_tools = ["github_list_prs", "github_get_pr", "github_list_issues"];
        let hint_gh_count = result_hint
            .tool_names
            .iter()
            .filter(|t| gh_tools.contains(&t.as_str()))
            .count();
        let no_hint_gh_count = result_no_hint
            .tool_names
            .iter()
            .filter(|t| gh_tools.contains(&t.as_str()))
            .count();

        assert!(
            hint_gh_count >= no_hint_gh_count,
            "GitHub domain hint should select ≥ GitHub tools: hint={}, no_hint={}",
            hint_gh_count,
            no_hint_gh_count
        );
    }

    #[tokio::test]
    async fn memory_domain_hint_git_boosts_git_tools() {
        let selector = TfIdfSelector::new(mock_registry());
        let ctx = SelectionContext {
            query: "这个项目的历史是什么",
            turn_count: 2,
            recent_tools: &[],
            budget_tokens: 800,
            boost_terms: vec![],
            budget_pressure: 0.0,
            memory_domain_hints: vec![DomainHint::Git],
            restricted_tools: vec![],
            file_context: vec![],
        };
        let result = selector.select(&ctx).await;
        // With Git hint, should include git tools despite vague query
        let has_git = result.tool_names.iter().any(|t| t.starts_with("git_"));
        assert!(
            has_git || result.tool_names.iter().any(|t| t.contains("git")),
            "Git domain hint should promote git tools, got: {:?}",
            result.tool_names
        );
    }

    #[tokio::test]
    async fn empty_domain_hints_same_as_no_hints() {
        let selector = TfIdfSelector::new(mock_registry());
        let ctx_none = SelectionContext {
            query: "list pull requests",
            turn_count: 1,
            recent_tools: &[],
            budget_tokens: 800,
            boost_terms: vec![],
            budget_pressure: 0.0,
            memory_domain_hints: vec![],
            restricted_tools: vec![],
            file_context: vec![],
        };
        let result_none = selector.select(&ctx_none).await;

        // Empty vec should produce identical results to no hints
        assert!(!result_none.failed);
        assert_eq!(result_none.strategy, "tfidf_routed");
    }

    #[tokio::test]
    async fn budget_pressure_and_domain_hints_combined() {
        let selector = TfIdfSelector::new(mock_registry());
        // Combine high pressure with domain hint
        let ctx = SelectionContext {
            query: "matrixorigin pr status",
            turn_count: 3,
            recent_tools: &[],
            budget_tokens: 800,
            boost_terms: vec![],
            budget_pressure: 0.6,
            memory_domain_hints: vec![DomainHint::GitHub],
            restricted_tools: vec![],
            file_context: vec![],
        };
        let result = selector.select(&ctx).await;
        // Should still select some tools (domain hint helps ranking despite pressure)
        assert!(!result.failed, "Combined pressure+hints should not fail");
        // The tools selected should be the most relevant (GitHub) ones
        let has_github = result.tool_names.iter().any(|t| t.starts_with("github_"));
        assert!(
            has_github,
            "Even under pressure, domain hint should keep highest-ranked GitHub tools: {:?}",
            result.tool_names
        );
    }

    #[tokio::test]
    async fn restricted_tools_excluded_from_selection() {
        let selector = TfIdfSelector::new(mock_registry());

        // Without restriction: github tools should be selected for this query
        let ctx_open = SelectionContext {
            query: "list open pull requests on github",
            turn_count: 1,
            recent_tools: &[],
            budget_tokens: 1200,
            boost_terms: vec![],
            budget_pressure: 0.0,
            memory_domain_hints: vec![],
            restricted_tools: vec![],
            file_context: vec![],
        };
        let result_open = selector.select(&ctx_open).await;
        assert!(
            result_open
                .tool_names
                .contains(&"github_list_prs".to_string()),
            "Without restriction, github_list_prs should be selected: {:?}",
            result_open.tool_names
        );

        // With restriction: github_list_prs should be filtered out
        let ctx_restricted = SelectionContext {
            query: "list open pull requests on github",
            turn_count: 1,
            recent_tools: &[],
            budget_tokens: 1200,
            boost_terms: vec![],
            budget_pressure: 0.0,
            memory_domain_hints: vec![],
            restricted_tools: vec!["github_list_prs".to_string()],
            file_context: vec![],
        };
        let result_restricted = selector.select(&ctx_restricted).await;
        assert!(
            !result_restricted
                .tool_names
                .contains(&"github_list_prs".to_string()),
            "Restricted tool should be excluded from selection: {:?}",
            result_restricted.tool_names
        );
    }

    #[tokio::test]
    async fn restricted_tools_dont_affect_unrelated() {
        let selector = TfIdfSelector::new(mock_registry());

        // Restrict a tool not relevant to the query
        let ctx = SelectionContext {
            query: "list open pull requests on github",
            turn_count: 1,
            recent_tools: &[],
            budget_tokens: 1200,
            boost_terms: vec![],
            budget_pressure: 0.0,
            memory_domain_hints: vec![],
            restricted_tools: vec!["mo_query".to_string()],
            file_context: vec![],
        };
        let result = selector.select(&ctx).await;
        // github_list_prs should still be selected (mo_query is irrelevant here)
        assert!(
            result.tool_names.contains(&"github_list_prs".to_string()),
            "Restricting unrelated tool should not affect relevant tools: {:?}",
            result.tool_names
        );
    }

    // ── Pressure-aware schema resolution ──

    #[test]
    fn resolve_schemas_no_pressure_includes_all_pinned() {
        let registry = mock_registry();
        let (schemas, _) = resolve_schemas(&registry, &[]);
        let names: Vec<&str> = schemas
            .iter()
            .filter_map(|s| {
                s.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
            })
            .collect();
        assert!(
            names.contains(&"memory_store"),
            "memory_store should be pinned: {:?}",
            names
        );
        assert!(
            names.contains(&"memory_search"),
            "memory_search should be pinned: {:?}",
            names
        );
        assert!(
            names.contains(&"bash"),
            "bash should be pinned: {:?}",
            names
        );
    }

    #[test]
    fn resolve_schemas_high_pressure_skips_deferrable_pinned() {
        let registry = mock_registry();
        let (schemas, _) = resolve_schemas_with_pressure(&registry, &[], 0.9);
        let names: Vec<&str> = schemas
            .iter()
            .filter_map(|s| {
                s.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
            })
            .collect();
        assert!(
            !names.contains(&"memory_store"),
            "memory_store should be deferred under high pressure: {:?}",
            names
        );
        assert!(
            !names.contains(&"memory_search"),
            "memory_search should be deferred under high pressure: {:?}",
            names
        );
        // Core pinned tools remain
        assert!(
            names.contains(&"bash"),
            "bash must always be included: {:?}",
            names
        );
        assert!(
            names.contains(&"read_file"),
            "read_file must always be included: {:?}",
            names
        );
    }

    #[test]
    fn resolve_schemas_high_pressure_keeps_memory_if_explicitly_selected() {
        let registry = mock_registry();
        let selected = vec!["memory_search".to_string()];
        let (schemas, _) = resolve_schemas_with_pressure(&registry, &selected, 0.9);
        let names: Vec<&str> = schemas
            .iter()
            .filter_map(|s| {
                s.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
            })
            .collect();
        // memory_search explicitly selected → should be kept even under pressure
        assert!(
            names.contains(&"memory_search"),
            "explicitly selected memory tool should be kept: {:?}",
            names
        );
    }

    #[test]
    fn resolve_schemas_moderate_pressure_keeps_all_pinned() {
        let registry = mock_registry();
        let (schemas, _) = resolve_schemas_with_pressure(&registry, &[], 0.7);
        let names: Vec<&str> = schemas
            .iter()
            .filter_map(|s| {
                s.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
            })
            .collect();
        // 0.7 < 0.8 threshold → all pinned tools kept
        assert!(
            names.contains(&"memory_store"),
            "moderate pressure should keep all pinned: {:?}",
            names
        );
        assert!(names.contains(&"memory_search"));
    }

    // ── Schema caching infrastructure ──

    #[test]
    fn registry_schema_index_enables_o1_lookup() {
        let registry = mock_registry();
        // Verify O(1) lookup works for all catalog tools
        for tool in TOOL_CATALOG.iter() {
            let found = registry.schema_by_name(tool.name);
            assert!(found.is_some(), "schema_by_name should find {}", tool.name);
            let schema_name = found
                .unwrap()
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .unwrap();
            assert_eq!(schema_name, tool.name);
        }
    }

    #[test]
    fn registry_pinned_schemas_cached_at_construction() {
        let registry = mock_registry();
        let pinned = registry.pinned_schemas();
        let pinned_count = TOOL_CATALOG.iter().filter(|t| t.pinned).count();
        assert_eq!(
            pinned.len(),
            pinned_count,
            "cached pinned schemas should match catalog pinned count"
        );
        // Verify all pinned names are correct
        for (name, schema) in pinned {
            let meta = TOOL_CATALOG.iter().find(|t| t.name == name).unwrap();
            assert!(meta.pinned, "{} should be pinned", name);
            let schema_name = schema
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .unwrap();
            assert_eq!(schema_name, name.as_str());
        }
    }

    #[test]
    fn schema_by_name_returns_none_for_unknown() {
        let registry = mock_registry();
        assert!(registry.schema_by_name("nonexistent_tool").is_none());
    }

    // ── Schema Pruning ──────────────────────────────────────────

    fn make_test_schema() -> Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "test_tool",
                "description": "A very long description that explains what this tool does in great detail and should be truncated at the light level to save tokens efficiently.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "File path relative to project root"},
                        "start_line": {"type": "integer", "description": "First line to read (1-based, optional)"},
                        "end_line": {"type": "integer", "description": "Last line to read (inclusive, optional)"}
                    },
                    "required": ["path"]
                }
            }
        })
    }

    #[test]
    fn prune_none_preserves_schema() {
        let schema = make_test_schema();
        let pruned = prune_schema(schema.clone(), PruneLevel::None);
        assert_eq!(schema, pruned);
    }

    #[test]
    fn prune_light_truncates_description() {
        let schema = make_test_schema();
        let pruned = prune_schema(schema, PruneLevel::Light);

        let desc = pruned["function"]["description"].as_str().unwrap();
        assert!(
            desc.len() <= 85,
            "description should be truncated, got {}",
            desc.len()
        );
        assert!(desc.ends_with('…'));

        // Param descriptions removed
        assert!(
            pruned["function"]["parameters"]["properties"]["path"]
                .get("description")
                .is_none()
        );
    }

    #[test]
    fn prune_medium_removes_description() {
        let schema = make_test_schema();
        let pruned = prune_schema(schema, PruneLevel::Medium);

        assert!(
            pruned["function"].get("description").is_none(),
            "description should be removed at medium level"
        );

        // All params still present (optional + required)
        let props = pruned["function"]["parameters"]["properties"]
            .as_object()
            .unwrap();
        assert_eq!(props.len(), 3, "all params should remain at medium level");

        // Param descriptions removed
        assert!(props["path"].get("description").is_none());
    }

    #[test]
    fn prune_aggressive_strips_optional_params() {
        let schema = make_test_schema();
        let pruned = prune_schema(schema, PruneLevel::Aggressive);

        assert!(
            pruned["function"].get("description").is_none(),
            "description should be removed"
        );

        // Only required params remain
        let props = pruned["function"]["parameters"]["properties"]
            .as_object()
            .unwrap();
        assert_eq!(props.len(), 1, "only required param should remain");
        assert!(props.contains_key("path"));
        assert!(!props.contains_key("start_line"));
        assert!(!props.contains_key("end_line"));
    }

    #[test]
    fn prune_light_preserves_short_description() {
        let schema = serde_json::json!({
            "type": "function",
            "function": {
                "name": "short_tool",
                "description": "Short description.",
                "parameters": {"type": "object", "properties": {}}
            }
        });
        let pruned = prune_schema(schema, PruneLevel::Light);
        assert_eq!(
            pruned["function"]["description"].as_str().unwrap(),
            "Short description."
        );
    }

    #[test]
    fn prune_schema_missing_function_key() {
        let schema = serde_json::json!({"type": "function"});
        let pruned = prune_schema(schema.clone(), PruneLevel::Aggressive);
        assert_eq!(schema, pruned, "should be no-op without function key");
    }

    #[test]
    fn prune_with_pressure_activates_at_thresholds() {
        let registry = mock_registry();
        let names = vec!["git_diff".to_string()];

        // No pressure: schemas have descriptions
        let (schemas_0, _) = resolve_schemas_with_pressure(&registry, &names, 0.0);
        let has_desc = schemas_0
            .iter()
            .all(|s| s["function"].get("description").is_some());
        assert!(has_desc, "zero pressure should keep descriptions");

        // Medium pressure: descriptions removed
        let (schemas_06, _) = resolve_schemas_with_pressure(&registry, &names, 0.6);
        let no_desc = schemas_06
            .iter()
            .all(|s| s["function"].get("description").is_none());
        assert!(no_desc, "medium pressure should remove descriptions");
    }

    #[test]
    fn prune_token_savings_measured() {
        let schema = make_test_schema();
        let full_size = serde_json::to_string(&schema).unwrap().len();

        let light = prune_schema(schema.clone(), PruneLevel::Light);
        let light_size = serde_json::to_string(&light).unwrap().len();

        let medium = prune_schema(schema.clone(), PruneLevel::Medium);
        let medium_size = serde_json::to_string(&medium).unwrap().len();

        let aggressive = prune_schema(schema, PruneLevel::Aggressive);
        let aggressive_size = serde_json::to_string(&aggressive).unwrap().len();

        // Each level should be strictly smaller than the previous
        assert!(
            light_size < full_size,
            "light ({}) should be smaller than full ({})",
            light_size,
            full_size
        );
        assert!(
            medium_size < light_size,
            "medium ({}) should be smaller than light ({})",
            medium_size,
            light_size
        );
        assert!(
            aggressive_size < medium_size,
            "aggressive ({}) should be smaller than medium ({})",
            aggressive_size,
            medium_size
        );

        // Aggressive should save at least 50%
        let savings = (full_size - aggressive_size) as f64 / full_size as f64;
        assert!(
            savings >= 0.50,
            "aggressive pruning should save >=50%, got {:.0}%",
            savings * 100.0
        );
    }

    #[test]
    fn truncate_at_boundary_works() {
        assert_eq!(truncate_at_boundary("hello world", 20), "hello world");
        assert_eq!(
            truncate_at_boundary("hello world foo bar baz", 12),
            "hello world…"
        );
        assert_eq!(truncate_at_boundary("abcdefghij", 5), "abcde…");
    }

    #[test]
    fn truncate_at_boundary_utf8_safe() {
        // Emoji: 🔴 is 4 bytes — slicing at byte 2 would panic without the fix
        let emoji = "🔴 This is a test";
        let result = truncate_at_boundary(emoji, 2);
        assert!(!result.is_empty(), "should not panic on emoji boundary");

        // CJK: 这 is 3 bytes — slicing at byte 5 would land inside '是'
        let cjk = "这是测试描述";
        let result = truncate_at_boundary(cjk, 5);
        assert!(!result.is_empty(), "should not panic on CJK boundary");
        assert!(result.ends_with('…'));

        // Mixed: ASCII + CJK
        let mixed = "read 文件内容 from disk";
        let result = truncate_at_boundary(mixed, 8);
        assert!(!result.is_empty());
    }
}
