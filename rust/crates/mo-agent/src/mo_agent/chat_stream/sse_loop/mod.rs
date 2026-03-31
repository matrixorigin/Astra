//! SSE multi-turn agentic loop (`stream_chat_sse`).
//!
//! Implementation lives in `run`; `prepare_turn_request` holds one iteration’s outbound payload assembly.

mod explain_sidecar;
mod prepare_turn_request;
mod run;
mod skill_instructions_round;
mod tool_round;

pub(crate) use run::stream_chat_sse;
