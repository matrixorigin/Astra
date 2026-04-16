pub use astra_turn_core::turn_trace_collector::*;

#[cfg(test)]
mod tests {
    use crate::observability_integration::{ObservabilityHub, on_context_assembled};
    use astra_turn_core::turn_trace_collector::TurnTraceCollector;

    #[test]
    fn finalize_feeds_observability_session() {
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
}
