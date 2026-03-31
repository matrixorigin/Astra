//! SSE multi-turn agentic loop (`stream_chat_sse`).
//!
//! Entry [`stream_chat_sse`] builds `agentic_sse_loop::AgenticSseLoopState`, runs `run_all_turns`, then `into_stream_result`.
//! One iteration is `agentic_loop_turn::run_agentic_loop_iteration` (payload + fetch + ingest + stall + tool round + post-tool).

mod agentic_loop_turn;
mod agentic_sse_loop;

use crate::StreamResult;

use super::ChatTurnParams;
use agentic_sse_loop::AgenticSseLoopState;

pub(crate) async fn stream_chat_sse(mut p: ChatTurnParams<'_>) -> Result<StreamResult, String> {
    let mut state = AgenticSseLoopState::new(&p);
    state.run_all_turns(&mut p).await?;
    Ok(state.into_stream_result(&p))
}
