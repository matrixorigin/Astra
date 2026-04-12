//! Turn Trace Collector — centralized trace assembly and storage.
//!
//! Provides a thread-safe collector that accumulates trace data throughout
//! a turn, then finalizes it for storage in the session journal.
//!
//! # Usage
//!
//! ```ignore
//! let collector = TurnTraceCollector::new("turn-1", "session-abc");
//!
//! // During context assembly:
//! collector.record_tool_selection(&selected_tools, strategy, confidence, &per_tool_costs, tools_available, latency_ms);
//! collector.record_memory_retrieval(&query, candidates, &ranked_results, latency_ms);
//! collector.record_compression(&pipeline_outcome, initial_msgs, final_msgs, ...);
//!
//! // At turn end:
//! let trace = collector.finalize();
//! journal.write_context_trace(&trace).await?;
//! ```

use std::sync::{Arc, RwLock};
use std::time::Instant;

use super::context_assembly_trace::{
    CompressionMethod, ContextAssemblyTrace, ContextAssemblyTraceBuilder, DecisionExplanation,
    HistorySelectionTrace, MemoryRetrievalTrace, SystemPromptBreakdown, TokenBudgetTrace,
    ToolSelectionTrace, build_history_trace_from_compression, build_memory_trace_from_retrieval,
    build_tool_trace_from_selection,
};

/// Thread-safe trace collector for a single turn.
///
/// Accumulates trace data from various context assembly stages and provides
/// a finalized trace for persistence.
#[derive(Debug)]
pub struct TurnTraceCollector {
    inner: Arc<RwLock<CollectorState>>,
}

#[derive(Debug)]
struct CollectorState {
    turn_id: String,
    session_id: String,
    started_at: Instant,
    system_prompt: Option<SystemPromptBreakdown>,
    history: Option<HistorySelectionTrace>,
    memory: Option<MemoryRetrievalTrace>,
    tools: Option<ToolSelectionTrace>,
    token_budget: Option<TokenBudgetTrace>,
    explanations: Vec<DecisionExplanation>,
}

