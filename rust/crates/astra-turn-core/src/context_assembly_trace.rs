//! Context Assembly Trace — deep observability into turn context composition.
//!
//! Records exactly what goes into each LLM request:
//! - System prompt breakdown (base, skills, environment, memories)
//! - Conversation history selection (retained, compressed, dropped)
//! - Memory retrieval (queries, candidates, selections)
//! - Tool selection (scoring, filtering, final set)
//!
//! This enables answering questions like:
//! - "Why did the agent lose focus?" → compression dropped critical context
//! - "Which memories were effective?" → see relevance scores + selection
//! - "Where are my tokens going?" → detailed breakdown by component

use serde::{Deserialize, Serialize};
use std::time::SystemTime;

// ─── Top-Level Trace ─────────────────────────────────────────────────────────

/// Complete trace of context assembly for one turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextAssemblyTrace {
    /// Unique turn identifier within the session.
    pub turn_id: String,
    /// When this trace was captured.
    pub timestamp: SystemTime,
    /// Session identifier.
    pub session_id: String,

    /// Breakdown of system prompt components.
    pub system_prompt: SystemPromptBreakdown,

    /// History selection and compression decisions.
    pub history: HistorySelectionTrace,

    /// Memory retrieval trace.
    pub memory: MemoryRetrievalTrace,

    /// Tool selection trace.
    pub tools: ToolSelectionTrace,

    /// Final token budget and allocation.
    pub token_budget: TokenBudgetTrace,

    /// Optional: why certain decisions were made.
    pub explanations: Vec<DecisionExplanation>,
}

impl ContextAssemblyTrace {
    /// Serialize this trace to a JSON value for journal persistence.
    ///
    /// The journal stores traces as `serde_json::Value` to avoid cross-crate
    /// type dependencies. This method provides a convenient serialization point.
    pub fn to_json_value(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or_else(|e| {
            serde_json::json!({
                "serialization_error": e.to_string(),
                "turn_id": self.turn_id,
            })
        })
    }
}

impl Default for ContextAssemblyTrace {
    fn default() -> Self {
        Self {
            turn_id: String::new(),
            timestamp: SystemTime::now(),
            session_id: String::new(),
            system_prompt: SystemPromptBreakdown::default(),
            history: HistorySelectionTrace::default(),
            memory: MemoryRetrievalTrace::default(),
            tools: ToolSelectionTrace::default(),
            token_budget: TokenBudgetTrace::default(),
            explanations: Vec::new(),
        }
    }
}

// ─── System Prompt Breakdown ─────────────────────────────────────────────────

/// Detailed breakdown of system prompt token allocation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SystemPromptBreakdown {
    /// Base persona/instructions token count.
    pub base_persona_tokens: u32,
    /// Skills injected into the prompt.
    pub skills_injected: Vec<SkillInjection>,
    /// Environment context (cwd, git info, etc.) tokens.
    pub environment_tokens: u32,
    /// Repository memories injected.
    pub repository_memories: Vec<MemoryInjection>,
    /// User preferences/settings tokens.
    pub user_preferences_tokens: u32,
    /// Structured dynamic context signals present in the prompt.
    #[serde(default)]
    pub context_signals: PromptContextSignals,
    /// Structured late-round guidance signals present in the dynamic prompt.
    #[serde(default)]
    pub guidance_signals: PromptGuidanceSignals,
    /// Total system prompt tokens.
    pub total_tokens: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PromptGuidanceSignals {
    pub round_budget_warning: bool,
    pub synthesize_or_batch: bool,
    pub parallel_feedback: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PromptContextSignals {
    pub active_output_skills: bool,
    pub learned_runtime_context: bool,
    pub memory_signal_detected: bool,
    pub effort_hint: bool,
    pub agent_type_hint: bool,
    pub self_awareness: bool,
    pub implicit_feedback: bool,
    pub learned_feedback_rules: bool,
    pub session_anchor: bool,
}

/// A skill that was injected into the system prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillInjection {
    pub skill_name: String,
    pub skill_version: Option<String>,
    pub tokens: u32,
    /// Why this skill was selected.
    pub selection_reason: String,
}

