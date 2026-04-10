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
//! collector.record_tool_selection(&selection_result, tools_available, latency_ms);
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
        budget_used: u32,
        selector_tokens_in: u64,
        selector_tokens_out: u64,
        tools_available: u32,
        latency_ms: u64,
    ) {
        let trace = build_tool_trace_from_selection(
            tools_available,
            selected_tools,
            strategy,
            confidence,
            budget_used,
            selector_tokens_in,
            selector_tokens_out,
            latency_ms,
        );
        if let Ok(mut state) = self.inner.write() {
            state.tools = Some(trace);
        }
    }

    /// Record memory retrieval results.
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
    pub fn record_token_budget(&self, budget: TokenBudgetTrace) {
        if let Ok(mut state) = self.inner.write() {
            state.token_budget = Some(budget);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_collector_basic_flow() {
        let collector = TurnTraceCollector::new("turn-1", "session-abc");

        // Record some data
        collector.record_tool_selection(
            &["view".to_string(), "edit".to_string()],
            "tfidf",
            0.85,
            1000,
            0,
            0,
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

        collector1.record_tool_selection(&["view".to_string()], "tfidf", 0.8, 100, 0, 0, 10, 5);

        // Both should see the same data
        assert!(collector2.has_data());
        let trace = collector2.finalize();
        assert_eq!(trace.tools.tools_selected.len(), 1);
    }
}
