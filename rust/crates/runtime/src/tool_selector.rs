//! Tool selection strategies.
//!
//! # Architecture
//!
//! **Production CLI** (`astra-cli` REPL and background plan) wires [`TfIdfSelector`] only so
//! tool subsetting does not add a second LLM round-trip before the main task call.
//! [`LlmToolSelector`] and [`FallbackSelector`] remain in this crate for unit tests and for
//! callers that choose to compose them explicitly.
//!
//! Tool selection is a **separate concern** from tool execution and LLM chat.
//! **Agent Skills** are surfaced and ranked in [`crate::turn::skill_tool`] (e.g. `select_skills_for_turn`);
//! this module only scores tools from [`crate::tool_registry`].
//!
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
//! - [`FallbackSelector`]: Runs TF-IDF first, then may call LLM (primary) when confidence is low;
//!   if the LLM yields nothing usable, falls back to the TF-IDF result.
//!
//! # Design rationale
#![allow(deprecated)]
//!
//! ConversationState was a **leaky abstraction**: every new edge case required
//! adding a field (`is_github`, `is_fetch`, `recent_tools`, etc.), effectively
//! simulating a mini language model with struct fields. The correct fix is to
//! let the actual LLM handle semantic understanding, and keep heuristics only
//! as a fallback for when the LLM is unavailable.
//!
//! The next edge case should be handled by **improving the LLM prompt**, not
//! by adding a field to ConversationState.

use crate::pipeline::routing::{DomainHint, RoutingEngine, TaskType, domain_hint_to_label};
use crate::tool_registry::{self, TOOL_CATALOG, ToolQualityTracker, ToolRegistry};
use astra_thin_client::ThinClient;
use astra_turn_core::routing_metrics::ConfidenceCalibrator;
use async_trait::async_trait;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Timeout for LLM-based tool selection requests.
const TOOL_SELECT_TIMEOUT: Duration = Duration::from_secs(8);

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
    /// Per-tool selector bias derived from recent outcome memory
    /// (`ToolHealthTracker::outcome_bias_by_tool`). Positive entries mildly
    /// boost a tool's score; negative entries soft-deprioritize it. Bounded
    /// to ±0.10 in the scoring pipeline so it never overrides strong
    /// textual/intent signals — use `restricted_tools` for hard exclusions.
    pub outcome_bias:
        std::collections::HashMap<String, astra_turn_core::tool_health::OutcomeBiasEntry>,
    /// Fallback action from the previous turn's confidence diagnosis.
    /// When `Some(Broaden)`, the selector should relax budget constraints
    /// and include more candidate tools.
    pub previous_confidence_fallback:
        Option<astra_turn_core::confidence_contract::ConfidenceFallback>,
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
    /// Deprecated: skill activation now goes through the `skill` tool in the
    /// agentic loop. This field is always empty and will be removed.
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