/// A memory that was injected into the system prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryInjection {
    pub memory_id: String,
    pub memory_type: String,
    pub tokens: u32,
    pub relevance_score: f64,
    /// First ~100 chars of content for identification.
    pub content_preview: String,
}

// ─── History Selection Trace ─────────────────────────────────────────────────

/// Trace of conversation history selection and compression.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HistorySelectionTrace {
    /// Total turns available in the conversation.
    pub total_turns_available: u32,
    /// Turns that were retained in full.
    pub turns_retained: Vec<TurnRetention>,
    /// Turns that were compressed.
    pub turns_compressed: Vec<TurnCompression>,
    /// Turns that were completely dropped.
    pub turns_dropped: Vec<u32>,
    /// Overall compression ratio achieved.
    pub compression_ratio: f64,
    /// Tokens before compression.
    pub tokens_before: u32,
    /// Tokens after compression.
    pub tokens_after: u32,
}

/// A turn that was retained in the history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnRetention {
    pub turn_index: u32,
    pub role: String,
    pub tokens: u32,
    /// Whether this turn contains tool calls (often preserved).
    pub has_tool_calls: bool,
}

/// A turn that was compressed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnCompression {
    pub turn_index: u32,
    pub role: String,
    pub original_tokens: u32,
    pub compressed_tokens: u32,
    pub compression_method: CompressionMethod,
    /// What information was lost (for explainability).
    pub information_lost: Vec<String>,
}

/// Method used to compress a turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CompressionMethod {
    /// Tool result truncation.
    ToolResultTruncation,
    /// Duplicate file read elimination.
    DuplicateReadElimination,
    /// LLM-based summarization.
    LlmSummarization,
    /// Tiered compaction.
    TieredCompaction,
    /// Emergency reactive compression.
    ReactiveCompact,
}

// ─── Memory Retrieval Trace ──────────────────────────────────────────────────

/// Trace of memory retrieval operations.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryRetrievalTrace {
    /// The query used for retrieval.
    pub query: String,
    /// Total candidates considered.
    pub candidates_considered: u32,
    /// Memories that were selected.
    pub memories_selected: Vec<MemorySelection>,
    /// Memories that were considered but not selected.
    pub memories_rejected: Vec<MemoryRejection>,
    /// Total tokens used by selected memories.
    pub total_tokens: u32,
    /// Retrieval latency in milliseconds.
    pub retrieval_latency_ms: u64,
}

/// A memory that was selected for inclusion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemorySelection {
    pub memory_id: String,
    pub memory_type: String,
    pub content_preview: String,
    pub relevance_score: f64,
    pub tokens: u32,
    /// Source of this memory (memoria, session, repository).
    pub source: MemorySource,
}

/// A memory that was considered but rejected.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryRejection {
    pub memory_id: String,
    pub relevance_score: f64,
    pub rejection_reason: RejectionReason,
}

/// Why a memory was rejected.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RejectionReason {
    /// Score below threshold.
    BelowThreshold { threshold: f64, score: f64 },
    /// Would exceed token budget.
    TokenBudgetExceeded { available: u32, required: u32 },
    /// Duplicate of another selected memory.
    Duplicate { of_memory_id: String },
    /// Stale/outdated.
    Stale { age_days: u32 },
}

/// Source of a memory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MemorySource {
    /// Memoria MCP server.
    Memoria,
    /// Session-local store_memory calls.
    Session,
    /// Repository .astra/memories.
    Repository,
    /// User profile memories.
    UserProfile,
}

// ─── Tool Selection Trace ────────────────────────────────────────────────────

/// Trace of tool selection process.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolSelectionTrace {
    /// Tools that were available.
    pub tools_available: u32,
    /// Tools that were selected for the LLM.
    pub tools_selected: Vec<ToolSelected>,
    /// Tools that were considered but not selected.
    pub tools_rejected: Vec<ToolRejected>,
    /// Strategy used for selection.
    pub selection_strategy: String,
    /// Confidence in the selection.
    pub selection_confidence: f64,
    /// Selection latency in milliseconds.
    pub selection_latency_ms: u64,
}