impl TurnTraceCollector {
    /// Create a new collector for a turn.
    pub fn new(turn_id: impl Into<String>, session_id: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(RwLock::new(CollectorState {
                turn_id: turn_id.into(),
                session_id: session_id.into(),
                started_at: Instant::now(),
                system_prompt: None,
                history: None,
                memory: None,
                tools: None,
                token_budget: None,
                explanations: Vec::new(),
            })),
        }
    }

    /// Clone with Arc sharing (cheap).
    pub fn clone_arc(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }

    pub fn set_session_id(&self, session_id: impl Into<String>) {
        if let Ok(mut state) = self.inner.write() {
            let session_id = session_id.into();
            if !session_id.is_empty() {
                state.session_id = session_id;
            }
        }
    }

    // ─── Recording Methods ───────────────────────────────────────────────────

    /// Record system prompt breakdown.
    pub fn record_system_prompt(&self, breakdown: SystemPromptBreakdown) {
        if let Ok(mut state) = self.inner.write() {
            state.system_prompt = Some(breakdown);
        }
    }

    /// Record tool selection results.
    pub fn record_tool_selection(
        &self,
        selected_tools: &[String],
        strategy: &str,
        confidence: f64,
        per_tool_costs: &[(String, u32)],
        tools_available: u32,
        latency_ms: u64,
    ) {
        let trace = build_tool_trace_from_selection(
            tools_available,
            selected_tools,
            strategy,
            confidence,
            per_tool_costs,
            latency_ms,
        );
        if let Ok(mut state) = self.inner.write() {
            state.tools = Some(trace);
        }
    }

    /// Record memory retrieval results.
    /// Set per-turn history retention data.
    pub fn set_history_retained(&self, turns: &[super::context_assembly_trace::TurnRetention]) {
        if let Ok(mut state) = self.inner.write() {
            let had_history = state.history.is_some();
            let hist = state
                .history
                .get_or_insert_with(HistorySelectionTrace::default);
            hist.turns_retained = turns.to_vec();
            hist.total_turns_available = turns.len() as u32;
            let total: u32 = turns.iter().map(|t| t.tokens).sum();
            hist.tokens_after = total;
            // If record_compression was never called (no prior history trace),
            // default tokens_before to tokens_after (no compression occurred).
            if !had_history {
                hist.tokens_before = total;
            }
        }
    }

    pub fn record_memory_retrieval(
        &self,
        query: &str,
        candidates_count: u32,
        ranked_results: &[(String, f64)],
        latency_ms: u64,
    ) {
        let trace =
            build_memory_trace_from_retrieval(query, candidates_count, ranked_results, latency_ms);
        if let Ok(mut state) = self.inner.write() {
            state.memory = Some(trace);
        }
    }

    /// Record compression pipeline results.
    pub fn record_compression(
        &self,
        layer_results: &[(String, CompressionMethod, u32)],
        initial_messages: usize,
        final_messages: usize,
        initial_tokens: u32,
        final_tokens: u32,
    ) {
        let trace = build_history_trace_from_compression(
            initial_messages,
            final_messages,
            initial_tokens,
            final_tokens,
            layer_results,
        );
        if let Ok(mut state) = self.inner.write() {
            state.history = Some(trace);
        }
    }

    /// Record token budget allocation.
    ///
    /// Merges with any existing estimate: preserves non-zero component breakdowns
    /// from `record_token_budget_estimate` while updating measured totals.
    pub fn record_token_budget(&self, budget: TokenBudgetTrace) {
        if let Ok(mut state) = self.inner.write() {
            if let Some(ref mut existing) = state.token_budget {
                // Preserve CLI-estimated component breakdown; overwrite measured fields.
                existing.max_tokens = budget.max_tokens;
                existing.budget_pressure = budget.budget_pressure;
                existing.compression_triggered = budget.compression_triggered;
                // Only overwrite component fields if the new budget has non-zero values.
                if budget.system_prompt_tokens > 0 {
                    existing.system_prompt_tokens = budget.system_prompt_tokens;
                }
                if budget.history_tokens > 0 {
                    existing.history_tokens = budget.history_tokens;
                }
                if budget.memory_tokens > 0 {
                    existing.memory_tokens = budget.memory_tokens;
                }
                if budget.tool_schema_tokens > 0 {
                    existing.tool_schema_tokens = budget.tool_schema_tokens;
                }
                if budget.user_message_tokens > 0 {
                    existing.user_message_tokens = budget.user_message_tokens;
                }
                let component_total = budget_component_total(existing);
                existing.total_used = if component_total > 0 {
                    component_total
                } else {
                    budget.total_used
                };
            } else {
                let mut budget = budget;
                let component_total = budget_component_total(&budget);
                if component_total > 0 {
                    budget.total_used = component_total;
                }
                state.token_budget = Some(budget);
            }
        }
    }

    /// Record pre-estimated token breakdown from context assembly.
    ///
    /// Unlike `record_token_budget`, this only sets the estimated component values
    /// (system_prompt, history, tool_schema, user_message) without overwriting
    /// actual measured values that runtime will set later.
    pub fn record_token_budget_estimate(
        &self,
        system_prompt_tokens: u32,
        history_tokens: u32,
        memory_tokens: u32,
        tool_schema_tokens: u32,
        user_message_tokens: u32,
        estimated_total: u32,
        max_tokens: u32,
        budget_pressure: f64,
    ) {
        if let Ok(mut state) = self.inner.write() {
            let budget = state
                .token_budget
                .get_or_insert_with(TokenBudgetTrace::default);
            budget.system_prompt_tokens = system_prompt_tokens;
            budget.history_tokens = history_tokens;
            budget.memory_tokens = memory_tokens;
            budget.tool_schema_tokens = tool_schema_tokens;
            budget.user_message_tokens = user_message_tokens;
            // Set estimated total (runtime will overwrite with actual measured value later)
            if budget.total_used == 0 {
                budget.total_used = estimated_total;
            }
            if budget.max_tokens == 0 {
                budget.max_tokens = max_tokens;
            }
            if budget.budget_pressure == 0.0 {
                budget.budget_pressure = budget_pressure;
            }
        }
    }

    /// Update just the system_prompt_tokens field without clobbering other budget fields.
    pub fn set_system_prompt_tokens(&self, tokens: u32) {
        if let Ok(mut state) = self.inner.write() {
            let budget = state
                .token_budget
                .get_or_insert_with(TokenBudgetTrace::default);
            budget.system_prompt_tokens = tokens;
        }
    }

    /// Add a decision explanation.
    pub fn add_explanation(&self, explanation: DecisionExplanation) {
        if let Ok(mut state) = self.inner.write() {
            state.explanations.push(explanation);
        }
    }

    // ─── Finalization ────────────────────────────────────────────────────────

    /// Finalize and return the complete trace.
    pub fn finalize(&self) -> ContextAssemblyTrace {
        let state = self.inner.read().expect("lock poisoned");

        let mut builder = ContextAssemblyTraceBuilder::new(&state.turn_id, &state.session_id);

        if let Some(ref sp) = state.system_prompt {
            builder = builder.with_system_prompt(sp.clone());
        }
        if let Some(ref h) = state.history {
            builder = builder.with_history(h.clone());
        }
        if let Some(ref m) = state.memory {
            builder = builder.with_memory(m.clone());
        }
        if let Some(ref t) = state.tools {
            builder = builder.with_tools(t.clone());
        }
        if let Some(ref tb) = state.token_budget {
            builder = builder.with_token_budget(tb.clone());
        }
        for exp in &state.explanations {
            builder = builder.add_explanation(exp.clone());
        }

        builder.build()
    }

    /// Finalize and persist the trace to the session journal.
    ///
    /// Returns the finalized trace if persistence succeeded, or an error.
    /// This is a convenience method that combines `finalize()` with journal write.
    pub fn finalize_and_persist(
        &self,
        turn_number: u32,
    ) -> Result<ContextAssemblyTrace, std::io::Error> {
        use astra_services::session_journal::{JournalEvent, JournalWriter};

        let trace = self.finalize();

        // Only persist if we have meaningful data
        if !self.has_data() {
            return Ok(trace);
        }

        let state = self.inner.read().expect("lock poisoned");
        let writer = JournalWriter::new(&state.session_id)?;
        let event = JournalEvent::context_assembly_recorded(
            Some(&state.session_id),
            turn_number,
            trace.to_json_value(),
        );
        writer.append(&event)?;

        Ok(trace)
    }

    /// Check if any data has been recorded.
    pub fn has_data(&self) -> bool {
        let state = self.inner.read().expect("lock poisoned");
        state.system_prompt.is_some()
            || state.history.is_some()
            || state.memory.is_some()
            || state.tools.is_some()
            || state.token_budget.is_some()
    }

    /// Check if tool selection trace has already been recorded.
    pub fn has_tool_trace(&self) -> bool {
        self.inner.read().expect("lock poisoned").tools.is_some()
    }

    /// Get elapsed time since collector creation.
    pub fn elapsed_ms(&self) -> u64 {
        let state = self.inner.read().expect("lock poisoned");
        state.started_at.elapsed().as_millis() as u64
    }
}

