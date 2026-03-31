//! Multi-turn `/chat/stream` loop (`stream_chat_sse`): thin entry — state lives in [`super::agentic_sse_loop::AgenticSseLoopState`].

use crate::StreamResult;

use super::super::ChatTurnParams;
use super::agentic_sse_loop::AgenticSseLoopState;

pub(crate) async fn stream_chat_sse(mut p: ChatTurnParams<'_>) -> Result<StreamResult, String> {
    let mut state = AgenticSseLoopState::new(&p);
    state.run_all_turns(&mut p).await?;
    Ok(state.into_stream_result(&p))
}