/// A tool that was selected.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSelected {
    pub tool_name: String,
    pub score: f64,
    pub tokens: u32,
    /// Why this tool was selected.
    pub selection_factors: Vec<SelectionFactor>,
}

/// A factor that contributed to tool selection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectionFactor {
    pub factor_name: String,
    pub weight: f64,
    pub contribution: f64,
}

/// A tool that was rejected.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolRejected {
    pub tool_name: String,
    pub score: f64,
    pub rejection_reason: String,
}

// ─── Token Budget Trace ──────────────────────────────────────────────────────

/// Final token budget allocation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenBudgetTrace {
    /// Maximum allowed tokens.
    pub max_tokens: u32,
    /// Tokens allocated to system prompt.
    pub system_prompt_tokens: u32,
    /// Tokens allocated to conversation history.
    pub history_tokens: u32,
    /// Tokens allocated to memory.
    pub memory_tokens: u32,
    /// Tokens allocated to tool schemas.
    pub tool_schema_tokens: u32,
    /// Tokens allocated to current user message.
    pub user_message_tokens: u32,
    /// Total tokens used.
    pub total_used: u32,
    /// Budget pressure (0.0 = relaxed, 1.0 = at limit, >1.0 = over).
    pub budget_pressure: f64,
    /// Whether compression was triggered.
    pub compression_triggered: bool,
}

// ─── Decision Explanations ───────────────────────────────────────────────────

/// Explanation of a context assembly decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionExplanation {
    pub decision_type: DecisionType,
    pub reasoning: String,
    pub alternatives_considered: Vec<Alternative>,
    pub confidence: f64,
}

/// Type of decision being explained.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DecisionType {
    /// Tool selection decision.
    ToolSelection { tools: Vec<String> },
    /// History compression decision.
    HistoryCompression { turns_affected: Vec<u32> },
    /// Memory retrieval decision.
    MemoryRetrieval { memories: Vec<String> },
    /// Strategy choice decision.
    StrategyChoice { strategy: String },
}

/// An alternative that was considered.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Alternative {
    pub description: String,
    pub score: f64,
    pub why_not_chosen: String,
}

// ─── Trace Builder ───────────────────────────────────────────────────────────

/// Builder for constructing a ContextAssemblyTrace incrementally.
#[derive(Debug, Default)]
pub struct ContextAssemblyTraceBuilder {
    trace: ContextAssemblyTrace,
}

impl ContextAssemblyTraceBuilder {
    pub fn new(turn_id: impl Into<String>, session_id: impl Into<String>) -> Self {
        Self {
            trace: ContextAssemblyTrace {
                turn_id: turn_id.into(),
                session_id: session_id.into(),
                timestamp: SystemTime::now(),
                ..Default::default()
            },
        }
    }

    pub fn with_system_prompt(mut self, breakdown: SystemPromptBreakdown) -> Self {
        self.trace.system_prompt = breakdown;
        self
    }

    pub fn with_history(mut self, history: HistorySelectionTrace) -> Self {
        self.trace.history = history;
        self
    }

    pub fn with_memory(mut self, memory: MemoryRetrievalTrace) -> Self {
        self.trace.memory = memory;
        self
    }

    pub fn with_tools(mut self, tools: ToolSelectionTrace) -> Self {
        self.trace.tools = tools;
        self
    }

    pub fn with_token_budget(mut self, budget: TokenBudgetTrace) -> Self {
        self.trace.token_budget = budget;
        self
    }

    pub fn add_explanation(mut self, explanation: DecisionExplanation) -> Self {
        self.trace.explanations.push(explanation);
        self
    }

    pub fn build(self) -> ContextAssemblyTrace {
        self.trace
    }
}

// ─── Trace Aggregation ───────────────────────────────────────────────────────

/// Aggregate statistics across multiple traces.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TraceAggregation {
    pub turn_count: u32,
    /// Average tokens per component.
    pub avg_system_prompt_tokens: f64,
    pub avg_history_tokens: f64,
    pub avg_memory_tokens: f64,
    pub avg_tool_schema_tokens: f64,
    /// Compression statistics.
    pub compression_trigger_rate: f64,
    pub avg_compression_ratio: f64,
    /// Memory effectiveness.
    pub avg_memories_selected: f64,
    pub avg_memory_relevance: f64,
    /// Tool selection statistics.
    pub avg_tools_selected: f64,
    pub avg_selection_confidence: f64,
}

