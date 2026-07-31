pub mod agentic;
pub mod agentic_loop;
pub mod bedrock;
pub mod bridge;
pub mod budget_messaging;
pub mod chat_turn_budget_pressure;
pub mod cloud;
pub mod compaction_replay;
pub(crate) mod deferred_tools_edge_profile;
/// Re-export compaction engine types and helpers for convenience.
pub use cloud::compaction_engine::{CompactionEngine, PipelineOutcome, TokenBudget};
pub(crate) mod context_pipeline_adapter;
pub mod harness_adapter;
pub mod headless_tool_pipeline;
pub mod inspection_service;
pub(crate) mod llm;
pub mod local_provider;
pub mod loop_dispatcher;
pub mod memory_prefetch;
pub mod observation_dispatcher;
pub mod observation_store;
pub mod permission_gate;
pub(crate) mod plan_mode_guard;
pub mod prompt_cache;
pub mod providers;
pub mod run_control;
pub mod runtime_policy;
pub mod session_current_date;
pub mod session_end_debounce;
/// Re-exported from astra-turn-types
pub mod result_quality {
    pub use astra_turn_types::{ResultQuality, classify_result, quality_feedback};
}
pub(crate) mod services;
pub mod skill_tool;
pub mod terminal_control;
pub mod token_usage;
pub(crate) mod tool_completion;
pub mod tool_side_effects;
pub mod tuning_consumer;
pub mod turn_trace_collector;
pub(crate) mod wire_assembly;

#[cfg(feature = "bridge-e2e-hooks")]
pub mod stream_idle_test_hooks {
    pub struct StreamIdleTimeoutGuard {
        inner: Option<super::llm::client::BridgeE2eStreamIdleTimeoutGuard>,
    }

    impl Drop for StreamIdleTimeoutGuard {
        fn drop(&mut self) {
            let _ = self.inner.take();
        }
    }

    pub fn set_stream_idle_timeouts_for_test(pre_ms: u64, post_ms: u64) -> StreamIdleTimeoutGuard {
        StreamIdleTimeoutGuard {
            inner: Some(
                super::llm::client::set_bridge_e2e_stream_idle_timeouts_for_test(pre_ms, post_ms),
            ),
        }
    }

    pub fn current_stream_idle_timeouts_for_test()
    -> (Option<std::time::Duration>, Option<std::time::Duration>) {
        super::llm::client::current_bridge_e2e_stream_idle_timeouts_for_test()
    }
}

// Re-export from astra-turn-core for public API compatibility
pub use astra_turn_core::{
    agentic_prepare_payload, agentic_recursion_guard, agentic_turn_telemetry, chat_history_openai,
    chat_turn_api_error, chat_turn_edge_profile, chat_turn_explain_wire, chat_turn_heuristics,
    chat_turn_payload, chat_turn_sse_dispatch, chat_turn_step_plan, cloud_tool_delivery,
    edge_ledger, edge_prompt_context, parallel_tool_exec, prepare_turn_explain_text,
    sse_stream_host, stop_hooks_yaml, streaming_tool_exec, tool_health, tool_schema_prune,
    turn_guard,
};
