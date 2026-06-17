//! Context Compression Pipeline
//!
//! Layered, progressive compression. Each layer fires in order, stopping when
//! the token budget is satisfied. The pipeline adjusts the effective budget
//! after each layer so later layers see accurate pressure.
//!
//! Layers (cheapest first):
//! 1. **DuplicateReadElimination** — stub duplicate file reads (cheapest, no content loss).
//! 2. **ToolResultTruncation** — shorten old tool-result content bodies.
//! 3. **TieredCompaction** — drop middle messages, keep system + first user + recent.
//! 4. **ReactiveCompact** — emergency: keep system + first user + last 4.

use astra_turn_core::compaction_types::CompactionTier;
pub use astra_turn_core::compression_types::{
    CompressionResult, Message, PipelineOutcome, TokenBudget,
};
use astra_turn_core::context_assembly_trace::CompressionMethod;

use astra_config::runtime_config::CompressionConfig;
use serde_json::Value;
use std::time::Duration;

use super::compaction::{compact_tiered_impl, CompactResult};
use super::layers::{
    DuplicateReadElimination, ReactiveCompact, TieredCompaction, ToolResultTruncation,
};

// ───────────────────────────── Pipeline ──────────────────────────────────

/// Ordered pipeline of compression layers.
pub struct CompactionEngine {
    layers: Vec<Box<dyn astra_turn_core::compression_types::CompressionLayer>>,
}

impl Default for CompactionEngine {
    fn default() -> Self {
        Self::default_pipeline_for(64_000)
    }
}

impl CompactionEngine {
    pub fn new() -> Self {
        Self { layers: Vec::new() }
    }

    pub fn add_layer(
        &mut self,
        layer: Box<dyn astra_turn_core::compression_types::CompressionLayer>,
    ) {
        self.layers.push(layer);
    }

    /// Return the minimum trigger pressure across all layers.
    /// Used for the fast-path check that skips conversion when under budget.
    fn min_trigger_pressure(&self) -> f64 {
        self.layers
            .iter()
            .map(|l| l.trigger_pressure())
            .fold(f64::MAX, f64::min)
    }

    /// Run all layers in order, adjusting the effective budget between layers.
    ///
    /// After each layer fires, `last_measured_tokens` is reduced by the freed
    /// amount so the next layer's trigger check sees accurate pressure.
    /// Stops early once the budget is satisfied.
    ///
    /// Accepts `Vec<Value>` (the system's wire format), converts to typed
    /// `Vec<Message>` internally, runs the pipeline, and converts back.
    /// When budget pressure is below every layer's trigger threshold, the
    /// conversion is skipped entirely to avoid allocation overhead.
    pub fn compress_if_needed(
        &self,
        messages: &mut Vec<Value>,
        budget: &TokenBudget,
    ) -> PipelineOutcome {
        astra_turn_core::chat_history_openai::sanitize_empty_assistant_tool_calls_mut(messages);

        // Fast-path: if no layer would fire, skip the Value→Message→Value
        // round-trip to avoid Vec allocation on the hot path.
        let pressure = budget.pressure();
        if pressure <= self.min_trigger_pressure() {
            return PipelineOutcome {
                layer_results: Vec::new(),
                total_tokens_freed: 0,
                budget_satisfied: !budget.is_over_budget(),
            };
        }

        // Convert to typed Messages. We clone Values into Messages
        // so the caller's original Vec<Value> survives any panic in
        // Message::from or in the layer loop below.
        let mut typed: Vec<Message> = messages.iter().map(|v| Message::from(v.clone())).collect();

        let outcome = self.compress_typed(&mut typed, budget);

        // Only now — after every layer succeeded — overwrite the caller's
        // messages. If any layer panicked, the original Vec<Value> is intact.
        *messages = typed.into_iter().map(Value::from).collect();

        // Re-sanitize after every typed round-trip: Message::from may filter
        // malformed tool_calls even when no layer ultimately frees tokens, and
        // that can still leave an empty tool_calls array on the way back out.
        astra_turn_core::chat_history_openai::sanitize_empty_assistant_tool_calls_mut(messages);

        outcome
    }

    /// Run the layer pipeline on typed `Vec<Message>` without the Value
    /// round-trip. Tests use this to inspect `_`-prefixed boundary markers
    /// (`_compact_boundary`, `_messages_removed`, ...) that the wire
    /// serializer (`From<Message> for Value`) intentionally strips to keep
    /// prompt-cache prefixes stable. Production callers use
    /// `compress_if_needed`, which delegates here after the Value→Message
    /// conversion.
    pub(crate) fn compress_typed(
        &self,
        typed: &mut Vec<Message>,
        budget: &TokenBudget,
    ) -> PipelineOutcome {
        let pressure = budget.pressure();
        if pressure <= self.min_trigger_pressure() {
            return PipelineOutcome {
                layer_results: Vec::new(),
                total_tokens_freed: 0,
                budget_satisfied: !budget.is_over_budget(),
            };
        }

        let mut running_budget = budget.clone();
        let mut layer_results = Vec::new();
        let mut total_freed: u64 = 0;

        for layer in &self.layers {
            if running_budget.pressure() <= layer.trigger_pressure() {
                continue;
            }

            let result = layer.compress(typed, &running_budget);
            if result.estimated_tokens_freed == 0 {
                continue;
            }

            total_freed += result.estimated_tokens_freed;
            running_budget.last_measured_tokens = running_budget
                .last_measured_tokens
                .saturating_sub(result.estimated_tokens_freed);

            layer_results.push((layer.name().to_string(), result));

            if !running_budget.is_over_budget() {
                break;
            }
        }

        PipelineOutcome {
            layer_results,
            total_tokens_freed: total_freed,
            budget_satisfied: !running_budget.is_over_budget(),
        }
    }