impl TraceAggregation {
    pub fn from_traces(traces: &[ContextAssemblyTrace]) -> Self {
        if traces.is_empty() {
            return Self::default();
        }

        let n = traces.len() as f64;
        let compression_triggered_count = traces
            .iter()
            .filter(|t| t.token_budget.compression_triggered)
            .count() as f64;

        Self {
            turn_count: traces.len() as u32,
            avg_system_prompt_tokens: traces
                .iter()
                .map(|t| t.system_prompt.total_tokens as f64)
                .sum::<f64>()
                / n,
            avg_history_tokens: traces
                .iter()
                .map(|t| t.token_budget.history_tokens as f64)
                .sum::<f64>()
                / n,
            avg_memory_tokens: traces
                .iter()
                .map(|t| t.token_budget.memory_tokens as f64)
                .sum::<f64>()
                / n,
            avg_tool_schema_tokens: traces
                .iter()
                .map(|t| t.token_budget.tool_schema_tokens as f64)
                .sum::<f64>()
                / n,
            compression_trigger_rate: compression_triggered_count / n,
            avg_compression_ratio: traces
                .iter()
                .map(|t| t.history.compression_ratio)
                .sum::<f64>()
                / n,
            avg_memories_selected: traces
                .iter()
                .map(|t| t.memory.memories_selected.len() as f64)
                .sum::<f64>()
                / n,
            avg_memory_relevance: {
                let total_relevance: f64 = traces
                    .iter()
                    .flat_map(|t| t.memory.memories_selected.iter())
                    .map(|m| m.relevance_score)
                    .sum();
                let total_memories: usize = traces
                    .iter()
                    .map(|t| t.memory.memories_selected.len())
                    .sum();
                if total_memories > 0 {
                    total_relevance / total_memories as f64
                } else {
                    0.0
                }
            },
            avg_tools_selected: traces
                .iter()
                .map(|t| t.tools.tools_selected.len() as f64)
                .sum::<f64>()
                / n,
            avg_selection_confidence: traces
                .iter()
                .map(|t| t.tools.selection_confidence)
                .sum::<f64>()
                / n,
        }
    }
}

// ─── Integration with Context Compression ────────────────────────────────────

/// Build HistorySelectionTrace from compression pipeline results.
///
/// This function converts the compression pipeline's internal metrics into
/// the telemetry trace format for observability.
pub fn build_history_trace_from_compression(
    initial_messages: usize,
    _final_messages: usize,
    initial_tokens: u32,
    final_tokens: u32,
    layer_results: &[(String, CompressionMethod, u32)], // (layer_name, method, tokens_freed)
) -> HistorySelectionTrace {
    let mut turns_compressed = Vec::new();

    for (idx, (layer_name, method, tokens_freed)) in layer_results.iter().enumerate() {
        if *tokens_freed > 0 {
            turns_compressed.push(TurnCompression {
                turn_index: idx as u32,
                role: layer_name.clone(),
                original_tokens: *tokens_freed, // Approximate - actual is unknown
                compressed_tokens: 0,
                compression_method: method.clone(),
                information_lost: vec![format!("~{} tokens freed by {}", tokens_freed, layer_name)],
            });
        }
    }

    let compression_ratio = if initial_tokens > 0 {
        final_tokens as f64 / initial_tokens as f64
    } else {
        1.0
    };

    HistorySelectionTrace {
        total_turns_available: initial_messages as u32,
        turns_retained: Vec::new(), // Would need message-level tracking
        turns_compressed,
        turns_dropped: Vec::new(), // Would need message-level tracking
        compression_ratio,
        tokens_before: initial_tokens,
        tokens_after: final_tokens,
    }
}