impl Clone for TurnTraceCollector {
    fn clone(&self) -> Self {
        self.clone_arc()
    }
}

fn budget_component_total(budget: &TokenBudgetTrace) -> u32 {
    budget.system_prompt_tokens
        + budget.history_tokens
        + budget.memory_tokens
        + budget.tool_schema_tokens
        + budget.user_message_tokens
}

#[cfg(test)]
mod tests {
    use super::super::context_assembly_trace::{MemoryInjection, SkillInjection};
    use super::*;

    #[test]
    fn test_collector_basic_flow() {
        let collector = TurnTraceCollector::new("turn-1", "session-abc");

        // Record some data
        collector.record_tool_selection(
            &["view".to_string(), "edit".to_string()],
            "tfidf",
            0.85,
            &[("view".into(), 500), ("edit".into(), 500)],
            50,
            15,
        );

        collector.record_memory_retrieval(
            "how to use rust",
            10,
            &[("Use cargo build".to_string(), 0.9)],
            5,
        );

        assert!(collector.has_data());

        let trace = collector.finalize();
        assert_eq!(trace.turn_id, "turn-1");
        assert_eq!(trace.session_id, "session-abc");
        assert_eq!(trace.tools.tools_selected.len(), 2);
        assert_eq!(trace.memory.memories_selected.len(), 1);
    }

