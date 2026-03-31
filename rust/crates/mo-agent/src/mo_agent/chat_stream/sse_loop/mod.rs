//! SSE multi-turn agentic loop (`stream_chat_sse`).
//!
//! Implementation lives in [`run`] so this directory can grow with further splits without a single huge file.

mod run;

pub(crate) use run::stream_chat_sse;