/// Strategy for selecting tools from the catalog.
#[async_trait]
pub trait ToolSelector: Send + Sync {
    async fn select(&self, ctx: &SelectionContext<'_>) -> SelectionResult;

    /// Access the underlying tool registry for schema/cost queries.
    /// Returns the default registry if the selector doesn't own one.
    fn registry(&self) -> &ToolRegistry {
        static DEFAULT: std::sync::LazyLock<ToolRegistry> =
            std::sync::LazyLock::new(|| ToolRegistry::new(vec![]));
        &DEFAULT
    }

    /// Record the outcome of a turn for progressive learning.
    /// Default is no-op.
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
pub struct TfIdfSelector {
    registry: ToolRegistry,
    /// Session-scoped quality tracker. When present, historical tool effectiveness
    /// boosts/penalizes tool rankings (self-improving feedback loop).
    quality_tracker: Option<Arc<Mutex<ToolQualityTracker>>>,
    /// Session-scoped confidence calibrator. When present, adjusts score thresholds
    /// based on historical correction rates per intent type.
    confidence_calibrator: Option<Arc<ConfidenceCalibrator>>,
}

fn routing_memory_hints_for_selection(
    ctx: &SelectionContext<'_>,
    boost_terms: &[String],
) -> Vec<String> {
    let mut hints = boost_terms.to_vec();
    for domain in &ctx.memory_domain_hints {
        hints.push(domain_hint_to_label(*domain).to_string());
    }
    for entry in &ctx.file_context {
        hints.push(entry.clone());
        if matches!(
            entry.as_str(),
            "rust" | "typescript" | "javascript" | "python" | "go" | "java" | "cpp" | "docker"
        ) {
            hints.push("code".to_string());
        }
    }
    hints.sort();
    hints.dedup();
    hints
}

impl TfIdfSelector {
    pub fn new(registry: ToolRegistry) -> Self {
        Self {
            registry,
            quality_tracker: None,
            confidence_calibrator: None,
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

    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    /// Get a reference to the quality tracker (if set) for recording feedback.
    pub fn quality_tracker(&self) -> Option<&Arc<Mutex<ToolQualityTracker>>> {
        self.quality_tracker.as_ref()
    }

    /// Record the outcome of a turn for learning.
    ///
    /// Updates the [`ToolQualityTracker`] (if wired) with per-tool success/
    /// quality scores. Entity/pattern/calibration learning has been removed.
    #[allow(clippy::too_many_arguments)]
    pub fn record_turn_outcome(
        &self,
        _query: &str,
        tools_used: &[String],
        _task_type: TaskType,
        _domain: Option<DomainHint>,
        success: bool,
        quality: f64,
        _was_corrected: bool,
        _user_feedback_score: Option<i64>,
    ) {
        // Record tool usage feedback → ToolQualityTracker
        if let Some(qt) = &self.quality_tracker
            && let Ok(mut guard) = qt.lock()
        {
            // Record which tools were actually used (vs selected)
            let feedback = tool_registry::SelectionFeedback {
                tools_used: tools_used.to_vec(),
                unused_count: 0, // not tracked at this level
                precision: 0.0,
                recall: 0.0,
            };
            guard.record_feedback(&feedback);

            // Record per-tool quality based on turn success
            let score = if success {
                quality.clamp(0.0, 1.0)
            } else {
                0.0
            };
            for tool in tools_used {
                guard.record_quality(tool, score);
            }
        }
    }
}

#[async_trait]
impl ToolSelector for TfIdfSelector {
    fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    async fn select(&self, ctx: &SelectionContext<'_>) -> SelectionResult {
        // Fast path: with all tools pinned (0 dynamic), skip the entire
        // scoring/ranking pipeline and return all schemas directly.
        if ToolRegistry::dynamic_count() == 0 {
            return SelectionResult {
                tool_names: self.registry.all_schema_names(),
                strategy: "all_pinned",
                budget_used: 0,
                failed: false,
                confidence: 1.0,
                selector_tokens_in: 0,
                selector_tokens_out: 0,
                selected_skills: vec![],
            };
        }

        // ── Phase 1: Gather boost terms from the caller-provided context ──
        let all_boost: Vec<String> = ctx.boost_terms.clone();
        let routing_memory_hints = routing_memory_hints_for_selection(ctx, &all_boost);

        // ── Phase 2: Compute unified routing decision ──
        let routing = RoutingEngine::analyze(
            ctx.query,
            ctx.turn_count,
            ctx.recent_tools,
            &routing_memory_hints,
            all_boost.clone(),
        );

        // Pattern-boost / co-occurrence hint slots — currently unused.
        // `SelectionContext::boost_terms` is the live boost channel; these
        // two were once populated from runtime learning and are now empty
        // placeholders preserved for the scoring function signature.
        let pattern_boost: Vec<String> = Vec::new();
        let co_occurrence: std::collections::HashMap<String, f64> =
            std::collections::HashMap::new();

        // ── Phase 4: Select tools via RoutingDecision path ──
        // Apply budget pressure: reduce effective budget under token pressure.
        // pressure=0.0 → full budget, pressure=1.0 → 30% budget.
        // Enforce a minimum floor so pressure can never starve tool selection.
        const MINIMUM_TOOL_BUDGET: u32 = 300;

        // If the previous turn diagnosed low confidence, reduce budget pressure
        // to broaden the tool set for this turn.
        let effective_pressure = match ctx.previous_confidence_fallback {
            Some(astra_turn_core::confidence_contract::ConfidenceFallback::Broaden) => {
                (ctx.budget_pressure * 0.3).min(0.3) // cap pressure at 0.3
            }
            Some(astra_turn_core::confidence_contract::ConfidenceFallback::EscalateToLlm) => {
                0.0 // remove all pressure to maximize tool availability
            }
            _ => ctx.budget_pressure,
        };

        let effective_budget = if effective_pressure > 0.0 {
            let scale = 1.0 - effective_pressure.clamp(0.0, 1.0) * 0.7;
            ((ctx.budget_tokens as f64 * scale) as u32).max(MINIMUM_TOOL_BUDGET)
        } else {
            ctx.budget_tokens.max(MINIMUM_TOOL_BUDGET)
        };

        let tracker_guard: Option<std::sync::MutexGuard<'_, ToolQualityTracker>> =
            self.quality_tracker.as_ref().and_then(|t| t.lock().ok());
        let tracker_ref: Option<&ToolQualityTracker> = tracker_guard.as_deref();
        let calibrator_ref = self.confidence_calibrator.as_deref();

        // Scoring pipeline only needs the numeric score; render-time tag
        // is consumed separately by SelfModel. Project entries down to f64
        // at this boundary to avoid threading OutcomeBiasEntry through the
        // scoring/ranker internals.
        let outcome_bias_scores: std::collections::HashMap<String, f64> = ctx
            .outcome_bias
            .iter()
            .map(|(k, v)| (k.clone(), v.score))
            .collect();
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
            &outcome_bias_scores,
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

/// System prompt for the tool selection LLM call.
const TOOL_SELECT_SYSTEM: &str = "\
You are a tool selector. Given the user's query and context, decide which tools are needed.
Return ONLY a JSON array of tool names. Select 1-5 items total. Do not explain.
Pinned tools (bash, read_file, str_replace) are always available — do NOT include them.
Only select from the dynamic tools listed below. The list is executable registry tools only — never output Agent Skill names (skills use the separate `skill` tool in the main agent loop).";

fn build_tool_select_prompt(query: &str, recent_tools: &[String], catalog: &str) -> Vec<Value> {
    let system = format!("{}\n\nDynamic tools:\n{}", TOOL_SELECT_SYSTEM, catalog);
    let mut user_msg = format!("Query: {}", query);
    if !recent_tools.is_empty() {
        user_msg.push_str(&format!("\nRecently used: {:?}", recent_tools));
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
    api: ThinClient,
    token: String,
    model: Option<String>,
    catalog_summary: String,
}

impl LlmToolSelector {
    pub fn new(api: ThinClient, token: String) -> Self {
        let catalog_summary = build_catalog_summary();
        Self {
            api,
            token,
            model: None,
            catalog_summary,
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Make a lightweight SSE call and collect the full response text.
    /// Returns (text, tokens_in, tokens_out).
    async fn call_llm(&self, messages: Vec<Value>) -> Result<(String, u64, u64), String> {
        let mut messages = messages;
        crate::turn::llm_client::strip_empty_assistant_tool_calls(&mut messages);
        let mut payload = serde_json::json!({
            "messages": messages,
        });
        if let Some(ref model) = self.model {
            payload["model"] = Value::String(model.clone());
        }

        let resp = self
            .api
            .post_chat_turn_timeout(&self.token, &payload, TOOL_SELECT_TIMEOUT)
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
                    // Raw upstream OpenAI shape (non-bridge path): usage is
                    // nested with OpenAI-native keys.
                    if let Some(usage) = chunk.get("usage") {
                        tin = usage
                            .get("prompt_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(tin);
                        tout = usage
                            .get("completion_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(tout);
                    }
                    // InProcess bridge format — events use canonical keys
                    // (see `turn::token_usage::TokenUsage`).
                    if let Some(t) = chunk.get("type").and_then(Value::as_str) {
                        if t == "text_delta"
                            && let Some(d) = chunk.get("content").and_then(Value::as_str)
                        {
                            text.push_str(d);
                        }
                        if t == "usage" {
                            if let Some(v) = chunk.get("input_tokens").and_then(|v| v.as_u64()) {
                                tin = v;
                            }
                            if let Some(v) = chunk.get("output_tokens").and_then(|v| v.as_u64()) {
                                tout = v;
                            }
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
        let messages = build_tool_select_prompt(ctx.query, ctx.recent_tools, &self.catalog_summary);

        match self.call_llm(messages).await {
            Ok((text, tin, tout)) => {
                let names = parse_tool_names_from_llm(&text);

                let valid_tools: std::collections::HashSet<&str> =
                    TOOL_CATALOG.iter().map(|t| t.name).collect();

                let tool_names: Vec<String> = names
                    .into_iter()
                    .filter(|n| valid_tools.contains(n.as_str()))
                    .collect();

                if tool_names.is_empty() {
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
                    selected_skills: vec![],
                }
            }
            Err(e) => {
                // LLM call failed — signal fallback
                tracing::debug!(error = %e, "LLM tool selection error");
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

    async fn select(&self, ctx: &SelectionContext<'_>) -> SelectionResult {
        // Fast path: run the TF-IDF fallback first.
        let fast_result = self.fallback.select(ctx).await;

        let has_dynamic_tools = fast_result.tool_names.iter().any(|n| {
            !crate::tool_registry::TOOL_CATALOG
                .iter()
                .any(|t| t.pinned && t.name == n.as_str())
        });

        // High confidence with dynamic tools → trust TF-IDF.
        const CONFIDENCE_THRESHOLD: f64 = 0.5;
        if fast_result.confidence >= CONFIDENCE_THRESHOLD && has_dynamic_tools {
            return fast_result;
        }

        // No dynamic tools → TF-IDF result is sufficient.
        if !has_dynamic_tools {
            return fast_result;
        }

        // Low/mid confidence with dynamic tools → ask the primary (LLM) selector.
        let result = self.primary.select(ctx).await;
        if !result.failed && !result.tool_names.is_empty() {
            result
        } else {
            let mut r = fast_result;
            // Primary (usually LLM) did run — its latency is in the outer wall clock even though
            // we discard its output and keep TF-IDF. Trace should not read as "pure tfidf_routed".
            r.strategy = "tfidf_routed_after_llm";
            r
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
/// JSON schemas from the registry. Pinned tools are always included.
pub fn resolve_schemas(
    registry: &ToolRegistry,
    selected_names: &[String],
) -> (Vec<Value>, tool_registry::SelectionReport) {
    resolve_schemas_with_pressure(registry, selected_names, 0.0)
}

/// Pressure-aware variant of [`resolve_schemas`].
///
/// **Post-Phase-6 behavior:** produces a byte-stable `tools[]` from the
/// [`ToolSurface`] rather than the selector's per-turn ranked output.
/// Every turn in a session returns the same schemas — the `<functions>`
/// block is static. Plugins stay in the deferred
/// listing; user pins them via `runtime.tool_surface.pinned_tools`.
///
/// `selected_names` is retained as observability input — the selector
/// still runs and records what it *would* have picked, but its output
/// no longer mutates `tools[]`.
///
/// At increasing pressure levels, schemas are progressively pruned:
/// - `>= 0.3`: Truncate descriptions + remove param descriptions (~40%)
/// - `>= 0.6`: Remove descriptions entirely (~60%)
/// - `>= 0.8`: Also skip deferrable pinned tools + strip optional params (~70%)
pub fn resolve_schemas_with_pressure(
    registry: &ToolRegistry,
    selected_names: &[String],
    budget_pressure: f64,
) -> (Vec<Value>, tool_registry::SelectionReport) {
    resolve_schemas_with_surface(
        registry,
        selected_names,
        budget_pressure,
        &astra_config::ToolSurfaceConfig::default(),
    )
}

/// Surface-aware resolver used by production call sites that have a
/// loaded [`ToolSurfaceConfig`] in scope. Prefer this entry point when
/// the user's pinned-tools override needs to take effect.
pub fn resolve_schemas_with_surface(
    registry: &ToolRegistry,
    selected_names: &[String],
    budget_pressure: f64,
    cfg: &astra_config::ToolSurfaceConfig,
) -> (Vec<Value>, tool_registry::SelectionReport) {
    let prune_level = if budget_pressure >= 0.8 {
        PruneLevel::Aggressive
    } else if budget_pressure >= 0.6 {
        PruneLevel::Medium
    } else if budget_pressure >= 0.3 {
        PruneLevel::Light
    } else {
        PruneLevel::None
    };

    // Build the surface from the registry's full catalog. Plugins live
    // in `all_schemas` post-register but NOT in `plugin_tool_names`
    // (Phase-5 rule); `ToolSurface::build` partitions everything that
    // isn't pinned into the deferred listing.
    let surface = crate::tool_registry::surface::ToolSurface::build(
        registry.all_schemas().to_vec(),
        cfg,
        &[],
    );

    let schemas: Vec<Value> = surface
        .pinned_schemas()
        .into_iter()
        .map(|s| prune_schema(s, prune_level))
        .collect();
    let names: Vec<String> = schemas
        .iter()
        .filter_map(|s| {
            s.get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .map(String::from)
        })
        .collect();

    // `selected_names` is preserved for the report so observability
    // still sees what the selector wanted, but it no longer mutates
    // the actual `tools[]`.
    let _ = selected_names;

    let budget_used: u32 = names.iter().map(|n| registry.token_cost(n)).sum();
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
        // Extract required names first (before mutable borrow of properties).
        // Include every field required by *any* branch of a consolidated
        // tool's `allOf` / `if-then-required` chain, not just the top-level
        // `required`. Otherwise AggressivePrune would strip per-action
        // required fields (e.g. `agent.spawn` needs `description`+`prompt`
        // only when `action=="spawn"`), leaving the LLM unable to call the
        // branch under context pressure.
        let required_names: std::collections::HashSet<String> = if level == PruneLevel::Aggressive {
            collect_schema_required_union(params)
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

/// Collect every field name that's required by *any* action of the
/// schema — top-level `required` plus every field listed under
/// `x-astra-per-action-required` (shape: `{action: [fields]}`).
/// Used by AggressivePrune so consolidated tools don't lose their
/// per-action required properties when the aggressive tier strips
/// "optional" properties.
///
/// See [`astra_turn_core::tool_schema_prune::collect_required_union`]
/// for the full rationale — Anthropic/Bedrock reject top-level
/// `allOf` in `input_schema`, so per-action required lives in a
/// vendor-prefixed extension instead.
fn collect_schema_required_union(params: &Value) -> std::collections::HashSet<String> {
    // Prefer the shared helper from `astra_turn_core` so the two
    // code paths stay byte-compatible — this is the runtime-side
    // mirror of `tool_schema_prune::collect_required_union`.
    match params.as_object() {
        Some(obj) => astra_turn_core::tool_schema_prune::collect_required_union(obj),
        None => std::collections::HashSet::new(),
    }
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
        let names = parse_tool_names_from_llm(r#"["github", "memory"]"#);
        assert_eq!(names, vec!["github", "memory"]);
    }

    #[test]
    fn parse_json_in_markdown_block() {
        let text = "```json\n[\"git_log\", \"git\"]\n```";
        let names = parse_tool_names_from_llm(text);
        assert_eq!(names, vec!["git_log", "git"]);
    }

    #[test]
    fn parse_json_with_trailing_text() {
        let text = "Based on the query, I'd select:\n[\"github\"]\nThese tools...";
        let names = parse_tool_names_from_llm(text);
        assert_eq!(names, vec!["github"]);
    }

    #[test]
    fn parse_no_json_returns_empty() {
        let names = parse_tool_names_from_llm("I don't know which tools to use");
        assert!(names.is_empty());
    }

    #[test]
    fn parse_malformed_json_returns_empty() {
        let names = parse_tool_names_from_llm("[github]");
        assert!(names.is_empty());
    }

    #[test]
    fn tool_selector_strips_empty_assistant_tool_calls_before_payload() {
        let mut messages = vec![
            serde_json::json!({"role": "assistant", "content": "Done.", "tool_calls": []}),
            serde_json::json!({"role": "user", "content": "hello"}),
        ];
        crate::turn::llm_client::strip_empty_assistant_tool_calls(&mut messages);
        assert!(messages[0].get("tool_calls").is_none(), "{messages:?}");
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
        // Should stay compact (~780 tokens at 4 chars/token). Bumped to 3200
        // after introspect tool added (44 tools total).
        assert!(
            summary.len() < 3200,
            "catalog summary too long: {} chars",
            summary.len()
        );
    }

    // ── Prompt construction ──

    #[test]
    fn prompt_includes_recent_tools() {
        let messages =
            build_tool_select_prompt("matrixone呢？", &["github".to_string()], "catalog");
        let user_msg = messages[1]["content"].as_str().unwrap();
        assert!(user_msg.contains("github"));
        assert!(user_msg.contains("matrixone"));
    }

    // ── TfIdfSelector ──

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
            outcome_bias: std::collections::HashMap::new(),
            previous_confidence_fallback: None,
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
    fn memory_query_has_memory_tools_available() {
        // The consolidated `memory` tool (action-aware: store/retrieve/purge/
        // correct/…) is pinned — always included in the static tool prefix —
        // so a memory-query doesn't need to "activate" it via dynamic ranking.
        // This test asserts the pinning contract: every turn, regardless of
        // query, the pinned prefix carries the memory tool.
        let pinned_memory: Vec<&str> = TOOL_CATALOG
            .iter()
            .filter(|t| t.pinned && t.intents.contains(&IntentType::Memory))
            .map(|t| t.name)
            .collect();
        assert!(
            pinned_memory.contains(&"memory"),
            "memory must be pinned — memory operations require it"
        );
        assert!(
            pinned_memory.len() >= 1,
            "at least 1 memory-intent tool should be pinned, got: {pinned_memory:?}"
        );
    }

    // ── Resolve schemas ──

    #[test]
    fn resolve_schemas_always_includes_pinned() {
        // Post-Phase-6: `tools[]` is sourced from ToolSurface's default
        // pinned set, independent of `selected_names`. The selector's
        // choice is preserved in the report for observability but does
        // NOT mutate the schemas: static tools[], model reaches
        // non-pinned tools via tool_search.
        let registry = mock_registry();
        let (schemas, report) = resolve_schemas(&registry, &["github".into()]);

        let names = &report.tools_selected;
        assert!(names.contains(&"bash".to_string()), "must include bash");
        assert!(
            names.contains(&"read_file".to_string()),
            "must include read_file"
        );
        // github is NOT in tools[] — it's deferred by default. User pins
        // via `runtime.tool_surface.pinned_tools = ["github"]` if desired.
        assert!(
            !names.contains(&"github".to_string()),
            "github must not be in tools[] post-Phase-6; it's deferred"
        );
        assert_eq!(schemas.len(), report.selected_count as usize);
    }

    #[test]
    fn resolve_schemas_produces_byte_stable_output_across_calls() {
        // Cache invariant: identical calls produce identical bytes.
        let registry = mock_registry();
        let (schemas_a, _) = resolve_schemas(&registry, &["bash".into(), "github".into()]);
        let (schemas_b, _) = resolve_schemas(&registry, &["read_file".into()]);
        let (schemas_c, _) = resolve_schemas(&registry, &[]);

        let bytes_a = serde_json::to_vec(&schemas_a).unwrap();
        let bytes_b = serde_json::to_vec(&schemas_b).unwrap();
        let bytes_c = serde_json::to_vec(&schemas_c).unwrap();
        // Regardless of what the selector requested, tools[] is byte-stable.
        assert_eq!(
            bytes_a, bytes_b,
            "tools[] must not depend on selector output"
        );
        assert_eq!(
            bytes_a, bytes_c,
            "tools[] must not depend on selector output"
        );
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
            outcome_bias: std::collections::HashMap::new(),
            previous_confidence_fallback: None,
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
                fixed_result(vec!["github".into()], "tfidf_high", 0.8)
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
    async fn fallback_skips_primary_when_no_dynamic_tools() {
        struct NeverCalledPrimary;
        #[async_trait]
        impl ToolSelector for NeverCalledPrimary {
            async fn select(&self, _ctx: &SelectionContext<'_>) -> SelectionResult {
                panic!("primary should not be called when fallback has no dynamic tools");
            }
        }

        struct PinnedOnlySelector;
        #[async_trait]
        impl ToolSelector for PinnedOnlySelector {
            async fn select(&self, _ctx: &SelectionContext<'_>) -> SelectionResult {
                fixed_result(vec!["bash".into()], "tfidf_conversational", 0.1)
            }
        }

        let selector =
            FallbackSelector::new(Box::new(NeverCalledPrimary), Box::new(PinnedOnlySelector));
        let result = selector.select(&make_ctx("something")).await;
        assert_eq!(result.strategy, "tfidf_conversational");
    }

    // ── Quality Tracker integration ──

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
            outcome_bias: std::collections::HashMap::new(),
            previous_confidence_fallback: None,
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
            outcome_bias: std::collections::HashMap::new(),
            previous_confidence_fallback: None,
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
            outcome_bias: std::collections::HashMap::new(),
            previous_confidence_fallback: None,
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
            outcome_bias: std::collections::HashMap::new(),
            previous_confidence_fallback: None,
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
            outcome_bias: std::collections::HashMap::new(),
            previous_confidence_fallback: None,
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
            outcome_bias: std::collections::HashMap::new(),
            previous_confidence_fallback: None,
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
            outcome_bias: std::collections::HashMap::new(),
            previous_confidence_fallback: None,
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
            outcome_bias: std::collections::HashMap::new(),
            previous_confidence_fallback: None,
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
            outcome_bias: std::collections::HashMap::new(),
            previous_confidence_fallback: None,
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
            outcome_bias: std::collections::HashMap::new(),
            previous_confidence_fallback: None,
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
            outcome_bias: std::collections::HashMap::new(),
            previous_confidence_fallback: None,
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
            outcome_bias: std::collections::HashMap::new(),
            previous_confidence_fallback: None,
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
            outcome_bias: std::collections::HashMap::new(),
            previous_confidence_fallback: None,
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
            outcome_bias: std::collections::HashMap::new(),
            previous_confidence_fallback: None,
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
            outcome_bias: std::collections::HashMap::new(),
            previous_confidence_fallback: None,
        };
        let r2 = selector.select(&ctx_boosted).await;
        assert!(
            r2.confidence >= r1.confidence,
            "boosted confidence ({}) should be >= unboosted ({})",
            r2.confidence,
            r1.confidence
        );
    }

    #[tokio::test]
    async fn record_turn_outcome_updates_quality_tracker() {
        let tracker = Arc::new(Mutex::new(ToolQualityTracker::new()));
        let selector = TfIdfSelector::new(mock_registry()).with_quality_tracker(tracker.clone());

        // Record a successful turn using two tools
        selector.record_turn_outcome(
            "check PRs",
            &["bash".into(), "grep".into()],
            TaskType::Fetch,
            None,
            true,
            0.9,
            false,
            None,
        );

        let guard = tracker.lock().unwrap();
        let entries = guard.all_entries();

        // Both tools should have 1 use and quality recorded
        let bash = entries.get("bash").expect("bash should be tracked");
        assert_eq!(bash.uses, 1, "bash should have 1 use");
        assert!(
            (bash.quality_sum - 0.9).abs() < 0.01,
            "bash quality should be 0.9"
        );

        let grep = entries.get("grep").expect("grep should be tracked");
        assert_eq!(grep.uses, 1, "grep should have 1 use");

        drop(guard);

        // Record a failed turn — quality should be 0.0
        selector.record_turn_outcome(
            "check PRs",
            &["bash".into()],
            TaskType::Fetch,
            None,
            false,
            0.5,
            false,
            None,
        );

        let guard = tracker.lock().unwrap();
        let bash = guard.all_entries().get("bash").unwrap();
        assert_eq!(bash.uses, 2, "bash should have 2 uses");
        assert!(
            (bash.quality_sum - 0.9).abs() < 0.01,
            "failed turn adds 0.0 quality"
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
            outcome_bias: std::collections::HashMap::new(),
            previous_confidence_fallback: None,
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
            outcome_bias: std::collections::HashMap::new(),
            previous_confidence_fallback: None,
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
            outcome_bias: std::collections::HashMap::new(),
            previous_confidence_fallback: None,
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
            outcome_bias: std::collections::HashMap::new(),
            previous_confidence_fallback: None,
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
            outcome_bias: std::collections::HashMap::new(),
            previous_confidence_fallback: None,
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
            outcome_bias: std::collections::HashMap::new(),
            previous_confidence_fallback: None,
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
            outcome_bias: std::collections::HashMap::new(),
            previous_confidence_fallback: None,
        };
        let result_hint = selector.select(&ctx_hint).await;

        // With domain hint, confidence should be >= no-hint case
        // (the hint adds score to GitHub tools, improving overall confidence)
        let gh_tools = ["github", "github", "github"];
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
            outcome_bias: std::collections::HashMap::new(),
            previous_confidence_fallback: None,
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
            outcome_bias: std::collections::HashMap::new(),
            previous_confidence_fallback: None,
        };
        let result = selector.select(&ctx).await;
        // Should still select some tools (domain hint helps ranking despite pressure)
        assert!(!result.failed, "Combined pressure+hints should not fail");
        // The tools selected should be the most relevant (GitHub) ones
        let has_github = result
            .tool_names
            .iter()
            .any(|t| t == "github" || t.starts_with("github_"));
        assert!(
            has_github,
            "Even under pressure, domain hint should keep github tool: {:?}",
            result.tool_names
        );
    }

    // ── Phase 3: Tool Selection E2E Tests ──────────────────────────────────────
    //
    // Verify tool selector picks correct tools for common query patterns.
    // These tests catch regressions in tool selection quality.

    /// Phase 3a: "review latest commit" selects git tools.
    #[tokio::test]
    async fn tool_select_review_commit_selects_git_tools() {
        let selector = TfIdfSelector::new(mock_registry());
        let ctx = SelectionContext {
            query: "review the latest commit",
            turn_count: 1,
            recent_tools: &[],
            budget_tokens: 800,
            boost_terms: vec![],
            budget_pressure: 0.0,
            memory_domain_hints: vec![],
            restricted_tools: vec![],
            file_context: vec![],
            outcome_bias: std::collections::HashMap::new(),
            previous_confidence_fallback: None,
        };
        let result = selector.select(&ctx).await;

        let has_git = result
            .tool_names
            .iter()
            .any(|n| n == "git" || n.starts_with("git_"));
        assert!(
            has_git,
            "'review latest commit' should select the git tool, got: {:?}",
            result.tool_names
        );
        // git tool should be selected for commit review
        let has_key_git_tool = result
            .tool_names
            .iter()
            .any(|n| n == "git_log" || n == "git" || n == "git_show");
        assert!(
            has_key_git_tool,
            "'review latest commit' should select git, got: {:?}",
            result.tool_names
        );
    }

    /// Phase 3b: "show git status" selects git_status.
    #[tokio::test]
    async fn tool_select_git_status_query() {
        let selector = TfIdfSelector::new(mock_registry());
        let ctx = SelectionContext {
            query: "show git status",
            turn_count: 1,
            recent_tools: &[],
            budget_tokens: 800,
            boost_terms: vec![],
            budget_pressure: 0.0,
            memory_domain_hints: vec![],
            restricted_tools: vec![],
            file_context: vec![],
            outcome_bias: std::collections::HashMap::new(),
            previous_confidence_fallback: None,
        };
        let result = selector.select(&ctx).await;
        assert!(
            result.tool_names.contains(&"git".to_string()),
            "'show git status' should select git_status, got: {:?}",
            result.tool_names
        );
    }

    /// Phase 3c: "hi" / conversational → only pinned tools, no dynamic.
    #[tokio::test]
    async fn tool_select_greeting_only_pinned() {
        let selector = TfIdfSelector::new(mock_registry());
        for query in &["hi", "hello", "thanks"] {
            let ctx = SelectionContext {
                query,
                turn_count: 1,
                recent_tools: &[],
                budget_tokens: 800,
                boost_terms: vec![],
                budget_pressure: 0.0,
                memory_domain_hints: vec![],
                restricted_tools: vec![],
                file_context: vec![],
                outcome_bias: std::collections::HashMap::new(),
                previous_confidence_fallback: None,
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
                "query '{query}' should have 0 dynamic tools, got {dynamic_count}: {:?}",
                result.tool_names
            );
        }
    }

    /// Phase 3d: "read and edit the config file" selects file tools.
    #[tokio::test]
    async fn tool_select_file_edit_query() {
        let selector = TfIdfSelector::new(mock_registry());
        let ctx = SelectionContext {
            query: "read and edit the config file",
            turn_count: 1,
            recent_tools: &[],
            budget_tokens: 800,
            boost_terms: vec![],
            budget_pressure: 0.0,
            memory_domain_hints: vec![],
            restricted_tools: vec![],
            file_context: vec![],
            outcome_bias: std::collections::HashMap::new(),
            previous_confidence_fallback: None,
        };
        let result = selector.select(&ctx).await;
        // read_file and str_replace are pinned, so always present.
        // But write_file should also be selected for edit queries.
        assert!(
            result.tool_names.contains(&"read_file".to_string()),
            "file edit query should include read_file (pinned): {:?}",
            result.tool_names
        );
        assert!(
            result.tool_names.contains(&"str_replace".to_string()),
            "file edit query should include str_replace (pinned): {:?}",
            result.tool_names
        );
    }

    #[tokio::test]
    async fn tool_select_new_file_query_includes_write_file() {
        let selector = TfIdfSelector::new(mock_registry());
        let ctx = SelectionContext {
            query: "create a new file called main.rs",
            turn_count: 1,
            recent_tools: &[],
            budget_tokens: 300,
            boost_terms: vec![],
            budget_pressure: 0.0,
            memory_domain_hints: vec![],
            restricted_tools: vec![],
            file_context: vec![],
            outcome_bias: std::collections::HashMap::new(),
            previous_confidence_fallback: None,
        };
        let result = selector.select(&ctx).await;
        assert!(
            result.tool_names.contains(&"write_file".to_string()),
            "new-file query should still include write_file after unpinning: {:?}",
            result.tool_names
        );
    }

    /// Phase 3e: "git diff HEAD~1" selects git_diff specifically.
    #[tokio::test]
    async fn tool_select_git_diff_query() {
        let selector = TfIdfSelector::new(mock_registry());
        let ctx = SelectionContext {
            query: "git diff HEAD~1",
            turn_count: 1,
            recent_tools: &[],
            budget_tokens: 800,
            boost_terms: vec![],
            budget_pressure: 0.0,
            memory_domain_hints: vec![],
            restricted_tools: vec![],
            file_context: vec![],
            outcome_bias: std::collections::HashMap::new(),
            previous_confidence_fallback: None,
        };
        let result = selector.select(&ctx).await;
        assert!(
            result.tool_names.contains(&"git".to_string()),
            "'git diff HEAD~1' must select git_diff, got: {:?}",
            result.tool_names
        );
    }

    /// Phase 3f: Pinned tools always present regardless of query.
    #[tokio::test]
    async fn tool_select_pinned_always_present() {
        let selector = TfIdfSelector::new(mock_registry());
        let pinned_names: Vec<&str> = TOOL_CATALOG
            .iter()
            .filter(|t| t.pinned)
            .map(|t| t.name)
            .collect();

        for query in &["review latest commit", "hi", "search github", "run tests"] {
            let ctx = SelectionContext {
                query,
                turn_count: 1,
                recent_tools: &[],
                budget_tokens: 800,
                boost_terms: vec![],
                budget_pressure: 0.0,
                memory_domain_hints: vec![],
                restricted_tools: vec![],
                file_context: vec![],
                outcome_bias: std::collections::HashMap::new(),
                previous_confidence_fallback: None,
            };
            let result = selector.select(&ctx).await;
            for pinned in &pinned_names {
                assert!(
                    result.tool_names.contains(&pinned.to_string()),
                    "query '{query}' missing pinned tool '{pinned}': {:?}",
                    result.tool_names
                );
            }
        }
    }
}
