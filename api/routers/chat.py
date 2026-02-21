"""Chat API endpoints — unified conversation entry point."""

import json
from typing import Annotated, Optional

from fastapi import APIRouter, Depends, HTTPException
from fastapi.responses import StreamingResponse
from pydantic import BaseModel, Field
from sqlalchemy.orm import Session
from sqlalchemy import text

from api.dependencies import get_current_user
from api.database import get_db_session
from core.logging_config import get_logger

logger = get_logger(__name__)
router = APIRouter()


# ---------------------------------------------------------------------------
# Request / Response models
# ---------------------------------------------------------------------------

class ChatRequest(BaseModel):
    """Chat request — session_id optional (auto-created if omitted)."""
    message: str = Field(description="User message")
    session_id: Optional[str] = Field(default=None, description="Session ID (auto-created if omitted)")
    agent_id: Optional[str] = Field(default=None, description="Agent ID")
    context: Optional[dict] = Field(default=None, description="Optional context")
    max_candidates: int = Field(default=5, description="Max skill candidates")


class ChatResponse(BaseModel):
    """Non-streaming chat response."""
    session_id: str
    message: str
    event_id: Optional[str] = None


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _ensure_session(db: Session, user_id: str, session_id: Optional[str], agent_id: Optional[str]) -> str:
    """Return existing session_id or create a new one."""
    if session_id:
        row = db.execute(
            text("SELECT session_id FROM sessions WHERE session_id = :sid"),
            {"sid": session_id},
        ).first()
        if not row:
            raise HTTPException(status_code=404, detail="Session not found")
        return session_id

    # Auto-create session
    from core.events.session_manager import SessionManager
    mgr = SessionManager(db)
    session = mgr.create_session(user_id=user_id, metadata={"agent_id": agent_id} if agent_id else None)
    return session.session_id


def _build_chat_loop(db: Session):
    """Build ChatLoop with all dependencies."""
    from core.agent.chat_loop import ChatLoop
    from core.agent.executor import AgentExecutor
    from core.context.manager import ContextManager
    from core.events.event_logger import EventLogger
    from core.verification.firewall import HallucinationFirewall
    from core.llm.client import LLMClient
    from core.skills.pipeline import SkillPipeline
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

    loop = ChatLoop(
        selector=selector,
        executor=executor,
        llm_client=llm_client,
        event_logger=event_logger,
        context_manager=context_manager,
        firewall=firewall,
    )

    from core.memory.observer import Observer
    loop.set_observer(Observer(db, llm_client=llm_client))

    return loop


# ---------------------------------------------------------------------------
# Endpoints
# ---------------------------------------------------------------------------

@router.post("/chat", response_model=ChatResponse)
async def chat(
    request: ChatRequest,
    current_user: Annotated[dict, Depends(get_current_user)],
    db: Annotated[Session, Depends(get_db_session)],
):
    """Non-streaming chat — send a message, get a response.

    If session_id is omitted, a new session is created automatically.

    Example:
        ```bash
        curl -H "Authorization: Bearer <token>" \\
             -H "Content-Type: application/json" \\
             -d '{"message":"Hello"}' \\
             http://localhost:8000/chat
        ```
    """
    user_id = current_user["user_id"]
    session_id = _ensure_session(db, user_id, request.session_id, request.agent_id)
    loop = _build_chat_loop(db)

    response_text = await loop.run_step(
        user_input=request.message,
        session_id=session_id,
        user_id=user_id,
        context=request.context,
        max_candidates=request.max_candidates,
    )

    return ChatResponse(session_id=session_id, message=response_text)


@router.post("/chat/stream")
async def chat_stream(
    request: ChatRequest,
    current_user: Annotated[dict, Depends(get_current_user)],
    db: Annotated[Session, Depends(get_db_session)],
):
    """Stream chat response as Server-Sent Events.

    If session_id is omitted, a new session is created automatically.
    The first event always contains the session_id.

    Example:
        ```bash
        curl -N -H "Authorization: Bearer <token>" \\
             -H "Content-Type: application/json" \\
             -d '{"message":"Hello"}' \\
             http://localhost:8000/chat/stream
        ```
    """
    user_id = current_user["user_id"]
    session_id = _ensure_session(db, user_id, request.session_id, request.agent_id)
    loop = _build_chat_loop(db)

    async def event_generator():
        # First event: session info (so client knows the session_id)
        yield f"data: {json.dumps({'event_type': 'session_info', 'data': {'session_id': session_id}})}\n\n"

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

    return StreamingResponse(
        event_generator(),
        media_type="text/event-stream",
        headers={
            "Cache-Control": "no-cache",
            "Connection": "keep-alive",
            "X-Accel-Buffering": "no",
        },
    )
