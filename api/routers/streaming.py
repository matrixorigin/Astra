"""Streaming API endpoints."""

import json
from typing import Annotated

from fastapi import APIRouter, Depends, HTTPException
from fastapi.responses import StreamingResponse
from pydantic import BaseModel, Field

from api.dependencies import get_current_user
from api.database import get_db_session
from core.agent.chat_loop import ChatLoop
from core.events.event_logger import EventLogger
from core.llm.client import LLMClient
from core.logging_config import get_logger
from core.skills.selector import SkillSelector
from sdk import Database

logger = get_logger(__name__)
router = APIRouter()


class StreamChatRequest(BaseModel):
    """Request to stream chat response."""

    session_id: str = Field(description="Session ID")
    message: str = Field(description="User message")
    context: dict | None = Field(default=None, description="Optional context")
    max_candidates: int = Field(default=5, description="Max skill candidates")


@router.post("/chat/stream")
async def stream_chat(
    request: StreamChatRequest,
    current_user: Annotated[dict, Depends(get_current_user)],
    db: Annotated[Database, Depends(get_db_session)],
):
    """Stream chat response as Server-Sent Events.

    Returns real-time events as the agent processes the request:
    - run_started: Agent begins processing
    - text_delta: Incremental text chunks
    - tool_call_start/end: Tool execution
    - tool_result: Tool results
    - run_finished: Agent completes

    Example:
        ```bash
        curl -N -H "Authorization: Bearer <token>" \\
             -H "Content-Type: application/json" \\
             -d '{"session_id":"sess_123","message":"Hello"}' \\
             http://localhost:8000/chat/stream
        ```
    """
    user_id = current_user["user_id"]

    # Verify session exists and belongs to user
    session = db.fetchone(
        "SELECT session_id FROM conversation_sessions WHERE session_id = %s",
        (request.session_id,),
    )
    if not session:
        raise HTTPException(status_code=404, detail="Session not found")

    # Initialize components
    event_logger = EventLogger(db)
    llm_client = LLMClient()
    selector = SkillSelector(db)
    chat_loop = ChatLoop(
        llm=llm_client,
        selector=selector,
        event_logger=event_logger,
    )

    async def event_generator():
        """Generate SSE events."""
        try:
            async for stream_event in chat_loop.run_step_stream(
                user_input=request.message,
                session_id=request.session_id,
                user_id=user_id,
                context=request.context,
                max_candidates=request.max_candidates,
            ):
                # Format as SSE
                event_data = {
                    "event_type": stream_event.event_type,
                    "data": stream_event.data,
                    "event_id": stream_event.event_id,
                    "causal_chain_id": stream_event.causal_chain_id,
                    "agent_id": stream_event.agent_id,
                }
                yield f"data: {json.dumps(event_data)}\n\n"

        except Exception as e:
            logger.error(f"Stream error: {e}", exc_info=True)
            error_event = {
                "event_type": "run_error",
                "data": {"error": str(e)},
            }
            yield f"data: {json.dumps(error_event)}\n\n"

    return StreamingResponse(
        event_generator(),
        media_type="text/event-stream",
        headers={
            "Cache-Control": "no-cache",
            "Connection": "keep-alive",
            "X-Accel-Buffering": "no",  # Disable nginx buffering
        },
    )
