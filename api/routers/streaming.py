"""Streaming API endpoints (deprecated — use /chat/stream instead)."""

import json
from typing import Annotated

from fastapi import APIRouter, Depends
from fastapi.responses import StreamingResponse
from pydantic import BaseModel, Field
from sqlalchemy.orm import Session

from api.database import get_db_session
from api.dependencies import get_current_user
from api.routers.chat import _build_chat_loop, _ensure_session
from core.logging_config import get_logger

logger = get_logger(__name__)
router = APIRouter()


class StreamChatRequest(BaseModel):
    """Request to stream chat response."""

    session_id: str = Field(description="Session ID")
    message: str = Field(description="User message")
    context: dict | None = Field(default=None, description="Optional context")
    max_candidates: int = Field(default=5, description="Max skill candidates")


@router.post("/streaming/chat", deprecated=True, include_in_schema=False)
async def stream_chat(
    request: StreamChatRequest,
    current_user: Annotated[dict, Depends(get_current_user)],
    db: Annotated[Session, Depends(get_db_session)],
):
    """Deprecated — use /chat/stream from chat router instead."""
    user_id = current_user["user_id"]
    session_id = _ensure_session(db, user_id, request.session_id, None)
    loop = _build_chat_loop(db)

    async def event_generator():
        try:
            async for stream_event in loop.run_step_stream(
                user_input=request.message,
                session_id=session_id,
                user_id=user_id,
                context=request.context,
                max_candidates=request.max_candidates,
            ):
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
            yield f"data: {json.dumps({'event_type': 'run_error', 'data': {'error': str(e)}})}\n\n"
        finally:
            _pipeline = getattr(getattr(loop, 'event_logger', None), '_pipeline', None)
            if _pipeline:
                _pipeline.shutdown()

    return StreamingResponse(
        event_generator(),
        media_type="text/event-stream",
        headers={
            "Cache-Control": "no-cache",
            "Connection": "keep-alive",
            "X-Accel-Buffering": "no",
        },
    )
