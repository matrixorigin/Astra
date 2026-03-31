//! SSE multi-turn agentic loop (`stream_chat_sse`).
//!
//! `run.rs` delegates to `agentic_sse_loop::AgenticSseLoopState` (bootstrap, `run_all_turns`, `into_stream_result`);
//! per-phase helpers live in sibling modules (`fetch_chat_turn_sse`, `tool_round`, …).

mod agentic_loop_turn;
mod agentic_sse_loop;
mod explain_sidecar;
mod fetch_chat_turn_sse;
mod post_tool_round;
mod prepare_turn_request;
mod run;
mod skill_instructions_round;
mod stall_preflight;
mod stream_result_finalize;
mod tool_round;
mod turn_result_ingest;

pub(crate) use run::stream_chat_sse;
