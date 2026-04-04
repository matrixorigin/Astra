//! Agentic SSE chat loop — split into submodules for future extraction to `astra-runtime`.
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
pub(crate) use params::{StreamEvent, StreamEventTx};
pub(crate) use sse_loop::stream_chat_sse;
