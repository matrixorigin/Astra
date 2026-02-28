"""Streaming API endpoints (deprecated — use /chat/stream instead)."""

import json
from fastapi import APIRouter, Depends
from fastapi.responses import StreamingResponse
from pydantic import BaseModel, Field

from api.database import SessionLocal
from api.dependencies import get_current_user
from api.routers.chat import _build_chat_loop, _ensure_session
from api.sse_errors import SSE_HEADERS
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
    current_user: dict = Depends(get_current_user),
):
    """Deprecated — use /chat/stream from chat router instead."""

    async def event_generator():
        loop = None
        try:
            user_id = current_user["user_id"]
            db = SessionLocal()
            try:
                session_id = _ensure_session(db, user_id, request.session_id, None)
            finally:
                db.close()
            loop = _build_chat_loop(SessionLocal)

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
            yield f"data: {json.dumps({'type': 'error', 'message': str(e), 'code': 'INTERNAL_ERROR', 'retryable': False})}\n\n"
        finally:
            if loop:
                _pipeline = getattr(getattr(loop, 'event_logger', None), '_pipeline', None)
                if _pipeline:
                    _pipeline.shutdown()

    return StreamingResponse(
        event_generator(),
        media_type="text/event-stream",
        headers=SSE_HEADERS,
    )
