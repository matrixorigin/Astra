pub mod agentic_adaptive_tuning;
pub mod harness_adapter;
pub mod agentic_auto_reflection;
pub mod agentic_delegate_interception;
pub mod agentic_headless_round;
pub mod agentic_loop_execution_phase;
pub mod agentic_loop_finalization;
pub mod agentic_loop_host;
pub mod agentic_loop_lifecycle;
pub mod agentic_loop_tool_phase;
pub mod agentic_loop_tool_support;
pub mod agentic_stage_bridge;
pub mod agentic_tool_interception;
pub mod bridge_inprocess;
pub mod bridge_llm_stream;
pub mod bridge_observability;
pub mod bridge_sse_helpers;
pub mod chat_turn_budget_pressure;
pub mod chat_turn_selection_context;
pub mod cloud;
pub mod compaction_replay;
pub mod context_compression;
pub mod headless_tool_pipeline;
/// Re-exported from astra-turn-types
pub mod implicit_feedback {
    pub use astra_turn_types::{
        ImplicitSignal, StructuredFeedback, detect_implicit_feedback_signal,
        implicit_feedback_context_injection, implicit_feedback_rating,
    };
}
pub(crate) mod llm_client;
pub(crate) mod llm_exchange_capture;
pub mod loop_dispatcher;
pub mod memory_prefetch;
pub mod permission_gate;
pub mod prompt_cache;
/// Re-exported from astra-turn-types
pub mod result_quality {
    pub use astra_turn_types::{ResultQuality, classify_result, quality_feedback};
}
pub mod bedrock_eventstream;
pub(crate) mod bedrock_stream;
pub(crate) mod bedrock_transport;
pub(crate) mod services;
pub mod skill_selector;
pub mod skill_tool;
pub mod token_usage;
pub(crate) mod tool_side_effects;
pub mod turn_trace_collector;

#[cfg(feature = "bridge-e2e-hooks")]
pub mod stream_idle_test_hooks {
    pub struct StreamIdleTimeoutGuard {
        inner: Option<super::llm_client::BridgeE2eStreamIdleTimeoutGuard>,
    }

    impl Drop for StreamIdleTimeoutGuard {
        fn drop(&mut self) {
            let _ = self.inner.take();
        }
    }

    pub fn set_stream_idle_timeouts_for_test(pre_ms: u64, post_ms: u64) -> StreamIdleTimeoutGuard {
        StreamIdleTimeoutGuard {
            inner: Some(
                super::llm_client::set_bridge_e2e_stream_idle_timeouts_for_test(pre_ms, post_ms),
            ),
        }
    }

    pub fn current_stream_idle_timeouts_for_test()
    -> (Option<std::time::Duration>, Option<std::time::Duration>) {
        super::llm_client::current_bridge_e2e_stream_idle_timeouts_for_test()
    }
}

// Re-export from astra-turn-core for public API compatibility
pub use astra_turn_core::{
    agentic_prepare_payload, agentic_recursion_guard, agentic_turn_telemetry, boost_domain_hints,
    chat_history_openai, chat_turn_api_error, chat_turn_edge_profile, chat_turn_explain_wire,
    chat_turn_heuristics, chat_turn_payload, chat_turn_sse_dispatch, chat_turn_step_plan,
    cloud_tool_delivery, edge_ledger, edge_prompt_context, parallel_tool_exec,
    prepare_turn_explain_text, sse_stream_host, stop_hooks_yaml, streaming_tool_exec, tool_health,
    tool_schema_prune, turn_guard,
};
