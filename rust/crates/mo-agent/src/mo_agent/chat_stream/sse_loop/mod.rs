//! SSE multi-turn agentic loop (`stream_chat_sse`).
//!
//! `run.rs` delegates to `agentic_sse_loop::AgenticSseLoopState` (bootstrap, loop, sidecars + `StreamResult`).
//! One iteration is `agentic_loop_turn::run_agentic_loop_iteration` (fetch through headless tool assembly + post-tool policy).
//! Outbound payload prep is `prepare_turn_request` (includes explain stderr + skill-instruction load).

mod agentic_loop_turn;
mod agentic_sse_loop;
mod prepare_turn_request;
mod run;

pub(crate) use run::stream_chat_sse;
