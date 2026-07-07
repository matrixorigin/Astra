//! Agentic SSE chat loop — split into submodules for future extraction to `astra-runtime`.
//!
//! Public surface for the crate root: `ChatTurnParams`, `stream_chat_sse`, `edge_executor_instance_id`.
//! The main loop lives under [`sse_loop`] (`mod.rs` entry + `agentic_sse_loop` / `agentic_loop_turn`).

mod edge_executor;
mod explain_reports;
mod params;
pub(crate) mod session_memory_ux;
mod sse_loop;

#[cfg(test)]
mod tests;

pub(crate) use edge_executor::edge_executor_instance_id;
#[cfg(test)]
pub(crate) use params::AskUserChoice;
pub(crate) use params::BasicCliChatContext;
pub(crate) use params::ChatTurnParams;
pub(crate) use params::DEFAULT_TURN_INDEX;
pub(crate) use params::{
    ApprovalRequest, ApprovalRequestTx, ApprovalResponse, AskUserAnnotation, AskUserAnswers,
    AskUserPrompt, AskUserQuestion, AskUserQuestionAnswer, AskUserRequest, AskUserRequestTx,
    AskUserResponse, PlanReviewDecision, PlanReviewRequest, PlanReviewRequestTx,
    SharedStreamEventSink, StreamEvent, StreamEventSink, StreamEventTx, ToolProgressSink,
};
pub(crate) use sse_loop::stream_chat_sse;
pub(crate) use sse_loop::turn_policy_from_payload_edge_tools;
