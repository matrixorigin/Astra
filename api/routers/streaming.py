"""Streaming API endpoints."""

import json
from typing import Annotated

from fastapi import APIRouter, Depends, HTTPException
from fastapi.responses import StreamingResponse
from pydantic import BaseModel, Field
from sqlalchemy.orm import Session

from api.dependencies import get_current_user
from api.database import get_db_session
from core.agent.chat_loop import ChatLoop
from core.agent.executor import AgentExecutor
from core.context.manager import ContextManager
from core.events.event_logger import EventLogger
from core.verification.firewall import HallucinationFirewall
from core.llm.client import LLMClient
from core.skills.pipeline import SkillPipeline
from core.logging_config import get_logger

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
    db: Annotated[Session, Depends(get_db_session)],
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
    from sqlalchemy import text
    result = db.execute(
        text("SELECT session_id FROM sessions WHERE session_id = :session_id"),
        {"session_id": request.session_id},
    )
    session = result.first()
    if not session:
        raise HTTPException(status_code=404, detail="Session not found")

    # Initialize components with injected db session
    from core.skills.registry import SkillRegistry
    from core.skills.builtin import register_builtin_skills
    from core.runtime import create_runtime, IsolationLevel
    from core.code_executor import CodeExecutor
    
    event_logger = EventLogger(db)
    llm_client = LLMClient()
    skill_registry = SkillRegistry(db)
    code_executor = CodeExecutor(
        runtime=create_runtime(min_isolation=IsolationLevel.PROCESS),
        db=db,
    )
    register_builtin_skills(skill_registry, db, code_executor=code_executor)
    context_manager = ContextManager(db)
    selector = SkillPipeline(db, llm_client, audit=True, learning=True)
    executor = AgentExecutor(db, skill_registry)
    firewall = HallucinationFirewall(db, context_manager)
    
    chat_loop = ChatLoop(
        selector=selector,
        executor=executor,
        llm_client=llm_client,
        event_logger=event_logger,
        context_manager=context_manager,
        firewall=firewall,
    )

    # Observational memory
    from core.memory.observer import Observer
    chat_loop.set_observer(Observer(db, llm_client=llm_client))

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