    /// Build a pipeline from RuntimeConfig's CompressionConfig.
    ///
    /// `max_tokens` is the model's context-window limit, used to compute
    /// adaptive thresholds via `CompactionTier` so that large-window models
    /// aren't over-compressed.
    pub fn from_config(config: &CompressionConfig, max_tokens: u64) -> Self {
        let base = CompactionTier::pre_turn_trigger(max_tokens);
        let mut p = Self::new();
        // Layer cascade (light → heavy) with CompactionTier-derived base.
        p.add_layer(Box::new(DuplicateReadElimination::new(
            (base * 0.625).clamp(0.0, 1.0),
        )));
        p.add_layer(Box::new(ToolResultTruncation::new(
            Duration::from_secs(3600),
            config
                .max_tool_result_length
                .try_into()
                .expect("u32 → usize conversion"),
            (base * 0.75).clamp(0.0, 1.0),
        )));
        p.add_layer(Box::new(TieredCompaction::new(
            config
                .preserve_recent_turns
                .try_into()
                .expect("u32 → usize conversion"),
            (base * 0.9375).clamp(0.0, 1.0),
        )));
        p.add_layer(Box::new(ReactiveCompact::new(0.95)));
        p
    }

    /// Default pipeline for the given context window size.
    /// Uses CompactionTier-derived adaptive thresholds.
    pub fn default_pipeline_for(max_tokens: u64) -> Self {
        Self::from_config(&CompressionConfig::default(), max_tokens)
    }

    /// Aggressive pipeline for second-chance compaction retries.
    /// All thresholds set to 0.0 so every layer fires unconditionally.
    pub fn aggressive_pipeline() -> Self {
        let mut p = Self::new();
        p.add_layer(Box::new(DuplicateReadElimination::new(0.0)));
        p.add_layer(Box::new(ToolResultTruncation::new(
            Duration::from_secs(300),
            512,
            0.0,
        )));
        p.add_layer(Box::new(TieredCompaction::new(2, 0.0)));
        p.add_layer(Box::new(ReactiveCompact::new(0.0)));
        p
    }

    /// Emergency pipeline — absolute last resort before propagating error.
    /// All thresholds set to 0.0 so every layer fires unconditionally.
    pub fn emergency_pipeline() -> Self {
        let mut p = Self::new();
        p.add_layer(Box::new(DuplicateReadElimination::new(0.0)));
        p.add_layer(Box::new(ToolResultTruncation::new(
            Duration::from_secs(0),
            128,
            0.0,
        )));
        p.add_layer(Box::new(TieredCompaction::new(1, 0.0)));
        p.add_layer(Box::new(ReactiveCompact::new(0.0)));
        p
    }

    /// Micro-compact pipeline for manual `/compact` preprocessing.
    ///
    /// Runs only DuplicateReadElimination + ToolResultTruncation (no
    /// message-dropping layers). Designed to reduce input tokens before
    /// the LLM summary call without losing conversation structure.
    /// All thresholds are 0.0 so layers fire unconditionally.
    pub fn micro_pipeline() -> Self {
        let mut p = Self::new();
        p.add_layer(Box::new(DuplicateReadElimination::new(0.0)));
        p.add_layer(Box::new(ToolResultTruncation::new(
            Duration::from_secs(0),
            2000,
            0.0,
        )));
        p
    }

    /// Tier-aware compaction that returns a [`CompactResult`] with rich metadata.
    ///
    /// Delegates to the pipeline's `compress_if_needed` and constructs a
    /// `CompactResult` with `CompactBoundary` from the pipeline outcome.
    /// Replaces the monolithic `compact_tiered_with_result` from `compaction.rs`.
    ///
    /// Creates a fresh engine configured for the given tier and turn preservation,
    /// then runs the pipeline.
    pub fn compact_tiered(
        messages: &mut Vec<Value>,
        budget_chars: usize,
        keep_chars: usize,
        tier: CompactionTier,
        keep_recent_turns: usize,
    ) -> CompactResult {
        let result =
            compact_tiered_impl(messages, budget_chars, keep_chars, tier, keep_recent_turns);
        *messages = result.messages.clone();
        result
    }
}

// ───────────────────────────── Shared helpers ───────────────────────────
// helpers.rs — free functions and constants used directly by compaction layers.

#[cfg(test)]
#[path = "compaction_engine_tests.rs"]
mod tests;
