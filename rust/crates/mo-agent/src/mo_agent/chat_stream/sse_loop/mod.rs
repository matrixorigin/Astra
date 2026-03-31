//! SSE multi-turn agentic loop (`stream_chat_sse`).
//!
//! `run.rs` delegates to `agentic_sse_loop::AgenticSseLoopState` (bootstrap, loop, sidecars + `StreamResult`).
//! One iteration is `agentic_loop_turn::run_agentic_loop_iteration` (fetch through post-tool policy + `tool_round`).
//! Outbound payload prep is `prepare_turn_request`.

mod agentic_loop_turn;
mod agentic_sse_loop;
mod explain_sidecar;
mod prepare_turn_request;
mod run;
mod skill_instructions_round;
mod tool_round;

pub(crate) use run::stream_chat_sse;
