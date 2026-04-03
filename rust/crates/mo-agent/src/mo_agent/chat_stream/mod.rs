//! Agentic SSE chat loop — split into submodules for future extraction to `mo-agent-runtime`.
//!
//! Public surface for the crate root: `ChatTurnParams`, `stream_chat_sse`, `edge_executor_instance_id`.
//! The main loop lives under [`sse_loop`] (`mod.rs` entry + `agentic_sse_loop` / `agentic_loop_turn`).

mod edge_executor;
mod explain_reports;
mod params;
mod sse_loop;

#[cfg(test)]
mod tests;

pub(crate) use edge_executor::edge_executor_instance_id;
pub(crate) use params::ChatTurnParams;
pub(crate) use sse_loop::{chat_turn_timing_stderr_enabled, stream_chat_sse};
