"""Chat API endpoints — unified conversation entry point with durable AgentRun."""

import json
from typing import Annotated, Optional

from fastapi import APIRouter, Depends, HTTPException, Query
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
    """Chat response — always returns run_id."""
    session_id: str
    run_id: str
    status: str


class RunStatusResponse(BaseModel):
    run_id: str
    session_id: str
    status: str
    waiting_for: Optional[str] = None
    events_count: int = 0


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


def _get_engine(db: Session):
    from core.agent.run_engine import RunEngine
    return RunEngine(db)


# ---------------------------------------------------------------------------
# Endpoints
# ---------------------------------------------------------------------------

@router.post("/chat", response_model=ChatResponse)
async def chat(
    request: ChatRequest,
    current_user: Annotated[dict, Depends(get_current_user)],
    db: Annotated[Session, Depends(get_db_session)],
):
    """Create an AgentRun. Returns run_id immediately.

    Poll /chat/runs/{run_id} or stream /chat/runs/{run_id}/stream for progress.
    """
    user_id = current_user["user_id"]
    session_id = _ensure_session(db, user_id, request.session_id, request.agent_id)

    engine = _get_engine(db)
    run = engine.create_run(
        session_id=session_id,
        user_id=user_id,
        user_input=request.message,
        context=request.context,
    )

    import asyncio
    asyncio.create_task(engine.start_run(run))

    return ChatResponse(session_id=session_id, run_id=run.run_id, status=run.status.value)


@router.post("/chat/stream")
async def chat_stream(
    request: ChatRequest,
    current_user: Annotated[dict, Depends(get_current_user)],
    db: Annotated[Session, Depends(get_db_session)],
):
    """Stream chat response as SSE. Returns run_id in first event."""
    user_id = current_user["user_id"]
    session_id = _ensure_session(db, user_id, request.session_id, request.agent_id)

    engine = _get_engine(db)
    run = engine.create_run(
        session_id=session_id,
        user_id=user_id,
        user_input=request.message,
        context=request.context,
    )

    import asyncio
    asyncio.create_task(engine.start_run(run))

    async def event_generator():
        yield f"data: {json.dumps({'event_type': 'session_info', 'data': {'session_id': session_id, 'run_id': run.run_id}})}\n\n"

        try:
            async for event in engine.stream_run_events(run.run_id):
                yield f"data: {json.dumps(event)}\n\n"
        except Exception as e:
            logger.error(f"Stream error: {e}", exc_info=True)
            yield f"data: {json.dumps({'event_type': 'run_error', 'data': {'error': str(e)}})}\n\n"

    return StreamingResponse(
        event_generator(),
        media_type="text/event-stream",
        headers={"Cache-Control": "no-cache", "Connection": "keep-alive", "X-Accel-Buffering": "no"},
    )


@router.get("/chat/runs/{run_id}", response_model=RunStatusResponse)
async def get_run_status(
    run_id: str,
    current_user: Annotated[dict, Depends(get_current_user)],
    db: Annotated[Session, Depends(get_db_session)],
):
    """Get run status and progress."""
    engine = _get_engine(db)
    run = engine.get_run(run_id)
    if not run:
        # Try restoring from DB
        run = engine.restore_run(run_id)
    if not run:
        raise HTTPException(status_code=404, detail="Run not found")

    events = engine.get_run_events(run_id)
    return RunStatusResponse(
        run_id=run.run_id,
        session_id=run.session_id,
        status=run.status.value,
        waiting_for=run.waiting_for,
        events_count=len(events),
    )


@router.get("/chat/runs/{run_id}/stream")
async def stream_run(
    run_id: str,
    current_user: Annotated[dict, Depends(get_current_user)],
    db: Annotated[Session, Depends(get_db_session)],
    last_index: int = Query(default=0, description="Resume from event index (for reconnection)"),
):
    """Stream run events as SSE. Supports reconnection via last_index."""
    engine = _get_engine(db)
    run = engine.get_run(run_id)
    if not run:
        raise HTTPException(status_code=404, detail="Run not found")

    async def event_generator():
        async for event in engine.stream_run_events(run_id, last_index=last_index):
            yield f"data: {json.dumps(event)}\n\n"

    return StreamingResponse(
        event_generator(),
        media_type="text/event-stream",
        headers={"Cache-Control": "no-cache", "Connection": "keep-alive", "X-Accel-Buffering": "no"},
    )


@router.delete("/chat/runs/{run_id}")
async def cancel_run(
    run_id: str,
    current_user: Annotated[dict, Depends(get_current_user)],
    db: Annotated[Session, Depends(get_db_session)],
):
    """Cancel a running or waiting run."""
    engine = _get_engine(db)
    if not engine.cancel_run(run_id):
        raise HTTPException(status_code=404, detail="Run not found or already finished")
    return {"run_id": run_id, "status": "cancelled"}
