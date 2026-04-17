//! Context compression shared types.
//!
//! Pure types and traits extracted from `context_compression` so that
//! downstream modules like `compaction_replay` can depend on them
//! without pulling in the full runtime.

use serde_json::Value;

/// Token budget for a single turn.
#[derive(Debug, Clone)]
pub struct TokenBudget {
    /// Maximum prompt tokens for the current turn.
    pub max_prompt_tokens: u64,
    /// Last measured prompt tokens from the LLM response.
    pub last_measured_tokens: u64,
    /// Characters-per-token estimate (for cheap pre-checks).
    pub chars_per_token: f64,
}

impl TokenBudget {
    pub fn is_over_budget(&self) -> bool {
        self.max_prompt_tokens > 0 && self.last_measured_tokens > self.max_prompt_tokens
    }

    /// Estimated excess tokens (0 if under budget).
    pub fn excess_tokens(&self) -> u64 {
        self.last_measured_tokens
            .saturating_sub(self.max_prompt_tokens)
    }

    /// Rough pressure ratio (0.0 = no pressure, 1.0+ = over budget).
    pub fn pressure(&self) -> f64 {
        if self.max_prompt_tokens == 0 {
            return 0.0;
        }
        self.last_measured_tokens as f64 / self.max_prompt_tokens as f64
    }
}

/// Result of a single compression layer execution.
#[derive(Debug, Clone, Default)]
pub struct CompressionResult {
    /// How many messages were removed or replaced.
    pub messages_removed: usize,
    /// Estimated tokens freed (approximate).
    pub estimated_tokens_freed: u64,
    /// Human-readable description of what this layer did.
    pub description: String,
    /// Turn indices that were compressed/modified by this layer.
    pub affected_turns: Vec<u32>,
}

/// Outcome of running the full compression pipeline.
#[derive(Debug, Clone)]
pub struct PipelineOutcome {
    /// Per-layer results in execution order.
    pub layer_results: Vec<(String, CompressionResult)>,
    /// Total estimated tokens freed across all layers.
    pub total_tokens_freed: u64,
    /// Whether we believe the budget is now satisfied.
    pub budget_satisfied: bool,
}

impl PipelineOutcome {
    /// Get all turn indices that were affected by compression.
    pub fn all_affected_turns(&self) -> Vec<u32> {
        let mut turns: Vec<u32> = self
            .layer_results
            .iter()
            .flat_map(|(_, result)| result.affected_turns.iter().copied())
            .collect();
        turns.sort_unstable();
        turns.dedup();
        turns
    }

    /// Convert to telemetry trace format.
    pub fn to_compression_trace(
        &self,
    ) -> Vec<(
        String,
        crate::context_assembly_trace::CompressionMethod,
        u32,
    )> {
        use crate::context_assembly_trace::CompressionMethod;

        self.layer_results
            .iter()
            .map(|(name, result)| {
                let method = match name.as_str() {
                    "ToolResultTruncation" => CompressionMethod::ToolResultTruncation,
                    "DuplicateReadElimination" => CompressionMethod::DuplicateReadElimination,
                    "TieredCompaction" => CompressionMethod::TieredCompaction,
                    "ReactiveCompact" => CompressionMethod::ReactiveCompact,
                    _ => CompressionMethod::TieredCompaction,
                };
                (name.clone(), method, result.estimated_tokens_freed as u32)
            })
            .collect()
    }
}

/// A single compression layer.
pub trait CompressionLayer: Send + Sync {
    /// Human-readable name for logging / audit.
    fn name(&self) -> &str;

    /// Quick estimate of how many tokens this layer *could* free.
    fn estimate_savings(&self, messages: &[Value], budget: &TokenBudget) -> u64;

    /// Whether this layer should fire given current pressure.
    fn should_trigger(&self, messages: &[Value], budget: &TokenBudget) -> bool;

    /// Execute compression, mutating the message list in place.
    fn compress(&self, messages: &mut Vec<Value>, budget: &TokenBudget) -> CompressionResult;
}