    #[test]
    fn test_collector_clone_shares_state() {
        let collector1 = TurnTraceCollector::new("turn-1", "session-abc");
        let collector2 = collector1.clone_arc();

        collector1.record_tool_selection(&["view".to_string()], "tfidf", 0.8, &[("view".into(), 100)], 10, 5);

        // Both should see the same data
        assert!(collector2.has_data());
        let trace = collector2.finalize();
        assert_eq!(trace.tools.tools_selected.len(), 1);
    }

    #[test]
    fn finalize_feeds_observability_session() {
        use crate::observability_integration::{ObservabilityHub, on_context_assembled};

        let collector = TurnTraceCollector::new("turn-0", "sess-1");
        collector
            .record_token_budget_estimate(14_000, 5_000, 500, 3_000, 200, 22_700, 128_000, 0.18);
        collector.record_tool_selection(
            &["bash".into(), "view".into()],
            "tfidf",
            0.85,
            &[("bash".into(), 1500), ("view".into(), 1500)],
            40,
            12,
        );
        collector.record_memory_retrieval(
            "rust error handling",
            5,
            &[("use thiserror".into(), 0.9)],
            25,
        );

        let trace = collector.finalize();
        assert_eq!(trace.token_budget.system_prompt_tokens, 14_000);
        assert_eq!(trace.token_budget.history_tokens, 5_000);
        assert_eq!(trace.tools.tools_selected.len(), 2);
        assert_eq!(trace.memory.memories_selected.len(), 1);

        // Feed to observability session
        let hub = ObservabilityHub::new();
        let session = hub.start_session("u1", "sess-1");
        {
            let mut guard = session.write().unwrap();
            on_context_assembled(&mut guard, trace);
        }
        let guard = session.read().unwrap();
        assert_eq!(guard.context_traces.len(), 1);
        let t = &guard.context_traces[0];
        assert_eq!(t.turn_id, "turn-0");
        assert_eq!(t.token_budget.system_prompt_tokens, 14_000);
        assert_eq!(t.tools.selection_strategy, "tfidf");
        assert_eq!(t.memory.retrieval_latency_ms, 25);
    }

    #[test]
    fn record_token_budget_keeps_component_totals_consistent() {
        let collector = TurnTraceCollector::new("turn-0", "s1");
        // First: CLI records component estimates
        collector.record_token_budget_estimate(14_000, 5_000, 0, 3_000, 200, 22_200, 128_000, 0.17);
        // Then: runtime overwrites measured totals (with zero component fields)
        collector.record_token_budget(TokenBudgetTrace {
            max_tokens: 128_000,
            total_used: 25_000,
            budget_pressure: 0.20,
            compression_triggered: true,
            ..Default::default()
        });

        let trace = collector.finalize();
        // Total stays aligned with the persisted component breakdown.
        assert_eq!(trace.token_budget.total_used, 22_200);
        assert_eq!(trace.token_budget.budget_pressure, 0.20);
        assert!(trace.token_budget.compression_triggered);
        // Component estimates preserved (runtime sent zeros)
        assert_eq!(trace.token_budget.system_prompt_tokens, 14_000);
        assert_eq!(trace.token_budget.history_tokens, 5_000);
        assert_eq!(trace.token_budget.tool_schema_tokens, 3_000);
        assert_eq!(trace.token_budget.user_message_tokens, 200);
    }

