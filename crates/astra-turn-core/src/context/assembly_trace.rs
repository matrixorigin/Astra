//! Context Assembly Trace — deep observability into turn context composition.
//!
//! Records exactly what goes into each LLM request:
//! - System prompt breakdown (base, skills, environment, memories)
//! - Conversation history selection (retained, compressed, dropped)
//! - Memory retrieval (queries, candidates, selections)
//! - tool surface (scoring, filtering, final set)
//!
//! This enables answering questions like:
//! - "Why did the agent lose focus?" → compression dropped critical context
//! - "Which memories were effective?" → see relevance scores + selection
//! - "Where are my tokens going?" → detailed breakdown by component

use serde::{Deserialize, Serialize};
use std::time::SystemTime;

use crate::section_types::estimate_text_tokens;

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

    /// tool surface trace.
    pub tools: ToolSurfaceTrace,

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
            tools: ToolSurfaceTrace::default(),
            token_budget: TokenBudgetTrace::default(),
            explanations: Vec::new(),
        }
    }
}

/// Normalize arbitrary memory content into a one-line preview suitable for
/// observability surfaces.
#[must_use]
pub fn normalize_content_preview(content: &str) -> String {
    content
        .replace(['\r', '\n'], " ⏎ ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// First `max_chars` chars of [`normalize_content_preview`].
#[must_use]
pub fn preview_snippet(content: &str, max_chars: usize) -> String {
    let normalized = normalize_content_preview(content);
    let trimmed: String = normalized.chars().take(max_chars).collect();
    if normalized.chars().count() > max_chars {
        format!("{trimmed}…")
    } else {
        trimmed
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
    /// Current-session memory injected through the dedicated pipeline lane.
    #[serde(default)]
    pub session_memory_injected: Option<MemoryInjection>,
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptGuidanceSignals {
    pub parallel_feedback: bool,
    /// Set when the trailing N rounds in conversation history each ran
    /// exactly one tool — strong signal the model is making sequential
    /// single-tool calls that should have been batched.
    pub parallel_batching_nudge: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptContextSignals {
    pub active_output_skills: bool,
    pub memory_signal_detected: bool,
    pub system_prompt_override: bool,
    pub effort_hint: bool,
    pub agent_type_hint: bool,
    pub self_awareness: bool,
    pub implicit_feedback: bool,
    pub learned_feedback_rules: bool,
    #[serde(default)]
    pub memoria_insights: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptTraceSignals {
    #[serde(default)]
    pub context_signals: PromptContextSignals,
    #[serde(default)]
    pub guidance_signals: PromptGuidanceSignals,
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

// ─── Tool Surface Trace ─────────────────────────────────────────────────────

/// Trace of the concrete tool schemas exposed to an LLM call.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolSurfaceTrace {
    /// Tools available before final payload filtering.
    pub tools_available: u32,
    /// Tools included in the LLM-visible surface.
    pub visible_tools: Vec<VisibleTool>,
    /// Deferred tools promoted into this request's visible surface by a prior
    /// activation. This is the raw signal needed for future T2->T1 policy
    /// analysis; it is telemetry only and does not change tool selection.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deferred_active_tools: Vec<String>,
    /// Count of runtime-activatable deferred tools advertised for this turn.
    #[serde(default)]
    pub deferred_available: u32,
    /// Deferred tools omitted from the manifest because the current context
    /// budget could not fit them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deferred_omitted_tools: Vec<String>,
    /// Tool surface assembly latency in milliseconds.
    pub surface_latency_ms: u64,
}

/// A tool schema included in the LLM-visible surface.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VisibleTool {
    pub tool_name: String,
    pub tokens: u32,
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
    /// Tool surface decision.
    ToolSurface { visible_tools: Vec<String> },
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

    pub fn with_tools(mut self, tools: ToolSurfaceTrace) -> Self {
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
    /// Tool surface statistics.
    pub avg_visible_tools: f64,
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
            avg_visible_tools: traces
                .iter()
                .map(|t| t.tools.visible_tools.len() as f64)
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

/// Build [`ToolSurfaceTrace`] from the effective tool surface.
pub fn build_tool_surface_trace(
    tools_available: u32,
    visible_tools: &[String],
    per_tool_costs: &[(String, u32)],
    surface_latency_ms: u64,
) -> ToolSurfaceTrace {
    build_tool_surface_trace_with_deferred(
        tools_available,
        visible_tools,
        per_tool_costs,
        surface_latency_ms,
        &[],
        0,
        &[],
    )
}

/// Build [`ToolSurfaceTrace`] with deferred activation telemetry.
pub fn build_tool_surface_trace_with_deferred(
    tools_available: u32,
    visible_tools: &[String],
    per_tool_costs: &[(String, u32)],
    surface_latency_ms: u64,
    deferred_active_tools: &[String],
    deferred_available: u32,
    deferred_omitted_tools: &[String],
) -> ToolSurfaceTrace {
    let visible_tools: Vec<VisibleTool> = visible_tools
        .iter()
        .map(|name| {
            let tokens = per_tool_costs
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, c)| *c)
                .unwrap_or(0);
            VisibleTool {
                tool_name: name.clone(),
                tokens,
            }
        })
        .collect();

    ToolSurfaceTrace {
        tools_available,
        visible_tools,
        deferred_active_tools: deferred_active_tools.to_vec(),
        deferred_available,
        deferred_omitted_tools: deferred_omitted_tools.to_vec(),
        surface_latency_ms,
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
    let mut seen = std::collections::HashSet::new();
    let memories_selected: Vec<MemorySelection> = ranked_results
        .iter()
        .filter(|(content, _)| !astra_prompts::memory_proto::is_session_namespace_memory(content))
        .filter(|(content, _)| !content.trim().is_empty())
        .filter(|(content, _)| seen.insert(memory_trace_dedup_key(content)))
        .enumerate()
        .map(|(idx, (content, score))| MemorySelection {
            memory_id: format!("mem-{}", idx),
            memory_type: "semantic".to_string(),
            content_preview: preview_snippet(content, 100),
            relevance_score: *score,
            tokens: estimate_text_tokens(content),
            source: MemorySource::Session,
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

fn memory_trace_dedup_key(content: &str) -> String {
    content
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_matches(|c: char| c.is_ascii_punctuation())
        .to_ascii_lowercase()
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
    fn memory_trace_deduplicates_equivalent_selected_memories() {
        let trace = build_memory_trace_from_retrieval(
            "fix task",
            3,
            &[
                ("Use file editing tools directly.".to_string(), 0.9),
                ("  use   file editing tools directly  ".to_string(), 0.8),
                ("Keep task board current.".to_string(), 0.7),
            ],
            12,
        );

        assert_eq!(trace.memories_selected.len(), 2);
        assert_eq!(trace.memories_selected[0].memory_id, "mem-0");
        assert_eq!(trace.memories_selected[1].memory_id, "mem-1");
        assert_eq!(
            trace.total_tokens,
            trace
                .memories_selected
                .iter()
                .map(|m| m.tokens)
                .sum::<u32>()
        );
    }

    #[test]
    fn memory_trace_uses_shared_unicode_token_estimate() {
        let content = "你好世界🚀🔥💻".to_string();
        let trace = build_memory_trace_from_retrieval("memory", 1, &[(content.clone(), 0.9)], 12);

        assert_eq!(trace.memories_selected.len(), 1);
        assert_eq!(
            trace.memories_selected[0].tokens,
            crate::section_types::estimate_text_tokens(&content)
        );
        assert_eq!(trace.total_tokens, trace.memories_selected[0].tokens);
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
    fn tool_surface_trace_preserves_visible_order() {
        let visible_tools: Vec<String> = (0..16).map(|i| format!("tool-{i}")).collect();
        let trace = build_tool_surface_trace(16, &visible_tools, &[], 5);
        assert_eq!(trace.visible_tools.len(), 16);
        assert_eq!(trace.visible_tools[0].tool_name, "tool-0");
        assert!(trace.deferred_active_tools.is_empty());
        assert_eq!(trace.deferred_available, 0);
        assert_eq!(trace.surface_latency_ms, 5);
    }

    #[test]
    fn tool_surface_trace_records_deferred_activation_telemetry() {
        let visible_tools = vec!["bash".to_string(), "web_fetch".to_string()];
        let deferred_active_tools = vec!["web_fetch".to_string()];
        let trace = build_tool_surface_trace_with_deferred(
            2,
            &visible_tools,
            &[("bash".to_string(), 100), ("web_fetch".to_string(), 250)],
            7,
            &deferred_active_tools,
            4,
            &["github".to_string()],
        );

        assert_eq!(trace.deferred_active_tools, vec!["web_fetch".to_string()]);
        assert_eq!(trace.deferred_available, 4);
        assert_eq!(trace.deferred_omitted_tools, vec!["github".to_string()]);
        assert_eq!(trace.visible_tools[1].tokens, 250);
    }

    #[test]
    fn memory_trace_drops_session_namespace_entries() {
        let trace = build_memory_trace_from_retrieval(
            "memory",
            4,
            &[
                ("[@session/active] Session 1 active state".to_string(), 0.9),
                ("[@session/memory] session_id=other body".to_string(), 0.8),
                ("[@pref/active] prefer Rust".to_string(), 0.7),
            ],
            12,
        );

        assert_eq!(trace.memories_selected.len(), 1);
        assert_eq!(
            trace.memories_selected[0].content_preview,
            "[@pref/active] prefer Rust"
        );
        assert_eq!(trace.candidates_considered, 4);
    }

    #[test]
    fn preview_snippet_normalizes_multiline_content() {
        let preview = preview_snippet("# Session Memory\n## Session Title\nAstra", 80);
        assert_eq!(preview, "# Session Memory ⏎ ## Session Title ⏎ Astra");
    }
}
