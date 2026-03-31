//! SSE multi-turn agentic loop (`stream_chat_sse`).
//!
//! Implementation lives in `run`; `prepare_turn_request` holds one iteration’s outbound payload assembly.

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