    #[test]
    fn record_system_prompt_breakdown_persists_in_trace() {
        let collector = TurnTraceCollector::new("turn-0", "sess-1");
        let breakdown = SystemPromptBreakdown {
            base_persona_tokens: 8000,
            environment_tokens: 300,
            user_preferences_tokens: 100,
            skills_injected: vec![SkillInjection {
                skill_name: "review".into(),
                skill_version: Some("1.0".into()),
                tokens: 500,
                selection_reason: "user_invoked".into(),
            }],
            repository_memories: vec![MemoryInjection {
                memory_id: "m-42".into(),
                memory_type: "hybrid".into(),
                tokens: 200,
                relevance_score: 0.85,
                content_preview: "prefers concise code".into(),
            }],
            total_tokens: 9100,
        };
        collector.record_system_prompt(breakdown);
        let trace = collector.finalize();
        let sp = &trace.system_prompt;
        assert_eq!(sp.total_tokens, 9100);
        assert_eq!(sp.base_persona_tokens, 8000);
        assert_eq!(sp.skills_injected.len(), 1);
        assert_eq!(sp.skills_injected[0].skill_name, "review");
        assert_eq!(sp.repository_memories.len(), 1);
        assert_eq!(sp.repository_memories[0].memory_id, "m-42");
    }

    #[test]
    fn record_tool_selection_applies_per_tool_costs() {
        let collector = TurnTraceCollector::new("turn-0", "s1");
        collector.record_tool_selection(
            &["bash".into(), "grep".into()],
            "tfidf",
            0.8,
            &[("bash".into(), 350), ("grep".into(), 280)],
            10,
            5,
        );
        let trace = collector.finalize();
        assert_eq!(trace.tools.tools_selected[0].tool_name, "bash");
        assert_eq!(trace.tools.tools_selected[0].tokens, 350);
        assert_eq!(trace.tools.tools_selected[1].tool_name, "grep");
        assert_eq!(trace.tools.tools_selected[1].tokens, 280);
    }

    #[test]
    fn set_history_retained_populates_trace() {
        let collector = TurnTraceCollector::new("turn-0", "s1");
        let turns = vec![
            super::super::context_assembly_trace::TurnRetention {
                turn_index: 0,
                role: "user".into(),
                tokens: 50,
                has_tool_calls: false,
            },
            super::super::context_assembly_trace::TurnRetention {
                turn_index: 1,
                role: "assistant".into(),
                tokens: 800,
                has_tool_calls: true,
            },
        ];
        collector.set_history_retained(&turns);
        let trace = collector.finalize();
        assert_eq!(trace.history.turns_retained.len(), 2);
        assert_eq!(trace.history.turns_retained[0].tokens, 50);
        assert_eq!(trace.history.turns_retained[1].tokens, 800);
        assert!(trace.history.turns_retained[1].has_tool_calls);
        assert_eq!(trace.history.total_turns_available, 2);
        assert_eq!(trace.history.tokens_after, 850);
        // When record_compression was not called, tokens_before defaults to tokens_after.
        assert_eq!(trace.history.tokens_before, 850);
    }

    #[test]
    fn set_session_id_updates_finalized_trace() {
        let collector = TurnTraceCollector::new("turn-0", "");
        collector.set_session_id("sess-1");
        let trace = collector.finalize();
        assert_eq!(trace.session_id, "sess-1");
    }
}