/// Build ToolSelectionTrace from SelectionResult.
///
/// This function converts the tool selector's result into the telemetry
/// trace format for observability.
pub fn build_tool_trace_from_selection(
    tools_available: u32,
    selected_tools: &[String],
    strategy: &str,
    confidence: f64,
    per_tool_costs: &[(String, u32)],
    selection_latency_ms: u64,
) -> ToolSelectionTrace {
    let tools_selected: Vec<ToolSelected> = selected_tools
        .iter()
        .enumerate()
        .map(|(idx, name)| {
            let tokens = per_tool_costs
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, c)| *c)
                .unwrap_or(0);
            ToolSelected {
                tool_name: name.clone(),
                score: (1.0 - (idx as f64 * 0.1)).max(0.0),
                tokens,
                selection_factors: vec![SelectionFactor {
                    factor_name: "selector".to_string(),
                    weight: 1.0,
                    contribution: confidence,
                }],
            }
        })
        .collect();

    ToolSelectionTrace {
        tools_available,
        tools_selected,
        tools_rejected: Vec::new(), // Would need scorer internals
        selection_strategy: strategy.to_string(),
        selection_confidence: confidence,
        selection_latency_ms,
    }
}

/// Build MemoryRetrievalTrace from ranked memory results.
///
/// This function converts the memory retrieval results into the telemetry
/// trace format for observability.
pub fn build_memory_trace_from_retrieval(
    query: &str,
    candidates_count: u32,
    ranked_results: &[(String, f64)], // (content, score)
    retrieval_latency_ms: u64,
) -> MemoryRetrievalTrace {
    let memories_selected: Vec<MemorySelection> = ranked_results
        .iter()
        .enumerate()
        .map(|(idx, (content, score))| {
            let preview = if content.len() > 100 {
                format!("{}...", &content[..content.floor_char_boundary(100)])
            } else {
                content.clone()
            };
            MemorySelection {
                memory_id: format!("mem-{}", idx),
                memory_type: "semantic".to_string(),
                content_preview: preview,
                relevance_score: *score,
                tokens: (content.len() / 4) as u32, // Rough estimate
                source: MemorySource::Session,
            }
        })
        .collect();

    let total_tokens: u32 = memories_selected.iter().map(|m| m.tokens).sum();

    MemoryRetrievalTrace {
        query: query.to_string(),
        candidates_considered: candidates_count,
        memories_selected,
        memories_rejected: Vec::new(), // Would need scoring internals
        total_tokens,
        retrieval_latency_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_trace_builder() {
        let trace = ContextAssemblyTraceBuilder::new("turn-1", "session-abc")
            .with_token_budget(TokenBudgetTrace {
                max_tokens: 100000,
                total_used: 45000,
                budget_pressure: 0.45,
                ..Default::default()
            })
            .build();

        assert_eq!(trace.turn_id, "turn-1");
        assert_eq!(trace.session_id, "session-abc");
        assert_eq!(trace.token_budget.max_tokens, 100000);
        assert!((trace.token_budget.budget_pressure - 0.45).abs() < 0.001);
    }

    #[test]
    fn test_trace_aggregation() {
        let traces = vec![
            ContextAssemblyTrace {
                system_prompt: SystemPromptBreakdown {
                    total_tokens: 1000,
                    ..Default::default()
                },
                token_budget: TokenBudgetTrace {
                    compression_triggered: true,
                    ..Default::default()
                },
                ..Default::default()
            },
            ContextAssemblyTrace {
                system_prompt: SystemPromptBreakdown {
                    total_tokens: 2000,
                    ..Default::default()
                },
                token_budget: TokenBudgetTrace {
                    compression_triggered: false,
                    ..Default::default()
                },
                ..Default::default()
            },
        ];

        let agg = TraceAggregation::from_traces(&traces);
        assert_eq!(agg.turn_count, 2);
        assert!((agg.avg_system_prompt_tokens - 1500.0).abs() < 0.001);
        assert!((agg.compression_trigger_rate - 0.5).abs() < 0.001);
    }

    #[test]
    fn tool_trace_scores_are_clamped_non_negative() {
        let selected_tools: Vec<String> = (0..16).map(|i| format!("tool-{i}")).collect();
        let trace = build_tool_trace_from_selection(16, &selected_tools, "tfidf", 0.4, &[], 5);
        assert!(trace.tools_selected.iter().all(|tool| tool.score >= 0.0));
    }
}
